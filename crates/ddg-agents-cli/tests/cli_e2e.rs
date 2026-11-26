#![cfg(unix)]

use ddg_agents_proto::{holder, id::encode_base64};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DDG: &str = env!("CARGO_BIN_EXE_ddg");

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Sandbox {
    root: PathBuf,
    host: Option<Process>,
    core: Option<Process>,
}

impl Sandbox {
    fn new() -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self {
            root: PathBuf::from(format!("/tmp/ddg-cli-{}-{now:x}", std::process::id())),
            host: None,
            core: None,
        }
    }

    fn start() -> Self {
        let mut sandbox = Self::new();
        let term = sibling("ddg-terminal-server");
        let host = sibling("ddg-agents-host");
        sandbox.core = Some(Process(sandbox.command(term).arg("core").spawn().unwrap()));
        eventually(|| UnixStream::connect(sandbox.root.join("run/terminal.sock")).ok());
        sandbox.host = Some(Process(sandbox.command(host).arg("serve").spawn().unwrap()));
        eventually(|| UnixStream::connect(sandbox.root.join("run/host.sock")).ok());
        sandbox
    }

    fn command(&self, bin: impl AsRef<Path>) -> Command {
        let mut cmd = Command::new(bin.as_ref());
        cmd.env("DDG_STATE_DIR", self.root.join("state"))
            .env("DDG_RUN_DIR", self.root.join("run"))
            .env("DDG_CONFIG_DIR", self.root.join("config"))
            .env("DDG_PLUGINS_DIR", self.root.join("plugins"))
            .env("DDG_TERMINAL_SERVER_BIN", sibling("ddg-terminal-server"))
            .env("DDG_AGENTS_HOST_BIN", sibling("ddg-agents-host"))
            .env("HOME", &self.root)
            .env("SHELL", "/bin/sh")
            .env_remove("DDG_HUB_ENABLED")
            .env_remove("DDG_SESSION_TOKEN")
            .env_remove("DDG_HOST_SOCK")
            .stdin(Stdio::null());
        cmd
    }

    fn cli(&self, argv: &[&str]) -> Output {
        self.command(DDG).args(argv).output().unwrap()
    }

    fn checked(&self, argv: &[&str]) -> String {
        let out = self.cli(argv);
        assert!(
            out.status.success(),
            "{argv:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.stderr.is_empty(),
            "{argv:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn json(&self, argv: &[&str]) -> Value {
        let mut cmd = argv.to_vec();
        let before = cmd.iter().position(|arg| *arg == "--").unwrap_or(cmd.len());
        cmd.insert(before, "--json");
        serde_json::from_str(&self.checked(&cmd)).unwrap()
    }

    fn background(&self, script: &str, args: &[&str]) -> Value {
        let mut argv = vec![
            "session",
            "create",
            "--background",
            "--",
            "/bin/sh",
            "-c",
            script,
            "ddg-test",
        ];
        argv.extend(args);
        self.json(&argv)
    }

    fn start_cli(&self, argv: &[&str]) -> Process {
        Process(
            self.command(DDG)
                .args(argv)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        )
    }

    fn error(&self, argv: &[&str], code: &str) -> String {
        let mut cmd = argv.to_vec();
        cmd.insert(0, "--json");
        let out = self.cli(&cmd);
        assert!(!out.status.success(), "{cmd:?}");
        assert!(out.stdout.is_empty());
        let value: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(value["error"]["code"], code, "{cmd:?}: {value}");
        value["error"]["message"].as_str().unwrap().into()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        drop(self.host.take());
        drop(self.core.take());
        if let Ok(mut socket) = UnixStream::connect(self.root.join("run/holder.sock")) {
            let _ = socket.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = socket.write_all(&holder::encode_json(
                holder::HELLO,
                0,
                &holder::HolderHello {
                    version: holder::HOLDER_PROTO_VERSION,
                },
            ));
            let _ = socket.write_all(&holder::encode_json(
                holder::GOODBYE,
                0,
                &holder::HolderGoodbye::Shutdown,
            ));
            while let Ok(Some((kind, _, _))) = holder::read_frame(&mut socket) {
                if kind == holder::BYE {
                    break;
                }
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn sibling(name: &str) -> PathBuf {
    let path = Path::new(DDG).with_file_name(name);
    assert!(
        path.is_file(),
        "{} not built; run cargo test --workspace",
        path.display()
    );
    path
}

fn eventually<T>(mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if let Some(result) = check() {
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for test condition"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn finish(mut process: Process) -> Output {
    let status = eventually(|| process.0.try_wait().unwrap());
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    process
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    process
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    Output {
        status,
        stdout,
        stderr,
    }
}

fn termios(slave: &File) -> libc::termios {
    let mut attributes = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut attributes) },
        0
    );
    attributes
}

fn attach_and_detach(sb: &Sandbox, id: &str) {
    let (mut master, mut slave) = (-1, -1);
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let (mut master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    for fd in [&master, &slave] {
        assert_eq!(
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
    }
    let before = termios(&slave);
    let flags = unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_GETFL) };
    assert_ne!(before.c_lflag & libc::ICANON, 0);
    let mut child = Process(
        sb.command(DDG)
            .args(["session", "attach", id, "--json"])
            .stdin(slave.try_clone().unwrap())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    eventually(|| (termios(&slave).c_lflag & libc::ICANON == 0).then_some(()));
    let mut stdout = child.0.stdout.take().unwrap();
    let mut poll = libc::pollfd {
        fd: stdout.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut poll, 1, 6000) }, 1);
    master.write_all(b"ATTACH_INPUT\r\x1d").unwrap();
    let status = eventually(|| child.0.try_wait().unwrap());
    assert!(status.success());
    let mut stream = String::new();
    stdout.read_to_string(&mut stream).unwrap();
    let frame: Value = serde_json::from_str(stream.lines().next().unwrap()).unwrap();
    assert_eq!(frame["session_id"], id);
    assert!(!frame["data_b64"].as_str().unwrap().is_empty());
    let after = termios(&slave);
    // BSD may set the kernel-owned PENDIN bit when canonical mode is restored.
    let user_flags = |t: libc::termios| (t.c_iflag, t.c_oflag, t.c_lflag & !libc::PENDIN, t.c_cc);
    assert_eq!(user_flags(after), user_flags(before));
    assert_eq!(
        unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_GETFL) },
        flags
    );
}

#[test]
fn skill_prints_and_installs_without_host_or_config() -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let sb = Sandbox::new();
    let skill = include_str!("../src/SKILL.md");
    let version = env!("CARGO_PKG_VERSION");
    for flag in ["--skill", "--skills", "skill"] {
        assert_eq!(sb.checked(&[flag]), skill);
    }
    assert_eq!(
        serde_json::from_str::<Value>(&sb.checked(&["--skill", "--json"]))?,
        json!({"skill": skill, "version": version})
    );
    assert!(
        !sb.error(&["--token=secret", "--skill"], "invalid_params")
            .contains("secret")
    );

    let dir = sb.root.join("custom/agent skill");
    let path = dir.join("SKILL.md");
    let args = ["skill", "install", "--dir", dir.to_str().unwrap()];
    assert_eq!(sb.checked(&args), format!("installed {}\n", path.display()));
    assert_eq!(fs::read_to_string(&path)?, skill);

    fs::write(&path, "old skill")?;
    let mut old = File::open(&path)?;
    assert_eq!(
        sb.json(&args),
        json!({"path": path, "bytes": skill.len(), "version": version})
    );
    assert_eq!(fs::read_to_string(&path)?, skill);
    assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o644);
    assert_eq!(fs::read_dir(&dir)?.count(), 1);
    let mut replaced = String::new();
    old.read_to_string(&mut replaced)?;
    assert_eq!(
        replaced, "old skill",
        "install should replace, not truncate"
    );
    sb.checked(&["skill", "install"]);
    assert_eq!(
        fs::read_to_string(sb.root.join(".agents/skills/ddg/SKILL.md"))?,
        skill
    );
    assert!(!sb.root.join("config").exists());
    assert!(!sb.root.join("run").exists());
    Ok(())
}

#[test]
fn operator_workflow_uses_the_local_host_end_to_end() -> io::Result<()> {
    let sb = Sandbox::start();
    let list = sb.json(&["machine", "ls"]);
    assert_eq!(list[0]["id"], "local");
    assert_eq!(list[0]["default"], true);
    let host = sb.json(&["machine", "test", "local"]);
    let host_id = host["host_id"].as_str().unwrap();
    assert!(!host_id.is_empty());
    assert_eq!(sb.json(&["m", "ls", "--probe"])[0]["host_id"], host_id);

    let room = sb.json(&["room", "create", "--name", "dev"]);
    let room_id = room["room"]["id"].as_str().unwrap();
    let tab = sb.json(&[
        "session",
        "create",
        "--room",
        room_id,
        "--title",
        "worker",
        "--",
        "/bin/sh",
        "-c",
        "stty -echo; printf 'READY\\n'; exec cat",
    ]);
    let id = tab["tab"]["id"].as_str().unwrap();
    assert_eq!(tab["tab"]["title"], "worker");
    eventually(|| {
        let screen = sb.json(&["session", "read", id]);
        screen["text"]
            .as_str()
            .unwrap()
            .contains("READY")
            .then_some(())
    });
    attach_and_detach(&sb, id);
    eventually(|| {
        sb.checked(&["session", "read", id])
            .contains("ATTACH_INPUT")
            .then_some(())
    });
    assert_eq!(
        sb.json(&["session", "send", id, "HELLO_CLI", "--enter"])["byte_len"],
        10
    );
    eventually(|| {
        sb.checked(&["session", "read", id])
            .contains("HELLO_CLI")
            .then_some(())
    });
    sb.error(
        &["s", "send", id, "discard", "--expect-gen", "2"],
        "generation_mismatch",
    );

    let split = sb.json(&[
        "session",
        "split",
        id,
        "--vertical",
        "--ratio",
        "35",
        "--title",
        "pane",
        "-H",
        "local",
        "--",
        "/bin/cat",
    ]);
    let pane = split["tab"]["id"].as_str().unwrap();
    assert_eq!(split["tab"]["title"], "pane");
    assert_eq!(split["room"]["id"], room_id);
    assert_eq!(
        split["room"]["layout"]["groups"][1],
        json!({
            "kind": "split", "axis": "vertical", "ratio_bp": 3500,
            "first": {"kind": "leaf", "tab_id": id},
            "second": {"kind": "leaf", "tab_id": pane},
        })
    );
    let plain = sb.checked(&[
        "s", "split", pane, "--cols", "80", "--rows", "24", "--", "/bin/cat",
    ]);
    let (new_id, new_room) = plain.strip_suffix('\n').unwrap().split_once('\t').unwrap();
    assert!(new_id.starts_with("ses_"));
    assert_eq!(new_room, room_id);
    sb.checked(&["session", "close", new_id]);
    sb.checked(&["session", "close", pane]);

    let bg = sb.json(&[
        "session",
        "create",
        "--background",
        "--title",
        "bgjob",
        "--",
        "/bin/sh",
        "-c",
        "exec sleep 30",
    ]);
    let bg_id = bg["tab"]["id"].as_str().unwrap();
    assert!(
        !sb.json(&["session", "ls"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["session_id"] == bg_id)
    );
    let hidden = sb.json(&["session", "ls", "--include-hidden"]);
    let hidden = hidden["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == bg_id)
        .unwrap();
    assert_eq!(hidden["title"], "bgjob");
    let hidden_room_id = hidden["room_id"].as_str().unwrap();
    assert!(hidden_room_id.starts_with("rm_bg_"));
    sb.error(
        &["room", "close", hidden_room_id, "--yes"],
        "permission_denied",
    );
    let moved = sb.json(&["session", "move", bg_id, "--new-room", "promoted"]);
    assert_eq!(moved["session"]["session_id"], bg_id);
    assert_eq!(moved["session"]["visibility"], "visible");
    let renamed = sb.json(&["room", "rename", room_id, "dev-renamed"]);
    assert_eq!(renamed["room"]["name"], "dev-renamed");
    sb.error(&["room", "close", room_id], "invalid_params");
    let closed = sb.json(&["room", "close", room_id, "--yes"]);
    assert_eq!(closed["closed"].as_array().unwrap().len(), 2);
    assert_eq!(sb.json(&["session", "close", bg_id])["removed_room"], true);
    let status = sb.json(&["status"]);
    assert_eq!(status["hosts"][0]["host_id"], host_id);
    assert_eq!(status["hosts"][0]["room_count"], 0);
    assert_eq!(status["errors"], json!([]));

    // One failing local Unix socket must not hide the reachable host or trigger SSH.
    let config = sb.root.join("test-hosts.json");
    fs::write(
        &config,
        serde_json::to_vec(
            &json!({"schema_version": 1, "default_host_id": "offline", "hosts": [
                {"id": "local", "label": "Local", "transport": {"kind": "local"}},
                {"id": "offline", "label": "Offline", "transport": {"kind": "local", "sock_path": sb.root.join("missing.sock")}},
            ]}),
        )?,
    )?;
    let path = config.to_str().unwrap();
    for group in ["room", "session"] {
        let all = sb.json(&[group, "ls", "--config", path, "-H", "all"]);
        assert_eq!(all["errors"][0]["host"], "offline");
        sb.error(&[group, "ls", "--config", path], "unavailable");
    }
    let all = sb.json(&["--config", path, "ls"]);
    assert_eq!(all["hosts"][0]["host"], "local");
    assert_eq!(all["errors"][0]["host"], "offline");
    assert!(
        !sb.error(&["ls", "--token=should-not-echo"], "invalid_params")
            .contains("should-not-echo")
    );
    Ok(())
}

#[test]
fn background_wait_and_operator_inbox_delivery() {
    let sb = Sandbox::start();
    assert!(
        sb.error(&["msg", "recv"], "missing_env")
            .contains("DDG_SESSION_TOKEN or DDG_HOST_SOCK")
    );
    sb.error(&["msg", "subscribe"], "missing_env");
    let inbox = sb.root.join("inbox.json");
    let tab = sb.background(
        r#"stty -echo; printf 'BOOT\n'; read go; sleep 0.2
           printf '\033[32mRE\033[0m'; sleep 0.15; printf '\033[32mADY\033[0m\n'
           "$1" msg recv --wait-ms 5000 --json > "$2" && printf 'INBOX_DONE\n'
           read stop; sleep 0.4; printf '\033[35mFINAL\033[0m\n'; exit 7"#,
        &[DDG, inbox.to_str().unwrap()],
    );
    let id = tab["tab"]["id"].as_str().unwrap();
    sb.error(&["session", "wait", id, "--or-message"], "missing_env");
    assert_eq!(
        sb.json(&["session", "wait", id, "--output", "BOOT", "--timeout", "5"])["reason"],
        "output"
    );
    let ready = sb.start_cli(&[
        "s",
        "wait",
        id,
        "--output",
        "READY",
        "--timeout",
        "5",
        "--json",
    ]);
    sb.json(&["session", "send", id, "go", "--enter"]);
    let ready = finish(ready);
    assert!(
        ready.status.success(),
        "{}",
        String::from_utf8_lossy(&ready.stderr)
    );
    let result: Value = serde_json::from_slice(&ready.stdout).unwrap();
    assert_eq!(result["reason"], "output");
    assert_eq!(result["matched"], true);
    assert_eq!(result["run_generation"], 1);

    let sent = sb.json(&["msg", "send", id, "ping-from-op"]);
    let done = sb.json(&[
        "session",
        "wait",
        id,
        "--output",
        "INBOX_DONE",
        "--timeout",
        "5",
    ]);
    assert_eq!(done["reason"], "output");
    let received: Value = serde_json::from_slice(&fs::read(inbox).unwrap()).unwrap();
    let message = &received.as_array().unwrap()[0];
    assert_eq!(message["from_session_id"], "operator");
    assert_eq!(
        message["from_host_id"],
        sb.json(&["machine", "test", "local"])["host_id"]
    );
    assert_eq!(message["is_remote"], false);
    assert_eq!(message["message_id"], sent["message_id"]);
    assert_eq!(message["data_b64"], encode_base64(b"ping-from-op"));

    let timeout = sb.json(&["session", "wait", id, "--timeout", "1"]);
    assert_eq!(timeout["reason"], "timeout");
    assert_eq!(timeout["state"], "running");
    let exit = sb.start_cli(&["session", "wait", id, "--json"]);
    let missing_output =
        sb.start_cli(&["session", "wait", id, "--output", "NEVER_PRINTED", "--json"]);
    let tail = sb.start_cli(&["session", "wait", id, "--output", "FINAL", "--json"]);
    sb.json(&["session", "send", id, "stop", "--enter"]);
    let tail = finish(tail);
    assert!(
        tail.status.success(),
        "{}",
        String::from_utf8_lossy(&tail.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&tail.stdout).unwrap()["reason"],
        "output"
    );
    let exit = finish(exit);
    assert!(
        exit.status.success(),
        "{}",
        String::from_utf8_lossy(&exit.stderr)
    );
    let result: Value = serde_json::from_slice(&exit.stdout).unwrap();
    assert_eq!(result["reason"], "exited");
    assert_eq!(result["exit_code"], 7);
    let missing_output = finish(missing_output);
    assert!(!missing_output.status.success());
    assert!(missing_output.stdout.is_empty());
    let result: Value = serde_json::from_slice(&missing_output.stderr).unwrap();
    assert_eq!(result["error"]["code"], "session_exited");

    let close = sb.background("read line", &[]);
    let id = close["tab"]["id"].as_str().unwrap();
    assert_eq!(sb.json(&["session", "close", id])["removed_room"], false);
    sb.error(&["session", "wait", id], "session_not_found");
}

#[test]
fn visible_wait_observes_catalog_exit_and_reports_already_exited() {
    let sb = Sandbox::start();
    let result = sb.json(&[
        "room",
        "create",
        "--",
        "/bin/sh",
        "-c",
        "stty -echo; read stop; sleep 0.4; exit 3",
    ]);
    let id = result["room"]["tabs"][0]["id"].as_str().unwrap();
    let wait = sb.start_cli(&["session", "wait", id, "--json"]);
    sb.json(&["session", "send", id, "stop", "--enter"]);
    let wait = finish(wait);
    assert!(
        wait.status.success(),
        "{}",
        String::from_utf8_lossy(&wait.stderr)
    );
    let first: Value = serde_json::from_slice(&wait.stdout).unwrap();
    assert_eq!(first["reason"], "exited");
    assert_eq!(first["state"], "exited");
    assert!(first["exit_status"].is_string());
    assert_eq!(sb.json(&["session", "wait", id, "--timeout", "1"]), first);
    sb.error(
        &["session", "wait", id, "--output", "NEVER_PRINTED"],
        "session_exited",
    );
}

#[test]
fn session_sender_wait_message_and_inbox_subscription() {
    let sb = Sandbox::start();
    let target = sb.background("read line", &[]);
    let target = target["tab"]["id"].as_str().unwrap();
    let config = sb.root.join("hosts.json");
    fs::write(
        &config,
        serde_json::to_vec(&json!({"schema_version": 1, "default_host_id": "local", "hosts": [
            {"id": "local", "label": "Local", "transport": {"kind": "local", "sock_path": sb.root.join("run/host.sock")}}
        ]})).unwrap(),
    ).unwrap();
    let caller = sb.background(
        r#"set -e; stty -echo; export DDG_CONFIG_DIR="$3"
           "$1" inbox send --to "$DDG_SESSION_ID" from-session
           printf profile-host | "$1" msg send -H local --to "$DDG_SESSION_ID" --stdin
           "$1" msg send --config "$3/hosts.json" "$DDG_SESSION_ID" profile-config
           "$1" session wait "$2" --output NEVER --or-message --timeout 5 --json > "$3/wait.json"
           "$1" msg recv --json > "$3/rest.json"
           "$1" msg subscribe --json > "$3/stream.json" &
           subscriber=$!; printf 'SUBSCRIBING\n'
           read stop
           kill -TERM "$subscriber"; wait "$subscriber"; printf 'SIGNAL_HANDLED\n'
           read exit"#,
        &[DDG, target, sb.root.to_str().unwrap()],
    );
    let caller = caller["tab"]["id"].as_str().unwrap();
    // Wait until the regular recv has finished, so it cannot claim the stream message.
    let subscribing = sb.json(&[
        "session",
        "wait",
        caller,
        "--output",
        "SUBSCRIBING",
        "--timeout",
        "5",
    ]);
    assert_eq!(subscribing["reason"], "output");
    let sent = sb.json(&["msg", "send", caller, "for-subscriber"]);
    let stream = eventually(|| {
        let bytes = fs::read(sb.root.join("stream.json")).ok()?;
        serde_json::from_slice::<Value>(&bytes).ok()
    });
    assert_eq!(stream["message_id"], sent["message_id"]);
    assert_eq!(stream["data_b64"], encode_base64(b"for-subscriber"));

    let first: Value =
        serde_json::from_slice(&fs::read(sb.root.join("wait.json")).unwrap()).unwrap();
    assert_eq!(first["reason"], "message");
    assert_eq!(first["matched"], false);
    assert_eq!(first["message"]["from_session_id"], caller);
    assert_eq!(first["message"]["data_b64"], encode_base64(b"from-session"));
    let rest: Value =
        serde_json::from_slice(&fs::read(sb.root.join("rest.json")).unwrap()).unwrap();
    let rest = rest.as_array().unwrap();
    assert_eq!(rest.len(), 2);
    for (message, body) in rest.iter().zip(["profile-host", "profile-config"]) {
        assert_eq!(message["from_session_id"], "operator");
        assert_eq!(message["data_b64"], encode_base64(body.as_bytes()));
    }
    sb.json(&["session", "send", caller, "stop", "--enter"]);
    let handled = sb.json(&[
        "session",
        "wait",
        caller,
        "--output",
        "SIGNAL_HANDLED",
        "--timeout",
        "5",
    ]);
    assert_eq!(handled["reason"], "output");
    sb.json(&["session", "close", caller]);
    sb.json(&["session", "close", target]);
}
