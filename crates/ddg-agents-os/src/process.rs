//! Detached process launch and executable path handling.

use std::io;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
#[path = "process_windows.rs"]
mod windows;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
#[cfg(windows)]
pub use windows::*;

/// Starts a child in its own session. Stdio is left as configured by the caller.
pub fn spawn_detached(mut command: Command) -> io::Result<Child> {
    configure_detached(&mut command);
    command.spawn()
}

pub fn configure_detached(command: &mut Command) {
    #[cfg(windows)]
    command.creation_flags(DETACHED_FLAGS);
    #[cfg(unix)]
    // SAFETY: only async-signal-safe operations run between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Register shutdown handlers inside the caller's Tokio runtime.
pub fn shutdown_signal() -> io::Result<impl Future<Output = ()>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        Ok(async move {
            tokio::select! { _ = term.recv() => {}, _ = interrupt.recv() => {} }
        })
    }
    #[cfg(windows)]
    {
        let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
        let mut ctrl_break = tokio::signal::windows::ctrl_break()?;
        Ok(async move {
            tokio::select! { _ = ctrl_c.recv() => {}, _ = ctrl_break.recv() => {} }
        })
    }
}

/// Strips the suffix Linux appends to the executable path of an unlinked image.
pub fn sanitize_canonical_exe_path(path: &Path) -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        if let Some(bytes) = path.as_os_str().as_bytes().strip_suffix(b" (deleted)") {
            return PathBuf::from(OsStr::from_bytes(bytes));
        }
    }
    path.to_path_buf()
}
