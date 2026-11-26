#![cfg(unix)]

mod common;

use common::{HostSandbox, code, eventually, plain_rpc};
use ddg_agents_os::lock::FileLock;
use ddg_agents_update::{ApplyOutcome, state::UpdateStateStore};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::Path,
    process::Command,
    time::Duration,
};

const HOST: &str = env!("CARGO_BIN_EXE_ddg-agents-host");

#[test]
fn detach_releases_core_lock_and_direct_reload_preserves_live_tokens() {
    let mut sb = HostSandbox::new("detach-reload");
    sb.start_core();
    sb.start_host();
    let old_host_pid = fs::read(sb.run.join("host.pid")).unwrap();
    let mut client = sb.connect();
    let live = client.visible().room.tabs[0].id.clone();
    let token = sb.get_token(&live).0;
    assert_eq!(sb.connect().hello["active_pty_count"], 1);
    let before = sb.terminal().hello.clone();
    let ptys = |sb: &HostSandbox| sb.terminal().call::<Value>("pty.list", json!({})).unwrap();
    let pid = ptys(&sb)["ptys"][0]["pid"].clone();
    let mut dead_params = json!({"cols":80,"rows":24,"executor": common::executor("exec cat")});
    let dead: Value = client.call("host.room_create", dead_params.take()).unwrap();
    let dead = dead["room"]["tabs"][0]["id"].as_str().unwrap();
    let dead_token = sb.get_token(dead).0;
    prep(&sb);
    let mut term = sb.terminal();
    let list = ptys(&sb);
    let pty_id = &list["ptys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["session_id"] == dead)
        .unwrap()["pty_id"];
    let _: Value = term.call("pty.kill", json!({"pty_id":pty_id})).unwrap();

    let terminal_admin = sb.run.join("terminal-admin.sock");
    let mut stream = UnixStream::connect(&terminal_admin).unwrap();
    assert_eq!(
        fs::metadata(terminal_admin).unwrap().permissions().mode() & 0o777,
        0o600
    );
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    writeln!(stream, "detach").unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&response).unwrap(),
        json!({"ok":true})
    );
    eventually(|| FileLock::try_lock_exclusive(&sb.state.join("terminal.lock")).ok());
    sb.wait_core_exit();
    sb.start_core();
    let after = sb.terminal().hello.clone();
    assert_eq!(after["holder_instance"], before["holder_instance"]);
    assert!(after["holder_instance"].is_string());
    assert_eq!(after["boot_generation"], before["boot_generation"]);
    assert_eq!(ptys(&sb)["ptys"][0]["pid"], pid);
    assert_eq!(after["active_pty_count"], 1);

    let host = Path::new(HOST).canonicalize().unwrap();
    let reply = plain_rpc(
        &sb.run.join("host-admin.sock"),
        "execute_reload",
        json!({"bin_path":host}),
    );
    assert_eq!(reply.result, Some(json!({"ok":true})), "{:?}", reply.error);
    assert_ne!(fs::read(sb.run.join("host.pid")).unwrap(), old_host_pid);
    assert!(
        sb.connect()
            .call::<Value>("host.session_list", json!({"session_token":token}))
            .is_ok()
    );
    let denied = sb
        .connect()
        .call::<Value>("host.session_list", json!({"session_token":dead_token}))
        .unwrap_err();
    assert_eq!(code(denied), "token_revoked");
}

#[test]
fn child_exit_after_bind_now_rebinds_old_host_and_resumes_events() {
    let mut sb = HostSandbox::new("reload-fail");
    sb.start_host();
    let pid = fs::read(sb.run.join("host.pid")).unwrap();
    let mut client = sb.connect();
    let live = client.visible().room.tabs[0].id.clone();
    let token = sb.get_token(&live).0;
    let dead = client.hidden(&token, "exec cat").session.session_id;
    let doomed_token = sb.get_token(&dead).0;
    let replacement = sb.root.join("ddg-agents-host");
    let reached_bind = sb.root.join("bind-now");
    fs::write(
        &replacement,
        format!(
            r#"#!/bin/sh
exec python3 -c '
import struct, sys
size = struct.unpack(">I", sys.stdin.buffer.read(4))[0]
sys.stdin.buffer.read(size)
print("tokens_loaded", flush=True)
assert sys.stdin.readline() == "bind_now\n"
open("{}", "w").close()
'
"#,
            reached_bind.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o700)).unwrap();
    prep(&sb);
    let list: Value = sb.terminal().call("pty.list", json!({})).unwrap();
    let id = &list["ptys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["session_id"] == dead)
        .unwrap()["pty_id"];
    let _: Value = sb
        .terminal()
        .call("pty.kill", json!({"pty_id":id}))
        .unwrap();
    let reply = plain_rpc(
        &sb.run.join("host-admin.sock"),
        "execute_reload",
        json!({"bin_path":replacement.canonicalize().unwrap()}),
    );
    assert!(reply.error.is_some());
    assert!(
        reached_bind.exists(),
        "fixture must fail after old listeners are dropped"
    );
    assert_eq!(fs::read(sb.run.join("host.pid")).unwrap(), pid);
    assert!(
        client
            .call::<Value>("host.session_list", json!({"session_token":token}))
            .is_ok()
    );
    eventually(|| {
        let result =
            client.call::<Value>("host.session_list", json!({"session_token":doomed_token}));
        result
            .err()
            .filter(|e| e.data.code.as_deref() == Some("token_revoked"))
    });
}

#[test]
fn daemon_swap_apply_rollback_and_failed_core_health_leave_tokens_live() {
    let mut sb = HostSandbox::new("daemon-swap");
    sb.start_core();
    sb.start_host();
    let mut client = sb.connect();
    let session = client.visible().room.tabs[0].id.clone();
    let token = sb.get_token(&session).0;
    let install = sb.root.join("install");
    let staging = sb.root.join("staging");
    for dir in [&install, &staging] {
        fs::create_dir(dir).unwrap();
    }
    let host = Path::new(HOST);
    let mut binaries = serde_json::Map::new();
    for (name, source) in [
        ("ddg-agents-host", host.to_owned()),
        (
            "ddg-terminal-server",
            host.with_file_name("ddg-terminal-server"),
        ),
    ] {
        for dir in [&install, &staging] {
            fs::copy(&source, dir.join(name)).unwrap();
        }
        let sha256 = format!("{:x}", Sha256::digest(fs::read(&source).unwrap()));
        binaries.insert(name.into(), json!({"path":name,"sha256":sha256}));
    }
    let ver = env!("CARGO_PKG_VERSION");
    let platform = sb.connect().hello["platform"].clone();
    let apply = |ver: &str| {
        let manifest = json!({"schema_version":1,"version":ver,"tag":format!("v{ver}"),"channel":"stable",
            "published_at":"","protocol_version":ddg_agents_proto::terminal::PROTOCOL_VERSION,
            "holder_protocol_version":ddg_agents_proto::holder::HOLDER_PROTO_VERSION,
            "holder_restart_required":false,"min_compatible_schema":1,"release_notes_summary":"",
            "artifacts":[{"platform":platform,"name":"fixture","sha256":"0".repeat(64),"binaries":binaries}]});
        let raw = serde_json::to_vec(&manifest).unwrap();
        fs::write(staging.join("manifest.json"), &raw).unwrap();
        let mut cmd = Command::new(HOST);
        sb.configure(&mut cmd);
        outcome(
            cmd.arg("__apply-daemon-swap")
                .arg("--install-dir")
                .arg(&install)
                .arg("--staging-dir")
                .arg(&staging)
                .arg("--version")
                .arg(format!("v{ver}"))
                .arg("--manifest-sha256")
                .arg(format!("{:x}", Sha256::digest(&raw))),
        )
    };
    assert!(matches!(
        apply(ver),
        ApplyOutcome::Completed {
            tokens_preserved: true,
            ..
        }
    ));
    assert_eq!(sb.connect().hello["rollback_version"], ver);
    let mut cmd = Command::new(install.join("ddg-agents-host"));
    sb.configure(&mut cmd);
    assert!(matches!(
        outcome(
            cmd.arg("__rollback-daemon-swap")
                .arg("--install-dir")
                .arg(&install)
        ),
        ApplyOutcome::Completed {
            tokens_preserved: true,
            ..
        }
    ));
    assert!(matches!(
        apply("999.0.0"),
        ApplyOutcome::AutoRolledBack { .. }
    ));
    let state = UpdateStateStore::new(&sb.state).load().unwrap();
    assert!(state.failed_version("999.0.0").is_some());
    assert!(state.in_progress.is_none());
    assert!(
        sb.connect()
            .call::<Value>("host.session_list", json!({"session_token":token}))
            .is_ok()
    );
}

fn prep(sb: &HostSandbox) {
    let response = plain_rpc(&sb.run.join("host-admin.sock"), "prepare_reload", json!({}));
    assert_eq!(
        response.result,
        Some(json!({"ok":true})),
        "{:?}",
        response.error
    );
}

fn outcome(cmd: &mut Command) -> ApplyOutcome {
    let result = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout.iter().filter(|&&b| b == b'\n').count(), 1);
    serde_json::from_slice(&result.stdout).unwrap()
}
