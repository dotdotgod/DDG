//! Runtime paths, 0700/0600 permissions, file locking, and atomic persistence.

use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct Paths {
    pub state_dir: PathBuf,
    pub run_dir: PathBuf,
    pub terminal_sock: PathBuf,
    pub holder_sock: PathBuf,
    pub lock_file: PathBuf,
    pub server_json: PathBuf,
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

        Ok(Self {
            terminal_sock: run_dir.join("terminal.sock"),
            holder_sock: run_dir.join("holder.sock"),
            lock_file: state_dir.join("terminal.lock"),
            server_json: state_dir.join("server.json"),
            state_dir,
            run_dir,
        })
    }

    pub fn create_dirs(&self) -> io::Result<()> {
        create_private_dir(&self.state_dir)?;
        create_private_dir(&self.run_dir)?;
        Ok(())
    }
}

#[cfg(unix)]
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
pub fn set_socket_permissions(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
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
    let mut file = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    Ok(())
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
