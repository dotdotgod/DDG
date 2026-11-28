//! Host profile definitions, strict argument validation, and `hosts.json` persistence.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const HOSTS_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostTransport {
    Local {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sock_path: Option<String>,
    },
    Ssh {
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        known_hosts_file: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity_file: Option<String>,
    },
    DockerExec {
        container: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ssh_host: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        known_hosts_file: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity_file: Option<String>,
    },
    ContainerSsh {
        host: String,
        port: u16,
        user: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        known_hosts_file: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity_file: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostProfile {
    pub id: String,
    pub label: String,
    pub transport: HostTransport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_host_id: Option<String>,
}

impl HostProfile {
    pub fn local_default() -> Self {
        Self {
            id: "local".into(),
            label: "Local".into(),
            transport: HostTransport::Local { sock_path: None },
            expected_host_id: None,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let id = self.id.trim();
        if id.is_empty() {
            return Err("host profile id must not be empty".into());
        }
        if id.eq_ignore_ascii_case("all") {
            return Err("host profile id 'all' is reserved".into());
        }
        if !id
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_' | b'.'))
        {
            return Err("invalid host profile id".into());
        }
        if self.label.trim().is_empty() {
            return Err("host profile label must not be empty".into());
        }
        match &self.transport {
            HostTransport::Local { sock_path } => {
                if let Some(p) = sock_path
                    && (p.trim().is_empty() || p.contains('\0'))
                {
                    return Err("invalid local socket path".into());
                }
            }
            HostTransport::Ssh {
                target,
                port,
                known_hosts_file,
                identity_file,
            } => {
                validate_ssh_target(target)?;
                if let Some(0) = port {
                    return Err("ssh port must be non-zero".into());
                }
                validate_optional_path(known_hosts_file, "known_hosts_file")?;
                validate_optional_path(identity_file, "identity_file")?;
            }
            HostTransport::DockerExec {
                container,
                ssh_host,
                known_hosts_file,
                identity_file,
            } => {
                validate_docker_container(container)?;
                if let Some(host) = ssh_host {
                    validate_ssh_target(host)?;
                }
                validate_optional_path(known_hosts_file, "known_hosts_file")?;
                validate_optional_path(identity_file, "identity_file")?;
            }
            HostTransport::ContainerSsh {
                host,
                port,
                user,
                known_hosts_file,
                identity_file,
            } => {
                validate_ssh_target(host)?;
                if *port == 0 {
                    return Err("container ssh port must be non-zero".into());
                }
                validate_ssh_user(user)?;
                validate_optional_path(known_hosts_file, "known_hosts_file")?;
                validate_optional_path(identity_file, "identity_file")?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostsConfig {
    pub schema_version: u32,
    pub default_host_id: String,
    pub hosts: Vec<HostProfile>,
}

impl Default for HostsConfig {
    fn default() -> Self {
        Self {
            schema_version: HOSTS_SCHEMA_VERSION,
            default_host_id: "local".into(),
            hosts: vec![HostProfile::local_default()],
        }
    }
}

impl HostsConfig {
    #[cfg(windows)]
    pub fn default_path() -> io::Result<PathBuf> {
        Ok(ddg_agents_os::paths::config_dir()?.join("hosts.json"))
    }

    #[cfg(unix)]
    pub fn default_path() -> io::Result<PathBuf> {
        if let Some(dir) = std::env::var_os("DDG_CONFIG_DIR") {
            return Ok(PathBuf::from(dir).join("hosts.json"));
        }
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(xdg).join("ddg/hosts.json"));
        }
        if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
            return Ok(PathBuf::from(home).join(".config/ddg/hosts.json"));
        }
        Err(io::Error::other(
            "cannot resolve DDG config directory (set HOME or DDG_CONFIG_DIR)",
        ))
    }

    pub fn load_or_default(path: &Path) -> Self {
        let Ok(bytes) = fs::read(path) else {
            return Self::default();
        };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(cfg) if cfg.validate().is_ok() && !cfg.hosts.is_empty() => cfg,
            _ => Self::default(),
        }
    }

    #[cfg(windows)]
    pub fn save(&self, path: &Path) -> io::Result<()> {
        self.validate().map_err(io::Error::other)?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            ddg_agents_os::fs::ensure_private_dir(parent)?;
        }
        ddg_agents_os::fs::write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }

    #[cfg(unix)]
    pub fn save(&self, path: &Path) -> io::Result<()> {
        self.validate().map_err(io::Error::other)?;
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        fs::write(&tmp, &bytes)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != HOSTS_SCHEMA_VERSION {
            return Err(format!(
                "unsupported hosts.json schema_version {}",
                self.schema_version
            ));
        }
        let mut ids = HashSet::new();
        for host in &self.hosts {
            host.validate()?;
            if !ids.insert(host.id.trim().to_ascii_lowercase()) {
                return Err(format!("duplicate host profile id: {}", host.id.trim()));
            }
        }
        Ok(())
    }

    pub fn find(&self, id: &str) -> Option<&HostProfile> {
        self.hosts
            .iter()
            .find(|h| h.id.trim().eq_ignore_ascii_case(id.trim()))
    }

    pub fn default_profile(&self) -> HostProfile {
        self.find(&self.default_host_id)
            .or_else(|| self.hosts.first())
            .cloned()
            .unwrap_or_else(HostProfile::local_default)
    }
}

pub fn validate_ssh_target(target: &str) -> Result<(), String> {
    if target.is_empty() || target.len() > 256 {
        return Err("ssh target length must be 1..=256".into());
    }
    if target.starts_with('-') {
        return Err(format!("ssh target must not start with '-': {target:?}"));
    }
    for ch in target.chars() {
        let allowed =
            ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '@' | ':' | '[' | ']');
        if !allowed {
            return Err(format!("disallowed character {ch:?} in ssh target"));
        }
    }
    Ok(())
}

pub fn validate_docker_container(container: &str) -> Result<(), String> {
    if container.is_empty() || container.len() > 128 {
        return Err("docker container name length must be 1..=128".into());
    }
    let mut chars = container.chars();
    let Some(first) = chars.next() else {
        return Err("empty container name".into());
    };
    if !first.is_ascii_alphanumeric() {
        return Err(format!(
            "docker container name must start with alphanumeric: {container:?}"
        ));
    }
    for ch in chars {
        if !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-')) {
            return Err(format!(
                "disallowed character {ch:?} in docker container name"
            ));
        }
    }
    Ok(())
}

pub fn validate_ssh_user(user: &str) -> Result<(), String> {
    if user.is_empty() || user.len() > 64 {
        return Err("ssh user length must be 1..=64".into());
    }
    if user.starts_with('-') {
        return Err("ssh user must not start with '-'".into());
    }
    for ch in user.chars() {
        if !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-')) {
            return Err(format!("disallowed character {ch:?} in ssh user"));
        }
    }
    Ok(())
}

fn validate_optional_path(path: &Option<String>, field: &str) -> Result<(), String> {
    if let Some(p) = path
        && (p.trim().is_empty() || p.contains('\0') || p.contains('\n') || p.contains('\r'))
    {
        return Err(format!("invalid {field}"));
    }
    Ok(())
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
    {
        return PathBuf::from(home)
            .join(rest)
            .to_string_lossy()
            .into_owned();
    }
    path.to_owned()
}

pub(crate) fn unsafe_text_char(ch: char) -> bool {
    ch.is_control()
        || matches!(ch, '\u{2028}' | '\u{2029}' | '\u{061c}' | '\u{200e}' | '\u{200f}'
            | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Splits a single shell-like command line into literal arguments without
/// shell evaluation. Unquoted shell operators and control characters are rejected.
pub fn split_safe_tokens(raw: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut has_token = false;

    for ch in raw.chars() {
        if unsafe_text_char(ch) && !(ch == '\t' && !escaped && !in_single && !in_double) {
            return Err("disallowed control character in command".into());
        }
        if escaped {
            current.push(ch);
            escaped = false;
            has_token = true;
            continue;
        }
        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                current.push(ch);
            }
            continue;
        }
        if in_double {
            match ch {
                '"' => in_double = false,
                '\\' => escaped = true,
                '$' | '`' => {
                    return Err(format!("disallowed shell expansion character {ch:?}"));
                }
                _ => current.push(ch),
            }
            continue;
        }
        match ch {
            '\'' => {
                in_single = true;
                has_token = true;
            }
            '"' => {
                in_double = true;
                has_token = true;
            }
            '\\' => {
                escaped = true;
                has_token = true;
            }
            ' ' | '\t' => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            ';' | '|' | '&' | '$' | '`' | '<' | '>' => {
                return Err(format!("disallowed shell operator {ch:?} in command"));
            }
            _ => {
                current.push(ch);
                has_token = true;
            }
        }
    }
    if in_single || in_double || escaped {
        return Err("unterminated quote or escape in command".into());
    }
    if has_token {
        tokens.push(current);
    }
    Ok(tokens)
}

/// Parses either a plain SSH target (`dev@gpu-box`, `dotdot-home`) or a full
/// SSH connect command (`ssh -i ~/.ssh/key.pem -p 2222 ubuntu@1.2.3.4`) into a
/// validated `HostTransport::Ssh`. Strictly rejects unsafe `-o` options such as
/// `ProxyCommand` or `StrictHostKeyChecking=no`.
pub fn parse_ssh_input(raw: &str) -> Result<HostTransport, String> {
    let tokens = split_safe_tokens(raw)?;
    if tokens.is_empty() {
        return Err("Enter an SSH target or command.".into());
    }

    // Plain single target (no leading `ssh` command).
    if tokens.len() == 1 && tokens[0] != "ssh" {
        let target = tokens[0].clone();
        validate_ssh_target(&target)?;
        return Ok(HostTransport::Ssh {
            target,
            port: None,
            known_hosts_file: None,
            identity_file: None,
        });
    }

    let mut idx = if tokens[0] == "ssh" || tokens[0].ends_with("/ssh") {
        1
    } else {
        0
    };

    let mut port: Option<u16> = None;
    let mut user: Option<String> = None;
    let mut identity_file: Option<String> = None;
    let mut known_hosts_file: Option<String> = None;
    let mut target: Option<String> = None;
    let mut after_double_dash = false;

    while idx < tokens.len() {
        let tok = &tokens[idx];
        if !after_double_dash && tok == "--" {
            after_double_dash = true;
            idx += 1;
            continue;
        }
        if !after_double_dash && tok.starts_with('-') && tok.len() >= 2 && target.is_none() {
            match tok.as_str() {
                "-p" => {
                    idx += 1;
                    let val = tokens.get(idx).ok_or("missing port after -p")?;
                    let p: u16 = val.parse().map_err(|_| format!("invalid port {val:?}"))?;
                    if p == 0 {
                        return Err("ssh port must be non-zero".into());
                    }
                    port = Some(p);
                }
                "-i" => {
                    idx += 1;
                    let val = tokens.get(idx).ok_or("missing identity path after -i")?;
                    identity_file = Some(expand_tilde(val));
                }
                "-l" => {
                    idx += 1;
                    let val = tokens.get(idx).ok_or("missing user after -l")?;
                    validate_ssh_user(val)?;
                    user = Some(val.clone());
                }
                "-o" => {
                    idx += 1;
                    let opt = tokens.get(idx).ok_or("missing option after -o")?;
                    apply_safe_ssh_option(
                        opt,
                        &mut port,
                        &mut user,
                        &mut identity_file,
                        &mut known_hosts_file,
                    )?;
                }
                // Benign transport flags that can safely be skipped (DDG always sets -T).
                "-T" | "-t" | "-tt" | "-A" | "-a" | "-4" | "-6" | "-q" | "-v" | "-vv" | "-vvv"
                | "-C" | "-N" => {}
                other if other.starts_with("-p") => {
                    let val = &other[2..];
                    let p: u16 = val.parse().map_err(|_| format!("invalid port {val:?}"))?;
                    if p == 0 {
                        return Err("ssh port must be non-zero".into());
                    }
                    port = Some(p);
                }
                other if other.starts_with("-i") => {
                    identity_file = Some(expand_tilde(&other[2..]));
                }
                other if other.starts_with("-l") => {
                    let val = &other[2..];
                    validate_ssh_user(val)?;
                    user = Some(val.to_owned());
                }
                other if other.starts_with("-o") => {
                    apply_safe_ssh_option(
                        &other[2..],
                        &mut port,
                        &mut user,
                        &mut identity_file,
                        &mut known_hosts_file,
                    )?;
                }
                other => {
                    return Err(format!("Unsupported SSH option: {other}"));
                }
            }
            idx += 1;
            continue;
        }

        if target.is_none() {
            target = Some(tok.clone());
            idx += 1;
        } else {
            // Ignore any trailing remote shell command (e.g. `ssh host bash`).
            break;
        }
    }

    let raw_target = target.ok_or("Could not find an SSH target host.")?;
    let final_target = match user {
        Some(u) if !raw_target.contains('@') => format!("{u}@{raw_target}"),
        _ => raw_target,
    };
    validate_ssh_target(&final_target)?;
    validate_optional_path(&identity_file, "identity_file")?;
    validate_optional_path(&known_hosts_file, "known_hosts_file")?;

    Ok(HostTransport::Ssh {
        target: final_target,
        port,
        known_hosts_file,
        identity_file,
    })
}

fn apply_safe_ssh_option(
    opt: &str,
    port: &mut Option<u16>,
    user: &mut Option<String>,
    identity_file: &mut Option<String>,
    known_hosts_file: &mut Option<String>,
) -> Result<(), String> {
    let (k, v) = opt
        .split_once('=')
        .or_else(|| opt.split_once(' '))
        .ok_or_else(|| format!("invalid -o option {opt:?}"))?;
    let key = k.trim().to_ascii_lowercase();
    let val = v.trim();
    match key.as_str() {
        "port" => {
            let p: u16 = val.parse().map_err(|_| format!("invalid port {val:?}"))?;
            if p == 0 {
                return Err("ssh port must be non-zero".into());
            }
            *port = Some(p);
        }
        "user" => {
            validate_ssh_user(val)?;
            *user = Some(val.to_owned());
        }
        "identityfile" => {
            *identity_file = Some(expand_tilde(val));
        }
        "userknownhostsfile" => {
            *known_hosts_file = Some(expand_tilde(val));
        }
        "identitiesonly"
        | "batchmode"
        | "connecttimeout"
        | "serveraliveinterval"
        | "serveralivecountmax" => {}
        "stricthostkeychecking" if val.eq_ignore_ascii_case("yes") => {}
        _ => {
            return Err(format!("SSH -o option is blocked for security: {k}"));
        }
    }
    Ok(())
}

/// Parses either a plain Docker container name (`ddg-dev`) or a pasted
/// `docker exec -it ddg-dev bash` command and returns the validated container name.
pub fn parse_docker_input(raw: &str) -> Result<String, String> {
    let tokens = split_safe_tokens(raw)?;
    if tokens.is_empty() {
        return Err("Enter a container name.".into());
    }
    if tokens.len() == 1 {
        validate_docker_container(&tokens[0])?;
        return Ok(tokens[0].clone());
    }
    if tokens[0] == "docker" && tokens.get(1).map(String::as_str) == Some("exec") {
        let mut idx = 2;
        while idx < tokens.len() {
            let t = &tokens[idx];
            if t == "-u"
                || t == "--user"
                || t == "-w"
                || t == "--workdir"
                || t == "-e"
                || t == "--env"
            {
                idx += 2;
                continue;
            }
            if t.starts_with('-') {
                idx += 1;
                continue;
            }
            validate_docker_container(t)?;
            return Ok(t.clone());
        }
        return Err("Could not find a container name in the docker exec command.".into());
    }
    Err("Enter a valid container name or `docker exec ...` command.".into())
}

/// Reads non-wildcard `Host` aliases from `~/.ssh/config` for quick selection.
pub fn discover_ssh_config_hosts() -> Vec<String> {
    let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) else {
        return Vec::new();
    };
    let path = PathBuf::from(home).join(".ssh/config");
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_ssh_config_hosts(&text)
}

pub fn parse_ssh_config_hosts(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        let mut parts = line.split_whitespace();
        let Some(keyword) = parts.next() else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case("Host") {
            continue;
        }
        for alias in parts {
            if alias.contains('*') || alias.contains('?') || alias.starts_with('!') {
                continue;
            }
            if validate_ssh_target(alias).is_ok() && !out.iter().any(|x| x == alias) {
                out.push(alias.to_owned());
            }
        }
    }
    out
}

/// Lists locally running Docker container names (`docker ps --format {{.Names}}`).
pub fn discover_docker_containers() -> Vec<String> {
    let Ok(output) = std::process::Command::new("docker")
        .args(["ps", "--format", "{{.Names}}"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|name| validate_docker_container(name).is_ok())
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_local_label_does_not_replace_a_saved_label() {
        let mut config = HostsConfig::default();
        assert_eq!(config.hosts[0].id, "local");
        assert_eq!(config.hosts[0].label, "Local");

        let custom_label = "\u{b85c}\u{ceec}";
        config.hosts[0].label = custom_label.into();
        let dir = std::env::temp_dir().join(ddg_agents_proto::id::generate_message_id().unwrap());
        fs::create_dir(&dir).unwrap();
        let path = dir.join("hosts.json");
        config.save(&path).unwrap();
        let loaded = HostsConfig::load_or_default(&path);
        fs::remove_dir_all(dir).unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn rejects_reserved_invalid_and_duplicate_profile_ids() {
        for id in ["all", "ALL", " all "] {
            let profile = HostProfile {
                id: id.into(),
                ..HostProfile::local_default()
            };
            assert!(
                profile.validate().unwrap_err().contains("reserved"),
                "{id:?}"
            );
            let config = HostsConfig {
                hosts: vec![profile],
                ..HostsConfig::default()
            };
            assert!(config.validate().unwrap_err().contains("reserved"));
        }
        for id in ["", "   ", "has spaces", "has/slash"] {
            let profile = HostProfile {
                id: id.into(),
                ..HostProfile::local_default()
            };
            assert!(profile.validate().is_err(), "{id:?}");
        }
        for duplicate in ["local", " LOCAL "] {
            let mut config = HostsConfig::default();
            config.hosts.push(HostProfile {
                id: duplicate.into(),
                ..HostProfile::local_default()
            });
            assert!(config.validate().unwrap_err().contains("duplicate"));
        }
    }

    #[test]
    fn rejects_option_and_shell_injection_in_profiles() {
        assert!(validate_ssh_target("-oProxyCommand=id").is_err());
        assert!(validate_ssh_target("user@host; rm -rf /").is_err());
        assert!(validate_ssh_target("user@host\nid").is_err());
        assert!(validate_ssh_target("dev@10.0.0.5").is_ok());
        assert!(validate_ssh_target("gpu-box").is_ok());

        assert!(validate_docker_container("ddg-dev_1.0").is_ok());
        assert!(validate_docker_container("-it").is_err());
        assert!(validate_docker_container("dev;id").is_err());
        assert!(validate_docker_container("dev $(whoami)").is_err());
        assert!(validate_docker_container("").is_err());

        let bad_profile = HostProfile {
            id: "c1".into(),
            label: "Bad".into(),
            transport: HostTransport::DockerExec {
                container: "foo;bar".into(),
                ssh_host: None,
                known_hosts_file: None,
                identity_file: None,
            },
            expected_host_id: None,
        };
        assert!(bad_profile.validate().is_err());
    }

    #[test]
    fn parses_ssh_commands_safely_and_blocks_unsafe_options() {
        assert_eq!(
            parse_ssh_input("").unwrap_err(),
            "Enter an SSH target or command."
        );
        assert_eq!(
            parse_ssh_input("dotdot-home").unwrap(),
            HostTransport::Ssh {
                target: "dotdot-home".into(),
                port: None,
                known_hosts_file: None,
                identity_file: None,
            }
        );

        assert_eq!(
            parse_ssh_input("ssh -i /tmp/key.pem -p 2222 ubuntu@10.0.0.8").unwrap(),
            HostTransport::Ssh {
                target: "ubuntu@10.0.0.8".into(),
                port: Some(2222),
                known_hosts_file: None,
                identity_file: Some("/tmp/key.pem".into()),
            }
        );

        assert_eq!(
            parse_ssh_input("ssh -l ddg -p2200 -o UserKnownHostsFile=/tmp/kh 127.0.0.1").unwrap(),
            HostTransport::Ssh {
                target: "ddg@127.0.0.1".into(),
                port: Some(2200),
                known_hosts_file: Some("/tmp/kh".into()),
                identity_file: None,
            }
        );

        assert!(parse_ssh_input("ssh -o ProxyCommand='nc %h %p' host").is_err());
        assert!(parse_ssh_input("ssh -o StrictHostKeyChecking=no host").is_err());
        assert!(parse_ssh_input("ssh host; whoami").is_err());
        assert!(parse_ssh_input("ssh $(whoami)@host").is_err());
        assert!(parse_ssh_input("ssh host\n").is_err());
    }

    #[test]
    fn parses_docker_exec_command_and_ssh_config_aliases() {
        assert_eq!(
            parse_docker_input("docker exec -it").unwrap_err(),
            "Could not find a container name in the docker exec command."
        );
        assert_eq!(
            parse_docker_input("docker exec -it ddg-dev /bin/zsh").unwrap(),
            "ddg-dev"
        );
        assert_eq!(parse_docker_input("ddg-dev").unwrap(), "ddg-dev");
        assert!(parse_docker_input("docker exec -it bad;name bash").is_err());
        assert!(parse_docker_input("\u{2028}ddg-dev").is_err());

        let cfg = r#"
            # My machines
            Host *
                ServerAliveInterval 60
            Host dotdot-home dotdot-workstation
                HostName 10.0.0.1
            Host *.internal !bastion
                User dev
            Host gpu-box # comment
                User ubuntu
        "#;
        assert_eq!(
            parse_ssh_config_hosts(cfg),
            vec!["dotdot-home", "dotdot-workstation", "gpu-box"]
        );
    }
}
