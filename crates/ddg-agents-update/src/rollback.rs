//! N-1 backup verification and restoration.

use crate::{
    UpdateError, UpdateErrorCode as Code,
    swap::{copy_verified, regular_path, verify_file, verify_files},
    version::ReleaseTag,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RollbackRecord {
    pub version: String,
    pub platform: String,
    pub recorded_at: u64,
    pub archive_rel_path: String,
    pub archive_sha256: String,
    pub binary_sha256s: BTreeMap<String, String>,
    pub catalog_schema_version: u32,
    pub hosts_schema_version: u32,
    pub holder_protocol_version: u32,
}

pub use crate::swap::copy_or_move_preserving_backup;

fn verification_failed(error: impl std::fmt::Display) -> UpdateError {
    UpdateError::new(Code::RollbackVerificationFailed, error.to_string())
}

pub fn verify_rollback_set(
    rollback_dir: &Path,
    record: &RollbackRecord,
) -> Result<(), UpdateError> {
    ReleaseTag::parse(&record.version).map_err(verification_failed)?;
    if record.binary_sha256s.is_empty() {
        return Err(verification_failed(
            "Rollback record contains no binary digests",
        ));
    }
    let archive =
        regular_path(rollback_dir, &record.archive_rel_path).map_err(verification_failed)?;
    verify_file(&archive, &record.archive_sha256).map_err(verification_failed)?;
    verify_files(
        rollback_dir,
        record
            .binary_sha256s
            .iter()
            .map(|(p, h)| (p.as_str(), h.as_str())),
    )
    .map_err(verification_failed)?;
    #[cfg(target_os = "macos")]
    {
        use crate::macos::{EXPECTED_APPLE_TEAM_ID, EXPECTED_BUNDLE_ID, verify_macos_app_bundle};
        let mut apps: std::collections::BTreeSet<_> = record
            .binary_sha256s
            .keys()
            .filter_map(|relative| Path::new(relative).ancestors().filter(|p| is_app(p)).last())
            .map(|app| rollback_dir.join(app))
            .collect();
        if is_app(rollback_dir) {
            apps.insert(rollback_dir.to_owned());
        }
        for app in apps {
            verify_macos_app_bundle(&app, EXPECTED_APPLE_TEAM_ID, EXPECTED_BUNDLE_ID)
                .map_err(verification_failed)?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn is_app(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
}

/// Restore a backed-up bundle, keeping the verified N-1 copy. The coordinator
/// holds update.lock and checks schema/session policy before calling this.
#[cfg(target_os = "macos")]
pub fn restore_rollback_app(
    rollback_dir: &Path,
    record: &RollbackRecord,
    relative_app: &str,
    destination: &Path,
) -> Result<(), UpdateError> {
    use crate::macos::{
        EXPECTED_APPLE_TEAM_ID, EXPECTED_BUNDLE_ID, replace_bundle, verify_macos_app_bundle,
    };
    verify_rollback_set(rollback_dir, record)?;
    let app = regular_path(rollback_dir, relative_app).map_err(verification_failed)?;
    if !is_app(&app) || !is_app(destination) {
        return Err(verification_failed(
            "Expected a .app rollback source and destination",
        ));
    }
    let binaries: Vec<_> = record
        .binary_sha256s
        .iter()
        .filter_map(|(p, h)| {
            let relative = Path::new(p).strip_prefix(relative_app).ok()?.to_str()?;
            (!relative.is_empty()).then_some((relative, h.as_str()))
        })
        .collect();
    if binaries.is_empty() {
        return Err(verification_failed("App has no recorded rollback digests"));
    }
    replace_bundle(&app, destination, None, |candidate| {
        verify_files(candidate, binaries.iter().copied()).map_err(verification_failed)?;
        verify_macos_app_bundle(candidate, EXPECTED_APPLE_TEAM_ID, EXPECTED_BUNDLE_ID)
            .map_err(verification_failed)
    })
}

/// Restore one member atomically, retaining the N-1 backup. The coordinator
/// holds update.lock and checks schema/session policy before calling this.
pub fn restore_rollback_binary(
    rollback_dir: &Path,
    record: &RollbackRecord,
    relative: &str,
    destination: &Path,
) -> Result<(), UpdateError> {
    verify_rollback_set(rollback_dir, record)?;
    let digest = record
        .binary_sha256s
        .get(relative)
        .ok_or_else(|| verification_failed("Binary is not part of the verified rollback set"))?;
    let source = regular_path(rollback_dir, relative).map_err(verification_failed)?;
    copy_verified(&source, destination, true, |tmp| {
        verify_file(tmp, digest).map_err(verification_failed)
    })
}
