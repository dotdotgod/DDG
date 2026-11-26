#![cfg(unix)]

mod common;

use common::{HostSandbox, code, control_note, eventually, executor};
use ddg_agents_proto::host::{HostStatus, ListHostsResult, SessionListResult, TabAttachResult};
use ddg_agents_proto::id::encode_base64;
use ddg_agents_proto::terminal::{ControlMode, SessionMessagePayload, SessionRecvResult};
use ddg_agents_transport::{HostProfile, HostTransport, HostsConfig};
use serde_json::{Value, json};
use std::{fs, process::Command, time::Duration};

#[test]
fn one_pinned_link_routes_both_directions_and_stops_after_replacement() {
    let mut remote = HostSandbox::new("relay-remote");
    remote.start_host();
    let mut leaf = remote.connect();
    let remote_id = leaf.hello["host_id"].as_str().unwrap().to_owned();
    let leaf_room = leaf.visible();
    let b = &leaf_room.room.tabs[0].id;
    let token_b = remote.get_token(b).0;

    let mut hub = HostSandbox::new("relay-hub");
    let config = HostsConfig {
        hosts: vec![
            HostProfile::local_default(),
            HostProfile {
                id: "leaf".into(),
                label: "Leaf".into(),
                expected_host_id: Some(remote_id.clone()),
                transport: HostTransport::Local {
                    sock_path: Some(remote.run.join("host.sock").to_str().unwrap().into()),
                },
            },
            HostProfile {
                id: "offline".into(),
                label: "Offline".into(),
                expected_host_id: Some("unavailable-host".into()),
                transport: HostTransport::Local {
                    sock_path: Some(
                        hub.run
                            .join("sensitive-offline.sock")
                            .to_str()
                            .unwrap()
                            .into(),
                    ),
                },
            },
        ],
        ..HostsConfig::default()
    };
    config.save(&hub.config.join("hosts.json")).unwrap();
    hub.start_hub();
    let mut primary = hub.connect();
    let hub_id = primary.hello["host_id"].as_str().unwrap().to_owned();
    let head_room = primary.visible();
    let a = &head_room.room.tabs[0].id;
    let token_a = hub.get_token(a).0;
    eventually(|| {
        let result: ListHostsResult = primary
            .call("host.list_hosts", json!({"session_token":token_a}))
            .unwrap();
        result
            .hosts
            .iter()
            .any(|h| h.host_id == remote_id && h.status == HostStatus::Connected)
            .then_some(())
    });
    let hosts: Value = leaf
        .call("host.list_hosts", json!({"session_token":token_b}))
        .unwrap();
    let hosts = hosts["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 3);
    assert_eq!(hosts[0]["host_id"], hub_id);
    assert!(hosts.iter().all(|h| h.as_object().unwrap().len() == 3));
    assert!(!remote.config.join("hosts.json").exists());

    let bulk = encode_base64(&vec![b'x'; 65536]);
    // The encoded messages in each direction exceed the stream's initial credit window.
    for _ in 0..4 {
        let _: Value = primary.call("host.msg_send", json!({"session_token":token_a,"host_id":"leaf","to_session_id":b,"data_b64":bulk,"from_host_id":"forged","from_session_id":b})).unwrap();
        let _: Value = leaf
            .call(
                "host.msg_send",
                json!({"session_token":token_b,"host_id":hub_id,"to_session_id":a,"data_b64":bulk}),
            )
            .unwrap();
    }
    for (receiver, token, from_host, from_session) in [
        (&mut leaf, &token_b, &hub_id, a),
        (&mut primary, &token_a, &remote_id, b),
    ] {
        let inbox: SessionRecvResult = receiver
            .call(
                "host.msg_recv",
                json!({"session_token":token,"max_messages":8,"wait_ms":100}),
            )
            .unwrap();
        assert_eq!(inbox.messages.len(), 4);
        for message in inbox.messages {
            assert_eq!(
                (
                    &message.from_host_id,
                    &message.from_session_id,
                    message.is_remote
                ),
                (from_host, from_session, true)
            );
            assert!(
                matches!(message.payload, SessionMessagePayload::Message { data_b64 } if data_b64 == bulk)
            );
        }
    }

    let owner: TabAttachResult = leaf
        .call("host.tab_attach", json!({"tab_id":b,"cols":80,"rows":24}))
        .unwrap();
    assert_eq!(owner.mode, ControlMode::Control);
    let _: Value = primary.call("host.session_write", json!({"session_token":token_a,"host_id":remote_id,"session_id":b,"expect_run_generation":1,"data_b64":encode_base64(b"hello leaf\n")})).unwrap();
    let event =
        leaf.read_where(|f| control_note(f).is_some_and(|n| n["method"] == "host.external_input"));
    let metadata = &control_note(&event).unwrap()["params"];
    assert_eq!(metadata["from_host_id"], hub_id);
    assert_eq!(metadata["from_session_id"], *a);
    assert_eq!(metadata["session_id"], *b);
    assert_eq!(metadata["is_remote"], true);
    assert!(metadata.get("data_b64").is_none());
    let control: TabAttachResult = remote
        .connect()
        .call("host.tab_attach", json!({"tab_id":b,"cols":80,"rows":24}))
        .unwrap();
    assert_eq!(control.mode, ControlMode::Readonly);
    eventually(|| {
        let screen: Value = primary.call("host.session_screen", json!({"session_token":token_a,"host_id":"leaf","session_id":b,"lines":24,"max_bytes":8192})).unwrap();
        screen["text"]
            .as_str()
            .unwrap()
            .contains("hello leaf")
            .then_some(())
    });

    let child: Value = primary
        .call(
            "host.session_create",
            json!({"session_token":token_a,"host_id":"leaf","executor":executor("exec cat")}),
        )
        .unwrap();
    assert_eq!(child["session"]["host_id"], remote_id);
    let child_id = &child["session"]["session_id"];
    let _: Value = primary.call("host.session_move", json!({"session_token":token_a,"host_id":"leaf","session_id":child_id,"target":{"kind":"new_room"}})).unwrap();
    let restart: Value = primary
        .call(
            "host.session_restart",
            json!({"session_token":token_a,"host_id":"leaf","session_id":child_id}),
        )
        .unwrap();
    assert_eq!(restart["session"]["run_generation"], 2);
    let _: Value = primary
        .call(
            "host.session_close",
            json!({"session_token":token_a,"host_id":"leaf","session_id":child_id}),
        )
        .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ddg-agents-host"))
        .args(["session", "list", "--host", "all"])
        .env("DDG_SESSION_TOKEN", &token_b)
        .env("DDG_HOST_SOCK", remote.run.join("host.sock"))
        .env("DDG_STATE_DIR", &remote.state)
        .env("DDG_RUN_DIR", &remote.run)
        .env("DDG_CONFIG_DIR", &remote.config)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let all: SessionListResult = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(all.sessions.len(), 2);
    assert!(all.sessions.iter().any(|s| s.session_id == *a));
    assert!(all.sessions.iter().any(|s| s.session_id == *b));
    assert_eq!(all.errors.len(), 1);
    assert_eq!(
        all.errors[0].error.data.code.as_deref(),
        Some("host_offline")
    );
    assert!(
        !all.errors[0]
            .error
            .message
            .contains("sensitive-offline.sock")
    );
    assert_eq!(
        code(
            primary
                .call::<Value>(
                    "host.session_list",
                    json!({"session_token":token_a,"host_id":"unknown"})
                )
                .unwrap_err()
        ),
        "host_not_found"
    );

    let mut replacement = remote.connect();
    let _: Value = replacement.call("host.relay_attach", json!({})).unwrap();
    eventually(|| {
        fs::read_to_string(hub.state.join("host.log"))
            .unwrap()
            .contains("relay_replaced leaf")
            .then_some(())
    });
    drop(replacement);
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(
        code(
            primary
                .call::<Value>(
                    "host.session_list",
                    json!({"session_token":token_a,"host_id":"leaf"})
                )
                .unwrap_err()
        ),
        "host_security_blocked"
    );
    assert_eq!(
        code(
            leaf.call::<Value>(
                "host.session_list",
                json!({"session_token":token_b,"host_id":hub_id})
            )
            .unwrap_err()
        ),
        "hub_unavailable"
    );
}
