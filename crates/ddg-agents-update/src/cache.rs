//! Versioned dist cache; persisted bytes are never a verification marker.

use crate::{
    UpdateError, UpdateErrorCode as Code,
    manifest::ReleaseManifest,
    swap::{is_safe_segment, open_regular, plain_dir, storage_error, verify_file, verify_files},
    version::ReleaseTag,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

/// Recheck the on-disk manifest against the caller's trusted release manifest
/// (signature-verified when keys are configured), then hash the actual cache files.
/// The caller must hold update.lock while using or evicting this entry.
pub fn verify_cached_artifact(
    dist_dir: &Path,
    expected: &ReleaseManifest,
    platform: &str,
) -> Result<PathBuf, UpdateError> {
    let tag = ReleaseTag::parse(&expected.tag)?;
    if !is_safe_segment(platform) || tag.version().to_string() != expected.version {
        return Err(UpdateError::new(
            Code::ManifestInvalid,
            "Invalid cache version or platform",
        ));
    }
    let asset = expected
        .artifacts
        .iter()
        .find(|asset| asset.platform == platform)
        .ok_or_else(|| {
            UpdateError::new(
                Code::AssetNotFound,
                "No artifact for the requested platform",
            )
        })?;
    if !is_safe_segment(&asset.name) {
        return Err(UpdateError::new(
            Code::ManifestInvalid,
            "Invalid cache artifact name",
        ));
    }
    plain_dir(dist_dir).map_err(storage_error)?;
    let version_dir = dist_dir.join(&expected.version);
    plain_dir(&version_dir).map_err(storage_error)?;
    let entry = version_dir.join(platform);
    plain_dir(&entry).map_err(storage_error)?;

    let result = (|| {
        const MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;
        let mut bytes = Vec::new();
        open_regular(&entry.join("manifest.json"))
            .map_err(|e| UpdateError::new(Code::ManifestInvalid, e.to_string()))?
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(storage_error)?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES
            || ReleaseManifest::parse(&bytes, &tag)? != *expected
        {
            return Err(UpdateError::new(
                Code::ManifestInvalid,
                "Cached manifest does not match the trusted manifest",
            ));
        }
        let archive = entry.join(&asset.name);
        verify_file(&archive, &asset.sha256)?;
        if let Some(size) = asset.size
            && fs::symlink_metadata(&archive).map_err(storage_error)?.len() != size
        {
            return Err(UpdateError::new(
                Code::ChecksumMismatch,
                "Cached archive size does not match its manifest",
            ));
        }
        verify_files(
            &entry,
            asset
                .binaries
                .values()
                .map(|b| (b.path.as_str(), b.sha256.as_str())),
        )?;
        Ok(archive)
    })();
    if let Err(error) = &result
        && matches!(error.code, Code::ManifestInvalid | Code::ChecksumMismatch)
    {
        fs::remove_dir_all(&entry).map_err(|e| {
            UpdateError::new(error.code, format!("{error}; cache eviction failed: {e}"))
        })?;
    }
    result
}

/// Run under update.lock so the pin set cannot change during eviction.
pub fn gc_dist_cache(dist_dir: &Path, pins: &BTreeSet<String>) -> Result<(), UpdateError> {
    match plain_dir(dist_dir) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(storage_error(err)),
    }
    let pins: BTreeSet<_> = pins
        .iter()
        .map(|v| v.strip_prefix('v').unwrap_or(v))
        .collect();
    let mut versions = Vec::new();
    let mut platforms: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for version in fs::read_dir(dist_dir).map_err(storage_error)? {
        let version = version.map_err(storage_error)?;
        if !version.file_type().map_err(storage_error)?.is_dir() {
            continue;
        }
        let Some(name) = version.file_name().into_string().ok() else {
            continue;
        };
        let Ok(tag) = ReleaseTag::parse(&name) else {
            continue;
        };
        let path = version.path();
        let pinned = pins.contains(tag.version().to_string().as_str());
        for platform in fs::read_dir(&path).map_err(storage_error)? {
            let platform = platform.map_err(storage_error)?;
            if platform.file_type().map_err(storage_error)?.is_dir() {
                platforms.entry(platform.file_name()).or_default().push((
                    tag.version().clone(),
                    platform.path(),
                    pinned,
                ));
            }
        }
        if !pinned {
            versions.push(path);
        }
    }
    for entries in platforms.values_mut() {
        entries.sort_unstable_by(|a, b| b.0.cmp_precedence(&a.0).then_with(|| a.1.cmp(&b.1)));
        for (_, path, pinned) in entries.iter().skip(2) {
            if !pinned {
                fs::remove_dir_all(path).map_err(storage_error)?;
            }
        }
    }
    for path in versions {
        match fs::remove_dir(path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => {}
            Err(err) => return Err(storage_error(err)),
        }
    }
    Ok(())
}
