#![cfg(unix)]

mod common;

use common::{HostSandbox, code, control_note, eventually, executor};
use ddg_agents_proto::frame::{Frame, FrameKind};
use ddg_agents_proto::host::{
    ListHostsResult, RoomCreateResult, SessionListResult, TabAttachResult,
};
use ddg_agents_proto::id::encode_base64;
use ddg_agents_proto::terminal::{
    ControlMode, SessionMessage, SessionMessagePayload, SessionRecvResult,
};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn messaging_uses_authenticated_ids_and_credit_and_input_does_not_steal_control() {
    let mut sb = HostSandbox::new("messaging");
    sb.start_host();
    let mut ui = sb.connect();
    let mut process = sb.connect();
    let mut command = executor(
        r#"printf 'ID:%s\nHOST:%s\nH:%s\nT:%s\n' "$DDG_SESSION_ID" "$DDG_HOST_ID" "$DDG_HOST_SOCK" "$DDG_TERMINAL_SOCK"; exec cat"#,
    );
    command["env"] =
        json!({"DDG_SESSION_TOKEN":"never-in-catalog", "DDG_ROOM_ID":"spoof", "DEMO":"retained"});
    let denied = ui
        .call::<Value>(
            "host.room_create",
            json!({"cols":80,"rows":24,"executor":command}),
        )
        .unwrap_err();
    assert_eq!(
        denied.data.kind,
        ddg_agents_proto::rpc::ErrorKind::InvalidParams
    );
    command["env"] = json!({"DEMO":"retained"});
    let visible: RoomCreateResult = ui
        .call(
            "host.room_create",
            json!({"cols":80,"rows":24,"executor":command}),
        )
        .unwrap();
    let a = &visible.room.tabs[0].id;
    let (token_a, _) = sb.get_token(a);
    let hidden = ui.hidden(&token_a, "exec cat");
    let b = &hidden.session.session_id;
    let (token_b, _) = sb.get_token(b);

    let missing = process
        .call::<Value>("host.session_list", json!({}))
        .unwrap_err();
    assert_eq!(code(missing), "token_invalid");
    let forged = token_a.replacen(a, b, 1);
    let forbidden = process
        .call::<Value>("host.session_list", json!({"session_token": forged}))
        .unwrap_err();
    assert_eq!(code(forbidden), "token_invalid");
    let remote = process
        .call::<Value>(
            "host.session_list",
            json!({"session_token": token_a, "host_id":"different-host"}),
        )
        .unwrap_err();
    assert_eq!(code(remote), "hub_unavailable");
    let hosts: ListHostsResult = process
        .call("host.list_hosts", json!({"session_token": token_a}))
        .unwrap();
    assert_eq!(hosts.hosts.len(), 1);
    assert_eq!(hosts.hosts[0].label, "local");
    assert_eq!(
        hosts.hosts[0].host_id,
        ui.hello["host_id"].as_str().unwrap()
    );
    let listed: SessionListResult = process.call("host.session_list", json!({"session_token": token_a,"include_hidden":true,"room_id":hidden.session.room_id})).unwrap();
    assert_eq!(listed.sessions.len(), 1);
    assert_eq!(&listed.sessions[0].session_id, b);
    assert!(listed.errors.is_empty());

    let _: Value = process.call("host.msg_send", json!({
        "session_token": token_a, "to_session_id": b, "data_b64": "c2VjcmV0LXBheWxvYWQ=", "from_session_id": b, "from_host_id":"fake", "is_remote":true,
    })).unwrap();
    let recv: SessionRecvResult = ui
        .call(
            "host.msg_recv",
            json!({"session_token":token_b,"max_messages":8,"wait_ms":100,"session_id":a}),
        )
        .unwrap();
    assert_eq!(recv.messages.len(), 1);
    let m = &recv.messages[0];
    assert_eq!(&m.from_session_id, a);
    assert_eq!(&m.to_session_id, b);
    assert_eq!(m.from_host_id, hosts.hosts[0].host_id);
    assert!(!m.is_remote);

    let sub: Value = ui
        .call(
            "host.msg_subscribe",
            json!({"session_token": token_b,"session_id":a}),
        )
        .unwrap();
    let channel = sub["channel"].as_u64().unwrap() as u32;
    let bulk = encode_base64(&vec![b'x'; 65536]);
    // Four 64-KiB messages exceed the upstream's initial 256-KiB credit once encoded.
    for _ in 0..4 {
        let _: Value = process
            .call(
                "host.msg_send",
                json!({"session_token": token_a, "to_session_id": b, "data_b64": bulk}),
            )
            .unwrap();
    }
    let mut bytes = Vec::new();
    let mut count = 0;
    while count < 4 {
        let data = ui.read_where(|f| f.channel == channel && f.kind == FrameKind::Data);
        ui.send(Frame::credit(channel, data.payload.len() as u32));
        bytes.extend_from_slice(&data.payload);
        while let Some(i) = bytes.iter().position(|b| *b == b'\n') {
            let message: SessionMessage =
                serde_json::from_slice(&bytes.drain(..=i).collect::<Vec<_>>()).unwrap();
            assert_eq!(&message.from_session_id, a);
            assert_eq!(&message.to_session_id, b);
            assert!(
                matches!(message.payload, SessionMessagePayload::Message { data_b64 } if data_b64 == bulk)
            );
            count += 1;
        }
    }

    let attached: TabAttachResult = ui
        .call("host.tab_attach", json!({"tab_id":a,"cols":80,"rows":24}))
        .unwrap();
    assert_eq!(attached.mode, ControlMode::Control);
    assert_eq!(attached.run_generation, 1);
    eventually(|| {
        let r: Value = process
            .call(
                "host.session_screen",
                json!({"session_token":token_b,"session_id":a,"lines":24,"max_bytes":65536}),
            )
            .unwrap();
        let text = r["text"].as_str().unwrap();
        (text.contains(&format!("ID:{a}"))
            && text.contains(&format!("HOST:{}", hosts.hosts[0].host_id))
            && text.contains(&format!("H:{}", sb.run.join("host.sock").display()))
            && text.contains(&format!("T:{}", sb.run.join("terminal.sock").display())))
        .then_some(())
    });
    for (lines, max_bytes) in [(0, 65536), (2001, 65536), (10, 0), (10, 262145)] {
        let err = process
            .call::<Value>(
                "host.session_screen",
                json!({"session_token":token_b,"session_id":a,"lines":lines,"max_bytes":max_bytes}),
            )
            .unwrap_err();
        assert_eq!(code(err), "invalid_params");
    }
    for text in [
        String::new(),
        "not-base64".to_owned(),
        encode_base64(&vec![b'x'; 65537]),
    ] {
        let err = process
            .call::<Value>(
                "host.session_write",
                json!({"session_token":token_b,"session_id":a,"data_b64":text}),
            )
            .unwrap_err();
        assert_eq!(code(err), "invalid_params");
    }
    let input = encode_base64(b"external-write\n");
    let wrong_generation = process.call::<Value>("host.session_write", json!({"session_token":token_b,"session_id":a,"data_b64":input,"expect_run_generation":2})).unwrap_err();
    assert_eq!(code(wrong_generation), "generation_mismatch");
    let written: Value = process.call("host.session_write", json!({"session_token":token_b,"session_id":a,"data_b64":input,"expect_run_generation":1,"from_session_id":a})).unwrap();
    assert_eq!(written["byte_len"], 15);
    let external =
        ui.read_where(|f| control_note(f).is_some_and(|n| n["method"] == "host.external_input"));
    let metadata = control_note(&external).unwrap();
    assert_eq!(metadata["params"]["from_session_id"], *b);
    assert_eq!(metadata["params"]["session_id"], *a);
    assert_eq!(metadata["params"]["byte_len"], 15);
    assert_eq!(metadata["params"]["is_remote"], false);
    assert!(
        !metadata.to_string().contains("external-write") && !metadata.to_string().contains(&input)
    );
    let readonly: TabAttachResult = process
        .call("host.tab_attach", json!({"tab_id":a,"cols":80,"rows":24}))
        .unwrap();
    assert_eq!(readonly.mode, ControlMode::Readonly);
    ui.send_input(attached.channel, b"controller-still-active\n");
    eventually(|| {
        let r: Value = process
            .call(
                "host.session_screen",
                json!({"session_token":token_b,"session_id":a,"lines":24,"max_bytes":65536}),
            )
            .unwrap();
        let text = r["text"].as_str().unwrap();
        (text.contains("external-write") && text.contains("controller-still-active")).then_some(())
    });

    let closed: Value = process
        .call(
            "host.session_close",
            json!({"session_token": token_b,"session_id": b}),
        )
        .unwrap();
    assert_eq!(closed["removed_room"], false);
    assert_eq!(
        code(
            process
                .call::<Value>(
                    "host.msg_recv",
                    json!({"session_token":token_b,"max_messages":1,"wait_ms":0})
                )
                .unwrap_err()
        ),
        "token_revoked"
    );
    let log = fs::read_to_string(sb.state.join("host.log")).unwrap();
    assert_eq!(
        fs::metadata(sb.state.join("host.log"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let catalog = fs::read_to_string(sb.state.join("catalog.json")).unwrap();
    for forbidden in [
        &token_a,
        &token_b,
        &input,
        "secret-payload",
        "external-write",
        "c2VjcmV0LXBheWxvYWQ=",
        "never-in-catalog",
    ] {
        assert!(!log.contains(forbidden) && !catalog.contains(forbidden));
    }
    assert!(catalog.contains("retained"));
}
