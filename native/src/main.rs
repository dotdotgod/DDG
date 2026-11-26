mod agent_launcher;
mod agent_list;
mod agent_modal;
mod config_watch;
mod host_bridge;
mod icons;
mod layout;
mod machine_modal;
mod ui_scale;
mod update_apply;
mod update_controller;
mod update_modal;

use agent_launcher::{AgentLauncher, LaunchTarget};
use agent_list::{AgentEntry, Row, build_agent_tree, move_subtree};
use agent_modal::AgentSettingsModal;
use ddg_agents_proto::frame::MAX_PENDING;
use ddg_agents_proto::host::{
    Catalog, ExecutorSpec, MAX_TABS_PER_ROOM, PaneNode, ResetNotice, RoomId, RoomKind, RoomLayout,
    RoomRecord, RoomVisibility, SessionMoveTarget, SplitAxis, TabActivity, TabId, TabRecord,
    TabRuntimeState,
};
use ddg_agents_proto::id::{parse_session_id, sanitize_display_text};
use ddg_agents_proto::terminal::{AgentState, CloseReason, ControlMode};
use ddg_agents_transport::{
    ConnectionState, DialErrorKind, HostProfile, HostTransport, HostsConfig, TabStreamHandle,
};
use ddg_agents_transport::{agent::AgentsConfig, programs::ProgramsConfig};
use ddg_agents_vt::{self as vt, Vt};
use gpui::{prelude::*, *};
use host_bridge::{BridgeEvent, BridgeMessage, HostBridge, TabFrame};
use icons::{AppAssets, Icon, icon};
use layout::{
    DividerHit, NavDir, PANE_DIVIDER, PaneRect, SplitPath, compute_layout, navigate_direction,
    pane_grid, ratio_at, ratio_bp_from_cursor, set_ratio_at,
};
use machine_modal::{MachineModal, MachineTab, ModalField, TestState};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use ui_scale::{AgentSortMode, BASE_TERM_FONT_PX, UI_SCALE_DEFAULT, UiPrefs, UiScale};

use update_controller::{Change, RemoteMachineUpdateState, UpdateController, UpdatePhase};
use update_modal::{BlockedCard, UPDATE_BANNER_HEIGHT, UpdateModal};

const SIDEBAR_MIN: f32 = 160.;
const SIDEBAR_MAX: f32 = 420.;
const DIVIDER_WIDTH: f32 = 6.;
const TAB_BAR_HEIGHT_REM: f32 = 2.;
const RESET_BANNER_HEIGHT: f32 = 32.;
const PANE_INSET: f32 = 1.;
static BUILTIN_PROGRAMS: LazyLock<ProgramsConfig> = LazyLock::new(ProgramsConfig::default);
const ZOOM_INDICATOR_DURATION: Duration = Duration::from_millis(1500);
const DISPLAY_TEXT_LIMIT: usize = 128;
const REMOTE_INSTALL_ACTION: &str = "원격 자동 설치 후 연결";
const REMOTE_INSTALL_MISSING: &str = "원격 머신에 ddg-agents-host가 설치되어 있지 않습니다";

struct TabView {
    id: TabId,
    agent_profile: Option<String>,
    parent: Option<(Option<String>, String)>,
    argv0: Option<String>,
    activity: Option<TabActivity>,
    pending_seen_seq: u64,
    title: String,
    run_generation: u64,
    runtime: TabRuntimeState,
    channel: Option<u32>,
    attaching: bool,
    attach_request: Option<u64>,
    early_frames: Vec<(u32, TabFrame)>,
    early_bytes: usize,
    mode: ControlMode,
    vt: Option<Vt>,
    status: String,
    closing: bool,
    size: (u16, u16),
}

impl TabView {
    fn from_record(rec: &TabRecord, default_size: (u16, u16)) -> Self {
        Self {
            id: rec.id.clone(),
            agent_profile: rec.agent_profile.clone(),
            parent: rec
                .parent_session_id
                .clone()
                .map(|id| (rec.parent_host_id.clone(), id)),
            argv0: match &rec.executor {
                ExecutorSpec::Command { argv, .. } => argv.first().cloned(),
                _ => None,
            },
            activity: None,
            pending_seen_seq: 0,
            title: rec.title.clone(),
            run_generation: rec.run_generation,
            runtime: rec.runtime.clone(),
            channel: None,
            attaching: false,
            attach_request: None,
            early_frames: Vec::new(),
            early_bytes: 0,
            mode: ControlMode::Readonly,
            vt: None,
            status: Self::runtime_status(&rec.runtime),
            closing: false,
            size: default_size,
        }
    }

    fn unseen_sequence(&self) -> Option<u64> {
        self.activity
            .as_ref()
            .filter(|activity| {
                self.is_running()
                    && self.pending_seen_seq == 0
                    && activity.activity_seq > activity.seen_seq
            })
            .map(|activity| activity.activity_seq)
    }

    fn metadata_copy(&self) -> Self {
        Self {
            id: self.id.clone(),
            agent_profile: self.agent_profile.clone(),
            parent: self.parent.clone(),
            argv0: self.argv0.clone(),
            activity: self.activity.clone(),
            pending_seen_seq: 0,
            title: self.title.clone(),
            runtime: self.runtime.clone(),
            run_generation: self.run_generation,
            channel: None,
            attaching: false,
            attach_request: None,
            early_frames: Vec::new(),
            early_bytes: 0,
            mode: ControlMode::Readonly,
            vt: None,
            status: self.status.clone(),
            closing: false,
            size: self.size,
        }
    }

    fn runtime_status(runtime: &TabRuntimeState) -> String {
        match runtime {
            TabRuntimeState::Running { .. } => "연결 준비 중".into(),
            TabRuntimeState::Exited { status } => status.clone(),
            TabRuntimeState::StoppedAfterReboot => "재부팅 후 중단됨 (자동 재실행 안 함)".into(),
        }
    }

    fn is_running(&self) -> bool {
        matches!(self.runtime, TabRuntimeState::Running { .. })
    }

    fn begin_attach(&mut self) -> bool {
        if self.is_running() && self.channel.is_none() && !self.attaching && !self.closing {
            self.attaching = true;
            self.status = "연결 중".into();
            true
        } else {
            false
        }
    }

    fn attach_failed(&mut self, message: &str) {
        self.reset_attach();
        self.status = message.to_owned();
    }

    fn reset_attach(&mut self) {
        self.attaching = false;
        self.attach_request = None;
        self.early_frames.clear();
        self.early_bytes = 0;
    }

    fn buffer_frame(&mut self, channel: u32, frame: TabFrame) {
        // A stream can deliver first paint before its attach reply reaches the UI.
        if self.early_bytes > MAX_PENDING {
            return;
        }
        self.early_bytes += match &frame {
            TabFrame::Data(bytes) => bytes.len(),
            TabFrame::Resize(..) => 0,
            TabFrame::Close(reason) => reason.len(),
        } + std::mem::size_of::<(u32, TabFrame)>();
        if self.early_bytes <= MAX_PENDING {
            self.early_frames.push((channel, frame));
        } else {
            self.early_frames.clear();
        }
    }
}

fn accept_tab_attach(tab: Option<&mut TabView>, handle: &TabStreamHandle) -> Option<u32> {
    let Some(tab) = tab else {
        return Some(handle.channel);
    };
    tab.attaching = false;
    tab.attach_request = None;
    if handle.run_generation < tab.run_generation
        || (handle.run_generation == tab.run_generation && !tab.is_running())
    {
        return Some(handle.channel);
    }

    let stale = tab
        .channel
        .replace(handle.channel)
        .filter(|&ch| ch != handle.channel);
    tab.run_generation = handle.run_generation;
    tab.mode = handle.mode;
    tab.size = (handle.cols, handle.rows);
    tab.status = "실행 중".into();
    tab.vt = Vt::new(handle.cols, handle.rows).ok();
    stale
}

struct RoomView {
    id: RoomId,
    tabs: Vec<TabView>,
    layout: RoomLayout,
    server_layout: RoomLayout,
    selected_group: usize,
    focused_tab_id: Option<TabId>,
    zoomed_tab_id: Option<TabId>,
    name: Option<String>,
    auto_name: String,
}

fn first_leaf(node: &PaneNode) -> &str {
    match node {
        PaneNode::Leaf { tab_id } => tab_id,
        PaneNode::Split { first, .. } => first_leaf(first),
    }
}

impl RoomView {
    fn from_record(rec: &RoomRecord, tabs: Vec<TabView>) -> Self {
        let mut layout = rec
            .layout
            .clone()
            .unwrap_or_else(|| RoomLayout::default_for_tabs(&rec.tabs));
        layout.reconcile(&tabs.iter().map(|tab| tab.id.clone()).collect::<Vec<_>>());
        Self {
            id: rec.id.clone(),
            tabs,
            server_layout: layout.clone(),
            layout,
            selected_group: 0,
            focused_tab_id: None,
            zoomed_tab_id: None,
            name: rec.name.clone(),
            auto_name: rec.auto_name.clone(),
        }
    }

    fn active_group(&self) -> Option<&PaneNode> {
        self.layout.groups.get(self.selected_group)
    }

    fn visible_tab_ids(&self) -> Vec<TabId> {
        let mut ids = Vec::new();
        if let Some(id) = self
            .zoomed_tab_id
            .as_ref()
            .filter(|id| Some(id.as_str()) == self.focused_tab_id())
        {
            ids.push(id.clone());
        } else if let Some(group) = self.active_group() {
            group.leaf_ids(&mut ids);
        }
        ids
    }

    fn focused_tab_id(&self) -> Option<&str> {
        let group = self.active_group()?;
        Some(
            self.focused_tab_id
                .as_deref()
                .filter(|id| group.contains(id))
                .unwrap_or_else(|| first_leaf(group)),
        )
    }

    fn focused_tab(&self) -> Option<&TabView> {
        let id = self.focused_tab_id()?;
        self.tabs.iter().find(|tab| tab.id == id)
    }

    fn focused_tab_mut(&mut self) -> Option<&mut TabView> {
        let id = self.focused_tab_id()?.to_owned();
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    fn select_tab_by_id(&mut self, tab_id: &str) -> bool {
        let Some(index) = self
            .layout
            .groups
            .iter()
            .position(|group| group.contains(tab_id))
        else {
            return false;
        };
        self.selected_group = index;
        self.focused_tab_id = Some(tab_id.to_owned());
        self.repair_selection();
        true
    }

    fn select_group(&mut self, index: usize) {
        self.selected_group = index.min(self.layout.groups.len().saturating_sub(1));
        self.repair_selection();
    }

    fn repair_selection(&mut self) {
        self.focused_tab_id = self.focused_tab_id().map(str::to_owned);
        if self.zoomed_tab_id != self.focused_tab_id || !self.is_active_group_split() {
            self.zoomed_tab_id = None;
        }
    }

    fn preserve_selection(&mut self, previous: &Self) {
        self.select_group(previous.selected_group);
        if !previous
            .focused_tab_id()
            .is_some_and(|id| self.select_tab_by_id(id))
            && let Some(group) = previous.active_group()
        {
            let mut siblings = Vec::new();
            group.leaf_ids(&mut siblings);
            // After closing a focused pane, stay with a surviving sibling if possible.
            siblings.iter().any(|id| self.select_tab_by_id(id));
        }
        self.zoomed_tab_id.clone_from(&previous.zoomed_tab_id);
        self.repair_selection();
    }

    fn is_active_group_split(&self) -> bool {
        matches!(self.active_group(), Some(PaneNode::Split { .. }))
    }

    fn restore_server_layout(&mut self) {
        let focus = self.focused_tab_id().map(str::to_owned);
        self.layout.clone_from(&self.server_layout);
        if !focus.is_some_and(|id| self.select_tab_by_id(&id)) {
            self.select_group(self.selected_group);
        }
    }

    fn label(&self) -> String {
        sanitize_display_text(
            self.name.as_deref().unwrap_or(&self.auto_name),
            DISPLAY_TEXT_LIMIT,
        )
    }
}

struct MachineView {
    profile: HostProfile,
    conn_state: ConnectionState,
    installing_stage: Option<String>,
    install_req_id: Option<u64>,
    remote_update: RemoteMachineUpdateState,
    connected_host_id: Option<String>,
    catalog_seq: Option<u64>,
    rooms: Vec<RoomView>,
    background: Vec<(RoomId, TabView)>,
    update_confirm: bool,
    collapsed: bool,
    reset_notice: Option<ResetNotice>,
    dismissed_notice_ids: HashSet<String>,
}

impl MachineView {
    fn new(profile: HostProfile) -> Self {
        Self {
            profile,
            conn_state: ConnectionState::Connecting,
            installing_stage: None,
            install_req_id: None,
            remote_update: RemoteMachineUpdateState::default(),
            connected_host_id: None,
            catalog_seq: None,
            rooms: Vec::new(),
            background: Vec::new(),
            update_confirm: false,
            collapsed: false,
            reset_notice: None,
            dismissed_notice_ids: HashSet::new(),
        }
    }

    fn is_connected(&self) -> bool {
        matches!(self.conn_state, ConnectionState::Connected { .. })
    }

    fn lacks_activity(&self) -> bool {
        matches!(&self.conn_state, ConnectionState::Connected { features, .. }
            if !features.iter().any(|feature| feature == "agent_activity_v1"))
            && !matches!(self.profile.transport, HostTransport::Local { .. })
    }

    fn is_security_blocked(&self) -> bool {
        matches!(self.conn_state, ConnectionState::HostKeyMismatch(_))
    }

    fn status_dot_color(&self) -> u32 {
        match &self.conn_state {
            ConnectionState::Connected { .. } => 0x9ece6a,
            ConnectionState::Connecting
            | ConnectionState::Reconnecting { .. }
            | ConnectionState::HostNotInstalled(_)
            | ConnectionState::ProtocolIncompatible { .. } => 0xe5c07b,
            ConnectionState::HostKeyMismatch(_) | ConnectionState::AuthRequired(_) => 0xe06c75,
            ConnectionState::Disconnected(_) => 0x606060,
        }
    }

    fn update_reset_notice(&mut self, notice: Option<ResetNotice>) {
        self.reset_notice =
            notice.filter(|notice| !self.dismissed_notice_ids.contains(&notice.notice_id));
    }

    fn dismiss_reset_notice(&mut self, notice_id: &str) {
        if self
            .reset_notice
            .as_ref()
            .is_some_and(|notice| notice.notice_id == notice_id)
        {
            self.dismissed_notice_ids.insert(notice_id.to_owned());
            self.reset_notice = None;
        }
    }
}

#[derive(Default)]
struct HostsConfigChanges {
    reconnect: Vec<HostProfile>,
    removed: Vec<String>,
    active_host_changed: bool,
}

impl HostsConfigChanges {
    fn discard_stale<T>(&self, target: &mut Option<T>, host: impl FnOnce(&T) -> &str) {
        if let Some(id) = target.as_ref().map(host)
            && (self.removed.iter().any(|removed| removed == id)
                || self.reconnect.iter().any(|profile| profile.id == id))
        {
            *target = None;
        }
    }
}

fn reconcile_hosts_config(
    machines: &mut Vec<MachineView>,
    default_host_id: &mut String,
    active_host_id: &mut String,
    active_room_id: &mut Option<RoomId>,
    cfg: HostsConfig,
) -> Option<HostsConfigChanges> {
    if *default_host_id == cfg.default_host_id && machines.iter().map(|m| &m.profile).eq(&cfg.hosts)
    {
        return None;
    }

    let mut changes = HostsConfigChanges::default();
    let fallback = (!cfg
        .hosts
        .iter()
        .any(|profile| profile.id == *active_host_id))
    .then(|| cfg.default_profile().id);
    let mut previous: HashMap<_, _> = machines
        .drain(..)
        .map(|m| (m.profile.id.clone(), m))
        .collect();
    machines.extend(cfg.hosts.into_iter().map(|profile| {
        let collapsed = match previous.remove(&profile.id) {
            Some(mut machine)
                if machine.profile.transport == profile.transport
                    && machine.profile.expected_host_id == profile.expected_host_id =>
            {
                if machine.profile.label != profile.label {
                    machine.profile.label = profile.label;
                }
                return machine;
            }
            old => old.is_some_and(|machine| machine.collapsed),
        };
        if profile.id == *active_host_id {
            *active_room_id = None;
        }
        changes.reconnect.push(profile.clone());
        let mut machine = MachineView::new(profile);
        machine.collapsed = collapsed;
        machine
    }));
    changes.removed = previous.into_keys().collect();
    *default_host_id = cfg.default_host_id;
    if let Some(id) = fallback {
        *active_room_id = machines
            .iter()
            .find(|m| m.profile.id == id)
            .and_then(|m| m.rooms.first())
            .map(|room| room.id.clone());
        *active_host_id = id;
        changes.active_host_changed = true;
    }
    Some(changes)
}

fn reset_backup_label(notice: &ResetNotice) -> String {
    let basename = notice
        .backup_file
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    sanitize_display_text(basename, 128)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RoomKey {
    host_id: String,
    room_id: RoomId,
}

struct PaneDrag {
    host_id: String,
    room_id: RoomId,
    group_idx: usize,
    path: SplitPath,
    axis: SplitAxis,
    parent_origin: f32,
    parent_extent: f32,
    canvas_origin: f32,
    grab_offset: f32,
    dirty: bool,
}

impl PaneDrag {
    fn ratio_for_position(&self, position: Point<Pixels>) -> u16 {
        let cursor = f32::from(match self.axis {
            SplitAxis::Horizontal => position.x,
            SplitAxis::Vertical => position.y,
        }) - self.canvas_origin
            - self.grab_offset;
        ratio_bp_from_cursor(cursor, self.parent_origin, self.parent_extent)
    }

    fn preserve_preview(&self, previous: &RoomLayout, next: &mut RoomLayout) -> bool {
        let Some((old, new)) = previous
            .groups
            .get(self.group_idx)
            .zip(next.groups.get_mut(self.group_idx))
        else {
            return false;
        };
        let Some(ratio) = ratio_at(old, &self.path) else {
            return false;
        };
        let mut preview = new.clone();
        // Preserve the local ratio only while the dragged divider still represents the same split.
        if !set_ratio_at(&mut preview, &self.path, ratio) || preview != *old {
            return false;
        }
        if self.dirty {
            *new = preview;
        }
        true
    }
}

type TabSelection<'a> = (&'a str, &'a str, u64);

struct ExternalInputIndicator {
    host_id: String,
    tab_id: TabId,
    run_generation: u64,
    label: String,
    expires_at: Instant,
}

impl ExternalInputIndicator {
    const DURATION: Duration = Duration::from_secs(3);

    fn new(
        now: Instant,
        active: Option<TabSelection<'_>>,
        target: TabSelection<'_>,
        from_host_id: &str,
        is_remote: bool,
        machines: &[MachineView],
    ) -> Option<Self> {
        if active != Some(target) {
            return None;
        }
        let (host_id, tab_id, run_generation) = target;
        let label = if is_remote {
            let source = machines
                .iter()
                .find(|m| m.profile.expected_host_id.as_deref() == Some(from_host_id))
                .or_else(|| {
                    machines
                        .iter()
                        .find(|m| m.connected_host_id.as_deref() == Some(from_host_id))
                });
            let label = source
                .map(|m| sanitize_display_text(&m.profile.label, DISPLAY_TEXT_LIMIT))
                .filter(|label| !label.trim().is_empty() && parse_session_id(label).is_err());
            format!("원격 입력 · {}", label.as_deref().unwrap_or("원격"))
        } else {
            "외부 입력".into()
        };
        Some(Self {
            host_id: host_id.to_owned(),
            tab_id: tab_id.to_owned(),
            run_generation,
            label,
            expires_at: now + Self::DURATION,
        })
    }

    fn label_at(&self, now: Instant, active: Option<TabSelection<'_>>) -> Option<&str> {
        (now < self.expires_at
            && active == Some((&self.host_id, &self.tab_id, self.run_generation)))
        .then_some(self.label.as_str())
    }
}

enum ActiveOverlay {
    MachineModal(Box<MachineModal>),
    RenameModal(RoomKey, String),
    AgentLauncher(AgentLauncher),
    AgentSettingsModal(Box<AgentSettingsModal>),
}

#[derive(Default)]
enum KeyboardFocusZone {
    #[default]
    Terminal,
    AgentList,
}

struct AgentTooltip(String);
impl Render for AgentTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_sm()
            .max_w(px(360.))
            .bg(rgb(0x313740))
            .text_color(rgb(0xf2f2f2))
            .text_xs()
            .child(self.0.clone())
    }
}

struct PeekState {
    host: String,
    room: RoomId,
    tab: TabView,
    claim: Option<Arc<host_bridge::PeekClaim>>,
}

fn peek_close_key(key: &str, primary: bool, mode: ControlMode) -> bool {
    (primary && key == "w") || (key == "escape" && mode == ControlMode::Readonly)
}

fn peek_release_allowed(normal_tab: bool, running: bool) -> bool {
    !normal_tab && running
}

fn spinner_due(active: bool, visible_work: bool, last: &mut Option<Instant>, now: Instant) -> bool {
    if !active || !visible_work {
        *last = None;
        return false;
    }
    let elapsed = last.is_some_and(|tick| now.duration_since(tick) >= Duration::from_millis(125));
    if last.is_none() || elapsed {
        *last = Some(now);
    }
    elapsed
}

fn settings_key(key: &str, m: Modifiers, mac: bool) -> bool {
    !m.alt
        && if mac {
            m.platform && !m.control && !m.shift && key == ","
        } else {
            m.control && !m.platform && m.shift && matches!(key, "," | "<")
        }
}

struct Desktop {
    ui_scale: UiScale,
    pending_ui_scale: Option<UiScale>,
    ui_prefs: UiPrefs,
    assets: AppAssets,
    agents: AgentsConfig,
    agent_error: Option<String>,
    programs: ProgramsConfig,
    programs_warned: bool,
    peek: Option<PeekState>,
    window_active: bool,
    focus_zone: KeyboardFocusZone,
    agent_cursor: usize,
    hovered_agent: Option<String>,
    keyboard_group: Option<String>,
    spinner_frame: usize,
    spinner_tick: Option<Instant>,
    ui_prefs_path: Option<PathBuf>,
    zoom_indicator: Option<(u16, Instant)>,
    font_grid: (&'static str, f32, f32),
    icon: Arc<Image>,
    default_host_id: String,
    machines: Vec<MachineView>,
    active_host_id: String,
    active_room_id: Option<RoomId>,
    pending_select: Option<(RoomKey, TabId)>,
    active_overlay: Option<ActiveOverlay>,
    update: UpdateController,
    update_modal: Option<UpdateModal>,
    last_local_change: Option<Change>,
    hovered_room: Option<RoomKey>,
    hovered_machine: Option<String>,
    notice: Option<String>,
    external_input_indicator: Option<ExternalInputIndicator>,
    bridge: HostBridge,
    focus: FocusHandle,
    sidebar_base: f32,
    sidebar_drag: Option<f32>,
    pane_drag: Option<PaneDrag>,
    canvas: PaneRect,
    viewport_grid: (u16, u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZoomAction {
    In,
    Out,
    Reset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyRoute {
    Zoom(ZoomAction),
    UpdateCancel,
    UpdateClose,
    UpdateTrap,
    Machine,
    Rename,
    ResetNotice,
    Terminal,
}

fn zoom_action(key: &str, m: Modifiers, mac: bool) -> Option<ZoomAction> {
    let primary = m.platform == mac && m.control != mac;
    if !primary || m.alt || m.function {
        return None;
    }
    match (key, m.shift) {
        ("+" | "=", _) => Some(ZoomAction::In),
        ("-" | "_", _) => Some(ZoomAction::Out),
        ("0", false) => Some(ZoomAction::Reset),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabAction {
    Add,
    NewRoom,
    Close,
    SplitHorizontal,
    SplitVertical,
    FocusDir(NavDir),
    ToggleZoom,
    Unsplit,
    Previous,
    Next,
    Number(usize),
}

impl TabAction {
    fn target(self, selected: usize, count: usize) -> Option<usize> {
        if count == 0 {
            return None;
        }
        match self {
            Self::Previous => Some((selected + count - 1) % count),
            Self::Next => Some((selected + 1) % count),
            Self::Number(n) if (1..=count).contains(&n) => Some(n - 1),
            _ => None,
        }
    }
}

fn tab_action(key: &str, m: Modifiers, mac: bool) -> Option<TabAction> {
    use TabAction::*;
    let primary = m.platform == mac && m.control != mac;
    if m.alt || m.function {
        if m.alt && !m.function && !m.shift && primary {
            return Some(FocusDir(match key {
                "left" => NavDir::Left,
                "right" => NavDir::Right,
                "up" => NavDir::Up,
                "down" => NavDir::Down,
                _ => return None,
            }));
        }
        return None;
    }
    if m.control && !m.platform && key == "tab" {
        return Some(if m.shift { Previous } else { Next });
    }
    if !primary {
        return None;
    }
    match (key, m.shift) {
        ("n", false) => Some(NewRoom),
        ("t", false) => Some(Add),
        ("w", false) => Some(Close),
        ("d", shift) if shift != mac => Some(SplitHorizontal),
        ("d", true) if mac => Some(SplitVertical),
        ("e", true) if !mac => Some(SplitVertical),
        ("enter", true) => Some(ToggleZoom),
        ("u", true) => Some(Unsplit),
        // GPUI folds Shift into the punctuation on macOS.
        ("[" | "{", true) | ("{", false) => Some(Previous),
        ("]" | "}", true) | ("}", false) => Some(Next),
        (_, false) => match key.as_bytes() {
            [digit @ b'1'..=b'9'] => Some(Number(usize::from(*digit - b'0'))),
            _ => None,
        },
        _ => None,
    }
}

/// Formats Finder/OS dropped file paths for pasting into a shell or CLI prompt,
/// backslash-escaping shell metacharacters (matching macOS Terminal/Ghostty).
fn format_dropped_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| escape_shell_path(&p.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn escape_shell_path(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if matches!(
            ch,
            ' ' | '\t'
                | '\''
                | '"'
                | '\\'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '&'
                | ';'
                | '|'
                | '<'
                | '>'
                | '$'
                | '`'
                | '*'
                | '?'
                | '!'
                | '#'
                | '~'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn stable_elem_id(kind: &'static str, parts: &[&str]) -> ElementId {
    let mut hash = 0xcbf29ce484222325u64;
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            hash = (hash ^ 0xff).wrapping_mul(0x100000001b3);
        }
        for byte in part.bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
    ElementId::NamedInteger(kind.into(), hash)
}

fn running_background_tabs(catalog: &Catalog) -> impl Iterator<Item = (&RoomId, &TabRecord)> {
    catalog
        .rooms
        .iter()
        .filter(|room| room.id.starts_with("rm_bg_") && room.visibility == RoomVisibility::Hidden)
        .flat_map(|room| room.tabs.iter().map(move |tab| (&room.id, tab)))
        .filter(|(_, tab)| matches!(tab.runtime, TabRuntimeState::Running { .. }))
}

fn sync_machine_rooms(
    machine: &mut MachineView,
    catalog: &Catalog,
    active_host_id: &mut String,
    active_room_id: &mut Option<RoomId>,
    default_size: (u16, u16),
    pane_drag: &mut Option<PaneDrag>,
) -> Vec<u32> {
    let was_empty = machine.rooms.is_empty();
    let mut prev_rooms = HashMap::new();
    let mut prev_tabs = HashMap::new();
    let mut stale = Vec::new();
    for mut room in machine.rooms.drain(..) {
        prev_tabs.extend(room.tabs.drain(..).map(|tab| (tab.id.clone(), tab)));
        prev_rooms.insert(room.id.clone(), room);
    }

    let mut has_visible_standard = false;
    machine.rooms = catalog
        .rooms
        .iter()
        .filter(|room| room.visibility != RoomVisibility::Hidden)
        .map(|room| {
            has_visible_standard |= room.kind == RoomKind::Standard;
            let tabs: Vec<_> = room
                .tabs
                .iter()
                .map(|rec| {
                    let Some(mut tab) = prev_tabs.remove(&rec.id) else {
                        return TabView::from_record(rec, default_size);
                    };
                    if rec.run_generation > tab.run_generation {
                        stale.extend(tab.channel.take());
                        let mut fresh = TabView::from_record(rec, default_size);
                        // An in-flight attach may already be for the new run on the server.
                        fresh.attaching = tab.attaching;
                        fresh.attach_request = tab.attach_request;
                        fresh.early_frames = tab.early_frames;
                        fresh.early_bytes = tab.early_bytes;
                        tab = fresh;
                    } else if rec.run_generation == tab.run_generation {
                        tab.title.clone_from(&rec.title);
                        tab.runtime.clone_from(&rec.runtime);
                        if !tab.is_running() {
                            stale.extend(tab.channel.take());
                            tab.status = TabView::runtime_status(&rec.runtime);
                        }
                    }
                    tab
                })
                .collect();
            let mut current = RoomView::from_record(room, tabs);
            if let Some(previous) = prev_rooms.get(&room.id) {
                if pane_drag.as_ref().is_some_and(|drag| {
                    drag.host_id == machine.profile.id
                        && drag.room_id == room.id
                        && !drag.preserve_preview(&previous.layout, &mut current.layout)
                }) {
                    *pane_drag = None;
                }
                current.preserve_selection(previous);
            }
            current
        })
        .collect();
    stale.extend(prev_tabs.into_values().filter_map(|tab| tab.channel));
    if pane_drag.as_ref().is_some_and(|drag| {
        drag.host_id == machine.profile.id
            && !machine.rooms.iter().any(|room| room.id == drag.room_id)
    }) {
        *pane_drag = None;
    }

    if has_visible_standard && let Some(notice) = machine.reset_notice.take() {
        machine.dismissed_notice_ids.insert(notice.notice_id);
    }
    if was_empty && !machine.rooms.is_empty() {
        machine.collapsed = false;
    }
    let host_id = &machine.profile.id;
    if active_host_id == host_id {
        if !active_room_id
            .as_ref()
            .is_some_and(|id| machine.rooms.iter().any(|room| room.id == *id))
        {
            *active_room_id = machine.rooms.first().map(|room| room.id.clone());
        }
    } else if was_empty
        && active_room_id.is_none()
        && let Some(first) = machine.rooms.first()
    {
        active_host_id.clone_from(host_id);
        *active_room_id = Some(first.id.clone());
    }
    stale
}

const ICON_HOVER_GROUP: &str = "icon-control";
const MACHINE_HEADER_GROUP: &str = "machine-header";

fn icon_box(size: Pixels) -> Div {
    div()
        .size(size)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
}

fn hover_icon(kind: Icon, size_px: f32, color: u32) -> Svg {
    icon(kind, size_px, rgb(color)).group_hover(ICON_HOVER_GROUP, |s| s.text_color(rgb(0xffffff)))
}

impl Desktop {
    fn new(
        hosts_path: Option<PathBuf>,
        hosts: HostsConfig,
        assets: AppAssets,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ui_prefs_path = UiPrefs::default_path();
        let prefs = ui_prefs_path
            .as_deref()
            .map(UiPrefs::load_or_default)
            .unwrap_or_default();
        let bridge = HostBridge::new(&hosts.hosts);
        bridge.start_config_watcher(
            hosts_path,
            hosts.clone(),
            ui_prefs_path.clone(),
            prefs.clone(),
        );
        let (agents, agent_error) = match AgentsConfig::default_path()
            .ok()
            .map(|path| config_watch::load_agents_snapshot(&path))
        {
            Some(Ok(cfg)) => (cfg, None),
            Some(Err(err)) => (AgentsConfig::default(), Some(err)),
            None => (AgentsConfig::default(), None),
        };
        Self::from_state(
            hosts,
            assets,
            (ui_prefs_path, prefs),
            (agents, agent_error),
            bridge,
            window,
            cx,
        )
    }

    fn from_state(
        hosts: HostsConfig,
        assets: AppAssets,
        (ui_prefs_path, prefs): (Option<PathBuf>, UiPrefs),
        (agents, agent_error): (AgentsConfig, Option<String>),
        bridge: HostBridge,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let active_host_id = hosts.default_profile().id;
        let ui_scale = UiScale::from_percent(prefs.ui_scale_pct);
        window.set_rem_size(px(ui_scale.rem_px()));
        cx.observe_window_activation(window, |this, window, cx| {
            this.window_active = window.is_window_active();
            this.mark_active_seen();
            cx.notify();
        })
        .detach();
        let this = Self {
            assets,
            agents,
            agent_error,
            programs: BUILTIN_PROGRAMS.clone(),
            programs_warned: false,
            peek: None,
            window_active: window.is_window_active(),
            focus_zone: KeyboardFocusZone::Terminal,
            agent_cursor: 0,
            hovered_agent: None,
            keyboard_group: None,
            spinner_frame: 0,
            spinner_tick: None,
            ui_scale,
            pending_ui_scale: None,
            ui_prefs: prefs,
            ui_prefs_path,
            zoom_indicator: None,
            icon: Arc::new(Image::from_bytes(
                ImageFormat::Png,
                include_bytes!("../../assets/ddg-icon.png").to_vec(),
            )),
            font_grid: font_grid(window, ui_scale),
            default_host_id: hosts.default_host_id,
            machines: hosts.hosts.into_iter().map(MachineView::new).collect(),
            active_host_id,
            active_room_id: None,
            pending_select: None,
            active_overlay: None,
            update: UpdateController::default(),
            update_modal: None,
            last_local_change: None,
            hovered_room: None,
            hovered_machine: None,
            notice: None,
            external_input_indicator: None,
            bridge,
            focus: cx.focus_handle(),
            sidebar_base: 224.,
            sidebar_drag: None,
            pane_drag: None,
            canvas: PaneRect {
                x: 0.,
                y: 0.,
                w: 1.,
                h: 1.,
            },
            viewport_grid: (80, 24),
        };
        this.focus.focus(window);
        this
    }

    fn machine_modal(&self) -> Option<&MachineModal> {
        match &self.active_overlay {
            Some(ActiveOverlay::MachineModal(m)) => Some(m),
            _ => None,
        }
    }

    fn machine_modal_mut(&mut self) -> Option<&mut MachineModal> {
        match &mut self.active_overlay {
            Some(ActiveOverlay::MachineModal(m)) => Some(m),
            _ => None,
        }
    }

    fn apply_zoom_action(
        &mut self,
        action: ZoomAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut scale = self.ui_scale;
        let changed = match action {
            ZoomAction::In => scale.zoom_in(),
            ZoomAction::Out => scale.zoom_out(),
            ZoomAction::Reset => scale.reset(),
        };
        self.apply_ui_scale(scale, window);
        if changed {
            self.pending_ui_scale = None;
            self.ui_prefs.ui_scale_pct = scale.percent();
            if let Some(path) = &self.ui_prefs_path
                && let Err(err) = self.ui_prefs.save(path)
            {
                eprintln!("could not save UI zoom to {}: {err}", path.display());
            }
        }
        cx.notify();
    }

    fn apply_ui_scale(&mut self, scale: UiScale, window: &mut Window) {
        self.zoom_indicator = Some((scale.percent(), Instant::now() + ZOOM_INDICATOR_DURATION));
        if scale != self.ui_scale {
            // The dragged dividers' original positions no longer match the layout after zoom.
            self.ui_scale = scale;
            self.sidebar_drag = None;
            self.finish_pane_drag();
            window.set_rem_size(px(scale.rem_px()));
            self.font_grid = font_grid(window, scale);
        }
    }

    fn apply_hosts_config(&mut self, cfg: HostsConfig) -> bool {
        let Some(changes) = reconcile_hosts_config(
            &mut self.machines,
            &mut self.default_host_id,
            &mut self.active_host_id,
            &mut self.active_room_id,
            cfg,
        ) else {
            return false;
        };
        changes.discard_stale(&mut self.hovered_machine, String::as_str);
        changes.discard_stale(&mut self.hovered_room, |key| &key.host_id);
        if matches!(&self.active_overlay, Some(ActiveOverlay::RenameModal(key, _))
            if changes.removed.contains(&key.host_id) || changes.reconnect.iter().any(|p| p.id == key.host_id))
        {
            self.active_overlay = None;
        }
        changes.discard_stale(&mut self.pane_drag, |drag| &drag.host_id);
        changes.discard_stale(&mut self.external_input_indicator, |badge| &badge.host_id);
        changes.discard_stale(&mut self.pending_select, |(key, _)| &key.host_id);
        if self.peek.as_ref().is_some_and(|p| {
            changes.removed.contains(&p.host)
                || changes.reconnect.iter().any(|profile| profile.id == p.host)
        }) {
            self.close_peek(false);
        }
        for id in changes.removed {
            self.bridge.remove_machine(&id);
        }
        for profile in changes.reconnect {
            self.bridge.add_or_update_machine(profile);
        }
        if changes.active_host_changed {
            self.ensure_selected_attached();
        }
        true
    }

    fn show_machine_sections(&self) -> bool {
        self.machines.len() >= 2
    }

    fn machine(&self, host_id: &str) -> Option<&MachineView> {
        self.machines.iter().find(|m| m.profile.id == host_id)
    }

    fn machine_mut(&mut self, host_id: &str) -> Option<&mut MachineView> {
        self.machines.iter_mut().find(|m| m.profile.id == host_id)
    }

    fn active_machine(&self) -> Option<&MachineView> {
        self.machine(&self.active_host_id)
            .or_else(|| self.machines.first())
    }

    fn room(&self) -> Option<&RoomView> {
        let rid = self.active_room_id.as_ref()?;
        self.active_machine()?.rooms.iter().find(|r| r.id == *rid)
    }

    fn room_mut(&mut self) -> Option<&mut RoomView> {
        let rid = self.active_room_id.as_ref()?;
        self.machines
            .iter_mut()
            .find(|m| m.profile.id == self.active_host_id)?
            .rooms
            .iter_mut()
            .find(|r| r.id == *rid)
    }

    fn active_tab_key(&self) -> Option<TabSelection<'_>> {
        let tab = self.room()?.focused_tab()?;
        (self.is_active_connected() && tab.is_running()).then_some((
            &self.active_host_id,
            &tab.id,
            tab.run_generation,
        ))
    }

    fn find_tab_mut(&mut self, host_id: &str, tab_id: &str) -> Option<&mut TabView> {
        if let Some(peek) = self
            .peek
            .as_mut()
            .filter(|p| p.host == host_id && p.tab.id == tab_id)
        {
            return Some(&mut peek.tab);
        }
        self.machines
            .iter_mut()
            .find(|m| m.profile.id == host_id)?
            .rooms
            .iter_mut()
            .flat_map(|r| &mut r.tabs)
            .find(|t| t.id == tab_id)
    }

    fn active_terminal_target(&self) -> Option<(&str, &TabView)> {
        if let Some(peek) = &self.peek {
            return Some((&peek.host, &peek.tab));
        }
        Some((&self.active_host_id, self.room()?.focused_tab()?))
    }

    fn mark_active_seen(&mut self) {
        if !self.window_active || !matches!(self.focus_zone, KeyboardFocusZone::Terminal) {
            return;
        }
        let Some((host, tab)) = self.active_terminal_target() else {
            return;
        };
        let Some(seq) = tab.unseen_sequence() else {
            return;
        };
        if !self.machine(host).is_some_and(MachineView::is_connected) {
            return;
        }
        let (host, id, run) = (host.to_owned(), tab.id.clone(), tab.run_generation);
        if let Some(tab) = self.find_tab_mut(&host, &id) {
            tab.pending_seen_seq = seq;
        }
        self.bridge.mark_seen(&host, id, run, seq);
    }

    fn apply_activity(&mut self, host: &str, activity: TabActivity) {
        if let Some(tab) = self
            .find_tab_mut(host, &activity.tab_id)
            .filter(|tab| tab.run_generation == activity.run_generation)
        {
            tab.activity = Some(activity.clone());
        }
        if let Some(tab) = self
            .machine_mut(host)
            .and_then(|machine| {
                machine
                    .background
                    .iter_mut()
                    .map(|(_, tab)| tab)
                    .find(|tab| tab.id == activity.tab_id)
            })
            .filter(|tab| tab.run_generation == activity.run_generation)
        {
            tab.activity = Some(activity);
        }
        self.mark_active_seen();
    }

    fn close_peek(&mut self, promoted: bool) {
        let Some(peek) = self.peek.take() else {
            return;
        };
        let release = peek_release_allowed(promoted, peek.tab.is_running());
        let channel = peek.tab.channel;
        let release = match peek.claim {
            None => false,
            Some(claim) => match claim.close(channel, release) {
                Some(release) => release,
                None => return,
            },
        };
        self.bridge
            .close_peek(&peek.host, peek.tab.id, channel, release);
    }

    fn select_agent(&mut self, entry: &AgentEntry, window: &mut Window) {
        self.focus_zone = KeyboardFocusZone::Terminal;
        self.focus.focus(window);
        if self
            .peek
            .as_ref()
            .is_some_and(|peek| peek.host == entry.host && peek.tab.id == entry.tab)
        {
            return;
        }
        self.close_peek(false);
        if entry.background {
            let Some(mut tab) = self
                .machine(&entry.host)
                .and_then(|m| m.background.iter().find(|(_, tab)| tab.id == entry.tab))
                .map(|(_, tab)| tab.metadata_copy())
            else {
                return;
            };
            if !tab.begin_attach() {
                return;
            }
            tab.attach_request = Some(self.bridge.attach_tab(
                &entry.host,
                entry.room.clone(),
                tab.id.clone(),
                tab.run_generation,
                self.viewport_grid,
                false,
            ));
            self.peek = Some(PeekState {
                host: entry.host.clone(),
                room: entry.room.clone(),
                tab,
                claim: None,
            });
        } else {
            self.select_room_and_tab(
                &RoomKey {
                    host_id: entry.host.clone(),
                    room_id: entry.room.clone(),
                },
                &entry.tab,
            );
            self.ensure_selected_attached();
        }
        self.mark_active_seen();
    }

    fn is_active_connected(&self) -> bool {
        self.active_machine().is_some_and(MachineView::is_connected)
    }

    fn new_room_on(&mut self, host_id: &str, window: &mut Window) {
        let Some(m) = self.machine_mut(host_id) else {
            return;
        };
        if !m.is_connected() {
            return;
        }
        m.collapsed = false;
        let (cols, rows) = self.viewport_grid;
        self.close_peek(false);
        self.active_overlay = None;
        self.bridge.create_room(host_id, cols, rows);
        self.focus.focus(window);
    }

    fn new_room(&mut self, window: &mut Window) {
        let hid = self.active_host_id.clone();
        self.new_room_on(&hid, window);
    }

    fn add_tab(&mut self) {
        if !self.is_active_connected() {
            return;
        }
        let hid = self.active_host_id.clone();
        let (cols, rows) = self.viewport_grid;
        if let Some(room) = self.room() {
            if room.tabs.len() >= MAX_TABS_PER_ROOM {
                return;
            }
            self.bridge.create_tab(&hid, room.id.clone(), cols, rows);
        } else {
            self.bridge.create_room(&hid, cols, rows);
        }
    }

    fn close_focused_tab(&mut self) {
        if !self.is_active_connected() {
            return;
        }
        let hid = self.active_host_id.clone();
        let Some(tab) = self.room_mut().and_then(RoomView::focused_tab_mut) else {
            return;
        };
        if tab.closing {
            return;
        }
        tab.closing = true;
        tab.status = "종료 중".into();
        let tab_id = tab.id.clone();
        self.bridge.close_tab(&hid, tab_id);
    }

    fn canvas_bounds(&self) -> PaneRect {
        PaneRect {
            x: 0.,
            y: 0.,
            ..self.canvas
        }
    }

    fn pane_geometry(&self) -> (Vec<(TabId, PaneRect)>, Vec<DividerHit>) {
        if let Some(peek) = &self.peek {
            return (
                vec![(peek.tab.id.clone(), self.canvas_bounds())],
                Vec::new(),
            );
        }
        let Some(room) = self.room() else {
            return Default::default();
        };
        let bounds = self.canvas_bounds();
        if let Some(id) = room
            .zoomed_tab_id
            .as_ref()
            .filter(|id| room.active_group().is_some_and(|group| group.contains(id)))
        {
            return (vec![(id.clone(), bounds)], Vec::new());
        }
        room.active_group().map_or_else(Default::default, |group| {
            compute_layout(group, bounds, self.ui_scale.px(PANE_DIVIDER))
        })
    }

    fn can_split(&self) -> bool {
        self.peek.is_none()
            && self.is_active_connected()
            && self.room().is_some_and(|room| {
                room.tabs.len() < MAX_TABS_PER_ROOM
                    && room.focused_tab().is_some_and(|tab| !tab.closing)
            })
    }

    fn split_focused(&self, axis: SplitAxis) {
        if !self.can_split() {
            return;
        }
        let Some(room) = self.room() else {
            return;
        };
        let (Some(group), Some(anchor)) = (room.active_group(), room.focused_tab_id()) else {
            return;
        };
        let divider = self.ui_scale.px(PANE_DIVIDER);
        let (panes, _) = compute_layout(group, self.canvas_bounds(), divider);
        let Some((_, mut rect)) = panes.into_iter().find(|(id, _)| id == anchor) else {
            return;
        };
        let extent = match axis {
            SplitAxis::Horizontal => &mut rect.w,
            SplitAxis::Vertical => &mut rect.h,
        };
        let avail = (*extent - divider).max(0.);
        *extent = (avail - (avail / 2.).round()).max(0.);
        let (_, font_w, font_h) = self.font_grid;
        let (cols, rows) = scaled_pane_grid(
            pane_body_rect(rect, pane_header_height(self.ui_scale, true)),
            font_w,
            font_h,
            self.ui_scale,
        );
        self.bridge
            .split_tab(&self.active_host_id, anchor.to_owned(), axis, cols, rows);
    }

    fn unsplit_focused(&mut self) {
        if !self.is_active_connected() {
            return;
        }
        let Some(room) = self.room_mut() else {
            return;
        };
        let Some(id) = room.focused_tab_id().map(str::to_owned) else {
            return;
        };
        if room.layout.unsplit_tab(&id) {
            room.select_tab_by_id(&id);
            let (room_id, layout) = (room.id.clone(), room.layout.clone());
            self.bridge
                .set_room_layout(&self.active_host_id, room_id, Some(layout));
        }
    }

    fn toggle_zoom(&mut self) {
        if let Some(room) = self.room_mut().filter(|room| room.is_active_group_split()) {
            room.zoomed_tab_id = if room.zoomed_tab_id.is_some() {
                None
            } else {
                room.focused_tab_id().map(str::to_owned)
            };
        }
    }

    fn focus_direction(&mut self, dir: NavDir) {
        let bounds = self.canvas_bounds();
        let divider = self.ui_scale.px(PANE_DIVIDER);
        if let Some(room) = self.room_mut()
            && let Some(group) = room.active_group()
            && let Some(id) = room.focused_tab_id()
            && let Some(target) =
                navigate_direction(&compute_layout(group, bounds, divider).0, id, dir)
        {
            room.select_tab_by_id(&target);
        }
    }

    fn ensure_selected_attached(&mut self) {
        if let Some(peek) = &mut self.peek {
            if self
                .machines
                .iter()
                .any(|m| m.profile.id == peek.host && m.is_connected() && m.catalog_seq.is_some())
                && peek.tab.begin_attach()
            {
                peek.tab.attach_request = Some(self.bridge.attach_tab(
                    &peek.host,
                    peek.room.clone(),
                    peek.tab.id.clone(),
                    peek.tab.run_generation,
                    self.viewport_grid,
                    false,
                ));
            }
            self.mark_active_seen();
            return;
        }
        self.mark_active_seen();
        if !self.is_active_connected() {
            return;
        }
        let hid = self.active_host_id.clone();
        let (_, font_w, font_h) = self.font_grid;
        let (panes, _) = self.pane_geometry();
        let scale = self.ui_scale;
        let header_h = pane_header_height(scale, panes.len() > 1);
        let Some(room) = self.room_mut() else {
            return;
        };
        let room_id = room.id.clone();
        let mut attach = Vec::new();
        for (id, rect) in panes {
            if let Some(tab) = room.tabs.iter_mut().find(|tab| tab.id == id)
                && tab.begin_attach()
            {
                attach.push((
                    id,
                    tab.run_generation,
                    scaled_pane_grid(pane_body_rect(rect, header_h), font_w, font_h, scale),
                ));
            }
        }
        for (tab_id, generation, size) in attach {
            let request = self.bridge.attach_tab(
                &hid,
                room_id.clone(),
                tab_id.clone(),
                generation,
                size,
                true,
            );
            if let Some(tab) = self.find_tab_mut(&hid, &tab_id) {
                tab.attach_request = Some(request);
            }
        }
    }

    fn tab_is_visible(&self, host_id: &str, tab_id: &str) -> bool {
        if let Some(peek) = &self.peek {
            return peek.host == host_id && peek.tab.id == tab_id;
        }
        host_id == self.active_host_id
            && self
                .room()
                .is_some_and(|room| room.visible_tab_ids().iter().any(|id| id == tab_id))
    }

    fn focus_pane(&mut self, tab_id: &str, window: &mut Window) -> bool {
        self.focus_zone = KeyboardFocusZone::Terminal;
        if self.peek.as_ref().is_some_and(|peek| peek.tab.id == tab_id) {
            self.focus.focus(window);
            self.mark_active_seen();
            return true;
        }
        if !self
            .room_mut()
            .is_some_and(|room| room.select_tab_by_id(tab_id))
        {
            return false;
        }
        self.ensure_selected_attached();
        self.focus.focus(window);
        true
    }

    fn handle_file_drop(&mut self, tab_id: &str, paths: &ExternalPaths, window: &mut Window) {
        let formatted = format_dropped_paths(paths.paths());
        if self.peek.is_some()
            || formatted.is_empty()
            || !self.focus_pane(tab_id, window)
            || !self.is_active_connected()
        {
            return;
        }
        if let Some(tab) = self.room().and_then(RoomView::focused_tab)
            && !tab.closing
            && tab.mode == ControlMode::Control
            && let Some(channel) = tab.channel
        {
            self.bridge
                .send_input(&self.active_host_id, channel, formatted.as_bytes());
        }
    }

    fn begin_pane_drag(&mut self, hit: &DividerHit, position: Point<Pixels>) {
        if !self.is_active_connected() || hit.parent_extent <= 0. {
            return;
        }
        let Some(room) = self.room() else {
            return;
        };
        let (canvas_origin, grab_offset) = match hit.axis {
            SplitAxis::Horizontal => (
                self.canvas.x,
                f32::from(position.x) - self.canvas.x - hit.rect.x,
            ),
            SplitAxis::Vertical => (
                self.canvas.y,
                f32::from(position.y) - self.canvas.y - hit.rect.y,
            ),
        };
        self.pane_drag = Some(PaneDrag {
            host_id: self.active_host_id.clone(),
            room_id: room.id.clone(),
            group_idx: room.selected_group,
            path: hit.path.clone(),
            axis: hit.axis,
            parent_origin: hit.parent_origin,
            parent_extent: hit.parent_extent,
            canvas_origin,
            grab_offset,
            dirty: false,
        });
    }

    fn move_pane_divider(&mut self, position: Point<Pixels>) -> bool {
        let Some(mut drag) = self.pane_drag.take() else {
            return false;
        };
        let ratio = drag.ratio_for_position(position);
        let Some(group) = self
            .machine_mut(&drag.host_id)
            .and_then(|machine| {
                machine
                    .rooms
                    .iter_mut()
                    .find(|room| room.id == drag.room_id)
            })
            .and_then(|room| room.layout.groups.get_mut(drag.group_idx))
        else {
            return false;
        };
        let changed =
            ratio_at(group, &drag.path) != Some(ratio) && set_ratio_at(group, &drag.path, ratio);
        drag.dirty |= changed;
        self.pane_drag = Some(drag);
        changed
    }

    fn finish_pane_drag(&mut self) -> bool {
        let Some(drag) = self.pane_drag.take() else {
            return false;
        };
        if drag.dirty
            && let Some(room) = self
                .machine(&drag.host_id)
                .and_then(|machine| machine.rooms.iter().find(|room| room.id == drag.room_id))
        {
            self.bridge
                .set_room_layout(&drag.host_id, drag.room_id, Some(room.layout.clone()));
        }
        true
    }

    fn end_resize_drag(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.button == MouseButton::Left {
            self.sidebar_drag = None;
            if self.finish_pane_drag() {
                cx.notify();
            }
        }
    }

    fn select_room_and_tab(&mut self, key: &RoomKey, tab_id: &str) -> bool {
        let Some(machine) = self.machine_mut(&key.host_id) else {
            return false;
        };
        let Some(room) = machine.rooms.iter_mut().find(|r| r.id == key.room_id) else {
            return false;
        };
        if !room.select_tab_by_id(tab_id) {
            return false;
        }
        machine.collapsed = false;
        self.active_host_id.clone_from(&key.host_id);
        self.active_room_id = Some(key.room_id.clone());
        true
    }

    fn apply_catalog(&mut self, host_id: &str, seq: u64, catalog: Catalog) {
        let Some(machine) = self.machines.iter_mut().find(|m| m.profile.id == host_id) else {
            return;
        };
        if machine.catalog_seq.is_some_and(|current| seq <= current) {
            return;
        }
        machine.catalog_seq = Some(seq);
        let mut prev: HashMap<_, _> = std::mem::take(&mut machine.background)
            .into_iter()
            .map(|(_, tab)| (tab.id.clone(), tab))
            .collect();
        machine.background = running_background_tabs(&catalog)
            .map(|(room, rec)| {
                let mut tab = TabView::from_record(rec, self.viewport_grid);
                if let Some(old) = prev
                    .remove(&rec.id)
                    .filter(|old| old.run_generation == rec.run_generation)
                {
                    tab.activity = old.activity;
                }
                (room.clone(), tab)
            })
            .collect();
        let promoted = self
            .peek
            .as_ref()
            .filter(|peek| peek.host == host_id)
            .is_some_and(|peek| {
                catalog
                    .rooms
                    .iter()
                    .filter(|room| room.visibility != RoomVisibility::Hidden)
                    .any(|room| room.tabs.iter().any(|tab| tab.id == peek.tab.id))
            });
        if !promoted
            && let Some(peek) = self
                .peek
                .as_mut()
                .filter(|peek| peek.host == host_id && peek.tab.is_running())
            && !machine.background.iter().any(|(_, tab)| {
                tab.id == peek.tab.id && tab.run_generation == peek.tab.run_generation
            })
        {
            peek.tab.runtime = TabRuntimeState::Exited {
                status: "종료됨".into(),
            };
            peek.tab.status = "백그라운드 세션이 종료되었습니다".into();
            peek.tab.mode = ControlMode::Readonly;
        }
        for ch in sync_machine_rooms(
            machine,
            &catalog,
            &mut self.active_host_id,
            &mut self.active_room_id,
            self.viewport_grid,
            &mut self.pane_drag,
        ) {
            self.bridge.detach_channel(host_id, ch);
        }
        for tab in machine.rooms.iter_mut().flat_map(|room| &mut room.tabs) {
            if let Some(old) = prev
                .remove(&tab.id)
                .filter(|old| old.run_generation == tab.run_generation)
            {
                tab.activity = old.activity;
            }
        }
        let removed = |key: &RoomKey| {
            key.host_id == host_id && !machine.rooms.iter().any(|r| r.id == key.room_id)
        };
        if self.hovered_room.as_ref().is_some_and(removed) {
            self.hovered_room = None;
        }
        if matches!(&self.active_overlay, Some(ActiveOverlay::RenameModal(key, _)) if removed(key))
        {
            self.active_overlay = None;
        }
        if promoted {
            self.close_peek(true);
        }
        if let Some((key, tab_id)) = self.pending_select.take()
            && (key.host_id != host_id || !self.select_room_and_tab(&key, &tab_id))
        {
            self.pending_select = Some((key, tab_id));
        }
        self.ensure_selected_attached();
    }

    fn handle_host_event(&mut self, host_id: &str, event: BridgeEvent) {
        match event {
            BridgeEvent::VersionInfo(version) => {
                if let Some(machine) = self.machine_mut(host_id) {
                    machine.remote_update.version = version;
                }
            }
            BridgeEvent::StateChanged(state) => {
                let lost_conn = !matches!(state, ConnectionState::Connected { .. });
                if let Some(machine) = self.machine_mut(host_id) {
                    if let ConnectionState::Connected { host_id, .. } = &state {
                        machine.connected_host_id = Some(host_id.clone());
                    } else {
                        machine.remote_update.version = update_controller::VersionInfo::default();
                        if let ConnectionState::ProtocolIncompatible { remote_version, .. } = &state
                        {
                            machine
                                .remote_update
                                .version
                                .app_version
                                .clone_from(remote_version);
                        }
                        machine.catalog_seq = None;
                        machine.background.clear();
                        for tab in machine.rooms.iter_mut().flat_map(|room| &mut room.tabs) {
                            tab.channel = None;
                            tab.activity = None;
                            tab.pending_seen_seq = 0;
                            tab.reset_attach();
                        }
                    }
                    machine.conn_state = state;
                }
                if lost_conn {
                    if let Some(peek) = self.peek.as_mut().filter(|peek| peek.host == host_id) {
                        peek.tab.channel = None;
                        peek.tab.mode = ControlMode::Readonly;
                        peek.tab.activity = None;
                        peek.tab.pending_seen_seq = 0;
                        peek.tab.reset_attach();
                        peek.claim = None;
                    }
                    if self
                        .pane_drag
                        .as_ref()
                        .is_some_and(|drag| drag.host_id == host_id)
                        && let Some(drag) = self.pane_drag.take()
                        && let Some(room) = self.machine_mut(host_id).and_then(|machine| {
                            machine
                                .rooms
                                .iter_mut()
                                .find(|room| room.id == drag.room_id)
                        })
                    {
                        room.restore_server_layout();
                    }
                    if self
                        .external_input_indicator
                        .as_ref()
                        .is_some_and(|b| b.host_id == host_id)
                    {
                        self.external_input_indicator = None;
                    }
                } else if host_id == self.active_host_id {
                    self.notice = None;
                }
            }
            BridgeEvent::CatalogSubscribed {
                seq,
                catalog,
                reset_notice,
                activities,
            } => {
                if let Some(machine) = self.machine_mut(host_id) {
                    machine.catalog_seq = None;
                    machine.update_reset_notice(reset_notice);
                }
                self.apply_catalog(host_id, seq, catalog);
                for activity in activities {
                    self.apply_activity(host_id, activity);
                }
            }
            BridgeEvent::TabActivityChanged(activity) => self.apply_activity(host_id, activity),
            BridgeEvent::TabSeenChanged {
                tab_id,
                run_generation,
                seen_seq,
                completed,
            } => {
                if let Some(tab) = self
                    .find_tab_mut(host_id, &tab_id)
                    .filter(|t| t.run_generation == run_generation)
                {
                    if completed {
                        tab.pending_seen_seq = 0;
                    }
                    if let Some(activity) = &mut tab.activity {
                        activity.seen_seq = activity.seen_seq.max(seen_seq);
                    }
                }
                if let Some(tab) = self.machine_mut(host_id).and_then(|m| {
                    m.background
                        .iter_mut()
                        .map(|(_, t)| t)
                        .find(|t| t.id == tab_id && t.run_generation == run_generation)
                }) && let Some(activity) = &mut tab.activity
                {
                    activity.seen_seq = activity.seen_seq.max(seen_seq);
                }
                if seen_seq != 0 {
                    self.mark_active_seen();
                }
            }
            BridgeEvent::CatalogUpdated { seq, catalog } => {
                self.apply_catalog(host_id, seq, catalog)
            }
            BridgeEvent::SelectRoomAndTab { room_id, tab_id } => {
                let promoted = self
                    .peek
                    .as_ref()
                    .is_some_and(|p| p.host == host_id && p.tab.id == tab_id && p.room != room_id);
                self.close_peek(promoted);
                let key = RoomKey {
                    host_id: host_id.to_owned(),
                    room_id,
                };
                if !self.select_room_and_tab(&key, &tab_id) {
                    self.pending_select = Some((key, tab_id));
                }
                self.ensure_selected_attached();
            }
            BridgeEvent::TabAttached { handle, request } => {
                let tab = self
                    .find_tab_mut(host_id, &handle.tab_id)
                    .filter(|tab| tab.attach_request == Some(request));
                let retry = tab
                    .as_ref()
                    .is_some_and(|tab| handle.run_generation < tab.run_generation);
                let mut early = Vec::new();
                if let Some(tab) = tab {
                    if !retry && tab.early_bytes > MAX_PENDING {
                        let message = "탭 연결 실패: 초기 터미널 출력이 너무 큽니다. 탭을 다시 선택해 주세요.";
                        tab.attach_failed(message);
                        self.notice = Some(message.into());
                        self.bridge.detach_channel(host_id, handle.channel);
                        return;
                    }
                    early = std::mem::take(&mut tab.early_frames);
                    tab.early_bytes = 0;
                    if let Some(ch) = accept_tab_attach(Some(tab), &handle) {
                        self.bridge.detach_channel(host_id, ch);
                    }
                } else {
                    self.bridge.detach_channel(host_id, handle.channel);
                }
                if !retry {
                    for (ch, frame) in early {
                        self.handle_tab_frame(host_id, &handle.tab_id, ch, frame);
                    }
                } else if self.tab_is_visible(host_id, &handle.tab_id) {
                    self.ensure_selected_attached();
                }
            }
            BridgeEvent::TabAttachFailed {
                request,
                tab_id,
                run_generation,
                message,
                ..
            } => {
                if let Some(tab) = self
                    .find_tab_mut(host_id, &tab_id)
                    .filter(|tab| tab.attach_request == Some(request))
                {
                    tab.attach_failed(&message);
                    if run_generation == tab.run_generation {
                        self.notice = Some(message);
                    } else if self.tab_is_visible(host_id, &tab_id) {
                        self.ensure_selected_attached();
                    }
                }
            }
            BridgeEvent::ExternalInput {
                session_id,
                run_generation,
                from_host_id,
                is_remote,
                ..
            } => {
                if let Some(badge) = ExternalInputIndicator::new(
                    Instant::now(),
                    self.active_tab_key(),
                    (host_id, &session_id, run_generation),
                    &from_host_id,
                    is_remote,
                    &self.machines,
                ) {
                    self.external_input_indicator = Some(badge);
                }
            }
            BridgeEvent::SessionExited {
                session_id,
                room_id,
                run_generation,
                exit_code,
            } => {
                if room_id.starts_with("rm_bg_")
                    && let Some(machine) = self.machine_mut(host_id)
                {
                    machine
                        .background
                        .retain(|(_, t)| t.id != session_id || t.run_generation != run_generation);
                }
                if let Some(tab) = self.find_tab_mut(host_id, &session_id)
                    && tab.run_generation == run_generation
                    && tab.is_running()
                {
                    let status = match exit_code {
                        Some(0) => "정상 종료".into(),
                        Some(code) => format!("종료 코드 {code}"),
                        None => "종료됨".into(),
                    };
                    tab.status.clone_from(&status);
                    tab.runtime = TabRuntimeState::Exited { status };
                    tab.mode = ControlMode::Readonly;
                }
            }
            BridgeEvent::TabControlChanged { tab_id, mode } => {
                if let Some(tab) = self.find_tab_mut(host_id, &tab_id) {
                    tab.mode = mode;
                }
                if mode == ControlMode::Readonly
                    && let Some(peek) = &self.peek
                    && peek.host == host_id
                    && peek.tab.id == tab_id
                    && let Some(claim) = &peek.claim
                {
                    claim.relinquished();
                }
            }
            BridgeEvent::TabStream {
                tab_id,
                channel,
                frame,
            } => {
                self.handle_tab_frame(host_id, &tab_id, channel, frame);
            }
            BridgeEvent::LayoutSaveFailed { room_id, attempted } => {
                let dragging = self
                    .pane_drag
                    .as_ref()
                    .is_some_and(|drag| drag.host_id == host_id && drag.room_id == room_id);
                if !dragging
                    && let Some(room) = self.machine_mut(host_id).and_then(|machine| {
                        machine.rooms.iter_mut().find(|room| room.id == room_id)
                    })
                    && attempted.as_ref() == Some(&room.layout)
                {
                    room.restore_server_layout();
                    self.ensure_selected_attached();
                }
            }
            BridgeEvent::TabCloseFailed { tab_id, message } => {
                if let Some(tab) = self.find_tab_mut(host_id, &tab_id) {
                    tab.closing = false;
                    tab.status = message.clone();
                }
                self.notice = Some(message);
            }
        }
    }

    fn handle_tab_frame(&mut self, host_id: &str, tab_id: &str, channel: u32, frame: TabFrame) {
        let peeking = self
            .peek
            .as_ref()
            .is_some_and(|p| p.host == host_id && p.tab.id == tab_id);
        let Some(tab) = self.find_tab_mut(host_id, tab_id) else {
            return;
        };
        if tab.channel != Some(channel) {
            if tab.attaching {
                tab.buffer_frame(channel, frame);
            }
            return;
        }
        match frame {
            TabFrame::Data(bytes) => {
                if let Some(vt) = &mut tab.vt {
                    vt.write(&bytes);
                    let replies = vt.replies();
                    if !replies.is_empty() && tab.mode == ControlMode::Control && !tab.closing {
                        self.bridge.send_input(host_id, channel, &replies);
                    }
                }
            }
            TabFrame::Resize(cols, rows) => {
                if tab.size != (cols, rows) {
                    tab.size = (cols, rows);
                    if let Some(vt) = &mut tab.vt {
                        vt.resize(cols, rows);
                    }
                }
            }
            TabFrame::Close(reason) => {
                tab.channel = None;
                tab.reset_attach();
                tab.mode = ControlMode::Readonly;
                let exited = peeking
                    && match gpui::private::serde_json::from_str::<CloseReason>(&reason) {
                        Ok(CloseReason::Exited { exit_code, .. }) => {
                            tab.status = exit_code.map_or_else(
                                || "백그라운드 세션이 종료되었습니다".into(),
                                |code| format!("백그라운드 세션 종료 · 코드 {code}"),
                            );
                            true
                        }
                        Ok(CloseReason::Killed { .. }) => {
                            tab.status = "백그라운드 세션이 종료되었습니다".into();
                            true
                        }
                        _ => false,
                    };
                if exited {
                    let generation = tab.run_generation;
                    tab.runtime = TabRuntimeState::Exited {
                        status: tab.status.clone(),
                    };
                    if let Some(machine) = self.machine_mut(host_id) {
                        machine
                            .background
                            .retain(|(_, t)| t.id != tab_id || t.run_generation != generation);
                    }
                } else if !reason.is_empty() {
                    tab.status = sanitize_display_text(&reason, 240);
                }
            }
        }
    }

    fn drain(&mut self, cx: &mut Context<Self>) -> bool {
        let now = Instant::now();
        let mut changed = false;
        if self
            .zoom_indicator
            .is_some_and(|(_, expires_at)| now >= expires_at)
        {
            self.zoom_indicator = None;
            changed = true;
        }
        if self
            .external_input_indicator
            .as_ref()
            .is_some_and(|b| now >= b.expires_at)
        {
            self.external_input_indicator = None;
            changed = true;
        }
        for _ in 0..128 {
            let Some(ev) = self.bridge.try_recv() else {
                break;
            };
            match ev {
                BridgeMessage::UpdateResult {
                    req_id,
                    machine_id,
                    result,
                } => self.finish_update(req_id, machine_id, result, cx),
                BridgeMessage::HostsConfigReloaded(cfg) => {
                    changed |= self.apply_hosts_config(cfg);
                    continue;
                }
                BridgeMessage::UiPrefsReloaded(prefs) => {
                    let scale = UiScale::from_percent(prefs.ui_scale_pct);
                    self.pending_ui_scale = (scale != self.ui_scale).then_some(scale);
                    changed |= self.ui_prefs != prefs || self.pending_ui_scale.is_some();
                    self.ui_prefs = prefs;
                    continue;
                }
                BridgeMessage::AgentsReloaded(Ok(cfg)) => {
                    if self.agents != cfg
                        && let Some(modal) = self.agent_modal_mut().filter(|m| !m.blocked)
                    {
                        modal.conflict = true;
                    }
                    self.agents = cfg;
                    self.agent_error = None;
                }
                BridgeMessage::AgentsReloaded(Err(err)) => {
                    self.agent_error = Some(err);
                    if let Some(modal) = self.agent_modal_mut().filter(|m| !m.blocked) {
                        modal.conflict = true;
                    }
                }
                BridgeMessage::ProgramsReloaded(Ok(cfg)) => self.programs = cfg,
                BridgeMessage::ProgramsReloaded(Err(err)) => {
                    if !self.programs_warned {
                        self.programs_warned = true;
                        self.notice = Some(err);
                    }
                }
                BridgeMessage::Host(host_id, _, event) => self.handle_host_event(&host_id, event),
                BridgeMessage::DiscoveredCandidates {
                    ssh_hosts,
                    docker_containers,
                } => {
                    if let Some(modal) = self.machine_modal_mut() {
                        modal.ssh_candidates = ssh_hosts;
                        modal.docker_candidates = docker_containers;
                    }
                }
                BridgeMessage::InstallProgress {
                    req_id,
                    machine_id,
                    stage,
                } => {
                    if let Some(mid) = machine_id {
                        if let Some(machine) = self.machine_mut(&mid)
                            && machine.install_req_id == Some(req_id)
                        {
                            machine.installing_stage = Some(stage);
                        }
                    } else if let Some(modal) = self.machine_modal_mut()
                        && matches!(modal.test_state, TestState::Installing { req_id: r, .. } if r == req_id)
                    {
                        modal.test_state = TestState::Installing { req_id, stage };
                    }
                }
                BridgeMessage::InstallResult {
                    req_id,
                    machine_id: Some(mid),
                    result,
                } => {
                    self.finish_machine_install(&mid, req_id, result);
                }
                BridgeMessage::TestConnectionResult { req_id, result }
                | BridgeMessage::InstallResult {
                    req_id,
                    machine_id: None,
                    result,
                } => {
                    if let Some(modal) = self.machine_modal_mut()
                        && matches!(modal.test_state, TestState::Running { req_id: r } | TestState::Installing { req_id: r, .. } if r == req_id)
                    {
                        modal.test_state = match result {
                            Ok(host_id) => TestState::Success { host_id },
                            Err((kind, message)) => {
                                let copy_hint = modal.build_copy_hint(kind);
                                TestState::Failed {
                                    kind,
                                    message: sanitize_display_text(&message, 1024),
                                    copy_hint,
                                }
                            }
                        };
                    }
                }
                BridgeMessage::Error(message) => {
                    self.notice = Some(message);
                }
            }
            changed = true;
        }
        let working = self.has_visible_working_spinner();
        if spinner_due(self.window_active, working, &mut self.spinner_tick, now) {
            self.spinner_frame = (self.spinner_frame + 1) % 8;
            changed = true;
        }
        changed
    }

    fn save_ui_prefs(&mut self) {
        if let Some(path) = &self.ui_prefs_path
            && let Err(err) = self.ui_prefs.save(path)
        {
            self.notice = Some(format!("UI 설정 저장 실패: {err}"));
        }
    }

    fn section_collapsed(&self, key: &str) -> bool {
        self.ui_prefs.collapsed_sections.contains(key)
    }

    fn toggle_section(&mut self, key: &str) {
        if !self.ui_prefs.collapsed_sections.remove(key) {
            self.ui_prefs.collapsed_sections.insert(key.to_owned());
        }
        self.save_ui_prefs();
    }

    fn program_icon_key(&self, machine: &MachineView, tab: &TabView) -> String {
        let local = matches!(machine.profile.transport, HostTransport::Local { .. });
        let profile = tab.agent_profile.as_deref();
        let candidate = profile
            .and_then(|id| self.agents.find(id))
            .and_then(|p| p.icon.as_deref());
        if let Some(key) = candidate
            && (local || (!key.ends_with(".svg") && !key.ends_with(".png")))
        {
            return key.into();
        }
        let programs = if local {
            &self.programs
        } else {
            &BUILTIN_PROGRAMS
        };
        let status = tab.activity.as_ref().and_then(|a| a.agent_status.as_ref());
        let foreground = tab.activity.as_ref().and_then(|a| a.foreground.as_ref());
        let argv = tab.argv0.as_deref().and_then(|s| s.rsplit('/').next());
        let matches = |keys: &[String], name: Option<&str>| {
            name.is_some_and(|name| keys.iter().any(|key| key.eq_ignore_ascii_case(name)))
        };
        let rows = &programs.programs;
        rows.iter()
            .find(|p| matches(&p.matchers.agent_profiles, profile))
            .or_else(|| {
                rows.iter()
                    .find(|p| matches(&p.matchers.osc_apps, status.and_then(|s| s.app.as_deref())))
            })
            .or_else(|| {
                rows.iter().find(|p| {
                    matches(
                        &p.matchers.script_hints,
                        foreground.and_then(|s| s.script_hint.as_deref()),
                    )
                })
            })
            .or_else(|| {
                rows.iter()
                    .find(|p| matches(&p.matchers.comms, foreground.map(|s| s.comm.as_str())))
            })
            .or_else(|| rows.iter().find(|p| matches(&p.matchers.comms, argv)))
            .map(|p| p.icon.clone())
            .unwrap_or_default()
    }

    fn display_agent_icon(&self, host: &str, key: &str, size: f32) -> AnyElement {
        let remote = self
            .machine(host)
            .is_none_or(|m| !matches!(m.profile.transport, HostTransport::Local { .. }));
        let dir = self
            .ui_prefs_path
            .as_ref()
            .map(|path| path.with_file_name("icons"));
        let path = self.assets.resolve_icon_key(dir.as_deref(), key, remote);
        if path == Icon::Terminal.path() {
            icon(Icon::Terminal, size, rgb(0xababab)).into_any_element()
        } else {
            img(ImageSource::Resource(Resource::Embedded(path)))
                .size(px(size))
                .flex_none()
                .into_any_element()
        }
    }

    fn tab_status_badge(&self, state: AgentState, unread: bool) -> AnyElement {
        let size = self.ui_scale.px(13.);
        match state {
            AgentState::Working => icon(Icon::Spinner, size, rgb(0x92b6ff))
                .with_transformation(Transformation::rotate(radians(
                    self.spinner_frame as f32 * std::f32::consts::FRAC_PI_4,
                )))
                .into_any_element(),
            AgentState::Blocked => div()
                .text_color(rgb(0xf2ca80))
                .child("!")
                .into_any_element(),
            AgentState::Error => div()
                .text_color(rgb(0xff9292))
                .child("오류")
                .into_any_element(),
            AgentState::Done if unread => div()
                .size(px(self.ui_scale.px(6.)))
                .rounded_full()
                .bg(rgb(0x8ad3a6))
                .into_any_element(),
            _ => div().into_any_element(),
        }
    }

    fn agent_rows(&self) -> Vec<Row> {
        let mut entries = Vec::new();
        for machine in self.machines.iter().filter(|m| m.is_connected()) {
            let host = &machine.profile.id;
            let rows = machine
                .rooms
                .iter()
                .flat_map(|room| {
                    room.tabs
                        .iter()
                        .map(move |tab| (&room.id, room.label(), tab, false))
                })
                .chain(
                    machine
                        .background
                        .iter()
                        .map(|(room, tab)| (room, "백그라운드".into(), tab, true)),
                );
            for (room, label, tab, background) in rows {
                let activity = tab.activity.as_ref();
                let status = activity.and_then(|a| a.agent_status.as_ref());
                if background && !tab.is_running()
                    || status.is_none() && tab.agent_profile.is_none() && !background
                {
                    continue;
                }
                let parent = tab.parent.as_ref().map(|(parent_host, parent)| {
                    let profile = parent_host
                        .as_ref()
                        .and_then(|id| {
                            self.machines
                                .iter()
                                .find(|m| m.connected_host_id.as_deref() == Some(id))
                                .map(|m| m.profile.id.as_str())
                        })
                        .unwrap_or(host);
                    format!("{profile}:{parent}")
                });
                let title = status
                    .and_then(|s| s.title.as_deref())
                    .filter(|s| !s.is_empty())
                    .or((!tab.title.is_empty()).then_some(tab.title.as_str()))
                    .or(tab.agent_profile.as_deref())
                    .unwrap_or(&tab.id);
                entries.push(AgentEntry {
                    key: format!("{host}:{}", tab.id),
                    host: host.clone(),
                    room: room.clone(),
                    tab: tab.id.clone(),
                    parent,
                    title: sanitize_display_text(title, DISPLAY_TEXT_LIMIT),
                    location: format!("{label} · {}", machine.profile.label),
                    message: status
                        .and_then(|s| s.msg.as_ref())
                        .map(|s| sanitize_display_text(s, 240)),
                    background,
                    state: if tab.is_running() {
                        status.map_or(AgentState::Idle, |s| s.state)
                    } else {
                        AgentState::Idle
                    },
                    unread: activity.is_some_and(|a| a.activity_seq > a.seen_seq),
                    icon_key: self.program_icon_key(machine, tab),
                });
            }
        }
        build_agent_tree(entries, &self.ui_prefs)
    }

    fn visible_agent_rows(&self) -> Vec<Row> {
        let mut rows = self.agent_rows();
        if self.ui_prefs.agent_sort == AgentSortMode::Grouped {
            rows.retain(|row| {
                row.pinned
                    || !self
                        .section_collapsed(&format!("group:{}:{}", row.entry.host, row.entry.room))
            });
        }
        rows
    }

    fn has_visible_working_spinner(&self) -> bool {
        let working = |tab: &TabView| {
            tab.is_running()
                && tab
                    .activity
                    .as_ref()
                    .and_then(|a| a.agent_status.as_ref())
                    .is_some_and(|s| s.state == AgentState::Working)
        };
        (!self.section_collapsed("section:agents")
            && self.machines.iter().filter(|m| m.is_connected()).any(|m| {
                let visible = |room: &str, tab: &TabView| {
                    self.ui_prefs.agent_sort != AgentSortMode::Grouped
                        || self
                            .ui_prefs
                            .pinned_agents
                            .contains(&format!("{}:{}", m.profile.id, tab.id))
                        || !self.section_collapsed(&format!("group:{}:{room}", m.profile.id))
                };
                m.rooms.iter().any(|room| {
                    room.tabs
                        .iter()
                        .any(|tab| working(tab) && visible(&room.id, tab))
                }) || m
                    .background
                    .iter()
                    .any(|(room, tab)| working(tab) && visible(room, tab))
            }))
            || if let Some(peek) = &self.peek {
                self.machine(&peek.host)
                    .is_some_and(MachineView::is_connected)
                    && working(&peek.tab)
            } else {
                self.is_active_connected()
                    && self
                        .room()
                        .is_some_and(|room| room.tabs.iter().any(working))
            }
    }

    fn action_button(
        cx: &mut Context<Self>,
        id: impl Into<ElementId>,
        label: impl IntoElement,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .px_2()
            .py_1()
            .rounded_sm()
            .cursor_pointer()
            .text_xs()
            .bg(rgb(0x262626))
            .hover(|s| s.bg(rgb(0x394354)))
            .child(label)
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                action(this, window, cx);
                cx.notify();
            }))
    }

    fn agent_sidebar(&self, sidebar: f32, height: f32, cx: &mut Context<Self>) -> Div {
        let collapsed = self.section_collapsed("section:agents");
        let compact = sidebar <= self.ui_scale.px(185.);
        let rows = self.agent_rows();
        let heading = div()
            .flex()
            .items_center()
            .gap_1()
            .child(Self::action_button(
                cx,
                "agents-section",
                format!("에이전트 ({})", rows.len()),
                |this, _, _| this.toggle_section("section:agents"),
            ))
            .child(Self::action_button(
                cx,
                "agents-sort",
                match self.ui_prefs.agent_sort {
                    AgentSortMode::Priority => {
                        if compact {
                            "우선"
                        } else {
                            "우선순위"
                        }
                    }
                    AgentSortMode::Grouped => "그룹",
                    AgentSortMode::Manual => "수동",
                },
                |this, _, _| this.cycle_agent_sort(),
            ))
            .child(Self::action_button(
                cx,
                "agents-launch",
                icon(Icon::Plus, self.ui_scale.px(13.), rgb(0xefefef)),
                |this, _, _| this.open_launcher(),
            ));
        let mut list = div()
            .id("agents-scroll")
            .min_h_0()
            .overflow_y_scroll()
            .flex_1();
        let mut group = String::new();
        let mut visible_index = 0;
        for row in rows {
            let entry = row.entry;
            if !row.pinned && self.ui_prefs.agent_sort == AgentSortMode::Grouped {
                let key = format!("group:{}:{}", entry.host, entry.room);
                if key != group {
                    let toggle = key.clone();
                    list = list.child(Self::action_button(
                        cx,
                        SharedString::from(key.clone()),
                        entry.location.clone(),
                        move |this, _, _| this.toggle_section(&toggle),
                    ));
                    group = key;
                }
                if self.section_collapsed(&group) {
                    continue;
                }
            }
            let active = matches!(self.focus_zone, KeyboardFocusZone::AgentList)
                && self.agent_cursor == visible_index;
            visible_index += 1;
            let mut title = div()
                .flex()
                .min_w_0()
                .items_center()
                .gap_1()
                .when(row.pinned && row.depth == 0, |d| {
                    d.child(icon(Icon::Pin, self.ui_scale.px(12.), rgb(0xe5c07b)))
                })
                .child(self.display_agent_icon(&entry.host, &entry.icon_key, self.ui_scale.px(14.)))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .child(entry.title.clone()),
                )
                .child(self.tab_status_badge(entry.state, entry.unread));
            if entry.background && !compact {
                title = title.child(div().text_xs().text_color(rgb(0xb6bbc1)).child("BG"));
            }
            if self.hovered_agent.as_deref() == Some(&entry.key) || active {
                let pin = entry.key.clone();
                let pinned = self.ui_prefs.pinned_agents.contains(&pin);
                title = title.child(
                    Self::action_button(
                        cx,
                        stable_elem_id("agent-pin", &[&pin]),
                        icon(
                            Icon::Pin,
                            self.ui_scale.px(11.),
                            rgb(if pinned { 0xe5c07b } else { 0xb6bbc1 }),
                        ),
                        move |this, _, _| this.toggle_agent_pin(&pin),
                    )
                    .px_1()
                    .tooltip(move |_, cx| {
                        cx.new(|_| AgentTooltip(if pinned { "핀 해제" } else { "핀 고정" }.into()))
                            .into()
                    }),
                );
                for (down, glyph, label) in [(false, "↑", "위로 이동"), (true, "↓", "아래로 이동")]
                {
                    let key = entry.key.clone();
                    title = title.child(
                        Self::action_button(
                            cx,
                            stable_elem_id("agent-move", &[&key, glyph]),
                            glyph,
                            move |this, _, _| this.move_agent(&key, down),
                        )
                        .px_1()
                        .tooltip(move |_, cx| cx.new(|_| AgentTooltip(label.into())).into()),
                    );
                }
            }
            let key: SharedString = entry.key.clone().into();
            let hover_key = entry.key.clone();
            list = list.child(
                div()
                    .id(key)
                    .cursor_pointer()
                    .relative()
                    .px_1()
                    .pl(px(self.ui_scale.px(
                        4. + row.depth.min(3) as f32 * if compact { 7. } else { 11. }
                    )))
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        if *hovered {
                            this.hovered_agent = Some(hover_key.clone());
                        } else if this.hovered_agent.as_ref() == Some(&hover_key) {
                            this.hovered_agent = None;
                        }
                        cx.notify();
                    }))
                    .py_1()
                    .min_w_0()
                    .rounded_sm()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .hover(|s| s.bg(rgb(0x2d3138)))
                    .when(active, |s| {
                        s.bg(rgb(0x30456b)).border_1().border_color(rgb(0x92b6ff))
                    })
                    .child(title)
                    .when(compact && entry.background, |d| {
                        d.child(
                            div()
                                .absolute()
                                .left_0()
                                .top_1()
                                .bottom_1()
                                .w(px(self.ui_scale.px(2.)))
                                .flex()
                                .flex_col()
                                .justify_between()
                                .children((0..3).map(|_| {
                                    div().w_full().h(px(self.ui_scale.px(3.))).bg(rgb(0x8798ac))
                                })),
                        )
                    })
                    .when(!compact, |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(rgb(0xb6bbc1))
                                .truncate()
                                .child(entry.location.clone()),
                        )
                    })
                    .when(!compact, |d| {
                        d.children(entry.message.as_ref().map(|msg| {
                            div()
                                .text_xs()
                                .text_color(rgb(0x8a8a8a))
                                .truncate()
                                .child(msg.chars().take(80).collect::<String>())
                        }))
                    })
                    .tooltip({
                        let label = format!(
                            "{} · {}{}",
                            entry.title,
                            entry.location,
                            entry
                                .message
                                .as_ref()
                                .map(|msg| format!(" · 세션 보고: {msg}"))
                                .unwrap_or_default()
                        );
                        move |_, cx| cx.new(|_| AgentTooltip(label.clone())).into()
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_agent(&entry, window);
                        cx.notify();
                    })),
            );
        }
        div()
            .flex()
            .flex_col()
            .min_h_0()
            .child(heading)
            .when(!collapsed, |d| {
                d.when(!self.section_collapsed("section:rooms"), |d| {
                    d.max_h(px(height * 0.45))
                })
                .when(self.section_collapsed("section:rooms"), |d| d.flex_1())
                .child(list)
            })
    }

    fn move_agent(&mut self, key: &str, down: bool) {
        let rows = self.agent_rows();
        if let Some(index) = rows.iter().position(|r| r.entry.key == key)
            && move_subtree(&rows, index, down, &mut self.ui_prefs)
        {
            self.save_ui_prefs();
            self.focus_agent_row(key);
        }
    }

    fn toggle_agent_pin(&mut self, key: &str) {
        let pins = &mut self.ui_prefs.pinned_agents;
        if let Some(i) = pins.iter().position(|id| id == key) {
            pins.remove(i);
        } else if pins.len() < 256 {
            pins.push(key.to_owned());
        } else {
            return;
        }
        self.save_ui_prefs();
        self.focus_agent_row(key);
    }

    fn focus_agent_row(&mut self, key: &str) {
        self.agent_cursor = self
            .visible_agent_rows()
            .iter()
            .position(|r| r.entry.key == key)
            .unwrap_or(0);
    }

    fn cycle_agent_sort(&mut self) {
        self.keyboard_group = None;
        self.ui_prefs.agent_sort = match self.ui_prefs.agent_sort {
            AgentSortMode::Priority => AgentSortMode::Grouped,
            AgentSortMode::Grouped => AgentSortMode::Manual,
            AgentSortMode::Manual => AgentSortMode::Priority,
        };
        self.agent_cursor = 0;
        self.save_ui_prefs();
    }

    fn aggregate_badge<'a>(
        &self,
        tabs: impl Iterator<Item = &'a TabView>,
        compact: bool,
    ) -> Option<AnyElement> {
        let mut summary = [0u32; 4];
        for activity in tabs
            .filter(|t| t.is_running())
            .filter_map(|tab| tab.activity.as_ref())
        {
            let Some(status) = &activity.agent_status else {
                continue;
            };
            let index = match status.state {
                AgentState::Blocked => 0,
                AgentState::Error => 1,
                AgentState::Working => 2,
                AgentState::Done if activity.activity_seq > activity.seen_seq => 3,
                _ => continue,
            };
            summary[index] += 1;
        }
        let colors = [0xf2ca80, 0xff9292, 0x92b6ff, 0x8ad3a6];
        let labels = ["대기", "오류", "실행", "완료"];
        let first = summary.iter().position(|n| *n > 0)?;
        Some(if compact {
            div()
                .size(px(self.ui_scale.px(6.)))
                .flex_none()
                .rounded_full()
                .bg(rgb(colors[first]))
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_none()
                .gap_1()
                .children(
                    summary
                        .into_iter()
                        .enumerate()
                        .filter(|(_, n)| *n > 0)
                        .map(|(i, n)| {
                            div()
                                .text_xs()
                                .text_color(rgb(colors[i]))
                                .child(format!("{} {n}", labels[i]))
                        }),
                )
                .into_any_element()
        })
    }

    fn render_room_row(
        &self,
        host_id: &str,
        room: &RoomView,
        indented: bool,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let scale = self.ui_scale;
        let key = RoomKey {
            host_id: host_id.to_owned(),
            room_id: room.id.clone(),
        };
        let is_active =
            self.active_host_id == host_id && self.active_room_id.as_ref() == Some(&room.id);
        let is_hovered = self.hovered_room.as_ref() == Some(&key);
        let parts = [host_id, room.id.as_str()];
        let key_hover = key.clone();
        let key_edit = key.clone();
        let key_click = key;

        div()
            .id(stable_elem_id("room", &parts))
            .flex()
            .items_center()
            .gap_1()
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    this.hovered_room = Some(key_hover.clone());
                } else if this.hovered_room.as_ref() == Some(&key_hover) {
                    this.hovered_room = None;
                }
                cx.notify();
            }))
            .pl(px(scale.px(if indented { 16. } else { 8. })))
            .pr_2()
            .py_1()
            .rounded_sm()
            .overflow_hidden()
            .whitespace_nowrap()
            .cursor_pointer()
            .bg(rgb(if is_active { 0x373737 } else { 0x191919 }))
            .hover(|s| s.bg(rgb(if is_active { 0x373737 } else { 0x262626 })))
            .child(div().flex_1().min_w_0().truncate().child(room.label()))
            .children(self.aggregate_badge(room.tabs.iter(), compact))
            .child(
                icon_box(px(scale.px(20.)))
                    .id(stable_elem_id("edit-room", &parts))
                    .when(is_hovered, |slot| {
                        slot.group(ICON_HOVER_GROUP)
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0x505050)))
                            .child(hover_icon(Icon::Pencil, scale.px(12.), 0x9a9a9a))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                let Some(room) = this.machine(&key_edit.host_id).and_then(|m| {
                                    m.rooms.iter().find(|r| r.id == key_edit.room_id)
                                }) else {
                                    return;
                                };
                                this.active_overlay = Some(ActiveOverlay::RenameModal(
                                    key_edit.clone(),
                                    room.name.clone().unwrap_or_default(),
                                ));
                                this.focus.focus(window);
                                cx.notify();
                            }))
                    }),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.active_host_id = key_click.host_id.clone();
                this.close_peek(false);
                this.focus_zone = KeyboardFocusZone::Terminal;
                this.active_room_id = Some(key_click.room_id.clone());
                this.active_overlay = None;
                this.ensure_selected_attached();
                this.focus.focus(window);
                cx.notify();
            }))
    }

    fn room_sidebar(&self, sidebar: f32, cx: &mut Context<Self>) -> Div {
        let scale = self.ui_scale;
        let compact = sidebar <= scale.px(185.);
        let grouped = self.show_machine_sections();
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(Self::action_button(
                cx,
                "rooms-section",
                "룸",
                |this, _, _| this.toggle_section("section:rooms"),
            ))
            .when(!grouped, |el| {
                el.child(
                    icon_box(px(scale.px(20.)))
                        .id("add-room")
                        .group(ICON_HOVER_GROUP)
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(0x262626)))
                        .child(hover_icon(Icon::Plus, scale.px(14.), 0x9a9a9a))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.new_room(window);
                            cx.notify();
                        })),
                )
            });

        if self.section_collapsed("section:rooms") {
            return header;
        }
        let list = if !grouped {
            let mut items = Vec::new();
            if let Some(m) = self.machines.first() {
                for room in &m.rooms {
                    items.push(
                        self.render_room_row(&m.profile.id, room, false, compact, cx)
                            .into_any_element(),
                    );
                }
            }
            div()
                .id("rooms")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(scale.px(2.)))
                .children(items)
        } else {
            let mut sections = Vec::new();
            for m in &self.machines {
                let hid = m.profile.id.clone();
                let hid_hover = hid.clone();
                let hid_edit = hid.clone();
                let hid_toggle = hid.clone();
                let hid_add = hid.clone();
                let dot_color = m.status_dot_color();
                let chevron = if m.collapsed {
                    Icon::ChevronRight
                } else {
                    Icon::ChevronDown
                };
                let control_size = px(scale.px(20.));
                let label = sanitize_display_text(&m.profile.label, DISPLAY_TEXT_LIMIT);
                let is_active_empty = m.rooms.is_empty() && self.active_host_id == hid;
                let is_remote = !matches!(m.profile.transport, HostTransport::Local { .. });
                let is_hovered = self.hovered_machine.as_deref() == Some(hid.as_str());
                let edit = is_remote.then(|| {
                    icon_box(control_size)
                        .id(stable_elem_id("machine-edit", &[&hid]))
                        .when(is_hovered, |button| {
                            button
                                .group(ICON_HOVER_GROUP)
                                .rounded_sm()
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(0x373737)))
                                .child(hover_icon(Icon::Pencil, scale.px(12.), 0x888888))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.open_edit_machine(&hid_edit, window);
                                    cx.notify();
                                }))
                        })
                });

                let section_header =
                    div()
                        .id(stable_elem_id("machine-hdr", &[&hid]))
                        .group(MACHINE_HEADER_GROUP)
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .mt_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .when(is_active_empty, |el| el.bg(rgb(0x373737)))
                        .hover(|s| s.bg(rgb(0x242424)))
                        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                            if *hovered {
                                this.hovered_machine = Some(hid_hover.clone());
                            } else if this.hovered_machine.as_deref() == Some(hid_hover.as_str()) {
                                this.hovered_machine = None;
                            }
                            cx.notify();
                        }))
                        .child(icon(Icon::Server, scale.px(14.), rgb(0x888888)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(rgb(0x9a9a9a))
                                .child(label),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .flex_none()
                                .children(edit)
                                .children(self.aggregate_badge(
                                    m.rooms.iter().flat_map(|room| &room.tabs),
                                    true,
                                ))
                                .child(
                                    icon_box(control_size)
                                        .id(stable_elem_id("machine-add", &[&hid]))
                                        .group(ICON_HOVER_GROUP)
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .hover(|s| s.bg(rgb(0x373737)))
                                        .child(hover_icon(Icon::Plus, scale.px(14.), 0x888888))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.new_room_on(&hid_add, window);
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .h(control_size)
                                        .px(px(scale.px(2.)))
                                        .child(
                                            div()
                                                .size(px(scale.px(7.)))
                                                .rounded_full()
                                                .bg(rgb(dot_color)),
                                        ),
                                )
                                .child(
                                    icon_box(control_size).w(px(scale.px(16.))).child(
                                        icon(chevron, scale.px(12.), rgb(0x888888))
                                            .group_hover(MACHINE_HEADER_GROUP, |s| {
                                                s.text_color(rgb(0xffffff))
                                            }),
                                    ),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(machine) = this.machine_mut(&hid_toggle) {
                                machine.collapsed = !machine.collapsed;
                                if machine.rooms.is_empty() {
                                    this.active_host_id.clone_from(&hid_toggle);
                                    this.active_room_id = None;
                                    this.active_overlay = None;
                                }
                            }
                            cx.notify();
                        }));

                sections.push(section_header.into_any_element());
                if !m.collapsed && !m.is_security_blocked() {
                    for room in &m.rooms {
                        sections.push(
                            self.render_room_row(&m.profile.id, room, true, compact, cx)
                                .into_any_element(),
                        );
                    }
                }
            }
            div()
                .id("rooms")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(scale.px(2.)))
                .children(sections)
        };

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_1()
            .child(header)
            .child(list)
    }

    fn save_hosts_config(&mut self) -> bool {
        let cfg = HostsConfig {
            schema_version: ddg_agents_transport::profile::HOSTS_SCHEMA_VERSION,
            default_host_id: self.default_host_id.clone(),
            hosts: self.machines.iter().map(|m| m.profile.clone()).collect(),
        };
        match HostsConfig::default_path().and_then(|p| cfg.save(&p)) {
            Ok(()) => true,
            Err(e) => {
                self.notice = Some(format!("hosts.json 저장 실패: {e}"));
                false
            }
        }
    }

    fn open_add_machine(&mut self, window: &mut Window) {
        if self.update_modal.is_some() {
            return;
        }
        self.active_overlay = Some(ActiveOverlay::MachineModal(Box::new(
            MachineModal::new_add(),
        )));
        self.bridge.discover_candidates();
        self.focus.focus(window);
    }

    fn open_edit_machine(&mut self, host_id: &str, window: &mut Window) {
        if self.update_modal.is_some() {
            return;
        }
        let Some(machine) = self
            .machine(host_id)
            .filter(|m| m.install_req_id.is_none() && m.remote_update.req_id.is_none())
        else {
            return;
        };
        let modal = MachineModal::from_profile(&machine.profile);
        self.active_overlay = Some(ActiveOverlay::MachineModal(Box::new(modal)));
        self.bridge.discover_candidates();
        self.focus.focus(window);
    }

    fn run_remote_install(&mut self, cx: &mut Context<Self>) {
        self.run_machine_check(true, cx);
    }

    fn run_machine_check(&mut self, install: bool, cx: &mut Context<Self>) {
        let existing_ids: Vec<String> =
            self.machines.iter().map(|m| m.profile.id.clone()).collect();
        let Some(ActiveOverlay::MachineModal(modal)) = &mut self.active_overlay else {
            return;
        };
        if modal.is_busy()
            || (install
                && !(modal.supports_remote_install()
                    && matches!(
                        modal.test_state,
                        TestState::Failed {
                            kind: DialErrorKind::HostNotInstalled,
                            ..
                        }
                    )))
        {
            return;
        }
        modal.error = None;
        modal.copied_hint = false;
        match modal.build_profile(&existing_ids) {
            Ok(profile) if install => {
                let req_id = self.bridge.install_remote_host(profile, None);
                modal.test_state = TestState::Installing {
                    req_id,
                    stage: "설치 준비 중...".into(),
                };
            }
            Ok(profile) => {
                let req_id = self.bridge.test_connection(profile);
                modal.test_state = TestState::Running { req_id };
            }
            Err(e) => {
                modal.error = Some(e);
            }
        }
        cx.notify();
    }

    fn start_machine_install(&mut self, host_id: &str, cx: &mut Context<Self>) {
        let Some(machine) = self.machine(host_id).filter(|m| {
            m.install_req_id.is_none()
                && m.remote_update.req_id.is_none()
                && (matches!(m.conn_state, ConnectionState::HostNotInstalled(_))
                    || m.lacks_activity())
                && matches!(
                    m.profile.transport,
                    HostTransport::Ssh { .. } | HostTransport::ContainerSsh { .. }
                )
        }) else {
            return;
        };
        if !machine.update_confirm {
            if let Some(machine) = self.machine_mut(host_id) {
                machine.update_confirm = true;
            }
            cx.notify();
            return;
        }
        let req_id = self
            .bridge
            .install_remote_host(machine.profile.clone(), Some(host_id.to_owned()));
        if let Some(machine) = self.machine_mut(host_id) {
            machine.update_confirm = false;
            machine.install_req_id = Some(req_id);
            machine.installing_stage = Some("설치 준비 중...".into());
        }
        cx.notify();
    }

    fn finish_machine_install(
        &mut self,
        host_id: &str,
        req_id: u64,
        result: Result<String, (DialErrorKind, String)>,
    ) {
        let Some(machine) = self
            .machine_mut(host_id)
            .filter(|m| m.install_req_id == Some(req_id))
        else {
            return;
        };
        machine.install_req_id = None;
        machine.installing_stage = None;
        match result {
            Ok(remote_host_id) => {
                machine
                    .profile
                    .expected_host_id
                    .get_or_insert(remote_host_id);
                let profile = machine.profile.clone();
                if self.save_hosts_config() {
                    if let Some(machine) = self.machine_mut(host_id) {
                        machine.conn_state = ConnectionState::Connecting;
                    }
                    self.bridge.add_or_update_machine(profile);
                }
            }
            Err((kind, message)) => {
                let message = sanitize_display_text(&message, 1024);
                machine.conn_state = match kind {
                    DialErrorKind::HostKeyMismatch
                    | DialErrorKind::HostIdMismatch
                    | DialErrorKind::SecurityBlocked => ConnectionState::HostKeyMismatch(message),
                    DialErrorKind::AuthRequired => ConnectionState::AuthRequired(message),
                    DialErrorKind::ProtocolIncompatible => ConnectionState::ProtocolIncompatible {
                        local_protocol: ddg_agents_proto::terminal::PROTOCOL_VERSION,
                        remote_protocol: 0,
                        remote_version: None,
                        message,
                    },
                    _ => {
                        ConnectionState::HostNotInstalled(format!("원격 자동 설치 실패: {message}"))
                    }
                };
            }
        }
    }

    fn dismiss_machine_modal(&mut self, cx: &mut Context<Self>) {
        if self
            .machine_modal()
            .is_some_and(|modal| !matches!(modal.test_state, TestState::Installing { .. }))
        {
            self.active_overlay = None;
            cx.notify();
        }
    }

    fn save_machine_modal(&mut self, cx: &mut Context<Self>) {
        let existing_ids: Vec<String> =
            self.machines.iter().map(|m| m.profile.id.clone()).collect();
        let profile = {
            let Some(modal) = self.machine_modal_mut().filter(|m| !m.is_busy()) else {
                return;
            };
            match modal.build_profile(&existing_ids) {
                Ok(p) => p,
                Err(e) => {
                    modal.error = Some(e);
                    cx.notify();
                    return;
                }
            }
        };
        if let Some(existing) = self
            .machines
            .iter_mut()
            .find(|m| m.profile.id == profile.id)
        {
            existing.profile = profile.clone();
            existing.conn_state = ConnectionState::Connecting;
            existing.connected_host_id = None;
            existing.catalog_seq = None;
            existing.reset_notice = None;
            existing.rooms.clear();
        } else {
            self.machines.push(MachineView::new(profile.clone()));
        }
        self.active_overlay = None;
        self.save_hosts_config();
        self.bridge.add_or_update_machine(profile);
        cx.stop_propagation();
        cx.notify();
    }

    fn remove_editing_machine(&mut self, cx: &mut Context<Self>) {
        let Some(host_id) = self
            .machine_modal()
            .filter(|m| !m.is_busy())
            .and_then(|m| m.editing_id.clone())
        else {
            return;
        };
        if self
            .machine(&host_id)
            .is_some_and(|m| matches!(m.profile.transport, HostTransport::Local { .. }))
        {
            return;
        }
        self.machines.retain(|m| m.profile.id != host_id);
        self.bridge.remove_machine(&host_id);
        self.active_overlay = None;
        if self.hovered_machine.as_deref() == Some(host_id.as_str()) {
            self.hovered_machine = None;
        }
        if self.active_host_id == host_id {
            if let Some(first) = self.machines.first() {
                self.active_host_id = first.profile.id.clone();
                self.active_room_id = first.rooms.first().map(|r| r.id.clone());
            } else {
                self.active_room_id = None;
            }
            self.ensure_selected_attached();
        }
        self.save_hosts_config();
        cx.stop_propagation();
        cx.notify();
    }

    fn modal_text_input(
        id: &'static str,
        label: &'static str,
        value: &str,
        placeholder: &'static str,
        field: ModalField,
        active_field: ModalField,
        cx: &mut Context<Self>,
    ) -> Div {
        let focused = field == active_field;
        let label_value;
        let value = if field == ModalField::Label {
            label_value = sanitize_display_text(value, DISPLAY_TEXT_LIMIT);
            label_value.as_str()
        } else {
            value
        };
        let display = if value.is_empty() && !focused {
            placeholder.to_owned()
        } else if focused {
            format!("{value}▏")
        } else {
            value.to_owned()
        };
        let text_color = if value.is_empty() && !focused {
            0x6e6e6e
        } else {
            0xf2f2f2
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_xs().text_color(rgb(0xbcbcbc)).child(label))
            .child(
                div()
                    .id(id)
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(if focused { 0x80a8ff } else { 0x454545 }))
                    .bg(rgb(0x191919))
                    .text_color(rgb(text_color))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .cursor_text()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(modal) = this.machine_modal_mut() {
                            modal.active_field = field;
                        }
                        this.focus.focus(window);
                        cx.notify();
                    }))
                    .child(display),
            )
    }

    fn machine_modal_view(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let modal = self.machine_modal()?;
        let scale = self.ui_scale;
        let is_edit = modal.editing_id.is_some();
        let busy = modal.is_busy();
        let installing = matches!(modal.test_state, TestState::Installing { .. });
        let active_field = modal.active_field;
        let preview = modal.ssh_command_preview();

        let tabs = div().flex().gap_1().children(
            [
                (MachineTab::Ssh, "SSH"),
                (MachineTab::Docker, "Docker"),
                (MachineTab::ContainerSsh, "컨테이너 SSH"),
            ]
            .into_iter()
            .enumerate()
            .map(|(idx, (tab, title))| {
                let selected = modal.tab == tab;
                div()
                    .id(("m-tab", idx))
                    .px_3()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .bg(rgb(if selected { 0x3b3b3b } else { 0x1d1d1d }))
                    .text_color(rgb(if selected { 0xffffff } else { 0x9a9a9a }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(m) = this.machine_modal_mut() {
                            m.switch_tab(tab);
                        }
                        this.focus.focus(window);
                        cx.notify();
                    }))
                    .child(title)
            }),
        );

        let ssh_chips =
            (!modal.ssh_candidates.is_empty() && modal.tab == MachineTab::Ssh).then(|| {
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x888888))
                            .child("빠른 선택 (~/.ssh/config)"),
                    )
                    .child(
                        div().flex().flex_wrap().gap_1().children(
                            modal
                                .ssh_candidates
                                .iter()
                                .take(8)
                                .enumerate()
                                .map(|(i, h)| {
                                    let host = h.clone();
                                    div()
                                        .id(("ssh-cand", i))
                                        .px_2()
                                        .py(px(scale.px(2.)))
                                        .rounded_sm()
                                        .bg(rgb(0x1f1f1f))
                                        .border_1()
                                        .border_color(rgb(0x3a3a3a))
                                        .hover(|s| s.bg(rgb(0x303030)))
                                        .text_xs()
                                        .cursor_pointer()
                                        .child(h.clone())
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            if let Some(m) =
                                                this.machine_modal_mut().filter(|m| !m.is_busy())
                                            {
                                                m.select_ssh_candidate(&host);
                                            }
                                            this.focus.focus(window);
                                            cx.notify();
                                        }))
                                }),
                        ),
                    )
            });

        let docker_chips = (!modal.docker_candidates.is_empty() && modal.tab == MachineTab::Docker)
            .then(|| {
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x888888))
                            .child("실행 중인 로컬 컨테이너"),
                    )
                    .child(
                        div().flex().flex_wrap().gap_1().children(
                            modal
                                .docker_candidates
                                .iter()
                                .take(8)
                                .enumerate()
                                .map(|(i, c)| {
                                    let ctr = c.clone();
                                    div()
                                        .id(("docker-cand", i))
                                        .px_2()
                                        .py(px(scale.px(2.)))
                                        .rounded_sm()
                                        .bg(rgb(0x1f1f1f))
                                        .border_1()
                                        .border_color(rgb(0x3a3a3a))
                                        .hover(|s| s.bg(rgb(0x303030)))
                                        .text_xs()
                                        .cursor_pointer()
                                        .child(c.clone())
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            if let Some(m) =
                                                this.machine_modal_mut().filter(|m| !m.is_busy())
                                            {
                                                m.select_docker_candidate(&ctr);
                                            }
                                            this.focus.focus(window);
                                            cx.notify();
                                        }))
                                }),
                        ),
                    )
            });

        let fields_body = match modal.tab {
            MachineTab::Ssh => div()
                .flex()
                .flex_col()
                .gap_2()
                .children(ssh_chips)
                .child(Self::modal_text_input(
                    "m-ssh-input",
                    "SSH 대상 또는 접속 명령어 붙여넣기",
                    &modal.ssh_input,
                    "예: dev@gpu-box 또는 ssh -i ~/.ssh/key.pem -p 2222 ubuntu@1.2.3.4",
                    ModalField::SshInput,
                    active_field,
                    cx,
                ))
                .when_some(preview, |el, res| match res {
                    Ok(summary) => el.child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x9ece6a))
                            .truncate()
                            .child(summary),
                    ),
                    Err(err) => el.child(div().text_xs().text_color(rgb(0xe06c75)).child(err)),
                })
                .child(Self::modal_text_input(
                    "m-label",
                    "표시 이름 (비우면 대상 이름 사용)",
                    &modal.label,
                    "예: gpu-box",
                    ModalField::Label,
                    active_field,
                    cx,
                )),
            MachineTab::Docker => div()
                .flex()
                .flex_col()
                .gap_2()
                .children(docker_chips)
                .child(Self::modal_text_input(
                    "m-docker-ctr",
                    "컨테이너 이름 (또는 docker exec 명령어)",
                    &modal.docker_container,
                    "예: ddg-dev 또는 docker exec -it ddg-dev bash",
                    ModalField::DockerContainer,
                    active_field,
                    cx,
                ))
                .child(Self::modal_text_input(
                    "m-docker-ssh",
                    "원격 SSH 호스트 (로컬 Docker면 비워두기)",
                    &modal.docker_ssh_host,
                    "예: dev@server",
                    ModalField::DockerSshHost,
                    active_field,
                    cx,
                ))
                .child(Self::modal_text_input(
                    "m-label",
                    "표시 이름 (비우면 컨테이너 이름 사용)",
                    &modal.label,
                    "예: ddg-dev",
                    ModalField::Label,
                    active_field,
                    cx,
                )),
            MachineTab::ContainerSsh => div()
                .flex()
                .flex_col()
                .gap_2()
                .child(Self::modal_text_input(
                    "m-ctr-host",
                    "호스트",
                    &modal.container_host,
                    "127.0.0.1",
                    ModalField::ContainerHost,
                    active_field,
                    cx,
                ))
                .child(Self::modal_text_input(
                    "m-ctr-port",
                    "포트",
                    &modal.port,
                    "2222",
                    ModalField::Port,
                    active_field,
                    cx,
                ))
                .child(Self::modal_text_input(
                    "m-ctr-user",
                    "사용자",
                    &modal.container_user,
                    "ddg",
                    ModalField::ContainerUser,
                    active_field,
                    cx,
                ))
                .child(Self::modal_text_input(
                    "m-label",
                    "표시 이름 (선택)",
                    &modal.label,
                    "예: dev-container",
                    ModalField::Label,
                    active_field,
                    cx,
                )),
        };

        let advanced_toggle = div()
            .id("m-adv-toggle")
            .group(ICON_HOVER_GROUP)
            .flex()
            .items_center()
            .gap_1()
            .text_xs()
            .text_color(rgb(0x9a9a9a))
            .hover(|s| s.text_color(rgb(0xffffff)))
            .cursor_pointer()
            .child(hover_icon(
                if modal.show_advanced {
                    Icon::ChevronDown
                } else {
                    Icon::ChevronRight
                },
                scale.px(12.),
                0x9a9a9a,
            ))
            .child("고급 옵션 (포트, 키, known_hosts)")
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(m) = this.machine_modal_mut() {
                    m.show_advanced = !m.show_advanced;
                }
                cx.notify();
            }));

        let advanced_fields = modal.show_advanced.then(|| {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .pl_2()
                .border_l_1()
                .border_color(rgb(0x3a3a3a))
                .when(modal.tab == MachineTab::Ssh, |el| {
                    el.child(Self::modal_text_input(
                        "m-adv-port",
                        "SSH 포트 (선택)",
                        &modal.port,
                        "22",
                        ModalField::Port,
                        active_field,
                        cx,
                    ))
                })
                .child(Self::modal_text_input(
                    "m-adv-id",
                    "IdentityFile (선택)",
                    &modal.identity_file,
                    "~/.ssh/id_ed25519",
                    ModalField::IdentityFile,
                    active_field,
                    cx,
                ))
                .child(Self::modal_text_input(
                    "m-adv-kh",
                    "UserKnownHostsFile (선택)",
                    &modal.known_hosts_file,
                    "~/.ssh/known_hosts",
                    ModalField::KnownHostsFile,
                    active_field,
                    cx,
                ))
        });

        let test_banner = match &modal.test_state {
            TestState::Idle => None,
            TestState::Running { .. } => Some(
                div()
                    .text_xs()
                    .text_color(rgb(0xe5c07b))
                    .child("연결 확인 중...")
                    .into_any_element(),
            ),
            TestState::Installing { stage, .. } => Some(
                div()
                    .p_2()
                    .rounded_sm()
                    .bg(rgb(0x2d2415))
                    .border_1()
                    .border_color(rgb(0x6b5225))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_xs()
                    .text_color(rgb(0xe5c07b))
                    .child("원격 자동 설치 진행 중...")
                    .child(div().text_color(rgb(0xe1d4b8)).child(stage.clone()))
                    .into_any_element(),
            ),
            TestState::Success { host_id } => {
                let short: String = host_id.chars().take(12).collect();
                Some(
                    div()
                        .text_xs()
                        .text_color(rgb(0x9ece6a))
                        .child(format!("연결 확인됨 (host_id: {short})"))
                        .into_any_element(),
                )
            }
            TestState::Failed {
                kind,
                message,
                copy_hint,
            } => {
                let headline = match kind {
                    DialErrorKind::HostKeyMismatch => {
                        "호스트 키 미등록 또는 불일치 (StrictHostKeyChecking=yes)"
                    }
                    DialErrorKind::AuthRequired => {
                        "SSH 인증 실패 — ssh-agent 또는 IdentityFile을 확인하세요"
                    }
                    DialErrorKind::HostIdMismatch => "연결된 host_id가 기존 값과 다릅니다",
                    DialErrorKind::SecurityBlocked => "호스트 ID를 고정해야 연결할 수 있습니다",
                    DialErrorKind::InvalidProfile => "입력 형식이 올바르지 않습니다",
                    DialErrorKind::HostNotInstalled => REMOTE_INSTALL_MISSING,
                    DialErrorKind::ProtocolIncompatible => "프로토콜 버전이 호환되지 않습니다",
                    DialErrorKind::Unavailable => {
                        "연결 실패 (원격 머신에 ddg-agents-host가 설치되어 있는지 확인하세요)"
                    }
                };
                let hint_opt = copy_hint.clone();
                let copied = modal.copied_hint;
                Some(
                    div()
                        .p_2()
                        .rounded_sm()
                        .bg(rgb(0x2a1b1d))
                        .border_1()
                        .border_color(rgb(0x6b3238))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .text_xs()
                        .child(div().text_color(rgb(0xf5c2c7)).child(headline))
                        .when(message.trim() != "DDG_HOST_NOT_INSTALLED", |el| {
                            el.child(div().text_color(rgb(0xc8a0a5)).child(message.clone()))
                        })
                        .when(
                            *kind == DialErrorKind::HostNotInstalled
                                && modal.supports_remote_install()
                                && !busy,
                            |el| {
                                el.child(
                                    div().flex().mt_1().child(
                                        div()
                                            .id("m-install-btn")
                                            .px_3()
                                            .py_1()
                                            .rounded_sm()
                                            .bg(rgb(0xe5c07b))
                                            .hover(|s| s.bg(rgb(0xf1d39a)))
                                            .text_color(rgb(0x191919))
                                            .cursor_pointer()
                                            .child(REMOTE_INSTALL_ACTION)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.run_remote_install(cx)
                                            })),
                                    ),
                                )
                            },
                        )
                        .when_some(hint_opt, |el, cmd| {
                            let cmd_copy = cmd.clone();
                            el.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .mt_1()
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .bg(rgb(0x181818))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_color(rgb(0xd0d0d0))
                                            .child(cmd),
                                    )
                                    .child(
                                        div()
                                            .id("copy-hint")
                                            .px_2()
                                            .py(px(scale.px(2.)))
                                            .rounded_sm()
                                            .bg(rgb(0x373737))
                                            .hover(|s| s.bg(rgb(0x505050)))
                                            .cursor_pointer()
                                            .child(if copied {
                                                "복사됨"
                                            } else {
                                                "명령 복사"
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    cmd_copy.clone(),
                                                ));
                                                if let Some(m) = this.machine_modal_mut() {
                                                    m.copied_hint = true;
                                                }
                                                cx.notify();
                                            })),
                                    ),
                            )
                        })
                        .into_any_element(),
                )
            }
        };

        let error_banner = modal
            .error
            .clone()
            .map(|e| div().text_xs().text_color(rgb(0xe06c75)).child(e));

        Some(deferred(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x000000aa))
                .occlude()
                .p_4()
                .child(
                    div()
                        .id("machine-modal-panel")
                        .w(px(scale.px(460.)))
                        .max_w_full()
                        .max_h_full()
                        .min_h_0()
                        .overflow_y_scroll()
                        .p_4()
                        .rounded_md()
                        .bg(rgb(0x252525))
                        .border_1()
                        .border_color(rgb(0x505050))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(div().text_base().child(if is_edit {
                                    "머신 설정"
                                } else {
                                    "머신 추가"
                                }))
                                .when(!installing, |el| {
                                    el.child(
                                        icon_box(px(scale.px(20.)))
                                            .id("m-close-x")
                                            .group(ICON_HOVER_GROUP)
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .hover(|s| s.bg(rgb(0x373737)))
                                            .child(hover_icon(Icon::X, scale.px(14.), 0x9a9a9a))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.dismiss_machine_modal(cx)
                                            })),
                                    )
                                }),
                        )
                        .child(tabs)
                        .child(fields_body)
                        .child(advanced_toggle)
                        .children(advanced_fields)
                        .children(test_banner)
                        .children(error_banner)
                        .when(is_edit, |el| {
                            el.child(div().text_xs().text_color(rgb(0x888888)).child(
                                "머신을 제거해도 원격 머신에서 실행 중인 세션은 종료되지 않습니다.",
                            ))
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .pt_1()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .id("m-test-btn")
                                                .px_3()
                                                .py_1()
                                                .rounded_sm()
                                                .bg(rgb(0x333333))
                                                .text_color(rgb(if busy {
                                                    0x9a9a9a
                                                } else {
                                                    0xf2f2f2
                                                }))
                                                .child("연결 테스트")
                                                .when(!busy, |el| {
                                                    el.cursor_pointer()
                                                        .hover(|s| s.bg(rgb(0x454545)))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.run_machine_check(false, cx);
                                                        }))
                                                }),
                                        )
                                        .when(is_edit && !busy, |el| {
                                            el.child(
                                                div()
                                                    .id("m-remove-btn")
                                                    .px_3()
                                                    .py_1()
                                                    .rounded_sm()
                                                    .cursor_pointer()
                                                    .bg(rgb(0x4a2226))
                                                    .hover(|s| s.bg(rgb(0x632b31)))
                                                    .text_color(rgb(0xf5c2c7))
                                                    .child("제거")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.remove_editing_machine(cx);
                                                    })),
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .when(!installing, |el| {
                                            el.child(
                                                div()
                                                    .id("m-cancel")
                                                    .px_3()
                                                    .py_1()
                                                    .rounded_sm()
                                                    .cursor_pointer()
                                                    .bg(rgb(0x373737))
                                                    .child("취소")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.dismiss_machine_modal(cx)
                                                    })),
                                            )
                                        })
                                        .child(
                                            div()
                                                .id("m-save")
                                                .px_3()
                                                .py_1()
                                                .rounded_sm()
                                                .bg(rgb(if busy { 0x333333 } else { 0xeeeeee }))
                                                .text_color(rgb(if busy {
                                                    0x9a9a9a
                                                } else {
                                                    0x191919
                                                }))
                                                .child(if is_edit { "저장" } else { "추가" })
                                                .when(!busy, |el| {
                                                    el.cursor_pointer().on_click(cx.listener(
                                                        |this, _, _, cx| {
                                                            this.save_machine_modal(cx);
                                                        },
                                                    ))
                                                }),
                                        ),
                                ),
                        ),
                ),
        ))
    }

    fn open_launcher(&mut self) {
        self.active_overlay =
            if matches!(self.active_overlay, Some(ActiveOverlay::AgentLauncher(_))) {
                None
            } else {
                let host = self.active_host_id.clone();
                Some(ActiveOverlay::AgentLauncher(AgentLauncher::new(
                    host.clone(),
                    self.launch_target(&host, 0).is_some(),
                )))
            };
    }

    fn launch_target(&self, host: &str, location: usize) -> Option<LaunchTarget> {
        match location {
            0 | 1 if host == self.active_host_id => {
                let room = self.room().filter(|r| r.tabs.len() < MAX_TABS_PER_ROOM)?;
                if location == 0 {
                    Some(LaunchTarget::Tab(room.id.clone()))
                } else if self.peek.is_none() && self.can_split() {
                    Some(LaunchTarget::Split(room.focused_tab_id()?.into()))
                } else {
                    None
                }
            }
            2 => Some(LaunchTarget::Room),
            3 => Some(LaunchTarget::Background),
            _ => None,
        }
    }

    fn launch_profile(&mut self, host: &str, location: usize, id: &str) {
        if self.agent_error.is_some() {
            return;
        }
        let Some(machine) = self.machine(host).filter(|m| m.is_connected()) else {
            self.notice = Some("연결된 대상 머신을 선택하세요".into());
            return;
        };
        let Some(target) = self.launch_target(host, location) else {
            return;
        };
        let Some(profile) = self.agents.find(id) else {
            return;
        };
        let remote = !matches!(machine.profile.transport, HostTransport::Local { .. });
        let resolved = profile.resolve_executor(Some(host), &[], None, remote);
        self.bridge.launch_agent(
            host,
            target,
            profile.id.clone(),
            resolved,
            self.viewport_grid,
        );
        self.active_overlay = None;
        self.focus_zone = KeyboardFocusZone::Terminal;
    }

    fn launch_from_picker(&mut self, index: usize) {
        let Some(ActiveOverlay::AgentLauncher(launcher)) = &self.active_overlay else {
            return;
        };
        let Some(profile) = launcher
            .matches(&self.agents)
            .get(index)
            .map(|p| p.id.clone())
        else {
            return;
        };
        let (host, location) = (launcher.host.clone(), launcher.target);
        self.launch_profile(&host, location, &profile);
    }

    fn launch_default(&mut self, location: usize) {
        let host = self.active_host_id.clone();
        let location = if self.launch_target(&host, location).is_some() {
            location
        } else {
            2
        };
        if self.agent_error.is_none()
            && let Some(id) = self.agents.default_agent_id.clone()
        {
            self.launch_profile(&host, location, &id);
        } else {
            self.open_launcher();
            if let Some(ActiveOverlay::AgentLauncher(launcher)) = &mut self.active_overlay {
                launcher.target = location;
            }
        }
    }

    fn overlay_panel(
        &self,
        label: &'static str,
        width: f32,
        content: impl IntoElement,
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .p_3()
            .bg(rgba(0x00000088))
            .child(
                div()
                    .w(px(width))
                    .max_w_full()
                    .max_h_full()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(0x424851))
                    .bg(rgb(0x202328))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(label))
                            .child(Self::action_button(
                                cx,
                                "close-agent-overlay",
                                icon(Icon::X, self.ui_scale.px(14.), rgb(0xe9e9e9)),
                                |this, _, _| this.active_overlay = None,
                            )),
                    )
                    .child(content),
            )
    }

    fn launcher_view(&self, cx: &mut Context<Self>) -> Option<Div> {
        let Some(ActiveOverlay::AgentLauncher(launcher)) = &self.active_overlay else {
            return None;
        };
        let mut content = div()
            .flex()
            .flex_col()
            .min_h_0()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .child("대상:")
                    .children(self.machines.iter().map(|m| {
                        let id = m.profile.id.clone();
                        Self::action_button(
                            cx,
                            stable_elem_id("launch-host", &[&id]),
                            m.profile.label.clone(),
                            move |this, _, _| {
                                let has_room = this.launch_target(&id, 0).is_some();
                                if let Some(ActiveOverlay::AgentLauncher(l)) =
                                    &mut this.active_overlay
                                {
                                    l.host.clone_from(&id);
                                    if !has_room && l.target < 2 {
                                        l.target = 2;
                                    }
                                }
                            },
                        )
                        .when(m.profile.id == launcher.host, |d| d.bg(rgb(0x30456b)))
                    })),
            )
            .child(
                div().flex().flex_wrap().gap_1().children(
                    ["현재 룸 새 탭", "분할", "새 룸", "백그라운드"]
                        .into_iter()
                        .enumerate()
                        .map(|(i, name)| {
                            let enabled = self.launch_target(&launcher.host, i).is_some();
                            Self::action_button(
                                cx,
                                ("launch-location", i),
                                name,
                                move |this, _, _| {
                                    if enabled
                                        && let Some(ActiveOverlay::AgentLauncher(l)) =
                                            &mut this.active_overlay
                                    {
                                        l.target = i;
                                    }
                                },
                            )
                            .when(!enabled, |d| d.text_color(rgb(0x858a92)).cursor_default())
                            .when(i == launcher.target && enabled, |d| d.bg(rgb(0x30456b)))
                        }),
                ),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(0x688ac1))
                    .child(if launcher.filter.is_empty() {
                        "이름 또는 명령어 필터…".into()
                    } else {
                        launcher.filter.clone()
                    }),
            );
        if let Some(error) = &self.agent_error {
            content = content.child(
                div()
                    .text_color(rgb(0xffad9a))
                    .child(format!("agents.json을 불러올 수 없습니다: {error}")),
            );
        } else if self.agents.agents.is_empty() {
            content = content.child("등록된 에이전트가 없습니다. 설정에서 추가하세요.");
        } else {
            let matches = launcher.matches(&self.agents);
            if matches.is_empty() {
                content = content.child("검색 결과가 없습니다.");
            }
            let mut rows = div()
                .id("launcher-rows")
                .overflow_y_scroll()
                .min_h_0()
                .max_h(px(self.ui_scale.px(275.)));
            for (i, profile) in matches.into_iter().enumerate() {
                let is_default = self.agents.default_agent_id.as_deref() == Some(&profile.id);
                let argv = profile
                    .host_overrides
                    .get(&launcher.host)
                    .and_then(|o| o.argv.as_ref())
                    .unwrap_or(&profile.argv);
                let programs = if self
                    .machine(&launcher.host)
                    .is_some_and(|m| matches!(m.profile.transport, HostTransport::Local { .. }))
                {
                    &self.programs
                } else {
                    &BUILTIN_PROGRAMS
                };
                let fallback = programs
                    .programs
                    .iter()
                    .find(|p| {
                        p.matchers.agent_profiles.contains(&profile.id)
                            || argv.first().is_some_and(|a| {
                                p.matchers
                                    .comms
                                    .iter()
                                    .any(|k| a.rsplit('/').next() == Some(k))
                            })
                    })
                    .map(|p| p.icon.as_str())
                    .unwrap_or_default();
                let label = div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(self.display_agent_icon(
                        &launcher.host,
                        profile.icon.as_deref().unwrap_or(fallback),
                        self.ui_scale.px(16.),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().child(format!(
                                "{}{}",
                                profile.label,
                                if is_default { " · 기본" } else { "" }
                            )))
                            .child(
                                div()
                                    .truncate()
                                    .text_color(rgb(0xb6bbc1))
                                    .child(agent_modal::argv_text(argv)),
                            ),
                    )
                    .when(i < 9, |d| {
                        d.child(format!(
                            "{}+{}",
                            if cfg!(target_os = "macos") {
                                "Cmd"
                            } else {
                                "Ctrl"
                            },
                            i + 1
                        ))
                    });
                rows = rows.child(
                    Self::action_button(cx, ("launch-agent", i), label, move |this, _, _| {
                        this.launch_from_picker(i)
                    })
                    .when(i == launcher.selected, |d| d.bg(rgb(0x30456b))),
                );
            }
            content = content.child(rows);
        }
        Some(self.overlay_panel(
            "에이전트 실행",
            self.ui_scale.px(505.),
            content.child(Self::action_button(
                cx,
                "launcher-settings",
                "에이전트 설정…",
                |this, _, _| this.open_agent_settings(),
            )),
            cx,
        ))
    }

    fn open_agent_settings(&mut self) {
        if let Ok(path) = AgentsConfig::default_path() {
            self.active_overlay = Some(ActiveOverlay::AgentSettingsModal(Box::new(
                AgentSettingsModal::load(path),
            )));
        } else {
            self.notice = Some("agents.json 설정 경로를 열 수 없습니다".into());
        }
    }

    fn agent_modal_mut(&mut self) -> Option<&mut AgentSettingsModal> {
        match &mut self.active_overlay {
            Some(ActiveOverlay::AgentSettingsModal(m)) => Some(m),
            _ => None,
        }
    }

    fn save_agent_settings(&mut self, overwrite: bool) {
        if let Some(config) = self.agent_modal_mut().and_then(|m| m.save(overwrite)) {
            self.agents = config;
            self.agent_error = None;
            self.active_overlay = None;
        }
    }

    fn reload_agent_settings(&mut self) {
        if let Some(m) = self.agent_modal_mut() {
            *m = AgentSettingsModal::load(m.path.clone());
        }
    }

    fn agent_field(
        modal: &AgentSettingsModal,
        field: agent_modal::Field,
        id: impl Into<ElementId>,
        label: &str,
        value: &str,
        masked: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let active = modal.field == field;
        let value = if masked && !value.is_empty() {
            "••••".into()
        } else {
            value.to_owned()
        };
        div()
            .flex()
            .flex_col()
            .min_w_0()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0xb6bbc1))
                    .child(label.to_owned()),
            )
            .child(
                Self::action_button(
                    cx,
                    id,
                    if value.is_empty() {
                        "—".into()
                    } else {
                        value
                    },
                    move |this, window, _| {
                        if let Some(m) = this.agent_modal_mut() {
                            m.field = field.clone();
                        }
                        this.focus.focus(window);
                    },
                )
                .min_w_0()
                .truncate()
                .border_1()
                .border_color(rgb(if active { 0x92b6ff } else { 0x424851 })),
            )
    }

    fn agent_settings_view(&self, cx: &mut Context<Self>) -> Option<Div> {
        use agent_modal::{Confirm, Field};
        let Some(ActiveOverlay::AgentSettingsModal(modal)) = &self.active_overlay else {
            return None;
        };
        let mut content = div()
            .id("agent-settings-scroll")
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .truncate()
                    .text_color(rgb(0xb6bbc1))
                    .child(modal.path.display().to_string()),
            );
        if let Some(error) = &modal.error {
            content = content.child(div().text_color(rgb(0xffad9a)).child(error.clone()));
        }
        if modal.blocked {
            let mut actions = div()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(Self::action_button(
                    cx,
                    "agent-reload",
                    "다시 불러오기",
                    |this, _, _| this.reload_agent_settings(),
                ))
                .child(Self::action_button(
                    cx,
                    "agent-path",
                    "경로 복사",
                    |this, _, cx| {
                        if let Some(m) = this.agent_modal_mut() {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                m.path.display().to_string(),
                            ));
                        }
                    },
                ));
            if let Some(backup) = &modal.recovery {
                content = content.child(
                    div()
                        .text_xs()
                        .child(format!("보호된 백업: {}", backup.display())),
                );
                actions = actions.child(Self::action_button(
                    cx,
                    "agent-reset",
                    if modal.confirm == Some(Confirm::Reset) {
                        "초기화 확인"
                    } else {
                        "백업 후 초기화"
                    },
                    |this, _, _| {
                        if let Some(m) = this.agent_modal_mut() {
                            if m.confirm == Some(Confirm::Reset) {
                                if m.reset() {
                                    this.agents = AgentsConfig::default();
                                    this.agent_error = None;
                                }
                            } else {
                                m.confirm = Some(Confirm::Reset);
                            }
                        }
                    },
                ));
            }
            if modal.confirm.is_some() {
                actions = actions.child(Self::action_button(
                    cx,
                    "agent-reset-cancel",
                    "취소",
                    |this, _, _| {
                        if let Some(m) = this.agent_modal_mut() {
                            m.confirm = None;
                        }
                    },
                ));
            }
            return Some(self.overlay_panel("에이전트 설정", 640., content.child(actions), cx));
        }
        if modal.conflict {
            content = content.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .bg(rgb(0x403421))
                    .child("agents.json이 외부에서 변경되었습니다. 현재 초안은 유지됩니다.")
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_1()
                            .child(Self::action_button(
                                cx,
                                "agent-load-external",
                                "외부 변경 불러오기",
                                |this, _, _| this.reload_agent_settings(),
                            ))
                            .child(Self::action_button(
                                cx,
                                "agent-overwrite",
                                "내 변경으로 덮어쓰기",
                                |this, _, _| this.save_agent_settings(true),
                            )),
                    ),
            );
        }
        let selector = div()
            .flex()
            .flex_col()
            .gap_1()
            .w(px(130.))
            .flex_none()
            .child(Self::action_button(
                cx,
                "agent-add-profile",
                "+ 추가",
                |this, _, _| {
                    if let Some(m) = this.agent_modal_mut() {
                        m.add();
                    }
                },
            ))
            .children(modal.config.agents.iter().enumerate().map(|(i, p)| {
                Self::action_button(
                    cx,
                    ("settings-profile", i),
                    div().truncate().child(format!(
                        "{}{}",
                        p.label,
                        if modal.config.default_agent_id.as_deref() == Some(&p.id) {
                            " · 기본"
                        } else {
                            ""
                        }
                    )),
                    move |this, _, _| {
                        if let Some(m) = this.agent_modal_mut()
                            && m.commit_fields()
                        {
                            m.select(i);
                        }
                    },
                )
                .when(i == modal.selected, |d| d.bg(rgb(0x30456b)))
            }));
        let mut fields = div().flex_1().min_w_0().flex().flex_col().gap_2();
        if let Some(profile) = modal.profile() {
            let selected_default = modal.config.default_agent_id.as_deref() == Some(&profile.id);
            fields = fields.child(div().flex().gap_2()
                .child(Self::agent_field(modal, Field::Id, "profile-id", "ID", &profile.id, false, cx).flex_1())
                .child(Self::agent_field(modal, Field::Name, "profile-name", "이름", &profile.label, false, cx).flex_1()))
                .child(Self::agent_field(modal, Field::Command, "profile-command", "실행 명령어 (셸 없이 argv 직접 실행)", &modal.command, false, cx))
                .child(div().flex().flex_wrap().gap_1().children(ddg_agents_transport::split_safe_tokens(&modal.command).unwrap_or_default()
                    .into_iter().map(|arg| div().px_1().rounded_sm().bg(rgb(0x343e4e)).text_xs().child(arg))))
                .child(div().flex().items_end().gap_2()
                    .child(Self::agent_field(modal, Field::Icon, "profile-icon", "아이콘 키 또는 로컬 파일명", profile.icon.as_deref().unwrap_or(""), false, cx).flex_1())
                    .child(self.display_agent_icon("local", profile.icon.as_deref().unwrap_or_default(), self.ui_scale.px(18.)))
                    .child(Self::action_button(cx, "profile-default", if selected_default { "기본 해제" } else { "기본으로 설정" }, move |this, _, _| {
                        if let Some(m) = this.agent_modal_mut() {
                            m.config.default_agent_id = if selected_default { None } else { m.profile().map(|p| p.id.clone()) };
                        }
                    })))
                .child(div().flex().items_center().justify_between().child("환경변수")
                    .child(Self::action_button(cx, "agent-reveal-env", if modal.reveal_env { "값 숨기기" } else { "값 보기" }, |this, _, _| {
                        if let Some(m) = this.agent_modal_mut() { m.reveal_env = !m.reveal_env; }
                    })))
                .child(div().text_xs().text_color(rgb(0xe5c07b)).child("비밀값 금지. DDG_*, LD_PRELOAD, LD_AUDIT, DYLD_INSERT_LIBRARIES, DYLD_FORCE_FLAT_NAMESPACE는 차단됩니다. 값은 호스트 카탈로그에 저장됩니다."));
            let mut keys: std::collections::BTreeSet<_> = profile.env.keys().cloned().collect();
            for (key, value) in &profile.env {
                let id = key.clone();
                fields = fields.child(
                    div()
                        .flex()
                        .items_end()
                        .gap_1()
                        .child(
                            Self::agent_field(
                                modal,
                                Field::EnvValue(key.clone()),
                                stable_elem_id("agent-env-value", &[key]),
                                key,
                                value,
                                !modal.reveal_env,
                                cx,
                            )
                            .flex_1(),
                        )
                        .child(Self::action_button(
                            cx,
                            stable_elem_id("agent-env-delete", &[key]),
                            "제거",
                            move |this, _, _| {
                                if let Some(profile) = this
                                    .agent_modal_mut()
                                    .and_then(AgentSettingsModal::profile_mut)
                                {
                                    profile.env.remove(&id);
                                }
                            },
                        )),
                );
            }
            fields = fields.child(
                div()
                    .flex()
                    .items_end()
                    .gap_1()
                    .child(
                        Self::agent_field(
                            modal,
                            Field::EnvKey,
                            "agent-env-key",
                            "변수 이름",
                            &modal.env_key,
                            false,
                            cx,
                        )
                        .flex_1(),
                    )
                    .child(Self::action_button(
                        cx,
                        "agent-env-add",
                        "+ 환경변수",
                        |this, _, _| {
                            if let Some(m) = this.agent_modal_mut()
                                && !m.env_key.is_empty()
                            {
                                let key = std::mem::take(&mut m.env_key);
                                if let Some(p) = m.profile_mut() {
                                    p.env.entry(key.clone()).or_default();
                                }
                                m.field = Field::EnvValue(key);
                            }
                        },
                    )),
            );
            let remotes: Vec<_> = self
                .machines
                .iter()
                .filter(|m| !matches!(m.profile.transport, HostTransport::Local { .. }))
                .collect();
            if !remotes.is_empty() {
                fields = fields.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_1()
                        .child("머신별 재정의")
                        .children(remotes.into_iter().map(|machine| {
                            let id = machine.profile.id.clone();
                            Self::action_button(
                                cx,
                                stable_elem_id("override-agent-host", &[&id]),
                                machine.profile.label.clone(),
                                move |this, _, _| {
                                    if let Some(m) = this.agent_modal_mut()
                                        && m.commit_fields()
                                    {
                                        m.host = if m.host.as_ref() == Some(&id) {
                                            None
                                        } else {
                                            Some(id.clone())
                                        };
                                        m.reload_host_command();
                                    }
                                },
                            )
                            .when(modal.host.as_ref() == Some(&machine.profile.id), |d| {
                                d.bg(rgb(0x30456b))
                            })
                        })),
                );
                if let Some(host) = &modal.host {
                    let over = profile.host_overrides.get(host);
                    fields = fields.child(Self::agent_field(
                        modal,
                        Field::HostCommand,
                        "override-argv",
                        "실행 명령어 (빈 값: 상속)",
                        &modal.host_command,
                        false,
                        cx,
                    ));
                    if let Some(over) = over {
                        keys.extend(over.env.keys().cloned());
                    }
                    for key in keys {
                        let value = over.and_then(|o| o.env.get(&key));
                        let state = match value {
                            None => 0,
                            Some(Some(_)) => 1,
                            Some(None) => 2,
                        };
                        let mut row = div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(div().text_xs().truncate().child(key.clone()));
                        if let Some(Some(value)) = value {
                            row = row.child(
                                Self::agent_field(
                                    modal,
                                    Field::HostValue(key.clone()),
                                    stable_elem_id("host-value", &[&key]),
                                    "값",
                                    value,
                                    !modal.reveal_env,
                                    cx,
                                )
                                .flex_1(),
                            );
                        }
                        fields = fields.child(
                            row.children(["상속", "재정의", "제거"].into_iter().enumerate().map(
                                |(i, name)| {
                                    let key = key.clone();
                                    Self::action_button(
                                        cx,
                                        stable_elem_id("host-env-state", &[&key, name]),
                                        name,
                                        move |this, _, _| {
                                            if let Some(m) = this.agent_modal_mut() {
                                                let inherited = m
                                                    .profile()
                                                    .and_then(|p| p.env.get(&key))
                                                    .cloned()
                                                    .unwrap_or_default();
                                                if let Some(o) = m.host_override() {
                                                    match i {
                                                        0 => {
                                                            o.env.remove(&key);
                                                        }
                                                        1 => {
                                                            o.env
                                                                .entry(key.clone())
                                                                .or_insert(None)
                                                                .get_or_insert(inherited);
                                                        }
                                                        _ => {
                                                            o.env.insert(key.clone(), None);
                                                        }
                                                    }
                                                }
                                                if i == 1 {
                                                    m.field = Field::HostValue(key.clone());
                                                }
                                            }
                                        },
                                    )
                                    .when(state == i, |d| d.bg(rgb(0x30456b)))
                                },
                            )),
                        );
                    }
                }
            }
        } else {
            fields = fields.child("등록된 에이전트가 없습니다. 추가 버튼으로 시작하세요.");
        }
        let mut footer = div().flex().items_center().gap_2();
        if modal.profile().is_some() {
            footer = footer.child(Self::action_button(
                cx,
                "agent-delete",
                if modal.confirm == Some(Confirm::Delete) {
                    "삭제 확인"
                } else {
                    "삭제"
                },
                |this, _, _| {
                    if let Some(m) = this.agent_modal_mut() {
                        if m.confirm == Some(Confirm::Delete) {
                            m.delete();
                        } else {
                            m.confirm = Some(Confirm::Delete);
                        }
                    }
                },
            ));
        }
        if modal.confirm.is_some() {
            footer = footer.child(Self::action_button(
                cx,
                "agent-confirm-cancel",
                "취소",
                |this, _, _| {
                    if let Some(m) = this.agent_modal_mut() {
                        m.confirm = None;
                    }
                },
            ));
        }
        footer = footer.child(
            Self::action_button(cx, "agent-save", "저장", |this, _, _| {
                this.save_agent_settings(false)
            })
            .ml_auto()
            .bg(rgb(0x30456b)),
        );
        Some(
            self.overlay_panel(
                "에이전트 설정",
                640.,
                content
                    .child(div().flex().gap_3().child(selector).child(fields))
                    .child(footer),
                cx,
            ),
        )
    }

    fn finish_rename(&mut self, save: bool, cx: &mut Context<Self>) {
        if let Some(ActiveOverlay::RenameModal(rk, name)) = self.active_overlay.take()
            && save
        {
            let trimmed = name.trim().to_owned();
            let opt = (!trimmed.is_empty()).then_some(trimmed);
            if let Some(room) = self
                .machine_mut(&rk.host_id)
                .and_then(|m| m.rooms.iter_mut().find(|r| r.id == rk.room_id))
            {
                room.name = opt.clone();
            }
            self.bridge.rename_room(&rk.host_id, rk.room_id, opt);
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn rename_modal(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let Some(ActiveOverlay::RenameModal(rk, name)) = &self.active_overlay else {
            return None;
        };
        let current_label = self
            .machine(&rk.host_id)?
            .rooms
            .iter()
            .find(|r| r.id == rk.room_id)?
            .label();
        Some(deferred(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x00000099))
                .occlude()
                .p_4()
                .child(
                    div()
                        .id("rename-modal-panel")
                        .w(px(self.ui_scale.px(360.)))
                        .max_w_full()
                        .max_h_full()
                        .min_h_0()
                        .overflow_y_scroll()
                        .p_4()
                        .rounded_md()
                        .bg(rgb(0x252525))
                        .border_1()
                        .border_color(rgb(0x505050))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(div().text_base().child("룸 이름 수정"))
                        .child(
                            div()
                                .text_color(rgb(0xbcbcbc))
                                .truncate()
                                .child(current_label),
                        )
                        .child(
                            div()
                                .px_2()
                                .py_2()
                                .rounded_sm()
                                .border_1()
                                .border_color(rgb(0x808080))
                                .bg(rgb(0x191919))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(format!(
                                    "{}▏",
                                    sanitize_display_text(name, DISPLAY_TEXT_LIMIT)
                                )),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(0xbcbcbc))
                                .child("빈 이름으로 저장하면 폴더 이름을 사용합니다."),
                        )
                        .child(
                            div().flex().justify_end().gap_2().children(
                                [(false, "취소"), (true, "저장")].into_iter().map(
                                    |(save, label)| {
                                        div()
                                            .id(("rename-action", usize::from(save)))
                                            .px_3()
                                            .py_1()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .bg(rgb(if save { 0xeeeeee } else { 0x373737 }))
                                            .text_color(rgb(if save { 0x191919 } else { 0xf2f2f2 }))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.finish_rename(save, cx)
                                            }))
                                            .child(label)
                                    },
                                ),
                            ),
                        ),
                ),
        ))
    }

    fn run_tab_action(&mut self, action: TabAction, window: &mut Window) {
        if self.pane_drag.is_some() {
            return;
        }
        match action {
            TabAction::NewRoom => self.new_room(window),
            TabAction::Add => self.add_tab(),
            TabAction::Close => self.close_focused_tab(),
            TabAction::SplitHorizontal => self.split_focused(SplitAxis::Horizontal),
            TabAction::SplitVertical => self.split_focused(SplitAxis::Vertical),
            TabAction::FocusDir(dir) => self.focus_direction(dir),
            TabAction::ToggleZoom => self.toggle_zoom(),
            TabAction::Unsplit => self.unsplit_focused(),
            TabAction::Previous | TabAction::Next | TabAction::Number(_) => {
                if let Some(room) = self.room_mut()
                    && let Some(index) =
                        action.target(room.selected_group, room.layout.groups.len())
                {
                    room.select_group(index);
                }
            }
        }
        self.ensure_selected_attached();
        self.focus.focus(window);
    }

    fn tab_bar_button(
        &self,
        id: &'static str,
        glyph: Icon,
        size: f32,
        action: TabAction,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let scale = self.ui_scale;
        icon_box(px(scale.px(20.)))
            .id(id)
            .rounded_sm()
            .child(if enabled {
                hover_icon(glyph, scale.px(size), 0xf2f2f2)
            } else {
                icon(glyph, scale.px(size), rgb(0x808080))
            })
            .when(enabled, |button| {
                button
                    .group(ICON_HOVER_GROUP)
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0x505050)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.run_tab_action(action, window);
                        cx.notify();
                    }))
            })
    }

    fn render_tab_controls(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let room = self.room()?;
        let scale = self.ui_scale;
        let tabs = room
            .layout
            .groups
            .iter()
            .enumerate()
            .map(|(i, group)| {
                let primary = first_leaf(group).to_owned();
                let active = i == room.selected_group;
                let label_id = if active {
                    room.focused_tab_id().unwrap_or(&primary)
                } else {
                    &primary
                };
                let tab = room.tabs.iter().find(|tab| tab.id == label_id);
                let title = tab.map_or_else(String::new, |tab| {
                    sanitize_display_text(&tab.title, DISPLAY_TEXT_LIMIT)
                });
                let tab_icon = self.active_machine().zip(tab).map(|(machine, tab)| {
                    self.display_agent_icon(
                        &self.active_host_id,
                        &self.program_icon_key(machine, tab),
                        scale.px(14.),
                    )
                });
                let tab_status = tab
                    .filter(|t| t.is_running())
                    .and_then(|t| t.activity.as_ref())
                    .and_then(|a| {
                        a.agent_status
                            .as_ref()
                            .map(|s| self.tab_status_badge(s.state, a.activity_seq > a.seen_seq))
                    });
                let mut ids = Vec::new();
                group.leaf_ids(&mut ids);
                let count = ids.len();

                div()
                    .id(stable_elem_id(
                        "terminal-tab",
                        &[&self.active_host_id, &primary],
                    ))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .bg(rgb(if active { 0x373737 } else { 0x191919 }))
                    .child((i + 1).to_string())
                    .children(tab_icon)
                    .when(!title.is_empty(), |el| {
                        el.child(div().max_w(px(scale.px(112.))).truncate().child(title))
                    })
                    .children(tab_status)
                    .when(count > 1, |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(rgb(0xc0c0c0))
                                .child(format!("· {count}")),
                        )
                    })
                    .when(active, |el| {
                        el.child(self.tab_bar_button(
                            "close-tab",
                            Icon::X,
                            11.,
                            TabAction::Close,
                            self.is_active_connected(),
                            cx,
                        ))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(room) = this.room_mut()
                            && let Some(index) = room
                                .layout
                                .groups
                                .iter()
                                .position(|group| group.contains(&primary))
                        {
                            room.select_group(index);
                            this.focus_zone = KeyboardFocusZone::Terminal;
                            this.ensure_selected_attached();
                            this.focus.focus(window);
                            cx.notify();
                        }
                    }))
            })
            .collect::<Vec<_>>();

        let connected = self.is_active_connected();
        let badge = self
            .external_input_indicator
            .as_ref()
            .and_then(|badge| badge.label_at(Instant::now(), self.active_tab_key()))
            .map(str::to_owned);
        Some(
            div()
                .id("tab-bar-controls")
                .flex()
                .flex_1()
                .min_w_0()
                .overflow_x_scroll()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .id("tab-scroll")
                        .flex()
                        .items_center()
                        .gap_1()
                        .min_w_0()
                        .overflow_x_scroll()
                        .children(tabs),
                )
                .child(Self::action_button(
                    cx,
                    "tab-agent-launch",
                    "에이전트 실행",
                    |this, _, _| this.open_launcher(),
                ))
                .child(self.tab_bar_button(
                    "add",
                    Icon::Plus,
                    13.,
                    TabAction::Add,
                    connected && room.tabs.len() < MAX_TABS_PER_ROOM,
                    cx,
                ))
                .when_some(badge, |bar, label| {
                    bar.child(
                        div()
                            .ml_auto()
                            .flex_shrink_0()
                            .mr_2()
                            .px_2()
                            .py_1()
                            .max_w(px(scale.px(160.)))
                            .truncate()
                            .rounded_sm()
                            .bg(rgb(0x28332b))
                            .text_color(rgb(0xc5e6cb))
                            .text_xs()
                            .child(label),
                    )
                }),
        )
    }

    fn zoom_badge_pct(&self) -> Option<u16> {
        let pct = self.ui_scale.percent();
        self.zoom_indicator
            .map(|(pct, _)| pct)
            .or((pct != UI_SCALE_DEFAULT).then_some(pct))
    }

    fn peek_banner(&self, cx: &mut Context<Self>) -> Option<Div> {
        let peek = self.peek.as_ref()?;
        let machine = self.machine(&peek.host);
        let connected = machine.is_some_and(MachineView::is_connected);
        let running = peek.tab.is_running();
        let control = peek.tab.mode == ControlMode::Control;
        let busy = peek.claim.as_ref().is_some_and(|c| !c.is_finished());
        let title = if peek.tab.title.is_empty() {
            &peek.tab.id
        } else {
            &peek.tab.title
        };
        let label = match (running, connected, control) {
            (false, _, _) => peek.tab.status.clone(),
            (_, false, _) => "연결 끊김".into(),
            (_, _, true) => "임시 제어 · Esc: 터미널".into(),
            _ => "읽기 전용 · Esc: 닫기".into(),
        };
        let current_room = self
            .room()
            .filter(|r| self.active_host_id == peek.host && r.tabs.len() < MAX_TABS_PER_ROOM)
            .map(|r| r.id.clone());
        let mut top = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().text_xs().child("BG"))
            .children(machine.map(|m| {
                self.display_agent_icon(
                    &peek.host,
                    &self.program_icon_key(m, &peek.tab),
                    self.ui_scale.px(15.),
                )
            }))
            .child(div().flex_1().min_w_0().truncate().child(format!(
                "{} · {}",
                sanitize_display_text(title, 80),
                machine.map_or(peek.host.as_str(), |m| &m.profile.label)
            )))
            .children(peek.tab.activity.as_ref().and_then(|a| {
                a.agent_status
                    .as_ref()
                    .map(|s| self.tab_status_badge(s.state, a.activity_seq > a.seen_seq))
            }))
            .child(
                div()
                    .text_xs()
                    .truncate()
                    .text_color(rgb(0xd0d6df))
                    .child(label),
            );
        let mut actions = div().flex().items_center().gap_1();
        if running && connected {
            if peek.tab.channel.is_some() && !control && !busy {
                actions = actions.child(Self::action_button(
                    cx,
                    "peek-claim",
                    "제어 요청",
                    |this, _, _| {
                        if let Some(peek) = &mut this.peek {
                            peek.claim = this.bridge.claim_peek(&peek.host, peek.tab.id.clone());
                        }
                    },
                ));
            }
            if control && peek.claim.is_some() {
                actions = actions.child(Self::action_button(
                    cx,
                    "peek-release",
                    "제어 반납",
                    |this, _, _| {
                        if let Some(peek) = &this.peek {
                            this.bridge
                                .close_peek(&peek.host, peek.tab.id.clone(), None, true);
                        }
                    },
                ));
            }
            if let Some(room_id) = current_room {
                actions = actions.child(Self::action_button(
                    cx,
                    "peek-move",
                    "현재 룸으로 이동",
                    move |this, _, _| {
                        if let Some(peek) = &this.peek {
                            this.bridge.move_tab(
                                &peek.host,
                                peek.tab.id.clone(),
                                SessionMoveTarget::ExistingRoom {
                                    room_id: room_id.clone(),
                                },
                            );
                        }
                    },
                ));
            }
            actions = actions.child(Self::action_button(
                cx,
                "peek-promote",
                "새 룸으로 승격",
                |this, _, _| {
                    if let Some(peek) = &this.peek {
                        this.bridge.move_tab(
                            &peek.host,
                            peek.tab.id.clone(),
                            SessionMoveTarget::NewRoom { name: None },
                        );
                    }
                },
            ));
        }
        top = top.child(Self::action_button(
            cx,
            "peek-close",
            "닫기",
            |this, _, _| {
                this.close_peek(false);
                this.ensure_selected_attached();
            },
        ));
        Some(
            div()
                .flex()
                .flex_col()
                .justify_center()
                .flex_none()
                .h(px(self.ui_scale.px(58.)))
                .px_2()
                .gap_1()
                .overflow_hidden()
                .bg(rgb(0x263343))
                .child(top)
                .child(actions),
        )
    }

    fn old_host_banner(&self, cx: &mut Context<Self>) -> Option<Div> {
        let host = self
            .peek
            .as_ref()
            .map_or(self.active_host_id.as_str(), |p| &p.host);
        let machine = self.machine(host).filter(|m| m.lacks_activity())?;
        let id = host.to_owned();
        let mut banner = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .px_2()
            .flex_none()
            .h(px(self.ui_scale.px(RESET_BANNER_HEIGHT)))
            .overflow_hidden()
            .text_xs()
            .bg(rgb(0x3a3020))
            .text_color(rgb(0xf0d5a4))
            .child(div().min_w_0().truncate().child(if machine.update_confirm {
                "같은 호스트 ID를 유지하며 원격 호스트를 업데이트할까요?"
            } else if matches!(machine.profile.transport, HostTransport::DockerExec { .. }) {
                "구버전 호스트: 에이전트 상태 미지원 · 컨테이너를 수동 업데이트하세요"
            } else {
                "구버전 호스트: 에이전트 상태 미지원"
            }));
        if let Some(stage) = &machine.installing_stage {
            banner = banner.child(div().truncate().child(stage.clone()));
        } else if matches!(
            machine.profile.transport,
            HostTransport::Ssh { .. } | HostTransport::ContainerSsh { .. }
        ) {
            let confirm = machine.update_confirm;
            banner = banner.child(Self::action_button(
                cx,
                "legacy-host-update",
                if confirm {
                    "업데이트 확인"
                } else {
                    "원격 업데이트"
                },
                move |this, _, cx| this.start_machine_install(&id, cx),
            ));
            if confirm {
                let id = host.to_owned();
                banner = banner.child(Self::action_button(
                    cx,
                    "legacy-host-update-cancel",
                    "취소",
                    move |this, _, _| {
                        if let Some(machine) = this.machine_mut(&id) {
                            machine.update_confirm = false;
                        }
                    },
                ));
            }
        }
        Some(banner)
    }

    fn render_terminal_panes(&mut self, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let (family, width, height) = self.font_grid;
        let scale = self.ui_scale;
        let inset = px(scale.px(PANE_INSET));
        let (panes, dividers) = self.pane_geometry();
        let peek = self.peek.is_some();
        let (host_id, focused) = self
            .active_terminal_target()
            .map_or((self.active_host_id.clone(), None), |(host, tab)| {
                (host.to_owned(), Some(tab.id.clone()))
            });
        let connected = self
            .machine(&host_id)
            .is_some_and(MachineView::is_connected);
        let show_cursor = self.focus.is_focused(window)
            && self.window_active
            && matches!(self.focus_zone, KeyboardFocusZone::Terminal)
            && self.active_overlay.is_none()
            && self.update_modal.is_none();
        let is_split = panes.len() > 1;
        let header_h = pane_header_height(scale, is_split);
        let mut terminal = div()
            .id("terminal")
            .debug_selector(|| "terminal".into())
            .flex_1()
            .min_h_0()
            .relative()
            .overflow_hidden();

        for (tab_id, rect) in panes {
            let size = scaled_pane_grid(pane_body_rect(rect, header_h), width, height, scale);
            let header = if is_split {
                self.machine(&host_id).and_then(|machine| {
                    machine
                        .rooms
                        .iter()
                        .flat_map(|r| &r.tabs)
                        .find(|tab| tab.id == tab_id)
                        .map(|tab| {
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .right_0()
                                .h(px(header_h))
                                .px_2()
                                .flex()
                                .items_center()
                                .gap_1()
                                .bg(rgb(0x252a31))
                                .font_family(".SystemUIFont")
                                .text_xs()
                                .child(self.display_agent_icon(
                                    &host_id,
                                    &self.program_icon_key(machine, tab),
                                    scale.px(14.),
                                ))
                                .child(div().flex_1().min_w_0().truncate().child(
                                    sanitize_display_text(
                                        if tab.title.is_empty() {
                                            &tab.id
                                        } else {
                                            &tab.title
                                        },
                                        100,
                                    ),
                                ))
                                .children(tab.activity.as_ref().and_then(|a| {
                                    a.agent_status.as_ref().map(|s| {
                                        self.tab_status_badge(s.state, a.activity_seq > a.seen_seq)
                                    })
                                }))
                        })
                })
            } else {
                None
            };
            let is_focused = focused.as_ref() == Some(&tab_id);
            let Some(tab) = self.find_tab_mut(&host_id, &tab_id) else {
                continue;
            };
            let running = tab.is_running();
            let readonly = running && tab.channel.is_some() && tab.mode == ControlMode::Readonly;
            let stopped = !running && !tab.closing;
            let status = tab.status.clone();
            let mut resize = None;
            if connected
                && running
                && !tab.closing
                && tab.mode == ControlMode::Control
                && let Some(ch) = tab.channel
                && tab.size != size
            {
                tab.size = size;
                if let Some(vt) = &mut tab.vt {
                    vt.resize(size.0, size.1);
                }
                resize = Some(ch);
            }
            let (lines, cursor) = if connected || peek {
                tab.vt.as_mut().map(Vt::frame).unwrap_or_default()
            } else {
                Default::default()
            };
            let cursor = cursor
                .filter(|_| is_focused && show_cursor && running && !readonly && !tab.closing);
            if let Some(ch) = resize {
                self.bridge.send_resize(&host_id, ch, size.0, size.1);
            }

            let click_id = tab_id.clone();
            let drop_id = tab_id.clone();
            let scroll_id = tab_id.clone();
            let scroll_host = host_id.clone();
            let button_id = tab_id.clone();
            let button_host = host_id.clone();
            let pane = pane_box(rect)
                .debug_selector(|| format!("pane-{tab_id}"))
                .id(stable_elem_id("terminal-pane", &[&host_id, &tab_id]))
                .overflow_hidden()
                .when(is_split, |d| {
                    d.border(inset)
                        .border_color(rgb(if is_focused { 0x5c8aff } else { 0x2b2b2b }))
                })
                .when(!is_split, |d| d.p(inset))
                .font_family(family)
                .text_size(px(scale.term_font_px()))
                .line_height(px(height))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        if this.focus_pane(&click_id, window) {
                            cx.notify();
                        }
                    }),
                )
                .drag_over::<ExternalPaths>(move |style, _, _, _| {
                    let style = if is_split { style } else { style.p_0() };
                    style.border(inset).border_color(rgb(0x6b9fff))
                })
                .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                    this.handle_file_drop(&drop_id, paths, window);
                    cx.notify();
                }))
                .on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, cx| {
                    let delta = f32::from(event.delta.pixel_delta(px(height)).y);
                    if delta != 0.
                        && let Some(vt) = this
                            .find_tab_mut(&scroll_host, &scroll_id)
                            .and_then(|tab| tab.vt.as_mut())
                    {
                        vt.scroll(-(delta / height).round() as isize);
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
                .children(header)
                .child(
                    div()
                        .absolute()
                        .top(px(header_h))
                        .bottom_0()
                        .left(inset)
                        .right(inset)
                        .overflow_hidden()
                        .children(lines.into_iter().map(|line| {
                            div().flex().flex_none().h(px(height)).children(
                                vt::style_runs(line).into_iter().map(|(cell, columns)| {
                                    div()
                                        .flex_none()
                                        .w(px(width * columns as f32))
                                        .h(px(height))
                                        .overflow_hidden()
                                        .text_color(rgb(cell.fg.unwrap_or(0xf2f2f2)))
                                        .when_some(cell.bg, |el, color| el.bg(rgb(color)))
                                        .whitespace_nowrap()
                                        .child(cell.text)
                                }),
                            )
                        }))
                        .when_some(cursor, |pane, (col, row)| {
                            pane.child(
                                div()
                                    .absolute()
                                    .left(px(f32::from(col) * width))
                                    .top(px((f32::from(row) + 1.) * height
                                        - scale.px(2.)
                                        - scale.px(1.)))
                                    .w(px(width))
                                    .h(px(scale.px(2.)))
                                    .bg(rgb(0xf2f2f2)),
                            )
                        })
                        .when(!peek && connected && (stopped || readonly), |pane| {
                            pane.child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .right_0()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_1()
                                    .px_2()
                                    .py_1()
                                    .bg(rgb(if readonly { 0x2d2415 } else { 0x222832 }))
                                    .font_family(".SystemUIFont")
                                    .text_xs()
                                    .line_height(px(scale.px(16.)))
                                    .child(div().min_w_0().truncate().child(if readonly {
                                        "읽기 전용".into()
                                    } else {
                                        status
                                    }))
                                    .child(
                                        div()
                                            .id(stable_elem_id(
                                                if readonly { "claim-control" } else { "restart" },
                                                &[&host_id, &tab_id],
                                            ))
                                            .flex_none()
                                            .px_2()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .whitespace_nowrap()
                                            .bg(rgb(if readonly { 0xe5c07b } else { 0x394354 }))
                                            .text_color(rgb(if readonly {
                                                0x191919
                                            } else {
                                                0xf2f2f2
                                            }))
                                            .hover(|s| {
                                                s.bg(rgb(if readonly {
                                                    0xf1d39a
                                                } else {
                                                    0x505b6b
                                                }))
                                            })
                                            .child(if readonly {
                                                "제어 요청"
                                            } else {
                                                "새 세션 시작"
                                            })
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                if readonly {
                                                    this.bridge.claim_control(
                                                        &button_host,
                                                        button_id.clone(),
                                                        true,
                                                    );
                                                } else {
                                                    this.bridge.restart_tab(
                                                        &button_host,
                                                        button_id.clone(),
                                                        size.0,
                                                        size.1,
                                                    );
                                                }
                                                this.focus_pane(&button_id, window);
                                                cx.notify();
                                            })),
                                    ),
                            )
                        }),
                );
            terminal = terminal.child(pane);
        }
        terminal.children(dividers.into_iter().enumerate().map(|(i, hit)| {
            pane_box(hit.rect)
                .id(("pane-divider", i))
                .bg(rgb(0x2b2b2b))
                .hover(|style| style.bg(rgb(0x5c8aff)))
                .cursor(match hit.axis {
                    SplitAxis::Horizontal => CursorStyle::ResizeColumn,
                    SplitAxis::Vertical => CursorStyle::ResizeRow,
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        this.begin_pane_drag(&hit, event.position);
                        cx.notify();
                    }),
                )
        }))
    }

    fn agent_list_key(&mut self, key: &Keystroke, window: &mut Window) -> bool {
        let rows = self.visible_agent_rows();
        let m = key.modifiers;
        self.agent_cursor = self.agent_cursor.min(rows.len().saturating_sub(1));
        let row = rows.get(self.agent_cursor);
        match (key.key.as_str(), m.alt) {
            ("escape", _) => self.focus_zone = KeyboardFocusZone::Terminal,
            ("up" | "down", true) => {
                self.keyboard_group = None;
                if let Some(row) = row {
                    self.move_agent(&row.entry.key, key.key == "down");
                }
            }
            ("up", false) => self.agent_cursor = self.agent_cursor.saturating_sub(1),
            ("down", false) => {
                self.agent_cursor = (self.agent_cursor + 1).min(rows.len().saturating_sub(1))
            }
            ("enter", false) => {
                if let Some(row) = row {
                    self.select_agent(&row.entry, window);
                }
            }
            ("p", false) => {
                if let Some(row) = row {
                    self.toggle_agent_pin(&row.entry.key);
                }
            }
            ("m", false) => self.cycle_agent_sort(),
            ("left", false) => {
                if let Some(row) = row
                    && self.ui_prefs.agent_sort == AgentSortMode::Grouped
                    && !row.pinned
                {
                    let group = format!("group:{}:{}", row.entry.host, row.entry.room);
                    self.keyboard_group = Some(group.clone());
                    self.toggle_section(&group);
                }
            }
            ("right", false) => {
                if let Some(group) = self.keyboard_group.take()
                    && self.section_collapsed(&group)
                {
                    self.toggle_section(&group);
                    if let Some(i) = self.visible_agent_rows().iter().position(|r| {
                        !r.pinned && group == format!("group:{}:{}", r.entry.host, r.entry.room)
                    }) {
                        self.agent_cursor = i;
                    }
                }
            }
            _ => return false,
        }
        true
    }

    fn jump_urgent(&mut self, window: &mut Window) {
        let mut rows = self.agent_rows();
        rows.retain(|r| r.entry.urgent() < 4);
        rows.sort_by_key(|r| r.entry.urgent());
        if rows.is_empty() {
            return;
        }
        let current = self.active_terminal_target().and_then(|(host, tab)| {
            rows.iter()
                .position(|r| r.entry.host == host && r.entry.tab == tab.id)
        });
        let index = current.map_or(0, |i| (i + 1) % rows.len());
        self.select_agent(&rows[index].entry, window);
    }

    fn route_key(
        zoom: Option<ZoomAction>,
        update: Option<bool>,
        machine: bool,
        rename: bool,
        reset: bool,
        key: &str,
    ) -> KeyRoute {
        if let Some(zoom) = zoom {
            KeyRoute::Zoom(zoom)
        } else if let Some(confirm) = update {
            match (key, confirm) {
                ("escape", true) => KeyRoute::UpdateCancel,
                ("escape", false) => KeyRoute::UpdateClose,
                _ => KeyRoute::UpdateTrap,
            }
        } else if machine {
            KeyRoute::Machine
        } else if rename {
            KeyRoute::Rename
        } else if reset && key == "escape" {
            KeyRoute::ResetNotice
        } else {
            KeyRoute::Terminal
        }
    }

    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        let m = key.modifiers;
        let reset = self.active_overlay.is_none()
            && self.peek.is_none()
            && self.active_room_id.is_none()
            && !matches!(self.focus_zone, KeyboardFocusZone::AgentList)
            && self
                .active_machine()
                .is_some_and(|machine| machine.reset_notice.is_some());
        let route = Self::route_key(
            zoom_action(&key.key, m, cfg!(target_os = "macos")),
            self.update_modal
                .as_ref()
                .map(UpdateModal::has_confirmation),
            self.machine_modal().is_some(),
            matches!(self.active_overlay, Some(ActiveOverlay::RenameModal(..))),
            reset,
            &key.key,
        );
        match route {
            KeyRoute::Zoom(action) => {
                cx.stop_propagation();
                self.apply_zoom_action(action, window, cx);
                return;
            }
            KeyRoute::UpdateCancel | KeyRoute::UpdateClose | KeyRoute::UpdateTrap => {
                cx.stop_propagation();
                if route == KeyRoute::UpdateClose {
                    self.close_update_modal(window, cx);
                } else if route == KeyRoute::UpdateCancel
                    && let Some(modal) = &mut self.update_modal
                {
                    modal.cancel_confirmation();
                    cx.notify();
                }
                return;
            }
            _ => {}
        }
        if route == KeyRoute::Machine {
            match key.key.as_str() {
                "escape" => {
                    self.dismiss_machine_modal(cx);
                    cx.stop_propagation();
                    return;
                }
                "enter" => {
                    cx.stop_propagation();
                    self.save_machine_modal(cx);
                    return;
                }
                "tab" => {
                    if let Some(modal) = self.machine_modal_mut() {
                        modal.cycle_field(m.shift);
                    }
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                "backspace" => {
                    if let Some(modal) = self.machine_modal_mut().filter(|m| !m.is_busy()) {
                        modal.active_value_mut().pop();
                        modal.error = None;
                        modal.test_state = TestState::Idle;
                    }
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                _ => {
                    let paste = (m.platform || m.control) && key.key == "v";
                    let text = if paste {
                        cx.read_from_clipboard().and_then(|item| item.text())
                    } else if !m.platform && !m.control {
                        key.key_char.clone()
                    } else {
                        None
                    };
                    if let Some(text) = text
                        && let Some(modal) = self.machine_modal_mut().filter(|m| !m.is_busy())
                    {
                        let target = modal.active_value_mut();
                        target.extend(
                            text.chars()
                                .filter(|c| !c.is_control())
                                .take(512usize.saturating_sub(target.chars().count())),
                        );
                        modal.error = None;
                        modal.test_state = TestState::Idle;
                    }
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
            }
        }
        if route == KeyRoute::Rename
            && let Some(ActiveOverlay::RenameModal(_, name)) = &mut self.active_overlay
        {
            match key.key.as_str() {
                "enter" | "escape" => return self.finish_rename(key.key == "enter", cx),
                "backspace" => {
                    name.pop();
                }
                _ => {
                    let text = if m.platform && key.key == "v" {
                        cx.read_from_clipboard().and_then(|item| item.text())
                    } else {
                        key.key_char.clone()
                    };
                    if let Some(text) = text {
                        name.extend(
                            text.chars()
                                .filter(|c| !c.is_control())
                                .take(80usize.saturating_sub(name.chars().count())),
                        );
                    }
                }
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let mac = cfg!(target_os = "macos");
        let primary = if mac {
            m.platform && !m.control
        } else {
            m.control && !m.platform
        };
        let global = primary && m.shift && !m.alt;
        if let Some(ActiveOverlay::AgentLauncher(launcher)) = &mut self.active_overlay {
            if key.key == "escape" || (global && key.key == "p") {
                self.active_overlay = None;
            } else if settings_key(&key.key, m, mac) {
                self.open_agent_settings();
            } else {
                let count = if self.agent_error.is_none() {
                    launcher.matches(&self.agents).len()
                } else {
                    0
                };
                if let Some(index) = launcher.key(key, count) {
                    self.launch_from_picker(index);
                }
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if let Some(ActiveOverlay::AgentSettingsModal(modal)) = &mut self.active_overlay {
            if key.key == "escape" {
                if modal.confirm.is_some() {
                    modal.confirm = None;
                } else {
                    self.active_overlay = None;
                }
            } else if !modal.blocked
                && key.key == "enter"
                && modal.confirm.is_none()
                && !modal.conflict
            {
                self.save_agent_settings(false);
            } else if !modal.blocked {
                let text = if primary && key.key == "v" {
                    cx.read_from_clipboard().and_then(|item| item.text())
                } else if !m.platform && !m.control {
                    key.key_char.clone()
                } else {
                    None
                };
                if key.key == "backspace" || text.is_some() {
                    modal.update_value(key.key == "backspace", text.as_deref());
                }
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if matches!(self.focus_zone, KeyboardFocusZone::AgentList)
            && !primary
            && !m.control
            && !m.platform
            && self.agent_list_key(key, window)
        {
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if settings_key(&key.key, m, mac)
            || global && matches!(key.key.as_str(), "p" | "l" | "a" | "t" | "n")
        {
            match key.key.as_str() {
                "p" => self.open_launcher(),
                "l" => {
                    self.focus_zone = match self.focus_zone {
                        KeyboardFocusZone::Terminal => {
                            if self.section_collapsed("section:agents") {
                                self.toggle_section("section:agents");
                            }
                            KeyboardFocusZone::AgentList
                        }
                        KeyboardFocusZone::AgentList => KeyboardFocusZone::Terminal,
                    };
                }
                "a" => self.jump_urgent(window),
                "t" => self.launch_default(0),
                "n" => self.launch_default(2),
                _ => self.open_agent_settings(),
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if let Some(peek) = &self.peek
            && peek_close_key(&key.key, primary && !m.shift && !m.alt, peek.tab.mode)
        {
            self.close_peek(false);
            self.ensure_selected_attached();
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if matches!(self.focus_zone, KeyboardFocusZone::AgentList) {
            return;
        }
        if route == KeyRoute::ResetNotice
            && let Some(machine) = self
                .machines
                .iter_mut()
                .find(|m| m.profile.id == self.active_host_id)
            && let Some(notice) = &machine.reset_notice
        {
            machine.dismiss_reset_notice(&notice.notice_id.clone());
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.peek.is_none()
            && let Some(action) = tab_action(&key.key, m, mac)
        {
            self.run_tab_action(action, window);
        } else if self.focus.is_focused(window)
            && let Some((host, tab)) = self.active_terminal_target()
            && self.machine(host).is_some_and(MachineView::is_connected)
        {
            let (hid, id) = (host.to_owned(), tab.id.clone());
            let mut to_send: Option<(u32, Vec<u8>)> = None;
            if let Some(tab) = self.find_tab_mut(&hid, &id)
                && tab.is_running()
                && !tab.closing
                && tab.mode == ControlMode::Control
                && let Some(ch) = tab.channel
            {
                if m.platform && key.key == "v" {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        to_send = Some((ch, text.into_bytes()));
                    }
                } else if !m.platform
                    && let Some(vt) = &mut tab.vt
                {
                    let bytes = if !m.control
                        && !m.alt
                        && key
                            .key_char
                            .as_ref()
                            .is_some_and(|s| !s.chars().any(char::is_control))
                    {
                        key.key_char.clone().map(String::into_bytes)
                    } else {
                        vt.key(&key.key, m.shift, m.control, m.alt)
                    };
                    if let Some(bytes) = bytes {
                        to_send = Some((ch, bytes));
                    }
                }
            }
            if let Some((ch, bytes)) = to_send {
                self.bridge.send_input(&hid, ch, &bytes);
            }
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }
}

fn sidebar_max_px(viewport_width: f32) -> f32 {
    (viewport_width * 0.55).round()
}

fn sidebar_px(base: f32, scale: UiScale, viewport_width: f32) -> f32 {
    let width = scale
        .px(base)
        .clamp(scale.px(SIDEBAR_MIN), scale.px(SIDEBAR_MAX));
    // The viewport cap takes precedence over the scaled minimum in narrow windows.
    width.min(sidebar_max_px(viewport_width))
}

fn pane_header_height(scale: UiScale, split: bool) -> f32 {
    if split { scale.px(22.) } else { 0. }
}

fn pane_body_rect(rect: PaneRect, header_h: f32) -> PaneRect {
    PaneRect {
        y: rect.y + header_h,
        h: (rect.h - header_h).max(0.),
        ..rect
    }
}

fn scaled_pane_grid(rect: PaneRect, width: f32, height: f32, scale: UiScale) -> (u16, u16) {
    // pane_grid already subtracts the two base insets.
    let extra_inset = 2. * (scale.px(PANE_INSET) - PANE_INSET);
    pane_grid(
        PaneRect {
            w: rect.w - extra_inset,
            h: rect.h - extra_inset,
            ..rect
        },
        width,
        height,
    )
}

fn pane_box(rect: PaneRect) -> Div {
    div()
        .absolute()
        .left(px(rect.x))
        .top(px(rect.y))
        .w(px(rect.w))
        .h(px(rect.h))
}

impl Render for Desktop {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(scale) = self.pending_ui_scale.take() {
            self.apply_ui_scale(scale, window);
        }
        let viewport = window.viewport_size();
        let viewport_width = f32::from(viewport.width);
        let scale = self.ui_scale;
        let sidebar = sidebar_px(self.sidebar_base, scale, viewport_width);
        let divider = scale
            .px(DIVIDER_WIDTH)
            .min((viewport_width - sidebar - 1.).max(0.));
        let tab_bar_height = TAB_BAR_HEIGHT_REM * scale.rem_px();
        let reset_banner_height = scale.px(RESET_BANNER_HEIGHT);
        let (_, width, height) = self.font_grid;

        let protocol_card = self.active_machine().and_then(BlockedCard::for_machine);
        let update_banner = self.update_banner_view(protocol_card.is_some(), cx);
        let blocked_msg = self.active_machine().and_then(|m| match &m.conn_state {
            ConnectionState::HostKeyMismatch(msg) => Some(format!(
                "보안 경고: 원격 호스트 키가 일치하지 않아 연결을 차단했습니다.\n{msg}"
            )),
            ConnectionState::AuthRequired(msg) => Some(format!("SSH 인증이 필요합니다.\n{msg}")),
            ConnectionState::ProtocolIncompatible { .. } => protocol_card
                .as_ref()
                .map(|BlockedCard::ProtocolIncompatible { title, .. }| title.clone()),
            ConnectionState::HostNotInstalled(msg) => {
                Some(if msg.trim() == "DDG_HOST_NOT_INSTALLED" {
                    REMOTE_INSTALL_MISSING.to_owned()
                } else {
                    format!(
                        "{REMOTE_INSTALL_MISSING}\n{}",
                        sanitize_display_text(msg, 1024)
                    )
                })
            }
            _ => None,
        });
        let install_target = self
            .active_machine()
            .filter(|m| {
                matches!(m.conn_state, ConnectionState::HostNotInstalled(_))
                    && matches!(
                        m.profile.transport,
                        HostTransport::Ssh { .. } | HostTransport::ContainerSsh { .. }
                    )
            })
            .map(|m| {
                (
                    m.profile.id.clone(),
                    m.installing_stage.clone(),
                    m.update_confirm,
                )
            });

        let has_active_room = self.room().is_some();
        let peek_banner = self.peek_banner(cx);
        let old_host_banner = self.old_host_banner(cx);
        let peeking = peek_banner.is_some();
        let show_tabs = !peeking && blocked_msg.is_none() && has_active_room;
        let show_empty =
            !peeking && blocked_msg.is_none() && self.is_active_connected() && !has_active_room;

        let reset_banner = self
            .active_machine()
            .filter(|_| !peeking)
            .and_then(|machine| {
                let notice = machine.reset_notice.as_ref()?;
                let host_id = machine.profile.id.clone();
                let notice_id = notice.notice_id.clone();
                Some(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap_2()
                        .h(px(reset_banner_height))
                        .overflow_hidden()
                        .px_3()
                        .bg(rgb(0x222832))
                        .text_color(rgb(0xdbe5f6))
                        .text_xs()
                        .child(div().flex_1().min_w_0().truncate().child(format!(
                            "기존 v1 카탈로그를 백업했습니다 ({})",
                            reset_backup_label(notice)
                        )))
                        .child(
                            icon_box(px(scale.px(20.)))
                                .id(stable_elem_id("dismiss-reset", &[&host_id, &notice_id]))
                                .group(ICON_HOVER_GROUP)
                                .rounded_sm()
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(0x394354)))
                                .child(hover_icon(Icon::X, scale.px(12.), 0xdbe5f6))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(machine) = this.machine_mut(&host_id) {
                                        machine.dismiss_reset_notice(&notice_id);
                                    }
                                    cx.notify();
                                })),
                        ),
                )
            });
        let zoom_badge_pct = self.zoom_badge_pct();
        let show_tab_bar = show_tabs || zoom_badge_pct.is_some();
        // GPUI 0.2.2 snaps the tab bar's layout bounds to whole pixels.
        let top = if show_tab_bar {
            tab_bar_height.round()
        } else {
            0.
        } + if peeking { scale.px(58.) } else { 0. }
            + if old_host_banner.is_some() {
                reset_banner_height
            } else {
                0.
            }
            + if reset_banner.is_some() {
                reset_banner_height
            } else {
                0.
            };
        let top = top
            + if update_banner.is_some() {
                scale.px(UPDATE_BANNER_HEIGHT)
            } else {
                0.
            };
        self.canvas = PaneRect {
            x: sidebar + divider,
            y: top,
            w: (viewport_width - sidebar - divider).max(0.),
            h: (f32::from(viewport.height) - top).max(0.),
        };
        self.viewport_grid = scaled_pane_grid(self.canvas, width, height, scale);
        let tab_bar = show_tab_bar.then(|| {
            let controls = if show_tabs {
                self.render_tab_controls(cx)
            } else {
                None
            };
            div()
                .debug_selector(|| "tab-bar".into())
                .flex()
                .flex_none()
                .items_center()
                .gap_1()
                .h(px(tab_bar_height))
                .children(controls)
                .when_some(zoom_badge_pct, |bar, pct| {
                    bar.child(
                        div()
                            .id("global-ui-zoom-reset")
                            .ml_auto()
                            .mr_2()
                            .px_2()
                            .py_1()
                            .flex_none()
                            .whitespace_nowrap()
                            .text_xs()
                            .text_color(rgb(0xbcbcbc))
                            .rounded_sm()
                            .bg(rgb(0x262626))
                            .hover(|s| s.bg(rgb(0x373737)).text_color(rgb(0xf2f2f2)))
                            .cursor_pointer()
                            .child(format!("{pct}%"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                cx.stop_propagation();
                                this.apply_zoom_action(ZoomAction::Reset, window, cx);
                            })),
                    )
                })
        });
        let terminal = (peeking || blocked_msg.is_none() && !show_empty)
            .then(|| self.render_terminal_panes(window, cx));

        div()
            .size_full()
            .relative()
            .flex()
            .bg(rgb(0x191919))
            .text_color(rgb(0xf2f2f2))
            .text_sm()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key))
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                if event.pressed_button == Some(MouseButton::Left) {
                    if this.pane_drag.is_some() {
                        if this.move_pane_divider(event.position) {
                            cx.notify();
                        }
                    } else if let Some(offset) = this.sidebar_drag {
                        let new_px = (f32::from(event.position.x) - offset)
                            .min(sidebar_max_px(viewport_width));
                        this.sidebar_base =
                            (new_px / this.ui_scale.factor()).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
                        cx.notify();
                    }
                } else {
                    this.sidebar_drag = None;
                    if this.finish_pane_drag() {
                        cx.notify();
                    }
                }
            }))
            .capture_any_mouse_up(cx.listener(Self::end_resize_drag))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::end_resize_drag))
            .child(
                div()
                    .debug_selector(|| "sidebar".into())
                    .flex_none()
                    .w(px(sidebar))
                    .min_w_0()
                    .overflow_hidden()
                    .when(sidebar >= scale.px(24.), |el| el.p_3())
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(scale.px(10.)))
                            .child(
                                img(self.icon.clone())
                                    .size(px(scale.px(16.)))
                                    .rounded(px(scale.px(3.))),
                            )
                            .child("Ddg"),
                    )
                    .child(self.agent_sidebar(sidebar, f32::from(viewport.height), cx))
                    .child(self.room_sidebar(sidebar, cx))
                    .when_some(self.notice.clone(), |el, msg| {
                        el.child(div().text_xs().text_color(rgb(0xe5c07b)).child(msg))
                    })
                    .child(
                        div()
                            .flex_none()
                            .pt_2()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_1()
                            .border_t_1()
                            .border_color(rgb(0x2b2b2b))
                            .child(
                                div()
                                    .id("add-machine-btn")
                                    .group(ICON_HOVER_GROUP)
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_color(rgb(0x9a9a9a))
                                    .hover(|s| s.bg(rgb(0x262626)).text_color(rgb(0xffffff)))
                                    .child(hover_icon(Icon::Server, scale.px(14.), 0x9a9a9a))
                                    .child(if sidebar <= scale.px(185.) {
                                        "+ 머신"
                                    } else {
                                        "머신 추가"
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_add_machine(window);
                                        cx.notify();
                                    })),
                            )
                            .child(Self::action_button(
                                cx,
                                "agent-settings",
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(icon(Icon::Settings, scale.px(14.), rgb(0xdddddd)))
                                    .when(sidebar > scale.px(185.), |d| d.child("에이전트 설정")),
                                |this, _, _| this.open_agent_settings(),
                            )),
                    )
                    .child(self.update_sidebar_row(sidebar, cx)),
            )
            .child(
                div()
                    .w(px(divider))
                    .flex_none()
                    .bg(rgb(0x353535))
                    .cursor_col_resize()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, _| {
                            this.sidebar_drag = Some(f32::from(event.position.x) - sidebar);
                        }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .bg(rgb(0x111513))
                    .children(update_banner)
                    .children(tab_bar)
                    .children(reset_banner)
                    .children(old_host_banner)
                    .children(peek_banner)
                    .when_some(blocked_msg.filter(|_| !peeking), |pane, msg| {
                        pane.child(
                            div()
                                .flex_1()
                                .flex()
                                .items_center()
                                .justify_center()
                                .p_6()
                                .child(
                                    div()
                                        .max_w(px(scale.px(480.)))
                                        .p_4()
                                        .rounded_md()
                                        .border_1()
                                        .flex()
                                        .flex_col()
                                        .items_start()
                                        .gap_2()
                                        .bg(rgb(if install_target.is_some() {
                                            0x2d2415
                                        } else {
                                            0x2c1a1c
                                        }))
                                        .border_color(rgb(if install_target.is_some() {
                                            0x6b5225
                                        } else {
                                            0xe06c75
                                        }))
                                        .text_color(rgb(if install_target.is_some() {
                                            0xe1d4b8
                                        } else {
                                            0xf5c2c7
                                        }))
                                        .child(div().child(msg))
                                        .when_some(
                                            protocol_card,
                                            |card,
                                             BlockedCard::ProtocolIncompatible {
                                                 action,
                                                 guide,
                                                 local_old,
                                                 ..
                                             }| {
                                                card.child(Self::action_button(
                                                    cx,
                                                    "protocol-update",
                                                    action,
                                                    move |this, window, cx| {
                                                        this.open_update_modal(guide, window, cx);
                                                        if local_old && this.update_modal.is_some()
                                                        {
                                                            this.check_updates(false);
                                                        }
                                                    },
                                                ))
                                            },
                                        )
                                        .when_some(install_target, |card, (id, stage, confirm)| {
                                            if let Some(stage) = stage {
                                                card.child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(rgb(0xe5c07b))
                                                        .child(stage),
                                                )
                                            } else {
                                                card.child(
                                                    div()
                                                        .id("machine-install-btn")
                                                        .px_3()
                                                        .py_1()
                                                        .rounded_sm()
                                                        .bg(rgb(0xe5c07b))
                                                        .hover(|s| s.bg(rgb(0xf1d39a)))
                                                        .text_color(rgb(0x191919))
                                                        .cursor_pointer()
                                                        .child(if confirm {
                                                            "원격 설치 확인"
                                                        } else {
                                                            REMOTE_INSTALL_ACTION
                                                        })
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                this.start_machine_install(&id, cx)
                                                            },
                                                        )),
                                                )
                                            }
                                        }),
                                ),
                        )
                    })
                    .when(show_empty, |pane| {
                        pane.child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .items_center()
                                .justify_center()
                                .gap_3()
                                .child(div().text_color(rgb(0xa0a0a0)).child("열린 룸이 없습니다."))
                                .child(Self::action_button(
                                    cx,
                                    "empty-agent",
                                    "기본 에이전트 시작",
                                    |this, _, _| this.launch_default(2),
                                ))
                                .child(
                                    div()
                                        .id("empty-new-room")
                                        .group(ICON_HOVER_GROUP)
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .px_3()
                                        .py_1()
                                        .rounded_sm()
                                        .bg(rgb(0x373737))
                                        .hover(|s| s.bg(rgb(0x505050)))
                                        .cursor_pointer()
                                        .child(hover_icon(Icon::Plus, scale.px(13.), 0xf2f2f2))
                                        .child("새 룸 만들기")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.new_room(window);
                                            cx.notify();
                                        })),
                                ),
                        )
                    })
                    .children(terminal),
            )
            .children(self.rename_modal(cx))
            .children(self.machine_modal_view(cx))
            .children(self.launcher_view(cx))
            .children(self.agent_settings_view(cx))
            .children(self.update_modal_view(cx))
    }
}

fn font_grid(window: &Window, scale: UiScale) -> (&'static str, f32, f32) {
    let ts = window.text_system();
    let family = if ts.all_font_names().iter().any(|name| name == "MesloLGS NF") {
        "MesloLGS NF"
    } else {
        "Menlo"
    };
    let font_size = px(scale.term_font_px());
    let id = ts.resolve_font(&font(family));
    let width = ts
        .advance(id, font_size, 'M')
        .map(|size| f32::from(size.width))
        .unwrap_or(8.43 * scale.term_font_px() / BASE_TERM_FONT_PX);
    let height =
        (1.15 * f32::from(ts.ascent(id, font_size) + ts.descent(id, font_size).abs())).ceil();
    (family, width, height)
}

#[cfg(target_os = "macos")]
actions!(ddg, [Quit, CheckUpdates]);

fn main() {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--version")
    {
        println!("ddg-agents-native {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if std::env::args().any(|arg| arg == "--check-vt") {
        Vt::new(80, 24).expect("initialize Ghostty");
        println!("libghostty-vt initialized");
        return;
    }
    let hosts_path = HostsConfig::default_path().ok();
    let hosts = hosts_path
        .as_deref()
        .map(HostsConfig::load_or_default)
        .unwrap_or_default();

    let assets = AppAssets::default();
    let app = Application::new().with_assets(assets.clone());
    app.run(move |cx: &mut App| {
        let _main_window = cx
            .open_window(Default::default(), move |window, cx| {
                window.set_window_title("Ddg");
                cx.new(|cx| {
                    let mut this = Desktop::new(hosts_path, hosts, assets, window, cx);
                    match this.update.load() {
                        Ok(()) => this.check_updates(true),
                        Err(error) => {
                            this.update.phase = UpdatePhase::Error(update_controller::card(&error))
                        }
                    }
                    let desktop = cx.weak_entity();
                    window.on_window_should_close(cx, move |_, cx| {
                        desktop
                            .update(cx, |this, cx| {
                                let hold = this.hold_update_quit();
                                if hold {
                                    cx.notify();
                                }
                                !hold
                            })
                            .unwrap_or(true)
                    });
                    let applying = this.update.applying_guard.clone();
                    cx.on_app_quit(move |desktop, _| {
                        if !desktop.update.quitting_for_update_restart {
                            // GPUI only polls shutdown futures for 100 ms. Wait before returning one.
                            let deadline = Instant::now() + Duration::from_secs(5);
                            while applying.load(std::sync::atomic::Ordering::Acquire)
                                && Instant::now() < deadline
                            {
                                std::thread::sleep(Duration::from_millis(25));
                            }
                        }
                        std::future::ready(())
                    })
                    .detach();
                    cx.spawn(async |this, cx| {
                        loop {
                            cx.background_executor()
                                .timer(Duration::from_millis(16))
                                .await;
                            if this
                                .update(cx, |this: &mut Desktop, cx| {
                                    if this.drain(cx) {
                                        cx.notify();
                                    }
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    })
                    .detach();
                    this
                })
            })
            .expect("open DDG Agents window");
        #[cfg(target_os = "macos")]
        {
            let desktop = _main_window.entity(cx).expect("desktop root").downgrade();
            let quit_desktop = desktop.clone();
            cx.on_action(move |_: &Quit, cx| {
                if !quit_desktop
                    .update(cx, |this, cx| {
                        let hold = this.hold_update_quit();
                        if hold {
                            cx.notify();
                        }
                        hold
                    })
                    .unwrap_or(false)
                {
                    cx.quit();
                }
            });
            cx.on_action(move |_: &CheckUpdates, cx| {
                let _ = cx.update_window(_main_window.into(), |_, window, cx| {
                    let _ = desktop.update(cx, |this, cx| {
                        this.open_update_modal(false, window, cx);
                        if this.update_modal.is_some() {
                            this.check_updates(false);
                        }
                    });
                });
            });
            cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
            cx.set_menus(vec![Menu {
                name: "Ddg".into(),
                items: vec![
                    MenuItem::action("업데이트 확인...", CheckUpdates),
                    MenuItem::separator(),
                    MenuItem::action("Ddg 종료", Quit),
                ],
            }]);
        }
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::{
        Catalog, ConnectionState, ControlMode, ExternalInputIndicator, HostProfile, HostTransport,
        HostsConfig, HostsConfigChanges, Instant, MachineView, Modifiers, NavDir, PaneDrag,
        PathBuf, ResetNotice, RoomId, RoomKey, RoomKind, RoomView, RoomVisibility, TabAction,
        TabFrame, TabRecord, TabRuntimeState, TabStreamHandle, TabView, Vt, ZoomAction,
        accept_tab_attach, format_dropped_paths, ratio_at, reconcile_hosts_config,
        reset_backup_label, set_ratio_at, stable_elem_id, sync_machine_rooms, tab_action,
        zoom_action,
    };
    use ddg_agents_proto::frame::MAX_PENDING;
    use ddg_agents_proto::host::{ExecutorSpec, PaneNode, RoomLayout, RoomRecord, SplitAxis};
    use std::time::Duration;

    #[test]
    fn key_routing_update_modal_priority_and_enter_trap() {
        use super::{Desktop, KeyRoute};
        let zoom = Some(ZoomAction::Reset);
        assert_eq!(
            Desktop::route_key(zoom, Some(true), true, true, true, "escape"),
            KeyRoute::Zoom(ZoomAction::Reset)
        );
        assert_eq!(
            Desktop::route_key(None, Some(true), true, true, true, "enter"),
            KeyRoute::UpdateTrap
        );
        assert_eq!(
            Desktop::route_key(None, Some(true), true, true, true, "escape"),
            KeyRoute::UpdateCancel
        );
        assert_eq!(
            Desktop::route_key(None, Some(false), true, true, true, "escape"),
            KeyRoute::UpdateClose
        );
        assert_eq!(
            Desktop::route_key(None, None, true, true, true, "escape"),
            KeyRoute::Machine
        );
        assert_eq!(
            Desktop::route_key(None, None, false, true, true, "escape"),
            KeyRoute::Rename
        );
        assert_eq!(
            Desktop::route_key(None, None, false, false, true, "escape"),
            KeyRoute::ResetNotice
        );
        assert_eq!(
            Desktop::route_key(None, None, false, false, false, "escape"),
            KeyRoute::Terminal
        );
    }

    fn tab_rec(id: u32, running: bool) -> TabRecord {
        TabRecord {
            id: format!("ses_{id:026}"),
            run_generation: 1,
            parent_session_id: None,
            parent_host_id: None,
            agent_profile: None,
            space_cwd: None,
            last_cwd: None,
            title: String::new(),
            executor: ExecutorSpec::DefaultShell,
            pty_id: running.then_some(id),
            runtime: if running {
                TabRuntimeState::Running {
                    pty_id: id,
                    pid: 1000 + id,
                }
            } else {
                TabRuntimeState::StoppedAfterReboot
            },
        }
    }

    fn room_rec(id: u32, tabs: Vec<TabRecord>) -> RoomRecord {
        RoomRecord {
            id: format!("rm_{id:026}"),
            kind: RoomKind::Standard,
            visibility: RoomVisibility::Visible,
            name: None,
            auto_name: format!("룸 {id}"),
            tabs,
            layout: None,
        }
    }

    mod headless {
        use super::super::{
            AppAssets, Desktop, HostBridge, HostsConfig, RoomView, TAB_BAR_HEIGHT_REM, TabView,
            UiScale,
        };
        use super::split_room_rec;
        use gpui::{Entity, TestAppContext, VisualTestContext, px, size};

        const LEFT: &str = "pane-ses_00000000000000000000000002";
        const RIGHT: &str = "pane-ses_00000000000000000000000003";

        fn desktop(cx: &mut TestAppContext) -> (Entity<Desktop>, &mut VisualTestContext) {
            cx.add_window_view(|window, cx| {
                let mut desktop = Desktop::from_state(
                    HostsConfig::default(),
                    AppAssets::default(),
                    (None, Default::default()),
                    (Default::default(), None),
                    HostBridge::new(&[]),
                    window,
                    cx,
                );
                let record = split_room_rec();
                let mut room = RoomView::from_record(
                    &record,
                    record
                        .tabs
                        .iter()
                        .map(|tab| TabView::from_record(tab, (80, 24)))
                        .collect(),
                );
                room.select_tab_by_id(&record.tabs[1].id);
                desktop.active_room_id = Some(room.id.clone());
                desktop.machines[0].rooms.push(room);
                desktop
            })
        }

        #[gpui::test]
        fn sidebar_and_terminal_follow_window_resize(cx: &mut TestAppContext) {
            let (_, cx) = desktop(cx);
            cx.simulate_resize(size(px(420.), px(360.)));
            let wide = cx.debug_bounds("sidebar").unwrap();
            cx.simulate_resize(size(px(200.), px(360.)));
            let sidebar = cx.debug_bounds("sidebar").unwrap();
            let terminal = cx.debug_bounds("terminal").unwrap();
            assert!(sidebar.size.width < wide.size.width);
            assert_eq!(sidebar.size.width, px(110.));
            assert_eq!(terminal.origin.x + terminal.size.width, px(200.));
        }

        #[gpui::test]
        fn split_panes_render_side_by_side_and_zoom_fills_terminal(cx: &mut TestAppContext) {
            let (view, cx) = desktop(cx);
            let left = cx.debug_bounds(LEFT).unwrap();
            let right = cx.debug_bounds(RIGHT).unwrap();
            assert_eq!(left.origin.y, right.origin.y);
            assert!(left.origin.x + left.size.width < right.origin.x);
            cx.update(|_, app| {
                view.update(app, |desktop, cx| {
                    desktop.toggle_zoom();
                    cx.notify();
                })
            });
            cx.run_until_parked();
            assert!(cx.debug_bounds(RIGHT).is_none());
            assert_eq!(cx.debug_bounds(LEFT), cx.debug_bounds("terminal"));
        }

        #[gpui::test]
        fn update_banner_and_reset_notice_both_offset_the_canvas(cx: &mut TestAppContext) {
            use super::super::{RESET_BANNER_HEIGHT, UPDATE_BANNER_HEIGHT, UpdatePhase};
            let (view, cx) = desktop(cx);
            let top = cx.update(|_, app| view.read(app).canvas.y);
            cx.update(|_, app| {
                view.update(app, |desktop, cx| {
                    desktop.machines[0].reset_notice = Some(super::ResetNotice {
                        notice_id: "backup".into(),
                        backup_file: "catalog.backup.json".into(),
                    });
                    desktop.update.phase = UpdatePhase::Applying("v2.0.0".into());
                    cx.notify();
                })
            });
            cx.run_until_parked();
            let new_top = cx.update(|_, app| view.read(app).canvas.y);
            assert_eq!(
                new_top - top,
                UiScale::default().px(UPDATE_BANNER_HEIGHT)
                    + UiScale::default().px(RESET_BANNER_HEIGHT)
            );
            assert_eq!(cx.debug_bounds("terminal").unwrap().origin.y, px(new_top));
        }

        #[gpui::test]
        fn ui_zoom_resizes_the_tab_bar_and_sidebar(cx: &mut TestAppContext) {
            let (view, cx) = desktop(cx);
            let before = cx.debug_bounds("tab-bar").unwrap();
            let unrounded_height = TAB_BAR_HEIGHT_REM * UiScale::default().rem_px();
            assert_eq!(before.size.height, px(unrounded_height.round()));
            let sidebar = cx.debug_bounds("sidebar").unwrap();
            cx.update(|window, app| {
                view.update(app, |desktop, cx| {
                    desktop.apply_ui_scale(UiScale::from_percent(150), window);
                    cx.notify();
                })
            });
            cx.run_until_parked();
            let bar = cx.debug_bounds("tab-bar").unwrap();
            assert_eq!(bar.size.height, px((unrounded_height * 1.5).round()));
            let canvas_top = cx.update(|_, app| view.read(app).canvas.y);
            assert_eq!(px(canvas_top), bar.origin.y + bar.size.height);
            assert!(cx.debug_bounds("sidebar").unwrap().size.width > sidebar.size.width);
        }
    }

    struct HostReload {
        machines: Vec<MachineView>,
        default: String,
        active: String,
        room: Option<RoomId>,
    }

    impl HostReload {
        fn new() -> Self {
            let machines: Vec<_> = ["local", "remote"]
                .into_iter()
                .enumerate()
                .map(|(i, id)| {
                    let mut profile = HostProfile::local_default();
                    profile.id = id.into();
                    let mut machine = MachineView::new(profile);
                    let rec = room_rec(i as u32 + 1, vec![tab_rec(i as u32 + 1, true)]);
                    let mut tab = TabView::from_record(&rec.tabs[0], (80, 24));
                    tab.vt = Some(Vt::new(80, 24).unwrap());
                    tab.channel = Some(42);
                    machine.rooms.push(RoomView::from_record(&rec, vec![tab]));
                    machine.catalog_seq = Some(12);
                    machine
                })
                .collect();
            Self {
                room: Some(machines[0].rooms[0].id.clone()),
                machines,
                default: "local".into(),
                active: "local".into(),
            }
        }

        fn config(&self) -> HostsConfig {
            HostsConfig {
                default_host_id: self.default.clone(),
                hosts: self.machines.iter().map(|m| m.profile.clone()).collect(),
                ..HostsConfig::default()
            }
        }

        fn apply(&mut self, cfg: HostsConfig) -> Option<HostsConfigChanges> {
            reconcile_hosts_config(
                &mut self.machines,
                &mut self.default,
                &mut self.active,
                &mut self.room,
                cfg,
            )
        }
    }

    #[test]
    fn hosts_reload_identical_is_noop() {
        let mut state = HostReload::new();
        let before = state.machines.as_ptr();
        assert!(state.apply(state.config()).is_none());
        assert_eq!(state.machines.as_ptr(), before);
        assert_eq!(state.machines[0].rooms[0].tabs[0].channel, Some(42));
    }

    #[test]
    fn hosts_reload_add_preserves_live_rooms_tabs_and_vt() {
        let mut state = HostReload::new();
        let before = state.machines[0].rooms.as_ptr();
        let room = state.room.clone();
        let mut cfg = state.config();
        let added = HostProfile {
            id: "added".into(),
            ..HostProfile::local_default()
        };
        cfg.hosts.push(added.clone());
        let changes = state.apply(cfg).unwrap();
        assert_eq!(changes.reconnect, [added]);
        assert!(changes.removed.is_empty());
        assert!(!changes.active_host_changed);
        assert_eq!(state.machines.len(), 3);
        assert_eq!(state.machines[2].profile.id, "added");
        assert_eq!(state.machines[0].rooms.as_ptr(), before);
        assert_eq!(state.machines[0].catalog_seq, Some(12));
        let tab = &state.machines[0].rooms[0].tabs[0];
        assert_eq!(tab.channel, Some(42));
        assert!(tab.vt.is_some());
        assert_eq!(state.room, room);
    }

    #[test]
    fn hosts_reload_label_only_does_not_reconnect() {
        let mut state = HostReload::new();
        let before = state.machines[0].rooms.as_ptr();
        let mut cfg = state.config();
        cfg.hosts[0].label = "renamed".into();
        let changes = state.apply(cfg).unwrap();
        assert!(changes.reconnect.is_empty());
        assert!(changes.removed.is_empty());
        assert_eq!(state.machines[0].profile.label, "renamed");
        assert_eq!(state.machines[0].rooms.as_ptr(), before);
        assert_eq!(state.machines[0].rooms[0].tabs[0].channel, Some(42));
    }

    #[test]
    fn hosts_reload_transport_or_expected_id_change_reconnects_and_resets_view() {
        for change_transport in [true, false] {
            let mut state = HostReload::new();
            let old = &mut state.machines[0];
            old.conn_state = ConnectionState::Connected {
                host_id: "old".into(),
                conn_id: 1,
                features: vec![],
            };
            old.connected_host_id = Some("old".into());
            old.install_req_id = Some(1);
            old.installing_stage = Some("installing".into());
            old.collapsed = true;
            let mut cfg = state.config();
            if change_transport {
                cfg.hosts[0].transport = HostTransport::Local {
                    sock_path: Some("other.sock".into()),
                };
            } else {
                cfg.hosts[0].expected_host_id = Some("new".into());
            }
            let expected = cfg.hosts[0].clone();
            let changes = state.apply(cfg).unwrap();
            assert_eq!(changes.reconnect, std::slice::from_ref(&expected));
            assert!(changes.removed.is_empty());
            assert!(!changes.active_host_changed);
            assert_eq!(state.active, "local");
            assert!(state.room.is_none());
            let current = &state.machines[0];
            assert_eq!(current.profile, expected);
            assert!(matches!(current.conn_state, ConnectionState::Connecting));
            assert!(current.connected_host_id.is_none());
            assert!(current.catalog_seq.is_none());
            assert!(current.installing_stage.is_none());
            assert!(current.install_req_id.is_none());
            assert!(current.rooms.is_empty());
            assert!(current.collapsed);
            assert_eq!(state.machines[1].catalog_seq, Some(12));
        }
    }

    #[test]
    fn hosts_reload_reorder_moves_existing_views() {
        let mut state = HostReload::new();
        let before: Vec<_> = state.machines.iter().map(|m| m.rooms.as_ptr()).collect();
        let mut cfg = state.config();
        cfg.hosts.reverse();
        let changes = state.apply(cfg).unwrap();
        assert!(changes.reconnect.is_empty());
        assert!(changes.removed.is_empty());
        assert!(!changes.active_host_changed);
        assert_eq!(state.machines[0].profile.id, "remote");
        assert_eq!(state.machines[1].profile.id, "local");
        assert_eq!(state.machines[0].rooms.as_ptr(), before[1]);
        assert_eq!(state.machines[1].rooms.as_ptr(), before[0]);
    }

    #[test]
    fn hosts_reload_remove_active_cleans_references_and_selects_default_or_first() {
        for default_exists in [true, false] {
            let mut state = HostReload::new();
            let old_key = RoomKey {
                host_id: state.active.clone(),
                room_id: state.room.clone().unwrap(),
            };
            let fallback = state.machines[1].rooms[0].id.clone();
            let mut cfg = state.config();
            cfg.hosts.remove(0);
            let other = HostProfile {
                id: "added".into(),
                ..HostProfile::local_default()
            };
            if default_exists {
                cfg.default_host_id = "REMOTE".into();
                cfg.hosts.insert(0, other);
            } else {
                cfg.hosts.push(other);
            }

            let changes = state.apply(cfg).unwrap();
            assert_eq!(changes.removed, ["local"]);
            assert!(changes.active_host_changed);
            assert_eq!(state.active, "remote");
            assert_eq!(state.room, Some(fallback.clone()));

            let mut hovered_machine = Some(old_key.host_id.clone());
            let mut hovered_room = Some(old_key.clone());
            let mut rename = Some((old_key.clone(), String::from("pending name")));
            let mut drag = Some(PaneDrag {
                host_id: old_key.host_id.clone(),
                room_id: old_key.room_id.clone(),
                group_idx: 0,
                path: Default::default(),
                axis: SplitAxis::Horizontal,
                parent_origin: 0.,
                parent_extent: 10.,
                canvas_origin: 0.,
                grab_offset: 0.,
                dirty: true,
            });
            let mut badge = Some(ExternalInputIndicator {
                host_id: old_key.host_id.clone(),
                tab_id: "tab".into(),
                run_generation: 1,
                label: "input".into(),
                expires_at: Instant::now(),
            });
            let mut pending = Some((old_key, "tab".into()));
            let mut other_room = Some(RoomKey {
                host_id: "remote".into(),
                room_id: fallback,
            });
            changes.discard_stale(&mut hovered_machine, String::as_str);
            changes.discard_stale(&mut hovered_room, |key| &key.host_id);
            changes.discard_stale(&mut rename, |(key, _)| &key.host_id);
            changes.discard_stale(&mut drag, |drag| &drag.host_id);
            changes.discard_stale(&mut badge, |badge| &badge.host_id);
            changes.discard_stale(&mut pending, |(key, _): &(RoomKey, String)| &key.host_id);
            changes.discard_stale(&mut other_room, |key| &key.host_id);
            assert!(hovered_machine.is_none());
            assert!(hovered_room.is_none());
            assert!(rename.is_none());
            assert!(drag.is_none());
            assert!(badge.is_none());
            assert!(pending.is_none());
            assert!(other_room.is_some());
        }
    }

    #[test]
    fn hosts_reload_default_change_does_not_steal_focus() {
        let mut state = HostReload::new();
        let room = state.room.clone();
        let mut cfg = state.config();
        cfg.default_host_id = "remote".into();
        let changes = state.apply(cfg).unwrap();
        assert!(changes.reconnect.is_empty());
        assert!(changes.removed.is_empty());
        assert!(!changes.active_host_changed);
        assert_eq!(state.default, "remote");
        assert_eq!(state.active, "local");
        assert_eq!(state.room, room);
    }

    #[test]
    fn tab_shortcuts() {
        let cmd = Modifiers {
            platform: true,
            ..Default::default()
        };
        let ctrl_shift = Modifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(tab_action("{", cmd, true).unwrap().target(0, 3), Some(2));
        assert_eq!(
            tab_action("]", ctrl_shift, false).unwrap().target(2, 3),
            Some(0)
        );
        assert_eq!(tab_action("n", cmd, true), Some(TabAction::NewRoom));
        let ctrl = Modifiers {
            shift: false,
            ..ctrl_shift
        };
        assert_eq!(tab_action("n", ctrl, false), Some(TabAction::NewRoom));
        assert_eq!(tab_action("n", ctrl, true), None);
        assert_eq!(tab_action("n", ctrl_shift, false), None);
        assert_eq!(tab_action("[", cmd, true), None);
        for (mac, primary) in [(true, cmd), (false, ctrl)] {
            assert_eq!(tab_action("0", primary, mac), None);
            for n in 1..=9 {
                assert_eq!(
                    tab_action(&n.to_string(), primary, mac).and_then(|a| a.target(0, 12)),
                    Some(n - 1),
                );
            }
            assert_eq!(tab_action("9", primary, mac).unwrap().target(0, 3), None);
            assert_eq!(tab_action("1", primary, mac).unwrap().target(0, 0), None);
        }
        assert_eq!(TabAction::Number(0).target(0, 12), None);
        assert_eq!(tab_action("1", Modifiers::default(), true), None);
    }

    #[test]
    fn zoom_shortcuts_require_the_platform_primary_modifier() {
        for mac in [true, false] {
            let primary = Modifiers {
                platform: mac,
                control: !mac,
                ..Default::default()
            };
            for (key, action) in [
                ("+", ZoomAction::In),
                ("=", ZoomAction::In),
                ("-", ZoomAction::Out),
                ("_", ZoomAction::Out),
                ("0", ZoomAction::Reset),
            ] {
                assert_eq!(zoom_action(key, primary, mac), Some(action));
                assert_eq!(
                    zoom_action(
                        key,
                        Modifiers {
                            shift: true,
                            ..primary
                        },
                        mac
                    ),
                    (action != ZoomAction::Reset).then_some(action),
                );
                for modifiers in [
                    Modifiers::default(),
                    Modifiers {
                        platform: !mac,
                        control: mac,
                        ..primary
                    },
                    Modifiers {
                        platform: true,
                        control: true,
                        ..primary
                    },
                    Modifiers {
                        alt: true,
                        ..primary
                    },
                    Modifiers {
                        function: true,
                        ..primary
                    },
                ] {
                    assert_eq!(zoom_action(key, modifiers, mac), None);
                }
            }
            for key in ["1", "9", ")", "enter", "n"] {
                assert_eq!(zoom_action(key, primary, mac), None);
            }
        }
    }

    #[test]
    fn scaled_layout_keeps_sidebar_and_split_dividers_in_pointer_coordinates() {
        use super::{
            DIVIDER_WIDTH, PANE_DIVIDER, PANE_INSET, PaneRect, RESET_BANNER_HEIGHT, SIDEBAR_MAX,
            SIDEBAR_MIN, TAB_BAR_HEIGHT_REM, UiScale, compute_layout, point, px, scaled_pane_grid,
            sidebar_px,
        };
        for (pct, expected_side) in [(50, 101.), (100, 202.), (150, 302.), (200, 403.)] {
            let scale = UiScale::from_percent(pct);
            let side = sidebar_px(224., scale, 1200.);
            assert_eq!(side, expected_side);
            assert_eq!(sidebar_px(224., scale, 120.), 66.);
            assert_eq!(sidebar_px(0., scale, 2000.), scale.px(SIDEBAR_MIN));
            assert_eq!(sidebar_px(999., scale, 2000.), scale.px(SIDEBAR_MAX));
            let new_side = side + 20.;
            assert_eq!(
                sidebar_px(new_side / scale.factor(), scale, 1200.),
                new_side
            );
            let x = side + scale.px(DIVIDER_WIDTH);
            let y = (TAB_BAR_HEIGHT_REM * scale.rem_px()).round() + scale.px(RESET_BANNER_HEIGHT);
            let bounds = PaneRect {
                x: 0.,
                y: 0.,
                w: 1200. - x,
                h: 800. - y,
            };
            let thickness = scale.px(PANE_DIVIDER);
            for axis in [SplitAxis::Horizontal, SplitAxis::Vertical] {
                let node = PaneNode::Split {
                    axis,
                    ratio_bp: 5000,
                    first: Box::new(PaneNode::Leaf {
                        tab_id: "left".into(),
                    }),
                    second: Box::new(PaneNode::Leaf {
                        tab_id: "right".into(),
                    }),
                };
                let (panes, dividers) = compute_layout(&node, bounds, thickness);
                let hit = &dividers[0];
                let first = panes[0].1;
                let second = panes[1].1;
                let (origin, first_extent, second_origin, divider) = match axis {
                    SplitAxis::Horizontal => (x, first.w, second.x, hit.rect.w),
                    SplitAxis::Vertical => (y, first.h, second.y, hit.rect.h),
                };
                assert_eq!(divider, thickness);
                assert_eq!(second_origin, first_extent + thickness);
                let grab_offset = thickness / 2.;
                let drag = PaneDrag {
                    host_id: String::new(),
                    room_id: String::new(),
                    group_idx: 0,
                    path: hit.path.clone(),
                    axis,
                    parent_origin: hit.parent_origin,
                    parent_extent: hit.parent_extent,
                    canvas_origin: origin,
                    grab_offset,
                    dirty: false,
                };
                let cursor = px(origin + grab_offset + hit.parent_extent * 0.75);
                let pos = match axis {
                    SplitAxis::Horizontal => point(cursor, px(y)),
                    SplitAxis::Vertical => point(px(x), cursor),
                };
                assert_eq!(drag.ratio_for_position(pos), 7500);
                let inset = scale.px(PANE_INSET);
                let font = scale.term_font_px();
                let fits = PaneRect {
                    w: 10. * font + 2. * inset,
                    h: 5. * font + 2. * inset,
                    ..first
                };
                assert_eq!(scaled_pane_grid(fits, font, font, scale), (10, 5));
            }
        }
    }

    #[test]
    fn pane_shortcuts_preserve_unmapped_alt_and_terminal_control_keys() {
        for mac in [true, false] {
            let primary = Modifiers {
                platform: mac,
                control: !mac,
                ..Default::default()
            };
            let shift = Modifiers {
                shift: true,
                ..primary
            };
            let alt = Modifiers {
                alt: true,
                ..primary
            };
            assert_eq!(
                tab_action("d", if mac { primary } else { shift }, mac),
                Some(TabAction::SplitHorizontal)
            );
            assert_eq!(
                tab_action(if mac { "d" } else { "e" }, shift, mac),
                Some(TabAction::SplitVertical)
            );
            assert_eq!(tab_action("enter", shift, mac), Some(TabAction::ToggleZoom));
            assert_eq!(tab_action("u", shift, mac), Some(TabAction::Unsplit));
            for (key, direction) in [
                ("left", NavDir::Left),
                ("right", NavDir::Right),
                ("up", NavDir::Up),
                ("down", NavDir::Down),
            ] {
                assert_eq!(
                    tab_action(key, alt, mac),
                    Some(TabAction::FocusDir(direction))
                );
                assert_eq!(
                    tab_action(
                        key,
                        Modifiers {
                            alt: true,
                            ..Default::default()
                        },
                        mac
                    ),
                    None
                );
            }
            assert_eq!(tab_action("d", alt, mac), None);
            assert_eq!(
                tab_action(
                    "up",
                    Modifiers {
                        function: true,
                        ..alt
                    },
                    mac
                ),
                None
            );
            assert_eq!(
                tab_action("up", Modifiers { shift: true, ..alt }, mac),
                None
            );
            assert_eq!(
                tab_action(
                    "d",
                    Modifiers {
                        control: true,
                        ..Default::default()
                    },
                    mac
                ),
                None
            );
        }
    }

    #[test]
    fn formats_dropped_file_paths_with_shell_escaping() {
        let single = format_dropped_paths(&[PathBuf::from("/Users/dotdot/shot.png")]);
        assert_eq!(single, "/Users/dotdot/shot.png");

        let complex = format_dropped_paths(&[
            PathBuf::from("/Users/dotdot/My Folder/a (1).png"),
            PathBuf::from("/tmp/b'c.txt"),
        ]);
        assert_eq!(
            complex,
            r"/Users/dotdot/My\ Folder/a\ \(1\).png /tmp/b\'c.txt"
        );
    }

    #[test]
    fn sync_machine_rooms_preserves_selection_and_detaches_removed_tabs() {
        let mut machine = MachineView::new(HostProfile::local_default());
        let first = room_rec(1, vec![tab_rec(1, true)]);
        let second = room_rec(2, vec![tab_rec(2, true), tab_rec(3, true)]);
        let mut catalog = Catalog::empty("h1");
        catalog.rooms = vec![first.clone(), second.clone()];
        let mut host = machine.profile.id.clone();
        let mut room = Some(second.id.clone());

        sync_machine_rooms(
            &mut machine,
            &catalog,
            &mut host,
            &mut room,
            (80, 24),
            &mut None,
        );
        machine.rooms[1].select_group(1);
        machine.rooms[1].tabs[0].channel = Some(4);
        machine.rooms[1].tabs[1].channel = Some(5);
        sync_machine_rooms(
            &mut machine,
            &catalog,
            &mut host,
            &mut room,
            (80, 24),
            &mut None,
        );
        assert_eq!(machine.rooms[1].selected_group, 1);
        assert_eq!(room.as_deref(), Some(second.id.as_str()));

        catalog.rooms[1].tabs.remove(0);
        assert_eq!(
            sync_machine_rooms(
                &mut machine,
                &catalog,
                &mut host,
                &mut room,
                (80, 24),
                &mut None
            ),
            [4]
        );
        assert_eq!(
            machine.rooms[1].focused_tab().unwrap().id,
            second.tabs[1].id
        );

        catalog.rooms.pop();
        assert_eq!(
            sync_machine_rooms(
                &mut machine,
                &catalog,
                &mut host,
                &mut room,
                (80, 24),
                &mut None
            ),
            [5]
        );
        assert_eq!(room, Some(first.id));
    }

    fn split_room_rec() -> RoomRecord {
        let mut room = room_rec(
            1,
            vec![tab_rec(1, true), tab_rec(2, true), tab_rec(3, true)],
        );
        let mut layout = RoomLayout::default_for_tabs(&room.tabs[..2]);
        assert!(
            layout
                .split_leaf(
                    &room.tabs[1].id,
                    SplitAxis::Horizontal,
                    room.tabs[2].id.clone(),
                    5000
                )
                .unwrap()
        );
        room.layout = Some(layout);
        room
    }

    #[test]
    fn catalog_reconciles_split_append_close_focus_and_zoom() {
        let mut machine = MachineView::new(HostProfile::local_default());
        let mut host = machine.profile.id.clone();
        let mut active = None;
        let mut catalog = Catalog::empty("h1");
        catalog.rooms.push(split_room_rec());
        let [standalone, left, right] = [0, 1, 2].map(|i| catalog.rooms[0].tabs[i].id.clone());
        sync_machine_rooms(
            &mut machine,
            &catalog,
            &mut host,
            &mut active,
            (80, 24),
            &mut None,
        );

        let room = &mut machine.rooms[0];
        assert!(room.select_tab_by_id(&right));
        assert_eq!(room.selected_group, 1);
        assert_eq!(room.visible_tab_ids(), [left.clone(), right.clone()]);
        room.zoomed_tab_id = Some(right.clone());
        room.tabs[1].channel = Some(4);
        room.tabs[2].channel = Some(5);

        let external = tab_rec(4, true);
        let external_id = external.id.clone();
        // Reconcile a membership update without losing the existing split.
        catalog.rooms[0].tabs.push(external);
        sync_machine_rooms(
            &mut machine,
            &catalog,
            &mut host,
            &mut active,
            (80, 24),
            &mut None,
        );
        let room = &mut machine.rooms[0];
        assert_eq!(room.layout.groups[0], PaneNode::leaf(standalone));
        assert_eq!(room.layout.groups[2], PaneNode::leaf(external_id));
        assert!(room.is_active_group_split());
        assert_eq!(room.focused_tab().unwrap().id, right);
        assert_eq!(room.visible_tab_ids(), [right]);
        assert!(room.select_tab_by_id(&left));
        assert!(room.zoomed_tab_id.is_none());
        room.zoomed_tab_id = Some(left.clone());

        catalog.rooms[0].tabs.remove(2);
        assert_eq!(
            sync_machine_rooms(
                &mut machine,
                &catalog,
                &mut host,
                &mut active,
                (80, 24),
                &mut None
            ),
            [5]
        );
        let room = &machine.rooms[0];
        assert!(!room.is_active_group_split());
        assert!(room.zoomed_tab_id.is_none());
        assert_eq!(room.focused_tab().unwrap().id, left);
        assert_eq!(room.focused_tab().unwrap().channel, Some(4));
    }

    #[test]
    fn drag_preview_survives_catalog_updates_until_the_split_changes() {
        let mut machine = MachineView::new(HostProfile::local_default());
        let mut host = machine.profile.id.clone();
        let mut active = None;
        let mut catalog = Catalog::empty("h1");
        catalog.rooms.push(split_room_rec());
        let mut drag = None;
        sync_machine_rooms(
            &mut machine,
            &catalog,
            &mut host,
            &mut active,
            (80, 24),
            &mut drag,
        );

        let room = &mut machine.rooms[0];
        room.select_group(1);
        assert!(set_ratio_at(&mut room.layout.groups[1], &[], 7000));
        drag = Some(PaneDrag {
            host_id: host.clone(),
            room_id: room.id.clone(),
            group_idx: 1,
            path: Vec::new(),
            axis: SplitAxis::Horizontal,
            parent_origin: 0.,
            parent_extent: 500.,
            canvas_origin: 230.,
            grab_offset: 0.,
            dirty: true,
        });
        catalog.rooms[0].tabs[1].title = "new title".into();
        catalog.rooms[0].tabs.push(tab_rec(4, true));
        sync_machine_rooms(
            &mut machine,
            &catalog,
            &mut host,
            &mut active,
            (80, 24),
            &mut drag,
        );
        assert!(drag.is_some());
        let room = &machine.rooms[0];
        assert_eq!(ratio_at(&room.layout.groups[1], &[]), Some(7000));
        assert_eq!(ratio_at(&room.server_layout.groups[1], &[]), Some(5000));
        assert_eq!(room.focused_tab().unwrap().title, "new title");
        assert_eq!(room.layout.groups.len(), 3);

        catalog.rooms[0].tabs.remove(2);
        sync_machine_rooms(
            &mut machine,
            &catalog,
            &mut host,
            &mut active,
            (80, 24),
            &mut drag,
        );
        assert!(drag.is_none());
        assert!(!machine.rooms[0].is_active_group_split());
    }

    #[test]
    fn early_stream_frames_are_bounded_and_cleared_on_failure() {
        let mut tab = TabView::from_record(&tab_rec(1, true), (80, 24));
        assert!(tab.begin_attach());
        tab.buffer_frame(4, TabFrame::Data(b"first paint".to_vec()));
        tab.buffer_frame(4, TabFrame::Resize(25, 30));
        assert!(
            matches!(&tab.early_frames[..], [(4, TabFrame::Data(bytes)), (4, TabFrame::Resize(25, 30))] if bytes == b"first paint")
        );
        tab.buffer_frame(4, TabFrame::Data(vec![0; MAX_PENDING]));
        assert!(tab.early_frames.is_empty() && tab.early_bytes > MAX_PENDING);
        tab.attach_failed("disconnected");
        assert_eq!(tab.early_bytes, 0);
        assert!(tab.begin_attach());
    }

    #[test]
    fn agent_list_background_disappears_as_soon_as_the_run_exits() {
        let mut catalog = Catalog::empty("local");
        let mut room = room_rec(1, vec![tab_rec(1, true)]);
        room.visibility = RoomVisibility::Hidden;
        room.id = "rm_bg_local".into();
        catalog.rooms.push(room);
        assert_eq!(super::running_background_tabs(&catalog).count(), 1);
        catalog.rooms[0].tabs[0].runtime = TabRuntimeState::Exited {
            status: "done".into(),
        };
        assert_eq!(super::running_background_tabs(&catalog).count(), 0);
        catalog.rooms[0].tabs[0] = tab_rec(1, true);
        catalog.rooms[0].visibility = RoomVisibility::Visible;
        assert_eq!(super::running_background_tabs(&catalog).count(), 0);
    }

    #[test]
    fn peek_layout_and_shortcuts() {
        assert!(super::peek_close_key(
            "escape",
            false,
            ControlMode::Readonly
        ));
        assert!(!super::peek_close_key(
            "escape",
            false,
            ControlMode::Control
        ));
        assert!(super::peek_close_key("w", true, ControlMode::Control));
        assert!(!super::peek_close_key("w", false, ControlMode::Control));
        assert!(super::peek_release_allowed(false, true));
        assert!(!super::peek_release_allowed(true, true));
        assert!(!super::peek_release_allowed(false, false));
        for key in [",", "<"] {
            assert!(super::settings_key(
                key,
                Modifiers {
                    control: true,
                    shift: true,
                    ..Default::default()
                },
                false
            ));
        }
        for percent in [50, 100, 200] {
            let scale = super::UiScale::from_percent(percent);
            let rect = super::PaneRect {
                x: 0.,
                y: 10.,
                w: 180.,
                h: 35.,
            };
            let header = super::pane_header_height(scale, true);
            let body = super::pane_body_rect(rect, header);
            assert_eq!(body.y, 10. + scale.px(22.));
            assert_eq!(body.h, (35. - scale.px(22.)).max(0.));
            assert!(super::scaled_pane_grid(body, 8., 14., scale).1 >= 1);
        }
        let now = Instant::now();
        let mut last = None;
        assert!(!super::spinner_due(true, true, &mut last, now));
        assert!(!super::spinner_due(
            true,
            true,
            &mut last,
            now + Duration::from_millis(124)
        ));
        assert!(super::spinner_due(
            true,
            true,
            &mut last,
            now + Duration::from_millis(125)
        ));
        assert!(!super::spinner_due(
            false,
            true,
            &mut last,
            now + Duration::from_secs(1)
        ));
        assert!(last.is_none());
        assert!(!super::spinner_due(
            true,
            false,
            &mut last,
            now + Duration::from_secs(2)
        ));
        let mut tab = TabView::from_record(&tab_rec(1, true), (80, 24));
        tab.activity = Some(super::TabActivity {
            tab_id: tab.id.clone(),
            run_generation: 1,
            agent_status: None,
            foreground: None,
            activity_seq: 6,
            seen_seq: 5,
        });
        assert_eq!(tab.unseen_sequence(), Some(6));
        tab.pending_seen_seq = 6;
        assert_eq!(tab.unseen_sequence(), None);
        tab.pending_seen_seq = 0;
        tab.activity.as_mut().unwrap().seen_seq = 6;
        assert_eq!(tab.unseen_sequence(), None);
    }

    #[test]
    fn hidden_room_is_not_stored_and_promotion_does_not_steal_focus() {
        for has_active_room in [false, true] {
            let mut machine = MachineView::new(HostProfile::local_default());
            machine.collapsed = true;
            let mut host = "other".to_owned();
            let mut room = has_active_room.then(|| "other-room".to_owned());
            let mut catalog = Catalog::empty("h1");
            let promoted = room_rec(1, vec![tab_rec(1, true)]);
            catalog.rooms.push(RoomRecord {
                id: "rm_bg_h1".into(),
                kind: RoomKind::Background,
                visibility: RoomVisibility::Hidden,
                ..promoted.clone()
            });

            assert!(
                sync_machine_rooms(
                    &mut machine,
                    &catalog,
                    &mut host,
                    &mut room,
                    (80, 24),
                    &mut None
                )
                .is_empty()
            );
            assert!(machine.rooms.is_empty());
            assert!(machine.collapsed);
            catalog.rooms.clear();
            catalog.rooms.push(promoted.clone());

            sync_machine_rooms(
                &mut machine,
                &catalog,
                &mut host,
                &mut room,
                (80, 24),
                &mut None,
            );
            assert!(!machine.collapsed);
            assert_eq!(machine.rooms[0].id, promoted.id);
            if has_active_room {
                assert_eq!(host, "other");
                assert_eq!(room.as_deref(), Some("other-room"));
            } else {
                assert_eq!(host, machine.profile.id);
                assert_eq!(room, Some(promoted.id));
            }
        }
    }

    #[test]
    fn generation_bump_detaches_and_resets_vt() {
        let mut machine = MachineView::new(HostProfile::local_default());
        let mut catalog = Catalog::empty("h1");
        let mut host = machine.profile.id.clone();
        let mut room = None;
        catalog.rooms.push(room_rec(1, vec![tab_rec(1, true)]));
        sync_machine_rooms(
            &mut machine,
            &catalog,
            &mut host,
            &mut room,
            (80, 24),
            &mut None,
        );
        let tab = &mut machine.rooms[0].tabs[0];
        let mut vt = Vt::new(80, 24).unwrap();
        vt.write(b"old screen");
        tab.vt = Some(vt);
        tab.channel = Some(7);
        tab.mode = ControlMode::Control;

        catalog.rooms[0].tabs[0].run_generation = 2;
        assert_eq!(
            sync_machine_rooms(
                &mut machine,
                &catalog,
                &mut host,
                &mut room,
                (80, 24),
                &mut None
            ),
            [7]
        );
        let tab = &machine.rooms[0].tabs[0];
        assert_eq!(tab.run_generation, 2);
        assert!(tab.channel.is_none() && tab.vt.is_none() && !tab.attaching);
        assert_eq!(tab.mode, ControlMode::Readonly);
        assert!(
            sync_machine_rooms(
                &mut machine,
                &catalog,
                &mut host,
                &mut room,
                (80, 24),
                &mut None
            )
            .is_empty()
        );
    }

    #[test]
    fn generation_bump_does_not_duplicate_an_in_flight_attach() {
        let mut machine = MachineView::new(HostProfile::local_default());
        let mut catalog = Catalog::empty("h1");
        catalog.rooms.push(room_rec(1, vec![tab_rec(1, true)]));
        let mut host = machine.profile.id.clone();
        let mut room = None;
        sync_machine_rooms(
            &mut machine,
            &catalog,
            &mut host,
            &mut room,
            (80, 24),
            &mut None,
        );
        assert!(machine.rooms[0].tabs[0].begin_attach());

        catalog.rooms[0].tabs[0].run_generation = 2;
        sync_machine_rooms(
            &mut machine,
            &catalog,
            &mut host,
            &mut room,
            (80, 24),
            &mut None,
        );
        let tab = &mut machine.rooms[0].tabs[0];
        assert!(!tab.begin_attach());
        let stale = TabStreamHandle {
            tab_id: tab.id.clone(),
            channel: 7,
            mode: ControlMode::Control,
            control_epoch: 1,
            cols: 80,
            rows: 24,
            run_generation: 1,
        };
        assert_eq!(accept_tab_attach(Some(tab), &stale), Some(7));
        assert!(tab.begin_attach());
        assert!(!tab.begin_attach());
    }

    #[test]
    fn rejects_stale_attach_and_clears_attaching_on_failure() {
        let mut rec = tab_rec(1, true);
        rec.run_generation = 2;
        let mut tab = TabView::from_record(&rec, (80, 24));
        tab.attaching = true;
        let stale = TabStreamHandle {
            tab_id: rec.id.clone(),
            channel: 7,
            mode: ControlMode::Control,
            control_epoch: 1,
            cols: 80,
            rows: 24,
            run_generation: 1,
        };
        assert_eq!(accept_tab_attach(Some(&mut tab), &stale), Some(7));
        assert!(tab.channel.is_none() && tab.vt.is_none() && !tab.attaching);
        assert_eq!(tab.run_generation, 2);

        tab.attaching = true;
        tab.attach_failed("failed");
        assert!(!tab.attaching);
        assert_eq!(tab.status, "failed");
        let current = TabStreamHandle {
            run_generation: 2,
            ..stale
        };
        assert_eq!(accept_tab_attach(Some(&mut tab), &current), None);
        assert_eq!(tab.channel, Some(7));
        assert!(tab.vt.is_some());
    }

    #[test]
    fn reset_notice_is_per_machine_and_lasts_until_dismissed_or_visible_room() {
        let notice = ResetNotice {
            notice_id: "reset-1".into(),
            backup_file: "/private/dir/catalog.json.v1.bak.1\u{202e}\n".into(),
        };
        let mut a = MachineView::new(HostProfile::local_default());
        let mut b = MachineView::new(HostProfile::local_default());
        a.update_reset_notice(Some(notice.clone()));
        b.update_reset_notice(Some(notice.clone()));
        assert_eq!(reset_backup_label(&notice), "catalog.json.v1.bak.1");

        a.dismiss_reset_notice("reset-1");
        a.update_reset_notice(Some(notice.clone()));
        assert!(a.reset_notice.is_none());
        assert_eq!(b.reset_notice.as_ref(), Some(&notice));

        let mut catalog = Catalog::empty("h1");
        let mut host = b.profile.id.clone();
        let mut room = None;
        let standard = room_rec(1, vec![tab_rec(1, true)]);
        catalog.rooms.push(RoomRecord {
            id: "rm_bg_h1".into(),
            kind: RoomKind::Background,
            visibility: RoomVisibility::Hidden,
            ..standard.clone()
        });
        sync_machine_rooms(&mut b, &catalog, &mut host, &mut room, (80, 24), &mut None);
        assert!(b.reset_notice.is_some());
        catalog.rooms.push(standard);
        sync_machine_rooms(&mut b, &catalog, &mut host, &mut room, (80, 24), &mut None);
        assert!(b.reset_notice.is_none());
        b.update_reset_notice(Some(notice.clone()));
        assert!(b.reset_notice.is_none());

        a.update_reset_notice(Some(ResetNotice {
            notice_id: "reset-2".into(),
            ..notice
        }));
        assert!(a.reset_notice.is_some());
        a.update_reset_notice(None);
        assert!(a.reset_notice.is_none());
    }

    #[test]
    fn external_input_badge_matches_active_generation_and_expires_exactly() {
        let now = Instant::now();
        let active_tab = tab_rec(1, true).id;
        let current = ("local", active_tab.as_str(), 2);
        let mut peer = MachineView::new(HostProfile::local_default());
        peer.profile.label = "bu\u{202e}ild\n".into();
        peer.profile.expected_host_id = Some("pinned".into());
        peer.connected_host_id = Some("peer".into());
        let machines = [peer];
        for event in [
            ("other", current.1, 2),
            ("local", "ses_other", 2),
            ("local", current.1, 1),
        ] {
            assert!(
                ExternalInputIndicator::new(now, Some(current), event, "peer", true, &machines)
                    .is_none()
            );
        }
        assert!(ExternalInputIndicator::new(now, None, current, "peer", true, &machines).is_none());
        for (host, is_remote, expected) in [
            ("peer", false, "외부 입력"),
            ("peer", true, "원격 입력 · build"),
            ("pinned", true, "원격 입력 · build"),
            ("ses_unknown", true, "원격 입력 · 원격"),
        ] {
            let badge = ExternalInputIndicator::new(
                now,
                Some(current),
                current,
                host,
                is_remote,
                &machines,
            )
            .unwrap();
            assert_eq!(
                badge.label_at(now + Duration::from_millis(2999), Some(current)),
                Some(expected)
            );
            assert_eq!(
                badge.label_at(now + Duration::from_secs(3), Some(current)),
                None
            );
            assert_eq!(badge.label_at(now, Some(("local", current.1, 3))), None);
            assert!(!expected.contains("ses_"));
        }
    }

    #[test]
    fn stable_element_ids_include_part_boundaries() {
        assert_ne!(
            stable_elem_id("room", &["a", "bc"]),
            stable_elem_id("room", &["ab", "c"])
        );
        assert_eq!(
            stable_elem_id("room", &["a", "bc"]),
            stable_elem_id("room", &["a", "bc"])
        );
    }
}
