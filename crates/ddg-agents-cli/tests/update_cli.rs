use ddg_agents_transport::{HostProfile, HostTransport, HostsConfig};
use ddg_agents_update::state::{PendingHolderRestart, UpdateStateStore};
use serde_json::Value;
use std::{
    fs,
    net::TcpListener,
    path::Path,
    process::{Command, Output, Stdio},
};

const DDG: &str = env!("CARGO_BIN_EXE_ddg");

fn command(root: &Path) -> Command {
    let mut cmd = Command::new(DDG);
    cmd.env("HOME", root)
        .env("DDG_CONFIG_DIR", root.join("config"))
        .env("DDG_STATE_DIR", root.join("state"))
        .env("DDG_RUN_DIR", root.join("run"))
        .env("DDG_DIST_DIR", root.join("dist"))
        .env_remove("DDG_UPDATE_REPO")
        .stdin(Stdio::null());
    cmd
}

fn result(output: Output, code: Option<&str>) -> Value {
    assert_eq!(
        output.status.code(),
        Some(i32::from(code.is_some())),
        "{output:?}"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1);
    assert!(stdout.ends_with('\n'));
    let value: Value = serde_json::from_str(&stdout).unwrap();
    assert!(value.is_object());
    if let Some(code) = code {
        assert_eq!(value["error"]["code"], code);
    }
    value
}

fn update(root: &Path, args: &[&str], code: Option<&str>) -> Value {
    result(
        command(root)
            .args(["--json", "update"])
            .args(args)
            .output()
            .unwrap(),
        code,
    )
}

#[test]
fn local_target_config_consent_and_container_policy() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path();
    let hosts = HostsConfig {
        default_host_id: "prod".into(),
        hosts: vec![
            HostProfile::local_default(),
            HostProfile {
                id: "prod".into(),
                label: "unreachable container by design".into(),
                expected_host_id: None,
                transport: HostTransport::DockerExec {
                    container: "prod".into(),
                    ssh_host: None,
                    known_hosts_file: None,
                    identity_file: None,
                },
            },
        ],
        ..HostsConfig::default()
    };
    fs::create_dir(path.join("config")).unwrap();
    fs::write(
        path.join("config/hosts.json"),
        serde_json::to_vec(&hosts).unwrap(),
    )
    .unwrap();
    let config = update(
        path,
        &["config", "--auto-check", "off", "--repo", "owner/repo"],
        None,
    );
    assert_eq!(config["source"]["custom"], true);
    assert_eq!(config["config"]["auto_check"], "off");
    let status = update(path, &["status"], None);
    assert_eq!(status["host"], "local");
    assert!(status["last_checked_at"].is_null());
    assert_eq!(status["source"]["repo"], "owner/repo");
    assert!(
        String::from_utf8_lossy(
            &command(path)
                .args(["update", "status"])
                .output()
                .unwrap()
                .stdout
        )
        .contains("[커스텀 소스]")
    );
    update(path, &["config", "--clear-repo"], None);
    for sub in ["apply", "rollback"] {
        update(path, &[sub, "-H", "all"], Some("invalid_params"));
        let blocked = update(
            path,
            &[sub, "-H", "prod", "--yes"],
            Some("container_update_unsupported"),
        );
        assert_eq!(
            blocked["error"]["message"]
                .as_str()
                .unwrap()
                .lines()
                .count(),
            1
        );
        update(path, &[sub], Some("confirmation_required"));
    }
    update(path, &["config", "-H", "local"], Some("invalid_params"));
    UpdateStateStore::new(path.join("state"))
        .update(|state| {
            state.pending_holder_restart = Some(PendingHolderRestart {
                to_version: "1.2.3".into(),
                active_pty_count: 2,
            });
            Ok(())
        })
        .unwrap();
    for sub in ["apply", "rollback"] {
        update(path, &[sub, "--yes"], Some("holder_restart_required"));
    }
    update(
        path,
        &[
            "apply",
            "--yes",
            "--restart-sessions",
            "--version",
            "not-semver",
        ],
        Some("manifest_invalid"),
    );
    update(
        path,
        &["rollback", "--yes", "--restart-sessions"],
        Some("rollback_unavailable"),
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_helper_consumes_a_bad_job_without_touching_installations() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let staging = state.join("staging");
    ddg_agents_os::fs::ensure_private_dir(&state).unwrap();
    ddg_agents_os::fs::ensure_private_dir(&staging).unwrap();
    let job = state.join("job.json");
    ddg_agents_os::fs::write_atomic(&job, b"not JSON").unwrap();
    let output = command(root.path())
        .args(["--json", "__update-apply-helper", "--job"])
        .arg(&job)
        .arg("--state-dir")
        .arg(&state)
        .output()
        .unwrap();
    result(output, Some("json_error"));
    assert!(!job.exists());
    assert!(!state.join("rollback").exists());
}

#[test]
fn token_is_never_echoed_and_other_commands_do_not_check_for_updates() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path();
    let trap = TcpListener::bind("127.0.0.1:0").unwrap();
    trap.set_nonblocking(true).unwrap();
    let proxy = format!("http://{}", trap.local_addr().unwrap());
    let launch = |argv: &[&str]| {
        command(path)
            .args(argv)
            .env("HTTPS_PROXY", &proxy)
            .env("https_proxy", &proxy)
            .env("NO_PROXY", "")
            .env("no_proxy", "")
            .env("DDG_UPDATE_REPO", "owner/repo")
            .output()
            .unwrap()
    };
    let version = launch(&["--version"]);
    assert!(version.status.success());
    assert_eq!(
        version.stdout,
        format!("ddg {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
    );
    assert!(version.stderr.is_empty());
    for cmd in [["machine", "ls"], ["agent", "ls"]] {
        let output = launch(&["--json", cmd[0], cmd[1]]);
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("update"));
    }
    assert!(!path.join("state/update-state.json").exists());
    assert_eq!(
        trap.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    for args in [
        &["--token=NEVER_ECHO_ME", "update", "status", "--json"][..],
        &["update", "--json", "status", "--token", "NEVER_ECHO_ME"][..],
    ] {
        let output = launch(args);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("NEVER_ECHO_ME"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("NEVER_ECHO_ME"));
        result(output, Some("invalid_params"));
    }
}
