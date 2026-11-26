//! Unix socket server speaking Plain NDJSON JSON-RPC 2.0 → 9B Binary Framed mode.

use crate::core::Core;
use crate::outbox::Conn;
use crate::paths::{self, Paths};
use ddg_agents_os::ipc::{self, AsyncStream};
use ddg_agents_proto::frame::{
    CONTROL_CHANNEL, FrameKind, HEADER_LEN, decode_credit, decode_header, decode_resize,
};
use ddg_agents_proto::rpc::{ErrorKind, Request, Response, RpcError};
use ddg_agents_proto::terminal::{
    HelloParams, HelloResult, PROTOCOL_VERSION, PingResult, PtyAttachParams, PtyClaimControlParams,
    PtyKillParams, PtyListResult, PtyReleaseControlParams, PtySpawnParams,
    SessionInboxSubscribeParams, SessionReadScreenParams, SessionRecvParams, SessionSendParams,
    SessionWriteInputParams,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

struct CorePid(PathBuf);

impl CorePid {
    fn write(run_dir: &Path) -> io::Result<Self> {
        let path = run_dir.join("core.pid");
        ddg_agents_os::fs::write_atomic(&path, format!("{}\n", std::process::id()).as_bytes())?;
        Ok(Self(path))
    }
}

impl Drop for CorePid {
    fn drop(&mut self) {
        paths::remove_if_exists(&self.0);
    }
}

#[cfg(all(test, unix))]
#[test]
fn core_activity_and_lifecycle_pid_is_private_and_cleans_up() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("ddg-core-pid-{}", paths::random_hex()));
    paths::create_private_dir(&dir).unwrap();
    let target = dir.join("untouched");
    std::fs::write(&target, "untouched").unwrap();
    let path = dir.join("core.pid");
    std::os::unix::fs::symlink(&target, &path).unwrap();
    {
        let _guard = CorePid::write(&dir).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{}\n", std::process::id())
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
        assert_eq!(dir.read_dir().unwrap().count(), 2);
    }
    assert!(!path.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

pub fn serve(bin_path: PathBuf) -> io::Result<()> {
    let paths = Paths::from_env()?;
    paths.create_dirs()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let shutdown = {
        let _enter = rt.enter();
        ddg_agents_os::process::shutdown_signal()?
    };
    let _lock = match ddg_agents_os::lock::FileLock::try_lock_exclusive(&paths.lock_file) {
        Ok(lock) => lock,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
        Err(e) => return Err(e),
    };
    let _core_pid = CorePid::write(&paths.run_dir)?;
    if ipc::connect(&paths.terminal_sock).is_ok() {
        return Err(io::Error::other(format!(
            "another ddg-terminal-server already listens at {}",
            paths.terminal_sock.display()
        )));
    }
    #[cfg(unix)]
    paths::remove_if_exists(&paths.terminal_sock);

    let core = Core::start(paths.clone(), &bin_path)?;

    rt.block_on(async move {
        use ddg_agents_os::ipc::LocalListener;
        let admin_path = paths.run_dir.join("terminal-admin.sock");
        #[cfg(unix)]
        paths::remove_if_exists(&admin_path);
        let admin = LocalListener::bind(&admin_path)?.into_tokio()?;
        let listener = LocalListener::bind(&paths.terminal_sock)?.into_tokio()?;

        let hb_core = core.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            tick.tick().await;
            loop {
                tick.tick().await;
                hb_core.heartbeat_all();
            }
        });

        let statuses = core.clone();
        tokio::spawn(async move { statuses.publish_status_updates().await });
        let foreground = core.clone();
        tokio::spawn(async move { foreground.poll_foreground().await });

        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                accepted = admin.accept() => {
                    let (stream, _) = accepted?;
                    if admin_request(&core, stream).await {
                        return Ok(());
                    }
                }
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let c = core.clone();
                    tokio::spawn(async move {
                        let _ = handle_connection(c, stream).await;
                    });
                }
                _ = &mut shutdown => break,
                _ = core.stop.notified() => break,
            }
        }
        let _ = core.goodbye_detach().await;
        Ok(())
    })
}

async fn admin_request(core: &Core, mut stream: AsyncStream) -> bool {
    use ddg_agents_proto::holder::HolderGoodbye;

    let authorized = ddg_agents_os::ipc::peer_credentials(&stream).is_ok_and(|peer| {
        ipc::peer_is_current_user(&peer)
            && peer
                .pid
                .is_some_and(|pid| !core.list_ptys().iter().any(|pty| pty.pid == pid))
    });

    if !authorized {
        let _ = stream
            .write_all(b"{\"error\":\"permission_denied\"}\n")
            .await;
        return false;
    }
    let mut line = String::new();
    let mut reader = BufReader::new(&mut stream).take(64);
    if !matches!(
        tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line)).await,
        Ok(Ok(_))
    ) {
        return false;
    }
    let result = match line.as_str() {
        "detach\n" => core.goodbye_detach().await,
        "shutdown-if-idle\n" if !core.is_idle() => {
            let _ = stream
                .write_all(b"{\"error\":\"holder_restart_required\"}\n")
                .await;
            return false;
        }
        "shutdown\n" | "shutdown-if-idle\n" => core.goodbye(HolderGoodbye::Shutdown).await,
        _ => return false,
    };
    let reply: &[u8] = if result.is_ok() {
        b"{\"ok\":true}\n"
    } else {
        b"{\"error\":\"holder_ack_failed\"}\n"
    };
    let _ = stream.write_all(reply).await;
    #[cfg(windows)]
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut [0])).await;
    // Even without an ack, the Holder may already have detached.
    true
}

async fn handle_connection(core: Arc<Core>, stream: AsyncStream) -> io::Result<()> {
    let (read_half, mut write_half) = ipc::split(stream);
    let mut reader = BufReader::new(read_half);
    let conn = Arc::new(Conn::new(core.next_conn_id()));
    let mut line = String::new();

    // Phase 1: Plain NDJSON JSON-RPC 2.0 until `hello { framed: true }`.
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (resp, upgrade, resume_ch) =
            dispatch_json(&core, &conn, trimmed.as_bytes(), false).await;
        let out = format!("{}\n", serde_json::to_string(&resp).expect("serializable"));
        write_half.write_all(out.as_bytes()).await?;
        if let Some(ch) = resume_ch {
            conn.with_outbox(|o| o.resume(ch));
        }
        if upgrade {
            break;
        }
    }

    // Phase 2: 9-byte binary framed multiplexing.
    let writer_conn = conn.clone();
    let writer_core = core.clone();
    let mut writer_task = tokio::spawn(async move {
        loop {
            if let Some(frame) = writer_conn.next_frame() {
                if write_half.write_all(&frame).await.is_err() {
                    return;
                }
            } else {
                writer_core.refill_inbox(&writer_conn);
                writer_conn.wake.notified().await;
            }
        }
    });

    let read_result = tokio::select! {
        res = read_framed_loop(&core, &conn, &mut reader) => res,
        _ = &mut writer_task => Ok(()),
        _ = conn.overflowed.notified() => Ok(()),
    };

    conn.gone.store(true, Ordering::SeqCst);
    conn.disconnected.notify_waiters();
    core.disconnect_conn(&conn);
    writer_task.abort();
    read_result
}

async fn read_framed_loop(
    core: &Arc<Core>,
    conn: &Arc<Conn>,
    reader: &mut BufReader<ipc::ReadHalf>,
) -> io::Result<()> {
    let mut header = [0u8; HEADER_LEN];
    loop {
        match reader.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let (channel, kind, len) = decode_header(&header).map_err(io::Error::other)?;
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload).await?;

        match (channel, kind) {
            (CONTROL_CHANNEL, FrameKind::Data) => {
                let core = core.clone();
                let conn = conn.clone();
                tokio::spawn(async move {
                    let (resp, _, resume_ch) = dispatch_json(&core, &conn, &payload, true).await;
                    if let Ok(bytes) = serde_json::to_vec(&resp) {
                        conn.with_outbox(|out| {
                            out.control_json(&bytes);
                            if let Some(ch) = resume_ch {
                                out.resume(ch);
                            }
                        });
                    }
                });
            }
            (CONTROL_CHANNEL, _) => {}
            (ch, FrameKind::Data) => {
                core.viewer_input(conn, ch, &payload);
            }
            (ch, FrameKind::Credit) => {
                if let Ok(bytes) = decode_credit(&payload) {
                    conn.with_outbox(|out| out.credit(ch, bytes as usize));
                }
            }
            (ch, FrameKind::Resize) => {
                if let Ok((cols, rows)) = decode_resize(&payload) {
                    core.viewer_resize(conn, ch, cols, rows);
                }
            }
            (ch, FrameKind::Close) => {
                core.detach_channel(conn, ch);
            }
        }
    }
}

fn parse_params<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    let value = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(value).map_err(|e| RpcError::invalid_params(e.to_string()))
}

async fn dispatch_json(
    core: &Arc<Core>,
    conn: &Arc<Conn>,
    raw: &[u8],
    is_framed: bool,
) -> (Response, bool, Option<u32>) {
    let req: Request = match serde_json::from_slice(raw) {
        Ok(req) => req,
        Err(e) => {
            return (
                Response::err(Value::Null, RpcError::invalid_params(e.to_string())),
                false,
                None,
            );
        }
    };
    let id = req.id.clone();
    if core.stopping.load(Ordering::Acquire) {
        return (
            Response::err(
                id,
                RpcError::new(ErrorKind::Unavailable, "Core is detaching", None),
            ),
            false,
            None,
        );
    }
    let mut upgrade = false;
    let mut resume_ch = None;

    let result: Result<Value, RpcError> = match req.method.as_str() {
        "hello" => match parse_params::<HelloParams>(req.params) {
            Ok(p) if p.protocol_version != PROTOCOL_VERSION => {
                Err(RpcError::invalid_params(format!(
                    "unsupported protocol_version {} (expected {})",
                    p.protocol_version, PROTOCOL_VERSION
                )))
            }
            Ok(p) => {
                upgrade = p.framed && !is_framed;
                Ok(json!(HelloResult {
                    protocol_version: PROTOCOL_VERSION,
                    server_id: core.identity.server_id.clone(),
                    boot_generation: core.identity.boot_generation,
                    conn_id: conn.id,
                    features: vec![
                        "credit_v1".into(),
                        "single_control_v1".into(),
                        "agent_activity_v1".into()
                    ],
                    app_version: Some(env!("CARGO_PKG_VERSION").into()),
                    platform: Some(match (std::env::consts::OS, std::env::consts::ARCH) {
                        ("macos", "aarch64") => "darwin-arm64".into(),
                        ("macos", arch) => format!("darwin-{arch}"),
                        (os, arch) => format!("{os}-{arch}"),
                    }),
                    holder_protocol_version: Some(ddg_agents_proto::holder::HOLDER_PROTO_VERSION),
                    holder_instance: Some(core.holder_instance.clone()),
                    active_pty_count: Some(core.list_ptys().len() as u32),
                }))
            }
            Err(e) => Err(e),
        },
        "ping" => Ok(json!(PingResult {
            ok: true,
            server_id: core.identity.server_id.clone(),
            boot_generation: core.identity.boot_generation,
        })),
        "pty.spawn" => match parse_params::<PtySpawnParams>(req.params) {
            Ok(p) => core.spawn_pty(p).await.map(|r| json!(r)),
            Err(e) => Err(e),
        },
        "pty.list" => Ok(json!(PtyListResult {
            ptys: core.list_ptys(),
        })),
        "pty.subscribe" => {
            if !is_framed {
                Err(RpcError::invalid_params(
                    "pty.subscribe requires framed mode (call hello with framed=true first)",
                ))
            } else {
                let res = core.subscribe(conn);
                resume_ch = res.channel;
                Ok(json!(res))
            }
        }
        "pty.attach" => {
            if !is_framed {
                Err(RpcError::invalid_params(
                    "pty.attach requires framed mode (call hello with framed=true first)",
                ))
            } else {
                match parse_params::<PtyAttachParams>(req.params) {
                    Ok(p) => core.attach(conn, p).map(|r| {
                        resume_ch = Some(r.channel);
                        json!(r)
                    }),
                    Err(e) => Err(e),
                }
            }
        }
        "pty.claim_control" => match parse_params::<PtyClaimControlParams>(req.params) {
            Ok(p) => core.claim_control(conn, p).map(|r| json!(r)),
            Err(e) => Err(e),
        },
        "pty.release_control" => match parse_params::<PtyReleaseControlParams>(req.params) {
            Ok(p) => core.release_control(conn, p).map(|r| json!(r)),
            Err(e) => Err(e),
        },
        "pty.kill" => match parse_params::<PtyKillParams>(req.params) {
            Ok(p) => core.kill_pty(p).await.map(|r| json!(r)),
            Err(e) => Err(e),
        },
        "session.send" => parse_params::<SessionSendParams>(req.params)
            .and_then(|p| core.session_send(p))
            .map(|r| json!(r)),
        "session.recv" => match parse_params::<SessionRecvParams>(req.params) {
            Ok(p) => {
                let disconnected = conn.disconnected.notified();
                if conn.gone.load(Ordering::SeqCst) {
                    Err(RpcError::new(
                        ErrorKind::Unavailable,
                        "connection closed",
                        None,
                    ))
                } else {
                    tokio::select! {
                        result = core.session_recv(p) => result.map(|r| json!(r)),
                        _ = disconnected => Err(RpcError::new(ErrorKind::Unavailable, "connection closed", None)),
                    }
                }
            }
            Err(e) => Err(e),
        },
        "session.inbox_subscribe" if !is_framed => Err(RpcError::invalid_params(
            "session.inbox_subscribe requires framed mode (call hello with framed=true first)",
        )),
        "session.inbox_subscribe" => parse_params::<SessionInboxSubscribeParams>(req.params)
            .and_then(|p| core.inbox_subscribe(conn, p))
            .map(|r| {
                resume_ch = Some(r.channel);
                json!(r)
            }),
        "session.read_screen" => parse_params::<SessionReadScreenParams>(req.params)
            .and_then(|p| core.read_screen(p))
            .map(|r| json!(r)),
        "session.write_input" => parse_params::<SessionWriteInputParams>(req.params)
            .and_then(|p| core.write_input(p))
            .map(|r| json!(r)),
        other => Err(RpcError::new(
            ErrorKind::NotFound,
            format!("unknown method {other:?}"),
            None,
        )),
    };

    let resp = match result {
        Ok(val) => Response::ok(id, val),
        Err(err) => Response::err(id, err),
    };
    (resp, upgrade, resume_ch)
}
