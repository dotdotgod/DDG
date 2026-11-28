mod agent_launcher;
mod agent_list;
mod agent_modal;
mod config_watch;
mod host_bridge;
mod icons;
mod layout;
mod machine_modal;
mod terminal_canvas;
mod terminal_input;
mod terminal_interaction;
mod ui_scale;
mod update_apply;
mod update_controller;
mod update_modal;

use agent_launcher::{AgentLauncher, LaunchTarget};
use agent_list::{AgentEntry, Row, build_agent_tree, move_subtree};
use agent_modal::{AgentSettingsModal, PresetDetectionState};
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
use ddg_agents_transport::{
    agent::AgentsConfig,
    presets::find_agent_preset,
    programs::{
        ProgramDefinition, ProgramsConfig, is_interpreter_name, normalize_process_name,
        strip_script_extension,
    },
};
use ddg_agents_vt::{self as vt, Vt};
use gpui::{prelude::*, *};
use host_bridge::{
    BridgeEvent, BridgeInbox, BridgeMessage, HostBridge, PresetConnection, QueuedMessage, TabFrame,
};
use icons::{AppAssets, Icon, icon};
use layout::{
    DividerHit, NavDir, PANE_DIVIDER, PaneRect, SplitPath, compute_layout, navigate_direction,
    pane_grid, ratio_at, ratio_bp_from_cursor, set_ratio_at,
};
use machine_modal::{MachineModal, MachineTab, ModalField, TestState};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use terminal_canvas::{BodyOptions, FontGrid, PaneCache};
use terminal_interaction::{
    InputTarget, MouseTarget, PendingPaste, ScrollAccumulator, TerminalMouseState, TerminalShortcut,
};
use ui_scale::{AgentSortMode, BASE_REM_PX, BASE_TERM_FONT_PX, UI_SCALE_DEFAULT, UiPrefs, UiScale};

use update_controller::{Change, RemoteMachineUpdateState, UpdateController, UpdatePhase};
use update_modal::{BlockedCard, UPDATE_BANNER_HEIGHT, UpdateModal};

const SIDEBAR_MIN: f32 = 136.;
const SIDEBAR_MAX: f32 = 357.;
const DIVIDER_WIDTH: f32 = 5.;
const TAB_BAR_HEIGHT_REM: f32 = 2.;
const RESET_BANNER_HEIGHT: f32 = 27.;
const SIDEBAR_COMPACT_MAX: f32 = 157.;
const PANE_HEADER_HEIGHT: f32 = 19.;
const PEEK_BANNER_HEIGHT: f32 = 49.;
const PANE_INSET: f32 = 1.;
static BUILTIN_PROGRAMS: LazyLock<ProgramsConfig> = LazyLock::new(ProgramsConfig::default);
const ZOOM_INDICATOR_DURATION: Duration = Duration::from_millis(1500);
const SPINNER_INTERVAL: Duration = Duration::from_millis(125);
const BRIDGE_BYTE_BUDGET: usize = 64 * 1024;
const BRIDGE_TIME_BUDGET: Duration = Duration::from_millis(2);
const BRIDGE_MESSAGE_BUDGET: usize = 128;

pub(crate) fn perf_enabled() -> bool {
    static ENABLED: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("DDG_NATIVE_PERF").is_some_and(|v| v == "1"));
    #[cfg(test)]
    if TEST_PERF.get() {
        return true;
    }
    *ENABLED
}

#[cfg(test)]
thread_local! {
    static TEST_PERF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Default, Debug)]
struct PerfSample {
    drains: u64,
    empty_drains: u64,
    messages: u64,
    data_bytes: u64,
    budget_yields: u64,
    queue_depth_max: usize,
    queue_wait_us_sum: u128,
    queue_wait_us_max: u128,
    queued_samples: u64,
    desktop_renders: u64,
    spinner_renders: u64,
    sidebar_rebuilds: u64,
    notifies: BTreeMap<&'static str, u64>,
    panes: BTreeMap<u64, PanePerf>,
}

#[derive(Default, Debug)]
struct PanePerf {
    frames: u64,
    rows: usize,
    cells: usize,
    runs: usize,
    extract_us: u128,
    compose_us: u128,
    builds: u64,
    reshapes: usize,
    paints: u64,
}

thread_local! {
    static PERF: RefCell<PerfSample> = RefCell::default();
}

fn perf(record: impl FnOnce(&mut PerfSample)) {
    if perf_enabled() {
        PERF.with_borrow_mut(record);
    }
}

fn notify<T: 'static>(cx: &mut Context<T>, reason: &'static str) {
    perf(|p| *p.notifies.entry(reason).or_default() += 1);
    cx.notify();
}
const DISPLAY_TEXT_LIMIT: usize = 128;
const REMOTE_INSTALL_ACTION: &str = "Install Remotely and Connect";
const REMOTE_INSTALL_MISSING: &str = "ddg-agents-host is not installed on the remote machine";

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
    surface: TerminalSurface,
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
            surface: TerminalSurface::default(),
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
            surface: TerminalSurface::default(),
            status: self.status.clone(),
            closing: false,
            size: self.size,
        }
    }

    fn runtime_status(runtime: &TabRuntimeState) -> String {
        match runtime {
            TabRuntimeState::Running { .. } => "Preparing to connect".into(),
            TabRuntimeState::Exited { status } => status.clone(),
            TabRuntimeState::StoppedAfterReboot => {
                "Stopped after reboot (not restarted automatically)".into()
            }
        }
    }

    fn is_running(&self) -> bool {
        matches!(self.runtime, TabRuntimeState::Running { .. })
    }

    fn is_selectable(&self) -> bool {
        !self.closing
    }

    fn begin_attach(&mut self) -> bool {
        if self.is_running() && self.channel.is_none() && !self.attaching && !self.closing {
            self.attaching = true;
            self.status = "Connecting".into();
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
    if tab.closing
        || handle.run_generation < tab.run_generation
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
    tab.status = "Running".into();
    tab.vt = Vt::new(handle.cols, handle.rows).ok();
    tab.surface = TerminalSurface::default();
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

fn prune_closing_node(node: &PaneNode, tabs: &[TabView]) -> Option<PaneNode> {
    match node {
        PaneNode::Leaf { tab_id } => (!tabs.iter().any(|tab| tab.id == *tab_id && tab.closing))
            .then(|| node.clone()),
        PaneNode::Split { axis, ratio_bp, first, second } => {
            match (prune_closing_node(first, tabs), prune_closing_node(second, tabs)) {
                (Some(first), Some(second)) => Some(PaneNode::Split {
                    axis: *axis,
                    ratio_bp: *ratio_bp,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (survivor, None) | (None, survivor) => survivor,
            }
        }
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

    fn is_tab_closing(&self, tab_id: &str) -> bool {
        self.tabs.iter().any(|tab| tab.id == tab_id && tab.closing)
    }

    fn has_selectable_tabs(&self) -> bool {
        self.tabs.iter().any(TabView::is_selectable)
    }

    fn projected_groups(&self) -> Vec<(usize, PaneNode)> {
        self.layout.groups.iter().enumerate().filter_map(|(index, node)| {
            prune_closing_node(node, &self.tabs).map(|node| (index, node))
        }).collect()
    }

    fn projected_active_group(&self) -> Option<PaneNode> {
        prune_closing_node(self.active_group()?, &self.tabs)
    }

    fn select_projected_group(&mut self, projected_index: usize) -> bool {
        if let Some((raw_index, _)) = self.projected_groups().get(projected_index) {
            self.focused_tab_id = None;
            self.select_group(*raw_index);
            true
        } else {
            false
        }
    }

    fn ensure_selectable_focus(&mut self) -> bool {
        if self.focused_tab_id().is_none() {
            let groups = self.projected_groups();
            if let Some((index, _)) = groups.iter().find(|(index, _)| *index > self.selected_group)
                .or_else(|| groups.last())
            {
                self.select_group(*index);
            }
        }
        self.repair_selection();
        self.focused_tab_id.is_some()
    }

    fn visible_tab_ids(&self) -> Vec<TabId> {
        let mut ids = Vec::new();
        if let Some(id) = self
            .zoomed_tab_id
            .as_ref()
            .filter(|id| Some(id.as_str()) == self.focused_tab_id())
        {
            ids.push(id.clone());
        } else if let Some(group) = self.projected_active_group() {
            group.leaf_ids(&mut ids);
        }
        ids
    }

    fn focused_tab_id(&self) -> Option<&str> {
        let group = self.projected_active_group()?;
        let id = self.focused_tab_id.as_deref().filter(|id| group.contains(id))
            .unwrap_or_else(|| first_leaf(&group));
        self.tabs.iter().find(|tab| tab.id == id).map(|tab| tab.id.as_str())
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
        if self.is_tab_closing(tab_id) {
            return false;
        }
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

    fn preserve_selection(&mut self, previous: &Self, focus: Option<&str>, siblings: &[TabId]) {
        self.selected_group = previous.selected_group;
        if !focus.is_some_and(|id| self.select_tab_by_id(id))
            && !siblings.iter().any(|id| self.select_tab_by_id(id))
        {
            self.ensure_selectable_focus();
        }
        self.zoomed_tab_id.clone_from(&previous.zoomed_tab_id);
        self.repair_selection();
    }

    fn is_active_group_split(&self) -> bool {
        matches!(self.projected_active_group(), Some(PaneNode::Split { .. }))
    }

    fn restore_server_layout(&mut self) {
        let focus = self.focused_tab_id().map(str::to_owned);
        self.layout.clone_from(&self.server_layout);
        if !focus.is_some_and(|id| self.select_tab_by_id(&id)) {
            self.ensure_selectable_focus();
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
            format!("Remote input · {}", label.as_deref().unwrap_or("Remote"))
        } else {
            "External input".into()
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

struct AgentTooltip(String, f32);
impl Render for AgentTooltip {
    fn render(&mut self, _window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_1()
            .py_0p5()
            .rounded_sm()
            .max_w(px(self.1))
            .debug_selector(|| "agent-tooltip".into())
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

type SpinnerSlots = Rc<RefCell<Vec<(Bounds<Pixels>, ContentMask<Pixels>)>>>;

struct SpinnerClock {
    slots: SpinnerSlots,
    frame: usize,
}

impl Render for SpinnerClock {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        perf(|p| p.spinner_renders += 1);
        let slots = self.slots.clone();
        let angle = radians(self.frame as f32 * std::f32::consts::FRAC_PI_4);
        canvas(
            |_, _, _| (),
            move |_, _, window, cx| {
                for (bounds, mask) in slots.borrow().iter() {
                    let center = bounds.center().scale(window.scale_factor());
                    let matrix = TransformationMatrix::unit()
                        .translate(center)
                        .rotate(angle)
                        .translate(center.negate());
                    window.with_content_mask(Some(mask.clone()), |window| {
                        let _ = window.paint_svg(
                            *bounds,
                            Icon::Spinner.path().into(),
                            matrix,
                            rgb(0x92b6ff).into(),
                            cx,
                        );
                    });
                }
            },
        )
        .absolute()
        .size_full()
    }
}

// GPUI invalidates ancestors of dirty views. Keep the spinner layer a sibling of
// the cached Desktop so a clock tick never enters Desktop::render or terminal layout.
struct DesktopWindow {
    desktop: Entity<Desktop>,
    spinner: Entity<SpinnerClock>,
}

impl DesktopWindow {
    fn new(desktop: Entity<Desktop>, cx: &App) -> Self {
        Self {
            spinner: desktop.read(cx).spinner.clone(),
            desktop,
        }
    }
}

impl Render for DesktopWindow {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .size_full()
            .child(
                AnyView::from(self.desktop.clone()).cached(StyleRefinement::default().size_full()),
            )
            .child(self.spinner.clone())
    }
}

struct TerminalSurface {
    id: u64,
    cache: PaneCache,
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    ime_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    scroll: ScrollAccumulator,
}

impl Default for TerminalSurface {
    fn default() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            cache: PaneCache::default(),
            bounds: Rc::default(),
            ime_bounds: Rc::default(),
            scroll: ScrollAccumulator::default(),
        }
    }
}

fn settings_key(key: &str, m: Modifiers, mac: bool) -> bool {
    !m.alt
        && if mac {
            m.platform && !m.control && !m.shift && key == ","
        } else {
            m.control && !m.platform && m.shift && matches!(key, "," | "<")
        }
}

#[derive(Clone)]
struct ResolvedProgramIcon {
    key: String,
    tint: bool,
    tint_color: u32,
}

impl ResolvedProgramIcon {
    fn new(key: &str, program: Option<&ProgramDefinition>) -> Self {
        Self {
            key: key.into(),
            tint: program.is_some_and(|p| p.tint),
            tint_color: program
                .and_then(|p| p.color.as_deref()?.strip_prefix('#'))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .unwrap_or(0xc8ccd0),
        }
    }
}

#[derive(Clone)]
struct TabIdentity {
    name: SharedString,
    icon: ResolvedProgramIcon,
}

type TabLabels = HashMap<String, HashMap<TabId, TabIdentity>>;

fn short_name(raw: &str, limit: usize) -> Option<String> {
    let text = sanitize_display_text(raw, limit);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn process_arg_name(raw: &str, allow_login: bool) -> Option<String> {
    if raw.contains(['=', '@'])
        || raw.contains("://")
        || raw.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    let base = raw.rsplit(['/', '\\']).next()?;
    let name = normalize_process_name(raw)?;
    let login = allow_login
        && raw == base
        && base.starts_with('-')
        && ["zsh", "bash", "fish", "sh", "dash"].contains(&name.as_str());
    if (!login && (raw.starts_with('-') || base.starts_with('-')))
        || name.starts_with('-')
        || matches!(name.as_str(), "." | "..")
    {
        return None;
    }
    Some(name)
}

#[derive(Default)]
struct AgentUiCache {
    tree: OnceCell<Rc<Vec<Row>>>,
    tab_labels: RefCell<TabLabels>,
}

impl AgentUiCache {
    fn invalidate(&mut self) {
        self.tree.take();
        self.tab_labels.get_mut().clear();
    }
}

#[cfg(test)]
type TestInputSink = Box<dyn Fn(&str, u32, &[u8])>;

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
enum TestTabRequest {
    Close(String, TabId),
    Attach(String, RoomId, TabId),
    Detach(String, u32),
}

struct Desktop {
    #[cfg(test)]
    test_input_sink: Option<TestInputSink>,
    #[cfg(test)]
    test_tab_requests: Vec<TestTabRequest>,
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
    terminal_input: Option<Entity<terminal_input::TerminalInput>>,
    terminal_mouse: Option<TerminalMouseState>,
    terminal_pointer: Point<Pixels>,
    terminal_autoscroll: Option<Task<()>>,
    pending_paste: Option<PendingPaste>,
    swallow_paste_enter_repeat: bool,
    agent_cursor: usize,
    hovered_agent: Option<String>,
    keyboard_group: Option<String>,
    agent_ui: AgentUiCache,
    spinner: Entity<SpinnerClock>,
    spinner_slots: SpinnerSlots,
    spinner_task: Option<Task<()>>,
    _bridge_task: Task<()>,
    _perf_task: Option<Task<()>>,
    zoom_expiry: Option<Task<()>>,
    external_input_expiry: Option<Task<()>>,
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
    preset_control: Option<PresetControl>,
    preset_connection: Option<PresetConnection>,
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
enum PresetControl {
    Machine,
    Refresh,
    Add(&'static str),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RoomNavDir {
    Previous,
    Next,
}

fn room_nav_action(key: &str, m: Modifiers, mac: bool) -> Option<RoomNavDir> {
    let primary = m.platform == mac && m.control != mac;
    if !primary || !m.alt || m.shift || m.function {
        return None;
    }
    match key {
        "[" => Some(RoomNavDir::Previous),
        "]" => Some(RoomNavDir::Next),
        _ => None,
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
        let focus = room.focused_tab_id().map(str::to_owned);
        let mut siblings = Vec::new();
        if let Some(group) = room.projected_active_group() {
            group.leaf_ids(&mut siblings);
        }
        prev_tabs.extend(room.tabs.drain(..).map(|tab| (tab.id.clone(), tab)));
        prev_rooms.insert(room.id.clone(), (room, focus, siblings));
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
                            tab.reset_attach();
                            if !tab.closing {
                                tab.status = TabView::runtime_status(&rec.runtime);
                            }
                        }
                    }
                    tab
                })
                .collect();
            let mut current = RoomView::from_record(room, tabs);
            if let Some((previous, focus, siblings)) = prev_rooms.get(&room.id) {
                if pane_drag.as_ref().is_some_and(|drag| {
                    drag.host_id == machine.profile.id
                        && drag.room_id == room.id
                        && !drag.preserve_preview(&previous.layout, &mut current.layout)
                }) {
                    *pane_drag = None;
                }
                current.preserve_selection(previous, focus.as_deref(), siblings);
            } else {
                current.ensure_selectable_focus();
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
    let current = active_room_id.as_ref()
        .and_then(|id| machine.rooms.iter().find(|room| room.id == *id));
    let first_selectable = machine.rooms.iter().find(|room| room.has_selectable_tabs());
    if active_host_id == host_id {
        if !current.is_some_and(RoomView::has_selectable_tabs) {
            *active_room_id = first_selectable.or(current).or_else(|| machine.rooms.first())
                .map(|room| room.id.clone());
        }
    } else if was_empty
        && active_room_id.is_none()
        && let Some(first) = first_selectable
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
        mut bridge: HostBridge,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let inbox = bridge.take_inbox();
        let bridge_task = Self::listen_bridge(inbox, window, cx);
        let perf_task = perf_enabled().then(|| {
            cx.spawn(async |_, cx| {
                let executor = cx.background_executor().clone();
                loop {
                    executor.timer(Duration::from_secs(1)).await;
                    let mut sample = PERF.with_borrow_mut(std::mem::take);
                    let panes = std::mem::take(&mut sample.panes);
                    eprintln!("ddg-native-perf {sample:?}");
                    for (pane, sample) in panes {
                        eprintln!("ddg-native-perf pane={pane} {sample:?}");
                    }
                }
            })
        });
        let spinner_slots = SpinnerSlots::default();
        let spinner = cx.new(|_| SpinnerClock {
            slots: spinner_slots.clone(),
            frame: 0,
        });
        let active_host_id = hosts.default_profile().id;
        let ui_scale = UiScale::from_percent(prefs.ui_scale_pct);
        window.set_rem_size(px(ui_scale.rem_px()));
        cx.observe_window_activation(window, |this, window, cx| {
            this.window_active = window.is_window_active();
            if !this.window_active {
                this.spinner_task = None;
                this.cancel_terminal_mouse();
                this.pending_paste = None;
                this.terminal_input = None;
            }
            this.mark_active_seen();
            notify(cx, "activation");
        })
        .detach();
        cx.observe_window_bounds(window, |this, _, _| this.cancel_terminal_mouse())
            .detach();
        let this = Self {
            #[cfg(test)]
            test_input_sink: None,
            #[cfg(test)]
            test_tab_requests: Vec::new(),
            assets,
            agents,
            agent_error,
            programs: BUILTIN_PROGRAMS.clone(),
            programs_warned: false,
            peek: None,
            window_active: window.is_window_active(),
            focus_zone: KeyboardFocusZone::Terminal,
            terminal_input: None,
            terminal_mouse: None,
            terminal_pointer: Point::default(),
            terminal_autoscroll: None,
            pending_paste: None,
            swallow_paste_enter_repeat: false,
            agent_cursor: 0,
            hovered_agent: None,
            keyboard_group: None,
            agent_ui: AgentUiCache::default(),
            spinner,
            spinner_slots,
            spinner_task: None,
            _bridge_task: bridge_task,
            _perf_task: perf_task,
            zoom_expiry: None,
            external_input_expiry: None,
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
            preset_control: None,
            preset_connection: None,
            update: UpdateController::default(),
            update_modal: None,
            last_local_change: None,
            hovered_room: None,
            hovered_machine: None,
            notice: None,
            external_input_indicator: None,
            bridge,
            focus: cx.focus_handle(),
            sidebar_base: 190.,
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

    fn listen_bridge(mut inbox: BridgeInbox, window: &Window, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn_in(window, async move |desktop, cx| {
            let executor = cx.background_executor().clone();
            let mut next_frame = None;
            while let Some(first) = inbox.recv().await {
                if next_frame.is_none() {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    if cx
                        .update(|window, _| {
                            window.on_next_frame(move |_, _| {
                                let _ = tx.send(());
                            })
                        })
                        .is_err()
                    {
                        break;
                    }
                    next_frame = Some(rx);
                }
                // Occluded/minimized windows can stop producing frames. Keep the host
                // state current without stacking up callbacks or polling an empty inbox.
                let frame = next_frame.as_mut().unwrap();
                tokio::select! {
                    _ = frame => next_frame = None,
                    _ = executor.timer(Duration::from_millis(32)) => {},
                }
                if desktop
                    .update(cx, |this, cx| this.drain(first, &mut inbox, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
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
        self.apply_ui_scale(scale, window, cx);
        if changed {
            self.pending_ui_scale = None;
            self.ui_prefs.ui_scale_pct = scale.percent();
            if let Some(path) = &self.ui_prefs_path
                && let Err(err) = self.ui_prefs.save(path)
            {
                eprintln!("could not save UI zoom to {}: {err}", path.display());
            }
        }
        notify(cx, "zoom");
    }

    fn apply_ui_scale(&mut self, scale: UiScale, window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_indicator = Some((scale.percent(), Instant::now() + ZOOM_INDICATOR_DURATION));
        self.zoom_expiry = Some(cx.spawn(async |desktop, cx| {
            cx.background_executor()
                .timer(ZOOM_INDICATOR_DURATION)
                .await;
            let _ = desktop.update(cx, |this, cx| {
                if this.zoom_indicator.take().is_some() {
                    notify(cx, "zoom_expiry");
                }
            });
        }));
        if scale != self.ui_scale {
            // The dragged dividers' original positions no longer match the layout after zoom.
            self.ui_scale = scale;
            self.cancel_terminal_mouse();
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
        self.agent_ui.invalidate();
        changes.discard_stale(&mut self.hovered_machine, String::as_str);
        changes.discard_stale(&mut self.hovered_room, |key| &key.host_id);
        if matches!(&self.active_overlay, Some(ActiveOverlay::RenameModal(key, _))
            if changes.removed.contains(&key.host_id) || changes.reconnect.iter().any(|p| p.id == key.host_id))
        {
            self.active_overlay = None;
        }
        changes.discard_stale(&mut self.pane_drag, |drag| &drag.host_id);
        changes.discard_stale(&mut self.external_input_indicator, |badge| &badge.host_id);
        if self.external_input_indicator.is_none() {
            self.external_input_expiry = None;
        }
        changes.discard_stale(&mut self.pending_select, |(key, _)| &key.host_id);
        if self.peek.as_ref().is_some_and(|p| {
            changes.removed.contains(&p.host)
                || changes.reconnect.iter().any(|profile| profile.id == p.host)
        }) {
            self.close_peek(false);
        }
        let detect_host = self.agent_modal().and_then(|modal| {
            if changes.removed.contains(&modal.detect_host) {
                Some(self.active_host_id.clone())
            } else if changes.reconnect.iter().any(|p| p.id == modal.detect_host) {
                Some(modal.detect_host.clone())
            } else {
                None
            }
        });
        for id in changes.removed {
            self.bridge.remove_machine(&id);
        }
        for profile in changes.reconnect {
            self.bridge.add_or_update_machine(profile);
        }
        if let Some(host) = detect_host {
            self.request_preset_detection(host);
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

    fn has_selectable_active_room(&self) -> bool {
        self.room().is_some_and(RoomView::has_selectable_tabs)
    }

    fn is_closing_last_room(&self) -> bool {
        self.room().is_some_and(|room| !room.tabs.is_empty() && !room.has_selectable_tabs())
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
        self.agent_ui.invalidate();
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
        if let Some(room) = self.room().filter(|room| room.has_selectable_tabs()) {
            if room.tabs.len() >= MAX_TABS_PER_ROOM {
                return;
            }
            self.bridge.create_tab(&hid, room.id.clone(), cols, rows);
        } else {
            self.bridge.create_room(&hid, cols, rows);
        }
    }

    fn close_focused_tab(&mut self, window: &Window, cx: &App) {
        if !self.is_active_connected() {
            return;
        }
        let hid = self.active_host_id.clone();
        let Some(room) = self.room_mut() else {
            return;
        };
        let Some(tab) = room.focused_tab_mut() else {
            return;
        };
        tab.closing = true;
        tab.status = "Closing".into();
        // Any late attach response must be detached rather than installed.
        tab.reset_attach();
        let tab_id = tab.id.clone();
        let mut leaves = Vec::new();
        if let Some(group) = room.active_group() {
            group.leaf_ids(&mut leaves);
        }
        if let Some(index) = leaves.iter().position(|id| *id == tab_id) {
            leaves[index + 1..].iter().chain(leaves[..index].iter().rev())
                .any(|id| room.select_tab_by_id(id));
        }
        if !room.ensure_selectable_focus() {
            let next = self.active_machine().and_then(|machine| {
                machine.rooms.iter().find(|room| room.has_selectable_tabs())
            });
            if let Some(next) = next {
                self.active_room_id = Some(next.id.clone());
                if let Some(room) = self.room_mut() {
                    room.ensure_selectable_focus();
                }
            }
        }
        if self.pending_select.as_ref().is_some_and(|(key, id)| key.host_id == hid && *id == tab_id) {
            self.pending_select = None;
        }
        self.agent_ui.invalidate();
        self.sync_terminal_interaction(Some(window), cx);
        #[cfg(test)]
        self.test_tab_requests.push(TestTabRequest::Close(hid.clone(), tab_id.clone()));
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
        let Some(group) = room.projected_active_group() else {
            return Default::default();
        };
        let bounds = self.canvas_bounds();
        if let Some(id) = room.zoomed_tab_id.as_ref().filter(|id| group.contains(id)) {
            return (vec![(id.clone(), bounds)], Vec::new());
        }
        let (panes, mut dividers) = compute_layout(&group, bounds, self.ui_scale.px(PANE_DIVIDER));
        if room.active_group() != Some(&group) {
            // Projected divider paths may point at a different split in the raw layout.
            dividers.clear();
        }
        (panes, dividers)
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
        let (Some(group), Some(anchor)) = (room.projected_active_group(), room.focused_tab_id()) else {
            return;
        };
        let divider = self.ui_scale.px(PANE_DIVIDER);
        let (panes, _) = compute_layout(&group, self.canvas_bounds(), divider);
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
        let projected = room.projected_active_group();
        if !matches!(projected, Some(PaneNode::Split { .. }))
            || projected.as_ref() != room.active_group()
        {
            return;
        }
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
            && let Some(group) = room.projected_active_group()
            && let Some(id) = room.focused_tab_id()
            && let Some(target) =
                navigate_direction(&compute_layout(&group, bounds, divider).0, id, dir)
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
            #[cfg(test)]
            self.test_tab_requests.push(TestTabRequest::Attach(hid.clone(), room_id.clone(), tab_id.clone()));
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

    fn handle_file_drop(
        &mut self,
        tab_id: &str,
        paths: &[PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.file_drop_blocked() || !self.focus_pane(tab_id, window) {
            return;
        }
        if let Some(target) = self.drop_target(tab_id) {
            let formatted = format_dropped_paths(paths);
            self.send_terminal_text(&target, formatted.as_bytes(), cx);
        }
    }

    fn begin_pane_drag(&mut self, hit: &DividerHit, position: Point<Pixels>) {
        if !self.is_active_connected() || hit.parent_extent <= 0. {
            return;
        }
        if !self.room().is_some_and(|room| {
            room.active_group() == room.projected_active_group().as_ref()
        }) {
            return;
        }
        self.cancel_terminal_mouse();
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

    fn end_resize_drag(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal_mouse_up(event, window, cx);
        if event.button == MouseButton::Left {
            self.sidebar_drag = None;
            if self.finish_pane_drag() {
                cx.notify();
            }
        }
    }

    fn navigable_rooms(&self) -> Vec<RoomKey> {
        self.machines
            .iter()
            .filter(|machine| machine.is_connected() && machine.catalog_seq.is_some())
            .flat_map(|machine| {
                machine.rooms.iter().filter(|room| room.has_selectable_tabs()).map(|room| RoomKey {
                    host_id: machine.profile.id.clone(),
                    room_id: room.id.clone(),
                })
            })
            .collect()
    }

    fn next_room_target(&self, dir: RoomNavDir) -> Option<RoomKey> {
        let rooms = self.navigable_rooms();
        let len = rooms.len();
        if len == 0 {
            return None;
        }
        if let Some(index) = rooms.iter().position(|room| {
            room.host_id == self.active_host_id && Some(&room.room_id) == self.active_room_id.as_ref()
        }) {
            if len == 1 {
                return None;
            }
            let next = match dir {
                RoomNavDir::Previous => (index + len - 1) % len,
                RoomNavDir::Next => (index + 1) % len,
            };
            return Some(rooms[next].clone());
        }
        let machine_index = self
            .machines
            .iter()
            .position(|machine| machine.profile.id == self.active_host_id);
        let adjacent = machine_index.and_then(|index| match dir {
            RoomNavDir::Previous => self.machines[..index].iter().rev().find_map(|machine| {
                rooms.iter().rfind(|room| room.host_id == machine.profile.id)
            }),
            RoomNavDir::Next => self.machines[index + 1..].iter().find_map(|machine| {
                rooms.iter().find(|room| room.host_id == machine.profile.id)
            }),
        });
        let wrap = match dir {
            RoomNavDir::Previous => &rooms[len - 1],
            RoomNavDir::Next => &rooms[0],
        };
        Some(adjacent.unwrap_or(wrap).clone())
    }

    fn switch_room(
        &mut self,
        dir: RoomNavDir,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.pane_drag.is_some() {
            return false;
        }
        let Some(target) = self.next_room_target(dir) else {
            return false;
        };
        self.active_host_id.clone_from(&target.host_id);
        self.active_room_id = Some(target.room_id);
        self.sync_terminal_interaction(Some(window), cx);
        if self.section_collapsed("section:rooms") {
            self.toggle_section("section:rooms");
        }
        if let Some(machine) = self.machine_mut(&target.host_id) {
            machine.collapsed = false;
        }
        self.ensure_selected_attached();
        self.focus.focus(window);
        true
    }

    fn target_is_closing(&self, host_id: &str, tab_id: &str) -> bool {
        self.machine(host_id)
            .is_some_and(|machine| machine.rooms.iter().any(|room| room.is_tab_closing(tab_id)))
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
        self.agent_ui.invalidate();
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
                status: "Exited".into(),
            };
            peek.tab.status = "Background session exited".into();
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
            #[cfg(test)]
            self.test_tab_requests.push(TestTabRequest::Detach(host_id.to_owned(), ch));
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
            && !self.target_is_closing(&key.host_id, &tab_id)
            && (key.host_id != host_id || !self.select_room_and_tab(&key, &tab_id))
        {
            self.pending_select = Some((key, tab_id));
        }
        self.ensure_selected_attached();
    }

    fn handle_host_event(
        &mut self,
        host_id: &str,
        generation: Option<u64>,
        event: BridgeEvent,
        inbox: &mut BridgeInbox,
        cx: &mut Context<Self>,
    ) {
        match event {
            BridgeEvent::AgentPresetsDetected {
                request_id,
                connection,
                result,
            } => {
                if self.bridge.preset_connection(host_id) == connection
                    && self.agent_modal_mut().is_some_and(|modal| {
                        modal.apply_preset_detection(host_id, request_id, result)
                    })
                {
                    self.preset_connection = connection;
                }
            }
            BridgeEvent::VersionInfo(version) => {
                if let Some(machine) = self.machine_mut(host_id) {
                    machine.remote_update.version = version;
                }
            }
            BridgeEvent::StateChanged(state) => {
                self.agent_ui.invalidate();
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
                        for room in &mut machine.rooms {
                            for tab in &mut room.tabs {
                                if tab.closing {
                                    tab.closing = false;
                                    tab.status = TabView::runtime_status(&tab.runtime);
                                }
                                tab.channel = None;
                                tab.activity = None;
                                tab.pending_seen_seq = 0;
                                tab.reset_attach();
                            }
                            room.ensure_selectable_focus();
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
                        self.external_input_expiry = None;
                    }
                } else {
                    if host_id == self.active_host_id {
                        self.notice = None;
                    }
                    if let Some(ui_path) = &self.ui_prefs_path
                        && let Ok(agents_path) = AgentsConfig::default_path()
                    {
                        self.bridge.start_preset_seed(host_id, ui_path.clone(), agents_path);
                    }
                }
                if self.agent_modal().is_some_and(|modal| modal.detect_host == host_id) {
                    self.request_preset_detection(host_id.to_owned());
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
                    self.agent_ui.invalidate();
                    self.mark_active_seen();
                }
            }
            BridgeEvent::CatalogUpdated { seq, catalog } => {
                self.apply_catalog(host_id, seq, catalog)
            }
            BridgeEvent::SelectRoomAndTab { room_id, tab_id } => {
                let key = RoomKey {
                    host_id: host_id.to_owned(),
                    room_id,
                };
                if self.target_is_closing(host_id, &tab_id) {
                    return;
                }
                let promoted = self.peek.as_ref().is_some_and(|p| {
                    p.host == host_id && p.tab.id == tab_id && p.room != key.room_id
                });
                self.close_peek(promoted);
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
                        let message = "Failed to attach tab: Initial terminal output is too large. Select the tab again.";
                        tab.attach_failed(message);
                        self.notice = Some(message.into());
                        self.bridge.detach_channel(host_id, handle.channel);
                        return;
                    }
                    let stale = accept_tab_attach(Some(tab), &handle);
                    if stale != Some(handle.channel)
                        && tab.channel == Some(handle.channel)
                        && tab.vt.is_some()
                    {
                        early = std::mem::take(&mut tab.early_frames);
                    }
                    tab.reset_attach();
                    let failed = if tab.channel == Some(handle.channel) && tab.vt.is_none() {
                        tab.channel = None;
                        tab.mode = ControlMode::Readonly;
                        tab.status = "Could not create the terminal screen.".into();
                        Some(handle.channel)
                    } else {
                        None
                    };
                    for ch in stale.into_iter().chain(failed) {
                        self.bridge.detach_channel(host_id, ch);
                    }
                } else {
                    self.bridge.detach_channel(host_id, handle.channel);
                }
                if retry {
                    if self.tab_is_visible(host_id, &handle.tab_id) {
                        self.ensure_selected_attached();
                    }
                } else {
                    // Only the accepted channel's early frames belong to this VT.
                    early.retain(|(channel, _)| *channel == handle.channel);
                    inbox.replay(host_id, generation, &handle.tab_id, early);
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
                    let remaining = badge.expires_at.saturating_duration_since(Instant::now());
                    self.external_input_indicator = Some(badge);
                    self.external_input_expiry = Some(cx.spawn(async move |desktop, cx| {
                        cx.background_executor().timer(remaining).await;
                        let _ = desktop.update(cx, |this, cx| {
                            if this.external_input_indicator.take().is_some() {
                                notify(cx, "external_input_expiry");
                            }
                        });
                    }));
                }
            }
            BridgeEvent::SessionExited {
                session_id,
                room_id,
                run_generation,
                exit_code,
            } => {
                self.agent_ui.invalidate();
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
                        Some(0) => "Exited cleanly".into(),
                        Some(code) => format!("Exit code {code}"),
                        None => "Exited".into(),
                    };
                    tab.status.clone_from(&status);
                    tab.runtime = TabRuntimeState::Exited { status };
                    tab.reset_attach();
                    tab.mode = ControlMode::Readonly;
                }
            }
            BridgeEvent::TabControlChanged { tab_id, mode } => {
                if let Some(tab) = self.find_tab_mut(host_id, &tab_id) {
                    if tab.mode != mode {
                        tab.surface.scroll.reset();
                    }
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
            BridgeEvent::TabStream { .. } => unreachable!("stream frames are dispatched by drain"),
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
                self.notice = Some(message.clone());
                let restore_focus =
                    self.active_host_id == host_id && !self.has_selectable_active_room();
                let restored = self.machine_mut(host_id).and_then(|machine| {
                    machine.rooms.iter_mut().find_map(|room| {
                        let tab = room.tabs.iter_mut().find(|tab| tab.id == tab_id)?;
                        tab.closing = false;
                        tab.status.clone_from(&message);
                        Some(room.id.clone())
                    })
                });
                if let Some(room_id) = restored {
                    self.agent_ui.invalidate();
                    if restore_focus {
                        let key = RoomKey { host_id: host_id.to_owned(), room_id };
                        self.select_room_and_tab(&key, &tab_id);
                    }
                    if self.tab_is_visible(host_id, &tab_id) {
                        self.ensure_selected_attached();
                    }
                }
            }
        }
    }

    fn handle_tab_frame(
        &mut self,
        host_id: &str,
        tab_id: &str,
        channel: u32,
        frame: TabFrame,
    ) -> bool {
        let peeking = self
            .peek
            .as_ref()
            .is_some_and(|p| p.host == host_id && p.tab.id == tab_id);
        let Some(tab) = self.find_tab_mut(host_id, tab_id) else {
            return false;
        };
        if tab.channel != Some(channel) {
            if tab.attaching {
                tab.buffer_frame(channel, frame);
            }
            return false;
        }
        match frame {
            TabFrame::Data(bytes) => {
                let Some(vt) = tab.vt.as_mut().filter(|_| !bytes.is_empty()) else {
                    return false;
                };
                vt.write(&bytes);
                drop(bytes); // Return stream credit only after applying the payload.
                let replies = vt.replies();
                if !replies.is_empty() && tab.mode == ControlMode::Control && !tab.closing {
                    self.bridge.send_input(host_id, channel, &replies);
                }
            }
            TabFrame::Resize(cols, rows) => {
                if tab.size == (cols, rows) {
                    return false;
                }
                tab.size = (cols, rows);
                if let Some(vt) = &mut tab.vt {
                    vt.resize(cols, rows);
                }
            }
            TabFrame::Close(reason) => {
                tab.channel = None;
                tab.reset_attach();
                tab.surface.scroll.reset();
                tab.mode = ControlMode::Readonly;
                let exited = peeking
                    && match gpui::private::serde_json::from_str::<CloseReason>(&reason) {
                        Ok(CloseReason::Exited { exit_code, .. }) => {
                            tab.status = exit_code.map_or_else(
                                || "Background session exited".into(),
                                |code| format!("Background session exited · Exit code {code}"),
                            );
                            true
                        }
                        Ok(CloseReason::Killed { .. }) => {
                            tab.status = "Background session exited".into();
                            true
                        }
                        _ => false,
                    };
                if exited {
                    let generation = tab.run_generation;
                    tab.runtime = TabRuntimeState::Exited {
                        status: tab.status.clone(),
                    };
                    self.agent_ui.invalidate();
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
        true
    }

    fn drain(&mut self, first: QueuedMessage, inbox: &mut BridgeInbox, cx: &mut Context<Self>) {
        let started = Instant::now();
        let mut bytes = 0;
        let mut messages = 0;
        let mut depth = 0;
        let mut stream_changed = false;
        let mut metadata_changed = false;
        let mut first = Some(first);
        loop {
            if perf_enabled() {
                depth = depth.max(inbox.len() + usize::from(first.is_some()));
            }
            let Some(QueuedMessage { message, queued_at }) =
                first.take().or_else(|| inbox.try_recv())
            else {
                break;
            };
            messages += 1;
            bytes += message.data_bytes();
            if let Some(queued_at) = queued_at {
                let wait = queued_at.elapsed().as_micros();
                perf(|p| {
                    p.queued_samples += 1;
                    p.queue_wait_us_sum += wait;
                    p.queue_wait_us_max = p.queue_wait_us_max.max(wait);
                });
            }
            if self.bridge.is_current(&message) {
                match message {
                    BridgeMessage::Host(
                        host,
                        _,
                        BridgeEvent::TabStream {
                            tab_id,
                            channel,
                            frame,
                        },
                    ) => {
                        stream_changed |= self.handle_tab_frame(&host, &tab_id, channel, frame)
                            && self.tab_is_visible(&host, &tab_id);
                    }
                    event => metadata_changed |= self.apply_bridge_message(event, inbox, cx),
                }
                metadata_changed |= self.sync_terminal_interaction(None, cx);
            }
            if bytes > BRIDGE_BYTE_BUDGET - ddg_agents_proto::frame::DATA_CHUNK
                || messages >= BRIDGE_MESSAGE_BUDGET
                || started.elapsed() >= BRIDGE_TIME_BUDGET
            {
                perf(|p| p.budget_yields += u64::from(inbox.len() != 0));
                break;
            }
        }
        perf(|p| {
            p.drains += 1;
            p.empty_drains += u64::from(messages == 0);
            p.messages += messages as u64;
            p.data_bytes += bytes as u64;
            p.queue_depth_max = p.queue_depth_max.max(depth);
        });
        if stream_changed || metadata_changed {
            notify(
                cx,
                match (stream_changed, metadata_changed) {
                    (true, true) => "bridge_mixed",
                    (true, false) => "bridge_stream",
                    _ => "bridge_metadata",
                },
            );
        }
    }

    fn apply_bridge_message(
        &mut self,
        event: BridgeMessage,
        inbox: &mut BridgeInbox,
        cx: &mut Context<Self>,
    ) -> bool {
        match event {
            BridgeMessage::UpdateResult {
                req_id,
                machine_id,
                result,
            } => self.finish_update(req_id, machine_id, result, cx),
            BridgeMessage::HostsConfigReloaded(cfg) => return self.apply_hosts_config(cfg),
            BridgeMessage::UiPrefsReloaded(prefs) => {
                let scale = UiScale::from_percent(prefs.ui_scale_pct);
                self.pending_ui_scale = (scale != self.ui_scale).then_some(scale);
                if self.ui_prefs.agent_sort != prefs.agent_sort
                    || self.ui_prefs.pinned_agents != prefs.pinned_agents
                    || self.ui_prefs.manual_agent_order != prefs.manual_agent_order
                {
                    self.agent_ui.invalidate();
                }
                let changed = self.ui_prefs != prefs || self.pending_ui_scale.is_some();
                self.ui_prefs = prefs;
                return changed;
            }
            BridgeMessage::AgentsReloaded(Ok(cfg)) => {
                if self.agents != cfg
                    && let Some(modal) = self.agent_modal_mut().filter(|m| !m.blocked)
                {
                    modal.conflict = true;
                }
                self.agent_ui.invalidate();
                self.agents = cfg;
                self.agent_error = None;
            }
            BridgeMessage::AgentsReloaded(Err(err)) => {
                self.agent_error = Some(err);
                if let Some(modal) = self.agent_modal_mut().filter(|m| !m.blocked) {
                    modal.conflict = true;
                }
            }
            BridgeMessage::ProgramsReloaded(Ok(cfg)) => {
                self.agent_ui.invalidate();
                self.programs = cfg;
            }
            BridgeMessage::ProgramsReloaded(Err(err)) => {
                if !self.programs_warned {
                    self.programs_warned = true;
                    self.notice = Some(err);
                }
            }
            BridgeMessage::Host(host_id, generation, event) => {
                self.handle_host_event(&host_id, generation, event, inbox, cx)
            }
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
        true
    }

    fn save_ui_prefs(&mut self) {
        if let Some(path) = &self.ui_prefs_path
            && let Err(err) = self.ui_prefs.save(path)
        {
            self.notice = Some(format!("Failed to save UI settings: {err}"));
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

    fn programs_for(&self, machine: Option<&MachineView>) -> &ProgramsConfig {
        if machine.is_some_and(|m| matches!(m.profile.transport, HostTransport::Local { .. })) {
            &self.programs
        } else {
            &BUILTIN_PROGRAMS
        }
    }

    fn named_program_icon(programs: &ProgramsConfig, key: &str) -> ResolvedProgramIcon {
        let program = programs
            .programs
            .iter()
            .chain(&BUILTIN_PROGRAMS.programs)
            .find(|p| p.icon == key);
        ResolvedProgramIcon::new(key, program)
    }

    fn resolve_tab_identity(&self, machine: &MachineView, tab: &TabView) -> TabIdentity {
        let local = matches!(machine.profile.transport, HostTransport::Local { .. });
        let programs = self.programs_for(Some(machine));
        let by_comm = |name: &str| programs.find_by_token(name, |m| &m.comms);
        let profile = tab
            .agent_profile
            .as_deref()
            .and_then(|id| self.agents.find(id));
        let explicit_icon = profile
            .and_then(|p| p.icon.as_deref())
            .filter(|key| local || (!key.ends_with(".svg") && !key.ends_with(".png")));
        let profile_argv = profile.map(|p| {
            p.host_overrides
                .get(&machine.profile.id)
                .and_then(|o| o.argv.as_ref())
                .unwrap_or(&p.argv)
        });
        let profile_launcher = || {
            profile_argv
                .and_then(|argv| argv.first())
                .and_then(|raw| process_arg_name(raw, true))
        };
        let app = tab
            .activity
            .as_ref()
            .filter(|_| tab.is_running())
            .and_then(|a| a.agent_status.as_ref())
            .and_then(|status| status.app.as_deref())
            .and_then(|raw| {
                let name = short_name(raw, 64)?;
                let program = programs.find_by_token(raw, |m| &m.osc_apps);
                Some((name, program))
            });
        let command_name = |name: String, program: Option<&ProgramDefinition>| {
            program
                .and_then(|p| p.command.as_deref())
                .and_then(|command| short_name(command, 64))
                .unwrap_or(name)
        };
        let from_program = |name: String, program: Option<&ProgramDefinition>| TabIdentity {
            name: name.into(),
            icon: ResolvedProgramIcon::new(program.map_or("", |p| p.icon.as_str()), program),
        };
        let from_app = |(name, program)| from_program(command_name(name, program), program);

        let foreground = tab.activity.as_ref().and_then(|a| a.foreground.as_ref());
        if let Some(fg) = foreground {
            let comm = normalize_process_name(&fg.comm);
            let script = fg
                .script_hint
                .as_deref()
                .and_then(|raw| process_arg_name(raw, false));
            let evidence = if fg.script_hint.is_some() {
                script.as_deref()
            } else {
                comm.as_deref()
            };
            let mut identity = if let Some(script) = script.as_ref() {
                let program = programs
                    .find_by_token(script, |m| &m.script_hints)
                    .or_else(|| {
                        programs.find_by_token(strip_script_extension(script), |m| &m.script_hints)
                    })
                    .or_else(|| {
                        programs.find_by_token(fg.script_hint.as_deref()?, |m| &m.script_hints)
                    });
                let mut identity = from_program(command_name(script.clone(), program), program);
                if program.is_none() {
                    identity.icon = Self::named_program_icon(programs, "program");
                }
                identity
            } else if let Some(comm) = comm.as_ref() {
                let program = by_comm(comm).or_else(|| by_comm(&fg.comm));
                if fg.script_hint.is_none()
                    && !program.is_some_and(|p| p.shell)
                    && is_interpreter_name(comm)
                {
                    app.map_or_else(|| from_program(comm.clone(), program), from_app)
                } else {
                    from_program(comm.clone(), program)
                }
            } else {
                from_program("Terminal".into(), None)
            };
            if let Some(key) = explicit_icon
                && let Some(launcher) = profile_launcher()
            {
                let matches_launcher =
                    evidence.is_some_and(|name| name.eq_ignore_ascii_case(&launcher));
                let matches_wrapped_script = comm
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(&launcher))
                    && script.as_deref().is_some_and(|script| {
                        profile_argv
                            .and_then(|argv| argv.get(1))
                            .and_then(|raw| process_arg_name(raw, false))
                            .is_some_and(|configured| configured.eq_ignore_ascii_case(script))
                    });
                if matches_launcher || matches_wrapped_script {
                    identity.icon = Self::named_program_icon(programs, key);
                }
            }
            return identity;
        }

        let mut identity = app
            .map(from_app)
            .or_else(|| {
                let argv0 = process_arg_name(tab.argv0.as_deref()?, true)?;
                let program = by_comm(&argv0);
                Some(from_program(argv0, program))
            })
            .or_else(|| {
                let id = tab.agent_profile.as_deref()?;
                let program = programs
                    .find_by_token(id, |m| &m.agent_profiles)
                    .or_else(|| profile_launcher().as_deref().and_then(by_comm));
                let command = program
                    .and_then(|p| p.command.as_deref())
                    .and_then(|command| short_name(command, 64));
                let label = profile.and_then(|p| short_name(&p.label, DISPLAY_TEXT_LIMIT));
                let preset_label = profile
                    .zip(find_agent_preset(id))
                    .is_some_and(|(p, preset)| p.label == preset.label);
                let name = if preset_label {
                    command.or(label)
                } else {
                    label.or(command)
                }
                .or_else(|| program.and_then(|p| short_name(&p.id, 64)))?;
                Some(from_program(name, program))
            })
            .or_else(|| {
                tab.title
                    .starts_with("π - ")
                    .then(|| from_program("pi".into(), by_comm("pi")))
            })
            .unwrap_or_else(|| from_program("Terminal".into(), None));
        if let Some(key) = explicit_icon {
            identity.icon = Self::named_program_icon(programs, key);
        }
        identity
    }

    fn top_tab_label(&self, machine: &MachineView, tab: &TabView) -> TabIdentity {
        self.agent_ui
            .tab_labels
            .borrow_mut()
            .entry(machine.profile.id.clone())
            .or_default()
            .entry(tab.id.clone())
            .or_insert_with(|| self.resolve_tab_identity(machine, tab))
            .clone()
    }

    fn display_agent_icon(
        &self,
        host: &str,
        icon_style: &ResolvedProgramIcon,
        size: f32,
    ) -> AnyElement {
        let remote = self
            .machine(host)
            .is_none_or(|m| !matches!(m.profile.transport, HostTransport::Local { .. }));
        let dir = self
            .ui_prefs_path
            .as_ref()
            .map(|path| path.with_file_name("icons"));
        let path = self
            .assets
            .resolve_icon_key(dir.as_deref(), &icon_style.key, remote);
        if path == Icon::Terminal.path() {
            icon(Icon::Terminal, size, rgb(0xababab)).into_any_element()
        } else if icon_style.tint && !path.starts_with("custom/") {
            svg()
                .path(path)
                .size(px(size))
                .flex_none()
                .text_color(rgb(icon_style.tint_color))
                .into_any_element()
        } else {
            img(ImageSource::Resource(Resource::Embedded(path)))
                .size(px(size))
                .flex_none()
                .into_any_element()
        }
    }

    fn tab_status_badge(&self, state: AgentState, unread: bool) -> AnyElement {
        let size = self.ui_scale.px(11.);
        match state {
            AgentState::Working if self.spinner_task.is_some() => {
                let slots = self.spinner_slots.clone();
                canvas(
                    move |bounds, window, _| {
                        let mask = window.content_mask();
                        if mask.bounds.intersects(&bounds) {
                            slots.borrow_mut().push((bounds, mask));
                        }
                    },
                    |_, _, _, _| (),
                )
                .size(px(size))
                .flex_none()
                .into_any_element()
            }
            AgentState::Working => icon(Icon::Spinner, size, rgb(0x92b6ff)).into_any_element(),
            AgentState::Blocked => div()
                .text_color(rgb(0xf2ca80))
                .child("!")
                .into_any_element(),
            AgentState::Error => div()
                .text_color(rgb(0xff9292))
                .child("Error")
                .into_any_element(),
            AgentState::Done if unread => div()
                .size(px(self.ui_scale.px(5.)))
                .rounded_full()
                .bg(rgb(0x8ad3a6))
                .into_any_element(),
            _ => div().into_any_element(),
        }
    }

    fn agent_rows(&self) -> Rc<Vec<Row>> {
        self.agent_ui
            .tree
            .get_or_init(|| {
                perf(|p| p.sidebar_rebuilds += 1);
                Rc::new(self.build_agent_rows())
            })
            .clone()
    }

    fn build_agent_rows(&self) -> Vec<Row> {
        let mut entries = Vec::new();
        for machine in self.machines.iter().filter(|m| m.is_connected()) {
            let host = &machine.profile.id;
            let rows = machine
                .rooms
                .iter()
                .flat_map(|room| {
                    room.tabs
                        .iter()
                        .filter(|tab| tab.is_selectable())
                        .map(move |tab| (&room.id, room.label(), tab, false))
                })
                .chain(
                    machine
                        .background
                        .iter()
                        .map(|(room, tab)| (room, "Background".into(), tab, true)),
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
                    icon_key: self.top_tab_label(machine, tab).icon.key,
                });
            }
        }
        build_agent_tree(entries, &self.ui_prefs)
    }

    fn visible_agent_rows<'a>(&self, rows: &'a [Row]) -> impl Iterator<Item = &'a Row> {
        rows.iter().filter(|row| {
            self.ui_prefs.agent_sort != AgentSortMode::Grouped
                || row.pinned
                || !self.section_collapsed(&format!("group:{}:{}", row.entry.host, row.entry.room))
        })
    }

    fn has_visible_working_spinner(&self) -> bool {
        // An opaque Desktop overlay must cover the original static icon, not a
        // sibling-layer icon. Deactivation likewise needs no animation timer.
        if !self.window_active || self.active_overlay.is_some() || self.update_modal.is_some() {
            return false;
        }
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

    fn sync_spinner(&mut self, cx: &mut Context<Self>) {
        if !self.has_visible_working_spinner() {
            self.spinner_task = None;
        } else if self.spinner_task.is_none() {
            self.spinner_task = Some(self.spinner.update(cx, |_, cx| {
                cx.spawn(async |clock, cx| {
                    let executor = cx.background_executor().clone();
                    loop {
                        executor.timer(SPINNER_INTERVAL).await;
                        if clock
                            .update(cx, |clock, cx| {
                                if !clock.slots.borrow().is_empty() {
                                    clock.frame = (clock.frame + 1) % 8;
                                    notify(cx, "spinner");
                                }
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
            }));
        }
    }

    fn action_button(
        cx: &mut Context<Self>,
        id: impl Into<ElementId>,
        label: impl IntoElement,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        Self::action_button_with_style(
            cx,
            id,
            label,
            true,
            |button| button.hover(|s| s.bg(rgb(0x394354))),
            action,
        )
    }

    fn action_button_with_style(
        cx: &mut Context<Self>,
        id: impl Into<ElementId>,
        label: impl IntoElement,
        enabled: bool,
        style: impl FnOnce(Stateful<Div>) -> Stateful<Div>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        style(div().id(id).px_1().py_0p5().rounded_sm().text_xs())
            .child(label)
            .when(enabled, |button| {
                button.cursor_pointer().on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    action(this, window, cx);
                    cx.notify();
                }))
            })
    }

    // GPUI accepts only one hover refinement per element.
    fn segmented_choice(
        button: Stateful<Div>,
        selected: bool,
        enabled: bool,
        hover_bg: u32,
    ) -> Stateful<Div> {
        let selected = enabled && selected;
        let color = if selected {
            0x8db7ff
        } else if enabled {
            0x9a9a9a
        } else {
            0x858a92
        };
        button
            .text_color(rgb(color))
            .border_b_1()
            .border_color(rgba(if selected { 0x8db7ffff } else { 0x00000000 }))
            .when(selected, |button| button.font_weight(FontWeight::SEMIBOLD))
            .when(enabled, |button| {
                button.hover(|s| s.bg(rgb(hover_bg)).text_color(rgb(0xffffff)))
            })
    }

    fn primary_action(button: Stateful<Div>, enabled: bool) -> Stateful<Div> {
        button
            .text_color(rgb(if enabled { 0x8db7ff } else { 0x9a9a9a }))
            .when(enabled, |button| {
                button.font_weight(FontWeight::SEMIBOLD)
                    .hover(|s| s.bg(rgb(0x30456b)).text_color(rgb(0xffffff)))
            })
    }

    fn warning_action(button: Stateful<Div>) -> Stateful<Div> {
        button.text_color(rgb(0xe5c07b))
            .font_weight(FontWeight::SEMIBOLD)
            .hover(|s| s.bg(rgb(0x4a3b20)).text_color(rgb(0xf6dfaa)))
    }

    fn agent_sidebar(&self, sidebar: f32, height: f32, cx: &mut Context<Self>) -> Div {
        let collapsed = self.section_collapsed("section:agents");
        let tooltip_width = self.ui_scale.px(306.);
        let compact = sidebar <= self.ui_scale.px(SIDEBAR_COMPACT_MAX);
        let rows = self.agent_rows();
        let heading = div()
            .flex()
            .items_center()
            .gap_1()
            .child(Self::action_button(
                cx,
                "agents-section",
                format!("Agents ({})", rows.len()),
                |this, _, _| this.toggle_section("section:agents"),
            ))
            .child(Self::action_button(
                cx,
                "agents-sort",
                match self.ui_prefs.agent_sort {
                    AgentSortMode::Priority => {
                        if compact {
                            "Prio"
                        } else {
                            "Priority"
                        }
                    }
                    AgentSortMode::Grouped => {
                        if compact { "Group" } else { "Grouped" }
                    }
                    AgentSortMode::Manual => "Manual",
                },
                |this, _, _| this.cycle_agent_sort(),
            ))
            .child(Self::action_button(
                cx,
                "agents-launch",
                icon(Icon::Plus, self.ui_scale.px(11.), rgb(0xefefef)),
                |this, _, _| this.open_launcher(),
            ));
        let mut list = div()
            .id("agents-scroll")
            .min_h_0()
            .overflow_y_scroll()
            .flex_1();
        let mut group = String::new();
        let mut visible_index = 0;
        for row in rows.iter() {
            let entry = row.entry.clone();
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
            let icon_style = self
                .agent_ui
                .tab_labels
                .borrow()
                .get(&entry.host)
                .and_then(|tabs| tabs.get(&entry.tab))
                .map(|identity| identity.icon.clone())
                .unwrap_or_else(|| {
                    Self::named_program_icon(
                        self.programs_for(self.machine(&entry.host)),
                        &entry.icon_key,
                    )
                });
            let mut title = div()
                .flex()
                .min_w_0()
                .items_center()
                .gap_1()
                .when(row.pinned && row.depth == 0, |d| {
                    d.child(icon(Icon::Pin, self.ui_scale.px(10.), rgb(0xe5c07b)))
                })
                .child(self.display_agent_icon(&entry.host, &icon_style, self.ui_scale.px(13.)))
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
                            self.ui_scale.px(9.),
                            rgb(if pinned { 0xe5c07b } else { 0xb6bbc1 }),
                        ),
                        move |this, _, _| this.toggle_agent_pin(&pin),
                    )
                    .px_0p5()
                    .tooltip(move |_, cx| {
                        cx.new(|_| AgentTooltip(if pinned { "Unpin" } else { "Pin" }.into(), tooltip_width))
                            .into()
                    }),
                );
                for (down, glyph, label) in [(false, "↑", "Move Up"), (true, "↓", "Move Down")]
                {
                    let key = entry.key.clone();
                    title = title.child(
                        Self::action_button(
                            cx,
                            stable_elem_id("agent-move", &[&key, glyph]),
                            glyph,
                            move |this, _, _| this.move_agent(&key, down),
                        )
                        .px_0p5()
                        .tooltip(move |_, cx| cx.new(|_| AgentTooltip(label.into(), tooltip_width)).into()),
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
                    .px_0p5()
                    .pl(px(self.ui_scale.px(
                        (if compact { [1.5, 4.5, 7.5, 10.5] } else { [1.5, 6., 10.5, 15.] })[row.depth.min(3) as usize]
                    )))
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        if *hovered {
                            this.hovered_agent = Some(hover_key.clone());
                        } else if this.hovered_agent.as_ref() == Some(&hover_key) {
                            this.hovered_agent = None;
                        }
                        cx.notify();
                    }))
                    .py_0p5()
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
                                .top_0p5()
                                .bottom_0p5()
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
                                .map(|msg| format!(" · Session report: {msg}"))
                                .unwrap_or_default()
                        );
                        move |_, cx| cx.new(|_| AgentTooltip(label.clone(), tooltip_width)).into()
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
            self.agent_ui.invalidate();
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
        self.agent_ui.invalidate();
        self.save_ui_prefs();
        self.focus_agent_row(key);
    }

    fn focus_agent_row(&mut self, key: &str) {
        let index = self
            .visible_agent_rows(&self.agent_rows())
            .position(|r| r.entry.key == key)
            .unwrap_or(0);
        self.agent_cursor = index;
    }

    fn cycle_agent_sort(&mut self) {
        self.keyboard_group = None;
        self.ui_prefs.agent_sort = match self.ui_prefs.agent_sort {
            AgentSortMode::Priority => AgentSortMode::Grouped,
            AgentSortMode::Grouped => AgentSortMode::Manual,
            AgentSortMode::Manual => AgentSortMode::Priority,
        };
        self.agent_cursor = 0;
        self.agent_ui.invalidate();
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
        let labels = ["Blocked", "Error", "Working", "Done"];
        let first = summary.iter().position(|n| *n > 0)?;
        Some(if compact {
            div()
                .size(px(self.ui_scale.px(5.)))
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
            .pl(px(scale.px(if indented { 7. } else { 3.5 })))
            .pr_1()
            .py_0p5()
            .rounded_sm()
            .overflow_hidden()
            .whitespace_nowrap()
            .cursor_pointer()
            .when(is_active, |row| row.bg(rgb(0x373737)))
            .hover(|s| s.bg(rgb(if is_active { 0x373737 } else { 0x262626 })))
            .child(div().flex_1().min_w_0().truncate().child(room.label()))
            .children(self.aggregate_badge(room.tabs.iter().filter(|tab| tab.is_selectable()), compact))
            .child(
                icon_box(px(scale.px(17.)))
                    .id(stable_elem_id("edit-room", &parts))
                    .when(is_hovered, |slot| {
                        slot.group(ICON_HOVER_GROUP)
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0x505050)))
                            .child(hover_icon(Icon::Pencil, scale.px(10.), 0x9a9a9a))
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
        let compact = sidebar <= scale.px(SIDEBAR_COMPACT_MAX);
        let grouped = self.show_machine_sections();
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(Self::action_button(
                cx,
                "rooms-section",
                "Rooms",
                |this, _, _| this.toggle_section("section:rooms"),
            ))
            .when(!grouped, |el| {
                el.child(
                    icon_box(px(scale.px(17.)))
                        .id("add-room")
                        .group(ICON_HOVER_GROUP)
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(0x262626)))
                        .child(hover_icon(Icon::Plus, scale.px(12.), 0x9a9a9a))
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
                for room in m.rooms.iter().filter(|room| room.has_selectable_tabs()) {
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
                let control_size = px(scale.px(17.));
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
                                .child(hover_icon(Icon::Pencil, scale.px(10.), 0x888888))
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
                        .px_1()
                        .py_0p5()
                        .mt_0p5()
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
                        .child(icon(Icon::Server, scale.px(12.), rgb(0x888888)))
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
                                    m.rooms.iter().flat_map(|room| &room.tabs).filter(|tab| tab.is_selectable()),
                                    true,
                                ))
                                .child(
                                    icon_box(control_size)
                                        .id(stable_elem_id("machine-add", &[&hid]))
                                        .group(ICON_HOVER_GROUP)
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .hover(|s| s.bg(rgb(0x373737)))
                                        .child(hover_icon(Icon::Plus, scale.px(12.), 0x888888))
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
                                        .px(px(scale.px(1.)))
                                        .child(
                                            div()
                                                .size(px(scale.px(6.)))
                                                .rounded_full()
                                                .bg(rgb(dot_color)),
                                        ),
                                )
                                .child(
                                    icon_box(control_size).w(px(scale.px(14.))).child(
                                        icon(chevron, scale.px(10.), rgb(0x888888))
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
                    for room in m.rooms.iter().filter(|room| room.has_selectable_tabs()) {
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
                self.notice = Some(format!("Failed to save hosts.json: {e}"));
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
                    stage: "Preparing installation...".into(),
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
            machine.installing_stage = Some("Preparing installation...".into());
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
                        ConnectionState::HostNotInstalled(format!("Remote auto-install failed: {message}"))
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
            self.agent_ui.invalidate();
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
        self.agent_ui.invalidate();
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
                    .px_1()
                    .py_0p5()
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
                (MachineTab::ContainerSsh, "Container SSH"),
            ]
            .into_iter()
            .enumerate()
            .map(|(idx, (tab, title))| {
                let selected = modal.tab == tab;
                Self::segmented_choice(
                    div().id(("m-tab", idx)).px_1p5().py_0p5().rounded_sm(),
                    selected,
                    !busy,
                    0x303030,
                )
                .when(!busy, |button| {
                    button.cursor_pointer().on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(m) = this.machine_modal_mut() {
                            m.switch_tab(tab);
                        }
                        this.focus.focus(window);
                        cx.notify();
                    }))
                })
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
                            .child("Quick Select (~/.ssh/config)"),
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
                                        .px_1()
                                        .py(px(scale.px(1.)))
                                        .rounded_sm()
                                        .text_xs()
                                        .text_color(rgb(if busy { 0x858a92 } else { 0xd0d0d0 }))
                                        .child(h.clone())
                                        .when(!busy, |button| {
                                            button.cursor_pointer()
                                                .hover(|s| s.bg(rgb(0x303030)).text_color(rgb(0xffffff)))
                                                .on_click(cx.listener(move |this, _, window, cx| {
                                                    if let Some(m) =
                                                        this.machine_modal_mut().filter(|m| !m.is_busy())
                                                    {
                                                        m.select_ssh_candidate(&host);
                                                    }
                                                    this.focus.focus(window);
                                                    cx.notify();
                                                }))
                                        })
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
                            .child("Running Local Containers"),
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
                                        .px_1()
                                        .py(px(scale.px(1.)))
                                        .rounded_sm()
                                        .text_xs()
                                        .text_color(rgb(if busy { 0x858a92 } else { 0xd0d0d0 }))
                                        .child(c.clone())
                                        .when(!busy, |button| {
                                            button.cursor_pointer()
                                                .hover(|s| s.bg(rgb(0x303030)).text_color(rgb(0xffffff)))
                                                .on_click(cx.listener(move |this, _, window, cx| {
                                                    if let Some(m) =
                                                        this.machine_modal_mut().filter(|m| !m.is_busy())
                                                    {
                                                        m.select_docker_candidate(&ctr);
                                                    }
                                                    this.focus.focus(window);
                                                    cx.notify();
                                                }))
                                        })
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
                    "Paste an SSH target or connection command",
                    &modal.ssh_input,
                    "Example: dev@gpu-box or ssh -i ~/.ssh/key.pem -p 2222 ubuntu@1.2.3.4",
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
                    "Display name (defaults to target name)",
                    &modal.label,
                    "Example: gpu-box",
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
                    "Container name (or docker exec command)",
                    &modal.docker_container,
                    "Example: ddg-dev or docker exec -it ddg-dev bash",
                    ModalField::DockerContainer,
                    active_field,
                    cx,
                ))
                .child(Self::modal_text_input(
                    "m-docker-ssh",
                    "Remote SSH host (leave blank for local Docker)",
                    &modal.docker_ssh_host,
                    "Example: dev@server",
                    ModalField::DockerSshHost,
                    active_field,
                    cx,
                ))
                .child(Self::modal_text_input(
                    "m-label",
                    "Display name (defaults to container name)",
                    &modal.label,
                    "Example: ddg-dev",
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
                    "Host",
                    &modal.container_host,
                    "127.0.0.1",
                    ModalField::ContainerHost,
                    active_field,
                    cx,
                ))
                .child(Self::modal_text_input(
                    "m-ctr-port",
                    "Port",
                    &modal.port,
                    "2222",
                    ModalField::Port,
                    active_field,
                    cx,
                ))
                .child(Self::modal_text_input(
                    "m-ctr-user",
                    "User",
                    &modal.container_user,
                    "ddg",
                    ModalField::ContainerUser,
                    active_field,
                    cx,
                ))
                .child(Self::modal_text_input(
                    "m-label",
                    "Display name (optional)",
                    &modal.label,
                    "Example: dev-container",
                    ModalField::Label,
                    active_field,
                    cx,
                )),
        };

        let advanced_toggle = div()
            .id("m-adv-toggle")
            .px_1()
            .py_0p5()
            .rounded_sm()
            .group(ICON_HOVER_GROUP)
            .flex()
            .items_center()
            .gap_1()
            .text_xs()
            .text_color(rgb(0x9a9a9a))
            .hover(|s| s.bg(rgb(0x303030)).text_color(rgb(0xffffff)))
            .cursor_pointer()
            .child(hover_icon(
                if modal.show_advanced {
                    Icon::ChevronDown
                } else {
                    Icon::ChevronRight
                },
                scale.px(10.),
                0x9a9a9a,
            ))
            .child("Advanced options (port, key, known_hosts)")
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
                .pl_1()
                .border_l_1()
                .border_color(rgb(0x3a3a3a))
                .when(modal.tab == MachineTab::Ssh, |el| {
                    el.child(Self::modal_text_input(
                        "m-adv-port",
                        "SSH port (optional)",
                        &modal.port,
                        "22",
                        ModalField::Port,
                        active_field,
                        cx,
                    ))
                })
                .child(Self::modal_text_input(
                    "m-adv-id",
                    "IdentityFile (optional)",
                    &modal.identity_file,
                    "~/.ssh/id_ed25519",
                    ModalField::IdentityFile,
                    active_field,
                    cx,
                ))
                .child(Self::modal_text_input(
                    "m-adv-kh",
                    "UserKnownHostsFile (optional)",
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
                    .child("Checking connection...")
                    .into_any_element(),
            ),
            TestState::Installing { stage, .. } => Some(
                div()
                    .p_1()
                    .rounded_sm()
                    .bg(rgb(0x2d2415))
                    .border_1()
                    .border_color(rgb(0x6b5225))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_xs()
                    .text_color(rgb(0xe5c07b))
                    .child("Installing on the remote host...")
                    .child(div().text_color(rgb(0xe1d4b8)).child(stage.clone()))
                    .into_any_element(),
            ),
            TestState::Success { host_id } => {
                let short: String = host_id.chars().take(12).collect();
                Some(
                    div()
                        .text_xs()
                        .text_color(rgb(0x9ece6a))
                        .child(format!("Connection verified (Host ID: {short})"))
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
                        "Host key is unknown or does not match (StrictHostKeyChecking=yes)"
                    }
                    DialErrorKind::AuthRequired => {
                        "SSH authentication failed — check ssh-agent or IdentityFile"
                    }
                    DialErrorKind::HostIdMismatch => "The connected Host ID does not match the saved value",
                    DialErrorKind::SecurityBlocked => "Pin the Host ID to connect",
                    DialErrorKind::InvalidProfile => "Invalid input format",
                    DialErrorKind::HostNotInstalled => REMOTE_INSTALL_MISSING,
                    DialErrorKind::ProtocolIncompatible => "Incompatible protocol version",
                    DialErrorKind::Unavailable => {
                        "Connection failed (check that ddg-agents-host is installed on the remote machine)"
                    }
                };
                let hint_opt = copy_hint.clone();
                let copied = modal.copied_hint;
                Some(
                    div()
                        .p_1()
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
                                    div().flex().mt_0p5().child(
                                        Self::warning_action(
                                            div().id("m-install-btn").px_1p5().py_0p5().rounded_sm(),
                                        )
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
                                    .mt_0p5()
                                    .px_1()
                                    .py_0p5()
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
                                            .px_1()
                                            .py(px(scale.px(1.)))
                                            .rounded_sm()
                                            .hover(|s| s.bg(rgb(0x505050)))
                                            .cursor_pointer()
                                            .child(if copied {
                                                "Copied"
                                            } else {
                                                "Copy Command"
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
                .p_2()
                .child(
                    div()
                        .id("machine-modal-panel")
                        .w(px(scale.px(391.)))
                        .max_w_full()
                        .max_h_full()
                        .min_h_0()
                        .overflow_y_scroll()
                        .p_2()
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
                                    "Machine Settings"
                                } else {
                                    "Add Machine"
                                }))
                                .when(!installing, |el| {
                                    el.child(
                                        icon_box(px(scale.px(17.)))
                                            .id("m-close-x")
                                            .group(ICON_HOVER_GROUP)
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .hover(|s| s.bg(rgb(0x373737)))
                                            .child(hover_icon(Icon::X, scale.px(12.), 0x9a9a9a))
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
                                "Removing this machine does not stop sessions running on the remote machine.",
                            ))
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .pt_0p5()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .id("m-test-btn")
                                                .px_1p5()
                                                .py_0p5()
                                                .rounded_sm()
                                                .text_color(rgb(if busy {
                                                    0x9a9a9a
                                                } else {
                                                    0xf2f2f2
                                                }))
                                                .child("Test Connection")
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
                                                    .px_1p5()
                                                    .py_0p5()
                                                    .rounded_sm()
                                                    .cursor_pointer()
                                                    .hover(|s| s.bg(rgb(0x4a2226)).text_color(rgb(0xffffff)))
                                                    .text_color(rgb(0xf5c2c7))
                                                    .child("Remove")
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
                                                    .px_1p5()
                                                    .py_0p5()
                                                    .rounded_sm()
                                                    .cursor_pointer()
                                                    .hover(|s| s.bg(rgb(0x373737)))
                                                    .child("Cancel")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.dismiss_machine_modal(cx)
                                                    })),
                                            )
                                        })
                                        .child(
                                            Self::primary_action(
                                                div().id("m-save").px_1p5().py_0p5().rounded_sm(),
                                                !busy,
                                            )
                                            .child(if is_edit { "Save" } else { "Add" })
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
            self.notice = Some("Select a connected target machine".into());
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
            .p_1p5()
            .bg(rgba(0x00000088))
            .child(
                div()
                    .w(px(self.ui_scale.px(width)))
                    .debug_selector(|| "agent-overlay-panel".into())
                    .max_w_full()
                    .max_h_full()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_1p5()
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
                                icon(Icon::X, self.ui_scale.px(12.), rgb(0xe9e9e9)),
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
                    .child("Target:")
                    .children(self.machines.iter().map(|m| {
                        let id = m.profile.id.clone();
                        Self::action_button_with_style(
                            cx,
                            stable_elem_id("launch-host", &[&id]),
                            m.profile.label.clone(),
                            true,
                            |button| {
                                Self::segmented_choice(button, m.profile.id == launcher.host, true, 0x394354)
                            },
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
                    })),
            )
            .child(
                div().flex().flex_wrap().gap_1().children(
                    ["New Tab in Current Room", "Split", "New Room", "Background"]
                        .into_iter()
                        .enumerate()
                        .map(|(i, name)| {
                            let enabled = self.launch_target(&launcher.host, i).is_some();
                            Self::action_button_with_style(
                                cx,
                                ("launch-location", i),
                                name,
                                enabled,
                                |button| {
                                    Self::segmented_choice(button, i == launcher.target, enabled, 0x394354)
                                },
                                move |this, _, _| {
                                    if enabled
                                        && let Some(ActiveOverlay::AgentLauncher(l)) =
                                            &mut this.active_overlay
                                    {
                                        l.target = i;
                                    }
                                },
                            )
                        }),
                ),
            )
            .child(
                div()
                    .px_1()
                    .py_0p5()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(0x688ac1))
                    .child(if launcher.filter.is_empty() {
                        "Filter by name or command…".into()
                    } else {
                        launcher.filter.clone()
                    }),
            );
        if let Some(error) = &self.agent_error {
            content = content.child(
                div()
                    .text_color(rgb(0xffad9a))
                    .child(format!("Could not load agents.json: {error}")),
            );
        } else if self.agents.agents.is_empty() {
            content = content.child("No agents configured. Add one in Agent Settings.");
        } else {
            let matches = launcher.matches(&self.agents);
            if matches.is_empty() {
                content = content.child("No results found.");
            }
            let mut rows = div()
                .id("launcher-rows")
                .overflow_y_scroll()
                .min_h_0()
                .max_h(px(self.ui_scale.px(234.)));
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
                        &Self::named_program_icon(
                            programs,
                            profile.icon.as_deref().unwrap_or(fallback),
                        ),
                        self.ui_scale.px(14.),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().child(format!(
                                "{}{}",
                                profile.label,
                                if is_default { " · Default" } else { "" }
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
            "Launch Agent",
            429.,
            content.child(Self::action_button(
                cx,
                "launcher-settings",
                "Agent Settings…",
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
            self.preset_control = None;
            self.request_preset_detection(self.active_host_id.clone());
        } else {
            self.notice = Some("Could not open the agents.json settings path".into());
        }
    }

    fn agent_modal(&self) -> Option<&AgentSettingsModal> {
        match &self.active_overlay {
            Some(ActiveOverlay::AgentSettingsModal(modal)) => Some(modal),
            _ => None,
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
            self.agent_ui.invalidate();
            self.agents = config;
            self.agent_error = None;
            self.close_agent_settings();
        }
    }

    fn close_agent_settings(&mut self) {
        self.active_overlay = None;
        self.preset_control = None;
        self.preset_connection = None;
    }

    fn reload_agent_settings(&mut self) {
        if let Some(m) = self.agent_modal_mut() {
            let host = m.detect_host.clone();
            *m = AgentSettingsModal::load(m.path.clone());
            self.preset_control = None;
            self.request_preset_detection(host);
        }
    }

    fn request_preset_detection(&mut self, host: String) {
        let request = self.bridge.detect_agent_presets(&host);
        if let Some(modal) = self.agent_modal_mut() {
            modal.begin_preset_detection(host, request);
            self.preset_connection = None;
            if matches!(self.preset_control, Some(PresetControl::Add(_))) {
                self.preset_control = Some(PresetControl::Refresh);
            }
        }
    }

    fn select_preset_machine(&mut self, backwards: bool) {
        let Some(modal) = self.agent_modal() else {
            return;
        };
        let host = self.machines.iter().position(|m| m.profile.id == modal.detect_host);
        let index = match (host, backwards) {
            (Some(i), false) => (i + 1) % self.machines.len(),
            (Some(i), true) => (i + self.machines.len() - 1) % self.machines.len(),
            (None, _) => 0,
        };
        if let Some(machine) = self.machines.get(index)
            && machine.profile.id != modal.detect_host
        {
            self.request_preset_detection(machine.profile.id.clone());
        }
    }

    fn activate_preset_control(&mut self, control: PresetControl, window: &mut Window) {
        let Some(host) = self.agent_modal().map(|modal| modal.detect_host.clone()) else {
            return;
        };
        match control {
            PresetControl::Machine => self.select_preset_machine(false),
            PresetControl::Refresh => self.request_preset_detection(host),
            PresetControl::Add(id) => {
                if self.preset_connection.is_none()
                    || self.preset_connection != self.bridge.preset_connection(&host)
                {
                    self.request_preset_detection(host);
                } else if let Some(preset) = find_agent_preset(id)
                    && self.agent_modal_mut().is_some_and(|modal| modal.add_preset(preset))
                {
                    self.preset_control = None;
                    self.focus.focus(window);
                }
            }
        }
    }

    fn preset_controls(modal: &AgentSettingsModal) -> Vec<Option<PresetControl>> {
        let mut controls = vec![None, Some(PresetControl::Machine), Some(PresetControl::Refresh)];
        if let PresetDetectionState::Loaded { found, .. } = &modal.detection {
            controls.extend(found.iter().filter_map(|id| {
                if modal.config.find(id).is_some() {
                    None
                } else {
                    find_agent_preset(id).map(|preset| Some(PresetControl::Add(preset.id)))
                }
            }));
        }
        controls
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
                div()
                    .id(id)
                    .px_1()
                    .py_0p5()
                    .rounded_sm()
                    .min_w_0()
                    .truncate()
                    .border_1()
                    .border_color(rgb(if active { 0x92b6ff } else { 0x424851 }))
                    .bg(rgb(0x191919))
                    .text_xs()
                    .cursor_text()
                    .child(if value.is_empty() { "—".into() } else { value })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.preset_control = None;
                        if let Some(m) = this.agent_modal_mut() {
                            m.field = field.clone();
                        }
                        this.focus.focus(window);
                        cx.notify();
                    })),
            )
    }

    fn preset_button(
        &self,
        host: &str,
        control: PresetControl,
        id: impl Into<ElementId>,
        label: impl IntoElement,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let host = host.to_owned();
        Self::action_button(cx, id, label, move |this, window, _| {
            if this.agent_modal().is_some_and(|modal| modal.detect_host == host) {
                this.preset_control = Some(control);
                this.activate_preset_control(control, window);
            }
        })
        .when(self.preset_control == Some(control), |button| {
            button.bg(rgb(0x30456b)).border_1().border_color(rgb(0x92b6ff))
        })
    }

    fn detected_presets_view(&self, modal: &AgentSettingsModal, cx: &mut Context<Self>) -> Div {
        let host = modal.detect_host.as_str();
        let label = self.machine(host).map_or(host, |m| m.profile.label.as_str());
        let mut section = div()
            .flex()
            .flex_col()
            .gap_1()
            .pt_1()
            .border_t_1()
            .border_color(rgb(0x424851))
            .child(div().text_xs().child("Detected"))
            .child(self.preset_button(
                host,
                PresetControl::Machine,
                "preset-machine",
                div().truncate().child(format!("Machine: {label}")),
                cx,
            ))
            .child(self.preset_button(host, PresetControl::Refresh, "preset-refresh", "Refresh", cx));
        let help = |label: &str| div().text_xs().text_color(rgb(0xb6bbc1)).child(label.to_owned());
        section = match &modal.detection {
            PresetDetectionState::Loading => section.child(help("Checking…")),
            PresetDetectionState::Unsupported => section.child(help("Detection unsupported")),
            PresetDetectionState::Unavailable(reason) => section.child(help("Unavailable")).child(help(reason)),
            PresetDetectionState::Loaded { found, incomplete } if found.is_empty() && !incomplete => {
                section.child(help("No presets found"))
            }
            PresetDetectionState::Loaded { found, incomplete } => {
                for preset in found.iter().filter_map(|id| find_agent_preset(id)) {
                    let label = div().flex().items_center().gap_1()
                        .child(self.display_agent_icon(
                            host,
                            &Self::named_program_icon(&self.programs, preset.icon),
                            self.ui_scale.px(12.),
                        ))
                        .child(preset.label);
                    section = if modal.config.find(preset.id).is_some() {
                        section.child(div().text_xs().child(label).child(help("Added")))
                    } else {
                        section.child(self.preset_button(
                            host,
                            PresetControl::Add(preset.id),
                            stable_elem_id("add-detected-preset", &[preset.id]),
                            div().flex().items_center().gap_1().child("+").child(label),
                            cx,
                        ))
                    };
                }
                if *incomplete {
                    section = section.child(help("Some presets could not be checked"));
                }
                section
            }
        };
        section.child(help("Tab to move · Enter to choose"))
    }

    fn handle_preset_key(&mut self, key: &Keystroke, window: &mut Window) -> bool {
        let Some(modal) = self.agent_modal().filter(|m| !m.blocked && m.confirm.is_none()) else {
            return false;
        };
        let m = key.modifiers;
        if m.control || m.platform || m.alt || m.function {
            return false;
        }
        if key.key == "tab" {
            let controls = Self::preset_controls(modal);
            let current = controls.iter().position(|c| *c == self.preset_control).unwrap_or(0);
            let next = if m.shift {
                current + controls.len() - 1
            } else {
                current + 1
            };
            self.preset_control = controls[next % controls.len()];
            return true;
        }
        match (self.preset_control, key.key.as_str()) {
            (Some(control), "enter" | "space") => self.activate_preset_control(control, window),
            (Some(PresetControl::Machine), "left" | "right") => {
                self.select_preset_machine(key.key == "left");
            }
            _ => return false,
        }
        true
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
                    "Reload",
                    |this, _, _| this.reload_agent_settings(),
                ))
                .child(Self::action_button(
                    cx,
                    "agent-path",
                    "Copy Path",
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
                        .child(format!("Protected backup: {}", backup.display())),
                );
                actions = actions.child(Self::action_button(
                    cx,
                    "agent-reset",
                    if modal.confirm == Some(Confirm::Reset) {
                        "Confirm Reset"
                    } else {
                        "Back Up and Reset"
                    },
                    |this, _, _| {
                        if let Some(m) = this.agent_modal_mut() {
                            if m.confirm == Some(Confirm::Reset) {
                                if m.reset() {
                                    this.agent_ui.invalidate();
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
                    "Cancel",
                    |this, _, _| {
                        if let Some(m) = this.agent_modal_mut() {
                            m.confirm = None;
                        }
                    },
                ));
            }
            return Some(self.overlay_panel("Agent Settings", 544., content.child(actions), cx));
        }
        if modal.conflict {
            content = content.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_1()
                    .bg(rgb(0x403421))
                    .child("agents.json changed externally. Your current draft is preserved.")
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_1()
                            .child(Self::action_button(
                                cx,
                                "agent-load-external",
                                "Load External Changes",
                                |this, _, _| this.reload_agent_settings(),
                            ))
                            .child(Self::action_button(
                                cx,
                                "agent-overwrite",
                                "Overwrite with My Changes",
                                |this, _, _| this.save_agent_settings(true),
                            )),
                    ),
            );
        }
        let selector = div()
            .flex()
            .flex_col()
            .gap_1()
            .w(px(self.ui_scale.px(111.)))
            .debug_selector(|| "agent-settings-selector".into())
            .flex_none()
            .child(Self::action_button(
                cx,
                "agent-add-profile",
                "+ Add",
                |this, _, _| {
                    this.preset_control = None;
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
                            " · Default"
                        } else {
                            ""
                        }
                    )),
                    move |this, _, _| {
                        this.preset_control = None;
                        if let Some(m) = this.agent_modal_mut()
                            && m.commit_fields()
                        {
                            m.select(i);
                        }
                    },
                )
                .when(i == modal.selected, |d| d.bg(rgb(0x30456b)))
            }))
            .child(self.detected_presets_view(modal, cx));
        let mut fields = div().flex_1().min_w_0().flex().flex_col().gap_2();
        if let Some(profile) = modal.profile() {
            let selected_default = modal.config.default_agent_id.as_deref() == Some(&profile.id);
            fields = fields.child(div().flex().gap_2()
                .child(Self::agent_field(modal, Field::Id, "profile-id", "ID", &profile.id, false, cx).flex_1())
                .child(Self::agent_field(modal, Field::Name, "profile-name", "Name", &profile.label, false, cx).flex_1()))
                .child(Self::agent_field(modal, Field::Command, "profile-command", "Launch command (argv, run directly without a shell)", &modal.command, false, cx))
                .child(div().flex().flex_wrap().gap_1().children(ddg_agents_transport::split_safe_tokens(&modal.command).unwrap_or_default()
                    .into_iter().map(|arg| div().px_0p5().rounded_sm().bg(rgb(0x343e4e)).text_xs().child(arg))))
                .child(div().flex().items_end().gap_2()
                    .child(Self::agent_field(modal, Field::Icon, "profile-icon", "Icon key or local filename", profile.icon.as_deref().unwrap_or(""), false, cx).flex_1())
                    .child(self.display_agent_icon(
                        "local",
                        &Self::named_program_icon(&self.programs, profile.icon.as_deref().unwrap_or_default()),
                        self.ui_scale.px(15.),
                    ))
                    .child(Self::action_button(cx, "profile-default", if selected_default { "Unset Default" } else { "Set as Default" }, move |this, _, _| {
                        if let Some(m) = this.agent_modal_mut() {
                            m.config.default_agent_id = if selected_default { None } else { m.profile().map(|p| p.id.clone()) };
                        }
                    })))
                .child(div().flex().items_center().justify_between().child("Environment Variables")
                    .child(Self::action_button(cx, "agent-reveal-env", if modal.reveal_env { "Hide Values" } else { "Show Values" }, |this, _, _| {
                        if let Some(m) = this.agent_modal_mut() { m.reveal_env = !m.reveal_env; }
                    })))
                .child(div().text_xs().text_color(rgb(0xe5c07b)).child("Do not enter secrets. DDG_*, LD_PRELOAD, LD_AUDIT, DYLD_INSERT_LIBRARIES, and DYLD_FORCE_FLAT_NAMESPACE are blocked. Values are stored in the host catalog."));
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
                            "Remove",
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
                            "Variable name",
                            &modal.env_key,
                            false,
                            cx,
                        )
                        .flex_1(),
                    )
                    .child(Self::action_button(
                        cx,
                        "agent-env-add",
                        "+ Environment Variable",
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
                        .child("Per-Machine Overrides")
                        .children(remotes.into_iter().map(|machine| {
                            let id = machine.profile.id.clone();
                            Self::action_button_with_style(
                                cx,
                                stable_elem_id("override-agent-host", &[&id]),
                                machine.profile.label.clone(),
                                true,
                                |button| {
                                    Self::segmented_choice(
                                        button,
                                        modal.host.as_ref() == Some(&machine.profile.id),
                                        true,
                                        0x394354,
                                    )
                                },
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
                        })),
                );
                if let Some(host) = &modal.host {
                    let over = profile.host_overrides.get(host);
                    fields = fields.child(Self::agent_field(
                        modal,
                        Field::HostCommand,
                        "override-argv",
                        "Launch command (leave blank to inherit)",
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
                                    "Value",
                                    value,
                                    !modal.reveal_env,
                                    cx,
                                )
                                .flex_1(),
                            );
                        }
                        fields = fields.child(
                            row.children(["Inherit", "Override", "Remove"].into_iter().enumerate().map(
                                |(i, name)| {
                                    let key = key.clone();
                                    Self::action_button_with_style(
                                        cx,
                                        stable_elem_id("host-env-state", &[&key, name]),
                                        name,
                                        true,
                                        |button| {
                                            Self::segmented_choice(button, state == i, true, 0x394354)
                                        },
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
                                },
                            )),
                        );
                    }
                }
            }
        } else {
            fields = fields.child("No agents configured. Click Add to get started.");
        }
        let mut footer = div().flex().items_center().gap_2();
        if modal.profile().is_some() {
            let confirming_delete = modal.confirm == Some(Confirm::Delete);
            footer = footer.child(Self::action_button_with_style(
                cx,
                "agent-delete",
                if confirming_delete { "Confirm Delete" } else { "Delete" },
                true,
                |button| {
                    if confirming_delete {
                        button.text_color(rgb(0xf5c2c7))
                            .hover(|s| s.bg(rgb(0x4a2226)).text_color(rgb(0xffffff)))
                    } else {
                        button.hover(|s| s.bg(rgb(0x394354)))
                    }
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
                "Cancel",
                |this, _, _| {
                    if let Some(m) = this.agent_modal_mut() {
                        m.confirm = None;
                    }
                },
            ));
        }
        footer = footer.child(
            Self::action_button_with_style(
                cx,
                "agent-save",
                "Save",
                true,
                |button| Self::primary_action(button, true),
                |this, _, _| this.save_agent_settings(false),
            )
            .ml_auto(),
        );
        Some(
            self.overlay_panel(
                "Agent Settings",
                544.,
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
                .p_2()
                .child(
                    div()
                        .id("rename-modal-panel")
                        .w(px(self.ui_scale.px(306.)))
                        .max_w_full()
                        .max_h_full()
                        .min_h_0()
                        .overflow_y_scroll()
                        .p_2()
                        .rounded_md()
                        .bg(rgb(0x252525))
                        .border_1()
                        .border_color(rgb(0x505050))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(div().text_base().child("Rename Room"))
                        .child(
                            div()
                                .text_color(rgb(0xbcbcbc))
                                .truncate()
                                .child(current_label),
                        )
                        .child(
                            div()
                                .px_1()
                                .py_1()
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
                                .child("Leave the name blank to use the folder name."),
                        )
                        .child(
                            div().flex().justify_end().gap_2().children(
                                [(false, "Cancel"), (true, "Save")].into_iter().map(
                                    |(save, label)| {
                                        div()
                                            .id(("rename-action", usize::from(save)))
                                            .px_1p5()
                                            .py_0p5()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .when(save, |button| Self::primary_action(button, true))
                                            .when(!save, |button| {
                                                button.text_color(rgb(0xf2f2f2))
                                                    .hover(|s| s.bg(rgb(0x373737)))
                                            })
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

    fn run_tab_action(&mut self, action: TabAction, window: &mut Window, cx: &App) {
        if self.pane_drag.is_some() {
            return;
        }
        match action {
            TabAction::NewRoom => self.new_room(window),
            TabAction::Add => self.add_tab(),
            TabAction::Close => self.close_focused_tab(window, cx),
            TabAction::SplitHorizontal => self.split_focused(SplitAxis::Horizontal),
            TabAction::SplitVertical => self.split_focused(SplitAxis::Vertical),
            TabAction::FocusDir(dir) => self.focus_direction(dir),
            TabAction::ToggleZoom => self.toggle_zoom(),
            TabAction::Unsplit => self.unsplit_focused(),
            TabAction::Previous | TabAction::Next | TabAction::Number(_) => {
                if let Some(room) = self.room_mut() {
                    let groups = room.projected_groups();
                    if let Some(active) = groups.iter().position(|(index, _)| *index == room.selected_group)
                        && let Some(index) = action.target(active, groups.len())
                    {
                        room.select_projected_group(index);
                    }
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
        icon_box(px(scale.px(17.)))
            .id(id)
            .debug_selector(|| id.into())
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
                        this.run_tab_action(action, window, cx);
                        cx.notify();
                    }))
            })
    }

    fn render_tab_controls(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let room = self.room()?;
        let scale = self.ui_scale;
        let tabs = room
            .projected_groups()
            .into_iter()
            .enumerate()
            .map(|(i, (raw_index, group))| {
                let primary = first_leaf(&group).to_owned();
                let active = raw_index == room.selected_group;
                let label_id = if active {
                    room.focused_tab_id().unwrap_or(&primary)
                } else {
                    &primary
                };
                let tab = room.tabs.iter().find(|tab| tab.id == label_id);
                let identity = self.active_machine().zip(tab);
                let (name, tab_icon) = identity.map_or_else(
                    || ("Terminal".into(), None),
                    |(machine, tab)| {
                        let identity = self.top_tab_label(machine, tab);
                        (
                            identity.name,
                            Some(self.display_agent_icon(
                                &self.active_host_id,
                                &identity.icon,
                                scale.px(12.),
                            )),
                        )
                    },
                );
                let tab_status = tab
                    .filter(|t| t.is_running())
                    .and_then(|t| t.activity.as_ref())
                    .and_then(|a| {
                        a.agent_status
                            .as_ref()
                            .map(|s| self.tab_status_badge(s.state, a.activity_seq > a.seen_seq))
                    });
                div()
                    .id(stable_elem_id(
                        "terminal-tab",
                        &[&self.active_host_id, &primary],
                    ))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_1()
                    .px_1()
                    .py_0p5()
                    .cursor_pointer()
                    .when(active, |tab| tab.bg(rgb(0x373737)))
                    .when(!active, |tab| tab.hover(|s| s.bg(rgb(0x262626))))
                    .child((i + 1).to_string())
                    .children(tab_icon)
                    .child(div().max_w(px(scale.px(95.))).truncate().child(name))
                    .children(tab_status)
                    .when(active, |el| {
                        el.child(self.tab_bar_button(
                            "close-tab",
                            Icon::X,
                            9.,
                            TabAction::Close,
                            self.is_active_connected(),
                            cx,
                        ))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(room) = this.room_mut()
                            && !room.is_tab_closing(&primary)
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
                    "Launch Agent",
                    |this, _, _| this.open_launcher(),
                ))
                .child(self.tab_bar_button(
                    "add",
                    Icon::Plus,
                    11.,
                    TabAction::Add,
                    connected && room.tabs.len() < MAX_TABS_PER_ROOM,
                    cx,
                ))
                .when_some(badge, |bar, label| {
                    bar.child(
                        div()
                            .ml_auto()
                            .flex_shrink_0()
                            .mr_1()
                            .px_1()
                            .py_0p5()
                            .max_w(px(scale.px(136.)))
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
            (_, false, _) => "Disconnected".into(),
            (_, _, true) => "Temporary control · Esc: Terminal".into(),
            _ => "Read-only · Esc: Close".into(),
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
                    &self.resolve_tab_identity(m, &peek.tab).icon,
                    self.ui_scale.px(13.),
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
                    "Request Control",
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
                    "Release Control",
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
                    "Move to Current Room",
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
                "Promote to New Room",
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
            "Close",
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
                .h(px(self.ui_scale.px(PEEK_BANNER_HEIGHT)))
                .px_1()
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
            .px_1()
            .flex_none()
            .h(px(self.ui_scale.px(RESET_BANNER_HEIGHT)))
            .overflow_hidden()
            .text_xs()
            .bg(rgb(0x3a3020))
            .text_color(rgb(0xf0d5a4))
            .child(div().min_w_0().truncate().child(if matches!(machine.profile.transport, HostTransport::Local { .. }) {
                "Older local daemon: Process names & agent status unavailable · Restart ddg-agents-host and ddg-terminal-server"
            } else if machine.update_confirm {
                "Update the remote host and keep the same Host ID?"
            } else if matches!(machine.profile.transport, HostTransport::DockerExec { .. }) {
                "Older host: Agent status unavailable · Update the container manually"
            } else {
                "Older host: Agent status unavailable"
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
                    "Confirm Update"
                } else {
                    "Update Remote Host"
                },
                move |this, _, cx| this.start_machine_install(&id, cx),
            ));
            if confirm {
                let id = host.to_owned();
                banner = banner.child(Self::action_button(
                    cx,
                    "legacy-host-update-cancel",
                    "Cancel",
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
        let grid = self.terminal_font_grid();
        let FontGrid {
            family,
            width,
            height,
            ..
        } = grid;
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
        let input = self.prepare_terminal_input(window, cx);
        let focus = self.focus.clone();
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
                                .px_1()
                                .flex()
                                .items_center()
                                .gap_1()
                                .bg(rgb(0x252a31))
                                .font_family(".SystemUIFont")
                                .text_xs()
                                .child(self.display_agent_icon(
                                    &host_id,
                                    &self.top_tab_label(machine, tab).icon,
                                    scale.px(12.),
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
            let capture_move = is_focused.then(|| {
                let desktop = cx.weak_entity();
                Box::new(
                    move |event: &MouseMoveEvent, window: &mut Window, app: &mut App| {
                        let _ = desktop.update(app, |this, cx| {
                            if this.terminal_mouse.is_some() {
                                this.terminal_mouse_move(event, window, cx);
                            }
                        });
                    },
                ) as Box<dyn Fn(&MouseMoveEvent, &mut Window, &mut App)>
            });
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
            let surface = &mut tab.surface;
            let frame = tab
                .vt
                .as_mut()
                .filter(|_| connected || peek)
                .map(|vt| {
                    terminal_canvas::prepare(&mut surface.cache, vt, grid, window, surface.id)
                })
                .unwrap_or_default();
            let body = terminal_canvas::body(
                frame,
                BodyOptions {
                    pane_id: surface.id,
                    tab_id: tab_id.clone(),
                    show_cursor: is_focused && show_cursor && running && !readonly && !tab.closing,
                    ime: input
                        .as_ref()
                        .filter(|_| is_focused)
                        .map(|input| (focus.clone(), input.clone())),
                    bounds: surface.bounds.clone(),
                    ime_bounds: surface.ime_bounds.clone(),
                    capture_move,
                },
            );
            if let Some(ch) = resize {
                self.bridge.send_resize(&host_id, ch, size.0, size.1);
            }

            let click_id = tab_id.clone();
            let drop_id = tab_id.clone();
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
                            notify(cx, "pane_focus");
                        }
                    }),
                )
                .drag_over::<ExternalPaths>(move |style, _, _, _| {
                    let style = if is_split { style } else { style.p_0() };
                    style.border(inset).border_color(rgb(0x6b9fff))
                })
                .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                    this.handle_file_drop(&drop_id, paths.paths(), window, cx);
                    cx.notify();
                }))
                .children(header)
                .child(
                    self.terminal_body_events(&host_id, &tab_id, cx)
                        .absolute()
                        .top(px(header_h))
                        .bottom_0()
                        .left(inset)
                        .right(inset)
                        .overflow_hidden()
                        .child(body)
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
                                    .px_1()
                                    .py_0p5()
                                    .bg(rgb(if readonly { 0x2d2415 } else { 0x222832 }))
                                    .font_family(".SystemUIFont")
                                    .text_xs()
                                    .line_height(px(scale.px(14.)))
                                    .child(div().min_w_0().truncate().child(if readonly {
                                        "Read-only".into()
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
                                            .px_1()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .whitespace_nowrap()
                                            .when(readonly, Self::warning_action)
                                            .when(!readonly, |button| {
                                                button.text_color(rgb(0xf2f2f2))
                                                    .hover(|s| s.bg(rgb(0x505b6b)))
                                            })
                                            .child(if readonly {
                                                "Request Control"
                                            } else {
                                                "Start New Session"
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
        terminal
            .children(dividers.into_iter().enumerate().map(|(i, hit)| {
                pane_box(hit.rect)
                    .id(("pane-divider", i))
                    .debug_selector(|| format!("pane-divider-{i}"))
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
            .children(self.render_pending_paste(cx))
    }

    fn agent_list_key(&mut self, key: &Keystroke, window: &mut Window) -> bool {
        let tree = self.agent_rows();
        let rows = self.visible_agent_rows(&tree).collect::<Vec<_>>();
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
                    let index = self.visible_agent_rows(&tree).position(|r| {
                        !r.pinned && group == format!("group:{}:{}", r.entry.host, r.entry.room)
                    });
                    if let Some(i) = index {
                        self.agent_cursor = i;
                    }
                }
            }
            _ => return false,
        }
        true
    }

    fn jump_urgent(&mut self, window: &mut Window) {
        let tree = self.agent_rows();
        let mut rows = tree
            .iter()
            .filter(|r| r.entry.urgent() < 4)
            .collect::<Vec<_>>();
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
        if self.handle_preset_key(key, window) {
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if let Some(ActiveOverlay::AgentSettingsModal(modal)) = &mut self.active_overlay {
            if key.key == "escape" {
                if modal.confirm.is_some() {
                    modal.confirm = None;
                } else {
                    self.close_agent_settings();
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
                    self.preset_control = None;
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
            && self.terminal_has_keyboard()
            && self.focus.is_focused(window)
            && let Some(dir) = room_nav_action(&key.key, m, mac)
        {
            let had_marked_ime = self
                .terminal_input
                .as_ref()
                .is_some_and(|input| !input.read(cx).marked.is_empty());
            let stale_paste = self.sync_terminal_interaction(Some(window), cx);
            let has_marked_text = self
                .terminal_input
                .as_ref()
                .is_some_and(|input| !input.read(cx).marked.is_empty());
            if self.pending_paste.is_some() || has_marked_text {
                if stale_paste {
                    cx.notify();
                }
                return self.terminal_key(event, window, cx);
            }
            let switched = self.switch_room(dir, window, cx);
            cx.stop_propagation();
            if switched {
                notify(cx, "keyboard");
            } else if stale_paste || (had_marked_ime && self.terminal_input.is_none()) {
                cx.notify();
            }
            return;
        }
        if self.peek.is_none()
            && let Some(action) = tab_action(&key.key, m, mac)
        {
            if event.is_held && action == TabAction::Close {
                cx.stop_propagation();
                return;
            }
            self.run_tab_action(action, window, cx);
        } else {
            return self.terminal_key(event, window, cx);
        }
        cx.stop_propagation();
        notify(cx, "keyboard");
    }
}

impl Desktop {
    fn terminal_font_grid(&self) -> FontGrid {
        let (family, width, height) = self.font_grid;
        FontGrid {
            family,
            width,
            height,
            font_size: self.ui_scale.term_font_px(),
        }
    }

    fn terminal_has_keyboard(&self) -> bool {
        self.window_active
            && matches!(self.focus_zone, KeyboardFocusZone::Terminal)
            && self.active_overlay.is_none()
            && self.update_modal.is_none()
    }

    fn terminal_tab(&self, host: &str, tab_id: &str) -> Option<&TabView> {
        if let Some(peek) = &self.peek
            && peek.host == host
            && peek.tab.id == tab_id
        {
            return Some(&peek.tab);
        }
        self.machine(host)?
            .rooms
            .iter()
            .flat_map(|room| &room.tabs)
            .find(|tab| tab.id == tab_id)
    }

    fn mouse_target(&self, host: &str, tab_id: &str) -> Option<MouseTarget> {
        let tab = self.terminal_tab(host, tab_id)?;
        let connected = self.machine(host)?.is_connected();
        if tab.closing || !connected && self.peek.is_none() {
            return None;
        }
        Some(MouseTarget {
            host_id: host.to_owned(),
            tab_id: tab.id.clone(),
            run_generation: tab.run_generation,
            channel: tab.channel,
            mode: tab.mode,
            connected,
            running: tab.is_running(),
            can_report: connected
                && tab.is_running()
                && tab.mode == ControlMode::Control
                && tab.channel.is_some(),
        })
    }

    fn terminal_mouse_is_current(&self, state: &TerminalMouseState) -> bool {
        let MouseTarget {
            host_id, tab_id, ..
        } = state.target();
        self.terminal_has_keyboard()
            && self.pane_drag.is_none()
            && self.sidebar_drag.is_none()
            && self
                .active_terminal_target()
                .is_some_and(|(host, tab)| host == host_id && &tab.id == tab_id)
            && self.tab_is_visible(host_id, tab_id)
            && self.mouse_target(host_id, tab_id).is_some_and(|target| {
                self.terminal_tab(host_id, tab_id)
                    .and_then(|tab| tab.vt.as_ref())
                    .is_some_and(|vt| state.matches(&target, vt))
            })
    }

    fn sync_terminal_interaction(&mut self, window: Option<&Window>, cx: &App) -> bool {
        let focused = window.is_none_or(|window| self.focus.is_focused(window));
        if self
            .terminal_mouse
            .as_ref()
            .is_some_and(|state| !focused || !self.terminal_mouse_is_current(state))
        {
            self.cancel_terminal_mouse();
        }
        if self.pending_paste.is_none() && self.terminal_input.is_none() {
            return false;
        }
        let control = focused.then(|| self.control_target()).flatten();
        let stale_ime = self.terminal_input.as_ref().is_some_and(|input| {
            let (host, tab, run, channel) = &input.read(cx).target;
            control != Some((host.as_str(), tab.as_str(), *run, *channel))
        });
        let stale_paste = self.pending_paste.as_ref().is_some_and(|pending| {
            let target = control
                .filter(|_| self.peek.is_none())
                .map(|(host, tab, run, channel)| (host.to_owned(), tab.to_owned(), run, channel));
            let bracketed = self.paste_is_bracketed(target.as_ref());
            !pending.matches(target.as_ref(), bracketed)
        });
        if stale_ime {
            self.terminal_input = None;
        }
        if stale_paste {
            self.pending_paste = None;
        }
        stale_paste
    }

    fn cancel_terminal_mouse(&mut self) {
        self.terminal_autoscroll = None;
        if let Some(state @ TerminalMouseState::AppMouse { button, .. }) =
            self.terminal_mouse.take()
        {
            self.release_app_mouse(&state, button, self.terminal_pointer, Modifiers::default());
        }
    }

    fn release_app_mouse(
        &mut self,
        state: &TerminalMouseState,
        button: MouseButton,
        position: Point<Pixels>,
        modifiers: Modifiers,
    ) {
        let MouseTarget {
            host_id, tab_id, ..
        } = state.target();
        let Some(live) = self
            .mouse_target(host_id, tab_id)
            .filter(|target| target.can_report)
        else {
            return;
        };
        if !self
            .terminal_tab(host_id, tab_id)
            .and_then(|tab| tab.vt.as_ref())
            .is_some_and(|vt| state.matches(&live, vt))
        {
            return;
        }
        // The encoder clamps an outside release to the original pane's grid.
        self.send_terminal_mouse(
            &live,
            position,
            modifiers,
            vt::terminal::MouseAction::Release,
            Some(button),
        );
    }

    fn send_terminal_mouse(
        &mut self,
        target: &MouseTarget,
        position: Point<Pixels>,
        modifiers: Modifiers,
        action: vt::terminal::MouseAction,
        button: Option<MouseButton>,
    ) -> bool {
        let (_, width, height) = self.font_grid;
        let Some(channel) = target.channel.filter(|_| target.can_report) else {
            return false;
        };
        let bytes = self
            .find_tab_mut(&target.host_id, &target.tab_id)
            .and_then(|tab| {
                terminal_interaction::encode_mouse(
                    tab.vt.as_mut()?,
                    tab.surface.bounds.get()?,
                    position,
                    (width, height),
                    modifiers,
                    action,
                    button,
                )
            });
        bytes.is_some_and(|bytes| self.send_terminal_bytes(&target.host_id, channel, &bytes))
    }

    fn terminal_body_events(&self, host: &str, tab: &str, cx: &mut Context<Self>) -> Div {
        let mut body = div();
        for button in [MouseButton::Left, MouseButton::Middle, MouseButton::Right] {
            let (host, tab) = (host.to_owned(), tab.to_owned());
            body = body.on_mouse_down(
                button,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_down(&host, &tab, event, window, cx);
                }),
            );
        }
        let (host, tab) = (host.to_owned(), tab.to_owned());
        let (hover_host, hover_tab) = (host.clone(), tab.clone());
        body.on_scroll_wheel(cx.listener(move |this, event, _, cx| {
            this.terminal_wheel(&host, &tab, event, cx);
        }))
        .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, window, _| {
            if this.terminal_mouse.is_none()
                && this.pane_drag.is_none()
                && this.sidebar_drag.is_none()
                && event.pressed_button.is_none()
                && !event.modifiers.shift
                && this
                    .input_target(window)
                    .is_some_and(|(host, tab, ..)| host == hover_host && tab == hover_tab)
                && let Some(target) = this.mouse_target(&hover_host, &hover_tab)
            {
                this.send_terminal_mouse(
                    &target,
                    event.position,
                    event.modifiers,
                    vt::terminal::MouseAction::Motion,
                    None,
                );
            }
        }))
    }

    fn terminal_mouse_down(
        &mut self,
        host: &str,
        tab_id: &str,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use terminal_interaction::{
            GestureModes, PointerRoute, hit_test_cell, pointer_route, return_to_bottom,
        };
        if self.pane_drag.is_some()
            || self.sidebar_drag.is_some()
            || self.terminal_mouse.is_some()
            || !self.window_active
            || self.active_overlay.is_some()
            || self.update_modal.is_some()
            || !self.tab_is_visible(host, tab_id)
        {
            return;
        }
        let grid = self.terminal_font_grid();
        let Some(target) = self.mouse_target(host, tab_id) else {
            return;
        };
        let Some((route, bounds, modes, badge)) = self.terminal_tab(host, tab_id).and_then(|tab| {
            let vt = tab.vt.as_ref()?;
            let bounds = tab.surface.bounds.get()?;
            Some((
                pointer_route(
                    target.can_report,
                    vt.mouse_tracking(),
                    event.modifiers.shift,
                ),
                bounds,
                GestureModes::read(vt),
                terminal_canvas::scroll_badge_bounds(bounds, vt.scrollbar(), grid),
            ))
        }) else {
            return;
        };
        if route == PointerRoute::Local && event.button != MouseButton::Left {
            return;
        }
        if !self.focus_pane(tab_id, window) {
            return;
        }

        self.terminal_pointer = event.position;
        let button = event.button;
        if button == MouseButton::Left
            && badge.is_some_and(|bounds| bounds.contains(&event.position))
        {
            if let Some(vt) = self
                .find_tab_mut(host, tab_id)
                .and_then(|tab| tab.vt.as_mut())
            {
                return_to_bottom(vt);
            }
        } else {
            match route {
                PointerRoute::App => {
                    if self.send_terminal_mouse(
                        &target,
                        event.position,
                        event.modifiers,
                        vt::terminal::MouseAction::Press,
                        Some(button),
                    ) {
                        self.terminal_mouse = Some(TerminalMouseState::AppMouse {
                            target,
                            modes,
                            button,
                        });
                    }
                }
                PointerRoute::Local => {
                    if let Some(vt) = self
                        .find_tab_mut(host, tab_id)
                        .and_then(|tab| tab.vt.as_mut())
                    {
                        let (cols, rows) = vt.size();
                        if let Some(hit) = hit_test_cell(
                            bounds,
                            event.position,
                            grid.width,
                            grid.height,
                            cols,
                            rows,
                        ) {
                            self.terminal_mouse = Some(TerminalMouseState::start_selection(
                                target,
                                vt,
                                event.position,
                                hit,
                                event.click_count,
                            ));
                        }
                    }
                }
            }
        }
        cx.stop_propagation();
        notify(cx, "terminal_mouse_down");
    }

    fn terminal_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_terminal_interaction(Some(window), cx);
        let Some(mut state) = self.terminal_mouse.take() else {
            return;
        };
        let button = match &state {
            TerminalMouseState::AppMouse { button, .. } => *button,
            TerminalMouseState::Selecting { .. } => MouseButton::Left,
        };
        if event.pressed_button != Some(button) {
            self.terminal_mouse = Some(state);
            self.cancel_terminal_mouse();
            return;
        }
        self.terminal_pointer = event.position;
        let mut outside = false;
        match &state {
            TerminalMouseState::AppMouse { target, .. } => {
                self.send_terminal_mouse(
                    target,
                    event.position,
                    event.modifiers,
                    vt::terminal::MouseAction::Motion,
                    Some(button),
                );
            }
            TerminalMouseState::Selecting { .. } => {
                if let Some((vt, hit)) = self.selection_at(state.target(), event.position) {
                    outside = !vt.alt_screen() && hit.vertical_outside != 0;
                    if state.update_selection(vt, event.position, hit) {
                        notify(cx, "selection");
                    }
                }
            }
        }
        self.terminal_mouse = Some(state);
        if outside && self.terminal_autoscroll.is_none() {
            self.terminal_autoscroll = Some(cx.spawn(async |desktop, cx| {
                loop {
                    cx.background_executor()
                        .timer(terminal_interaction::AUTOSCROLL_INTERVAL)
                        .await;
                    if !desktop
                        .update(cx, |this, cx| this.tick_terminal_autoscroll(cx))
                        .unwrap_or(false)
                    {
                        break;
                    }
                }
            }));
        } else if !outside {
            self.terminal_autoscroll = None;
        }
    }

    fn selection_at(
        &mut self,
        target: &MouseTarget,
        position: Point<Pixels>,
    ) -> Option<(&mut Vt, terminal_interaction::CellHit)> {
        let (_, width, height) = self.font_grid;
        let tab = self.find_tab_mut(&target.host_id, &target.tab_id)?;
        let vt = tab.vt.as_mut()?;
        let (cols, rows) = vt.size();
        let hit = terminal_interaction::hit_test_cell(
            tab.surface.bounds.get()?,
            position,
            width,
            height,
            cols,
            rows,
        )?;
        Some((vt, hit))
    }

    fn tick_terminal_autoscroll(&mut self, cx: &mut Context<Self>) -> bool {
        self.sync_terminal_interaction(None, cx);
        let Some(mut state @ TerminalMouseState::Selecting { .. }) = self.terminal_mouse.take()
        else {
            return false;
        };
        let changed = self
            .selection_at(state.target(), self.terminal_pointer)
            .is_some_and(|(vt, hit)| state.autoscroll(vt, hit));
        self.terminal_mouse = Some(state);
        if changed {
            notify(cx, "selection_autoscroll");
        } else {
            self.terminal_autoscroll = None;
        }
        changed
    }

    fn terminal_mouse_up(&mut self, event: &MouseUpEvent, window: &Window, cx: &mut Context<Self>) {
        self.sync_terminal_interaction(Some(window), cx);
        let Some(mut state) = self.terminal_mouse.take() else {
            return;
        };
        match &state {
            TerminalMouseState::AppMouse { button, .. } if *button == event.button => {
                self.release_app_mouse(&state, *button, event.position, event.modifiers);
            }
            TerminalMouseState::Selecting { .. } if event.button == MouseButton::Left => {
                if let Some((vt, hit)) = self.selection_at(state.target(), event.position)
                    && state.update_selection(vt, event.position, hit)
                {
                    notify(cx, "selection");
                }
            }
            _ => {
                self.terminal_mouse = Some(state);
                return;
            }
        }
        self.terminal_autoscroll = None;
    }

    fn terminal_wheel(
        &mut self,
        host: &str,
        tab_id: &str,
        event: &ScrollWheelEvent,
        cx: &mut Context<Self>,
    ) {
        use terminal_interaction::{WheelRoute, encode_wheel, wheel_route};
        if self.terminal_mouse.is_some()
            || self.pane_drag.is_some()
            || self.sidebar_drag.is_some()
            || !self.window_active
            || self.active_overlay.is_some()
            || self.update_modal.is_some()
            || !self.tab_is_visible(host, tab_id)
        {
            return;
        }
        let (_, width, height) = self.font_grid;
        let Some(target) = self.mouse_target(host, tab_id) else {
            return;
        };
        let Some(tab) = self.find_tab_mut(host, tab_id) else {
            return;
        };
        let (Some(vt), Some(bounds)) = (tab.vt.as_mut(), tab.surface.bounds.get()) else {
            return;
        };
        let route = wheel_route(target.can_report, vt, event.modifiers.shift);
        tab.surface.scroll.set_route(route);
        cx.stop_propagation();
        if matches!(route, WheelRoute::Ignore) {
            tab.surface.scroll.reset();
            return;
        }
        let rows = tab
            .surface
            .scroll
            .rows(f32::from(event.delta.pixel_delta(px(height)).y), height);
        if rows == 0 {
            return;
        }
        if matches!(route, WheelRoute::Local) {
            let offset = vt.scrollbar().offset;
            vt.scroll(rows);
            if vt.scrollbar().offset != offset {
                notify(cx, "scroll");
            }
        } else if let Some(channel) = target.channel {
            let bytes = encode_wheel(
                vt,
                route,
                rows,
                bounds,
                event.position,
                (width, height),
                event.modifiers,
            );
            if !bytes.is_empty() {
                self.send_terminal_bytes(host, channel, &bytes);
            }
        }
    }

    fn send_terminal_bytes(&self, host: &str, channel: u32, bytes: &[u8]) -> bool {
        #[cfg(test)]
        if let Some(sink) = &self.test_input_sink {
            sink(host, channel, bytes);
            return true;
        }
        self.bridge.send_input(host, channel, bytes)
    }

    fn send_terminal_text(
        &mut self,
        target: &InputTarget,
        bytes: &[u8],
        cx: &mut Context<Self>,
    ) -> bool {
        let (host, tab_id, _, channel) = target;
        if bytes.is_empty() || !self.send_terminal_bytes(host, *channel, bytes) {
            return false;
        }
        if let Some(tab) = self.find_tab_mut(host, tab_id) {
            tab.surface.scroll.reset();
            if tab
                .vt
                .as_mut()
                .is_some_and(terminal_interaction::return_to_bottom)
            {
                notify(cx, "terminal_input");
            }
        }
        true
    }

    fn paste_target(&self, window: &Window) -> Option<InputTarget> {
        self.peek
            .is_none()
            .then(|| self.input_target(window))
            .flatten()
    }

    fn paste_is_bracketed(&self, target: Option<&InputTarget>) -> bool {
        target
            .and_then(|(host, tab_id, ..)| self.terminal_tab(host, tab_id))
            .and_then(|tab| tab.vt.as_ref())
            .is_some_and(Vt::bracketed_paste)
    }

    fn send_paste(&mut self, target: &InputTarget, bytes: &[u8], cx: &mut Context<Self>) {
        if !bytes.is_empty() && !self.send_terminal_text(target, bytes, cx) {
            self.notice = Some("Failed to paste into terminal.".into());
            cx.notify();
        }
    }

    fn confirm_paste(&mut self, accept: bool, window: &Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_paste.take() else {
            return;
        };
        if accept {
            let target = self.paste_target(window);
            let bracketed = self.paste_is_bracketed(target.as_ref());
            if let Some(bytes) = pending.into_bytes(target.as_ref(), bracketed)
                && let Some(target) = target
            {
                self.send_paste(&target, &bytes, cx);
            }
        }
        cx.notify();
    }

    fn render_pending_paste(&self, cx: &mut Context<Self>) -> Option<Div> {
        let pending = self.pending_paste.as_ref()?;
        Some(
            div()
                .debug_selector(|| "paste-confirmation".into())
                .absolute()
                .left_0()
                .right_0()
                .top_0()
                .occlude()
                .flex()
                .items_center()
                .gap_2()
                .px_1()
                .py_0p5()
                .bg(rgb(0x2d2415))
                .child(div().flex_1().child(format!(
                    "Paste multiple lines? {} lines · {} bytes",
                    pending.line_count, pending.byte_len
                )))
                .child(Self::action_button(
                    cx,
                    "confirm-paste",
                    "Paste (Enter)",
                    |this, window, cx| this.confirm_paste(true, window, cx),
                ))
                .child(Self::action_button(
                    cx,
                    "cancel-paste",
                    "Cancel (Esc)",
                    |this, window, cx| this.confirm_paste(false, window, cx),
                )),
        )
    }

    fn terminal_key(&mut self, event: &KeyDownEvent, window: &Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        let was_pending = self.pending_paste.is_some();
        if self.sync_terminal_interaction(Some(window), cx) {
            cx.notify();
        }
        if key.key == "enter" {
            if event.is_held && self.swallow_paste_enter_repeat {
                cx.stop_propagation();
                return;
            }
            self.swallow_paste_enter_repeat = false;
        }
        let m = key.modifiers;
        if was_pending && !m.control && !m.platform && !m.shift && !m.alt && !m.function {
            match key.key.as_str() {
                "enter" | "escape" => {
                    if !event.is_held {
                        self.swallow_paste_enter_repeat = key.key == "enter";
                        self.confirm_paste(key.key == "enter", window, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }
        if self.terminal_shortcut(key, window, cx) {
            cx.stop_propagation();
            return;
        }
        if self.pending_paste.is_some() {
            if m.control && !m.shift && !m.alt && !m.platform && key.key == "c" {
                self.confirm_paste(false, window, cx);
            } else {
                cx.stop_propagation();
                return;
            }
        }
        let Some(target) = self.input_target(window) else {
            return;
        };
        if m.platform {
            return;
        }
        if !m.control
            && !m.alt
            && key
                .key_char
                .as_ref()
                .is_some_and(|s| !s.chars().any(char::is_control))
        {
            // The platform input handler owns committed text and IME composition.
            return;
        }
        if let Some(bytes) = self
            .find_tab_mut(&target.0, &target.1)
            .and_then(|tab| tab.vt.as_mut())
            .and_then(|vt| vt.key(&key.key, m.shift, m.control, m.alt))
        {
            self.send_terminal_text(&target, &bytes, cx);
        }
        cx.stop_propagation();
    }

    fn request_terminal_paste(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
        read: impl FnOnce(&mut Context<Self>) -> Option<String>,
    ) {
        use terminal_interaction::{Paste, prepare_paste};
        if self.pending_paste.take().is_some() {
            cx.notify();
        }
        let Some(target) = self.paste_target(window) else {
            return;
        };
        let Some(vt) = self
            .terminal_tab(&target.0, &target.1)
            .and_then(|tab| tab.vt.as_ref())
        else {
            return;
        };
        match prepare_paste(Some(target.clone()), vt, || read(cx)) {
            Ok(Some(Paste::Ready(bytes))) => self.send_paste(&target, &bytes, cx),
            Ok(Some(Paste::Confirm(pending))) => {
                self.pending_paste = Some(pending);
                self.terminal_input = None;
                cx.notify();
            }
            Ok(None) => {}
            Err(error) => {
                self.notice = Some(
                    match error {
                        vt::terminal::PasteError::TooLarge => "The paste limit is 64 KiB.",
                        _ => "This paste is unsafe or cannot be processed.",
                    }
                    .into(),
                );
                cx.notify();
            }
        }
    }

    fn terminal_shortcut(
        &mut self,
        key: &Keystroke,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        use terminal_interaction::{copy_selection, scroll_shortcut};
        if !self.terminal_has_keyboard() || !self.focus.is_focused(window) {
            return false;
        }
        let Some(shortcut) = TerminalShortcut::from_keystroke(key, cfg!(target_os = "macos"))
        else {
            return false;
        };
        match shortcut {
            TerminalShortcut::Copy => {
                if let Some(vt) = self
                    .active_terminal_target()
                    .and_then(|(_, tab)| tab.vt.as_ref())
                {
                    copy_selection(vt, |text| {
                        cx.write_to_clipboard(ClipboardItem::new_string(text))
                    });
                }
            }
            TerminalShortcut::Paste => self.request_terminal_paste(window, cx, |cx| {
                cx.read_from_clipboard().and_then(|item| item.text())
            }),
            _ => {
                let Some((host, tab)) = self.active_terminal_target() else {
                    return false;
                };
                let (host, tab) = (host.to_owned(), tab.id.clone());
                let Some(vt) = self
                    .find_tab_mut(&host, &tab)
                    .and_then(|tab| tab.vt.as_mut())
                else {
                    return false;
                };
                if vt.alt_screen() {
                    return false;
                }
                if scroll_shortcut(vt, shortcut) {
                    notify(cx, "scroll");
                }
            }
        }
        true
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
    if split {
        scale.px(PANE_HEADER_HEIGHT)
    } else {
        0.
    }
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
        perf(|p| p.desktop_renders += 1);
        self.sync_terminal_interaction(Some(window), cx);
        self.spinner_slots.borrow_mut().clear();
        self.sync_spinner(cx);
        if let Some(scale) = self.pending_ui_scale.take() {
            self.apply_ui_scale(scale, window, cx);
        }
        let viewport = window.viewport_size();
        let viewport_width = f32::from(viewport.width);
        let scale = self.ui_scale;
        let sidebar = sidebar_px(self.sidebar_base, scale, viewport_width);
        let divider = scale
            .px(DIVIDER_WIDTH)
            .min((viewport_width - sidebar - 1.).max(0.));
        let tab_bar_height = scale.lookup(&const {
            ui_scale::zoom_table(TAB_BAR_HEIGHT_REM * BASE_REM_PX, false, 0.)
        });
        let reset_banner_height = scale.px(RESET_BANNER_HEIGHT);
        let (_, width, height) = self.font_grid;

        let protocol_card = self.active_machine().and_then(BlockedCard::for_machine);
        let update_banner = self.update_banner_view(protocol_card.is_some(), cx);
        let blocked_msg = self.active_machine().and_then(|m| match &m.conn_state {
            ConnectionState::HostKeyMismatch(msg) => Some(format!(
                "Security warning: Connection blocked because the remote host key does not match.\n{msg}"
            )),
            ConnectionState::AuthRequired(msg) => Some(format!("SSH authentication is required.\n{msg}")),
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

        let peek_banner = self.peek_banner(cx);
        let old_host_banner = self.old_host_banner(cx);
        let peeking = peek_banner.is_some();
        let show_tabs = !peeking && blocked_msg.is_none() && self.has_selectable_active_room();
        let show_closing = !peeking && blocked_msg.is_none() && self.is_closing_last_room();
        let show_empty = !peeking && blocked_msg.is_none() && self.is_active_connected()
            && !show_tabs && !show_closing;

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
                        .px_1p5()
                        .bg(rgb(0x222832))
                        .text_color(rgb(0xdbe5f6))
                        .text_xs()
                        .child(div().flex_1().min_w_0().truncate().child(format!(
                            "Backed up the existing v1 catalog ({})",
                            reset_backup_label(notice)
                        )))
                        .child(
                            icon_box(px(scale.px(17.)))
                                .id(stable_elem_id("dismiss-reset", &[&host_id, &notice_id]))
                                .group(ICON_HOVER_GROUP)
                                .rounded_sm()
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(0x394354)))
                                .child(hover_icon(Icon::X, scale.px(10.), 0xdbe5f6))
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
            scale.px(TAB_BAR_HEIGHT_REM * BASE_REM_PX)
        } else {
            0.
        } + if peeking {
            scale.px(PEEK_BANNER_HEIGHT)
        } else {
            0.
        }
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
                            .mr_1()
                            .px_1()
                            .py_0p5()
                            .flex_none()
                            .whitespace_nowrap()
                            .text_xs()
                            .text_color(rgb(0xbcbcbc))
                            .rounded_sm()
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
        let terminal = (peeking || blocked_msg.is_none() && !show_empty && !show_closing)
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
                            notify(cx, "pane_drag");
                        }
                    } else if let Some(offset) = this.sidebar_drag {
                        let new_px = (f32::from(event.position.x) - offset)
                            .min(sidebar_max_px(viewport_width));
                        let base =
                            (new_px / this.ui_scale.factor()).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
                        if base != this.sidebar_base {
                            this.sidebar_base = base;
                            if sidebar_px(base, this.ui_scale, viewport_width) != sidebar {
                                notify(cx, "sidebar_drag");
                            }
                        }
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
            .on_mouse_up_out(MouseButton::Right, cx.listener(Self::end_resize_drag))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::end_resize_drag))
            .child(
                div()
                    .debug_selector(|| "sidebar".into())
                    .flex_none()
                    .w(px(sidebar))
                    .min_w_0()
                    .overflow_hidden()
                    .when(sidebar >= scale.px(20.), |el| el.p_1p5())
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(scale.px(9.)))
                            .child(
                                img(self.icon.clone())
                                    .size(px(scale.px(14.)))
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
                            .pt_1()
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
                                    .px_1()
                                    .py_0p5()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_color(rgb(0x9a9a9a))
                                    .hover(|s| s.bg(rgb(0x262626)).text_color(rgb(0xffffff)))
                                    .child(hover_icon(Icon::Server, scale.px(12.), 0x9a9a9a))
                                    .child(if sidebar <= scale.px(SIDEBAR_COMPACT_MAX) {
                                        "+ Machine"
                                    } else {
                                        "Add Machine"
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
                                    .child(icon(Icon::Settings, scale.px(12.), rgb(0xdddddd)))
                                    .when(sidebar > scale.px(SIDEBAR_COMPACT_MAX), |d| {
                                        d.child(if sidebar <= scale.px(204.) {
                                            "Agents"
                                        } else {
                                            "Agent Settings"
                                        })
                                    }),
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
                            this.cancel_terminal_mouse();
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
                                .p_3()
                                .child(
                                    div()
                                        .max_w(px(scale.px(408.)))
                                        .p_2()
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
                                                    Self::warning_action(
                                                        div().id("machine-install-btn").px_1p5().py_0p5().rounded_sm(),
                                                    )
                                                        .cursor_pointer()
                                                        .child(if confirm {
                                                            "Confirm Remote Installation"
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
                    .when(show_closing, |pane| {
                        pane.child(
                            div()
                                .debug_selector(|| "closing-room-placeholder".into())
                                .flex_1()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(rgb(0xa0a0a0))
                                .child("Closing session..."),
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
                                .child(
                                    div()
                                        .debug_selector(|| "empty-room-placeholder".into())
                                        .text_color(rgb(0xa0a0a0))
                                        .child("No open rooms."),
                                )
                                .child(Self::action_button(
                                    cx,
                                    "empty-agent",
                                    "Start Default Agent",
                                    |this, _, _| this.launch_default(2),
                                ))
                                .child(
                                    div()
                                        .id("empty-new-room")
                                        .group(ICON_HOVER_GROUP)
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .px_1p5()
                                        .py_0p5()
                                        .rounded_sm()
                                        .hover(|s| s.bg(rgb(0x505050)))
                                        .cursor_pointer()
                                        .child(hover_icon(Icon::Plus, scale.px(11.), 0xf2f2f2))
                                        .child("New Room")
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
    let width = terminal_cell_width(ts.advance(id, font_size, 'M').ok(), scale);
    let height =
        (1.15 * f32::from(ts.ascent(id, font_size) + ts.descent(id, font_size).abs())).ceil();
    (family, width, height)
}

fn terminal_cell_width(measured: Option<Size<Pixels>>, scale: UiScale) -> f32 {
    measured
        .map(|advance| f32::from(advance.width))
        .unwrap_or(scale.term_font_px() * (8.43 / 14.))
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
                let desktop = cx.new(|cx| {
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
                    this
                });
                cx.new(|cx| DesktopWindow::new(desktop, cx))
            })
            .expect("open DDG Agents window");
        #[cfg(target_os = "macos")]
        {
            let desktop = _main_window
                .entity(cx)
                .expect("desktop root")
                .read(cx)
                .desktop
                .downgrade();
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
                    MenuItem::action("Check for Updates...", CheckUpdates),
                    MenuItem::separator(),
                    MenuItem::action("Quit Ddg", Quit),
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

    #[test]
    fn pending_close_projection_keeps_stopped_survivor_and_authoritative_layout() {
        let mut record = split_room_rec();
        record.tabs[2] = tab_rec(3, false);
        let tabs = record.tabs.iter().map(|tab| TabView::from_record(tab, (80, 24))).collect();
        let mut room = RoomView::from_record(&record, tabs);
        room.tabs[0].closing = true;
        room.tabs[1].closing = true;
        assert!(!room.select_tab_by_id(&record.tabs[1].id));
        assert!(room.select_projected_group(0));
        assert_eq!(room.selected_group, 1);
        assert_eq!(room.focused_tab_id(), Some(record.tabs[2].id.as_str()));
        assert_eq!(room.visible_tab_ids(), [record.tabs[2].id.clone()]);
        assert!(!room.is_active_group_split());
        assert!(!room.select_projected_group(1));
        room.tabs[2].closing = true;
        assert!(!room.ensure_selectable_focus());
        assert!(room.visible_tab_ids().is_empty());
        assert!(room.projected_groups().is_empty());
        assert_eq!(room.layout, *record.layout.as_ref().unwrap());
        assert_eq!(room.layout, room.server_layout);
    }

    #[test]
    fn pending_close_rejects_late_attach_without_replacing_existing_channel() {
        let record = tab_rec(1, true);
        let mut tab = TabView::from_record(&record, (80, 24));
        tab.closing = true;
        tab.channel = Some(4);
        let handle = TabStreamHandle {
            tab_id: record.id,
            channel: 5,
            mode: ControlMode::Control,
            control_epoch: 1,
            cols: 80,
            rows: 24,
            run_generation: record.run_generation,
        };
        assert_eq!(accept_tab_attach(Some(&mut tab), &handle), Some(5));
        assert_eq!(tab.channel, Some(4));
        assert!(tab.vt.is_none());
        assert!(tab.closing);
    }

    mod headless {
        use super::super::{
            AppAssets, Desktop, HostBridge, HostsConfig, RoomView, TabView, UiScale,
        };
        use super::split_room_rec;
        use gpui::{AppContext, Entity, TestAppContext, VisualTestContext, px, size};

        const LEFT: &str = "pane-ses_00000000000000000000000002";
        const RIGHT: &str = "pane-ses_00000000000000000000000003";
        const LEFT_BODY: &str = "terminal-body-ses_00000000000000000000000002";

        fn desktop(cx: &mut TestAppContext) -> (Entity<Desktop>, &mut VisualTestContext) {
            let (root, cx) = cx.add_window_view(|window, cx| {
                let desktop = cx.new(|cx| {
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
                });
                super::super::DesktopWindow::new(desktop, cx)
            });
            (cx.update(|_, cx| root.read(cx).desktop.clone()), cx)
        }

        #[gpui::test]
        fn preset_keyboard_switch_refresh_and_reopen_reject_stale_answers(cx: &mut TestAppContext) {
            use super::super::{
                AgentSettingsModal, MachineView, PresetControl, PresetDetectionState,
            };
            use crate::{HostProfile, agent_modal::Field};
            use gpui::{Keystroke, Modifiers};

            let dir = tempfile::tempdir().unwrap();
            let (view, cx) = desktop(cx);
            cx.update(|window, app| {
                view.update(app, |d, cx| {
                    let mut bridge = HostBridge::new(&[]);
                    let mut inbox = bridge.take_inbox();
                    d.bridge = bridge;
                    let mut remote = HostProfile::local_default();
                    remote.id = "remote".into();
                    d.machines.push(MachineView::new(remote));
                    let path = dir.path().join("agents.json");
                    let open = |d: &mut Desktop| {
                        d.active_overlay = Some(crate::ActiveOverlay::AgentSettingsModal(Box::new(
                            AgentSettingsModal::load(path.clone()),
                        )));
                    };
                    let key = |key: &str| Keystroke {
                        key: key.into(),
                        key_char: None,
                        modifiers: Modifiers::default(),
                    };
                    open(d);
                    d.agent_modal_mut().unwrap().host = Some("override".into());
                    d.request_preset_detection("local".into());
                    let old_host = inbox.try_recv().unwrap().message;
                    assert!(d.handle_preset_key(&key("tab"), window));
                    assert_eq!(d.preset_control, Some(PresetControl::Machine));
                    assert!(d.handle_preset_key(&key("right"), window));
                    let before_refresh = inbox.try_recv().unwrap().message;
                    assert_eq!(d.agent_modal().unwrap().detect_host, "remote");
                    assert_eq!(d.agent_modal().unwrap().host.as_deref(), Some("override"));
                    assert!(d.handle_preset_key(&key("tab"), window));
                    assert_eq!(d.preset_control, Some(PresetControl::Refresh));
                    assert!(d.handle_preset_key(&key("enter"), window));
                    let before_close = inbox.try_recv().unwrap().message;
                    d.close_agent_settings();
                    open(d);
                    d.request_preset_detection("remote".into());
                    let current = inbox.try_recv().unwrap().message;
                    for stale in [old_host, before_refresh, before_close] {
                        assert!(d.bridge.is_current(&stale), "request guard must reject it");
                        d.apply_bridge_message(stale, &mut inbox, cx);
                        assert_eq!(d.agent_modal().unwrap().detection, PresetDetectionState::Loading);
                    }
                    d.apply_bridge_message(current, &mut inbox, cx);
                    let modal = d.agent_modal().unwrap();
                    assert!(matches!(modal.detection, PresetDetectionState::Unavailable(_)));
                    assert!(modal.field == Field::Id);
                    assert!(!path.exists(), "navigation and detection must not save the draft");
                });
            });
        }

        fn test_tab<'a>(d: &'a Desktop, id: &str) -> &'a TabView {
            d.room()
                .unwrap()
                .tabs
                .iter()
                .find(|tab| tab.id == id)
                .unwrap()
        }

        fn connected_terminals(d: &mut Desktop) {
            use super::super::{ConnectionState, ControlMode, Vt};
            d.machines[0].conn_state = ConnectionState::Connected {
                host_id: "local".into(),
                conn_id: 1,
                features: vec![],
            };
            for (i, tab) in d.room_mut().unwrap().tabs.iter_mut().enumerate() {
                tab.channel = Some(10 + i as u32);
                tab.mode = ControlMode::Readonly;
                tab.vt = Some(Vt::new(80, 24).unwrap());
            }
        }

        fn observe_input(d: &mut Desktop) -> std::rc::Rc<std::cell::RefCell<Vec<Vec<u8>>>> {
            let bytes = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let sink = bytes.clone();
            d.test_input_sink = Some(Box::new(move |host, channel, packet| {
                assert_eq!((host, channel), ("local", 11));
                sink.borrow_mut().push(packet.to_vec());
            }));
            bytes
        }

        mod pending_close {
            use super::{TestAppContext, VisualTestContext, desktop};
            use crate::tests::{room_rec, split_room_rec, tab_rec};
            use crate::{
                BridgeEvent, BridgeInbox, Catalog, ConnectionState, ControlMode, Desktop,
                HostBridge, HostProfile, MachineView, NavDir, RoomLayout, RoomRecord, RoomView,
                SplitAxis, TestTabRequest, Vt, first_leaf,
            };
            use gpui::{Context, Entity, KeyDownEvent, Keystroke, Modifiers, MouseButton};

            fn fixture<'a>(
                cx: &'a mut TestAppContext,
                rooms: &[RoomRecord],
                active: usize,
                focused: usize,
            ) -> (Entity<Desktop>, &'a mut VisualTestContext, BridgeInbox) {
                let (view, cx) = desktop(cx);
                let mut bridge = HostBridge::new(&[]);
                let inbox = bridge.take_inbox();
                // No client, and no inbox consumer: only events explicitly injected by tests apply.
                cx.update(|window, app| {
                    view.update(app, |d, cx| {
                        d.bridge = bridge;
                        d.machines[0].rooms = rooms.iter().map(room_view).collect();
                        d.machines[0].conn_state = ConnectionState::Connected {
                            host_id: "local".into(),
                            conn_id: 1,
                            features: vec![],
                        };
                        d.machines[0].catalog_seq = Some(1);
                        d.active_room_id = Some(rooms[active].id.clone());
                        assert!(d.room_mut().unwrap().select_tab_by_id(&rooms[active].tabs[focused].id));
                        window.activate_window();
                        d.window_active = true;
                        d.focus.focus(window);
                        cx.notify();
                    });
                });
                frame(cx);
                (view, cx, inbox)
            }

            fn room_view(record: &RoomRecord) -> RoomView {
                RoomView::from_record(
                    record,
                    record.tabs.iter().map(|tab| crate::TabView::from_record(tab, (80, 24))).collect(),
                )
            }

            fn standalone() -> RoomRecord {
                room_rec(1, (1..=3).map(|id| tab_rec(id, true)).collect())
            }

            fn chord(key: &str) -> Keystroke {
                Keystroke {
                    key: key.into(),
                    key_char: None,
                    modifiers: Modifiers {
                        platform: cfg!(target_os = "macos"),
                        control: !cfg!(target_os = "macos"),
                        ..Modifiers::default()
                    },
                }
            }

            fn frame(cx: &mut VisualTestContext) {
                cx.update(|window, app| {
                    // Cached GPUI views don't replay debug selectors on a cache hit.
                    window.refresh();
                    window.draw(app).clear();
                });
            }

            fn press(cx: &mut VisualTestContext, key: &str) {
                assert!(cx.update(|window, app| window.dispatch_keystroke(chord(key), app)));
                frame(cx);
            }

            fn inject(
                d: &mut Desktop,
                cx: &mut Context<Desktop>,
                inbox: &mut BridgeInbox,
                event: BridgeEvent,
            ) {
                d.handle_host_event("local", None, event, inbox, cx);
                cx.notify();
            }

            fn catalog(seq: u64, rooms: Vec<RoomRecord>) -> BridgeEvent {
                let mut catalog = Catalog::empty("local");
                catalog.rooms = rooms;
                BridgeEvent::CatalogUpdated { seq, catalog }
            }

            fn failed(tab_id: &str) -> BridgeEvent {
                BridgeEvent::TabCloseFailed {
                    tab_id: tab_id.into(),
                    message: "Close failed".into(),
                }
            }

            fn close(tab_id: &str) -> TestTabRequest {
                TestTabRequest::Close("local".into(), tab_id.into())
            }

            fn attach(room: &RoomRecord, index: usize) -> TestTabRequest {
                TestTabRequest::Attach("local".into(), room.id.clone(), room.tabs[index].id.clone())
            }

            fn projected(d: &Desktop) -> Vec<(usize, String)> {
                d.room().unwrap().projected_groups().iter()
                    .map(|(index, node)| (*index, first_leaf(node).into()))
                    .collect()
            }

            #[gpui::test]
            fn first_middle_and_last_close_use_visible_neighbor_before_reply(cx: &mut TestAppContext) {
                let record = standalone();
                for (target, survivor) in [(0, 1), (1, 2), (2, 1)] {
                    let (view, cx, _inbox) = fixture(cx, std::slice::from_ref(&record), 0, target);
                    let id = |i: usize| record.tabs[i].id.as_str();
                    press(cx, "w");
                    cx.update(|_, app| view.update(app, |d, _| {
                        let room = d.room().unwrap();
                        assert!(room.tabs[target].closing);
                        assert_eq!(room.tabs[target].status, "Closing");
                        assert_eq!(room.tabs.len(), 3);
                        assert_eq!(room.layout, RoomLayout::default_for_tabs(&record.tabs));
                        assert_eq!(room.layout, room.server_layout);
                        assert_eq!(room.focused_tab_id(), Some(id(survivor)));
                        assert_eq!(projected(d), (0..3).filter(|i| *i != target)
                            .map(|i| (i, id(i).to_owned())).collect::<Vec<_>>());
                        assert_eq!(d.pane_geometry().0, [(id(survivor).to_owned(), d.canvas_bounds())]);
                        assert_eq!(d.test_tab_requests, [close(id(target)), attach(&record, survivor)]);
                    }));
                    if target == 1 {
                        cx.update(|_, app| view.update(app, |d, _| {
                            let tab = &mut d.room_mut().unwrap().tabs[2];
                            tab.channel = Some(12);
                            tab.mode = ControlMode::Control;
                            tab.vt = Some(Vt::new(80, 24).unwrap());
                            tab.reset_attach();
                            assert!(d.control_target().is_some());
                            d.test_input_sink = Some(Box::new(|_, _, _| {
                                panic!("held close must not send terminal input");
                            }));
                        }));
                        cx.simulate_event(KeyDownEvent { keystroke: chord("w"), is_held: true });
                        cx.update(|_, app| view.update(app, |d, _| {
                            assert_eq!(d.room().unwrap().focused_tab_id(), Some(id(2)));
                            assert_eq!(d.test_tab_requests.len(), 2);
                        }));
                        press(cx, "w");
                        cx.update(|_, app| view.update(app, |d, _| {
                            assert!(d.room().unwrap().tabs[2].closing);
                            assert_eq!(d.room().unwrap().focused_tab_id(), Some(id(0)));
                            assert_eq!(d.pane_geometry().0, [(id(0).to_owned(), d.canvas_bounds())]);
                            assert_eq!(d.test_tab_requests, [
                                close(id(1)), attach(&record, 2), close(id(2)), attach(&record, 0),
                            ]);
                        }));
                    }
                }
            }

            #[gpui::test]
            fn active_cross_collapses_zoomed_split_to_full_canvas(cx: &mut TestAppContext) {
                let record = split_room_rec();
                let (view, cx, _inbox) = fixture(cx, std::slice::from_ref(&record), 0, 2);
                cx.update(|_, app| view.update(app, |d, cx| {
                    d.toggle_zoom();
                    cx.notify();
                }));
                frame(cx);
                assert_eq!(cx.debug_bounds(super::RIGHT), cx.debug_bounds("terminal"));
                let cross = cx.debug_bounds("close-tab").unwrap();
                cx.simulate_click(cross.center(), Modifiers::none());
                frame(cx);
                assert_eq!(cx.debug_bounds(super::LEFT), cx.debug_bounds("terminal"));
                cx.update(|_, app| view.update(app, |d, _| {
                    let room = d.room().unwrap();
                    assert_eq!(room.focused_tab_id(), Some(record.tabs[1].id.as_str()));
                    assert_eq!(room.zoomed_tab_id, None);
                    assert!(!room.is_active_group_split());
                    let layout = room.layout.clone();
                    d.unsplit_focused();
                    assert_eq!(d.room().unwrap().layout, layout);
                    assert_eq!(d.test_tab_requests, [close(&record.tabs[2].id), attach(&record, 1)]);
                }));
            }

            #[gpui::test]
            fn nested_close_suppresses_divider_until_failure_or_authoritative_removal(cx: &mut TestAppContext) {
                let mut record = split_room_rec();
                record.tabs.push(tab_rec(4, true));
                let target = record.tabs[2].id.clone();
                let layout = record.layout.as_mut().unwrap();
                assert!(layout.split_leaf(&target, SplitAxis::Vertical, record.tabs[3].id.clone(), 5000).unwrap());
                let (view, cx, mut inbox) = fixture(cx, std::slice::from_ref(&record), 0, 2);
                let prior = cx.debug_bounds("pane-divider-0").unwrap();
                assert!(cx.debug_bounds("pane-divider-1").is_some());
                press(cx, "w");
                cx.simulate_mouse_down(prior.center(), MouseButton::Left, Modifiers::none());
                cx.update(|_, app| view.update(app, |d, _| {
                    assert!(d.pane_drag.is_none());
                    assert_eq!(d.pane_geometry().0.len(), 2, "a split still survives");
                    assert!(d.pane_geometry().1.is_empty());
                    assert_eq!(d.room().unwrap().layout, *record.layout.as_ref().unwrap());
                }));
                cx.simulate_mouse_up(prior.center(), MouseButton::Left, Modifiers::none());
                cx.update(|_, app| view.update(app, |d, cx| {
                    d.focus_direction(NavDir::Left);
                    assert_eq!(d.room().unwrap().focused_tab_id(), Some(record.tabs[1].id.as_str()));
                    inject(d, cx, &mut inbox, failed(&target));
                    assert_eq!(d.pane_geometry().1.len(), 2);
                }));
                frame(cx);
                let restored = cx.debug_bounds("pane-divider-1").unwrap().center();
                cx.simulate_mouse_down(restored, MouseButton::Left, Modifiers::none());
                cx.update(|_, app| view.update(app, |d, _| assert!(d.pane_drag.is_some())));
                cx.simulate_mouse_up(restored, MouseButton::Left, Modifiers::none());
                cx.update(|_, app| view.update(app, |d, _| {
                    assert_eq!(d.room().unwrap().focused_tab_id(), Some(record.tabs[1].id.as_str()));
                    assert!(d.room_mut().unwrap().select_tab_by_id(&target));
                }));
                press(cx, "w");
                let mut removed = record.clone();
                removed.tabs.remove(2);
                cx.update(|_, app| view.update(app, |d, cx| {
                    inject(d, cx, &mut inbox, catalog(2, vec![removed]));
                    assert_eq!(d.pane_geometry().1.len(), 1);
                }));
                frame(cx);
                let divider = cx.debug_bounds("pane-divider-0").unwrap().center();
                cx.simulate_mouse_down(divider, MouseButton::Left, Modifiers::none());
                cx.update(|_, app| view.update(app, |d, _| assert!(d.pane_drag.is_some())));
                cx.simulate_mouse_up(divider, MouseButton::Left, Modifiers::none());
            }

            #[gpui::test]
            fn last_tab_switches_only_to_selectable_sibling_room(cx: &mut TestAppContext) {
                let first = room_rec(1, vec![tab_rec(1, true)]);
                let second = room_rec(2, vec![tab_rec(2, true)]);
                let (view, cx, mut inbox) = fixture(cx, &[first.clone(), second.clone()], 1, 0);
                press(cx, "w");
                cx.update(|_, app| view.update(app, |d, cx| {
                    assert_eq!(d.active_host_id, "local");
                    assert_eq!(d.active_room_id.as_ref(), Some(&first.id));
                    assert_eq!(d.room().unwrap().focused_tab_id(), Some(first.tabs[0].id.as_str()));
                    assert_eq!(d.navigable_rooms().len(), 1);
                    assert_eq!(d.navigable_rooms()[0].room_id, first.id);
                    assert_eq!(d.machines[0].rooms.len(), 2);
                    assert_eq!(d.test_tab_requests, [close(&second.tabs[0].id), attach(&first, 0)]);
                    inject(d, cx, &mut inbox, failed(&second.tabs[0].id));
                    assert_eq!(d.active_room_id.as_ref(), Some(&first.id));
                    assert_eq!(d.navigable_rooms().len(), 2);
                }));
            }

            #[gpui::test]
            fn last_tab_placeholder_gates_input_and_rolls_back_or_becomes_empty(cx: &mut TestAppContext) {
                let record = room_rec(1, vec![tab_rec(2, true)]);
                let id = &record.tabs[0].id;
                let (view, cx, mut inbox) = fixture(cx, std::slice::from_ref(&record), 0, 0);
                let writes = std::rc::Rc::new(std::cell::Cell::new(0));
                cx.update(|_, app| view.update(app, |d, _| {
                    let tab = &mut d.room_mut().unwrap().tabs[0];
                    tab.channel = Some(10);
                    tab.mode = ControlMode::Control;
                    tab.vt = Some(Vt::new(80, 24).unwrap());
                    let mut profile = HostProfile::local_default();
                    profile.id = "remote".into();
                    let mut remote = MachineView::new(profile);
                    remote.rooms.push(room_view(&room_rec(2, vec![tab_rec(3, true)])));
                    remote.conn_state = d.machines[0].conn_state.clone();
                    remote.catalog_seq = Some(1);
                    d.machines.push(remote);
                    let writes = writes.clone();
                    d.test_input_sink = Some(Box::new(move |_, _, _| writes.set(writes.get() + 1)));
                }));
                assert!(cx.debug_bounds(super::LEFT).is_some());
                press(cx, "w");
                assert!(cx.debug_bounds("closing-room-placeholder").is_some());
                assert!(cx.debug_bounds("empty-room-placeholder").is_none());
                cx.simulate_keystrokes("x");
                press(cx, "w");
                assert_eq!(writes.get(), 0);
                cx.update(|_, app| view.update(app, |d, cx| {
                    assert_eq!(d.active_host_id, "local");
                    assert_eq!(d.active_room_id.as_ref(), Some(&record.id));
                    assert!(!d.has_selectable_active_room());
                    assert!(d.is_closing_last_room());
                    assert!(d.pane_geometry().0.is_empty());
                    assert!(d.control_target().is_none());
                    assert_eq!(d.test_tab_requests, [close(id)]);
                    d.room_mut().unwrap().tabs[0].channel = None;
                    inject(d, cx, &mut inbox, failed(id));
                    assert!(!d.room().unwrap().tabs[0].closing);
                    assert_eq!(d.room().unwrap().focused_tab_id(), Some(id.as_str()));
                    assert_eq!(d.notice.as_deref(), Some("Close failed"));
                    assert_eq!(d.test_tab_requests, [close(id), attach(&record, 0)]);
                }));
                frame(cx);
                assert!(cx.debug_bounds(super::LEFT).is_some());
                press(cx, "w");
                cx.update(|_, app| view.update(app, |d, cx| {
                    inject(d, cx, &mut inbox, catalog(2, vec![]));
                    assert_eq!(d.active_host_id, "local");
                    assert!(d.room().is_none());
                    assert!(!d.is_closing_last_room());
                    assert_eq!(d.test_tab_requests, [close(id), attach(&record, 0), close(id)]);
                }));
                frame(cx);
                assert!(cx.debug_bounds("empty-room-placeholder").is_some());
            }

            #[gpui::test]
            fn catalog_keeps_survivor_and_detaches_only_once(cx: &mut TestAppContext) {
                let record = standalone();
                let b = &record.tabs[1].id;
                let c = &record.tabs[2].id;
                let (view, cx, mut inbox) = fixture(cx, std::slice::from_ref(&record), 0, 1);
                cx.update(|_, app| view.update(app, |d, _| {
                    d.room_mut().unwrap().tabs[1].channel = Some(11);
                }));
                press(cx, "w");
                cx.update(|_, app| view.update(app, |d, cx| {
                    inject(d, cx, &mut inbox, catalog(2, vec![record.clone()]));
                    assert_eq!(d.room().unwrap().focused_tab_id(), Some(c.as_str()));
                    assert!(d.room().unwrap().is_tab_closing(b));
                    assert_eq!(d.room().unwrap().tabs[1].channel, Some(11));
                    assert_eq!(d.test_tab_requests, [close(b), attach(&record, 2)]);
                    let mut removed = record.clone();
                    removed.tabs.remove(1);
                    inject(d, cx, &mut inbox, catalog(3, vec![removed.clone()]));
                    inject(d, cx, &mut inbox, catalog(2, vec![record.clone()]));
                    inject(d, cx, &mut inbox, catalog(4, vec![removed]));
                    inject(d, cx, &mut inbox, failed(b));
                    assert_eq!(d.room().unwrap().focused_tab_id(), Some(c.as_str()));
                    assert_eq!(d.room().unwrap().tabs.len(), 2);
                    assert_eq!(d.notice.as_deref(), Some("Close failed"));
                    assert_eq!(d.test_tab_requests, [
                        close(b), attach(&record, 2), TestTabRequest::Detach("local".into(), 11),
                    ]);
                }));
            }

            #[gpui::test]
            fn failed_close_does_not_steal_focus_and_disconnect_restores_pending_tab(cx: &mut TestAppContext) {
                let record = standalone();
                let b = &record.tabs[1].id;
                let c = &record.tabs[2].id;
                let (view, cx, mut inbox) = fixture(cx, std::slice::from_ref(&record), 0, 1);
                press(cx, "w");
                // Numbered navigation addresses the visible order, not the raw group index.
                press(cx, "1");
                press(cx, "2");
                cx.update(|_, app| view.update(app, |d, cx| {
                    assert_eq!(d.room().unwrap().focused_tab_id(), Some(c.as_str()));
                    inject(d, cx, &mut inbox, BridgeEvent::SelectRoomAndTab {
                        room_id: record.id.clone(), tab_id: b.clone(),
                    });
                    assert!(d.pending_select.is_none());
                    inject(d, cx, &mut inbox, failed(b));
                    assert_eq!(d.notice.as_deref(), Some("Close failed"));
                    assert_eq!(d.room().unwrap().tabs[1].status, "Close failed");
                    assert_eq!(projected(d).len(), 3);
                    inject(d, cx, &mut inbox, catalog(2, vec![record.clone()]));
                    assert_eq!(d.room().unwrap().focused_tab_id(), Some(c.as_str()));
                }));
                press(cx, "2");
                press(cx, "w");
                cx.update(|_, app| view.update(app, |d, cx| {
                    assert!(d.room().unwrap().is_tab_closing(b));
                    inject(d, cx, &mut inbox, BridgeEvent::StateChanged(
                        ConnectionState::Disconnected("offline".into()),
                    ));
                    assert!(!d.room().unwrap().is_tab_closing(b));
                    assert_eq!(projected(d).len(), 3);
                    assert_eq!(d.room().unwrap().focused_tab_id(), Some(c.as_str()));
                }));
            }
        }

        mod room_navigation {
            use super::{TestAppContext, VisualTestContext, desktop};
            use crate::tests::{room_rec, tab_rec};
            use crate::{
                ActiveOverlay, AgentLauncher, AgentSettingsModal, BridgeEvent, Catalog,
                ConnectionState, ControlMode, Desktop, HostBridge, HostProfile, KeyboardFocusZone,
                MachineModal, MachineView, PaneDrag, PeekState, ResetNotice, RoomKey, RoomLayout,
                RoomNavDir, RoomView, SplitAxis, TabAction, TabView, UiPrefs, UpdateModal, Vt,
                room_nav_action, tab_action,
            };
            use gpui::{Entity, KeyDownEvent, Keystroke, Modifiers};
            use std::{cell::RefCell, rc::Rc};

            type InputPackets = Rc<RefCell<Vec<(String, u32, Vec<u8>)>>>;

            fn connected(host: &str) -> ConnectionState {
                ConnectionState::Connected {
                    host_id: host.into(),
                    conn_id: 1,
                    features: vec![],
                }
            }

            fn room_id(id: u32) -> String {
                format!("rm_{id:026}")
            }

            fn fixture_room(id: u32, group: usize, channel: u32) -> RoomView {
                let mut record = room_rec(
                    id,
                    (channel..channel + 4).map(|id| tab_rec(id, true)).collect(),
                );
                let mut layout = RoomLayout::default_for_tabs(&record.tabs[..2]);
                for i in 0..2 {
                    assert!(
                        layout
                            .split_leaf(
                                &record.tabs[i].id,
                                SplitAxis::Horizontal,
                                record.tabs[i + 2].id.clone(),
                                5000,
                            )
                            .unwrap()
                    );
                }
                record.layout = Some(layout);
                let mut tabs = Vec::new();
                for (i, rec) in record.tabs.iter().enumerate() {
                    let mut tab = TabView::from_record(rec, (80, 24));
                    tab.mode = ControlMode::Control;
                    tab.channel = Some(channel + i as u32);
                    tab.vt = Some(Vt::new(80, 24).unwrap());
                    tabs.push(tab);
                }
                let mut room = RoomView::from_record(&record, tabs);
                let focused = record.tabs[3 * group].id.clone();
                assert!(room.select_tab_by_id(&focused));
                room.zoomed_tab_id = Some(focused);
                room
            }

            fn fixture(
                cx: &mut TestAppContext,
            ) -> (Entity<Desktop>, &mut VisualTestContext, InputPackets) {
                let (view, cx) = desktop(cx);
                let packets = InputPackets::default();
                cx.update(|window, app| {
                    view.update(app, |d, cx| {
                        window.activate_window();
                        d.window_active = true;
                        d.focus.focus(window);
                        d.ui_prefs_path = None;
                        d.ui_prefs.collapsed_sections.insert("section:rooms".into());
                        for (host, base) in [("local", 10), ("remote", 30)] {
                            let mut profile = HostProfile::local_default();
                            profile.id = host.into();
                            let mut machine = MachineView::new(profile);
                            machine.conn_state = connected(host);
                            machine.catalog_seq = Some(1);
                            machine.rooms =
                                vec![fixture_room(1, 0, base), fixture_room(2, 1, base + 4)];
                            machine.collapsed = true;
                            if host == "local" {
                                d.machines[0] = machine;
                            } else {
                                d.machines.push(machine);
                            }
                        }
                        d.active_room_id = Some(room_id(2));
                        let sink = packets.clone();
                        d.test_input_sink = Some(Box::new(move |host, channel, bytes| {
                            sink.borrow_mut()
                                .push((host.into(), channel, bytes.to_vec()));
                        }));
                        cx.notify();
                    });
                });
                cx.run_until_parked();
                (view, cx, packets)
            }

            fn modifiers(mac: bool) -> Modifiers {
                Modifiers {
                    platform: mac,
                    control: !mac,
                    alt: true,
                    ..Modifiers::default()
                }
            }

            fn chord(key: &str) -> Keystroke {
                Keystroke {
                    key: key.into(),
                    modifiers: modifiers(cfg!(target_os = "macos")),
                    key_char: None,
                }
            }

            fn nav(cx: &mut VisualTestContext, key: &str) -> bool {
                // Modified keys have no simulated IME text, so true means propagation stopped.
                cx.update(|window, app| window.dispatch_keystroke(chord(key), app))
            }

            fn active(d: &Desktop, host: &str, id: u32) {
                assert_eq!(d.active_host_id, host);
                assert_eq!(d.active_room_id.as_deref(), Some(room_id(id).as_str()));
            }

            #[test]
            fn room_nav_matches_only_the_exact_platform_chord() {
                use RoomNavDir::{Next, Previous};
                for mac in [true, false] {
                    let m = modifiers(mac);
                    assert_eq!(room_nav_action("[", m, mac), Some(Previous));
                    assert_eq!(room_nav_action("]", m, mac), Some(Next));
                    for key in ["[", "]"] {
                        for wrong in [
                            modifiers(!mac),
                            Modifiers {
                                platform: true,
                                control: true,
                                ..m
                            },
                            Modifiers {
                                control: true,
                                ..Modifiers::default()
                            },
                            Modifiers {
                                alt: true,
                                ..Modifiers::default()
                            },
                            Modifiers { shift: true, ..m },
                            Modifiers {
                                function: true,
                                ..m
                            },
                            Modifiers { alt: false, ..m },
                        ] {
                            assert_eq!(room_nav_action(key, wrong, mac), None, "{mac}: {wrong:?}");
                        }
                        let shift = Modifiers {
                            shift: true,
                            alt: false,
                            ..m
                        };
                        assert_eq!(room_nav_action(key, shift, mac), None);
                        let tab = if key == "[" {
                            TabAction::Previous
                        } else {
                            TabAction::Next
                        };
                        assert_eq!(tab_action(key, shift, mac), Some(tab));
                        assert_eq!(tab_action(key, m, mac), None);
                    }
                    for key in ["{", "}", "“", "‘", "left", "right", "up", "down"] {
                        assert_eq!(room_nav_action(key, m, mac), None, "{mac}: {key}");
                    }
                    for key in ["left", "right", "up", "down"] {
                        assert!(matches!(
                            tab_action(key, m, mac),
                            Some(TabAction::FocusDir(_))
                        ));
                    }
                }
            }

            #[gpui::test]
            fn room_nav_computes_ordered_catalog_qualified_targets(cx: &mut TestAppContext) {
                use RoomNavDir::{Next, Previous};
                let (view, cx, _) = fixture(cx);
                cx.update(|_, app| {
                    view.update(app, |d, _| {
                        let key = |host: &str, id| RoomKey {
                            host_id: host.into(),
                            room_id: room_id(id),
                        };
                        let select = |d: &mut Desktop, host: &str, id: Option<u32>| {
                            d.active_host_id = host.into();
                            d.active_room_id = id.map(room_id);
                        };
                        let check = |d: &Desktop, next, previous| {
                            assert_eq!(d.next_room_target(Next), next);
                            assert_eq!(d.next_room_target(Previous), previous);
                        };
                        let remote = d.machines.pop().unwrap();
                        d.machines[0].rooms.push(fixture_room(3, 0, 20));
                        let [a, b, c] = [1, 2, 3].map(|id| key("local", id));
                        for (from, next, prev) in [(1, &b, &c), (2, &c, &a), (3, &a, &b)] {
                            select(d, "local", Some(from));
                            check(d, Some(next.clone()), Some(prev.clone()));
                        }
                        d.machines[0].rooms.pop();
                        for (host, state, seq, id) in [
                            ("empty", connected("empty"), Some(1), None),
                            ("stale", connected("stale"), None, Some(10)),
                            (
                                "offline",
                                ConnectionState::Disconnected("offline".into()),
                                Some(1),
                                Some(11),
                            ),
                            (
                                "blocked",
                                ConnectionState::HostKeyMismatch("blocked".into()),
                                Some(1),
                                Some(12),
                            ),
                            (
                                "auth",
                                ConnectionState::AuthRequired("auth".into()),
                                Some(1),
                                Some(13),
                            ),
                            (
                                "missing",
                                ConnectionState::HostNotInstalled("missing".into()),
                                Some(1),
                                Some(14),
                            ),
                        ] {
                            let mut profile = HostProfile::local_default();
                            profile.id = host.into();
                            let mut machine = MachineView::new(profile);
                            machine.conn_state = state;
                            machine.catalog_seq = seq;
                            if let Some(id) = id {
                                machine.rooms.push(fixture_room(id, 0, 50));
                            }
                            d.machines.push(machine);
                        }
                        d.machines.push(remote);
                        d.machines[0].background.push((
                            room_id(100),
                            TabView::from_record(&tab_rec(100, true), (80, 24)),
                        ));
                        let [c, end] = [1, 2].map(|id| key("remote", id));
                        assert_eq!(
                            d.navigable_rooms(),
                            [a.clone(), b.clone(), c.clone(), end.clone()]
                        );
                        for (host, id, dir, target) in [
                            ("local", 2, Next, &c),
                            ("remote", 1, Previous, &b),
                            ("remote", 2, Next, &a),
                            ("local", 1, Previous, &end),
                        ] {
                            select(d, host, Some(id));
                            assert_eq!(d.next_room_target(dir).as_ref(), Some(target));
                        }
                        for (host, id) in [
                            ("offline", Some(11)),
                            ("empty", None),
                            ("blocked", Some(12)),
                            ("stale", Some(10)),
                        ] {
                            select(d, host, id);
                            check(d, Some(c.clone()), Some(b.clone()));
                        }
                        select(d, "remote", Some(999));
                        check(d, Some(a.clone()), Some(b.clone()));
                        select(d, "unknown", None);
                        check(d, Some(a.clone()), Some(end));
                        d.machines.last_mut().unwrap().rooms.clear();
                        d.machines[0].rooms.truncate(1);
                        check(d, Some(a.clone()), Some(a));
                        select(d, "local", Some(1));
                        check(d, None, None);
                        d.machines[0].rooms.clear();
                        check(d, None, None);
                    })
                });
            }

            #[gpui::test]
            fn room_nav_switches_without_forwarding_to_either_terminal(cx: &mut TestAppContext) {
                use gpui::EntityInputHandler;
                let (view, cx, packets) = fixture(cx);
                let enter = Keystroke::parse("enter").unwrap();
                let (old_input, before) = cx.update(|window, app| {
                    assert!(window.dispatch_keystroke(enter.clone(), app));
                    view.update(app, |d, cx| {
                        let before: Vec<_> = d
                            .machines
                            .iter()
                            .flat_map(|m| &m.rooms)
                            .map(|room| {
                                (
                                    room.selected_group,
                                    room.focused_tab_id.clone(),
                                    room.zoomed_tab_id.clone(),
                                )
                            })
                            .collect();
                        (d.prepare_terminal_input(window, cx).unwrap(), before)
                    })
                });
                assert_eq!(
                    std::mem::take(&mut *packets.borrow_mut()),
                    [("local".into(), 17, b"\r".to_vec()),]
                );
                cx.update(|window, app| {
                    let consumed = window.dispatch_keystroke(chord("]"), app);
                    let d = view.read(app);
                    active(d, "remote", 1);
                    assert!(consumed, "room chord must stop propagation");
                    assert!(d.focus.is_focused(window));
                    assert!(!d.section_collapsed("section:rooms"));
                    assert!(d.machines[0].collapsed && !d.machines[1].collapsed);
                    // Check invalidation before painting a fresh input handler for the new pane.
                    assert!(d.terminal_input.is_none());
                });
                assert!(packets.borrow().is_empty());
                cx.update(|window, app| {
                    old_input.update(app, |input, cx| {
                        input.replace_text_in_range(None, "stale", window, cx);
                    });
                    assert!(packets.borrow().is_empty());
                    assert!(window.dispatch_keystroke(enter.clone(), app));
                });
                assert_eq!(
                    std::mem::take(&mut *packets.borrow_mut()),
                    [("remote".into(), 30, b"\r".to_vec()),]
                );

                assert!(nav(cx, "["));
                cx.update(|_, app| active(view.read(app), "local", 2));
                cx.simulate_event(KeyDownEvent {
                    keystroke: chord("]"),
                    is_held: true,
                });
                cx.update(|_, app| active(view.read(app), "remote", 1));
                cx.update(|_, app| {
                    view.update(app, |d, _| {
                        d.machines[1].rooms[1].focused_tab_mut().unwrap().mode =
                            ControlMode::Readonly;
                    })
                });
                assert!(nav(cx, "]"));
                cx.update(|_, app| {
                    let d = view.read(app);
                    active(d, "remote", 2);
                    assert_eq!(
                        d.room().unwrap().focused_tab().unwrap().mode,
                        ControlMode::Readonly
                    );
                    let after: Vec<_> = d
                        .machines
                        .iter()
                        .flat_map(|m| &m.rooms)
                        .map(|room| {
                            (
                                room.selected_group,
                                room.focused_tab_id.clone(),
                                room.zoomed_tab_id.clone(),
                            )
                        })
                        .collect();
                    assert_eq!(after, before);
                });
                // Read-only is also a valid starting point.
                assert!(nav(cx, "]"));
                cx.update(|_, app| active(view.read(app), "local", 1));

                let temp = tempfile::tempdir().unwrap();
                let file = temp.path().join("ui.json");
                let prefs = cx.update(|_, app| {
                    view.update(app, |d, _| {
                        d.ui_prefs_path = Some(file.clone());
                        d.machines[0].rooms.truncate(1);
                        d.machines[1].rooms.clear();
                        d.machines[0].collapsed = true;
                        d.ui_prefs.collapsed_sections.insert("section:rooms".into());
                        d.ui_prefs.clone()
                    })
                });
                assert!(nav(cx, "]"));
                assert!(nav(cx, "["));
                cx.update(|window, app| {
                    let d = view.read(app);
                    active(d, "local", 1);
                    assert_eq!(d.ui_prefs, prefs);
                    assert!(d.machines[0].collapsed && d.focus.is_focused(window));
                    assert!(
                        d.room()
                            .unwrap()
                            .tabs
                            .iter()
                            .all(|t| t.attach_request.is_none())
                    );
                });
                assert!(!file.exists(), "a no-op must not write preferences");
                assert!(packets.borrow().is_empty());
            }

            #[gpui::test]
            fn room_nav_respects_other_keyboard_owners(cx: &mut TestAppContext) {
                let (view, cx, packets) = fixture(cx);
                let temp = tempfile::tempdir().unwrap();
                let source = RoomKey {
                    host_id: "local".into(),
                    room_id: room_id(2),
                };
                for (label, overlay) in [
                    (
                        "machine form",
                        ActiveOverlay::MachineModal(Box::new(MachineModal::new_add())),
                    ),
                    (
                        "rename form",
                        ActiveOverlay::RenameModal(source.clone(), String::new()),
                    ),
                    (
                        "launcher",
                        ActiveOverlay::AgentLauncher(AgentLauncher::new("local".into(), true)),
                    ),
                    (
                        "agent settings",
                        ActiveOverlay::AgentSettingsModal(Box::new(AgentSettingsModal::load(
                            temp.path().join("agents.json"),
                        ))),
                    ),
                ] {
                    cx.update(|_, app| view.update(app, |d, _| d.active_overlay = Some(overlay)));
                    assert!(nav(cx, "]"), "{label}");
                    cx.update(|_, app| {
                        let d = view.read(app);
                        active(d, "local", 2);
                        assert!(d.active_overlay.is_some(), "{label}");
                    });
                    assert!(packets.borrow().is_empty(), "{label}");
                }

                cx.update(|_, app| {
                    view.update(app, |d, _| {
                        d.active_overlay = None;
                        d.update_modal = Some(UpdateModal::default());
                    })
                });
                assert!(nav(cx, "]"));
                cx.update(|_, app| {
                    view.update(app, |d, _| {
                        active(d, "local", 2);
                        assert!(d.update_modal.take().is_some());
                        d.focus_zone = KeyboardFocusZone::AgentList;
                    })
                });
                assert!(!nav(cx, "]"));
                cx.update(|_, app| {
                    view.update(app, |d, _| {
                        active(d, "local", 2);
                        d.focus_zone = KeyboardFocusZone::Terminal;
                        let tab = d.room_mut().unwrap().focused_tab_mut().unwrap();
                        let mut peek = tab.metadata_copy();
                        peek.channel = Some(90);
                        peek.vt = Some(Vt::new(80, 24).unwrap());
                        peek.mode = ControlMode::Readonly;
                        d.peek = Some(PeekState {
                            host: "local".into(),
                            room: "background".into(),
                            tab: peek,
                            claim: None,
                        });
                    })
                });
                nav(cx, "]");
                cx.update(|_, app| {
                    view.update(app, |d, _| {
                        active(d, "local", 2);
                        assert!(d.peek.take().is_some());
                        d.pane_drag = Some(PaneDrag {
                            host_id: source.host_id.clone(),
                            room_id: source.room_id.clone(),
                            group_idx: 1,
                            path: vec![],
                            axis: SplitAxis::Horizontal,
                            parent_origin: 0.,
                            parent_extent: 100.,
                            canvas_origin: 0.,
                            grab_offset: 0.,
                            dirty: false,
                        });
                    })
                });
                let consumed = nav(cx, "]");
                cx.update(|_, app| {
                    view.update(app, |d, _| {
                        active(d, "local", 2);
                        assert!(consumed, "drag no-op must consume the chord");
                        assert!(d.pane_drag.take().is_some());
                        assert!(d.section_collapsed("section:rooms") && d.machines[1].collapsed);
                        d.window_active = false;
                    })
                });
                assert!(packets.borrow().is_empty());
                assert!(!nav(cx, "]"));
                let other_focus = cx.update(|window, app| {
                    view.update(app, |d, cx| {
                        active(d, "local", 2);
                        d.window_active = true;
                        let other = cx.focus_handle();
                        other.focus(window);
                        assert!(!d.focus.is_focused(window));
                        // Call the root listener directly even if GPUI omits it from this focus path.
                        d.key(
                            &KeyDownEvent {
                                keystroke: chord("]"),
                                is_held: false,
                            },
                            window,
                            cx,
                        );
                        active(d, "local", 2);
                        other
                    })
                });
                cx.update(|window, app| {
                    view.update(app, |d, cx| {
                        drop(other_focus);
                        d.focus.focus(window);
                        d.request_terminal_paste(window, cx, |_| Some("first\nsecond".into()));
                        assert!(d.pending_paste.is_some());
                    })
                });
                assert!(packets.borrow().is_empty());

                assert!(nav(cx, "]"));
                cx.update(|_, app| {
                    view.update(app, |d, _| {
                        active(d, "local", 2);
                        assert_eq!(d.pending_paste.as_ref().unwrap().line_count, 2);
                        // Same tab, changed channel: the old confirmation is no longer valid.
                        d.room_mut().unwrap().focused_tab_mut().unwrap().channel = Some(99);
                    })
                });
                assert!(packets.borrow().is_empty());
                assert!(nav(cx, "]"));
                let marked = cx.update(|window, app| {
                    view.update(app, |d, cx| {
                        active(d, "remote", 1);
                        assert!(d.pending_paste.is_none());
                        let marked = d.prepare_terminal_input(window, cx).unwrap();
                        marked.update(cx, |input, _| input.marked = "한".into());
                        marked
                    })
                });
                assert!(packets.borrow().is_empty());
                let consumed = nav(cx, "]");
                cx.update(|_, app| {
                    let d = view.read(app);
                    active(d, "remote", 1);
                    assert_eq!(d.terminal_input.as_ref(), Some(&marked));
                    assert_eq!(marked.read(app).marked, "한");
                });
                if cfg!(target_os = "macos") {
                    assert!(
                        !consumed,
                        "IME delegates to the existing macOS terminal path"
                    );
                    assert!(packets.borrow().is_empty());
                }
            }

            #[gpui::test]
            fn room_nav_saves_only_section_prefs_and_keeps_switch_on_save_error(
                cx: &mut TestAppContext,
            ) {
                let temp = tempfile::tempdir().unwrap();
                let file = temp.path().join("ui.json");
                let (view, cx, packets) = fixture(cx);
                cx.update(|_, app| {
                    view.update(app, |d, _| {
                        assert!(d.ui_prefs_path.is_none());
                        d.ui_prefs
                            .collapsed_sections
                            .insert("section:agents".into());
                    })
                });
                assert!(nav(cx, "]"));
                let expected = cx.update(|_, app| {
                    view.update(app, |d, _| {
                        active(d, "remote", 1);
                        assert!(!d.section_collapsed("section:rooms"));
                        assert!(d.notice.is_none());
                        let expected = d.ui_prefs.clone();
                        d.ui_prefs_path = Some(file.clone());
                        d.ui_prefs.collapsed_sections.insert("section:rooms".into());
                        d.ui_prefs.save(&file).unwrap();
                        expected
                    })
                });
                assert!(nav(cx, "["));
                cx.update(|_, app| {
                    let d = view.read(app);
                    active(d, "local", 2);
                    assert_eq!(d.ui_prefs, expected);
                    assert!(!d.machines[0].collapsed && d.notice.is_none());
                });
                assert_eq!(UiPrefs::load_or_default(&file), expected);
                assert!(UiPrefs::load(&file).is_ok());
                let saved: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
                assert_eq!(saved, serde_json::to_value(&expected).unwrap());
                cx.update(|_, app| {
                    view.update(app, |d, _| {
                        d.ui_prefs_path = Some(file.join("ui.json")); // regular file, not a directory
                        d.ui_prefs.collapsed_sections.insert("section:rooms".into());
                        d.machines[1].collapsed = true;
                    })
                });
                assert!(nav(cx, "]"));
                cx.update(|_, app| {
                    let d = view.read(app);
                    active(d, "remote", 1);
                    assert_eq!(d.ui_prefs, expected);
                    assert!(!d.machines[1].collapsed);
                    assert!(
                        d.notice
                            .as_deref()
                            .unwrap()
                            .starts_with("Failed to save UI settings:")
                    );
                });
                assert_eq!(UiPrefs::load(&file).unwrap(), expected);
                assert!(packets.borrow().is_empty());
            }

            #[gpui::test]
            fn room_nav_ignores_blocked_and_stale_catalogs_until_resubscribed(
                cx: &mut TestAppContext,
            ) {
                let (view, cx, packets) = fixture(cx);
                let mut inbox = HostBridge::new(&[]).take_inbox();
                let (prefs, notice, hovered, pending) = cx.update(|_, app| {
                    view.update(app, |d, _| {
                        d.machines[0].rooms.clear();
                        d.machines[1].rooms.truncate(1);
                        d.active_room_id = None;
                        // An otherwise attachable tab on a cached but blocked machine.
                        let remote = &mut d.machines[1];
                        remote.conn_state =
                            ConnectionState::HostKeyMismatch("fixture mismatch".into());
                        let tab = remote.rooms[0].focused_tab_mut().unwrap();
                        tab.channel = None;
                        tab.attach_request = None;
                        let notice = ResetNotice {
                            notice_id: "fixture-reset".into(),
                            backup_file: "backup.json".into(),
                        };
                        d.machines[0].reset_notice = Some(notice.clone());
                        let hovered = RoomKey {
                            host_id: "remote".into(),
                            room_id: room_id(1),
                        };
                        d.hovered_room = Some(hovered.clone());
                        let pending = (hovered.clone(), tab_rec(30, true).id);
                        d.pending_select = Some(pending.clone());
                        (d.ui_prefs.clone(), notice, hovered, pending)
                    })
                });
                cx.update(|window, app| {
                    view.update(app, |d, cx| {
                        assert!(!d.switch_room(RoomNavDir::Next, window, cx));
                    })
                });
                for state in [
                    ConnectionState::HostKeyMismatch("fixture mismatch".into()),
                    ConnectionState::Reconnecting {
                        attempt: 1,
                        reason: "fixture reconnect".into(),
                    },
                    connected("remote"),
                ] {
                    // Keep the blocked fixture's seq to exercise both halves of the filter.
                    if !matches!(state, ConnectionState::HostKeyMismatch(..)) {
                        cx.update(|_, app| {
                            view.update(app, |d, cx| {
                                d.handle_host_event(
                                    "remote",
                                    None,
                                    BridgeEvent::StateChanged(state),
                                    &mut inbox,
                                    cx,
                                );
                            })
                        });
                    }
                    assert!(nav(cx, "]"));
                    cx.update(|_, app| {
                        let d = view.read(app);
                        assert_eq!(
                            (d.active_host_id.as_str(), &d.active_room_id),
                            ("local", &None)
                        );
                        assert_eq!(d.ui_prefs, prefs);
                        assert!(d.machines.iter().all(|m| m.collapsed));
                        assert!(
                            d.machines[1].rooms[0]
                                .focused_tab()
                                .unwrap()
                                .attach_request
                                .is_none()
                        );
                        assert_eq!(d.machines[0].reset_notice.as_ref(), Some(&notice));
                        assert_eq!(d.hovered_room.as_ref(), Some(&hovered));
                        assert_eq!(d.pending_select.as_ref(), Some(&pending));
                    });
                }
                // Replay a new catalog with the cached room deleted and a different room present.
                cx.update(|_, app| {
                    view.update(app, |d, cx| {
                        // The earlier no-op checks cover pending select; remove it so the normal
                        // catalog reconciliation in this fixture cannot select a newly created room.
                        d.pending_select = None;
                        let mut catalog = Catalog::empty("remote");
                        catalog.rooms.push(room_rec(3, vec![tab_rec(50, true)]));
                        d.handle_host_event(
                            "remote",
                            None,
                            BridgeEvent::CatalogSubscribed {
                                seq: 1,
                                catalog,
                                reset_notice: None,
                                activities: vec![],
                            },
                            &mut inbox,
                            cx,
                        );
                        assert_eq!(d.machines[1].catalog_seq, Some(1));
                        assert_eq!(d.machines[1].rooms.len(), 1);
                        assert_eq!(d.machines[1].rooms[0].id, room_id(3));
                        assert_eq!(d.active_room_id, None);
                        // Preserve a fresh hover/pending selection across successful keyboard routing.
                        let key = RoomKey {
                            host_id: "remote".into(),
                            room_id: room_id(3),
                        };
                        d.hovered_room = Some(key.clone());
                        d.pending_select = Some((key, tab_rec(50, true).id));
                    })
                });
                assert!(nav(cx, "]"));
                cx.update(|_, app| {
                    let d = view.read(app);
                    active(d, "remote", 3);
                    assert!(!d.section_collapsed("section:rooms"));
                    assert!(d.machines[0].collapsed && !d.machines[1].collapsed);
                    assert_eq!(d.machines[0].reset_notice.as_ref(), Some(&notice));
                    let key = RoomKey {
                        host_id: "remote".into(),
                        room_id: room_id(3),
                    };
                    assert_eq!(d.hovered_room.as_ref(), Some(&key));
                    assert_eq!(
                        d.pending_select.as_ref(),
                        Some(&(key, tab_rec(50, true).id))
                    );
                    assert!(
                        d.room()
                            .unwrap()
                            .focused_tab()
                            .unwrap()
                            .attach_request
                            .is_some()
                    );
                });
                assert!(packets.borrow().is_empty());
            }
        }

        fn status(
            state: super::super::AgentState,
            app: Option<&str>,
        ) -> ddg_agents_proto::terminal::AgentStatus {
            ddg_agents_proto::terminal::AgentStatus {
                state,
                app: app.map(str::to_owned),
                kind: None,
                progress: None,
                title: None,
                msg: None,
                records: vec![],
                status_ms: 0,
            }
        }

        #[gpui::test]
        fn spinner_tick_repaints_only_the_sibling_layer(cx: &mut TestAppContext) {
            use super::super::{AgentState, PERF, SPINNER_INTERVAL, TEST_PERF, TabActivity};
            let timer = cx.executor();
            let (view, cx) = desktop(cx);
            TEST_PERF.set(true);
            cx.update(|window, app| {
                view.update(app, |d, cx| {
                    window.activate_window();
                    d.window_active = true;
                    connected_terminals(d);
                    let mut worker = TabView::from_record(&super::tab_rec(43, true), (80, 24));
                    worker.activity = Some(TabActivity {
                        tab_id: worker.id.clone(),
                        run_generation: 1,
                        agent_status: Some(status(AgentState::Working, Some("agy"))),
                        foreground: None,
                        activity_seq: 1,
                        seen_seq: 1,
                    });
                    d.machines[0]
                        .background
                        .push(("rm_bg_worker".into(), worker));
                    d.ui_prefs.collapsed_sections.insert("section:rooms".into());
                    d.agent_ui.invalidate();
                    cx.notify();
                })
            });
            cx.run_until_parked();
            let (clock, slots) = cx.update(|_, app| {
                let d = view.read(app);
                assert!(d.spinner_task.is_some());
                let slots = d.spinner_slots.borrow().clone();
                assert!(!slots.is_empty(), "real background working badge");
                (d.spinner.clone(), slots)
            });
            let startup = PERF.with_borrow_mut(std::mem::take);
            assert!(
                startup.desktop_renders > 0 && startup.panes.values().any(|pane| pane.frames > 0)
            );
            let frame = cx.update(|_, app| clock.read(app).frame);
            timer.advance_clock(SPINNER_INTERVAL);
            cx.run_until_parked();
            let tick = PERF.with_borrow_mut(std::mem::take);
            assert_eq!(tick.notifies.get("spinner"), Some(&1));
            assert!(tick.spinner_renders > 0);
            assert_eq!(tick.desktop_renders, 0);
            assert!(tick.panes.is_empty());
            cx.update(|_, app| {
                let clock = clock.read(app);
                assert_eq!(clock.frame, (frame + 1) % 8);
                assert_eq!(clock.slots.borrow().as_slice(), slots);
            });

            cx.deactivate_window();
            cx.update(|_, app| {
                let d = view.read(app);
                assert!(!d.window_active);
                assert!(d.spinner_task.is_none() && d.spinner_slots.borrow().is_empty());
            });
            PERF.with_borrow_mut(std::mem::take);
            timer.advance_clock(3 * SPINNER_INTERVAL);
            cx.run_until_parked();
            let inactive = PERF.with_borrow_mut(std::mem::take);
            assert!(inactive.notifies.is_empty() && inactive.drains == 0);
            assert_eq!(cx.update(|_, app| clock.read(app).frame), (frame + 1) % 8);
            TEST_PERF.set(false);
        }

        #[gpui::test]
        fn queued_stream_yields_and_retains_order_without_rebuilding_the_sidebar(
            cx: &mut TestAppContext,
        ) {
            use super::super::{BridgeEvent, BridgeMessage, PERF, TEST_PERF, TabFrame};
            let timer = cx.executor();
            let (view, cx) = desktop(cx);
            TEST_PERF.set(true);
            let (tab_id, other_slot, sidebar) = cx.update(|_, app| {
                view.update(app, |d, cx| {
                    connected_terminals(d);
                    cx.notify();
                    let tabs = &d.room().unwrap().tabs;
                    (tabs[1].id.clone(), tabs[2].id.clone(), d.agent_rows())
                })
            });
            cx.run_until_parked();
            PERF.with_borrow_mut(std::mem::take);

            // GPUI's test platform does not emit display callbacks; exercise the
            // production queued wakeup and occluded-window fallback instead.
            timer.advance_clock(std::time::Duration::from_millis(128));
            cx.run_until_parked();
            assert_eq!(PERF.with_borrow(|p| p.drains), 0);

            cx.update(|_, app| {
                let d = view.read(app);
                let enqueue = |generation, frame| {
                    d.bridge.enqueue_test(BridgeMessage::Host(
                        "local".into(),
                        generation,
                        BridgeEvent::TabStream {
                            tab_id: tab_id.clone(),
                            channel: 11,
                            frame,
                        },
                    ))
                };
                enqueue(Some(999), TabFrame::Data(b"STALE".to_vec().into()));
                enqueue(None, TabFrame::Data(b"before".to_vec().into()));
                for _ in 0..5 {
                    enqueue(
                        None,
                        TabFrame::Data(vec![0; ddg_agents_proto::frame::DATA_CHUNK].into()),
                    );
                }
                enqueue(None, TabFrame::Resize(40, 12));
                enqueue(None, TabFrame::Data(b"-close".to_vec().into()));
                enqueue(None, TabFrame::Close(String::new()));
                enqueue(None, TabFrame::Data(b"TOO-LATE".to_vec().into()));
            });
            cx.run_until_parked();
            let wait = std::time::Duration::from_millis(32);
            timer.advance_clock(wait);
            cx.run_until_parked();
            assert_eq!(PERF.with_borrow(|p| p.drains), 1);
            cx.update(|_, app| {
                let d = view.read(app);
                assert_eq!(test_tab(d, &tab_id).channel, Some(11));
            });
            for _ in 0..12 {
                timer.advance_clock(wait);
                cx.run_until_parked();
                if PERF.with_borrow(|p| p.messages) == 11 {
                    break;
                }
            }
            cx.update(|_, app| {
                let d = view.read(app);
                let tab = test_tab(d, &tab_id);
                assert_eq!(tab.channel, None);
                assert_eq!(tab.size, (40, 12));
                let text = tab.vt.as_ref().unwrap().plain_text();
                assert!(text.contains("before-close"), "{text}");
                assert!(!text.contains("STALE") && !text.contains("TOO-LATE"));
                assert!(std::rc::Rc::ptr_eq(&sidebar, &d.agent_rows()));
                let other = test_tab(d, &other_slot);
                let other_pane = other.surface.id;
                assert_eq!(
                    PERF.with_borrow(|p| p.panes.get(&other_pane).map_or(0, |s| s.frames)),
                    0
                );
            });
            let perf = PERF.with_borrow_mut(std::mem::take);
            assert!(perf.budget_yields > 0 && perf.drains >= 2);
            assert_eq!(perf.messages, 11);
            assert_eq!(perf.empty_drains, 0);
            assert_eq!(perf.sidebar_rebuilds, 0);
            TEST_PERF.set(false);
        }

        #[gpui::test]
        fn early_stream_replay_returns_credit_once_on_apply_or_discard(cx: &mut TestAppContext) {
            use super::super::{
                BridgeEvent, BridgeMessage, ControlMode, QueuedMessage, TabFrame, TabStreamHandle,
                host_bridge::TabData,
            };
            use std::sync::{Arc, Mutex};
            let (view, cx) = desktop(cx);
            let retired = Arc::new(Mutex::new(Vec::new()));
            let credit = |label: &'static str, bytes: &[u8]| {
                let retired = retired.clone();
                TabFrame::Data(TabData::with_test_credit(bytes.to_vec(), move || {
                    retired.lock().unwrap().push(label)
                }))
            };
            let mut bridge = HostBridge::new(&[]);
            let mut inbox = bridge.take_inbox();
            cx.update(|_, app| {
                view.update(app, |d, cx| {
                    connected_terminals(d);
                    let tab = d.room_mut().unwrap().focused_tab_mut().unwrap();
                    tab.channel = None;
                    tab.vt = None;
                    assert!(tab.begin_attach());
                    tab.attach_request = Some(1);
                    let tab_id = tab.id.clone();
                    let run_generation = tab.run_generation;
                    let stream = |channel, frame| QueuedMessage {
                        message: BridgeMessage::Host(
                            "local".into(),
                            None,
                            BridgeEvent::TabStream {
                                tab_id: tab_id.clone(),
                                channel,
                                frame,
                            },
                        ),
                        queued_at: None,
                    };
                    d.drain(stream(4, credit("paint", b"first")), &mut inbox, cx);
                    d.drain(
                        stream(8, credit("wrong channel", b"discard")),
                        &mut inbox,
                        cx,
                    );
                    d.drain(stream(4, credit("tail", b" paint")), &mut inbox, cx);
                    assert!(
                        retired.lock().unwrap().is_empty(),
                        "buffering must not return credit"
                    );

                    let handle = TabStreamHandle {
                        tab_id: tab_id.clone(),
                        channel: 4,
                        mode: ControlMode::Readonly,
                        control_epoch: 1,
                        cols: 80,
                        rows: 24,
                        run_generation,
                    };
                    d.apply_bridge_message(
                        BridgeMessage::Host(
                            "local".into(),
                            None,
                            BridgeEvent::TabAttached { request: 1, handle },
                        ),
                        &mut inbox,
                        cx,
                    );
                    assert_eq!(*retired.lock().unwrap(), ["wrong channel"]);
                    assert_eq!(inbox.len(), 2);
                    while let Some(item) = inbox.try_recv() {
                        d.drain(item, &mut inbox, cx);
                    }
                    assert_eq!(*retired.lock().unwrap(), ["wrong channel", "paint", "tail"]);
                    let tab = d.find_tab_mut("local", &tab_id).unwrap();
                    let text = tab.vt.as_ref().unwrap().plain_text();
                    assert!(
                        text.contains("first paint") && !text.contains("discard"),
                        "{text}"
                    );

                    tab.channel = None;
                    assert!(tab.begin_attach());
                    tab.attach_request = Some(2);
                    d.drain(
                        stream(5, credit("failed attach", b"discard")),
                        &mut inbox,
                        cx,
                    );
                    assert_eq!(retired.lock().unwrap().len(), 3);
                    d.apply_bridge_message(
                        BridgeMessage::Host(
                            "local".into(),
                            None,
                            BridgeEvent::TabAttachFailed {
                                request: 2,
                                room_id: "test".into(),
                                tab_id: tab_id.clone(),
                                run_generation,
                                message: "test failure".into(),
                            },
                        ),
                        &mut inbox,
                        cx,
                    );
                    assert_eq!(
                        *retired.lock().unwrap(),
                        ["wrong channel", "paint", "tail", "failed attach"]
                    );
                    assert_eq!(inbox.len(), 0);
                })
            });
        }

        #[gpui::test]
        fn zero_row_wheel_and_clamped_sidebar_drag_do_not_invalidate(cx: &mut TestAppContext) {
            use super::super::{PERF, SIDEBAR_MAX, TEST_PERF};
            use gpui::{MouseButton, MouseMoveEvent, ScrollDelta, ScrollWheelEvent, point};
            let (view, cx) = desktop(cx);
            cx.simulate_resize(size(px(1200.), px(800.)));
            TEST_PERF.set(true);
            cx.update(|_, app| {
                view.update(app, |d, cx| {
                    connected_terminals(d);
                    d.sidebar_base = SIDEBAR_MAX;
                    d.sidebar_drag = Some(0.);
                    cx.notify();
                })
            });
            cx.run_until_parked();
            PERF.with_borrow_mut(std::mem::take);

            let body = cx.debug_bounds(LEFT).unwrap();
            cx.simulate_event(ScrollWheelEvent {
                position: body.center(),
                delta: ScrollDelta::Pixels(point(px(0.), px(1.))),
                ..Default::default()
            });
            cx.simulate_event(MouseMoveEvent {
                position: body.center(),
                pressed_button: Some(MouseButton::Left),
                modifiers: Default::default(),
            });
            let sample = PERF.with_borrow_mut(std::mem::take);
            assert!(!sample.notifies.contains_key("scroll"));
            assert!(!sample.notifies.contains_key("sidebar_drag"));
            assert!(sample.panes.values().all(|pane| pane.frames == 0));
            TEST_PERF.set(false);
        }

        #[gpui::test]
        fn live_and_restored_exit_status_copy(cx: &mut TestAppContext) {
            use super::super::{BridgeEvent, TabRuntimeState};

            let (view, cx) = desktop(cx);
            let mut bridge = HostBridge::new(&[]);
            let mut inbox = bridge.take_inbox();
            cx.update(|_, app| {
                view.update(app, |d, cx| {
                    let room = split_room_rec();
                    for (exit_code, expected) in [
                        (Some(0), "Exited cleanly"),
                        (Some(7), "Exit code 7"),
                        (None, "Exited"),
                    ] {
                        let record = &room.tabs[1];
                        *d.find_tab_mut("local", &record.id).unwrap() =
                            TabView::from_record(record, (80, 24));
                        d.handle_host_event(
                            "local",
                            None,
                            BridgeEvent::SessionExited {
                                session_id: record.id.clone(),
                                room_id: room.id.clone(),
                                run_generation: record.run_generation,
                                exit_code,
                            },
                            &mut inbox,
                            cx,
                        );
                        let tab = test_tab(d, &record.id);
                        assert_eq!(tab.status, expected);
                        assert_eq!(
                            tab.runtime,
                            TabRuntimeState::Exited {
                                status: expected.into(),
                            }
                        );
                        assert_eq!(TabView::runtime_status(&tab.runtime), expected);
                    }
                })
            });
            let signal = TabRuntimeState::Exited {
                status: "Terminated by signal 15".into(),
            };
            assert_eq!(TabView::runtime_status(&signal), "Terminated by signal 15");
        }

        #[gpui::test]
        fn indicator_deadlines_wake_without_host_events(cx: &mut TestAppContext) {
            use super::super::{
                BridgeEvent, BridgeMessage, PERF, TEST_PERF, ZOOM_INDICATOR_DURATION,
            };
            use std::time::Duration;
            let timer = cx.executor();
            let (view, cx) = desktop(cx);
            TEST_PERF.set(true);
            cx.update(|window, app| {
                view.update(app, |d, cx| {
                    connected_terminals(d);
                    d.apply_ui_scale(UiScale::default(), window, cx);
                    let (_, tab_id, run_generation) = d.active_tab_key().unwrap();
                    d.bridge.enqueue_test(BridgeMessage::Host(
                        "local".into(),
                        None,
                        BridgeEvent::ExternalInput {
                            session_id: tab_id.to_owned(),
                            pty_id: 1,
                            run_generation,
                            byte_len: 1,
                            from_session_id: "test".into(),
                            from_host_id: "local".into(),
                            is_remote: false,
                        },
                    ));
                    cx.notify();
                })
            });
            cx.run_until_parked();
            timer.advance_clock(Duration::from_millis(32));
            cx.run_until_parked();
            cx.update(|_, app| {
                let d = view.read(app);
                assert!(d.zoom_indicator.is_some() && d.external_input_indicator.is_some());
            });
            PERF.with_borrow_mut(std::mem::take);

            timer.advance_clock(ZOOM_INDICATOR_DURATION);
            cx.run_until_parked();
            assert!(cx.update(|_, app| view.read(app).zoom_indicator.is_none()));
            assert!(cx.update(|_, app| view.read(app).external_input_indicator.is_some()));
            timer.advance_clock(Duration::from_secs(3));
            cx.run_until_parked();
            assert!(cx.update(|_, app| view.read(app).external_input_indicator.is_none()));
            let sample = PERF.with_borrow_mut(std::mem::take);
            assert_eq!(sample.drains, 0);
            assert_eq!(sample.notifies.get("zoom_expiry"), Some(&1));
            assert_eq!(sample.notifies.get("external_input_expiry"), Some(&1));
            TEST_PERF.set(false);
        }

        #[gpui::test]
        fn top_tab_name_and_icon_follow_profile_and_program_evidence(cx: &mut TestAppContext) {
            use super::super::{AgentState, HostTransport, MachineView, TabActivity};
            use ddg_agents_proto::terminal::ForegroundHint;
            let (view, cx) = desktop(cx);
            cx.update(|_, app| {
                view.update(app, |d, _| {
                    d.agents
                        .agents
                        .push(ddg_agents_transport::agent::AgentProfile {
                            id: "neon".into(),
                            label: " Neon\u{202e} ".into(),
                            argv: vec!["agy".into()],
                            default_cwd: None,
                            env: Default::default(),
                            icon: Some("neon.svg".into()),
                            host_overrides: Default::default(),
                        });
                    d.agents
                        .agents
                        .push(ddg_agents_transport::agent::AgentProfile {
                            id: "wrapper".into(),
                            label: "Wrapper".into(),
                            argv: vec!["node".into(), "/work/vite.js".into()],
                            default_cwd: None,
                            env: Default::default(),
                            icon: Some("neon.svg".into()),
                            host_overrides: Default::default(),
                        });
                    d.agents.agents.push(
                        super::super::find_agent_preset("antigravity")
                            .unwrap()
                            .to_profile(),
                    );
                    for remote in [false, true] {
                        let mut machine = MachineView::new(super::HostProfile::local_default());
                        if remote {
                            machine.profile.transport = HostTransport::Ssh {
                                target: "test".into(),
                                port: None,
                                known_hosts_file: None,
                                identity_file: None,
                            };
                        }
                        let assert_identity = |tab: &TabView, name: &str, key: &str, tint: bool| {
                            let identity = d.resolve_tab_identity(&machine, tab);
                            assert_eq!(identity.name.as_ref(), name);
                            assert_eq!(identity.icon.key, key);
                            assert_eq!(identity.icon.tint, tint, "{name}");
                        };
                        let mut tab = TabView::from_record(&super::tab_rec(43, true), (80, 24));
                        tab.title = "arbitrary OSC title".into();
                        assert_identity(&tab, "Terminal", "", false);
                        tab.title = "π - legacy holder".into();
                        assert_identity(&tab, "pi", "pi", true);
                        tab.title = "still not a tab label".into();
                        tab.argv0 = Some("/bin/agy".into());
                        assert_identity(&tab, "agy", "antigravity", false);
                        tab.argv0 = Some("/token=secret/bin/agy".into());
                        assert_identity(&tab, "Terminal", "", false);
                        tab.argv0 = None;
                        tab.agent_profile = Some("agy".into());
                        assert_identity(&tab, "agy", "antigravity", false);
                        tab.agent_profile = Some("antigravity".into());
                        assert_identity(&tab, "agy", "antigravity", false);
                        tab.agent_profile = Some("neon".into());
                        let profile_icon = if remote { "antigravity" } else { "neon.svg" };
                        assert_identity(&tab, "Neon", profile_icon, false);
                        tab.agent_profile = None;
                        tab.activity = Some(TabActivity {
                            tab_id: tab.id.clone(),
                            run_generation: 1,
                            agent_status: Some(status(AgentState::Idle, Some("antigravity"))),
                            foreground: None,
                            activity_seq: 0,
                            seen_seq: 0,
                        });
                        tab.argv0 = Some("codex".into());
                        assert_identity(&tab, "agy", "antigravity", false);

                        tab.argv0 = Some("/bin/agy".into());
                        tab.agent_profile = Some("neon".into());
                        for (comm, script, name, key, tint) in [
                            ("agy", None, "agy", profile_icon, false),
                            ("node", Some("pi"), "pi", "pi", true),
                            ("node", Some("vite.JS"), "vite", "vite", true),
                            ("python3", Some("app.py"), "app.py", "program", true),
                            ("codex.exe", None, "codex", "codex", false),
                            ("opencode", None, "opencode", "opencode", true),
                            ("-zsh", None, "zsh", "zsh", true),
                            ("sh", None, "sh", "shell", true),
                            ("git", None, "git", "git", true),
                            ("htop", None, "htop", "htop", true),
                            ("my-unlisted-tool", None, "my-unlisted-tool", "", false),
                            ("node", None, "agy", "antigravity", false),
                            ("node", Some("/token=secret/bin/pi"), "node", "node", true),
                        ] {
                            tab.activity.as_mut().unwrap().foreground = Some(ForegroundHint {
                                pid: 1,
                                comm: comm.into(),
                                script_hint: script.map(str::to_owned),
                            });
                            assert_identity(&tab, name, key, tint);
                        }
                        for name in ["zsh", "htop"] {
                            tab.argv0 = Some(format!("/bin/{name}"));
                            tab.activity.as_mut().unwrap().foreground = Some(ForegroundHint {
                                pid: 1,
                                comm: name.into(),
                                script_hint: None,
                            });
                            assert_identity(&tab, name, name, true);
                        }
                        let activity = tab.activity.as_mut().unwrap();
                        activity.agent_status = None;
                        activity.foreground = Some(ForegroundHint {
                            pid: 1,
                            comm: "python3".into(),
                            script_hint: None,
                        });
                        assert_identity(&tab, "python3", "python", true);

                        tab.agent_profile = Some("wrapper".into());
                        for script in ["vite.js", "pi"] {
                            tab.activity.as_mut().unwrap().foreground = Some(ForegroundHint {
                                pid: 1,
                                comm: "node".into(),
                                script_hint: Some(script.into()),
                            });
                            match (script, remote) {
                                ("vite.js", false) => {
                                    assert_identity(&tab, "vite", "neon.svg", false)
                                }
                                ("vite.js", true) => assert_identity(&tab, "vite", "vite", true),
                                _ => assert_identity(&tab, "pi", "pi", true),
                            }
                        }
                    }
                })
            });
        }

        #[gpui::test]
        fn local_outdated_daemon_shows_banner_without_remote_install(cx: &mut TestAppContext) {
            use super::super::ConnectionState;
            let (view, cx) = desktop(cx);
            cx.update(|_, app| {
                view.update(app, |d, cx| {
                    connected_terminals(d);
                    assert!(d.machines[0].lacks_activity());
                    assert!(d.old_host_banner(cx).is_some());
                    d.start_machine_install("local", cx);
                    assert!(!d.machines[0].update_confirm);
                    d.machines[0].update_confirm = true;
                    d.start_machine_install("local", cx);
                    assert!(d.machines[0].install_req_id.is_none());
                    let ConnectionState::Connected { features, .. } = &mut d.machines[0].conn_state
                    else {
                        panic!("expected a connected local machine");
                    };
                    features.push("agent_activity_v1".into());
                    assert!(!d.machines[0].lacks_activity());
                    assert!(d.old_host_banner(cx).is_none());
                })
            });
        }

        #[gpui::test]
        fn composing_hangul_uses_the_canvas_anchor_with_hidden_cursor(cx: &mut TestAppContext) {
            use super::super::ControlMode;
            use gpui::{EntityInputHandler, point};
            let (view, cx) = desktop(cx);
            let (tab_id, cell, sent) = cx.update(|window, app| {
                view.update(app, |d, cx| {
                    d.window_active = true;
                    d.focus.focus(window);
                    connected_terminals(d);
                    let sent = observe_input(d);
                    let (_, width, height) = d.font_grid;
                    let tab = d.room_mut().unwrap().focused_tab_mut().unwrap();
                    tab.mode = ControlMode::Control;
                    tab.vt.as_mut().unwrap().write(b"\x1b[?25l\x1b[3;5H");
                    cx.notify();
                    (tab.id.clone(), size(px(width), px(height)), sent)
                })
            });
            cx.run_until_parked();
            let body = cx.debug_bounds(LEFT_BODY).unwrap();
            let mut last = None;
            for marked in ["ㅎ", "하", "한"] {
                cx.update(|window, app| {
                    let input = view.read(app).terminal_input.clone().unwrap();
                    input.update(app, |i, cx| {
                        i.replace_and_mark_text_in_range(None, marked, None, window, cx);
                        assert_eq!(i.marked, marked);
                        assert_eq!(i.marked_text_range(window, cx), Some(0..1));
                    });
                    last = Some(input);
                });
                cx.run_until_parked();
                let anchor = cx.update(|_, app| {
                    test_tab(view.read(app), &tab_id)
                        .surface
                        .ime_bounds
                        .get()
                        .unwrap()
                });
                assert_eq!(
                    anchor.origin,
                    body.origin + point(cell.width * 4., cell.height * 2.)
                );
                assert!((f32::from(anchor.size.width - cell.width)).abs() < 0.01);
                assert_eq!(anchor.size.height, cell.height);
                assert!(sent.borrow().is_empty(), "preedit must never reach the PTY");
            }
            cx.update(|window, app| {
                last.as_ref().unwrap().update(app, |i, cx| {
                    i.replace_text_in_range(None, "한", window, cx);
                });
            });
            assert_eq!(*sent.borrow(), ["한".as_bytes()]);
            cx.update(|_, app| {
                view.update(app, |d, cx| {
                    d.find_tab_mut("local", &tab_id).unwrap().mode = ControlMode::Readonly;
                    cx.notify();
                });
            });
            cx.run_until_parked();
            cx.update(|window, app| {
                let d = view.read(app);
                assert!(d.terminal_input.is_none());
                assert!(test_tab(d, &tab_id).surface.ime_bounds.get().is_none());
                last.unwrap().update(app, |i, cx| {
                    i.replace_text_in_range(None, "stale", window, cx)
                });
            });
            assert_eq!(*sent.borrow(), ["한".as_bytes()]);
        }

        #[gpui::test]
        fn canvas_wheel_drag_and_outside_mouse_up(cx: &mut TestAppContext) {
            use super::super::ControlMode;
            use gpui::{
                MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ScrollDelta,
                ScrollWheelEvent, point,
            };
            let (view, cx) = desktop(cx);
            let (tab_id, row_height) = cx.update(|window, app| {
                view.update(app, |d, cx| {
                    connected_terminals(d);
                    d.window_active = true;
                    d.focus.focus(window);
                    let row_height = d.font_grid.2;
                    let tab = d.room_mut().unwrap().focused_tab_mut().unwrap();
                    tab.vt
                        .as_mut()
                        .unwrap()
                        .write("selection text\r\n".repeat(100).as_bytes());
                    cx.notify();
                    (tab.id.clone(), row_height)
                })
            });
            cx.run_until_parked();
            let body = cx.debug_bounds(LEFT_BODY).unwrap();
            let offset = |cx: &mut VisualTestContext| {
                cx.update(|_, app| {
                    test_tab(view.read(app), &tab_id)
                        .vt
                        .as_ref()
                        .unwrap()
                        .scrollbar()
                        .offset
                })
            };
            let start = offset(cx);
            assert!(start > 3);
            for i in 1..=4 {
                cx.simulate_event(ScrollWheelEvent {
                    position: body.center(),
                    delta: ScrollDelta::Pixels(point(px(0.), px(row_height / 4.))),
                    ..Default::default()
                });
                assert_eq!(offset(cx), start - u64::from(i == 4));
            }
            let press = body.origin + point(px(2.), px(row_height * 4.));
            let outside = point(press.x + px(48.), body.origin.y - px(300.));
            let down = MouseDownEvent {
                position: press,
                button: MouseButton::Left,
                click_count: 1,
                ..Default::default()
            };
            let move_out = MouseMoveEvent {
                position: outside,
                pressed_button: Some(MouseButton::Left),
                modifiers: Default::default(),
            };
            cx.simulate_event(down.clone());
            cx.simulate_event(move_out.clone());
            cx.update(|_, app| {
                let d = view.read(app);
                assert!(d.terminal_mouse.is_some());
                assert!(d.terminal_autoscroll.is_some());
                assert!(test_tab(d, &tab_id).vt.as_ref().unwrap().has_selection());
            });
            cx.simulate_event(MouseUpEvent {
                position: outside,
                button: MouseButton::Left,
                ..Default::default()
            });
            cx.update(|_, app| {
                let d = view.read(app);
                assert!(d.terminal_mouse.is_none() && d.terminal_autoscroll.is_none());
            });
            cx.simulate_event(down);
            cx.simulate_event(move_out);
            cx.update(|_, app| {
                view.update(app, |d, cx| {
                    assert!(d.terminal_mouse.is_some());
                    d.find_tab_mut("local", &tab_id).unwrap().mode = ControlMode::Control;
                    cx.notify();
                })
            });
            cx.run_until_parked();
            cx.update(|_, app| {
                let d = view.read(app);
                assert!(d.terminal_mouse.is_none() && d.terminal_autoscroll.is_none());
            });
        }

        #[gpui::test]
        fn canvas_routes_tui_mouse_and_alternate_wheel_to_the_pty(cx: &mut TestAppContext) {
            use super::super::ControlMode;
            use gpui::{
                Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, ScrollDelta,
                ScrollWheelEvent, point,
            };
            let (view, cx) = desktop(cx);
            let (sent, cell_height) = cx.update(|window, app| {
                view.update(app, |d, cx| {
                    connected_terminals(d);
                    d.window_active = true;
                    d.focus.focus(window);
                    let sent = observe_input(d);
                    let tab = d.room_mut().unwrap().focused_tab_mut().unwrap();
                    tab.mode = ControlMode::Control;
                    tab.vt.as_mut().unwrap().write(b"\x1b[?1000h\x1b[?1006h");
                    cx.notify();
                    (sent, d.font_grid.2)
                })
            });
            cx.run_until_parked();
            let body = cx.debug_bounds(LEFT_BODY).unwrap();
            let position = body.origin + point(px(3.), px(cell_height * 2.));
            let wheel = ScrollWheelEvent {
                position,
                delta: ScrollDelta::Pixels(point(px(0.), px(cell_height))),
                ..Default::default()
            };
            cx.simulate_event(MouseDownEvent {
                position,
                button: MouseButton::Left,
                click_count: 1,
                ..Default::default()
            });
            cx.simulate_event(MouseUpEvent {
                position,
                button: MouseButton::Left,
                ..Default::default()
            });
            cx.simulate_event(wheel.clone());
            assert_eq!(
                *sent.borrow(),
                [
                    b"\x1b[<0;1;3M".as_slice(),
                    b"\x1b[<0;1;3m",
                    b"\x1b[<64;1;3M"
                ]
            );
            sent.borrow_mut().clear();
            cx.simulate_event(ScrollWheelEvent {
                modifiers: Modifiers {
                    shift: true,
                    ..Default::default()
                },
                ..wheel.clone()
            });
            assert!(
                sent.borrow().is_empty(),
                "Shift routes to local scroll, not TUI"
            );
            let mode = |cx: &mut VisualTestContext, bytes: &[u8]| {
                cx.update(|_, app| {
                    view.update(app, |d, cx| {
                        d.room_mut()
                            .unwrap()
                            .focused_tab_mut()
                            .unwrap()
                            .vt
                            .as_mut()
                            .unwrap()
                            .write(bytes);
                        cx.notify();
                    })
                })
            };
            mode(cx, b"\x1b[?1000l\x1b[?1049h\x1b[?1007h");
            cx.simulate_event(wheel.clone());
            mode(cx, b"\x1b[?1007l");
            cx.simulate_event(wheel);
            cx.simulate_keystrokes("ctrl-c");
            assert_eq!(*sent.borrow(), [b"\x1b[A".as_slice(), b"\x03"]);
        }

        #[gpui::test]
        fn file_drop_forwards_escaped_paths_to_an_inactive_window(cx: &mut TestAppContext) {
            use super::super::{ControlMode, PathBuf};

            let (view, cx) = desktop(cx);
            cx.deactivate_window();
            cx.update(|window, app| {
                view.update(app, |d, cx| {
                    connected_terminals(d);
                    let tabs = &mut d.room_mut().unwrap().tabs;
                    tabs[1].mode = ControlMode::Control;
                    let left = tabs[1].id.clone();
                    let right = tabs[2].id.clone();
                    assert!(d.room_mut().unwrap().select_tab_by_id(&right));
                    let sent = observe_input(d);

                    d.handle_file_drop(
                        &left,
                        &[
                            PathBuf::from("/tmp/Screen Shot (1).png"),
                            PathBuf::from("/tmp/it's $here.png"),
                        ],
                        window,
                        cx,
                    );

                    assert_eq!(d.room().unwrap().focused_tab_id(), Some(left.as_str()));
                    assert_eq!(
                        *sent.borrow(),
                        [br"/tmp/Screen\ Shot\ \(1\).png /tmp/it\'s\ \$here.png".as_slice()]
                    );
                    assert!(!d.window_active);
                    assert!(d.focus.is_focused(window));
                    assert!(d.control_target().is_none());
                    assert!(d.input_target(window).is_none());
                })
            });
        }

        #[gpui::test]
        fn file_drop_does_not_change_focus_or_selection_behind_modals_or_peek(
            cx: &mut TestAppContext,
        ) {
            use super::super::{
                ActiveOverlay, ControlMode, KeyboardFocusZone, PathBuf, PeekState, RoomKey,
            };

            let (view, cx) = desktop(cx);
            cx.update(|window, app| {
                view.update(app, |d, cx| {
                    connected_terminals(d);
                    let tabs = &mut d.room_mut().unwrap().tabs;
                    tabs[1].mode = ControlMode::Control;
                    let left = tabs[1].id.clone();
                    let right = tabs[2].id.clone();
                    tabs[2].mode = ControlMode::Control;
                    let mut peek_tab = tabs[1].metadata_copy();
                    peek_tab.mode = ControlMode::Control;
                    peek_tab.channel = tabs[1].channel;
                    assert!(d.room_mut().unwrap().select_tab_by_id(&right));
                    let group = d.room().unwrap().selected_group;
                    let other_focus = cx.focus_handle();
                    other_focus.focus(window);
                    d.focus_zone = KeyboardFocusZone::AgentList;
                    let sent = observe_input(d);

                    let mut peek = Some(PeekState {
                        host: "local".into(),
                        room: d.room().unwrap().id.clone(),
                        tab: peek_tab,
                        claim: None,
                    });
                    for blocker in ["overlay", "update modal", "peek"] {
                        match blocker {
                            "overlay" => {
                                d.active_overlay = Some(ActiveOverlay::RenameModal(
                                    RoomKey {
                                        host_id: "local".into(),
                                        room_id: d.room().unwrap().id.clone(),
                                    },
                                    String::new(),
                                ));
                            }
                            "update modal" => d.update_modal = Some(Default::default()),
                            _ => d.peek = peek.take(),
                        }

                        let active = if blocker == "peek" { &left } else { &right };
                        assert!(d.drop_target(active).is_none(), "{blocker}");
                        d.handle_file_drop(&left, &[PathBuf::from("/tmp/shot.png")], window, cx);

                        assert!(sent.borrow().is_empty(), "{blocker} forwarded input");
                        assert_eq!(
                            d.room().unwrap().focused_tab_id(),
                            Some(right.as_str()),
                            "{blocker}"
                        );
                        assert_eq!(d.room().unwrap().selected_group, group, "{blocker}");
                        assert!(
                            matches!(d.focus_zone, KeyboardFocusZone::AgentList),
                            "{blocker}"
                        );
                        assert!(other_focus.is_focused(window), "{blocker}");
                        d.active_overlay = None;
                        d.update_modal = None;
                        d.peek = None;
                    }
                })
            });
        }

        #[gpui::test]
        fn file_drop_rejects_ineligible_or_mismatched_targets(cx: &mut TestAppContext) {
            use super::super::{ConnectionState, ControlMode, PathBuf, TabRuntimeState};

            let (view, cx) = desktop(cx);
            cx.update(|window, app| {
                view.update(app, |d, cx| {
                    connected_terminals(d);
                    let sent = observe_input(d);
                    let tab = d.room().unwrap().focused_tab().unwrap();
                    let id = tab.id.clone();
                    let running = tab.runtime.clone();
                    let connected = d.machines[0].conn_state.clone();
                    let paths = [PathBuf::from("/tmp/shot.png")];
                    for blocker in [
                        "disconnected",
                        "stopped",
                        "closing",
                        "readonly",
                        "no channel",
                        "mismatch",
                    ] {
                        d.machines[0].conn_state = if blocker == "disconnected" {
                            ConnectionState::Disconnected("test".into())
                        } else {
                            connected.clone()
                        };
                        let tab = d.find_tab_mut("local", &id).unwrap();
                        tab.runtime = if blocker == "stopped" {
                            TabRuntimeState::StoppedAfterReboot
                        } else {
                            running.clone()
                        };
                        tab.closing = blocker == "closing";
                        tab.mode = if blocker == "readonly" {
                            ControlMode::Readonly
                        } else {
                            ControlMode::Control
                        };
                        tab.channel = (blocker != "no channel").then_some(11);
                        let target = if blocker == "mismatch" { "missing-tab" } else { &id };

                        assert!(d.drop_target(target).is_none(), "{blocker}");
                        d.handle_file_drop(target, &paths, window, cx);
                        assert!(sent.borrow().is_empty(), "{blocker} forwarded input");
                        d.find_tab_mut("local", &id).unwrap().closing = false;
                    }
                })
            });
        }

        #[gpui::test]
        fn paste_checks_permission_before_read_and_keyboard_confirms_only_the_banner(
            cx: &mut TestAppContext,
        ) {
            use super::super::{ControlMode, PeekState};
            use gpui::ClipboardItem;
            let (view, cx) = desktop(cx);
            let tab_id = cx.update(|window, app| {
                view.update(app, |d, cx| {
                    connected_terminals(d);
                    d.window_active = true;
                    d.focus.focus(window);
                    d.request_terminal_paste(window, cx, |_| {
                        panic!("read-only paste read clipboard")
                    });
                    let tab = d.room_mut().unwrap().focused_tab_mut().unwrap();
                    tab.mode = ControlMode::Control;
                    let mut peek = tab.metadata_copy();
                    peek.mode = ControlMode::Control;
                    peek.channel = tab.channel;
                    peek.vt = Some(super::super::Vt::new(80, 24).unwrap());
                    let id = tab.id.clone();
                    d.peek = Some(PeekState {
                        host: "local".into(),
                        room: "test".into(),
                        tab: peek,
                        claim: None,
                    });
                    d.request_terminal_paste(window, cx, |_| panic!("Peek paste read clipboard"));
                    d.peek = None;
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        "secret payload\nnext line".into(),
                    ));
                    cx.notify();
                    id
                })
            });
            cx.run_until_parked();
            let body = cx.debug_bounds(LEFT_BODY).unwrap();
            let paste = if cfg!(target_os = "macos") {
                "cmd-v"
            } else {
                "ctrl-shift-v"
            };
            cx.simulate_keystrokes(paste);
            assert!(cx.debug_bounds("paste-confirmation").is_some());
            assert_eq!(
                cx.debug_bounds(LEFT_BODY),
                Some(body),
                "confirmation overlays the body without moving it"
            );
            cx.update(|_, app| {
                let d = view.read(app);
                let pending = d.pending_paste.as_ref().unwrap();
                assert_eq!((pending.line_count, pending.byte_len), (2, 24));
                assert!(d.terminal_input.is_none() && d.notice.is_none());
            });
            cx.simulate_keystrokes("escape");
            assert!(cx.debug_bounds("paste-confirmation").is_none());
            assert!(cx.update(|_, app| view.read(app).notice.is_none()));

            cx.simulate_keystrokes(paste);
            cx.simulate_keystrokes("enter");
            assert!(cx.debug_bounds("paste-confirmation").is_none());
            // The fake bridge has no client: only an attempted paste produces this notice.
            assert_eq!(
                cx.update(|_, app| view.read(app).notice.clone()).as_deref(),
                Some("Failed to paste into terminal.")
            );

            cx.simulate_keystrokes(paste);
            assert!(cx.debug_bounds("paste-confirmation").is_some());
            cx.update(|_, app| {
                view.update(app, |d, cx| {
                    d.find_tab_mut("local", &tab_id)
                        .unwrap()
                        .vt
                        .as_mut()
                        .unwrap()
                        .write(b"\x1b[?2004h");
                    cx.notify();
                })
            });
            cx.run_until_parked();
            assert!(cx.debug_bounds("paste-confirmation").is_none());
        }

        #[gpui::test]
        fn clipboard_copy_works_in_readonly_and_exited_peek_only_with_selection(
            cx: &mut TestAppContext,
        ) {
            use super::super::{PeekState, TabRuntimeState, Vt};
            use ddg_agents_vt::terminal::SelectionKind;
            use gpui::ClipboardItem;
            let (view, cx) = desktop(cx);
            let copy = if cfg!(target_os = "macos") {
                "cmd-c"
            } else {
                "ctrl-shift-c"
            };
            let clipboard = |cx: &mut VisualTestContext| {
                cx.update(|_, app| app.read_from_clipboard().and_then(|item| item.text()))
            };
            cx.update(|window, app| {
                view.update(app, |d, cx| {
                    connected_terminals(d);
                    d.window_active = true;
                    d.focus.focus(window);
                    cx.write_to_clipboard(ClipboardItem::new_string("unchanged".into()));
                    cx.notify();
                })
            });
            cx.simulate_keystrokes(copy);
            assert_eq!(clipboard(cx).as_deref(), Some("unchanged"));
            cx.update(|_, app| {
                view.update(app, |d, cx| {
                    let vt = d
                        .room_mut()
                        .unwrap()
                        .focused_tab_mut()
                        .unwrap()
                        .vt
                        .as_mut()
                        .unwrap();
                    vt.write(b"readonly");
                    assert!(vt.select_start(1, 0, SelectionKind::Word));
                    cx.notify();
                })
            });
            cx.simulate_keystrokes(copy);
            assert_eq!(clipboard(cx).as_deref(), Some("readonly"));

            cx.update(|_, app| {
                view.update(app, |d, cx| {
                    let mut tab = d.room().unwrap().focused_tab().unwrap().metadata_copy();
                    tab.runtime = TabRuntimeState::Exited {
                        status: "test".into(),
                    };
                    let mut vt = Vt::new(80, 24).unwrap();
                    vt.write(b"exited");
                    assert!(vt.select_start(1, 0, SelectionKind::Word));
                    tab.vt = Some(vt);
                    d.peek = Some(PeekState {
                        host: "local".into(),
                        room: "test".into(),
                        tab,
                        claim: None,
                    });
                    cx.notify();
                })
            });
            cx.simulate_keystrokes(copy);
            assert_eq!(clipboard(cx).as_deref(), Some("exited"));
        }

        #[gpui::test]
        fn modal_and_tooltip_widths_scale_once(cx: &mut TestAppContext) {
            use super::super::{ActiveOverlay, AgentSettingsModal, AgentTooltip};
            use gpui::{Context, IntoElement, ParentElement, Render, Styled, Window, div};

            struct TooltipHost(Entity<AgentTooltip>);
            impl Render for TooltipHost {
                fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                    div().flex().items_start().child(self.0.clone())
                }
            }

            let temp = tempfile::tempdir().unwrap();
            let good = temp.path().join("agents.json");
            let bad = temp.path().join("broken.json");
            std::fs::write(&bad, b"{").unwrap();

            for (percent, launcher, settings, selector, update, tooltip) in [
                (100, 472., 598., 122., 486., 337.),
                (150, 708., 898., 183., 729., 505.),
            ] {
                let scale = UiScale::from_percent(percent);
                let (view, visual) = desktop(cx);
                visual.simulate_resize(size(px(1400.), px(900.)));
                visual.update(|window, app| {
                    view.update(app, |d, cx| {
                        d.apply_ui_scale(scale, window, cx);
                        d.open_launcher();
                        cx.notify();
                    })
                });
                visual.run_until_parked();
                assert_eq!(
                    visual.debug_bounds("agent-overlay-panel").unwrap().size.width,
                    px(launcher)
                );

                for (path, blocked) in [(&good, false), (&bad, true)] {
                    visual.update(|_, app| {
                        view.update(app, |d, cx| {
                            let modal = AgentSettingsModal::load(path.clone());
                            assert_eq!(modal.blocked, blocked);
                            d.active_overlay =
                                Some(ActiveOverlay::AgentSettingsModal(Box::new(modal)));
                            cx.notify();
                        })
                    });
                    visual.run_until_parked();
                    assert_eq!(
                        visual.debug_bounds("agent-overlay-panel").unwrap().size.width,
                        px(settings)
                    );
                    let actual = visual
                        .debug_bounds("agent-settings-selector")
                        .map(|b| b.size.width);
                    assert_eq!(actual, (!blocked).then_some(px(selector)));
                }

                visual.update(|window, app| {
                    view.update(app, |d, cx| d.open_update_modal(false, window, cx))
                });
                visual.run_until_parked();
                assert_eq!(
                    visual.debug_bounds("update-modal-panel").unwrap().size.width,
                    px(update)
                );

                let (_, tooltip_window) = cx.add_window_view(|window, cx| {
                    window.set_rem_size(px(scale.rem_px()));
                    TooltipHost(cx.new(|_| {
                        AgentTooltip("A long agent status that wraps across several lines. ".repeat(12), scale.px(306.))
                    }))
                });
                tooltip_window.simulate_resize(size(px(1400.), px(900.)));
                let bounds = tooltip_window.debug_bounds("agent-tooltip").unwrap();
                assert_eq!(bounds.size.width, px(tooltip));
                assert!(bounds.size.height > px(2. * scale.rem_px()));
            }
        }

        #[gpui::test]
        fn resetting_zoom_preserves_v2_preferences_and_skips_unchanged_write(cx: &mut TestAppContext) {
            use super::super::{UiPrefs, ZoomAction};
            use std::{fs, time::SystemTime};
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("ui.json");
            fs::write(
                &path,
                br#"{"schema_version":2,"ui_scale_pct":90,"agent_sort":"manual","pinned_agents":["local:t1"],"manual_agent_order":["local:t1","local:t2"],"collapsed_sections":["section:rooms"]}"#,
            )
            .unwrap();
            let mut expected = UiPrefs::load(&path).unwrap();
            assert_eq!(expected.ui_scale_pct, 90);
            let (view, cx) = desktop(cx);
            cx.update(|window, app| {
                view.update(app, |d, cx| {
                    d.ui_prefs_path = Some(path.clone());
                    d.ui_prefs = expected.clone();
                    d.apply_ui_scale(UiScale::from_percent(d.ui_prefs.ui_scale_pct), window, cx);
                    assert_eq!(d.ui_scale.percent(), 90);
                    d.apply_zoom_action(ZoomAction::Reset, window, cx);
                    assert_eq!(d.zoom_badge_pct(), Some(100));
                })
            });
            expected.ui_scale_pct = 100;
            assert_eq!(UiPrefs::load(&path).unwrap(), expected);

            let sentinel = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(sentinel)
                .unwrap();
            let bytes = fs::read(&path).unwrap();
            cx.update(|window, app| {
                view.update(app, |d, cx| {
                    d.zoom_indicator = None;
                    d.apply_zoom_action(ZoomAction::Reset, window, cx);
                    assert_eq!(d.zoom_badge_pct(), Some(100));
                })
            });
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), sentinel);
            assert_eq!(fs::read(&path).unwrap(), bytes);
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
            use super::super::{PeekState, UpdatePhase};
            let (view, cx) = desktop(cx);
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
            let tab_bar = cx.debug_bounds("tab-bar").unwrap();
            assert_eq!(tab_bar.top(), px(30.));
            assert_eq!(tab_bar.bottom(), px(61.));
            let terminal = cx.debug_bounds("terminal").unwrap();
            assert_eq!(terminal.top(), px(91.));
            assert_eq!(cx.update(|_, app| view.read(app).canvas.y), 91.);
            let pane = cx.debug_bounds(LEFT).unwrap();
            let body = cx.debug_bounds(LEFT_BODY).unwrap();
            assert_eq!(pane.top(), terminal.top());
            assert_eq!(body.top(), pane.top() + px(22.));
            assert_eq!(body.left(), pane.left() + px(2.));
            assert_eq!(body.right(), pane.right() - px(2.));
            assert_eq!(body.bottom(), pane.bottom() - px(1.));

            cx.update(|_, app| {
                view.update(app, |d, cx| {
                    d.peek = Some(PeekState {
                        host: "local".into(),
                        room: "peek-test".into(),
                        tab: d.room().unwrap().focused_tab().unwrap().metadata_copy(),
                        claim: None,
                    });
                    cx.notify();
                })
            });
            cx.run_until_parked();
            // Only the update and Peek banners contribute when normal tabs are hidden.
            assert_eq!(cx.update(|_, app| view.read(app).canvas.y), 84.);
            assert_eq!(cx.debug_bounds("terminal").unwrap().top(), px(84.));
            cx.update(|_, app| {
                view.update(app, |d, cx| {
                    d.update.phase = UpdatePhase::Idle;
                    cx.notify();
                })
            });
            cx.run_until_parked();
            let terminal = cx.debug_bounds("terminal").unwrap();
            assert_eq!(terminal.top(), px(54.));
            assert_eq!(cx.update(|_, app| view.read(app).canvas.y), 54.);
            assert_eq!(cx.debug_bounds(LEFT_BODY).unwrap().top(), terminal.top());
        }

        #[test]
        fn semantic_ghost_buttons_keep_foreground_and_stable_underline_at_rest() {
            use gpui::{FontWeight, InteractiveElement, Styled, div, rgb, rgba};

            for (enabled, selected, color, underline, weight) in [
                (true, true, 0x8db7ff, 0x8db7ffff, Some(FontWeight::SEMIBOLD)),
                (true, false, 0x9a9a9a, 0, None),
                (false, true, 0x858a92, 0, None),
            ] {
                let mut button =
                    Desktop::segmented_choice(div().id("choice"), selected, enabled, 0x394354);
                let style = button.style();
                assert!(style.background.is_none());
                assert_eq!(style.border_widths.bottom, Some(px(1.).into()));
                assert_eq!(style.border_color, Some(rgba(underline).into()));
                let text = button.text_style().as_ref().unwrap();
                assert_eq!(text.color, Some(rgb(color).into()));
                assert_eq!(text.font_weight, weight);
            }

            for (mut button, color, weight) in [
                (Desktop::primary_action(div().id("save"), true), 0x8db7ff, Some(FontWeight::SEMIBOLD)),
                (Desktop::primary_action(div().id("busy"), false), 0x9a9a9a, None),
                (Desktop::warning_action(div().id("install")), 0xe5c07b, Some(FontWeight::SEMIBOLD)),
            ] {
                assert!(button.style().background.is_none());
                assert!(button.style().border_color.is_none());
                let text = button.text_style().as_ref().unwrap();
                assert_eq!(text.color, Some(rgb(color).into()));
                assert_eq!(text.font_weight, weight);
            }
        }

        #[gpui::test]
        fn ghost_button_spacing_and_disabled_hover_and_click(cx: &mut TestAppContext) {
            use gpui::{
                Context, Hsla, InteractiveElement, IntoElement, Modifiers, ParentElement, Render,
                Styled, Window, canvas, div, rems, rgb,
            };
            use std::{cell::Cell, rc::Rc};

            struct ButtonProbe {
                desktop: Entity<Desktop>,
                colors: Rc<Cell<[Hsla; 2]>>,
                clicks: Rc<Cell<usize>>,
            }

            impl Render for ButtonProbe {
                fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                    window.set_rem_size(px(14.));
                    self.desktop.update(cx, |_, cx| {
                        let mut neutral = Desktop::action_button(cx, "neutral", "", |_, _, _| {});
                        let style = neutral.style();
                        assert_eq!(style.padding.left, Some(rems(0.25).into()));
                        assert_eq!(style.padding.top, Some(rems(0.125).into()));
                        assert!(style.background.is_none());

                        div().flex().items_start().gap_2().children([true, false].into_iter().enumerate().map(
                            |(index, enabled)| {
                                let colors = self.colors.clone();
                                let clicks = self.clicks.clone();
                                let id = if enabled { "enabled-choice" } else { "disabled-choice" };
                                Desktop::action_button_with_style(
                                    cx,
                                    id,
                                    canvas(
                                        |_, _, _| {},
                                        move |_, _, window, _| {
                                            let mut drawn = colors.get();
                                            drawn[index] = window.text_style().color;
                                            colors.set(drawn);
                                        },
                                    )
                                    .size(px(12.)),
                                    enabled,
                                    |button| Desktop::segmented_choice(button, true, enabled, 0x394354),
                                    move |_, _, _| clicks.set(clicks.get() + 1),
                                )
                                .debug_selector(|| id.to_owned())
                            },
                        ))
                    })
                }
            }

            let (desktop, _) = desktop(cx);
            let colors = Rc::new(Cell::new([rgb(0).into(); 2]));
            let clicks = Rc::new(Cell::new(0));
            let (_, visual) = cx.add_window_view(|_, _| ButtonProbe {
                desktop,
                colors: colors.clone(),
                clicks: clicks.clone(),
            });
            visual.simulate_mouse_move(gpui::point(px(300.), px(300.)), None, Modifiers::none());
            visual.run_until_parked();
            let enabled = visual.debug_bounds("enabled-choice").unwrap();
            let disabled = visual.debug_bounds("disabled-choice").unwrap();
            assert_eq!(disabled.size, enabled.size);
            assert_eq!(colors.get(), [rgb(0x8db7ff).into(), rgb(0x858a92).into()]);

            visual.simulate_mouse_move(enabled.center(), None, Modifiers::none());
            visual.run_until_parked();
            assert_eq!(colors.get()[0], rgb(0xffffff).into());
            assert_eq!(visual.debug_bounds("enabled-choice"), Some(enabled));
            visual.simulate_click(enabled.center(), Modifiers::none());
            assert_eq!(clicks.get(), 1);

            visual.simulate_mouse_move(disabled.center(), None, Modifiers::none());
            visual.run_until_parked();
            assert_eq!(colors.get(), [rgb(0x8db7ff).into(), rgb(0x858a92).into()]);
            assert_eq!(visual.debug_bounds("disabled-choice"), Some(disabled));
            visual.simulate_click(disabled.center(), Modifiers::none());
            assert_eq!(clicks.get(), 1);
        }

        #[gpui::test]
        fn ui_zoom_resizes_the_tab_bar_and_sidebar(cx: &mut TestAppContext) {
            let (view, cx) = desktop(cx);
            cx.simulate_resize(size(px(1200.), px(800.)));
            for (percent, bar_height, sidebar_width) in
                [(100, 31., 209.), (150, 46., 314.), (90, 27.5, 188.)]
            {
                cx.update(|window, app| {
                    view.update(app, |desktop, cx| {
                        desktop.apply_ui_scale(UiScale::from_percent(percent), window, cx);
                        cx.notify();
                    })
                });
                cx.run_until_parked();
                let bar = cx.debug_bounds("tab-bar").unwrap();
                assert_eq!(bar.size.height, px(bar_height));
                assert_eq!(
                    cx.debug_bounds("sidebar").unwrap().size.width,
                    px(sidebar_width)
                );
                assert_eq!(cx.debug_bounds("terminal").unwrap().top(), bar.bottom());
                assert_eq!(cx.update(|_, app| view.read(app).canvas.y), bar_height.round());
            }
        }
    }

    #[test]
    fn font_grid_uses_measured_advance_or_calibrated_fallback() {
        use super::{UiScale, px, size, terminal_cell_width};
        for (percent, fallback) in [(100, 7.948), (50, 4.215)] {
            let scale = UiScale::from_percent(percent);
            assert_eq!(terminal_cell_width(Some(size(px(6.25), px(9.))), scale), 6.25);
            assert!((terminal_cell_width(None, scale) - fallback).abs() < 0.001);
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
        for (pct, expected_side) in [(50, 105.), (100, 209.), (150, 314.), (200, 418.)] {
            let scale = UiScale::from_percent(pct);
            let side = sidebar_px(190., scale, 1200.);
            assert_eq!(side, expected_side);
            assert_eq!(sidebar_px(190., scale, 120.), 66.);
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
                    w: 10.5 * font + 2. * inset,
                    h: 5.5 * font + 2. * inset,
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
        tab.buffer_frame(4, TabFrame::Data(b"first paint".to_vec().into()));
        tab.buffer_frame(4, TabFrame::Resize(25, 30));
        assert!(
            matches!(&tab.early_frames[..], [(4, TabFrame::Data(bytes)), (4, TabFrame::Resize(25, 30))] if &bytes[..] == b"first paint")
        );
        tab.buffer_frame(4, TabFrame::Data(vec![0; MAX_PENDING].into()));
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
        for (percent, expected_header) in [(50, 10.), (100, 21.), (200, 42.)] {
            let scale = super::UiScale::from_percent(percent);
            let rect = super::PaneRect {
                x: 0.,
                y: 10.,
                w: 180.,
                h: 35.,
            };
            let header = super::pane_header_height(scale, true);
            assert_eq!(header, expected_header);
            assert_eq!(super::pane_header_height(scale, false), 0.);
            let body = super::pane_body_rect(rect, header);
            assert_eq!(body.y, 10. + expected_header);
            assert_eq!(body.h, (35. - expected_header).max(0.));
            assert!(super::scaled_pane_grid(body, 8., 14., scale).1 >= 1);
        }
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
            ("peer", false, "External input"),
            ("peer", true, "Remote input · build"),
            ("pinned", true, "Remote input · build"),
            ("ses_unknown", true, "Remote input · Remote"),
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
