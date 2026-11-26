// Each integration binary uses a different subset of the shared isolated harness.
#![allow(dead_code)]

use ddg_agents_proto::frame::{CONTROL_CHANNEL, Frame, FrameKind, read_frame, write_frame};
use ddg_agents_proto::holder as hp;
use ddg_agents_proto::host::{CatalogSubscribeResult, RoomCreateResult, SessionCreateResult};
use ddg_agents_proto::rpc::{Request, Response, RpcError};
use ddg_agents_proto::terminal::{HelloParams, PROTOCOL_VERSION};
use ddg_agents_vt::Vt;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TOKEN_MARKERS: &str = r#"printf 'TOKEN:%s\nSECRET:%s\nROOM:%s\n' "${DDG_SESSION_TOKEN%.*}" "${DDG_SESSION_TOKEN##*.}" "$DDG_ROOM_ID"; "#;

pub struct HostSandbox {
    pub root: PathBuf,
    pub state: PathBuf,
    pub run: PathBuf,
    pub config: PathBuf,
    pub space: PathBuf,
    host: Option<Child>,
    core: Option<Child>,
}

impl HostSandbox {
    pub fn new(name: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from(format!(
            "/tmp/ddg-{}-{name}-{:x}",
            std::process::id(),
            nanos
        ));
        let state = root.join("st");
        let run = root.join("rn");
        let config = root.join("cfg");
        let space = root.join("my-space");
        for path in [&state, &run, &config, &space] {
            std::fs::create_dir_all(path).unwrap();
        }
        Self {
            root,
            state,
            run,
            config,
            space,
            host: None,
            core: None,
        }
    }

    fn host_bin() -> &'static str {
        env!("CARGO_BIN_EXE_ddg-agents-host")
    }

    fn term_bin() -> PathBuf {
        Path::new(Self::host_bin())
            .parent()
            .unwrap()
            .join("ddg-terminal-server")
    }

    pub fn configure(&self, command: &mut Command) {
        command
            .env("DDG_STATE_DIR", &self.state)
            .env("DDG_RUN_DIR", &self.run)
            .env("DDG_CONFIG_DIR", &self.config)
            .env_remove("DDG_HUB_ENABLED")
            .env_remove("DDG_SESSION_TOKEN")
            .env("DDG_PLUGINS_DIR", self.root.join("plugins"))
            .env("DDG_TERMINAL_SERVER_BIN", Self::term_bin())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }

    pub fn start_host(&mut self) {
        self.start_host_as(false);
    }

    pub fn start_hub(&mut self) {
        self.start_host_as(true);
    }

    fn start_host_as(&mut self, hub: bool) {
        let mut command = Command::new(Self::host_bin());
        self.configure(command.arg("serve"));
        if hub {
            command.env("DDG_HUB_ENABLED", "1");
        }
        self.host = Some(command.spawn().unwrap());
        wait_for_socket(&self.run.join("host.sock"));
    }

    pub fn start_core(&mut self) {
        let mut command = Command::new(Self::term_bin());
        self.configure(command.arg("core"));
        self.core = Some(command.spawn().unwrap());
        wait_for_socket(&self.run.join("terminal.sock"));
    }

    pub fn wait_core_exit(&mut self) {
        assert!(self.core.take().unwrap().wait().unwrap().success());
    }

    pub fn crash_core(&mut self) {
        let mut core = self.core.take().unwrap();
        core.kill().unwrap();
        core.wait().unwrap();
    }

    pub fn stop_host(&mut self) {
        stop(&mut self.host);
        stop_replacement(&self.run.join("host.sock"));
    }

    pub fn stop_host_and_terminal(&mut self) {
        self.stop_host();
        stop(&mut self.core);
        stop_replacement(&self.run.join("terminal.sock"));
        if let Ok(mut holder) = UnixStream::connect(self.run.join("holder.sock")) {
            holder
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let _ = holder.write_all(&hp::encode_json(
                hp::HELLO,
                0,
                &hp::HolderHello {
                    version: hp::HOLDER_PROTO_VERSION,
                },
            ));
            let _ = holder.write_all(&hp::encode_json(
                hp::GOODBYE,
                0,
                &hp::HolderGoodbye::Shutdown,
            ));
            while let Ok(Some((kind, _, _))) = hp::read_frame(&mut holder) {
                if kind == hp::BYE {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(80));
        for name in [
            "host.sock",
            "terminal.sock",
            "holder.sock",
            "host-admin.sock",
            "terminal-admin.sock",
        ] {
            let _ = std::fs::remove_file(self.run.join(name));
        }
    }

    pub fn bridge(&self) -> HostClient {
        let mut command = Command::new(Self::host_bin());
        self.configure(command.arg("connect"));
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let writer = child.stdin.take().unwrap();
        let reader = child.stdout.take().unwrap();
        HostClient::new(Box::new(writer), Box::new(reader), Some(child))
    }

    pub fn connect(&self) -> HostClient {
        HostClient::socket(&self.run.join("host.sock"))
    }
    pub fn terminal(&self) -> HostClient {
        HostClient::socket(&self.run.join("terminal.sock"))
    }

    pub fn get_token(&self, id: &str) -> (String, String) {
        let mut term = self.terminal();
        eventually(|| {
            let result = term
                .call::<Value>(
                    "session.read_screen",
                    json!({"session_id": id, "lines": 24, "max_bytes": 8192}),
                )
                .ok()?;
            let plain = result["text"].as_str()?;
            let prefix = plain
                .lines()
                .find_map(|line| line.trim().strip_prefix("TOKEN:"))?;
            let secret = plain
                .lines()
                .find_map(|line| line.trim().strip_prefix("SECRET:"))?;
            let room = plain
                .lines()
                .find_map(|line| line.trim().strip_prefix("ROOM:"))?;
            (secret.len() == 64 && !room.is_empty())
                .then(|| (format!("{prefix}.{secret}"), room.to_owned()))
        })
    }
}

impl Drop for HostSandbox {
    fn drop(&mut self) {
        self.stop_host_and_terminal();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stop(proc: &mut Option<Child>) {
    if let Some(mut child) = proc.take() {
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        let _ = child.wait();
    }
}

fn stop_replacement(path: &Path) {
    if let Ok(stream) = UnixStream::connect(path)
        && let Ok(peer) = ddg_agents_os::ipc::peer_credentials(&stream)
        && peer.uid == unsafe { libc::geteuid() }
        && let Some(pid) = peer.pid.filter(|pid| *pid > 1)
    {
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        eventually(|| UnixStream::connect(path).is_err().then_some(()));
    }
}

pub fn plain_rpc(path: &Path, method: &str, params: Value) -> Response {
    let mut stream = UnixStream::connect(path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(16)))
        .unwrap();
    writeln!(stream, "{}", json!(Request::new(1, method, params))).unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn wait_for_socket(path: &Path) {
    eventually(|| UnixStream::connect(path).ok());
}

pub fn eventually<T>(mut check: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(6);
    loop {
        if let Some(result) = check() {
            return result;
        }
        assert!(Instant::now() < end, "timed out waiting for condition");
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub struct HostClient {
    child: Option<Child>,
    writer: Box<dyn Write>,
    reader: BufReader<Box<dyn Read>>,
    next_id: u64,
    pending: VecDeque<Frame>,
    pub hello: Value,
}

impl HostClient {
    fn new(mut writer: Box<dyn Write>, reader: Box<dyn Read>, child: Option<Child>) -> Self {
        let mut reader = BufReader::new(reader);
        let req = Request::new(
            1,
            "hello",
            json!(HelloParams {
                protocol_version: PROTOCOL_VERSION,
                framed: true,
                client_id: Some("host-test".into())
            }),
        );
        writeln!(writer, "{}", json!(req)).unwrap();
        writer.flush().unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let r: Response = serde_json::from_str(&line).unwrap();
        assert!(r.error.is_none());
        let hello = r.result.unwrap();
        assert_eq!(hello["protocol_version"], PROTOCOL_VERSION);
        Self {
            child,
            writer,
            reader,
            next_id: 2,
            pending: VecDeque::new(),
            hello,
        }
    }

    fn socket(path: &Path) -> Self {
        let reader = UnixStream::connect(path).unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(6)))
            .unwrap();
        let writer = reader.try_clone().unwrap();
        Self::new(Box::new(writer), Box::new(reader), None)
    }

    pub fn call<T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<T, RpcError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request::new(id, method, params);
        self.send(Frame::data(
            CONTROL_CHANNEL,
            serde_json::to_vec(&req).unwrap(),
        ));
        let frame =
            self.read_where(|frame| control_note(frame).is_some_and(|value| value["id"] == id));
        let resp: Response = serde_json::from_slice(&frame.payload).unwrap();
        match resp.error {
            Some(err) => Err(err),
            None => Ok(serde_json::from_value(resp.result.unwrap()).unwrap()),
        }
    }

    pub fn send(&mut self, frame: Frame) {
        write_frame(&mut self.writer, &frame).unwrap();
        self.writer.flush().unwrap();
    }

    pub fn read_where(&mut self, mut check: impl FnMut(&Frame) -> bool) -> Frame {
        if let Some(i) = self.pending.iter().position(&mut check) {
            return self.pending.remove(i).unwrap();
        }
        loop {
            let frame = read_frame(&mut self.reader)
                .unwrap()
                .expect("unexpected socket EOF");
            if check(&frame) {
                return frame;
            }
            self.pending.push_back(frame);
        }
    }

    pub fn send_input(&mut self, channel: u32, bytes: &[u8]) {
        self.send(Frame::data(channel, bytes.to_vec()));
    }

    pub fn read_until(&mut self, channel: u32, vt: &mut Vt, needle: &str) {
        while !vt.plain_text().contains(needle) {
            let frame = self.read_where(|f| f.channel == channel && f.kind == FrameKind::Data);
            vt.write(&frame.payload);
            self.send(Frame::credit(channel, frame.payload.len() as u32));
        }
    }

    pub fn catalog(&mut self) -> CatalogSubscribeResult {
        self.call("host.catalog_subscribe", json!({})).unwrap()
    }

    pub fn visible(&mut self) -> RoomCreateResult {
        self.call(
            "host.room_create",
            json!({"cols":80, "rows":24, "executor": executor("exec cat")}),
        )
        .unwrap()
    }

    pub fn hidden(&mut self, token: &str, script: &str) -> SessionCreateResult {
        self.call(
            "host.session_create",
            json!({"session_token": token, "executor": executor(script)}),
        )
        .unwrap()
    }
}

impl Drop for HostClient {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub fn control_note(frame: &Frame) -> Option<Value> {
    (frame.channel == CONTROL_CHANNEL && frame.kind == FrameKind::Data)
        .then(|| serde_json::from_slice(&frame.payload).unwrap())
}

pub fn executor(script: &str) -> Value {
    json!({"kind": "command", "argv": ["/bin/sh", "-c", format!("{TOKEN_MARKERS}{script}")]})
}

pub fn code(error: RpcError) -> String {
    error.data.code.unwrap()
}
