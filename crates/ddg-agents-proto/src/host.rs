//! `ddg-agents-host` catalog, room/tab/space/executor, and standard client RPC types.

use crate::id::{parse_session_id, parse_standard_room_id, validate_background_room_id};
use crate::rpc::RpcError;
use crate::terminal::{self, AgentStatus, ControlMode, ForegroundHint};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

pub const CATALOG_SCHEMA_VERSION: u32 = 2;
pub const MAX_TABS_PER_ROOM: usize = 16;
pub const MAX_BACKGROUND_TABS: usize = 64;
pub const MAX_CATALOG_BYTES: usize = 2 << 20;
pub const MIN_SPLIT_RATIO_BP: u16 = 1500;
pub const MAX_SPLIT_RATIO_BP: u16 = 8500;
pub const DEFAULT_SPLIT_RATIO_BP: u16 = 5000;
pub const MAX_SPLIT_DEPTH: usize = 8;

pub type RoomId = String;
pub type TabId = String;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RoomKind {
    Standard,
    Background,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RoomVisibility {
    Visible,
    Hidden,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutorSpec {
    #[default]
    DefaultShell,
    Command {
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TabRuntimeState {
    Running { pty_id: u32, pid: u32 },
    Exited { status: String },
    StoppedAfterReboot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TabRecord {
    pub id: TabId,
    #[schemars(range(min = 1))]
    pub run_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_host_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_cwd: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub executor: ExecutorSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty_id: Option<u32>,
    pub runtime: TabRuntimeState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    /// Side by side: first on the left, second on the right.
    Horizontal,
    /// Stacked: first on top, second on the bottom.
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneNode {
    Leaf {
        tab_id: TabId,
    },
    Split {
        axis: SplitAxis,
        /// Share of the first pane, in basis points.
        #[schemars(range(min = MIN_SPLIT_RATIO_BP, max = MAX_SPLIT_RATIO_BP))]
        ratio_bp: u16,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

impl PaneNode {
    pub fn leaf(tab_id: TabId) -> Self {
        Self::Leaf { tab_id }
    }

    pub fn leaf_ids(&self, out: &mut Vec<TabId>) {
        match self {
            Self::Leaf { tab_id } => out.push(tab_id.clone()),
            Self::Split { first, second, .. } => {
                first.leaf_ids(out);
                second.leaf_ids(out);
            }
        }
    }

    pub fn contains(&self, tab_id: &str) -> bool {
        match self {
            Self::Leaf { tab_id: id } => id == tab_id,
            Self::Split { first, second, .. } => first.contains(tab_id) || second.contains(tab_id),
        }
    }

    pub fn split_leaf(
        &mut self,
        anchor: &str,
        axis: SplitAxis,
        new_tab_id: TabId,
        ratio_bp: u16,
    ) -> Result<bool, String> {
        validate_split_ratio(ratio_bp)?;
        let Some((leaf, depth)) = self.find_leaf_mut(anchor, 0) else {
            return Ok(false);
        };
        if depth >= MAX_SPLIT_DEPTH {
            return Err(format!("pane depth exceeds {MAX_SPLIT_DEPTH}"));
        }
        *leaf = Self::Split {
            axis,
            ratio_bp,
            first: Box::new(Self::leaf(anchor.to_owned())),
            second: Box::new(Self::leaf(new_tab_id)),
        };
        Ok(true)
    }

    fn find_leaf_mut(&mut self, anchor: &str, depth: usize) -> Option<(&mut Self, usize)> {
        match self {
            Self::Leaf { tab_id } if tab_id == anchor => Some((self, depth)),
            Self::Leaf { .. } => None,
            Self::Split { first, second, .. } => first
                .find_leaf_mut(anchor, depth + 1)
                .or_else(|| second.find_leaf_mut(anchor, depth + 1)),
        }
    }

    pub fn remove_tab(self, tab_id: &str) -> (Option<Self>, bool) {
        match self {
            Self::Leaf { tab_id: ref id } if id == tab_id => (None, true),
            Self::Leaf { .. } => (Some(self), false),
            Self::Split {
                axis,
                ratio_bp,
                first,
                second,
            } => {
                let (first, removed_first) = first.remove_tab(tab_id);
                let (second, removed_second) = second.remove_tab(tab_id);
                let node = match (first, second) {
                    (Some(first), Some(second)) => Some(Self::Split {
                        axis,
                        ratio_bp,
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                    (first, second) => first.or(second),
                };
                (node, removed_first || removed_second)
            }
        }
    }

    fn validate(&self, depth: usize, remaining: &mut HashSet<&str>) -> Result<(), String> {
        if depth > MAX_SPLIT_DEPTH {
            return Err(format!("pane depth exceeds {MAX_SPLIT_DEPTH}"));
        }
        match self {
            Self::Leaf { tab_id } => {
                if !remaining.remove(tab_id.as_str()) {
                    return Err(format!("unknown or duplicate pane tab: {tab_id}"));
                }
            }
            Self::Split {
                ratio_bp,
                first,
                second,
                ..
            } => {
                validate_split_ratio(*ratio_bp)?;
                first.validate(depth + 1, remaining)?;
                second.validate(depth + 1, remaining)?;
            }
        }
        Ok(())
    }
}

fn validate_split_ratio(ratio_bp: u16) -> Result<(), String> {
    if !(MIN_SPLIT_RATIO_BP..=MAX_SPLIT_RATIO_BP).contains(&ratio_bp) {
        return Err(format!(
            "ratio_bp must be in {MIN_SPLIT_RATIO_BP}..={MAX_SPLIT_RATIO_BP}"
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomLayout {
    pub groups: Vec<PaneNode>,
}

impl RoomLayout {
    pub fn default_for_tabs(tabs: &[TabRecord]) -> Self {
        Self {
            groups: tabs
                .iter()
                .map(|tab| PaneNode::leaf(tab.id.clone()))
                .collect(),
        }
    }

    pub fn is_trivial_single_leaves(&self) -> bool {
        self.groups
            .iter()
            .all(|group| matches!(group, PaneNode::Leaf { .. }))
    }

    pub fn validate_for_tabs(&self, tabs: &[TabRecord]) -> Result<(), String> {
        if self.groups.is_empty() {
            return Err("layout must have at least one group".into());
        }
        let mut remaining: HashSet<_> = tabs.iter().map(|tab| tab.id.as_str()).collect();
        if remaining.len() != tabs.len() {
            return Err("duplicate room tabs".into());
        }
        for group in &self.groups {
            group.validate(0, &mut remaining)?;
        }
        if !remaining.is_empty() {
            return Err("layout is missing room tabs".into());
        }
        Ok(())
    }

    pub fn split_leaf(
        &mut self,
        anchor: &str,
        axis: SplitAxis,
        new_tab_id: TabId,
        ratio_bp: u16,
    ) -> Result<bool, String> {
        if let Some(group) = self.groups.iter_mut().find(|group| group.contains(anchor)) {
            group.split_leaf(anchor, axis, new_tab_id, ratio_bp)
        } else {
            validate_split_ratio(ratio_bp)?;
            Ok(false)
        }
    }

    pub fn append_tab(&mut self, tab_id: TabId) {
        self.groups.push(PaneNode::leaf(tab_id));
    }

    pub fn remove_tab(&mut self, tab_id: &str) -> bool {
        let Some(index) = self.groups.iter().position(|group| group.contains(tab_id)) else {
            return false;
        };
        let (group, removed) = self.groups.remove(index).remove_tab(tab_id);
        if let Some(group) = group {
            self.groups.insert(index, group);
        }
        removed
    }

    pub fn unsplit_tab(&mut self, tab_id: &str) -> bool {
        let Some(index) = self
            .groups
            .iter()
            .position(|group| matches!(group, PaneNode::Split { .. }) && group.contains(tab_id))
        else {
            return false;
        };
        let (group, _) = self.groups.remove(index).remove_tab(tab_id);
        if let Some(group) = group {
            self.groups.insert(index, group);
            self.groups
                .insert(index + 1, PaneNode::leaf(tab_id.to_owned()));
        } else {
            self.groups.insert(index, PaneNode::leaf(tab_id.to_owned()));
        }
        true
    }

    pub fn reconcile(&mut self, valid_ids: &[TabId]) {
        let mut ids = Vec::new();
        for group in &self.groups {
            group.leaf_ids(&mut ids);
        }
        for id in ids.iter().filter(|id| !valid_ids.contains(id)) {
            self.remove_tab(id);
        }
        for id in valid_ids {
            if !self.groups.iter().any(|group| group.contains(id)) {
                self.append_tab(id.clone());
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RoomRecord {
    pub id: RoomId,
    pub kind: RoomKind,
    pub visibility: RoomVisibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub auto_name: String,
    pub tabs: Vec<TabRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<RoomLayout>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Catalog {
    pub schema_version: u32,
    pub host_id: String,
    pub rooms: Vec<RoomRecord>,
}

pub fn validate_agent_profile_slug(slug: &str) -> Result<(), String> {
    if !(1..=64).contains(&slug.len())
        || slug.contains("..")
        || !slug
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return Err("invalid agent_profile slug".into());
    }
    Ok(())
}

impl Catalog {
    pub fn empty(host_id: impl Into<String>) -> Self {
        Self {
            schema_version: CATALOG_SCHEMA_VERSION,
            host_id: host_id.into(),
            rooms: Vec::new(),
        }
    }

    /// Validates identity, room and tab invariants before accepting a persisted catalog.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != CATALOG_SCHEMA_VERSION {
            return Err(format!(
                "unsupported catalog schema_version {} (expected {})",
                self.schema_version, CATALOG_SCHEMA_VERSION
            ));
        }
        if self.host_id.trim().is_empty() {
            return Err("catalog host_id must not be empty".into());
        }
        let mut room_ids = HashSet::new();
        let mut tab_ids = HashSet::new();
        let mut has_background = false;
        for room in &self.rooms {
            if !room_ids.insert(room.id.as_str()) {
                return Err(format!("duplicate room id: {}", room.id));
            }
            let (visibility, tab_bounds) = match room.kind {
                RoomKind::Standard => {
                    parse_standard_room_id(&room.id)
                        .map_err(|e| format!("invalid standard room id {}: {e}", room.id))?;
                    if let Some(layout) = &room.layout {
                        layout.validate_for_tabs(&room.tabs)?;
                    }
                    (RoomVisibility::Visible, 1..=MAX_TABS_PER_ROOM)
                }
                RoomKind::Background => {
                    validate_background_room_id(&room.id)
                        .map_err(|e| format!("invalid background room id {}: {e}", room.id))?;
                    if has_background {
                        return Err("catalog has more than one background room".into());
                    }
                    if room.layout.is_some() {
                        return Err("background room cannot have a pane layout".into());
                    }
                    has_background = true;
                    (RoomVisibility::Hidden, 0..=MAX_BACKGROUND_TABS)
                }
            };
            if room.visibility != visibility {
                return Err(format!("room {} must be {visibility:?}", room.id));
            }
            if !tab_bounds.contains(&room.tabs.len()) {
                return Err(format!(
                    "room {} has {} tabs (expected {tab_bounds:?})",
                    room.id,
                    room.tabs.len()
                ));
            }
            for tab in &room.tabs {
                if parse_session_id(&tab.id).is_err() || !tab_ids.insert(tab.id.as_str()) {
                    return Err(format!("invalid or duplicate tab id: {}", tab.id));
                }
                if tab.run_generation == 0 {
                    return Err(format!("tab {} has run_generation 0", tab.id));
                }
                if let Some(parent) = &tab.parent_session_id
                    && parse_session_id(parent).is_err()
                {
                    return Err(format!("tab {} has invalid parent_session_id", tab.id));
                }
                if let Some(profile) = &tab.agent_profile {
                    validate_agent_profile_slug(profile)
                        .map_err(|e| format!("tab {}: {e}", tab.id))?;
                }
                if let Some(host_id) = &tab.parent_host_id
                    && (tab.parent_session_id.is_none()
                        || host_id.is_empty()
                        || host_id.chars().count() > 128
                        || host_id.chars().any(char::is_control))
                {
                    return Err(format!("tab {} has invalid parent_host_id", tab.id));
                }
                if let ExecutorSpec::Command { argv, .. } = &tab.executor
                    && argv.is_empty()
                {
                    return Err(format!("tab {} has empty command argv", tab.id));
                }
            }
        }
        Ok(())
    }

    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_CATALOG_BYTES {
            return Err(format!(
                "catalog file size {} exceeds limit {}",
                bytes.len(),
                MAX_CATALOG_BYTES
            ));
        }
        let catalog: Self =
            serde_json::from_slice(bytes).map_err(|e| format!("invalid catalog json: {e}"))?;
        catalog.validate()?;
        Ok(catalog)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HostHelloResult {
    pub protocol_version: u32,
    pub host_id: String,
    pub boot_generation: u64,
    pub conn_id: u64,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_server_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_protocol_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_pty_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_version: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CatalogSubscribeParams {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TabActivity {
    pub tab_id: TabId,
    pub run_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status: Option<AgentStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<ForegroundHint>,
    pub activity_seq: u64,
    pub seen_seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CatalogSubscribeResult {
    pub seq: u64,
    pub catalog: Catalog,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activities: Vec<TabActivity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_notice: Option<ResetNotice>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResetNotice {
    pub notice_id: String,
    /// Backup file basename, not an absolute path.
    pub backup_file: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RoomCreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_cwd: Option<String>,
    #[serde(default)]
    pub executor: ExecutorSpec,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RoomCreateResult {
    pub room: RoomRecord,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RoomRenameParams {
    pub room_id: RoomId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RoomRenameResult {
    pub room: RoomRecord,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomSetLayoutParams {
    pub room_id: RoomId,
    #[serde(default)]
    pub layout: Option<RoomLayout>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomSetLayoutResult {
    pub room: RoomRecord,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TabCreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// If omitted, create in the host's hidden background room.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_id: Option<RoomId>,
    /// Explicitly request background creation; cannot be combined with a room_id.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub executor: ExecutorSpec,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TabCreateResult {
    pub tab: TabRecord,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TabSplitParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_profile: Option<String>,
    pub request_id: Option<String>,
    pub anchor_tab_id: TabId,
    pub axis: SplitAxis,
    /// Defaults to 5000 (an even split) when omitted.
    #[schemars(range(min = MIN_SPLIT_RATIO_BP, max = MAX_SPLIT_RATIO_BP))]
    pub ratio_bp: Option<u16>,
    pub space_cwd: Option<String>,
    pub title: Option<String>,
    #[serde(default)]
    pub executor: ExecutorSpec,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TabSplitResult {
    pub room: RoomRecord,
    pub tab: TabRecord,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TabMarkSeenParams {
    pub tab_id: TabId,
    pub run_generation: u64,
    pub seen_seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TabMarkSeenResult {
    pub tab_id: TabId,
    pub run_generation: u64,
    pub activity_seq: u64,
    pub seen_seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TabScreenParams {
    pub tab_id: TabId,
    #[serde(default = "default_screen_lines")]
    #[schemars(range(min = 1, max = 2000))]
    pub lines: u32,
    #[serde(default = "default_screen_max_bytes")]
    #[schemars(range(min = 1, max = 262144))]
    pub max_bytes: u32,
}

fn default_screen_lines() -> u32 {
    24
}

fn default_screen_max_bytes() -> u32 {
    262_144
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TabWriteParams {
    pub tab_id: TabId,
    pub data_b64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_run_generation: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TabMsgSendParams {
    pub to_session_id: TabId,
    pub data_b64: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TabMoveParams {
    pub tab_id: TabId,
    pub target: SessionMoveTarget,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TabAttachParams {
    pub tab_id: TabId,
    #[serde(default = "default_true")]
    pub want_control: bool,
    pub cols: u16,
    pub rows: u16,
    #[serde(default = "default_history_lines")]
    pub history_lines: u32,
}

fn default_true() -> bool {
    true
}

fn default_history_lines() -> u32 {
    2000
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TabAttachResult {
    pub channel: u32,
    #[schemars(range(min = 1))]
    pub run_generation: u64,
    pub mode: ControlMode,
    pub control_epoch: u64,
    pub cols: u16,
    pub rows: u16,
    pub runtime: TabRuntimeState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TabCloseParams {
    pub tab_id: TabId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TabCloseResult {
    pub removed_room: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TabRestartParams {
    pub tab_id: TabId,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TabRestartResult {
    pub tab: TabRecord,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TabClaimControlParams {
    pub tab_id: TabId,
    #[serde(default)]
    pub force: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TabReleaseControlParams {
    pub tab_id: TabId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionCreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_id: Option<RoomId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<ExecutorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Required for session authentication; missing credentials are rejected by the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSummary {
    pub session_id: TabId,
    pub room_id: RoomId,
    pub room_kind: RoomKind,
    pub visibility: RoomVisibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    pub host_id: String,
    pub title: String,
    #[schemars(range(min = 1))]
    pub run_generation: u64,
    pub runtime: TabRuntimeState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionCreateResult {
    pub session: SessionSummary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionListParams {
    pub session_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_hidden: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_id: Option<RoomId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionListResult {
    pub sessions: Vec<SessionSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<HostQueryError>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HostQueryError {
    pub host_id: String,
    pub error: RpcError,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionMoveTarget {
    ExistingRoom {
        room_id: RoomId,
    },
    NewRoom {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionMoveParams {
    pub session_token: String,
    pub session_id: TabId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    pub target: SessionMoveTarget,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionMoveResult {
    pub session: SessionSummary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionCloseParams {
    pub session_token: String,
    pub session_id: TabId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
}

pub type SessionCloseResult = TabCloseResult;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionRestartParams {
    pub session_token: String,
    pub session_id: TabId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionRestartResult {
    pub session: SessionSummary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionScreenParams {
    pub session_token: String,
    pub session_id: TabId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    #[schemars(range(min = 1, max = 2000))]
    pub lines: u32,
    #[schemars(range(min = 1, max = 262144))]
    pub max_bytes: u32,
}

pub type SessionScreenResult = terminal::SessionReadScreenResult;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionWriteParams {
    pub session_token: String,
    pub session_id: TabId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    pub data_b64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_run_generation: Option<u64>,
}

pub type SessionWriteResult = terminal::SessionWriteInputResult;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MsgSendParams {
    pub session_token: String,
    pub to_session_id: TabId,
    pub data_b64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
}

pub type MsgSendResult = terminal::SessionSendResult;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MsgRecvParams {
    pub session_token: String,
    pub max_messages: u32,
    pub wait_ms: u64,
}

pub type MsgRecvResult = terminal::SessionRecvResult;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MsgSubscribeParams {
    pub session_token: String,
}

pub type MsgSubscribeResult = terminal::SessionInboxSubscribeResult;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ListHostsParams {
    pub session_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostStatus {
    Connected,
    Offline,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HostSummary {
    pub host_id: String,
    pub label: String,
    pub status: HostStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ListHostsResult {
    pub hosts: Vec<HostSummary>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RelayAttachParams {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RelayAttachResult {
    /// Nonzero 9-byte framed channel carrying credit-controlled NDJSON RelayFrame records in both directions.
    #[schemars(range(min = 1))]
    pub channel: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RelayEnvelope {
    /// The host must reject hops other than 1.
    #[schemars(range(min = 1, max = 1))]
    pub hop: u8,
    pub request_id: String,
    pub method: String,
    pub params: Value,
    /// The hub replaces this value with the authenticated peer identity.
    pub from_host_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayFrame {
    Request {
        #[serde(flatten)]
        envelope: RelayEnvelope,
    },
    Response {
        request_id: String,
        result: Value,
    },
    Error {
        request_id: String,
        error: RpcError,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostEvent {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_write_accepts_missing_or_explicit_generation() {
        let mut wire = json!({
            "session_token": "test", "session_id": "ses_00000000000000000000000001",
            "data_b64": "Zg==",
        });
        let write: SessionWriteParams = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(json!(write), wire);
        wire["expect_run_generation"] = json!(2);
        let write: SessionWriteParams = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(json!(write), wire);
    }

    #[test]
    fn session_list_round_trips_room_metadata_and_partial_errors() {
        let mut wire = json!({
            "sessions": [{
                "session_id": "ses_00000000000000000000000002", "room_id": "rm_bg_local",
                "room_kind": "background", "visibility": "hidden", "host_id": "local",
                "title": "worker", "run_generation": 2, "runtime": {"state": "stopped_after_reboot"},
            }]
        });
        let result: SessionListResult = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(json!(result), wire);

        wire["sessions"][0]["parent_session_id"] = json!("ses_00000000000000000000000001");
        wire["errors"] =
            json!([{"host_id": "peer", "error": RpcError::host_offline("peer offline")}]);
        let result: SessionListResult = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(json!(result), wire);
    }

    #[test]
    fn host_list_and_external_input_only_expose_metadata() {
        let summary = HostSummary {
            host_id: "peer".into(),
            label: "Peer".into(),
            status: HostStatus::Connected,
        };
        assert_eq!(
            json!(summary),
            json!({"host_id": "peer", "label": "Peer", "status": "connected"})
        );

        let event = HostEvent::ExternalInput {
            session_id: "ses_00000000000000000000000002".into(),
            pty_id: 5,
            run_generation: 2,
            byte_len: 3,
            from_session_id: "ses_00000000000000000000000001".into(),
            from_host_id: "peer".into(),
            is_remote: true,
        };
        let metadata = json!({
            "type": "external_input", "session_id": "ses_00000000000000000000000002",
            "pty_id": 5, "run_generation": 2, "byte_len": 3,
            "from_session_id": "ses_00000000000000000000000001", "from_host_id": "peer", "is_remote": true,
        });
        assert_eq!(json!(event), metadata);
        assert_eq!(
            json!(serde_json::from_value::<terminal::PtyEvent>(metadata.clone()).unwrap()),
            metadata
        );
    }

    #[test]
    fn relay_request_and_replacement_close_have_stable_wire_shapes() {
        let request = json!({
            "type": "request", "hop": 1, "request_id": "1", "method": "host.list_hosts",
            "params": {}, "from_host_id": "peer",
        });
        let frame: RelayFrame = serde_json::from_value(request.clone()).unwrap();
        assert!(matches!(&frame, RelayFrame::Request { envelope } if envelope.hop == 1));
        assert_eq!(json!(frame), request);
        assert_eq!(
            json!(terminal::CloseReason::RelayReplaced),
            json!({"reason": "relay_replaced"})
        );
    }

    #[test]
    fn activity_types_and_catalog_v2_validation() {
        let mut wire = json!({"schema_version": 2, "host_id": "local", "rooms": [{
            "id": "rm_00000000000000000000000000", "kind": "standard",
            "visibility": "visible", "auto_name": "test", "tabs": [{
                "id": "ses_00000000000000000000000001", "run_generation": 1,
                "runtime": {"state": "stopped_after_reboot"},
            }],
        }]});
        let load = |wire: &Value| Catalog::from_json_bytes(wire.to_string().as_bytes());
        let legacy = json!(load(&wire).unwrap().rooms[0].tabs[0]);
        for field in ["agent_profile", "parent_host_id"] {
            assert!(legacy.get(field).is_none());
        }

        let mut metadata = wire["rooms"][0]["tabs"][0].clone();
        for invalid in ["", "../x", "a..b", "Pi", "a/b", "a\n", &"a".repeat(65)] {
            metadata["agent_profile"] = json!(invalid);
            wire["rooms"][0]["tabs"][0] = metadata.clone();
            assert!(
                load(&wire).unwrap_err().contains("agent_profile"),
                "{invalid:?}"
            );
        }
        for valid in ["a", &"a".repeat(64), "a.b_c-0"] {
            metadata["agent_profile"] = json!(valid);
            wire["rooms"][0]["tabs"][0] = metadata.clone();
            let catalog = load(&wire).unwrap();
            assert_eq!(json!(catalog.rooms[0].tabs[0])["agent_profile"], valid);
        }

        metadata["parent_host_id"] = json!("peer");
        wire["rooms"][0]["tabs"][0] = metadata.clone();
        assert!(load(&wire).unwrap_err().contains("parent_host_id"));
        metadata["parent_session_id"] = json!("ses_00000000000000000000000002");
        for invalid in ["", "peer\u{0085}", &"한".repeat(129)] {
            metadata["parent_host_id"] = json!(invalid);
            wire["rooms"][0]["tabs"][0] = metadata.clone();
            assert!(
                load(&wire).unwrap_err().contains("parent_host_id"),
                "{invalid:?}"
            );
        }
        for valid in ["peer", &"한".repeat(128)] {
            metadata["parent_host_id"] = json!(valid);
            wire["rooms"][0]["tabs"][0] = metadata.clone();
            let catalog = load(&wire).unwrap();
            assert_eq!(json!(catalog.rooms[0].tabs[0])["parent_host_id"], valid);
        }
    }

    #[test]
    fn activity_types_and_catalog_v2_create_params() {
        fn check<T: serde::de::DeserializeOwned + Serialize>(
            mut legacy: Value,
            forbidden: &[&str],
        ) {
            let params = serde_json::from_value::<T>(legacy.clone()).unwrap();
            assert!(json!(params).get("agent_profile").is_none());
            for key in forbidden {
                legacy[*key] = json!("not allowed");
                assert!(
                    serde_json::from_value::<T>(legacy.clone()).is_err(),
                    "{key}"
                );
                legacy.as_object_mut().unwrap().remove(*key);
            }
            legacy["agent_profile"] = json!("pi");
            let params = serde_json::from_value::<T>(legacy).unwrap();
            assert_eq!(json!(params)["agent_profile"], "pi");
        }

        check::<RoomCreateParams>(json!({"cols": 80, "rows": 24}), &[]);
        check::<TabCreateParams>(
            json!({"cols": 80, "rows": 24}),
            &["unknown", "parent_session_id", "parent_host_id"],
        );
        check::<TabSplitParams>(
            json!({"anchor_tab_id": "ses_00000000000000000000000001", "axis": "horizontal",
                "cols": 80, "rows": 24}),
            &["unknown", "parent_session_id", "parent_host_id"],
        );
        check::<SessionCreateParams>(json!({}), &[]);
    }

    #[test]
    fn activity_types_and_catalog_v2_activity_wire() {
        let mut snapshot = json!({"seq": 1, "catalog": Catalog::empty("local")});
        let result: CatalogSubscribeResult = serde_json::from_value(snapshot.clone()).unwrap();
        assert!(result.activities.is_empty());
        assert_eq!(json!(result), snapshot);

        let tab_id = "ses_00000000000000000000000001";
        let activity = json!({"tab_id": tab_id, "run_generation": 2, "activity_seq": 1,
            "seen_seq": 0, "agent_status": {"state": "done", "records": [], "status_ms": 1},
            "foreground": {"pid": 42, "comm": "pi"}});
        snapshot["activities"] = json!([activity]);
        let result: CatalogSubscribeResult = serde_json::from_value(snapshot.clone()).unwrap();
        assert_eq!(json!(result), snapshot);

        let mut delta = activity;
        delta["type"] = json!("tab_activity_changed");
        delta["room_id"] = json!("rm_00000000000000000000000000");
        let seen = json!({"type": "tab_seen_changed", "tab_id": tab_id, "run_generation": 2,
            "activity_seq": 1, "seen_seq": 1});
        for wire in [delta, seen] {
            let event: HostEvent = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(json!(event), wire);
        }

        let mut params = json!({"tab_id": tab_id, "run_generation": 2, "seen_seq": 1});
        let decoded: TabMarkSeenParams = serde_json::from_value(params.clone()).unwrap();
        assert_eq!(json!(decoded), params);
        params["session_token"] = json!("not allowed");
        assert!(serde_json::from_value::<TabMarkSeenParams>(params).is_err());
    }

    #[test]
    fn catalog_v2_room_and_generation_bounds() {
        fn room(kind: &str, visibility: &str, tab_count: u32) -> serde_json::Value {
            serde_json::json!({
                "id": if kind == "standard" { "rm_00000000000000000000000000" } else { "rm_bg_host1" },
                "kind": kind,
                "visibility": visibility,
                "auto_name": kind,
                "tabs": (0..tab_count).map(|n| serde_json::json!({
                    "id": format!("ses_{n:026}"),
                    "run_generation": 1,
                    "runtime": {"state": "stopped_after_reboot"},
                })).collect::<Vec<_>>(),
            })
        }

        fn validate(rooms: Vec<serde_json::Value>) -> Result<Catalog, String> {
            let json = serde_json::json!({"schema_version": 2, "host_id": "host1", "rooms": rooms});
            Catalog::from_json_bytes(json.to_string().as_bytes())
        }

        for (kind, visibility, count, valid) in [
            ("standard", "visible", 0, false),
            ("standard", "visible", 1, true),
            ("standard", "visible", 16, true),
            ("standard", "visible", 17, false),
            ("standard", "hidden", 1, false),
            ("background", "hidden", 0, true),
            ("background", "hidden", 64, true),
            ("background", "hidden", 65, false),
            ("background", "visible", 0, false),
        ] {
            assert_eq!(
                validate(vec![room(kind, visibility, count)]).is_ok(),
                valid,
                "{kind} {visibility} {count}"
            );
        }
        let mut background_with_layout = room("background", "hidden", 1);
        background_with_layout["layout"] = json!({"groups": [PaneNode::leaf(sample_tab(0).id)]});
        assert!(
            validate(vec![background_with_layout])
                .unwrap_err()
                .contains("background room")
        );

        let bg = room("background", "hidden", 0);
        let mut other_bg = bg.clone();
        other_bg["id"] = serde_json::json!("rm_bg_host2");
        assert!(validate(vec![bg, other_bg]).is_err());

        let mut standard = room("standard", "visible", 1);
        standard["tabs"][0]["run_generation"] = serde_json::json!(0);
        assert!(validate(vec![standard]).is_err());
    }

    fn sample_tab(id: u64) -> TabRecord {
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
            pty_id: None,
            runtime: TabRuntimeState::StoppedAfterReboot,
        }
    }

    #[test]
    fn layout_splits_prunes_and_reconciles_in_group_order() {
        let [a, b, c, d, e] = [1, 2, 3, 4, 5].map(sample_tab);
        let leaf = |tab: &TabRecord| PaneNode::leaf(tab.id.clone());
        let mut layout = RoomLayout::default_for_tabs(&[a.clone(), b.clone()]);
        assert!(layout.is_trivial_single_leaves());
        assert!(
            !layout
                .split_leaf("missing", SplitAxis::Horizontal, c.id.clone(), 5000)
                .unwrap()
        );
        assert!(
            layout
                .split_leaf(&a.id, SplitAxis::Horizontal, c.id.clone(), 5000)
                .unwrap()
        );
        assert!(
            layout
                .split_leaf(&c.id, SplitAxis::Vertical, d.id.clone(), 6000)
                .unwrap()
        );
        layout
            .validate_for_tabs(&[a.clone(), b.clone(), c.clone(), d.clone()])
            .unwrap();
        let mut ids = Vec::new();
        layout.groups[0].leaf_ids(&mut ids);
        assert_eq!(ids, [a.id.as_str(), c.id.as_str(), d.id.as_str()]);
        assert!(!layout.unsplit_tab(&b.id));
        assert!(layout.unsplit_tab(&c.id));
        assert!(!layout.is_trivial_single_leaves());
        assert_eq!(layout.groups[1..], [leaf(&c), leaf(&b)]);
        assert!(layout.remove_tab(&d.id));
        assert!(!layout.remove_tab(&d.id));
        assert_eq!(layout.groups, [leaf(&a), leaf(&c), leaf(&b)]);
        layout.reconcile(&[e.id.clone(), a.id.clone(), b.id.clone()]);
        assert_eq!(layout.groups, [leaf(&a), leaf(&b), leaf(&e)]);
        layout.validate_for_tabs(&[a, b, e]).unwrap();
    }

    #[test]
    fn layout_enforces_split_depth_and_ratio_bounds() {
        let mut tabs = (0..=MAX_SPLIT_DEPTH as u64 + 1)
            .map(sample_tab)
            .collect::<Vec<_>>();
        let extra = tabs.pop().unwrap();
        let mut layout = RoomLayout::default_for_tabs(&tabs[..1]);
        let anchor = &tabs[0].id;
        for (tab, ratio) in tabs[1..]
            .iter()
            .zip([MIN_SPLIT_RATIO_BP, MAX_SPLIT_RATIO_BP].into_iter().cycle())
        {
            assert!(
                layout
                    .split_leaf(anchor, SplitAxis::Horizontal, tab.id.clone(), ratio)
                    .unwrap()
            );
        }
        layout.validate_for_tabs(&tabs).unwrap();
        assert!(
            layout
                .split_leaf(anchor, SplitAxis::Horizontal, extra.id.clone(), 5000)
                .is_err()
        );
        for ratio in [MIN_SPLIT_RATIO_BP - 1, MAX_SPLIT_RATIO_BP + 1] {
            let mut leaf = PaneNode::leaf(anchor.clone());
            assert!(
                leaf.split_leaf(anchor, SplitAxis::Vertical, extra.id.clone(), ratio)
                    .is_err()
            );
            assert_eq!(leaf, PaneNode::leaf(anchor.clone()));

            let mut invalid = layout.clone();
            let PaneNode::Split { ratio_bp, .. } = &mut invalid.groups[0] else {
                unreachable!()
            };
            *ratio_bp = ratio;
            assert!(
                invalid
                    .validate_for_tabs(&tabs)
                    .unwrap_err()
                    .contains("ratio")
            );
        }
        layout.groups = vec![PaneNode::Split {
            axis: SplitAxis::Horizontal,
            ratio_bp: 5000,
            first: Box::new(layout.groups.remove(0)),
            second: Box::new(PaneNode::leaf(extra.id.clone())),
        }];
        tabs.push(extra);
        assert!(
            layout
                .validate_for_tabs(&tabs)
                .unwrap_err()
                .contains("depth")
        );
    }

    #[test]
    fn layout_must_partition_room_tabs() {
        let tabs = [sample_tab(1), sample_tab(2)];
        let layout = RoomLayout::default_for_tabs(&tabs);
        for groups in [
            vec![],
            vec![layout.groups[0].clone()],
            vec![layout.groups[0].clone(), layout.groups[0].clone()],
            vec![layout.groups[0].clone(), PaneNode::leaf(sample_tab(3).id)],
        ] {
            assert!(RoomLayout { groups }.validate_for_tabs(&tabs).is_err());
        }
        assert!(
            layout
                .validate_for_tabs(&[tabs[0].clone(), tabs[0].clone()])
                .is_err()
        );
    }

    #[test]
    fn validates_catalog_constraints_and_rejects_corruption() {
        let mut cat = Catalog::empty("host-1");
        cat.rooms.push(RoomRecord {
            id: "rm_00000000000000000000000001".into(),
            kind: RoomKind::Standard,
            visibility: RoomVisibility::Visible,
            name: Some("작업".into()),
            auto_name: "룸 1".into(),
            tabs: vec![sample_tab(1)],
            layout: None,
        });
        let bytes = serde_json::to_vec(&cat).unwrap();
        assert!(json!(cat)["rooms"][0].get("layout").is_none());
        assert_eq!(Catalog::from_json_bytes(&bytes).unwrap(), cat);

        let mut invalid_layout = cat.clone();
        invalid_layout.rooms[0].layout = Some(RoomLayout { groups: vec![] });
        assert!(invalid_layout.validate().is_err());

        // Unsupported schema version
        let mut bad = cat.clone();
        bad.schema_version = 99;
        assert!(bad.validate().is_err());

        // Duplicate tab ID
        let mut dup = cat.clone();
        dup.rooms[0].tabs.push(sample_tab(1));
        assert!(dup.validate().is_err());

        let mut duplicate_room = cat.clone();
        let mut room = cat.rooms[0].clone();
        room.tabs = vec![sample_tab(2)];
        duplicate_room.rooms.push(room);
        assert!(duplicate_room.validate().is_err());

        let mut bad_id = cat.clone();
        bad_id.rooms[0].id = "rm_bg_host1".into();
        assert!(bad_id.validate().is_err());
        bad_id.rooms[0].id = cat.rooms[0].id.clone();
        bad_id.rooms[0].tabs[0].id = "ses_foo".into();
        assert!(bad_id.validate().is_err());
    }
}
