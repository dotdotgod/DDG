//! In-place updates of pinned SSH hosts. Container bootstrap remains in `install`.

use crate::dial::{
    DialError, DialErrorKind, DialPolicy, build_pinned_ssh_command, dial_with_policy,
    sanitize_dial_error,
};
use crate::install::{
    HOST_BIN, PROBE_COMMAND, REMOTE_SHA256, TERMINAL_BIN, parse_uname_output, run_ssh,
    run_ssh_with_stderr, sh_command,
};
use crate::profile::{HostProfile, HostTransport};
use ddg_agents_proto::host::HostHelloResult;
use ddg_agents_update::{
    ApplyOutcome, UpdateError, UpdateErrorCode as Code, integrity::verify_sha256,
    manifest::ReleaseManifest, version::ReleaseTag,
};
use std::{path::Path, time::Duration};
use tokio::{fs, process::Command};

const STEP_TIMEOUT: Duration = Duration::from_secs(120);
const HOME_GUARD: &str = r#"
case ${HOME-} in /*) ;; *) exit 1 ;; esac
case "$HOME/" in */../*|*/./*) exit 1 ;; esac
"#;
const ROLLBACK: &str = r#"exec "$HOME/.local/bin/ddg-agents-host" __rollback-daemon-swap --install-dir "$HOME/.local/bin""#;

#[derive(Clone, Copy, Debug, Default)]
pub struct RemoteUpdateOptions {
    pub allow_downgrade: bool,
    pub restart_sessions: bool,
    pub force: bool,
}

fn ssh_target(profile: &HostProfile) -> Result<impl Fn(&str) -> Command + '_, UpdateError> {
    let (target, port, known_hosts_file, identity_file) = match &profile.transport {
        HostTransport::Ssh {
            target,
            port,
            known_hosts_file,
            identity_file,
        } => (target, *port, known_hosts_file, identity_file),
        HostTransport::ContainerSsh { .. } | HostTransport::DockerExec { .. } => {
            return Err(UpdateError::new(
                Code::ContainerUpdateUnsupported,
                "In-place container updates are unsupported",
            ));
        }
        HostTransport::Local { .. } => {
            return Err(UpdateError::new(
                Code::HostSecurityBlocked,
                "Remote update requires an SSH host",
            ));
        }
    };
    Ok(move |script: &str| {
        build_pinned_ssh_command(port, known_hosts_file, identity_file, target, script)
    })
}

fn dial_error(err: DialError) -> UpdateError {
    let code = match err.kind {
        DialErrorKind::ProtocolIncompatible => Code::ProtocolIncompatible,
        DialErrorKind::InvalidProfile
        | DialErrorKind::SecurityBlocked
        | DialErrorKind::HostKeyMismatch
        | DialErrorKind::AuthRequired
        | DialErrorKind::HostIdMismatch => Code::HostSecurityBlocked,
        DialErrorKind::HostNotInstalled | DialErrorKind::Unavailable => Code::NetworkError,
    };
    UpdateError::new(code, sanitize_dial_error(&err.message))
}

async fn pinned_hello(profile: &HostProfile, stage: &str) -> Result<HostHelloResult, UpdateError> {
    dial_with_policy(profile, stage, DialPolicy::RequirePinnedHostId)
        .await
        .map(|transport| transport.hello)
        .map_err(dial_error)
}

fn safe_path(path: &str) -> bool {
    path.split('/').all(|segment| {
        !matches!(segment, "" | "." | "..")
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    })
}

struct Stage<'a>(&'a str);

impl<'a> Stage<'a> {
    fn new(nonce: &'a str) -> Result<Self, UpdateError> {
        if nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(Self(nonce))
        } else {
            Err(UpdateError::new(
                Code::HostSecurityBlocked,
                "Invalid remote staging nonce",
            ))
        }
    }

    fn script(&self, body: &str) -> String {
        sh_command(&format!(
            r#"
set -eu
{HOME_GUARD}
export LC_ALL=C
stage="$HOME/.local/state/ddg/update/staging/{}"
{body}
"#,
            self.0
        ))
    }

    fn prepare(&self) -> String {
        self.script(
            r#"
umask 077
root=$HOME
for part in .local state ddg update staging; do
    root=$root/$part
    [ ! -L "$root" ]
    mkdir -p "$root"
done
mkdir "$stage"
chmod 700 "$stage" || { rmdir "$stage"; exit 1; }
"#,
        )
    }

    fn upload(&self, path: &str, size: usize, digest: &str, executable: bool) -> String {
        let mode = if executable { "700" } else { "600" };
        self.script(&format!(
            r#"
{REMOTE_SHA256}
umask 077
[ -d "$stage" ] && [ ! -L "$stage" ] || exit 1
trap 'rm -rf "$stage"' EXIT
trap 'exit 1' HUP INT TERM
file="$stage"/'{path}'
mkdir -p "${{file%/*}}"
(set -C; cat > "$file")
if ! [ "$(wc -c < "$file")" -eq {size} ] || ! verify_sha256 "$file" '{digest}'; then
    echo DDG_STAGE_CHECKSUM_MISMATCH >&2
    exit 1
fi
chmod {mode} "$file"
trap - EXIT HUP INT TERM
"#
        ))
    }

    fn cleanup(&self) -> String {
        self.script(r#"rm -rf "$stage""#)
    }
}

/// Apply using a locally extracted release and the digest of its verified manifest.
/// No HTTP requests are made here; artifacts are verified again before transfer.
pub async fn apply_remote_update(
    profile: &HostProfile,
    version: &str,
    staging_dir: &Path,
    manifest_sha256: &str,
    options: RemoteUpdateOptions,
) -> Result<ApplyOutcome, UpdateError> {
    let ssh = ssh_target(profile)?;
    let tag = ReleaseTag::parse(version)?;
    let before = pinned_hello(profile, "ddg-update-preflight").await?;
    let uname = run_ssh(
        ssh(PROBE_COMMAND),
        None,
        STEP_TIMEOUT,
        "probe remote platform",
    )
    .await
    .map_err(dial_error)?;
    let platform = parse_uname_output(&String::from_utf8_lossy(&uname))
        .map_err(|err| UpdateError::new(Code::AssetNotFound, err))?;
    let manifest_bytes =
        artifact_bytes(&staging_dir.join("manifest.json"), manifest_sha256).await?;
    let manifest = ReleaseManifest::parse(&manifest_bytes, &tag)?;
    if manifest.protocol_version != ddg_agents_proto::terminal::PROTOCOL_VERSION {
        return Err(UpdateError::new(
            Code::ProtocolIncompatible,
            "The staged release uses an incompatible protocol",
        ));
    }
    let artifact = manifest
        .artifacts
        .iter()
        .find(|asset| asset.platform == platform.slug())
        .ok_or_else(|| {
            UpdateError::new(
                Code::AssetNotFound,
                "No release artifact for the remote platform",
            )
        })?;
    let invalid = || {
        UpdateError::new(
            Code::ManifestInvalid,
            "Invalid remote daemon artifact paths",
        )
    };
    let host = artifact.binaries.get(HOST_BIN).ok_or_else(invalid)?;
    if !artifact.binaries.contains_key(TERMINAL_BIN)
        || artifact.binaries.values().any(|binary| {
            !safe_path(&binary.path) || binary.path.split('/').next() == Some("manifest.json")
        })
    {
        return Err(invalid());
    }
    let digest = manifest_sha256.to_ascii_lowercase();
    let mut random = [0; 16];
    ddg_agents_proto::id::fill_csprng(&mut random).map_err(|_| {
        UpdateError::new(
            Code::HostSecurityBlocked,
            "Could not generate a staging nonce",
        )
    })?;
    let nonce = format!("{:032x}", u128::from_be_bytes(random));
    let stage = Stage::new(&nonce)?;
    run_ssh(
        ssh(&stage.prepare()),
        None,
        STEP_TIMEOUT,
        "create remote staging",
    )
    .await
    .map_err(dial_error)?;
    let result = async {
        upload(&ssh, &stage, "manifest.json", &manifest_bytes, &digest, false).await?;
        for binary in artifact.binaries.values() {
            let bytes = artifact_bytes(&staging_dir.join(&binary.path), &binary.sha256).await?;
            upload(&ssh, &stage, &binary.path, &bytes, &binary.sha256.to_ascii_lowercase(), true).await?;
        }
        let mut flags = String::new();
        for (enabled, flag) in [(options.allow_downgrade, " --allow-downgrade"), (options.restart_sessions, " --restart-sessions"), (options.force, " --force")] {
            if enabled { flags.push_str(flag); }
        }
        let normalized = tag.version();
        let command = stage.script(&format!(r#"
host="$stage"/'{host}'
reported=$("$host" --version) || exit 1
case "$reported" in
    '{normalized}'|'v{normalized}'|'ddg-agents-host {normalized}'|'ddg-agents-host v{normalized}') ;;
    *) exit 1 ;;
esac
exec "$host" __apply-daemon-swap --install-dir "$HOME/.local/bin" --staging-dir "$stage" --version '{version}' --manifest-sha256 '{digest}'{flags}
"#, host = host.path));
        run_helper(ssh(&command)).await
    }.await;
    let cleanup = run_ssh(
        ssh(&stage.cleanup()),
        None,
        STEP_TIMEOUT,
        "remove remote staging",
    )
    .await;
    if let Err(err) = cleanup {
        let message = result
            .as_ref()
            .err()
            .map_or_else(|| "Remote swap ran".into(), |error| error.to_string());
        return Err(UpdateError::new(
            Code::HostSecurityBlocked,
            format!("{message}; staging cleanup failed: {}", dial_error(err)),
        ));
    }
    verify_outcome(profile, before.app_version.as_deref(), &tag, result?).await
}

/// Restore the rollback version advertised by the pinned remote host.
pub async fn rollback_remote_update(
    profile: &HostProfile,
    restart_sessions: bool,
) -> Result<ApplyOutcome, UpdateError> {
    let ssh = ssh_target(profile)?;
    let before = pinned_hello(profile, "ddg-rollback-preflight").await?;
    let version = before.rollback_version.as_deref().ok_or_else(|| {
        UpdateError::new(
            Code::RollbackUnavailable,
            "The remote host reports no rollback version",
        )
    })?;
    let version = ReleaseTag::parse(version)?;
    let flag = if restart_sessions {
        " --restart-sessions"
    } else {
        ""
    };
    let command = sh_command(&format!("{HOME_GUARD}\n{ROLLBACK}{flag}"));
    let outcome = run_helper(ssh(&command)).await?;
    verify_outcome(profile, before.app_version.as_deref(), &version, outcome).await
}

async fn artifact_bytes(path: &Path, digest: &str) -> Result<Vec<u8>, UpdateError> {
    let bytes = fs::read(path).await.map_err(|_| {
        UpdateError::new(
            Code::AssetNotFound,
            "Could not read staged release artifact",
        )
    })?;
    verify_sha256(bytes.as_slice(), digest)?;
    if bytes.is_empty() {
        return Err(UpdateError::new(
            Code::ChecksumMismatch,
            "Staged release artifact is empty",
        ));
    }
    Ok(bytes)
}

async fn upload(
    ssh: &impl Fn(&str) -> Command,
    stage: &Stage<'_>,
    path: &str,
    bytes: &[u8],
    digest: &str,
    executable: bool,
) -> Result<(), UpdateError> {
    let script = stage.upload(path, bytes.len(), digest, executable);
    run_ssh_with_stderr(
        ssh(&script),
        Some(bytes),
        STEP_TIMEOUT,
        "upload staged release artifact",
    )
    .await
    .map_err(|failure| {
        if failure
            .stderr
            .split(|b| *b == b'\n')
            .any(|line| line == b"DDG_STAGE_CHECKSUM_MISMATCH")
        {
            UpdateError::new(
                Code::ChecksumMismatch,
                "Remote size or SHA-256 verification failed",
            )
        } else {
            dial_error(failure.error)
        }
    })?;
    Ok(())
}

async fn run_helper(command: Command) -> Result<ApplyOutcome, UpdateError> {
    #[derive(serde::Deserialize)]
    struct HelperError {
        code: Code,
        message: String,
    }

    match run_ssh_with_stderr(command, None, STEP_TIMEOUT, "remote daemon swap").await {
        Ok(output) => serde_json::from_slice(&output).map_err(|_| {
            UpdateError::new(
                Code::HostSecurityBlocked,
                "Remote swap returned an invalid outcome",
            )
        }),
        Err(failure) => {
            if let Some(error) = String::from_utf8_lossy(&failure.stderr)
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .and_then(|line| serde_json::from_str::<HelperError>(line).ok())
            {
                return Err(UpdateError::new(error.code, error.message));
            }
            let error = dial_error(failure.error);
            Err(UpdateError::new(Code::HostSecurityBlocked, error.message()))
        }
    }
}

fn matches_version(reported: &str, expected: &ReleaseTag) -> bool {
    ReleaseTag::parse(reported).is_ok_and(|tag| tag.version() == expected.version())
}

async fn verify_outcome(
    profile: &HostProfile,
    previous: Option<&str>,
    target: &ReleaseTag,
    outcome: ApplyOutcome,
) -> Result<ApplyOutcome, UpdateError> {
    let hello = pinned_hello(profile, "ddg-update-verify").await?;
    let mismatch = || {
        UpdateError::new(
            Code::HostSecurityBlocked,
            "Pinned remote host did not report the expected version after the swap",
        )
    };
    let current = hello.app_version.as_deref().ok_or_else(mismatch)?;
    let version_ok = match &outcome {
        ApplyOutcome::Completed { to_version, .. } => {
            matches_version(to_version, target) && matches_version(current, target)
        }
        ApplyOutcome::PendingHolderRestart { to_version, .. } => {
            matches_version(to_version, target)
                && previous
                    .and_then(|v| ReleaseTag::parse(v).ok())
                    .is_some_and(|tag| matches_version(current, &tag))
        }
        ApplyOutcome::AutoRolledBack {
            attempted_version,
            restored_version,
            ..
        } => {
            matches_version(attempted_version, target)
                && ReleaseTag::parse(restored_version).is_ok_and(|tag| {
                    previous.is_none_or(|v| matches_version(v, &tag))
                        && matches_version(current, &tag)
                })
        }
        ApplyOutcome::HandedOffToHelper { .. } => false,
    };
    if version_ok {
        Ok(outcome)
    } else {
        Err(mismatch())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    #[test]
    fn safe_paths_and_staging_nonces() {
        for value in [
            "", ".", "..", "a/../b", "/root", "a//b", "a;b", "a'b", "a\0b", "$(id)", "foo\nbar",
        ] {
            assert!(!safe_path(value), "{value:?}");
            assert!(Stage::new(value).is_err(), "{value:?}");
        }
        assert!(safe_path("bin/DDG-1.2.3"));
        assert!(Stage::new("1234567890abcdef1234567890abcdef").is_ok());
    }

    #[test]
    fn staging_requires_exact_size_and_all_hash_fallback_formats() {
        const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let dir = std::env::temp_dir().join(format!("ddg-update-script-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("tools");
        fs::create_dir(&path).unwrap();
        for tool in ["awk", "cat", "chmod", "mkdir", "rm", "sh", "wc"] {
            std::os::unix::fs::symlink(
                format!(
                    "{}{}",
                    if tool == "chmod"
                        || tool == "mkdir"
                        || tool == "rm"
                        || tool == "cat"
                        || tool == "sh"
                    {
                        "/bin/"
                    } else {
                        "/usr/bin/"
                    },
                    tool
                ),
                path.join(tool),
            )
            .unwrap();
        }
        let stage = Stage::new("1234567890abcdef1234567890abcdef").unwrap();
        let remote = dir.join(".local/state/ddg/update/staging/1234567890abcdef1234567890abcdef");
        let run = |cmd: &str, bytes: &[u8]| {
            // Execute our command with a deliberately sparse PATH.
            let mut child = Command::new("/bin/sh")
                .args(["-c", cmd])
                .env("HOME", &dir)
                .env("PATH", &path)
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let _ = child.stdin.take().unwrap().write_all(bytes);
            child.wait_with_output().unwrap().status.success()
        };
        for (tool, output, bytes, success) in [
            ("sha256sum", format!("{ABC}  -"), &b"abc"[..], true),
            (
                "shasum",
                format!("{}  -", ABC.to_ascii_uppercase()),
                &b"abc"[..],
                true,
            ),
            (
                "openssl",
                format!("SHA2-256(stdin)= {ABC}"),
                &b"abc"[..],
                true,
            ),
            ("openssl", format!("(stdin)= {ABC}0"), &b"abc"[..], false),
            (
                "sha256sum",
                format!("{}  -", "0".repeat(64)),
                &b"abc"[..],
                false,
            ),
            ("sha256sum", format!("{ABC}  -"), &b"ab"[..], false),
            ("", String::new(), &b"abc"[..], false),
        ] {
            if !tool.is_empty() {
                let args = match tool {
                    "shasum" => "-a 256",
                    "openssl" => "dgst -sha256",
                    _ => "",
                };
                let executable = path.join(tool);
                fs::write(
                    &executable,
                    format!("#!/bin/sh\n[ \"$*\" = '{args}' ] && printf '%s\\n' '{output}'\n"),
                )
                .unwrap();
                fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
            }
            assert!(run(&stage.prepare(), b""));
            assert_eq!(
                fs::metadata(&remote).unwrap().permissions().mode() & 0o777,
                0o700
            );
            let upload = stage.upload("bin/file", 3, ABC, true);
            assert_eq!(run(&upload, bytes), success, "{tool}: {output}");
            assert_eq!(remote.exists(), success);
            if success {
                assert!(run(&stage.cleanup(), b""));
            }
            if !tool.is_empty() {
                fs::remove_file(path.join(tool)).unwrap();
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
