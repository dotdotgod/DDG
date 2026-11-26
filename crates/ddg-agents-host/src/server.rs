//! Unix socket server for `ddg-agents-host` speaking Plain NDJSON → 9B Binary
//! Framed mode, with per-client `terminal.sock` links for zero-parse stream
//! passthrough and native single-controller isolation.

use crate::admin::{self, Binding};
use crate::host_service::HostService;
use crate::hub::{self, Relay};
use crate::paths::{self, Paths};
use crate::term_conn::TermConn;
use ddg_agents_os::ipc::{self, AsyncStream};
use ddg_agents_proto::frame::{
    CONTROL_CHANNEL, Frame, FrameKind, HEADER_LEN, decode_header, encode_frame,
};
use ddg_agents_proto::host::{
    HostHelloResult, MsgRecvParams, RelayAttachResult, RoomCreateParams, RoomRenameParams,
    RoomSetLayoutParams, TabAttachParams, TabAttachResult, TabClaimControlParams, TabCloseParams,
    TabCreateParams, TabMarkSeenParams, TabMoveParams, TabMsgSendParams, TabReleaseControlParams,
    TabRestartParams, TabScreenParams, TabSplitParams, TabWriteParams,
};
use ddg_agents_proto::openrpc::agents_host_openrpc;
use ddg_agents_proto::rpc::{ErrorKind, Request, Response, RpcError};
use ddg_agents_proto::terminal::{
    CloseReason, HelloParams, PROTOCOL_VERSION, PingResult, PtyAttachParams, PtyAttachResult,
    PtyClaimControlParams, PtyClaimControlResult, PtyReleaseControlParams, PtyReleaseControlResult,
    SessionInboxSubscribeParams, SessionInboxSubscribeResult, SessionRecvParams,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

pub struct HostPid(PathBuf);

impl HostPid {
    pub fn write(paths: &Paths) -> io::Result<Self> {
        let path = paths.pid_file();
        paths::write_atomic(&path, format!("{}\n", std::process::id()).as_bytes())?;
        Ok(Self(path))
    }
}

impl Drop for HostPid {
    fn drop(&mut self) {
        paths::remove_if_exists(&self.0);
    }
}

pub fn serve(restoring: bool) -> io::Result<()> {
    let mut input = io::stdin().lock();
    let tokens = if restoring {
        Some(admin::read_snapshot(&mut input)?)
    } else {
        None
    };
    let paths = Paths::from_env()?;
    paths.create_dirs()?;
    #[cfg(windows)]
    if let Ok(_lock) =
        ddg_agents_os::lock::FileLock::try_lock_exclusive(&paths.state_dir.join("update.lock"))
        && let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let _ = ddg_agents_os::replace::gc_stale_tombstones(parent);
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        use ddg_agents_os::lock::FileLock;
        let shutdown = ddg_agents_os::process::shutdown_signal()?;
        tokio::pin!(shutdown);
        let startup = if restoring {
            None
        } else {
            let start = match FileLock::try_lock_exclusive(&admin::start_lock(&paths)) {
                Ok(lock) => lock,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            };
            let lock = match FileLock::try_lock_exclusive(&paths.lock_file) {
                Ok(lock) => lock,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            };
            Some((start, lock))
        };
        let host = HostService::start(paths.clone(), Relay::from_env()?, tokens).await?;
        let mut binding = if let Some((_start, lock)) = startup {
            host.plugins
                .load_dir(&paths.plugins_dir, Arc::downgrade(&host))
                .await;
            Some(Binding::with_lock(&paths, lock)?)
        } else {
            let mut output = io::stdout().lock();
            admin::await_bind(&mut input, &mut output)?;
            let binding = Binding::bind(&paths)?;
            admin::ready(&mut output)?;
            drop(output);
            if let Err(e) = host.flush_catalog() {
                eprintln!("ddg-agents-host: could not flush restored catalog: {e}");
            }
            host.plugins
                .load_dir(&paths.plugins_dir, Arc::downgrade(&host))
                .await;
            Some(binding)
        };
        drop(input);
        Relay::start(&host);
        let mut prepared = None;
        loop {
            let sockets = binding.as_ref().expect("host binding");
            tokio::select! {
                accepted = sockets.public.accept() => {
                    let (stream, _) = accepted?;
                    let h = host.clone();
                    #[cfg(unix)]
                    tokio::spawn(async move {
                        let _ = handle_client(h, stream).await;
                    });
                    #[cfg(windows)]
                    {
                        let sockets = binding.as_mut().unwrap();
                        let closing = sockets.closing.subscribe();
                        sockets.clients.spawn(async move { let _ = handle_client(h, stream, closing).await; });
                        while sockets.clients.try_join_next().is_some() {}
                    }
                }
                accepted = sockets.admin.accept() => {
                    let (stream, _) = accepted?;
                    if admin::handle(&host, stream, &mut prepared, &mut binding).await? {
                        break;
                    }
                }
                _ = admin::expire(prepared.as_ref()) => { prepared.take(); }
                _ = &mut shutdown => break,
                _ = host.stop.notified() => break,
            }
        }
        Ok(())
    })
}

struct ClientSession {
    conn_id: u64,
    out_tx: mpsc::UnboundedSender<Vec<u8>>,
    next_channel: AtomicU32,
    closed: AtomicBool,
    token_seen: AtomicBool,
    /// Per-client connection to `ddg-terminal-server` so single-controller leases
    /// natively isolate every client connection!
    term: tokio::sync::Mutex<Option<Arc<TermConn>>>,
    /// Client channel → terminal-server channel for zero-parse passthrough.
    passthrough: Mutex<HashMap<u32, (Arc<TermConn>, u32)>>,
}

impl ClientSession {
    fn new(conn_id: u64, out_tx: mpsc::UnboundedSender<Vec<u8>>) -> Self {
        Self {
            conn_id,
            out_tx,
            next_channel: AtomicU32::new(1),
            closed: AtomicBool::new(false),
            token_seen: AtomicBool::new(false),
            term: tokio::sync::Mutex::new(None),
            passthrough: Mutex::new(HashMap::new()),
        }
    }

    fn alloc_channel(&self) -> u32 {
        self.next_channel.fetch_add(1, Ordering::Relaxed)
    }

    fn parse_request(&self, host: &HostService, raw: &[u8]) -> serde_json::Result<Request> {
        let request: Request = serde_json::from_slice(raw)?;
        if request.params.get("session_token").is_some()
            && !self.token_seen.swap(true, Ordering::AcqRel)
        {
            host.relay.detach(self.conn_id, None);
        }
        Ok(request)
    }

    async fn ensure_term(&self, host: &HostService) -> Result<Arc<TermConn>, RpcError> {
        let mut guard = self.term.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(RpcError::new(
                ErrorKind::Unavailable,
                "client disconnected",
                None,
            ));
        }
        let control = host.term()?;
        if let Some(conn) = guard.as_ref()
            && conn.is_alive()
            && conn.hello.boot_generation == control.hello.boot_generation
        {
            return Ok(conn.clone());
        }
        let conn = TermConn::connect(&host.paths, &format!("host-client-{}", self.conn_id))
            .await
            .map_err(|e| RpcError::new(ErrorKind::Unavailable, e.to_string(), None))?;
        if conn.hello.server_id != host.host_id
            || conn.hello.boot_generation != host.term()?.hello.boot_generation
        {
            return Err(RpcError::new(
                ErrorKind::Unavailable,
                "terminal generation changed",
                None,
            ));
        }
        host.register_client_term_link(self.conn_id, conn.hello.conn_id);
        if let Some(previous) = guard.replace(conn.clone()) {
            previous.close();
        }
        Ok(conn)
    }
}

async fn handle_client(
    host: Arc<HostService>,
    stream: AsyncStream,
    #[cfg(windows)] mut closing: tokio::sync::watch::Receiver<bool>,
) -> io::Result<()> {
    let (read_half, mut write_half) = ipc::split(stream);
    let mut reader = BufReader::new(read_half);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let session = Arc::new(ClientSession::new(host.next_conn_id(), out_tx));
    let mut line = String::new();

    // Phase 1: Plain NDJSON JSON-RPC 2.0 until `hello { framed: true }`.
    let negotiate = async {
        loop {
            line.clear();
            if reader.read_line(&mut line).await? == 0 {
                return Ok(false);
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let request = session.parse_request(&host, trimmed.as_bytes());
            let (resp, upgrade, after_reply) = dispatch_rpc(&host, &session, request, false).await;
            let out = format!("{}\n", serde_json::to_string(&resp).expect("serializable"));
            write_half.write_all(out.as_bytes()).await?;
            if let Some(go) = after_reply {
                let _ = go.send(());
            }
            if upgrade {
                return Ok::<_, io::Error>(true);
            }
        }
    };
    #[cfg(unix)]
    let upgraded = negotiate.await?;
    #[cfg(windows)]
    let upgraded = tokio::select! {
        result = negotiate => result?,
        _ = closing.wait_for(|close| *close) => false,
    };
    if !upgraded {
        return Ok(());
    }

    // Phase 2: 9-byte binary framed mode.
    host.register_client(session.conn_id, session.out_tx.clone());
    let writer = async move {
        while let Some(frame) = out_rx.recv().await {
            if write_half.write_all(&frame).await.is_err() {
                break;
            }
        }
    };
    #[cfg(unix)]
    let mut writer_task = tokio::spawn(writer);

    #[cfg(unix)]
    let read_res = tokio::select! {
        res = read_framed(&host, &session, &mut reader) => res,
        _ = &mut writer_task => Ok(()),
    };
    #[cfg(windows)]
    let read_res = tokio::select! {
        res = read_framed(&host, &session, &mut reader) => res,
        _ = writer => Ok(()),
        _ = closing.wait_for(|close| *close) => Ok(()),
    };

    session.closed.store(true, Ordering::Release);
    host.relay.detach(session.conn_id, None);
    host.disconnect_conn(session.conn_id);
    // Close the client link even if an in-flight receive is holding a reference to it.
    session
        .passthrough
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
    if let Some(term) = session.term.lock().await.take() {
        term.close();
    }
    #[cfg(unix)]
    writer_task.abort();
    read_res
}

async fn read_framed(
    host: &Arc<HostService>,
    session: &Arc<ClientSession>,
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

        if channel == CONTROL_CHANNEL && kind == FrameKind::Data {
            let request = session.parse_request(host, &payload);
            let h = host.clone();
            let s = session.clone();
            tokio::spawn(async move {
                let (resp, _, after_reply) = dispatch_rpc(&h, &s, request, true).await;
                if let Ok(bytes) = serde_json::to_vec(&resp)
                    && let Ok(frame) = encode_frame(CONTROL_CHANNEL, FrameKind::Data, &bytes)
                    && s.out_tx.send(frame).is_ok()
                    && let Some(go) = after_reply
                {
                    let _ = go.send(());
                }
            });
            continue;
        }

        let Some(Frame { payload, .. }) = host.relay.feed(
            session.conn_id,
            Frame {
                channel,
                kind,
                payload,
            },
        ) else {
            continue;
        };
        // Zero-parse 9B passthrough for attached PTY channels!
        let upstream = {
            let mut links = session
                .passthrough
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if kind == FrameKind::Close {
                links.remove(&channel)
            } else {
                links.get(&channel).cloned()
            }
        };
        if let Some((term, term_ch)) = upstream {
            let _ = term.send_frame(term_ch, kind, &payload);
            if kind == FrameKind::Close {
                term.detach_stream(term_ch);
            }
        } else if kind == FrameKind::Close {
            host.detach_sub_channel(session.conn_id, channel);
        }
    }
}

fn parse_params<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    let value = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(value).map_err(|e| RpcError::invalid_params(e.to_string()))
}

async fn dispatch_rpc(
    host: &Arc<HostService>,
    session: &Arc<ClientSession>,
    request: serde_json::Result<Request>,
    is_framed: bool,
) -> (Response, bool, Option<tokio::sync::oneshot::Sender<()>>) {
    let req = match request {
        Ok(r) => r,
        Err(e) => {
            return (
                Response::err(Value::Null, RpcError::invalid_params(e.to_string())),
                false,
                None,
            );
        }
    };
    let id = req.id.clone();
    let Ok(_op) = host.reload_gate.try_read() else {
        return (Response::err(id, admin::draining()), false, None);
    };
    let mut upgrade = false;
    let mut after_reply = None;

    let result: Result<Value, RpcError> = match req.method.as_str() {
        "hello" => match parse_params::<HelloParams>(req.params) {
            Ok(params) if params.protocol_version != PROTOCOL_VERSION => Err(RpcError::with_code(
                ErrorKind::InvalidParams,
                ddg_agents_proto::rpc::error_code::PROTOCOL_INCOMPATIBLE,
                format!("unsupported protocol_version {}, expected {}", params.protocol_version, PROTOCOL_VERSION),
                Some(json!({"remote_protocol": PROTOCOL_VERSION, "remote_version": env!("CARGO_PKG_VERSION")}).to_string()),
            )),
            Ok(p) => {
                upgrade = p.framed && !is_framed;
                let term = host.term().ok();
                let active_pty_count = match &term {
                    Some(term) => term.call::<ddg_agents_proto::terminal::PtyListResult>("pty.list", json!({})).await.ok().map(|r| r.ptys.len() as u32),
                    None => None,
                };
                let state_dir = host.paths.state_dir.clone();
                let rollback_version = tokio::task::spawn_blocking(move || crate::daemon_swap::rollback_version(&state_dir)).await.ok().flatten();
                Ok(json!(HostHelloResult {
                    protocol_version: PROTOCOL_VERSION,
                    host_id: host.host_id.clone(),
                    boot_generation: host.boot_generation(),
                    conn_id: session.conn_id,
                    features: vec![
                        "credit_v1".into(),
                        "single_control_v1".into(),
                        "passthrough_v1".into(),
                        "agent_activity_v1".into(),
                    ],
                    app_version: Some(env!("CARGO_PKG_VERSION").into()),
                    platform: Some(crate::daemon_swap::platform()),
                    terminal_server_version: term.as_ref().and_then(|term| term.hello.app_version.clone()),
                    holder_protocol_version: term.as_ref().and_then(|term| term.hello.holder_protocol_version),
                    active_pty_count,
                    rollback_version,
                }))
            }
            Err(e) => Err(e),
        },
        "ping" => match host.term() {
            Ok(term) => term
                .call::<PingResult>("ping", json!({}))
                .await
                .map(|r| json!(r)),
            Err(e) => Err(e),
        },
        "rpc.discover" => {
            let mut doc = agents_host_openrpc();
            if let Some(methods) = doc["methods"].as_array_mut() {
                methods.extend(host.plugins.discover_methods());
            }
            Ok(doc)
        }
        "host.catalog_subscribe" => {
            if !is_framed {
                Err(RpcError::invalid_params(
                    "host.catalog_subscribe requires framed mode",
                ))
            } else {
                let ch = session.alloc_channel();
                let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Vec<u8>>();
                let (go_tx, go_rx) = tokio::sync::oneshot::channel();
                after_reply = Some(go_tx);
                let out_tx = session.out_tx.clone();
                tokio::spawn(async move {
                    if go_rx.await.is_err() {
                        return;
                    }
                    while let Some(bytes) = ev_rx.recv().await {
                        if let Ok(frame) = encode_frame(ch, FrameKind::Data, &bytes)
                            && out_tx.send(frame).is_err()
                        {
                            break;
                        }
                    }
                });
                Ok(json!(host.catalog_subscribe(session.conn_id, ch, ev_tx)))
            }
        }
        "host.room_create" => match parse_params::<RoomCreateParams>(req.params) {
            Ok(p) => host.room_create(p).await.map(|r| json!(r)),
            Err(e) => Err(e),
        },
        "host.room_rename" => match parse_params::<RoomRenameParams>(req.params) {
            Ok(p) => host.room_rename(p).map(|r| json!(r)),
            Err(e) => Err(e),
        },
        "host.room_set_layout" => parse_params::<RoomSetLayoutParams>(req.params)
            .and_then(|p| host.room_set_layout(p).map(|r| json!(r))),
        "host.tab_split" => match parse_params::<TabSplitParams>(req.params) {
            Ok(p) => host.tab_split(p).await.map(|r| json!(r)),
            Err(e) => Err(e),
        },
        "host.tab_create" => match parse_params::<TabCreateParams>(req.params) {
            Ok(p) => host.tab_create(p).await.map(|r| json!(r)),
            Err(e) => Err(e),
        },
        "host.tab_screen" => match parse_params::<TabScreenParams>(req.params) {
            Ok(p) => host.tab_screen(p).await.map(|r| json!(r)),
            Err(e) => Err(e),
        },
        "host.tab_write" => match parse_params::<TabWriteParams>(req.params) {
            Ok(p) => host.tab_write(p).await.map(|r| json!(r)),
            Err(e) => Err(e),
        },
        "host.tab_msg_send" => match parse_params::<TabMsgSendParams>(req.params) {
            Ok(p) => host.tab_msg_send(p).await.map(|r| json!(r)),
            Err(e) => Err(e),
        },
        "host.tab_move" => parse_params::<TabMoveParams>(req.params)
            .and_then(|p| host.tab_move(p).map(|r| json!(r))),
        "host.tab_mark_seen" => parse_params::<TabMarkSeenParams>(req.params)
            .and_then(|p| host.tab_mark_seen(p).map(|r| json!(r))),
        "host.tab_close" => match parse_params::<TabCloseParams>(req.params) {
            Ok(p) => host
                .tab_close_with_visibility(p, false)
                .await
                .map(|r| json!(r)),
            Err(e) => Err(e),
        },
        "host.tab_restart" => match parse_params::<TabRestartParams>(req.params) {
            Ok(p) => host
                .tab_restart_with_visibility(p, false)
                .await
                .map(|r| json!(r)),
            Err(e) => Err(e),
        },
        "host.tab_attach" => {
            if !is_framed {
                Err(RpcError::invalid_params(
                    "host.tab_attach requires framed mode",
                ))
            } else {
                match handle_tab_attach(host, session, req.params).await {
                    Ok((val, go_tx)) => {
                        after_reply = Some(go_tx);
                        Ok(val)
                    }
                    Err(e) => Err(e),
                }
            }
        }
        "host.tab_claim_control" => handle_tab_claim_control(host, session, req.params).await,
        "host.tab_release_control" => handle_tab_release_control(host, session, req.params).await,
        "host.msg_recv" => handle_msg_recv(host, session, req.params).await,
        "host.msg_subscribe" => {
            match handle_msg_subscribe(host, session, req.params, is_framed).await {
                Ok((val, go)) => {
                    after_reply = Some(go);
                    Ok(val)
                }
                Err(e) => Err(e),
            }
        }
        "host.relay_attach" => {
            let channel = session.alloc_channel();
            match host.relay.attach(
                session.conn_id,
                channel,
                session.out_tx.clone(),
                is_framed,
                || {
                    session.closed.load(Ordering::Acquire)
                        || session.token_seen.load(Ordering::Acquire)
                },
            ) {
                Ok(wire) => {
                    let (go, ready) = tokio::sync::oneshot::channel();
                    after_reply = Some(go);
                    tokio::spawn(wire.run(host.clone(), session.conn_id, ready));
                    Ok(json!(RelayAttachResult { channel }))
                }
                Err(error) => Err(error),
            }
        }
        method if hub::allowed(method) => host.session_rpc(method, req.params).await,
        ext if ext.starts_with("ext.") => host.plugins.call_ext(ext, req.params).await,
        other => Err(RpcError::new(
            ErrorKind::NotFound,
            format!("unknown method {other:?}"),
            None,
        )),
    };

    let resp = match result {
        Ok(v) => Response::ok(id, v),
        Err(e) => Response::err(id, e),
    };
    (resp, upgrade, after_reply)
}

async fn handle_tab_attach(
    host: &Arc<HostService>,
    session: &Arc<ClientSession>,
    params: Value,
) -> Result<(Value, tokio::sync::oneshot::Sender<()>), RpcError> {
    let p: TabAttachParams = parse_params(params)?;
    let term = session.ensure_term(host).await?;
    let tab = host.lookup_tab_any(&p.tab_id)?;
    let pty_id = tab
        .pty_id
        .ok_or_else(|| RpcError::session_not_running("tab is not running"))?;
    let client_ch = session.alloc_channel();

    let (sink_tx, sink_rx) = mpsc::unbounded_channel::<Frame>();
    let attached: PtyAttachResult = term
        .call_into_stream(
            "pty.attach",
            json!(PtyAttachParams {
                pty_id,
                want_control: p.want_control,
                cols: p.cols,
                rows: p.rows,
                history_lines: p.history_lines,
            }),
            sink_tx,
        )
        .await?;
    let go_tx = forward_stream(session, (term, attached.channel), client_ch, sink_rx);
    Ok((
        json!(TabAttachResult {
            channel: client_ch,
            run_generation: tab.run_generation,
            mode: attached.mode,
            control_epoch: attached.control_epoch,
            cols: attached.cols,
            rows: attached.rows,
            runtime: tab.runtime,
        }),
        go_tx,
    ))
}

fn authenticate_inbox(host: &HostService, params: &Value) -> Result<(), RpcError> {
    host.authenticate_local(params)?;
    if params
        .get("host_id")
        .is_some_and(|id| !id.is_null() && id != &host.host_id && id != "local")
    {
        return Err(RpcError::invalid_params(
            "inbox methods only address the local host",
        ));
    }
    Ok(())
}

async fn handle_msg_recv(
    host: &HostService,
    session: &ClientSession,
    params: Value,
) -> Result<Value, RpcError> {
    authenticate_inbox(host, &params)?;
    let term = session.ensure_term(host).await?;
    let caller = host.authenticate_local(&params)?;
    let p: MsgRecvParams = parse_params(params)?;
    term.call_value(
        "session.recv",
        json!(SessionRecvParams {
            session_id: caller.session_id,
            max_messages: p.max_messages,
            wait_ms: p.wait_ms,
        }),
    )
    .await
}

async fn handle_msg_subscribe(
    host: &HostService,
    session: &Arc<ClientSession>,
    params: Value,
    is_framed: bool,
) -> Result<(Value, tokio::sync::oneshot::Sender<()>), RpcError> {
    authenticate_inbox(host, &params)?;
    if !is_framed {
        return Err(RpcError::invalid_params(
            "host.msg_subscribe requires framed mode",
        ));
    }
    let term = session.ensure_term(host).await?;
    let caller = host.authenticate_local(&params)?;
    let (tx, rx) = mpsc::unbounded_channel();
    let upstream: SessionInboxSubscribeResult = term
        .call_into_stream(
            "session.inbox_subscribe",
            json!(SessionInboxSubscribeParams {
                session_id: caller.session_id
            }),
            tx,
        )
        .await?;
    let channel = session.alloc_channel();
    let go = forward_stream(session, (term, upstream.channel), channel, rx);
    Ok((json!(SessionInboxSubscribeResult { channel }), go))
}

fn forward_stream(
    session: &Arc<ClientSession>,
    upstream: (Arc<TermConn>, u32),
    channel: u32,
    mut rx: mpsc::UnboundedReceiver<Frame>,
) -> tokio::sync::oneshot::Sender<()> {
    session
        .passthrough
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(channel, upstream);
    let session = Arc::downgrade(session);
    let (go, ready) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = ready.await;
        while let Some(frame) = rx.recv().await {
            let Some(client) = session.upgrade() else {
                return;
            };
            if let Ok(bytes) = encode_frame(channel, frame.kind, &frame.payload) {
                let _ = client.out_tx.send(bytes);
            }
            if frame.kind == FrameKind::Close {
                client
                    .passthrough
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&channel);
                return;
            }
        }
        if let Some(client) = session.upgrade()
            && client
                .passthrough
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&channel)
                .is_some()
            && let Ok(bytes) = encode_frame(
                channel,
                FrameKind::Close,
                &serde_json::to_vec(&CloseReason::ServerStopped).expect("serializable"),
            )
        {
            let _ = client.out_tx.send(bytes);
        }
    });
    go
}

async fn handle_tab_claim_control(
    host: &Arc<HostService>,
    session: &Arc<ClientSession>,
    params: Value,
) -> Result<Value, RpcError> {
    let p: TabClaimControlParams = parse_params(params)?;
    let term = session.ensure_term(host).await?;
    let pty_id = host
        .lookup_tab_any(&p.tab_id)?
        .pty_id
        .ok_or_else(|| RpcError::session_not_running("tab is not running"))?;
    let res: PtyClaimControlResult = term
        .call(
            "pty.claim_control",
            json!(PtyClaimControlParams {
                pty_id,
                force: p.force,
            }),
        )
        .await?;
    Ok(json!(res))
}

async fn handle_tab_release_control(
    host: &Arc<HostService>,
    session: &Arc<ClientSession>,
    params: Value,
) -> Result<Value, RpcError> {
    let p: TabReleaseControlParams = parse_params(params)?;
    let term = session.ensure_term(host).await?;
    let pty_id = host
        .lookup_tab_any(&p.tab_id)?
        .pty_id
        .ok_or_else(|| RpcError::session_not_running("tab is not running"))?;
    let res: PtyReleaseControlResult = term
        .call(
            "pty.release_control",
            json!(PtyReleaseControlParams { pty_id }),
        )
        .await?;
    Ok(json!(res))
}

#[cfg(all(test, unix))]
#[tokio::test]
async fn activity_and_seen_wait_for_catalog_subscription_reply() {
    use crate::host_service::tests::{Fixture, fixture_catalog};
    use ddg_agents_proto::rpc::Notification;
    use ddg_agents_proto::terminal::PtyEvent;

    let catalog = fixture_catalog();
    let parent = catalog.rooms[0].tabs[0].id.clone();
    let child = catalog.rooms[1].tabs[0].id.clone();
    let f = Fixture::new(catalog).await;
    let (tx, mut outgoing) = mpsc::unbounded_channel();
    let client = Arc::new(ClientSession::new(1, tx));
    let call = |method: &str, params| {
        dispatch_rpc(&f.host, &client, Ok(Request::new(1, method, params)), true)
    };
    let (reply, _, after_reply) = call("host.catalog_subscribe", json!({})).await;
    let snapshot = reply.result.as_ref().unwrap();
    let channel = snapshot["channel"].as_u64().unwrap() as u32;
    let before = snapshot["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["tab_id"] == child)
        .unwrap();
    assert_eq!(before["activity_seq"], 0);
    f.event(PtyEvent::ExternalInput {
        session_id: child.clone(),
        pty_id: 11,
        run_generation: 1,
        byte_len: 1,
        from_session_id: parent,
        from_host_id: "test".into(),
        is_remote: false,
    })
    .await;
    let (seen, _, _) = call(
        "host.tab_mark_seen",
        json!({"tab_id":child,"run_generation":1,"seen_seq":1}),
    )
    .await;
    assert_eq!(seen.result.unwrap()["seen_seq"], 1);
    tokio::task::yield_now().await;
    assert!(outgoing.try_recv().is_err());
    client
        .out_tx
        .send(
            encode_frame(
                CONTROL_CHANNEL,
                FrameKind::Data,
                &serde_json::to_vec(&reply).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    after_reply.unwrap().send(()).unwrap();
    for (ch, method, expected_seen) in [
        (CONTROL_CHANNEL, "", 0),
        (channel, "host.tab_activity_changed", 0),
        (channel, "host.tab_seen_changed", 1),
    ] {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), outgoing.recv())
            .await
            .unwrap()
            .unwrap();
        let (channel, kind, _) = decode_header(frame[..HEADER_LEN].try_into().unwrap()).unwrap();
        assert_eq!((channel, kind), (ch, FrameKind::Data));
        if ch != CONTROL_CHANNEL {
            let note: Notification = serde_json::from_slice(&frame[HEADER_LEN..]).unwrap();
            assert_eq!(note.method, method);
            assert_eq!(note.params["tab_id"], child);
            assert_eq!(note.params["activity_seq"], 1);
            assert_eq!(note.params["seen_seq"], expected_seen);
        }
    }
}
