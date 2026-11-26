use crate::{fs, ipc, lock::FileLock, process};
use std::fs as stdfs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> io::Result<Self> {
        // Keep the path short enough for sockaddr_un on macOS.
        let mut template = *b"/tmp/ddg-os.XXXXXX\0";
        // SAFETY: mkdtemp takes a writable NUL-terminated template.
        if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(PathBuf::from(
            std::str::from_utf8(&template[..template.len() - 1]).unwrap(),
        )))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = stdfs::remove_dir_all(&self.0);
    }
}

fn mode(path: &Path) -> io::Result<u32> {
    Ok(stdfs::symlink_metadata(path)?.permissions().mode() & 0o7777)
}

#[test]
fn private_state_atomic_write_and_lock() -> io::Result<()> {
    let sandbox = Sandbox::new()?;
    let dir = sandbox.0.join("state");
    fs::ensure_private_dir(&dir)?;
    assert_eq!(mode(&dir)?, 0o700);
    stdfs::set_permissions(&dir, stdfs::Permissions::from_mode(0o755))?;
    fs::ensure_private_dir(&dir)?;
    assert_eq!(mode(&dir)?, 0o700);
    let link = sandbox.0.join("link");
    std::os::unix::fs::symlink(&dir, &link)?;
    assert!(fs::ensure_private_dir(&link).is_err());

    let file = dir.join("state.json");
    for bytes in [b"old bytes".as_slice(), b"new"] {
        fs::write_atomic(&file, bytes)?;
        assert_eq!(stdfs::read(&file)?, bytes);
        assert_eq!(mode(&file)?, 0o600);
    }
    assert!(fs::write_atomic(&dir, b"cannot replace directory").is_err());
    assert_eq!(stdfs::read_dir(&sandbox.0)?.count(), 2);

    let path = dir.join("update.lock");
    let lock = FileLock::try_lock_exclusive(&path)?;
    assert_eq!(mode(&path)?, 0o600);
    assert!(matches!(
        FileLock::try_lock_exclusive(&path),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock
    ));
    drop(lock);
    let _lock = FileLock::try_lock_exclusive(&path)?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn ipc_private_socket_credentials_concurrent_clients_and_rebind() -> io::Result<()> {
    let sandbox = Sandbox::new()?;
    let dir = sandbox.0.join("run");
    let path = dir.join("test.sock");
    let listener = ipc::LocalListener::bind(&path)?;
    assert_eq!(mode(&dir)?, 0o700);
    assert_eq!(mode(&path)?, 0o600);
    assert!(matches!(
        ipc::LocalListener::bind(&path),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse
    ));

    let peers = [ipc::connect(&path)?, ipc::connect(&path)?];
    for (mut client, byte) in peers.into_iter().zip([1, 2]) {
        client.write_all(&[byte])?;
        let (mut server, _) = listener.accept()?;
        let cred = ipc::peer_credentials(&server)?;
        // SAFETY: geteuid takes no arguments and has no preconditions.
        assert_eq!(cred.uid, unsafe { libc::geteuid() });
        assert_eq!(cred.pid, Some(std::process::id()));
        let mut received = [0];
        server.read_exact(&mut received)?;
        assert_eq!(received, [byte]);
    }
    drop(listener);
    assert!(!path.exists());
    let _listener = ipc::LocalListener::bind(&path)?;
    Ok(())
}

#[tokio::test]
async fn ipc_tokio_listener_keeps_socket_until_drop() -> io::Result<()> {
    let sandbox = Sandbox::new()?;
    let path = sandbox.0.join("async.sock");
    let listener = ipc::LocalListener::bind(&path)?.into_tokio()?;
    let _client = tokio::net::UnixStream::connect(&path).await?;
    let _server = listener.accept().await?;
    drop(listener);
    assert!(!path.exists());
    Ok(())
}

#[test]
fn process_starts_a_new_session() -> io::Result<()> {
    use std::process::{Command, Stdio};

    let mut command = Command::new("/bin/cat");
    command.stdin(Stdio::piped()).stdout(Stdio::null());
    let mut child = process::spawn_detached(command)?;
    let pid = child.id() as libc::pid_t;
    // SAFETY: getsid accepts a PID without any memory arguments.
    let sid = unsafe { libc::getsid(pid) };
    assert!(child.wait()?.success());
    assert_eq!(sid, pid);
    Ok(())
}

#[test]
fn process_sanitizes_linux_deleted_suffix_only_at_end() {
    for (input, expected) in [
        ("/bin/ddg", "/bin/ddg"),
        ("/bin/ddg (deleted)/child", "/bin/ddg (deleted)/child"),
        (
            "/bin/ddg (deleted)",
            if cfg!(target_os = "linux") {
                "/bin/ddg"
            } else {
                "/bin/ddg (deleted)"
            },
        ),
    ] {
        assert_eq!(
            process::sanitize_canonical_exe_path(Path::new(input)),
            Path::new(expected)
        );
    }
}
