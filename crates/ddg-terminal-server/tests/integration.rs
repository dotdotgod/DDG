//! End-to-end integration tests for `ddg-terminal-server` using real OS PTYs,
//! real `Holder` + `Core` processes, and the `connect` stdio bridge in isolated
//! temporary directories.

#![cfg(unix)]

use ddg_agents_proto::frame::{CONTROL_CHANNEL, Frame, FrameKind, read_frame, write_frame};
use ddg_agents_proto::rpc::{Request, Response};
use ddg_agents_proto::terminal::{
    ControlMode, HelloParams, HelloResult, PROTOCOL_VERSION, PtyAttachParams, PtyAttachResult,
    PtyClaimControlParams, PtyClaimControlResult, PtyKillParams, PtyListResult, PtySpawnParams,
    PtySpawnResult, SessionInboxSubscribeResult, SessionMessage, SessionReadScreenResult,
    SessionRecvResult, SessionSendResult, SessionWriteInputResult,
};
use ddg_agents_vt::Vt;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

struct Sandbox {
    root: PathBuf,
    state: PathBuf,
    run: PathBuf,
    children: Vec<Child>,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let id = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        // Keep Unix socket paths short (< 104 bytes on macOS).
        let root = PathBuf::from(format!(
            "/tmp/ddg-ts-{}-{}-{}",
            std::process::id(),
            id,
            name
        ));
        let state = root.join("st");
        let run = root.join("rn");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        Self {
            root,
            state,
            run,
            children: Vec::new(),
        }
    }

    fn bin() -> &'static str {
        env!("CARGO_BIN_EXE_ddg-terminal-server")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(Self::bin());
        cmd.args(args)
            .env("DDG_STATE_DIR", &self.state)
            .env("DDG_RUN_DIR", &self.run)
            .env("DDG_HOLDER_GRACE_MS", "100");
        cmd
    }

    fn spawn_core(&mut self) -> u32 {
        let child = self
            .cmd(&["core"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        self.children.push(child);
        self.wait_for_terminal_sock(Duration::from_secs(5));
        pid
    }

    fn wait_for_terminal_sock(&self, timeout: Duration) {
        let sock = self.run.join("terminal.sock");
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if UnixStream::connect(&sock).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("terminal.sock did not appear at {}", sock.display());
    }

    fn connect_stdio(&self) -> StdioClient {
        let mut child = self
            .cmd(&["connect"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut client = StdioClient {
            child,
            stdin,
            stdout,
            next_id: 1,
            framed: false,
            notifications: Vec::new(),
        };
        let hello: HelloResult = client
            .call(
                "hello",
                json!(HelloParams {
                    protocol_version: PROTOCOL_VERSION,
                    framed: true,
                    client_id: Some("test-client".into()),
                }),
            )
            .unwrap();
        assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
        client.framed = true;
        client
    }

    fn shutdown_all(&self) {
        // Kill any surviving PTYs via a clean connect, then stop core & holder cleanly.
        if UnixStream::connect(self.run.join("terminal.sock")).is_ok() {
            let mut client = self.connect_stdio();
            if let Ok(list) = client.call::<PtyListResult>("pty.list", json!({})) {
                for pty in list.ptys {
                    let _ = client
                        .call::<Value>("pty.kill", json!(PtyKillParams { pty_id: pty.pty_id }));
                }
            }
            drop(client);
        }
        // Tell holder to shut down cleanly and wait for BYE so both holder and core exit cleanly.
        if let Ok(mut hs) = UnixStream::connect(self.run.join("holder.sock")) {
            let _ = hs.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = hs.write_all(&ddg_agents_proto::holder::encode_json(
                ddg_agents_proto::holder::HELLO,
                0,
                &ddg_agents_proto::holder::HolderHello {
                    version: ddg_agents_proto::holder::HOLDER_PROTO_VERSION,
                },
            ));
            let _ = hs.write_all(&ddg_agents_proto::holder::encode_json(
                ddg_agents_proto::holder::GOODBYE,
                0,
                &ddg_agents_proto::holder::HolderGoodbye::Shutdown,
            ));
            while let Ok(Some((kind, _, _))) = ddg_agents_proto::holder::read_frame(&mut hs) {
                if kind == ddg_agents_proto::holder::BYE {
                    break;
                }
            }
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        self.shutdown_all();
        for mut child in self.children.drain(..) {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct StdioClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    framed: bool,
    notifications: Vec<Value>,
}

impl StdioClient {
    fn call_raw(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request::new(id, method, params);
        if !self.framed {
            let line = format!("{}\n", serde_json::to_string(&req).unwrap());
            self.stdin.write_all(line.as_bytes()).unwrap();
            self.stdin.flush().unwrap();
            let mut reply = String::new();
            self.stdout.read_line(&mut reply).unwrap();
            let resp: Response = serde_json::from_str(&reply).unwrap();
            return match resp.error {
                Some(e) => Err(json!(e)),
                None => Ok(resp.result.unwrap_or(Value::Null)),
            };
        }
        let payload = serde_json::to_vec(&req).unwrap();
        write_frame(&mut self.stdin, &Frame::data(CONTROL_CHANNEL, payload)).unwrap();
        self.stdin.flush().unwrap();

        loop {
            let frame = read_frame(&mut self.stdout)
                .unwrap()
                .expect("unexpected EOF waiting for RPC reply");
            if frame.channel == CONTROL_CHANNEL && frame.kind == FrameKind::Data {
                let val: Value = serde_json::from_slice(&frame.payload).unwrap();
                if val.get("id").and_then(Value::as_u64) == Some(id) {
                    if let Some(err) = val.get("error") {
                        return Err(err.clone());
                    }
                    return Ok(val.get("result").cloned().unwrap_or(Value::Null));
                }
                self.notifications.push(val);
            }
        }
    }

    fn call<T: DeserializeOwned>(&mut self, method: &str, params: Value) -> Result<T, Value> {
        let val = self.call_raw(method, params)?;
        Ok(serde_json::from_value(val).unwrap())
    }

    fn send_input(&mut self, channel: u32, bytes: &[u8]) {
        write_frame(&mut self.stdin, &Frame::data(channel, bytes.to_vec())).unwrap();
        self.stdin.flush().unwrap();
    }

    fn read_stream_until(
        &mut self,
        channel: u32,
        vt: &mut Vt,
        needle: &str,
        timeout: Duration,
    ) -> String {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if vt.plain_text().contains(needle) {
                return vt.plain_text();
            }
            let frame = read_frame(&mut self.stdout)
                .unwrap()
                .expect("unexpected EOF reading stream");
            match (frame.channel, frame.kind) {
                (CONTROL_CHANNEL, FrameKind::Data) => {
                    if let Ok(v) = serde_json::from_slice::<Value>(&frame.payload) {
                        self.notifications.push(v);
                    }
                }
                (ch, FrameKind::Data) if ch == channel => {
                    vt.write(&frame.payload);
                    let len = frame.payload.len() as u32;
                    if len > 0 {
                        let _ = write_frame(&mut self.stdin, &Frame::credit(channel, len));
                        let _ = self.stdin.flush();
                    }
                    if vt.plain_text().contains(needle) {
                        return vt.plain_text();
                    }
                }
                (ch, FrameKind::Resize) if ch == channel => {
                    if let Ok((cols, rows)) = ddg_agents_proto::frame::decode_resize(&frame.payload)
                    {
                        vt.resize(cols, rows);
                    }
                }
                _ => {}
            }
        }
        panic!(
            "timed out waiting for {needle:?}; screen was:\n{}",
            vt.plain_text()
        );
    }

    fn wait_for_notification(&mut self, method: &str) -> Value {
        if let Some(pos) = self
            .notifications
            .iter()
            .position(|v| v["method"] == method)
        {
            return self.notifications.remove(pos);
        }
        loop {
            let frame = read_frame(&mut self.stdout)
                .unwrap()
                .expect("unexpected EOF waiting for notification");
            if frame.channel == CONTROL_CHANNEL && frame.kind == FrameKind::Data {
                let val: Value = serde_json::from_slice(&frame.payload).unwrap();
                if val["method"] == method {
                    return val;
                }
                self.notifications.push(val);
            }
        }
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn spawn_detach_reattach_same_pid_and_explicit_kill() {
    let sb = Sandbox::new("reattach");
    let mut c1 = sb.connect_stdio();

    let spawned: PtySpawnResult = c1
        .call(
            "pty.spawn",
            json!(PtySpawnParams {
                session_id: String::new(),
                run_generation: 0,
                cols: 80,
                rows: 24,
                cwd: None,
                argv: Some(vec!["/bin/sh".into()]),
                env: None,
            }),
        )
        .unwrap();

    let attached: PtyAttachResult = c1
        .call(
            "pty.attach",
            json!(PtyAttachParams {
                pty_id: spawned.pty_id,
                want_control: true,
                cols: 80,
                rows: 24,
                history_lines: 100,
            }),
        )
        .unwrap();
    assert_eq!(attached.mode, ControlMode::Control);

    let mut vt1 = Vt::new(80, 24).unwrap();
    c1.send_input(attached.channel, b"echo HELLO_DDG_MARKER\n");
    c1.read_stream_until(
        attached.channel,
        &mut vt1,
        "HELLO_DDG_MARKER",
        Duration::from_secs(5),
    );

    // Detach by dropping the first client completely.
    drop(c1);

    // Health check passes while PTY stays alive.
    assert!(sb.cmd(&["health"]).status().unwrap().success());

    // Reattach from a brand new client: same PID and prior screen preserved!
    let mut c2 = sb.connect_stdio();
    let list: PtyListResult = c2.call("pty.list", json!({})).unwrap();
    assert_eq!(list.ptys.len(), 1);
    assert_eq!(list.ptys[0].pty_id, spawned.pty_id);
    assert_eq!(list.ptys[0].pid, spawned.pid);

    let reattached: PtyAttachResult = c2
        .call(
            "pty.attach",
            json!(PtyAttachParams {
                pty_id: spawned.pty_id,
                want_control: true,
                cols: 80,
                rows: 24,
                history_lines: 100,
            }),
        )
        .unwrap();
    assert_eq!(reattached.mode, ControlMode::Control);
    let mut vt2 = Vt::new(80, 24).unwrap();
    c2.read_stream_until(
        reattached.channel,
        &mut vt2,
        "HELLO_DDG_MARKER",
        Duration::from_secs(5),
    );
    c2.send_input(reattached.channel, b"echo AFTER_REATTACH\n");
    c2.read_stream_until(
        reattached.channel,
        &mut vt2,
        "AFTER_REATTACH",
        Duration::from_secs(5),
    );

    // Explicit kill reaps the process and removes it from pty.list.
    c2.call::<Value>(
        "pty.kill",
        json!(PtyKillParams {
            pty_id: spawned.pty_id
        }),
    )
    .unwrap();
    let list_after: PtyListResult = c2.call("pty.list", json!({})).unwrap();
    assert!(list_after.ptys.is_empty());
}

#[test]
fn single_controller_rejects_readonly_and_stale_epoch_after_takeover() {
    let sb = Sandbox::new("control");
    let mut c1 = sb.connect_stdio();
    let spawned: PtySpawnResult = c1
        .call(
            "pty.spawn",
            json!(PtySpawnParams {
                session_id: String::new(),
                run_generation: 0,
                cols: 80,
                rows: 24,
                cwd: None,
                argv: Some(vec!["/bin/sh".into()]),
                env: None,
            }),
        )
        .unwrap();

    let a1: PtyAttachResult = c1
        .call(
            "pty.attach",
            json!(PtyAttachParams {
                pty_id: spawned.pty_id,
                want_control: true,
                cols: 80,
                rows: 24,
                history_lines: 50,
            }),
        )
        .unwrap();
    assert_eq!(a1.mode, ControlMode::Control);

    // Second client attaches read-only while c1 holds control.
    let mut c2 = sb.connect_stdio();
    let a2: PtyAttachResult = c2
        .call(
            "pty.attach",
            json!(PtyAttachParams {
                pty_id: spawned.pty_id,
                want_control: true,
                cols: 80,
                rows: 24,
                history_lines: 50,
            }),
        )
        .unwrap();
    assert_eq!(a2.mode, ControlMode::Readonly);

    // Non-forced claim by c2 must be denied.
    let denied_claim = c2.call_raw(
        "pty.claim_control",
        json!(PtyClaimControlParams {
            pty_id: spawned.pty_id,
            force: false,
        }),
    );
    assert!(denied_claim.is_err());

    // Read-only input from c2 triggers pty.control_denied.
    c2.send_input(a2.channel, b"echo SHOULD_NOT_RUN\n");
    let denied = c2.wait_for_notification("pty.control_denied");
    assert_eq!(denied["params"]["kind"], "control_denied");

    // Forced takeover transfers control to c2 and invalidates c1's old epoch.
    let claimed: PtyClaimControlResult = c2
        .call(
            "pty.claim_control",
            json!(PtyClaimControlParams {
                pty_id: spawned.pty_id,
                force: true,
            }),
        )
        .unwrap();
    assert!(claimed.control_epoch > a1.control_epoch);

    c1.send_input(a1.channel, b"echo STALE_C1\n");
    let stale = c1.wait_for_notification("pty.control_denied");
    assert_eq!(stale["params"]["kind"], "control_denied");

    // c2 can now drive the shell, and both c2 and c1 (observer) see the output!
    let mut vt1 = Vt::new(80, 24).unwrap();
    let mut vt2 = Vt::new(80, 24).unwrap();
    c2.send_input(a2.channel, b"echo C2_IN_CONTROL\n");
    c2.read_stream_until(
        a2.channel,
        &mut vt2,
        "C2_IN_CONTROL",
        Duration::from_secs(5),
    );
    c1.read_stream_until(
        a1.channel,
        &mut vt1,
        "C2_IN_CONTROL",
        Duration::from_secs(5),
    );
}

#[test]
fn core_sigkill_recovery_preserves_same_pid_and_screen_via_holder() {
    let mut sb = Sandbox::new("crash");
    let core_pid = sb.spawn_core();

    let mut c1 = sb.connect_stdio();
    let spawned: PtySpawnResult = c1
        .call(
            "pty.spawn",
            json!(PtySpawnParams {
                session_id: "ses_00000000000000000000000001".into(),
                run_generation: 7,
                cols: 80,
                rows: 24,
                cwd: None,
                argv: Some(vec!["/bin/sh".into()]),
                env: None,
            }),
        )
        .unwrap();
    let a1: PtyAttachResult = c1
        .call(
            "pty.attach",
            json!(PtyAttachParams {
                pty_id: spawned.pty_id,
                want_control: true,
                cols: 80,
                rows: 24,
                history_lines: 50,
            }),
        )
        .unwrap();
    let mut vt1 = Vt::new(80, 24).unwrap();
    c1.send_input(a1.channel, b"echo BEFORE_CORE_CRASH\n");
    c1.read_stream_until(
        a1.channel,
        &mut vt1,
        "BEFORE_CORE_CRASH",
        Duration::from_secs(5),
    );
    drop(c1);

    // Hard-kill Core with SIGKILL! Holder must keep PTY alive and respawn Core.
    unsafe {
        libc::kill(core_pid as libc::pid_t, libc::SIGKILL);
    }
    let _ = sb.children[0].wait();

    // Wait for Holder's supervisor to respawn a fresh Core on terminal.sock.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut recovered = None;
    while Instant::now() < deadline {
        if let Ok(mut s) = UnixStream::connect(sb.run.join("terminal.sock")) {
            let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
            let req = format!(
                "{}\n",
                serde_json::to_string(&Request::new(1, "ping", json!({}))).unwrap()
            );
            if s.write_all(req.as_bytes()).is_ok() {
                let mut buf = String::new();
                if BufReader::new(s).read_line(&mut buf).unwrap_or(0) > 0
                    && buf.contains("\"ok\":true")
                {
                    recovered = Some(());
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        recovered.is_some(),
        "holder did not restart core after SIGKILL"
    );

    let mut c2 = sb.connect_stdio();
    let list: PtyListResult = c2.call("pty.list", json!({})).unwrap();
    assert_eq!(list.ptys.len(), 1);
    assert_eq!(list.ptys[0].pty_id, spawned.pty_id);
    assert_eq!(
        list.ptys[0].pid, spawned.pid,
        "PTY PID must survive Core SIGKILL!"
    );
    assert_eq!(list.ptys[0].session_id, "ses_00000000000000000000000001");
    assert_eq!(list.ptys[0].run_generation, 7);

    let a2: PtyAttachResult = c2
        .call(
            "pty.attach",
            json!(PtyAttachParams {
                pty_id: spawned.pty_id,
                want_control: true,
                cols: 80,
                rows: 24,
                history_lines: 50,
            }),
        )
        .unwrap();
    let mut vt2 = Vt::new(80, 24).unwrap();
    c2.read_stream_until(
        a2.channel,
        &mut vt2,
        "BEFORE_CORE_CRASH",
        Duration::from_secs(5),
    );
    let written: SessionWriteInputResult = c2
        .call(
            "session.write_input",
            json!({
                "session_id": list.ptys[0].session_id,
                "data_b64": ddg_agents_proto::id::encode_base64(b"echo AFTER_CORE_CRASH\n"),
                "expect_run_generation": 7,
                "from_session_id": list.ptys[0].session_id,
                "from_host_id": "test",
                "is_remote": false,
            }),
        )
        .unwrap();
    assert_eq!((written.byte_len, written.run_generation), (22, 7));
    c2.read_stream_until(
        a2.channel,
        &mut vt2,
        "AFTER_CORE_CRASH",
        Duration::from_secs(5),
    );
    let screen: SessionReadScreenResult = c2
        .call(
            "session.read_screen",
            json!({
                "session_id": list.ptys[0].session_id, "lines": 24, "max_bytes": 4096,
            }),
        )
        .unwrap();
    assert_eq!(screen.run_generation, 7);
    assert!(screen.text.contains("AFTER_CORE_CRASH"));
}

#[test]
fn session_inbox_requires_framing_and_delivers_ndjson_until_kill() {
    let sb = Sandbox::new("inbox");
    let mut sender = sb.connect_stdio();
    let session = "ses_00000000000000000000000001";
    let child: PtySpawnResult = sender.call("pty.spawn", json!({
        "session_id": session, "run_generation": 1, "cols": 80, "rows": 24, "argv": ["/bin/sh"],
    })).unwrap();

    let params = json!({"session_id": session});
    let mut socket = UnixStream::connect(sb.run.join("terminal.sock")).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let req = Request::new(1, "session.inbox_subscribe", params.clone());
    writeln!(socket, "{}", serde_json::to_string(&req).unwrap()).unwrap();
    let mut response = String::new();
    BufReader::new(socket).read_line(&mut response).unwrap();
    let response: Response = serde_json::from_str(&response).unwrap();
    assert_eq!(
        response.error.unwrap().data.code.as_deref(),
        Some("invalid_params")
    );

    let mut receiver = sb.connect_stdio();
    let inbox: SessionInboxSubscribeResult =
        receiver.call("session.inbox_subscribe", params).unwrap();
    assert_ne!(inbox.channel, 0);
    let msg: SessionSendResult = sender
        .call(
            "session.send",
            json!({
                "to_session_id": session, "from_session_id": session, "from_host_id": "test",
                "is_remote": false, "kind": "message", "data_b64": "aGk=",
            }),
        )
        .unwrap();
    let frame = read_frame(&mut receiver.stdout).unwrap().unwrap();
    assert_eq!(
        (frame.channel, frame.kind),
        (inbox.channel, FrameKind::Data)
    );
    assert_eq!(frame.payload.last(), Some(&b'\n'));
    let record: SessionMessage = serde_json::from_slice(&frame.payload).unwrap();
    assert_eq!(
        (record.message_id, record.to_session_id.as_str()),
        (msg.message_id, session)
    );
    let empty: SessionRecvResult = sender
        .call(
            "session.recv",
            json!({
                "session_id": session, "max_messages": 1, "wait_ms": 5,
            }),
        )
        .unwrap();
    assert!(empty.messages.is_empty());

    sender
        .call::<Value>("pty.kill", json!({"pty_id": child.pty_id}))
        .unwrap();
    let closed = read_frame(&mut receiver.stdout).unwrap().unwrap();
    assert_eq!(
        (closed.channel, closed.kind),
        (inbox.channel, FrameKind::Close)
    );
}
