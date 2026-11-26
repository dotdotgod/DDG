use crate::{
    args::{Action, Args},
    config_path, connect, load_config,
    output::{Error, Output, Result, cell, tab_error},
    selected,
};
use ddg_agents_proto::{
    host::{MsgRecvParams, MsgSendParams, MsgSubscribeParams},
    id::{decode_base64_bounded, encode_base64, sanitize_display_text},
    terminal::{SessionMessage, SessionMessagePayload},
};
use ddg_agents_transport::{HostProfile, HostTransport, StreamError};
use std::io::{self, Read};

const MAX_PAYLOAD: usize = 65_536;

pub fn in_session_context() -> Option<(String, String)> {
    let env = |name| std::env::var(name).ok().filter(|value| !value.is_empty());
    Some((env("DDG_HOST_SOCK")?, env("DDG_SESSION_TOKEN")?))
}

pub fn inbox_profile(sock: String) -> HostProfile {
    HostProfile {
        transport: HostTransport::Local {
            sock_path: Some(sock),
        },
        ..HostProfile::local_default()
    }
}

pub fn missing_env(command: &'static str) -> Error {
    // This diagnostic names the variables, never their values.
    Error {
        code: "missing_env".into(),
        message: format!(
            "{command} must run inside a DDG session (missing DDG_SESSION_TOKEN or DDG_HOST_SOCK); from an external shell use ddg session read or ddg session wait"
        ),
    }
}

fn payload(input: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    input.take(MAX_PAYLOAD as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PAYLOAD {
        return Err(Error::usage("message must not exceed 65536 bytes"));
    }
    if bytes.is_empty() {
        return Err(Error::usage("message must not be empty"));
    }
    Ok(encode_base64(&bytes))
}

fn plain(message: &SessionMessage) -> String {
    let id = cell(&message.message_id);
    match &message.payload {
        SessionMessagePayload::Message { data_b64 } => {
            let text = match decode_base64_bounded(data_b64, MAX_PAYLOAD) {
                Ok(bytes) => match std::str::from_utf8(&bytes) {
                    Ok(text) => sanitize_display_text(
                        &text.replace(['\u{2028}', '\u{2029}'], " "),
                        MAX_PAYLOAD,
                    ),
                    Err(_) => format!("<{} bytes binary>", bytes.len()),
                },
                Err(_) => "<invalid base64>".into(),
            };
            format!(
                "[{id}] {}@{}: {text}\n",
                cell(&message.from_session_id),
                cell(&message.from_host_id)
            )
        }
        SessionMessagePayload::SessionExited {
            session_id,
            run_generation,
            exit_code,
            ..
        } => {
            let code = exit_code.map_or_else(|| "unknown".into(), |code| code.to_string());
            format!(
                "[{id}] session_exited {} (gen {run_generation}, exit {code})\n",
                cell(session_id)
            )
        }
    }
}

pub async fn run(args: &Args<'_>, out: Output) -> Result {
    let context = in_session_context();
    if args.action == Action::MsgSend {
        let (id, text) = match args.value("to") {
            Some(id) => (id, args.pos.first()),
            None => (args.pos[0], args.pos.get(1)),
        };
        let data_b64 = if args.has("stdin") {
            payload(io::stdin().lock())?
        } else {
            payload(text.expect("validated message text").as_bytes())?
        };
        let result = if let Some((sock, session_token)) = context
            && args.host.is_none()
            && args.config.is_none()
        {
            let (client, _events) = connect(&inbox_profile(sock)).await?;
            client
                .msg_send(MsgSendParams {
                    session_token,
                    to_session_id: id.into(),
                    data_b64,
                    host_id: None,
                })
                .await?
        } else {
            let config = load_config(&config_path(args)?)?;
            let host = selected(args, &config)?;
            let (client, _events) = connect(host).await?;
            client
                .tab_msg_send(id.into(), data_b64)
                .await
                .map_err(|error| tab_error(error, &host.id))?
        };
        return out.show(&result, || {
            format!("sent {} to {}\n", cell(&result.message_id), cell(id))
        });
    }

    let (sock, session_token) = context.ok_or_else(|| {
        missing_env(if args.action == Action::MsgRecv {
            "ddg msg recv"
        } else {
            "ddg msg subscribe"
        })
    })?;
    if args.action == Action::MsgRecv {
        let params = MsgRecvParams {
            session_token,
            max_messages: args.number("max")?.unwrap_or(256),
            wait_ms: args.number("wait-ms")?.unwrap_or(0),
        };
        let (client, _events) = connect(&inbox_profile(sock)).await?;
        let result = client.msg_recv(params).await?;
        return out.show(&result.messages, || {
            result.messages.iter().map(plain).collect()
        });
    }

    let shutdown = ddg_agents_os::process::shutdown_signal()?;
    tokio::pin!(shutdown);
    let profile = inbox_profile(sock);
    let (client, _events) = tokio::select! {
        connection = connect(&profile) => connection?,
        _ = &mut shutdown => return Ok(()),
    };
    let result = tokio::select! {
        result = async {
            let mut stream = client.msg_subscribe(MsgSubscribeParams { session_token }).await?;
            loop {
                match stream.recv().await {
                    Ok(message) => out.show(&message, || plain(&message))?,
                    Err(StreamError::Closed(_)) => break Ok(()),
                    Err(error) => break Err(Error::new("unavailable", error.to_string())),
                }
            }
        } => result,
        _ = &mut shutdown => Ok(()),
    };
    client.close();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_checks_decoded_byte_limits() {
        assert_eq!(payload(&b"abc\0"[..]).unwrap(), "YWJjAA==");
        assert!(payload(&[][..]).is_err());
        assert!(payload(&vec![0; MAX_PAYLOAD][..]).is_ok());
        assert!(payload(&vec![0; MAX_PAYLOAD + 1][..]).is_err());
    }

    #[test]
    fn formats_text_binary_and_exit_as_single_safe_lines() {
        let mut message = SessionMessage {
            message_id: "msg_1".into(),
            to_session_id: "target".into(),
            from_session_id: "ses_1\n".into(),
            from_host_id: "host\u{202e}".into(),
            is_remote: false,
            payload: SessionMessagePayload::Message {
                data_b64: encode_base64("hi\n\x1b\u{2028}there\u{2029}again".as_bytes()),
            },
        };
        assert_eq!(plain(&message), "[msg_1] ses_1@host: hi there again\n");
        message.payload = SessionMessagePayload::Message {
            data_b64: encode_base64(&[255, 254]),
        };
        assert_eq!(plain(&message), "[msg_1] ses_1@host: <2 bytes binary>\n");
        message.payload = SessionMessagePayload::SessionExited {
            session_id: "ses_1".into(),
            room_id: "rm_bg_1".into(),
            run_generation: 2,
            exit_code: Some(7),
        };
        assert_eq!(
            plain(&message),
            "[msg_1] session_exited ses_1 (gen 2, exit 7)\n"
        );
    }
}
