//! Local update coordinator. This module is deliberately network-free.

use crate::paths::{self, Paths};
use ddg_agents_os::{
    fs::{ensure_private_dir, write_atomic},
    ipc::{LocalStream, connect, peer_credentials, peer_is_current_user},
    lock::FileLock,
    process::spawn_detached,
};
use ddg_agents_proto::{
    holder,
    rpc::{Request, Response},
    terminal::{HelloResult, PROTOCOL_VERSION, PtyListResult},
};
use ddg_agents_update::{
    ApplyOutcome, UpdateError, UpdateErrorCode as Code,
    integrity::{RELEASE_PUBKEYS, verify_sha256},
    manifest::ReleaseManifest,
    rollback::{RollbackRecord, verify_rollback_set},
    state::{
        AutoRolledBackNotice, FailedVersionRecord, InProgressSwap, PendingHolderRestart,
        UpdateState, UpdateStateStore,
    },
    version::{ReleaseTag, validate_apply_consent},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[cfg(unix)]
use ddg_agents_update::rollback::restore_rollback_binary;
#[cfg(unix)]
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
};

type Result<T> = std::result::Result<T, UpdateError>;
#[cfg(unix)]
const HOST: &str = "ddg-agents-host";
#[cfg(windows)]
const HOST: &str = "ddg-agents-host.exe";
#[cfg(unix)]
const CORE: &str = "ddg-terminal-server";
#[cfg(windows)]
const CORE: &str = "ddg-terminal-server.exe";
#[cfg(unix)]
const CLI: &str = "ddg";
#[cfg(windows)]
const CLI: &str = "ddg.exe";
const LEGACY: &str = "0.1.0";
const WAIT: Duration = Duration::from_secs(5);

pub fn platform() -> String {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64".into(),
        ("macos", arch) => format!("darwin-{arch}"),
        (os, arch) => format!("{os}-{arch}"),
    }
}

pub fn rollback_version(state_dir: &Path) -> Option<String> {
    let record = UpdateStateStore::new(state_dir).load().ok()?.rollback?;
    verify_rollback_set(&state_dir.join("rollback/prev"), &record).ok()?;
    Some(record.version)
}

pub fn run(args: &[String]) {
    match Options::parse(args).and_then(coordinate) {
        Ok(outcome) => println!(
            "{}",
            serde_json::to_string(&outcome).expect("serializable outcome")
        ),
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::to_string(&error).expect("serializable update error")
            );
            std::process::exit(1);
        }
    }
}

#[derive(Default)]
struct Options {
    install: PathBuf,
    staging: Option<PathBuf>,
    tag: Option<ReleaseTag>,
    digest: Option<String>,
    downgrade: bool,
    restart: bool,
    force: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        let apply = args[0] == "__apply-daemon-swap";
        let invalid = || UpdateError::new(Code::ManifestInvalid, "invalid daemon swap arguments");
        let mut options = Self::default();
        let mut seen = std::collections::HashSet::new();
        let mut args = args[1..].iter().map(String::as_str);
        while let Some(flag) = args.next() {
            if !seen.insert(flag) {
                return Err(invalid());
            }
            let mut value = || {
                args.next()
                    .filter(|v| !v.is_empty() && !v.starts_with("--"))
                    .ok_or_else(invalid)
            };
            match flag {
                "--install-dir" => options.install = PathBuf::from(value()?),
                "--staging-dir" if apply => options.staging = Some(PathBuf::from(value()?)),
                "--version" if apply => options.tag = Some(ReleaseTag::parse(value()?)?),
                "--manifest-sha256" if apply => options.digest = Some(value()?.into()),
                "--allow-downgrade" if apply => options.downgrade = true,
                "--force" if apply => options.force = true,
                "--restart-sessions" => options.restart = true,
                _ => return Err(invalid()),
            }
        }
        if options.install.as_os_str().is_empty()
            || (apply && (options.tag.is_none() || options.staging.is_none()))
        {
            return Err(invalid());
        }
        options.install = options.install.canonicalize().map_err(storage)?;
        Ok(options)
    }
}

fn storage(error: impl std::fmt::Display) -> UpdateError {
    UpdateError::new(Code::InstallDirNotWritable, error.to_string())
}

fn failed(error: impl std::fmt::Display) -> UpdateError {
    UpdateError::new(Code::DaemonSwapFailedRollbackFailed, error.to_string())
}

fn coordinate(opts: Options) -> Result<ApplyOutcome> {
    let paths = Paths::from_env().map_err(storage)?;
    UpdateStateStore::new(&paths.state_dir).update(|state| transaction(&opts, &paths, state))?
}

// Inner errors are committed to the state store when recovery itself fails.
fn transaction(
    opts: &Options,
    paths: &Paths,
    state: &mut UpdateState,
) -> Result<Result<ApplyOutcome>> {
    if state.in_progress.is_some() && opts.tag.is_some() {
        return Err(failed(
            "an interrupted daemon swap needs recovery before another update",
        ));
    }
    #[cfg(windows)]
    let _ = ddg_agents_os::replace::gc_stale_tombstones(&opts.install);
    let prev = paths.state_dir.join("rollback/prev");
    let mode = host_mode(paths).map_err(failed)?;
    let host_socket = &paths.host_sock;
    let terminal = probe_terminal(paths).map_err(failed)?;
    let current = match mode {
        HostMode::Legacy(_) => {
            if hello(host_socket).map_err(failed)?["app_version"].is_string() {
                return Err(failed(
                    "a versioned Host is missing its admin socket; refusing legacy replacement",
                ));
            }
            ReleaseTag::parse(LEGACY)?
        }
        HostMode::Direct => {
            let hello = hello(host_socket).map_err(failed)?;
            ReleaseTag::parse(
                hello["app_version"]
                    .as_str()
                    .ok_or_else(|| failed("current Host version missing"))?,
            )?
        }
        HostMode::Stopped => ReleaseTag::parse(&state.current_version)?,
    };
    let from = current.version().to_string();
    let old_holder = terminal
        .as_ref()
        .and_then(|t| t.holder_protocol_version)
        .unwrap_or(holder::HOLDER_PROTO_VERSION);
    let active = match &terminal {
        Some(t) if t.active_pty_count.is_some() => t.active_pty_count.unwrap(),
        Some(_) => serde_json::from_value::<PtyListResult>(
            rpc(&paths.terminal_sock, "pty.list", json!({})).map_err(failed)?,
        )
        .map_err(failed)?
        .ptys
        .len() as u32,
        None => 0,
    };
    let mut rollback_legacy = false;
    let mut installed_record = None;
    let target = if let Some(tag) = &opts.tag {
        state.should_attempt(tag.as_str(), opts.force)?;
        let staging = opts
            .staging
            .as_ref()
            .expect("parsed staging directory")
            .canonicalize()
            .map_err(storage)?;
        let manifest = read_manifest(&staging, tag, opts.digest.as_deref())?;
        let artifact = manifest
            .artifacts
            .iter()
            .find(|asset| asset.platform == platform())
            .ok_or_else(|| {
                UpdateError::new(Code::AssetNotFound, "no release artifact for this platform")
            })?;
        let mut bins = Vec::new();
        for (name, binary) in &artifact.binaries {
            let src = staging.join(&binary.path);
            if src.canonicalize().map_err(storage)? != src {
                return Err(UpdateError::new(
                    Code::ArchiveUnsafe,
                    "staged binary path is not canonical",
                ));
            }
            verify_sha256(open_regular(&src).map_err(storage)?, &binary.sha256)?;
            let installed = [HOST, CORE, CLI].into_iter().find(|&bin| {
                name == bin || (cfg!(windows) && bin.strip_suffix(".exe") == Some(name.as_str()))
            });
            if let Some(bin) = installed
                && (bin != CLI || opts.install.join(CLI).exists())
            {
                if bins.iter().any(|(name, _, _)| name == bin) {
                    return Err(UpdateError::new(
                        Code::ManifestInvalid,
                        "manifest lists the same executable twice",
                    ));
                }
                bins.push((bin.to_owned(), src, binary.sha256.clone()));
            }
        }
        if !bins.iter().any(|(name, _, _)| name == HOST)
            || !bins.iter().any(|(name, _, _)| name == CORE)
        {
            return Err(UpdateError::new(
                Code::ManifestInvalid,
                "manifest omits a required daemon",
            ));
        }
        Target {
            version: tag.clone(),
            bins,
            schema_ok: manifest.min_compatible_schema <= 1,
            restart_holder: manifest.holder_restart_required
                || manifest.holder_protocol_version != old_holder,
            holder_protocol: manifest.holder_protocol_version,
        }
    } else {
        let record = state.rollback.as_ref().ok_or_else(|| {
            UpdateError::new(
                Code::RollbackUnavailable,
                "no verified N-1 rollback recorded",
            )
        })?;
        verify_rollback_set(&prev, record)?;
        if record.platform != platform()
            || !record.binary_sha256s.contains_key(HOST)
            || !record.binary_sha256s.contains_key(CORE)
        {
            return Err(UpdateError::new(
                Code::RollbackVerificationFailed,
                "rollback set is not for these daemons",
            ));
        }
        rollback_legacy = record.archive_rel_path == "legacy-host";
        let bins = record
            .binary_sha256s
            .iter()
            .map(|(name, digest)| (name.clone(), prev.join(name), digest.clone()))
            .collect::<Vec<_>>();
        if bins
            .iter()
            .any(|(name, _, _)| ![HOST, CORE, CLI].contains(&name.as_str()))
        {
            return Err(UpdateError::new(
                Code::RollbackVerificationFailed,
                "unexpected rollback binary",
            ));
        }
        installed_record = Some(record.clone());
        Target {
            version: ReleaseTag::parse(&record.version)?,
            bins,
            schema_ok: record.catalog_schema_version == 1 && record.hosts_schema_version == 1,
            restart_holder: record.holder_protocol_version != old_holder,
            holder_protocol: record.holder_protocol_version,
        }
    };
    let to = target.version.version().to_string();
    if let Err(err) = validate_apply_consent(
        current.version(),
        target.version.version(),
        target.schema_ok,
        target.restart_holder,
        active,
        opts.downgrade || opts.tag.is_none(),
        opts.restart,
    ) {
        if opts.tag.is_some() && err.code == Code::HolderRestartRequired {
            state.pending_holder_restart = Some(PendingHolderRestart {
                to_version: to.clone(),
                active_pty_count: active,
            });
            return Ok(Ok(ApplyOutcome::PendingHolderRestart {
                to_version: to,
                active_pty_count: active,
            }));
        }
        return Err(err);
    }
    let old_instance = terminal.as_ref().and_then(|t| t.holder_instance.as_deref());
    let holder_pid = socket_pid(&paths.run_dir.join("holder.sock")).map_err(failed)?;
    if terminal.is_none() && holder_pid.is_some() {
        return Err(failed(
            "Holder is live without a Core; cannot verify active sessions",
        ));
    }
    let old_core_legacy = terminal.as_ref().is_some_and(|t| t.app_version.is_none());
    let names: Vec<_> = target
        .bins
        .iter()
        .map(|(name, _, _)| name.as_str())
        .collect();
    let backup = Backup::create(
        &prev,
        &opts.install,
        &names,
        &from,
        old_holder,
        matches!(mode, HostMode::Legacy(_)),
    )?;
    // Manual rollback sources must remain available when the current N is put in prev.
    let mut backup = backup;
    let result = (|| {
        let _terminal_start =
            FileLock::try_lock_exclusive(&paths.state_dir.join("terminal-start.lock"))
                .map_err(failed)?;
        let mut swap = Swap::new(paths, &opts.install, mode);
        swap.prepare()?;
        let result: Result<()> = (|| {
            backup.publish()?;
            state.in_progress = Some(InProgressSwap {
                from_version: from.clone(),
                to_version: to.clone(),
            });
            let mut journal = state.clone();
            journal.rollback = Some(backup.record.clone());
            journal.current_version.clone_from(&from);
            write_atomic(
                &paths.state_dir.join("update-state.json"),
                &serde_json::to_vec(&journal).map_err(storage)?,
            )
            .map_err(storage)?;
            for (name, src, digest) in &target.bins {
                let dest = opts.install.join(name);
                if let Some(record) = &installed_record {
                    // Recheck the manually selected source after it moved aside.
                    restore_rollback_binary(&backup.prior, record, name, &dest)?;
                } else {
                    copy_binary(src, &dest, Some(digest))?;
                }
            }
            swap.replace_core(target.restart_holder, opts.restart, old_holder)?;
            let expected_holder = (!target.restart_holder).then_some((old_instance, holder_pid));
            core_health(
                paths,
                &to,
                target.holder_protocol,
                expected_holder,
                rollback_legacy,
            )
            .map_err(failed)?;
            swap.replace_host(rollback_legacy, &to)?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                state.current_version.clone_from(&from);
                state.record_success(&to);
                state.previous_version = Some(from.clone());
                state.rollback = Some(backup.record.clone());
                state.auto_rolled_back = None;
                backup.commit();
                Ok(ApplyOutcome::Completed {
                    from_version: from.clone(),
                    to_version: to.clone(),
                    tokens_preserved: mode == HostMode::Direct
                        && !rollback_legacy
                        && !target.restart_holder,
                })
            }
            Err(error) => {
                let recovery: Result<()> = (|| {
                    if backup.published {
                        verify_rollback_set(&prev, &backup.record)?;
                        for name in &names {
                            restore_rollback_binary(
                                &prev,
                                &backup.record,
                                name,
                                &opts.install.join(name),
                            )?;
                        }
                    }
                    swap.recover(&from, old_holder, old_core_legacy, old_instance, holder_pid)?;
                    backup.restore_prior()?;
                    Ok(())
                })();
                let reason = error.message().to_owned();
                state.failed_versions.insert(
                    to.clone(),
                    FailedVersionRecord {
                        failed_at: paths::unix_secs(),
                        reason: reason.clone(),
                    },
                );
                if let Err(recovery) = recovery {
                    if backup.published {
                        state.rollback = Some(backup.record.clone());
                    }
                    return Err(failed(format!("{reason}; recovery failed: {recovery}")));
                }
                state.current_version.clone_from(&from);
                state.in_progress = None;
                state.pending_holder_restart = None;
                state.auto_rolled_back = Some(AutoRolledBackNotice {
                    attempted_version: to.clone(),
                    restored_version: from.clone(),
                    reason: reason.clone(),
                });
                Ok(ApplyOutcome::AutoRolledBack {
                    attempted_version: to.clone(),
                    restored_version: from.clone(),
                    reason,
                })
            }
        }
    })();
    Ok(result)
}

struct Target {
    version: ReleaseTag,
    bins: Vec<(String, PathBuf, String)>,
    schema_ok: bool,
    restart_holder: bool,
    holder_protocol: u32,
}

fn read_manifest(
    staging: &Path,
    tag: &ReleaseTag,
    digest: Option<&str>,
) -> Result<ReleaseManifest> {
    // The network-free helper must fail closed when the pinned-key rollout begins.
    if !RELEASE_PUBKEYS.is_empty() {
        return Err(UpdateError::new(
            Code::SignatureMissing,
            "signed manifests are not supported by this daemon helper",
        ));
    }
    let mut raw = Vec::new();
    open_regular(&staging.join("manifest.json"))
        .map_err(storage)?
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut raw)
        .map_err(storage)?;
    if raw.len() > 2 * 1024 * 1024 {
        return Err(UpdateError::new(
            Code::ResponseTooLarge,
            "manifest is too large",
        ));
    }
    if let Some(digest) = digest {
        verify_sha256(raw.as_slice(), digest)?;
    }
    let manifest = ReleaseManifest::parse(&raw, tag)?;
    if manifest.protocol_version != PROTOCOL_VERSION {
        return Err(UpdateError::new(
            Code::ProtocolIncompatible,
            "staged release protocol is incompatible",
        ));
    }
    Ok(manifest)
}

#[cfg(windows)]
use ddg_agents_os::fs::open_regular;

#[cfg(unix)]
fn open_regular(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("expected a regular file"));
    }
    Ok(file)
}

fn copy_binary(src: &Path, dest: &Path, expected: Option<&str>) -> Result<String> {
    let mut input = open_regular(src).map_err(storage)?;
    #[cfg(unix)]
    let mode = input.metadata().map_err(storage)?.mode() & 0o777;
    #[cfg(unix)]
    if mode & 0o111 == 0 {
        return Err(storage("binary is not executable"));
    }
    if let Ok(metadata) = dest.symlink_metadata()
        && !metadata.is_file()
    {
        return Err(storage("destination is not a regular file"));
    }
    let temp = dest.with_file_name(format!(".ddg-swap-{}", paths::random_hex()));
    #[cfg(windows)]
    let mut output = ddg_agents_os::fs::create_private_file(&temp).map_err(storage)?;
    #[cfg(unix)]
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(storage)?;
    let result = (|| {
        io::copy(&mut input, &mut output).map_err(storage)?;
        #[cfg(unix)]
        output
            .set_permissions(fs::Permissions::from_mode(mode))
            .map_err(storage)?;
        output.sync_all().map_err(storage)?;
        let mut readable = open_regular(&temp).map_err(storage)?;
        let digest = if let Some(expected) = expected {
            verify_sha256(&mut readable, expected)?;
            expected.to_owned()
        } else {
            let mut digest = Sha256::new();
            io::copy(&mut readable, &mut digest).map_err(storage)?;
            format!("{:x}", digest.finalize())
        };
        drop(readable);
        drop(output);
        ddg_agents_os::replace::replace_executable(&temp, dest).map_err(storage)?;
        sync_dir(dest.parent().expect("absolute binary path")).map_err(storage)?;
        Ok(digest)
    })();
    if result.is_err() {
        paths::remove_if_exists(&temp);
    }
    result
}

#[cfg(windows)]
fn restore_rollback_binary(
    root: &Path,
    record: &RollbackRecord,
    name: &str,
    dest: &Path,
) -> Result<()> {
    verify_rollback_set(root, record)?;
    let digest = record.binary_sha256s.get(name).ok_or_else(|| {
        UpdateError::new(
            Code::RollbackVerificationFailed,
            "binary absent from verified rollback set",
        )
    })?;
    copy_binary(&root.join(name), dest, Some(digest)).map(|_| ())
}

fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return File::open(path)?.sync_all();
    #[cfg(windows)]
    {
        // Windows only supports write-through for the executable file moves.
        let _ = path;
        Ok(())
    }
}

struct Backup {
    prev: PathBuf,
    next: PathBuf,
    prior: PathBuf,
    record: RollbackRecord,
    published: bool,
}

impl Backup {
    fn create(
        prev: &Path,
        install: &Path,
        names: &[&str],
        version: &str,
        holder_protocol: u32,
        legacy: bool,
    ) -> Result<Self> {
        let parent = prev.parent().expect("rollback parent");
        ensure_private_dir(parent).map_err(storage)?;
        let nonce = paths::random_hex();
        let mut backup = Self {
            prev: prev.into(),
            next: parent.join(format!(".next-{nonce}")),
            prior: parent.join(format!(".prior-{nonce}")),
            record: RollbackRecord::default(),
            published: false,
        };
        ensure_private_dir(&backup.next).map_err(storage)?;
        for &name in names {
            let digest = copy_binary(&install.join(name), &backup.next.join(name), None)?;
            backup.record.binary_sha256s.insert(name.into(), digest);
        }
        backup.record.version = version.into();
        backup.record.platform = platform();
        backup.record.recorded_at = paths::unix_secs();
        // Standalone installed daemons have no release archive. Verify the backup's
        // legacy marker as part of the rollback set, just like a binary.
        if legacy {
            let bytes = b"v0.1.0\n";
            write_atomic(&backup.next.join("legacy-host"), bytes).map_err(storage)?;
            backup.record.archive_rel_path = "legacy-host".into();
            backup.record.archive_sha256 = format!("{:x}", Sha256::digest(bytes));
        } else {
            backup.record.archive_rel_path = HOST.into();
            backup.record.archive_sha256 = backup.record.binary_sha256s[HOST].clone();
        }
        backup.record.catalog_schema_version = 1;
        backup.record.hosts_schema_version = 1;
        backup.record.holder_protocol_version = holder_protocol;
        write_atomic(
            &backup.next.join("rollback.json"),
            &serde_json::to_vec(&backup.record).map_err(storage)?,
        )
        .map_err(storage)?;
        verify_rollback_set(&backup.next, &backup.record)?;
        Ok(backup)
    }

    fn publish(&mut self) -> Result<()> {
        match fs::rename(&self.prev, &self.prior) {
            Ok(()) => (),
            Err(err) if err.kind() == io::ErrorKind::NotFound => (),
            Err(err) => return Err(storage(err)),
        }
        if let Err(err) = fs::rename(&self.next, &self.prev) {
            if self.prior.exists() {
                fs::rename(&self.prior, &self.prev).map_err(failed)?;
            }
            return Err(storage(err));
        }
        self.published = true;
        sync_dir(self.prev.parent().expect("rollback parent")).map_err(storage)
    }

    fn restore_prior(&mut self) -> Result<()> {
        if !self.published && self.prior.exists() {
            fs::rename(&self.prior, &self.prev).map_err(storage)?;
        }
        if self.published {
            fs::rename(&self.prev, &self.next).map_err(storage)?;
            if self.prior.exists()
                && let Err(error) = fs::rename(&self.prior, &self.prev)
            {
                fs::rename(&self.next, &self.prev).map_err(failed)?;
                return Err(storage(error));
            }
            self.published = false;
        }
        Ok(())
    }

    fn commit(&self) {
        let _ = fs::remove_dir_all(&self.prior);
    }
}

impl Drop for Backup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.next);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostMode {
    Direct,
    Legacy(u32),
    Stopped,
}

fn classify_host(admin: io::Result<()>, public: Option<u32>) -> io::Result<HostMode> {
    match admin {
        Ok(()) if public.is_some() => Ok(HostMode::Direct),
        Ok(()) => Err(io::Error::other("admin socket is live without host.sock")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Ok(public.map_or(HostMode::Stopped, HostMode::Legacy))
        }
        Err(e) => Err(e),
    }
}

fn host_mode(paths: &Paths) -> io::Result<HostMode> {
    let public = socket_pid(&paths.host_sock)?;
    let admin = connect(&crate::admin::path(paths)).and_then(|s| {
        if Some(verified_pid(&s)?) != public {
            return Err(io::Error::other(
                "host sockets belong to different processes",
            ));
        }
        Ok(())
    });
    if public.is_none()
        && admin
            .as_ref()
            .is_err_and(|e| e.kind() == io::ErrorKind::ConnectionRefused)
    {
        return Ok(HostMode::Stopped);
    }
    classify_host(admin, public)
}

struct Swap<'a> {
    paths: &'a Paths,
    install: &'a Path,
    mode: HostMode,
    host_stopped: bool,
    core_changed: bool,
    holder_changed: bool,
}

impl<'a> Swap<'a> {
    fn new(paths: &'a Paths, install: &'a Path, mode: HostMode) -> Self {
        Self {
            paths,
            install,
            mode,
            host_stopped: false,
            core_changed: false,
            holder_changed: false,
        }
    }

    fn prepare(&mut self) -> Result<()> {
        match self.mode {
            HostMode::Direct => self.host_admin("prepare_reload", json!({})).map_err(failed),
            HostMode::Legacy(pid) => self.stop_host(pid).map_err(failed),
            HostMode::Stopped => Ok(()),
        }
    }

    fn stop_host(&mut self, pid: u32) -> io::Result<()> {
        terminate(&self.paths.host_sock, pid)?;
        self.host_stopped = true;
        wait_lock(&self.paths.lock_file)
    }

    fn host_admin(&self, method: &str, params: Value) -> io::Result<()> {
        rpc(&crate::admin::path(self.paths), method, params).and_then(ok)
    }

    fn replace_core(
        &mut self,
        restart_holder: bool,
        restart_sessions: bool,
        old_protocol: u32,
    ) -> Result<()> {
        self.core_changed = true;
        stop_core(self.paths, restart_holder, restart_sessions, old_protocol).map_err(failed)?;
        self.holder_changed = restart_holder;
        start_core(self.paths, self.install).map_err(failed)
    }

    fn replace_host(&mut self, legacy_target: bool, _version: &str) -> Result<()> {
        if self.mode == HostMode::Direct && !legacy_target {
            let host = self.install.join(HOST);
            #[cfg(windows)]
            let old_pid = socket_pid(&self.paths.host_sock).map_err(failed)?;
            self.host_admin("execute_reload", json!({"bin_path":host}))
                .map_err(failed)?;
            #[cfg(windows)]
            {
                let deadline = Instant::now() + Duration::from_secs(15);
                loop {
                    if let Ok(Some(pid)) = socket_pid(&self.paths.host_sock)
                        && Some(pid) != old_pid
                        && socket_pid(&crate::admin::path(self.paths)).ok().flatten() == Some(pid)
                        && hello(&self.paths.host_sock)
                            .is_ok_and(|reply| reply["app_version"] == _version)
                        && rpc(&self.paths.host_sock, "ping", json!({}))
                            .and_then(ok)
                            .is_ok()
                    {
                        break;
                    }
                    if Instant::now() >= deadline {
                        return Err(failed("successor Host did not become ready"));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            return Ok(());
        }
        if self.mode == HostMode::Direct {
            let pid = socket_pid(&self.paths.host_sock)
                .map_err(failed)?
                .ok_or_else(|| failed("old Host is absent"))?;
            self.stop_host(pid).map_err(failed)?;
        }
        start_host(self.paths, self.install).map_err(failed)
    }

    fn recover(
        &mut self,
        version: &str,
        old_protocol: u32,
        old_core_legacy: bool,
        instance: Option<&str>,
        holder_pid: Option<u32>,
    ) -> Result<()> {
        if self.core_changed {
            let protocol = probe_terminal(self.paths)
                .ok()
                .flatten()
                .and_then(|t| t.holder_protocol_version)
                .unwrap_or(old_protocol);
            stop_core(self.paths, self.holder_changed, true, protocol).map_err(failed)?;
            start_core(self.paths, self.install).map_err(failed)?;
            core_health(
                self.paths,
                version,
                old_protocol,
                (!self.holder_changed).then_some((instance, holder_pid)),
                old_core_legacy,
            )
            .map_err(failed)?;
        }
        if self.mode == HostMode::Direct && !self.host_stopped {
            self.host_admin("abort_reload", json!({})).map_err(failed)?;
        } else if self.host_stopped || socket_pid(&self.paths.host_sock).map_err(failed)?.is_some()
        {
            if let Some(pid) = socket_pid(&self.paths.host_sock).map_err(failed)? {
                self.stop_host(pid).map_err(failed)?;
            }
            start_host(self.paths, self.install).map_err(failed)?;
        }
        Ok(())
    }
}

fn stop_core(
    paths: &Paths,
    restart_holder: bool,
    restart_sessions: bool,
    protocol: u32,
) -> io::Result<()> {
    let recovering = paths.run_dir.join("terminal-admin.sock");
    match connect(&recovering) {
        Ok(mut stream) => {
            configure(&stream)?;
            let pid = verified_pid(&stream)?;
            if socket_pid(&paths.terminal_sock)? != Some(pid) {
                return Err(io::Error::other(
                    "Core sockets belong to different processes",
                ));
            }
            let command: &[u8] = match (restart_holder, restart_sessions) {
                (true, true) => b"shutdown\n",
                (true, false) => b"shutdown-if-idle\n",
                _ => b"detach\n",
            };
            stream.write_all(command)?;
            ok(read_json(stream)?)?;
            wait_lock(&paths.state_dir.join("terminal.lock"))?;
        }
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            if let Some(pid) = socket_pid(&paths.terminal_sock)? {
                terminate(&paths.terminal_sock, pid)?;
                wait_lock(&paths.state_dir.join("terminal.lock"))?;
            }
            if restart_holder && let Some(pid) = socket_pid(&paths.run_dir.join("holder.sock"))? {
                let mut stream = connect(&paths.run_dir.join("holder.sock"))?;
                configure(&stream)?;
                if verified_pid(&stream)? != pid {
                    return Err(io::Error::other("Holder PID changed"));
                }
                stream.write_all(&holder::encode_json(
                    holder::HELLO,
                    0,
                    &holder::HolderHello { version: protocol },
                ))?;
                if !restart_sessions {
                    let Some((holder::WELCOME, 0, payload)) = holder::read_frame(&mut stream)?
                    else {
                        return Err(io::Error::other("Holder omitted session state"));
                    };
                    let welcome: holder::HolderWelcome = holder::parse_json(&payload)?;
                    if welcome.ptys.iter().any(|pty| pty.exit.is_none()) {
                        return Err(io::Error::other(
                            "explicit consent required to restart active sessions",
                        ));
                    }
                }
                stream.write_all(&holder::encode_json(
                    holder::GOODBYE,
                    0,
                    &holder::HolderGoodbye::Shutdown,
                ))?;
                loop {
                    match holder::read_frame(&mut stream)? {
                        Some((holder::BYE, 0, _)) => break,
                        Some(_) => (),
                        None => return Err(io::Error::other("Holder closed without BYE")),
                    }
                }
            }
        }
        Err(e) => return Err(e),
    }
    if restart_holder {
        wait_until(|| {
            Ok(socket_pid(&paths.run_dir.join("holder.sock"))?
                .is_none()
                .then_some(()))
        })?;
    }
    Ok(())
}

fn start_core(paths: &Paths, install: &Path) -> io::Result<()> {
    let bin = install.join(CORE);
    let mut cmd = command(paths, &bin);
    cmd.arg("core").arg("--bin-path").arg(&bin);
    spawn_detached(cmd)?;
    Ok(())
}

fn start_host(paths: &Paths, install: &Path) -> io::Result<()> {
    let mut cmd = command(paths, &install.join(HOST));
    cmd.arg("serve")
        .env("DDG_TERMINAL_SERVER_BIN", install.join(CORE));
    let mut child = spawn_detached(cmd)?;
    let result = wait_until(|| {
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!("Host exited: {status}")));
        }
        Ok(rpc(&paths.host_sock, "ping", json!({}))
            .and_then(ok)
            .is_ok()
            .then_some(()))
    });
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn command(paths: &Paths, bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    cmd.env("DDG_STATE_DIR", &paths.state_dir)
        .env("DDG_RUN_DIR", &paths.run_dir)
        .env("DDG_PLUGINS_DIR", &paths.plugins_dir)
        .env_remove("DDG_SESSION_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

fn probe_terminal(paths: &Paths) -> io::Result<Option<HelloResult>> {
    if socket_pid(&paths.terminal_sock)?.is_none() {
        return Ok(None);
    }
    serde_json::from_value(hello(&paths.terminal_sock)?)
        .map(Some)
        .map_err(io::Error::other)
}

fn core_health(
    paths: &Paths,
    version: &str,
    protocol: u32,
    holder: Option<(Option<&str>, Option<u32>)>,
    legacy: bool,
) -> io::Result<()> {
    let hello = wait_until(|| Ok(probe_terminal(paths).ok().flatten()))?;
    ok(rpc(&paths.terminal_sock, "ping", json!({}))?)?;
    if (!legacy || hello.app_version.is_some())
        && (hello.app_version.as_deref() != Some(version)
            || hello.holder_protocol_version != Some(protocol)
            || hello.holder_instance.is_none())
    {
        return Err(io::Error::other(
            "Core version or Holder metadata did not match the release",
        ));
    }
    if let Some((instance, pid)) = holder
        && ((instance.is_some() && instance != hello.holder_instance.as_deref() && !legacy)
            || (pid.is_some() && pid != socket_pid(&paths.run_dir.join("holder.sock"))?))
    {
        return Err(io::Error::other("Holder changed during Core detach"));
    }
    Ok(())
}

fn rpc(path: &Path, method: &str, params: Value) -> io::Result<Value> {
    let mut stream = connect(path)?;
    configure(&stream)?;
    verified_pid(&stream)?;
    serde_json::to_writer(&mut stream, &Request::new(1, method, params))?;
    stream.write_all(b"\n")?;
    let resp: Response = read_json(stream)?;
    if let Some(error) = resp.error {
        return Err(io::Error::other(error.message));
    }
    resp.result
        .ok_or_else(|| io::Error::other("missing RPC result"))
}

fn hello(path: &Path) -> io::Result<Value> {
    rpc(
        path,
        "hello",
        json!({"protocol_version":PROTOCOL_VERSION,"framed":false}),
    )
}

fn ok(result: Value) -> io::Result<()> {
    if result["ok"] == true {
        Ok(())
    } else {
        Err(io::Error::other("daemon did not acknowledge request"))
    }
}

fn read_json<T: serde::de::DeserializeOwned>(stream: LocalStream) -> io::Result<T> {
    let mut line = Vec::new();
    BufReader::new(stream)
        .take(32 * 1024 * 1024)
        .read_until(b'\n', &mut line)?;
    if !line.ends_with(b"\n") {
        return Err(io::Error::other("incomplete daemon reply"));
    }
    serde_json::from_slice(&line).map_err(io::Error::other)
}

fn configure(stream: &LocalStream) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(WAIT))
}

fn verified_pid(stream: &LocalStream) -> io::Result<u32> {
    let peer = peer_credentials(stream)?;
    if peer_is_current_user(&peer)
        && let Some(pid) = peer.pid
        && pid > 1
        && pid <= i32::MAX as u32
    {
        Ok(pid)
    } else {
        Err(io::ErrorKind::PermissionDenied.into())
    }
}

fn socket_pid(path: &Path) -> io::Result<Option<u32>> {
    match connect(path) {
        Ok(stream) => Ok(Some(verified_pid(&stream)?)),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

fn terminate(path: &Path, pid: u32) -> io::Result<()> {
    if socket_pid(path)? != Some(pid) {
        return Err(io::Error::other("daemon PID changed"));
    }
    #[cfg(windows)]
    ddg_agents_os::process::terminate_process(pid)?;
    #[cfg(unix)]
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
        return Err(io::Error::last_os_error());
    }
    wait_until(|| Ok((socket_pid(path)? != Some(pid)).then_some(())))
}

fn wait_lock(path: &Path) -> io::Result<()> {
    wait_until(|| match FileLock::try_lock_exclusive(path) {
        Ok(_) => Ok(Some(())),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e),
    })
}

fn wait_until<T>(mut probe: impl FnMut() -> io::Result<Option<T>>) -> io::Result<T> {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = probe()? {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_missing_admin_socket_allows_legacy() {
        assert_eq!(
            classify_host(Err(io::ErrorKind::NotFound.into()), Some(29)).unwrap(),
            HostMode::Legacy(29)
        );
        assert_eq!(classify_host(Ok(()), Some(29)).unwrap(), HostMode::Direct);
        assert!(classify_host(Err(io::ErrorKind::PermissionDenied.into()), Some(29)).is_err());
        assert!(classify_host(Err(io::ErrorKind::ConnectionRefused.into()), Some(29)).is_err());
    }
}
