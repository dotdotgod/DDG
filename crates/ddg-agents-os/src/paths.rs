//! DDG's application directories. Resolution does not create them.

use std::env;
use std::io;
use std::path::PathBuf;

#[cfg(windows)]
pub fn config_dir() -> io::Result<PathBuf> {
    windows_dir("DDG_CONFIG_DIR", "APPDATA", "ddg")
}

#[cfg(windows)]
pub fn state_dir() -> io::Result<PathBuf> {
    windows_dir("DDG_STATE_DIR", "LOCALAPPDATA", "ddg/state")
}

#[cfg(windows)]
pub fn dist_dir() -> io::Result<PathBuf> {
    windows_dir("DDG_DIST_DIR", "LOCALAPPDATA", "ddg/dist")
}

#[cfg(windows)]
pub fn bin_dir() -> io::Result<PathBuf> {
    windows_dir("DDG_BIN_DIR", "LOCALAPPDATA", "ddg/bin")
}

#[cfg(unix)]
pub fn bin_dir() -> io::Result<PathBuf> {
    home_dir("DDG_BIN_DIR", "XDG_BIN_HOME", ".local", "bin")
}

#[cfg(windows)]
pub fn run_dir() -> io::Result<PathBuf> {
    match env::var_os("DDG_RUN_DIR").filter(|value| !value.is_empty()) {
        Some(path) => Ok(path.into()),
        None => state_dir(),
    }
}

#[cfg(windows)]
fn windows_dir(ddg: &str, root: &str, suffix: &str) -> io::Result<PathBuf> {
    env::var_os(ddg)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os(root)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|p| p.join(suffix))
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("set {ddg} or {root}")))
}

#[cfg(unix)]
pub fn config_dir() -> io::Result<PathBuf> {
    home_dir("DDG_CONFIG_DIR", "XDG_CONFIG_HOME", ".config", "ddg")
}

#[cfg(unix)]
pub fn state_dir() -> io::Result<PathBuf> {
    home_dir("DDG_STATE_DIR", "XDG_STATE_HOME", ".local/state", "ddg")
}

#[cfg(unix)]
pub fn dist_dir() -> io::Result<PathBuf> {
    home_dir("DDG_DIST_DIR", "XDG_DATA_HOME", ".local/share", "ddg/dist")
}

#[cfg(unix)]
pub fn run_dir() -> io::Result<PathBuf> {
    if let Some(path) = override_dir("DDG_RUN_DIR", "XDG_RUNTIME_DIR", "ddg") {
        return Ok(path);
    }
    state_dir()
}

#[cfg(unix)]
fn home_dir(ddg: &str, xdg: &str, fallback: &str, suffix: &str) -> io::Result<PathBuf> {
    override_dir(ddg, xdg, suffix)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(fallback).join(suffix))
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("set {ddg}, {xdg}, or HOME"),
            )
        })
}

#[cfg(unix)]
fn override_dir(ddg: &str, xdg: &str, suffix: &str) -> Option<PathBuf> {
    env::var_os(ddg)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os(xdg)
                .map(PathBuf::from)
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join(suffix))
        })
}
