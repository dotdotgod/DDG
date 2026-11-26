//! JSON-RPC 2.0 envelope and stable DDG error kinds.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";

/// Stable values of `ErrorData::code` (distinct from the numeric JSON-RPC code).
pub mod error_code {
    pub const ROOM_TAB_LIMIT: &str = "room_tab_limit";
    pub const ROOM_NOT_FOUND: &str = "room_not_found";
    pub const SESSION_NOT_FOUND: &str = "session_not_found";
    pub const SESSION_NOT_RUNNING: &str = "session_not_running";
    pub const TOKEN_REVOKED: &str = "token_revoked";
    pub const TOKEN_INVALID: &str = "token_invalid";
    pub const GENERATION_MISMATCH: &str = "generation_mismatch";
    pub const HOST_NOT_FOUND: &str = "host_not_found";
    pub const HOST_OFFLINE: &str = "host_offline";
    pub const RELAY_TIMEOUT: &str = "relay_timeout";
    pub const HOST_SECURITY_BLOCKED: &str = "host_security_blocked";
    pub const HOST_AUTH_REQUIRED: &str = "host_auth_required";
    pub const HUB_UNAVAILABLE: &str = "hub_unavailable";
    pub const INVALID_PARAMS: &str = "invalid_params";
    pub const PROTOCOL_INCOMPATIBLE: &str = "protocol_incompatible";
    pub const MISSING_ENV: &str = "missing_env";
    pub const PERMISSION_DENIED: &str = "permission_denied";
}

macro_rules! coded_errors {
    ($($name:ident: $kind:ident, $code:ident;)*) => {
        $(
            pub fn $name(message: impl Into<String>) -> Self {
                Self::with_code(ErrorKind::$kind, error_code::$code, message, None)
            }
        )*
    };
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidParams,
    NotFound,
    ControlDenied,
    Conflict,
    Unavailable,
    HostKeyMismatch,
    AuthRequired,
    PluginUnavailable,
    PermissionDenied,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorData {
    pub kind: ErrorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    pub data: ErrorData,
}

impl RpcError {
    pub fn new(kind: ErrorKind, message: impl Into<String>, hint: Option<String>) -> Self {
        let code = match kind {
            ErrorKind::InvalidParams => -32602,
            ErrorKind::NotFound => -32001,
            ErrorKind::ControlDenied => -32002,
            ErrorKind::Conflict => -32003,
            ErrorKind::Unavailable => -32004,
            ErrorKind::HostKeyMismatch => -32005,
            ErrorKind::AuthRequired => -32006,
            ErrorKind::PluginUnavailable => -32007,
            ErrorKind::PermissionDenied => -32008,
            ErrorKind::Internal => -32603,
        };
        Self {
            code,
            message: message.into(),
            data: ErrorData {
                kind,
                code: None,
                hint,
            },
        }
    }

    pub fn with_code(
        kind: ErrorKind,
        code: impl Into<String>,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        let mut error = Self::new(kind, message, hint);
        error.data.code = Some(code.into());
        error
    }

    coded_errors! {
        room_tab_limit: Conflict, ROOM_TAB_LIMIT;
        room_not_found: NotFound, ROOM_NOT_FOUND;
        session_not_found: NotFound, SESSION_NOT_FOUND;
        session_not_running: Conflict, SESSION_NOT_RUNNING;
        token_revoked: PermissionDenied, TOKEN_REVOKED;
        token_invalid: PermissionDenied, TOKEN_INVALID;
        generation_mismatch: Conflict, GENERATION_MISMATCH;
        host_not_found: NotFound, HOST_NOT_FOUND;
        host_offline: Unavailable, HOST_OFFLINE;
        relay_timeout: Unavailable, RELAY_TIMEOUT;
        host_security_blocked: HostKeyMismatch, HOST_SECURITY_BLOCKED;
        host_auth_required: AuthRequired, HOST_AUTH_REQUIRED;
        hub_unavailable: Unavailable, HUB_UNAVAILABLE;
        invalid_params: InvalidParams, INVALID_PARAMS;
        missing_env: InvalidParams, MISSING_ENV;
        permission_denied: PermissionDenied, PERMISSION_DENIED;
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message, None)
    }

    pub fn control_denied(message: impl Into<String>) -> Self {
        Self::new(
            ErrorKind::ControlDenied,
            message,
            Some("request control before sending input or resize".into()),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn new(id: impl Into<Value>, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            method: method.into(),
            params,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stable_error_codes_coexist_with_numeric_codes_and_legacy_errors() {
        let legacy: RpcError = serde_json::from_value(json!({
            "code": -32602, "message": "invalid", "data": {"kind": "invalid_params"}
        }))
        .unwrap();
        assert_eq!(legacy.data.code, None);
        assert!(json!(legacy)["data"].get("code").is_none());
        assert_eq!(json!(RpcError::invalid_params("invalid"))["code"], -32602);
        for (error, code) in [
            (RpcError::room_tab_limit("test"), "room_tab_limit"),
            (RpcError::room_not_found("test"), "room_not_found"),
            (RpcError::session_not_found("test"), "session_not_found"),
            (RpcError::session_not_running("test"), "session_not_running"),
            (RpcError::token_revoked("test"), "token_revoked"),
            (RpcError::token_invalid("test"), "token_invalid"),
            (RpcError::generation_mismatch("test"), "generation_mismatch"),
            (RpcError::host_not_found("test"), "host_not_found"),
            (RpcError::host_offline("test"), "host_offline"),
            (RpcError::relay_timeout("test"), "relay_timeout"),
            (
                RpcError::host_security_blocked("test"),
                "host_security_blocked",
            ),
            (RpcError::host_auth_required("test"), "host_auth_required"),
            (RpcError::hub_unavailable("test"), "hub_unavailable"),
            (RpcError::invalid_params("test"), "invalid_params"),
            (RpcError::missing_env("test"), "missing_env"),
            (RpcError::permission_denied("test"), "permission_denied"),
        ] {
            assert_eq!(json!(error)["data"]["code"], code);
        }
    }
}
