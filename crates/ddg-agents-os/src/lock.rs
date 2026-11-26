//! Exclusive, nonblocking file locks.

use std::fs::File;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Holds the lock until its descriptor closes. Leave the lock file in place:
/// deleting it can let another process lock a different inode at the same path.
#[derive(Debug)]
pub struct FileLock {
    _file: File,
}

impl FileLock {
    /// Returns `WouldBlock` if another open file description holds the lock.
    #[cfg(unix)]
    pub fn try_lock_exclusive(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        Self::lock(file)
    }

    #[cfg(windows)]
    pub fn try_lock_exclusive(path: &Path) -> io::Result<Self> {
        Self::lock(crate::fs::open_private(path, false)?)
    }

    #[cfg(unix)]
    pub(crate) fn lock(file: File) -> io::Result<Self> {
        // SAFETY: file owns a valid descriptor throughout the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }

    #[cfg(windows)]
    pub(crate) fn lock(file: File) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{Foundation::ERROR_LOCK_VIOLATION, Storage::FileSystem::*};
        let mut range = Default::default();
        let result = unsafe {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                u32::MAX,
                u32::MAX,
                &mut range,
            )
        };
        if result == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, error));
            }
            return Err(error);
        }
        Ok(Self { _file: file })
    }
}

#[cfg(windows)]
impl Drop for FileLock {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
        unsafe {
            UnlockFileEx(
                self._file.as_raw_handle(),
                0,
                u32::MAX,
                u32::MAX,
                &mut Default::default(),
            );
        }
    }
}
