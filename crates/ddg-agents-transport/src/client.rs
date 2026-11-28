//! Framed async client for `ddg-agents-host` with auto-reconnect, automatic
//! `CREDIT` replenishment, single-controller notifications, and strict input
//! dropping while disconnected (never queues keystrokes across reconnects).

use crate::AGENT_ACTIVITY_V1;
use crate::dial::{DialError, DialErrorKind, DialPolicy, FramedTransport, dial, dial_with_policy};
use crate::presets::BUILTIN_AGENT_PRESETS;
use crate::profile::HostProfile;
use ddg_agents_proto::frame::{
    CONTROL_CHANNEL, DATA_CHUNK, FrameKind, HEADER_LEN, INITIAL_CREDIT, MAX_PENDING, decode_header,
    decode_resize, encode_frame,
};
use ddg_agents_proto::host::{
    Catalog, CatalogSubscribeResult, ExecutorSpec, HostEvent as WireHostEvent, HostHelloResult,
    ListHostsParams, ListHostsResult, MsgRecvParams, MsgRecvResult, MsgSendParams, MsgSendResult,
    MsgSubscribeParams, MsgSubscribeResult, RelayAttachResult, ResetNotice, RoomCreateParams,
    RoomCreateResult, RoomId, RoomLayout, RoomRenameParams, RoomRenameResult, RoomSetLayoutParams,
    RoomSetLayoutResult, SessionCloseParams, SessionCloseResult, SessionCreateParams,
    SessionCreateResult, SessionListParams, SessionListResult, SessionMoveParams,
    SessionMoveResult, SessionMoveTarget, SessionRestartParams, SessionRestartResult,
    SessionScreenParams, SessionScreenResult, SessionWriteParams, SessionWriteResult, TabActivity,
    TabAttachParams, TabAttachResult, TabCloseParams, TabCloseResult, TabCreateParams,
    TabCreateResult, TabId, TabMarkSeenParams, TabMarkSeenResult, TabMoveParams, TabMsgSendParams,
    TabRestartParams, TabRestartResult, TabScreenParams, TabSplitParams, TabSplitResult,
    TabWriteParams,
};
use ddg_agents_proto::host::{
    AgentPresetDetection, FEATURE_AGENT_PRESETS_V1, HostAgentPresetsDetectParams,
    HostAgentPresetsDetectResult, PresetDetectStatus,
};
use ddg_agents_proto::rpc::{ErrorKind, Notification, Request, Response, RpcError};
use ddg_agents_proto::terminal::{
    AgentStatus, ControlMode, ForegroundHint, PingResult, PtyClaimControlResult,
    PtyReleaseControlResult,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, broadcast, mpsc, oneshot};

#[path = "stream.rs"]
mod stream;
pub use stream::{MessageStreamHandle, RelayStreamHandle, StreamError};
use stream::{NdjsonStream, StreamIngress, send_close};

const RPC_TIMEOUT: Duration = Duration::from_secs(15);
const PRESET_DETECT_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_PRESET_DETECTION_ITEMS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresetDetectOutcome {
    Unsupported,
    Detected(Vec<AgentPresetDetection>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresetDetectError {
    Rpc(RpcError),
    Protocol(String),
}

impl std::fmt::Display for PresetDetectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc(error) => f.write_str(&error.message),
            Self::Protocol(message) => write!(f, "invalid preset detection response: {message}"),
        }
    }
}

impl std::error::Error for PresetDetectError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected {
        host_id: String,
        conn_id: u64,
        features: Vec<String>,
    },
    Reconnecting {
        attempt: u32,
        reason: String,
    },
    HostKeyMismatch(String),
    AuthRequired(String),
    HostNotInstalled(String),
    ProtocolIncompatible {
        local_protocol: u32,
        /// Zero if a protocol rejection supplied no remote version number.
        remote_protocol: u32,
        remote_version: Option<String>,
        message: String,
    },
    Disconnected(String),
}

#[derive(Clone, Debug)]
pub enum HostEvent {
    StateChanged(ConnectionState),
    VersionInfo {
        app_version: Option<String>,
        platform: Option<String>,
        terminal_server_version: Option<String>,
        holder_protocol_version: Option<u32>,
        active_pty_count: Option<u32>,
        rollback_version: Option<String>,
    },
    CatalogSubscribed {
        seq: u64,
        catalog: Catalog,
        reset_notice: Option<ResetNotice>,
        activities: Vec<TabActivity>,
    },
    CatalogChanged {
        seq: u64,
        catalog: Catalog,
    },
    TabActivityChanged {
        tab_id: TabId,
        room_id: RoomId,
        run_generation: u64,
        agent_status: Option<AgentStatus>,
        foreground: Option<ForegroundHint>,
        activity_seq: u64,
        seen_seq: u64,
    },
    TabSeenChanged {
        tab_id: TabId,
        run_generation: u64,
        activity_seq: u64,
        seen_seq: u64,
    },
    TabControlChanged {
        tab_id: TabId,
        pty_id: u32,
        mode: ControlMode,
        control_epoch: u64,
    },
    ExternalInput {
        session_id: TabId,
        pty_id: u32,
        run_generation: u64,
        byte_len: u32,
        from_session_id: TabId,
        from_host_id: String,
        is_remote: bool,
    },
    SessionExited {
        session_id: TabId,
        room_id: RoomId,
        run_generation: u64,
        exit_code: Option<i32>,
    },
    StreamData {
        tab_id: TabId,
        channel: u32,
        bytes: Vec<u8>,
    },
    /// Keep `credit` while buffering these bytes; drop it after consuming or discarding them.
    DeferredStreamData {
        tab_id: TabId,
        channel: u32,
        bytes: Vec<u8>,
        credit: TabCredit,
    },
    StreamResize {
        tab_id: TabId,
        channel: u32,
        cols: u16,
        rows: u16,
    },
    StreamClose {
        tab_id: TabId,
        channel: u32,
        reason: String,
    },
}

#[derive(Clone, Debug, Deserialize)]
pub struct CatalogChange {
    pub seq: u64,
    pub catalog: Catalog,
}

pub struct CatalogSubscription {
    pub seq: u64,
    pub catalog: Catalog,
    pub reset_notice: Option<ResetNotice>,
    pub activities: Vec<TabActivity>,
    pub channel: Option<u32>,
    /// Full catalog snapshots. On lag, the next event is still a complete snapshot.
    pub events: broadcast::Receiver<CatalogChange>,
}

#[derive(Clone, Debug)]
pub struct TabStreamHandle {
    pub tab_id: TabId,
    pub channel: u32,
    pub mode: ControlMode,
    pub control_epoch: u64,
    pub cols: u16,
    pub rows: u16,
    pub run_generation: u64,
}

enum ChannelKind {
    CatalogSub {
        line: Vec<u8>,
        events: broadcast::Sender<CatalogChange>,
    },
    TabStream {
        tab_id: TabId,
        credit: Option<TabCreditWindow>,
    },
    Ndjson(StreamIngress),
}

struct TabCreditWindow {
    registration: Arc<()>,
    available: usize,
}

/// A DATA receipt bound to one connection and one tab registration. Clones share
/// the receipt; the last drop releases any bytes not explicitly granted.
#[derive(Clone, Debug)]
pub struct TabCredit(Arc<TabCreditReceipt>);

#[derive(Debug)]
struct TabCreditReceipt {
    conn: Weak<ActiveConn>,
    registration: Arc<()>,
    channel: u32,
    remaining: AtomicU32,
}

impl TabCreditReceipt {
    fn grant(&self, bytes: u32) -> bool {
        if bytes == 0 {
            return true;
        }
        let Some(conn) = self.conn.upgrade() else {
            return false;
        };
        let mut channels = conn.channels.lock().unwrap_or_else(|p| p.into_inner());
        let Some(ChannelKind::TabStream {
            credit: Some(window),
            ..
        }) = channels.get_mut(&self.channel)
        else {
            return false;
        };
        let remaining = self.remaining.load(Ordering::Relaxed);
        if !Arc::ptr_eq(&window.registration, &self.registration) || bytes > remaining {
            return false;
        }
        if !send_credit(&conn.tx, self.channel, bytes) {
            return false;
        }
        self.remaining.store(remaining - bytes, Ordering::Relaxed);
        window.available += bytes as usize;
        true
    }
}

impl Drop for TabCreditReceipt {
    fn drop(&mut self) {
        let bytes = *self.remaining.get_mut();
        self.grant(bytes);
    }
}

fn send_credit(tx: &mpsc::UnboundedSender<Vec<u8>>, channel: u32, bytes: u32) -> bool {
    encode_frame(channel, FrameKind::Credit, &bytes.to_le_bytes())
        .is_ok_and(|frame| tx.send(frame).is_ok())
}

enum PendingAttach {
    None,
    CatalogSub(broadcast::Sender<CatalogChange>),
    TabStream(TabId),
    Ndjson(StreamIngress),
}

type PendingCalls = Mutex<HashMap<u64, (oneshot::Sender<Response>, PendingAttach)>>;

struct PendingCallGuard {
    pending: Arc<PendingCalls>,
    id: u64,
}

impl Drop for PendingCallGuard {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
    }
}

struct ActiveConn {
    hello: HostHelloResult,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    pending: Arc<PendingCalls>,
    channels: Arc<Mutex<HashMap<u32, ChannelKind>>>,
    next_id: AtomicU64,
    shutdown: Arc<Notify>,
}

impl ActiveConn {
    fn supports_feature(&self, name: &str) -> bool {
        self.hello.features.iter().any(|feature| feature == name)
    }
}

pub struct HostClient {
    profile: HostProfile,
    active: Mutex<Option<Arc<ActiveConn>>>,
    state: Mutex<ConnectionState>,
    closed: AtomicBool,
    deferred_tab_credit: bool,
    event_tx: mpsc::UnboundedSender<HostEvent>,
}

impl HostClient {
    /// Connects once without background reconnection (ideal for CLI tools & tests).
    pub async fn connect_once(
        profile: HostProfile,
        client_id: impl Into<String>,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<HostEvent>), DialError> {
        Self::connect_once_with_policy(profile, client_id, DialPolicy::Standard).await
    }

    /// Connects without automatic reconnection, with an explicit host identity policy.
    pub async fn connect_once_with_policy(
        profile: HostProfile,
        client_id: impl Into<String>,
        policy: DialPolicy,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<HostEvent>), DialError> {
        Self::connect_once_inner(profile, client_id, policy, false).await
    }

    /// Connects once with tab DATA credit tied to the lifetime of each DATA receipt.
    pub async fn connect_once_with_deferred_tab_credit(
        profile: HostProfile,
        client_id: impl Into<String>,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<HostEvent>), DialError> {
        Self::connect_once_inner(profile, client_id, DialPolicy::Standard, true).await
    }

    async fn connect_once_inner(
        profile: HostProfile,
        client_id: impl Into<String>,
        policy: DialPolicy,
        deferred_tab_credit: bool,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<HostEvent>), DialError> {
        let client_id = client_id.into();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let transport = dial_with_policy(&profile, &client_id, policy).await?;
        let client = Arc::new(Self {
            profile,
            active: Mutex::new(None),
            state: Mutex::new(ConnectionState::Connecting),
            closed: AtomicBool::new(false),
            deferred_tab_credit,
            event_tx,
        });
        let done_rx = client.install_transport(transport);
        let c = client.clone();
        tokio::spawn(async move {
            let reason = done_rx.await.unwrap_or_else(|_| "connection closed".into());
            c.clear_active();
            c.set_state(ConnectionState::Disconnected(reason));
        });
        Ok((client, event_rx))
    }

    /// Starts a managed client that automatically reconnects with exponential
    /// backoff (250ms..30s) and halts immediately on `HostKeyMismatch`,
    /// `AuthRequired`, `HostIdMismatch`, or `HostNotInstalled`.
    pub fn start_managed(
        profile: HostProfile,
        client_id: impl Into<String>,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<HostEvent>) {
        Self::start_managed_inner(profile, client_id, false)
    }

    /// Starts a managed client with tab DATA credit tied to each DATA receipt.
    pub fn start_managed_with_deferred_tab_credit(
        profile: HostProfile,
        client_id: impl Into<String>,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<HostEvent>) {
        Self::start_managed_inner(profile, client_id, true)
    }

    fn start_managed_inner(
        profile: HostProfile,
        client_id: impl Into<String>,
        deferred_tab_credit: bool,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<HostEvent>) {
        let client_id = client_id.into();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let client = Arc::new(Self {
            profile: profile.clone(),
            active: Mutex::new(None),
            state: Mutex::new(ConnectionState::Connecting),
            closed: AtomicBool::new(false),
            deferred_tab_credit,
            event_tx,
        });
        let c = client.clone();
        tokio::spawn(async move {
            let mut attempt: u32 = 0;
            let mut backoff = Duration::from_millis(250);
            let mut pinned_profile = profile;

            while !c.closed.load(Ordering::Relaxed) {
                match dial(&pinned_profile, &client_id).await {
                    Ok(transport) => {
                        attempt = 0;
                        backoff = Duration::from_millis(250);
                        // Pin host_id after first successful handshake so reconnects
                        // detect unexpected host swaps!
                        if pinned_profile.expected_host_id.is_none() {
                            pinned_profile.expected_host_id = Some(transport.hello.host_id.clone());
                        }
                        let done_rx = c.install_transport(transport);
                        // The connection reader publishes initial and live catalog events in wire order.
                        let _ = c.catalog_subscribe().await;
                        let reason = done_rx.await.unwrap_or_else(|_| "disconnected".into());
                        c.clear_active();
                        if c.closed.load(Ordering::Relaxed) {
                            c.set_state(ConnectionState::Disconnected("closed".into()));
                            break;
                        }
                        attempt += 1;
                        c.set_state(ConnectionState::Reconnecting { attempt, reason });
                    }
                    Err(err) => {
                        c.clear_active();
                        match err.kind {
                            DialErrorKind::HostKeyMismatch
                            | DialErrorKind::HostIdMismatch
                            | DialErrorKind::SecurityBlocked => {
                                c.set_state(ConnectionState::HostKeyMismatch(err.message));
                                break;
                            }
                            DialErrorKind::AuthRequired => {
                                c.set_state(ConnectionState::AuthRequired(err.message));
                                break;
                            }
                            DialErrorKind::HostNotInstalled => {
                                c.set_state(ConnectionState::HostNotInstalled(err.message));
                                break;
                            }
                            DialErrorKind::InvalidProfile => {
                                c.set_state(ConnectionState::Disconnected(err.message));
                                break;
                            }
                            DialErrorKind::ProtocolIncompatible => {
                                c.set_state(ConnectionState::ProtocolIncompatible {
                                    local_protocol: ddg_agents_proto::terminal::PROTOCOL_VERSION,
                                    remote_protocol: err.remote_protocol.unwrap_or(0),
                                    remote_version: err.remote_version,
                                    message: err.message,
                                });
                                break;
                            }
                            DialErrorKind::Unavailable => {
                                attempt += 1;
                                c.set_state(ConnectionState::Reconnecting {
                                    attempt,
                                    reason: err.message,
                                });
                            }
                        }
                    }
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        });
        (client, event_rx)
    }

    pub fn profile(&self) -> &HostProfile {
        &self.profile
    }

    pub fn state(&self) -> ConnectionState {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn hello(&self) -> Option<HostHelloResult> {
        self.active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|a| a.hello.clone())
    }

    pub fn supports_feature(&self, name: &str) -> bool {
        self.connection()
            .is_some_and(|conn| conn.supports_feature(name))
    }

    pub fn supports_agent_presets(&self) -> bool {
        self.supports_feature(FEATURE_AGENT_PRESETS_V1)
    }

    /// Probe the current host once; never retry on a replacement connection.
    pub async fn detect_agent_presets(&self) -> Result<PresetDetectOutcome, PresetDetectError> {
        let conn = self.require_connection().map_err(PresetDetectError::Rpc)?;
        if !conn.supports_feature(FEATURE_AGENT_PRESETS_V1) {
            return Ok(PresetDetectOutcome::Unsupported);
        }
        let (conn, value) = Self::call_on(
            conn,
            "host.agent_presets_detect",
            json!(HostAgentPresetsDetectParams {}),
            PendingAttach::None,
            PRESET_DETECT_TIMEOUT,
        )
        .await
        .map_err(PresetDetectError::Rpc)?;
        if !self
            .connection()
            .is_some_and(|active| Arc::ptr_eq(&active, &conn))
        {
            return Err(PresetDetectError::Rpc(RpcError::new(
                ErrorKind::Unavailable,
                "host connection changed during preset detection",
                None,
            )));
        }

        let response: HostAgentPresetsDetectResult = serde_json::from_value(value)
            .map_err(|error| PresetDetectError::Protocol(error.to_string()))?;
        if response.items.len() > MAX_PRESET_DETECTION_ITEMS {
            return Err(PresetDetectError::Protocol("too many items".into()));
        }
        let mut statuses = HashMap::with_capacity(response.items.len());
        for item in response.items {
            if statuses.insert(item.id, item.status).is_some() {
                return Err(PresetDetectError::Protocol("duplicate preset ID".into()));
            }
        }
        Ok(PresetDetectOutcome::Detected(
            BUILTIN_AGENT_PRESETS
                .iter()
                .map(|preset| AgentPresetDetection {
                    id: preset.id.into(),
                    status: statuses
                        .remove(preset.id)
                        .unwrap_or(PresetDetectStatus::Unknown),
                })
                .collect(),
        ))
    }

    fn connection(&self) -> Option<Arc<ActiveConn>> {
        self.active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn require_connection(&self) -> Result<Arc<ActiveConn>, RpcError> {
        self.connection()
            .ok_or_else(|| RpcError::new(ErrorKind::Unavailable, "not connected to host", None))
    }

    pub fn is_connected(&self) -> bool {
        self.active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.clear_active();
        self.set_state(ConnectionState::Disconnected("closed by client".into()));
    }

    fn set_state(&self, new_state: ConnectionState) {
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) = new_state.clone();
        let _ = self.event_tx.send(HostEvent::StateChanged(new_state));
    }

    fn clear_active(&self) {
        let prev = self.active.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(conn) = prev {
            conn.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clear();
            conn.channels
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clear();
            conn.shutdown.notify_one();
        }
    }

    fn install_transport(
        self: &Arc<Self>,
        transport: FramedTransport,
    ) -> oneshot::Receiver<String> {
        let FramedTransport {
            hello,
            mut reader,
            mut writer,
            guard,
        } = transport;
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let pending = Arc::new(PendingCalls::default());
        let channels: Arc<Mutex<HashMap<u32, ChannelKind>>> = Arc::new(Mutex::new(HashMap::new()));
        let shutdown = Arc::new(Notify::new());

        let active = Arc::new(ActiveConn {
            hello: hello.clone(),
            tx: tx.clone(),
            pending: pending.clone(),
            channels: channels.clone(),
            next_id: AtomicU64::new(2),
            shutdown: shutdown.clone(),
        });
        let conn = Arc::downgrade(&active);
        let deferred_tab_credit = self.deferred_tab_credit;
        *self.active.lock().unwrap_or_else(|p| p.into_inner()) = Some(active);
        self.set_state(ConnectionState::Connected {
            host_id: hello.host_id,
            conn_id: hello.conn_id,
            features: hello.features,
        });
        let _ = self.event_tx.send(HostEvent::VersionInfo {
            app_version: hello.app_version,
            platform: hello.platform,
            terminal_server_version: hello.terminal_server_version,
            holder_protocol_version: hello.holder_protocol_version,
            active_pty_count: hello.active_pty_count,
            rollback_version: hello.rollback_version,
        });

        let (done_tx, done_rx) = oneshot::channel();
        let event_tx = self.event_tx.clone();

        let mut writer_task = tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if writer.write_all(&frame).await.is_err() {
                    break;
                }
            }
        });

        tokio::spawn(async move {
            let _guard = guard;
            let read_frames = async {
                let mut header = [0u8; HEADER_LEN];
                loop {
                    if let Err(e) = reader.read_exact(&mut header).await {
                        break format!("stream closed: {e}");
                    }
                    let (channel, kind, len) = match decode_header(&header) {
                        Ok(h) => h,
                        Err(e) => break format!("invalid frame header: {e}"),
                    };
                    let mut payload = vec![0u8; len];
                    if let Err(e) = reader.read_exact(&mut payload).await {
                        break format!("stream payload closed: {e}");
                    }

                    if channel == CONTROL_CHANNEL && kind == FrameKind::Data {
                        if let Ok(val) = serde_json::from_slice::<Value>(&payload) {
                            if let Some(id) = val.get("id").and_then(Value::as_u64) {
                                if let Ok(mut resp) = serde_json::from_value::<Response>(val)
                                    && let Some((waiter, attach)) = pending
                                        .lock()
                                        .unwrap_or_else(|p| p.into_inner())
                                        .remove(&id)
                                {
                                    let mut ch_map =
                                        channels.lock().unwrap_or_else(|p| p.into_inner());
                                    let attached = if resp.error.is_none() {
                                        match attach.register(
                                            resp.result.as_ref(),
                                            &mut ch_map,
                                            &event_tx,
                                            deferred_tab_credit,
                                        ) {
                                            Ok(ch) => ch,
                                            Err(err) => {
                                                resp.result = None;
                                                resp.error = Some(err);
                                                None
                                            }
                                        }
                                    } else {
                                        None
                                    };
                                    if waiter.send(resp).is_err()
                                        && let Some(ch) = attached
                                    {
                                        ch_map.remove(&ch);
                                        send_close(&tx, ch);
                                    }
                                }
                            } else if let Ok(note) = serde_json::from_value::<Notification>(val)
                                && let Some(event) = host_notification(note, false)
                            {
                                let _ = event_tx.send(event);
                            }
                        }
                        continue;
                    }

                    let mut ch_guard = channels.lock().unwrap_or_else(|p| p.into_inner());
                    match ch_guard.get_mut(&channel) {
                        Some(ChannelKind::Ndjson(stream)) => {
                            if !stream.on_frame(&tx, channel, kind, payload) {
                                ch_guard.remove(&channel);
                            }
                            continue;
                        }
                        Some(ChannelKind::TabStream {
                            tab_id,
                            credit: Some(window),
                        }) if kind == FrameKind::Data => {
                            let Some(available) = window.available.checked_sub(payload.len())
                            else {
                                break "tab DATA exceeded receive credit".into();
                            };
                            window.available = available;
                            let registration = window.registration.clone();
                            let tab_id = tab_id.clone();
                            // Failed sends drop receipts, which also lock the channel map.
                            drop(ch_guard);
                            let send = |bytes: Vec<u8>| {
                                let credit = TabCredit(Arc::new(TabCreditReceipt {
                                    conn: conn.clone(),
                                    channel,
                                    registration: registration.clone(),
                                    remaining: AtomicU32::new(bytes.len() as u32),
                                }));
                                event_tx
                                    .send(HostEvent::DeferredStreamData {
                                        tab_id: tab_id.clone(),
                                        channel,
                                        bytes,
                                        credit,
                                    })
                                    .is_ok()
                            };
                            let sent = if payload.len() <= DATA_CHUNK {
                                payload.is_empty() || send(payload)
                            } else {
                                payload.chunks(DATA_CHUNK).all(|bytes| send(bytes.to_vec()))
                            };
                            if !sent {
                                break "tab DATA consumer closed".into();
                            }
                            continue;
                        }
                        Some(_) if kind == FrameKind::Data && !payload.is_empty() => {
                            send_credit(&tx, channel, payload.len() as u32);
                        }
                        _ => {}
                    }
                    match ch_guard.get_mut(&channel) {
                        Some(ChannelKind::CatalogSub { line, events }) => match kind {
                            FrameKind::Close => {
                                ch_guard.remove(&channel);
                            }
                            FrameKind::Data => {
                                line.extend_from_slice(&payload);
                                let mut consumed = 0;
                                for record in line.split_inclusive(|&b| b == b'\n') {
                                    if record.last() != Some(&b'\n') {
                                        break;
                                    }
                                    consumed += record.len();
                                    if record.len() <= MAX_PENDING
                                        && let Ok(note) =
                                            serde_json::from_slice::<Notification>(record)
                                        && let Some(event) = host_notification(note, true)
                                    {
                                        if let HostEvent::CatalogChanged { seq, catalog } = &event
                                            && events.receiver_count() != 0
                                        {
                                            let _ = events.send(CatalogChange {
                                                seq: *seq,
                                                catalog: catalog.clone(),
                                            });
                                        }
                                        let _ = event_tx.send(event);
                                    }
                                }
                                line.drain(..consumed);
                                if line.len() > MAX_PENDING {
                                    ch_guard.remove(&channel);
                                    send_close(&tx, channel);
                                }
                            }
                            _ => {}
                        },
                        Some(ChannelKind::TabStream { tab_id, .. }) => {
                            let tab_id = tab_id.clone();
                            match kind {
                                FrameKind::Data => {
                                    let _ = event_tx.send(HostEvent::StreamData {
                                        tab_id,
                                        channel,
                                        bytes: payload,
                                    });
                                }
                                FrameKind::Resize => {
                                    if let Ok((cols, rows)) = decode_resize(&payload) {
                                        let _ = event_tx.send(HostEvent::StreamResize {
                                            tab_id,
                                            channel,
                                            cols,
                                            rows,
                                        });
                                    }
                                }
                                FrameKind::Close => {
                                    ch_guard.remove(&channel);
                                    let reason = String::from_utf8_lossy(&payload).into_owned();
                                    let _ = event_tx.send(HostEvent::StreamClose {
                                        tab_id,
                                        channel,
                                        reason,
                                    });
                                }
                                FrameKind::Credit => {}
                            }
                        }
                        Some(ChannelKind::Ndjson(_)) | None => {}
                    }
                }
            };
            let reason = tokio::select! {
                reason = read_frames => reason,
                _ = shutdown.notified() => "closed by client".into(),
                _ = &mut writer_task => "stream writer closed".into(),
            };
            pending.lock().unwrap_or_else(|p| p.into_inner()).clear();
            channels.lock().unwrap_or_else(|p| p.into_inner()).clear();
            writer_task.abort();
            let _ = done_tx.send(reason);
        });

        done_rx
    }

    /// Explicitly grants consumed or discarded bytes, up to the receipt's remaining bytes.
    /// A numeric channel alone cannot identify a stream across detach or reconnect.
    pub fn grant_tab_credit(channel: &TabCredit, bytes: u32) -> bool {
        channel.0.grant(bytes)
    }

    /// Sends raw terminal input bytes on an attached channel.
    /// Strictly drops input when disconnected (returns `false`) and NEVER queues.
    pub fn send_input(&self, channel: u32, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return true;
        }
        let conn = self
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let Some(conn) = conn else {
            return false;
        };
        let Ok(frame) = encode_frame(channel, FrameKind::Data, bytes) else {
            return false;
        };
        conn.tx.send(frame).is_ok()
    }

    /// Sends terminal resize on an attached channel. Dropped when disconnected.
    pub fn send_resize(&self, channel: u32, cols: u16, rows: u16) -> bool {
        let conn = self
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let Some(conn) = conn else {
            return false;
        };
        let mut payload = [0u8; 4];
        payload[0..2].copy_from_slice(&cols.to_le_bytes());
        payload[2..4].copy_from_slice(&rows.to_le_bytes());
        let Ok(frame) = encode_frame(channel, FrameKind::Resize, &payload) else {
            return false;
        };
        conn.tx.send(frame).is_ok()
    }

    /// Detaches an attached tab stream without killing the remote PTY.
    pub fn detach_channel(&self, channel: u32) {
        let conn = self
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(conn) = conn {
            conn.channels
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&channel);
            send_close(&conn.tx, channel);
        }
    }

    pub async fn call_raw(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.call_internal(method, params, PendingAttach::None, RPC_TIMEOUT)
            .await
            .map(|(_, value)| value)
    }

    async fn call_internal(
        &self,
        method: &str,
        params: Value,
        attach: PendingAttach,
        timeout: Duration,
    ) -> Result<(Arc<ActiveConn>, Value), RpcError> {
        Self::call_on(self.require_connection()?, method, params, attach, timeout).await
    }

    async fn call_on(
        conn: Arc<ActiveConn>,
        method: &str,
        params: Value,
        attach: PendingAttach,
        timeout: Duration,
    ) -> Result<(Arc<ActiveConn>, Value), RpcError> {
        let id = conn.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Request::new(id, method, params);
        let bytes = serde_json::to_vec(&req)
            .map_err(|e| RpcError::new(ErrorKind::Internal, e.to_string(), None))?;
        let frame = encode_frame(CONTROL_CHANNEL, FrameKind::Data, &bytes)
            .map_err(|e| RpcError::new(ErrorKind::Internal, e, None))?;
        let (tx, rx) = oneshot::channel();
        // Stream replies need their attachment long enough to close canceled channels.
        let _guard = matches!(attach, PendingAttach::None).then(|| PendingCallGuard {
            pending: conn.pending.clone(),
            id,
        });
        conn.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, (tx, attach));
        if conn.tx.send(frame).is_err() {
            conn.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            return Err(RpcError::new(
                ErrorKind::Unavailable,
                "host connection closed",
                None,
            ));
        }
        let resp = tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| {
                conn.pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                RpcError::new(
                    ErrorKind::Unavailable,
                    format!("RPC {method} timed out"),
                    None,
                )
            })?
            .map_err(|_| {
                RpcError::new(
                    ErrorKind::Unavailable,
                    "host connection closed before reply",
                    None,
                )
            })?;
        match resp.error {
            Some(err) => Err(err),
            None => Ok((conn, resp.result.unwrap_or(Value::Null))),
        }
    }

    async fn call_typed_on<T: DeserializeOwned>(
        conn: Arc<ActiveConn>,
        method: &str,
        params: Value,
    ) -> Result<T, RpcError> {
        let (_, value) =
            Self::call_on(conn, method, params, PendingAttach::None, RPC_TIMEOUT).await?;
        decode_result(value)
    }

    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, RpcError> {
        let val = self.call_raw(method, params).await?;
        decode_result(val)
    }

    pub async fn ping(&self) -> Result<PingResult, RpcError> {
        self.call("ping", json!({})).await
    }

    pub async fn discover(&self) -> Result<Value, RpcError> {
        self.call_raw("rpc.discover", json!({})).await
    }

    pub async fn catalog_subscribe(&self) -> Result<CatalogSubscription, RpcError> {
        let (events, updates) = broadcast::channel(16);
        let (_, value) = self
            .call_internal(
                "host.catalog_subscribe",
                json!({}),
                PendingAttach::CatalogSub(events),
                RPC_TIMEOUT,
            )
            .await?;
        let CatalogSubscribeResult {
            seq,
            catalog,
            reset_notice,
            activities,
            channel,
        } = decode_result(value)?;
        Ok(CatalogSubscription {
            seq,
            catalog,
            reset_notice,
            activities,
            channel,
            events: updates,
        })
    }

    pub async fn relay_attach(&self) -> Result<RelayStreamHandle, RpcError> {
        self.open_ndjson("host.relay_attach", json!({}), |value| {
            decode_result::<RelayAttachResult>(value).map(|result| result.channel)
        })
        .await
    }

    pub async fn msg_subscribe(
        &self,
        params: MsgSubscribeParams,
    ) -> Result<MessageStreamHandle, RpcError> {
        self.open_ndjson("host.msg_subscribe", json!(params), |value| {
            decode_result::<MsgSubscribeResult>(value).map(|result| result.channel)
        })
        .await
    }

    async fn open_ndjson<T>(
        &self,
        method: &str,
        params: Value,
        channel: impl FnOnce(Value) -> Result<u32, RpcError>,
    ) -> Result<NdjsonStream<T>, RpcError> {
        let (ingress, rx) = StreamIngress::new();
        let (connection, value) = self
            .call_internal(method, params, PendingAttach::Ndjson(ingress), RPC_TIMEOUT)
            .await?;
        Ok(NdjsonStream::new(connection, channel(value)?, rx))
    }

    pub async fn msg_recv(&self, params: MsgRecvParams) -> Result<MsgRecvResult, RpcError> {
        let timeout = RPC_TIMEOUT + Duration::from_millis(params.wait_ms);
        let (_, value) = self
            .call_internal("host.msg_recv", json!(params), PendingAttach::None, timeout)
            .await?;
        decode_result(value)
    }

    pub async fn room_create(
        &self,
        name: Option<String>,
        space_cwd: Option<String>,
        executor: ExecutorSpec,
        cols: u16,
        rows: u16,
        request_id: Option<String>,
    ) -> Result<RoomCreateResult, RpcError> {
        self.room_create_with_agent(RoomCreateParams {
            agent_profile: None,
            request_id,
            name,
            space_cwd,
            executor,
            cols,
            rows,
        })
        .await
    }

    pub async fn room_create_with_agent(
        &self,
        mut params: RoomCreateParams,
    ) -> Result<RoomCreateResult, RpcError> {
        let conn = self.require_connection()?;
        if !conn.supports_feature(AGENT_ACTIVITY_V1) {
            params.agent_profile = None;
        }
        Self::call_typed_on(conn, "host.room_create", json!(params)).await
    }

    pub async fn room_rename(
        &self,
        room_id: RoomId,
        name: Option<String>,
    ) -> Result<RoomRenameResult, RpcError> {
        self.call(
            "host.room_rename",
            json!(RoomRenameParams { room_id, name }),
        )
        .await
    }

    pub async fn room_set_layout(
        &self,
        room_id: RoomId,
        layout: Option<RoomLayout>,
    ) -> Result<RoomSetLayoutResult, RpcError> {
        self.call(
            "host.room_set_layout",
            json!(RoomSetLayoutParams { room_id, layout }),
        )
        .await
    }

    pub async fn tab_split(&self, params: TabSplitParams) -> Result<TabSplitResult, RpcError> {
        self.tab_split_with_agent(params).await
    }

    pub async fn tab_split_with_agent(
        &self,
        mut params: TabSplitParams,
    ) -> Result<TabSplitResult, RpcError> {
        let conn = self.require_connection()?;
        if !conn.supports_feature(AGENT_ACTIVITY_V1) {
            params.agent_profile = None;
        }
        Self::call_typed_on(conn, "host.tab_split", json!(params)).await
    }

    pub async fn tab_create(
        &self,
        room_id: RoomId,
        space_cwd: Option<String>,
        executor: ExecutorSpec,
        cols: u16,
        rows: u16,
        request_id: Option<String>,
    ) -> Result<TabCreateResult, RpcError> {
        self.tab_create_ext(
            Some(room_id),
            space_cwd,
            None,
            executor,
            cols,
            rows,
            request_id,
        )
        .await
    }

    /// Omit `room_id` to create a tab in the host's hidden background room.
    #[allow(clippy::too_many_arguments)]
    pub async fn tab_create_ext(
        &self,
        room_id: Option<RoomId>,
        space_cwd: Option<String>,
        title: Option<String>,
        executor: ExecutorSpec,
        cols: u16,
        rows: u16,
        request_id: Option<String>,
    ) -> Result<TabCreateResult, RpcError> {
        self.tab_create_with_agent(TabCreateParams {
            agent_profile: None,
            request_id,
            room_id,
            background: false,
            space_cwd,
            title,
            executor,
            cols,
            rows,
        })
        .await
    }

    pub async fn tab_create_with_agent(
        &self,
        mut params: TabCreateParams,
    ) -> Result<TabCreateResult, RpcError> {
        let conn = self.require_connection()?;
        if !conn.supports_feature(AGENT_ACTIVITY_V1) {
            params.agent_profile = None;
        }
        Self::call_typed_on(conn, "host.tab_create", json!(params)).await
    }

    /// Returns `None` without an RPC when activity was not negotiated.
    pub async fn tab_mark_seen(
        &self,
        tab_id: TabId,
        run_generation: u64,
        seen_seq: u64,
    ) -> Result<Option<TabMarkSeenResult>, RpcError> {
        let Some(conn) = self
            .connection()
            .filter(|conn| conn.supports_feature(AGENT_ACTIVITY_V1))
        else {
            return Ok(None);
        };
        Self::call_typed_on(
            conn,
            "host.tab_mark_seen",
            json!(TabMarkSeenParams {
                tab_id,
                run_generation,
                seen_seq
            }),
        )
        .await
        .map(Some)
    }

    pub async fn tab_screen(
        &self,
        tab_id: TabId,
        lines: u32,
        max_bytes: u32,
    ) -> Result<SessionScreenResult, RpcError> {
        self.call(
            "host.tab_screen",
            json!(TabScreenParams {
                tab_id,
                lines,
                max_bytes,
            }),
        )
        .await
    }

    pub async fn tab_write(
        &self,
        tab_id: TabId,
        data_b64: String,
        expect_run_generation: Option<u64>,
    ) -> Result<SessionWriteResult, RpcError> {
        self.call(
            "host.tab_write",
            json!(TabWriteParams {
                tab_id,
                data_b64,
                expect_run_generation,
            }),
        )
        .await
    }

    pub async fn tab_msg_send(
        &self,
        to_session_id: TabId,
        data_b64: String,
    ) -> Result<MsgSendResult, RpcError> {
        self.call(
            "host.tab_msg_send",
            json!(TabMsgSendParams {
                to_session_id,
                data_b64,
            }),
        )
        .await
    }

    pub async fn tab_move(
        &self,
        tab_id: TabId,
        target: SessionMoveTarget,
    ) -> Result<SessionMoveResult, RpcError> {
        self.call("host.tab_move", json!(TabMoveParams { tab_id, target }))
            .await
    }

    pub async fn tab_attach(
        &self,
        tab_id: TabId,
        want_control: bool,
        cols: u16,
        rows: u16,
        history_lines: u32,
    ) -> Result<TabStreamHandle, RpcError> {
        let (_, val) = self
            .call_internal(
                "host.tab_attach",
                json!(TabAttachParams {
                    tab_id: tab_id.clone(),
                    want_control,
                    cols,
                    rows,
                    history_lines,
                }),
                PendingAttach::TabStream(tab_id.clone()),
                RPC_TIMEOUT,
            )
            .await?;
        let res: TabAttachResult = decode_result(val)?;
        Ok(TabStreamHandle {
            tab_id,
            channel: res.channel,
            mode: res.mode,
            control_epoch: res.control_epoch,
            cols: res.cols,
            rows: res.rows,
            run_generation: res.run_generation,
        })
    }

    pub async fn tab_claim_control(
        &self,
        tab_id: TabId,
        force: bool,
    ) -> Result<PtyClaimControlResult, RpcError> {
        self.call(
            "host.tab_claim_control",
            json!({ "tab_id": tab_id, "force": force }),
        )
        .await
    }

    pub async fn tab_release_control(
        &self,
        tab_id: TabId,
    ) -> Result<PtyReleaseControlResult, RpcError> {
        self.call("host.tab_release_control", json!({ "tab_id": tab_id }))
            .await
    }

    pub async fn tab_close(
        &self,
        tab_id: TabId,
        request_id: Option<String>,
    ) -> Result<TabCloseResult, RpcError> {
        self.call(
            "host.tab_close",
            json!(TabCloseParams { tab_id, request_id }),
        )
        .await
    }

    pub async fn tab_restart(
        &self,
        tab_id: TabId,
        cols: u16,
        rows: u16,
    ) -> Result<TabRestartResult, RpcError> {
        self.call(
            "host.tab_restart",
            json!(TabRestartParams { tab_id, cols, rows }),
        )
        .await
    }
}

macro_rules! host_rpc_methods {
    ($($name:ident: $params:ty => $result:ty,)*) => {
        impl HostClient {
            $(
                pub async fn $name(&self, params: $params) -> Result<$result, RpcError> {
                    self.call(concat!("host.", stringify!($name)), json!(params)).await
                }
            )*
        }
    };
}

host_rpc_methods! {
    list_hosts: ListHostsParams => ListHostsResult,
    session_create: SessionCreateParams => SessionCreateResult,
    session_list: SessionListParams => SessionListResult,
    session_move: SessionMoveParams => SessionMoveResult,
    session_close: SessionCloseParams => SessionCloseResult,
    session_restart: SessionRestartParams => SessionRestartResult,
    session_screen: SessionScreenParams => SessionScreenResult,
    session_write: SessionWriteParams => SessionWriteResult,
    msg_send: MsgSendParams => MsgSendResult,
}

fn decode_result<T: DeserializeOwned>(value: Value) -> Result<T, RpcError> {
    serde_json::from_value(value)
        .map_err(|e| RpcError::new(ErrorKind::Internal, e.to_string(), None))
}

impl PendingAttach {
    fn register(
        self,
        value: Option<&Value>,
        channels: &mut HashMap<u32, ChannelKind>,
        events: &mpsc::UnboundedSender<HostEvent>,
        deferred_tab_credit: bool,
    ) -> Result<Option<u32>, RpcError> {
        if matches!(self, Self::None) {
            return Ok(None);
        }
        let result = value.unwrap_or(&Value::Null);
        let channel = result["channel"]
            .as_u64()
            .and_then(|id| u32::try_from(id).ok())
            .filter(|&id| id != CONTROL_CHANNEL);
        if channel.is_some_and(|ch| channels.contains_key(&ch))
            || (channel.is_none()
                && !(matches!(self, Self::CatalogSub(_)) && result["channel"].is_null()))
        {
            return Err(RpcError::new(
                ErrorKind::Internal,
                "invalid or duplicate stream channel",
                None,
            ));
        }

        let stream = match self {
            Self::None => return Ok(None),
            Self::CatalogSub(updates) => {
                let CatalogSubscribeResult {
                    seq,
                    catalog,
                    reset_notice,
                    activities,
                    ..
                } = CatalogSubscribeResult::deserialize(result)
                    .map_err(|e| RpcError::new(ErrorKind::Internal, e.to_string(), None))?;
                let _ = events.send(HostEvent::CatalogSubscribed {
                    seq,
                    catalog,
                    reset_notice,
                    activities,
                });
                ChannelKind::CatalogSub {
                    line: Vec::new(),
                    events: updates,
                }
            }
            Self::TabStream(tab_id) => {
                TabAttachResult::deserialize(result)
                    .map_err(|e| RpcError::new(ErrorKind::Internal, e.to_string(), None))?;
                ChannelKind::TabStream {
                    tab_id,
                    credit: deferred_tab_credit.then(|| TabCreditWindow {
                        registration: Arc::new(()),
                        available: INITIAL_CREDIT,
                    }),
                }
            }
            Self::Ndjson(stream) => ChannelKind::Ndjson(stream),
        };
        if let Some(ch) = channel {
            channels.insert(ch, stream);
        }
        Ok(channel)
    }
}

fn host_notification(note: Notification, catalog_channel: bool) -> Option<HostEvent> {
    let mut params = note.params;
    match (catalog_channel, note.method.as_str()) {
        (false, "host.tab_control_changed") => Some(HostEvent::TabControlChanged {
            tab_id: params["tab_id"].as_str()?.to_owned(),
            pty_id: u32::try_from(params["pty_id"].as_u64()?).ok()?,
            mode: ControlMode::deserialize(&params["mode"]).ok()?,
            control_epoch: params["control_epoch"].as_u64()?,
        }),
        (_, "host.catalog_changed") => {
            let CatalogChange { seq, catalog } = serde_json::from_value(params).ok()?;
            Some(HostEvent::CatalogChanged { seq, catalog })
        }
        (false, "host.external_input" | "host.session_exited")
        | (true, "host.tab_activity_changed" | "host.tab_seen_changed") => {
            let method = note.method.strip_prefix("host.")?;
            params.as_object_mut()?.insert("type".into(), json!(method));
            match serde_json::from_value(params).ok()? {
                WireHostEvent::TabActivityChanged {
                    tab_id,
                    room_id,
                    run_generation,
                    agent_status,
                    foreground,
                    activity_seq,
                    seen_seq,
                } => Some(HostEvent::TabActivityChanged {
                    tab_id,
                    room_id,
                    run_generation,
                    agent_status,
                    foreground,
                    activity_seq,
                    seen_seq,
                }),
                WireHostEvent::TabSeenChanged {
                    tab_id,
                    run_generation,
                    activity_seq,
                    seen_seq,
                } => Some(HostEvent::TabSeenChanged {
                    tab_id,
                    run_generation,
                    activity_seq,
                    seen_seq,
                }),
                WireHostEvent::ExternalInput {
                    session_id,
                    pty_id,
                    run_generation,
                    byte_len,
                    from_session_id,
                    from_host_id,
                    is_remote,
                } => Some(HostEvent::ExternalInput {
                    session_id,
                    pty_id,
                    run_generation,
                    byte_len,
                    from_session_id,
                    from_host_id,
                    is_remote,
                }),
                WireHostEvent::SessionExited {
                    session_id,
                    room_id,
                    run_generation,
                    exit_code,
                } => Some(HostEvent::SessionExited {
                    session_id,
                    room_id,
                    run_generation,
                    exit_code,
                }),
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dial::TransportGuard;
    use ddg_agents_proto::frame::{Frame, INITIAL_CREDIT};
    use ddg_agents_proto::host::RelayFrame;
    use ddg_agents_proto::terminal::CloseReason;
    use tokio::io::{BufReader, DuplexStream, duplex, split};

    #[tokio::test]
    async fn protocol_incompatibility_stops_managed_reconnect() {
        use ddg_agents_os::ipc::LocalListener;
        use tokio::io::AsyncBufReadExt;

        // Use a short private directory to stay below the Unix socket limit.
        let dir = std::env::temp_dir().join(format!("ddg-incompat-{}", std::process::id()));
        ddg_agents_os::fs::ensure_private_dir(&dir).unwrap();
        let socket = dir.join("host.sock");
        let listener = LocalListener::bind(&socket).unwrap().into_tokio().unwrap();
        let profile = HostProfile {
            transport: crate::HostTransport::Local {
                sock_path: Some(socket.to_str().unwrap().into()),
            },
            ..HostProfile::local_default()
        };
        let (client, mut events) = HostClient::start_managed(profile, "test");
        let mut peer = BufReader::new(listener.accept().await.unwrap().0);
        let mut request = String::new();
        peer.read_line(&mut request).await.unwrap();
        peer.get_mut().write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocol_version\":99,\"host_id\":\"peer\",\"boot_generation\":1,\"conn_id\":1,\"app_version\":\"9.9.9\"}}\n").await.unwrap();
        assert!(
            matches!(event(&mut events).await, HostEvent::StateChanged(ConnectionState::ProtocolIncompatible { remote_protocol: 99, remote_version: Some(version), .. }) if version == "9.9.9")
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(500), listener.accept())
                .await
                .is_err()
        );
        drop(client);
        drop(listener);
        std::fs::remove_dir(dir).unwrap();
    }

    fn mock_client() -> (
        Arc<HostClient>,
        mpsc::UnboundedReceiver<HostEvent>,
        DuplexStream,
    ) {
        mock_client_with_features(vec![])
    }

    fn mock_client_with_features(
        features: Vec<String>,
    ) -> (
        Arc<HostClient>,
        mpsc::UnboundedReceiver<HostEvent>,
        DuplexStream,
    ) {
        mock_client_with_options(features, false)
    }

    fn mock_client_with_options(
        features: Vec<String>,
        deferred_tab_credit: bool,
    ) -> (
        Arc<HostClient>,
        mpsc::UnboundedReceiver<HostEvent>,
        DuplexStream,
    ) {
        let (event_tx, mut events) = mpsc::unbounded_channel();
        let client = Arc::new(HostClient {
            profile: HostProfile::local_default(),
            active: Mutex::new(None),
            state: Mutex::new(ConnectionState::Connecting),
            closed: AtomicBool::new(false),
            deferred_tab_credit,
            event_tx,
        });
        let peer = install_mock_connection(&client, features);
        assert!(matches!(
            events.try_recv().unwrap(),
            HostEvent::StateChanged(ConnectionState::Connected { .. })
        ));
        assert!(
            matches!(events.try_recv().unwrap(), HostEvent::VersionInfo {
            app_version: Some(app), platform: Some(platform), terminal_server_version: Some(terminal),
            holder_protocol_version: Some(3), active_pty_count: Some(4), rollback_version: Some(rollback),
        } if app == "1.2.3" && platform == "linux-x86_64" && terminal == "1.2.2" && rollback == "1.2.1")
        );
        (client, events, peer)
    }

    fn install_mock_connection(client: &Arc<HostClient>, features: Vec<String>) -> DuplexStream {
        let (client_io, peer) = duplex(32 * 1024);
        let (reader, writer) = split(client_io);
        let _done = client.install_transport(FramedTransport {
            hello: HostHelloResult {
                protocol_version: 1,
                host_id: "peer".into(),
                boot_generation: 1,
                conn_id: 1,
                features,
                app_version: Some("1.2.3".into()),
                platform: Some("linux-x86_64".into()),
                terminal_server_version: Some("1.2.2".into()),
                holder_protocol_version: Some(3),
                active_pty_count: Some(4),
                rollback_version: Some("1.2.1".into()),
            },
            reader: BufReader::new(Box::new(reader)),
            writer: Box::new(writer),
            guard: TransportGuard::default(),
        });
        peer
    }

    async fn read_frame(peer: &mut DuplexStream) -> Frame {
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut header = [0; HEADER_LEN];
            peer.read_exact(&mut header).await.unwrap();
            let (channel, kind, len) = decode_header(&header).unwrap();
            let mut payload = vec![0; len];
            peer.read_exact(&mut payload).await.unwrap();
            Frame {
                channel,
                kind,
                payload,
            }
        })
        .await
        .expect("frame timed out")
    }

    async fn rpc_reply(peer: &mut DuplexStream, method: &str, params: Value, result: Value) {
        let frame = read_frame(peer).await;
        assert_eq!(
            (frame.channel, frame.kind),
            (CONTROL_CHANNEL, FrameKind::Data)
        );
        let request: Request = serde_json::from_slice(&frame.payload).unwrap();
        assert_eq!(request.method, method);
        assert_eq!(request.params, params);
        let response = Response::ok(request.id, result);
        peer.write_all(
            &Frame::data(CONTROL_CHANNEL, serde_json::to_vec(&response).unwrap())
                .encode()
                .unwrap(),
        )
        .await
        .unwrap();
    }

    async fn notify(peer: &mut DuplexStream, channel: u32, method: &str, params: Value) {
        let mut bytes =
            serde_json::to_vec(&ddg_agents_proto::rpc::Notification::new(method, params)).unwrap();
        if channel != CONTROL_CHANNEL {
            bytes.push(b'\n');
        }
        peer.write_all(&Frame::data(channel, bytes).encode().unwrap())
            .await
            .unwrap();
    }

    async fn event(events: &mut mpsc::UnboundedReceiver<HostEvent>) -> HostEvent {
        tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap()
    }

    const ROOM_ID: &str = "rm_00000000000000000000000001";
    const TAB_ID: &str = "ses_00000000000000000000000001";
    const SENDER_ID: &str = "ses_00000000000000000000000002";

    #[tokio::test]
    async fn v2_ids_and_attached_run_generation() {
        let (client, mut events, mut peer) = mock_client();
        let serve = tokio::spawn(async move {
            rpc_reply(&mut peer, "host.room_rename", json!({"room_id": ROOM_ID, "name": "new"}),
                json!({"room": {"id": ROOM_ID, "kind": "standard", "visibility": "visible", "name": "new", "auto_name": "room", "tabs": []}})).await;
            rpc_reply(&mut peer, "host.tab_attach", json!({"tab_id": TAB_ID, "want_control": true, "cols": 80, "rows": 24, "history_lines": 10}),
                json!({"channel": 3, "run_generation": 8, "mode": "control", "control_epoch": 1, "cols": 80, "rows": 24, "runtime": {"state": "running", "pty_id": 7, "pid": 8}})).await;
            peer.write_all(&Frame::data(3, b"paint".to_vec()).encode().unwrap())
                .await
                .unwrap();
            notify(
                &mut peer,
                0,
                "host.tab_control_changed",
                json!({"tab_id": TAB_ID, "pty_id": 7, "mode": "readonly", "control_epoch": 2}),
            )
            .await;
            assert_eq!(read_frame(&mut peer).await, Frame::credit(3, 5));
        });
        let room = client
            .room_rename(ROOM_ID.into(), Some("new".into()))
            .await
            .unwrap()
            .room;
        assert_eq!(room.id, ROOM_ID);
        let tab = client
            .tab_attach(TAB_ID.into(), true, 80, 24, 10)
            .await
            .unwrap();
        assert_eq!(tab.tab_id, TAB_ID);
        assert_eq!(tab.run_generation, 8);
        assert!(
            matches!(event(&mut events).await, HostEvent::StreamData { tab_id, channel: 3, bytes } if tab_id == TAB_ID && bytes == b"paint")
        );
        assert!(
            matches!(event(&mut events).await, HostEvent::TabControlChanged { tab_id, pty_id: 7, mode: ControlMode::Readonly, control_epoch: 2 } if tab_id == TAB_ID)
        );
        serve.await.unwrap();
    }

    async fn mock_attach(client: &HostClient, peer: &mut DuplexStream) {
        let (attached, _) = tokio::join!(
            client.tab_attach(TAB_ID.into(), true, 80, 24, 0),
            rpc_reply(
                peer,
                "host.tab_attach",
                json!({"tab_id": TAB_ID, "want_control": true, "cols": 80, "rows": 24, "history_lines": 0}),
                json!({"channel": 3, "run_generation": 8, "mode": "control", "control_epoch": 1, "cols": 80, "rows": 24, "runtime": {"state": "running", "pty_id": 7, "pid": 8}})
            ),
        );
        assert_eq!(attached.unwrap().channel, 3);
    }

    #[tokio::test]
    async fn deferred_credit_splits_oversized_data_and_cannot_cross_registrations() {
        let (client, mut events, mut peer) = mock_client_with_options(vec![], true);
        mock_attach(&client, &mut peer).await;
        peer.write_all(&Frame::data(3, vec![b'a'; DATA_CHUNK + 7]).encode().unwrap())
            .await
            .unwrap();
        peer.write_all(&Frame::resize(3, 100, 30).encode().unwrap())
            .await
            .unwrap();
        let HostEvent::DeferredStreamData {
            bytes,
            credit: stale,
            ..
        } = event(&mut events).await
        else {
            panic!("expected first DATA chunk");
        };
        assert_eq!(bytes, vec![b'a'; DATA_CHUNK]);
        let HostEvent::DeferredStreamData { bytes, credit, .. } = event(&mut events).await else {
            panic!("expected last DATA chunk");
        };
        assert_eq!(bytes, b"aaaaaaa");
        assert!(matches!(
            event(&mut events).await,
            HostEvent::StreamResize {
                channel: 3,
                cols: 100,
                rows: 30,
                ..
            }
        ));
        let copy = credit.clone();
        drop(credit);
        assert!(!HostClient::grant_tab_credit(&copy, 8));
        assert!(HostClient::grant_tab_credit(&copy, 2));
        drop(copy);
        assert_eq!(read_frame(&mut peer).await, Frame::credit(3, 2));
        assert_eq!(read_frame(&mut peer).await, Frame::credit(3, 5));

        client.detach_channel(3);
        assert_eq!(read_frame(&mut peer).await, Frame::close(3, vec![]));
        mock_attach(&client, &mut peer).await;
        assert!(!HostClient::grant_tab_credit(&stale, 1));
        drop(stale);
        peer.write_all(&Frame::data(3, vec![b'b']).encode().unwrap())
            .await
            .unwrap();
        let HostEvent::DeferredStreamData { credit, .. } = event(&mut events).await else {
            panic!("expected fresh DATA");
        };
        drop(credit);
        assert_eq!(read_frame(&mut peer).await, Frame::credit(3, 1));
    }

    #[tokio::test]
    async fn deferred_credit_cannot_cross_reconnect_with_reused_ids() {
        let (client, mut events, mut peer) = mock_client_with_options(vec![], true);
        mock_attach(&client, &mut peer).await;
        let data = Frame::data(3, vec![b'a'; 5]).encode().unwrap();
        peer.write_all(&data).await.unwrap();
        let HostEvent::DeferredStreamData { credit: stale, .. } = event(&mut events).await else {
            panic!("expected original connection DATA");
        };

        client.clear_active();
        let mut peer = install_mock_connection(&client, vec![]);
        assert!(matches!(
            event(&mut events).await,
            HostEvent::StateChanged(ConnectionState::Connected { conn_id: 1, .. })
        ));
        assert!(matches!(
            event(&mut events).await,
            HostEvent::VersionInfo { .. }
        ));
        mock_attach(&client, &mut peer).await;
        assert!(!HostClient::grant_tab_credit(&stale, 1));
        drop(stale);

        peer.write_all(&data).await.unwrap();
        let HostEvent::DeferredStreamData { credit, .. } = event(&mut events).await else {
            panic!("expected replacement connection DATA");
        };
        assert!(HostClient::grant_tab_credit(&credit, 2));
        assert_eq!(read_frame(&mut peer).await, Frame::credit(3, 2));
        drop(credit);
        assert_eq!(read_frame(&mut peer).await, Frame::credit(3, 3));
    }

    #[tokio::test]
    async fn deferred_credit_rejects_peer_overdraw() {
        let (client, mut events, mut peer) = mock_client_with_options(vec![], true);
        mock_attach(&client, &mut peer).await;
        peer.write_all(
            &Frame::data(3, vec![0; INITIAL_CREDIT + 1])
                .encode()
                .unwrap(),
        )
        .await
        .unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), peer.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(events.try_recv().is_err());
    }

    // UT-PRESET-03
    #[tokio::test]
    async fn preset_detection_negotiates_and_normalizes_on_the_wire() {
        use ddg_agents_proto::host::PresetDetectStatus::{Found, NotFound, Unknown};

        let (client, _events, mut peer) = mock_client();
        assert!(!client.supports_agent_presets());
        assert_eq!(
            client.detect_agent_presets().await.unwrap(),
            PresetDetectOutcome::Unsupported
        );
        // The next frame must be ping, not an unadvertised preset RPC.
        let (ping, _) = tokio::join!(
            client.call_raw("ping", json!({})),
            rpc_reply(&mut peer, "ping", json!({}), json!({"ok": true}))
        );
        ping.unwrap();
        client.close();
        assert!(matches!(
            client.detect_agent_presets().await,
            Err(PresetDetectError::Rpc(e)) if e.data.kind == ErrorKind::Unavailable
        ));

        let (client, _events, mut peer) =
            mock_client_with_features(vec![FEATURE_AGENT_PRESETS_V1.into()]);
        assert!(client.supports_agent_presets());
        let complete = json!([
            {"id":"claude-code","status":"unknown"}, {"id":"codex","status":"found"},
            {"id":"antigravity","status":"not_found"}, {"id":"pi","status":"found"},
            {"id":"opencode","status":"unknown"}
        ]);
        let partial = json!([
            {"id":"future-agent","status":"found"}, {"id":"codex","status":"not_found"}
        ]);
        for (items, expected) in [
            (complete, [Found, NotFound, Found, Unknown, Unknown]),
            (partial, [Unknown, Unknown, NotFound, Unknown, Unknown]),
        ] {
            let (result, _) = tokio::join!(
                client.detect_agent_presets(),
                rpc_reply(&mut peer, "host.agent_presets_detect", json!({}), json!({"items": items}))
            );
            let PresetDetectOutcome::Detected(items) = result.unwrap() else {
                panic!("advertised preset detection unsupported");
            };
            assert_eq!(
                items
                    .iter()
                    .map(|item| item.id.as_str())
                    .collect::<Vec<_>>(),
                ["pi", "antigravity", "codex", "opencode", "claude-code"]
            );
            assert_eq!(
                items.into_iter().map(|item| item.status).collect::<Vec<_>>(),
                expected
            );
        }
        for invalid in [
            json!([{"id":"pi","status":"found"},{"id":"pi","status":"unknown"}]),
            json!([{"id":"future","status":"found"},{"id":"future","status":"unknown"}]),
            json!(
                (0..33)
                    .map(|i| json!({"id": format!("future-{i}"), "status":"found"}))
                    .collect::<Vec<_>>()
            ),
            json!([{"id":"pi","status":"maybe"}]),
        ] {
            let (result, _) = tokio::join!(
                client.detect_agent_presets(),
                rpc_reply(&mut peer, "host.agent_presets_detect", json!({}), json!({"items": invalid}))
            );
            assert!(matches!(result, Err(PresetDetectError::Protocol(_))));
        }
    }

    #[tokio::test]
    async fn preset_detection_cleans_pending_on_cancel_timeout_and_reconnect() {
        let (client, _events, mut peer) =
            mock_client_with_features(vec![FEATURE_AGENT_PRESETS_V1.into()]);
        let conn = client.require_connection().unwrap();
        let mut timed = Box::pin(client.detect_agent_presets());
        tokio::select! {
            _ = read_frame(&mut peer) => {},
            result = &mut timed => panic!("replied before request: {result:?}"),
        }
        assert_eq!(conn.pending.lock().unwrap().len(), 1);
        drop(timed);
        assert!(conn.pending.lock().unwrap().is_empty());

        let (result, _) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(5), client.detect_agent_presets()),
            read_frame(&mut peer),
        );
        assert!(matches!(
            result.unwrap(),
            Err(PresetDetectError::Rpc(e)) if e.data.kind == ErrorKind::Unavailable
                && e.message.contains("timed out")
        ));
        assert!(conn.pending.lock().unwrap().is_empty());

        let mut detect = Box::pin(client.detect_agent_presets());
        tokio::select! {
            _ = read_frame(&mut peer) => {},
            result = &mut detect => panic!("replied before request: {result:?}"),
        }
        client.clear_active();
        let mut replacement = install_mock_connection(&client, vec![]);
        assert!(matches!(
            detect.await,
            Err(PresetDetectError::Rpc(e)) if e.data.kind == ErrorKind::Unavailable
        ));
        assert!(conn.pending.lock().unwrap().is_empty());
        let (ping, _) = tokio::join!(
            client.call_raw("ping", json!({})),
            rpc_reply(&mut replacement, "ping", json!({}), json!({"ok": true}))
        );
        ping.unwrap();
    }

    #[tokio::test]
    async fn client_and_install_feature_gates_requests_on_the_wire() {
        let feature = "agent_activity_v1";
        for supported in [false, true] {
            let features = if supported {
                vec![feature.into()]
            } else {
                vec![]
            };
            let (client, _events, mut peer) = mock_client_with_features(features.clone());
            assert_eq!(client.supports_feature(feature), supported);
            assert!(!client.supports_feature("other"));
            assert!(
                matches!(client.state(), ConnectionState::Connected { features: got, .. } if got == features)
            );
            let room = json!({"agent_profile":"pi", "executor":{"kind":"default_shell"}, "cols":80, "rows":24});
            let mut tab = room.clone();
            tab["room_id"] = json!(ROOM_ID);
            let split = json!({"anchor_tab_id":TAB_ID, "axis":"horizontal", "request_id":null, "ratio_bp":null,
                "space_cwd":null, "title":null, "agent_profile":"pi", "executor":{"kind":"default_shell"}, "cols":80, "rows":24});
            let mut expected = [room.clone(), tab.clone(), split.clone()];
            if !supported {
                for params in &mut expected {
                    params.as_object_mut().unwrap().remove("agent_profile");
                }
            }
            let serve = async {
                for (method, params) in ["host.room_create", "host.tab_create", "host.tab_split"]
                    .into_iter()
                    .zip(expected)
                {
                    // Deliberately omit results: this fixture tests exact outgoing JSON.
                    rpc_reply(&mut peer, method, params, Value::Null).await;
                }
                if supported {
                    rpc_reply(
                        &mut peer,
                        "host.tab_mark_seen",
                        json!({"tab_id":TAB_ID,"run_generation":8,"seen_seq":3}),
                        json!({"tab_id":TAB_ID,"run_generation":8,"activity_seq":4,"seen_seq":3}),
                    )
                    .await;
                }
                rpc_reply(&mut peer, "ping", json!({}), json!({"ok":true})).await;
            };
            let call = async {
                let _ = client
                    .room_create_with_agent(serde_json::from_value(room).unwrap())
                    .await;
                let _ = client
                    .tab_create_with_agent(serde_json::from_value(tab).unwrap())
                    .await;
                let _ = client
                    .tab_split_with_agent(serde_json::from_value(split).unwrap())
                    .await;
                let seen = client.tab_mark_seen(TAB_ID.into(), 8, 3).await.unwrap();
                assert_eq!(seen.map(|s| s.seen_seq), supported.then_some(3));
                let _ = client.ping().await;
            };
            tokio::join!(serve, call);
        }
    }

    #[tokio::test]
    async fn client_and_install_catalog_activity_snapshot_precedes_stream_deltas() {
        let (client, mut events, mut peer) =
            mock_client_with_features(vec!["agent_activity_v1".into()]);
        let activity = json!({"tab_id":TAB_ID,"run_generation":8,"activity_seq":3,"seen_seq":2});
        let serve = async {
            rpc_reply(&mut peer, "host.catalog_subscribe", json!({}),
                json!({"catalog":Catalog::empty("peer"),"seq":1,"channel":5,"activities":[activity]})).await;
            notify(&mut peer, 0, "host.tab_seen_changed", activity.clone()).await;
            let mut delta = activity.clone();
            delta["room_id"] = json!(ROOM_ID);
            delta["activity_seq"] = json!(4);
            notify(&mut peer, 5, "host.tab_activity_changed", delta).await;
            notify(
                &mut peer,
                5,
                "host.tab_seen_changed",
                json!({"tab_id":TAB_ID,"run_generation":8,"activity_seq":4,"seen_seq":4}),
            )
            .await;
        };
        let check = async {
            let sub = client.catalog_subscribe().await.unwrap();
            assert_eq!(sub.activities.len(), 1);
            assert!(
                matches!(event(&mut events).await, HostEvent::CatalogSubscribed { activities, .. } if activities == sub.activities)
            );
            assert!(matches!(
                event(&mut events).await,
                HostEvent::TabActivityChanged {
                    activity_seq: 4,
                    ..
                }
            ));
            assert!(matches!(
                event(&mut events).await,
                HostEvent::TabSeenChanged { seen_seq: 4, .. }
            ));
        };
        tokio::join!(serve, check);
    }

    #[tokio::test]
    async fn catalog_reset_notice_and_live_host_notifications() {
        let (client, mut events, mut peer) = mock_client_with_options(vec![], true);
        let expected_catalog = Catalog::empty("peer");
        let expected_notice = ResetNotice {
            notice_id: "v1-reset".into(),
            backup_file: "catalog.json.v1.bak.1".into(),
        };
        let catalog = expected_catalog.clone();
        let notice = expected_notice.clone();
        let serve = tokio::spawn(async move {
            rpc_reply(
                &mut peer,
                "host.catalog_subscribe",
                json!({}),
                json!({"catalog": catalog, "seq": 1, "channel": 5, "reset_notice": notice}),
            )
            .await;
            notify(
                &mut peer,
                5,
                "host.catalog_changed",
                json!({"catalog": catalog, "seq": 2}),
            )
            .await;
            notify(&mut peer, 0, "host.external_input", json!({"session_id": TAB_ID, "pty_id": 7, "run_generation": 8, "byte_len": 47, "from_session_id": SENDER_ID, "from_host_id": "hub", "is_remote": true})).await;
            notify(&mut peer, 0, "host.session_exited", json!({"session_id": TAB_ID, "room_id": ROOM_ID, "run_generation": 8, "exit_code": 42})).await;
            let ack = read_frame(&mut peer).await;
            assert_eq!((ack.channel, ack.kind), (5, FrameKind::Credit));
            assert!(ddg_agents_proto::frame::decode_credit(&ack.payload).unwrap() > 0);
        });
        let mut sub = client.catalog_subscribe().await.unwrap();
        assert_eq!(sub.seq, 1);
        assert_eq!(sub.catalog, expected_catalog);
        assert_eq!(sub.reset_notice.as_ref(), Some(&expected_notice));
        let change = tokio::time::timeout(Duration::from_secs(2), sub.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(change.seq, 2);
        assert_eq!(change.catalog, expected_catalog);
        assert!(
            matches!(event(&mut events).await, HostEvent::CatalogSubscribed { seq: 1, reset_notice: Some(notice), .. } if notice == expected_notice)
        );
        assert!(matches!(
            event(&mut events).await,
            HostEvent::CatalogChanged { seq: 2, .. }
        ));
        assert!(
            matches!(event(&mut events).await, HostEvent::ExternalInput { session_id, pty_id: 7, run_generation: 8, byte_len: 47, from_session_id, from_host_id, is_remote: true } if session_id == TAB_ID && from_session_id == SENDER_ID && from_host_id == "hub")
        );
        assert!(
            matches!(event(&mut events).await, HostEvent::SessionExited { session_id, room_id, run_generation: 8, exit_code: Some(42) } if session_id == TAB_ID && room_id == ROOM_ID)
        );
        serve.await.unwrap();
    }

    #[tokio::test]
    async fn relay_bidirectional_ndjson_credit_and_replacement() {
        let (client, _events, mut peer) = mock_client();
        let request = RelayFrame::Request {
            envelope: ddg_agents_proto::host::RelayEnvelope {
                hop: 1,
                request_id: "r1".into(),
                method: "session_screen".into(),
                params: json!({}),
                from_host_id: "hub".into(),
            },
        };
        let reply = RelayFrame::Response {
            request_id: "r1".into(),
            result: json!("x".repeat(INITIAL_CREDIT)),
        };
        let (actual_request, actual_reply) = (request.clone(), reply.clone());
        let (start_receiving, receive_ready) = oneshot::channel();
        let serve = tokio::spawn(async move {
            rpc_reply(
                &mut peer,
                "host.relay_attach",
                json!({}),
                json!({"channel": 9}),
            )
            .await;
            let line = format!("{}\n", json!(actual_request));
            let (first, second) = line.as_bytes().split_at(10);
            for chunk in [first, second] {
                peer.write_all(&Frame::data(9, chunk.to_vec()).encode().unwrap())
                    .await
                    .unwrap();
            }
            let mut extra = [0];
            assert!(
                tokio::time::timeout(Duration::from_millis(30), peer.read_exact(&mut extra))
                    .await
                    .is_err()
            );
            start_receiving.send(()).unwrap();
            for chunk in [first, second] {
                assert_eq!(
                    read_frame(&mut peer).await,
                    Frame::credit(9, chunk.len() as u32)
                );
            }
            let mut sent = Vec::new();
            while sent.len() < INITIAL_CREDIT {
                let frame = read_frame(&mut peer).await;
                assert_eq!((frame.channel, frame.kind), (9, FrameKind::Data));
                sent.extend(frame.payload);
            }
            assert_eq!(sent.len(), INITIAL_CREDIT);
            let mut extra = [0];
            assert!(
                tokio::time::timeout(Duration::from_millis(30), peer.read_exact(&mut extra))
                    .await
                    .is_err()
            );
            peer.write_all(&Frame::credit(9, 4096).encode().unwrap())
                .await
                .unwrap();
            sent.extend(read_frame(&mut peer).await.payload);
            assert_eq!(sent.last(), Some(&b'\n'));
            assert_eq!(
                serde_json::from_slice::<RelayFrame>(&sent).unwrap(),
                actual_reply
            );
            peer.write_all(
                &Frame::close(9, serde_json::to_vec(&CloseReason::RelayReplaced).unwrap())
                    .encode()
                    .unwrap(),
            )
            .await
            .unwrap();
        });
        let mut relay = client.relay_attach().await.unwrap();
        assert_eq!(relay.channel(), 9);
        receive_ready.await.unwrap();
        assert_eq!(relay.recv().await.unwrap(), request);
        relay.send(&reply).unwrap();
        let close = tokio::time::timeout(Duration::from_secs(2), relay.recv())
            .await
            .unwrap();
        assert_eq!(close, Err(StreamError::Closed(CloseReason::RelayReplaced)));
        assert!(relay.send(&reply).is_err());
        serve.await.unwrap();
    }

    #[tokio::test]
    async fn msg_subscribe_reads_coalesced_records_and_detaches_on_drop() {
        let (client, _events, mut peer) = mock_client();
        let message = json!({
            "message_id": "msg_00000000000000000000000001",
            "to_session_id": TAB_ID, "from_session_id": SENDER_ID,
            "from_host_id": "peer", "is_remote": false, "kind": "message", "data_b64": "aGk=",
        });
        let messages = format!("{message}\n{message}\n");
        let (detached, wait_detached) = oneshot::channel();
        let peer_task = tokio::spawn(async move {
            rpc_reply(
                &mut peer,
                "host.msg_subscribe",
                json!({"session_token": "test-token"}),
                json!({"channel": 4}),
            )
            .await;
            let len = messages.len() as u32;
            peer.write_all(&Frame::data(4, messages.into_bytes()).encode().unwrap())
                .await
                .unwrap();
            assert_eq!(read_frame(&mut peer).await, Frame::credit(4, len));
            assert_eq!(read_frame(&mut peer).await, Frame::close(4, Vec::new()));
            detached.send(()).unwrap();
            let mut byte = [0];
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), peer.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        });
        let mut inbox = client
            .msg_subscribe(MsgSubscribeParams {
                session_token: "test-token".into(),
            })
            .await
            .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(2), inbox.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.from_session_id, SENDER_ID);
        assert_eq!(inbox.recv().await.unwrap(), first);
        drop(inbox);
        wait_detached.await.unwrap();
        client.close();
        peer_task.await.unwrap();
    }

    #[test]
    fn drops_input_and_resize_when_disconnected() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = HostClient {
            profile: HostProfile::local_default(),
            active: Mutex::new(None),
            state: Mutex::new(ConnectionState::Reconnecting {
                attempt: 1,
                reason: "network down".into(),
            }),
            closed: AtomicBool::new(false),
            deferred_tab_credit: false,
            event_tx: tx,
        };
        assert!(!client.is_connected());
        assert!(!client.send_input(1, b"rm -rf /\n"));
        assert!(!client.send_resize(1, 80, 24));
    }
}
