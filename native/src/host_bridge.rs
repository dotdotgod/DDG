//! Background Tokio bridge connecting GPUI (`Desktop`) to one or more
//! `ddg-agents-transport::HostClient` instances without ever blocking the UI thread.

use crate::agent_launcher::LaunchTarget;
use crate::config_watch::{ConfigReloadState, spawn_config_watcher};
use crate::ui_scale::UiPrefs;
use ddg_agents_proto::host::{
    Catalog, ExecutorSpec, ResetNotice, RoomCreateParams, RoomId, RoomLayout, SessionMoveTarget,
    SplitAxis, TabActivity, TabCreateParams, TabId, TabSplitParams,
};
use ddg_agents_proto::rpc::ErrorKind;
use ddg_agents_proto::terminal::ControlMode;
use ddg_agents_transport::agent::{AgentsConfig, ResolvedAgent};
use ddg_agents_transport::programs::ProgramsConfig;
use ddg_agents_transport::{
    ConnectionState, DialErrorKind, HostClient, HostEvent, HostProfile, HostsConfig,
    TabStreamHandle, dial_and_handshake, discover_docker_containers, discover_ssh_config_hosts,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

pub enum BridgeEvent {
    StateChanged(ConnectionState),
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
    Data(Vec<u8>),
    Resize(u16, u16),
    Close(String),
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

struct ClientState {
    client: Arc<HostClient>,
    forward: AbortHandle,
    generation: u64,
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

pub struct HostBridge {
    rt: Handle,
    _shutdown: Option<oneshot::Sender<()>>,
    clients: HashMap<String, ClientState>,
    rx: mpsc::UnboundedReceiver<BridgeMessage>,
    tx: mpsc::UnboundedSender<BridgeMessage>,
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
            rx,
            tx,
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
        // This task owns the watcher and ends with the bridge runtime.
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
            self.tx.clone(),
        );
    }

    fn spawn_client(
        rt: &Handle,
        profile: HostProfile,
        generation: u64,
        out_tx: mpsc::UnboundedSender<BridgeMessage>,
    ) -> ClientState {
        let host_id = profile.id.clone();
        let _guard = rt.enter();
        let (client, mut ev_rx) = HostClient::start_managed(profile, "ddg-native");
        let forward = rt.spawn(async move {
            while let Some(ev) = ev_rx.recv().await {
                let mapped = match ev {
                    HostEvent::StateChanged(state) => BridgeEvent::StateChanged(state),
                    HostEvent::VersionInfo {
                        app_version,
                        platform,
                        terminal_server_version,
                        holder_protocol_version,
                        active_pty_count,
                        rollback_version,
                    } => BridgeEvent::VersionInfo(crate::update_controller::VersionInfo {
                        app_version,
                        platform,
                        terminal_server_version,
                        holder_protocol_version,
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
                    } => BridgeEvent::TabStream {
                        tab_id,
                        channel,
                        frame: TabFrame::Data(bytes),
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
        }
    }

    pub fn try_recv(&mut self) -> Option<BridgeMessage> {
        while let Ok(message) = self.rx.try_recv() {
            // The retired client may already have queued events when the config changes.
            if let BridgeMessage::Host(id, generation, _) = &message
                && *generation != self.generation(id)
            {
                continue;
            }
            return Some(message);
        }
        None
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
            state.forward.abort();
            state.client.close();
        }
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
                ddg_agents_transport::install_remote_host(&profile, Some(progress_tx)),
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
                    let _ = tx.send(BridgeMessage::Error(format!("룸 생성 실패: {}", e.message)));
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
                    let _ = tx.send(BridgeMessage::Error("에이전트 실행 실패".into()));
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
                let _ = tx.send(BridgeMessage::Error("Peek 제어 요청 실패".into()));
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
                Err(_) => BridgeMessage::Error("백그라운드 세션 이동 실패".into()),
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
                    "룸 이름 변경 실패: {}",
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
                    let _ = tx.send(BridgeMessage::Error(format!("탭 생성 실패: {}", e.message)));
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
                        "화면 분할 실패: 원격 호스트 업그레이드가 필요합니다 (ddg machine install)"
                            .into(),
                    )
                }
                Err(e) => BridgeMessage::Error(format!("화면 분할 실패: {}", e.message)),
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
                None => Some("머신 연결이 없습니다.".into()),
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
                let _ = tx.send(BridgeMessage::Error(format!("레이아웃 저장 실패: {error}")));
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
                    .map_err(|e| format!("탭 연결 실패: {}", e.message)),
                None => Err("머신 연결이 없습니다.".into()),
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
                        "제어 요청 실패: {}",
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
                        message: format!("탭 종료 실패: {}", e.message),
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
                    "세션 재시작 실패: {}",
                    e.message
                )));
            }
        });
    }

    pub fn send_input(&self, host_id: &str, channel: u32, bytes: &[u8]) {
        if let Some(client) = self.client(host_id) {
            let _ = client.send_input(channel, bytes);
        }
    }

    pub fn send_resize(&self, host_id: &str, channel: u32, cols: u16, rows: u16) {
        if let Some(client) = self.client(host_id) {
            let _ = client.send_resize(channel, cols, rows);
        }
    }
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

impl Drop for HostBridge {
    fn drop(&mut self) {
        // Window close / Cmd+Q = detach only; remote PTYs stay alive!
        for state in self.clients.values() {
            state.forward.abort();
            state.client.close();
        }
    }
}
