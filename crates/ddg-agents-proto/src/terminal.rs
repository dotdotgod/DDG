//! `ddg-terminal-server` RPC types for handshakes, PTYs, and session messaging and I/O.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HelloParams {
    pub protocol_version: u32,
    #[serde(default)]
    pub framed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HelloResult {
    pub protocol_version: u32,
    pub server_id: String,
    pub boot_generation: u64,
    pub conn_id: u64,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_protocol_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_instance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_pty_count: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PingParams {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PingResult {
    pub ok: bool,
    pub server_id: String,
    pub boot_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ControlMode {
    Control,
    Readonly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ControlLease {
    pub conn_id: u64,
    pub epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PtySpawnParams {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub run_generation: u64,
    pub cols: u16,
    pub rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PtySpawnResult {
    pub pty_id: u32,
    pub pid: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
    Done,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BlockedKind {
    Permission,
    Question,
    Auth,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentStatusRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub state: AgentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<BlockedKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
    pub updated_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentStatus {
    pub state: AgentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<BlockedKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
    #[schemars(length(max = 16))]
    pub records: Vec<AgentStatusRecord>,
    pub status_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ForegroundHint {
    pub pid: u32,
    pub comm: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_hint: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PtyInfo {
    pub pty_id: u32,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub run_generation: u64,
    pub pid: u32,
    pub cols: u16,
    pub rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<ControlLease>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status: Option<AgentStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<ForegroundHint>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PtyListParams {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PtyListResult {
    pub ptys: Vec<PtyInfo>,
}

/// Probe only the fixed built-in basenames; the request must be an empty object.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PtyProbeBinsParams {}

impl<'de> Deserialize<'de> for PtyProbeBinsParams {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EmptyObject;

        impl<'de> serde::de::Visitor<'de> for EmptyObject {
            type Value = PtyProbeBinsParams;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an empty object")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                if let Some(key) = map.next_key::<String>()? {
                    return Err(serde::de::Error::unknown_field(&key, &[]));
                }
                Ok(PtyProbeBinsParams {})
            }
        }

        // Serde's derived struct deserializer would also accept an empty array.
        deserializer.deserialize_map(EmptyObject)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BinProbeStatus {
    Found,
    NotFound,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BinProbeItem {
    pub bin: String,
    pub status: BinProbeStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PtyProbeBinsResult {
    pub items: Vec<BinProbeItem>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PtySubscribeParams {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PtySubscribeResult {
    pub seq: u64,
    pub ptys: Vec<PtyInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PtyAttachParams {
    pub pty_id: u32,
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
pub struct PtyAttachResult {
    pub channel: u32,
    pub mode: ControlMode,
    pub control_epoch: u64,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PtyClaimControlParams {
    pub pty_id: u32,
    #[serde(default)]
    pub force: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PtyClaimControlResult {
    pub control_epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PtyReleaseControlParams {
    pub pty_id: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PtyReleaseControlResult {
    pub released: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PtyKillParams {
    pub pty_id: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PtyKillResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum CloseReason {
    Detached,
    Exited {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    Killed {
        signal: Option<i32>,
    },
    ServerStopped,
    RelayReplaced,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PtyEvent {
    Spawned {
        pty: PtyInfo,
    },
    Exited {
        pty_id: u32,
        #[serde(default)]
        session_id: String,
        #[serde(default)]
        run_generation: u64,
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    Resized {
        pty_id: u32,
        #[serde(default)]
        session_id: String,
        #[serde(default)]
        run_generation: u64,
        cols: u16,
        rows: u16,
    },
    CwdChanged {
        pty_id: u32,
        #[serde(default)]
        session_id: String,
        #[serde(default)]
        run_generation: u64,
        cwd: String,
    },
    TitleChanged {
        pty_id: u32,
        #[serde(default)]
        session_id: String,
        #[serde(default)]
        run_generation: u64,
        title: String,
    },
    ControlChanged {
        pty_id: u32,
        #[serde(default)]
        session_id: String,
        #[serde(default)]
        run_generation: u64,
        controller: Option<ControlLease>,
    },
    Resynced {
        pty_id: u32,
        #[serde(default)]
        session_id: String,
        #[serde(default)]
        run_generation: u64,
    },
    AgentStatusChanged {
        pty_id: u32,
        session_id: String,
        run_generation: u64,
        status: Option<AgentStatus>,
    },
    ForegroundChanged {
        pty_id: u32,
        session_id: String,
        run_generation: u64,
        foreground: Option<ForegroundHint>,
    },
    ExternalInput {
        session_id: String,
        pty_id: u32,
        run_generation: u64,
        byte_len: u32,
        from_session_id: String,
        from_host_id: String,
        is_remote: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSendParams {
    /// Destination mailbox, distinct from the exited session in a system notification.
    pub to_session_id: String,
    pub from_session_id: String,
    pub from_host_id: String,
    pub is_remote: bool,
    #[serde(flatten)]
    pub payload: SessionMessagePayload,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSendResult {
    pub message_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionRecvParams {
    pub session_id: String,
    pub max_messages: u32,
    pub wait_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionRecvResult {
    pub messages: Vec<SessionMessage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionMessage {
    pub message_id: String,
    /// Destination session.
    pub to_session_id: String,
    pub from_session_id: String,
    pub from_host_id: String,
    pub is_remote: bool,
    #[serde(flatten)]
    pub payload: SessionMessagePayload,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionMessagePayload {
    Message {
        /// Canonical RFC 4648 base64; at most 65536 decoded bytes.
        data_b64: String,
    },
    SessionExited {
        session_id: String,
        room_id: String,
        run_generation: u64,
        exit_code: Option<i32>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionInboxSubscribeParams {
    pub session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionInboxSubscribeResult {
    /// Nonzero 9-byte framed channel carrying credit-controlled NDJSON SessionMessage records.
    #[schemars(range(min = 1))]
    pub channel: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionReadScreenParams {
    pub session_id: String,
    #[schemars(range(min = 1, max = 2000))]
    pub lines: u32,
    #[schemars(range(min = 1, max = 262144))]
    pub max_bytes: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionReadScreenResult {
    pub text: String,
    pub run_generation: u64,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionWriteInputParams {
    pub session_id: String,
    /// Canonical RFC 4648 base64; 1 to 65536 decoded bytes.
    pub data_b64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_run_generation: Option<u64>,
    pub from_session_id: String,
    pub from_host_id: String,
    pub is_remote: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionWriteInputResult {
    pub byte_len: u32,
    pub run_generation: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn activity_types_and_catalog_v2_status_wire() {
        let mut wire = json!({"state": "idle", "records": [], "status_ms": 2});
        for (state, name) in [
            (AgentState::Idle, "idle"),
            (AgentState::Working, "working"),
            (AgentState::Blocked, "blocked"),
            (AgentState::Done, "done"),
            (AgentState::Error, "error"),
        ] {
            wire["state"] = json!(name);
            let status: AgentStatus = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(status.state, state);
            assert_eq!(json!(status), wire);
        }
        for (kind, name) in [
            (BlockedKind::Permission, "permission"),
            (BlockedKind::Question, "question"),
            (BlockedKind::Auth, "auth"),
        ] {
            let wire = json!({
                "state": "blocked", "kind": name, "app": "pi", "title": "Review",
                "msg": "Action needed", "progress": 50, "status_ms": 2,
                "records": [{"id": "job", "state": "blocked", "kind": name, "updated_ms": 2}],
            });
            let status: AgentStatus = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(status.kind, Some(kind));
            assert_eq!(status.records[0].kind, Some(kind));
            assert_eq!(json!(status), wire);
        }
        for (field, value) in [
            ("state", json!("running")),
            ("state", json!("waiting")),
            ("kind", json!("approval")),
            ("kind", json!("input")),
            ("turn", json!(1)),
        ] {
            let mut invalid = wire.clone();
            invalid[field] = value;
            assert!(
                serde_json::from_value::<AgentStatus>(invalid).is_err(),
                "{field}"
            );
        }
        wire["records"] = json!([{"state": "idle", "updated_ms": 1, "turn": 1}]);
        assert!(serde_json::from_value::<AgentStatus>(wire).is_err());
    }

    #[test]
    fn activity_types_and_catalog_v2_pty_wire() {
        let mut pty = json!({"pty_id": 1, "session_id": "ses_00000000000000000000000001",
            "run_generation": 2, "pid": 42, "cols": 80, "rows": 24, "title": ""});
        let info: PtyInfo = serde_json::from_value(pty.clone()).unwrap();
        assert_eq!(json!(info), pty);
        pty["agent_status"] = json!({"state": "working", "records": [], "status_ms": 1});
        pty["foreground"] = json!({"pid": 42, "comm": "node", "script_hint": "app.js"});
        let info: PtyInfo = serde_json::from_value(pty.clone()).unwrap();
        assert_eq!(json!(info), pty);
        for (tag, field, value) in [
            (
                "agent_status_changed",
                "status",
                pty["agent_status"].clone(),
            ),
            (
                "foreground_changed",
                "foreground",
                pty["foreground"].clone(),
            ),
        ] {
            let mut event = json!({"type": tag, "pty_id": 1, "session_id": pty["session_id"],
                "run_generation": 2});
            for value in [value, serde_json::Value::Null] {
                event[field] = value;
                let decoded: PtyEvent = serde_json::from_value(event.clone()).unwrap();
                assert_eq!(json!(decoded), event);
            }
        }
    }

    #[test]
    fn session_send_round_trips_message_and_exit_payloads() {
        let message = json!({
            "to_session_id": "ses_00000000000000000000000002",
            "from_session_id": "ses_00000000000000000000000001",
            "from_host_id": "test", "is_remote": false,
            "kind": "message", "data_b64": "Zg==",
        });
        let exit = json!({
            "to_session_id": "ses_00000000000000000000000002",
            "from_session_id": "ses_00000000000000000000000001",
            "from_host_id": "test", "is_remote": false,
            "kind": "session_exited", "session_id": "ses_00000000000000000000000001",
            "room_id": "rm_bg_test", "run_generation": 2, "exit_code": 0,
        });
        for wire in [message, exit] {
            let send: SessionSendParams = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(json!(send), wire);
        }
    }

    #[test]
    fn session_write_input_accepts_missing_or_explicit_generation() {
        let mut wire = json!({
            "session_id": "ses_00000000000000000000000002", "data_b64": "Zg==",
            "from_session_id": "ses_00000000000000000000000001",
            "from_host_id": "test", "is_remote": false,
        });
        let write: SessionWriteInputParams = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(json!(write), wire);
        wire["expect_run_generation"] = json!(2);
        let write: SessionWriteInputParams = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(json!(write), wire);
    }

    #[test]
    fn session_mailbox_can_distinguish_messages_from_system_exits() {
        let normal = json!({
            "message_id": "msg_00000000000000000000000001",
            "to_session_id": "ses_00000000000000000000000002",
            "from_session_id": "ses_00000000000000000000000001",
            "from_host_id": "test", "is_remote": false,
            "kind": "message", "data_b64": "Zg==",
        });
        let mut exit = normal.clone();
        let fields = exit.as_object_mut().unwrap();
        fields.remove("data_b64");
        fields.extend(
            json!({
                "kind": "session_exited", "session_id": "ses_00000000000000000000000001",
                "room_id": "rm_bg_test", "run_generation": 2, "exit_code": 0,
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let inbox = json!({"messages": [normal, exit]});
        let received: SessionRecvResult = serde_json::from_value(inbox.clone()).unwrap();
        assert!(matches!(
            &received.messages[0].payload,
            SessionMessagePayload::Message { .. }
        ));
        assert!(matches!(
            &received.messages[1].payload,
            SessionMessagePayload::SessionExited { .. }
        ));
        assert_eq!(json!(received), inbox);
    }
}
