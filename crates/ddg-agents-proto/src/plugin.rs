//! Custom backend plugin extension contracts (`ext.<plugin_name>.*`) for `ddg-agents-host`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginCapability {
    HostRead,
    HostWrite,
    PtyObserve,
    PtyControl,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<PluginCapability>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    10_000
}

impl PluginManifest {
    pub fn validate(&self) -> Result<(), String> {
        if !is_valid_plugin_name(&self.name) {
            return Err(format!(
                "invalid plugin name {:?}: must match ^[a-z][a-z0-9_-]{{0,31}}$",
                self.name
            ));
        }
        if self.command.is_empty() || self.command[0].trim().is_empty() {
            return Err("plugin command must not be empty".into());
        }
        if self.timeout_ms == 0 || self.timeout_ms > 120_000 {
            return Err(format!(
                "plugin timeout_ms {} out of range (1..=120000)",
                self.timeout_ms
            ));
        }
        Ok(())
    }

    pub fn namespace_prefix(&self) -> String {
        format!("ext.{}.", self.name)
    }
}

pub fn is_valid_plugin_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=32).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

pub fn is_valid_ext_method(plugin_name: &str, method: &str) -> bool {
    if !is_valid_plugin_name(plugin_name) {
        return false;
    }
    let Some(rest) = method
        .strip_prefix("ext.")
        .and_then(|s| s.strip_prefix(plugin_name))
        .and_then(|s| s.strip_prefix('.'))
    else {
        return false;
    };
    !rest.is_empty()
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PluginMethodSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_schema: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PluginHelloResult {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub methods: Vec<PluginMethodSpec>,
    #[serde(default)]
    pub events: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_plugin_name_and_ext_namespace() {
        assert!(is_valid_plugin_name("git"));
        assert!(is_valid_plugin_name("bot_runner-2"));
        assert!(!is_valid_plugin_name(""));
        assert!(!is_valid_plugin_name("Git"));
        assert!(!is_valid_plugin_name("1git"));
        assert!(!is_valid_plugin_name("a.b"));

        assert!(is_valid_ext_method("git", "ext.git.status"));
        assert!(is_valid_ext_method("git", "ext.git.diff.file"));
        assert!(!is_valid_ext_method("git", "host.tab_close"));
        assert!(!is_valid_ext_method("git", "pty.kill"));
        assert!(!is_valid_ext_method("git", "ext.other.status"));
        assert!(!is_valid_ext_method("git", "ext.git."));
    }
}
