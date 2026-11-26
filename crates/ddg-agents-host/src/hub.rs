//! An opt-in Hub owns pinned peers; a non-Hub owns at most one upstream link.
use crate::host_service::HostService;
use ddg_agents_proto::frame::{
    DATA_CHUNK, Frame, FrameKind, INITIAL_CREDIT, MAX_PENDING, decode_credit,
};
use ddg_agents_proto::host::{
    HostQueryError, HostStatus, HostSummary, ListHostsResult, RelayEnvelope, RelayFrame,
    SessionListResult,
};
use ddg_agents_proto::id::{parse_session_id, sanitize_display_text};
use ddg_agents_proto::rpc::{RpcError, error_code};
use ddg_agents_proto::terminal::CloseReason;
use ddg_agents_transport::{
    DialPolicy, HostClient, HostProfile, HostTransport, HostsConfig, RelayStreamHandle,
    StreamError, sanitize_dial_error,
};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

const MAX_CALLS: usize = 32;
const PEER_TIMEOUT: Duration = Duration::from_secs(8);
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Debug)]
pub struct Caller {
    pub session_id: String,
    pub host_id: String,
    pub is_remote: bool,
}

#[derive(Default)]
pub struct Relay {
    peers: Option<Vec<Peer>>,
    upstream: Mutex<Option<Upstream>>,
}

struct Peer {
    profile: HostProfile,
    state: Mutex<PeerState>,
}

enum PeerState {
    Offline(RpcError),
    Connected { host_id: String, link: Arc<Link> },
}

struct Upstream {
    conn_id: u64,
    channel: u32,
    frames: mpsc::Sender<Frame>,
    receive_credit: Arc<AtomicUsize>,
    stop: oneshot::Sender<CloseReason>,
    link: Arc<Link>,
}

fn enabled(value: Option<&str>) -> bool {
    value == Some("1")
}

pub fn allowed(method: &str) -> bool {
    // Catalog subscriptions and their activity/seen RPCs are operator-only.
    matches!(
        method,
        "host.msg_send"
            | "host.session_create"
            | "host.session_list"
            | "host.session_move"
            | "host.session_close"
            | "host.session_restart"
            | "host.session_screen"
            | "host.session_write"
            | "host.list_hosts"
    )
}

fn attach_allowed(hub: bool, token: bool, framed: bool) -> Result<(), RpcError> {
    if hub || token {
        return Err(RpcError::permission_denied(
            "relay_attach is unavailable to this connection",
        ));
    }
    if !framed {
        return Err(RpcError::invalid_params(
            "host.relay_attach requires framed mode",
        ));
    }
    Ok(())
}

fn check_peer_id(self_id: &str, peer_id: &str) -> Result<(), RpcError> {
    if peer_id == self_id {
        return Err(RpcError::host_security_blocked("self relay is forbidden"));
    }
    Ok(())
}

fn validate_relay(
    envelope: &mut RelayEnvelope,
    local: &str,
    peer_id: Option<&str>,
) -> Result<Caller, RpcError> {
    if envelope.hop != 1 {
        return Err(RpcError::invalid_params("relay hop must be 1"));
    }
    if !allowed(&envelope.method) {
        return Err(RpcError::permission_denied(
            "method is not allowed over relay",
        ));
    }
    if let Some(peer_id) = peer_id {
        check_peer_id(local, peer_id)?;
        envelope.from_host_id = peer_id.to_owned();
    }
    let params = envelope
        .params
        .as_object_mut()
        .ok_or_else(|| RpcError::invalid_params("relay params must be an object"))?;
    params.remove("session_token");
    let session_id = params
        .remove("from_session_id")
        .and_then(|v| v.as_str().map(str::to_owned))
        .filter(|id| parse_session_id(id).is_ok())
        .ok_or_else(|| RpcError::invalid_params("relay requires a caller session ID"))?;
    if envelope.from_host_id.trim().is_empty() {
        return Err(RpcError::invalid_params("relay requires a caller host ID"));
    }
    Ok(Caller {
        session_id,
        host_id: envelope.from_host_id.clone(),
        is_remote: true,
    })
}

fn safe_error(mut error: RpcError) -> RpcError {
    error.message = sanitize_dial_error(&error.message);
    error.data.hint = error.data.hint.as_deref().map(sanitize_dial_error);
    error
}

impl Relay {
    pub fn from_env() -> io::Result<Self> {
        if !enabled(std::env::var("DDG_HUB_ENABLED").ok().as_deref()) {
            return Ok(Self::default());
        }
        let config = HostsConfig::load_or_default(&HostsConfig::default_path()?);
        let peers = config
            .hosts
            .into_iter()
            .filter(|profile| {
                !matches!(profile.transport, HostTransport::Local { sock_path: None })
            })
            .map(|profile| {
                let error = if profile
                    .expected_host_id
                    .as_deref()
                    .is_none_or(|id| id.trim().is_empty())
                {
                    RpcError::host_security_blocked("a pinned expected_host_id is required")
                } else {
                    RpcError::host_offline("peer is not connected")
                };
                Peer {
                    profile,
                    state: Mutex::new(PeerState::Offline(error)),
                }
            })
            .collect();
        Ok(Self {
            peers: Some(peers),
            upstream: Mutex::new(None),
        })
    }

    pub fn start(host: &Arc<HostService>) {
        for idx in 0..host.relay.peers.as_ref().map_or(0, Vec::len) {
            tokio::spawn(manage_peer(host.clone(), idx));
        }
    }

    pub fn attach(
        &self,
        conn_id: u64,
        channel: u32,
        out: mpsc::UnboundedSender<Vec<u8>>,
        framed: bool,
        forbidden: impl FnOnce() -> bool,
    ) -> Result<ServerWire, RpcError> {
        let mut slot = self.upstream.lock().unwrap();
        attach_allowed(self.peers.is_some(), forbidden(), framed)?;
        let (link, calls) = Link::new(true);
        let (frames, rx) = mpsc::channel(64);
        let receive_credit = Arc::new(AtomicUsize::new(INITIAL_CREDIT));
        let (stop, stopped) = oneshot::channel();
        let old = slot.replace(Upstream {
            conn_id,
            channel,
            frames,
            receive_credit: receive_credit.clone(),
            stop,
            link,
        });
        if let Some(old) = old {
            let _ = old.stop.send(CloseReason::RelayReplaced);
        }
        Ok(ServerWire {
            channel,
            out,
            rx,
            stopped,
            calls: Some(calls),
            receive_credit,
            send_credit: INITIAL_CREDIT,
            chunk: Vec::new(),
            offset: 0,
            line: Vec::new(),
            pending: VecDeque::new(),
        })
    }

    pub fn detach(&self, conn_id: u64, channel: Option<u32>) {
        let mut slot = self.upstream.lock().unwrap();
        if slot
            .as_ref()
            .is_some_and(|u| u.conn_id == conn_id && channel.is_none_or(|ch| u.channel == ch))
        {
            let _ = slot.take().unwrap().stop.send(CloseReason::Detached);
        }
    }

    pub fn feed(&self, conn_id: u64, frame: Frame) -> Option<Frame> {
        let mut slot = self.upstream.lock().unwrap();
        let Some(u) = slot
            .as_ref()
            .filter(|u| u.conn_id == conn_id && u.channel == frame.channel)
        else {
            return Some(frame);
        };
        if frame.kind == FrameKind::Data && frame.payload.is_empty() {
            return None;
        }
        let valid = match frame.kind {
            FrameKind::Data => u
                .receive_credit
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |credit| {
                    credit.checked_sub(frame.payload.len())
                })
                .is_ok(),
            FrameKind::Credit => decode_credit(&frame.payload).is_ok(),
            FrameKind::Close => false,
            FrameKind::Resize => false,
        };
        if !valid || u.frames.try_send(frame).is_err() {
            let _ = slot.take().unwrap().stop.send(CloseReason::Detached);
        }
        None
    }

    fn upstream(&self) -> Option<Arc<Link>> {
        self.upstream
            .lock()
            .unwrap()
            .as_ref()
            .map(|u| u.link.clone())
    }

    pub fn summaries(&self, self_id: &str) -> ListHostsResult {
        let mut hosts = vec![HostSummary {
            host_id: self_id.into(),
            label: "local".into(),
            status: HostStatus::Connected,
        }];
        for peer in self.peers.iter().flatten() {
            let (host_id, link) = peer.route(self_id);
            hosts.push(HostSummary {
                host_id,
                label: sanitize_display_text(&peer.profile.label, 160),
                status: if link.is_ok() {
                    HostStatus::Connected
                } else {
                    HostStatus::Offline
                },
            });
        }
        ListHostsResult { hosts }
    }

    pub async fn route(
        &self,
        host: &HostService,
        method: &str,
        params: Value,
        caller: Caller,
    ) -> Result<Value, RpcError> {
        let target = target(method, &params)?;
        if let Some(peers) = &self.peers {
            if method == "host.list_hosts" {
                return Ok(json!(self.summaries(&host.host_id)));
            }
            if target.is_some_and(is_all) {
                let local = host.session_local(method, params.clone(), &caller).await;
                let mut result = SessionListResult {
                    sessions: vec![],
                    errors: vec![],
                };
                merge_list(&mut result, host.host_id.clone(), local);
                let mut calls = JoinSet::new();
                for peer in peers {
                    let (id, link) = peer.route(&host.host_id);
                    match link {
                        Ok(link) => {
                            let envelope = link.envelope(method, params.clone(), &caller);
                            calls.spawn(async move { (id, link.call(envelope).await) });
                        }
                        Err(error) => merge_list(&mut result, id, Err(error)),
                    }
                }
                while let Some(call) = calls.join_next().await {
                    let (id, reply) = call.expect("list relay task");
                    merge_list(&mut result, id, reply);
                }
                return Ok(json!(result));
            }
            if let Some(id) = target.filter(|&id| id != "local" && id != host.host_id) {
                let peer = peers
                    .iter()
                    .find(|p| p.profile.id.trim().eq_ignore_ascii_case(id))
                    .or_else(|| {
                        peers.iter().find(|p| {
                            p.profile.expected_host_id.as_deref() == Some(id)
                                || p.route(&host.host_id).0 == id
                        })
                    })
                    .ok_or_else(|| RpcError::host_not_found("unknown host"))?;
                let link = peer.route(&host.host_id).1?;
                return link.call(link.envelope(method, params, &caller)).await;
            }
        } else if method == "host.list_hosts" {
            if let Some(link) = self.upstream() {
                return link.call(link.envelope(method, params, &caller)).await;
            }
            return Ok(json!(self.summaries(&host.host_id)));
        } else if target.is_some_and(|id| id != "local" && id != host.host_id) {
            let link = self.upstream().ok_or_else(no_hub)?;
            return link.call(link.envelope(method, params, &caller)).await;
        }
        host.session_local(method, params, &caller).await
    }
}

fn is_all(id: &str) -> bool {
    id.trim().eq_ignore_ascii_case("all")
}

fn target<'a>(method: &str, params: &'a Value) -> Result<Option<&'a str>, RpcError> {
    match params.get("host_id") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id)) if is_all(id) && method != "host.session_list" => Err(
            RpcError::invalid_params("host_id all is only valid for session list"),
        ),
        Some(Value::String(id)) => Ok(Some(id.trim())),
        _ => Err(RpcError::invalid_params("host_id must be a string")),
    }
}

fn merge_list(result: &mut SessionListResult, host_id: String, reply: Result<Value, RpcError>) {
    match reply.and_then(|v| {
        serde_json::from_value::<SessionListResult>(v)
            .map_err(|_| RpcError::invalid_params("invalid session list reply"))
    }) {
        Ok(list) => result.sessions.extend(list.sessions),
        Err(error) => result.errors.push(HostQueryError {
            host_id,
            error: safe_error(error),
        }),
    }
}

fn no_hub() -> RpcError {
    RpcError::hub_unavailable("no upstream Hub is attached")
}

impl Peer {
    fn route(&self, self_id: &str) -> (String, Result<Arc<Link>, RpcError>) {
        match &*self.state.lock().unwrap() {
            PeerState::Connected { host_id, link } => (host_id.clone(), Ok(link.clone())),
            PeerState::Offline(error) => (
                self.profile
                    .expected_host_id
                    .as_deref()
                    .filter(|&id| !id.trim().is_empty() && id != self_id)
                    .unwrap_or(self.profile.id.trim())
                    .to_owned(),
                Err(error.clone()),
            ),
        }
    }
}

async fn manage_peer(host: Arc<HostService>, idx: usize) {
    let peer = &host.relay.peers.as_ref().unwrap()[idx];
    let Some(pin) = peer
        .profile
        .expected_host_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
    else {
        return;
    };
    if let Err(error) = check_peer_id(&host.host_id, pin) {
        *peer.state.lock().unwrap() = PeerState::Offline(error);
        return;
    }
    loop {
        let error = match HostClient::connect_once_with_policy(
            peer.profile.clone(),
            "hub-relay",
            DialPolicy::RequirePinnedHostId,
        )
        .await
        {
            Ok((client, events)) => {
                drop(events);
                let result = async {
                    let id = client.hello().map(|hello| hello.host_id).ok_or_else(|| {
                        RpcError::host_offline("peer disconnected during handshake")
                    })?;
                    check_peer_id(&host.host_id, &id)?;
                    let stream = client.relay_attach().await?;
                    let (link, calls) = Link::new(false);
                    *peer.state.lock().unwrap() = PeerState::Connected {
                        host_id: id.clone(),
                        link,
                    };
                    Ok(drive(&host, Some(id), Wire::Client(stream), calls).await)
                }
                .await;
                client.close();
                match result {
                    Ok(end) => {
                        let (error, replaced) = peer_end(end);
                        if replaced {
                            *peer.state.lock().unwrap() = PeerState::Offline(error);
                            host.audit("relay_replaced", &peer.profile.id);
                            eprintln!(
                                "ddg-agents-host: relay replaced for peer {}; reconnect stopped",
                                peer.profile.id.trim()
                            );
                            return;
                        }
                        error
                    }
                    Err(error) => error,
                }
            }
            Err(error) => error.into(),
        };
        let stop = matches!(
            error.data.code.as_deref(),
            Some(
                error_code::HOST_SECURITY_BLOCKED
                    | error_code::HOST_AUTH_REQUIRED
                    | error_code::PERMISSION_DENIED
                    | error_code::INVALID_PARAMS
            )
        );
        *peer.state.lock().unwrap() = PeerState::Offline(safe_error(error));
        if stop {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn peer_end(error: StreamError) -> (RpcError, bool) {
    if matches!(error, StreamError::Closed(CloseReason::RelayReplaced)) {
        (
            RpcError::host_security_blocked("peer relay was replaced"),
            true,
        )
    } else {
        (RpcError::host_offline("peer relay disconnected"), false)
    }
}

struct Link {
    calls: mpsc::Sender<Call>,
    next_id: AtomicU64,
    upstream: bool,
}

type Reply = oneshot::Sender<Result<Value, RpcError>>;
struct Call {
    envelope: RelayEnvelope,
    reply: Reply,
}

impl Link {
    fn new(upstream: bool) -> (Arc<Self>, mpsc::Receiver<Call>) {
        let (calls, rx) = mpsc::channel(MAX_CALLS);
        (
            Arc::new(Self {
                calls,
                next_id: AtomicU64::new(1),
                upstream,
            }),
            rx,
        )
    }

    fn envelope(&self, method: &str, mut params: Value, caller: &Caller) -> RelayEnvelope {
        let fields = params.as_object_mut().expect("authenticated object params");
        fields.remove("session_token");
        fields.remove("from_host_id");
        fields.remove("is_remote");
        if !self.upstream {
            fields.remove("host_id");
        }
        fields.insert("from_session_id".into(), json!(caller.session_id));
        RelayEnvelope {
            hop: 1,
            request_id: self.next_id.fetch_add(1, Ordering::Relaxed).to_string(),
            method: method.into(),
            params,
            from_host_id: caller.host_id.clone(),
        }
    }

    async fn call(&self, envelope: RelayEnvelope) -> Result<Value, RpcError> {
        let timeout = if self.upstream {
            UPSTREAM_TIMEOUT
        } else {
            PEER_TIMEOUT
        };
        let disconnected = || {
            if self.upstream {
                no_hub()
            } else {
                RpcError::host_offline("peer relay disconnected")
            }
        };
        tokio::time::timeout(timeout, async {
            let (reply, result) = oneshot::channel();
            self.calls
                .send(Call { envelope, reply })
                .await
                .map_err(|_| disconnected())?;
            result.await.map_err(|_| disconnected())?
        })
        .await
        .map_err(|_| RpcError::relay_timeout("relay request timed out"))?
    }
}

async fn execute(
    host: &HostService,
    peer_id: Option<&str>,
    mut envelope: RelayEnvelope,
) -> RelayFrame {
    let request_id = std::mem::take(&mut envelope.request_id);
    let result = async {
        let _op = host
            .reload_gate
            .try_read()
            .map_err(|_| crate::admin::draining())?;
        let caller = validate_relay(&mut envelope, &host.host_id, peer_id)?;
        if peer_id.is_some() {
            host.relay
                .route(host, &envelope.method, envelope.params, caller)
                .await
        } else {
            // Hub-to-peer calls terminate here; this side cannot route them onward.
            if target(&envelope.method, &envelope.params)?.is_some() {
                return Err(RpcError::invalid_params(
                    "relay to a destination cannot specify host_id",
                ));
            }
            host.session_local(&envelope.method, envelope.params, &caller)
                .await
        }
    }
    .await;
    match result {
        Ok(result) => RelayFrame::Response { request_id, result },
        Err(error) => RelayFrame::Error { request_id, error },
    }
}

async fn drive(
    host: &Arc<HostService>,
    peer: Option<String>,
    mut wire: Wire,
    mut calls: mpsc::Receiver<Call>,
) -> StreamError {
    let mut pending: HashMap<String, Reply> = HashMap::new();
    let mut requests = JoinSet::new();
    let end = 'drive: loop {
        let output = tokio::select! {
            incoming = wire.recv() => match incoming {
                Ok(RelayFrame::Request { envelope }) => {
                    if requests.len() < MAX_CALLS {
                        let (host, peer) = (host.clone(), peer.clone());
                        requests.spawn(async move { execute(&host, peer.as_deref(), envelope).await });
                        continue;
                    }
                    RelayFrame::Error { request_id: envelope.request_id, error: RpcError::relay_timeout("relay is busy") }
                }
                Ok(RelayFrame::Response { request_id, result }) => {
                    if let Some(reply) = pending.remove(&request_id) { let _ = reply.send(Ok(result)); }
                    continue;
                }
                Ok(RelayFrame::Error { request_id, error }) => {
                    if let Some(reply) = pending.remove(&request_id) { let _ = reply.send(Err(safe_error(error))); }
                    continue;
                }
                Err(end) => break end,
            },
            Some(call) = calls.recv() => {
                pending.retain(|_, reply| !reply.is_closed());
                if call.reply.is_closed() { continue; }
                if pending.len() >= MAX_CALLS {
                    let _ = call.reply.send(Err(RpcError::relay_timeout("relay is busy")));
                    continue;
                }
                pending.insert(call.envelope.request_id.clone(), call.reply);
                RelayFrame::Request { envelope: call.envelope }
            }
            Some(Ok(reply)) = requests.join_next(), if !requests.is_empty() => reply,
        };
        if let Err(error) = wire.send(&output) {
            // The client stream may already have queued a replacement CLOSE when send notices removal.
            if matches!(
                (&wire, &error),
                (Wire::Client(_), StreamError::Disconnected)
            ) {
                loop {
                    if let Err(end) = wire.recv().await {
                        break 'drive end;
                    }
                }
            }
            break error;
        }
    };
    // An accepted mutation must finish its catalog commit or compensation even if the link closes.
    requests.detach_all();
    end
}

// The transport client already implements its half of the credit-controlled stream.
enum Wire {
    Client(RelayStreamHandle),
    Server(ServerWire),
}

impl Wire {
    async fn recv(&mut self) -> Result<RelayFrame, StreamError> {
        match self {
            Self::Client(wire) => wire.recv().await,
            Self::Server(wire) => wire.recv().await,
        }
    }

    fn send(&mut self, frame: &RelayFrame) -> Result<(), StreamError> {
        match self {
            Self::Client(wire) => wire.send(frame),
            Self::Server(wire) => wire.send(frame),
        }
    }
}

pub struct ServerWire {
    channel: u32,
    out: mpsc::UnboundedSender<Vec<u8>>,
    rx: mpsc::Receiver<Frame>,
    stopped: oneshot::Receiver<CloseReason>,
    calls: Option<mpsc::Receiver<Call>>,
    receive_credit: Arc<AtomicUsize>,
    send_credit: usize,
    chunk: Vec<u8>,
    offset: usize,
    line: Vec<u8>,
    pending: VecDeque<u8>,
}

impl ServerWire {
    pub async fn run(self, host: Arc<HostService>, conn_id: u64, ready: oneshot::Receiver<()>) {
        let mut wire = self;
        if ready.await.is_ok() {
            let calls = wire.calls.take().unwrap();
            let channel = wire.channel;
            let out = wire.out.clone();
            let end = drive(&host, None, Wire::Server(wire), calls).await;
            let reason = match end {
                StreamError::Closed(reason) => reason,
                _ => CloseReason::Detached,
            };
            let _ = out.send(
                Frame::close(channel, serde_json::to_vec(&reason).unwrap())
                    .encode()
                    .unwrap(),
            );
            host.relay.detach(conn_id, Some(channel));
        } else {
            host.relay.detach(conn_id, Some(wire.channel));
        }
    }

    fn emit(&self, frame: Frame) -> Result<(), StreamError> {
        self.out
            .send(frame.encode().map_err(|_| StreamError::InvalidFrame)?)
            .map_err(|_| StreamError::Disconnected)
    }

    fn flush(&mut self) -> Result<(), StreamError> {
        while self.send_credit > 0 && !self.pending.is_empty() {
            let bytes = self.send_credit.min(DATA_CHUNK).min(self.pending.len());
            let payload = self.pending.drain(..bytes).collect();
            self.emit(Frame::data(self.channel, payload))?;
            self.send_credit -= bytes;
        }
        Ok(())
    }

    fn send(&mut self, frame: &RelayFrame) -> Result<(), StreamError> {
        let mut line = serde_json::to_vec(frame).map_err(|_| StreamError::InvalidFrame)?;
        line.push(b'\n');
        if self.pending.len() + line.len() > MAX_PENDING {
            return Err(StreamError::Backpressure);
        }
        self.pending.extend(line);
        self.flush()
    }

    async fn recv(&mut self) -> Result<RelayFrame, StreamError> {
        loop {
            match self.stopped.try_recv() {
                Ok(reason) => return Err(StreamError::Closed(reason)),
                Err(oneshot::error::TryRecvError::Closed) => return Err(StreamError::Disconnected),
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
            if self.offset == self.chunk.len() {
                let frame = tokio::select! {
                    biased;
                    reason = &mut self.stopped => return Err(reason.map_or(StreamError::Disconnected, StreamError::Closed)),
                    frame = self.rx.recv() => frame.ok_or(StreamError::Disconnected)?,
                };
                if frame.kind == FrameKind::Credit {
                    let credit =
                        decode_credit(&frame.payload).map_err(|_| StreamError::InvalidFrame)?;
                    self.send_credit = self
                        .send_credit
                        .saturating_add(credit as usize)
                        .min(INITIAL_CREDIT);
                    self.flush()?;
                    continue;
                }
                self.chunk = frame.payload;
                self.offset = 0;
            }
            let rest = &self.chunk[self.offset..];
            let newline = rest.iter().position(|&b| b == b'\n');
            let len = newline.map_or(rest.len(), |pos| pos + 1);
            if self.line.len() + len > MAX_PENDING {
                return Err(StreamError::InvalidFrame);
            }
            self.line.extend_from_slice(&rest[..len]);
            self.offset += len;
            if self.offset == self.chunk.len() {
                self.receive_credit
                    .fetch_add(self.offset, Ordering::Relaxed);
                self.emit(Frame::credit(self.channel, self.offset as u32))?;
            }
            if newline.is_some() {
                let result =
                    serde_json::from_slice(&self.line).map_err(|_| StreamError::InvalidFrame);
                self.line.clear();
                return result;
            }
        }
    }
}

#[cfg(test)]
mod tests;
