#![cfg(unix)]

mod common;

use common::{HostSandbox, code, control_note, executor};
use ddg_agents_proto::host::{
    Catalog, ExecutorSpec, PaneNode, RoomKind, RoomLayout, RoomRecord, RoomVisibility,
    SessionMoveTarget, SessionScreenResult, SplitAxis, TabCreateResult, TabSplitParams,
    TabSplitResult,
};
use ddg_agents_proto::id::{background_room_id, encode_base64};
use ddg_agents_proto::terminal::{ControlMode, SessionMessagePayload, SessionRecvResult};
use ddg_agents_transport::{HostClient, HostProfile, HostTransport};
use serde_json::{Value, json};
use std::time::Duration;

#[tokio::test]
async fn operator_can_message_attach_restart_and_close_hidden_tabs() {
    let mut sb = HostSandbox::new("op-background");
    sb.start_host();
    let mut inbox = sb.connect();
    let (operator, _events) = HostClient::connect_once(
        HostProfile {
            transport: HostTransport::Local {
                sock_path: Some(sb.run.join("host.sock").display().to_string()),
            },
            ..HostProfile::local_default()
        },
        "operator-test",
    )
    .await
    .unwrap();
    let tab = operator
        .tab_create_ext(
            None,
            None,
            None,
            serde_json::from_value(executor("exec cat")).unwrap(),
            80,
            24,
            None,
        )
        .await
        .unwrap()
        .tab;
    let (token, room_id) = sb.get_token(&tab.id);
    assert!(room_id.starts_with("rm_bg_"));
    let rename = inbox
        .call::<Value>(
            "host.room_rename",
            json!({"room_id": room_id, "name": "No"}),
        )
        .unwrap_err();
    assert_eq!(code(rename), "permission_denied");

    for invalid in [
        String::new(),
        "not-base64".into(),
        encode_base64(&vec![b'x'; 65537]),
    ] {
        let err = operator
            .tab_msg_send(tab.id.clone(), invalid)
            .await
            .unwrap_err();
        assert_eq!(code(err), "invalid_params");
    }
    let data_b64 = encode_base64(b"hello-from-op");
    let sent = operator
        .tab_msg_send(tab.id.clone(), data_b64.clone())
        .await
        .unwrap();
    let received: SessionRecvResult = inbox
        .call(
            "host.msg_recv",
            json!({"session_token": token, "max_messages": 8, "wait_ms": 0}),
        )
        .unwrap();
    assert_eq!(received.messages.len(), 1);
    let message = &received.messages[0];
    assert_eq!(message.message_id, sent.message_id);
    assert_eq!(message.to_session_id, tab.id);
    assert_eq!(message.from_session_id, "operator");
    assert_eq!(message.from_host_id, operator.hello().unwrap().host_id);
    assert!(!message.is_remote);
    assert_eq!(
        message.payload,
        SessionMessagePayload::Message {
            data_b64: data_b64.clone()
        }
    );
    let log = std::fs::read_to_string(sb.state.join("host.log")).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.ends_with(" tab_msg_send operator"))
            .count(),
        1
    );
    assert!(!log.contains("hello-from-op") && !log.contains(&data_b64));

    let attached = operator
        .tab_attach(tab.id.clone(), false, 80, 24, 0)
        .await
        .unwrap();
    assert_eq!(attached.mode, ControlMode::Readonly);
    assert_eq!(attached.run_generation, tab.run_generation);
    operator
        .tab_claim_control(tab.id.clone(), false)
        .await
        .unwrap();
    assert!(
        operator
            .tab_release_control(tab.id.clone())
            .await
            .unwrap()
            .released
    );

    let restarted = operator
        .tab_restart(tab.id.clone(), 80, 24)
        .await
        .unwrap()
        .tab;
    assert_eq!(restarted.id, tab.id);
    assert_eq!(restarted.run_generation, tab.run_generation + 1);
    assert_eq!(sb.get_token(&tab.id).1, room_id);
    let closed = operator.tab_close(tab.id.clone(), None).await.unwrap();
    assert!(!closed.removed_room);
    let catalog = inbox.catalog().catalog;
    let room = catalog
        .rooms
        .iter()
        .find(|room| room.id == room_id)
        .unwrap();
    assert_eq!(room.visibility, RoomVisibility::Hidden);
    assert!(room.tabs.is_empty());
    operator.close();
}

#[tokio::test]
async fn operator_splits_and_persists_room_layout() {
    let mut sb = HostSandbox::new("op-split");
    sb.start_host();
    let mut ui = sb.connect();
    let (operator, _events) = HostClient::connect_once(
        HostProfile {
            transport: HostTransport::Local {
                sock_path: Some(sb.run.join("host.sock").display().to_string()),
            },
            ..HostProfile::local_default()
        },
        "operator-test",
    )
    .await
    .unwrap();
    let root = sb.root.display().to_string();
    let cwd = sb.space.display().to_string();
    let room = operator
        .room_create(
            None,
            Some(root.clone()),
            serde_json::from_value(executor(&format!(
                "printf '\\033]7;file://localhost%s\\007' '{cwd}'; exec cat"
            )))
            .unwrap(),
            80,
            24,
            None,
        )
        .await
        .unwrap()
        .room;
    let id = room.id;
    let anchor = &room.tabs[0].id;
    common::eventually(|| {
        let catalog = ui.catalog().catalog;
        let tab = &catalog.rooms[0].tabs[0];
        assert_eq!(tab.space_cwd.as_deref(), Some(root.as_str()));
        (tab.last_cwd.as_deref() == Some(cwd.as_str())).then_some(())
    });

    let command = serde_json::from_value::<ExecutorSpec>(executor("pwd -P; exec cat")).unwrap();
    let params = TabSplitParams {
        agent_profile: None,
        request_id: Some("split-once".into()),
        anchor_tab_id: anchor.clone(),
        axis: SplitAxis::Horizontal,
        ratio_bp: None,
        space_cwd: None,
        title: Some("right".into()),
        executor: command.clone(),
        cols: 80,
        rows: 24,
    };
    let first = operator.tab_split(params.clone()).await.unwrap();
    assert_eq!(operator.tab_split(params.clone()).await.unwrap(), first);
    assert_eq!(first.tab.title, "right");
    assert_eq!(first.tab.space_cwd.as_deref(), Some(cwd.as_str()));
    let physical_cwd = sb.space.canonicalize().unwrap();
    common::eventually(|| {
        let screen: SessionScreenResult = ui
            .call("host.tab_screen", json!({"tab_id": first.tab.id}))
            .unwrap();
        screen
            .text
            .contains(physical_cwd.to_str().unwrap())
            .then_some(())
    });

    let horizontal = |ratio_bp, second| PaneNode::Split {
        axis: SplitAxis::Horizontal,
        ratio_bp,
        first: Box::new(PaneNode::leaf(anchor.clone())),
        second: Box::new(second),
    };
    let right = PaneNode::leaf(first.tab.id.clone());
    assert_eq!(
        first.room.layout.unwrap().groups,
        [horizontal(5000, right.clone())]
    );
    let nested = operator
        .tab_split(TabSplitParams {
            request_id: None,
            anchor_tab_id: first.tab.id.clone(),
            axis: SplitAxis::Vertical,
            ratio_bp: Some(6500),
            space_cwd: Some(root.clone()),
            ..params.clone()
        })
        .await
        .unwrap();
    assert_eq!(nested.tab.space_cwd.as_deref(), Some(root.as_str()));
    let vertical = PaneNode::Split {
        axis: SplitAxis::Vertical,
        ratio_bp: 6500,
        first: Box::new(right.clone()),
        second: Box::new(PaneNode::leaf(nested.tab.id.clone())),
    };
    assert_eq!(
        nested.room.layout.unwrap().groups,
        [horizontal(5000, vertical.clone())]
    );

    let updated = RoomLayout {
        groups: vec![horizontal(4000, vertical)],
    };
    let changed = operator
        .room_set_layout(id.clone(), Some(updated.clone()))
        .await
        .unwrap();
    assert_eq!(changed.room.layout.as_ref(), Some(&updated));
    let invalid = RoomLayout {
        groups: vec![PaneNode::leaf(anchor.clone())],
    };
    assert_eq!(
        code(
            operator
                .room_set_layout(id.clone(), Some(invalid))
                .await
                .unwrap_err()
        ),
        "invalid_params"
    );
    let on_disk = std::fs::read(sb.state.join("catalog.json")).unwrap();
    assert_eq!(
        Catalog::from_json_bytes(&on_disk).unwrap().rooms[0]
            .layout
            .as_ref(),
        Some(&updated)
    );
    assert_eq!(
        ui.catalog().catalog.rooms[0].layout.as_ref(),
        Some(&updated)
    );

    let regular = operator
        .tab_create(id.clone(), None, command.clone(), 80, 24, None)
        .await
        .unwrap()
        .tab;
    let hidden = operator
        .tab_create_ext(None, None, None, command, 80, 24, None)
        .await
        .unwrap()
        .tab;
    let hidden_room = ui
        .catalog()
        .catalog
        .rooms
        .into_iter()
        .find(|r| r.kind == RoomKind::Background)
        .unwrap();
    assert_eq!(
        code(
            operator
                .room_set_layout(hidden_room.id, None)
                .await
                .unwrap_err()
        ),
        "permission_denied"
    );
    assert_eq!(
        code(
            operator
                .tab_split(TabSplitParams {
                    request_id: None,
                    anchor_tab_id: hidden.id.clone(),
                    ..params
                })
                .await
                .unwrap_err()
        ),
        "permission_denied"
    );
    operator
        .tab_move(
            hidden.id.clone(),
            SessionMoveTarget::ExistingRoom {
                room_id: id.clone(),
            },
        )
        .await
        .unwrap();

    let mut expected = updated;
    expected.append_tab(regular.id);
    expected.append_tab(hidden.id);
    assert_eq!(
        ui.catalog().catalog.rooms[0].layout.as_ref(),
        Some(&expected)
    );
    operator.tab_close(nested.tab.id, None).await.unwrap();
    expected.groups[0] = horizontal(4000, right);
    assert_eq!(
        ui.catalog().catalog.rooms[0].layout.as_ref(),
        Some(&expected)
    );
    operator
        .tab_close(first.tab.id.clone(), None)
        .await
        .unwrap();
    let room = ui.catalog().catalog.rooms.remove(0);
    assert!(room.layout.is_none());

    let mut reordered = RoomLayout::default_for_tabs(&room.tabs);
    reordered.groups.swap(0, 1);
    let custom = operator
        .room_set_layout(id.clone(), Some(reordered.clone()))
        .await
        .unwrap();
    assert_eq!(custom.room.layout, Some(reordered));
    assert!(
        operator
            .room_set_layout(id.clone(), Some(RoomLayout::default_for_tabs(&room.tabs)))
            .await
            .unwrap()
            .room
            .layout
            .is_none()
    );
    ui.call::<TabSplitResult>(
        "host.tab_split",
        json!({
            "anchor_tab_id": anchor, "axis": "horizontal", "executor": executor("exec cat"),
            "cols": 80, "rows": 24,
        }),
    )
    .unwrap();
    assert!(
        operator
            .room_set_layout(id.clone(), None)
            .await
            .unwrap()
            .room
            .layout
            .is_none()
    );
    let log = std::fs::read_to_string(sb.state.join("host.log")).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.ends_with(&format!(" tab_split {}", first.tab.id)))
            .count(),
        1
    );
    assert!(
        log.lines()
            .any(|line| line.ends_with(&format!(" room_set_layout {id}")))
    );
    operator.close();
}

#[test]
fn write_capable_plugins_cannot_split_tabs_or_set_layouts() {
    let mut sb = HostSandbox::new("op-split-plugin");
    let plugin = sb.root.join("plugins/pane");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(
        plugin.join("plugin.toml"),
        r#"
name = "pane"
version = "0.1.0"
command = ["python3", "./reverse.py"]
capabilities = ["host_write"]
"#,
    )
    .unwrap();
    std::fs::write(plugin.join("reverse.py"), r#"
import json
import sys

def send(**fields):
    print(json.dumps(dict(jsonrpc="2.0", **fields)), flush=True)

for line in sys.stdin:
    req = json.loads(line)
    if req["method"] == "plugin.hello":
        send(id=req["id"], result={"name": "pane", "version": "0.1.0",
            "methods": [{"name": "ext.pane.reverse", "description": "Probe reverse RPC"}], "events": []})
    else:
        send(id=999, **req["params"])
        reply = json.loads(sys.stdin.readline())
        reply["id"] = req["id"]
        print(json.dumps(reply), flush=True)
"#).unwrap();
    sb.start_host();
    let mut ui = sb.connect();
    let room = ui.visible().room;
    let permitted: Value = ui.call("ext.pane.reverse", json!({
        "method": "host.room_rename", "params": {"room_id": room.id, "name": "Plugin renamed"}
    })).unwrap();
    assert_eq!(permitted["room"]["name"], "Plugin renamed");
    for (method, params) in [
        (
            "host.tab_split",
            json!({"anchor_tab_id": room.tabs[0].id, "axis": "horizontal", "cols": 80, "rows": 24}),
        ),
        (
            "host.room_set_layout",
            json!({"room_id": room.id, "layout": null}),
        ),
    ] {
        let error = ui
            .call::<Value>(
                "ext.pane.reverse",
                json!({"method": method, "params": params}),
            )
            .unwrap_err();
        assert_eq!(code(error), "permission_denied", "{method}");
    }
    let live: RoomRecord = ui.catalog().catalog.rooms.remove(0);
    assert!(live.layout.is_none());
    assert_eq!(live.tabs.len(), 1);
}

#[tokio::test]
async fn operator_can_read_write_and_promote_tabs_without_a_session_token() {
    let mut sb = HostSandbox::new("op-tabs");
    sb.start_host();
    let mut ui = sb.connect();
    let (operator, _events) = HostClient::connect_once(
        HostProfile {
            transport: HostTransport::Local {
                sock_path: Some(sb.run.join("host.sock").display().to_string()),
            },
            ..HostProfile::local_default()
        },
        "operator-test",
    )
    .await
    .unwrap();
    let command = json!({"kind": "command", "argv": ["/bin/sh", "-c", "stty -echo; printf 'ready\\n'; exec cat"]});
    let created = operator
        .tab_create_ext(
            None,
            None,
            Some("Back\nground".into()),
            serde_json::from_value(command.clone()).unwrap(),
            80,
            24,
            None,
        )
        .await
        .unwrap()
        .tab;
    let tab_id = &created.id;
    let host_id = operator.hello().unwrap().host_id;
    assert_eq!(created.title, "Background");
    assert_eq!(created.parent_session_id, None);
    let catalog = ui.catalog().catalog;
    let room = &catalog.rooms[0];
    assert_eq!(catalog.rooms.len(), 1);
    assert_eq!(room.id, background_room_id(&host_id[..12]).unwrap());
    assert_eq!(room.kind, RoomKind::Background);
    assert_eq!(room.visibility, RoomVisibility::Hidden);
    assert_eq!(&room.tabs[0].id, tab_id);

    common::eventually(|| {
        let screen: SessionScreenResult = ui
            .call("host.tab_screen", json!({"tab_id": tab_id}))
            .unwrap();
        (screen.text.contains("ready") && screen.run_generation == created.run_generation)
            .then_some(())
    });
    let screen = operator.tab_screen(tab_id.clone(), 24, 2).await.unwrap();
    assert_eq!(screen.text, "re");
    assert!(screen.truncated);
    for (lines, max_bytes) in [(0, 24), (2001, 24), (24, 0), (24, 262145)] {
        let err = operator
            .tab_screen(tab_id.clone(), lines, max_bytes)
            .await
            .unwrap_err();
        assert_eq!(code(err), "invalid_params");
    }

    let promoted = operator
        .tab_move(
            tab_id.clone(),
            SessionMoveTarget::NewRoom {
                name: Some("Promoted".into()),
            },
        )
        .await
        .unwrap()
        .session;
    assert_eq!(&promoted.session_id, tab_id);
    assert_eq!(promoted.runtime, created.runtime);
    assert_eq!(promoted.run_generation, created.run_generation);
    assert_eq!(promoted.visibility, RoomVisibility::Visible);
    assert_ne!(promoted.room_id, room.id);
    let catalog = ui.catalog().catalog;
    assert!(
        catalog
            .rooms
            .iter()
            .find(|r| r.id == room.id)
            .unwrap()
            .tabs
            .is_empty()
    );
    assert_eq!(
        catalog
            .rooms
            .iter()
            .find(|r| r.id == promoted.room_id)
            .unwrap()
            .name
            .as_deref(),
        Some("Promoted")
    );

    let controller = ui
        .call::<ddg_agents_proto::host::TabAttachResult>(
            "host.tab_attach",
            json!({"tab_id": tab_id, "cols": 80, "rows": 24}),
        )
        .unwrap();
    assert_eq!(controller.mode, ControlMode::Control);
    for input in [
        String::new(),
        "not-base64".into(),
        encode_base64(&vec![b'x'; 65537]),
    ] {
        let err = operator
            .tab_write(tab_id.clone(), input, None)
            .await
            .unwrap_err();
        assert_eq!(code(err), "invalid_params");
    }
    let mismatched = operator
        .tab_write(
            tab_id.clone(),
            encode_base64(b"wrong-generation\n"),
            Some(created.run_generation + 1),
        )
        .await
        .unwrap_err();
    assert_eq!(code(mismatched), "generation_mismatch");
    let input = b"operator-write\n";
    let written = operator
        .tab_write(
            tab_id.clone(),
            encode_base64(input),
            Some(created.run_generation),
        )
        .await
        .unwrap();
    assert_eq!(written.byte_len as usize, input.len());
    assert_eq!(written.run_generation, created.run_generation);
    let external =
        ui.read_where(|f| control_note(f).is_some_and(|n| n["method"] == "host.external_input"));
    let notification = control_note(&external).unwrap();
    assert_eq!(notification["params"]["from_session_id"], "operator");
    assert_eq!(notification["params"]["from_host_id"], host_id);
    assert_eq!(notification["params"]["session_id"], *tab_id);
    assert_eq!(notification["params"]["is_remote"], false);
    assert!(notification["params"].get("data_b64").is_none());
    let readonly = operator
        .tab_attach(tab_id.clone(), true, 80, 24, 0)
        .await
        .unwrap();
    assert_eq!(readonly.mode, ControlMode::Readonly);
    assert_eq!(readonly.control_epoch, controller.control_epoch);
    ui.send_input(controller.channel, b"controller-write\n");
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let screen = operator
                .tab_screen(tab_id.clone(), 24, 262144)
                .await
                .unwrap();
            assert!(!screen.text.contains("wrong-generation"));
            if screen.text.contains("operator-write") && screen.text.contains("controller-write") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let log = std::fs::read_to_string(sb.state.join("host.log")).unwrap();
    assert!(
        log.lines()
            .any(|line| line.ends_with(" tab_write operator"))
    );
    assert!(!log.contains("operator-write") && !log.contains(&encode_base64(input)));

    let err = ui
        .call::<Value>(
            "host.tab_create",
            json!({
                "room_id": promoted.room_id, "background": true, "cols": 80, "rows": 24,
            }),
        )
        .unwrap_err();
    assert_eq!(code(err), "invalid_params");
    let explicit: TabCreateResult = ui
        .call(
            "host.tab_create",
            json!({
                "background": true, "executor": command, "cols": 80, "rows": 24,
            }),
        )
        .unwrap();
    let moved = operator
        .tab_move(
            explicit.tab.id.clone(),
            SessionMoveTarget::ExistingRoom {
                room_id: promoted.room_id.clone(),
            },
        )
        .await
        .unwrap()
        .session;
    assert_eq!(moved.session_id, explicit.tab.id);
    assert_eq!(moved.room_id, promoted.room_id);
    operator.close();
}
