//! OpenRPC 1.3 document generation for `ddg-terminal-server` and `ddg-agents-host`.

use crate::{host, plugin, rpc, terminal};
use schemars::{JsonSchema, schema_for};
use serde_json::{Value, json};

fn method<P: JsonSchema, R: JsonSchema>(name: &str, summary: &str) -> Value {
    json!({
        "name": name,
        "summary": summary,
        "paramStructure": "by-name",
        "params": [{
            "name": "params",
            "required": true,
            "schema": schema_for!(P),
        }],
        "result": {
            "name": "result",
            "schema": schema_for!(R),
        }
    })
}

pub fn terminal_server_openrpc() -> Value {
    json!({
        "openrpc": "1.3.2",
        "info": {
            "title": "ddg-terminal-server",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Single-host terminal server: PTY lifecycle, VT snapshots, single-controller leases, session inboxes and I/O, and credit-based framed multiplexing."
        },
        "methods": [
            method::<terminal::HelloParams, terminal::HelloResult>("hello", "Negotiate protocol version and optionally upgrade to 9-byte binary framed mode."),
            method::<terminal::PingParams, terminal::PingResult>("ping", "Health check returning server identity and boot generation."),
            method::<terminal::PtySpawnParams, terminal::PtySpawnResult>("pty.spawn", "Spawn an OS PTY process."),
            method::<terminal::PtyListParams, terminal::PtyListResult>("pty.list", "List live PTYs."),
            method::<terminal::PtyProbeBinsParams, terminal::PtyProbeBinsResult>("pty.probe_bins", "Check the fixed built-in executable basenames in the Holder environment without executing them."),
            method::<terminal::PtySubscribeParams, terminal::PtySubscribeResult>("pty.subscribe", "Subscribe to PTY lifecycle, size, title, CWD, control, agent status, and foreground changes."),
            method::<terminal::PtyAttachParams, terminal::PtyAttachResult>("pty.attach", "Attach to a PTY stream; receives first_paint VT snapshot followed by live output."),
            method::<terminal::PtyClaimControlParams, terminal::PtyClaimControlResult>("pty.claim_control", "Claim exclusive input/resize control over a PTY."),
            method::<terminal::PtyReleaseControlParams, terminal::PtyReleaseControlResult>("pty.release_control", "Release exclusive control over a PTY."),
            method::<terminal::PtyKillParams, terminal::PtyKillResult>("pty.kill", "Terminate a PTY and wait for child reaping."),
            method::<terminal::SessionSendParams, terminal::SessionSendResult>("session.send", "Enqueue a message or a protected session-exit notification for a session."),
            method::<terminal::SessionRecvParams, terminal::SessionRecvResult>("session.recv", "Receive up to max_messages from a session inbox; optionally wait wait_ms, then return an empty list if none arrived."),
            method::<terminal::SessionInboxSubscribeParams, terminal::SessionInboxSubscribeResult>("session.inbox_subscribe", "Subscribe to a session inbox over a 9-byte framed Credit stream."),
            method::<terminal::SessionReadScreenParams, terminal::SessionReadScreenResult>("session.read_screen", "Read plain UTF-8 screen text, truncating only at character boundaries."),
            method::<terminal::SessionWriteInputParams, terminal::SessionWriteInputResult>("session.write_input", "Write input without taking control; check the generation if supplied and publish an event with metadata only."),
        ],
        "components": {
            "schemas": {
                "CloseReason": schema_for!(terminal::CloseReason),
                "PtyEvent": schema_for!(terminal::PtyEvent),
                "RpcError": schema_for!(rpc::RpcError),
                "SessionMessage": schema_for!(terminal::SessionMessage),
            }
        }
    })
}

pub fn agents_host_openrpc() -> Value {
    json!({
        "openrpc": "1.3.2",
        "info": {
            "title": "ddg-agents-host",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "DDG Agents entry point: room/tab catalog v2, terminal streams, authenticated session messaging and I/O, optional cross-host relay, and ext.<plugin>.* plugins."
        },
        "methods": [
            method::<terminal::HelloParams, host::HostHelloResult>("hello", "Negotiate protocol version and optionally upgrade to 9-byte binary framed mode."),
            method::<terminal::PingParams, terminal::PingResult>("ping", "Health check verifying host and underlying ddg-terminal-server."),
            method::<host::CatalogSubscribeParams, host::CatalogSubscribeResult>("host.catalog_subscribe", "Fetch the room/tab catalog and tab activity snapshot and subscribe to changes."),
            method::<host::HostAgentPresetsDetectParams, host::HostAgentPresetsDetectResult>("host.agent_presets_detect", "Check availability of the fixed built-in agent presets as the operator."),
            method::<host::RoomCreateParams, host::RoomCreateResult>("host.room_create", "Create a new room with its first tab."),
            method::<host::RoomRenameParams, host::RoomRenameResult>("host.room_rename", "Rename a room (null reverts to auto folder name)."),
            method::<host::RoomSetLayoutParams, host::RoomSetLayoutResult>("host.room_set_layout", "Replace or reset a visible room's persisted pane layout as the operator."),
            method::<host::TabSplitParams, host::TabSplitResult>("host.tab_split", "Create a session alongside an anchor tab in a visible room as the operator."),
            method::<host::TabCreateParams, host::TabCreateResult>("host.tab_create", "Create a tab in the chosen room, or in the hidden background room when room_id is omitted."),
            method::<host::TabScreenParams, host::SessionScreenResult>("host.tab_screen", "Read bounded UTF-8 screen text from a local tab as the operator."),
            method::<host::TabMarkSeenParams, host::TabMarkSeenResult>("host.tab_mark_seen", "Mark tab activity as seen for the given run generation as the operator."),
            method::<host::TabWriteParams, host::SessionWriteResult>("host.tab_write", "Write to a local tab as the operator without taking control; check the generation if supplied."),
            method::<host::TabMsgSendParams, host::MsgSendResult>("host.tab_msg_send", "Send a message to a local session's inbox as the operator."),
            method::<host::TabMoveParams, host::SessionMoveResult>("host.tab_move", "Promote a local background tab into an existing or newly created visible room as the operator."),
            method::<host::TabAttachParams, host::TabAttachResult>("host.tab_attach", "Attach to a tab's terminal stream (zero-parse passthrough to ddg-terminal-server)."),
            method::<host::TabClaimControlParams, terminal::PtyClaimControlResult>("host.tab_claim_control", "Claim exclusive input/resize control over a tab's running PTY."),
            method::<host::TabReleaseControlParams, terminal::PtyReleaseControlResult>("host.tab_release_control", "Release exclusive input/resize control over a tab's running PTY."),
            method::<host::TabCloseParams, host::TabCloseResult>("host.tab_close", "Explicitly terminate a tab's PTY and remove the tab (and empty room) after reaping."),
            method::<host::TabRestartParams, host::TabRestartResult>("host.tab_restart", "Explicitly start a new PTY on an exited or post-reboot stopped tab."),
            method::<host::SessionCreateParams, host::SessionCreateResult>("host.session_create", "Create an authenticated session, using the hidden background room when room_id is omitted."),
            method::<host::SessionListParams, host::SessionListResult>("host.session_list", "List sessions, optionally including hidden sessions; multi-host queries may return sessions and per-host errors together."),
            method::<host::SessionMoveParams, host::SessionMoveResult>("host.session_move", "Move a session into an existing or newly created room."),
            method::<host::SessionCloseParams, host::SessionCloseResult>("host.session_close", "Close a session by its stable ID."),
            method::<host::SessionRestartParams, host::SessionRestartResult>("host.session_restart", "Restart a session, preserving its ID and increasing its run generation."),
            method::<host::SessionScreenParams, host::SessionScreenResult>("host.session_screen", "Read bounded UTF-8 screen text from a session."),
            method::<host::SessionWriteParams, host::SessionWriteResult>("host.session_write", "Write input without taking the terminal's control lease; check the generation if supplied."),
            method::<host::MsgSendParams, host::MsgSendResult>("host.msg_send", "Send a message to a local or relayed session."),
            method::<host::MsgRecvParams, host::MsgRecvResult>("host.msg_recv", "Receive messages from the authenticated caller's inbox."),
            method::<host::MsgSubscribeParams, host::MsgSubscribeResult>("host.msg_subscribe", "Subscribe to the authenticated caller's inbox over a 9-byte framed Credit stream."),
            method::<host::ListHostsParams, host::ListHostsResult>("host.list_hosts", "List hosts with only identity, label, and status."),
            method::<host::RelayAttachParams, host::RelayAttachResult>("host.relay_attach", "Attach one bidirectional relay channel; requests and responses use the same Credit stream."),
        ],
        "components": {
            "schemas": {
                "Catalog": schema_for!(host::Catalog),
                "CloseReason": schema_for!(terminal::CloseReason),
                "HostEvent": schema_for!(host::HostEvent),
                "PluginManifest": schema_for!(plugin::PluginManifest),
                "PluginHelloResult": schema_for!(plugin::PluginHelloResult),
                "RelayEnvelope": schema_for!(host::RelayEnvelope),
                "RelayFrame": schema_for!(host::RelayFrame),
                "RpcError": schema_for!(rpc::RpcError),
                "SessionMessage": schema_for!(terminal::SessionMessage),
            }
        }
    })
}

fn canonicalize(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(canonicalize).collect()),
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<_, _> =
                map.into_iter().map(|(k, v)| (k, canonicalize(v))).collect();
            Value::Object(sorted.into_iter().collect())
        }
        other => other,
    }
}

pub fn to_pretty_json(doc: Value) -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(&canonicalize(doc)).expect("serializable")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn schema_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("schema")
    }

    // UT-PROTO-02
    #[test]
    fn preset_probe_methods_require_empty_object_params() {
        for (doc, name, item_key) in [
            (terminal_server_openrpc(), "pty.probe_bins", "bin"),
            (agents_host_openrpc(), "host.agent_presets_detect", "id"),
        ] {
            let mut methods = doc["methods"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["name"] == name);
            let method = methods.next().expect(name);
            assert!(methods.next().is_none(), "duplicate method {name}");
            let params = &method["params"][0]["schema"];
            assert_eq!(params["type"], "object");
            assert_eq!(params["additionalProperties"], false);
            assert!(
                params["properties"]
                    .as_object()
                    .is_none_or(|p| p.is_empty())
            );
            let result = &method["result"]["schema"];
            assert_eq!(result["required"], json!(["items"]));
            let defs = result["$defs"].as_object().unwrap();
            assert!(
                defs.values()
                    .any(|s| s["required"] == json!([item_key, "status"]))
            );
            assert!(
                defs.values()
                    .any(|s| s["enum"] == json!(["found", "not_found", "unknown"]))
            );
        }
    }

    #[test]
    fn excludes_untrusted_parent_reset_notice_and_private_methods() {
        let host_doc = agents_host_openrpc();
        let methods = host_doc["methods"].as_array().unwrap();
        let tab_create = methods
            .iter()
            .find(|m| m["name"] == "host.tab_create")
            .unwrap();
        assert!(
            tab_create["params"][0]["schema"]["properties"]
                .get("parent_session_id")
                .is_none()
        );
        let mut params =
            json!({"room_id": "rm_00000000000000000000000000", "cols": 80, "rows": 24});
        assert!(serde_json::from_value::<host::TabCreateParams>(params.clone()).is_ok());
        params["parent_session_id"] = json!("ses_00000000000000000000000000");
        assert!(serde_json::from_value::<host::TabCreateParams>(params).is_err());

        let hello = methods.iter().find(|m| m["name"] == "hello").unwrap();
        assert!(
            hello["result"]["schema"]["properties"]
                .get("reset_notice")
                .is_none()
        );
        let hello: host::HostHelloResult = serde_json::from_value(json!({
            "protocol_version": 1, "host_id": "test", "boot_generation": 1, "conn_id": 1,
            "reset_notice": {"notice_id": "reset", "backup_file": "catalog.json.v1.bak.0"}
        }))
        .unwrap();
        assert!(json!(hello).get("reset_notice").is_none());

        assert!(methods.iter().all(|m| m["name"] != "host.relay_exec"));
        assert!(
            terminal_server_openrpc()["methods"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["name"] != "session.purge_mailbox")
        );
    }

    #[test]
    fn operator_tab_rpc_schemas_match_wire_params() {
        let doc = agents_host_openrpc();
        let methods = doc["methods"].as_array().unwrap();
        let schema = |name: &str| {
            let mut matching = methods.iter().filter(|m| m["name"] == name);
            let method = matching.next().expect(name);
            assert!(matching.next().is_none(), "duplicate method {name}");
            &method["params"][0]["schema"]
        };

        let create = schema("host.tab_create");
        assert_eq!(create["required"], json!(["cols", "rows"]));
        let legacy = json!({"room_id": "rm_00000000000000000000000000", "cols": 80, "rows": 24});
        let params: host::TabCreateParams = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(json!(params)["room_id"], legacy["room_id"]);
        assert!(json!(params).get("background").is_none());
        assert!(json!(params).get("title").is_none());
        let params: host::TabCreateParams =
            serde_json::from_value(json!({"cols": 80, "rows": 24})).unwrap();
        assert_eq!(params.room_id, None);
        assert!(!params.background);

        for (name, required) in [
            ("host.tab_screen", json!(["tab_id"])),
            ("host.tab_write", json!(["tab_id", "data_b64"])),
            ("host.tab_move", json!(["tab_id", "target"])),
            (
                "host.tab_split",
                json!(["anchor_tab_id", "axis", "cols", "rows"]),
            ),
            ("host.room_set_layout", json!(["room_id"])),
        ] {
            let params = schema(name);
            assert_eq!(params["required"], required);
            assert_eq!(params["additionalProperties"], false);
            assert!(params["properties"].get("session_token").is_none());
        }
        let screen = schema("host.tab_screen");
        let params: host::TabScreenParams =
            serde_json::from_value(json!({"tab_id": "ses_00000000000000000000000001"})).unwrap();
        assert_eq!(screen["properties"]["lines"]["default"], params.lines);
        assert_eq!(
            screen["properties"]["max_bytes"]["default"],
            params.max_bytes
        );
        assert_eq!((params.lines, params.max_bytes), (24, 262_144));
    }

    #[test]
    fn advertises_session_and_relay_methods() {
        for (doc, names) in [
            (
                terminal_server_openrpc(),
                &[
                    "session.send",
                    "session.recv",
                    "session.inbox_subscribe",
                    "session.read_screen",
                    "session.write_input",
                ][..],
            ),
            (
                agents_host_openrpc(),
                &[
                    "host.session_create",
                    "host.session_list",
                    "host.session_move",
                    "host.session_close",
                    "host.session_restart",
                    "host.session_screen",
                    "host.session_write",
                    "host.msg_send",
                    "host.msg_recv",
                    "host.msg_subscribe",
                    "host.list_hosts",
                    "host.relay_attach",
                ][..],
            ),
        ] {
            let methods = doc["methods"].as_array().unwrap();
            for name in names {
                assert_eq!(
                    methods.iter().filter(|m| m["name"] == *name).count(),
                    1,
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn agent_activity_openrpc_matches_wire_contract() {
        let host = agents_host_openrpc();
        let methods = host["methods"].as_array().unwrap();
        let mut mark_seen = methods.iter().filter(|m| m["name"] == "host.tab_mark_seen");
        let params = &mark_seen.next().expect("host.tab_mark_seen")["params"][0]["schema"];
        assert!(mark_seen.next().is_none());
        assert_eq!(params["additionalProperties"], false);
        assert_eq!(
            params["required"],
            json!(["tab_id", "run_generation", "seen_seq"])
        );
        assert!(params["properties"].get("session_token").is_none());
        assert!(
            methods
                .iter()
                .all(|m| m["name"] != "host.tab_activity_snapshot")
        );

        let create = methods
            .iter()
            .find(|m| m["name"] == "host.tab_create")
            .unwrap();
        for field in ["parent_session_id", "parent_host_id"] {
            assert!(
                create["params"][0]["schema"]["properties"]
                    .get(field)
                    .is_none()
            );
        }

        let terminal = terminal_server_openrpc();
        let host_events = &host["components"]["schemas"]["HostEvent"];
        let pty_events = &terminal["components"]["schemas"]["PtyEvent"];
        for (schema, expected) in [
            (host_events, ["tab_activity_changed", "tab_seen_changed"]),
            (pty_events, ["agent_status_changed", "foreground_changed"]),
        ] {
            let variants = schema["oneOf"].as_array().unwrap();
            for name in expected {
                assert!(
                    variants
                        .iter()
                        .any(|event| event["properties"]["type"]["const"] == name),
                    "missing {name}"
                );
            }
            let status = schema["$defs"]["AgentStatus"]["properties"]
                .as_object()
                .unwrap();
            assert!(!status.contains_key("turn"));
        }
    }

    #[test]
    fn openrpc_schemas_are_valid_and_up_to_date() {
        let docs = [
            (
                "ddg-terminal-server.openrpc.json",
                terminal_server_openrpc(),
            ),
            ("ddg-agents-host.openrpc.json", agents_host_openrpc()),
        ];
        let update = std::env::var_os("DDG_UPDATE_SCHEMA").is_some();
        let dir = schema_dir();
        if update {
            std::fs::create_dir_all(&dir).unwrap();
        }
        for (file, doc) in docs {
            assert_eq!(doc["openrpc"], "1.3.2");
            let methods = doc["methods"].as_array().unwrap();
            let hello = methods.iter().find(|m| m["name"] == "hello").unwrap();
            let schema = &hello["result"]["schema"];
            let mut fields = vec![
                "app_version",
                "platform",
                "holder_protocol_version",
                "active_pty_count",
            ];
            if file == "ddg-agents-host.openrpc.json" {
                fields.extend(["terminal_server_version", "rollback_version"]);
                assert!(schema["properties"].get("rollback_available").is_none());
            } else {
                fields.push("holder_instance");
            }
            for field in fields {
                assert!(schema["properties"].get(field).is_some(), "{file}: {field}");
                assert!(
                    !schema["required"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(field))
                );
            }
            let formatted = to_pretty_json(doc);
            let path = dir.join(file);
            if update {
                std::fs::write(&path, &formatted).unwrap();
            } else {
                let existing = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                    panic!(
                        "missing schema {}: {e} (run with DDG_UPDATE_SCHEMA=1)",
                        path.display()
                    )
                });
                assert_eq!(
                    existing.replace("\r\n", "\n"),
                    formatted,
                    "stale schema {} (run with DDG_UPDATE_SCHEMA=1)",
                    path.display()
                );
            }
        }
    }
}
