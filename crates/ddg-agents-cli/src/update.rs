//! Explicit operator update commands. Normal CLI dispatch never enters this module.

#[cfg(target_os = "macos")]
mod app;

use crate::{
    args::{Action, Args},
    output::{Error, Output, Result, cell, diagnostic},
};
use ddg_agents_os::fs::{ensure_private_dir, write_atomic};
use ddg_agents_proto::{host::HostHelloResult, terminal::PROTOCOL_VERSION};
use ddg_agents_transport::{
    HostProfile, HostTransport, HostsConfig,
    dial::{DialPolicy, dial_with_policy},
    install::{RemotePlatform, parse_uname_output},
    remote_update::{RemoteUpdateOptions, apply_remote_update, rollback_remote_update},
};
use ddg_agents_update::{
    ApplyOutcome, UpdateErrorCode as Code,
    archive::{extract_tar_gz, extract_zip},
    config::{AutoCheckPolicy, RepoSlug, SourceOrigin, UpdateChannel, UpdateConfig},
    github::{self, FetchResult},
    integrity::{RELEASE_PUBKEYS, verify_manifest, verify_sha256},
    manager::{CheckOutcome, UpdateManager, UpdatePaths},
    manifest::{ReleaseArtifact, ReleaseManifest},
    release::GitHubRelease,
    state::{ReleaseSummary, UpdateState},
    version::{ReleaseTag, validate_apply_consent},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{process::Command, time::timeout};

const HOST: &str = "ddg-agents-host";
const CORE: &str = "ddg-terminal-server";
#[cfg(target_os = "macos")]
const CLI: &str = "ddg";
const STAGING_PREFIX: &str = ".ddg-cli-";
const LEGACY: &str = "0.1.0";
const CONTAINER_GUIDE: &str = "Update the container image in its original Docker or Compose configuration, then recreate the container.";

pub async fn run(args: &Args<'_>, out: Output) -> Result {
    if args.action == Action::UpdateApplyHelper {
        #[cfg(target_os = "macos")]
        return show_outcome(out, "local", None, app::run_helper(args).await?);
        #[cfg(not(target_os = "macos"))]
        return Err(Error::update(
            Code::HostSecurityBlocked,
            "The app helper requires macOS",
        ));
    }
    let manager = UpdateManager::new(UpdatePaths::from_environment()?);
    let mut config = manager.load_config()?;
    if args.action == Action::UpdateConfig {
        return configure(args, out, &manager, &mut config);
    }
    let mut hosts = crate::load_config(&crate::config_path(args)?)?;
    if hosts.find("local").is_none() {
        hosts.hosts.push(HostProfile::local_default());
    }
    let targets = targets(args, &hosts)?;
    if matches!(args.action, Action::UpdateApply | Action::UpdateRollback) {
        return change(args, out, &manager, config, targets[0]).await;
    }

    let (repo, origin) = config.effective_repo()?;
    let source = source_json(&repo, origin);
    let mut check = None;
    if args.action == Action::UpdateCheck {
        check = manager.check_for_updates(false).await?;
        if let Some(CheckOutcome::RateLimited { reset_at, .. }) = check {
            return Err(Error::update(
                Code::RateLimited,
                match reset_at {
                    Some(time) => format!("GitHub rate limit reached; resets at Unix time {time}"),
                    None => "GitHub rate limit reached".into(),
                },
            ));
        }
    }
    let state = manager.load_state()?;
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    let all = args
        .host
        .is_some_and(|id| id.trim().eq_ignore_ascii_case("all"));
    for target in targets {
        match status(target, &repo, &config, &state, check.is_some()).await {
            Ok(entry) => entries.push(entry),
            Err(error) if all => errors.push(json!({"host": target.id, "error": error})),
            Err(error) => return Err(error),
        }
    }
    let render = || {
        let mut text = source_text(&repo, origin);
        for entry in &entries {
            let field = |key: &str| entry[key].as_str().unwrap_or("-");
            text.push_str(&format!(
                "{}: {} (current {}, latest {}, rollback {})\n",
                cell(field("host")),
                cell(field("status")),
                cell(field("current_version")),
                cell(field("latest_version")),
                cell(field("rollback_version"))
            ));
        }
        for entry in &errors {
            text.push_str(&format!(
                "{}: {}\n",
                cell(entry["host"].as_str().unwrap_or_default()),
                diagnostic(entry["error"]["message"].as_str().unwrap_or_default())
            ));
        }
        text
    };
    let mut value = if all {
        json!({"hosts": entries, "errors": errors})
    } else {
        entries[0].clone()
    };
    value["source"] = source;
    out.show(&value, render)
}

fn targets<'a>(args: &Args<'_>, hosts: &'a HostsConfig) -> Result<Vec<&'a HostProfile>> {
    match args.host.unwrap_or("local") {
        all if all.trim().eq_ignore_ascii_case("all") => Ok(hosts.hosts.iter().collect()),
        id => {
            let target = crate::profile(hosts, id)?;
            if id.trim().eq_ignore_ascii_case("local")
                && !matches!(target.transport, HostTransport::Local { .. })
            {
                return Err(Error::update(
                    Code::HostSecurityBlocked,
                    "The local update target must use local transport",
                ));
            }
            Ok(vec![target])
        }
    }
}

fn configure(
    args: &Args<'_>,
    out: Output,
    manager: &UpdateManager,
    config: &mut UpdateConfig,
) -> Result {
    let old = config.clone();
    if let Some(policy) = args.value("auto-check") {
        config.auto_check = if policy == "on" {
            AutoCheckPolicy::On
        } else {
            AutoCheckPolicy::Off
        };
    }
    if let Some(channel) = channel(args) {
        config.channel = channel;
    }
    if let Some(repo) = args.value("repo") {
        config.repo = Some(RepoSlug::parse(repo)?.as_str().into());
    } else if args.has("clear-repo") {
        config.repo = None;
    }
    let (repo, origin) = config.effective_repo()?;
    if old != *config {
        manager.save_config(config)?;
    }
    out.show(
        &json!({"config": config, "source": source_json(&repo, origin)}),
        || {
            format!(
                "{}{}\n",
                source_text(&repo, origin),
                serde_json::to_string_pretty(&config).expect("serializable update config")
            )
        },
    )
}

fn channel(args: &Args<'_>) -> Option<UpdateChannel> {
    args.value("channel").map(|value| {
        if value == "stable" {
            UpdateChannel::Stable
        } else {
            UpdateChannel::Prerelease
        }
    })
}

fn source_json(repo: &RepoSlug, origin: SourceOrigin) -> Value {
    json!({"repo": repo.as_str(), "origin": origin, "custom": origin != SourceOrigin::Default})
}

fn source_text(repo: &RepoSlug, origin: SourceOrigin) -> String {
    match origin {
        SourceOrigin::Default => String::new(),
        SourceOrigin::Config => format!("[커스텀 소스] {} (config)\n", diagnostic(repo.as_str())),
        SourceOrigin::Env => format!(
            "[커스텀 소스] {} (DDG_UPDATE_REPO)\n",
            diagnostic(repo.as_str())
        ),
    }
}

async fn status(
    target: &HostProfile,
    repo: &RepoSlug,
    config: &UpdateConfig,
    state: &UpdateState,
    checked: bool,
) -> Result<Value> {
    let local = matches!(target.transport, HostTransport::Local { sock_path: None });
    let (current, platform, rollback, active, holder) = if local {
        let rollback = state.rollback.as_ref().map(|record| record.version.clone());
        #[cfg(target_os = "macos")]
        let rollback = rollback.or_else(|| {
            let dir = ddg_agents_os::paths::state_dir().ok()?;
            app::rollback_version(&dir)
        });
        (
            Some(state.current_version.clone()),
            local_platform().map(str::to_owned),
            rollback,
            state
                .pending_holder_restart
                .as_ref()
                .map(|pending| pending.active_pty_count),
            None,
        )
    } else {
        let hello = hello(target, false).await?;
        (
            hello.app_version,
            hello.platform,
            hello.rollback_version,
            hello.active_pty_count,
            hello.holder_protocol_version,
        )
    };
    let latest = state
        .cached_latest
        .as_ref()
        .filter(|release| release.repo == repo.as_str() && release.check_channel == config.channel);
    let available = match (&current, latest) {
        (Some(current), Some(latest)) => ReleaseTag::parse(&latest.tag)?
            .version()
            .cmp_precedence(ReleaseTag::parse(current)?.version())
            .is_gt(),
        _ => false,
    };
    let pending = local
        .then_some(state.pending_holder_restart.as_ref())
        .flatten();
    let failed = latest
        .filter(|_| local && available)
        .and_then(|latest| state.failed_version(&latest.version));
    Ok(json!({
        "host": target.id, "current_version": current.as_deref().map(diagnostic),
        "platform": platform.as_deref().map(diagnostic),
        "rollback_version": rollback.as_deref().map(diagnostic),
        "holder_protocol_version": holder, "active_pty_count": active,
        "latest_version": latest.map(|release| diagnostic(&release.version)),
        "release": latest.map(release_json).transpose()?,
        "last_checked_at": state.last_checked_at, "auto_check": config.auto_check, "channel": config.channel,
        "pending_holder_restart": pending.map(|pending| json!({
            "to_version": diagnostic(&pending.to_version), "active_pty_count": pending.active_pty_count,
        })),
        "auto_rolled_back": local.then_some(state.auto_rolled_back.as_ref()).flatten().map(|notice| json!({
            "attempted_version": diagnostic(&notice.attempted_version),
            "restored_version": diagnostic(&notice.restored_version), "reason": diagnostic(&notice.reason),
        })),
        "status": if pending.is_some() { "pending_holder_restart" } else if failed.is_some() { "failed_version_blocked" }
            else if available { "update_available" } else if checked && current.is_some() { "up_to_date" } else { "status" },
    }))
}

fn release_json(release: &ReleaseSummary) -> Result<Value> {
    let repo = RepoSlug::parse(&release.repo)?;
    let tag = ReleaseTag::parse(&release.tag)?;
    Ok(
        json!({"tag": tag.as_str(), "version": tag.version().to_string(), "notes":
            ddg_agents_update::redact::redact_sensitive_text(&ddg_agents_update::release::sanitize_release_notes(&release.notes)),
            "web_url": ddg_agents_update::release::build_release_web_url(&repo, &tag),
        }),
    )
}

fn require_restart(restart: bool, needed: bool) -> Result {
    if needed && !restart {
        return Err(Error::update(
            Code::HolderRestartRequired,
            "Restart requires --restart-sessions; active sessions may stop",
        ));
    }
    Ok(())
}

fn confirmation(yes: bool, tty: bool, prompt: impl FnOnce() -> io::Result<bool>) -> Result {
    if yes || (tty && prompt()?) {
        return Ok(());
    }
    Err(Error::update(
        Code::ConfirmationRequired,
        "Explicit confirmation required; use --yes in scripts",
    ))
}

fn confirm(args: &Args<'_>) -> Result {
    confirmation(
        args.has("yes"),
        io::stdin().is_terminal() && io::stderr().is_terminal(),
        || {
            eprint!("Change the installed DDG version? [y/N] ");
            io::stderr().flush()?;
            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            Ok(matches!(
                answer.trim().to_ascii_lowercase().as_str(),
                "y" | "yes"
            ))
        },
    )
}

async fn change(
    args: &Args<'_>,
    out: Output,
    manager: &UpdateManager,
    config: UpdateConfig,
    target: &HostProfile,
) -> Result {
    let local = match &target.transport {
        HostTransport::Local { sock_path: None } => true,
        HostTransport::Local {
            sock_path: Some(path),
        } => {
            if Path::new(path) != ddg_agents_os::paths::run_dir()?.join("host.sock") {
                return Err(Error::update(
                    Code::HostSecurityBlocked,
                    "Updates cannot target another local runtime via a custom socket",
                ));
            }
            true
        }
        HostTransport::Ssh { .. } => false,
        HostTransport::ContainerSsh { .. } | HostTransport::DockerExec { .. } => {
            return Err(Error::update(
                Code::ContainerUpdateUnsupported,
                CONTAINER_GUIDE,
            ));
        }
    };
    let restart = args.has("restart-sessions");
    let state = if local {
        Some(manager.load_state()?)
    } else {
        None
    };
    require_restart(
        restart,
        state
            .as_ref()
            .is_some_and(|state| state.pending_holder_restart.is_some()),
    )?;
    confirm(args)?;

    if args.action == Action::UpdateRollback {
        let outcome = if local {
            local_rollback(
                manager.paths(),
                state.as_ref().expect("local update state"),
                restart,
            )
            .await?
        } else {
            rollback_remote_update(target, restart).await?
        };
        return show_outcome(out, &target.id, None, outcome);
    }

    let info = if local {
        local_hello(target).await?
    } else {
        Some(hello(target, true).await?)
    };
    let current = info
        .as_ref()
        .map(|h| h.app_version.as_deref().unwrap_or(LEGACY))
        .or_else(|| state.as_ref().map(|s| s.current_version.as_str()))
        .unwrap_or(LEGACY);
    let platform = if local {
        local_platform()
            .ok_or_else(|| Error::update(Code::AssetNotFound, "Unsupported local platform"))?
            .to_owned()
    } else {
        match info.as_ref().and_then(|h| h.platform.as_deref()) {
            Some(platform) => platform.to_owned(),
            None => probe_legacy_platform(target).await?,
        }
    };
    let (repo, origin) = config.effective_repo()?;
    let channel = channel(args).unwrap_or(config.channel);
    if !out.0 {
        eprint!("{}", source_text(&repo, origin));
    }
    let current = ReleaseTag::parse(current)?;
    let tag = match args.value("version") {
        Some(tag) => ReleaseTag::parse(tag)?,
        None => {
            let Some(tag) = latest_tag(&repo, channel)
                .await?
                .filter(|tag| tag.version().cmp_precedence(current.version()).is_gt())
            else {
                return out.show(&json!({"host": target.id, "status": "up_to_date", "current_version": current.version().to_string(),
                    "source": source_json(&repo, origin)}), || format!("{}: up to date ({})\n", target.id, current.version()));
            };
            tag
        }
    };
    if channel == UpdateChannel::Stable && !tag.version().pre.is_empty() {
        return Err(Error::update(
            Code::ManifestInvalid,
            "Select the prerelease channel to apply a prerelease",
        ));
    }
    let force = args.has("force");
    let allow_downgrade = args.has("allow-downgrade");
    validate_apply_consent(
        current.version(),
        tag.version(),
        true,
        false,
        0,
        allow_downgrade,
        restart,
    )?;
    if let Some(state) = &state {
        state.should_attempt(tag.as_str(), force)?;
    }
    eprintln!("ddg: verifying release {}", tag.as_str());
    let verified = fetch_release(&repo, &tag, &platform).await?;
    let manifest = &verified.manifest;
    let holder = info.as_ref().and_then(|h| h.holder_protocol_version);
    let active = info.as_ref().and_then(|h| h.active_pty_count).unwrap_or(0);
    let needs_restart = manifest.holder_restart_required
        || holder.is_some_and(|v| v != manifest.holder_protocol_version);
    validate_apply_consent(
        current.version(),
        tag.version(),
        manifest.min_compatible_schema <= 1,
        needs_restart,
        active,
        allow_downgrade,
        restart,
    )?;
    // Verify the compiled Apple identity before using a Darwin bundle. A
    // non-macOS CLI cannot run the system signature and Gatekeeper checks.
    #[cfg(not(target_os = "macos"))]
    if platform.starts_with("darwin-") {
        return Err(Error::update(
            Code::CodesignInvalid,
            "Darwin app artifacts require a macOS update client",
        ));
    }
    let stage = stage_release(manager.paths(), &verified).await?;
    #[cfg(target_os = "macos")]
    let bundle = app::verified_bundle(stage.path(), verified.artifact())?;
    let outcome = if local {
        #[cfg(target_os = "macos")]
        if let Some(bundle) = bundle {
            let install = install_dir()?;
            if let Some(target_app) = app::enclosing_app(&install) {
                return show_outcome(
                    out,
                    &target.id,
                    Some((&repo, origin)),
                    app::handoff(
                        manager.paths(),
                        stage,
                        bundle,
                        target_app,
                        &verified,
                        &tag,
                        args,
                    )?,
                );
            }
        }
        local_apply(
            manager.paths(),
            stage.path(),
            &verified,
            &tag,
            RemoteUpdateOptions {
                allow_downgrade,
                restart_sessions: restart,
                force,
            },
        )
        .await?
    } else {
        apply_remote_update(
            target,
            tag.as_str(),
            stage.path(),
            &verified.digest(),
            RemoteUpdateOptions {
                allow_downgrade,
                restart_sessions: restart,
                force,
            },
        )
        .await?
    };
    show_outcome(out, &target.id, Some((&repo, origin)), outcome)
}

async fn local_hello(profile: &HostProfile) -> Result<Option<HostHelloResult>> {
    let socket = ddg_agents_os::paths::run_dir()?.join("host.sock");
    #[cfg(unix)]
    let present = socket.symlink_metadata().map(drop);
    #[cfg(windows)]
    let present = ddg_agents_os::ipc::connect_async(&socket).await.map(drop);
    match present {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(_) => {
            let mut profile = profile.clone();
            profile.transport = HostTransport::Local {
                sock_path: Some(socket.to_string_lossy().into_owned()),
            };
            hello(&profile, false).await.map(Some)
        }
    }
}

async fn hello(profile: &HostProfile, pinned: bool) -> Result<HostHelloResult> {
    let policy = if pinned {
        DialPolicy::RequirePinnedHostId
    } else {
        DialPolicy::Standard
    };
    let transport = timeout(
        crate::CONNECT_TIMEOUT,
        dial_with_policy(profile, "ddg-update", policy),
    )
    .await
    .map_err(|_| Error::update(Code::NetworkError, "Update target connection timed out"))??;
    Ok(transport.hello)
}

fn artifact_binary<'a>(
    artifact: &'a ReleaseArtifact,
    name: &str,
) -> Option<&'a ddg_agents_update::manifest::BinaryDigest> {
    artifact.binaries.get(name).or_else(|| {
        if cfg!(windows) {
            artifact.binaries.get(&format!("{name}.exe"))
        } else {
            None
        }
    })
}

fn local_platform() -> Option<&'static str> {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        return Some("windows-x86_64");
    }
    RemotePlatform::current().map(|platform| platform.slug())
}

async fn probe_legacy_platform(target: &HostProfile) -> Result<String> {
    let HostTransport::Ssh {
        target,
        port,
        known_hosts_file,
        identity_file,
    } = &target.transport
    else {
        unreachable!()
    };
    // The pinned DDG handshake has already succeeded. Legacy hellos omit platform.
    let mut cmd = Command::new("ssh");
    cmd.env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .args([
            "-T",
            "-oBatchMode=yes",
            "-oStrictHostKeyChecking=yes",
            "-oControlPath=none",
            "-oClearAllForwardings=yes",
        ]);
    if let Some(port) = port {
        cmd.arg("-p").arg(port.to_string());
    }
    if let Some(file) = known_hosts_file {
        cmd.arg("-o").arg(format!("UserKnownHostsFile={file}"));
    }
    if let Some(file) = identity_file {
        cmd.arg("-i").arg(file).arg("-oIdentitiesOnly=yes");
    }
    let output = timeout(
        crate::CONNECT_TIMEOUT,
        cmd.args([
            "--",
            target,
            r#"printf 'DDG_UNAME:%s:%s\n' "$(uname -s)" "$(uname -m)""#,
        ])
        .output(),
    )
    .await
    .map_err(|_| Error::update(Code::NetworkError, "Remote platform probe timed out"))??;
    if !output.status.success() {
        return Err(Error::update(
            Code::HostSecurityBlocked,
            "Pinned SSH platform probe failed",
        ));
    }
    parse_uname_output(&String::from_utf8_lossy(&output.stdout))
        .map(|platform| platform.slug().to_owned())
        .map_err(|err| Error::update(Code::AssetNotFound, err))
}

async fn latest_tag(repo: &RepoSlug, channel: UpdateChannel) -> Result<Option<ReleaseTag>> {
    let FetchResult::Modified { body, .. } =
        github::auto_check(AutoCheckPolicy::On, repo, None).await?
    else {
        return Err(Error::update(
            Code::NetworkError,
            "Unexpected response from GitHub releases",
        ));
    };
    let releases: Vec<GitHubRelease> = serde_json::from_slice(&body)
        .map_err(|_| Error::update(Code::ManifestInvalid, "Invalid GitHub release list"))?;
    Ok(releases
        .into_iter()
        .filter(|release| !release.draft)
        .filter_map(|release| {
            let tag = release.tag().ok()?;
            (channel == UpdateChannel::Prerelease
                || (!release.prerelease && tag.version().pre.is_empty()))
            .then_some(tag)
        })
        .max_by(|a, b| {
            a.version()
                .cmp_precedence(b.version())
                .then_with(|| a.as_str().cmp(b.as_str()))
        }))
}

struct VerifiedRelease {
    raw: Vec<u8>,
    signature: Option<Vec<u8>>,
    manifest: ReleaseManifest,
    artifact_index: usize,
    asset_url: String,
}

impl VerifiedRelease {
    fn artifact(&self) -> &ReleaseArtifact {
        &self.manifest.artifacts[self.artifact_index]
    }
    fn digest(&self) -> String {
        format!("{:x}", Sha256::digest(&self.raw))
    }
}

async fn fetch_release(
    repo: &RepoSlug,
    tag: &ReleaseTag,
    platform: &str,
) -> Result<VerifiedRelease> {
    let tag_path = tag.as_str().replace('+', "%2B");
    let base = format!(
        "https://github.com/{}/releases/download/{tag_path}",
        repo.as_str()
    );
    let raw = fetch_bytes(&format!("{base}/manifest.json")).await?;
    let signature = if RELEASE_PUBKEYS.is_empty() {
        None
    } else {
        match fetch_bytes(&format!("{base}/manifest.json.sig")).await {
            Ok(bytes) => Some(bytes),
            Err(error) if error.code == "asset_not_found" => None,
            Err(error) => return Err(error),
        }
    };
    let manifest = verify_manifest(&raw, signature.as_deref(), tag)?;
    if manifest.protocol_version != PROTOCOL_VERSION {
        return Err(Error::update(
            Code::ProtocolIncompatible,
            "Release protocol is incompatible",
        ));
    }
    let artifact_index = manifest
        .artifacts
        .iter()
        .position(|asset| asset.platform == platform)
        .ok_or_else(|| Error::update(Code::AssetNotFound, "No artifact for the target platform"))?;
    let asset_url = format!("{base}/{}", manifest.artifacts[artifact_index].name);
    Ok(VerifiedRelease {
        raw,
        signature,
        manifest,
        artifact_index,
        asset_url,
    })
}

async fn fetch_bytes(url: &str) -> Result<Vec<u8>> {
    match github::fetch_json(url, None).await? {
        FetchResult::Modified { body, .. } => Ok(body),
        _ => Err(Error::update(
            Code::NetworkError,
            "Unexpected response while fetching release metadata",
        )),
    }
}

async fn stage_release(
    paths: &UpdatePaths,
    verified: &VerifiedRelease,
) -> Result<tempfile::TempDir> {
    let staging = paths.staging_dir();
    ensure_private_dir(&paths.state_dir)?;
    ensure_private_dir(&staging)?;
    let stage = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(staging.canonicalize()?)?;
    ensure_private_dir(stage.path())?;
    let archive = stage.path().join("release.archive");
    let artifact = verified.artifact();
    if artifact_binary(artifact, HOST).is_none() || artifact_binary(artifact, CORE).is_none() {
        return Err(Error::update(
            Code::ManifestInvalid,
            "Release does not list both daemons",
        ));
    }
    github::download_archive(&verified.asset_url, &archive, &artifact.sha256).await?;
    let file = File::open(&archive)?;
    if artifact.name.ends_with(".zip") {
        extract_zip(file, stage.path())?;
    } else if artifact.name.ends_with(".tar.gz") {
        extract_tar_gz(file, stage.path())?;
    } else {
        return Err(Error::update(
            Code::ArchiveUnsafe,
            "Unsupported release archive format",
        ));
    }
    for binary in artifact.binaries.values() {
        let path = stage.path().join(&binary.path);
        if path.canonicalize()? != path || !path.symlink_metadata()?.is_file() {
            return Err(Error::update(
                Code::ArchiveUnsafe,
                "Non-regular staged binary path",
            ));
        }
        verify_sha256(File::open(path)?, &binary.sha256)?;
    }
    if stage
        .path()
        .join("manifest.json")
        .symlink_metadata()
        .is_ok()
    {
        return Err(Error::update(
            Code::ArchiveUnsafe,
            "Archive overlaps the verified manifest",
        ));
    }
    write_atomic(&stage.path().join("manifest.json"), &verified.raw)?;
    if let Some(signature) = &verified.signature {
        if stage
            .path()
            .join("manifest.json.sig")
            .symlink_metadata()
            .is_ok()
        {
            return Err(Error::update(
                Code::ArchiveUnsafe,
                "Archive overlaps the verified manifest signature",
            ));
        }
        write_atomic(&stage.path().join("manifest.json.sig"), signature)?;
    }
    Ok(stage)
}

fn install_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = exe.parent().ok_or_else(|| {
        Error::update(
            Code::InstallDirNotWritable,
            "Executable has no install directory",
        )
    })?;
    Ok(dir.into())
}

async fn local_rollback(
    paths: &UpdatePaths,
    state: &UpdateState,
    restart: bool,
) -> Result<ApplyOutcome> {
    let install = install_dir()?;
    #[cfg(target_os = "macos")]
    if let Some(app) = app::enclosing_app(&install) {
        return app::rollback(paths, app, restart).await;
    }
    if state.rollback.is_none() {
        return Err(Error::update(
            Code::RollbackUnavailable,
            "No verified N-1 daemon backup recorded",
        ));
    }
    let mut command = daemon_command(
        &install.join(format!("{HOST}{}", std::env::consts::EXE_SUFFIX)),
        paths,
        &install,
        "__rollback-daemon-swap",
    );
    if restart {
        command.arg("--restart-sessions");
    }
    run_daemon(command).await
}

async fn local_apply(
    paths: &UpdatePaths,
    stage: &Path,
    verified: &VerifiedRelease,
    tag: &ReleaseTag,
    options: RemoteUpdateOptions,
) -> Result<ApplyOutcome> {
    let install = install_dir()?;
    let mut command = daemon_apply(
        paths,
        &install,
        stage,
        verified.artifact(),
        tag,
        &verified.digest(),
    );
    for (enabled, flag) in [
        (options.allow_downgrade, "--allow-downgrade"),
        (options.restart_sessions, "--restart-sessions"),
        (options.force, "--force"),
    ] {
        if enabled {
            command.arg(flag);
        }
    }
    run_daemon(command).await
}

fn daemon_apply(
    paths: &UpdatePaths,
    install: &Path,
    stage: &Path,
    artifact: &ReleaseArtifact,
    tag: &ReleaseTag,
    digest: &str,
) -> Command {
    let mut command = daemon_command(
        &stage.join(
            &artifact_binary(artifact, HOST)
                .expect("validated artifact")
                .path,
        ),
        paths,
        install,
        "__apply-daemon-swap",
    );
    command.arg("--staging-dir").arg(stage).args([
        "--version",
        tag.as_str(),
        "--manifest-sha256",
        digest,
    ]);
    command
}

fn daemon_command(exe: &Path, paths: &UpdatePaths, install: &Path, subcommand: &str) -> Command {
    let mut command = Command::new(exe);
    command
        .arg(subcommand)
        .arg("--install-dir")
        .arg(install)
        .env("DDG_STATE_DIR", &paths.state_dir)
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .env_remove("DDG_SESSION_TOKEN")
        .stdin(Stdio::null());
    command
}

async fn run_daemon(mut command: Command) -> Result<ApplyOutcome> {
    #[derive(serde::Deserialize)]
    struct HelperError {
        code: Code,
        message: String,
    }
    let output = command.output().await?;
    if output.status.success() {
        return Ok(serde_json::from_slice(&output.stdout)?);
    }
    if let Ok(error) = serde_json::from_slice::<HelperError>(&output.stderr) {
        return Err(Error::update(error.code, error.message));
    }
    Err(Error::update(
        Code::DaemonSwapFailedRollbackFailed,
        String::from_utf8_lossy(&output.stderr),
    ))
}

fn show_outcome(
    out: Output,
    host: &str,
    source: Option<(&RepoSlug, SourceOrigin)>,
    outcome: ApplyOutcome,
) -> Result {
    let summary = match &outcome {
        ApplyOutcome::Completed {
            from_version,
            to_version,
            tokens_preserved,
        } => format!(
            "updated {} → {} (tokens preserved: {tokens_preserved})",
            diagnostic(from_version),
            diagnostic(to_version)
        ),
        ApplyOutcome::HandedOffToHelper {
            to_version,
            helper_pid,
        } => format!("applying {} in helper {helper_pid}", diagnostic(to_version)),
        ApplyOutcome::PendingHolderRestart { .. } => {
            return require_restart(false, true);
        }
        ApplyOutcome::AutoRolledBack {
            attempted_version,
            restored_version,
            reason,
        } => {
            return Err(Error::update(
                Code::DaemonSwapFailedRolledBack,
                format!(
                    "Update {} rolled back to {}: {}",
                    diagnostic(attempted_version),
                    diagnostic(restored_version),
                    diagnostic(reason)
                ),
            ));
        }
    };
    out.show(
        &json!({"host": host, "result": outcome,
        "source": source.map(|(repo, origin)| source_json(repo, origin))}),
        || format!("{host}: {summary}\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_restart_requires_its_own_flag_and_non_tty_never_prompts() {
        for command in [Action::UpdateApply, Action::UpdateRollback] {
            for restart in [false, true] {
                let sub = if command == Action::UpdateApply {
                    "apply"
                } else {
                    "rollback"
                };
                let mut raw = vec!["update".to_owned(), sub.into(), "--yes".into()];
                if restart {
                    raw.push("--restart-sessions".into());
                }
                let args = crate::args::parse(&raw).unwrap();
                let result = require_restart(args.has("restart-sessions"), true);
                if restart {
                    result.unwrap();
                } else {
                    assert_eq!(result.unwrap_err().code, "holder_restart_required");
                }
            }
        }
        assert_eq!(
            confirmation(false, false, || panic!("non-TTY prompt"))
                .unwrap_err()
                .code,
            "confirmation_required"
        );
        assert!(confirmation(true, false, || panic!("explicit consent prompted")).is_ok());
    }
}
