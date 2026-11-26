//! Shared v1 launch profiles and compare-and-swap storage for `agents.json`.

use crate::profile::{HostsConfig, unsafe_text_char};
use ddg_agents_proto::host::{ExecutorSpec, validate_agent_profile_slug};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const AGENTS_SCHEMA_VERSION: u32 = 1;
pub const MAX_AGENTS_BYTES: usize = 512 * 1024;
pub const MAX_AGENT_PROFILES: usize = 128;
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentHostOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, Option<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    pub id: String,
    pub label: String,
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub host_overrides: BTreeMap<String, AgentHostOverride>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsConfig {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent_id: Option<String>,
    #[serde(default)]
    pub agents: Vec<AgentProfile>,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            schema_version: AGENTS_SCHEMA_VERSION,
            default_agent_id: None,
            agents: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAgent {
    pub executor: ExecutorSpec,
    pub cwd: Option<String>,
}

/// The metadata and original bytes observed when reading a configuration.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ConfigStamp {
    file: Option<StampedFile>,
}

impl fmt::Debug for ConfigStamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigStamp").finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct StampedFile {
    metadata: (i128, u64, u64),
    bytes: Vec<u8>,
}

#[derive(Debug)]
pub enum AgentConfigError {
    Io(io::Error),
    Invalid(String),
    Conflict,
    LockTimeout,
    /// The original is unchanged; pass `stamp` to a CAS save to reset it explicitly.
    Corrupt {
        backup: PathBuf,
        stamp: ConfigStamp,
    },
}

impl fmt::Display for AgentConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "agents.json: {e}"),
            Self::Invalid(rule) => write!(f, "agents.json: {rule}"),
            Self::Conflict => f.write_str("agents.json changed since it was read"),
            Self::LockTimeout => f.write_str("timed out waiting for agents.lock"),
            Self::Corrupt { .. } => {
                f.write_str("invalid agents.json structure; original bytes backed up")
            }
        }
    }
}

impl std::error::Error for AgentConfigError {}

impl From<io::Error> for AgentConfigError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn invalid(rule: impl Into<String>) -> AgentConfigError {
    AgentConfigError::Invalid(rule.into())
}

impl AgentsConfig {
    pub fn default_path() -> io::Result<PathBuf> {
        Ok(HostsConfig::default_path()?.with_file_name("agents.json"))
    }

    pub fn find(&self, id: &str) -> Option<&AgentProfile> {
        self.agents
            .iter()
            .find(|profile| profile.id.eq_ignore_ascii_case(id))
    }

    pub fn validate(&self) -> Result<(), AgentConfigError> {
        if self.schema_version != AGENTS_SCHEMA_VERSION {
            return Err(invalid("unsupported_schema_version"));
        }
        if self.agents.len() > MAX_AGENT_PROFILES {
            return Err(invalid("too_many_agent_profiles"));
        }
        let mut ids = HashSet::new();
        for profile in &self.agents {
            if !ids.insert(profile.id.to_ascii_lowercase()) {
                return Err(invalid("duplicate_agent_id"));
            }
            profile.validate()?;
        }
        if let Some(id) = &self.default_agent_id
            && !self.agents.iter().any(|profile| profile.id == *id)
        {
            return Err(invalid("invalid_default_agent_id"));
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<(Self, ConfigStamp), AgentConfigError> {
        match private_dir(path, false) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok((Self::default(), ConfigStamp::default()));
            }
            Err(e) => return Err(e.into()),
        }
        let stamp = read_stamp(path)?;
        let Some(file) = &stamp.file else {
            return Ok((Self::default(), stamp));
        };
        match serde_json::from_slice::<Self>(&file.bytes) {
            Ok(config) => {
                config.validate()?;
                Ok((config, stamp))
            }
            // Serde errors can include source strings (including invalid env values).
            Err(_) => {
                let _lock = lock_config(path, LOCK_TIMEOUT)?;
                if read_stamp(path)? != stamp {
                    return Err(AgentConfigError::Conflict);
                }
                let backup = backup_corrupt(path, &file.bytes, timestamp().as_secs())?;
                Err(AgentConfigError::Corrupt { backup, stamp })
            }
        }
    }

    pub fn save(
        &self,
        path: &Path,
        expected: &ConfigStamp,
    ) -> Result<ConfigStamp, AgentConfigError> {
        self.save_with_timeout(path, expected, LOCK_TIMEOUT)
    }

    pub fn save_with_timeout(
        &self,
        path: &Path,
        expected: &ConfigStamp,
        timeout: Duration,
    ) -> Result<ConfigStamp, AgentConfigError> {
        self.validate()?;
        let mut bytes =
            serde_json::to_vec_pretty(self).map_err(|_| invalid("invalid_structure"))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_AGENTS_BYTES {
            return Err(invalid("file_too_large"));
        }
        let _lock = lock_config(path, timeout)?;
        if &read_stamp(path)? != expected {
            return Err(AgentConfigError::Conflict);
        }
        let tmp = path.with_extension(format!(
            "json.tmp.{}.{}",
            std::process::id(),
            timestamp().as_nanos()
        ));
        let file = write_private_new(&tmp, &bytes)?;
        let publish = || -> io::Result<_> {
            let metadata = stamp_metadata(&file, &file.metadata()?)?;
            fs::rename(&tmp, path)?;
            Ok(ConfigStamp {
                file: Some(StampedFile { metadata, bytes }),
            })
        };
        let result = publish();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        Ok(result?)
    }
}

impl AgentProfile {
    pub fn validate(&self) -> Result<(), AgentConfigError> {
        if validate_agent_profile_slug(&self.id).is_err()
            || !matches!(self.id.as_bytes().first(), Some(b'a'..=b'z' | b'0'..=b'9'))
            || matches!(self.id.as_str(), "all" | "default" | "none")
        {
            return Err(invalid("invalid_agent_id"));
        }
        validate_argv(&self.argv)?;
        validate_cwd(self.default_cwd.as_deref())?;
        for (key, value) in &self.env {
            validate_env(key, Some(value))?;
        }
        if self.host_overrides.len() > 256 {
            return Err(invalid("too_many_host_overrides"));
        }
        for host in self.host_overrides.values() {
            if let Some(argv) = &host.argv {
                validate_argv(argv)?;
            }
            validate_cwd(host.cwd.as_deref())?;
            for (key, value) in &host.env {
                validate_env(key, value.as_deref())?;
            }
        }
        Ok(())
    }

    /// Merges validated profile fields without copying any of the caller's environment.
    pub fn resolve_executor(
        &self,
        host_id: Option<&str>,
        extra_args: &[String],
        cli_cwd: Option<&str>,
        is_remote: bool,
    ) -> ResolvedAgent {
        let host = host_id.and_then(|id| self.host_overrides.get(id));
        let mut argv = host
            .and_then(|h| h.argv.as_ref())
            .unwrap_or(&self.argv)
            .clone();
        argv.extend_from_slice(extra_args);
        let mut env = BTreeMap::new();
        env.extend(self.env.clone());
        if let Some(host) = host {
            for (key, value) in &host.env {
                if let Some(value) = value {
                    env.insert(key.clone(), value.clone());
                } else {
                    env.remove(key);
                }
            }
        }
        let cwd = cli_cwd
            .or_else(|| host.and_then(|h| h.cwd.as_deref()))
            .or(self.default_cwd.as_deref())
            .map(str::to_owned)
            .or_else(|| {
                if is_remote {
                    return None;
                }
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            });
        ResolvedAgent {
            executor: ExecutorSpec::Command { argv, env },
            cwd,
        }
    }
}

fn validate_argv(argv: &[String]) -> Result<(), AgentConfigError> {
    if argv.first().is_none_or(String::is_empty)
        || argv.iter().any(|arg| arg.chars().any(unsafe_text_char))
    {
        return Err(invalid("invalid_argv"));
    }
    Ok(())
}

fn validate_cwd(cwd: Option<&str>) -> Result<(), AgentConfigError> {
    if cwd.is_some_and(|cwd| cwd.contains('\0')) {
        return Err(invalid("invalid_cwd"));
    }
    Ok(())
}

fn validate_env(key: &str, value: Option<&str>) -> Result<(), AgentConfigError> {
    if !matches!(
        key.as_bytes().first(),
        Some(b'a'..=b'z' | b'A'..=b'Z' | b'_')
    ) || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(invalid("invalid_env_key"));
    }
    let upper = key.to_ascii_uppercase();
    let rule = if upper.starts_with("DDG_")
        || matches!(
            upper.as_str(),
            "LD_PRELOAD" | "LD_AUDIT" | "DYLD_INSERT_LIBRARIES" | "DYLD_FORCE_FLAT_NAMESPACE"
        ) {
        "reserved_env_key"
    } else if upper.split('_').any(|word| {
        matches!(
            word,
            "TOKEN" | "SECRET" | "PASSWORD" | "PASSWD" | "CREDENTIAL" | "CREDENTIALS"
        )
    }) || ["API_KEY", "ACCESS_KEY", "PRIVATE_KEY"]
        .iter()
        .any(|pattern| {
            upper.match_indices(pattern).any(|(start, _)| {
                let end = start + pattern.len();
                (start == 0 || upper.as_bytes()[start - 1] == b'_')
                    && (end == upper.len() || upper.as_bytes()[end] == b'_')
            })
        })
    {
        "secret_env_rejected"
    } else if value.is_some_and(|value| value.contains('\0')) {
        "invalid_env_value"
    } else {
        return Ok(());
    };
    Err(invalid(format!("{rule}: {key}")))
}

fn config_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

#[cfg(unix)]
fn private_dir(path: &Path, create: bool) -> io::Result<()> {
    let dir = config_dir(path);
    if create {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let fd = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)?;
    if fd.metadata()?.mode() & 0o7777 != 0o700 {
        fd.set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(windows)]
fn private_dir(path: &Path, create: bool) -> io::Result<()> {
    let dir = config_dir(path);
    if !create {
        dir.symlink_metadata()?;
    }
    ddg_agents_os::fs::ensure_private_dir(dir)
}

#[cfg(windows)]
fn lock_config(
    path: &Path,
    timeout: Duration,
) -> Result<ddg_agents_os::lock::FileLock, AgentConfigError> {
    private_dir(path, true)?;
    let start = Instant::now();
    loop {
        match ddg_agents_os::lock::FileLock::try_lock_exclusive(
            &config_dir(path).join("agents.lock"),
        ) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            result => return Ok(result?),
        }
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Err(AgentConfigError::LockTimeout);
        }
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

#[cfg(unix)]
fn lock_config(path: &Path, timeout: Duration) -> Result<File, AgentConfigError> {
    private_dir(path, true)?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(config_dir(path).join("agents.lock"))?;
    if !lock.metadata()?.is_file() {
        return Err(invalid("lock_not_regular_file"));
    }
    let start = Instant::now();
    loop {
        // The owned descriptor releases the lock when dropped.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(lock);
        }
        let error = io::Error::last_os_error();
        if !matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        ) {
            return Err(error.into());
        }
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Err(AgentConfigError::LockTimeout);
        }
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

#[cfg(unix)]
fn stamp_metadata(_: &File, metadata: &fs::Metadata) -> io::Result<(i128, u64, u64)> {
    Ok((
        i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec()),
        metadata.len(),
        metadata.ino(),
    ))
}

#[cfg(windows)]
fn stamp_metadata(file: &File, metadata: &fs::Metadata) -> io::Result<(i128, u64, u64)> {
    use std::os::windows::fs::MetadataExt;
    let (volume, id) = ddg_agents_os::fs::file_identity(file)?;
    Ok((
        (i128::from(volume) << 64) | i128::from(metadata.last_write_time()),
        metadata.len(),
        id,
    ))
}

fn read_stamp(path: &Path) -> Result<ConfigStamp, AgentConfigError> {
    #[cfg(unix)]
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    #[cfg(windows)]
    let opened = ddg_agents_os::fs::open_regular(path);
    let mut file = match opened {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ConfigStamp::default()),
        Err(e) => return Err(e.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid("not_regular_file"));
    }
    let metadata = stamp_metadata(&file, &metadata)?;
    let mut bytes = Vec::new();
    if metadata.1 > MAX_AGENTS_BYTES as u64 {
        return Err(invalid("file_too_large"));
    }
    (&mut file)
        .take((MAX_AGENTS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_AGENTS_BYTES {
        return Err(invalid("file_too_large"));
    }
    if metadata != stamp_metadata(&file, &file.metadata()?)? || metadata.1 != bytes.len() as u64 {
        return Err(AgentConfigError::Conflict);
    }
    Ok(ConfigStamp {
        file: Some(StampedFile { metadata, bytes }),
    })
}

fn timestamp() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

fn write_private_new(path: &Path, bytes: &[u8]) -> io::Result<File> {
    let mut file = ddg_agents_os::fs::create_private_file(path)?;
    if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(path);
        return Err(e);
    }
    Ok(file)
}

fn backup_corrupt(path: &Path, bytes: &[u8], ts: u64) -> io::Result<PathBuf> {
    for n in 0.. {
        let suffix = if n == 0 {
            format!("json.corrupt.{ts}")
        } else {
            format!("json.corrupt.{ts}.{n}")
        };
        let backup = path.with_extension(suffix);
        match write_private_new(&backup, bytes) {
            Ok(_) => return Ok(backup),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    pub(crate) struct TempDir(pub(crate) PathBuf);

    impl TempDir {
        pub(crate) fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ddg-config-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn config() -> AgentsConfig {
        serde_json::from_value(serde_json::json!({
            "schema_version": 1, "default_agent_id": "pi", "agents": [{
                "id": "pi", "label": "Pi", "argv": ["pi"], "default_cwd": "/default",
                "env": {"KEEP": "1", "DROP_ME": "x", "LD_LIBRARY_PATH": "/opt/lib"},
                "host_overrides": {"remote": {
                    "argv": ["/usr/local/bin/pi"], "cwd": "/remote",
                    "env": {"KEEP": "2", "DROP_ME": null}
                }}
            }]
        }))
        .unwrap()
    }

    #[test]
    fn validates_and_resolves_without_inheriting_parent_environment() {
        let mut config = config();
        config.validate().unwrap();
        let profile = &mut config.agents[0];
        let resolved = profile.resolve_executor(Some("remote"), &["--fast".into()], None, true);
        assert_eq!(resolved.cwd.as_deref(), Some("/remote"));
        assert_eq!(
            resolved.executor,
            ExecutorSpec::Command {
                argv: vec!["/usr/local/bin/pi".into(), "--fast".into()],
                env: BTreeMap::from([
                    ("KEEP".into(), "2".into()),
                    ("LD_LIBRARY_PATH".into(), "/opt/lib".into())
                ])
            }
        );
        let resolved = profile.resolve_executor(Some("remote"), &[], Some("/cli"), true);
        assert_eq!(resolved.cwd.as_deref(), Some("/cli"));
        assert_eq!(
            profile
                .resolve_executor(None, &[], None, true)
                .cwd
                .as_deref(),
            Some("/default")
        );
        profile.default_cwd = None;
        assert!(
            profile
                .resolve_executor(None, &[], None, true)
                .cwd
                .is_none()
        );
        assert_eq!(
            profile.resolve_executor(None, &[], None, false).cwd,
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn rejects_invalid_config_and_never_echoes_values_or_source() {
        const VALUE: &str = "super-secret-value-123";
        for (key, rule) in [
            ("DDG_FOO", "reserved_env_key"),
            ("ddg_foo", "reserved_env_key"),
            ("OPENAI_API_KEY", "secret_env_rejected"),
            ("AWS_SECRET_ACCESS_KEY", "secret_env_rejected"),
            ("GH_TOKEN", "secret_env_rejected"),
            ("LD_PRELOAD", "reserved_env_key"),
            ("LD_AUDIT", "reserved_env_key"),
            ("DYLD_INSERT_LIBRARIES", "reserved_env_key"),
            ("DYLD_FORCE_FLAT_NAMESPACE", "reserved_env_key"),
        ] {
            for is_override in [false, true] {
                let mut config = config();
                let profile = &mut config.agents[0];
                if is_override {
                    profile
                        .host_overrides
                        .get_mut("remote")
                        .unwrap()
                        .env
                        .insert(key.into(), Some(VALUE.into()));
                } else {
                    profile.env.insert(key.into(), VALUE.into());
                }
                let error = config.validate().unwrap_err().to_string();
                assert!(error.contains(key) && error.contains(rule), "{error}");
                assert!(!error.contains(VALUE));
            }
        }
        for (key, value, rule) in [
            ("9KEY", VALUE, "invalid_env_key"),
            ("KEY=super-secret-value-123", VALUE, "invalid_env_key"),
            ("KEY", "super-secret-value-123\0", "invalid_env_value"),
        ] {
            let error = validate_env(key, Some(value)).unwrap_err().to_string();
            assert!(error.contains(rule));
            assert!(!error.contains(VALUE));
        }
        let mut config = config();
        for id in [
            "",
            "-a",
            "all",
            "default",
            "none",
            "PI",
            "a/b",
            "a..b",
            &"a".repeat(65),
        ] {
            config.agents[0].id = id.into();
            assert!(config.agents[0].validate().is_err(), "{id:?}");
        }
        config.agents[0].id = "a.b".into();
        config.agents[0].validate().unwrap();
        config.agents[0].id = "pi".into();
        let mut duplicate = config.agents[0].clone();
        duplicate.id = "PI".into();
        config.agents.push(duplicate);
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("duplicate_agent_id")
        );
        config.agents.pop();
        for argv in [vec![], vec!["".into()], vec!["pi".into(), "x\0y".into()]] {
            config.agents[0].argv = argv;
            assert!(config.validate().is_err());
        }

        let tmp = TempDir::new();
        let path = tmp.0.join("agents.json");
        fs::write(
            &path,
            format!(r#"{{"schema_version":1,"{VALUE}":"{VALUE}"}}"#),
        )
        .unwrap();
        let error = AgentsConfig::load(&path).unwrap_err();
        assert!(!error.to_string().contains(VALUE));
        assert!(!format!("{error:?}").contains(VALUE));
    }

    #[test]
    fn bounds_profile_count_and_file_size() {
        let mut config = config();
        config.agents = (0..MAX_AGENT_PROFILES)
            .map(|i| AgentProfile {
                id: format!("pi-{i}"),
                ..config.agents[0].clone()
            })
            .collect();
        config.default_agent_id = None;
        config.validate().unwrap();
        config.agents.push(AgentProfile {
            id: "overflow".into(),
            ..config.agents[0].clone()
        });
        assert!(config.validate().is_err());

        let tmp = TempDir::new();
        let path = tmp.0.join("agents.json");
        let mut bytes = serde_json::to_vec(&AgentsConfig::default()).unwrap();
        bytes.resize(MAX_AGENTS_BYTES, b' ');
        fs::write(&path, &bytes).unwrap();
        let (_, stamp) = AgentsConfig::load(&path).unwrap();
        bytes.push(b' ');
        fs::write(&path, &bytes).unwrap();
        assert!(
            AgentsConfig::load(&path)
                .unwrap_err()
                .to_string()
                .contains("file_too_large")
        );
        assert!(
            AgentsConfig::default()
                .save(&path, &stamp)
                .unwrap_err()
                .to_string()
                .contains("file_too_large")
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn locks_and_saves_by_stamp_and_byte_cas() {
        let tmp = TempDir::new();
        let dir = tmp.0.join("config");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.join("agents.json");
        let (_, absent) = AgentsConfig::load(&path).unwrap();
        let lock = lock_config(&path, Duration::ZERO).unwrap();
        assert!(matches!(
            config().save_with_timeout(&path, &absent, Duration::from_millis(15)),
            Err(AgentConfigError::LockTimeout)
        ));
        drop(lock);
        let stamp = config().save(&path, &absent).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(AgentsConfig::load(&path).unwrap().0, config());
        assert!(matches!(
            config().save(&path, &absent),
            Err(AgentConfigError::Conflict)
        ));
        let original = fs::read_to_string(&path).unwrap();
        fs::write(&path, original.replace("Pi", "PX")).unwrap();
        assert!(matches!(
            config().save(&path, &stamp),
            Err(AgentConfigError::Conflict)
        ));
        // With identical metadata, CAS still compares every byte.
        let mut stale = read_stamp(&path).unwrap();
        stale.file.as_mut().unwrap().bytes = original.into_bytes();
        assert!(matches!(
            config().save(&path, &stale),
            Err(AgentConfigError::Conflict)
        ));
        let (_, fresh) = AgentsConfig::load(&path).unwrap();
        let replacement = path.with_extension("replacement");
        fs::copy(&path, &replacement).unwrap();
        fs::rename(&replacement, &path).unwrap();
        assert!(matches!(
            config().save(&path, &fresh),
            Err(AgentConfigError::Conflict)
        ));
    }

    #[test]
    fn corrupt_backup_is_private_and_does_not_follow_or_overwrite_links() {
        let tmp = TempDir::new();
        let path = tmp.0.join("agents.json");
        let target = tmp.0.join("target");
        fs::write(&target, "unchanged").unwrap();
        let temp_path = tmp.0.join("agents.json.tmp.test");
        symlink(&target, &temp_path).unwrap();
        assert!(write_private_new(&temp_path, b"not written").is_err());
        symlink(&target, &path).unwrap();
        assert!(AgentsConfig::load(&path).is_err());
        assert!(config().save(&path, &ConfigStamp::default()).is_err());
        fs::remove_file(&path).unwrap();

        fs::write(&path, b"{broken").unwrap();
        let first = backup_corrupt(&path, b"{broken", 7).unwrap();
        let second = backup_corrupt(&path, b"{also broken", 7).unwrap();
        assert_eq!(first, tmp.0.join("agents.json.corrupt.7"));
        assert_eq!(second, tmp.0.join("agents.json.corrupt.7.1"));
        assert_eq!(fs::read(&first).unwrap(), b"{broken");
        assert_eq!(
            fs::metadata(&second).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let link = tmp.0.join("agents.json.corrupt.8");
        symlink(&target, &link).unwrap();
        assert_eq!(
            backup_corrupt(&path, b"{broken", 8).unwrap(),
            tmp.0.join("agents.json.corrupt.8.1")
        );
        assert!(fs::symlink_metadata(link).unwrap().is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"unchanged");

        let Err(AgentConfigError::Corrupt { backup, stamp }) = AgentsConfig::load(&path) else {
            panic!("expected corrupt config");
        };
        assert_eq!(fs::read(backup).unwrap(), b"{broken");
        assert_eq!(fs::read(&path).unwrap(), b"{broken");
        AgentsConfig::default().save(&path, &stamp).unwrap();
        assert!(AgentsConfig::load(&path).unwrap().0.agents.is_empty());
    }
}
