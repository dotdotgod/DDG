#![cfg(unix)]

mod common;

use common::{HostSandbox, code, control_note, executor};
use ddg_agents_proto::frame::FrameKind;
use ddg_agents_proto::host::{
    RoomVisibility, SessionCreateResult, SessionMoveResult, SessionRestartResult, TabRestartResult,
};
use ddg_agents_proto::id::encode_base64;
use ddg_agents_proto::terminal::{SessionMessagePayload, SessionRecvResult};
use serde_json::{Value, json};

#[test]
fn restart_rotates_token_and_inbox_promotion_keeps_identity_and_hidden_exit_notifies_parent() {
    let mut sb = HostSandbox::new("lifecycle");
    sb.start_host();
    let mut host = sb.connect();
    let visible = host.visible();
    let parent_id = &visible.room.tabs[0].id;
    let (parent_token, parent_room) = sb.get_token(parent_id);
    assert_eq!(parent_room, visible.room.id);

    let child: SessionCreateResult = host
        .call(
            "host.session_create",
            json!({
                "session_token": parent_token, "executor": executor("exec cat"),
                "title": "Bo\nu\u{202e}nd", "parent_session_id": "ses_00000000000000000000000042",
            }),
        )
        .unwrap();
    let id = &child.session.session_id;
    assert_eq!(child.session.visibility, RoomVisibility::Hidden);
    assert_eq!(
        child.session.parent_session_id.as_deref(),
        Some(parent_id.as_str())
    );
    assert_eq!(child.session.title, "Bound");
    let (old_token, original_room) = sb.get_token(id);
    assert_eq!(original_room, child.session.room_id);
    let _: Value = host
        .call(
            "host.msg_send",
            json!({"session_token": parent_token, "to_session_id": id, "data_b64": "b2xk"}),
        )
        .unwrap();

    let restarted: SessionRestartResult = host
        .call(
            "host.session_restart",
            json!({"session_token": parent_token, "session_id": id}),
        )
        .unwrap();
    assert_eq!(&restarted.session.session_id, id);
    assert_eq!(restarted.session.run_generation, 2);
    let (new_token, _) = sb.get_token(id);
    assert!(old_token != new_token);
    assert_eq!(
        code(
            host.call::<Value>("host.session_list", json!({"session_token": old_token}))
                .unwrap_err()
        ),
        "token_revoked"
    );
    let after_restart: SessionRecvResult = host
        .call(
            "host.msg_recv",
            json!({"session_token": new_token, "max_messages": 8, "wait_ms": 0}),
        )
        .unwrap();
    assert!(after_restart.messages.is_empty());

    let promoted: SessionMoveResult = host.call("host.session_move", json!({
        "session_token": new_token, "session_id": id, "target": {"kind":"new_room", "name":"Shown\n\u{200f}"},
    })).unwrap();
    assert_eq!(promoted.session.visibility, RoomVisibility::Visible);
    assert_eq!(&promoted.session.session_id, id);
    assert_eq!(promoted.session.runtime, restarted.session.runtime);
    assert_eq!(
        host.catalog()
            .catalog
            .rooms
            .iter()
            .find(|r| r.id == promoted.session.room_id)
            .unwrap()
            .name
            .as_deref(),
        Some("Shown")
    );
    let no_demotion = host.call::<Value>("host.session_move", json!({
        "session_token": new_token, "session_id": id, "target": {"kind":"existing_room", "room_id": original_room},
    })).unwrap_err();
    assert_eq!(code(no_demotion), "permission_denied");
    let tab_restart: TabRestartResult = host
        .call(
            "host.tab_restart",
            json!({"tab_id": id, "cols":80, "rows":24}),
        )
        .unwrap();
    assert_eq!(tab_restart.tab.run_generation, 3);
    assert_eq!(sb.get_token(id).1, promoted.session.room_id);

    let exiting = host.hidden(&parent_token, "read line; exit 7");
    let exiting_id = &exiting.session.session_id;
    let (exit_token, _) = sb.get_token(exiting_id);
    let channel = host.catalog().channel.unwrap();
    let _: Value = host.call("host.session_write", json!({
        "session_token": parent_token, "session_id": exiting_id, "data_b64": encode_base64(b"finish\n"), "expect_run_generation": 1,
    })).unwrap();
    let first = host.read_where(|frame| {
        if let Some(note) = control_note(frame) {
            return note["method"] == "host.session_exited"
                && note["params"]["session_id"] == *exiting_id;
        }
        frame.channel == channel
            && frame.kind == FrameKind::Data
            && catalog_missing(&frame.payload, exiting_id)
    });
    let exit = control_note(&first).expect("session exit must precede catalog removal");
    assert_eq!(exit["params"]["exit_code"], 7);
    host.read_where(|f| {
        f.channel == channel && f.kind == FrameKind::Data && catalog_missing(&f.payload, exiting_id)
    });
    assert_eq!(
        code(
            host.call::<Value>("host.session_list", json!({"session_token": exit_token}))
                .unwrap_err()
        ),
        "token_revoked"
    );
    let inbox: SessionRecvResult = host
        .call(
            "host.msg_recv",
            json!({"session_token": parent_token, "max_messages":8, "wait_ms":1000}),
        )
        .unwrap();
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(inbox.messages[0].from_session_id, *exiting_id);
    assert_eq!(
        inbox.messages[0].payload,
        SessionMessagePayload::SessionExited {
            session_id: exiting_id.clone(),
            room_id: exiting.session.room_id,
            run_generation: 1,
            exit_code: Some(7),
        }
    );
}

fn catalog_missing(bytes: &[u8], session: &str) -> bool {
    let note: Value = serde_json::from_slice(bytes).unwrap();
    note["method"] == "host.catalog_changed"
        && !note["params"]["catalog"]["rooms"]
            .as_array()
            .unwrap()
            .iter()
            .any(|room| {
                room["tabs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tab| tab["id"] == session)
            })
}
