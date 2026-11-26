//! Async framed client connection to local `ddg-terminal-server` (`terminal.sock`).
//! Used both for the host's control/event link and for per-client zero-parse
//! 9-byte binary stream passthrough (preserving distinct `conn_id`s for native
//! single-controller enforcement!).

use crate::paths::{self, Paths};
use ddg_agents_os::ipc::{connect_async, split};
use ddg_agents_proto::frame::{
    CONTROL_CHANNEL, Frame, FrameKind, HEADER_LEN, decode_header, encode_frame,
};
use ddg_agents_proto::rpc::{ErrorKind, Request, Response, RpcError};
use ddg_agents_proto::terminal::{HelloParams, HelloResult, PROTOCOL_VERSION};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

async fn ensure_terminal_server(paths: &Paths) -> io::Result<()> {
    if connect_async(&paths.terminal_sock).await.is_ok() {
        return Ok(());
    }
    paths.create_dirs()?;
    let start = ddg_agents_os::lock::FileLock::try_lock_exclusive(
        &paths.state_dir.join("terminal-start.lock"),
    );
    match &start {
        Ok(_) if connect_async(&paths.terminal_sock).await.is_err() => spawn_core(paths)?,
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
        Err(_) => {
            start?;
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if connect_async(&paths.terminal_sock).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err(io::Error::other(format!(
        "ddg-terminal-server did not listen within 5s at {}",
        paths.terminal_sock.display()
    )))
}

fn spawn_core(paths: &Paths) -> io::Result<()> {
    let bin = paths::terminal_server_bin();
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.state_dir.join("terminal-server.log"))?;
    let log_err = log.try_clone()?;

    let mut cmd = Command::new(&bin);
    cmd.arg("core")
        .env("DDG_STATE_DIR", &paths.state_dir)
        .env("DDG_RUN_DIR", &paths.run_dir)
        .env_remove("DDG_SESSION_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    ddg_agents_os::process::configure_detached(cmd.as_std_mut());
    let mut child = cmd.spawn().map_err(|e| {
        io::Error::other(format!(
            "failed to spawn ddg-terminal-server ({}): {e}",
            bin.display()
        ))
    })?;

    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

/// Sink for forwarding non-zero channel frames from `ddg-terminal-server`
/// directly to a downstream client (zero-parse passthrough) or event handler.
pub type StreamSink = mpsc::UnboundedSender<Frame>;

type PendingEntry = (oneshot::Sender<Response>, Option<StreamSink>);

pub struct TermConn {
    pub hello: HelloResult,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    pending: Arc<Mutex<HashMap<u64, PendingEntry>>>,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    streams: Arc<Mutex<HashMap<u32, StreamSink>>>,
    writer: tokio::task::AbortHandle,
}

impl TermConn {
    pub async fn connect(paths: &Paths, client_id: &str) -> io::Result<Arc<Self>> {
        ensure_terminal_server(paths).await?;
        Self::connect_existing(paths, client_id).await
    }

    pub async fn connect_existing(paths: &Paths, client_id: &str) -> io::Result<Arc<Self>> {
        let stream = connect_async(&paths.terminal_sock).await?;
        let (read_half, mut write_half) = split(stream);
        let mut reader = BufReader::new(read_half);

        // Plain NDJSON hello → upgrade to 9B binary framed mode.
        let req = Request::new(
            1,
            "hello",
            json!(HelloParams {
                protocol_version: PROTOCOL_VERSION,
                framed: true,
                client_id: Some(client_id.into()),
            }),
        );
        let line = format!("{}\n", serde_json::to_string(&req).expect("serializable"));
        write_half.write_all(line.as_bytes()).await?;

        let mut reply_line = String::new();
        reader.read_line(&mut reply_line).await?;
        let resp: Response =
            serde_json::from_str(&reply_line).map_err(|e| io::Error::other(e.to_string()))?;
        if let Some(err) = resp.error {
            return Err(io::Error::other(err.message));
        }
        let hello: HelloResult = serde_json::from_value(resp.result.unwrap_or(Value::Null))
            .map_err(|e| io::Error::other(e.to_string()))?;

        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let pending: Arc<Mutex<HashMap<u64, PendingEntry>>> = Arc::new(Mutex::new(HashMap::new()));
        let streams: Arc<Mutex<HashMap<u32, StreamSink>>> = Arc::new(Mutex::new(HashMap::new()));

        let writer = tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if write_half.write_all(&frame).await.is_err() {
                    break;
                }
            }
        });

        let writer = writer.abort_handle();
        let writer_r = writer.clone();
        let pending_r = pending.clone();
        let streams_r = streams.clone();
        let alive = Arc::new(AtomicBool::new(true));
        let alive_r = alive.clone();
        tokio::spawn(async move {
            let mut header = [0u8; HEADER_LEN];
            loop {
                if reader.read_exact(&mut header).await.is_err() {
                    break;
                }
                let Ok((channel, kind, len)) = decode_header(&header) else {
                    break;
                };
                let mut payload = vec![0u8; len];
                if reader.read_exact(&mut payload).await.is_err() {
                    break;
                }
                if channel == CONTROL_CHANNEL && kind == FrameKind::Data {
                    if let Ok(resp) = serde_json::from_slice::<Response>(&payload)
                        && let Some(id) = resp.id.as_u64()
                        && let Some((waiter, sink)) = pending_r
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .remove(&id)
                    {
                        // Register before reading the next frame; buffered stream data can arrive with the reply.
                        if let (Some(sink), Some(ch)) = (
                            sink,
                            resp.result
                                .as_ref()
                                .and_then(|r| r["channel"].as_u64())
                                .map(|c| c as u32),
                        ) {
                            streams_r
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .insert(ch, sink);
                        }
                        let _ = waiter.send(resp);
                    }
                } else {
                    let frame = Frame {
                        channel,
                        kind,
                        payload,
                    };
                    let sink = streams_r
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .get(&channel)
                        .cloned();
                    if let Some(sink) = sink {
                        let _ = sink.send(frame);
                    }
                    if kind == FrameKind::Close {
                        streams_r
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .remove(&channel);
                    }
                }
            }
            alive_r.store(false, Ordering::Release);
            writer_r.abort();
            pending_r.lock().unwrap_or_else(|p| p.into_inner()).clear();
            streams_r.lock().unwrap_or_else(|p| p.into_inner()).clear();
        });

        Ok(Arc::new(Self {
            hello,
            tx,
            pending,
            next_id: AtomicU64::new(2),
            alive,
            streams,
            writer,
        }))
    }

    pub fn close(&self) {
        self.alive.store(false, Ordering::Release);
        self.writer.abort();
    }

    pub fn detach_stream(&self, channel: u32) {
        self.streams
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&channel);
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub fn send_frame(&self, channel: u32, kind: FrameKind, payload: &[u8]) -> Result<(), String> {
        let encoded = encode_frame(channel, kind, payload)?;
        self.tx
            .send(encoded)
            .map_err(|_| "terminal server connection closed".into())
    }

    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, RpcError> {
        let val = self.call_with_stream(method, params, None).await?;
        serde_json::from_value(val)
            .map_err(|e| RpcError::new(ErrorKind::Internal, e.to_string(), None))
    }

    pub async fn call_into_stream<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        sink: StreamSink,
    ) -> Result<T, RpcError> {
        let val = self.call_with_stream(method, params, Some(sink)).await?;
        serde_json::from_value(val)
            .map_err(|e| RpcError::new(ErrorKind::Internal, e.to_string(), None))
    }

    pub async fn call_value(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.call_with_stream(method, params, None).await
    }

    async fn call_with_stream(
        &self,
        method: &str,
        params: Value,
        sink: Option<StreamSink>,
    ) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Request::new(id, method, params);
        let bytes = serde_json::to_vec(&req)
            .map_err(|e| RpcError::new(ErrorKind::Internal, e.to_string(), None))?;
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            if !self.is_alive() {
                return Err(RpcError::new(
                    ErrorKind::Unavailable,
                    "terminal connection closed",
                    None,
                ));
            }
            pending.insert(id, (tx, sink));
        }
        if self
            .send_frame(CONTROL_CHANNEL, FrameKind::Data, &bytes)
            .is_err()
        {
            self.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            return Err(RpcError::new(
                ErrorKind::Unavailable,
                "ddg-terminal-server connection closed",
                None,
            ));
        }
        let timeout = if method == "session.recv" {
            Duration::from_millis(req.params["wait_ms"].as_u64().unwrap_or(0))
                .saturating_add(Duration::from_secs(15))
        } else {
            Duration::from_secs(15)
        };
        let resp = tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| {
                self.pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                RpcError::new(
                    ErrorKind::Unavailable,
                    format!("ddg-terminal-server timed out on {method}"),
                    None,
                )
            })?
            .map_err(|_| {
                RpcError::new(
                    ErrorKind::Unavailable,
                    "ddg-terminal-server closed before reply",
                    None,
                )
            })?;
        match resp.error {
            Some(err) => Err(err),
            None => Ok(resp.result.unwrap_or(Value::Null)),
        }
    }
}
