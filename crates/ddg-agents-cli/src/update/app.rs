//! Signed bundles are exchanged as units. The host coordinator owns daemon
//! replacement and its independent N-1 set; never rewrite a signed bundle.

use super::*;
use ddg_agents_os::{lock::FileLock, process::spawn_detached};
use ddg_agents_update::{
    UpdateError,
    macos::{
        EXPECTED_APPLE_TEAM_ID, EXPECTED_BUNDLE_ID, SwapJob, macos_preflight_check, swap_macos_app,
        validate_helper_swap_job, verify_macos_app_bundle,
    },
    rollback::{RollbackRecord, restore_rollback_app, verify_rollback_set},
    state::{AutoRolledBackNotice, FailedVersionRecord, UpdateStateStore},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt, symlink},
    process::Command as StdCommand,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const JOB: &str = ".ddg-cli-job.json";
const APP_FILE: &str = "Previous.app";
const APP_RECORD: &str = "app-rollback.json";
const APP_DAEMONS: &str = "app-daemons";
const MAX_METADATA: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Deserialize, Serialize)]
struct CliConsent {
    allow_downgrade: bool,
    force: bool,
}

#[derive(Deserialize, Serialize)]
struct AppBackup {
    generation: String,
    target: PathBuf,
    record: RollbackRecord,
    daemon_swapped: bool,
}

pub(super) fn enclosing_app(path: &Path) -> Option<&Path> {
    path.ancestors().find(|p| {
        p.extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
    })
}

fn verify_bundle(path: &Path) -> Result {
    Ok(verify_macos_app_bundle(
        path,
        EXPECTED_APPLE_TEAM_ID,
        EXPECTED_BUNDLE_ID,
    )?)
}

pub(super) fn verified_bundle(stage: &Path, artifact: &ReleaseArtifact) -> Result<Option<PathBuf>> {
    let mut app = None;
    for binary in artifact.binaries.values() {
        if let Some(bundle) = enclosing_app(Path::new(&binary.path)) {
            if app.replace(bundle).is_some_and(|other| other != bundle) {
                return Err(Error::update(
                    Code::ArchiveUnsafe,
                    "Binaries refer to different app bundles",
                ));
            }
        } else if artifact.platform.starts_with("darwin-") {
            return Err(Error::update(
                Code::CodesignInvalid,
                "Darwin release binaries must be inside a signed bundle",
            ));
        }
    }
    let app = app.map(|path| stage.join(path));
    if let Some(app) = &app {
        verify_bundle(app)?;
    }
    Ok(app)
}

pub(super) fn handoff(
    paths: &UpdatePaths,
    stage: tempfile::TempDir,
    bundle: PathBuf,
    target: &Path,
    verified: &VerifiedRelease,
    tag: &ReleaseTag,
    args: &Args<'_>,
) -> Result<ApplyOutcome> {
    macos_preflight_check(target)?;
    let hashes = verified
        .artifact()
        .binaries
        .values()
        .map(|bin| {
            let path = stage.path().join(&bin.path);
            let relative = path.strip_prefix(&bundle).map_err(|_| {
                Error::update(Code::ArchiveUnsafe, "Binary is outside the staged bundle")
            })?;
            Ok((relative.to_string_lossy().into_owned(), bin.sha256.clone()))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let job = SwapJob {
        staging_app_path: bundle,
        target_app_path: target.into(),
        archive_path: stage.path().join("release.archive"),
        archive_sha256: verified.artifact().sha256.clone(),
        binary_sha256s: hashes,
    };
    let job_path = stage.path().join(JOB);
    write_atomic(&job_path, &serde_json::to_vec(&job)?)?;
    write_atomic(
        &stage.path().join("cli-consent.json"),
        &serde_json::to_vec(&CliConsent {
            allow_downgrade: args.has("allow-downgrade"),
            force: args.has("force"),
        })?,
    )?;
    let mut command = StdCommand::new(std::env::current_exe()?);
    command
        .arg("__update-apply-helper")
        .arg("--job")
        .arg(job_path)
        .arg("--state-dir")
        .arg(paths.state_dir.canonicalize()?)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if args.has("restart-sessions") {
        command.arg("--restart-sessions");
    }
    let helper_pid = spawn_detached(command)?.id();
    let _ = stage.keep();
    Ok(ApplyOutcome::HandedOffToHelper {
        helper_pid,
        to_version: tag.version().to_string(),
    })
}

async fn wait_for_exit(pid: i32) -> Result {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // SAFETY: pid is positive; signal 0 only checks process existence.
        if unsafe { libc::kill(pid, 0) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            if error.raw_os_error() != Some(libc::EPERM) {
                return Err(error.into());
            }
        }
        if Instant::now() >= deadline {
            return Err(Error::update(
                Code::LockBusy,
                "Parent did not exit within 10 seconds; no app was swapped",
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn lock_error(error: io::Error) -> UpdateError {
    let code = if error.kind() == io::ErrorKind::WouldBlock {
        Code::LockBusy
    } else {
        Code::InstallDirNotWritable
    };
    UpdateError::new(code, error.to_string())
}

fn storage(error: impl std::fmt::Display) -> UpdateError {
    UpdateError::new(Code::InstallDirNotWritable, error.to_string())
}

fn security(message: &str) -> Error {
    Error::update(Code::HostSecurityBlocked, message)
}

fn read_private(path: &Path) -> io::Result<Vec<u8>> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    // SAFETY: geteuid takes no arguments.
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.mode() & 0o7777 != 0o600
        || meta.uid() != unsafe { libc::geteuid() }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Expected owned single-link 0600 update metadata",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_METADATA + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_METADATA {
        return Err(io::Error::other("Update metadata is too large"));
    }
    Ok(bytes)
}

fn app_version(app: &Path, cli: &Path) -> Result<ReleaseTag> {
    let cli = app.join(cli);
    if cli.canonicalize()? != cli || !cli.symlink_metadata()?.is_file() {
        return Err(security("Bundle CLI is not a regular file"));
    }
    let output = StdCommand::new(cli)
        .arg("--version")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .stdin(Stdio::null())
        .output()?;
    let text = std::str::from_utf8(&output.stdout)
        .ok()
        .and_then(|text| text.trim().strip_prefix("ddg "));
    if let Some(text) = text.filter(|_| output.status.success()) {
        return Ok(ReleaseTag::parse(text)?);
    }
    Err(security("Signed bundle did not report a DDG version"))
}

fn backup_root(state: &Path) -> PathBuf {
    state.join("rollback/apps")
}

fn load_backup(state: &Path) -> Result<AppBackup> {
    let missing = || {
        Error::update(
            Code::RollbackUnavailable,
            "No verified N-1 app backup recorded",
        )
    };
    let bytes = match read_private(&state.join(APP_RECORD)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(missing()),
        Err(error) => return Err(error.into()),
    };
    let backup: AppBackup = serde_json::from_slice(&bytes)?;
    if !backup.generation.starts_with(".ddg-app-")
        || !backup
            .generation
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
    {
        return Err(Error::update(
            Code::RollbackVerificationFailed,
            "Invalid app backup generation",
        ));
    }
    Ok(backup)
}

pub(super) fn rollback_version(state: &Path) -> Option<String> {
    let backup = load_backup(state).ok()?;
    Some(backup.record.version)
}

fn save_backup(state: &Path, backup: &AppBackup) -> Result {
    write_atomic(&state.join(APP_RECORD), &serde_json::to_vec(backup)?)?;
    Ok(())
}

fn take_digests(
    target: &Path,
    binaries: impl Iterator<Item = String>,
) -> Result<BTreeMap<String, String>> {
    binaries
        .map(|relative| {
            let path = target.join(&relative);
            if !path.starts_with(target)
                || path.canonicalize()? != path
                || !path.symlink_metadata()?.is_file()
            {
                return Err(security("Backup requires canonical bundle binaries"));
            }
            let mut hash = Sha256::new();
            io::copy(&mut File::open(path)?, &mut hash)?;
            let digest = format!("{:x}", hash.finalize());
            Ok((format!("{APP_FILE}/{relative}"), digest))
        })
        .collect()
}

fn snapshot(
    target: &Path,
    stage: &Path,
    version: &ReleaseTag,
    cli: &Path,
    binaries: impl Iterator<Item = String>,
    daemon_swapped: bool,
) -> Result<AppBackup> {
    let binary_sha256s = take_digests(target, binaries)?;
    // The installed app has no retained release ZIP. Pin its signed CLI as the
    // rollback-set anchor, plus every recorded binary and the full code signature.
    let archive_rel_path = format!("{APP_FILE}/{}", cli.display());
    let archive_sha256 = binary_sha256s
        .get(&archive_rel_path)
        .ok_or_else(|| security("App backup omits the CLI"))?
        .clone();
    Ok(AppBackup {
        generation: stage.file_name().unwrap().to_string_lossy().into_owned(),
        target: target.into(),
        daemon_swapped,
        record: RollbackRecord {
            version: version.version().to_string(),
            platform: local_platform().unwrap_or_default().into(),
            recorded_at: timestamp(),
            archive_rel_path,
            archive_sha256,
            binary_sha256s,
            catalog_schema_version: ddg_agents_proto::host::CATALOG_SCHEMA_VERSION,
            hosts_schema_version: ddg_agents_transport::profile::HOSTS_SCHEMA_VERSION,
            holder_protocol_version: ddg_agents_proto::holder::HOLDER_PROTO_VERSION,
        },
    })
}

fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

struct Link {
    path: PathBuf,
    previous: PathBuf,
    next: PathBuf,
}

fn bundle_links(
    target: &Path,
    artifact: Option<&ReleaseArtifact>,
    stage: &Path,
) -> Result<Vec<Link>> {
    let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
        return Ok(Vec::new());
    };
    let bin = PathBuf::from(home).join(".local/bin");
    let mut links = Vec::new();
    for name in [CLI, HOST, CORE] {
        let path = bin.join(name);
        let Ok(resolved) = path.canonicalize() else {
            continue;
        };
        if !resolved.starts_with(target) {
            continue;
        }
        let Ok(previous) = fs::read_link(&path) else {
            continue;
        };
        let Some(binary) = artifact.and_then(|art| art.binaries.get(name)) else {
            continue;
        };
        let staged = stage.join(&binary.path);
        let app =
            enclosing_app(&staged).ok_or_else(|| security("Expected a signed bundle binary"))?;
        let next = target.join(staged.strip_prefix(app).expect("enclosing bundle"));
        if resolved != next {
            links.push(Link {
                path,
                previous,
                next,
            });
        }
    }
    Ok(links)
}

fn relink(links: &[Link], undo: bool) -> Result {
    for link in links {
        let (expected, replacement) = if undo {
            (&link.next, &link.previous)
        } else {
            (&link.previous, &link.next)
        };
        if fs::read_link(&link.path)? != *expected {
            continue;
        }
        let temp = tempfile::tempdir_in(link.path.parent().expect("bin directory"))?;
        let symlink_path = temp.path().join("link");
        symlink(replacement, &symlink_path)?;
        fs::rename(symlink_path, &link.path)?;
    }
    Ok(())
}

fn restore(state: &Path, backup: &AppBackup) -> Result {
    restore_rollback_app(
        &backup_root(state).join(&backup.generation),
        &backup.record,
        APP_FILE,
        &backup.target,
    )?;
    Ok(())
}

fn relaunch(target: &Path) -> Result {
    if !StdCommand::new("/usr/bin/open")
        .arg("-n")
        .arg(target)
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .stdin(Stdio::null())
        .output()?
        .status
        .success()
    {
        return Err(Error::update(
            Code::DaemonSwapFailedRollbackFailed,
            "Could not relaunch the DDG app",
        ));
    }
    Ok(())
}

struct RelaunchOnFailure<'a>(Option<&'a Path>);

impl Drop for RelaunchOnFailure<'_> {
    fn drop(&mut self) {
        if let Some(target) = self.0
            && verify_bundle(target).is_ok()
        {
            let _ = relaunch(target);
        }
    }
}

// A GUI job with a live runtime must retain the verified original manifest
// beside the archive, with binary paths relative to that same staging root.
fn verified_manifest(
    stage: &Path,
    job: &SwapJob,
    tag: &ReleaseTag,
) -> Result<Option<(ReleaseManifest, String)>> {
    let raw = match read_private(&stage.join("manifest.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let signature = if RELEASE_PUBKEYS.is_empty() {
        None
    } else {
        Some(read_private(&stage.join("manifest.json.sig"))?)
    };
    #[derive(Deserialize)]
    struct ManifestTag {
        tag: String,
    }
    let reported: ManifestTag = serde_json::from_slice(&raw)?;
    let expected = ReleaseTag::parse(&reported.tag)?;
    if tag.version() != expected.version() {
        return Err(security(
            "Signed app version does not match the staged manifest",
        ));
    }
    let manifest = verify_manifest(&raw, signature.as_deref(), &expected)?;
    let asset = selected_artifact(&manifest)?;
    if manifest.protocol_version != PROTOCOL_VERSION
        || asset.sha256 != job.archive_sha256
        || asset.binaries.values().any(|bin| {
            let staged = stage.join(&bin.path);
            staged
                .strip_prefix(&job.staging_app_path)
                .ok()
                .and_then(Path::to_str)
                .and_then(|relative| job.binary_sha256s.get(relative))
                != Some(&bin.sha256)
        })
    {
        return Err(security(
            "Daemon manifest does not match the verified app job",
        ));
    }
    Ok(Some((manifest, format!("{:x}", Sha256::digest(&raw)))))
}

fn selected_artifact(manifest: &ReleaseManifest) -> Result<&ReleaseArtifact> {
    manifest
        .artifacts
        .iter()
        .find(|artifact| Some(artifact.platform.as_str()) == local_platform())
        .filter(|asset| {
            [HOST, CORE]
                .into_iter()
                .all(|name| asset.binaries.contains_key(name))
        })
        .ok_or_else(|| {
            Error::update(
                Code::AssetNotFound,
                "No local daemon artifact in the verified manifest",
            )
        })
}

fn app_daemons(
    paths: &UpdatePaths,
    old_bundle: &Path,
    stage: &Path,
    artifact: &ReleaseArtifact,
) -> Result<PathBuf> {
    let install = paths.state_dir.join(APP_DAEMONS);
    ensure_private_dir(&install)?;
    for name in [HOST, CORE] {
        let staged = stage.join(&artifact.binaries[name].path);
        let relative = staged
            .strip_prefix(
                enclosing_app(&staged)
                    .ok_or_else(|| security("Daemon must be in the signed bundle"))?,
            )
            .expect("enclosing bundle");
        let source = old_bundle.join(relative);
        let dest = install.join(name);
        match dest.symlink_metadata() {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if source.canonicalize()? != source {
                    return Err(security("Old daemon is not inside the verified bundle"));
                }
                let temp = tempfile::NamedTempFile::new_in(&install)?;
                fs::copy(source, temp.path())?;
                temp.as_file().sync_all()?;
                temp.persist_noclobber(dest).map_err(|error| error.error)?;
            }
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => return Err(security("Managed daemon path is not a regular file")),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(install)
}

async fn daemon_rollback(paths: &UpdatePaths, run: &Path, restart: bool) -> Result<ApplyOutcome> {
    let install = paths.state_dir.join(APP_DAEMONS);
    let mut command = daemon_command(
        &install.join(HOST),
        paths,
        &install,
        "__rollback-daemon-swap",
    );
    command.env("DDG_RUN_DIR", run);
    if restart {
        command.arg("--restart-sessions");
    }
    run_daemon(command).await
}

fn active_runtime(run: &Path) -> Result<bool> {
    use std::os::unix::fs::FileTypeExt;
    for name in ["host.sock", "terminal.sock", "holder.sock"] {
        match run.join(name).symlink_metadata() {
            Ok(info) if info.file_type().is_socket() => return Ok(true),
            Ok(_) => return Err(security("DDG runtime entry is not a socket")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

pub(super) async fn run_helper(args: &Args<'_>) -> Result<ApplyOutcome> {
    if let Some(pid) = args.number("parent-pid")? {
        wait_for_exit(pid).await?;
    }
    let state_dir = PathBuf::from(args.value("state-dir").expect("validated state directory"))
        .canonicalize()?;
    ensure_private_dir(&state_dir)?;
    let _app_lock =
        FileLock::try_lock_exclusive(&state_dir.join("app-update.lock")).map_err(lock_error)?;
    let store = UpdateStateStore::new(&state_dir);
    let paths = UpdatePaths {
        state_dir: state_dir.clone(),
        ..UpdatePaths::from_environment()?
    };
    let restart = args.has("restart-sessions");
    let should_open = args.has("relaunch");
    let requested = Path::new(args.value("job").expect("validated job path"));
    let staging_root = state_dir.join("staging");
    ensure_private_dir(&staging_root)?;
    // Claim the job under the same private state root before reading its fields.
    let claimed = tempfile::tempdir_in(&state_dir)?;
    let job_path = claimed.path().join("job.json");
    let root_request = Path::new(args.value("state-dir").unwrap());
    let relative = requested
        .strip_prefix(root_request)
        .or_else(|_| requested.strip_prefix(&state_dir))
        .map_err(|_| security("Helper job is outside the state directory"))?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err(security("Invalid helper job path"));
    }
    let original = state_dir.join(relative);
    if original
        .parent()
        .is_none_or(|parent| parent.canonicalize().ok().as_deref() != Some(parent))
    {
        return Err(security("Helper job parent is not canonical"));
    }
    let state_lock =
        FileLock::try_lock_exclusive(&state_dir.join("update.lock")).map_err(lock_error)?;
    fs::rename(&original, &job_path)?;
    let mut job: SwapJob = serde_json::from_slice(&read_private(&job_path)?)?;
    let checked = validate_helper_swap_job(
        &job_path,
        &state_dir,
        EXPECTED_APPLE_TEAM_ID,
        EXPECTED_BUNDLE_ID,
    )?;
    job.target_app_path = job.target_app_path.canonicalize()?;
    job.staging_app_path = job.staging_app_path.canonicalize()?;
    job.archive_path = job.archive_path.canonicalize()?;
    let target = &job.target_app_path;
    let stage = job.archive_path.parent().expect("validated archive path");
    let cli_stage = original == stage.join(JOB)
        && stage.parent() == Some(staging_root.as_path())
        && stage
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(STAGING_PREFIX));
    let consent = if cli_stage {
        serde_json::from_slice(&read_private(&stage.join("cli-consent.json"))?)?
    } else {
        // The desktop launches this one-shot helper after an explicit Apply.
        CliConsent {
            allow_downgrade: false,
            force: true,
        }
    };
    let _cleanup = cli_stage.then(|| StagingCleanup(stage.to_owned()));
    verify_bundle(target)?;
    let mut reopen = RelaunchOnFailure(should_open.then_some(target));
    let cli = job
        .binary_sha256s
        .keys()
        .find(|key| Path::new(key).file_name().is_some_and(|name| name == CLI))
        .map(Path::new)
        .ok_or_else(|| security("App job omits the CLI digest"))?;
    let old = app_version(target, cli)?;
    let new = app_version(&job.staging_app_path, cli)?;
    let manifest = verified_manifest(stage, &job, &new)?;
    validate_apply_consent(
        old.version(),
        new.version(),
        manifest
            .as_ref()
            .is_none_or(|(manifest, _)| manifest.min_compatible_schema <= 1),
        false,
        0,
        consent.allow_downgrade,
        restart,
    )?;
    // A manifest tag can include a v-prefix while --version reports normalized SemVer.
    let tag = manifest.as_ref().map_or(Ok(new.clone()), |(manifest, _)| {
        ReleaseTag::parse(&manifest.tag)
    })?;
    let run_dir = if std::env::var_os("DDG_RUN_DIR").is_some()
        || std::env::var_os("XDG_RUNTIME_DIR").is_some()
    {
        ddg_agents_os::paths::run_dir()?
    } else {
        state_dir.clone()
    };
    let runtime = active_runtime(&run_dir)?;
    if runtime && manifest.is_none() {
        return Err(security(
            "Live daemon replacement requires a verified staged manifest",
        ));
    }
    let daemon = manifest
        .as_ref()
        .filter(|_| runtime)
        .map(|(manifest, digest)| {
            let artifact = selected_artifact(manifest)?;
            Ok::<_, Error>((
                app_daemons(&paths, target, stage, artifact)?,
                artifact,
                digest.clone(),
            ))
        })
        .transpose()?;
    let links = bundle_links(
        target,
        manifest
            .as_ref()
            .map(|(manifest, _)| selected_artifact(manifest))
            .transpose()?,
        stage,
    )?;
    let root = backup_root(&state_dir);
    ensure_private_dir(&root)?;
    let previous = load_backup(&state_dir).ok();
    let backup_dir = tempfile::Builder::new()
        .prefix(".ddg-app-")
        .tempdir_in(&root)?;
    let backup = snapshot(
        target,
        backup_dir.path(),
        &old,
        cli,
        job.binary_sha256s.keys().cloned(),
        daemon.is_some(),
    )?;
    // Use the state store for record mutations. Reacquire the lock before any
    // swap, and let the host helper take it independently for daemon handoff.
    drop(state_lock);
    store.update(|state| {
        if state.pending_holder_restart.is_some() && !restart {
            return Err(UpdateError::new(
                Code::HolderRestartRequired,
                "Use --restart-sessions to allow Holder restart",
            ));
        }
        if state.in_progress.is_some() {
            return Err(UpdateError::new(
                Code::DaemonSwapFailedRollbackFailed,
                "Recover the interrupted daemon swap before replacing the app",
            ));
        }
        state.should_attempt(tag.as_str(), consent.force)
    })?;
    let state_lock =
        FileLock::try_lock_exclusive(&state_dir.join("update.lock")).map_err(lock_error)?;
    write_atomic(
        &backup_dir.path().join(APP_RECORD),
        &serde_json::to_vec(&backup)?,
    )?;
    if let Err(error) = swap_macos_app(&checked, &backup_dir.path().join(APP_FILE)) {
        if error.code == Code::DaemonSwapFailedRollbackFailed {
            // The bundle helper could not prove restoration: retain the N-1 set.
            let _ = backup_dir.keep();
        }
        return Err(error.into());
    }
    let backup_path = backup_dir.keep();
    drop(state_lock);
    let mut daemon_completed = false;
    let mut tokens_preserved = !runtime;
    let finish = async {
        relink(&links, false)?;
        if let Some((install, artifact, digest)) = daemon {
            let mut command = daemon_apply(&paths, &install, stage, artifact, &tag, &digest);
            command.env("DDG_RUN_DIR", &run_dir);
            for (enabled, flag) in [
                (restart, "--restart-sessions"),
                (consent.allow_downgrade, "--allow-downgrade"),
                (consent.force, "--force"),
            ] {
                if enabled {
                    command.arg(flag);
                }
            }
            match run_daemon(command).await? {
                ApplyOutcome::Completed {
                    tokens_preserved: preserved,
                    ..
                } => {
                    daemon_completed = true;
                    tokens_preserved = preserved;
                }
                outcome => return show_unfinished(outcome),
            }
        }
        save_backup(&state_dir, &backup)?;
        store.update(|state| {
            state.record_success(tag.as_str());
            state.auto_rolled_back = None;
            Ok(())
        })?;
        if should_open {
            relaunch(target)?;
            reopen.0 = None;
        }
        Ok(())
    }
    .await;
    if let Err(error) = finish {
        let recover = async {
            if daemon_completed {
                expect_completed(daemon_rollback(&paths, &run_dir, restart).await?)?;
            }
            let lock =
                FileLock::try_lock_exclusive(&state_dir.join("update.lock")).map_err(lock_error)?;
            restore(&state_dir, &backup)?;
            relink(&links, true)?;
            if let Some(previous) = &previous {
                save_backup(&state_dir, previous)?;
            } else if state_dir.join(APP_RECORD).exists() {
                fs::remove_file(state_dir.join(APP_RECORD))?;
            }
            drop(lock);
            store.update(|state| {
                if error.code != "holder_restart_required" {
                    state.record_success(old.as_str());
                    state.failed_versions.insert(
                        tag.version().to_string(),
                        FailedVersionRecord {
                            failed_at: timestamp(),
                            reason: error.message.clone(),
                        },
                    );
                }
                state.auto_rolled_back = Some(AutoRolledBackNotice {
                    attempted_version: tag.version().to_string(),
                    restored_version: old.version().to_string(),
                    reason: error.message.clone(),
                });
                Ok(())
            })?;
            if should_open {
                relaunch(target)?;
                reopen.0 = None;
            }
            Ok::<_, Error>(())
        }
        .await;
        if let Err(recovery) = recover {
            return Err(Error::update(
                Code::DaemonSwapFailedRollbackFailed,
                format!(
                    "{}; app recovery failed: {}",
                    error.message, recovery.message
                ),
            ));
        }
        let _ = fs::remove_dir_all(backup_path);
        if error.code == "holder_restart_required" {
            return Err(error);
        }
        return Ok(ApplyOutcome::AutoRolledBack {
            attempted_version: tag.version().to_string(),
            restored_version: old.version().to_string(),
            reason: error.message,
        });
    }
    if let Some(previous) = previous {
        let _ = fs::remove_dir_all(root.join(previous.generation));
    }
    Ok(ApplyOutcome::Completed {
        from_version: old.version().to_string(),
        to_version: tag.version().to_string(),
        tokens_preserved,
    })
}

fn show_unfinished(outcome: ApplyOutcome) -> Result {
    match outcome {
        ApplyOutcome::PendingHolderRestart { .. } => require_restart(false, true),
        ApplyOutcome::AutoRolledBack { reason, .. } => {
            Err(Error::update(Code::DaemonSwapFailedRolledBack, reason))
        }
        _ => Err(security("Unexpected daemon swap outcome")),
    }
}

fn expect_completed(outcome: ApplyOutcome) -> Result {
    if matches!(outcome, ApplyOutcome::Completed { .. }) {
        Ok(())
    } else {
        show_unfinished(outcome)
    }
}

struct StagingCleanup(PathBuf);
impl Drop for StagingCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) async fn rollback(
    paths: &UpdatePaths,
    target: &Path,
    restart: bool,
) -> Result<ApplyOutcome> {
    let state = &paths.state_dir;
    let _app_lock =
        FileLock::try_lock_exclusive(&state.join("app-update.lock")).map_err(lock_error)?;
    let backup = load_backup(state)?;
    if backup.target != target
        || backup.record.catalog_schema_version != ddg_agents_proto::host::CATALOG_SCHEMA_VERSION
        || backup.record.hosts_schema_version != ddg_agents_transport::profile::HOSTS_SCHEMA_VERSION
    {
        return Err(Error::update(
            Code::SchemaIncompatibleDowngrade,
            "Backup does not match the app install or current schema",
        ));
    }
    verify_rollback_set(&backup_root(state).join(&backup.generation), &backup.record)?;
    verify_bundle(target)?;
    let cli = Path::new(&backup.record.archive_rel_path)
        .strip_prefix(APP_FILE)
        .map_err(|_| security("App rollback CLI path is invalid"))?;
    let current = app_version(target, cli)?;
    let store = UpdateStateStore::new(state);
    let run = ddg_agents_os::paths::run_dir()?;
    let mut tokens_preserved = true;
    if backup.daemon_swapped {
        let daemon = store.load()?.rollback.ok_or_else(|| {
            Error::update(
                Code::RollbackUnavailable,
                "App daemon backup is unavailable",
            )
        })?;
        if ReleaseTag::parse(&daemon.version)?.version()
            != ReleaseTag::parse(&backup.record.version)?.version()
        {
            return Err(Error::update(
                Code::RollbackVerificationFailed,
                "App and daemon backups have different versions",
            ));
        }
        match daemon_rollback(paths, &run, restart).await? {
            ApplyOutcome::Completed {
                tokens_preserved: preserved,
                ..
            } => tokens_preserved = preserved,
            outcome => {
                show_unfinished(outcome)?;
                unreachable!();
            }
        }
    }
    let mut restored = false;
    let result = store.update(|state| {
        restore_rollback_app(
            &backup_root(&paths.state_dir).join(&backup.generation),
            &backup.record,
            APP_FILE,
            target,
        )?;
        restored = true;
        state.record_success(&backup.record.version);
        state.auto_rolled_back = None;
        Ok(())
    });
    if let Err(error) = result {
        if backup.daemon_swapped && !restored {
            expect_completed(daemon_rollback(paths, &run, restart).await?).map_err(|recovery| {
                Error::update(
                    Code::DaemonSwapFailedRollbackFailed,
                    format!("{error}; daemon recovery failed: {}", recovery.message),
                )
            })?;
        }
        return Err(error.into());
    }
    fs::remove_file(state.join(APP_RECORD)).map_err(storage)?;
    let _ = fs::remove_dir_all(backup_root(state).join(&backup.generation));
    Ok(ApplyOutcome::Completed {
        from_version: current.version().to_string(),
        to_version: backup.record.version,
        tokens_preserved,
    })
}
