#![cfg(unix)]

use ddg_agents_proto::frame::{DATA_CHUNK, INITIAL_CREDIT};
use ddg_agents_proto::holder as hproto;
use ddg_agents_proto::host::ExecutorSpec;
use ddg_agents_proto::terminal::ControlMode;
use ddg_agents_transport::{DialErrorKind, HostClient, HostEvent, HostProfile, HostTransport};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Sandbox {
    dir: PathBuf,
    core_child: Option<Child>,
    host_child: Option<Child>,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = PathBuf::from(format!("/tmp/ddg-tr-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = Self {
            dir,
            core_child: None,
            host_child: None,
        };
        s.start_host();
        s
    }

    fn target_debug_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug")
    }

    fn host_sock(&self) -> PathBuf {
        self.dir.join("host.sock")
    }

    fn holder_sock(&self) -> PathBuf {
        self.dir.join("holder.sock")
    }

    fn start_host(&mut self) {
        let bin_dir = Self::target_debug_dir();
        let term_bin = bin_dir.join("ddg-terminal-server");
        let core_child = Command::new(&term_bin)
            .arg("core")
            .env("DDG_STATE_DIR", &self.dir)
            .env("DDG_RUN_DIR", &self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ddg-terminal-server core");
        self.core_child = Some(core_child);
        let term_sock = self.dir.join("terminal.sock");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if StdUnixStream::connect(&term_sock).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            StdUnixStream::connect(&term_sock).is_ok(),
            "ddg-terminal-server core failed to listen"
        );

        let child = Command::new(bin_dir.join("ddg-agents-host"))
            .arg("serve")
            .env("DDG_STATE_DIR", &self.dir)
            .env("DDG_RUN_DIR", &self.dir)
            .env("DDG_TERMINAL_SERVER_BIN", &term_bin)
            .env("DDG_CONFIG_DIR", &self.dir)
            .env("DDG_PLUGINS_DIR", self.dir.join("plugins"))
            .env("DDG_HUB_ENABLED", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ddg-agents-host");
        self.host_child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if StdUnixStream::connect(self.host_sock()).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("ddg-agents-host failed to listen");
    }

    fn profile(&self, expected_host_id: Option<String>) -> HostProfile {
        HostProfile {
            id: "test".into(),
            label: "Test".into(),
            transport: HostTransport::Local {
                sock_path: Some(self.host_sock().to_string_lossy().into_owned()),
            },
            expected_host_id,
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(mut c) = self.host_child.take() {
            unsafe {
                libc::kill(c.id() as libc::pid_t, libc::SIGTERM);
            }
            let _ = c.wait();
        }
        if let Some(mut c) = self.core_child.take() {
            unsafe {
                libc::kill(c.id() as libc::pid_t, libc::SIGTERM);
            }
            let _ = c.wait();
        }
        if let Ok(mut hs) = StdUnixStream::connect(self.holder_sock()) {
            let _ = hs.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = hs.write_all(&hproto::encode_json(
                hproto::HELLO,
                0,
                &hproto::HolderHello {
                    version: hproto::HOLDER_PROTO_VERSION,
                },
            ));
            let _ = hproto::read_frame(&mut hs);
            let _ = hs.write_all(&hproto::encode_frame(hproto::GOODBYE, 0, &[]));
            while let Ok(Some((kind, _, _))) = hproto::read_frame(&mut hs) {
                if kind == hproto::BYE {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn next_data(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<HostEvent>,
    expected_channel: u32,
) -> (usize, usize, ddg_agents_transport::client::TabCredit) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.expect("event stream closed") {
                HostEvent::DeferredStreamData {
                    channel,
                    bytes,
                    credit,
                    ..
                } => {
                    assert_eq!(channel, expected_channel);
                    assert!(bytes.len() <= DATA_CHUNK);
                    return (
                        bytes.len(),
                        bytes.iter().filter(|&&b| b == b'@').count(),
                        credit,
                    );
                }
                HostEvent::StreamData { .. } | HostEvent::StreamClose { .. } => {
                    panic!("expected a live deferred DATA stream");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for deferred DATA")
}

async fn no_tab_data(events: &mut tokio::sync::mpsc::UnboundedReceiver<HostEvent>) -> bool {
    tokio::time::timeout(Duration::from_millis(150), async {
        loop {
            match events.recv().await.expect("event stream closed") {
                HostEvent::DeferredStreamData { .. }
                | HostEvent::StreamData { .. }
                | HostEvent::StreamClose { .. } => break,
                _ => {}
            }
        }
    })
    .await
    .is_err()
}

#[tokio::test]
async fn deferred_tab_credit_stalls_and_resumes_real_core_through_host() {
    let sb = Sandbox::new("credit");
    let (client, mut events) =
        HostClient::connect_once_with_deferred_tab_credit(sb.profile(None), "slow-viewer")
            .await
            .unwrap();
    let done = sb.dir.join("producer-done");
    let output_len = INITIAL_CREDIT + DATA_CHUNK * 8;
    let created = client
        .room_create(
            None,
            Some("/tmp".into()),
            ExecutorSpec::Command {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!(
                        "read -r line; head -c {output_len} /dev/zero | tr '\\000' '@'; : > '{}'; read -r line",
                        done.display()
                    ),
                ],
                env: BTreeMap::new(),
            },
            80,
            24,
            None,
        )
        .await
        .unwrap();
    let tab_id = created.room.tabs[0].id.clone();
    let tab = client
        .tab_attach(tab_id.clone(), true, 80, 24, 0)
        .await
        .unwrap();
    assert!(client.send_input(tab.channel, b"go\n"));

    let mut received = 0;
    let mut markers = 0;
    let mut held = Vec::new();
    while received < INITIAL_CREDIT {
        let (len, count, credit) = next_data(&mut events, tab.channel).await;
        received += len;
        markers += count;
        held.push((len, credit));
    }
    assert_eq!(received, INITIAL_CREDIT);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !done.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("PTY producer must finish even with a stalled viewer");
    client.ping().await.unwrap();
    assert!(
        no_tab_data(&mut events).await,
        "core sent beyond the initial window"
    );

    let (released, credit) = held.pop().unwrap();
    drop(credit);
    let mut resumed = 0;
    while resumed < released {
        let (len, count, credit) = next_data(&mut events, tab.channel).await;
        resumed += len;
        markers += count;
        held.push((len, credit));
    }
    assert_eq!(resumed, released);
    assert!(
        no_tab_data(&mut events).await,
        "core sent more than the returned credit"
    );

    drop(held);
    while markers < output_len {
        let (_, count, credit) = next_data(&mut events, tab.channel).await;
        markers += count;
        drop(credit);
    }
    assert_eq!(markers, output_len);
    client.tab_close(tab_id, None).await.unwrap();
    client.close();
}

#[tokio::test]
async fn transport_client_shares_catalog_and_notifies_control_takeover() {
    let sb = Sandbox::new("ctrl");

    let (c1, mut ev1) = HostClient::connect_once(sb.profile(None), "c1")
        .await
        .expect("c1 connect");
    let (c2, mut ev2) = HostClient::connect_once(sb.profile(None), "c2")
        .await
        .expect("c2 connect");

    let host_id = c1.hello().unwrap().host_id;
    assert_eq!(host_id, c2.hello().unwrap().host_id);

    // Wrong expected_host_id must be rejected immediately.
    let err = HostClient::connect_once(sb.profile(Some("wrong-host-id".into())), "c3")
        .await
        .err()
        .expect("host_id mismatch must fail");
    assert_eq!(err.kind, DialErrorKind::HostIdMismatch);

    let sub1 = c1.catalog_subscribe().await.unwrap();
    assert!(sub1.catalog.rooms.is_empty());
    let _sub2 = c2.catalog_subscribe().await.unwrap();

    let created = c1
        .room_create(
            Some("테스트 룸".into()),
            Some("/tmp".into()),
            ExecutorSpec::Command {
                argv: vec!["/bin/sh".into(), "-i".into()],
                env: BTreeMap::new(),
            },
            80,
            24,
            Some("req-tr-1".into()),
        )
        .await
        .unwrap();
    let tab_id = created.room.tabs[0].id.clone();

    // Client 2 receives catalog update notification.
    let mut saw_room = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !saw_room {
        if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(200), ev2.recv()).await
            && let HostEvent::CatalogChanged { catalog, .. } = ev
            && catalog.rooms.iter().any(|r| r.id == created.room.id)
        {
            saw_room = true;
        }
    }
    assert!(saw_room, "c2 must receive live catalog update");

    // Client 1 attaches with control, Client 2 attaches readonly.
    let s1 = c1
        .tab_attach(tab_id.clone(), true, 80, 24, 100)
        .await
        .unwrap();
    assert_eq!(s1.mode, ControlMode::Control);
    let s2 = c2
        .tab_attach(tab_id.clone(), true, 80, 24, 100)
        .await
        .unwrap();
    assert_eq!(s2.mode, ControlMode::Readonly);

    // Client 2 forces takeover → Client 1 receives TabControlChanged(Readonly)!
    c2.tab_claim_control(tab_id.clone(), true).await.unwrap();
    let mut c1_demoted = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !c1_demoted {
        if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(200), ev1.recv()).await
            && let HostEvent::TabControlChanged {
                tab_id: tid,
                mode: ControlMode::Readonly,
                ..
            } = ev
            && tid == tab_id
        {
            c1_demoted = true;
        }
    }
    assert!(c1_demoted, "c1 must be notified of control demotion");

    // Client 2 sends input & receives echoed output!
    assert!(c2.send_input(s2.channel, b"echo TRANSPORT_OK\n"));
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(200), ev2.recv()).await
            && let HostEvent::StreamData {
                tab_id: tid, bytes, ..
            } = ev
            && tid == tab_id
        {
            out.extend_from_slice(&bytes);
            if String::from_utf8_lossy(&out).contains("TRANSPORT_OK") {
                break;
            }
        }
    }
    assert!(
        String::from_utf8_lossy(&out).contains("TRANSPORT_OK"),
        "expected TRANSPORT_OK in output, got: {:?}",
        String::from_utf8_lossy(&out)
    );

    c1.tab_close(tab_id, None).await.unwrap();
    c1.close();
    c2.close();
}
