//! Runtime paths, permissions, atomic writes, and binary discovery for `ddg-agents-host`.

use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct Paths {
    pub state_dir: PathBuf,
    pub run_dir: PathBuf,
    pub plugins_dir: PathBuf,
    pub host_sock: PathBuf,
    pub terminal_sock: PathBuf,
    pub lock_file: PathBuf,
    pub catalog_file: PathBuf,
}

impl Paths {
    pub fn from_env() -> io::Result<Self> {
        #[cfg(windows)]
        let state_dir = ddg_agents_os::paths::state_dir()?;
        #[cfg(unix)]
        let state_dir = if let Some(dir) = std::env::var_os("DDG_STATE_DIR") {
            PathBuf::from(dir)
        } else if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
            PathBuf::from(xdg).join("ddg")
        } else if let Some(home) = std::env::var_os("HOME") {
            PathBuf::from(home).join(".local/state/ddg")
        } else {
            return Err(io::Error::other(
                "cannot resolve DDG state directory (set HOME or DDG_STATE_DIR)",
            ));
        };

        #[cfg(windows)]
        let run_dir = ddg_agents_os::paths::run_dir()?;
        #[cfg(unix)]
        let run_dir = if let Some(dir) = std::env::var_os("DDG_RUN_DIR") {
            PathBuf::from(dir)
        } else if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(xdg).join("ddg")
        } else {
            state_dir.clone()
        };

        #[cfg(windows)]
        let plugins_dir = std::env::var_os("DDG_PLUGINS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                ddg_agents_os::paths::config_dir()
                    .unwrap_or_else(|_| state_dir.clone())
                    .join("plugins")
            });
        #[cfg(unix)]
        let plugins_dir = if let Some(dir) = std::env::var_os("DDG_PLUGINS_DIR") {
            PathBuf::from(dir)
        } else if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            PathBuf::from(xdg).join("ddg/plugins")
        } else if let Some(home) = std::env::var_os("HOME") {
            PathBuf::from(home).join(".config/ddg/plugins")
        } else {
            state_dir.join("plugins")
        };

        Ok(Self {
            host_sock: run_dir.join("host.sock"),
            terminal_sock: run_dir.join("terminal.sock"),
            lock_file: state_dir.join("host.lock"),
            catalog_file: state_dir.join("catalog.json"),
            state_dir,
            run_dir,
            plugins_dir,
        })
    }

    pub fn pid_file(&self) -> PathBuf {
        self.run_dir.join("host.pid")
    }

    pub fn create_dirs(&self) -> io::Result<()> {
        create_private_dir(&self.state_dir)?;
        create_private_dir(&self.run_dir)?;
        Ok(())
    }
}

pub fn terminal_server_bin() -> PathBuf {
    if let Some(bin) = std::env::var_os("DDG_TERMINAL_SERVER_BIN") {
        return PathBuf::from(bin);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(format!(
            "ddg-terminal-server{}",
            std::env::consts::EXE_SUFFIX
        ));
        if sibling.is_file() {
            return sibling;
        }
    }
    #[cfg(windows)]
    if let Ok(dir) = ddg_agents_os::paths::bin_dir() {
        let candidate = dir.join("ddg-terminal-server.exe");
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("ddg-terminal-server")
}

#[cfg(unix)]
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn remove_if_exists(path: &Path) {
    let _ = fs::remove_file(path);
}

#[cfg(unix)]
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), random_hex()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    let result = file
        .set_permissions(fs::Permissions::from_mode(0o600))
        .and_then(|()| file.write_all(bytes))
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        remove_if_exists(&tmp);
    }
    result
}

pub fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(windows)]
pub use ddg_agents_os::fs::ensure_private_dir as create_private_dir;

#[cfg(windows)]
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    ddg_agents_os::fs::write_atomic(path, bytes)
}

pub fn random_hex() -> String {
    let mut buf = [0u8; 16];
    if ddg_agents_proto::id::fill_csprng(&mut buf).is_ok() {
        return buf.iter().map(|b| format!("{b:02x}")).collect();
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{:016x}{:08x}", nanos as u64, std::process::id())
}
