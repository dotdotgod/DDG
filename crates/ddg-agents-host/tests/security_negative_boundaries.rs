#![cfg(unix)]

mod common;

use common::{HostSandbox, code, eventually, executor, plain_rpc};
use ddg_agents_proto::frame::{Frame, FrameKind};
use ddg_agents_proto::host::RelayFrame;
use ddg_agents_proto::rpc::ErrorKind;
use ddg_agents_proto::terminal::CloseReason;
use ddg_agents_transport::{HostProfile, HostTransport, HostsConfig};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::{
    fs::PermissionsExt,
    net::{UnixListener, UnixStream},
};
use std::process::Command;
use std::time::Duration;

#[test]
fn pinning_tokens_hidden_pty_and_audit_boundaries_are_enforced() {
    let mut sb = HostSandbox::new("relay-sec");
    let trap_path = sb.run.join("never-dial.sock");
    let trap = UnixListener::bind(&trap_path).unwrap();
    trap.set_nonblocking(true).unwrap();
    sb.start_core();
    let self_id = sb.terminal().hello["server_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let profiles = [
        ("unpinned", None),
        ("empty", Some(String::new())),
        ("blank", Some(" \t ".into())),
        ("self", Some(self_id)),
    ];
    let config = HostsConfig {
        hosts: std::iter::once(HostProfile::local_default())
            .chain(profiles.into_iter().map(|(id, pin)| HostProfile {
                id: id.into(),
                label: id.into(),
                expected_host_id: pin,
                transport: HostTransport::Local {
                    sock_path: Some(trap_path.to_str().unwrap().into()),
                },
            }))
            .collect(),
        ..HostsConfig::default()
    };
    config.save(&sb.config.join("hosts.json")).unwrap();
    install_plugin(&sb);
    sb.start_hub();
    let mut admin = sb.connect();
    let a = admin.visible().room.tabs[0].id.clone();
    let token = sb.get_token(&a).0;
    let hidden = admin.hidden(&token, "exec cat");
    let b = hidden.session.session_id;
    let runtime = json!(hidden.session.runtime);
    let error = admin
        .call::<Value>(
            "host.session_list",
            json!({"session_token":token.replacen(&a, &b, 1)}),
        )
        .unwrap_err();
    assert_eq!(code(error), "token_invalid");
    for id in ["unpinned", "empty", "blank", "self"] {
        eventually(|| {
            let error = admin
                .call::<Value>(
                    "host.session_list",
                    json!({"session_token":token,"host_id":id}),
                )
                .unwrap_err();
            (error.data.code.as_deref() == Some("host_security_blocked")).then_some(())
        });
    }
    assert_eq!(
        trap.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(
        code(
            sb.connect()
                .call::<Value>("host.relay_attach", json!({}))
                .unwrap_err()
        ),
        "permission_denied"
    );
    for method in [
        "host.session_create",
        "host.session_close",
        "host.session_move",
        "host.session_restart",
        "host.session_write",
        "host.msg_send",
    ] {
        assert_eq!(
            code(
                admin
                    .call::<Value>(method, json!({"session_token":token,"host_id":" ALL "}))
                    .unwrap_err()
            ),
            "invalid_params"
        );
    }

    let denied = admin
        .call::<Value>(
            "ext.boundary.reverse",
            json!({"method":"pty.kill","params":{"pty_id":runtime["pty_id"]}}),
        )
        .unwrap_err();
    assert_eq!(code(denied), "permission_denied");
    let list: Value = admin
        .call("ext.boundary.reverse", json!({"method":"pty.list"}))
        .unwrap();
    assert_eq!(list["ptys"].as_array().unwrap().len(), 1);
    assert_eq!(list["ptys"][0]["session_id"], a);
    let secret = "c2VjcmV0LWJvZHk=";
    let _: Value = admin
        .call(
            "host.msg_send",
            json!({"session_token":token,"to_session_id":b,"data_b64":secret}),
        )
        .unwrap();
    let log = sb.state.join("host.log");
    assert_eq!(
        fs::metadata(&log).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let audit = fs::read_to_string(log).unwrap();
    assert!(audit.contains("msg_send") && !audit.contains(&token) && !audit.contains(secret));
}

#[test]
fn default_remote_rejects_bad_relay_frames_and_revokes_token_bearing_links() {
    let mut sb = HostSandbox::new("relay-rpc");
    sb.start_host();
    let sock = sb.run.join("host.sock");
    let mut plain = BufReader::new(UnixStream::connect(&sock).unwrap());
    plain
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    writeln!(
        plain.get_mut(),
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"host.relay_attach","params":{}})
    )
    .unwrap();
    let mut line = String::new();
    plain.read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["error"]["data"]["code"],
        "invalid_params"
    );

    let mut client = sb.connect();
    let attached: Value = client.call("host.relay_attach", json!({})).unwrap();
    let ch = attached["channel"].as_u64().unwrap() as u32;
    for (hop, method, expected) in [
        (2, "host.session_list", "invalid_params"),
        (1, "host.relay_exec", "permission_denied"),
        (1, "host.msg_recv", "permission_denied"),
    ] {
        let wire = json!({"type":"request", "hop":hop, "request_id":"a", "method":method, "from_host_id":"hub", "params":{}});
        client.send(Frame::data(ch, format!("{wire}\n").into_bytes()));
        let data = client.read_where(|f| f.channel == ch && f.kind == FrameKind::Data);
        client.send(Frame::credit(ch, data.payload.len() as u32));
        let RelayFrame::Error { request_id, error } =
            serde_json::from_slice(&data.payload).unwrap()
        else {
            panic!("expected a relay error")
        };
        assert_eq!(request_id, "a");
        assert_eq!(code(error), expected);
    }
    let err = client
        .call::<Value>("host.relay_exec", json!({}))
        .unwrap_err();
    assert_eq!(err.data.kind, ErrorKind::NotFound);
    assert_eq!(
        code(
            client
                .call::<Value>("host.session_list", json!({"session_token":"invalid"}))
                .unwrap_err()
        ),
        "token_invalid"
    );
    let closed = client.read_where(|f| f.channel == ch && f.kind == FrameKind::Close);
    assert_eq!(
        serde_json::from_slice::<CloseReason>(&closed.payload).unwrap(),
        CloseReason::Detached
    );
    assert_eq!(
        code(
            client
                .call::<Value>("host.relay_attach", json!({}))
                .unwrap_err()
        ),
        "permission_denied"
    );
    assert_eq!(
        code(
            sb.connect()
                .call::<Value>("host.relay_attach", json!({"session_token":null}))
                .unwrap_err()
        ),
        "permission_denied"
    );
}

#[test]
fn cli_errors_never_echo_token_arguments_and_require_environment() {
    let sb = HostSandbox::new("cli-sec");
    let run = |args: &[&str], token: Option<&str>| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ddg-agents-host"));
        cmd.args(args)
            .env_remove("DDG_SESSION_TOKEN")
            .env_remove("DDG_HOST_SOCK")
            .env("DDG_STATE_DIR", &sb.state)
            .env("DDG_RUN_DIR", &sb.run)
            .env("DDG_CONFIG_DIR", &sb.config);
        if let Some(token) = token {
            cmd.env("DDG_SESSION_TOKEN", token);
        }
        let output = cmd.output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        String::from_utf8(output.stderr).unwrap()
    };
    let err = run(&["host", "list", "--token=SECRET"], None);
    assert!(err.contains("invalid_params") && !err.contains("SECRET"));
    for (token, name) in [
        (None, "DDG_SESSION_TOKEN"),
        (Some("SECRET"), "DDG_HOST_SOCK"),
    ] {
        let err = run(&["host", "list"], token);
        assert!(err.contains("missing_env") && err.contains(name) && !err.contains("SECRET"));
    }
}

#[test]
fn update_admin_is_private_and_rejects_known_child_pids() {
    let mut sb = HostSandbox::new("admin-sec");
    install_plugin(&sb);
    sb.start_host();
    let path = sb.run.join("host-admin.sock");
    assert_eq!(
        fs::metadata(&sb.run).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let mut client = sb.connect();
    let session = client.visible().room.tabs[0].id.clone();
    let token = sb.get_token(&session).0;
    for name in [
        "prepare_reload",
        "abort_reload",
        "execute_reload",
        "update.apply",
        "host.update",
        "host.reload",
    ] {
        assert!(
            client.call::<Value>(name, json!({})).is_err(),
            "public socket accepted {name}"
        );
    }
    let plugin: Value = client
        .call("ext.boundary.admin", json!({"path":path}))
        .unwrap();
    assert_eq!(plugin["error"]["data"]["code"], "permission_denied");
    let script = sb.root.join("pty-admin.py");
    fs::write(
        &script,
        format!(
            r#"
import json, socket, time
s = socket.socket(socket.AF_UNIX)
s.connect({})
s.sendall(b'{{"jsonrpc":"2.0","id":1,"method":"prepare_reload","params":{{}}}}\n')
print(json.loads(s.makefile().readline())["error"]["data"]["code"], flush=True)
time.sleep(30)
"#,
            json!(path)
        ),
    )
    .unwrap();
    let pty: Value = client.call("host.room_create", json!({"cols":80, "rows":24,"executor":executor(&format!("exec python3 {}", script.display()))})).unwrap();
    let id = &pty["room"]["tabs"][0]["id"];
    eventually(|| {
        let screen: Value = sb
            .terminal()
            .call(
                "session.read_screen",
                json!({"session_id": id, "lines":24,"max_bytes":8192}),
            )
            .ok()?;
        screen["text"]
            .as_str()?
            .contains("permission_denied")
            .then_some(())
    });
    for method in [
        "prepare_reload",
        "token_snapshot",
        "abort_reload",
        "execute_reload",
    ] {
        let response = plain_rpc(&path, method, json!({}));
        let wire = json!(response).to_string();
        assert!(!wire.contains(&token) && !wire.contains("revoked") && !wire.contains("active"));
        if ["prepare_reload", "abort_reload"].contains(&method) {
            assert_eq!(response.result.unwrap(), json!({"ok":true}));
        } else {
            assert!(response.error.is_some());
        }
    }
    let mismatch = plain_rpc(
        &sb.run.join("host.sock"),
        "hello",
        json!({"framed":false,"protocol_version":999}),
    )
    .error
    .unwrap();
    assert_eq!(mismatch.data.code.as_deref(), Some("protocol_incompatible"));
    let hint: Value = serde_json::from_str(mismatch.data.hint.as_deref().unwrap()).unwrap();
    assert_eq!(
        hint["remote_protocol"],
        ddg_agents_proto::terminal::PROTOCOL_VERSION
    );
    assert_eq!(hint["remote_version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn version_does_not_touch_runtime() {
    let sandbox = HostSandbox::new("version");
    let host = std::path::Path::new(env!("CARGO_BIN_EXE_ddg-agents-host"));
    for bin in [host.to_owned(), host.with_file_name("ddg-terminal-server")] {
        let runtime = sandbox.root.join("must-not-be-created");
        let output = Command::new(&bin)
            .arg("--version")
            .env("DDG_STATE_DIR", &runtime)
            .env("DDG_RUN_DIR", &runtime)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "{} {}\n",
                bin.file_name().unwrap().to_str().unwrap(),
                env!("CARGO_PKG_VERSION")
            )
        );
        assert!(!runtime.exists());
    }
    let restore = Command::new(host)
        .args(["serve", "--restore-tokens-pipe"])
        .env("DDG_RUN_DIR", sandbox.root.join("invalid-restore"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .output()
        .unwrap();
    assert!(!restore.status.success());
    assert!(restore.stdout.is_empty());
    assert!(!sandbox.root.join("invalid-restore").exists());
}

fn install_plugin(sb: &HostSandbox) {
    let dir = sb.root.join("plugins/boundary");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("plugin.toml"),
        r#"
name = "boundary"
version = "0.1.0"
command = ["python3", "./boundary.py"]
capabilities = ["pty_observe", "pty_control"]
"#,
    )
    .unwrap();
    fs::write(
        dir.join("boundary.py"),
        r#"
import json, socket, sys

def send(record):
    print(json.dumps(record), flush=True)

for line in sys.stdin:
    call = json.loads(line)
    if call["method"] == "plugin.hello":
        result = {"result": {"name": "boundary", "version": "0.1.0", "methods": [
            {"name": "ext.boundary.reverse", "description": "Test reverse RPC"},
            {"name": "ext.boundary.admin", "description": "Test admin boundary"}], "events": []}}
    elif call["method"] == "ext.boundary.admin":
        with socket.socket(socket.AF_UNIX) as sock:
            sock.connect(call["params"]["path"])
            sock.sendall(b'{"jsonrpc":"2.0","id":1,"method":"prepare_reload","params":{}}\n')
            result = {"result": json.loads(sock.makefile().readline())}
    else:
        send({"jsonrpc": "2.0", "id": 99, **call["params"]})
        result = json.loads(sys.stdin.readline())
    send({**result, "jsonrpc": "2.0", "id": call["id"]})
"#,
    )
    .unwrap();
}
