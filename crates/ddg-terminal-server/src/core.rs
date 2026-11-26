//! Core runtime: adopts/spawns the Holder, maintains per-PTY Ghostty VT 1.0.1
//! state, enforces single-controller leases, fans out credit-controlled viewer
//! streams, and broadcasts lifecycle/metadata events.

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, unix))]
mod core_activity_and_lifecycle {
    use super::*;
    use ddg_agents_proto::terminal::{AgentState, BlockedKind};

    fn recovered_core(features: Vec<String>, tail: &[u8]) -> (Arc<Core>, mpsc::Receiver<Vec<u8>>) {
        let (mut core, mut holder) = LocalStream::pair().unwrap();
        let welcome = hproto::HolderWelcome {
            version: 1,
            instance: "test".into(),
            pid: 1,
            features,
            ptys: vec![hproto::HeldPty {
                pty_id: 1,
                pid: 100,
                cols: 80,
                rows: 24,
                exit: None,
                raw_offset: 0,
                session_id: "session".into(),
                run_generation: 7,
            }],
        };
        holder
            .write_all(&hproto::encode_json(hproto::WELCOME, 0, &welcome))
            .unwrap();
        holder
            .write_all(&hproto::encode_frame(hproto::TAIL, 1, tail))
            .unwrap();
        let (ptys, next_id, fg_pgrp, instance) = recover_ptys(&mut core).unwrap();
        holder.set_nonblocking(true).unwrap();
        assert_eq!(
            hproto::read_frame(&mut holder).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let (tx, rx) = mpsc::channel();
        let core = Core::new(
            ServerIdentity {
                server_id: "test".into(),
                boot_generation: 1,
            },
            tx,
            ptys,
            next_id,
            fg_pgrp,
            instance,
        );
        (Arc::new(core), rx)
    }

    #[test]
    fn status_transitions_and_trailing_edge() {
        let now = Instant::now();
        let (core, _) = recovered_core(vec![], b"");
        let conn = Arc::new(Conn::new(1));
        let channel = core.subscribe(&conn).channel.unwrap();
        conn.with_outbox(|out| out.resume(channel));
        let event = || {
            let frame = conn.next_frame().expect("expected event");
            let notice: serde_json::Value = serde_json::from_slice(&frame[9..]).unwrap();
            serde_json::from_value::<PtyEvent>(notice["params"]["event"].clone()).unwrap()
        };
        let status = || match event() {
            PtyEvent::AgentStatusChanged {
                pty_id: 1,
                run_generation: 7,
                status,
                ..
            } => status,
            other => panic!("unexpected event: {other:?}"),
        };
        core.handle_output_at(1, b"\x1b]7501;state=working\x07", now);
        assert_eq!(status().unwrap().state, AgentState::Working);
        for i in 1..=5 {
            let at = now + Duration::from_millis(i * 30);
            core.handle_output_at(
                1,
                format!("\x1b]7501;state=working:progress={i}\x07").as_bytes(),
                at,
            );
            core.flush_status_updates(at);
            assert!(conn.next_frame().is_none());
        }
        let last = now + Duration::from_millis(150);
        core.flush_status_updates(last + Duration::from_millis(199));
        assert!(conn.next_frame().is_none());
        assert_eq!(
            core.list_ptys()[0].agent_status.as_ref().unwrap().progress,
            Some(5)
        );
        core.flush_status_updates(last + Duration::from_millis(200));
        assert_eq!(status().unwrap().progress, Some(5));
        core.flush_status_updates(last + Duration::from_millis(201));
        assert!(conn.next_frame().is_none());

        // A state transition, including a non-winning record, cancels any pending update.
        core.handle_output_at(1, b"\x1b]7501;state=working:progress=6\x07", last);
        assert!(conn.next_frame().is_none());
        core.handle_output_at(1, b"\x1b]7501;state=blocked:kind=permission\x07", last);
        assert_eq!(status().unwrap().kind, Some(BlockedKind::Permission));
        for frame in [
            b"\x1b]7501;state=blocked:kind=question\x07".as_slice(),
            b"\x1b]7501;id=sub:state=working\x07",
            b"\x1b]7501;id=sub:state=done\x07",
            b"\x1b]7501;id=sub:state=clear\x07",
        ] {
            core.handle_output_at(1, frame, last);
            assert_eq!(status().unwrap().kind, Some(BlockedKind::Question));
        }
        core.handle_output_at(1, b"\x1b]7501;state=clear\x07", last);
        assert!(status().is_none());
        core.flush_status_updates(last + Duration::from_secs(1));
        assert!(conn.next_frame().is_none());
    }

    #[test]
    fn replay_fifo_and_poll_negotiation() {
        let tail = b"old\x1b[6n\x1b]7501;?\x07\x1b]7501;state=blocked:kind=auth\x07";
        for features in [vec![], vec!["fg_pgrp_v1".into()]] {
            let supported = !features.is_empty();
            let (core, holder) = recovered_core(features, tail);
            assert_eq!(
                core.list_ptys()[0].agent_status.as_ref().unwrap().kind,
                Some(BlockedKind::Auth)
            );
            core.query_foreground();
            assert_eq!(
                holder.try_recv().ok(),
                supported.then(|| hproto::encode_frame(hproto::QUERY_FG_PGRP, 0, &[]))
            );
            core.handle_output(1, b"\x1b[6n\x1b]7501;?\x07x\x1b[6n");
            let bytes = holder.try_recv().unwrap();
            assert_eq!(
                hproto::read_frame(&mut bytes.as_slice()).unwrap().unwrap(),
                (
                    hproto::INPUT,
                    1,
                    b"\x1b[1;4R\x1b]7501;?\x1b\\\x1b[1;5R".to_vec()
                )
            );
            core.handle_exited(
                1,
                hproto::HolderExit {
                    code: Some(0),
                    signal: None,
                },
            );
            assert_eq!(
                holder.try_recv().unwrap(),
                hproto::encode_frame(hproto::RELEASE, 1, &[])
            );
            core.query_foreground();
            assert!(holder.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn goodbye_waits_for_bye() {
        use std::task::{Context, Poll, Waker};
        let (core, holder) = recovered_core(vec![], b"");
        let mut wait = std::pin::pin!(core.goodbye_detach());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(wait.as_mut().poll(&mut cx).is_pending());
        let bytes = holder.try_recv().unwrap();
        let (kind, pty_id, payload) = hproto::read_frame(&mut bytes.as_slice()).unwrap().unwrap();
        assert_eq!((kind, pty_id), (hproto::GOODBYE, 0));
        assert_eq!(
            hproto::parse_json::<hproto::HolderGoodbye>(&payload).unwrap(),
            hproto::HolderGoodbye::Detach
        );
        core.handle_holder_frame(hproto::BYE, 0, vec![]);
        assert!(matches!(wait.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
    }
}

use crate::control::ControlState;
use crate::holder;
use crate::mailbox::{self, MAX_MESSAGE_BYTES, MAX_MESSAGES, Mailboxes};
use crate::outbox::Conn;
use crate::paths::{self, Paths};
use crate::proc_info;
use ddg_agents_os::ipc::{LocalStream, connect};
use ddg_agents_proto::holder as hproto;
use ddg_agents_proto::id::decode_base64_bounded;
use ddg_agents_proto::rpc::{ErrorKind, Notification, RpcError};
use ddg_agents_proto::terminal::{
    AgentStatus, CloseReason, ControlMode, ForegroundHint, PtyAttachParams, PtyAttachResult,
    PtyClaimControlParams, PtyClaimControlResult, PtyEvent, PtyInfo, PtyKillParams, PtyKillResult,
    PtyReleaseControlParams, PtyReleaseControlResult, PtySpawnParams, PtySpawnResult,
    PtySubscribeResult, SessionInboxSubscribeParams, SessionInboxSubscribeResult,
    SessionReadScreenParams, SessionReadScreenResult, SessionRecvParams, SessionRecvResult,
    SessionSendParams, SessionSendResult, SessionWriteInputParams, SessionWriteInputResult,
};
use ddg_agents_vt::{Relay, Vt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Notify, oneshot};

const STATUS_DELAY: Duration = Duration::from_millis(200);
const FOREGROUND_INTERVAL: Duration = Duration::from_secs(1);
const DETACH_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Default)]
struct StatusCoalescer {
    deadline: Option<Instant>,
}

impl StatusCoalescer {
    /// Returns true for a change that must be published without waiting.
    fn update(
        &mut self,
        old: Option<&AgentStatus>,
        new: Option<&AgentStatus>,
        now: Instant,
    ) -> bool {
        let (old, new) = match (old, new) {
            (Some(old), Some(new)) => (old, new),
            (None, None) => return false,
            _ => {
                self.deadline = None;
                return true;
            }
        };
        let mut immediate = (old.state, old.kind, &old.app) != (new.state, new.kind, &new.app)
            || old.records.len() != new.records.len();
        let mut changed =
            (old.progress, &old.msg, &old.title) != (new.progress, &new.msg, &new.title);
        // The relay can move a refreshed record to the end. Compare records by ID, not position.
        for record in &new.records {
            let Some(before) = old.records.iter().find(|r| r.id == record.id) else {
                immediate = true;
                break;
            };
            immediate |= (before.state, before.kind, &before.app)
                != (record.state, record.kind, &record.app);
            changed |= (before.progress, &before.msg, &before.title)
                != (record.progress, &record.msg, &record.title);
        }
        if immediate {
            self.deadline = None;
        } else if changed {
            self.deadline = Some(now + STATUS_DELAY);
        }
        immediate
    }

    fn take_due(&mut self, now: Instant) -> bool {
        if self.deadline.is_some_and(|at| at <= now) {
            self.deadline = None;
            true
        } else {
            false
        }
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerIdentity {
    pub server_id: String,
    pub boot_generation: u64,
}

struct Viewer {
    conn: Arc<Conn>,
    channel: u32,
    granted_epoch: Option<u64>,
}

struct LivePty {
    pty_id: u32,
    session_id: String,
    run_generation: u64,
    pid: u32,
    cols: u16,
    rows: u16,
    cwd: Option<String>,
    title: String,
    vt: Vt,
    relay: Relay,
    agent_status: Option<AgentStatus>,
    foreground: Option<ForegroundHint>,
    status_coalescer: StatusCoalescer,
    control: ControlState,
    viewers: Vec<Viewer>,
}

impl LivePty {
    fn info(&self) -> PtyInfo {
        PtyInfo {
            pty_id: self.pty_id,
            session_id: self.session_id.clone(),
            run_generation: self.run_generation,
            pid: self.pid,
            cols: self.cols,
            rows: self.rows,
            cwd: self.cwd.clone(),
            title: self.title.clone(),
            controller: self.control.current(),
            agent_status: self.agent_status.clone(),
            foreground: self.foreground.clone(),
        }
    }

    fn agent_status_event(&self) -> PtyEvent {
        PtyEvent::AgentStatusChanged {
            pty_id: self.pty_id,
            session_id: self.session_id.clone(),
            run_generation: self.run_generation,
            status: self.agent_status.clone(),
        }
    }

    fn control_event(&self) -> PtyEvent {
        PtyEvent::ControlChanged {
            pty_id: self.pty_id,
            session_id: self.session_id.clone(),
            run_generation: self.run_generation,
            controller: self.control.current(),
        }
    }

    fn resize_event(&self) -> PtyEvent {
        PtyEvent::Resized {
            pty_id: self.pty_id,
            session_id: self.session_id.clone(),
            run_generation: self.run_generation,
            cols: self.cols,
            rows: self.rows,
        }
    }
}

struct PendingSpawn {
    cols: u16,
    rows: u16,
    vt: Vt,
    reply: oneshot::Sender<Result<u32, String>>,
}

struct EventSub {
    conn: Arc<Conn>,
    channel: u32,
}

struct CoreState {
    next_pty_id: u32,
    ptys: HashMap<u32, LivePty>,
    mailboxes: Mailboxes,
    pending_spawns: HashMap<u32, PendingSpawn>,
    pending_kills: HashMap<u32, Vec<oneshot::Sender<hproto::HolderExit>>>,
    event_seq: u64,
    event_subs: Vec<EventSub>,
}

pub struct Core {
    pub identity: ServerIdentity,
    holder_tx: Mutex<Sender<Vec<u8>>>,
    holder_fg_pgrp: bool,
    pub holder_instance: String,
    holder_bye: Notify,
    status_pending: Notify,
    state: Mutex<CoreState>,
    next_conn_id: AtomicU64,
    pub stop: Notify,
    pub stopping: AtomicBool,
}

impl Core {
    pub fn start(paths: Paths, bin_path: &std::path::Path) -> io::Result<Arc<Self>> {
        paths.create_dirs()?;
        let (mut stream, adopted) = connect_or_spawn_holder(&paths, bin_path)?;
        let identity = load_or_bump_identity(&paths, adopted)?;

        // Handshake with Holder and adopt any surviving PTYs + 1 MiB tails.
        let hello = hproto::encode_json(
            hproto::HELLO,
            0,
            &hproto::HolderHello {
                version: hproto::HOLDER_PROTO_VERSION,
            },
        );
        stream.write_all(&hello)?;
        let (ptys, next_pty_id, holder_fg_pgrp, instance) = recover_ptys(&mut stream)?;
        let (holder_tx, holder_rx) = mpsc::channel::<Vec<u8>>();

        let mut writer_stream = stream.try_clone()?;
        std::thread::Builder::new()
            .name("core-holder-writer".into())
            .spawn(move || {
                for frame in holder_rx {
                    if writer_stream.write_all(&frame).is_err() {
                        break;
                    }
                }
            })?;

        let core = Arc::new(Self::new(
            identity,
            holder_tx,
            ptys,
            next_pty_id,
            holder_fg_pgrp,
            instance,
        ));

        let reader_core = core.clone();
        std::thread::Builder::new()
            .name("core-holder-reader".into())
            .spawn(move || reader_core.read_holder_loop(stream))?;

        Ok(core)
    }

    fn new(
        identity: ServerIdentity,
        holder_tx: Sender<Vec<u8>>,
        ptys: HashMap<u32, LivePty>,
        next_pty_id: u32,
        holder_fg_pgrp: bool,
        holder_instance: String,
    ) -> Self {
        let mut mailboxes = Mailboxes::default();
        for pty in ptys.values() {
            if mailboxes
                .owner(&pty.session_id)
                .is_none_or(|id| id < pty.pty_id)
            {
                mailboxes.open(&pty.session_id, pty.pty_id);
            }
        }
        Self {
            identity,
            holder_tx: Mutex::new(holder_tx),
            holder_fg_pgrp,
            holder_instance,
            holder_bye: Notify::new(),
            status_pending: Notify::new(),
            state: Mutex::new(CoreState {
                next_pty_id,
                ptys,
                mailboxes,
                pending_spawns: HashMap::new(),
                pending_kills: HashMap::new(),
                event_seq: 0,
                event_subs: Vec::new(),
            }),
            next_conn_id: AtomicU64::new(1),
            stop: Notify::new(),
            stopping: AtomicBool::new(false),
        }
    }

    pub fn next_conn_id(&self) -> u64 {
        self.next_conn_id.fetch_add(1, Ordering::Relaxed)
    }

    fn lock(&self) -> MutexGuard<'_, CoreState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn send_holder(&self, frame: Vec<u8>) -> bool {
        let tx = self.holder_tx.lock().unwrap_or_else(|p| p.into_inner());
        tx.send(frame).is_ok()
    }

    fn read_holder_loop(self: Arc<Self>, mut stream: LocalStream) {
        while let Ok(Some((kind, pty_id, payload))) = hproto::read_frame(&mut stream) {
            self.handle_holder_frame(kind, pty_id, payload);
        }
        // Holder exited or handed ownership to a newer Core: shut down cleanly.
        self.stop.notify_one();
    }

    fn handle_holder_frame(&self, kind: u8, pty_id: u32, payload: Vec<u8>) {
        match kind {
            hproto::SPAWNED => {
                if let Ok(msg) = hproto::parse_json(&payload) {
                    self.handle_spawned(pty_id, msg);
                }
            }
            hproto::SPAWN_FAILED => {
                if let Ok(msg) = hproto::parse_json::<hproto::HolderSpawnFailed>(&payload)
                    && let Some(waiter) = self.lock().pending_spawns.remove(&pty_id)
                {
                    let _ = waiter.reply.send(Err(msg.error));
                }
            }
            hproto::OUTPUT => {
                self.handle_output(pty_id, &payload);
            }
            hproto::EXITED => {
                if let Ok(exit) = hproto::parse_json::<hproto::HolderExit>(&payload) {
                    self.handle_exited(pty_id, exit);
                }
            }
            hproto::FG_PGRP if self.holder_fg_pgrp => {
                if let Ok(batch) = hproto::parse_json(&payload) {
                    self.handle_foreground(batch);
                }
            }
            hproto::BYE => self.holder_bye.notify_one(),
            _ => {}
        }
    }

    pub async fn poll_foreground(&self) {
        if !self.holder_fg_pgrp {
            return;
        }
        let mut tick = tokio::time::interval(FOREGROUND_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await;
        loop {
            tick.tick().await;
            self.query_foreground();
        }
    }

    fn query_foreground(&self) {
        let has_live = !self.lock().ptys.is_empty();
        if self.holder_fg_pgrp && has_live {
            self.send_holder(hproto::encode_frame(hproto::QUERY_FG_PGRP, 0, &[]));
        }
    }

    fn handle_foreground(&self, batch: hproto::HolderFgPgrpBatch) {
        let live: Vec<_> = {
            let st = self.lock();
            batch
                .entries
                .into_iter()
                .filter_map(|entry| {
                    let pty = st.ptys.get(&entry.pty_id)?;
                    Some((entry.pty_id, pty.pid, entry.pgrp))
                })
                .collect()
        };
        let updates: Vec<_> = live
            .into_iter()
            .map(|(id, pid, pgrp)| {
                (
                    id,
                    pgrp.and_then(|pgid| proc_info::inspect_foreground(pgid, pid)),
                )
            })
            .collect();
        let mut st = self.lock();
        for (pty_id, foreground) in updates {
            let Some(pty) = st.ptys.get_mut(&pty_id) else {
                continue;
            };
            if pty.foreground == foreground {
                continue;
            }
            pty.foreground = foreground.clone();
            let event = PtyEvent::ForegroundChanged {
                pty_id,
                session_id: pty.session_id.clone(),
                run_generation: pty.run_generation,
                foreground,
            };
            self.emit_event(&mut st, event);
        }
    }

    fn handle_spawned(&self, pty_id: u32, msg: hproto::HolderSpawned) {
        let mut st = self.lock();
        let Some(pending) = st.pending_spawns.remove(&pty_id) else {
            if !st.ptys.contains_key(&pty_id) {
                self.send_holder(hproto::encode_frame(hproto::RELEASE, pty_id, &[]));
            }
            return;
        };
        // Register before reading the next Holder frame: fast children may already have exited.
        let pty = LivePty {
            pty_id,
            session_id: msg.session_id,
            run_generation: msg.run_generation,
            pid: msg.pid,
            cols: pending.cols,
            rows: pending.rows,
            cwd: None,
            title: String::new(),
            vt: pending.vt,
            relay: Relay::default(),
            agent_status: None,
            foreground: None,
            status_coalescer: StatusCoalescer::default(),
            control: ControlState::default(),
            viewers: Vec::new(),
        };
        if let Some(old) = st.mailboxes.owner(&pty.session_id)
            && st.ptys[&old].run_generation != pty.run_generation
        {
            st.mailboxes
                .purge_session(&pty.session_id, &CloseReason::Detached);
        }
        st.mailboxes.open(&pty.session_id, pty_id);
        let info = pty.info();
        st.ptys.insert(pty_id, pty);
        self.emit_event(&mut st, PtyEvent::Spawned { pty: info });
        let _ = pending.reply.send(Ok(msg.pid));
    }

    fn handle_output(&self, pty_id: u32, bytes: &[u8]) {
        self.handle_output_at(pty_id, bytes, Instant::now());
    }

    fn handle_output_at(&self, pty_id: u32, bytes: &[u8], now: Instant) {
        let mut st = self.lock();
        let Some(pty) = st.ptys.get_mut(&pty_id) else {
            return;
        };
        let due = pty
            .status_coalescer
            .take_due(now)
            .then(|| pty.agent_status_event());
        let output = pty
            .relay
            .feed_with_events(&mut pty.vt, bytes, unix_now_ms(), false);
        if !output.replies.is_empty() {
            self.send_holder(hproto::encode_frame(hproto::INPUT, pty_id, &output.replies));
        }
        let status_event = if output.status_changed {
            let status = pty.relay.agent_status();
            let immediate = pty
                .status_coalescer
                .update(pty.agent_status.as_ref(), status, now);
            pty.agent_status = status.cloned();
            if pty.status_coalescer.deadline.is_some() {
                self.status_pending.notify_one();
            }
            immediate.then(|| pty.agent_status_event())
        } else {
            None
        };
        let viewer_bytes = output.viewer_bytes;

        let new_cwd = pty.vt.pwd();
        let cwd_changed = if new_cwd != pty.cwd {
            pty.cwd = new_cwd.clone();
            new_cwd
        } else {
            None
        };
        let new_title = pty.vt.title();
        let title_changed = if new_title != pty.title {
            pty.title = new_title.clone();
            Some(new_title)
        } else {
            None
        };

        let mut behind = Vec::new();
        if !viewer_bytes.is_empty() {
            for (idx, viewer) in pty.viewers.iter().enumerate() {
                let ok = viewer
                    .conn
                    .with_outbox(|out| out.data(viewer.channel, &viewer_bytes));
                if !ok {
                    behind.push(idx);
                }
            }
        }
        let resynced = !behind.is_empty();
        if resynced {
            let paint = pty.vt.paint(0);
            for idx in behind {
                let viewer = &pty.viewers[idx];
                viewer.conn.with_outbox(|out| {
                    let _ = out.data(viewer.channel, &paint);
                });
            }
        }

        let run_generation = pty.run_generation;
        let events = [
            due,
            status_event,
            cwd_changed.map(|cwd| PtyEvent::CwdChanged {
                pty_id,
                session_id: pty.session_id.clone(),
                run_generation,
                cwd,
            }),
            title_changed.map(|title| PtyEvent::TitleChanged {
                pty_id,
                session_id: pty.session_id.clone(),
                run_generation,
                title,
            }),
            resynced.then(|| PtyEvent::Resynced {
                pty_id,
                session_id: pty.session_id.clone(),
                run_generation,
            }),
        ];
        for event in events.into_iter().flatten() {
            self.emit_event(&mut st, event);
        }
    }

    fn flush_status_updates(&self, now: Instant) -> Option<Instant> {
        let mut st = self.lock();
        let mut due = Vec::new();
        for pty in st.ptys.values_mut() {
            if pty.status_coalescer.take_due(now) {
                due.push(pty.agent_status_event());
            }
        }
        for event in due {
            self.emit_event(&mut st, event);
        }
        st.ptys
            .values()
            .filter_map(|pty| pty.status_coalescer.deadline)
            .min()
    }

    pub async fn publish_status_updates(&self) {
        loop {
            if let Some(at) = self.flush_status_updates(Instant::now()) {
                tokio::select! {
                    _ = tokio::time::sleep_until(at.into()) => {}
                    _ = self.status_pending.notified() => {}
                }
            } else {
                self.status_pending.notified().await;
            }
        }
    }

    fn handle_exited(&self, pty_id: u32, exit: hproto::HolderExit) {
        let mut st = self.lock();
        if let Some(pty) = st.ptys.remove(&pty_id) {
            let reason = CloseReason::Exited {
                exit_code: exit.code,
                signal: exit.signal,
            };
            if st.mailboxes.owner(&pty.session_id) == Some(pty_id) {
                st.mailboxes.purge_session(&pty.session_id, &reason);
            }
            let reason = serde_json::to_vec(&reason).expect("serializable");
            for viewer in pty.viewers {
                viewer
                    .conn
                    .with_outbox(|out| out.finish(viewer.channel, &reason));
            }
            self.emit_event(
                &mut st,
                PtyEvent::Exited {
                    pty_id,
                    session_id: pty.session_id,
                    run_generation: pty.run_generation,
                    exit_code: exit.code,
                    signal: exit.signal,
                },
            );
        }
        if let Some(waiters) = st.pending_kills.remove(&pty_id) {
            for w in waiters {
                let _ = w.send(exit);
            }
        }
        self.send_holder(hproto::encode_frame(hproto::RELEASE, pty_id, &[]));
    }

    fn emit_event(&self, st: &mut CoreState, event: PtyEvent) {
        st.event_seq += 1;
        let seq = st.event_seq;
        let payload = json!({
            "seq": seq,
            "event": event,
        });
        let line = format!(
            "{}\n",
            serde_json::to_string(&Notification::new("pty.event", payload)).expect("serializable")
        );
        let bytes = line.as_bytes();
        let mut missed = Vec::new();
        for (idx, sub) in st.event_subs.iter().enumerate() {
            let ok = sub
                .conn
                .with_outbox(|out| out.data_lines(sub.channel, bytes));
            if !ok {
                missed.push(idx);
            }
        }
        if !missed.is_empty() {
            let snapshot: Vec<PtyInfo> = st.ptys.values().map(LivePty::info).collect();
            let missed_line = format!(
                "{}\n",
                serde_json::to_string(&Notification::new(
                    "pty.missed",
                    json!({ "seq": seq, "ptys": snapshot }),
                ))
                .expect("serializable")
            );
            for idx in missed {
                let sub = &st.event_subs[idx];
                sub.conn.with_outbox(|out| {
                    let _ = out.data(sub.channel, missed_line.as_bytes());
                });
            }
        }
    }

    pub async fn spawn_pty(&self, params: PtySpawnParams) -> Result<PtySpawnResult, RpcError> {
        let cols = params.cols.clamp(1, 500);
        let rows = params.rows.clamp(1, 300);
        let vt = Vt::new(cols, rows).map_err(|e| RpcError::new(ErrorKind::Internal, e, None))?;
        let (pty_id, rx) = {
            let mut st = self.lock();
            let id = st.next_pty_id;
            st.next_pty_id += 1;
            let (reply, rx) = oneshot::channel();
            st.pending_spawns.insert(
                id,
                PendingSpawn {
                    cols,
                    rows,
                    vt,
                    reply,
                },
            );
            let spec = hproto::HolderSpawn {
                session_id: params.session_id,
                run_generation: params.run_generation,
                argv: params.argv.unwrap_or_default(),
                env: params.env.unwrap_or_default(),
                cwd: params.cwd.unwrap_or_default(),
                cols,
                rows,
            };
            if !self.send_holder(hproto::encode_json(hproto::SPAWN, id, &spec)) {
                st.pending_spawns.remove(&id);
                return Err(RpcError::new(
                    ErrorKind::Unavailable,
                    "holder disconnected during spawn",
                    None,
                ));
            }
            (id, rx)
        };

        let pid = match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(Ok(pid))) => pid,
            Ok(Ok(Err(err))) => return Err(RpcError::new(ErrorKind::Unavailable, err, None)),
            Ok(Err(_)) => {
                return Err(RpcError::new(
                    ErrorKind::Unavailable,
                    "holder disconnected during spawn",
                    None,
                ));
            }
            Err(_) => {
                self.lock().pending_spawns.remove(&pty_id);
                return Err(RpcError::new(
                    ErrorKind::Unavailable,
                    "timed out waiting for holder to spawn PTY",
                    None,
                ));
            }
        };
        Ok(PtySpawnResult { pty_id, pid })
    }

    pub fn session_send(&self, params: SessionSendParams) -> Result<SessionSendResult, RpcError> {
        self.lock().mailboxes.send(params)
    }

    pub async fn session_recv(
        &self,
        params: SessionRecvParams,
    ) -> Result<SessionRecvResult, RpcError> {
        let max = (params.max_messages as usize).min(MAX_MESSAGES);
        if max == 0 {
            return Err(RpcError::invalid_params("max_messages must be positive"));
        }
        let session = &params.session_id;
        let signal = self.lock().mailboxes.signal(session)?;
        let receive = async {
            loop {
                // Arm before checking the queue so an arrival or purge cannot be missed.
                let notified = signal.notified();
                {
                    let mut st = self.lock();
                    if !st
                        .mailboxes
                        .signal(session)
                        .is_ok_and(|s| Arc::ptr_eq(&s, &signal))
                    {
                        return Vec::new();
                    }
                    let messages = st.mailboxes.take(session, max);
                    if !messages.is_empty() || params.wait_ms == 0 {
                        return messages;
                    }
                }
                notified.await;
            }
        };
        let messages = tokio::time::timeout(Duration::from_millis(params.wait_ms), receive)
            .await
            .unwrap_or_default();
        Ok(SessionRecvResult { messages })
    }

    pub fn inbox_subscribe(
        &self,
        conn: &Arc<Conn>,
        params: SessionInboxSubscribeParams,
    ) -> Result<SessionInboxSubscribeResult, RpcError> {
        let channel = self.lock().mailboxes.subscribe(&params.session_id, conn)?;
        Ok(SessionInboxSubscribeResult { channel })
    }

    pub fn refill_inbox(&self, conn: &Conn) {
        self.lock().mailboxes.refill(conn);
    }

    pub fn read_screen(
        &self,
        params: SessionReadScreenParams,
    ) -> Result<SessionReadScreenResult, RpcError> {
        if !(1..=2000).contains(&params.lines) || !(1..=262_144).contains(&params.max_bytes) {
            return Err(RpcError::invalid_params(
                "lines must be 1..=2000 and max_bytes must be 1..=262144",
            ));
        }
        let (screen, run_generation) = {
            let st = self.lock();
            let pty = session_pty(&st, &params.session_id)?;
            (pty.vt.plain_text(), pty.run_generation)
        };
        let screen = screen.trim_end_matches('\n');
        let start = screen
            .rmatch_indices('\n')
            .nth(params.lines as usize - 1)
            .map_or(0, |(i, _)| i + 1);
        let text = &screen[start..];
        let max = params.max_bytes as usize;
        Ok(SessionReadScreenResult {
            text: text[..text.floor_char_boundary(max)].to_owned(),
            run_generation,
            truncated: text.len() > max,
        })
    }

    pub fn write_input(
        &self,
        params: SessionWriteInputParams,
    ) -> Result<SessionWriteInputResult, RpcError> {
        let bytes = decode_base64_bounded(&params.data_b64, MAX_MESSAGE_BYTES)
            .map_err(|e| RpcError::invalid_params(e.to_string()))?;
        if bytes.is_empty() {
            return Err(RpcError::invalid_params("input must not be empty"));
        }
        let mut st = self.lock();
        let pty = session_pty(&st, &params.session_id)?;
        let (pty_id, run_generation) = (pty.pty_id, pty.run_generation);
        if let Some(expected) = params.expect_run_generation
            && expected != run_generation
        {
            return Err(RpcError::generation_mismatch(format!(
                "expected run generation {expected}, found {run_generation}"
            )));
        }
        if !self.send_holder(hproto::encode_frame(hproto::INPUT, pty_id, &bytes)) {
            return Err(RpcError::new(
                ErrorKind::Unavailable,
                "holder disconnected during input",
                None,
            ));
        }
        let byte_len = bytes.len() as u32;
        self.emit_event(
            &mut st,
            PtyEvent::ExternalInput {
                session_id: params.session_id,
                pty_id,
                run_generation,
                byte_len,
                from_session_id: params.from_session_id,
                from_host_id: params.from_host_id,
                is_remote: params.is_remote,
            },
        );
        Ok(SessionWriteInputResult {
            byte_len,
            run_generation,
        })
    }

    pub fn list_ptys(&self) -> Vec<PtyInfo> {
        let st = self.lock();
        let mut list: Vec<PtyInfo> = st.ptys.values().map(LivePty::info).collect();
        list.sort_by_key(|p| p.pty_id);
        list
    }

    pub fn subscribe(&self, conn: &Arc<Conn>) -> PtySubscribeResult {
        let mut st = self.lock();
        let channel = conn.new_channel();
        conn.with_outbox(|out| out.open_paused(channel));
        let mut ptys: Vec<PtyInfo> = st.ptys.values().map(LivePty::info).collect();
        ptys.sort_by_key(|p| p.pty_id);
        st.event_subs.push(EventSub {
            conn: conn.clone(),
            channel,
        });
        PtySubscribeResult {
            seq: st.event_seq,
            ptys,
            channel: Some(channel),
        }
    }

    pub fn heartbeat_all(&self) {
        let st = self.lock();
        let line = format!(
            "{}\n",
            serde_json::to_string(&Notification::new(
                "pty.heartbeat",
                json!({ "seq": st.event_seq }),
            ))
            .expect("serializable")
        );
        for sub in &st.event_subs {
            sub.conn.with_outbox(|out| {
                if out.queued(sub.channel) == 0 {
                    let _ = out.data(sub.channel, line.as_bytes());
                }
            });
        }
    }

    pub fn attach(
        &self,
        conn: &Arc<Conn>,
        params: PtyAttachParams,
    ) -> Result<PtyAttachResult, RpcError> {
        let mut st = self.lock();
        let pty = st
            .ptys
            .get_mut(&params.pty_id)
            .ok_or_else(|| RpcError::not_found(format!("no live PTY with id {}", params.pty_id)))?;
        let (mode, control_epoch, control_changed) =
            pty.control.on_attach(conn.id, params.want_control);
        let mut resized = false;
        if mode == ControlMode::Control && params.cols > 0 && params.rows > 0 {
            let cols = params.cols.clamp(1, 500);
            let rows = params.rows.clamp(1, 300);
            if (cols, rows) != (pty.cols, pty.rows) {
                pty.cols = cols;
                pty.rows = rows;
                pty.vt.resize(cols, rows);
                self.send_holder(hproto::encode_json(
                    hproto::RESIZE,
                    pty.pty_id,
                    &hproto::HolderSize { cols, rows },
                ));
                for v in &pty.viewers {
                    v.conn.with_outbox(|out| out.resize(v.channel, cols, rows));
                }
                resized = true;
            }
        }
        let channel = conn.new_channel();
        let paint = pty.vt.paint(params.history_lines as usize);
        let (cols, rows) = (pty.cols, pty.rows);
        let control_event = control_changed.then(|| pty.control_event());
        let resize_event = resized.then(|| pty.resize_event());
        pty.viewers.push(Viewer {
            conn: conn.clone(),
            channel,
            granted_epoch: (mode == ControlMode::Control).then_some(control_epoch),
        });
        conn.with_outbox(|out| {
            out.open_paused(channel);
            out.resize(channel, cols, rows);
            let _ = out.data(channel, &paint);
        });
        for event in [control_event, resize_event].into_iter().flatten() {
            self.emit_event(&mut st, event);
        }
        Ok(PtyAttachResult {
            channel,
            mode,
            control_epoch,
            cols,
            rows,
        })
    }

    pub fn claim_control(
        &self,
        conn: &Arc<Conn>,
        params: PtyClaimControlParams,
    ) -> Result<PtyClaimControlResult, RpcError> {
        let mut st = self.lock();
        let pty = st
            .ptys
            .get_mut(&params.pty_id)
            .ok_or_else(|| RpcError::not_found(format!("no live PTY with id {}", params.pty_id)))?;
        let (epoch, changed) = pty
            .control
            .claim(conn.id, params.force)
            .map_err(RpcError::control_denied)?;
        for viewer in &mut pty.viewers {
            if viewer.conn.id == conn.id {
                viewer.granted_epoch = Some(epoch);
            }
        }
        if changed {
            let event = pty.control_event();
            self.emit_event(&mut st, event);
        }
        Ok(PtyClaimControlResult {
            control_epoch: epoch,
        })
    }

    pub fn release_control(
        &self,
        conn: &Arc<Conn>,
        params: PtyReleaseControlParams,
    ) -> Result<PtyReleaseControlResult, RpcError> {
        let mut st = self.lock();
        let pty = st
            .ptys
            .get_mut(&params.pty_id)
            .ok_or_else(|| RpcError::not_found(format!("no live PTY with id {}", params.pty_id)))?;
        let released = pty.control.release(conn.id);
        if released {
            for viewer in &mut pty.viewers {
                if viewer.conn.id == conn.id {
                    viewer.granted_epoch = None;
                }
            }
            let event = pty.control_event();
            self.emit_event(&mut st, event);
        }
        Ok(PtyReleaseControlResult { released })
    }

    pub async fn kill_pty(&self, params: PtyKillParams) -> Result<PtyKillResult, RpcError> {
        let rx = {
            let mut st = self.lock();
            let pty = st.ptys.get(&params.pty_id).ok_or_else(|| {
                RpcError::not_found(format!("no live PTY with id {}", params.pty_id))
            })?;
            let session_id = pty.session_id.clone();
            let signal = crate::pty::TERMINATE;
            // Graceful termination first, escalating to SIGKILL on timeout.
            if !self.send_holder(hproto::encode_json(
                hproto::SIGNAL,
                params.pty_id,
                &hproto::HolderSignal { signal },
            )) {
                return Err(RpcError::new(
                    ErrorKind::Unavailable,
                    "holder disconnected during kill",
                    None,
                ));
            }
            if st.mailboxes.owner(&session_id) == Some(params.pty_id) {
                st.mailboxes.purge_session(
                    &session_id,
                    &CloseReason::Killed {
                        signal: Some(signal),
                    },
                );
            }
            let (tx, rx) = oneshot::channel();
            st.pending_kills.entry(params.pty_id).or_default().push(tx);
            rx
        };
        let mut rx = rx;
        let exit = match tokio::time::timeout(Duration::from_millis(800), &mut rx).await {
            Ok(Ok(exit)) => exit,
            Ok(Err(_)) => {
                return Err(RpcError::new(
                    ErrorKind::Unavailable,
                    "holder disconnected during kill",
                    None,
                ));
            }
            Err(_) => {
                self.send_holder(hproto::encode_json(
                    hproto::SIGNAL,
                    params.pty_id,
                    &hproto::HolderSignal {
                        signal: crate::pty::KILL,
                    },
                ));
                match tokio::time::timeout(Duration::from_secs(5), rx).await {
                    Ok(Ok(exit)) => exit,
                    _ => {
                        return Err(RpcError::new(
                            ErrorKind::Unavailable,
                            "timed out waiting for PTY child reap",
                            None,
                        ));
                    }
                }
            }
        };
        Ok(PtyKillResult {
            exit_code: exit.code,
            signal: exit.signal,
        })
    }

    pub fn viewer_input(&self, conn: &Arc<Conn>, channel: u32, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let st = self.lock();
        let Some((pty_id, pty, viewer)) = find_viewer(&st, conn.id, channel) else {
            return;
        };
        match pty
            .control
            .verify(conn.id, viewer.granted_epoch.unwrap_or(0))
        {
            Ok(()) => {
                self.send_holder(hproto::encode_frame(hproto::INPUT, pty_id, bytes));
            }
            Err(reason) => {
                send_control_denied(conn, pty_id, &reason);
            }
        }
    }

    pub fn viewer_resize(&self, conn: &Arc<Conn>, channel: u32, cols: u16, rows: u16) {
        let cols = cols.clamp(1, 500);
        let rows = rows.clamp(1, 300);
        let mut st = self.lock();
        let Some((pty_id, granted_epoch)) =
            find_viewer(&st, conn.id, channel).map(|(id, _, v)| (id, v.granted_epoch.unwrap_or(0)))
        else {
            return;
        };
        let pty = st.ptys.get_mut(&pty_id).expect("pty exists");
        if let Err(reason) = pty.control.verify(conn.id, granted_epoch) {
            send_control_denied(conn, pty_id, &reason);
            return;
        }
        if (pty.cols, pty.rows) == (cols, rows) {
            return;
        }
        pty.cols = cols;
        pty.rows = rows;
        pty.vt.resize(cols, rows);
        self.send_holder(hproto::encode_json(
            hproto::RESIZE,
            pty_id,
            &hproto::HolderSize { cols, rows },
        ));
        for v in &pty.viewers {
            if v.conn.id != conn.id || v.channel != channel {
                v.conn.with_outbox(|out| out.resize(v.channel, cols, rows));
            }
        }
        let event = pty.resize_event();
        self.emit_event(&mut st, event);
    }

    pub fn detach_channel(&self, conn: &Arc<Conn>, channel: u32) {
        let mut st = self.lock();
        st.event_subs
            .retain(|s| !(s.conn.id == conn.id && s.channel == channel));
        st.mailboxes.unsubscribe(conn, Some(channel));
        let mut events = Vec::new();
        for pty in st.ptys.values_mut() {
            let before = pty.viewers.len();
            pty.viewers
                .retain(|v| !(v.conn.id == conn.id && v.channel == channel));
            if pty.viewers.len() != before {
                let still_attached = pty.viewers.iter().any(|v| v.conn.id == conn.id);
                if !still_attached && pty.control.release(conn.id) {
                    events.push(pty.control_event());
                }
            }
        }
        let reason = serde_json::to_vec(&CloseReason::Detached).expect("serializable");
        conn.with_outbox(|out| out.close_now(channel, &reason));
        for event in events {
            self.emit_event(&mut st, event);
        }
    }

    pub fn disconnect_conn(&self, conn: &Arc<Conn>) {
        let mut st = self.lock();
        st.event_subs.retain(|s| s.conn.id != conn.id);
        st.mailboxes.unsubscribe(conn, None);
        let mut events = Vec::new();
        for pty in st.ptys.values_mut() {
            pty.viewers.retain(|v| v.conn.id != conn.id);
            if pty.control.release(conn.id) {
                events.push(pty.control_event());
            }
        }
        for event in events {
            self.emit_event(&mut st, event);
        }
    }

    pub fn is_idle(&self) -> bool {
        let st = self.lock();
        st.ptys.is_empty() && st.pending_spawns.is_empty()
    }

    pub async fn goodbye_detach(&self) -> io::Result<()> {
        self.goodbye(hproto::HolderGoodbye::Detach).await
    }

    pub async fn goodbye(&self, mode: hproto::HolderGoodbye) -> io::Result<()> {
        self.stopping.store(true, Ordering::Release);
        if !self.send_holder(hproto::encode_json(hproto::GOODBYE, 0, &mode)) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        tokio::time::timeout(DETACH_TIMEOUT, self.holder_bye.notified()).await?;
        Ok(())
    }
}

fn recover_ptys(
    stream: &mut LocalStream,
) -> io::Result<(HashMap<u32, LivePty>, u32, bool, String)> {
    let welcome = match hproto::read_frame(stream)? {
        Some((hproto::WELCOME, _, payload)) => {
            hproto::parse_json::<hproto::HolderWelcome>(&payload)?
        }
        _ => return Err(io::Error::other("holder closed before WELCOME")),
    };
    if welcome.version != hproto::HOLDER_PROTO_VERSION {
        return Err(io::Error::other("incompatible Holder protocol"));
    }
    let mut live = HashMap::new();
    let mut max_id = 0;
    let holder_fg_pgrp = welcome
        .features
        .iter()
        .any(|feature| feature == "fg_pgrp_v1");
    let tail_count = welcome.ptys.len();
    for held in welcome.ptys {
        max_id = max_id.max(held.pty_id);
        if held.exit.is_some() {
            stream.write_all(&hproto::encode_frame(hproto::RELEASE, held.pty_id, &[]))?;
        } else {
            live.insert(held.pty_id, held);
        }
    }

    let mut ptys = HashMap::new();
    for _ in 0..tail_count {
        let Some((hproto::TAIL, id, tail)) = hproto::read_frame(stream)? else {
            return Err(io::Error::other("holder closed before sending PTY tails"));
        };
        let Some(held) = live.remove(&id) else {
            continue;
        };
        let cols = held.cols.max(1);
        let rows = held.rows.max(1);
        let mut vt = Vt::new(cols, rows).map_err(io::Error::other)?;
        let mut relay = Relay::default();
        let _ = relay.feed_with_events(&mut vt, &tail, unix_now_ms(), true);
        let agent_status = relay.agent_status().cloned();
        ptys.insert(
            id,
            LivePty {
                pty_id: id,
                session_id: held.session_id,
                run_generation: held.run_generation,
                pid: held.pid,
                cols,
                rows,
                cwd: vt.pwd(),
                title: vt.title(),
                vt,
                relay,
                agent_status,
                foreground: None,
                status_coalescer: StatusCoalescer::default(),
                control: ControlState::default(),
                viewers: Vec::new(),
            },
        );
    }
    if !live.is_empty() {
        return Err(io::Error::other("holder did not send all live PTY tails"));
    }
    Ok((ptys, max_id + 1, holder_fg_pgrp, welcome.instance))
}

fn session_pty<'a>(st: &'a CoreState, session: &str) -> Result<&'a LivePty, RpcError> {
    let id = st
        .mailboxes
        .owner(session)
        .ok_or_else(|| mailbox::missing(session))?;
    Ok(&st.ptys[&id])
}

fn find_viewer(st: &CoreState, conn_id: u64, channel: u32) -> Option<(u32, &LivePty, &Viewer)> {
    st.ptys.iter().find_map(|(&id, pty)| {
        pty.viewers
            .iter()
            .find(|v| v.conn.id == conn_id && v.channel == channel)
            .map(|v| (id, pty, v))
    })
}

fn send_control_denied(conn: &Arc<Conn>, pty_id: u32, reason: &str) {
    let notif = Notification::new(
        "pty.control_denied",
        json!({
            "pty_id": pty_id,
            "kind": ErrorKind::ControlDenied,
            "message": reason,
        }),
    );
    if let Ok(bytes) = serde_json::to_vec(&notif) {
        conn.with_outbox(|out| out.control_json(&bytes));
    }
}

fn connect_or_spawn_holder(
    paths: &Paths,
    bin_path: &std::path::Path,
) -> io::Result<(LocalStream, bool)> {
    if let Ok(stream) = connect(&paths.holder_sock) {
        return Ok((stream, true));
    }
    paths::remove_if_exists(&paths.holder_sock);
    holder::spawn_detached(
        paths,
        bin_path,
        std::process::Command::new(bin_path)
            .arg("holder")
            .arg("--socket")
            .arg(&paths.holder_sock),
        "holder.log",
    )?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(stream) = connect(&paths.holder_sock) {
            return Ok((stream, false));
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    Err(io::Error::other("timed out waiting for holder socket"))
}

fn load_or_bump_identity(paths: &Paths, adopted: bool) -> io::Result<ServerIdentity> {
    let existing = std::fs::read(&paths.server_json)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ServerIdentity>(&bytes).ok())
        .filter(|id| !id.server_id.trim().is_empty());
    let identity = match existing {
        Some(mut id) => {
            if !adopted {
                id.boot_generation += 1;
            }
            id
        }
        None => ServerIdentity {
            server_id: paths::random_hex(),
            boot_generation: 1,
        },
    };
    let bytes = serde_json::to_vec_pretty(&identity).map_err(io::Error::other)?;
    paths::write_atomic(&paths.server_json, &bytes)?;
    Ok(identity)
}
