//! Multi-host transport dialer:
//! 1. Local Unix socket (auto-starts local `ddg-agents-host serve` when default)
//! 2. Remote OpenSSH (`ssh -T -o BatchMode=yes -o StrictHostKeyChecking=yes ...`)
//! 3. `docker exec -i <ctr> ddg-agents-host connect` (never `-t`, optional SSH jump)
//! 4. Container internal SSHD (`ssh -T -p <port> ... <user>@<host>`, same remote lookup)

use crate::profile::{HostProfile, HostTransport};
use ddg_agents_os::ipc::{AsyncStream, connect_async, split};
use ddg_agents_proto::host::HostHelloResult;
use ddg_agents_proto::id::sanitize_display_text;
use ddg_agents_proto::rpc::{ErrorKind, Request, Response, RpcError, error_code};
use ddg_agents_proto::terminal::{HelloParams, PROTOCOL_VERSION};
use serde_json::{Value, json};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

const MAX_BANNER_LINES: usize = 64;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(12);
const REMOTE_CONNECT_COMMAND: &str = r#"sh -c 'PATH="$HOME/.local/bin:$PATH"; p=$(command -v ddg-agents-host 2>/dev/null) || p="$HOME/.local/bin/ddg-agents-host"; [ -x "$p" ] || { echo "DDG_HOST_NOT_INSTALLED" >&2; exit 127; }; exec "$p" connect'"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialPolicy {
    Standard,
    RequirePinnedHostId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialErrorKind {
    InvalidProfile,
    SecurityBlocked,
    HostKeyMismatch,
    AuthRequired,
    HostIdMismatch,
    HostNotInstalled,
    ProtocolIncompatible,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialError {
    pub kind: DialErrorKind,
    pub message: String,
    pub remote_protocol: Option<u32>,
    pub remote_version: Option<String>,
}

impl DialError {
    pub fn new(kind: DialErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            remote_protocol: None,
            remote_version: None,
        }
    }

    fn protocol_incompatible(
        message: String,
        remote_protocol: Option<u32>,
        remote_version: Option<String>,
    ) -> Self {
        Self {
            remote_protocol,
            remote_version,
            ..Self::new(DialErrorKind::ProtocolIncompatible, message)
        }
    }
}

impl fmt::Display for DialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for DialError {}

/// Convert to a wire-safe error. Keep raw transport diagnostics local.
impl From<DialError> for RpcError {
    fn from(error: DialError) -> Self {
        let message = sanitize_dial_error(&error.message);
        match error.kind {
            DialErrorKind::SecurityBlocked
            | DialErrorKind::HostKeyMismatch
            | DialErrorKind::HostIdMismatch => Self::host_security_blocked(message),
            DialErrorKind::AuthRequired => Self::host_auth_required(message),
            DialErrorKind::InvalidProfile => Self::invalid_params(message),
            DialErrorKind::ProtocolIncompatible => Self::with_code(
                ErrorKind::InvalidParams,
                error_code::PROTOCOL_INCOMPATIBLE,
                message,
                error.remote_protocol.map(|remote_protocol| {
                    let mut hint = json!({"remote_protocol": remote_protocol});
                    if let Some(version) = error.remote_version {
                        hint["remote_version"] = json!(version);
                    }
                    hint.to_string()
                }),
            ),
            DialErrorKind::Unavailable | DialErrorKind::HostNotInstalled => {
                Self::host_offline(message)
            }
        }
    }
}

/// Redacts diagnostics with path, SSH identity or user markers rather than guessing
/// where an unquoted path ends. Use the structured error code for classification.
pub fn sanitize_dial_error(err: &str) -> String {
    let clean = sanitize_display_text(err, usize::MAX);
    let lower = clean.to_ascii_lowercase();
    if err.contains(['/', '\\', '@'])
        || err
            .split_whitespace()
            .chain(clean.split_whitespace())
            .any(|word| {
                let word = word.trim_start_matches(['\'', '"', '(', '[']);
                word.starts_with("-i") || word.starts_with("-l")
            })
        || [
            "identity file",
            "identityfile",
            "private key",
            "knownhostsfile",
            "attempt key:",
            "load key",
            "local socket",
            "start local ddg-agents-host",
            "local ddg-agents-host did not listen",
            "authenticating to",
            " user ",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return "transport diagnostic redacted".into();
    }
    clean
        .trim()
        .chars()
        .filter(|ch| !matches!(ch, '\u{2028}' | '\u{2029}'))
        .take(512)
        .collect()
}

/// Identifies SSH failures that need user action instead of automatic retries.
pub fn classify_ssh_stderr(stderr: &str) -> Option<DialErrorKind> {
    let lower = stderr.to_ascii_lowercase();
    if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
        || lower.contains("host key verification failed")
        || lower.contains("possible dns spoofing detected")
        || (lower.contains("offending") && lower.contains("key in"))
    {
        return Some(DialErrorKind::HostKeyMismatch);
    }
    if lower.contains("permission denied")
        || lower.contains("too many authentication failures")
        || lower.contains("no more authentication methods")
        || lower.contains("authentication failed")
        || lower.contains("enter passphrase")
    {
        return Some(DialErrorKind::AuthRequired);
    }
    if stderr.contains("DDG_HOST_NOT_INSTALLED")
        || (lower.contains("ddg-agents-host")
            && (lower.contains("not found") || lower.contains("no such file or directory")))
    {
        return Some(DialErrorKind::HostNotInstalled);
    }
    None
}

#[derive(Default)]
pub struct TransportGuard {
    child: Option<Child>,
    stderr: Arc<Mutex<String>>,
}

impl TransportGuard {
    pub fn stderr_snapshot(&self) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

impl Drop for TransportGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}

pub struct FramedTransport {
    pub hello: HostHelloResult,
    pub reader: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    pub writer: Box<dyn AsyncWrite + Unpin + Send>,
    pub guard: TransportGuard,
}

pub fn default_local_host_sock() -> io::Result<PathBuf> {
    #[cfg(windows)]
    let run_dir = ddg_agents_os::paths::run_dir()?;
    #[cfg(unix)]
    let run_dir = if let Some(dir) = std::env::var_os("DDG_RUN_DIR") {
        PathBuf::from(dir)
    } else if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(xdg).join("ddg")
    } else if let Some(dir) = std::env::var_os("DDG_STATE_DIR") {
        PathBuf::from(dir)
    } else if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(xdg).join("ddg")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".local/state/ddg")
    } else {
        return Err(io::Error::other(
            "cannot resolve local host.sock path (set HOME or DDG_RUN_DIR)",
        ));
    };
    Ok(run_dir.join("host.sock"))
}

fn find_local_host_bin() -> PathBuf {
    if let Some(bin) = std::env::var_os("DDG_AGENTS_HOST_BIN") {
        return PathBuf::from(bin);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(format!("ddg-agents-host{}", std::env::consts::EXE_SUFFIX));
        if sibling.is_file() {
            return sibling;
        }
    }
    #[cfg(windows)]
    if let Ok(dir) = ddg_agents_os::paths::bin_dir() {
        let candidate = dir.join("ddg-agents-host.exe");
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("ddg-agents-host")
}

async fn ensure_local_host_running(sock_path: &Path) -> Result<AsyncStream, DialError> {
    if let Ok(stream) = connect_async(sock_path).await {
        return Ok(stream);
    }
    let bin = find_local_host_bin();
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    ddg_agents_os::process::spawn_detached(cmd).map_err(|e| {
        DialError::new(
            DialErrorKind::Unavailable,
            format!(
                "failed to start local ddg-agents-host ({}): {e}",
                bin.display()
            ),
        )
    })?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(stream) = connect_async(sock_path).await {
            return Ok(stream);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(DialError::new(
        DialErrorKind::Unavailable,
        format!(
            "local ddg-agents-host did not listen within 5s at {}",
            sock_path.display()
        ),
    ))
}

pub(crate) fn build_pinned_ssh_command(
    port: Option<u16>,
    known_hosts_file: &Option<String>,
    identity_file: &Option<String>,
    target: &str,
    remote_cmd: &str,
) -> Command {
    ssh_command(
        port,
        known_hosts_file,
        identity_file,
        target,
        remote_cmd,
        true,
    )
}

pub(crate) fn build_ssh_command(
    port: Option<u16>,
    known_hosts_file: &Option<String>,
    identity_file: &Option<String>,
    target: &str,
    remote_cmd: &str,
) -> Command {
    ssh_command(
        port,
        known_hosts_file,
        identity_file,
        target,
        remote_cmd,
        false,
    )
}

fn ssh_command(
    port: Option<u16>,
    known_hosts_file: &Option<String>,
    identity_file: &Option<String>,
    target: &str,
    remote_cmd: &str,
    pinned: bool,
) -> Command {
    let mut cmd = Command::new(if cfg!(windows) { "ssh.exe" } else { "ssh" });
    cmd.env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .arg("-T")
        .args(["-o", "BatchMode=yes"])
        .args(["-o", "StrictHostKeyChecking=yes"])
        .args(["-o", "ConnectTimeout=10"])
        .args(["-o", "ServerAliveInterval=15"])
        .args(["-o", "ServerAliveCountMax=4"]);
    if pinned {
        // Revalidate the host key instead of reusing a pre-existing SSH session.
        cmd.args(["-o", "ControlPath=none"]);
    }
    if let Some(p) = port {
        cmd.arg("-p").arg(p.to_string());
    }
    if let Some(kh) = known_hosts_file {
        #[cfg(unix)]
        cmd.arg("-o").arg(format!("UserKnownHostsFile={kh}"));
        #[cfg(windows)]
        cmd.arg("-o")
            .arg(format!(r#"UserKnownHostsFile="{}""#, kh.replace('\\', "/")));
    }
    if let Some(id) = identity_file {
        cmd.arg("-i").arg(id).args(["-o", "IdentitiesOnly=yes"]);
    }
    cmd.arg("--").arg(target).arg(remote_cmd);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

async fn spawn_stdio_command(
    mut cmd: Command,
) -> Result<
    (
        Box<dyn AsyncRead + Unpin + Send>,
        Box<dyn AsyncWrite + Unpin + Send>,
        TransportGuard,
    ),
    DialError,
> {
    let mut child = cmd
        .spawn()
        .map_err(|e| DialError::new(DialErrorKind::Unavailable, format!("spawn failed: {e}")))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| DialError::new(DialErrorKind::Unavailable, "missing child stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| DialError::new(DialErrorKind::Unavailable, "missing child stdout"))?;
    let stderr = child.stderr.take();
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    if let Some(err_pipe) = stderr {
        let buf = stderr_buf.clone();
        tokio::spawn(async move {
            let mut r = BufReader::new(err_pipe);
            let mut line = String::new();
            while let Ok(n) = r.read_line(&mut line).await {
                if n == 0 {
                    break;
                }
                let mut g = buf.lock().unwrap_or_else(|p| p.into_inner());
                if g.len() < 16 * 1024 {
                    g.push_str(&line);
                }
                line.clear();
            }
        });
    }
    Ok((
        Box::new(stdout),
        Box::new(stdin),
        TransportGuard {
            child: Some(child),
            stderr: stderr_buf,
        },
    ))
}

/// Opens the configured transport, runs the plain NDJSON `hello { framed: true }`
/// handshake (skipping non-JSON login banners), verifies `expected_host_id`, and
/// returns the upgraded 9-byte binary framed stream.
pub async fn dial(profile: &HostProfile, client_id: &str) -> Result<FramedTransport, DialError> {
    dial_with_policy(profile, client_id, DialPolicy::Standard).await
}

// Existing GUI callers use this name.
pub use dial as dial_and_handshake;

pub async fn dial_with_policy(
    profile: &HostProfile,
    client_id: &str,
    policy: DialPolicy,
) -> Result<FramedTransport, DialError> {
    let expected = profile.expected_host_id.as_deref();
    if policy == DialPolicy::RequirePinnedHostId && expected.is_none_or(|id| id.trim().is_empty()) {
        return Err(DialError::new(
            DialErrorKind::SecurityBlocked,
            "a pinned expected_host_id is required",
        ));
    }
    profile
        .validate()
        .map_err(|e| DialError::new(DialErrorKind::InvalidProfile, e))?;

    let (read_half, write_half, guard): (
        Box<dyn AsyncRead + Unpin + Send>,
        Box<dyn AsyncWrite + Unpin + Send>,
        TransportGuard,
    ) = match &profile.transport {
        HostTransport::Local { sock_path } => {
            let stream = match sock_path {
                Some(path) => connect_async(Path::new(path)).await.map_err(|e| {
                    DialError::new(
                        DialErrorKind::Unavailable,
                        format!("failed to connect to local socket {path}: {e}"),
                    )
                })?,
                None => {
                    let path = default_local_host_sock()
                        .map_err(|e| DialError::new(DialErrorKind::Unavailable, e.to_string()))?;
                    ensure_local_host_running(&path).await?
                }
            };
            let (r, w) = split(stream);
            (Box::new(r), Box::new(w), TransportGuard::default())
        }
        HostTransport::Ssh {
            target,
            port,
            known_hosts_file,
            identity_file,
        } => {
            let cmd = ssh_command(
                *port,
                known_hosts_file,
                identity_file,
                target,
                REMOTE_CONNECT_COMMAND,
                policy == DialPolicy::RequirePinnedHostId,
            );
            spawn_stdio_command(cmd).await?
        }
        HostTransport::DockerExec {
            container,
            ssh_host,
            known_hosts_file,
            identity_file,
        } => {
            let cmd = match ssh_host {
                None => {
                    let mut c = Command::new("docker");
                    // Strictly `-i` only, NEVER `-t`!
                    c.args(["exec", "-i", container, "ddg-agents-host", "connect"])
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .kill_on_drop(true);
                    c
                }
                Some(host) => build_ssh_command(
                    None,
                    known_hosts_file,
                    identity_file,
                    host,
                    &format!("exec docker exec -i {container} ddg-agents-host connect"),
                ),
            };
            spawn_stdio_command(cmd).await?
        }
        HostTransport::ContainerSsh {
            host,
            port,
            user,
            known_hosts_file,
            identity_file,
        } => {
            let target = format!("{user}@{host}");
            let cmd = build_ssh_command(
                Some(*port),
                known_hosts_file,
                identity_file,
                &target,
                REMOTE_CONNECT_COMMAND,
            );
            spawn_stdio_command(cmd).await?
        }
    };
    handshake(read_half, write_half, guard, client_id, expected).await
}

async fn handshake(
    read_half: Box<dyn AsyncRead + Unpin + Send>,
    mut write_half: Box<dyn AsyncWrite + Unpin + Send>,
    guard: TransportGuard,
    client_id: &str,
    expected: Option<&str>,
) -> Result<FramedTransport, DialError> {
    let req = Request::new(
        1,
        "hello",
        json!(HelloParams {
            protocol_version: PROTOCOL_VERSION,
            framed: true,
            client_id: Some(client_id.into()),
        }),
    );
    let line = format!("{}\n", serde_json::to_string(&req).expect("serializable"));
    if let Err(e) = write_half.write_all(line.as_bytes()).await {
        tokio::time::sleep(Duration::from_millis(80)).await;
        return Err(enrich_error_from_stderr(
            &guard,
            format!("write hello failed: {e}"),
        ));
    }
    let _ = write_half.flush().await;

    let mut reader = BufReader::new(read_half);
    let hello = match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_hello_reply(&mut reader)).await {
        Ok(Ok(h)) => h,
        Ok(Err(err)) if err.kind == DialErrorKind::ProtocolIncompatible => return Err(err),
        Ok(Err(err)) => {
            tokio::time::sleep(Duration::from_millis(80)).await;
            return Err(enrich_error_from_stderr(&guard, err.message));
        }
        Err(_) => {
            return Err(enrich_error_from_stderr(
                &guard,
                "handshake timed out waiting for hello reply".into(),
            ));
        }
    };

    if let Some(expected) = expected
        && expected != hello.host_id
    {
        return Err(DialError::new(
            DialErrorKind::HostIdMismatch,
            format!(
                "connected host_id {:?} does not match expected {:?}",
                hello.host_id, expected
            ),
        ));
    }

    if hello.protocol_version != PROTOCOL_VERSION {
        return Err(DialError::protocol_incompatible(
            format!(
                "protocol mismatch: local {PROTOCOL_VERSION}, remote {}",
                hello.protocol_version
            ),
            Some(hello.protocol_version),
            hello.app_version,
        ));
    }

    Ok(FramedTransport {
        hello,
        reader,
        writer: write_half,
        guard,
    })
}

async fn read_hello_reply<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<HostHelloResult, DialError> {
    let unavailable = |message| DialError::new(DialErrorKind::Unavailable, message);
    let mut buf = String::new();
    for _ in 0..MAX_BANNER_LINES {
        buf.clear();
        let n = reader
            .read_line(&mut buf)
            .await
            .map_err(|e| unavailable(format!("read hello reply failed: {e}")))?;
        if n == 0 {
            return Err(unavailable("connection closed before hello reply".into()));
        }
        let trimmed = buf.trim();
        if !trimmed.starts_with('{') {
            // Skip remote shell login banner / MOTD lines.
            continue;
        }
        let Ok(resp) = serde_json::from_str::<Response>(trimmed) else {
            continue;
        };
        if resp.id != json!(1) {
            continue;
        }
        if let Some(err) = resp.error {
            if err.data.code.as_deref() == Some(error_code::PROTOCOL_INCOMPATIBLE) {
                #[derive(Default, serde::Deserialize)]
                struct ProtocolHint {
                    remote_protocol: Option<u32>,
                    remote_version: Option<String>,
                }
                let hint = err
                    .data
                    .hint
                    .as_deref()
                    .and_then(|hint| serde_json::from_str::<ProtocolHint>(hint).ok())
                    .unwrap_or_default();
                return Err(DialError::protocol_incompatible(
                    sanitize_dial_error(&err.message),
                    hint.remote_protocol,
                    hint.remote_version,
                ));
            }
            return Err(unavailable(format!("hello rejected: {}", err.message)));
        }
        let val = resp.result.unwrap_or(Value::Null);
        return serde_json::from_value::<HostHelloResult>(val)
            .map_err(|e| unavailable(format!("invalid hello result: {e}")));
    }
    Err(unavailable(format!(
        "exceeded {MAX_BANNER_LINES} banner lines without JSON-RPC hello reply"
    )))
}

fn enrich_error_from_stderr(guard: &TransportGuard, fallback: String) -> DialError {
    let stderr = guard.stderr_snapshot();
    if let Some(kind) = classify_ssh_stderr(&stderr) {
        return DialError::new(kind, stderr.trim().to_owned());
    }
    if !stderr.trim().is_empty() {
        return DialError::new(
            DialErrorKind::Unavailable,
            format!("{fallback} ({})", stderr.trim()),
        );
    }
    DialError::new(DialErrorKind::Unavailable, fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pinned_policy_rejects_missing_ids_before_opening_transport() {
        for expected_host_id in [None, Some("".into()), Some("   ".into())] {
            let profile = HostProfile {
                expected_host_id,
                transport: HostTransport::Local {
                    sock_path: Some("/nonexistent/ddg.sock".into()),
                },
                ..HostProfile::local_default()
            };
            let err = dial_with_policy(&profile, "test", DialPolicy::RequirePinnedHostId)
                .await
                .err()
                .expect("unpinned profile must not dial");
            assert_eq!(err.kind, DialErrorKind::SecurityBlocked);
            assert_eq!(
                RpcError::from(err).data.code.as_deref(),
                Some("host_security_blocked")
            );
        }
    }

    #[tokio::test]
    async fn pinned_handshake_checks_the_peer_host_id() {
        let peer = HostHelloResult {
            protocol_version: PROTOCOL_VERSION,
            host_id: "peer".into(),
            boot_generation: 1,
            conn_id: 2,
            features: vec![],
            app_version: None,
            platform: None,
            terminal_server_version: None,
            holder_protocol_version: None,
            active_pty_count: None,
            rollback_version: None,
        };
        for expected in ["peer", "different"] {
            let (client, server) = tokio::io::duplex(4096);
            let (read, write) = tokio::io::split(client);
            let hello = peer.clone();
            let serve = tokio::spawn(async move {
                let mut server = BufReader::new(server);
                let mut line = String::new();
                server.read_line(&mut line).await.unwrap();
                assert_eq!(
                    serde_json::from_str::<Request>(&line).unwrap().method,
                    "hello"
                );
                let response = Response::ok(json!(1), json!(hello));
                server
                    .write_all(format!("{}\n", json!(response)).as_bytes())
                    .await
                    .unwrap();
            });
            let result = handshake(
                Box::new(read),
                Box::new(write),
                TransportGuard::default(),
                "test",
                Some(expected),
            )
            .await;
            serve.await.unwrap();
            if expected == "peer" {
                assert_eq!(result.ok().unwrap().hello, peer);
            } else {
                let err = result.err().unwrap();
                assert_eq!(err.kind, DialErrorKind::HostIdMismatch);
                assert_eq!(
                    RpcError::from(err).data.code.as_deref(),
                    Some("host_security_blocked")
                );
            }
        }
    }

    #[test]
    fn ssh_diagnostics_have_a_fixed_locale_and_remote_messages_are_redacted() {
        let cmd = build_ssh_command(None, &None, &None, "peer", "connect");
        let env: std::collections::HashMap<_, _> = cmd.as_std().get_envs().collect();
        for key in ["LC_ALL", "LANG"] {
            assert_eq!(env[std::ffi::OsStr::new(key)].unwrap(), "C");
        }
        for (message, sensitive) in [
            (
                "offending key in /Users/alice/keys and work/known_hosts:1",
                "alice",
            ),
            ("socket /home/bob/private.sock", "private.sock"),
            ("key ~/.ssh/my-key", "my-key"),
            ("ssh -i \"private key\" peer", "private key"),
            ("ssh -ikey.pem peer", "key.pem"),
            ("local-user@peer: denied", "local-user"),
            ("Authenticating to peer as 'local-user'", "local-user"),
            (
                "failed to connect to local socket private.sock",
                "private.sock",
            ),
        ] {
            assert!(!sanitize_dial_error(message).contains(sensitive));
            let error = RpcError::from(DialError::new(DialErrorKind::AuthRequired, message));
            assert!(!error.message.contains(sensitive));
        }
        let clean = sanitize_dial_error("connection \u{2028}refused\r\n\x1b\u{202e}");
        assert_eq!(clean, "connection refused");
        assert!(sanitize_dial_error(&"🦀".repeat(2048)).chars().count() <= 512);
    }

    #[test]
    fn classifies_ssh_host_key_mismatch_and_auth_required() {
        let mismatch = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
            @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n\
            @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
            Host key verification failed.\n";
        assert_eq!(
            classify_ssh_stderr(mismatch),
            Some(DialErrorKind::HostKeyMismatch)
        );

        let auth = "ddg@127.0.0.1: Permission denied (publickey).";
        assert_eq!(classify_ssh_stderr(auth), Some(DialErrorKind::AuthRequired));

        let net = "ssh: connect to host 10.0.0.1 port 22: Connection refused";
        assert_eq!(classify_ssh_stderr(net), None);
    }

    #[test]
    fn detects_missing_host_after_security_diagnostics() {
        for stderr in [
            "DDG_HOST_NOT_INSTALLED",
            "sh: ddg-agents-host: not found",
            "ddg-agents-host: No such file or directory",
        ] {
            assert_eq!(
                classify_ssh_stderr(stderr),
                Some(DialErrorKind::HostNotInstalled)
            );
            assert_eq!(
                classify_ssh_stderr(&format!("{stderr}\nHost key verification failed.")),
                Some(DialErrorKind::HostKeyMismatch)
            );
            assert_eq!(
                classify_ssh_stderr(&format!("{stderr}\nPermission denied (publickey).")),
                Some(DialErrorKind::AuthRequired)
            );
        }
        assert_eq!(classify_ssh_stderr("sh: docker: not found"), None);
    }

    #[test]
    fn remote_connect_searches_the_install_dir_and_reports_missing_host() {
        assert!(REMOTE_CONNECT_COMMAND.contains(r#"PATH="$HOME/.local/bin:$PATH""#));
        assert!(REMOTE_CONNECT_COMMAND.contains("DDG_HOST_NOT_INSTALLED"));
    }

    #[tokio::test]
    async fn coded_protocol_rejection_retains_optional_peer_metadata() {
        let hint = r#"{"remote_protocol":42,"remote_version":"2.0.0"}"#;
        for (input, expected) in [
            (Some(hint), Some(42)),
            (Some("invalid"), None),
            (None, None),
        ] {
            let response = Response::err(
                json!(1),
                RpcError::with_code(
                    ErrorKind::InvalidParams,
                    error_code::PROTOCOL_INCOMPATIBLE,
                    "incompatible",
                    input.map(str::to_owned),
                ),
            );
            let wire = format!("{}\n", json!(response));
            let err = read_hello_reply(&mut BufReader::new(wire.as_bytes()))
                .await
                .unwrap_err();
            assert_eq!(err.kind, DialErrorKind::ProtocolIncompatible);
            assert_eq!(err.remote_protocol, expected);
            assert_eq!(err.remote_version.as_deref(), expected.map(|_| "2.0.0"));
            let wire = RpcError::from(err);
            assert_eq!(
                wire.data.code.as_deref(),
                Some(error_code::PROTOCOL_INCOMPATIBLE)
            );
            assert_eq!(wire.data.hint.as_deref(), expected.map(|_| hint));
        }
    }

    #[tokio::test]
    async fn skips_motd_banner_lines_until_hello_reply() {
        let wire = concat!(
            "Welcome to Ubuntu 24.04 LTS!\n",
            "Last login: Mon Nov 25 00:00:00 2026\n",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocol_version\":1,\"host_id\":\"h-1\",\"boot_generation\":2,\"conn_id\":9,\"features\":[]}}\n"
        );
        let mut reader = BufReader::new(wire.as_bytes());
        let hello = read_hello_reply(&mut reader).await.unwrap();
        assert_eq!(hello.host_id, "h-1");
        assert_eq!(hello.conn_id, 9);
    }
}
