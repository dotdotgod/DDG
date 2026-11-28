//! Background Tokio bridge connecting GPUI (`Desktop`) to one or more
//! `ddg-agents-transport::HostClient` instances without ever blocking the UI thread.

use crate::agent_launcher::LaunchTarget;
use crate::config_watch::{ConfigReloadState, auto_seed_detected_presets, spawn_config_watcher};
use crate::ui_scale::UiPrefs;
use ddg_agents_proto::host::{
    Catalog, ExecutorSpec, FEATURE_AGENT_PRESETS_V1, HostHelloResult, PresetDetectStatus,
    ResetNotice, RoomCreateParams, RoomId, RoomLayout, SessionMoveTarget, SplitAxis, TabActivity,
    TabCreateParams, TabId, TabSplitParams,
};
use ddg_agents_proto::rpc::ErrorKind;
use ddg_agents_proto::terminal::ControlMode;
use ddg_agents_transport::agent::{AgentsConfig, ResolvedAgent};
use ddg_agents_transport::programs::ProgramsConfig;
use ddg_agents_transport::{
    ConnectionState, DialErrorKind, HostClient, HostEvent, HostProfile, HostTransport, HostsConfig,
    PresetDetectOutcome, TabStreamHandle, dial_and_handshake, discover_docker_containers,
    discover_ssh_config_hosts,
};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

pub enum BridgeEvent {
    StateChanged(ConnectionState),
    AgentPresetsDetected {
        request_id: u64,
        connection: Option<PresetConnection>,
        result: Result<PresetDetectOutcome, String>,
    },
    VersionInfo(crate::update_controller::VersionInfo),
    CatalogSubscribed {
        seq: u64,
        catalog: Catalog,
        reset_notice: Option<ResetNotice>,
        activities: Vec<TabActivity>,
    },
    CatalogUpdated {
        seq: u64,
        catalog: Catalog,
    },
    TabActivityChanged(TabActivity),
    TabSeenChanged {
        tab_id: TabId,
        run_generation: u64,
        seen_seq: u64,
        completed: bool,
    },
    TabControlChanged {
        tab_id: TabId,
        mode: ControlMode,
    },
    TabAttached {
        request: u64,
        handle: TabStreamHandle,
    },
    TabAttachFailed {
        request: u64,
        #[expect(
            dead_code,
            reason = "Session IDs still identify tabs after a room move"
        )]
        room_id: RoomId,
        tab_id: TabId,
        run_generation: u64,
        message: String,
    },
    ExternalInput {
        session_id: TabId,
        #[expect(
            dead_code,
            reason = "The badge only uses session, generation, and host identity"
        )]
        pty_id: u32,
        run_generation: u64,
        #[expect(dead_code, reason = "Input length is not shown")]
        byte_len: u32,
        #[expect(dead_code, reason = "Sender session IDs are not shown")]
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
    TabStream {
        tab_id: TabId,
        channel: u32,
        frame: TabFrame,
    },
    LayoutSaveFailed {
        room_id: RoomId,
        attempted: Option<RoomLayout>,
    },
    SelectRoomAndTab {
        room_id: RoomId,
        tab_id: TabId,
    },
    TabCloseFailed {
        tab_id: TabId,
        message: String,
    },
}

pub enum TabFrame {
    Data(TabData),
    Resize(u16, u16),
    Close(String),
}

pub struct TabData {
    bytes: Vec<u8>,
    _credit: Option<ddg_agents_transport::client::TabCredit>,
    #[cfg(test)]
    on_drop: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl std::ops::Deref for TabData {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

impl From<Vec<u8>> for TabData {
    fn from(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            _credit: None,
            #[cfg(test)]
            on_drop: None,
        }
    }
}

#[cfg(test)]
impl TabData {
    pub(crate) fn with_test_credit(
        bytes: Vec<u8>,
        on_drop: impl FnOnce() + Send + Sync + 'static,
    ) -> Self {
        Self {
            bytes,
            _credit: None,
            on_drop: Some(Box::new(on_drop)),
        }
    }
}

#[cfg(test)]
impl Drop for TabData {
    fn drop(&mut self) {
        if let Some(on_drop) = self.on_drop.take() {
            on_drop();
        }
    }
}

pub enum BridgeMessage {
    Host(String, Option<u64>, BridgeEvent),
    UpdateResult {
        req_id: u64,
        machine_id: Option<String>,
        result: crate::update_controller::UpdateResult,
    },
    HostsConfigReloaded(HostsConfig),
    UiPrefsReloaded(UiPrefs),
    AgentsReloaded(Result<AgentsConfig, String>),
    ProgramsReloaded(Result<ProgramsConfig, String>),
    DiscoveredCandidates {
        ssh_hosts: Vec<String>,
        docker_containers: Vec<String>,
    },
    TestConnectionResult {
        req_id: u64,
        result: Result<String, (DialErrorKind, String)>,
    },
    InstallProgress {
        req_id: u64,
        machine_id: Option<String>,
        stage: String,
    },
    InstallResult {
        req_id: u64,
        machine_id: Option<String>,
        result: Result<String, (DialErrorKind, String)>,
    },
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresetConnection {
    host_id: String,
    conn_id: u64,
    boot_generation: u64,
    epoch: u64,
    supported: bool,
}

impl PresetConnection {
    fn from_hello(hello: HostHelloResult, epoch: u64) -> Self {
        Self {
            host_id: hello.host_id,
            conn_id: hello.conn_id,
            boot_generation: hello.boot_generation,
            epoch,
            supported: hello.features.iter().any(|f| f == FEATURE_AGENT_PRESETS_V1),
        }
    }

    fn capture(client: &HostClient, epoch: &AtomicU64) -> Option<Self> {
        let before = epoch.load(Ordering::Acquire);
        let hello = client.hello()?;
        (epoch.load(Ordering::Acquire) == before).then(|| Self::from_hello(hello, before))
    }
}

struct PresetFlight {
    connection: PresetConnection,
    modal_request: Option<u64>,
    seed_paths: Option<(PathBuf, PathBuf)>,
    abort: AbortHandle,
}

struct ClientState {
    client: Arc<HostClient>,
    forward: AbortHandle,
    generation: u64,
    preset_flight: Arc<Mutex<Option<PresetFlight>>>,
    seeded_connection: Option<PresetConnection>,
    preset_epoch: Arc<AtomicU64>,
}

fn invalidate_preset_flight(epoch: &AtomicU64, flight: &Mutex<Option<PresetFlight>>) {
    let mut flight = flight.lock().unwrap();
    // conn_id can repeat after a host-only restart while the terminal boot is unchanged.
    epoch.fetch_add(1, Ordering::Release);
    if let Some(old) = flight.take() {
        old.abort.abort();
    }
}

impl ClientState {
    fn preset_connection(&self) -> Option<PresetConnection> {
        PresetConnection::capture(&self.client, &self.preset_epoch)
    }

    fn retire(&self) {
        invalidate_preset_flight(&self.preset_epoch, &self.preset_flight);
        self.forward.abort();
        self.client.close();
    }
}

#[derive(Default)]
pub struct PeekClaim(Mutex<PeekClaimState>);

#[derive(Default)]
struct PeekClaimState {
    acquired: bool,
    finished: bool,
    closed: Option<(Option<u32>, bool)>,
}

impl PeekClaim {
    // If the RPC is still in flight, its task owns cleanup so a late claim cannot leak.
    pub fn close(&self, channel: Option<u32>, release: bool) -> Option<bool> {
        let mut state = self.0.lock().unwrap();
        if state.finished {
            Some(release && state.acquired)
        } else {
            state.closed = Some((channel, release));
            None
        }
    }

    pub fn is_finished(&self) -> bool {
        self.0.lock().unwrap().finished
    }

    pub fn relinquished(&self) {
        self.0.lock().unwrap().acquired = false;
    }
}

pub struct QueuedMessage {
    pub message: BridgeMessage,
    pub queued_at: Option<Instant>,
}

impl BridgeMessage {
    pub fn data_bytes(&self) -> usize {
        match self {
            Self::Host(
                _,
                _,
                BridgeEvent::TabStream {
                    frame: TabFrame::Data(data),
                    ..
                },
            ) => data.len(),
            _ => 0,
        }
    }
}

#[derive(Clone)]
struct BridgeSender(mpsc::UnboundedSender<QueuedMessage>);

impl BridgeSender {
    fn send(&self, message: BridgeMessage) -> Result<(), ()> {
        self.0
            .send(QueuedMessage {
                message,
                queued_at: crate::perf_enabled().then(Instant::now),
            })
            .map_err(|_| ())
    }

    fn stream_data(
        &self,
        host: &str,
        generation: u64,
        tab_id: TabId,
        channel: u32,
        bytes: Vec<u8>,
    ) -> Result<(), ()> {
        let send = |tab_id, data: Vec<u8>| {
            self.send(BridgeMessage::Host(
                host.to_owned(),
                Some(generation),
                BridgeEvent::TabStream {
                    tab_id,
                    channel,
                    frame: TabFrame::Data(data.into()),
                },
            ))
        };
        let chunk = ddg_agents_proto::frame::DATA_CHUNK;
        if bytes.len() <= chunk {
            return send(tab_id, bytes);
        }
        // Bound even a peer's oversized DATA event without moving Resize/Close ahead of it.
        for data in bytes.chunks(chunk) {
            send(tab_id.clone(), data.to_vec())?;
        }
        Ok(())
    }
}

pub struct BridgeInbox {
    rx: mpsc::UnboundedReceiver<QueuedMessage>,
    replay: VecDeque<QueuedMessage>,
}

impl BridgeInbox {
    pub async fn recv(&mut self) -> Option<QueuedMessage> {
        match self.replay.pop_front() {
            Some(item) => Some(item),
            None => self.rx.recv().await,
        }
    }

    pub fn try_recv(&mut self) -> Option<QueuedMessage> {
        self.replay.pop_front().or_else(|| self.rx.try_recv().ok())
    }

    pub fn len(&self) -> usize {
        self.replay.len() + self.rx.len()
    }

    pub fn replay(
        &mut self,
        host: &str,
        generation: Option<u64>,
        tab_id: &str,
        frames: Vec<(u32, TabFrame)>,
    ) {
        let queued_at = crate::perf_enabled().then(Instant::now);
        for (channel, frame) in frames.into_iter().rev() {
            self.replay.push_front(QueuedMessage {
                message: BridgeMessage::Host(
                    host.to_owned(),
                    generation,
                    BridgeEvent::TabStream {
                        tab_id: tab_id.to_owned(),
                        channel,
                        frame,
                    },
                ),
                queued_at,
            });
        }
    }
}

async fn resolve_cross_darwin_daemons(
    platform: ddg_agents_transport::RemotePlatform,
    progress: Option<mpsc::UnboundedSender<String>>,
) -> Result<ddg_agents_transport::VerifiedDaemonPair, ddg_agents_transport::DialError> {
    let rt = Handle::current();
    let (tx, rx) = oneshot::channel();
    // The updater's streaming futures are !Send. Drive them on the existing runtime without blocking its GUI bridge thread.
    std::thread::Builder::new()
        .name("ddg-verified-bootstrap".into())
        .spawn(move || {
            let result = rt.block_on(
                ddg_agents_update::bootstrap::resolve_verified_darwin_daemons(
                    platform.slug(),
                    env!("CARGO_PKG_VERSION"),
                    progress.as_ref(),
                ),
            );
            let _ = tx.send(result);
        })
        .map_err(|e| {
            ddg_agents_transport::DialError::new(DialErrorKind::Unavailable, e.to_string())
        })?;
    let pair = rx.await.map_err(|_| {
        ddg_agents_transport::DialError::new(DialErrorKind::Unavailable, "release verifier stopped")
    })??;
    Ok(ddg_agents_transport::VerifiedDaemonPair {
        host_bin: pair.host_bin,
        host_sha256: pair.host_sha256,
        terminal_server_bin: pair.terminal_server_bin,
        terminal_server_sha256: pair.terminal_server_sha256,
    })
}

pub struct HostBridge {
    rt: Handle,
    _shutdown: Option<oneshot::Sender<()>>,
    clients: HashMap<String, ClientState>,
    inbox: Option<BridgeInbox>,
    tx: BridgeSender,
    req_seq: AtomicU64,
}

impl HostBridge {
    pub fn new(profiles: &[HostProfile]) -> Self {
        let (handle_tx, handle_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        std::thread::Builder::new()
            .name("ddg-host-bridge".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build tokio runtime for host bridge");
                let _ = handle_tx.send(rt.handle().clone());
                rt.block_on(async move {
                    let _ = stop_rx.await;
                });
            })
            .expect("spawn host bridge thread");
        let rt = handle_rx.recv().expect("receive tokio handle");
        let (tx, rx) = mpsc::unbounded_channel();
        let mut bridge = Self {
            rt,
            _shutdown: Some(stop_tx),
            clients: HashMap::new(),
            inbox: Some(BridgeInbox {
                rx,
                replay: VecDeque::new(),
            }),
            tx: BridgeSender(tx),
            req_seq: AtomicU64::new(1),
        };
        for profile in profiles {
            bridge.add_or_update_machine(profile.clone());
        }
        bridge
    }

    pub fn start_config_watcher(
        &self,
        hosts_path: Option<PathBuf>,
        initial_hosts: HostsConfig,
        ui_prefs_path: Option<PathBuf>,
        initial_ui: UiPrefs,
    ) {
        // The watcher has a plain message channel; route its events through the same UI inbox.
        let (config_tx, mut config_rx) = mpsc::unbounded_channel();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            while let Some(message) = config_rx.recv().await {
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        spawn_config_watcher(
            &self.rt,
            ConfigReloadState {
                hosts_path,
                last_hosts: initial_hosts,
                ui_prefs_path,
                last_ui: initial_ui,
                agents_path: AgentsConfig::default_path().ok(),
                programs_path: ProgramsConfig::default_path().ok(),
            },
            config_tx,
        );
    }

    fn spawn_client(
        rt: &Handle,
        profile: HostProfile,
        generation: u64,
        out_tx: BridgeSender,
    ) -> ClientState {
        let host_id = profile.id.clone();
        let _guard = rt.enter();
        let (client, mut ev_rx) =
            HostClient::start_managed_with_deferred_tab_credit(profile, "ddg-native");
        let preset_flight = Arc::new(Mutex::new(None));
        let preset_epoch = Arc::new(AtomicU64::new(0));
        let flight = preset_flight.clone();
        let epoch = preset_epoch.clone();
        let forward = rt.spawn(async move {
            while let Some(ev) = ev_rx.recv().await {
                let mapped = match ev {
                    HostEvent::StateChanged(state) => {
                        if !matches!(state, ConnectionState::Connected { .. }) {
                            invalidate_preset_flight(&epoch, &flight);
                        }
                        BridgeEvent::StateChanged(state)
                    }
                    HostEvent::VersionInfo {
                        app_version,
                        platform,
                        terminal_server_version,
                        #[cfg(target_os = "macos")]
                        holder_protocol_version,
                        #[cfg(target_os = "macos")]
                        active_pty_count,
                        rollback_version,
                        ..
                    } => BridgeEvent::VersionInfo(crate::update_controller::VersionInfo {
                        app_version,
                        platform,
                        terminal_server_version,
                        #[cfg(target_os = "macos")]
                        holder_protocol_version,
                        #[cfg(target_os = "macos")]
                        active_pty_count,
                        rollback_version,
                    }),
                    HostEvent::CatalogSubscribed {
                        seq,
                        catalog,
                        reset_notice,
                        activities,
                    } => BridgeEvent::CatalogSubscribed {
                        seq,
                        catalog,
                        reset_notice,
                        activities,
                    },
                    HostEvent::TabActivityChanged {
                        tab_id,
                        run_generation,
                        agent_status,
                        foreground,
                        activity_seq,
                        seen_seq,
                        ..
                    } => BridgeEvent::TabActivityChanged(TabActivity {
                        tab_id,
                        run_generation,
                        agent_status,
                        foreground,
                        activity_seq,
                        seen_seq,
                    }),
                    HostEvent::TabSeenChanged {
                        tab_id,
                        run_generation,
                        seen_seq,
                        ..
                    } => BridgeEvent::TabSeenChanged {
                        tab_id,
                        run_generation,
                        seen_seq,
                        completed: false,
                    },
                    HostEvent::CatalogChanged { seq, catalog } => {
                        BridgeEvent::CatalogUpdated { seq, catalog }
                    }
                    HostEvent::TabControlChanged { tab_id, mode, .. } => {
                        BridgeEvent::TabControlChanged { tab_id, mode }
                    }
                    HostEvent::ExternalInput {
                        session_id,
                        pty_id,
                        run_generation,
                        byte_len,
                        from_session_id,
                        from_host_id,
                        is_remote,
                    } => BridgeEvent::ExternalInput {
                        session_id,
                        pty_id,
                        run_generation,
                        byte_len,
                        from_session_id,
                        from_host_id,
                        is_remote,
                    },
                    HostEvent::SessionExited {
                        session_id,
                        room_id,
                        run_generation,
                        exit_code,
                    } => BridgeEvent::SessionExited {
                        session_id,
                        room_id,
                        run_generation,
                        exit_code,
                    },
                    HostEvent::StreamData {
                        tab_id,
                        channel,
                        bytes,
                    } => {
                        if out_tx
                            .stream_data(&host_id, generation, tab_id, channel, bytes)
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                    HostEvent::DeferredStreamData {
                        tab_id,
                        channel,
                        bytes,
                        credit,
                    } => BridgeEvent::TabStream {
                        tab_id,
                        channel,
                        frame: TabFrame::Data(TabData {
                            bytes,
                            _credit: Some(credit),
                            #[cfg(test)]
                            on_drop: None,
                        }),
                    },
                    HostEvent::StreamResize {
                        tab_id,
                        channel,
                        cols,
                        rows,
                    } => BridgeEvent::TabStream {
                        tab_id,
                        channel,
                        frame: TabFrame::Resize(cols, rows),
                    },
                    HostEvent::StreamClose {
                        tab_id,
                        channel,
                        reason,
                    } => BridgeEvent::TabStream {
                        tab_id,
                        channel,
                        frame: TabFrame::Close(reason),
                    },
                };
                if out_tx
                    .send(BridgeMessage::Host(
                        host_id.clone(),
                        Some(generation),
                        mapped,
                    ))
                    .is_err()
                {
                    break;
                }
            }
        });
        ClientState {
            client,
            forward: forward.abort_handle(),
            generation,
            preset_flight,
            seeded_connection: None,
            preset_epoch,
        }
    }

    pub fn take_inbox(&mut self) -> BridgeInbox {
        self.inbox.take().expect("one UI consumer")
    }

    #[cfg(test)]
    pub(crate) fn enqueue_test(&self, message: BridgeMessage) {
        self.tx.send(message).unwrap();
    }

    pub fn is_current(&self, message: &BridgeMessage) -> bool {
        // The retired client may already have queued events when the config changes.
        match message {
            BridgeMessage::Host(
                id,
                generation,
                BridgeEvent::AgentPresetsDetected { connection, .. },
            ) => {
                *generation == self.generation(id)
                    && *connection == self.preset_connection(id)
            }
            BridgeMessage::Host(id, generation, _) => *generation == self.generation(id),
            _ => true,
        }
    }

    pub fn preset_connection(&self, host_id: &str) -> Option<PresetConnection> {
        self.clients.get(host_id).and_then(ClientState::preset_connection)
    }

    pub fn add_or_update_machine(&mut self, profile: HostProfile) {
        self.remove_machine(&profile.id);
        let id = profile.id.clone();
        let generation = self.req_seq.fetch_add(1, Ordering::Relaxed);
        let client = Self::spawn_client(&self.rt, profile, generation, self.tx.clone());
        self.clients.insert(id, client);
    }

    pub fn remove_machine(&mut self, host_id: &str) {
        if let Some(state) = self.clients.remove(host_id) {
            state.retire();
        }
    }

    pub fn detect_agent_presets(&self, host_id: &str) -> u64 {
        let request_id = self.req_seq.fetch_add(1, Ordering::Relaxed);
        if let Some(state) = self.clients.get(host_id)
            && let Some(connection) = state.preset_connection()
        {
            self.probe_presets(state, connection, Some(request_id), None);
        } else {
            let _ = self.tx.send(BridgeMessage::Host(
                host_id.into(),
                self.generation(host_id),
                BridgeEvent::AgentPresetsDetected {
                    request_id,
                    connection: None,
                    result: Err("Not connected to this machine".into()),
                },
            ));
        }
        request_id
    }

    pub fn start_preset_seed(&mut self, host_id: &str, ui_path: PathBuf, agents_path: PathBuf) {
        let Some(state) = self.clients.get_mut(host_id) else {
            return;
        };
        if !matches!(state.client.profile().transport, HostTransport::Local { .. }) {
            return;
        }
        let Some(connection) = state.preset_connection() else {
            return;
        };
        if !connection.supported || state.seeded_connection.as_ref() == Some(&connection) {
            return;
        }
        state.seeded_connection = Some(connection.clone());
        self.probe_presets(
            &self.clients[host_id],
            connection,
            None,
            Some((ui_path, agents_path)),
        );
    }

    fn probe_presets(
        &self,
        state: &ClientState,
        connection: PresetConnection,
        modal_request: Option<u64>,
        seed_paths: Option<(PathBuf, PathBuf)>,
    ) {
        let mut pending = state.preset_flight.lock().unwrap();
        if state.preset_epoch.load(Ordering::Acquire) != connection.epoch {
            return;
        }
        if let Some(flight) = pending
            .as_mut()
            .filter(|flight| flight.connection == connection)
        {
            if modal_request.is_some() {
                flight.modal_request = modal_request;
            }
            if seed_paths.is_some() {
                flight.seed_paths = seed_paths;
            }
            return;
        }
        if let Some(flight) = pending.take() {
            flight.abort.abort();
        }
        let client = state.client.clone();
        let host_id = client.profile().id.clone();
        let generation = state.generation;
        let epoch = state.preset_epoch.clone();
        let flight_slot = state.preset_flight.clone();
        let snapshot = connection.clone();
        let tx = self.tx.clone();
        let task = self.rt.spawn(async move {
            let current = || PresetConnection::capture(&client, &epoch).as_ref() == Some(&snapshot);
            // Managed reconnect and the first RPC poll run on this same current-thread runtime.
            let result = if current() {
                Some(
                    client
                        .detect_agent_presets()
                        .await
                        .map_err(|err| err.to_string()),
                )
            } else {
                None
            };
            let Some(flight) = flight_slot
                .lock()
                .unwrap()
                .take_if(|flight| flight.connection == snapshot)
            else {
                return;
            };
            let Some(result) = result.filter(|_| current()) else {
                return;
            };
            if let Some((ui_path, agents_path)) = flight.seed_paths
                && let Ok(PresetDetectOutcome::Detected(items)) = &result
            {
                let found: Vec<_> = items
                    .iter()
                    .filter(|item| item.status == PresetDetectStatus::Found)
                    .map(|item| item.id.clone())
                    .collect();
                if !found.is_empty() && current() {
                    tokio::task::spawn_blocking(move || {
                        if PresetConnection::capture(&client, &epoch).as_ref() == Some(&snapshot)
                            && let Err(err) =
                                auto_seed_detected_presets(&ui_path, &agents_path, &found)
                        {
                            eprintln!("Skipping automatic agent preset registration: {err}");
                        }
                    });
                }
            }
            if let Some(request_id) = flight.modal_request {
                let _ = tx.send(BridgeMessage::Host(
                    host_id,
                    Some(generation),
                    BridgeEvent::AgentPresetsDetected {
                        request_id,
                        connection: Some(flight.connection),
                        result,
                    },
                ));
            }
        });
        *pending = Some(PresetFlight {
            connection,
            modal_request,
            seed_paths,
            abort: task.abort_handle(),
        });
    }

    pub fn discover_candidates(&self) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let ssh_hosts = discover_ssh_config_hosts();
            let docker_containers = discover_docker_containers();
            let _ = tx.send(BridgeMessage::DiscoveredCandidates {
                ssh_hosts,
                docker_containers,
            });
        });
    }

    pub fn run_update<F: std::future::Future<Output = crate::update_controller::UpdateResult>>(
        &self,
        machine_id: Option<String>,
        work: impl FnOnce() -> F + Send + 'static,
    ) -> u64 {
        let req_id = self.req_seq.fetch_add(1, Ordering::Relaxed);
        let tx = self.tx.clone();
        let rt = self.rt.clone();
        self.rt.spawn_blocking(move || {
            let result = rt.block_on(work());
            let _ = tx.send(BridgeMessage::UpdateResult {
                req_id,
                machine_id,
                result,
            });
        });
        req_id
    }

    pub fn test_connection(&self, profile: HostProfile) -> u64 {
        let req_id = self.req_seq.fetch_add(1, Ordering::Relaxed);
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let result = match dial_and_handshake(&profile, "ddg-native-test").await {
                Ok(t) => Ok(t.hello.host_id),
                Err(err) => Err((err.kind, err.message)),
            };
            let _ = tx.send(BridgeMessage::TestConnectionResult { req_id, result });
        });
        req_id
    }

    pub fn install_remote_host(&self, profile: HostProfile, machine_id: Option<String>) -> u64 {
        let req_id = self.req_seq.fetch_add(1, Ordering::Relaxed);
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
            let forward_progress = async {
                while let Some(stage) = progress_rx.recv().await {
                    let _ = tx.send(BridgeMessage::InstallProgress {
                        req_id,
                        machine_id: machine_id.clone(),
                        stage,
                    });
                }
            };
            let (result, ()) = tokio::join!(
                ddg_agents_transport::install_remote_host_with_resolver(
                    &profile,
                    Some(progress_tx),
                    resolve_cross_darwin_daemons,
                ),
                forward_progress,
            );
            let result = result
                .map(|info| info.host_id)
                .map_err(|err| (err.kind, err.message));
            let _ = tx.send(BridgeMessage::InstallResult {
                req_id,
                machine_id,
                result,
            });
        });
        req_id
    }

    fn next_req_id(&self, prefix: &str) -> String {
        let n = self.req_seq.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{}-{n}", std::process::id())
    }

    fn client(&self, host_id: &str) -> Option<Arc<HostClient>> {
        self.clients.get(host_id).map(|state| state.client.clone())
    }

    fn generation(&self, host_id: &str) -> Option<u64> {
        self.clients.get(host_id).map(|state| state.generation)
    }

    pub fn create_room(&self, host_id: &str, cols: u16, rows: u16) {
        let Some(client) = self.client(host_id) else {
            return;
        };
        let generation = self.generation(host_id);
        let host_id = host_id.to_owned();
        let tx = self.tx.clone();
        let req_id = self.next_req_id("room-create");
        self.rt.spawn(async move {
            match client
                .room_create(
                    None,
                    None,
                    ExecutorSpec::DefaultShell,
                    cols,
                    rows,
                    Some(req_id),
                )
                .await
            {
                Ok(res) => {
                    if let Some(tab) = res.room.tabs.first() {
                        let _ = tx.send(BridgeMessage::Host(
                            host_id,
                            generation,
                            BridgeEvent::SelectRoomAndTab {
                                room_id: res.room.id,
                                tab_id: tab.id.clone(),
                            },
                        ));
                    }
                }
                Err(e) => {
                    let _ = tx.send(BridgeMessage::Error(format!("Failed to create room: {}", e.message)));
                }
            }
        });
    }

    pub fn launch_agent(
        &self,
        host: &str,
        target: LaunchTarget,
        profile: String,
        resolved: ResolvedAgent,
        (cols, rows): (u16, u16),
    ) {
        let Some(client) = self.client(host) else {
            return;
        };
        let generation = self.generation(host);
        let host = host.to_owned();
        let tx = self.tx.clone();
        let request_id = Some(self.next_req_id("agent-launch"));
        let agent_profile = Some(profile);
        let ResolvedAgent {
            executor,
            cwd: space_cwd,
        } = resolved;
        self.rt.spawn(async move {
            let result = match target {
                LaunchTarget::Room => client
                    .room_create_with_agent(RoomCreateParams {
                        agent_profile,
                        request_id,
                        name: None,
                        space_cwd,
                        executor,
                        cols,
                        rows,
                    })
                    .await
                    .map(|res| {
                        res.room
                            .tabs
                            .first()
                            .map(|tab| (res.room.id.clone(), tab.id.clone()))
                    }),
                LaunchTarget::Split(anchor_tab_id) => client
                    .tab_split_with_agent(TabSplitParams {
                        agent_profile,
                        request_id,
                        anchor_tab_id,
                        axis: SplitAxis::Horizontal,
                        ratio_bp: None,
                        space_cwd,
                        title: None,
                        executor,
                        cols,
                        rows,
                    })
                    .await
                    .map(|res| Some((res.room.id, res.tab.id))),
                target => {
                    let room_id = match target {
                        LaunchTarget::Tab(id) => Some(id),
                        _ => None,
                    };
                    let background = room_id.is_none();
                    client
                        .tab_create_with_agent(TabCreateParams {
                            agent_profile,
                            request_id,
                            room_id: room_id.clone(),
                            background,
                            space_cwd,
                            title: None,
                            executor,
                            cols,
                            rows,
                        })
                        .await
                        .map(|res| room_id.map(|id| (id, res.tab.id)))
                }
            };
            match result {
                Ok(Some((room_id, tab_id))) => {
                    let _ = tx.send(BridgeMessage::Host(
                        host,
                        generation,
                        BridgeEvent::SelectRoomAndTab { room_id, tab_id },
                    ));
                }
                Ok(None) => {}
                Err(_) => {
                    let _ = tx.send(BridgeMessage::Error("Failed to launch agent".into()));
                }
            }
        });
    }

    pub fn mark_seen(&self, host: &str, tab_id: TabId, run_generation: u64, seq: u64) {
        let Some(client) = self.client(host) else {
            return;
        };
        let generation = self.generation(host);
        let host = host.to_owned();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let seen_seq = client
                .tab_mark_seen(tab_id.clone(), run_generation, seq)
                .await
                .ok()
                .flatten()
                .map_or(0, |result| result.seen_seq);
            let _ = tx.send(BridgeMessage::Host(
                host,
                generation,
                BridgeEvent::TabSeenChanged {
                    tab_id,
                    run_generation,
                    seen_seq,
                    completed: true,
                },
            ));
        });
    }

    pub fn claim_peek(&self, host: &str, tab_id: TabId) -> Option<Arc<PeekClaim>> {
        let client = self.client(host)?;
        let generation = self.generation(host);
        let host = host.to_owned();
        let tx = self.tx.clone();
        let ConnectionState::Connected { conn_id, .. } = client.state() else {
            return None;
        };
        let claim = Arc::new(PeekClaim::default());
        let task = claim.clone();
        self.rt.spawn(async move {
            let acquired = client.tab_claim_control(tab_id.clone(), true).await.is_ok();
            if !matches!(client.state(), ConnectionState::Connected { conn_id: current, .. } if current == conn_id) { return; }
            let closed = {
                let mut state = task.0.lock().unwrap();
                state.finished = true;
                state.acquired = acquired;
                state.closed
            };
            if let Some((channel, release)) = closed {
                if acquired && release {
                    let _ = client.tab_release_control(tab_id).await;
                }
                if let Some(channel) = channel { client.detach_channel(channel); }
            } else if acquired {
                let _ = tx.send(BridgeMessage::Host(host, generation,
                    BridgeEvent::TabControlChanged { tab_id, mode: ControlMode::Control }));
            } else {
                let _ = tx.send(BridgeMessage::Error("Failed to request Peek control".into()));
            }
        });
        Some(claim)
    }

    pub fn close_peek(&self, host: &str, tab_id: TabId, channel: Option<u32>, release: bool) {
        let Some(client) = self.client(host) else {
            return;
        };
        let ConnectionState::Connected { conn_id, .. } = client.state() else {
            return;
        };
        self.rt.spawn(async move {
            if !matches!(client.state(), ConnectionState::Connected { conn_id: current, .. } if current == conn_id) { return; }
            if release {
                let _ = client.tab_release_control(tab_id).await;
            }
            if let Some(channel) = channel
                && matches!(client.state(), ConnectionState::Connected { conn_id: current, .. } if current == conn_id)
            { client.detach_channel(channel); }
        });
    }

    pub fn move_tab(&self, host: &str, tab: TabId, target: SessionMoveTarget) {
        let Some(client) = self.client(host) else {
            return;
        };
        let generation = self.generation(host);
        let host = host.to_owned();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let message = match client.tab_move(tab.clone(), target).await {
                Ok(res) => BridgeMessage::Host(
                    host,
                    generation,
                    BridgeEvent::SelectRoomAndTab {
                        room_id: res.session.room_id,
                        tab_id: tab,
                    },
                ),
                Err(_) => BridgeMessage::Error("Failed to move background session".into()),
            };
            let _ = tx.send(message);
        });
    }

    pub fn rename_room(&self, host_id: &str, room_id: RoomId, name: Option<String>) {
        let Some(client) = self.client(host_id) else {
            return;
        };
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            if let Err(e) = client.room_rename(room_id, name).await {
                let _ = tx.send(BridgeMessage::Error(format!(
                    "Failed to rename room: {}",
                    e.message
                )));
            }
        });
    }

    pub fn create_tab(&self, host_id: &str, room_id: RoomId, cols: u16, rows: u16) {
        let Some(client) = self.client(host_id) else {
            return;
        };
        let generation = self.generation(host_id);
        let host_id = host_id.to_owned();
        let tx = self.tx.clone();
        let req_id = self.next_req_id("tab-create");
        self.rt.spawn(async move {
            match client
                .tab_create(
                    room_id.clone(),
                    None,
                    ExecutorSpec::DefaultShell,
                    cols,
                    rows,
                    Some(req_id),
                )
                .await
            {
                Ok(res) => {
                    let _ = tx.send(BridgeMessage::Host(
                        host_id,
                        generation,
                        BridgeEvent::SelectRoomAndTab {
                            room_id,
                            tab_id: res.tab.id,
                        },
                    ));
                }
                Err(e) => {
                    let _ = tx.send(BridgeMessage::Error(format!("Failed to create tab: {}", e.message)));
                }
            }
        });
    }

    pub fn split_tab(
        &self,
        host_id: &str,
        anchor_tab_id: TabId,
        axis: SplitAxis,
        cols: u16,
        rows: u16,
    ) {
        let Some(client) = self.client(host_id) else {
            return;
        };
        let generation = self.generation(host_id);
        let host_id = host_id.to_owned();
        let tx = self.tx.clone();
        let req_id = self.next_req_id("tab-split");
        self.rt.spawn(async move {
            let message = match client
                .tab_split(TabSplitParams {
                    agent_profile: None,
                    request_id: Some(req_id),
                    anchor_tab_id,
                    axis,
                    ratio_bp: None,
                    space_cwd: None,
                    title: None,
                    executor: ExecutorSpec::DefaultShell,
                    cols,
                    rows,
                })
                .await
            {
                Ok(res) => BridgeMessage::Host(
                    host_id,
                    generation,
                    BridgeEvent::SelectRoomAndTab {
                        room_id: res.room.id,
                        tab_id: res.tab.id,
                    },
                ),
                Err(e) if e.data.kind == ErrorKind::NotFound && e.data.code.is_none() => {
                    BridgeMessage::Error(
                        "Failed to split pane: remote host upgrade required (ddg machine install)"
                            .into(),
                    )
                }
                Err(e) => BridgeMessage::Error(format!("Failed to split pane: {}", e.message)),
            };
            let _ = tx.send(message);
        });
    }

    pub fn set_room_layout(&self, host_id: &str, room_id: RoomId, layout: Option<RoomLayout>) {
        let client = self.client(host_id);
        let generation = self.generation(host_id);
        let host_id = host_id.to_owned();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let error = match client {
                Some(client) => client
                    .room_set_layout(room_id.clone(), layout.clone())
                    .await
                    .err()
                    .map(|e| e.message),
                None => Some("No machine connection.".into()),
            };
            if let Some(error) = error {
                let _ = tx.send(BridgeMessage::Host(
                    host_id,
                    generation,
                    BridgeEvent::LayoutSaveFailed {
                        room_id,
                        attempted: layout,
                    },
                ));
                let _ = tx.send(BridgeMessage::Error(format!("Failed to save layout: {error}")));
            }
        });
    }

    pub fn attach_tab(
        &self,
        host_id: &str,
        room_id: RoomId,
        tab_id: TabId,
        run_generation: u64,
        (cols, rows): (u16, u16),
        want_control: bool,
    ) -> u64 {
        let request = self.req_seq.fetch_add(1, Ordering::Relaxed);
        let client = self.client(host_id);
        let generation = self.generation(host_id);
        let host_id = host_id.to_owned();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let result = match client {
                Some(client) => client
                    .tab_attach(tab_id.clone(), want_control, cols, rows, 2000)
                    .await
                    .map_err(|e| format!("Failed to attach tab: {}", e.message)),
                None => Err("No machine connection.".into()),
            };
            let event = match result {
                Ok(handle) => BridgeEvent::TabAttached { request, handle },
                Err(message) => BridgeEvent::TabAttachFailed {
                    request,
                    room_id,
                    tab_id,
                    run_generation,
                    message,
                },
            };
            let _ = tx.send(BridgeMessage::Host(host_id, generation, event));
        });
        request
    }

    pub fn detach_channel(&self, host_id: &str, channel: u32) {
        if let Some(client) = self.client(host_id) {
            client.detach_channel(channel);
        }
    }

    pub fn claim_control(&self, host_id: &str, tab_id: TabId, force: bool) {
        let Some(client) = self.client(host_id) else {
            return;
        };
        let generation = self.generation(host_id);
        let host_id = host_id.to_owned();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            match client.tab_claim_control(tab_id.clone(), force).await {
                Ok(_) => {
                    let _ = tx.send(BridgeMessage::Host(
                        host_id,
                        generation,
                        BridgeEvent::TabControlChanged {
                            tab_id,
                            mode: ControlMode::Control,
                        },
                    ));
                }
                Err(e) => {
                    let _ = tx.send(BridgeMessage::Error(format!(
                        "Failed to request control: {}",
                        e.message
                    )));
                }
            }
        });
    }

    pub fn close_tab(&self, host_id: &str, tab_id: TabId) {
        let Some(client) = self.client(host_id) else {
            return;
        };
        let generation = self.generation(host_id);
        let host_id = host_id.to_owned();
        let tx = self.tx.clone();
        let req_id = self.next_req_id("tab-close");
        self.rt.spawn(async move {
            if let Err(e) = client.tab_close(tab_id.clone(), Some(req_id)).await {
                let _ = tx.send(BridgeMessage::Host(
                    host_id,
                    generation,
                    BridgeEvent::TabCloseFailed {
                        tab_id,
                        message: format!("Failed to close tab: {}", e.message),
                    },
                ));
            }
        });
    }

    pub fn restart_tab(&self, host_id: &str, tab_id: TabId, cols: u16, rows: u16) {
        let Some(client) = self.client(host_id) else {
            return;
        };
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            if let Err(e) = client.tab_restart(tab_id, cols, rows).await {
                let _ = tx.send(BridgeMessage::Error(format!(
                    "Failed to restart session: {}",
                    e.message
                )));
            }
        });
    }

    pub fn send_input(&self, host_id: &str, channel: u32, bytes: &[u8]) -> bool {
        self.client(host_id)
            .is_some_and(|client| client.send_input(channel, bytes))
    }

    pub fn send_resize(&self, host_id: &str, channel: u32, cols: u16, rows: u16) {
        if let Some(client) = self.client(host_id) {
            let _ = client.send_resize(channel, cols, rows);
        }
    }
}

#[cfg(test)]
#[test]
fn oversized_data_is_bounded_without_reordering_resize_or_close() {
    let mut bridge = HostBridge::new(&[]);
    let mut inbox = bridge.take_inbox();
    let chunk = ddg_agents_proto::frame::DATA_CHUNK;
    let data = (0..(chunk * 2 + 7))
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    bridge
        .tx
        .stream_data("test", 1, "tab".into(), 7, data.clone())
        .unwrap();
    for frame in [TabFrame::Resize(80, 24), TabFrame::Close(String::new())] {
        bridge.enqueue_test(BridgeMessage::Host(
            "test".into(),
            Some(1),
            BridgeEvent::TabStream {
                tab_id: "tab".into(),
                channel: 7,
                frame,
            },
        ));
    }
    let mut frames = std::iter::from_fn(|| inbox.try_recv()).map(|item| match item.message {
        BridgeMessage::Host(
            _,
            Some(1),
            BridgeEvent::TabStream {
                channel: 7, frame, ..
            },
        ) => frame,
        _ => panic!("unexpected queue item"),
    });
    let mut rebuilt = Vec::new();
    for size in [chunk, chunk, 7] {
        let Some(TabFrame::Data(bytes)) = frames.next() else {
            panic!("DATA must lead")
        };
        assert_eq!(bytes.len(), size);
        rebuilt.extend_from_slice(&bytes);
    }
    assert_eq!(rebuilt, data);
    assert!(matches!(frames.next(), Some(TabFrame::Resize(80, 24))));
    assert!(matches!(frames.next(), Some(TabFrame::Close(_))));
    assert!(frames.next().is_none());
}

#[cfg(test)]
#[test]
fn peek_layout_and_shortcuts_release_only_its_own_claim() {
    for (acquired, eligible, release) in [
        (false, true, false),
        (true, false, false),
        (true, true, true),
    ] {
        let claim = PeekClaim(Mutex::new(PeekClaimState {
            acquired,
            finished: true,
            closed: None,
        }));
        assert_eq!(claim.close(Some(7), eligible), Some(release));
    }
    let pending = PeekClaim::default();
    assert_eq!(pending.close(Some(7), true), None);
    assert_eq!(pending.0.lock().unwrap().closed, Some((Some(7), true)));
}

#[cfg(test)]
#[test]
fn preset_snapshot_rejects_reused_hello_after_disconnect() {
    let hello = serde_json::from_value::<HostHelloResult>(serde_json::json!({
        "protocol_version": 1,
        "host_id": "test",
        "conn_id": 7,
        "boot_generation": 3,
    }))
    .unwrap();
    let epoch = AtomicU64::new(0);
    let before = PresetConnection::from_hello(hello.clone(), epoch.load(Ordering::Acquire));
    assert_eq!(before, PresetConnection::from_hello(hello.clone(), 0));
    invalidate_preset_flight(&epoch, &Mutex::new(None));
    assert_ne!(
        before,
        PresetConnection::from_hello(hello.clone(), epoch.load(Ordering::Acquire))
    );
    assert_ne!(
        before,
        PresetConnection::from_hello(
            HostHelloResult {
                boot_generation: 4,
                ..hello
            },
            0
        )
    );
}

impl Drop for HostBridge {
    fn drop(&mut self) {
        // Window close / Cmd+Q = detach only; remote PTYs stay alive!
        for state in self.clients.values() {
            state.retire();
        }
    }
}
