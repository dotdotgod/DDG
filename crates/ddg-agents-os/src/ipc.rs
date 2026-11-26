//! Private filesystem Unix sockets and kernel-supplied peer credentials.

#[cfg(unix)]
use crate::fs::ensure_private_dir;
#[cfg(unix)]
use std::fs;
use std::io;
#[cfg(unix)]
use std::ops::Deref;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

#[cfg(unix)]
pub type LocalStream = UnixStream;
#[cfg(unix)]
pub type AsyncStream = tokio::net::UnixStream;
#[cfg(unix)]
pub type AsyncListener = LocalListener<tokio::net::UnixListener>;

#[cfg(unix)]
pub type ReadHalf = tokio::net::unix::OwnedReadHalf;
#[cfg(unix)]
pub type WriteHalf = tokio::net::unix::OwnedWriteHalf;
#[cfg(windows)]
pub type ReadHalf = tokio::io::ReadHalf<AsyncStream>;
#[cfg(windows)]
pub type WriteHalf = tokio::io::WriteHalf<AsyncStream>;

pub fn split(stream: AsyncStream) -> (ReadHalf, WriteHalf) {
    #[cfg(unix)]
    return stream.into_split();
    #[cfg(windows)]
    tokio::io::split(stream)
}

#[cfg(unix)]
pub async fn connect_async(path: &Path) -> io::Result<AsyncStream> {
    AsyncStream::connect(path).await
}

#[cfg(unix)]
pub fn peer_is_current_user(peer: &PeerCred) -> bool {
    peer.uid == unsafe { libc::geteuid() }
}

#[cfg(windows)]
pub fn peer_is_current_user(_: &PeerCred) -> bool {
    // A bound server pipe has a protected DACL granting access solely to the current SID.
    true
}

#[cfg(any(windows, test))]
fn pipe_name(path: &Path) -> io::Result<String> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or(io::ErrorKind::InvalidInput)?;
    // FNV-1a is fixed across compiler/toolchain versions; the DACL is the access check.
    let mut hash = 0xcbf29ce484222325_u64;
    #[cfg(windows)]
    let units = std::os::windows::ffi::OsStrExt::encode_wide(path.as_os_str());
    #[cfg(not(windows))]
    let text = path.to_string_lossy();
    #[cfg(not(windows))]
    let units = text.encode_utf16();
    for byte in units.flat_map(u16::to_le_bytes) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    Ok(format!(r"\\.\pipe\ddg-{hash:016x}-{name}"))
}

#[cfg(test)]
#[test]
fn deterministic_pipe_name() {
    let a = Path::new("alpha").join("host.sock");
    let b = Path::new("beta").join("host.sock");
    let name = pipe_name(&a).unwrap();
    assert!(name.starts_with(r"\\.\pipe\ddg-"));
    assert!(name.ends_with("-host.sock"));
    assert_eq!(name, pipe_name(&a).unwrap());
    assert_ne!(name, pipe_name(&b).unwrap());
}

#[cfg(windows)]
#[path = "ipc_windows.rs"]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(unix)]
pub struct LocalListener<L = UnixListener> {
    listener: L,
    socket: SocketPath,
}

#[cfg(unix)]
impl LocalListener {
    /// Binds in a dedicated, trusted application directory. A pre-existing
    /// socket is not removed; the caller must handle stale sockets separately.
    pub fn bind(path: &Path) -> io::Result<Self> {
        if path.file_name().is_none() || path.parent().is_none_or(|p| p.as_os_str().is_empty()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket needs a parent directory and filename",
            ));
        }
        let path = std::path::absolute(path)?;
        ensure_private_dir(path.parent().unwrap())?;
        let listener = UnixListener::bind(&path)?;
        let metadata = fs::symlink_metadata(&path)?;
        let socket = SocketPath {
            path,
            identity: (metadata.dev(), metadata.ino()),
        };
        fs::set_permissions(&socket.path, fs::Permissions::from_mode(0o600))?;
        Ok(Self { listener, socket })
    }

    /// Converts a bound listener within a Tokio runtime without unbinding it.
    pub fn into_tokio(self) -> io::Result<LocalListener<tokio::net::UnixListener>> {
        self.listener.set_nonblocking(true)?;
        Ok(LocalListener {
            listener: tokio::net::UnixListener::from_std(self.listener)?,
            socket: self.socket,
        })
    }
}

#[cfg(unix)]
impl<L> Deref for LocalListener<L> {
    type Target = L;

    fn deref(&self) -> &L {
        &self.listener
    }
}

#[cfg(unix)]
struct SocketPath {
    path: PathBuf,
    identity: (u64, u64),
}

#[cfg(unix)]
impl Drop for SocketPath {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && (metadata.dev(), metadata.ino()) == self.identity
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
pub fn connect(path: &Path) -> io::Result<UnixStream> {
    UnixStream::connect(path)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerCred {
    #[cfg(unix)]
    pub uid: u32,
    pub pid: Option<u32>,
}

#[cfg(target_os = "linux")]
pub fn peer_credentials(stream: &impl AsRawFd) -> io::Result<PeerCred> {
    // SAFETY: SO_PEERCRED returns a libc::ucred.
    let cred: libc::ucred = unsafe { peer_option(stream, libc::SOL_SOCKET, libc::SO_PEERCRED)? };
    Ok(PeerCred {
        uid: cred.uid,
        pid: u32::try_from(cred.pid).ok().filter(|&pid| pid != 0),
    })
}

#[cfg(target_os = "macos")]
pub fn peer_credentials(stream: &impl AsRawFd) -> io::Result<PeerCred> {
    // SAFETY: LOCAL_PEERCRED returns a libc::xucred and LOCAL_PEERPID a libc::pid_t.
    let (cred, pid): (libc::xucred, libc::pid_t) = unsafe {
        (
            peer_option(stream, libc::SOL_LOCAL, libc::LOCAL_PEERCRED)?,
            peer_option(stream, libc::SOL_LOCAL, libc::LOCAL_PEERPID)?,
        )
    };
    if cred.cr_version != libc::XUCRED_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected xucred version",
        ));
    }
    Ok(PeerCred {
        uid: cred.cr_uid,
        pid: u32::try_from(pid).ok().filter(|&pid| pid != 0),
    })
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub fn peer_credentials(_: &impl AsRawFd) -> io::Result<PeerCred> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
unsafe fn peer_option<T>(
    stream: &impl AsRawFd,
    level: libc::c_int,
    option: libc::c_int,
) -> io::Result<T> {
    use std::mem::{MaybeUninit, size_of};

    let mut value = MaybeUninit::<T>::uninit();
    let mut len = size_of::<T>() as libc::socklen_t;
    // SAFETY: the descriptor is live and the output buffer has len bytes.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            level,
            option,
            value.as_mut_ptr().cast(),
            &mut len,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if len as usize != size_of::<T>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected peer option size",
        ));
    }
    // SAFETY: the caller supplied the kernel's output type for this option.
    Ok(unsafe { value.assume_init() })
}
