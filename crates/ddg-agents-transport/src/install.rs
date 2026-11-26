//! Explicit SSH bootstrap and pinned upgrades for DDG hosts.

use crate::AGENT_ACTIVITY_V1;
use crate::dial::{
    DialError, DialErrorKind, DialPolicy, build_ssh_command, classify_ssh_stderr,
    dial_and_handshake, dial_with_policy,
};
use crate::profile::{HostProfile, HostTransport};
use ddg_agents_os::fs::ensure_private_dir;
use ddg_agents_update::integrity::verify_sha256;
use sha2::{Digest, Sha256};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::fs;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;

pub(crate) const HOST_BIN: &str = "ddg-agents-host";
pub(crate) const TERMINAL_BIN: &str = "ddg-terminal-server";
pub(crate) const PROBE_COMMAND: &str =
    r#"sh -c 'printf "DDG_UNAME:%s:%s\n" "$(uname -s)" "$(uname -m)"'"#;

// POSIX sh; do not take a PID from shell expansions or signal the PTY holder.
const RESTART_SERVICES: &str = r#"
set -eu
export LC_ALL=C
if [ "${DDG_RUN_DIR+x}" = x ]; then
    run_dir=$DDG_RUN_DIR
elif [ "${XDG_RUNTIME_DIR+x}" = x ]; then
    run_dir=$XDG_RUNTIME_DIR/ddg
else
    run_dir=${DDG_STATE_DIR-${XDG_STATE_HOME-$HOME/.local/state}/ddg}
fi
case "$run_dir" in /*) ;; *) run_dir=./$run_dir ;; esac
uid=$(id -u)
owner=$(id -un)

list_args() {
    if listing=$(ps -ww -u "$uid" -o pid=,args= 2>/dev/null) || listing=$(ps -u "$uid" -o pid=,args= 2>/dev/null); then
        printf '%s\n' "$listing"
        return
    fi
    # BusyBox ps may not implement -u or keyword= headings.
    if ! listing=$(ps -ww -o pid,user,args 2>/dev/null); then
        listing=$(ps -o pid,user,args) || return 1
    fi
    printf '%s\n' "$listing" | awk -v owner="$owner" '$2 == owner { sub(/^[[:space:]]*/, ""); sub(/[[:space:]]+[^[:space:]]+/, ""); print }'
}

service_pids() {
    listing=$(list_args) || return 1
    printf '%s\n' "$listing" | awk -v self="$$" -v bin="$1" -v mode="$2" '
        $1 ~ /^[1-9][0-9]*$/ && $1+0 > 1 && $1+0 <= 2147483647 && $1 != self {
            i = ($2 == "{" bin "}" ? 3 : 2)
            exe = $i
            sub(/^.*\//, "", exe)
            if (NF == i+1 && exe == bin && $(i+1) == mode) print $1
        }'
}

check_comm() {
    if ! comm=$(ps -p "$1" -o comm= 2>/dev/null); then
        listing=$(ps -o pid,user,comm) || return 1
        comm=$(printf '%s\n' "$listing" | awk -v pid="$1" -v owner="$owner" '$1 == pid && $2 == owner { print $3 }')
    fi
    comm=$(printf '%s\n' "$comm" | awk 'NR != 1 || NF != 1 { exit 1 } { print $1 }') || return 1
    [ "${comm##*/}" = "$2" ] || { [ "$2" = ddg-terminal-server ] && [ "$comm" = ddg-terminal-se ]; }
}

find_service() {
    f=$run_dir/$1.pid
    candidates=$(service_pids "$2" "$3") || return 1
    if [ -e "$f" ] || [ -L "$f" ]; then
        [ ! -L "$f" ] && [ -f "$f" ] || return 1
        # Some awk versions truncate NUL in text records. Read raw bytes instead.
        bytes=$(od -An -v -tu1 -N 12 "$f") || return 1
        pid=$(printf '%s\n' "$bytes" | awk '
            { for (i=1; i<=NF; ++i) {
                if (eol || length(pid)>10) exit 1
                if ($i == 10) eol = 1
                else if ($i >= 48 && $i <= 57) pid = pid sprintf("%c", $i)
                else exit 1
            } }
            END { if (pid !~ /^[1-9][0-9]*$/ || pid+0 <= 1 || pid+0 > 2147483647) exit 1; print pid }') || return 1
        printf '%s\n' "$candidates" | grep -Fxq "$pid" || return 1
    else
        # Legacy hosts have no PID file. Do not guess when several instances match.
        case "$candidates" in
            '') return 0 ;;
            *'
'*) return 1 ;;
            *) pid=$candidates ;;
        esac
    fi
    [ "$pid" != "$$" ] && check_comm "$pid" "$2" || return 1
    printf '%s\n' "$pid"
}

stop_service() {
    pid=$1
    if [ -n "$pid" ]; then
        candidates=$(service_pids "$2" "$3") || return 1
        printf '%s\n' "$candidates" | grep -Fxq "$pid" && check_comm "$pid" "$2" || return 1
        kill -TERM "$pid" || return 1
    fi
    attempts=0
    while :; do
        candidates=$(service_pids "$2" "$3") || return 1
        if [ ! -S "$run_dir/$4" ] && ! printf '%s\n' "$candidates" | grep -Fxq "${pid:-0}"; then
            return 0
        fi
        attempts=$((attempts + 1))
        [ "$attempts" -lt 50 ] || return 1
        sleep 0.1
    done
}

host=$(find_service host ddg-agents-host serve) || { echo 'cannot verify host PID' >&2; exit 1; }
core=$(find_service core ddg-terminal-server core) || { echo 'cannot verify core PID' >&2; exit 1; }
stop_service "$host" ddg-agents-host serve host.sock || { echo 'host did not stop safely' >&2; exit 1; }
stop_service "$core" ddg-terminal-server core terminal.sock || { echo 'core did not stop safely' >&2; exit 1; }
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemotePlatform {
    DarwinArm64,
    DarwinX86_64,
    LinuxX86_64,
    LinuxArm64,
}

impl RemotePlatform {
    pub fn slug(&self) -> &'static str {
        match self {
            Self::DarwinArm64 => "darwin-arm64",
            Self::DarwinX86_64 => "darwin-x86_64",
            Self::LinuxX86_64 => "linux-x86_64",
            Self::LinuxArm64 => "linux-aarch64",
        }
    }

    pub fn docker_platform(&self) -> Option<&'static str> {
        match self {
            Self::LinuxX86_64 => Some("linux/amd64"),
            Self::LinuxArm64 => Some("linux/arm64"),
            Self::DarwinArm64 | Self::DarwinX86_64 => None,
        }
    }

    pub fn current() -> Option<Self> {
        match std::env::consts::OS {
            "macos" => platform_for("Darwin", std::env::consts::ARCH),
            "linux" => platform_for("Linux", std::env::consts::ARCH),
            _ => None,
        }
    }
}

fn platform_for(sysname: &str, machine: &str) -> Option<RemotePlatform> {
    match (sysname, machine) {
        ("Darwin", "arm64" | "aarch64") => Some(RemotePlatform::DarwinArm64),
        ("Darwin", "x86_64") => Some(RemotePlatform::DarwinX86_64),
        ("Linux", "x86_64" | "amd64") => Some(RemotePlatform::LinuxX86_64),
        ("Linux", "aarch64" | "arm64") => Some(RemotePlatform::LinuxArm64),
        _ => None,
    }
}

pub fn parse_uname_output(output: &str) -> Result<RemotePlatform, String> {
    let parts = match output
        .lines()
        .find_map(|line| line.trim().strip_prefix("DDG_UNAME:"))
    {
        Some(line) => line.split(':').collect::<Vec<_>>(),
        None => output.split_whitespace().collect(),
    };
    if let [sysname, machine] = parts.as_slice() {
        platform_for(sysname.trim(), machine.trim())
            .ok_or_else(|| format!("unsupported remote OS or architecture: {sysname} {machine}"))
    } else {
        Err("could not read remote OS and architecture from uname".into())
    }
}

#[derive(Debug)]
pub struct RemoteInstallInfo {
    pub host_id: String,
    pub platform: RemotePlatform,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ResolvedBinaries {
    pub host_bin: PathBuf,
    pub terminal_server_bin: PathBuf,
}

impl ResolvedBinaries {
    fn if_present(host_bin: PathBuf, terminal_server_bin: PathBuf) -> Option<Self> {
        (host_bin.is_file() && terminal_server_bin.is_file()).then_some(Self {
            host_bin,
            terminal_server_bin,
        })
    }

    fn in_dir(dir: &Path) -> Option<Self> {
        Self::if_present(dir.join(HOST_BIN), dir.join(TERMINAL_BIN))
    }
}

pub fn default_dist_dir() -> Option<PathBuf> {
    ddg_agents_os::paths::dist_dir().ok()
}

pub fn find_workspace_root() -> Option<PathBuf> {
    let is_workspace =
        |dir: &Path| dir.join("docker/Dockerfile").is_file() && dir.join("Cargo.toml").is_file();
    std::env::var_os("DDG_WORKSPACE_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .filter(|dir| is_workspace(dir))
        .or_else(|| std::env::current_dir().ok().filter(|dir| is_workspace(dir)))
        .or_else(|| {
            let exe = std::env::current_exe().ok()?;
            exe.parent()?
                .ancestors()
                .find(|dir| is_workspace(dir))
                .map(Path::to_path_buf)
        })
}

pub fn resolve_cached_or_local_binaries_in(
    platform: RemotePlatform,
    dist_dir: Option<&Path>,
    workspace_root: Option<&Path>,
) -> Option<ResolvedBinaries> {
    if Some(platform) == RemotePlatform::current() {
        let overrides = || {
            ResolvedBinaries::if_present(
                std::env::var_os("DDG_AGENTS_HOST_BIN")?.into(),
                std::env::var_os("DDG_TERMINAL_SERVER_BIN")?.into(),
            )
        };
        let siblings = || ResolvedBinaries::in_dir(std::env::current_exe().ok()?.parent()?);
        if let Some(bins) = overrides().or_else(siblings) {
            return Some(bins);
        }
        for dir in ["target/release", "target/debug"] {
            if let Some(bins) =
                workspace_root.and_then(|root| ResolvedBinaries::in_dir(&root.join(dir)))
            {
                return Some(bins);
            }
        }
    }
    ResolvedBinaries::in_dir(&dist_dir?.join(platform.slug()))
}

pub fn resolve_cached_or_local_binaries(platform: RemotePlatform) -> Option<ResolvedBinaries> {
    resolve_cached_or_local_binaries_in(
        platform,
        default_dist_dir().as_deref(),
        find_workspace_root().as_deref(),
    )
}

fn unavailable(message: impl Into<String>) -> DialError {
    DialError::new(DialErrorKind::Unavailable, message)
}

fn progress_stage(progress: Option<&mpsc::UnboundedSender<String>>, stage: impl Into<String>) {
    if let Some(tx) = progress {
        let _ = tx.send(stage.into());
    }
}

fn stderr_tail(stderr: &[u8]) -> String {
    let stderr = stderr.trim_ascii_end();
    String::from_utf8_lossy(&stderr[stderr.len().saturating_sub(1024)..])
        .trim()
        .to_owned()
}

/// Explicit bootstrap uses local binaries or a BuildKit export, not the automatic update checker.
pub async fn resolve_or_build_binaries(
    platform: RemotePlatform,
    progress: Option<&mpsc::UnboundedSender<String>>,
) -> Result<ResolvedBinaries, DialError> {
    if let Some(bins) = resolve_cached_or_local_binaries(platform) {
        return Ok(bins);
    }
    let dist_dir = default_dist_dir()
        .ok_or_else(|| unavailable("cannot locate binary cache: set DDG_DIST_DIR or HOME"))?;
    let destination = dist_dir.join(platform.slug());
    let Some(docker_platform) = platform.docker_platform() else {
        return Err(unavailable(format!(
            "place prebuilt {HOST_BIN} and {TERMINAL_BIN} in {}",
            destination.display()
        )));
    };
    let workspace = find_workspace_root().ok_or_else(|| unavailable(format!(
        "cannot find workspace for Docker BuildKit: set DDG_WORKSPACE_DIR or place prebuilt binaries in {}",
        destination.display()
    )))?;

    progress_stage(
        progress,
        format!(
            "{} 원격 바이너리 빌드 중 (최초 1회, Docker BuildKit)...",
            platform.slug()
        ),
    );
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let staging = dist_dir.join(format!(
        ".staging-{}-{}-{nanos}",
        platform.slug(),
        std::process::id()
    ));
    ensure_private_dir(&dist_dir).map_err(|e| unavailable(format!("secure binary cache: {e}")))?;
    ensure_private_dir(&staging)
        .map_err(|e| unavailable(format!("secure binary build staging directory: {e}")))?;
    let result = async {
        let dest_field = format!("dest={}", staging.display());
        let dest_field = if dest_field.contains([',', '"', '\r', '\n']) {
            format!("\"{}\"", dest_field.replace('"', "\"\""))
        } else {
            dest_field
        };
        let output = Command::new("docker")
            .args([
                "buildx",
                "build",
                "--platform",
                docker_platform,
                "--target",
                "binaries",
                "--output",
            ])
            .arg(format!("type=local,{dest_field}"))
            .arg("-f")
            .arg(workspace.join("docker/Dockerfile"))
            .arg(&workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| unavailable(format!("run docker buildx: {e}")))?;
        if !output.status.success() {
            return Err(unavailable(format!(
                "docker buildx failed ({}): {}",
                output.status,
                stderr_tail(&output.stderr)
            )));
        }
        publish_binaries(&staging, &destination)
            .await
            .map_err(|e| unavailable(format!("cache built binaries: {e}")))
    }
    .await;
    let _ = fs::remove_dir_all(&staging).await;
    result
}

async fn publish_binaries(staging: &Path, destination: &Path) -> io::Result<ResolvedBinaries> {
    let bins = ResolvedBinaries::in_dir(staging)
        .ok_or_else(|| io::Error::other("Docker export is missing one or both DDG binaries"))?;
    #[cfg(unix)]
    for bin in [&bins.terminal_server_bin, &bins.host_bin] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(bin, std::fs::Permissions::from_mode(0o755)).await?;
    }

    for (name, path) in [
        (HOST_BIN, &bins.host_bin),
        (TERMINAL_BIN, &bins.terminal_server_bin),
    ] {
        let digest = sha256_hex(&fs::read(path).await?);
        ddg_agents_os::fs::write_atomic(
            &staging.join(format!("{name}.sha256")),
            digest.as_bytes(),
        )?;
    }

    if let Err(err) = fs::rename(staging, destination).await {
        if let Some(cached) = ResolvedBinaries::in_dir(destination) {
            return Ok(cached);
        }
        if !destination.is_dir() {
            return Err(err);
        }
        // Preserve unrelated entries in a partial cache.
        ensure_private_dir(destination)?;
        for name in [HOST_BIN, TERMINAL_BIN] {
            fs::rename(staging.join(name), destination.join(name)).await?;
            let checksum = format!("{name}.sha256");
            fs::rename(staging.join(&checksum), destination.join(&checksum)).await?;
        }
    }
    ResolvedBinaries::in_dir(destination)
        .ok_or_else(|| io::Error::other("binary cache is incomplete"))
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn verified_install_payload(
    path: &Path,
    cache: Option<&Path>,
    name: &str,
) -> Result<(Vec<u8>, String), DialError> {
    let bytes = fs::read(path)
        .await
        .map_err(|e| unavailable(format!("read {name}: {e}")))?;
    if bytes.is_empty() {
        return Err(unavailable(format!("{name} is empty")));
    }
    let digest = if let Some(cache) = cache {
        // BuildKit writes these at export; manually supplied cache entries need an independent digest.
        let digest = fs::read_to_string(cache.join(format!("{name}.sha256")))
            .await
            .map_err(|e| unavailable(format!("read cached {name} SHA-256: {e}")))?;
        verify_sha256(bytes.as_slice(), digest.trim()).map_err(|e| {
            DialError::new(
                DialErrorKind::SecurityBlocked,
                format!("cached {name}: {e}"),
            )
        })?;
        digest.trim().to_ascii_lowercase()
    } else {
        // Workspace and explicit development binaries are locally trusted sources.
        sha256_hex(&bytes)
    };
    Ok((bytes, digest))
}

pub fn build_atomic_upload_script(bin_name: &str) -> Result<String, String> {
    atomic_upload_script(bin_name, None)
}

// Hash stdin so the tool's output is independent of the uploaded filename.
// A present but failing or malformed tool must not be masked by fallback.
pub(crate) const REMOTE_SHA256: &str = r#"
verify_sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        line=$(sha256sum < "$1") || return 1
        style=sum
    elif command -v shasum >/dev/null 2>&1; then
        line=$(shasum -a 256 < "$1") || return 1
        style=sum
    elif command -v openssl >/dev/null 2>&1; then
        line=$(openssl dgst -sha256 < "$1") || return 1
        style=openssl
    else
        return 1
    fi
    hash=$(printf '%s\n' "$line" | awk -v style="$style" '
        NR == 1 && NF == 2 && style == "sum" && $2 == "-" {hash=$1}
        NR == 1 && NF == 2 && style == "openssl" && ($1 == "SHA256(stdin)=" || $1 == "SHA2-256(stdin)=" || $1 == "(stdin)=") {hash=$2}
        END {if (NR != 1 || length(hash) != 64 || hash ~ /[^0-9a-fA-F]/) exit 1; print tolower(hash)}
    ') || return 1
    [ "$hash" = "$2" ]
}
"#;

pub(crate) fn sh_command(script: &str) -> String {
    format!("sh -c '{}'", script.replace('\'', "'\\''"))
}

fn atomic_upload_script_with_hash(
    bin_name: &str,
    len: usize,
    sha256: &str,
) -> Result<String, String> {
    upload_script(bin_name, Some(len), Some(sha256))
}

fn atomic_upload_script(bin_name: &str, expected_len: Option<usize>) -> Result<String, String> {
    upload_script(bin_name, expected_len, None)
}

fn upload_script(
    bin_name: &str,
    expected_len: Option<usize>,
    sha256: Option<&str>,
) -> Result<String, String> {
    if bin_name.is_empty()
        || !bin_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err("invalid binary name".into());
    }
    // An interrupted SSH stream can look like EOF remotely; never publish a short upload.
    let check = expected_len.map_or_else(String::new, |len| {
        format!(
            r#"[ "$(wc -c < "$tmp")" -eq {len} ] && [ ! -d "$HOME/.local/bin/{bin_name}" ] && "#
        )
    });
    let hash_check = sha256.map_or_else(String::new, |sha| {
        format!(r#"verify_sha256 "$tmp" '{sha}' && "#)
    });
    let hash_fn = if sha256.is_some() { REMOTE_SHA256 } else { "" };
    Ok(sh_command(&format!(
        r#"{hash_fn}
umask 077 && mkdir -p "$HOME/.local/bin" && tmp="$HOME/.local/bin/.{bin_name}.tmp.$$" && trap 'rm -f "$tmp"' EXIT && trap 'exit 1' INT TERM && (set -C; cat > "$tmp") && {check}{hash_check}chmod 755 "$tmp" && mv -f "$tmp" "$HOME/.local/bin/{bin_name}" && trap - EXIT INT TERM"#
    )))
}

async fn read_ssh_output(
    pipe: Option<impl AsyncRead + Unpin>,
    buf: &mut Vec<u8>,
) -> io::Result<()> {
    if let Some(pipe) = pipe {
        const LIMIT: usize = 64 * 1024;
        pipe.take((LIMIT + 1) as u64).read_to_end(buf).await?;
        if buf.len() > LIMIT {
            return Err(io::Error::other("SSH output exceeds 64 KiB"));
        }
    }
    Ok(())
}

pub(crate) struct SshFailure {
    pub error: DialError,
    pub stderr: Vec<u8>,
}

pub(crate) async fn run_ssh(
    cmd: Command,
    payload: Option<&[u8]>,
    timeout: Duration,
    step: &str,
) -> Result<Vec<u8>, DialError> {
    run_ssh_with_stderr(cmd, payload, timeout, step)
        .await
        .map_err(|failure| failure.error)
}

pub(crate) async fn run_ssh_with_stderr(
    mut cmd: Command,
    payload: Option<&[u8]>,
    timeout: Duration,
    step: &str,
) -> Result<Vec<u8>, SshFailure> {
    if payload.is_none() {
        cmd.stdin(Stdio::null());
    } else {
        cmd.stdout(Stdio::null());
    }
    let mut child = cmd.spawn().map_err(|e| SshFailure {
        error: unavailable(format!("{step}: {e}")),
        stderr: Vec::new(),
    })?;
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let send = async move {
        if let Some(payload) = payload {
            let mut stdin = stdin.ok_or_else(|| io::Error::other("missing SSH stdin"))?;
            stdin.write_all(payload).await?;
            stdin.shutdown().await?;
        }
        Ok::<_, io::Error>(())
    };
    let outcome = tokio::time::timeout(timeout, async {
        let (status, stdout, stderr, sent) = tokio::join!(
            child.wait(),
            read_ssh_output(stdout, &mut out),
            read_ssh_output(stderr, &mut err),
            send,
        );
        let status = status.map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(status.to_string());
        }
        stdout.and(stderr).and(sent).map_err(|e| e.to_string())
    })
    .await;
    let reason = match outcome {
        Ok(Ok(())) => return Ok(out),
        Ok(Err(reason)) => reason,
        Err(_) => format!("timed out after {}s", timeout.as_secs()),
    };
    let kind =
        classify_ssh_stderr(&String::from_utf8_lossy(&err)).unwrap_or(DialErrorKind::Unavailable);
    let tail = stderr_tail(&err);
    let mut message = format!("{step}: {reason}");
    if !tail.is_empty() {
        message.push_str(": ");
        message.push_str(&tail);
    }
    Err(SshFailure {
        error: DialError::new(kind, message),
        stderr: err,
    })
}

/// Returns whether this is an upgrade. Only an unpinned, missing host can bootstrap.
async fn verify_install_target(profile: &HostProfile) -> Result<bool, DialError> {
    if profile.expected_host_id.is_some() {
        dial_with_policy(
            profile,
            "ddg-install-preflight",
            DialPolicy::RequirePinnedHostId,
        )
        .await?;
        return Ok(true);
    }
    match dial_and_handshake(profile, "ddg-install-preflight").await {
        Err(error) if error.kind == DialErrorKind::HostNotInstalled => Ok(false),
        Err(error) => Err(error),
        Ok(_) => Err(DialError::new(
            DialErrorKind::SecurityBlocked,
            "a pinned expected_host_id is required to upgrade an existing host",
        )),
    }
}

async fn verify_installed_host(profile: &HostProfile) -> Result<String, DialError> {
    let transport = dial_and_handshake(profile, "ddg-native-install-verify").await?;
    if transport
        .hello
        .features
        .iter()
        .any(|feature| feature == AGENT_ACTIVITY_V1)
    {
        Ok(transport.hello.host_id)
    } else {
        Err(unavailable(
            "installed host does not advertise agent_activity_v1",
        ))
    }
}

pub async fn install_remote_host(
    profile: &HostProfile,
    progress: Option<mpsc::UnboundedSender<String>>,
) -> Result<RemoteInstallInfo, DialError> {
    profile
        .validate()
        .map_err(|e| DialError::new(DialErrorKind::InvalidProfile, e))?;
    let (target, port, known_hosts, identity) = match &profile.transport {
        HostTransport::Ssh {
            target,
            port,
            known_hosts_file,
            identity_file,
        } => (target.clone(), *port, known_hosts_file, identity_file),
        HostTransport::ContainerSsh {
            host,
            port,
            user,
            known_hosts_file,
            identity_file,
        } => (
            format!("{user}@{host}"),
            Some(*port),
            known_hosts_file,
            identity_file,
        ),
        _ => {
            return Err(DialError::new(
                DialErrorKind::InvalidProfile,
                "remote installation requires an SSH transport",
            ));
        }
    };
    let ssh = |command: &str| build_ssh_command(port, known_hosts, identity, &target, command);
    let progress = progress.as_ref();
    progress_stage(progress, "원격 호스트 신원 확인 중...");
    let upgrade = verify_install_target(profile).await?;
    progress_stage(progress, "원격 플랫폼 확인 중...");
    let output = run_ssh(
        ssh(PROBE_COMMAND),
        None,
        Duration::from_secs(15),
        "probe remote platform",
    )
    .await?;
    let platform = parse_uname_output(&String::from_utf8_lossy(&output)).map_err(unavailable)?;
    let bins = resolve_or_build_binaries(platform, progress).await?;
    let cache = default_dist_dir().filter(|root| bins.host_bin.starts_with(root));
    let cache_dir = if let Some(root) = &cache {
        let dir = root.join(platform.slug());
        ensure_private_dir(root)
            .and_then(|()| ensure_private_dir(&dir))
            .map_err(|e| {
                DialError::new(
                    DialErrorKind::SecurityBlocked,
                    format!("secure binary cache: {e}"),
                )
            })?;
        Some(dir)
    } else {
        None
    };
    // Verify both artifacts before changing either remote binary.
    let mut payloads = Vec::new();
    for (name, path) in [
        (TERMINAL_BIN, &bins.terminal_server_bin),
        (HOST_BIN, &bins.host_bin),
    ] {
        payloads.push((
            name,
            verified_install_payload(path, cache_dir.as_deref(), name).await?,
        ));
    }
    for (name, (bytes, digest)) in payloads {
        progress_stage(progress, format!("{name} 업로드 중..."));
        let script =
            atomic_upload_script_with_hash(name, bytes.len(), &digest).map_err(unavailable)?;
        run_ssh(
            ssh(&script),
            Some(&bytes),
            Duration::from_secs(120),
            &format!("upload {name}"),
        )
        .await?;
    }
    if upgrade {
        progress_stage(progress, "원격 호스트와 코어 재시작 중...");
        let script = format!("sh -c '{}'", RESTART_SERVICES.replace('\'', "'\\''"));
        run_ssh(
            ssh(&script),
            None,
            Duration::from_secs(30),
            "stop remote host and core",
        )
        .await?;
    }
    progress_stage(progress, "설치 완료 확인 및 연결 중...");
    Ok(RemoteInstallInfo {
        host_id: verify_installed_host(profile).await?,
        platform,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("ddg-install-{name}-{}-{nanos}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_pair(dir: &Path) -> ResolvedBinaries {
        std::fs::create_dir_all(dir).unwrap();
        for name in [HOST_BIN, TERMINAL_BIN] {
            std::fs::write(dir.join(name), name).unwrap();
        }
        ResolvedBinaries::in_dir(dir).unwrap()
    }

    #[tokio::test]
    async fn client_and_install_rejects_pin_mismatch_before_changes() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let tmp = TempDir::new("id");
        let sock = tmp.0.join("s");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let profile = HostProfile {
            expected_host_id: Some("expected".into()),
            transport: HostTransport::Local {
                sock_path: Some(sock.to_string_lossy().into_owned()),
            },
            ..HostProfile::local_default()
        };
        let config_path = tmp.0.join("hosts.json");
        let bytes = serde_json::to_vec(&profile).unwrap();
        std::fs::write(&config_path, &bytes).unwrap();
        let serve = |host_id: &str, supported: bool| {
            let mut hello = serde_json::json!({"protocol_version":1, "host_id":host_id, "boot_generation":1, "conn_id":1});
            if supported {
                hello["features"] = serde_json::json!(["agent_activity_v1"]);
            }
            let response = format!(
                "{}\n",
                serde_json::json!({"jsonrpc":"2.0", "id":1, "result":hello})
            );
            let listener = &listener;
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                stream
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            }
        };
        let (result, ()) = tokio::join!(verify_install_target(&profile), serve("actual", false));
        assert_eq!(result.unwrap_err().kind, DialErrorKind::HostIdMismatch);
        let (result, ()) = tokio::join!(verify_install_target(&profile), serve("expected", false));
        assert!(result.unwrap());
        let (result, ()) = tokio::join!(verify_installed_host(&profile), serve("expected", false));
        assert!(
            result
                .unwrap_err()
                .message
                .contains("does not advertise agent_activity_v1")
        );
        let (result, ()) = tokio::join!(verify_installed_host(&profile), serve("actual", true));
        assert_eq!(result.unwrap_err().kind, DialErrorKind::HostIdMismatch);
        let (result, ()) = tokio::join!(verify_installed_host(&profile), serve("expected", true));
        assert_eq!(result.unwrap(), "expected");
        assert_eq!(std::fs::read(config_path).unwrap(), bytes);
        assert_eq!(profile.expected_host_id.as_deref(), Some("expected"));
    }

    #[test]
    fn client_and_install_restarts_only_verified_services() {
        let tmp = TempDir::new("r");
        let run = &tmp.0;
        let host = run.join("host.pid");
        let core = run.join("core.pid");
        // Stub both the listing and the per-PID comm lookup; never signal real processes.
        let fixtures = r#"
ps() {
    case "$*" in
        *'-p 42 '*) [ "$FORMAT" != busybox ] || return 1
            if [ "$FORMAT" = wrong_comm ]; then printf '%s\n' sleep; else printf '%s\n' '/bin/ddg-agents-host'; fi ;;
        *'-p 43 '*) [ "$FORMAT" != busybox ] || return 1
            printf '%s\n' "$CORE_COMM" ;;
        *'-p 44 '*) printf '%s\n' ddg-terminal-se ;;
        *'-p 45 '*) printf '%s\n' sleep ;;
        *'-p 46 '*) printf '%s\n' ddg-agents-host ;;
        *'-o pid,user,comm'*)
            [ "$FORMAT" = busybox ] || return 1
            owner=$(id -un)
            printf 'PID USER COMMAND\n42 %s ddg-agents-host\n43 %s ddg-terminal-server\n' "$owner" "$owner" ;;
        *'-o pid,user,args'*)
            [ "$FORMAT" = busybox ] || return 1
            owner=$(id -un)
            printf 'PID USER COMMAND\n 99 other ddg-agents-host serve\n'
            [ -e "$DDG_RUN_DIR/42.stopped" ] || printf '42 %s {ddg-agents-host} /home/user/.local/bin/ddg-agents-host serve\n' "$owner"
            [ -e "$DDG_RUN_DIR/43.stopped" ] || printf '43 %s {ddg-terminal-server} /bin/ddg-terminal-server core\n' "$owner"
            printf '44 %s ddg-terminal-server holder\n' "$owner" ;;
        *'-o pid=,args='*)
            [ "$FORMAT" != busybox ] || return 1
            printf '%s ddg-agents-host serve\n' "$$"
            [ -e "$DDG_RUN_DIR/42.stopped" ] || printf '%s\n' '   42 /home/user/.local/bin/ddg-agents-host serve'
            [ -e "$DDG_RUN_DIR/43.stopped" ] || printf '   43\t/bin/ddg-terminal-server core\n'
            printf '%s\n' '44 ddg-terminal-server holder' '45 sleep 60' '46 ddg-agents-host serve-extra' '47 sh -c ddg-agents-host serve' ;;
        *) return 1 ;;
    esac
}
kill() {
    [ "$1" = -TERM ] || return 1
    if [ "$2" = 43 ]; then
        [ -e "$DDG_RUN_DIR/42.stopped" ] && [ ! -S "$DDG_RUN_DIR/host.sock" ] || return 1
    fi
    printf '%s\n' "$2" >> "$DDG_RUN_DIR/signals"
    : > "$DDG_RUN_DIR/$2.stopped"
}
sleep() {
    [ -e "$DDG_RUN_DIR/42.stopped" ] && [ -S "$DDG_RUN_DIR/host.sock" ] || return 1
    rm "$DDG_RUN_DIR/host.sock"
}
"#;
        let execute = |format: &str, comm: &str| {
            for file in ["signals", "42.stopped", "43.stopped"] {
                let _ = std::fs::remove_file(run.join(file));
            }
            let script = format!("{fixtures}\n{RESTART_SERVICES}");
            let output = std::process::Command::new("sh")
                .args(["-c", &script])
                .env("DDG_RUN_DIR", run)
                .env("FORMAT", format)
                .env("CORE_COMM", comm)
                .current_dir(run)
                .output()
                .unwrap();
            let signals = std::fs::read_to_string(run.join("signals")).unwrap_or_default();
            assert!(!run.join("marker").exists());
            (output.status.success(), signals)
        };
        for (format, comm) in [
            ("linux", "ddg-terminal-se"),
            ("bsd", "/bin/ddg-terminal-server"),
            ("busybox", "ddg-terminal-server"),
        ] {
            assert_eq!(execute(format, comm), (true, "42\n43\n".into()));
        }
        std::fs::write(&host, "42\n").unwrap();
        std::fs::write(&core, "43").unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(run.join("host.sock")).unwrap();
        assert_eq!(
            execute("linux", "ddg-terminal-se"),
            (true, "42\n43\n".into())
        );
        assert!(!run.join("host.sock").exists());
        assert_eq!(
            execute("wrong_comm", "ddg-terminal-se"),
            (false, String::new())
        );
        assert_eq!(execute("linux", "sleep"), (false, String::new()));
        for payload in [
            "-1",
            "0",
            "1",
            "-42",
            "123;id",
            "$(touch marker)",
            "44",
            "45",
            "46",
            "42\n43",
            "42\n\n",
            "42\0",
            "042",
            "2147483648",
        ] {
            std::fs::write(&host, payload).unwrap();
            assert_eq!(
                execute("linux", "ddg-terminal-se"),
                (false, String::new()),
                "{payload:?}"
            );
        }
        std::fs::remove_file(&host).unwrap();
        std::os::unix::fs::symlink(&core, &host).unwrap();
        assert_eq!(execute("linux", "ddg-terminal-se"), (false, String::new()));
    }

    #[test]
    fn parses_plain_and_bannered_uname_output() {
        use RemotePlatform::*;
        for (input, expected) in [
            ("Darwin arm64", DarwinArm64),
            ("Darwin\nx86_64\n", DarwinX86_64),
            ("Linux x86_64", LinuxX86_64),
            ("Linux amd64", LinuxX86_64),
            ("Linux aarch64", LinuxArm64),
            ("Linux arm64", LinuxArm64),
            (
                "Welcome!\r\nLinux x86_64\r\nDDG_UNAME:Darwin:arm64\r\nBye!",
                DarwinArm64,
            ),
        ] {
            assert_eq!(parse_uname_output(input), Ok(expected));
        }
        for input in [
            "FreeBSD amd64",
            "Linux riscv64",
            "Linux",
            "DDG_UNAME:Linux:",
            "DDG_UNAME:Linux:x86_64:extra",
        ] {
            assert!(parse_uname_output(input).is_err(), "{input}");
        }
    }

    #[test]
    fn upload_script_validates_names() {
        for name in [HOST_BIN, TERMINAL_BIN, "safe_BIN-42"] {
            let script = build_atomic_upload_script(name).unwrap();
            assert!(script.contains(&format!(r#"tmp="$HOME/.local/bin/.{name}.tmp.$$""#)));
            assert!(script.contains(&format!(r#"mv -f "$tmp" "$HOME/.local/bin/{name}""#)));
        }
        for name in [
            "", "../host", "a/b", "a;id", "a'b", "$(id)", "a\nb", "a b", "한글",
        ] {
            assert!(build_atomic_upload_script(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn interrupted_upload_does_not_replace_existing_file() {
        use std::io::Write;
        let tmp = TempDir::new("upload");
        let dir = tmp.0.join(".local/bin");
        let bin = write_pair(&dir).host_bin;
        let script = atomic_upload_script(HOST_BIN, Some(4)).unwrap();
        for (bytes, success) in [(b"bad".as_slice(), false), (b"good".as_slice(), true)] {
            let mut child = std::process::Command::new("sh")
                .args(["-c", &script])
                .env("HOME", &tmp.0)
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(bytes).unwrap();
            assert_eq!(child.wait().unwrap().success(), success);
            assert_eq!(
                std::fs::read(&bin).unwrap(),
                if success { bytes } else { HOST_BIN.as_bytes() }
            );
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        }
    }

    #[test]
    fn cross_platform_lookup_requires_both_cached_binaries() {
        let tmp = TempDir::new("cross");
        let platform = if RemotePlatform::current() == Some(RemotePlatform::LinuxArm64) {
            RemotePlatform::LinuxX86_64
        } else {
            RemotePlatform::LinuxArm64
        };
        let dist = tmp.0.join("dist");
        let cache = dist.join(platform.slug());
        let lookup = || resolve_cached_or_local_binaries_in(platform, Some(&dist), Some(&tmp.0));
        write_pair(&tmp.0.join("target/release"));
        let expected = write_pair(&cache);
        std::fs::remove_file(&expected.terminal_server_bin).unwrap();
        assert!(lookup().is_none());
        let expected = write_pair(&cache);
        assert_eq!(lookup(), Some(expected));
    }

    #[test]
    fn local_lookup_prefers_workspace_release_then_debug_then_cache() {
        let Some(platform) = RemotePlatform::current() else {
            return;
        };
        let tmp = TempDir::new("local");
        let dist = tmp.0.join("dist");
        let lookup = || resolve_cached_or_local_binaries_in(platform, Some(&dist), Some(&tmp.0));
        let override_pair = lookup();
        let cached = write_pair(&dist.join(platform.slug()));
        let debug = write_pair(&tmp.0.join("target/debug"));
        let release = write_pair(&tmp.0.join("target/release"));
        assert_eq!(lookup().as_ref(), override_pair.as_ref().or(Some(&release)));
        std::fs::remove_file(&release.host_bin).unwrap();
        assert_eq!(lookup().as_ref(), override_pair.as_ref().or(Some(&debug)));
        std::fs::remove_file(&debug.terminal_server_bin).unwrap();
        assert_eq!(lookup(), override_pair.or(Some(cached)));
    }

    #[tokio::test]
    async fn ssh_step_preserves_security_diagnostics_on_exit_and_timeout() {
        for (script, expected) in [
            (
                "echo 'Permission denied (publickey).' >&2; exit 255",
                DialErrorKind::AuthRequired,
            ),
            (
                "echo 'Host key verification failed.' >&2; exec sleep 10",
                DialErrorKind::HostKeyMismatch,
            ),
        ] {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", script])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let error = run_ssh(cmd, None, Duration::from_millis(500), "probe")
                .await
                .unwrap_err();
            assert_eq!(error.kind, expected);
        }
    }

    #[tokio::test]
    async fn cache_publish_requires_a_complete_pair_and_repairs_partial_cache() {
        let tmp = TempDir::new("publish");
        let staging = tmp.0.join("staging");
        let destination = tmp.0.join("destination");
        let pair = write_pair(&staging);
        std::fs::remove_file(&pair.terminal_server_bin).unwrap();
        assert!(publish_binaries(&staging, &destination).await.is_err());
        assert!(!destination.exists());
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(destination.join(HOST_BIN), b"old host").unwrap();
        write_pair(&staging);
        let cached = publish_binaries(&staging, &destination).await.unwrap();
        assert_eq!(
            std::fs::read(&cached.host_bin).unwrap(),
            HOST_BIN.as_bytes()
        );
        assert_eq!(
            std::fs::read(&cached.terminal_server_bin).unwrap(),
            TERMINAL_BIN.as_bytes()
        );
    }
}
