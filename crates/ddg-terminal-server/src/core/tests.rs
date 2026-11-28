use super::*;
use ddg_agents_proto::frame::{FrameKind, INITIAL_CREDIT, decode_header};
use ddg_agents_proto::id::encode_base64;
use ddg_agents_proto::terminal::{ControlLease, SessionMessage, SessionMessagePayload};
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::mpsc::Receiver;
use std::task::Poll;

const SESSION: &str = "ses_00000000000000000000000001";

fn core() -> (Core, Receiver<Vec<u8>>) {
    let (tx, rx) = mpsc::channel();
    (
        Core::new(
            ServerIdentity {
                server_id: "test".into(),
                boot_generation: 1,
            },
            tx,
            HashMap::new(),
            1,
            false,
            "test".into(),
        ),
        rx,
    )
}

async fn spawn(core: &Core, holder: &Receiver<Vec<u8>>, run_generation: u64) -> u32 {
    let params = PtySpawnParams {
        session_id: SESSION.into(),
        run_generation,
        cols: 80,
        rows: 24,
        cwd: None,
        argv: None,
        env: None,
    };
    let (result, ()) = tokio::join!(core.spawn_pty(params), async {
        let bytes = holder.try_recv().unwrap();
        let (kind, pty_id, payload) = hproto::read_frame(&mut bytes.as_slice()).unwrap().unwrap();
        assert_eq!(kind, hproto::SPAWN);
        let spec: hproto::HolderSpawn = hproto::parse_json(&payload).unwrap();
        assert_eq!(
            (&*spec.session_id, spec.run_generation),
            (SESSION, run_generation)
        );
        core.handle_spawned(
            pty_id,
            hproto::HolderSpawned {
                pid: 999,
                session_id: spec.session_id,
                run_generation: spec.run_generation,
            },
        );
    });
    result.unwrap().pty_id
}

fn message() -> SessionSendParams {
    SessionSendParams {
        to_session_id: SESSION.into(),
        from_session_id: SESSION.into(),
        from_host_id: "test".into(),
        is_remote: false,
        payload: SessionMessagePayload::Message {
            data_b64: "Zg==".into(),
        },
    }
}

fn recv(wait_ms: u64) -> SessionRecvParams {
    SessionRecvParams {
        session_id: SESSION.into(),
        max_messages: 256,
        wait_ms,
    }
}

fn inbox(core: &Core) -> (Arc<Conn>, u32) {
    let conn = Arc::new(Conn::new(core.next_conn_id()));
    let result = core
        .inbox_subscribe(
            &conn,
            SessionInboxSubscribeParams {
                session_id: SESSION.into(),
            },
        )
        .unwrap();
    conn.with_outbox(|out| out.resume(result.channel));
    (conn, result.channel)
}

fn spend_credit(conn: &Conn, channel: u32, bytes: usize) {
    conn.with_outbox(|out| assert!(out.data(channel, &vec![b'x'; bytes])));
    while conn.next_frame().is_some() {}
}

fn frame(conn: &Conn, channel: u32, kind: FrameKind) -> Vec<u8> {
    let bytes = conn.next_frame().expect("expected frame");
    let (actual_channel, actual_kind, len) = decode_header(bytes[..9].try_into().unwrap()).unwrap();
    assert_eq!(
        (actual_channel, actual_kind, len),
        (channel, kind, bytes.len() - 9)
    );
    bytes[9..].to_vec()
}

#[test]
fn recovery_releases_exited_pty_and_preserves_live_metadata() {
    let (mut core, mut holder) = LocalStream::pair().unwrap();
    let mut exited = hproto::HeldPty {
        pty_id: 9,
        pid: 99,
        cols: 80,
        rows: 24,
        exit: None,
        raw_offset: 0,
        session_id: SESSION.into(),
        run_generation: 7,
    };
    let live = exited.clone();
    exited.pty_id = 11;
    exited.exit = Some(hproto::HolderExit {
        code: Some(0),
        signal: None,
    });
    let welcome = hproto::HolderWelcome {
        version: 1,
        instance: "test".into(),
        pid: 1,
        ptys: vec![live, exited],
        features: vec![],
    };
    // Wait for RELEASE before sending tails, so deferred release deadlocks/fails.
    core.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    holder
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let exchange = std::thread::spawn(move || {
        holder
            .write_all(&hproto::encode_json(hproto::WELCOME, 0, &welcome))
            .unwrap();
        assert_eq!(
            hproto::read_frame(&mut holder).unwrap(),
            Some((hproto::RELEASE, 11, vec![]))
        );
        holder
            .write_all(&hproto::encode_frame(hproto::TAIL, 9, b"live"))
            .unwrap();
        holder
            .write_all(&hproto::encode_frame(hproto::TAIL, 11, b"exited"))
            .unwrap();
    });
    let (ptys, next_id, holder_fg_pgrp, instance) = recover_ptys(&mut core).unwrap();
    assert_eq!(instance, "test");
    assert!(!holder_fg_pgrp);
    exchange.join().unwrap();
    assert_eq!((ptys.len(), next_id), (1, 12));
    assert_eq!(
        (&*ptys[&9].session_id, ptys[&9].run_generation),
        (SESSION, 7)
    );
    assert!(ptys[&9].vt.plain_text().contains("live"));
}

#[tokio::test]
async fn validates_screen_and_input_and_preserves_gui_control() {
    let (core, holder) = core();
    let pty_id = spawn(&core, &holder, 7).await;
    core.handle_output(pty_id, "ignore\r\n한글🙂\r\n".as_bytes());
    let read = |lines, max_bytes| {
        core.read_screen(SessionReadScreenParams {
            session_id: SESSION.into(),
            lines,
            max_bytes,
        })
    };
    for (lines, max_bytes) in [(0, 1), (1, 0), (2001, 1), (1, 262_145)] {
        assert_eq!(
            read(lines, max_bytes).unwrap_err().data.code.as_deref(),
            Some("invalid_params")
        );
    }
    for (len, text, truncated) in [(5, "한", true), (9, "한글", true), (10, "한글🙂", false)]
    {
        let result = read(1, len).unwrap();
        assert_eq!(
            (&*result.text, result.run_generation, result.truncated),
            (text, 7, truncated)
        );
    }
    let gui = Arc::new(Conn::new(1));
    let attached = core
        .attach(
            &gui,
            PtyAttachParams {
                pty_id,
                want_control: true,
                cols: 80,
                rows: 24,
                history_lines: 0,
            },
        )
        .unwrap();
    assert_eq!(attached.mode, ControlMode::Control);
    let events = Arc::new(Conn::new(2));
    let channel = core.subscribe(&events).channel.unwrap();
    events.with_outbox(|out| out.resume(channel));
    let write = |data_b64, expect_run_generation| {
        core.write_input(SessionWriteInputParams {
            session_id: SESSION.into(),
            data_b64,
            expect_run_generation,
            from_session_id: "caller".into(),
            from_host_id: "host".into(),
            is_remote: true,
        })
    };
    for bad in [
        String::new(),
        "!!!!".into(),
        encode_base64(&vec![0; 65_537]),
    ] {
        assert_eq!(
            write(bad, None).unwrap_err().data.code.as_deref(),
            Some("invalid_params")
        );
    }
    assert_eq!(
        write("Zg==".into(), Some(6))
            .unwrap_err()
            .data
            .code
            .as_deref(),
        Some("generation_mismatch")
    );
    assert!(holder.try_recv().is_err());
    let result = write("Zm9v".into(), Some(7)).unwrap();
    assert_eq!((result.byte_len, result.run_generation), (3, 7));
    assert_eq!(
        holder.try_recv().unwrap(),
        hproto::encode_frame(hproto::INPUT, pty_id, b"foo")
    );
    assert_eq!(
        core.list_ptys()[0].controller,
        Some(ControlLease {
            conn_id: gui.id,
            epoch: attached.control_epoch
        })
    );
    let event: serde_json::Value =
        serde_json::from_slice(&frame(&events, channel, FrameKind::Data)).unwrap();
    assert_eq!(
        event["params"]["event"],
        json!({
            "type": "external_input", "session_id": SESSION, "pty_id": pty_id, "run_generation": 7,
            "byte_len": 3, "from_session_id": "caller", "from_host_id": "host", "is_remote": true,
        })
    );
}

#[tokio::test]
async fn mailbox_waits_and_purges_on_generation_bump_exit_and_kill() {
    let (core, holder) = core();
    assert_eq!(
        core.session_send(message())
            .unwrap_err()
            .data
            .code
            .as_deref(),
        Some("session_not_found")
    );
    let old = spawn(&core, &holder, 1).await;
    let (result, ()) = tokio::join!(core.session_recv(recv(1000)), async {
        core.session_send(message()).unwrap();
    });
    assert_eq!(result.unwrap().messages.len(), 1);
    assert!(
        core.session_recv(recv(1))
            .await
            .unwrap()
            .messages
            .is_empty()
    );
    assert_eq!(
        core.session_recv(SessionRecvParams {
            max_messages: 0,
            ..recv(0)
        })
        .await
        .unwrap_err()
        .data
        .code
        .as_deref(),
        Some("invalid_params")
    );
    assert_eq!(
        core.session_send(SessionSendParams {
            payload: SessionMessagePayload::Message {
                data_b64: "!".into()
            },
            ..message()
        })
        .unwrap_err()
        .data
        .code
        .as_deref(),
        Some("invalid_params")
    );

    let (sub, ch) = inbox(&core);
    spend_credit(&sub, ch, INITIAL_CREDIT);
    let id = core.session_send(message()).unwrap().message_id;
    assert!(sub.next_frame().is_none());
    sub.with_outbox(|out| out.credit(ch, 512));
    core.refill_inbox(&sub);
    let bytes = frame(&sub, ch, FrameKind::Data);
    let delivered: SessionMessage = serde_json::from_slice(&bytes).unwrap();
    spend_credit(&sub, ch, 512 - bytes.len());
    assert_eq!(delivered.message_id, id);
    assert!(ddg_agents_proto::id::parse_message_id(&id).is_ok());
    assert!(
        core.session_recv(recv(0))
            .await
            .unwrap()
            .messages
            .is_empty()
    );

    let (wait, new) = tokio::join!(core.session_recv(recv(1000)), async {
        // Leave a message in the old generation's queue behind the stalled stream.
        core.session_send(message()).unwrap();
        assert!(sub.next_frame().is_none());
        spawn(&core, &holder, 2).await
    });
    assert!(wait.unwrap().messages.is_empty());
    frame(&sub, ch, FrameKind::Close);
    assert!(
        core.session_recv(recv(0))
            .await
            .unwrap()
            .messages
            .is_empty()
    );
    core.session_send(message()).unwrap();
    let exit = hproto::HolderExit {
        code: Some(0),
        signal: None,
    };
    core.handle_exited(old, exit);
    assert_eq!(
        holder.try_recv().unwrap(),
        hproto::encode_frame(hproto::RELEASE, old, &[])
    );
    assert_eq!(core.session_recv(recv(0)).await.unwrap().messages.len(), 1);
    let (sub, ch) = inbox(&core);
    spend_credit(&sub, ch, INITIAL_CREDIT);
    let (wait, ()) = tokio::join!(core.session_recv(recv(1000)), async {
        core.session_send(message()).unwrap();
        core.handle_exited(new, exit);
    });
    assert!(wait.unwrap().messages.is_empty());
    frame(&sub, ch, FrameKind::Close);
    assert_eq!(
        holder.try_recv().unwrap(),
        hproto::encode_frame(hproto::RELEASE, new, &[])
    );

    let pty_id = spawn(&core, &holder, 3).await;
    let (sub, ch) = inbox(&core);
    spend_credit(&sub, ch, INITIAL_CREDIT);
    core.session_send(message()).unwrap();
    let (result, ()) = tokio::join!(core.kill_pty(PtyKillParams { pty_id }), async {
        frame(&sub, ch, FrameKind::Close);
        assert!(core.session_send(message()).is_err());
        core.handle_exited(pty_id, exit);
    });
    result.unwrap();
}

fn kill_with_test_timeouts(
    core: &Core,
    pty_id: u32,
) -> impl Future<Output = Result<PtyKillResult, RpcError>> + '_ {
    core.kill_pty_with_timeouts(
        PtyKillParams { pty_id },
        #[cfg(unix)]
        Duration::from_millis(30),
        Duration::from_millis(50),
        Duration::from_millis(80),
    )
}

async fn expect_kill_signal(
    mut kill: Pin<&mut impl Future<Output = Result<PtyKillResult, RpcError>>>,
    holder: &Receiver<Vec<u8>>,
    pty_id: u32,
    signal: i32,
) {
    poll_fn(|cx| {
        assert!(kill.as_mut().poll(cx).is_pending());
        match holder.try_recv() {
            Ok(bytes) => {
                assert_eq!(
                    bytes,
                    hproto::encode_json(hproto::SIGNAL, pty_id, &hproto::HolderSignal { signal })
                );
                Poll::Ready(())
            }
            Err(mpsc::TryRecvError::Empty) => Poll::Pending,
            Err(err) => panic!("fake Holder disconnected: {err}"),
        }
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn kill_pty_sends_hangup_first_and_stops_when_child_exits() {
    let (core, holder) = core();
    let pty_id = spawn(&core, &holder, 1).await;
    let (sub, channel) = inbox(&core);
    spend_credit(&sub, channel, INITIAL_CREDIT);
    core.session_send(message()).unwrap();
    let mut kill = std::pin::pin!(kill_with_test_timeouts(&core, pty_id));
    expect_kill_signal(kill.as_mut(), &holder, pty_id, crate::pty::HANGUP).await;
    let reason: CloseReason =
        serde_json::from_slice(&frame(&sub, channel, FrameKind::Close)).unwrap();
    assert_eq!(
        reason,
        CloseReason::Killed {
            signal: Some(crate::pty::HANGUP)
        }
    );
    assert!(core.session_send(message()).is_err());
    core.handle_exited(
        pty_id,
        hproto::HolderExit {
            code: Some(42),
            signal: None,
        },
    );
    assert_eq!(
        kill.await.unwrap(),
        PtyKillResult {
            exit_code: Some(42),
            signal: None,
        }
    );
    assert_eq!(
        holder.try_recv().unwrap(),
        hproto::encode_frame(hproto::RELEASE, pty_id, &[])
    );
    assert!(holder.try_recv().is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn kill_pty_escalates_hangup_to_term_and_stops_on_term_exit() {
    let (core, holder) = core();
    let pty_id = spawn(&core, &holder, 1).await;
    let mut kill = std::pin::pin!(kill_with_test_timeouts(&core, pty_id));
    expect_kill_signal(kill.as_mut(), &holder, pty_id, crate::pty::HANGUP).await;
    expect_kill_signal(kill.as_mut(), &holder, pty_id, crate::pty::TERMINATE).await;
    core.handle_exited(
        pty_id,
        hproto::HolderExit {
            code: Some(0),
            signal: None,
        },
    );
    assert_eq!(
        kill.await.unwrap(),
        PtyKillResult {
            exit_code: Some(0),
            signal: None,
        }
    );
    assert_eq!(
        holder.try_recv().unwrap(),
        hproto::encode_frame(hproto::RELEASE, pty_id, &[])
    );
    assert!(holder.try_recv().is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn kill_pty_escalates_hangup_term_to_kill_and_errors_on_reap_timeout() {
    let (core, holder) = core();
    let pty_id = spawn(&core, &holder, 1).await;
    let mut kill = std::pin::pin!(kill_with_test_timeouts(&core, pty_id));
    expect_kill_signal(kill.as_mut(), &holder, pty_id, crate::pty::HANGUP).await;
    expect_kill_signal(kill.as_mut(), &holder, pty_id, crate::pty::TERMINATE).await;
    expect_kill_signal(kill.as_mut(), &holder, pty_id, crate::pty::KILL).await;
    let err = kill.await.unwrap_err();
    assert_eq!(err.data.kind, ErrorKind::Unavailable);
    assert_eq!(err.message, "timed out waiting for PTY child reap");
    assert!(holder.try_recv().is_err());
}

#[tokio::test]
async fn kill_pty_returns_unavailable_if_holder_disconnects_before_escalation() {
    let (core, holder) = core();
    let pty_id = spawn(&core, &holder, 1).await;
    let mut kill = std::pin::pin!(kill_with_test_timeouts(&core, pty_id));
    #[cfg(unix)]
    let initial = crate::pty::HANGUP;
    #[cfg(not(unix))]
    let initial = crate::pty::TERMINATE;
    expect_kill_signal(kill.as_mut(), &holder, pty_id, initial).await;
    core.handle_holder_disconnect();
    let err = poll_fn(|cx| {
        let Poll::Ready(result) = kill.as_mut().poll(cx) else {
            panic!("kill did not wake on Holder disconnect");
        };
        Poll::Ready(result.unwrap_err())
    })
    .await;
    assert_eq!(err.data.kind, ErrorKind::Unavailable);
    assert_eq!(err.message, "holder disconnected during kill");
    assert!(holder.try_recv().is_err());
}
