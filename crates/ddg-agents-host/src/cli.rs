use ddg_agents_proto::host::{MsgRecvParams, MsgSubscribeParams, SessionMoveTarget};
use ddg_agents_proto::id::encode_base64;
use ddg_agents_proto::rpc::RpcError;
use ddg_agents_transport::{HostClient, HostProfile, HostTransport, StreamError};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, Read, Write};

#[derive(Debug)]
struct Command {
    method: &'static str,
    params: Value,
    text: Option<String>,
    stdin: bool,
    enter: bool,
}

const USAGE: &str = "usage: ddg-agents-host <serve|connect|health|host list|session <create|list|move|close|restart|screen|write>|msg <send|recv|subscribe>>; Agent? Run: ddg-agents-host --skill";
const SKILL: &str = include_str!("SKILL.md");
const MAX_INPUT: usize = 65536;

pub fn check_token(args: &[String]) -> Result<(), RpcError> {
    if args
        .iter()
        .any(|s| s == "--token" || s.starts_with("--token="))
    {
        return Err(RpcError::invalid_params(
            "--token is not allowed; use DDG_SESSION_TOKEN",
        ));
    }
    Ok(())
}

fn parse(args: &[String]) -> Result<Command, RpcError> {
    check_token(args)?;
    let [group, command, rest @ ..] = args else {
        return Err(RpcError::invalid_params(USAGE));
    };
    let (method, values, switches): (_, &[_], &[_]) = match (group.as_str(), command.as_str()) {
        ("host", "list") => ("host.list_hosts", &[], &[]),
        ("session", "create") => (
            "host.session_create",
            &["room", "host", "cwd", "title"],
            &[],
        ),
        ("session", "list") => ("host.session_list", &["host", "room"], &["include-hidden"]),
        ("session", "move") => ("host.session_move", &["room", "host", "new-room"], &[]),
        ("session", "close") => ("host.session_close", &["host"], &[]),
        ("session", "restart") => ("host.session_restart", &["host"], &[]),
        ("session", "screen") => ("host.session_screen", &["host", "lines", "max-bytes"], &[]),
        ("session", "write") => (
            "host.session_write",
            &["host", "expect-gen"],
            &["enter", "stdin"],
        ),
        ("msg", "send") => ("host.msg_send", &["host", "to"], &["stdin"]),
        ("msg", "recv") => ("host.msg_recv", &["max", "wait-ms"], &[]),
        ("msg", "subscribe") => ("host.msg_subscribe", &[], &[]),
        _ => return Err(RpcError::invalid_params(USAGE)),
    };
    let mut flags = HashMap::new();
    let mut positional = Vec::new();
    let mut rest = rest.iter().map(String::as_str).peekable();
    let mut argv = Vec::new();
    while let Some(arg) = rest.next() {
        if arg == "--" {
            if method == "host.session_create" {
                argv.extend(rest);
                if argv.is_empty() {
                    return Err(RpcError::invalid_params("expected a command after --"));
                }
            } else {
                positional.extend(rest);
            }
            break;
        }
        let Some(option) = arg.strip_prefix("--") else {
            positional.push(arg);
            continue;
        };
        let (flag, inline) = option
            .split_once('=')
            .map_or((option, None), |(k, v)| (k, Some(v)));
        let value = if values.contains(&flag) {
            let value = inline.or_else(|| rest.next_if(|s| !s.starts_with("--")));
            if value.is_none() && flag != "new-room" {
                return Err(RpcError::invalid_params(format!(
                    "--{flag} requires a value"
                )));
            }
            value
        } else if switches.contains(&flag) && inline.is_none() {
            None
        } else {
            return Err(RpcError::invalid_params(
                "unknown option or unexpected option value",
            ));
        };
        if flags.insert(flag, value).is_some() {
            return Err(RpcError::invalid_params(format!("duplicate --{flag}")));
        }
    }

    let flag = |name| flags.get(name).copied().flatten();
    let has = |name| flags.contains_key(name);
    let mut params = json!({});
    if let Some(host) = flag("host") {
        let host = host.trim();
        if host.eq_ignore_ascii_case("all") {
            if method != "host.session_list" {
                return Err(RpcError::invalid_params(
                    "--host all is only valid for session list",
                ));
            }
            params["host_id"] = json!("all");
        } else {
            params["host_id"] = json!(host);
        }
    }
    let mut positional = positional.into_iter();
    if matches!(
        method,
        "host.session_move"
            | "host.session_close"
            | "host.session_restart"
            | "host.session_screen"
            | "host.session_write"
    ) {
        params["session_id"] = json!(
            positional
                .next()
                .ok_or_else(|| RpcError::invalid_params("expected a session ID"))?
        );
    }
    match method {
        "host.session_create" => {
            for (option, key) in [("room", "room_id"), ("cwd", "cwd"), ("title", "title")] {
                if let Some(value) = flag(option) {
                    params[key] = json!(value);
                }
            }
            if !argv.is_empty() {
                params["executor"] = json!({"kind": "command", "argv": argv});
            }
        }
        "host.session_list" => {
            if let Some(room) = flag("room") {
                params["room_id"] = json!(room);
            }
            params["include_hidden"] = json!(has("include-hidden"));
        }
        "host.session_move" => {
            let target = match (flag("room"), has("new-room")) {
                (Some(room), false) => SessionMoveTarget::ExistingRoom {
                    room_id: room.into(),
                },
                (None, true) => SessionMoveTarget::NewRoom {
                    name: flag("new-room").map(str::to_owned),
                },
                _ => {
                    return Err(RpcError::invalid_params(
                        "specify exactly one of --room or --new-room",
                    ));
                }
            };
            params["target"] = json!(target);
        }
        "host.session_screen" => {
            params["lines"] = json!(number("lines", flag("lines"))?.unwrap_or(24_u32));
            params["max_bytes"] =
                json!(number("max-bytes", flag("max-bytes"))?.unwrap_or(262144_u32));
        }
        "host.session_write" => {
            if let Some(generation) = number::<u64>("expect-gen", flag("expect-gen"))? {
                params["expect_run_generation"] = json!(generation);
            }
        }
        "host.msg_send" => {
            params["to_session_id"] =
                json!(flag("to").or_else(|| positional.next()).ok_or_else(|| {
                    RpcError::invalid_params("expected a destination session ID")
                })?);
        }
        "host.msg_recv" => {
            params["max_messages"] = json!(number("max", flag("max"))?.unwrap_or(256_u32));
            params["wait_ms"] = json!(number("wait-ms", flag("wait-ms"))?.unwrap_or(0_u64));
        }
        _ => {}
    }
    let takes_text = matches!(method, "host.msg_send" | "host.session_write");
    let text = if takes_text {
        positional.next().map(str::to_owned)
    } else {
        None
    };
    let stdin = has("stdin");
    let enter = has("enter");
    if positional.next().is_some() || (stdin && text.is_some()) {
        return Err(RpcError::invalid_params(
            "unexpected positional argument; pass text or --stdin",
        ));
    }
    if takes_text && text.is_none() && !stdin && !enter {
        return Err(RpcError::invalid_params("pass text or --stdin"));
    }
    Ok(Command {
        method,
        params,
        text,
        stdin,
        enter,
    })
}

fn number<T: std::str::FromStr>(name: &str, value: Option<&str>) -> Result<Option<T>, RpcError> {
    value
        .map(|s| {
            s.parse().map_err(|_| {
                RpcError::invalid_params(format!("--{name} requires an unsigned integer"))
            })
        })
        .transpose()
}

pub fn error(err: RpcError) -> io::Error {
    io::Error::other(json!(err).to_string())
}

fn required_env(name: &str) -> io::Result<String> {
    std::env::var(name)
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| error(RpcError::missing_env(format!("{name} is required"))))
}

pub fn run(args: &[String]) -> io::Result<()> {
    if args
        .iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| matches!(arg.as_str(), "--skill" | "--skills"))
    {
        check_token(args).map_err(error)?;
        return io::stdout().lock().write_all(SKILL.as_bytes());
    }
    let mut command = parse(args).map_err(error)?;
    let token = required_env("DDG_SESSION_TOKEN")?;
    let socket = required_env("DDG_HOST_SOCK")?;
    if matches!(command.method, "host.session_write" | "host.msg_send") {
        let mut data = command.text.take().unwrap_or_default().into_bytes();
        if command.stdin {
            io::stdin()
                .take((MAX_INPUT + 1) as u64)
                .read_to_end(&mut data)?;
        }
        if command.enter {
            data.push(b'\r');
        }
        if data.len() > MAX_INPUT {
            return Err(error(RpcError::invalid_params("input exceeds 65536 bytes")));
        }
        command.params["data_b64"] = json!(encode_base64(&data));
    }
    command.params["session_token"] = json!(token);
    let profile = HostProfile {
        transport: HostTransport::Local {
            sock_path: Some(socket),
        },
        ..HostProfile::local_default()
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let (client, events) = HostClient::connect_once(profile, "bridge-cli")
                .await
                .map_err(|e| error(e.into()))?;
            drop(events);
            let mut out = io::stdout().lock();
            match command.method {
                "host.msg_subscribe" => {
                    let params: MsgSubscribeParams = serde_json::from_value(command.params)?;
                    let mut stream = client.msg_subscribe(params).await.map_err(error)?;
                    loop {
                        match stream.recv().await {
                            Ok(message) => print_result(&mut out, command.method, json!(message))?,
                            Err(StreamError::Closed(_) | StreamError::Disconnected) => break,
                            Err(e) => return Err(io::Error::other(e)),
                        }
                    }
                }
                "host.msg_recv" => {
                    let params: MsgRecvParams = serde_json::from_value(command.params)?;
                    let result = client.msg_recv(params).await.map_err(error)?;
                    print_result(&mut out, command.method, json!(result))?;
                }
                method => {
                    let result = client
                        .call_raw(method, command.params)
                        .await
                        .map_err(error)?;
                    print_result(&mut out, method, result)?;
                }
            }
            client.close();
            Ok(())
        })
}

fn print_result(out: &mut impl Write, method: &str, result: Value) -> io::Result<()> {
    let value = if method == "host.msg_recv" {
        &result["messages"]
    } else {
        &result
    };
    serde_json::to_writer(&mut *out, value)?;
    writeln!(out)?;
    out.flush()
}

#[cfg(test)]
mod tests;
