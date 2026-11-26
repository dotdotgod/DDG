use crate::{
    args::Args,
    connect, msg,
    output::{Error, Output, Result, cell, tab_error},
    selected,
};
use ddg_agents_proto::{
    host::{Catalog, MsgRecvParams, SessionScreenResult, TabRecord, TabRuntimeState},
    rpc::ErrorKind,
    terminal::{CloseReason, SessionMessage},
};
use ddg_agents_transport::{ConnectionState, HostClient, HostEvent, HostsConfig};
use serde::Serialize;
use std::{future::pending, time::Duration};
use tokio::{sync::mpsc, time::Instant};

const WINDOW_BYTES: usize = 65_536;

#[derive(Serialize)]
struct WaitResult<'a> {
    host: &'a str,
    session_id: &'a str,
    reason: &'static str,
    run_generation: u64,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<SessionMessage>,
}

impl WaitResult<'_> {
    fn update(&mut self, tab: &TabRecord) {
        self.run_generation = tab.run_generation;
        self.exit_code = None;
        self.exit_status = None;
        self.state = match &tab.runtime {
            TabRuntimeState::Running { .. } => "running",
            TabRuntimeState::Exited { status } => {
                self.exit_status = Some(status.clone());
                "exited"
            }
            TabRuntimeState::StoppedAfterReboot => "stopped_after_reboot",
        };
    }

    fn plain(&self, timeout: u64) -> String {
        let id = cell(self.session_id);
        let generation = self.run_generation;
        match self.reason {
            "exited" => {
                let status = self
                    .exit_status
                    .as_deref()
                    .map(cell)
                    .or_else(|| self.exit_code.map(|code| format!("exit {code}")))
                    .unwrap_or_else(|| self.state.into());
                format!("exited {id} (gen {generation}, {status})\n")
            }
            "closed" => format!("closed {id} (gen {generation})\n"),
            "output" => format!("matched {id} (gen {generation})\n"),
            "timeout" => format!("timeout {id} (after {timeout}s)\n"),
            "message" => {
                let message = self.message.as_ref().expect("message result");
                format!(
                    "message {} from {}\n",
                    cell(&message.message_id),
                    cell(&message.from_session_id)
                )
            }
            _ => unreachable!(),
        }
    }

    fn matched(&mut self, generation: u64) {
        self.reason = "output";
        self.run_generation = generation;
        self.matched = Some(true);
    }
}

fn find_tab<'a>(catalog: &'a Catalog, id: &str) -> Option<&'a TabRecord> {
    catalog
        .rooms
        .iter()
        .flat_map(|room| &room.tabs)
        .find(|tab| tab.id == id)
}

fn exited_before_match() -> Error {
    Error::new("session_exited", "session exited before matching output")
}

async fn screen(
    client: &HostClient,
    status: &WaitResult<'_>,
) -> Result<Option<SessionScreenResult>> {
    match client
        .tab_screen(status.session_id.into(), 2000, 262_144)
        .await
    {
        Ok(screen) => Ok(Some(screen)),
        Err(error) if error.data.code.as_deref() == Some("session_not_found") => Ok(None),
        Err(error) => Err(tab_error(error, status.host)),
    }
}

async fn screen_matches(
    client: &HostClient,
    status: &mut WaitResult<'_>,
    pattern: &str,
) -> Result<bool> {
    let Some(screen) = screen(client, status).await? else {
        return Err(exited_before_match());
    };
    if screen.text.contains(pattern) {
        status.matched(screen.run_generation);
        return Ok(true);
    }
    Ok(false)
}

pub async fn run(args: &Args<'_>, config: &HostsConfig, out: Output) -> Result {
    let timeout = args.number("timeout")?.unwrap_or(180);
    let pattern = args.value("output");
    if pattern.is_some_and(|text| text.len() > WINDOW_BYTES) {
        return Err(Error::usage("--output must not exceed 65536 bytes"));
    }
    let context = if args.has("or-message") {
        Some(
            msg::in_session_context()
                .ok_or_else(|| msg::missing_env("ddg session wait --or-message"))?,
        )
    } else {
        None
    };
    let host = selected(args, config)?;
    let id = args.pos[0];
    let (client, events) = connect(host).await?;
    let result = async {
        let catalog = client.catalog_subscribe().await?.catalog;
        let tab = find_tab(&catalog, id)
            .ok_or_else(|| Error::new("session_not_found", "session not found"))?;
        let mut status = WaitResult {
            host: &host.id,
            session_id: id,
            reason: "exited",
            run_generation: tab.run_generation,
            state: "running",
            exit_status: None,
            exit_code: None,
            matched: pattern.map(|_| false),
            message: None,
        };
        status.update(tab);
        wait(
            &client,
            events,
            &mut status,
            pattern,
            Duration::from_secs(timeout),
            context,
        )
        .await?;
        out.show(&status, || status.plain(timeout))
    }
    .await;
    client.close();
    result
}

async fn wait(
    client: &HostClient,
    events: mpsc::UnboundedReceiver<HostEvent>,
    status: &mut WaitResult<'_>,
    pattern: Option<&str>,
    timeout: Duration,
    context: Option<(String, String)>,
) -> Result {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| Error::usage("--timeout is too large"))?;
    let stream = if let Some(pattern) = pattern {
        let current = screen(client, status).await?;
        if let Some(screen) = &current
            && screen.text.contains(pattern)
        {
            status.matched(screen.run_generation);
            return Ok(());
        }
        if current.is_none() || status.state != "running" {
            return Err(exited_before_match());
        }
        let attached = match client
            .tab_attach(status.session_id.into(), false, 80, 24, 0)
            .await
        {
            Ok(attached) => attached,
            Err(error)
                if error.data.kind == ErrorKind::NotFound
                    || error.data.code.as_deref() == Some("session_not_running") =>
            {
                let current = screen(client, status).await?;
                if let Some(screen) = current {
                    if screen.text.contains(pattern) {
                        status.matched(screen.run_generation);
                        return Ok(());
                    }
                    return Err(tab_error(error, status.host));
                }
                return Err(exited_before_match());
            }
            Err(error) => return Err(tab_error(error, status.host)),
        };
        status.run_generation = attached.run_generation;
        Some((attached.channel, pattern))
    } else {
        if status.state != "running" {
            return Ok(());
        }
        None
    };

    let result = async {
        let inbox = match context {
            Some((sock, token)) => Some((connect(&msg::inbox_profile(sock)).await?.0, token)),
            None => None,
        };
        let receive = async {
            let Some((client, token)) = &inbox else {
                return pending().await;
            };
            // A Credit stream can dequeue multiple messages before the first is read.
            let wait_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis();
            if wait_ms == 0 {
                return pending().await;
            }
            let received = client
                .msg_recv(MsgRecvParams {
                    session_token: token.clone(),
                    max_messages: 1,
                    wait_ms: wait_ms.try_into().unwrap_or(u64::MAX),
                })
                .await?;
            match received.messages.into_iter().next() {
                Some(message) => Ok(message),
                None => pending().await,
            }
        };
        let result = observe(client, events, status, stream, deadline, receive).await;
        if let Some((client, _)) = inbox {
            client.close();
        }
        result
    }
    .await;
    if let Some((channel, _)) = stream {
        client.detach_channel(channel);
    }
    result
}

async fn observe(
    client: &HostClient,
    mut events: mpsc::UnboundedReceiver<HostEvent>,
    status: &mut WaitResult<'_>,
    stream: Option<(u32, &str)>,
    deadline: Instant,
    receive: impl Future<Output = Result<SessionMessage>>,
) -> Result {
    let timeout = tokio::time::sleep_until(deadline);
    tokio::pin!(receive, timeout);
    let mut output = OutputWindow::default();
    loop {
        tokio::select! {
            biased;
            message = &mut receive => {
                status.message = Some(message?);
                status.reason = "message";
                return Ok(());
            }
            _ = &mut timeout => {
                if let Some((_, pattern)) = stream {
                    // The initial repaint can split a match across the attach boundary.
                    if screen_matches(client, status, pattern).await? {
                        return Ok(());
                    }
                    if status.state != "running" {
                        return Err(exited_before_match());
                    }
                }
                status.reason = "timeout";
                return Ok(());
            }
            event = events.recv() => match event {
                Some(HostEvent::SessionExited { session_id, run_generation, exit_code, .. })
                    if session_id == status.session_id
                        && run_generation >= status.run_generation
                        && (stream.is_none() || run_generation == status.run_generation) =>
                {
                    status.run_generation = run_generation;
                    status.state = "exited";
                    status.exit_code = exit_code;
                }
                Some(HostEvent::CatalogChanged { catalog, .. }) => match find_tab(&catalog, status.session_id) {
                    Some(tab) if status.state == "running"
                        && tab.run_generation >= status.run_generation
                        && (stream.is_none() || tab.run_generation == status.run_generation) => status.update(tab),
                    None if status.state == "running" => status.state = "closed",
                    _ => {}
                },
                Some(HostEvent::StreamData { channel, bytes, .. }) => {
                    if let Some((ch, pattern)) = stream && channel == ch && output.push(&bytes, pattern) {
                        status.matched(status.run_generation);
                        return Ok(());
                    }
                }
                Some(HostEvent::StreamClose { channel, reason, .. }) => {
                    if let Some((ch, pattern)) = stream && channel == ch {
                        if let Ok(CloseReason::Exited { exit_code, .. }) = serde_json::from_str(&reason) {
                            status.state = "exited";
                            status.exit_code = exit_code;
                        }
                        return if screen_matches(client, status, pattern).await? {
                            Ok(())
                        } else {
                            Err(exited_before_match())
                        };
                    }
                }
                Some(HostEvent::StateChanged(ConnectionState::Disconnected(_))) | None => {
                    return Err(Error::new("unavailable", "host connection closed while waiting"));
                }
                _ => {}
            }
        }
        // Lifecycle notices and stream bytes travel on separate channels. CLOSE
        // on the attached stream follows its last bytes, even if a notice wins the race.
        if stream.is_none() && status.state != "running" {
            status.reason = if status.state == "closed" {
                "closed"
            } else {
                "exited"
            };
            return Ok(());
        }
    }
}

#[derive(Default)]
enum AnsiStripper {
    #[default]
    Text,
    Esc,
    EscIntermediate,
    Csi,
    Osc,
    OscEsc,
    ControlString,
    ControlStringEsc,
}

impl AnsiStripper {
    fn push(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        use AnsiStripper::*;
        for &byte in bytes {
            if matches!(byte, 0x18 | 0x1a) {
                *self = Text;
                continue;
            }
            *self = match (&*self, byte) {
                (Text, 0x1b) => Esc,
                (Text, b'\n' | b'\t' | 0x20..=0x7e | 0x80..=0xff) => {
                    out.push(byte);
                    Text
                }
                (Esc, b'[') => Csi,
                (Esc, b']') => Osc,
                (Esc, b'P' | b'X' | b'^' | b'_') => ControlString,
                (Esc | EscIntermediate, 0x20..=0x2f) => EscIntermediate,
                (Csi, 0x20..=0x3f) => Csi,
                (Osc | OscEsc, 0x07) | (OscEsc | ControlStringEsc, b'\\') => Text,
                (Osc | OscEsc, 0x1b) => OscEsc,
                (ControlString | ControlStringEsc, 0x1b) => ControlStringEsc,
                (Osc | OscEsc, _) => Osc,
                (ControlString | ControlStringEsc, _) => ControlString,
                (_, 0x1b) => Esc,
                _ => Text,
            };
        }
    }
}

#[derive(Default)]
struct OutputWindow {
    ansi: AnsiStripper,
    text: String,
    utf8_tail: Vec<u8>,
}

impl OutputWindow {
    fn push(&mut self, bytes: &[u8], pattern: &str) -> bool {
        let mut plain = std::mem::take(&mut self.utf8_tail);
        self.ansi.push(bytes, &mut plain);
        let old_len = self.text.len();
        let mut chunks = plain.utf8_chunks().peekable();
        while let Some(chunk) = chunks.next() {
            self.text.push_str(chunk.valid());
            let invalid = chunk.invalid();
            if !invalid.is_empty() {
                if chunks.peek().is_none()
                    && std::str::from_utf8(invalid)
                        .unwrap_err()
                        .error_len()
                        .is_none()
                {
                    self.utf8_tail.extend_from_slice(invalid);
                } else {
                    self.text.push('\u{fffd}');
                }
            }
        }
        let start = self
            .text
            .floor_char_boundary(old_len.saturating_sub(pattern.len()));
        let matched = self.text[start..].contains(pattern);
        let trim = self
            .text
            .ceil_char_boundary(self.text.len().saturating_sub(WINDOW_BYTES));
        self.text.drain(..trim);
        matched
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_across_split_escapes_csi_and_both_osc_terminators() {
        let mut window = OutputWindow::default();
        for bytes in [
            &b"\x1b"[..],
            b"[32",
            b"mRE\x1b]0;not output",
            b"\x1b",
            b"\\A\x1b]title",
            b"\x07D\x1b",
            b"7",
        ] {
            assert!(!window.push(bytes, "READY"));
        }
        assert!(window.push(b"Y\x1b[0m", "READY"));
        assert_eq!(window.text, "READY");
    }

    #[test]
    fn matches_split_utf8_at_a_sliding_window_boundary() {
        let mut window = OutputWindow::default();
        let prefix = "é".repeat(WINDOW_BYTES / 2);
        assert!(!window.push(prefix.as_bytes(), "界READY"));
        assert!(!window.push(&[0xe7, 0x95], "界READY"));
        assert!(window.push(b"\x8cREADY", "界READY"));
        assert!(window.text.len() <= WINDOW_BYTES);
        assert!(window.text.ends_with("界READY"));
        assert!(window.text.starts_with('é'));
    }
}
