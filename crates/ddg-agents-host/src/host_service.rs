//! Room/session lifecycle, catalog transactions, and terminal event handling.

use crate::catalog_store::{
    SESSION_ENV_KEYS, display_name, folder_basename, has_visible_room, is_reserved_env,
    load_catalog, reconcile_with_live_ptys, save_catalog,
};
use crate::hub::{Caller, Relay};
use crate::paths::{self, Paths};
use crate::plugin_host::PluginHost;
use crate::term_conn::TermConn;
use crate::token_store::{AuthenticatedCaller, TokenStore};
use ddg_agents_proto::frame::{CONTROL_CHANNEL, Frame, FrameKind, encode_frame};
use ddg_agents_proto::host::*;
use ddg_agents_proto::id::{
    background_room_id, decode_base64_bounded, generate_room_id, generate_session_id,
    sanitize_display_text,
};
use ddg_agents_proto::rpc::{ErrorKind, Notification, RpcError};
use ddg_agents_proto::terminal::*;
use ddg_agents_transport::presets::BUILTIN_AGENT_PRESETS;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::File;
#[cfg(unix)]
use std::fs::{OpenOptions, Permissions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{Notify, RwLock, mpsc, watch};

struct CatalogSub {
    conn_id: u64,
    channel: u32,
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

struct ClientNoticeSink {
    conn_id: u64,
    term_conn_id: Option<u64>,
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

#[derive(Default)]
struct Operations {
    in_flight_creates: HashMap<String, usize>,
    in_flight_restarts: HashSet<String>,
    in_flight_closes: HashSet<String>,
    spawning: HashMap<(String, u64), watch::Sender<()>>,
    idempotency: HashMap<IdempotencyKey, Option<Value>>,
}

type IdempotencyKey = (&'static str, String, String);

enum Reservation {
    Create(String),
    Restart(String),
    Close(String),
    Spawn(String, u64),
    Request(IdempotencyKey),
}

struct Guard<'a>(&'a HostService, Reservation);

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let mut st = self.0.lock();
        let ops = &mut st.ops;
        match &self.1 {
            Reservation::Create(id) => {
                let count = ops.in_flight_creates.get_mut(id).expect("reserved create");
                *count -= 1;
                if *count == 0 {
                    ops.in_flight_creates.remove(id);
                }
            }
            Reservation::Restart(id) => {
                ops.in_flight_restarts.remove(id);
            }
            Reservation::Close(id) => {
                ops.in_flight_closes.remove(id);
            }
            Reservation::Spawn(id, generation) => {
                ops.spawning.remove(&(id.clone(), *generation));
            }
            Reservation::Request(key) => {
                if ops.idempotency.get(key).is_some_and(Option::is_none) {
                    ops.idempotency.remove(key);
                }
            }
        }
    }
}

impl Guard<'_> {
    fn cache(&self, result: Value) {
        if let Reservation::Request(key) = &self.1 {
            self.0
                .lock()
                .ops
                .idempotency
                .insert(key.clone(), Some(result));
        }
    }
}

struct HostState {
    catalog: Catalog,
    catalog_save_pending: bool,
    activities: HashMap<TabId, TabActivity>,
    seq: u64,
    reset_notice: Option<ResetNotice>,
    subs: Vec<CatalogSub>,
    clients: Vec<ClientNoticeSink>,
    tokens: TokenStore,
    ops: Operations,
    term: Arc<TermConn>,
}

pub struct HostService {
    pub paths: Paths,
    pub host_id: String,
    pub plugins: Arc<PluginHost>,
    pub relay: Relay,
    state: Mutex<HostState>,
    audit: Mutex<File>,
    next_conn_id: AtomicU64,
    pub stop: Notify,
    pub reload_gate: Arc<RwLock<()>>,
}

enum CreateTarget {
    Existing(String),
    Split {
        room_id: RoomId,
        anchor: TabId,
        axis: SplitAxis,
        ratio_bp: u16,
    },
    Standard {
        id: String,
        name: Option<String>,
    },
    Background(String),
}

impl CreateTarget {
    fn id(&self) -> &str {
        match self {
            Self::Existing(id) | Self::Standard { id, .. } | Self::Background(id) => id,
            Self::Split { room_id, .. } => room_id,
        }
    }

    fn split_layout(
        &self,
        room: &RoomRecord,
        new_id: &str,
    ) -> Result<Option<RoomLayout>, RpcError> {
        let Self::Split {
            anchor,
            axis,
            ratio_bp,
            ..
        } = self
        else {
            return Ok(None);
        };
        let mut layout = room
            .layout
            .clone()
            .unwrap_or_else(|| RoomLayout::default_for_tabs(&room.tabs));
        if !layout
            .split_leaf(anchor, *axis, new_id.to_owned(), *ratio_bp)
            .map_err(RpcError::invalid_params)?
        {
            return Err(RpcError::session_not_found("split anchor not found"));
        }
        Ok(Some(layout))
    }

    fn new_room(self, cwd: Option<&str>) -> Result<RoomRecord, RpcError> {
        match self {
            Self::Existing(_) | Self::Split { .. } => Err(room_not_found()),
            Self::Standard { id, name } => Ok(standard_room(id, name, cwd)),
            Self::Background(id) => Ok(RoomRecord {
                id,
                kind: RoomKind::Background,
                visibility: RoomVisibility::Hidden,
                name: None,
                auto_name: "Background".into(),
                tabs: Vec::new(),
                layout: None,
            }),
        }
    }
}

impl HostService {
    pub async fn start(
        paths: Paths,
        relay: Relay,
        restored: Option<TokenStore>,
    ) -> io::Result<Arc<Self>> {
        paths.create_dirs()?;
        let audit = open_audit(&paths.state_dir.join("host.log"))?;
        let restoring = restored.is_some();
        let (term, ptys, ch, rx) = Self::connect_terminal(&paths, !restoring).await?;
        let host_id = term.hello.server_id.clone();
        let (mut catalog, reset_notice) = load_catalog(&paths.catalog_file, &host_id)?;
        if !restoring {
            Self::kill_legacy_ptys(&term, &ptys).await?;
        }
        reconcile_with_live_ptys(&mut catalog, &ptys);
        let mut tokens = restored.unwrap_or_default();
        tokens.reconcile_with_live_ptys(&ptys);

        let service = Arc::new(Self {
            paths,
            host_id,
            plugins: PluginHost::empty(),
            relay,
            state: Mutex::new(HostState {
                catalog,
                catalog_save_pending: false,
                activities: HashMap::new(),
                seq: 0,
                reset_notice,
                subs: Vec::new(),
                clients: Vec::new(),
                tokens,
                ops: Operations::default(),
                term: term.clone(),
            }),
            audit: Mutex::new(audit),
            next_conn_id: AtomicU64::new(1),
            stop: Notify::new(),
            reload_gate: Arc::new(RwLock::new(())),
        });
        Self::restore_activities(&mut service.lock(), &ptys);
        if !restoring {
            service.flush_catalog()?;
        }
        service.watch_terminal(term, ch, rx);
        Ok(service)
    }

    async fn connect_terminal(
        paths: &Paths,
        start: bool,
    ) -> io::Result<(
        Arc<TermConn>,
        Vec<PtyInfo>,
        u32,
        mpsc::UnboundedReceiver<Frame>,
    )> {
        let term = if start {
            TermConn::connect(paths, "ddg-agents-host-control").await?
        } else {
            TermConn::connect_existing(paths, "ddg-agents-host-control").await?
        };
        // Subscribe before using the snapshot so exits between snapshot and restore are not lost.
        let (tx, rx) = mpsc::unbounded_channel();
        let sub: PtySubscribeResult = term
            .call_into_stream("pty.subscribe", json!({}), tx)
            .await
            .map_err(|e| io::Error::other(e.message))?;
        let ch = sub
            .channel
            .ok_or_else(|| io::Error::other("terminal omitted event channel"))?;
        Ok((term, sub.ptys, ch, rx))
    }

    async fn kill_legacy_ptys(term: &TermConn, ptys: &[PtyInfo]) -> io::Result<()> {
        for pty in ptys.iter().filter(|pty| pty.session_id.is_empty()) {
            Self::kill(term, pty.pty_id)
                .await
                .map_err(|e| io::Error::other(e.message))?;
        }
        Ok(())
    }

    fn watch_terminal(
        self: &Arc<Self>,
        mut term: Arc<TermConn>,
        mut ch: u32,
        mut rx: mpsc::UnboundedReceiver<Frame>,
    ) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let mut line_buf = Vec::new();
                while let Some(frame) = rx.recv().await {
                    match frame.kind {
                        FrameKind::Close => break,
                        FrameKind::Data => {}
                        _ => continue,
                    }
                    let Some(host) = weak.upgrade() else { return };
                    let _watch = host.reload_gate.read().await;
                    line_buf.extend_from_slice(&frame.payload);
                    while let Some(pos) = line_buf.iter().position(|b| *b == b'\n') {
                        let line: Vec<_> = line_buf.drain(..=pos).collect();
                        host.handle_pty_event_line(&term, &line).await;
                    }
                    let _ = term.send_frame(
                        ch,
                        FrameKind::Credit,
                        &(frame.payload.len() as u32).to_le_bytes(),
                    );
                }
                term.close();
                loop {
                    let Some(host) = weak.upgrade() else { return };
                    let watch = host.reload_gate.read().await;
                    if let Ok((next, ptys, channel, frames)) =
                        Self::connect_terminal(&host.paths, true).await
                        && next.hello.server_id == host.host_id
                        && Self::kill_legacy_ptys(&next, &ptys).await.is_ok()
                    {
                        let mut st = host.lock();
                        host.reconcile(&mut st, &ptys);
                        if st.term.hello.boot_generation != next.hello.boot_generation {
                            for client in &mut st.clients {
                                client.term_conn_id = None;
                            }
                        }
                        st.term = next.clone();
                        term = next;
                        ch = channel;
                        rx = frames;
                        break;
                    }
                    drop(watch);
                    drop(host);
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        });
    }

    fn lock(&self) -> MutexGuard<'_, HostState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn term(&self) -> Result<Arc<TermConn>, RpcError> {
        let term = self.lock().term.clone();
        if term.is_alive() {
            Ok(term)
        } else {
            Err(terminal_unavailable())
        }
    }

    pub async fn detect_agent_presets(&self) -> HostAgentPresetsDetectResult {
        let probed = match self.term() {
            Ok(term) => term.probe_bins().await,
            Err(_) => None,
        };
        HostAgentPresetsDetectResult {
            items: BUILTIN_AGENT_PRESETS
                .iter()
                .map(|preset| AgentPresetDetection {
                    id: preset.id.into(),
                    status: probed
                        .as_ref()
                        .and_then(|result| {
                            result
                                .items
                                .iter()
                                .find(|item| item.bin == preset.detect_bin)
                        })
                        .map_or(PresetDetectStatus::Unknown, |item| item.status),
                })
                .collect(),
        }
    }

    pub fn flush_catalog(&self) -> io::Result<()> {
        self.save_observed(&mut self.lock())
    }

    pub fn token_snapshot(&self) -> io::Result<zeroize::Zeroizing<Vec<u8>>> {
        self.lock().tokens.snapshot()
    }

    pub fn clear_tokens(&self) {
        self.lock().tokens = TokenStore::default();
    }

    pub fn boot_generation(&self) -> u64 {
        self.lock().term.hello.boot_generation
    }

    pub fn next_conn_id(&self) -> u64 {
        self.next_conn_id.fetch_add(1, Ordering::Relaxed)
    }

    fn commit(&self, st: &mut HostState, catalog: Catalog) -> Result<(), RpcError> {
        catalog.validate().map_err(internal)?;
        save_catalog(&self.paths.catalog_file, &catalog, st.reset_notice.as_ref())
            .map_err(internal)?;
        st.catalog = catalog;
        st.catalog_save_pending = false;
        self.catalog_changed(st);
        Ok(())
    }

    fn save_observed(&self, st: &mut HostState) -> io::Result<()> {
        let result = save_catalog(
            &self.paths.catalog_file,
            &st.catalog,
            st.reset_notice.as_ref(),
        );
        st.catalog_save_pending = result.is_err();
        if st.catalog_save_pending {
            self.audit("catalog_save_failed", "");
        }
        result
    }

    // Exit/recovery is irreversible even if disk I/O fails; keep the live snapshot truthful.
    fn publish_observed(&self, st: &mut HostState) -> Result<(), RpcError> {
        let result = self.save_observed(st).map_err(internal);
        self.catalog_changed(st);
        result
    }

    fn publish_observed_if_changed(&self, st: &mut HostState, changed: bool) {
        if changed {
            let _ = self.publish_observed(st);
        } else if st.catalog_save_pending {
            // The live change was already broadcast; only durability needs a retry.
            let _ = self.save_observed(st);
        }
    }

    fn catalog_changed(&self, st: &mut HostState) {
        if has_visible_room(&st.catalog) {
            st.reset_notice = None;
        }
        st.seq += 1;
        Self::notify_catalog(
            st,
            "host.catalog_changed",
            json!({
                "seq": st.seq, "catalog": st.catalog, "reset_notice": st.reset_notice,
            }),
        );
    }

    fn notify_catalog(st: &mut HostState, method: &str, params: Value) {
        let note = Notification::new(method, params);
        let line =
            format!("{}\n", serde_json::to_string(&note).expect("serializable")).into_bytes();
        st.subs.retain(|sub| sub.tx.send(line.clone()).is_ok());
    }

    fn notify(st: &mut HostState, method: &str, params: Value) {
        let bytes = serde_json::to_vec(&Notification::new(method, params)).expect("serializable");
        if let Ok(frame) = encode_frame(CONTROL_CHANNEL, FrameKind::Data, &bytes) {
            st.clients
                .retain(|client| client.tx.send(frame.clone()).is_ok());
        }
    }

    pub fn catalog_subscribe(
        &self,
        conn_id: u64,
        channel: u32,
        tx: mpsc::UnboundedSender<Vec<u8>>,
    ) -> CatalogSubscribeResult {
        let mut st = self.lock();
        st.subs.push(CatalogSub {
            conn_id,
            channel,
            tx,
        });
        CatalogSubscribeResult {
            seq: st.seq,
            catalog: st.catalog.clone(),
            activities: st.activities.values().cloned().collect(),
            channel: Some(channel),
            reset_notice: st.reset_notice.clone(),
        }
    }

    fn activity_changed(st: &mut HostState, room_id: &str, tab_id: &str) {
        if st.subs.is_empty() {
            return;
        }
        let activity = &st.activities[tab_id];
        Self::notify_catalog(
            st,
            "host.tab_activity_changed",
            json!(HostEvent::TabActivityChanged {
                tab_id: activity.tab_id.clone(),
                room_id: room_id.to_owned(),
                run_generation: activity.run_generation,
                agent_status: activity.agent_status.clone(),
                foreground: activity.foreground.clone(),
                activity_seq: activity.activity_seq,
                seen_seq: activity.seen_seq,
            }),
        );
    }

    pub fn tab_mark_seen(&self, params: TabMarkSeenParams) -> Result<TabMarkSeenResult, RpcError> {
        let mut st = self.lock();
        let (_, tab) = find_tab(&st.catalog, &params.tab_id)?;
        if tab.run_generation != params.run_generation {
            return Err(RpcError::generation_mismatch("tab run generation changed"));
        }
        let activity = st
            .activities
            .get_mut(&params.tab_id)
            .ok_or_else(|| RpcError::session_not_found("tab activity ended"))?;
        let activity_seq = activity.activity_seq;
        let seen_seq = activity.seen_seq.max(params.seen_seq.min(activity_seq));
        if seen_seq != activity.seen_seq {
            activity.seen_seq = seen_seq;
            Self::notify_catalog(
                &mut st,
                "host.tab_seen_changed",
                json!(HostEvent::TabSeenChanged {
                    tab_id: params.tab_id.clone(),
                    run_generation: params.run_generation,
                    activity_seq,
                    seen_seq,
                }),
            );
        }
        Ok(TabMarkSeenResult {
            tab_id: params.tab_id,
            run_generation: params.run_generation,
            activity_seq,
            seen_seq,
        })
    }

    pub fn register_client(&self, conn_id: u64, tx: mpsc::UnboundedSender<Vec<u8>>) {
        self.lock().clients.push(ClientNoticeSink {
            conn_id,
            term_conn_id: None,
            tx,
        });
    }

    pub fn register_client_term_link(&self, conn_id: u64, term_conn_id: u64) {
        if let Some(client) = self
            .lock()
            .clients
            .iter_mut()
            .find(|c| c.conn_id == conn_id)
        {
            client.term_conn_id = Some(term_conn_id);
        }
    }

    pub fn disconnect_conn(&self, conn_id: u64) {
        let mut st = self.lock();
        st.subs.retain(|s| s.conn_id != conn_id);
        st.clients.retain(|s| s.conn_id != conn_id);
    }

    pub fn detach_sub_channel(&self, conn_id: u64, channel: u32) {
        self.lock()
            .subs
            .retain(|s| !(s.conn_id == conn_id && s.channel == channel));
    }

    fn reconcile(&self, st: &mut HostState, ptys: &[PtyInfo]) {
        let mut catalog = st.catalog.clone();
        reconcile_with_live_ptys(&mut catalog, ptys);
        st.tokens.reconcile_with_live_ptys(ptys);
        let changed = catalog != st.catalog;
        if changed {
            st.catalog = catalog;
        }
        self.publish_observed_if_changed(st, changed);
        Self::restore_activities(st, ptys);
    }

    fn restore_activities(st: &mut HostState, ptys: &[PtyInfo]) {
        let live: HashMap<_, _> = ptys
            .iter()
            .map(|pty| ((pty.session_id.as_str(), pty.run_generation), pty))
            .collect();
        let mut previous = std::mem::take(&mut st.activities);
        let mut changed = Vec::new();
        for room in &st.catalog.rooms {
            for tab in &room.tabs {
                if room.kind == RoomKind::Background
                    && !matches!(tab.runtime, TabRuntimeState::Running { .. })
                {
                    continue;
                }
                let old = previous
                    .remove(&tab.id)
                    .filter(|a| a.run_generation == tab.run_generation);
                let mut activity = old.clone().unwrap_or_else(|| blank_activity(tab));
                if let Some(pty) = live.get(&(tab.id.as_str(), tab.run_generation)) {
                    update_status(&mut activity, pty.agent_status.clone());
                    activity.foreground.clone_from(&pty.foreground);
                } else if matches!(tab.runtime, TabRuntimeState::StoppedAfterReboot) {
                    update_status(&mut activity, None);
                    activity.foreground = None;
                }
                if old.as_ref() != Some(&activity) {
                    changed.push((room.id.clone(), tab.id.clone()));
                }
                st.activities.insert(tab.id.clone(), activity);
            }
        }
        for (room, tab) in changed {
            Self::activity_changed(st, &room, &tab);
        }
    }

    async fn handle_pty_event_line(&self, term: &TermConn, line: &[u8]) {
        let Ok(note) = serde_json::from_slice::<Notification>(line) else {
            return;
        };
        if note.method == "pty.missed" {
            let seq = self.lock().seq;
            if let Ok::<PtyListResult, _>(live) = term.call("pty.list", json!({})).await {
                let mut st = self.lock();
                // A listing taken during mutations could predate their catalog commits.
                if st.seq == seq
                    && st.ops.spawning.is_empty()
                    && st.ops.in_flight_restarts.is_empty()
                    && st.ops.in_flight_closes.is_empty()
                {
                    self.reconcile(&mut st, &live.ptys);
                }
            }
            return;
        }
        if note.method != "pty.event" {
            return;
        }
        let Ok(event) = serde_json::from_value::<PtyEvent>(note.params["event"].clone()) else {
            return;
        };
        let (session_id, generation, pty_id) = match &event {
            PtyEvent::Exited {
                session_id,
                run_generation,
                pty_id,
                ..
            }
            | PtyEvent::CwdChanged {
                session_id,
                run_generation,
                pty_id,
                ..
            }
            | PtyEvent::TitleChanged {
                session_id,
                run_generation,
                pty_id,
                ..
            }
            | PtyEvent::ControlChanged {
                session_id,
                run_generation,
                pty_id,
                ..
            }
            | PtyEvent::AgentStatusChanged {
                session_id,
                run_generation,
                pty_id,
                ..
            }
            | PtyEvent::ForegroundChanged {
                session_id,
                run_generation,
                pty_id,
                ..
            }
            | PtyEvent::ExternalInput {
                session_id,
                run_generation,
                pty_id,
                ..
            } => (session_id, *run_generation, *pty_id),
            _ => return,
        };
        // A short-lived child can emit events before pty.spawn returns to the host.
        let waiting = self
            .lock()
            .ops
            .spawning
            .get(&(session_id.clone(), generation))
            .map(watch::Sender::subscribe);
        if let Some(mut waiting) = waiting {
            let _ = waiting.changed().await;
        }
        if let PtyEvent::Exited {
            exit_code, signal, ..
        } = event
        {
            let (guard, tab) = {
                let mut st = self.lock();
                let Ok((_, tab)) = find_tab(&st.catalog, session_id) else {
                    return;
                };
                if tab.pty_id != Some(pty_id)
                    || tab.run_generation != generation
                    || busy(&st.ops, session_id)
                {
                    return;
                }
                let tab = tab.clone();
                st.ops.in_flight_closes.insert(session_id.clone());
                (Guard(self, Reservation::Close(session_id.clone())), tab)
            };
            let result = PtyKillResult { exit_code, signal };
            // Explicit close handles its own event (and restart ignores the old generation's exit).
            let _ = self.finish_exit(term, &tab, result, false).await;
            drop(guard);
            return;
        }
        let mut st = self.lock();
        let Ok((r, t)) = tab_position(&st.catalog, session_id) else {
            return;
        };
        let room = &mut st.catalog.rooms[r];
        let tab = &mut room.tabs[t];
        if tab.pty_id != Some(pty_id) || tab.run_generation != generation {
            return;
        }
        match event {
            PtyEvent::AgentStatusChanged {
                session_id, status, ..
            } => {
                let room_id = room.id.clone();
                let Some(activity) = st.activities.get_mut(&session_id) else {
                    return;
                };
                update_status(activity, status);
                Self::activity_changed(&mut st, &room_id, &session_id);
            }
            PtyEvent::ForegroundChanged {
                session_id,
                foreground,
                ..
            } => {
                let room_id = room.id.clone();
                let Some(activity) = st.activities.get_mut(&session_id) else {
                    return;
                };
                activity.foreground = foreground;
                Self::activity_changed(&mut st, &room_id, &session_id);
            }
            PtyEvent::CwdChanged { cwd, .. } => {
                // The decoded path is used for spawning, not just display. Only sanitize its label.
                let path_changed = tab.last_cwd.as_deref() != Some(cwd.as_str());
                let mut name_changed = false;
                if t == 0
                    && let Some(base) = folder_basename(&cwd)
                    && room.auto_name != base
                {
                    room.auto_name = base;
                    name_changed = true;
                }
                if path_changed {
                    tab.last_cwd = Some(cwd);
                }
                self.publish_observed_if_changed(&mut st, path_changed || name_changed);
            }
            PtyEvent::TitleChanged { title, .. } => {
                let title = sanitize_display_text(&title, 160);
                let changed = tab.title != title;
                if changed {
                    tab.title = title;
                }
                self.publish_observed_if_changed(&mut st, changed);
            }
            PtyEvent::ControlChanged { controller, .. } => {
                st.clients.retain(|client| {
                    let mode = if controller.is_some_and(|c| Some(c.conn_id) == client.term_conn_id)
                    {
                        "control"
                    } else {
                        "readonly"
                    };
                    let note = Notification::new(
                        "host.tab_control_changed",
                        json!({
                            "tab_id": session_id, "pty_id": pty_id, "mode": mode,
                            "control_epoch": controller.map_or(0, |c| c.epoch),
                        }),
                    );
                    let bytes = serde_json::to_vec(&note).expect("serializable");
                    encode_frame(CONTROL_CHANNEL, FrameKind::Data, &bytes)
                        .is_ok_and(|frame| client.tx.send(frame).is_ok())
                });
            }
            PtyEvent::ExternalInput {
                session_id,
                byte_len,
                from_session_id,
                from_host_id,
                is_remote,
                ..
            } => {
                let room_id = room.id.clone();
                let Some(activity) = st.activities.get_mut(&session_id) else {
                    return;
                };
                activity.activity_seq += 1;
                Self::activity_changed(&mut st, &room_id, &session_id);
                Self::notify(
                    &mut st,
                    "host.external_input",
                    json!(HostEvent::ExternalInput {
                        session_id: session_id.clone(),
                        pty_id,
                        run_generation: generation,
                        byte_len,
                        from_session_id,
                        from_host_id,
                        is_remote,
                    }),
                );
                self.audit("external_input", &session_id);
            }
            _ => {}
        }
    }

    async fn finish_exit(
        &self,
        term: &TermConn,
        tab: &TabRecord,
        result: PtyKillResult,
        close: bool,
    ) -> Result<TabCloseResult, RpcError> {
        let parent_notice = {
            let mut st = self.lock();
            let (room, _) = find_tab(&st.catalog, &tab.id)?;
            let room_id = room.id.clone();
            let background = room.kind == RoomKind::Background;
            let parent = tab.parent_session_id.as_ref().filter(|id| {
                tab.parent_host_id
                    .as_ref()
                    .is_none_or(|host_id| host_id == &self.host_id)
                    && find_tab(&st.catalog, id).is_ok_and(|(_, parent)| parent.pty_id.is_some())
            });
            let parent_notice = parent.filter(|_| background).map(|id| SessionSendParams {
                to_session_id: id.clone(),
                from_session_id: tab.id.clone(),
                from_host_id: self.host_id.clone(),
                is_remote: false,
                payload: SessionMessagePayload::SessionExited {
                    session_id: tab.id.clone(),
                    room_id: room_id.clone(),
                    run_generation: tab.run_generation,
                    exit_code: result.exit_code,
                },
            });
            if background {
                // Activity ends now; Q2 still sends the parent notice before catalog deletion.
                st.activities.remove(&tab.id);
                Self::notify(
                    &mut st,
                    "host.session_exited",
                    json!(HostEvent::SessionExited {
                        session_id: tab.id.clone(),
                        room_id,
                        run_generation: tab.run_generation,
                        exit_code: result.exit_code,
                    }),
                );
            }
            parent_notice
        };
        if let Some(notice) = parent_notice
            && term
                .call_value("session.send", json!(notice))
                .await
                .is_err()
        {
            self.audit("parent_notice_failed", &tab.id);
        }
        let mut st = self.lock();
        let (r, t) = tab_position(&st.catalog, &tab.id)?;
        let room = &mut st.catalog.rooms[r];
        let background = room.kind == RoomKind::Background;
        let room_id = room.id.clone();
        let mut removed_room = false;
        if close || background {
            remove_room_tab(room, t);
            if room.tabs.is_empty() && !background {
                st.catalog.rooms.remove(r);
                removed_room = true;
            }
        } else {
            let tab = &mut room.tabs[t];
            tab.pty_id = None;
            tab.runtime = TabRuntimeState::Exited {
                status: exit_status(&result),
            };
        }
        if close || background {
            st.activities.remove(&tab.id);
        } else {
            record_exit_activity(&mut st, &tab.id, result.exit_code);
        }
        let saved = self.publish_observed(&mut st);
        if !close && !background {
            Self::activity_changed(&mut st, &room_id, &tab.id);
        }
        st.tokens.revoke(&tab.id, tab.run_generation);
        self.audit(
            if close {
                "session_close"
            } else {
                "session_exit"
            },
            &tab.id,
        );
        saved.map(|()| TabCloseResult { removed_room })
    }

    fn begin_request<T: DeserializeOwned>(
        &self,
        method: &'static str,
        scope: &str,
        key: Option<String>,
    ) -> Result<(Option<Guard<'_>>, Option<T>), RpcError> {
        let Some(key) = key else {
            return Ok((None, None));
        };
        let key = (method, scope.to_owned(), key);
        let mut st = self.lock();
        match st.ops.idempotency.get(&key) {
            Some(Some(result)) => Ok((
                None,
                Some(serde_json::from_value(result.clone()).map_err(internal)?),
            )),
            Some(None) => Err(conflict("idempotent request is already in flight")),
            None => {
                st.ops.idempotency.insert(key.clone(), None);
                Ok((Some(Guard(self, Reservation::Request(key))), None))
            }
        }
    }

    fn reserve_create(
        &self,
        target: &CreateTarget,
        new_tab_id: &str,
        visible_only: bool,
    ) -> Result<Guard<'_>, RpcError> {
        let mut st = self.lock();
        let id = target.id();
        let room = st.catalog.rooms.iter().find(|r| r.id == id);
        if matches!(
            target,
            CreateTarget::Existing(_) | CreateTarget::Split { .. }
        ) && room.is_none()
        {
            return Err(room_not_found());
        }
        if let Some(room) = room
            && visible_only
        {
            ensure_visible(room)?;
        }
        let count = st.ops.in_flight_creates.get(id).copied().unwrap_or(0);
        let limit = if room.is_some_and(|r| r.kind == RoomKind::Background)
            || matches!(target, CreateTarget::Background(_))
        {
            MAX_BACKGROUND_TABS
        } else {
            MAX_TABS_PER_ROOM
        };
        if room.map_or(0, |r| r.tabs.len()) + count >= limit {
            return Err(tab_limit());
        }
        if let Some(room) = room {
            target.split_layout(room, new_tab_id)?;
        }
        *st.ops.in_flight_creates.entry(id.to_owned()).or_default() += 1;
        Ok(Guard(self, Reservation::Create(id.to_owned())))
    }

    fn reserve_lifecycle(
        &self,
        id: &str,
        visible_only: bool,
        restart: bool,
    ) -> Result<(Guard<'_>, RoomId, TabRecord), RpcError> {
        let mut st = self.lock();
        let (room, tab) = find_tab(&st.catalog, id)?;
        if visible_only {
            ensure_visible(room)?;
        }
        if busy(&st.ops, id) {
            return Err(conflict("session lifecycle operation in flight"));
        }
        let (room_id, tab) = (room.id.clone(), tab.clone());
        let reservation = if restart {
            st.ops.in_flight_restarts.insert(id.to_owned());
            Reservation::Restart(id.to_owned())
        } else {
            st.ops.in_flight_closes.insert(id.to_owned());
            Reservation::Close(id.to_owned())
        };
        Ok((Guard(self, reservation), room_id, tab))
    }

    async fn kill(term: &TermConn, pty_id: u32) -> Result<PtyKillResult, RpcError> {
        match term.call("pty.kill", json!(PtyKillParams { pty_id })).await {
            Err(e) if e.data.kind == ErrorKind::NotFound => Ok(PtyKillResult {
                exit_code: None,
                signal: None,
            }),
            result => result,
        }
    }

    async fn compensate_spawn(&self, term: &TermConn, tab: &TabRecord) {
        self.lock().tokens.revoke(&tab.id, tab.run_generation);
        if Self::kill_spawn(term, tab, tab.pty_id).await.is_ok() {
            return;
        }
        // A disconnected Core may have handed the new PTY to its replacement.
        // Resolve by session/generation there; numeric PTY IDs may be reused.
        if let Ok(next) = TermConn::connect(&self.paths, "host-compensation").await
            && next.hello.server_id == self.host_id
            && Self::kill_spawn(&next, tab, None).await.is_ok()
        {
            return;
        }
        self.audit("spawn_compensation_failed", &tab.id);
    }

    async fn kill_spawn(
        term: &TermConn,
        tab: &TabRecord,
        pty_id: Option<u32>,
    ) -> Result<(), RpcError> {
        let pty_id = match pty_id {
            Some(id) => Some(id),
            None => {
                let live: PtyListResult = term.call("pty.list", json!({})).await?;
                live.ptys
                    .into_iter()
                    .find(|p| p.session_id == tab.id && p.run_generation == tab.run_generation)
                    .map(|p| p.pty_id)
            }
        };
        if let Some(pty_id) = pty_id {
            Self::kill(term, pty_id).await?;
        }
        Ok(())
    }

    async fn spawn(
        &self,
        term: &TermConn,
        room_id: &str,
        tab: &mut TabRecord,
        cwd: Option<String>,
        size: (u16, u16),
    ) -> Result<(), RpcError> {
        if let Some(dir) = &cwd
            && (!Path::new(dir).is_absolute() || !Path::new(dir).is_dir())
        {
            return Err(RpcError::invalid_params(
                "cwd must be an existing absolute directory on this host",
            ));
        }
        let (argv, mut env) = match &tab.executor {
            ExecutorSpec::DefaultShell => (None, Default::default()),
            ExecutorSpec::Command { argv, env } => {
                if argv.is_empty() {
                    return Err(RpcError::invalid_params("executor command argv is empty"));
                }
                (Some(argv.clone()), env.clone())
            }
        };
        let token = self.lock().tokens.issue(&tab.id, tab.run_generation)?;
        env.extend(SESSION_ENV_KEYS.into_iter().map(str::to_owned).zip([
            tab.id.clone(),
            room_id.to_owned(),
            self.host_id.clone(),
            self.paths.host_sock.to_string_lossy().into_owned(),
            self.paths.terminal_sock.to_string_lossy().into_owned(),
            token,
        ]));
        let result: Result<PtySpawnResult, RpcError> = term
            .call(
                "pty.spawn",
                json!(PtySpawnParams {
                    session_id: tab.id.clone(),
                    run_generation: tab.run_generation,
                    cols: size.0,
                    rows: size.1,
                    cwd,
                    argv,
                    env: Some(env),
                }),
            )
            .await;
        match result {
            Ok(spawned) => {
                tab.pty_id = Some(spawned.pty_id);
                tab.runtime = TabRuntimeState::Running {
                    pty_id: spawned.pty_id,
                    pid: spawned.pid,
                };
                Ok(())
            }
            Err(e) => {
                self.compensate_spawn(term, tab).await;
                Err(e)
            }
        }
    }

    fn spawning(&self, tab: &TabRecord) -> Guard<'_> {
        let (tx, _) = watch::channel(());
        self.lock()
            .ops
            .spawning
            .insert((tab.id.clone(), tab.run_generation), tx);
        Guard(self, Reservation::Spawn(tab.id.clone(), tab.run_generation))
    }

    async fn create(
        &self,
        target: CreateTarget,
        mut tab: TabRecord,
        size: (u16, u16),
        visible_only: bool,
    ) -> Result<RoomRecord, RpcError> {
        let _create = self.reserve_create(&target, &tab.id, visible_only)?;
        let event = if matches!(target, CreateTarget::Split { .. }) {
            "tab_split"
        } else {
            "session_create"
        };
        let term = self.term()?;
        let _spawning = self.spawning(&tab);
        let cwd = tab.space_cwd.clone();
        self.spawn(&term, target.id(), &mut tab, cwd, size).await?;
        let result = (|| {
            let mut st = self.lock();
            ensure_current_term(&st, &term)?;
            let mut catalog = st.catalog.clone();
            let room = if let Some(room) = catalog.rooms.iter_mut().find(|r| r.id == target.id()) {
                // The anchor or its depth may have changed while spawning.
                if let Some(layout) = target.split_layout(room, &tab.id)? {
                    room.layout = Some(layout);
                    room.tabs.push(tab.clone());
                } else {
                    append_room_tab(room, tab.clone());
                }
                room.clone()
            } else {
                let mut room = target.new_room(tab.space_cwd.as_deref())?;
                room.tabs.push(tab.clone());
                catalog.rooms.push(room.clone());
                room
            };
            self.commit(&mut st, catalog)?;
            st.activities.insert(tab.id.clone(), blank_activity(&tab));
            Self::activity_changed(&mut st, &room.id, &tab.id);
            Ok(room)
        })();
        if result.is_err() {
            self.compensate_spawn(&term, &tab).await;
        } else {
            self.audit(event, &tab.id);
        }
        result
    }

    pub async fn room_create(
        &self,
        params: RoomCreateParams,
    ) -> Result<RoomCreateResult, RpcError> {
        validate_launch(&params.executor, params.agent_profile.as_deref())?;
        let (request, cached) = self.begin_request("room_create", "", params.request_id)?;
        if let Some(result) = cached {
            return Ok(result);
        }
        let target = CreateTarget::Standard {
            id: generate_room_id().map_err(internal)?,
            name: params.name,
        };
        let tab = new_tab(
            params.space_cwd,
            params.executor,
            None,
            params.agent_profile,
        )?;
        let room = self
            .create(target, tab, (params.cols, params.rows), true)
            .await?;
        let result = RoomCreateResult { room };
        if let Some(request) = request {
            request.cache(json!(&result));
        }
        Ok(result)
    }

    pub fn room_rename(&self, params: RoomRenameParams) -> Result<RoomRenameResult, RpcError> {
        let mut st = self.lock();
        let mut catalog = st.catalog.clone();
        let room = catalog
            .rooms
            .iter_mut()
            .find(|r| r.id == params.room_id)
            .ok_or_else(room_not_found)?;
        ensure_visible(room)?;
        room.name = display_name(params.name);
        let room = room.clone();
        self.commit(&mut st, catalog)?;
        self.audit("room_rename", &room.id);
        Ok(RoomRenameResult { room })
    }

    pub fn room_set_layout(
        &self,
        params: RoomSetLayoutParams,
    ) -> Result<RoomSetLayoutResult, RpcError> {
        let mut st = self.lock();
        let mut catalog = st.catalog.clone();
        let room = catalog
            .rooms
            .iter_mut()
            .find(|r| r.id == params.room_id)
            .ok_or_else(room_not_found)?;
        ensure_visible(room)?;
        if let Some(layout) = &params.layout {
            layout
                .validate_for_tabs(&room.tabs)
                .map_err(RpcError::invalid_params)?;
        }
        room.layout = params.layout;
        normalize_layout(room);
        let room = room.clone();
        self.commit(&mut st, catalog)?;
        self.audit("room_set_layout", &room.id);
        Ok(RoomSetLayoutResult { room })
    }

    fn create_target(&self, room_id: Option<RoomId>) -> Result<CreateTarget, RpcError> {
        Ok(match room_id {
            Some(id) => CreateTarget::Existing(id),
            None => CreateTarget::Background(
                background_room_id(self.host_id.get(..12).unwrap_or(&self.host_id))
                    .map_err(internal)?,
            ),
        })
    }

    pub async fn tab_create(&self, params: TabCreateParams) -> Result<TabCreateResult, RpcError> {
        self.tab_create_with_visibility(params, false).await
    }

    pub async fn tab_create_with_visibility(
        &self,
        params: TabCreateParams,
        visible_only: bool,
    ) -> Result<TabCreateResult, RpcError> {
        if visible_only && (params.background || params.room_id.is_none()) {
            return Err(RpcError::permission_denied(
                "plugins cannot create background tabs",
            ));
        }
        if params.background && params.room_id.is_some() {
            return Err(RpcError::invalid_params(
                "background cannot be combined with room_id",
            ));
        }
        validate_launch(&params.executor, params.agent_profile.as_deref())?;
        let scope = if visible_only { "plugin" } else { "operator" };
        let (request, cached) = self.begin_request("tab_create", scope, params.request_id)?;
        if let Some(result) = cached {
            return Ok(result);
        }
        let explicit_room = params.room_id.is_some();
        let target = self.create_target(params.room_id)?;
        let tab = new_tab(
            params.space_cwd,
            params.executor,
            params.title,
            params.agent_profile,
        )?;
        let mut room = self
            .create(target, tab, (params.cols, params.rows), explicit_room)
            .await?;
        let result = TabCreateResult {
            tab: room.tabs.pop().expect("created tab"),
        };
        if let Some(request) = request {
            request.cache(json!(&result));
        }
        Ok(result)
    }

    pub async fn tab_split(&self, params: TabSplitParams) -> Result<TabSplitResult, RpcError> {
        let ratio_bp = params.ratio_bp.unwrap_or(DEFAULT_SPLIT_RATIO_BP);
        if !(MIN_SPLIT_RATIO_BP..=MAX_SPLIT_RATIO_BP).contains(&ratio_bp) {
            return Err(RpcError::invalid_params(format!(
                "ratio_bp must be in {MIN_SPLIT_RATIO_BP}..={MAX_SPLIT_RATIO_BP}"
            )));
        }
        validate_launch(&params.executor, params.agent_profile.as_deref())?;
        let (request, cached) = self.begin_request("tab_split", "operator", params.request_id)?;
        if let Some(result) = cached {
            return Ok(result);
        }
        let (room_id, cwd) = {
            let st = self.lock();
            let (room, anchor) = find_tab(&st.catalog, &params.anchor_tab_id)?;
            ensure_visible(room)?;
            let cwd = params
                .space_cwd
                .or_else(|| anchor.last_cwd.clone().or_else(|| anchor.space_cwd.clone()));
            (room.id.clone(), cwd)
        };
        let target = CreateTarget::Split {
            room_id,
            anchor: params.anchor_tab_id,
            axis: params.axis,
            ratio_bp,
        };
        let tab = new_tab(cwd, params.executor, params.title, params.agent_profile)?;
        let room = self
            .create(target, tab, (params.cols, params.rows), true)
            .await?;
        let tab = room.tabs.last().expect("created tab").clone();
        let result = TabSplitResult { room, tab };
        if let Some(request) = request {
            request.cache(json!(&result));
        }
        Ok(result)
    }

    pub async fn tab_screen(
        &self,
        params: TabScreenParams,
    ) -> Result<SessionScreenResult, RpcError> {
        self.read_screen(SessionReadScreenParams {
            session_id: params.tab_id,
            lines: params.lines,
            max_bytes: params.max_bytes,
        })
        .await
    }

    pub async fn tab_write(&self, params: TabWriteParams) -> Result<SessionWriteResult, RpcError> {
        self.write_input(
            SessionWriteInputParams {
                session_id: params.tab_id,
                data_b64: params.data_b64,
                expect_run_generation: params.expect_run_generation,
                from_session_id: "operator".into(),
                from_host_id: self.host_id.clone(),
                is_remote: false,
            },
            "tab_write",
        )
        .await
    }

    pub async fn tab_msg_send(&self, params: TabMsgSendParams) -> Result<MsgSendResult, RpcError> {
        if decode_base64_bounded(&params.data_b64, 65536)
            .map_err(|e| RpcError::invalid_params(e.to_string()))?
            .is_empty()
        {
            return Err(RpcError::invalid_params("message must not be empty"));
        }
        self.audit("tab_msg_send", "operator");
        self.term()?
            .call(
                "session.send",
                json!(SessionSendParams {
                    to_session_id: params.to_session_id,
                    from_session_id: "operator".into(),
                    from_host_id: self.host_id.clone(),
                    is_remote: false,
                    payload: SessionMessagePayload::Message {
                        data_b64: params.data_b64,
                    },
                }),
            )
            .await
    }

    pub fn tab_move(&self, params: TabMoveParams) -> Result<SessionMoveResult, RpcError> {
        let result = self.move_tab(&params.tab_id, params.target)?;
        self.audit("tab_move", &params.tab_id);
        Ok(result)
    }

    pub fn lookup_tab_any(&self, tab_id: &str) -> Result<TabRecord, RpcError> {
        let st = self.lock();
        let (_, tab) = find_tab(&st.catalog, tab_id)?;
        Ok(tab.clone())
    }

    pub async fn tab_close_with_visibility(
        &self,
        params: TabCloseParams,
        visible_only: bool,
    ) -> Result<TabCloseResult, RpcError> {
        // A plugin must not receive a cached result for an operator's hidden tab.
        let scope = if visible_only { "plugin" } else { "operator" };
        let (request, cached) = self.begin_request("tab_close", scope, params.request_id)?;
        if let Some(result) = cached {
            return Ok(result);
        }
        let result = self.close(&params.tab_id, visible_only).await?;
        if let Some(request) = request {
            request.cache(json!(&result));
        }
        Ok(result)
    }

    async fn close(&self, id: &str, visible_only: bool) -> Result<TabCloseResult, RpcError> {
        let (_close, _, tab) = self.reserve_lifecycle(id, visible_only, false)?;
        let term = self.lock().term.clone();
        let result = match tab.pty_id {
            Some(pty_id) => Self::kill(&term, pty_id).await?,
            None => PtyKillResult {
                exit_code: None,
                signal: None,
            },
        };
        self.finish_exit(&term, &tab, result, true).await
    }

    pub async fn tab_restart_with_visibility(
        &self,
        params: TabRestartParams,
        visible_only: bool,
    ) -> Result<TabRestartResult, RpcError> {
        self.restart(
            &params.tab_id,
            visible_only,
            Some((params.cols, params.rows)),
        )
        .await
        .map(|(_, tab)| TabRestartResult { tab })
    }

    async fn restart(
        &self,
        id: &str,
        visible_only: bool,
        size: Option<(u16, u16)>,
    ) -> Result<(RoomRecord, TabRecord), RpcError> {
        let (_restart, room_id, old) = self.reserve_lifecycle(id, visible_only, true)?;
        let term = self.term()?;
        let mut tab = old.clone();
        tab.run_generation = tab
            .run_generation
            .checked_add(1)
            .ok_or_else(|| internal("run generation exhausted"))?;
        tab.pty_id = None;
        let size = if let (None, Some(pty_id)) = (size, old.pty_id) {
            let live: PtyListResult = term.call("pty.list", json!({})).await?;
            live.ptys
                .iter()
                .find(|p| p.pty_id == pty_id)
                .map(|p| (p.cols, p.rows))
        } else {
            size
        }
        .unwrap_or((80, 24));
        if let Some(pty_id) = old.pty_id {
            let result = Self::kill(&term, pty_id).await?;
            let mut st = self.lock();
            let (r, t) = tab_position(&st.catalog, id)?;
            let room = &mut st.catalog.rooms[r];
            let background = room.kind == RoomKind::Background;
            let old_tab = &mut room.tabs[t];
            old_tab.pty_id = None;
            old_tab.runtime = TabRuntimeState::Exited {
                status: exit_status(&result),
            };
            st.tokens.revoke(id, old.run_generation);
            if background {
                st.activities.remove(id);
            } else {
                record_exit_activity(&mut st, id, result.exit_code);
            }
            let _ = self.publish_observed(&mut st);
            if !background {
                Self::activity_changed(&mut st, &room_id, id);
            }
        }
        let _spawning = self.spawning(&tab);
        let cwd = tab.space_cwd.clone().or(tab.last_cwd.clone());
        self.spawn(&term, &room_id, &mut tab, cwd, size).await?;
        let result = (|| {
            let mut st = self.lock();
            ensure_current_term(&st, &term)?;
            let mut catalog = st.catalog.clone();
            let (r, t) = tab_position(&catalog, id)?;
            catalog.rooms[r].tabs[t] = tab.clone();
            let room = catalog.rooms[r].clone();
            self.commit(&mut st, catalog)?;
            st.activities.insert(tab.id.clone(), blank_activity(&tab));
            Self::activity_changed(&mut st, &room.id, id);
            Ok((room, tab.clone()))
        })();
        if result.is_err() {
            self.compensate_spawn(&term, &tab).await;
        } else {
            self.audit("session_restart", id);
        }
        result
    }

    pub fn authenticate_local(&self, params: &Value) -> Result<AuthenticatedCaller, RpcError> {
        let token = params["session_token"]
            .as_str()
            .ok_or_else(|| RpcError::token_invalid("session token required"))?;
        self.lock().tokens.verify(token)
    }

    pub async fn session_rpc(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let local = self.authenticate_local(&params)?;
        let caller = Caller {
            session_id: local.session_id,
            host_id: self.host_id.clone(),
            is_remote: false,
        };
        self.relay.route(self, method, params, caller).await
    }

    pub async fn session_local(
        &self,
        method: &str,
        mut params: Value,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        if caller.is_remote {
            // These public parameter types require the token field; relay auth is already out of band.
            params["session_token"] = json!("");
        }
        match method {
            "host.session_create" => {
                let params: SessionCreateParams = parse(params)?;
                let executor = params.executor.unwrap_or_default();
                validate_launch(&executor, params.agent_profile.as_deref())?;
                if caller.is_remote
                    && (caller.host_id.is_empty()
                        || caller.host_id.chars().count() > 128
                        || caller.host_id.chars().any(char::is_control))
                {
                    return Err(RpcError::invalid_params("invalid parent host ID"));
                }
                let (request, cached) = self.begin_request(
                    "session_create",
                    &format!("{}:{}", caller.host_id, caller.session_id),
                    params.idempotency_key,
                )?;
                if let Some(result) = cached {
                    return Ok(result);
                }
                let target = self.create_target(params.room_id)?;
                let mut tab = new_tab(params.cwd, executor, params.title, params.agent_profile)?;
                tab.parent_session_id = Some(caller.session_id.clone());
                tab.parent_host_id = caller.is_remote.then(|| caller.host_id.clone());
                let room = self.create(target, tab, (80, 24), false).await?;
                let result = json!(SessionCreateResult {
                    session: self.summary(&room, room.tabs.last().expect("created session"))
                });
                if let Some(request) = request {
                    request.cache(result.clone());
                }
                Ok(result)
            }
            "host.session_list" => {
                let p: SessionListParams = parse(params)?;
                let st = self.lock();
                let sessions = st
                    .catalog
                    .rooms
                    .iter()
                    .filter(|room| {
                        p.include_hidden.unwrap_or(false)
                            || room.visibility == RoomVisibility::Visible
                    })
                    .filter(|room| p.room_id.as_ref().is_none_or(|id| id == &room.id))
                    .flat_map(|room| room.tabs.iter().map(|tab| self.summary(room, tab)))
                    .collect();
                Ok(json!(SessionListResult {
                    sessions,
                    errors: Vec::new()
                }))
            }
            "host.session_move" => {
                let p: SessionMoveParams = parse(params)?;
                self.session_move(&p.session_id, p.target).map(|r| json!(r))
            }
            "host.session_close" => {
                let p: SessionCloseParams = parse(params)?;
                self.close(&p.session_id, false).await.map(|r| json!(r))
            }
            "host.session_restart" => {
                let p: SessionRestartParams = parse(params)?;
                let (room, tab) = self.restart(&p.session_id, false, None).await?;
                Ok(json!(SessionRestartResult {
                    session: self.summary(&room, &tab)
                }))
            }
            "host.session_screen" => {
                let p: SessionScreenParams = parse(params)?;
                self.read_screen(SessionReadScreenParams {
                    session_id: p.session_id,
                    lines: p.lines,
                    max_bytes: p.max_bytes,
                })
                .await
                .map(|r| json!(r))
            }
            "host.session_write" => {
                let p: SessionWriteParams = parse(params)?;
                self.write_input(
                    SessionWriteInputParams {
                        session_id: p.session_id,
                        data_b64: p.data_b64,
                        expect_run_generation: p.expect_run_generation,
                        from_session_id: caller.session_id.clone(),
                        from_host_id: caller.host_id.clone(),
                        is_remote: caller.is_remote,
                    },
                    "session_write",
                )
                .await
                .map(|r| json!(r))
            }
            "host.msg_send" => {
                let p: MsgSendParams = parse(params)?;
                self.audit("msg_send", &caller.session_id);
                self.term()?
                    .call_value(
                        "session.send",
                        json!(SessionSendParams {
                            to_session_id: p.to_session_id,
                            from_session_id: caller.session_id.clone(),
                            from_host_id: caller.host_id.clone(),
                            is_remote: caller.is_remote,
                            payload: SessionMessagePayload::Message {
                                data_b64: p.data_b64
                            },
                        }),
                    )
                    .await
            }
            "host.list_hosts" => Ok(json!(self.relay.summaries(&self.host_id))),
            _ => Err(RpcError::not_found("unknown session method")),
        }
    }

    async fn read_screen(
        &self,
        params: SessionReadScreenParams,
    ) -> Result<SessionScreenResult, RpcError> {
        if !(1..=2000).contains(&params.lines) || !(1..=262144).contains(&params.max_bytes) {
            return Err(RpcError::invalid_params(
                "screen bounds: lines 1..=2000, max_bytes 1..=262144",
            ));
        }
        self.term()?
            .call("session.read_screen", json!(params))
            .await
    }

    async fn write_input(
        &self,
        params: SessionWriteInputParams,
        audit_event: &'static str,
    ) -> Result<SessionWriteResult, RpcError> {
        if decode_base64_bounded(&params.data_b64, 65536)
            .map_err(|e| RpcError::invalid_params(e.to_string()))?
            .is_empty()
        {
            return Err(RpcError::invalid_params("input must not be empty"));
        }
        self.audit(audit_event, &params.from_session_id);
        self.term()?
            .call("session.write_input", json!(params))
            .await
    }

    fn summary(&self, room: &RoomRecord, tab: &TabRecord) -> SessionSummary {
        SessionSummary {
            session_id: tab.id.clone(),
            room_id: room.id.clone(),
            room_kind: room.kind,
            visibility: room.visibility,
            parent_session_id: tab.parent_session_id.clone(),
            host_id: self.host_id.clone(),
            title: tab.title.clone(),
            run_generation: tab.run_generation,
            runtime: tab.runtime.clone(),
        }
    }

    fn session_move(
        &self,
        id: &str,
        target: SessionMoveTarget,
    ) -> Result<SessionMoveResult, RpcError> {
        let result = self.move_tab(id, target)?;
        self.audit("session_move", id);
        Ok(result)
    }

    fn move_tab(&self, id: &str, target: SessionMoveTarget) -> Result<SessionMoveResult, RpcError> {
        let mut st = self.lock();
        if busy(&st.ops, id) {
            return Err(conflict("session lifecycle operation in flight"));
        }
        let mut catalog = st.catalog.clone();
        let (src, tab_idx) = tab_position(&catalog, id)?;
        if catalog.rooms[src].kind != RoomKind::Background {
            return Err(RpcError::permission_denied(
                "only background sessions can be promoted",
            ));
        }
        let dst = match target {
            SessionMoveTarget::ExistingRoom { room_id } => {
                let idx = catalog
                    .rooms
                    .iter()
                    .position(|r| r.id == room_id)
                    .ok_or_else(room_not_found)?;
                let room = &catalog.rooms[idx];
                ensure_visible(room)?;
                if room.tabs.len() + st.ops.in_flight_creates.get(&room.id).copied().unwrap_or(0)
                    >= MAX_TABS_PER_ROOM
                {
                    return Err(tab_limit());
                }
                idx
            }
            SessionMoveTarget::NewRoom { name } => {
                let cwd = catalog.rooms[src].tabs[tab_idx].space_cwd.as_deref();
                catalog.rooms.push(standard_room(
                    generate_room_id().map_err(internal)?,
                    name,
                    cwd,
                ));
                catalog.rooms.len() - 1
            }
        };
        // Room membership is the live catalog metadata; an existing process's environment is immutable.
        let tab = remove_room_tab(&mut catalog.rooms[src], tab_idx);
        append_room_tab(&mut catalog.rooms[dst], tab.clone());
        let session = self.summary(&catalog.rooms[dst], &tab);
        self.commit(&mut st, catalog)?;
        st.activities
            .entry(tab.id.clone())
            .or_insert_with(|| blank_activity(&tab));
        Self::activity_changed(&mut st, &session.room_id, id);
        Ok(SessionMoveResult { session })
    }

    pub fn plugin_catalog(&self) -> Catalog {
        let mut catalog = self.lock().catalog.clone();
        catalog
            .rooms
            .retain(|r| r.visibility == RoomVisibility::Visible);
        catalog
    }

    pub fn plugin_pty_visible(&self, pty: &PtyInfo) -> bool {
        // Unmanaged v2 PTYs have no catalog visibility to prove they are safe to expose.
        pty.session_id.is_empty()
            || find_tab(&self.lock().catalog, &pty.session_id).is_ok_and(|(room, tab)| {
                room.visibility == RoomVisibility::Visible
                    && tab.pty_id == Some(pty.pty_id)
                    && tab.run_generation == pty.run_generation
            })
    }

    pub fn audit(&self, event: &'static str, id: &str) {
        let mut file = self.audit.lock().unwrap_or_else(|p| p.into_inner());
        let _ = writeln!(
            file,
            "{} {event} {}",
            paths::unix_secs(),
            sanitize_display_text(id, 80)
        );
    }
}

#[cfg(unix)]
fn open_audit(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(windows)]
fn open_audit(path: &Path) -> io::Result<File> {
    match ddg_agents_os::fs::create_private_file(path) {
        Ok(file) => Ok(file),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            std::fs::OpenOptions::new().append(true).open(path)
        }
        Err(e) => Err(e),
    }
}

fn blank_activity(tab: &TabRecord) -> TabActivity {
    TabActivity {
        tab_id: tab.id.clone(),
        run_generation: tab.run_generation,
        agent_status: None,
        foreground: None,
        activity_seq: 0,
        seen_seq: 0,
    }
}

fn update_status(activity: &mut TabActivity, status: Option<AgentStatus>) {
    let state = |value: Option<&AgentStatus>| value.map_or(AgentState::Idle, |s| s.state);
    if state(activity.agent_status.as_ref()) != state(status.as_ref()) {
        activity.activity_seq += 1;
    }
    activity.agent_status = status;
}

fn record_exit_activity(st: &mut HostState, id: &str, exit_code: Option<i32>) {
    let activity = st.activities.get_mut(id).expect("running tab activity");
    activity.activity_seq += 1;
    activity.foreground = None;
    activity.agent_status = Some(AgentStatus {
        state: if exit_code == Some(0) {
            AgentState::Done
        } else {
            AgentState::Error
        },
        kind: None,
        progress: None,
        app: None,
        title: None,
        msg: None,
        records: Vec::new(),
        status_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
    });
}

fn validate_launch(executor: &ExecutorSpec, profile: Option<&str>) -> Result<(), RpcError> {
    if let ExecutorSpec::Command { env, .. } = executor
        && env.iter().any(|(key, value)| {
            key.is_empty()
                || key.contains(['=', '\0'])
                || is_reserved_env(key)
                || value.contains('\0')
        })
    {
        return Err(RpcError::invalid_params(
            "invalid or reserved executor environment variable",
        ));
    }
    if let Some(profile) = profile {
        validate_agent_profile_slug(profile).map_err(RpcError::invalid_params)?;
    }
    Ok(())
}

fn new_tab(
    cwd: Option<String>,
    executor: ExecutorSpec,
    title: Option<String>,
    agent_profile: Option<String>,
) -> Result<TabRecord, RpcError> {
    Ok(TabRecord {
        id: generate_session_id().map_err(internal)?,
        run_generation: 1,
        agent_profile,
        parent_session_id: None,
        parent_host_id: None,
        space_cwd: cwd.clone(),
        last_cwd: cwd,
        title: sanitize_display_text(title.as_deref().unwrap_or_default(), 160),
        executor,
        pty_id: None,
        runtime: TabRuntimeState::StoppedAfterReboot,
    })
}

fn standard_room(id: String, name: Option<String>, cwd: Option<&str>) -> RoomRecord {
    RoomRecord {
        id,
        kind: RoomKind::Standard,
        visibility: RoomVisibility::Visible,
        name: display_name(name),
        auto_name: cwd
            .and_then(folder_basename)
            .unwrap_or_else(|| "Terminal".into()),
        tabs: Vec::new(),
        layout: None,
    }
}

fn append_room_tab(room: &mut RoomRecord, tab: TabRecord) {
    if let Some(layout) = &mut room.layout {
        layout.append_tab(tab.id.clone());
    }
    room.tabs.push(tab);
}

fn remove_room_tab(room: &mut RoomRecord, index: usize) -> TabRecord {
    let tab = room.tabs.remove(index);
    if let Some(layout) = &mut room.layout {
        layout.remove_tab(&tab.id);
        normalize_layout(room);
    }
    tab
}

fn normalize_layout(room: &mut RoomRecord) {
    if room.layout.as_ref().is_some_and(|layout| {
        layout.groups.len() == room.tabs.len()
            && layout
                .groups
                .iter()
                .zip(&room.tabs)
                .all(|(group, tab)| matches!(group, PaneNode::Leaf { tab_id } if tab_id == &tab.id))
    }) {
        room.layout = None;
    }
}

fn find_tab<'a>(
    catalog: &'a Catalog,
    id: &str,
) -> Result<(&'a RoomRecord, &'a TabRecord), RpcError> {
    let (r, t) = tab_position(catalog, id)?;
    Ok((&catalog.rooms[r], &catalog.rooms[r].tabs[t]))
}

fn tab_position(catalog: &Catalog, id: &str) -> Result<(usize, usize), RpcError> {
    catalog
        .rooms
        .iter()
        .enumerate()
        .find_map(|(r, room)| room.tabs.iter().position(|t| t.id == id).map(|t| (r, t)))
        .ok_or_else(|| RpcError::session_not_found("session not found"))
}

fn busy(ops: &Operations, id: &str) -> bool {
    ops.in_flight_restarts.contains(id) || ops.in_flight_closes.contains(id)
}

fn ensure_visible(room: &RoomRecord) -> Result<(), RpcError> {
    if room.visibility == RoomVisibility::Visible {
        Ok(())
    } else {
        Err(RpcError::permission_denied(
            "background room unavailable to this method",
        ))
    }
}

fn ensure_current_term(st: &HostState, term: &Arc<TermConn>) -> Result<(), RpcError> {
    if Arc::ptr_eq(term, &st.term) && term.is_alive() {
        Ok(())
    } else {
        Err(terminal_unavailable())
    }
}

fn exit_status(result: &PtyKillResult) -> String {
    match (result.exit_code, result.signal) {
        (Some(0), None) => "Exited cleanly".into(),
        (Some(code), _) => format!("Exit code {code}"),
        (None, Some(sig)) => format!("Terminated by signal {sig}"),
        (None, None) => "Exited".into(),
    }
}

#[test]
fn restored_exit_status_describes_code_signal_or_unknown_result() {
    for (exit_code, signal, expected) in [
        (Some(0), None, "Exited cleanly"),
        (Some(7), None, "Exit code 7"),
        (None, Some(15), "Terminated by signal 15"),
        (None, None, "Exited"),
    ] {
        assert_eq!(exit_status(&PtyKillResult { exit_code, signal }), expected);
    }
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::invalid_params(e.to_string()))
}

fn internal(error: impl std::fmt::Display) -> RpcError {
    RpcError::new(ErrorKind::Internal, error.to_string(), None)
}

fn conflict(message: &str) -> RpcError {
    RpcError::new(ErrorKind::Conflict, message, None)
}

fn terminal_unavailable() -> RpcError {
    RpcError::new(
        ErrorKind::Unavailable,
        "terminal connection unavailable",
        None,
    )
}

fn room_not_found() -> RpcError {
    RpcError::room_not_found("room not found")
}
fn tab_limit() -> RpcError {
    RpcError::room_tab_limit("room tab limit reached")
}

#[cfg(all(test, unix))]
pub(crate) mod tests;
