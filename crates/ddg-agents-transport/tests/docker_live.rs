//! Live end-to-end Docker integration test (run with `--ignored --nocapture`
//! after building `ddg-agents-host:test` from `docker/Dockerfile`).
//!
//! Verifies:
//! 1. `HostTransport::DockerExec` (`docker exec -i <ctr> ddg-agents-host connect`)
//!    with 2 simultaneous clients, shared catalog, single-controller takeover,
//!    and detach/reattach preserving the exact same shell PID & VT screen.
//! 2. `HostTransport::ContainerSsh` (`ssh -T -p <port> ddg@127.0.0.1 ...`)
//!    sharing the exact same `host_id` and rooms, plus strict
//!    `DialErrorKind::HostKeyMismatch` and `DialErrorKind::AuthRequired` blocking.
//! 3. `docker restart <ctr>`: SSH host key persists, catalog restores tabs as
//!    `StoppedAfterReboot` (zero auto-reexec), and `host.tab_restart` starts a
//!    fresh live session.

use ddg_agents_proto::host::{ExecutorSpec, TabRuntimeState};
use ddg_agents_proto::terminal::ControlMode;
use ddg_agents_transport::{DialErrorKind, HostClient, HostEvent, HostProfile, HostTransport};
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct DockerSandbox {
    dir: PathBuf,
    ctr_name: String,
    state_vol: String,
    ssh_vol: String,
    ssh_port: u16,
    good_key: PathBuf,
    bad_key: PathBuf,
    known_hosts_good: PathBuf,
    known_hosts_bad: PathBuf,
}

impl DockerSandbox {
    fn start(image: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tag = format!("{}-{}", std::process::id(), nanos % 1_000_000);
        let dir = PathBuf::from(format!("/tmp/ddg-docker-e2e-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();

        let good_key = dir.join("id_ed25519_good");
        let bad_key = dir.join("id_ed25519_bad");
        let fake_host_key = dir.join("fake_host_key");
        for key in [&good_key, &bad_key, &fake_host_key] {
            let status = Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(key)
                .status()
                .expect("ssh-keygen");
            assert!(status.success(), "ssh-keygen failed for {}", key.display());
        }
        let auth_keys = dir.join("authorized_keys");
        std::fs::copy(dir.join("id_ed25519_good.pub"), &auth_keys).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let ssh_port = listener.local_addr().unwrap().port();
        drop(listener);

        let ctr_name = format!("ddg-e2e-{tag}");
        let state_vol = format!("ddg-e2e-state-{tag}");
        let ssh_vol = format!("ddg-e2e-ssh-{tag}");

        let out = Command::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                &ctr_name,
                "--init",
                "-e",
                "DDG_ENABLE_SSHD=1",
                "-p",
                &format!("127.0.0.1:{ssh_port}:2222"),
                "-v",
                &format!("{state_vol}:/home/ddg/.local/state/ddg"),
                "-v",
                &format!("{ssh_vol}:/etc/ddg/ssh"),
                "-v",
                &format!("{}:/etc/ddg/ssh/authorized_keys:ro", auth_keys.display()),
                image,
            ])
            .output()
            .expect("docker run");
        assert!(
            out.status.success(),
            "docker run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let sb = Self {
            dir: dir.clone(),
            ctr_name,
            state_vol,
            ssh_vol,
            ssh_port,
            good_key,
            bad_key,
            known_hosts_good: dir.join("known_hosts_good"),
            known_hosts_bad: dir.join("known_hosts_bad"),
        };
        sb.wait_healthy();

        // Capture real container SSH host key into known_hosts_good.
        let scan = Command::new("ssh-keyscan")
            .args(["-p", &ssh_port.to_string(), "-t", "ed25519", "127.0.0.1"])
            .output()
            .expect("ssh-keyscan");
        assert!(
            scan.status.success() && !scan.stdout.is_empty(),
            "ssh-keyscan failed: {}",
            String::from_utf8_lossy(&scan.stderr)
        );
        std::fs::write(&sb.known_hosts_good, &scan.stdout).unwrap();

        // Build a bogus known_hosts entry for the same host:port to test HostKeyMismatch.
        let fake_pub = std::fs::read_to_string(dir.join("fake_host_key.pub")).unwrap();
        let fake_fields: Vec<&str> = fake_pub.split_whitespace().collect();
        let bad_line = format!(
            "[127.0.0.1]:{ssh_port} {} {}\n",
            fake_fields[0], fake_fields[1]
        );
        std::fs::write(&sb.known_hosts_bad, bad_line).unwrap();

        sb
    }

    fn wait_healthy(&self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let status = Command::new("docker")
                .args(["exec", &self.ctr_name, "ddg-agents-host", "health"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if status.is_ok_and(|s| s.success()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let logs = Command::new("docker")
            .args(["logs", &self.ctr_name])
            .output()
            .map(|o| {
                format!(
                    "stdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                )
            })
            .unwrap_or_default();
        panic!(
            "container {} did not become healthy in 15s:\n{logs}",
            self.ctr_name
        );
    }

    fn docker_exec_profile(&self) -> HostProfile {
        HostProfile {
            id: "docker-exec".into(),
            label: "Docker Exec".into(),
            transport: HostTransport::DockerExec {
                container: self.ctr_name.clone(),
                ssh_host: None,
                known_hosts_file: None,
                identity_file: None,
            },
            expected_host_id: None,
        }
    }

    fn container_ssh_profile(&self, known_hosts: &Path, identity: &Path) -> HostProfile {
        HostProfile {
            id: "container-ssh".into(),
            label: "Container SSH".into(),
            transport: HostTransport::ContainerSsh {
                host: "127.0.0.1".into(),
                port: self.ssh_port,
                user: "ddg".into(),
                known_hosts_file: Some(known_hosts.to_string_lossy().into_owned()),
                identity_file: Some(identity.to_string_lossy().into_owned()),
            },
            expected_host_id: None,
        }
    }
}

impl Drop for DockerSandbox {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.ctr_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("docker")
            .args(["volume", "rm", "-f", &self.state_vol, &self.ssh_vol])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn collect_until(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<HostEvent>,
    tab_id: &str,
    needle: &str,
    timeout: Duration,
) -> String {
    let mut buf = Vec::new();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await
            && let HostEvent::StreamData {
                tab_id: tid, bytes, ..
            } = ev
            && tid == tab_id
        {
            buf.extend_from_slice(&bytes);
            let text = String::from_utf8_lossy(&buf);
            if text.contains(needle) {
                return text.into_owned();
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[tokio::test]
#[ignore = "requires built ddg-agents-host:test Docker image"]
async fn live_docker_exec_sshd_control_takeover_and_restart_restore() {
    let image = std::env::var("DDG_DOCKER_IMAGE").unwrap_or_else(|_| "ddg-agents-host:test".into());
    let sb = DockerSandbox::start(&image);

    // 1. Two simultaneous clients over `docker exec -i` sharing rooms & single-controller.
    let (c1, mut ev1) = HostClient::connect_once(sb.docker_exec_profile(), "exec-1")
        .await
        .expect("docker exec client 1 connect");
    let (c2, _ev2) = HostClient::connect_once(sb.docker_exec_profile(), "exec-2")
        .await
        .expect("docker exec client 2 connect");

    let host_id = c1.hello().unwrap().host_id;
    assert_eq!(host_id, c2.hello().unwrap().host_id);
    let _ = c1.catalog_subscribe().await.unwrap();
    let _ = c2.catalog_subscribe().await.unwrap();

    let created = c1
        .room_create(
            Some("컨테이너 작업실".into()),
            Some("/workspace".into()),
            ExecutorSpec::Command {
                argv: vec!["/bin/sh".into(), "-i".into()],
                env: BTreeMap::new(),
            },
            80,
            24,
            Some("docker-req-1".into()),
        )
        .await
        .unwrap();
    let tab_id = created.room.tabs[0].id.clone();
    let TabRuntimeState::Running { pid: orig_pid, .. } = created.room.tabs[0].runtime else {
        panic!("expected running tab");
    };

    let s1 = c1
        .tab_attach(tab_id.clone(), true, 80, 24, 200)
        .await
        .unwrap();
    assert_eq!(s1.mode, ControlMode::Control);
    let s2 = c2
        .tab_attach(tab_id.clone(), true, 80, 24, 200)
        .await
        .unwrap();
    assert_eq!(s2.mode, ControlMode::Readonly);

    assert!(c1.send_input(s1.channel, b"echo DOCKER_EXEC_MARKER_123\n"));
    let out1 = collect_until(
        &mut ev1,
        &tab_id,
        "DOCKER_EXEC_MARKER_123",
        Duration::from_secs(5),
    )
    .await;
    assert!(out1.contains("DOCKER_EXEC_MARKER_123"), "got: {out1:?}");

    // Client 2 forces takeover → Client 1 demoted to Readonly.
    c2.tab_claim_control(tab_id.clone(), true).await.unwrap();
    let mut demoted = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !demoted {
        if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(200), ev1.recv()).await
            && let HostEvent::TabControlChanged {
                tab_id: tid,
                mode: ControlMode::Readonly,
                ..
            } = ev
            && tid == tab_id
        {
            demoted = true;
        }
    }
    assert!(demoted, "c1 must be demoted on takeover");

    // Detach both clients — PTY must remain alive with the same PID!
    c1.close();
    c2.close();

    // 2. Connect via Container SSHD (`ssh -T -p <port> ddg@127.0.0.1 "exec ddg-agents-host connect"`).
    let (ssh_c, mut ssh_ev) = HostClient::connect_once(
        sb.container_ssh_profile(&sb.known_hosts_good, &sb.good_key),
        "ssh-1",
    )
    .await
    .expect("container ssh connect");
    assert_eq!(ssh_c.hello().unwrap().host_id, host_id);

    let sub = ssh_c.catalog_subscribe().await.unwrap();
    assert_eq!(sub.catalog.rooms.len(), 1);
    assert_eq!(
        sub.catalog.rooms[0].name.as_deref(),
        Some("컨테이너 작업실")
    );
    assert_eq!(
        sub.catalog.rooms[0].tabs[0].runtime,
        TabRuntimeState::Running {
            pty_id: 1,
            pid: orig_pid,
        }
    );

    // Reattach over SSHD and confirm Ghostty first_paint replays earlier output!
    let _s_ssh = ssh_c
        .tab_attach(tab_id.clone(), true, 80, 24, 200)
        .await
        .unwrap();
    let replayed = collect_until(
        &mut ssh_ev,
        &tab_id,
        "DOCKER_EXEC_MARKER_123",
        Duration::from_secs(5),
    )
    .await;
    assert!(
        replayed.contains("DOCKER_EXEC_MARKER_123"),
        "expected first_paint replay over SSHD, got: {replayed:?}"
    );
    ssh_c.close();

    // Verify strict SSH security checks: HostKeyMismatch & AuthRequired!
    let mismatch = HostClient::connect_once(
        sb.container_ssh_profile(&sb.known_hosts_bad, &sb.good_key),
        "ssh-mismatch",
    )
    .await
    .err()
    .expect("host key mismatch must be rejected");
    assert_eq!(mismatch.kind, DialErrorKind::HostKeyMismatch);

    let auth_err = HostClient::connect_once(
        sb.container_ssh_profile(&sb.known_hosts_good, &sb.bad_key),
        "ssh-unauth",
    )
    .await
    .err()
    .expect("unauthorized key must fail with AuthRequired");
    assert_eq!(auth_err.kind, DialErrorKind::AuthRequired);

    // 3. Restart the container → SSH host key persists, catalog restores StoppedAfterReboot!
    let restart = Command::new("docker")
        .args(["restart", &sb.ctr_name])
        .status()
        .expect("docker restart");
    assert!(restart.success());
    sb.wait_healthy();

    let (post_c, mut post_ev) = HostClient::connect_once(
        sb.container_ssh_profile(&sb.known_hosts_good, &sb.good_key),
        "ssh-post-restart",
    )
    .await
    .expect("SSH reconnect with persistent host key after docker restart");

    let post_sub = post_c.catalog_subscribe().await.unwrap();
    assert_eq!(post_sub.catalog.rooms.len(), 1);
    assert_eq!(
        post_sub.catalog.rooms[0].tabs[0].runtime,
        TabRuntimeState::StoppedAfterReboot
    );

    // Explicit host.tab_restart spawns a fresh PTY!
    let restarted = post_c.tab_restart(tab_id.clone(), 80, 24).await.unwrap();
    assert!(matches!(
        restarted.tab.runtime,
        TabRuntimeState::Running { .. }
    ));
    let s_new = post_c
        .tab_attach(tab_id.clone(), true, 80, 24, 100)
        .await
        .unwrap();
    assert!(post_c.send_input(s_new.channel, b"echo AFTER_RESTART_OK\n"));
    let out_new = collect_until(
        &mut post_ev,
        &tab_id,
        "AFTER_RESTART_OK",
        Duration::from_secs(5),
    )
    .await;
    assert!(out_new.contains("AFTER_RESTART_OK"), "got: {out_new:?}");

    post_c.close();
}
