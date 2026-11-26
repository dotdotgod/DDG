//! macOS bundle checks and one-shot replacement job.

use crate::{
    UpdateError, UpdateErrorCode as Code,
    swap::{open_regular, plain_dir, storage_error, verify_file, verify_files},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

#[cfg(unix)]
use std::fs::File;
#[cfg(target_os = "macos")]
use {crate::swap::nonce, std::io};

pub const EXPECTED_APPLE_TEAM_ID: &str = "PLACEHOLDER_TEAM_ID";
pub const EXPECTED_BUNDLE_ID: &str = "com.dotdotgod.agents";

pub fn codesign_requirement(team_id: &str, bundle_id: &str) -> String {
    format!(
        r#"anchor apple generic and identifier "{bundle_id}" and certificate 1[field.1.2.840.113635.100.6.2.6] /* exists */ and certificate leaf[field.1.2.840.113635.100.6.1.13] /* exists */ and certificate leaf[subject.OU] = "{team_id}""#
    )
}

fn pinned_identity(team_id: &str, bundle_id: &str) -> Result<(), UpdateError> {
    if team_id != EXPECTED_APPLE_TEAM_ID || bundle_id != EXPECTED_BUNDLE_ID {
        return Err(UpdateError::new(
            Code::CodesignInvalid,
            "Bundle identity must match the compiled release identity",
        ));
    }
    Ok(())
}

fn translocation(path: &Path) -> Result<(), UpdateError> {
    if path
        .components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case("AppTranslocation"))
    {
        return Err(UpdateError::new(
            Code::AppTranslocated,
            "Move the app out of AppTranslocation before updating",
        ));
    }
    Ok(())
}

pub fn macos_preflight_check(app_path: &Path) -> Result<(), UpdateError> {
    translocation(app_path)?;
    let parent = app_path
        .parent()
        .ok_or_else(|| storage_error("App has no install directory"))?;
    let parent = parent.canonicalize().map_err(storage_error)?;
    translocation(&parent)?;
    plain_dir(&parent).map_err(storage_error)?;
    #[cfg(unix)]
    {
        use std::{
            ffi::CString,
            os::unix::{ffi::OsStrExt, fs::PermissionsExt},
        };
        let mode = fs::metadata(&parent)
            .map_err(storage_error)?
            .permissions()
            .mode();
        let path = CString::new(parent.as_os_str().as_bytes()).map_err(storage_error)?;
        // SAFETY: path is an owned, NUL-terminated filesystem path.
        let access = unsafe {
            libc::faccessat(
                libc::AT_FDCWD,
                path.as_ptr(),
                libc::W_OK | libc::X_OK,
                libc::AT_EACCESS,
            )
        };
        if mode & 0o222 == 0 || access != 0 {
            return Err(storage_error("App install directory is not writable"));
        }
    }
    #[cfg(not(unix))]
    if fs::metadata(parent)
        .map_err(storage_error)?
        .permissions()
        .readonly()
    {
        return Err(storage_error("App install directory is not writable"));
    }
    Ok(())
}

pub fn verify_macos_app_bundle(
    app_path: &Path,
    team_id: &str,
    bundle_id: &str,
) -> Result<(), UpdateError> {
    pinned_identity(team_id, bundle_id)?;
    plain_dir(app_path).map_err(|e| UpdateError::new(Code::CodesignInvalid, e.to_string()))?;
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        let app = app_path
            .canonicalize()
            .map_err(|e| UpdateError::new(Code::CodesignInvalid, e.to_string()))?;
        let requirement = codesign_requirement(EXPECTED_APPLE_TEAM_ID, EXPECTED_BUNDLE_ID);
        let signed = Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict", "-R", &requirement])
            .arg(&app)
            .output();
        if !signed.is_ok_and(|result| result.status.success()) {
            return Err(UpdateError::new(
                Code::CodesignInvalid,
                "App does not pass the compiled Developer ID signing requirement",
            ));
        }
        let assessed = Command::new("/usr/sbin/spctl")
            .args(["--assess", "--type", "execute", "--verbose=4"])
            .arg(&app)
            .output();
        if !assessed.is_ok_and(|result| result.status.success()) {
            return Err(UpdateError::new(
                Code::NotarizationInvalid,
                "App failed the Gatekeeper assessment",
            ));
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    Err(UpdateError::new(
        Code::CodesignInvalid,
        "Apple code signature verification requires macOS",
    ))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SwapJob {
    pub staging_app_path: PathBuf,
    pub target_app_path: PathBuf,
    pub archive_path: PathBuf,
    pub archive_sha256: String,
    /// Release binary paths relative to the staged .app.
    pub binary_sha256s: BTreeMap<String, String>,
}

fn unsafe_job(message: impl AsRef<str>) -> UpdateError {
    UpdateError::new(Code::HostSecurityBlocked, message)
}

fn job_io(e: impl std::fmt::Display) -> UpdateError {
    unsafe_job(e.to_string())
}

fn private_dir(path: &Path) -> Result<(), UpdateError> {
    plain_dir(path).map_err(job_io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = path.symlink_metadata().map_err(job_io)?;
        // SAFETY: geteuid has no preconditions.
        if meta.mode() & 0o7777 != 0o700 || meta.uid() != unsafe { libc::geteuid() } {
            return Err(unsafe_job(
                "Update state and staging roots must be owner-only directories",
            ));
        }
    }
    Ok(())
}

fn path_under(
    path: &Path,
    input_root: &Path,
    canonical_root: &Path,
) -> Result<PathBuf, UpdateError> {
    let relative = path
        .strip_prefix(input_root)
        .or_else(|_| path.strip_prefix(canonical_root))
        .map_err(|_| unsafe_job("Helper path is outside its private root"))?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(unsafe_job("Invalid helper path"));
    }
    let mut resolved = canonical_root.to_owned();
    let mut parts = relative.components().peekable();
    while let Some(part) = parts.next() {
        resolved.push(part);
        if parts.peek().is_some() {
            plain_dir(&resolved).map_err(job_io)?;
        }
    }
    Ok(resolved)
}

fn app_name(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
}

#[cfg(unix)]
fn same_inode(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[derive(Debug)]
struct PinnedDir {
    path: PathBuf,
    #[cfg(unix)]
    file: File,
}

impl PinnedDir {
    fn new(path: PathBuf) -> Result<Self, UpdateError> {
        plain_dir(&path).map_err(job_io)?;
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
                .open(&path)
                .map_err(job_io)?
        };
        let dir = Self {
            path,
            #[cfg(unix)]
            file,
        };
        dir.recheck()?;
        Ok(dir)
    }

    fn recheck(&self) -> Result<(), UpdateError> {
        let current = self.path.symlink_metadata().map_err(job_io)?;
        if !current.is_dir() {
            return Err(unsafe_job("Update directory was replaced"));
        }
        #[cfg(unix)]
        if !same_inode(&current, &self.file.metadata().map_err(job_io)?) {
            return Err(unsafe_job("Update directory identity changed"));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct ValidatedSwapJob {
    job: SwapJob,
    stage_root: PinnedDir,
    staged: PinnedDir,
    target_parent: PinnedDir,
    target: PinnedDir,
}

impl ValidatedSwapJob {
    /// Recheck held inode identities immediately before any use of the job.
    pub fn recheck(&self) -> Result<(), UpdateError> {
        self.stage_root.recheck()?;
        private_dir(&self.stage_root.path)?;
        path_under(
            &self.staged.path,
            &self.stage_root.path,
            &self.stage_root.path,
        )?;
        path_under(
            &self.job.archive_path,
            &self.stage_root.path,
            &self.stage_root.path,
        )?;
        self.staged.recheck()?;
        self.target_parent.recheck()?;
        self.target.recheck()?;
        Ok(())
    }

    fn verify_hashes(&self) -> Result<(), UpdateError> {
        verify_file(&self.job.archive_path, &self.job.archive_sha256)?;
        self.verify_staged_files(&self.staged.path)
    }

    fn verify_staged_files(&self, stage: &Path) -> Result<(), UpdateError> {
        verify_files(
            stage,
            self.job
                .binary_sha256s
                .iter()
                .map(|(p, h)| (p.as_str(), h.as_str())),
        )
    }
}

fn consume_job(job_path: &Path) -> Result<SwapJob, UpdateError> {
    let file = open_regular(job_path).map_err(job_io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = file.metadata().map_err(job_io)?;
        // SAFETY: geteuid has no preconditions.
        if opened.mode() & 0o7777 != 0o600
            || opened.uid() != unsafe { libc::geteuid() }
            || opened.nlink() != 1
            || !same_inode(&opened, &job_path.symlink_metadata().map_err(job_io)?)
        {
            return Err(unsafe_job(
                "Swap job must be a single-link 0600 regular file owned by this user",
            ));
        }
    }
    // Only this process can consume the private, one-shot job. Unlink before
    // parsing or verification, including malformed jobs.
    fs::remove_file(job_path).map_err(job_io)?;
    const MAX_JOB_BYTES: u64 = 2 * 1024 * 1024;
    let mut bytes = Vec::new();
    file.take(MAX_JOB_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(job_io)?;
    if bytes.len() as u64 > MAX_JOB_BYTES {
        return Err(unsafe_job("Swap job is too large"));
    }
    serde_json::from_slice(&bytes).map_err(job_io)
}

/// Job and stage roots are private to this account. The launcher/helper
/// serializes replacement operations with update.lock.
pub fn validate_helper_swap_job(
    job_path: &Path,
    state_dir: &Path,
    expected_team_id: &str,
    expected_bundle_id: &str,
) -> Result<ValidatedSwapJob, UpdateError> {
    validate_job(job_path, state_dir, |app| {
        pinned_identity(expected_team_id, expected_bundle_id)?;
        #[cfg(target_os = "macos")]
        verify_macos_app_bundle(app, EXPECTED_APPLE_TEAM_ID, EXPECTED_BUNDLE_ID)?;
        #[cfg(not(target_os = "macos"))]
        let _ = app;
        Ok(())
    })
}

fn validate_job(
    job_path: &Path,
    state_dir: &Path,
    verify_identity: impl FnOnce(&Path) -> Result<(), UpdateError>,
) -> Result<ValidatedSwapJob, UpdateError> {
    private_dir(state_dir)?;
    let state_root = state_dir.canonicalize().map_err(job_io)?;
    let job_path = path_under(job_path, state_dir, &state_root)?;
    let mut job = consume_job(&job_path)?;

    let staging = state_root.join("staging");
    private_dir(&staging)?;
    let staged = path_under(&job.staging_app_path, &state_dir.join("staging"), &staging)?;
    let archive = path_under(&job.archive_path, &state_dir.join("staging"), &staging)?;
    if !job.target_app_path.is_absolute()
        || !app_name(&job.target_app_path)
        || !app_name(&staged)
        || job.binary_sha256s.is_empty()
    {
        return Err(unsafe_job(
            "Helper requires real .app paths and a nonempty binary digest set",
        ));
    }
    let parent = job
        .target_app_path
        .parent()
        .ok_or_else(|| unsafe_job("Invalid target path"))?;
    macos_preflight_check(&job.target_app_path)?;
    let target_parent = parent.canonicalize().map_err(job_io)?;
    let target = target_parent.join(job.target_app_path.file_name().unwrap());
    if target.starts_with(&state_root) || staged == target {
        return Err(unsafe_job("Install target cannot be inside update state"));
    }

    job.archive_path = archive;
    let checked = ValidatedSwapJob {
        job,
        stage_root: PinnedDir::new(staging)?,
        staged: PinnedDir::new(staged)?,
        target_parent: PinnedDir::new(target_parent)?,
        target: PinnedDir::new(target)?,
    };
    checked.verify_hashes()?;
    verify_identity(&checked.staged.path)?;
    checked.recheck()?;
    Ok(checked)
}

/// Keep a complete N-1 copy before exposing the replacement app. The caller
/// holds update.lock and provides a fresh destination under rollback/prev.
#[cfg(target_os = "macos")]
pub fn swap_macos_app(job: &ValidatedSwapJob, backup_app: &Path) -> Result<(), UpdateError> {
    job.recheck()?;
    job.verify_hashes()?;
    verify_macos_app_bundle(&job.staged.path, EXPECTED_APPLE_TEAM_ID, EXPECTED_BUNDLE_ID)?;
    replace_bundle(
        &job.staged.path,
        &job.target.path,
        Some(backup_app),
        |candidate| {
            job.recheck()?;
            job.verify_staged_files(candidate)?;
            verify_macos_app_bundle(candidate, EXPECTED_APPLE_TEAM_ID, EXPECTED_BUNDLE_ID)
        },
    )
}

#[cfg(target_os = "macos")]
pub(crate) fn replace_bundle(
    staged: &Path,
    target: &Path,
    backup: Option<&Path>,
    verify: impl Fn(&Path) -> Result<(), UpdateError>,
) -> Result<(), UpdateError> {
    use crate::swap::copy_verified;
    macos_preflight_check(target)?;
    let target = PinnedDir::new(target.to_owned())?;
    let parent = target.path.parent().unwrap();
    let incoming = parent.join(format!(
        ".ddg-incoming-{}.app",
        nonce().map_err(storage_error)?
    ));
    copy_verified(staged, &incoming, false, &verify)?;
    let incoming = PinnedDir::new(incoming)?;
    let result = (|| {
        if let Some(backup) = backup {
            if !app_name(backup) {
                return Err(storage_error("Rollback app path must end in .app"));
            }
            // Copy with fsync and SHA-256 into rollback/prev before swapping. This
            // also works across filesystems, without ever risking the live copy.
            copy_verified(&target.path, backup, false, |_| Ok(()))?;
        }
        verify(&incoming.path)?;
        target.recheck()?;
        incoming.recheck()?;
        exchange(&incoming.path, &target.path)
    })();
    match result {
        Ok(old) => {
            let _ = fs::remove_dir_all(old);
            Ok(())
        }
        Err(error) => {
            if error.code != Code::DaemonSwapFailedRollbackFailed {
                let _ = fs::remove_dir_all(&incoming.path);
            }
            Err(error)
        }
    }
}

#[cfg(target_os = "macos")]
fn exchange(incoming: &Path, target: &Path) -> Result<PathBuf, UpdateError> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let from = CString::new(incoming.as_os_str().as_bytes()).map_err(storage_error)?;
    let to = CString::new(target.as_os_str().as_bytes()).map_err(storage_error)?;
    // SAFETY: Both paths are NUL-terminated and pinned directories in the same parent.
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_SWAP) } == 0 {
        return Ok(incoming.to_owned());
    }
    let error = io::Error::last_os_error();
    if !matches!(
        error.raw_os_error(),
        Some(libc::ENOTSUP | libc::ENOSYS | libc::EINVAL)
    ) {
        return Err(storage_error(error));
    }

    let aside = target.with_file_name(format!(
        ".ddg-displaced-{}.app",
        nonce().map_err(storage_error)?
    ));
    fs::rename(target, &aside).map_err(storage_error)?;
    if let Err(error) = fs::rename(incoming, target) {
        let code = if fs::rename(&aside, target).is_ok() {
            Code::DaemonSwapFailedRolledBack
        } else {
            Code::DaemonSwapFailedRollbackFailed
        };
        return Err(UpdateError::new(
            code,
            format!("Bundle exchange failed: {error}"),
        ));
    }
    Ok(aside)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_and_rollback_bundle_exchange_preserves_prev() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("staging/DDG.app");
        let target = dir.path().join("DDG.app");
        let backup = dir.path().join("rollback/prev/DDG.app");
        fs::create_dir_all(&stage).unwrap();
        fs::create_dir_all(backup.parent().unwrap()).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(stage.join("ddg"), b"abc").unwrap();
        fs::write(target.join("ddg"), b"previous").unwrap();
        symlink("ddg", stage.join("linked")).unwrap();
        let verify = |app: &Path| {
            verify_file(
                &app.join("ddg"),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            )
        };

        replace_bundle(&stage, &target, Some(&backup), verify).unwrap();
        assert_eq!(fs::read(backup.join("ddg")).unwrap(), b"previous");
        assert_eq!(fs::read(target.join("linked")).unwrap(), b"abc");
        assert!(
            target
                .join("linked")
                .symlink_metadata()
                .unwrap()
                .is_symlink()
        );
        assert!(!dir.path().read_dir().unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".ddg-")
        }));
    }

    #[test]
    fn macos_and_rollback_helper_rechecks_pinned_directory() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("staging");
        let staged = stage.join("Candidate.app");
        for path in [&stage, &staged] {
            fs::create_dir(path).unwrap();
        }
        fs::set_permissions(&stage, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let archive = stage.join("release.zip");
        fs::write(&archive, b"abc").unwrap();
        fs::write(staged.join("ddg"), b"abc").unwrap();
        let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let outer = tempfile::tempdir().unwrap();
        let target = outer.path().join("Target.app");
        fs::create_dir(&target).unwrap();
        let job = SwapJob {
            staging_app_path: staged.clone(),
            target_app_path: target,
            archive_path: archive,
            archive_sha256: hash.into(),
            binary_sha256s: BTreeMap::from([("ddg".into(), hash.into())]),
        };
        let path = dir.path().join("job.json");
        ddg_agents_os::fs::write_atomic(&path, &serde_json::to_vec(&job).unwrap()).unwrap();
        let checked = validate_job(&path, dir.path(), |_| Ok(())).unwrap();
        assert!(!path.exists());
        let saved = stage.join("Saved.app");
        fs::rename(&staged, &saved).unwrap();
        symlink(&saved, &staged).unwrap();
        assert_eq!(
            checked.recheck().unwrap_err().code,
            Code::HostSecurityBlocked
        );
        fs::remove_file(&staged).unwrap();
        fs::create_dir(&staged).unwrap();
        assert_eq!(
            checked.recheck().unwrap_err().code,
            Code::HostSecurityBlocked
        );
    }
}
