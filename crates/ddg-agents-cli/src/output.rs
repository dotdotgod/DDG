use ddg_agents_proto::{
    id::sanitize_display_text,
    rpc::{ErrorKind, RpcError},
};
use ddg_agents_transport::{DialError, DialErrorKind, sanitize_dial_error};
use ddg_agents_update::{UpdateError, UpdateErrorCode, redact::redact_sensitive_text};
use serde::Serialize;
use serde_json::json;
use std::io::{self, Write};

pub type Result<T = ()> = std::result::Result<T, Error>;

#[derive(Debug, Serialize)]
pub struct Error {
    pub code: String,
    pub message: String,
}

impl Error {
    pub fn new(code: &str, message: impl AsRef<str>) -> Self {
        let message = message.as_ref();
        let lower = message.to_ascii_lowercase();
        let message = if lower.contains("private key")
            || lower.contains("ddg_session_token")
            || message
                .split_whitespace()
                .any(|word| word.contains("ses_") && word.contains('.'))
        {
            "diagnostic redacted".into()
        } else {
            diagnostic(message)
        };
        Self {
            code: clean(&redact_sensitive_text(code), 80),
            message,
        }
    }

    pub fn usage(message: impl AsRef<str>) -> Self {
        Self::new("invalid_params", message)
    }
}

impl From<UpdateError> for Error {
    fn from(error: UpdateError) -> Self {
        Self::update(error.code, error.message())
    }
}

impl Error {
    pub fn update(code: UpdateErrorCode, message: impl AsRef<str>) -> Self {
        let code = serde_json::to_value(code).expect("serializable update error code");
        Self::new(code.as_str().expect("string update error code"), message)
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::new("io_error", error.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::new("json_error", error.to_string())
    }
}

impl From<RpcError> for Error {
    fn from(error: RpcError) -> Self {
        let code = error.data.code.as_deref().unwrap_or(match error.data.kind {
            ErrorKind::InvalidParams => "invalid_params",
            ErrorKind::NotFound => "not_found",
            ErrorKind::ControlDenied => "control_denied",
            ErrorKind::Conflict => "conflict",
            ErrorKind::Unavailable => "unavailable",
            ErrorKind::HostKeyMismatch => "host_key_mismatch",
            ErrorKind::AuthRequired => "auth_required",
            ErrorKind::PluginUnavailable => "plugin_unavailable",
            ErrorKind::PermissionDenied => "permission_denied",
            ErrorKind::Internal => "internal",
        });
        Self::new(code, error.message)
    }
}

pub fn tab_error(error: RpcError, host: &str) -> Error {
    // Modern hosts use a distinct stable code for a missing session, not an unknown method.
    let missing_method = error.data.kind == ErrorKind::NotFound && error.data.code.is_none();
    let mut result = Error::from(error);
    if missing_method {
        result.message.push_str(&format!(
            "; remote host may need upgrade (run ddg machine install {} or restart local ddg-agents-host)",
            diagnostic(host)
        ));
    }
    result
}

pub fn dial_status(kind: DialErrorKind) -> &'static str {
    match kind {
        DialErrorKind::HostNotInstalled => "not_installed",
        DialErrorKind::HostKeyMismatch => "host_key_mismatch",
        DialErrorKind::AuthRequired => "auth_required",
        DialErrorKind::ProtocolIncompatible => "protocol_incompatible",
        _ => "unavailable",
    }
}

impl From<DialError> for Error {
    fn from(error: DialError) -> Self {
        let code = match error.kind {
            DialErrorKind::InvalidProfile => "invalid_profile",
            DialErrorKind::SecurityBlocked => "host_security_blocked",
            DialErrorKind::HostIdMismatch => "host_id_mismatch",
            kind => dial_status(kind),
        };
        Self::new(
            code,
            sanitize_dial_error(&redact_sensitive_text(&error.message)),
        )
    }
}

fn clean(text: &str, max: usize) -> String {
    sanitize_display_text(text, max)
}

pub fn diagnostic(text: &str) -> String {
    clean(&redact_sensitive_text(text), 1024)
}

pub fn cell(text: &str) -> String {
    clean(text, 240)
}

pub fn table<const N: usize>(
    header: [&str; N],
    rows: impl IntoIterator<Item = [String; N]>,
) -> String {
    let mut out = header.join("\t");
    out.push('\n');
    for row in rows {
        out.push_str(&row.map(|value| cell(&value)).join("\t"));
        out.push('\n');
    }
    out
}

#[derive(Clone, Copy)]
pub struct Output(pub bool);

impl Output {
    pub fn show(self, value: &impl Serialize, plain: impl FnOnce() -> String) -> Result {
        let mut stdout = io::stdout().lock();
        if self.0 {
            serde_json::to_writer(&mut stdout, value)?;
            stdout.write_all(b"\n")?;
        } else {
            stdout.write_all(plain().as_bytes())?;
        }
        stdout.flush()?;
        Ok(())
    }

    pub fn error(self, error: &Error, update: bool) {
        if self.0 && update {
            let _ = self.show(&json!({"error": error}), String::new);
        } else {
            eprintln!("{}", self.error_line(error));
        }
    }

    pub(crate) fn error_line(self, error: &Error) -> String {
        if self.0 {
            json!({"error": error}).to_string()
        } else {
            format!("ddg: {}: {}", error.code, error.message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_secrets_unconditionally_before_rendering() {
        for (input, secret) in [
            (
                "GITHUB_TOKEN=<token> on one line\nGITHUB_TOKEN=real_secret",
                "real_secret",
            ),
            ("gh_token=assigned", "assigned"),
            ("Authorization: header-secret", "header-secret"),
            ("Bearer bearer-secret", "bearer-secret"),
            ("https://account:password@example.com/", "account:password"),
        ] {
            let error = Error::new("network_error", input);
            for json in [false, true] {
                let line = Output(json).error_line(&error);
                assert!(!line.contains(secret));
                assert!(!line.contains("<token>"));
                if json {
                    assert!(
                        serde_json::from_str::<serde_json::Value>(&line)
                            .unwrap()
                            .is_object()
                    );
                }
            }
        }
        for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"] {
            assert!(
                !Error::usage(format!("bad {prefix}secret"))
                    .message
                    .contains("secret")
            );
        }
    }

    #[test]
    fn tables_and_errors_strip_terminal_controls() {
        let text = table(
            ["HOST", "TITLE"],
            [["a\t\nb".into(), "x\x1b[31m\u{202e}\u{2028}y".into()]],
        );
        assert_eq!(text.lines().count(), 2);
        assert!(!text.contains(['\x1b', '\u{202e}', '\u{2028}']));
        assert_eq!(text.lines().nth(1).unwrap().split('\t').count(), 2);
        let error = Error::from(DialError::new(
            DialErrorKind::AuthRequired,
            "load key /home/user/secret",
        ));
        assert_eq!(error.code, "auth_required");
        assert!(!error.message.contains("secret"));
        let error = Error::from(RpcError::not_found("DDG_SESSION_TOKEN=secret"));
        assert_eq!(error.message, "diagnostic redacted");
    }

    #[test]
    fn older_method_hint_does_not_mislabel_missing_sessions() {
        assert!(
            tab_error(RpcError::not_found("method not found"), "work")
                .message
                .contains("ddg machine install work")
        );
        assert!(
            !tab_error(RpcError::session_not_found("session not found"), "work")
                .message
                .contains("upgrade")
        );
    }
}
