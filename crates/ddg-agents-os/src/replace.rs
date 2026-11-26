//! Same-directory executable replacement. Call under the update lock.
use std::{fs, io, path::Path};

pub fn replace_executable(staged: &Path, installed: &Path) -> io::Result<()> {
    #[cfg(windows)]
    return replace_with_tombstone(staged, installed);
    #[cfg(unix)]
    fs::rename(staged, installed)
}

#[cfg(any(windows, test))]
fn replace_with_tombstone(staged: &Path, installed: &Path) -> io::Result<()> {
    let mut nonce = [0; 16];
    ddg_agents_proto::id::fill_csprng(&mut nonce).map_err(io::Error::other)?;
    let mut name = installed
        .file_name()
        .ok_or(io::ErrorKind::InvalidInput)?
        .to_os_string();
    name.push(format!(".old.{:032x}", u128::from_be_bytes(nonce)));
    let old = installed.with_file_name(name);
    let backed_up = match move_file(installed, &old) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e),
    };
    if let Err(error) = move_file(staged, installed) {
        if backed_up && let Err(restore) = move_file(&old, installed) {
            return Err(io::Error::other(format!(
                "{error}; restoring {} failed: {restore}",
                old.display()
            )));
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(windows)]
fn move_file(src: &Path, dest: &Path) -> io::Result<()> {
    use crate::windows::{check, wide};
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
    let src = wide(src.as_os_str())?;
    let dest = wide(dest.as_os_str())?;
    check(unsafe { MoveFileExW(src.as_ptr(), dest.as_ptr(), MOVEFILE_WRITE_THROUGH) })
}

#[cfg(all(unix, test))]
use fs::rename as move_file;

/// Best effort: leave in-use tombstones for the next startup.
pub fn gc_stale_tombstones(dir: &Path) -> io::Result<usize> {
    let mut removed = 0;
    for entry in fs::read_dir(dir)?.flatten() {
        let name = entry.file_name();
        if !name
            .to_str()
            .and_then(|s| s.rsplit_once(".old."))
            .is_some_and(|(base, n)| {
                !base.is_empty() && n.len() == 32 && n.bytes().all(|b| b.is_ascii_hexdigit())
            })
        {
            continue;
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            options
                .write(true)
                // GC removes the tombstone before releasing this handle.
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let Ok(lock) = options
            .open(entry.path())
            .and_then(crate::lock::FileLock::lock)
        else {
            continue;
        };
        if fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
        drop(lock);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::FileLock;
    use std::time::{Duration, Instant};

    #[test]
    fn replace_twice_while_first_tombstone_is_locked() -> io::Result<()> {
        let dir = std::env::temp_dir()
            .join(ddg_agents_proto::id::generate_message_id().map_err(io::Error::other)?);
        crate::fs::ensure_private_dir(&dir)?;
        let result = (|| {
            let bin = dir.join("ddg.exe");
            let staged = dir.join("staged.exe");
            fs::write(&bin, b"v1")?;
            assert!(replace_with_tombstone(&staged, &bin).is_err());
            assert_eq!(fs::read(&bin)?, b"v1");
            fs::write(&staged, b"v2")?;
            replace_with_tombstone(&staged, &bin)?;
            let old = dir
                .read_dir()?
                .flatten()
                .map(|e| e.path())
                .find(|p| p != &bin)
                .unwrap();
            assert_eq!(fs::read(&old)?, b"v1");
            let lock = FileLock::try_lock_exclusive(&old)?;
            assert_eq!(
                FileLock::try_lock_exclusive(&old).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            fs::write(&staged, b"v3")?;
            replace_with_tombstone(&staged, &bin)?;
            assert_eq!(fs::read(&bin)?, b"v3");
            assert_eq!(dir.read_dir()?.count(), 3);
            assert_eq!(gc_stale_tombstones(&dir)?, 1);
            assert!(old.exists());
            drop(lock);
            // A parallel fork can retain the flock until exec.
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let removed = gc_stale_tombstones(&dir)?;
                if removed != 0 || Instant::now() >= deadline {
                    assert_eq!(removed, 1);
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(())
        })();
        fs::remove_dir_all(dir)?;
        result
    }

    #[cfg(windows)]
    #[test]
    fn replace_twice_while_windows_image_is_running() -> io::Result<()> {
        use std::{
            io::{BufRead, BufReader, Read, Write},
            process::{Child, Command, Stdio},
            time::Duration,
        };
        const CHILD: &str = "DDG_TEST_TOMBSTONE_IMAGE";
        const MARKER: &str = "DDG_IMAGE_READY";
        if std::env::var_os(CHILD).is_some() {
            let mut out = io::stdout().lock();
            writeln!(out, "{MARKER}")?;
            out.flush()?;
            drop(out);
            io::stdin().read_exact(&mut [0])?;
            return Ok(());
        }

        struct Running(Child);
        impl Drop for Running {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let dir = std::env::temp_dir()
            .join(ddg_agents_proto::id::generate_message_id().map_err(io::Error::other)?);
        crate::fs::ensure_private_dir(&dir)?;
        let result = (|| {
            let bin = dir.join("test.exe");
            let stage = dir.join("staged.exe");
            fs::copy(std::env::current_exe()?, &bin)?;
            let mut child = Running(
                Command::new(&bin)
                    .args([
                        "--exact",
                        "replace::tests::replace_twice_while_windows_image_is_running",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()?,
            );
            let stdout = child.0.stdout.take().unwrap();
            let (ready, read) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if line.contains(MARKER) {
                        let _ = ready.send(());
                    }
                }
            });
            read.recv_timeout(Duration::from_secs(5))
                .expect("child image did not start");
            for bytes in [b"v2", b"v3"] {
                fs::write(&stage, bytes)?;
                replace_executable(&stage, &bin)?;
            }
            assert_eq!(fs::read(&bin)?, b"v3");
            assert_eq!(dir.read_dir()?.count(), 3);
            assert_eq!(gc_stale_tombstones(&dir)?, 1);
            drop(child);
            assert_eq!(gc_stale_tombstones(&dir)?, 1);
            Ok(())
        })();
        fs::remove_dir_all(dir)?;
        result
    }

    #[cfg(windows)]
    #[test]
    fn replace_gc_retries_windows_sharing_violation() -> io::Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        let dir = std::env::temp_dir()
            .join(ddg_agents_proto::id::generate_message_id().map_err(io::Error::other)?);
        crate::fs::ensure_private_dir(&dir)?;
        let path = dir.join("ddg.exe.old.00000000000000000000000000000001");
        fs::write(&path, b"v1")?;
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)?;
        assert_eq!(gc_stale_tombstones(&dir)?, 0);
        drop(held);
        assert_eq!(gc_stale_tombstones(&dir)?, 1);
        fs::remove_dir(dir)
    }
}
