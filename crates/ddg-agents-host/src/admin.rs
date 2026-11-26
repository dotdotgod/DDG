//! Private operator socket. Token snapshots only travel over the spawned child's pipes.

use crate::{
    host_service::HostService,
    paths::Paths,
    term_conn::TermConn,
    token_store::{MAX_SNAPSHOT, TokenStore},
};
use ddg_agents_os::{
    ipc::{AsyncListener, AsyncStream, LocalListener, peer_credentials, peer_is_current_user},
    lock::FileLock,
};
use ddg_agents_proto::{
    rpc::{ErrorKind, Request, Response, RpcError},
    terminal::PtyListResult,
};
use serde_json::json;
use std::{
    io::{self, Read, Write},
    path::Path,
    time::Duration,
};
#[cfg(unix)]
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    process::Stdio,
};
#[cfg(unix)]
use tokio::process::Command;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::OwnedRwLockWriteGuard,
    time::{Instant, timeout},
};
use zeroize::Zeroizing;

const TIMEOUT: Duration = Duration::from_secs(2);
const PIPE_TIMEOUT: Duration = Duration::from_secs(10);
const WATCHDOG: Duration = Duration::from_secs(30);
const TOKENS_LOADED: &str = "tokens_loaded\n";
const BIND_NOW: &str = "bind_now\n";
const READY: &str = "ready\n";

pub fn path(paths: &Paths) -> std::path::PathBuf {
    paths.run_dir.join("host-admin.sock")
}

pub fn start_lock(paths: &Paths) -> std::path::PathBuf {
    paths.state_dir.join("host-start.lock")
}

pub struct Binding {
    pub public: AsyncListener,
    pub admin: AsyncListener,
    _pid: crate::server::HostPid,
    #[cfg(windows)]
    pub clients: tokio::task::JoinSet<()>,
    #[cfg(windows)]
    pub closing: tokio::sync::watch::Sender<bool>,
    _lock: FileLock,
}

impl Binding {
    pub fn bind(paths: &Paths) -> io::Result<Self> {
        let lock = FileLock::try_lock_exclusive(&paths.lock_file)?;
        Self::with_lock(paths, lock)
    }

    pub fn with_lock(paths: &Paths, lock: FileLock) -> io::Result<Self> {
        let bind = |path: &Path| {
            #[cfg(unix)]
            match ddg_agents_os::ipc::connect(path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "host socket in use",
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(path)?
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            LocalListener::bind(path)?.into_tokio()
        };
        let admin = bind(&path(paths))?;
        let pid = crate::server::HostPid::write(paths)?;
        Ok(Self {
            public: bind(&paths.host_sock)?,
            admin,
            _pid: pid,
            #[cfg(windows)]
            clients: tokio::task::JoinSet::new(),
            #[cfg(windows)]
            closing: tokio::sync::watch::channel(false).0,
            _lock: lock,
        })
    }
}

pub struct Prepared {
    _drain: OwnedRwLockWriteGuard<()>,
    pub deadline: Instant,
}

pub fn draining() -> RpcError {
    RpcError::new(ErrorKind::Unavailable, "host is draining for reload", None)
}

fn denied() -> RpcError {
    RpcError::permission_denied("admin peer is not authorized")
}

fn internal(error: impl std::fmt::Display) -> RpcError {
    RpcError::new(ErrorKind::Internal, error.to_string(), None)
}

async fn authorize(host: &HostService, peer: ddg_agents_os::ipc::PeerCred) -> Result<(), RpcError> {
    let pid = peer.pid.ok_or_else(denied)?;
    if !peer_is_current_user(&peer) || host.plugins.owns_pid(pid) {
        return Err(denied());
    }
    // The ordinary watcher is paused during swap; query the currently bound Core.
    let term = TermConn::connect_existing(&host.paths, "ddg-agents-host-admin")
        .await
        .map_err(|_| denied())?;
    let live = term.call::<PtyListResult>("pty.list", json!({})).await;
    term.close();
    if live
        .map_err(|_| denied())?
        .ptys
        .iter()
        .any(|pty| pty.pid == pid)
    {
        return Err(denied());
    }
    Ok(())
}

pub async fn handle(
    host: &HostService,
    stream: AsyncStream,
    prepared: &mut Option<Prepared>,
    binding: &mut Option<Binding>,
) -> io::Result<bool> {
    let peer = peer_credentials(&stream);
    let mut stream = Some(stream);
    let mut line = String::new();
    let mut reader = BufReader::new(stream.as_mut().unwrap()).take(8192);
    if !matches!(timeout(TIMEOUT, reader.read_line(&mut line)).await, Ok(Ok(n)) if n > 0)
        || !line.ends_with('\n')
    {
        return Ok(false);
    }
    let request: Request = match serde_json::from_str(&line) {
        Ok(req) => req,
        Err(_) => return Ok(false),
    };
    let authorized = match peer {
        Ok(peer) => timeout(TIMEOUT, authorize(host, peer))
            .await
            .unwrap_or_else(|_| Err(denied())),
        Err(_) => Err(denied()),
    };
    let mut done = false;
    let result = if let Err(err) = authorized {
        Err(err)
    } else {
        if prepared
            .as_ref()
            .is_some_and(|p| p.deadline <= Instant::now())
        {
            prepared.take();
        }
        match request.method.as_str() {
            "prepare_reload" if prepared.is_none() => {
                match timeout(TIMEOUT, host.reload_gate.clone().write_owned()).await {
                    Ok(drain) => host
                        .flush_catalog()
                        .map(|()| {
                            *prepared = Some(Prepared {
                                _drain: drain,
                                deadline: Instant::now() + WATCHDOG,
                            });
                            json!({"ok": true})
                        })
                        .map_err(internal),
                    Err(_) => Err(RpcError::new(
                        ErrorKind::Unavailable,
                        "host operations did not drain",
                        None,
                    )),
                }
            }
            "prepare_reload" => Err(draining()),
            "abort_reload" => {
                prepared.take();
                Ok(json!({"ok": true}))
            }
            "execute_reload" if prepared.is_some() => {
                let start = FileLock::try_lock_exclusive(&start_lock(&host.paths));
                let result = match &start {
                    Ok(_) => {
                        execute(
                            host,
                            request.params["bin_path"].as_str(),
                            binding,
                            #[cfg(windows)]
                            (&mut stream, &request.id),
                        )
                        .await
                    }
                    Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
                };
                if binding.is_none() && result.is_err() {
                    // Even after releasing the socket, keep the old state and restore its listeners.
                    *binding = Some(Binding::bind(&host.paths)?);
                }
                if result.is_ok() {
                    host.clear_tokens();
                    done = true;
                } else {
                    prepared.take();
                }
                result.map(|()| json!({"ok": true})).map_err(internal)
            }
            "execute_reload" => Err(RpcError::invalid_params("reload is not prepared")),
            _ => Err(RpcError::not_found("unknown admin method")),
        }
    };
    let response = match result {
        Ok(value) => Response::ok(request.id, value),
        Err(err) => Response::err(request.id, err),
    };
    // Loss of the coordinator socket cannot undo a committed handoff.
    if let Some(mut stream) = stream
        && let Ok(mut bytes) = serde_json::to_vec(&response)
    {
        bytes.push(b'\n');
        let _ = timeout(TIMEOUT, stream.write_all(&bytes)).await;
    }
    Ok(done)
}

fn executable(path: Option<&str>) -> io::Result<&Path> {
    let path = path
        .map(Path::new)
        .ok_or_else(|| io::Error::other("missing bin_path"))?;
    let metadata = path.metadata()?;
    #[cfg(unix)]
    let permitted =
        metadata.uid() == unsafe { libc::geteuid() } && metadata.permissions().mode() & 0o111 != 0;
    #[cfg(windows)]
    let permitted = ddg_agents_os::fs::is_current_user_file(path)?;
    let name = format!("ddg-agents-host{}", std::env::consts::EXE_SUFFIX);
    if path.canonicalize()? != path
        || path
            .file_name()
            .is_none_or(|filename| filename != name.as_str())
        || !metadata.is_file()
        || !permitted
    {
        return Err(io::Error::other(
            "bin_path must be a canonical, user-owned ddg-agents-host executable",
        ));
    }
    Ok(path)
}

async fn execute(
    host: &HostService,
    path: Option<&str>,
    binding: &mut Option<Binding>,
    #[cfg(windows)] response: (&mut Option<AsyncStream>, &serde_json::Value),
) -> io::Result<()> {
    let path = executable(path)?;
    let snapshot = host.token_snapshot()?;
    #[cfg(unix)]
    let (mut child, mut input, mut output) = {
        let mut cmd = Command::new(path);
        cmd.args(["serve", "--restore-tokens-pipe"])
            .env_remove("DDG_SESSION_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        ddg_agents_os::process::configure_detached(cmd.as_std_mut());
        let mut child = cmd.spawn()?;
        let input = child.stdin.take().expect("piped stdin");
        let output = child.stdout.take().expect("piped stdout");
        (child, input, output)
    };
    #[cfg(windows)]
    let (child, mut input, mut output) = {
        let (child, input, output) =
            ddg_agents_os::process::spawn_handoff(path, &["serve", "--restore-tokens-pipe"])?;
        (
            child,
            tokio::fs::File::from_std(input),
            tokio::fs::File::from_std(output),
        )
    };
    let result = timeout(PIPE_TIMEOUT, async {
        input
            .write_all(&(snapshot.len() as u32).to_be_bytes())
            .await?;
        input.write_all(&snapshot).await?;
        #[cfg(windows)]
        input.flush().await?;
        drop(snapshot);
        let mut phase = Handoff::Loading;
        phase.advance(&expect_marker(&mut output, TOKENS_LOADED).await?)?;
        #[cfg(windows)]
        {
            // FIRST_PIPE_INSTANCE requires all old instances to close. The
            // coordinator verifies the successor PID and health after this ack.
            let mut pipe = response.0.take().expect("admin connection");
            let mut ack =
                serde_json::to_vec(&Response::ok(response.1.clone(), json!({"ok": true})))?;
            ack.push(b'\n');
            pipe.write_all(&ack).await?;
            if pipe.read(&mut [0]).await? != 0 {
                return Err(io::Error::other(
                    "unexpected data after handoff acknowledgement",
                ));
            }
            drop(pipe);
            if let Some(sockets) = binding.as_mut() {
                sockets.closing.send_replace(true);
                while sockets.clients.join_next().await.is_some() {}
            }
        }
        drop(binding.take());
        input.write_all(BIND_NOW.as_bytes()).await?;
        #[cfg(windows)]
        input.flush().await?;
        phase.advance(&expect_marker(&mut output, READY).await?)?;
        Ok(())
    })
    .await
    .unwrap_or_else(|_| {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "host reload timed out",
        ))
    });
    if result.is_err() {
        #[cfg(windows)]
        if let Some(sockets) = binding.as_mut() {
            sockets.closing.send_replace(false);
        }
        drop(input);
        #[cfg(unix)]
        child.kill().await?;
        #[cfg(windows)]
        {
            let _ = child.kill();
            tokio::task::spawn_blocking(move || child.wait()).await??;
        }
    }
    result
}

#[derive(Debug, PartialEq, Eq)]
enum Handoff {
    Loading,
    Binding,
    Ready,
}

impl Handoff {
    fn advance(&mut self, marker: &[u8]) -> io::Result<()> {
        *self = match (&*self, marker) {
            (Self::Loading, bytes) if bytes == TOKENS_LOADED.as_bytes() => Self::Binding,
            (Self::Binding, bytes) if bytes == READY.as_bytes() => Self::Ready,
            _ => return Err(io::Error::other("invalid token handoff acknowledgement")),
        };
        Ok(())
    }
}

async fn expect_marker(
    pipe: &mut (impl AsyncReadExt + Unpin),
    marker: &str,
) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; marker.len()];
    pipe.read_exact(&mut bytes).await?;
    Ok(bytes)
}

pub fn read_snapshot(input: &mut impl Read) -> io::Result<TokenStore> {
    #[cfg(windows)]
    if !ddg_agents_os::process::stdio_is_pipe() {
        return Err(io::Error::other("token restore requires anonymous pipes"));
    }
    #[cfg(unix)]
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO] {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0
            || (unsafe { stat.assume_init() }.st_mode & libc::S_IFMT) != libc::S_IFIFO
        {
            return Err(io::Error::other("token restore requires anonymous pipes"));
        }
    }
    let mut size = [0; 4];
    input.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    if size > MAX_SNAPSHOT {
        return Err(io::Error::other("invalid token snapshot length"));
    }
    let mut bytes = Zeroizing::new(vec![0; size]);
    input.read_exact(&mut bytes)?;
    TokenStore::restore(&bytes)
}

pub fn await_bind(input: &mut impl Read, output: &mut impl Write) -> io::Result<()> {
    output.write_all(TOKENS_LOADED.as_bytes())?;
    output.flush()?;
    let mut marker = [0; BIND_NOW.len()];
    input.read_exact(&mut marker)?;
    if marker != BIND_NOW.as_bytes() {
        return Err(io::Error::other("invalid bind_now marker"));
    }
    Ok(())
}

pub fn ready(output: &mut impl Write) -> io::Result<()> {
    output.write_all(READY.as_bytes())?;
    output.flush()
}

pub async fn expire(prepared: Option<&Prepared>) {
    match prepared {
        Some(p) => tokio::time::sleep_until(p.deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_cannot_bind_or_commit_out_of_order() {
        let mut phase = Handoff::Loading;
        assert!(phase.advance(READY.as_bytes()).is_err());
        assert_eq!(phase, Handoff::Loading);
        phase.advance(TOKENS_LOADED.as_bytes()).unwrap();
        assert!(phase.advance(b"error\n").is_err());
        assert_eq!(phase, Handoff::Binding);
        let mut output = Vec::new();
        assert!(await_bind(&mut b"wrong___\n".as_slice(), &mut output).is_err());
        assert_eq!(output, TOKENS_LOADED.as_bytes());
        await_bind(&mut BIND_NOW.as_bytes(), &mut output).unwrap();
        phase.advance(READY.as_bytes()).unwrap();
        assert_eq!(phase, Handoff::Ready);
    }
}
