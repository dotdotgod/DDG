use super::*;
use ddg_agents_proto::frame::{HEADER_LEN, decode_header};
use ddg_agents_proto::rpc::{Request, Response};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

pub(super) const VISIBLE: &str = "rm_00000000000000000000000001";
pub(super) const BACKGROUND: &str = "rm_bg_test";
pub(super) const PARENT: &str = "ses_00000000000000000000000001";
pub(super) const CHILD: &str = "ses_00000000000000000000000002";

pub(crate) struct Fixture {
    pub(crate) host: Arc<HostService>,
    pub(crate) peer: BufReader<UnixStream>,
    root: PathBuf,
}

impl Fixture {
    pub(crate) async fn new(catalog: Catalog) -> Self {
        let root = PathBuf::from(format!("/tmp/ddg-hunit-{}", paths::random_hex()));
        std::fs::create_dir(&root).unwrap();
        let paths = Paths {
            state_dir: root.clone(),
            run_dir: root.clone(),
            plugins_dir: root.join("plugins"),
            host_sock: root.join("host.sock"),
            terminal_sock: root.join("term.sock"),
            catalog_file: root.join("catalog.json"),
            lock_file: root.join("lock"),
        };
        let listener = UnixListener::bind(&paths.terminal_sock).unwrap();
        let connect = TermConn::connect(&paths, "test-host");
        let handshake = async {
            // ensure_terminal_server probes with a disposable connection before the hello.
            let _probe = listener.accept().await.unwrap();
            let (peer, _) = listener.accept().await.unwrap();
            let mut peer = BufReader::new(peer);
            let mut line = String::new();
            peer.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(req.method, "hello");
            let hello = Response::ok(
                req.id,
                json!(HelloResult {
                    protocol_version: PROTOCOL_VERSION,
                    server_id: "test".into(),
                    boot_generation: 1,
                    conn_id: 1,
                    features: Vec::new(),
                    app_version: None,
                    platform: None,
                    holder_protocol_version: None,
                    holder_instance: None,
                    active_pty_count: None,
                }),
            );
            peer.get_mut()
                .write_all(format!("{}\n", json!(hello)).as_bytes())
                .await
                .unwrap();
            peer
        };
        let (term, peer) = tokio::join!(connect, handshake);
        let audit_path = root.join("host.log");
        File::create(&audit_path)
            .unwrap()
            .set_permissions(Permissions::from_mode(0o644))
            .unwrap();
        let host = Arc::new(HostService {
            audit: Mutex::new(open_audit(&audit_path).unwrap()),
            host_id: "test".into(),
            paths,
            plugins: PluginHost::empty(),
            relay: Relay::default(),
            state: Mutex::new(HostState {
                catalog,
                activities: HashMap::new(),
                seq: 0,
                reset_notice: None,
                subs: vec![],
                clients: vec![],
                tokens: TokenStore::default(),
                ops: Operations::default(),
                term: term.unwrap(),
            }),
            next_conn_id: AtomicU64::new(1),
            stop: Notify::new(),
            reload_gate: Arc::new(RwLock::new(())),
        });
        HostService::restore_activities(&mut host.lock(), &[]);
        Self { host, peer, root }
    }

    pub(crate) async fn expect(&mut self, method: &str) -> Request {
        let mut header = [0; HEADER_LEN];
        tokio::time::timeout(Duration::from_secs(2), self.peer.read_exact(&mut header))
            .await
            .unwrap()
            .unwrap();
        let (channel, kind, len) = decode_header(&header).unwrap();
        assert_eq!((channel, kind), (CONTROL_CHANNEL, FrameKind::Data));
        let mut payload = vec![0; len];
        self.peer.read_exact(&mut payload).await.unwrap();
        let req: Request = serde_json::from_slice(&payload).unwrap();
        assert_eq!(req.method, method);
        req
    }

    pub(crate) async fn reply(&mut self, req: Request, value: Value) {
        self.respond(Response::ok(req.id, value)).await;
    }

    async fn respond(&mut self, response: Response) {
        let reply = serde_json::to_vec(&response).unwrap();
        self.peer
            .get_mut()
            .write_all(&encode_frame(CONTROL_CHANNEL, FrameKind::Data, &reply).unwrap())
            .await
            .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

pub(crate) fn fixture_catalog() -> Catalog {
    let mut catalog = Catalog::empty("test");
    let mut visible = standard_room(VISIBLE.to_owned(), None, None);
    visible.tabs.push(running_tab(PARENT, 10, None));
    let mut hidden = CreateTarget::Background(BACKGROUND.to_owned())
        .new_room(None)
        .unwrap();
    hidden
        .tabs
        .push(running_tab(CHILD, 11, Some(PARENT.into())));
    catalog.rooms = vec![visible, hidden];
    catalog
}

fn running_tab(id: &str, pty_id: u32, parent: Option<String>) -> TabRecord {
    TabRecord {
        id: id.into(),
        pty_id: Some(pty_id),
        runtime: TabRuntimeState::Running {
            pty_id,
            pid: pty_id + 100,
        },
        run_generation: 1,
        agent_profile: None,
        parent_session_id: parent,
        parent_host_id: None,
        space_cwd: None,
        last_cwd: None,
        title: String::new(),
        executor: ExecutorSpec::DefaultShell,
    }
}

fn tab_create(key: &str) -> TabCreateParams {
    TabCreateParams {
        agent_profile: None,
        request_id: Some(key.into()),
        room_id: Some(VISIBLE.into()),
        background: false,
        space_cwd: None,
        title: None,
        executor: ExecutorSpec::DefaultShell,
        cols: 80,
        rows: 24,
    }
}

#[tokio::test]
async fn inflight_keys_and_tab_limits_hold_until_failed_commit_is_compensated() {
    let mut catalog = fixture_catalog();
    for _ in 1..MAX_TABS_PER_ROOM - 1 {
        catalog.rooms[0]
            .tabs
            .push(new_tab(None, ExecutorSpec::DefaultShell, None, None).unwrap());
    }
    let mut f = Fixture::new(catalog).await;
    std::fs::create_dir(&f.host.paths.catalog_file).unwrap();
    let host = f.host.clone();
    let attempt = tokio::spawn(async move { host.tab_create(tab_create("same")).await });
    let spawn = f.expect("pty.spawn").await;
    let failed_token = spawn.params["env"]["DDG_SESSION_TOKEN"].clone();
    let retry = tokio::time::timeout(
        Duration::from_secs(1),
        f.host.tab_create(tab_create("same")),
    )
    .await
    .expect("in-flight idempotency must not block")
    .unwrap_err();
    assert_eq!(retry.data.kind, ErrorKind::Conflict);
    let another = f.host.tab_create(tab_create("another")).await.unwrap_err();
    assert_eq!(another.data.code.as_deref(), Some("room_tab_limit"));
    let promote = f.host.session_move(
        CHILD,
        SessionMoveTarget::ExistingRoom {
            room_id: VISIBLE.into(),
        },
    );
    assert_eq!(
        promote.unwrap_err().data.code.as_deref(),
        Some("room_tab_limit")
    );
    f.reply(
        spawn,
        json!(PtySpawnResult {
            pty_id: 23,
            pid: 123
        }),
    )
    .await;
    let kill = f.expect("pty.kill").await;
    assert_eq!(kill.params["pty_id"], 23);
    let revoked = f
        .host
        .authenticate_local(&json!({"session_token":failed_token}))
        .unwrap_err();
    assert_eq!(revoked.data.code.as_deref(), Some("token_revoked"));
    assert_eq!(
        f.host.lock().catalog.rooms[0].tabs.len(),
        MAX_TABS_PER_ROOM - 1
    );
    f.reply(
        kill,
        json!(PtyKillResult {
            exit_code: None,
            signal: Some(15)
        }),
    )
    .await;
    assert_eq!(
        attempt.await.unwrap().unwrap_err().data.kind,
        ErrorKind::Internal
    );

    std::fs::remove_dir(&f.host.paths.catalog_file).unwrap();
    let host = f.host.clone();
    let retry = tokio::spawn(async move { host.tab_create(tab_create("same")).await });
    let spawn = f.expect("pty.spawn").await;
    f.reply(
        spawn,
        json!(PtySpawnResult {
            pty_id: 24,
            pid: 124
        }),
    )
    .await;
    assert_eq!(retry.await.unwrap().unwrap().tab.pty_id, Some(24));
    assert!(f.host.lock().ops.in_flight_creates.is_empty());
    let promote = f.host.session_move(
        CHILD,
        SessionMoveTarget::ExistingRoom {
            room_id: VISIBLE.into(),
        },
    );
    assert_eq!(
        promote.unwrap_err().data.code.as_deref(),
        Some("room_tab_limit")
    );
    let st = f.host.lock();
    assert_eq!(st.catalog.rooms[0].tabs.len(), MAX_TABS_PER_ROOM);
    assert_eq!(st.catalog.rooms[1].tabs[0].id, CHILD);
}

#[tokio::test]
async fn ambiguous_spawn_failure_is_compensated_by_session_and_generation() {
    let mut f = Fixture::new(fixture_catalog()).await;
    let host = f.host.clone();
    let attempt = tokio::spawn(async move { host.tab_create(tab_create("retry")).await });
    let spawn = f.expect("pty.spawn").await;
    let session = spawn.params["session_id"].clone();
    let fail = RpcError::new(ErrorKind::Unavailable, "spawn outcome unknown", None);
    f.respond(Response::err(spawn.id, fail.clone())).await;
    let list = f.expect("pty.list").await;
    let live = json!({"pty_id": 42, "session_id": session, "run_generation": 1, "pid": 142, "cols": 80, "rows": 24});
    let unrelated = json!({"pty_id": 43, "session_id": session, "run_generation": 2, "pid": 143, "cols": 80, "rows": 24});
    f.reply(list, json!({"ptys": [unrelated, live]})).await;
    let kill = f.expect("pty.kill").await;
    assert_eq!(kill.params["pty_id"], 42);
    f.reply(kill, json!({"signal": 15})).await;
    assert_eq!(attempt.await.unwrap().unwrap_err(), fail);
    let st = f.host.lock();
    assert_eq!(st.catalog.rooms[0].tabs.len(), 1);
    assert!(
        st.ops.idempotency.is_empty()
            && st.ops.in_flight_creates.is_empty()
            && st.ops.spawning.is_empty()
    );
}

#[tokio::test]
async fn split_checks_depth_before_spawn_and_compensates_if_layout_changes() {
    let mut catalog = fixture_catalog();
    let room = &mut catalog.rooms[0];
    let mut deep = RoomLayout::default_for_tabs(&room.tabs);
    for _ in 0..MAX_SPLIT_DEPTH {
        let tab = new_tab(None, ExecutorSpec::DefaultShell, None, None).unwrap();
        assert!(
            deep.split_leaf(
                PARENT,
                SplitAxis::Horizontal,
                tab.id.clone(),
                DEFAULT_SPLIT_RATIO_BP
            )
            .unwrap()
        );
        room.tabs.push(tab);
    }
    let mut f = Fixture::new(catalog).await;
    let params = TabSplitParams {
        agent_profile: None,
        request_id: Some("split".into()),
        anchor_tab_id: PARENT.into(),
        axis: SplitAxis::Vertical,
        ratio_bp: None,
        space_cwd: None,
        title: None,
        executor: ExecutorSpec::DefaultShell,
        cols: 80,
        rows: 24,
    };
    let host = f.host.clone();
    let initial = params.clone();
    let attempt = tokio::spawn(async move { host.tab_split(initial).await });
    let spawn = f.expect("pty.spawn").await;
    f.host
        .room_set_layout(RoomSetLayoutParams {
            room_id: VISIBLE.into(),
            layout: Some(deep.clone()),
        })
        .unwrap();
    f.reply(spawn, json!({"pty_id": 42, "pid": 99})).await;
    let kill = f.expect("pty.kill").await;
    assert_eq!(kill.params["pty_id"], 42);
    f.reply(kill, json!({"signal": 15})).await;
    let error = attempt.await.unwrap().unwrap_err();
    assert_eq!(error.data.kind, ErrorKind::InvalidParams);
    assert!(error.message.contains("depth"));

    let retry = f.host.tab_split(params);
    let error = tokio::time::timeout(Duration::from_secs(1), retry)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.data.kind, ErrorKind::InvalidParams);
    assert!(error.message.contains("depth"));
    let st = f.host.lock();
    assert_eq!(st.catalog.rooms[0].tabs.len(), MAX_SPLIT_DEPTH + 1);
    assert_eq!(st.catalog.rooms[0].layout.as_ref(), Some(&deep));
    assert_eq!(st.seq, 1);
    assert!(st.ops.in_flight_creates.is_empty() && st.ops.idempotency.is_empty());
}

#[tokio::test]
async fn background_reservations_enforce_64_and_close_restart_exclude_each_other() {
    let mut catalog = fixture_catalog();
    for _ in 1..MAX_BACKGROUND_TABS - 1 {
        catalog.rooms[1]
            .tabs
            .push(new_tab(None, ExecutorSpec::DefaultShell, None, None).unwrap());
    }
    let f = Fixture::new(catalog).await;
    let target = CreateTarget::Background(BACKGROUND.into());
    let guard = f.host.reserve_create(&target, CHILD, false).unwrap();
    assert!(matches!(f.host.reserve_create(&target, CHILD, false), Err(e) if e == tab_limit()));
    drop(guard);
    assert!(f.host.reserve_create(&target, CHILD, false).is_ok());
    let (close, _, _) = f.host.reserve_lifecycle(CHILD, false, false).unwrap();
    assert!(
        matches!(f.host.reserve_lifecycle(CHILD, false, true), Err(e) if e.data.kind == ErrorKind::Conflict)
    );
    drop(close);
    let (restart, _, _) = f.host.reserve_lifecycle(CHILD, false, true).unwrap();
    assert!(
        matches!(f.host.reserve_lifecycle(CHILD, false, false), Err(e) if e.data.kind == ErrorKind::Conflict)
    );
    drop(restart);
    assert!(f.host.reserve_lifecycle(CHILD, false, false).is_ok());
}

#[tokio::test]
async fn q2_notifies_before_deletion_and_revokes_after_catalog_update() {
    let mut catalog = fixture_catalog();
    catalog.rooms[1].tabs[0].parent_host_id = Some("test".into());
    let mut f = Fixture::new(catalog).await;
    let token = f.host.lock().tokens.issue(CHILD, 1).unwrap();
    let (notices, mut events) = mpsc::unbounded_channel();
    f.host.register_client(1, notices);
    let (sub, mut catalog) = mpsc::unbounded_channel();
    f.host.catalog_subscribe(1, 2, sub);
    let event = Notification::new(
        "pty.event",
        json!({"event": PtyEvent::Exited {
            pty_id: 11, session_id: CHILD.into(), run_generation: 1, exit_code: Some(7), signal: None,
        }}),
    );
    let host = f.host.clone();
    let exiting = tokio::spawn(async move {
        host.handle_pty_event_line(&host.term().unwrap(), &serde_json::to_vec(&event).unwrap())
            .await;
    });
    let send = f.expect("session.send").await;
    assert_eq!(send.params["to_session_id"], PARENT);
    assert_eq!(send.params["kind"], "session_exited");
    assert_eq!(send.params["exit_code"], 7);
    let event = events.try_recv().unwrap();
    let payload: Notification = serde_json::from_slice(&event[HEADER_LEN..]).unwrap();
    assert_eq!(payload.method, "host.session_exited");
    assert!(catalog.try_recv().is_err());
    assert!(f.host.lock().tokens.verify(&token).is_ok());
    assert_eq!(f.host.lock().catalog.rooms[1].tabs[0].id, CHILD);
    assert!(!f.host.lock().activities.contains_key(CHILD));
    let err = f
        .host
        .tab_mark_seen(TabMarkSeenParams {
            tab_id: CHILD.into(),
            run_generation: 1,
            seen_seq: 0,
        })
        .unwrap_err();
    assert_eq!(err.data.code.as_deref(), Some("session_not_found"));
    f.reply(
        send,
        json!({"message_id": "msg_00000000000000000000000001"}),
    )
    .await;
    exiting.await.unwrap();
    let change: Notification = serde_json::from_slice(&catalog.try_recv().unwrap()).unwrap();
    assert_eq!(change.method, "host.catalog_changed");
    assert_eq!(change.params["catalog"]["rooms"][1]["tabs"], json!([]));
    assert_eq!(
        f.host
            .lock()
            .tokens
            .verify(&token)
            .unwrap_err()
            .data
            .code
            .as_deref(),
        Some("token_revoked")
    );
}

#[tokio::test]
async fn terminal_titles_are_sanitized_and_stale_generations_are_ignored() {
    let f = Fixture::new(fixture_catalog()).await;
    let term = f.host.term().unwrap();
    for (generation, title) in [(1, "clean\n\u{061c} title"), (2, "stale")] {
        let note = Notification::new(
            "pty.event",
            json!({"event": PtyEvent::TitleChanged {
                pty_id: 10, session_id: PARENT.into(), run_generation: generation, title: title.into(),
            }}),
        );
        f.host
            .handle_pty_event_line(&term, &serde_json::to_vec(&note).unwrap())
            .await;
        assert_eq!(f.host.lookup_tab_any(PARENT).unwrap().title, "clean title");
    }
}

fn test_status(state: AgentState, progress: Option<u8>) -> AgentStatus {
    AgentStatus {
        state,
        kind: (state == AgentState::Blocked).then_some(BlockedKind::Permission),
        progress,
        app: None,
        title: None,
        msg: None,
        records: vec![],
        status_ms: 1,
    }
}

impl Fixture {
    pub(crate) async fn event(&self, event: PtyEvent) {
        let term = self.host.term().unwrap();
        let note = Notification::new("pty.event", json!({"event": event}));
        self.host
            .handle_pty_event_line(&term, &serde_json::to_vec(&note).unwrap())
            .await;
    }
}

#[tokio::test]
async fn activity_and_seen() {
    let mut catalog = fixture_catalog();
    catalog.rooms[1].tabs[0].parent_session_id = None;
    let f = Fixture::new(catalog).await;
    let (tx, mut deltas) = mpsc::unbounded_channel();
    let (control_tx, mut control) = mpsc::unbounded_channel();
    f.host.register_client(1, control_tx);
    let snapshot = f.host.catalog_subscribe(1, 2, tx);
    assert_eq!(snapshot.activities.len(), 2);
    assert!(
        snapshot
            .activities
            .iter()
            .all(|a| a.activity_seq == 0 && a.seen_seq == 0)
    );
    assert!(snapshot.activities.iter().any(|a| a.tab_id == CHILD));

    let file = &f.host.paths.catalog_file;
    save_catalog(file, &snapshot.catalog, None).unwrap();
    let bytes = std::fs::read(file).unwrap();
    let mtime = file.metadata().unwrap().modified().unwrap();
    let mut delta = || serde_json::from_slice::<Notification>(&deltas.try_recv().unwrap()).unwrap();

    for i in 0..10 {
        let state = match i {
            0..=5 => AgentState::Working,
            6 => AgentState::Blocked,
            7 => AgentState::Done,
            8 => AgentState::Error,
            _ => AgentState::Idle,
        };
        f.event(PtyEvent::AgentStatusChanged {
            pty_id: 11,
            session_id: CHILD.into(),
            run_generation: 1,
            status: Some(test_status(state, Some(i))),
        })
        .await;
        let note = delta();
        assert_eq!(note.method, "host.tab_activity_changed");
        assert_eq!(note.params["room_id"], BACKGROUND);
        assert_eq!(note.params["activity_seq"], if i <= 5 { 1 } else { i - 4 });
    }
    f.event(PtyEvent::ForegroundChanged {
        pty_id: 11,
        session_id: CHILD.into(),
        run_generation: 1,
        foreground: Some(ForegroundHint {
            pid: 111,
            comm: "pi".into(),
            script_hint: None,
        }),
    })
    .await;
    assert_eq!(delta().params["activity_seq"], 5);
    f.event(PtyEvent::ExternalInput {
        pty_id: 11,
        session_id: CHILD.into(),
        run_generation: 1,
        byte_len: 1,
        from_session_id: PARENT.into(),
        from_host_id: "test".into(),
        is_remote: false,
    })
    .await;
    assert_eq!(delta().params["activity_seq"], 6);
    let note: Notification =
        serde_json::from_slice(&control.try_recv().unwrap()[HEADER_LEN..]).unwrap();
    assert_eq!(note.method, "host.external_input");
    assert!(control.try_recv().is_err());
    assert_eq!(std::fs::read(file).unwrap(), bytes);
    assert_eq!(file.metadata().unwrap().modified().unwrap(), mtime);

    let mut seen = TabMarkSeenParams {
        tab_id: CHILD.into(),
        run_generation: 1,
        seen_seq: 3,
    };
    assert_eq!(f.host.tab_mark_seen(seen.clone()).unwrap().seen_seq, 3);
    let note = delta();
    assert_eq!(note.method, "host.tab_seen_changed");
    assert_eq!(note.params["seen_seq"], 3);
    seen.seen_seq = 100;
    assert_eq!(f.host.tab_mark_seen(seen.clone()).unwrap().seen_seq, 6);
    assert_eq!(delta().params["seen_seq"], 6);
    seen.seen_seq = 2;
    assert_eq!(f.host.tab_mark_seen(seen.clone()).unwrap().seen_seq, 6);
    seen.run_generation = 2;
    assert_eq!(
        f.host
            .tab_mark_seen(seen.clone())
            .unwrap_err()
            .data
            .code
            .as_deref(),
        Some("generation_mismatch")
    );
    assert!(deltas.try_recv().is_err());

    f.event(PtyEvent::Exited {
        pty_id: 11,
        session_id: CHILD.into(),
        run_generation: 1,
        exit_code: Some(0),
        signal: None,
    })
    .await;
    assert!(!f.host.lock().activities.contains_key(CHILD));
    assert_eq!(
        serde_json::from_slice::<Notification>(&deltas.try_recv().unwrap())
            .unwrap()
            .method,
        "host.catalog_changed"
    );
    assert_eq!(
        f.host.tab_mark_seen(seen).unwrap_err().data.code.as_deref(),
        Some("session_not_found")
    );
}

#[tokio::test]
async fn activity_and_seen_rebuilds_from_pty_info() {
    let f = Fixture::new(fixture_catalog()).await;
    let foreground = ForegroundHint {
        pid: 41,
        comm: "pi".into(),
        script_hint: None,
    };
    let pty = |id: &str, pty_id, state| PtyInfo {
        pty_id,
        session_id: id.into(),
        run_generation: 1,
        pid: 41,
        cols: 80,
        rows: 24,
        cwd: None,
        title: String::new(),
        controller: None,
        foreground: Some(foreground.clone()),
        agent_status: Some(test_status(state, None)),
    };
    let ptys = [
        pty(PARENT, 10, AgentState::Idle),
        pty(CHILD, 11, AgentState::Blocked),
    ];
    let mut st = f.host.lock();
    st.activities.clear(); // No activity survives a Host restart.
    HostService::restore_activities(&mut st, &ptys);
    assert_eq!(st.activities[PARENT].activity_seq, 0);
    assert_eq!(st.activities[PARENT].seen_seq, 0);
    let activity = &st.activities[CHILD];
    assert_eq!((activity.activity_seq, activity.seen_seq), (1, 0));
    assert_eq!(activity.agent_status, ptys[1].agent_status);
    assert_eq!(activity.foreground, ptys[1].foreground);
    drop(st);
    f.host
        .tab_mark_seen(TabMarkSeenParams {
            tab_id: CHILD.into(),
            run_generation: 1,
            seen_seq: 1,
        })
        .unwrap();
    let mut st = f.host.lock();
    f.host.reconcile(&mut st, &ptys);
    assert_eq!(
        (
            st.activities[CHILD].activity_seq,
            st.activities[CHILD].seen_seq
        ),
        (1, 1)
    );
}

#[tokio::test]
async fn lifecycle_and_security_promote_restart_exit_and_close() {
    let mut catalog = fixture_catalog();
    catalog.rooms[1].tabs[0].agent_profile = Some("pi".into());
    let mut f = Fixture::new(catalog).await;
    let (tx, mut deltas) = mpsc::unbounded_channel();
    f.host.catalog_subscribe(1, 1, tx);
    let mut delta = || serde_json::from_slice::<Notification>(&deltas.try_recv().unwrap()).unwrap();
    f.event(PtyEvent::AgentStatusChanged {
        pty_id: 11,
        session_id: CHILD.into(),
        run_generation: 1,
        status: Some(test_status(AgentState::Blocked, None)),
    })
    .await;
    assert_eq!(delta().params["activity_seq"], 1);
    f.event(PtyEvent::ForegroundChanged {
        pty_id: 11,
        session_id: CHILD.into(),
        run_generation: 1,
        foreground: Some(ForegroundHint {
            pid: 111,
            comm: "pi".into(),
            script_hint: None,
        }),
    })
    .await;
    delta();
    f.host
        .tab_mark_seen(TabMarkSeenParams {
            tab_id: CHILD.into(),
            run_generation: 1,
            seen_seq: 1,
        })
        .unwrap();
    delta();
    let previous = f.host.lock().activities[CHILD].clone();

    f.host
        .tab_move(TabMoveParams {
            tab_id: CHILD.into(),
            target: SessionMoveTarget::ExistingRoom {
                room_id: VISIBLE.into(),
            },
        })
        .unwrap();
    assert_eq!(delta().method, "host.catalog_changed");
    let promoted = delta();
    assert_eq!(promoted.method, "host.tab_activity_changed");
    assert_eq!(promoted.params["room_id"], VISIBLE);
    assert_eq!(f.host.lock().activities[CHILD], previous);
    assert_eq!(
        f.host
            .lookup_tab_any(CHILD)
            .unwrap()
            .agent_profile
            .as_deref(),
        Some("pi")
    );

    for (id, pty_id, exit_code, state) in [(CHILD, 11, 7, "error"), (PARENT, 10, 0, "done")] {
        f.event(PtyEvent::Exited {
            pty_id,
            session_id: id.into(),
            run_generation: 1,
            exit_code: Some(exit_code),
            signal: None,
        })
        .await;
        assert_eq!(delta().method, "host.catalog_changed");
        let exit = delta();
        assert_eq!(exit.method, "host.tab_activity_changed");
        assert_eq!(exit.params["agent_status"]["state"], state);
        assert!(exit.params["foreground"].is_null());
        assert_eq!(exit.params["activity_seq"], if id == CHILD { 2 } else { 1 });
    }

    let host = f.host.clone();
    let restart = tokio::spawn(async move {
        host.tab_restart_with_visibility(
            TabRestartParams {
                tab_id: CHILD.into(),
                cols: 80,
                rows: 24,
            },
            false,
        )
        .await
        .unwrap()
    });
    let spawn = f.expect("pty.spawn").await;
    assert_eq!(spawn.params["run_generation"], 2);
    f.reply(spawn, json!({"pty_id": 13, "pid": 113})).await;
    let restarted = restart.await.unwrap().tab;
    assert_eq!(restarted.run_generation, 2);
    assert_eq!(restarted.agent_profile.as_deref(), Some("pi"));
    assert_eq!(delta().method, "host.catalog_changed");
    let reset = delta();
    assert_eq!(reset.params["run_generation"], 2);
    let activity = f.host.lock().activities[CHILD].clone();
    assert_eq!((activity.activity_seq, activity.seen_seq), (0, 0));
    assert!(activity.agent_status.is_none() && activity.foreground.is_none());

    f.host
        .tab_close_with_visibility(
            TabCloseParams {
                tab_id: PARENT.into(),
                request_id: None,
            },
            false,
        )
        .await
        .unwrap();
    assert!(!f.host.lock().activities.contains_key(PARENT));
}

#[tokio::test]
async fn lifecycle_and_security_derives_remote_parent_and_never_notifies_local_collision() {
    let mut f = Fixture::new(fixture_catalog()).await;
    let host = f.host.clone();
    let create = tokio::spawn(async move {
        host.session_local(
            "host.session_create",
            json!({
                "parent_session_id":CHILD, "parent_host_id":"forged", "agent_profile":"pi",
            }),
            &Caller {
                session_id: PARENT.into(),
                host_id: "remote".into(),
                is_remote: true,
            },
        )
        .await
        .unwrap()
    });
    let spawn = f.expect("pty.spawn").await;
    f.reply(spawn, json!({"pty_id": 12, "pid": 112})).await;
    let result = create.await.unwrap();
    let id = result["session"]["session_id"].as_str().unwrap();
    let tab = f.host.lookup_tab_any(id).unwrap();
    assert_eq!(tab.parent_session_id.as_deref(), Some(PARENT));
    assert_eq!(tab.parent_host_id.as_deref(), Some("remote"));
    assert_eq!(tab.agent_profile.as_deref(), Some("pi"));
    tokio::time::timeout(
        Duration::from_secs(1),
        f.event(PtyEvent::Exited {
            pty_id: 12,
            session_id: id.into(),
            run_generation: 1,
            exit_code: Some(0),
            signal: None,
        }),
    )
    .await
    .expect("must not call session.send to the matching local ID");
    assert!(f.host.lookup_tab_any(id).is_err());
    assert!(!f.host.lock().activities.contains_key(id));
}

#[tokio::test]
async fn lifecycle_and_security_rejects_invalid_executor_and_profile() {
    let f = Fixture::new(fixture_catalog()).await;
    let cases = [
        ("DDG_HOST_SOCK", "value"),
        ("ddg_session_token", "value"),
        ("Ddg_Foo", "value"),
        ("=", "value"),
        ("", "value"),
        ("K\0", "value"),
        ("K", "value\0"),
    ];
    for (key, value) in cases {
        let mut params = tab_create(key);
        params.executor = ExecutorSpec::Command {
            argv: vec!["cat".into()],
            env: [(key.into(), value.into())].into(),
        };
        let err = f.host.tab_create(params).await.unwrap_err();
        assert_eq!(err.data.kind, ErrorKind::InvalidParams);
    }
    let mut params = tab_create("profile");
    params.agent_profile = Some("../pi".into());
    assert_eq!(
        f.host.tab_create(params).await.unwrap_err().data.kind,
        ErrorKind::InvalidParams
    );

    let mut catalog = fixture_catalog();
    catalog.rooms[0].tabs[0].executor = ExecutorSpec::Command {
        argv: vec!["cat".into()],
        env: [
            ("ddg_custom".into(), "bad".into()),
            ("Ddg_Foo".into(), "bad".into()),
            ("SAFE".into(), "ok".into()),
        ]
        .into(),
    };
    save_catalog(&f.host.paths.catalog_file, &catalog, None).unwrap();
    let (loaded, _) = load_catalog(&f.host.paths.catalog_file, "test").unwrap();
    let ExecutorSpec::Command { env, .. } = &loaded.rooms[0].tabs[0].executor else {
        panic!("command")
    };
    assert_eq!(*env, [("SAFE".into(), "ok".into())].into());
}

#[tokio::test]
async fn audit_log_is_private_and_only_records_metadata() {
    let mut f = Fixture::new(fixture_catalog()).await;
    let token = f.host.lock().tokens.issue(PARENT, 1).unwrap();
    let host = f.host.clone();
    let params = json!({"session_token": token, "to_session_id": CHILD, "data_b64": "c2VjcmV0LXBheWxvYWQ=", "from_session_id": CHILD});
    let send = tokio::spawn(async move { host.session_rpc("host.msg_send", params).await });
    let req = f.expect("session.send").await;
    assert_eq!(req.params["from_session_id"], PARENT);
    assert_eq!(req.params["from_host_id"], "test");
    assert!(req.params.get("session_token").is_none());
    f.reply(req, json!({"message_id": "msg_00000000000000000000000001"}))
        .await;
    send.await.unwrap().unwrap();
    f.host.audit("unit", "a\nb\u{202e}c");
    let path = f.root.join("host.log");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let audit = std::fs::read_to_string(path).unwrap();
    assert!(audit.contains(&format!("msg_send {PARENT}\n")));
    assert!(audit.ends_with("unit abc\n"));
    assert!(
        !audit.contains(&token)
            && !audit.contains("secret-payload")
            && !audit.contains("c2VjcmV0LXBheWxvYWQ=")
    );
}
