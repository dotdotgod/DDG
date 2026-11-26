use super::{PeerCred, pipe_name};
use crate::windows::{check, is_current_owner, owner_sid, private_security};
use std::{
    io::{self, Read, Write},
    net::Shutdown,
    os::windows::io::{AsRawHandle, RawHandle},
    path::Path,
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf, ReadHalf, WriteHalf},
    net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions},
    runtime::Runtime,
    sync::{Mutex, watch},
};
use windows_sys::Win32::{
    Foundation::{ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_BUSY},
    Security::Authorization::SE_FILE_OBJECT,
    System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId},
};

fn runtime() -> io::Result<&'static Runtime> {
    static RT: OnceLock<io::Result<Runtime>> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
    })
    .as_ref()
    .map_err(|e| io::Error::other(e.to_string()))
}

fn name(path: &Path) -> io::Result<String> {
    let absolute = std::path::absolute(path)?;
    let parent = absolute
        .parent()
        .ok_or(io::ErrorKind::InvalidInput)?
        .canonicalize()?;
    pipe_name(&parent.join(absolute.file_name().ok_or(io::ErrorKind::InvalidInput)?))
}

pub struct LocalListener(AsyncListener);
impl LocalListener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        let name = name(path)?;
        let _enter = if tokio::runtime::Handle::try_current().is_err() {
            Some(runtime()?.enter())
        } else {
            None
        };
        Ok(Self(AsyncListener::bind(name)?))
    }
    pub fn into_tokio(self) -> io::Result<AsyncListener> {
        Ok(self.0)
    }
    pub fn incoming(self) -> io::Result<impl Iterator<Item = io::Result<LocalStream>>> {
        let rt = runtime()?;
        let listener = self.0;
        Ok(std::iter::from_fn(move || {
            Some(
                rt.block_on(listener.accept())
                    .map(|(stream, ())| LocalStream::new(stream)),
            )
        }))
    }
}

pub struct AsyncListener {
    name: String,
    next: Mutex<NamedPipeServer>,
}
impl AsyncListener {
    fn bind(name: String) -> io::Result<Self> {
        let next = Self::create(&name, true)?;
        Ok(Self {
            name,
            next: Mutex::new(next),
        })
    }

    fn create(name: &str, first: bool) -> io::Result<NamedPipeServer> {
        let (_descriptor, mut attrs) = private_security(false)?;
        // Tokio opens with FILE_FLAG_OVERLAPPED; the descriptor is consumed synchronously.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    (&mut attrs as *mut windows_sys::Win32::Security::SECURITY_ATTRIBUTES).cast(),
                )
        }
    }

    pub async fn accept(&self) -> io::Result<(AsyncStream, ())> {
        let mut next = self.next.lock().await;
        loop {
            match next.connect().await {
                Ok(()) => {
                    let replacement = Self::create(&self.name, false)?;
                    return Ok((
                        AsyncStream::Server(std::mem::replace(&mut *next, replacement)),
                        (),
                    ));
                }
                Err(e)
                    if e.raw_os_error().is_some_and(|code| {
                        matches!(code as u32, ERROR_NO_DATA | ERROR_BROKEN_PIPE)
                    }) =>
                {
                    // A liveness probe can disconnect before its accept is polled.
                    *next = Self::create(&self.name, false)?;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

pub enum AsyncStream {
    Server(NamedPipeServer),
    Client(NamedPipeClient),
}

macro_rules! pipe_call {
    ($stream:expr, $pipe:ident, $call:expr) => {
        match $stream {
            AsyncStream::Server($pipe) => $call,
            AsyncStream::Client($pipe) => $call,
        }
    };
}

impl AsRawHandle for AsyncStream {
    fn as_raw_handle(&self) -> RawHandle {
        pipe_call!(self, pipe, pipe.as_raw_handle())
    }
}
impl AsyncRead for AsyncStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        pipe_call!(self.get_mut(), pipe, Pin::new(pipe).poll_read(cx, buf))
    }
}
impl AsyncWrite for AsyncStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        pipe_call!(self.get_mut(), pipe, Pin::new(pipe).poll_write(cx, buf))
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        pipe_call!(self.get_mut(), pipe, Pin::new(pipe).poll_flush(cx))
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        pipe_call!(self.get_mut(), pipe, Pin::new(pipe).poll_shutdown(cx))
    }
}

fn open(name: &str) -> io::Result<AsyncStream> {
    let stream = ClientOptions::new().open(name)?;
    // Do not send anything to a pipe planted by another local user.
    if !is_current_owner(&owner_sid(stream.as_raw_handle(), SE_FILE_OBJECT)?)? {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(AsyncStream::Client(stream))
}

pub async fn connect_async(path: &Path) -> io::Result<AsyncStream> {
    let name = name(path)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match open(&name) {
            Err(e)
                if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            result => return result,
        }
    }
}

pub fn connect(path: &Path) -> io::Result<LocalStream> {
    let name = name(path)?;
    let _enter = runtime()?.enter();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match open(&name) {
            Err(e)
                if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result.map(LocalStream::new),
        }
    }
}

pub trait PipePeer {
    fn peer_pid(&self) -> io::Result<u32>;
}
impl PipePeer for AsyncStream {
    fn peer_pid(&self) -> io::Result<u32> {
        let mut pid = 0;
        let handle = self.as_raw_handle();
        check(unsafe {
            match self {
                Self::Server(_) => GetNamedPipeClientProcessId(handle, &mut pid),
                Self::Client(_) => GetNamedPipeServerProcessId(handle, &mut pid),
            }
        })?;
        Ok(pid)
    }
}

pub fn peer_credentials(stream: &impl PipePeer) -> io::Result<PeerCred> {
    let pid = stream.peer_pid()?;
    Ok(PeerCred {
        pid: (pid != 0).then_some(pid),
    })
}

#[derive(Clone)]
pub struct LocalStream {
    shared: Arc<BlockingPipe>,
    read_timeout: std::cell::Cell<Option<Duration>>,
    write_timeout: std::cell::Cell<Option<Duration>>,
}

struct BlockingPipe {
    read: Mutex<ReadHalf<AsyncStream>>,
    write: Mutex<WriteHalf<AsyncStream>>,
    closed: watch::Sender<bool>,
    peer: io::Result<u32>,
}

impl PipePeer for LocalStream {
    fn peer_pid(&self) -> io::Result<u32> {
        self.shared
            .peer
            .as_ref()
            .copied()
            .map_err(|e| io::Error::new(e.kind(), e.to_string()))
    }
}

impl LocalStream {
    fn new(stream: AsyncStream) -> Self {
        let peer = stream.peer_pid();
        let (read, write) = tokio::io::split(stream);
        Self {
            shared: Arc::new(BlockingPipe {
                read: Mutex::new(read),
                write: Mutex::new(write),
                closed: watch::channel(false).0,
                peer,
            }),
            read_timeout: None.into(),
            write_timeout: None.into(),
        }
    }
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(self.clone())
    }
    pub fn set_read_timeout(&self, value: Option<Duration>) -> io::Result<()> {
        self.read_timeout.set(value);
        Ok(())
    }
    pub fn set_write_timeout(&self, value: Option<Duration>) -> io::Result<()> {
        self.write_timeout.set(value);
        Ok(())
    }
    pub fn shutdown(&self, _: Shutdown) -> io::Result<()> {
        // Byte-mode pipes have no half-close. Wake both pending operations.
        self.shared.closed.send_replace(true);
        Ok(())
    }

    fn blocking<T>(
        &self,
        deadline: Option<Duration>,
        closed: io::Result<T>,
        operation: impl Future<Output = io::Result<T>>,
    ) -> io::Result<T> {
        let mut stop = self.shared.closed.subscribe();
        runtime()?.block_on(async {
            let operation = async {
                tokio::select! {
                    biased;
                    _ = stop.wait_for(|closed| *closed) => closed,
                    result = operation => result,
                }
            };
            if let Some(deadline) = deadline {
                tokio::time::timeout(deadline, operation)
                    .await
                    .map_err(|_| io::ErrorKind::TimedOut)?
            } else {
                operation.await
            }
        })
    }
}

impl Read for LocalStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.blocking(self.read_timeout.get(), Ok(0), async {
            self.shared.read.lock().await.read(bytes).await
        })
    }
}
impl Write for LocalStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.blocking(
            self.write_timeout.get(),
            Err(io::ErrorKind::BrokenPipe.into()),
            async { self.shared.write.lock().await.write(bytes).await },
        )
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn ipc_pipe_accepts_multiple_clients_and_reports_pid() -> io::Result<()> {
    let path = std::env::temp_dir().join(ddg_agents_proto::id::generate_message_id().unwrap());
    let listener = LocalListener::bind(&path)?.into_tokio()?;
    assert!(LocalListener::bind(&path).is_err());
    let mut clients = Vec::new();
    for byte in [1, 2] {
        let mut client = connect_async(&path).await?;
        let (mut server, ()) = listener.accept().await?;
        assert_eq!(peer_credentials(&server)?.pid, Some(std::process::id()));
        client.write_all(&[byte]).await?;
        assert_eq!(server.read_u8().await?, byte);
        clients.push((client, server));
    }
    drop(clients);
    drop(listener);
    let _rebound = LocalListener::bind(&path)?.into_tokio()?;
    Ok(())
}
