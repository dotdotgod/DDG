//! Resolve signed, exact-version Darwin daemons for cross-architecture SSH installs.

use crate::{UpdateError, UpdateErrorCode as Code, macos::macho_cpu};
use std::path::PathBuf;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedBootstrapDaemons {
    pub host_bin: PathBuf,
    pub host_sha256: String,
    pub terminal_server_bin: PathBuf,
    pub terminal_server_sha256: String,
    pub version: String,
}

pub async fn resolve_verified_darwin_daemons(
    target_platform: &str,
    client_version: &str,
    progress: Option<&UnboundedSender<String>>,
) -> Result<VerifiedBootstrapDaemons, UpdateError> {
    macho_cpu(target_platform)?;
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (client_version, progress);
        Err(UpdateError::new(
            Code::CodesignInvalid,
            "Darwin app artifacts require a macOS client for Apple code signature verification",
        ))
    }
    #[cfg(target_os = "macos")]
    {
        use resolver::*;
        let tag = exact_tag(client_version)?;
        let paths = UpdatePaths::from_environment()?;
        let (repo, _) =
            crate::config::UpdateConfig::load(&paths.config_file())?.effective_repo()?;
        resolve(
            paths,
            &repo,
            tag,
            target_platform,
            progress,
            fetch_bytes,
            download_bytes,
            APPLE_BUNDLE_VERIFIER,
        )
        .await
    }
}

#[cfg(not(target_os = "macos"))]
#[cfg(test)]
#[tokio::test]
async fn darwin_bootstrap_requires_a_macos_client() {
    for platform in ["darwin-arm64", "darwin-x86_64"] {
        let error = resolve_verified_darwin_daemons(platform, "1.0.0", None)
            .await
            .unwrap_err();
        assert_eq!(error.code, Code::CodesignInvalid);
    }
}

#[cfg(any(target_os = "macos", test))]
mod resolver {
    use super::*;
    use crate::{
        archive::extract_zip,
        cache::verify_cached_artifact,
        config::RepoSlug,
        github::MAX_JSON_BYTES,
        integrity::{RELEASE_PUBKEYS, verify_manifest},
        macos::verify_macho_arch,
        manifest::{ReleaseArtifact, ReleaseManifest},
        swap::{copy_verified, open_regular, storage_error, verify_file, verify_files},
        version::ReleaseTag,
    };
    use ddg_agents_os::{
        fs::{ensure_private_dir, write_atomic},
        lock::FileLock,
    };
    use ddg_agents_proto::terminal::PROTOCOL_VERSION;
    use std::{
        fs,
        io::{self, Read},
        path::Path,
        time::Duration,
    };

    pub(super) use crate::manager::UpdatePaths;
    const HOST: &str = "ddg-agents-host";
    const TERMINAL: &str = "ddg-terminal-server";
    #[derive(Clone, Copy)]
    pub(super) struct BundleVerifier {
        assess: fn(&Path) -> Result<(), UpdateError>,
        pins: fn(&Path, &str, &str) -> Result<(), UpdateError>,
    }

    #[cfg(target_os = "macos")]
    pub(super) const APPLE_BUNDLE_VERIFIER: BundleVerifier = BundleVerifier {
        assess: assess_apple_bundle,
        pins: verify_bundle_pins,
    };

    pub(super) fn exact_tag(client_version: &str) -> Result<ReleaseTag, UpdateError> {
        ReleaseTag::parse(&format!("v{}", client_version.trim_start_matches('v')))
    }

    #[cfg(target_os = "macos")]
    pub(super) async fn fetch_bytes(url: String) -> Result<Vec<u8>, UpdateError> {
        use crate::github::{self, FetchResult};
        match github::fetch_json(&url, None).await? {
            FetchResult::Modified { body, .. } => Ok(body),
            _ => Err(UpdateError::new(
                Code::ManifestInvalid,
                "Unexpected release metadata response",
            )),
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) async fn download_bytes(
        url: String,
        path: PathBuf,
        sha256: String,
    ) -> Result<(), UpdateError> {
        crate::github::download_archive(&url, &path, &sha256).await
    }

    #[cfg(target_os = "macos")]
    fn assess_apple_bundle(app: &Path) -> Result<(), UpdateError> {
        use crate::macos::{EXPECTED_APPLE_TEAM_ID, EXPECTED_BUNDLE_ID, verify_macos_app_bundle};
        verify_macos_app_bundle(app, EXPECTED_APPLE_TEAM_ID, EXPECTED_BUNDLE_ID)
    }

    #[cfg(target_os = "macos")]
    fn verify_bundle_pins(app: &Path, version: &str, digests: &str) -> Result<(), UpdateError> {
        let invalid = |key| {
            UpdateError::new(
                Code::CodesignInvalid,
                format!("Signed app {key} does not match the release manifest"),
            )
        };
        let plist = crate::swap::regular_path(app, "Contents/Info.plist")
            .map_err(|_| invalid("Info.plist"))?;
        open_regular(&plist).map_err(|_| invalid("Info.plist"))?;
        // The signed numeric CFBundleVersion omits prereleases; nested code seals need not pin exact bytes.
        for (key, expected) in [
            ("DDGReleaseVersion", version),
            ("DDGBootstrapSHA256", digests),
        ] {
            let result = std::process::Command::new("/usr/libexec/PlistBuddy")
                .args(["-c", &format!("Print :{key}")])
                .arg(&plist)
                .output()
                .map_err(|_| invalid(key))?;
            if !result.status.success()
                || result.stdout.strip_suffix(b"\n") != Some(expected.as_bytes())
            {
                return Err(invalid(key));
            }
        }
        Ok(())
    }

    struct Layout<'a> {
        app: &'a Path,
        artifact: &'a ReleaseArtifact,
        version: &'a str,
    }

    impl<'a> Layout<'a> {
        fn new(manifest: &'a ReleaseManifest, platform: &str) -> Result<Self, UpdateError> {
            if manifest.protocol_version != PROTOCOL_VERSION {
                return Err(UpdateError::new(
                    Code::ProtocolIncompatible,
                    "Release protocol is incompatible",
                ));
            }
            let artifact = manifest
                .artifacts
                .iter()
                .find(|asset| asset.platform == platform)
                .ok_or_else(|| {
                    UpdateError::new(
                        Code::AssetNotFound,
                        "No Darwin artifact for the target platform",
                    )
                })?;
            let invalid = || {
                UpdateError::new(
                    Code::ArchiveUnsafe,
                    "Darwin ZIP binaries must be in the same app's Contents/MacOS directory",
                )
            };
            let host = artifact.binaries.get(HOST).ok_or_else(invalid)?;
            let terminal = artifact.binaries.get(TERMINAL).ok_or_else(invalid)?;
            let app = Path::new(&host.path)
                .ancestors()
                .nth(3)
                .ok_or_else(invalid)?;
            let execs = app.join("Contents/MacOS");
            if !artifact.name.ends_with(".zip")
                || app.components().count() != 1
                || !app
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
                || Path::new(&host.path) != execs.join(HOST)
                || Path::new(&terminal.path) != execs.join(TERMINAL)
                || artifact
                    .binaries
                    .values()
                    .any(|binary| Path::new(&binary.path).parent() != Some(&execs))
            {
                return Err(invalid());
            }
            Ok(Self {
                app,
                artifact,
                version: &manifest.version,
            })
        }

        fn verify_archive(&self, path: &Path) -> Result<(), UpdateError> {
            let artifact = self.artifact;
            verify_file(path, &artifact.sha256)?;
            if let Some(size) = artifact.size
                && fs::symlink_metadata(path).map_err(storage_error)?.len() != size
            {
                return Err(UpdateError::new(
                    Code::ChecksumMismatch,
                    "Darwin ZIP size does not match its manifest",
                ));
            }
            Ok(())
        }

        fn verify_bundle(&self, root: &Path, verify: BundleVerifier) -> Result<(), UpdateError> {
            let app = root.join(self.app);
            (verify.assess)(&app)?;
            let daemons = self.daemons(root);
            let digests = format!(
                "{}:{}",
                daemons.host_sha256.to_ascii_lowercase(),
                daemons.terminal_server_sha256.to_ascii_lowercase()
            );
            (verify.pins)(&app, self.version, &digests)?;
            verify_macho_arch(&daemons.host_bin, &self.artifact.platform)?;
            verify_macho_arch(&daemons.terminal_server_bin, &self.artifact.platform)
        }

        fn daemons(&self, root: &Path) -> VerifiedBootstrapDaemons {
            let host = &self.artifact.binaries[HOST];
            let terminal = &self.artifact.binaries[TERMINAL];
            VerifiedBootstrapDaemons {
                host_bin: root.join(&host.path),
                host_sha256: host.sha256.clone(),
                terminal_server_bin: root.join(&terminal.path),
                terminal_server_sha256: terminal.sha256.clone(),
                version: self.version.to_owned(),
            }
        }
    }

    async fn on_disk<T: Send + 'static>(
        op: impl FnOnce() -> Result<T, UpdateError> + Send + 'static,
    ) -> Result<T, UpdateError> {
        tokio::task::spawn_blocking(op)
            .await
            .map_err(storage_error)?
    }

    fn optional_dir(path: &Path) -> Result<bool, UpdateError> {
        match path.symlink_metadata() {
            Ok(metadata) if metadata.is_dir() => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(storage_error(error)),
            Ok(_) => Err(UpdateError::new(
                Code::HostSecurityBlocked,
                "Darwin cache path is not a real directory",
            )),
        }
    }

    #[derive(Clone)]
    struct CacheSlot {
        paths: UpdatePaths,
        version_dir: PathBuf,
        entry: PathBuf,
    }

    impl CacheSlot {
        fn prepare(
            mut paths: UpdatePaths,
            tag: &ReleaseTag,
            platform: &str,
        ) -> Result<Self, UpdateError> {
            // As elsewhere in the updater, configured roots are under trusted ancestors.
            for path in [&mut paths.state_dir, &mut paths.dist_dir] {
                ensure_private_dir(path).map_err(storage_error)?;
                *path = path.canonicalize().map_err(storage_error)?;
            }
            ensure_private_dir(&paths.staging_dir()).map_err(storage_error)?;
            let version_dir = paths.dist_dir.join(tag.version().to_string());
            let entry = version_dir.join(platform);
            Ok(Self {
                paths,
                version_dir,
                entry,
            })
        }

        async fn locked<T: Send + 'static>(
            &self,
            op: impl FnOnce(&Self) -> Result<T, UpdateError> + Send + 'static,
        ) -> Result<T, UpdateError> {
            let slot = self.clone();
            on_disk(move || {
                ensure_private_dir(&slot.paths.state_dir).map_err(storage_error)?;
                ensure_private_dir(&slot.paths.dist_dir).map_err(storage_error)?;
                let path = slot.paths.state_dir.join("update.lock");
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let _lock = loop {
                    match path.symlink_metadata() {
                        Ok(metadata) if !metadata.is_file() => {
                            return Err(UpdateError::new(
                                Code::HostSecurityBlocked,
                                "update.lock is not a regular file",
                            ));
                        }
                        Ok(_) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(storage_error(error)),
                    }
                    match FileLock::try_lock_exclusive(&path) {
                        Ok(lock) => break lock,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            if std::time::Instant::now() >= deadline {
                                return Err(UpdateError::new(
                                    Code::LockBusy,
                                    "Another update operation holds update.lock",
                                ));
                            }
                            // shortcut: bound synchronous lock retries to five seconds; use a cancellable lock API if contention grows.
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        Err(error) => return Err(storage_error(error)),
                    }
                };
                op(&slot)
            })
            .await
        }

        fn check(
            &self,
            tag: &ReleaseTag,
            platform: &str,
            expected: Option<&ReleaseManifest>,
            verify: BundleVerifier,
        ) -> Result<Option<VerifiedBootstrapDaemons>, UpdateError> {
            if !optional_dir(&self.version_dir)? || !optional_dir(&self.entry)? {
                return Ok(None);
            }
            ensure_private_dir(&self.version_dir).map_err(storage_error)?;
            ensure_private_dir(&self.entry).map_err(storage_error)?;
            let result = (|| {
                let raw = read_metadata(&self.entry.join("manifest.json"), Code::ManifestInvalid)?;
                let signature = if RELEASE_PUBKEYS.is_empty() {
                    None
                } else {
                    Some(read_metadata(
                        &self.entry.join("manifest.json.sig"),
                        Code::SignatureInvalid,
                    )?)
                };
                let disk = verify_manifest(&raw, signature.as_deref(), tag)?;
                if expected.is_some_and(|manifest| *manifest != disk) {
                    return Err(UpdateError::new(
                        Code::ManifestInvalid,
                        "Cached manifest does not match the exact online release",
                    ));
                }
                let layout = Layout::new(&disk, platform)?;
                verify_cached_artifact(&self.paths.dist_dir, &disk, platform)?;
                layout.verify_bundle(&self.entry, verify)?;
                Ok(layout.daemons(&self.entry))
            })();
            match result {
                Ok(daemons) => Ok(Some(daemons)),
                Err(error)
                    if matches!(
                        error.code,
                        Code::ManifestInvalid
                            | Code::ChecksumMismatch
                            | Code::SignatureMissing
                            | Code::SignatureInvalid
                            | Code::AssetNotFound
                            | Code::ProtocolIncompatible
                            | Code::ArchiveUnsafe
                            | Code::CodesignInvalid
                            | Code::NotarizationInvalid
                    ) =>
                {
                    // The shared hash checker may already have removed this entry.
                    match fs::remove_dir_all(&self.entry) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(UpdateError::new(
                                error.code,
                                format!("{error}; cache eviction failed: {e}"),
                            ));
                        }
                    }
                    if expected.is_some() {
                        Ok(None)
                    } else {
                        Err(error)
                    }
                }
                Err(error) => Err(error),
            }
        }
    }

    fn read_metadata(path: &Path, code: Code) -> Result<Vec<u8>, UpdateError> {
        match path.symlink_metadata() {
            Ok(metadata) if metadata.is_file() => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let code = if code == Code::SignatureInvalid {
                    Code::SignatureMissing
                } else {
                    code
                };
                return Err(UpdateError::new(code, "Cached release metadata is missing"));
            }
            Err(error) => return Err(storage_error(error)),
            Ok(_) => {
                return Err(UpdateError::new(
                    code,
                    "Cached release metadata is not a regular file",
                ));
            }
        }
        let mut bytes = Vec::new();
        open_regular(path)
            .map_err(storage_error)?
            .take(MAX_JSON_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(storage_error)?;
        if bytes.len() as u64 > MAX_JSON_BYTES {
            return Err(UpdateError::new(
                code,
                "Cached release metadata exceeds the size limit",
            ));
        }
        Ok(bytes)
    }

    // Hooks keep the cache and archive checks real while tests replace GitHub and Apple tools.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn resolve<F, D>(
        paths: UpdatePaths,
        repo: &RepoSlug,
        tag: ReleaseTag,
        platform: &str,
        progress: Option<&UnboundedSender<String>>,
        fetch: impl Fn(String) -> F,
        download: impl Fn(String, PathBuf, String) -> D,
        verify: BundleVerifier,
    ) -> Result<VerifiedBootstrapDaemons, UpdateError>
    where
        F: std::future::Future<Output = Result<Vec<u8>, UpdateError>>,
        D: std::future::Future<Output = Result<(), UpdateError>>,
    {
        macho_cpu(platform)?;
        let emit = |message| {
            if let Some(progress) = progress {
                let _ = progress.send(message);
            }
        };
        let tag_path = tag.as_str().replace('+', "%2B");
        let base = format!(
            "https://github.com/{}/releases/download/{tag_path}",
            repo.as_str()
        );
        let target = platform.to_owned();
        let slot = on_disk({
            let tag = tag.clone();
            let target = target.clone();
            move || CacheSlot::prepare(paths, &tag, &target)
        })
        .await?;
        emit(format!(
            "Resolving verified {platform} release ({})...",
            tag.as_str()
        ));
        let raw = match fetch(format!("{base}/manifest.json")).await {
            Ok(raw) => raw,
            Err(error) if error.code == Code::NetworkError => {
                let context = format!(
                    "No verified cached release for {} ({platform}) is available offline",
                    tag.as_str()
                );
                return match slot
                    .locked(move |slot| slot.check(&tag, &target, None, verify))
                    .await
                {
                    Ok(Some(daemons)) => Ok(daemons),
                    Ok(None) => Err(UpdateError::new(error.code, format!("{context}: {error}"))),
                    Err(cached) => Err(UpdateError::new(
                        cached.code,
                        format!("{context}: {cached}"),
                    )),
                };
            }
            Err(error) => return Err(error),
        };
        let signature = if RELEASE_PUBKEYS.is_empty() {
            None
        } else {
            match fetch(format!("{base}/manifest.json.sig")).await {
                Ok(signature) => Some(signature),
                Err(error) if error.code == Code::AssetNotFound => None,
                Err(error) => return Err(error),
            }
        };
        let manifest = verify_manifest(&raw, signature.as_deref(), &tag)?;
        let layout = Layout::new(&manifest, platform)?;
        if let Some(daemons) = slot
            .locked({
                let (tag, target, manifest) = (tag.clone(), target.clone(), manifest.clone());
                move |slot| slot.check(&tag, &target, Some(&manifest), verify)
            })
            .await?
        {
            return Ok(daemons);
        }

        // No cache lock across either download; the final publish rechecks the slot.
        let stage = tempfile::Builder::new()
            .prefix("bootstrap-")
            .tempdir_in(slot.paths.staging_dir())
            .map_err(storage_error)?;
        ensure_private_dir(stage.path()).map_err(storage_error)?;
        let archive_path = stage.path().join("release.zip");
        let artifact = layout.artifact;
        emit(format!(
            "Downloading {platform} release archive ({})...",
            tag.as_str()
        ));
        download(
            format!("{base}/{}", artifact.name),
            archive_path.clone(),
            artifact.sha256.clone(),
        )
        .await?;
        emit(format!(
            "Verifying macOS app signature and {platform} binaries..."
        ));
        let publish = on_disk({
            let manifest = manifest.clone();
            let target = target.clone();
            move || {
                let layout = Layout::new(&manifest, &target)?;
                let artifact = layout.artifact;
                layout.verify_archive(&archive_path)?;
                let extracted = stage.path().join("extracted");
                extract_zip(
                    open_regular(&archive_path).map_err(storage_error)?,
                    &extracted,
                )?;
                verify_files(
                    &extracted,
                    artifact
                        .binaries
                        .values()
                        .map(|b| (b.path.as_str(), b.sha256.as_str())),
                )?;
                layout.verify_bundle(&extracted, verify)?;

                let publish = stage.path().join("publish");
                ensure_private_dir(&publish).map_err(storage_error)?;
                fs::rename(extracted.join(layout.app), publish.join(layout.app))
                    .map_err(storage_error)?;
                fs::rename(archive_path, publish.join(&artifact.name)).map_err(storage_error)?;
                write_atomic(&publish.join("manifest.json"), &raw).map_err(storage_error)?;
                if let Some(signature) = signature {
                    write_atomic(&publish.join("manifest.json.sig"), &signature)
                        .map_err(storage_error)?;
                }
                Ok((stage, publish))
            }
        })
        .await?;
        slot.locked(move |slot| {
            let (_stage, publish) = publish;
            if let Some(daemons) = slot.check(&tag, &target, Some(&manifest), verify)? {
                return Ok(daemons);
            }
            ensure_private_dir(&slot.version_dir).map_err(storage_error)?;
            let layout = Layout::new(&manifest, &target)?;
            // DDG_DIST_DIR may be on another volume. Only the verified sibling copy is renamed into place.
            copy_verified(&publish, &slot.entry, false, |incoming| {
                layout.verify_bundle(incoming, verify)
            })?;
            Ok(layout.daemons(&slot.entry))
        })
        .await
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use sha2::{Digest, Sha256};
        use std::io::{Cursor, Write};
        use zip::{ZipWriter, write::SimpleFileOptions};

        const TARGET: &str = "darwin-x86_64";
        const VERSION: &str = "1.2.3-rc.1+build";
        const NAME: &str = "ddg-agents.app.zip";
        const HOST_PATH: &str = "DDG Agents.app/Contents/MacOS/ddg-agents-host";
        const TERMINAL_PATH: &str = "DDG Agents.app/Contents/MacOS/ddg-terminal-server";
        const PLIST_PATH: &str = "DDG Agents.app/Contents/Info.plist";
        const BASE: &str = "https://github.com/dotdotgod/DDG/releases/download/v1.2.3-rc.1%2Bbuild";

        fn no_download(
            _: String,
            _: PathBuf,
            _: String,
        ) -> std::future::Ready<Result<(), UpdateError>> {
            panic!("must not download")
        }

        fn hex(bytes: &[u8]) -> String {
            Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        }

        fn fixture_macho(cpu: u32) -> [u8; 32] {
            let mut bytes = [0; 32];
            bytes[..4].copy_from_slice(&0xfeed_facfu32.to_le_bytes());
            bytes[4..8].copy_from_slice(&cpu.to_le_bytes());
            bytes
        }

        fn plist(version: Option<&str>, digests: Option<&str>) -> String {
            let version = version.map_or_else(String::new, |v| {
                format!("<key>DDGReleaseVersion</key><string>{v}</string>")
            });
            let digests = digests.map_or_else(String::new, |v| {
                format!("<key>DDGBootstrapSHA256</key><string>{v}</string>")
            });
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict>{version}{digests}</dict></plist>"#
            )
        }

        fn test_pins(app: &Path, version: &str, digests: &str) -> Result<(), UpdateError> {
            #[cfg(target_os = "macos")]
            {
                verify_bundle_pins(app, version, digests)
            }
            #[cfg(not(target_os = "macos"))]
            {
                if fs::read_to_string(app.join("Contents/Info.plist")).unwrap()
                    != plist(Some(version), Some(digests))
                {
                    return Err(UpdateError::new(
                        Code::CodesignInvalid,
                        "mock signed pins differ",
                    ));
                }
                Ok(())
            }
        }

        const TEST_BUNDLE: BundleVerifier = BundleVerifier {
            assess: |_| Ok(()),
            pins: test_pins,
        };

        struct Fixture {
            temp: tempfile::TempDir,
            raw: Vec<u8>,
            archive: Vec<u8>,
            repo: RepoSlug,
        }

        impl Fixture {
            fn new(
                terminal_cpu: u32,
                version: &str,
                pins_version: &str,
                pins_hash: Option<&str>,
            ) -> Self {
                let host = fixture_macho(0x0100_0007);
                let terminal = fixture_macho(terminal_cpu);
                let host_hash = hex(&host);
                let terminal_hash = hex(&terminal);
                let pins = format!("{host_hash}:{terminal_hash}");
                let plist = plist(Some(pins_version), Some(pins_hash.unwrap_or(&pins)));
                let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
                for (path, bytes) in [
                    (HOST_PATH, host.as_slice()),
                    (TERMINAL_PATH, terminal.as_slice()),
                    (PLIST_PATH, plist.as_bytes()),
                ] {
                    zip.start_file(path, SimpleFileOptions::default()).unwrap();
                    zip.write_all(bytes).unwrap();
                }
                let archive = zip.finish().unwrap().into_inner();
                let raw = serde_json::to_vec(&serde_json::json!({
                    "schema_version": 1,
                    "version": version,
                    "tag": format!("v{version}"),
                    "channel": if version.contains('-') { "prerelease" } else { "stable" },
                    "published_at": "2026-01-01T12:00:00Z",
                    "protocol_version": PROTOCOL_VERSION,
                    "holder_protocol_version": 1,
                    "holder_restart_required": false,
                    "min_compatible_schema": 1,
                    "release_notes_summary": "",
                    "artifacts": [{
                        "platform": TARGET,
                        "name": NAME,
                        "size": archive.len(),
                        "sha256": hex(&archive),
                        "binaries": {
                            (HOST): { "path": HOST_PATH, "sha256": host_hash },
                            (TERMINAL): { "path": TERMINAL_PATH, "sha256": terminal_hash }
                        }
                    }]
                }))
                .unwrap();
                Self {
                    temp: tempfile::tempdir().unwrap(),
                    raw,
                    archive,
                    repo: RepoSlug::parse("dotdotgod/DDG").unwrap(),
                }
            }

            fn valid() -> Self {
                Self::new(0x0100_0007, VERSION, VERSION, None)
            }

            fn paths(&self) -> UpdatePaths {
                UpdatePaths {
                    config_dir: self.temp.path().join("config"),
                    state_dir: self.temp.path().join("state"),
                    dist_dir: self.temp.path().join("dist"),
                }
            }

            fn entry(&self) -> PathBuf {
                self.paths().dist_dir.join(VERSION).join(TARGET)
            }

            fn seed(&self) {
                let entry = self.entry();
                fs::create_dir_all(entry.parent().unwrap()).unwrap();
                extract_zip(Cursor::new(&self.archive), &entry).unwrap();
                fs::write(entry.join(NAME), &self.archive).unwrap();
                fs::write(entry.join("manifest.json"), &self.raw).unwrap();
            }

            async fn resolve<F, D>(
                &self,
                fetch: impl Fn(String) -> F,
                download: impl Fn(String, PathBuf, String) -> D,
                bundle: BundleVerifier,
            ) -> Result<VerifiedBootstrapDaemons, UpdateError>
            where
                F: std::future::Future<Output = Result<Vec<u8>, UpdateError>>,
                D: std::future::Future<Output = Result<(), UpdateError>>,
            {
                resolve(
                    self.paths(),
                    &self.repo,
                    exact_tag(VERSION).unwrap(),
                    TARGET,
                    None,
                    fetch,
                    download,
                    bundle,
                )
                .await
            }

            async fn fail_fetch(
                &self,
                code: Code,
            ) -> Result<VerifiedBootstrapDaemons, UpdateError> {
                self.resolve(
                    |_| async move { Err(UpdateError::new(code, "test failure")) },
                    no_download,
                    TEST_BUNDLE,
                )
                .await
            }
        }

        #[tokio::test]
        async fn bootstrap_pins_exact_tag_and_reuses_verified_online_cache() {
            let f = Fixture::valid();
            f.seed();
            assert_eq!(
                exact_tag("v1.2.3-rc.1+build").unwrap().as_str(),
                format!("v{VERSION}")
            );
            assert!(exact_tag("latest").is_err());
            let verified = f
                .resolve(
                    |url| {
                        assert_eq!(url, format!("{BASE}/manifest.json"));
                        std::future::ready(Ok(f.raw.clone()))
                    },
                    no_download,
                    TEST_BUNDLE,
                )
                .await
                .unwrap();
            let expected = f.entry().canonicalize().unwrap();
            assert_eq!(verified.host_bin, expected.join(HOST_PATH));
            assert_eq!(verified.terminal_server_bin, expected.join(TERMINAL_PATH));
            assert_eq!(verified.version, VERSION);
        }

        #[tokio::test]
        async fn bootstrap_offline_requires_an_exact_intact_signed_bundle_and_evicts_tampering() {
            let f = Fixture::valid();
            let missing = f.fail_fetch(Code::NetworkError).await.unwrap_err();
            assert_eq!(missing.code, Code::NetworkError);
            assert!(missing.message().contains("No verified cached release"));
            f.seed();
            let daemons = f.fail_fetch(Code::NetworkError).await.unwrap();
            assert_eq!(daemons.version, VERSION);
            assert_eq!(
                daemons.host_sha256,
                hex(&fs::read(&daemons.host_bin).unwrap())
            );
            assert_eq!(
                daemons.terminal_server_sha256,
                hex(&fs::read(&daemons.terminal_server_bin).unwrap())
            );
            fs::write(f.entry().join(HOST_PATH), b"tampered").unwrap();
            assert_eq!(
                f.fail_fetch(Code::NetworkError).await.unwrap_err().code,
                Code::ChecksumMismatch
            );
            assert!(!f.entry().exists());

            for (fixture, expected) in [
                (
                    Fixture::new(0x0100_0007, "1.2.3", "1.2.3", None),
                    Code::ManifestInvalid,
                ),
                (
                    Fixture::new(0x0100_0007, VERSION, "1.2.3-rc.0+build", None),
                    Code::CodesignInvalid,
                ),
                (
                    Fixture::new(0x0100_0007, VERSION, VERSION, Some("old daemon digests")),
                    Code::CodesignInvalid,
                ),
                (
                    Fixture::new(0x0100_000c, VERSION, VERSION, None),
                    Code::ArchiveUnsafe,
                ),
            ] {
                fixture.seed();
                assert_eq!(
                    fixture
                        .fail_fetch(Code::NetworkError)
                        .await
                        .unwrap_err()
                        .code,
                    expected
                );
                assert!(!fixture.entry().exists());
            }
        }

        #[tokio::test]
        async fn bootstrap_online_errors_never_use_offline_or_flat_cache() {
            let f = Fixture::valid();
            f.seed();
            let flat = f.paths().dist_dir.join(TARGET);
            fs::create_dir(&flat).unwrap();
            for name in [HOST, TERMINAL] {
                fs::write(flat.join(name), b"unverified").unwrap();
            }
            for code in [
                Code::AssetNotFound,
                Code::DisallowedUrlOrHost,
                Code::RateLimited,
                Code::ManifestInvalid,
            ] {
                assert_eq!(f.fail_fetch(code).await.unwrap_err().code, code);
                assert!(f.entry().exists());
            }
            // A successful but malformed online response is also authoritative.
            let error = f
                .resolve(
                    |_| async { Ok(b"not JSON".to_vec()) },
                    no_download,
                    TEST_BUNDLE,
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, Code::ManifestInvalid);
            fs::remove_dir_all(f.entry()).unwrap();
            assert_eq!(
                f.fail_fetch(Code::NetworkError).await.unwrap_err().code,
                Code::NetworkError
            );
        }

        #[tokio::test]
        async fn bootstrap_rejects_incompatible_protocol_platform_and_daemon_paths() {
            let f = Fixture::valid();
            f.seed();
            let manifest: serde_json::Value = serde_json::from_slice(&f.raw).unwrap();
            let mut wrong_protocol = manifest.clone();
            wrong_protocol["protocol_version"] = (PROTOCOL_VERSION + 1).into();
            let mut wrong_platform = manifest.clone();
            wrong_platform["artifacts"][0]["platform"] = "darwin-arm64".into();
            let mut wrong_path = manifest;
            wrong_path["artifacts"][0]["binaries"][TERMINAL]["path"] =
                "Other.app/Contents/MacOS/ddg-terminal-server".into();
            for (manifest, expected) in [
                (wrong_protocol, Code::ProtocolIncompatible),
                (wrong_platform, Code::AssetNotFound),
                (wrong_path, Code::ArchiveUnsafe),
            ] {
                let raw = serde_json::to_vec(&manifest).unwrap();
                let error = f
                    .resolve(
                        |_| std::future::ready(Ok(raw.clone())),
                        no_download,
                        TEST_BUNDLE,
                    )
                    .await
                    .unwrap_err();
                assert_eq!(error.code, expected);
                assert!(
                    f.entry().exists(),
                    "an invalid online manifest must not evict the cache"
                );
            }
        }

        #[tokio::test]
        async fn bootstrap_stages_then_publishes_or_reuses_a_concurrent_entry() {
            for concurrent in [false, true] {
                let f = Fixture::valid();
                let verified = f
                    .resolve(
                        |_| std::future::ready(Ok(f.raw.clone())),
                        |url, archive_path, sha256| {
                            assert_eq!(url, format!("{BASE}/{NAME}"));
                            assert_eq!(sha256, hex(&f.archive));
                            let paths = f.paths();
                            assert!(
                                archive_path
                                    .starts_with(paths.staging_dir().canonicalize().unwrap())
                            );
                            assert!(!f.entry().exists());
                            let _lock =
                                FileLock::try_lock_exclusive(&paths.state_dir.join("update.lock"))
                                    .unwrap();
                            if concurrent {
                                f.seed();
                                fs::write(f.entry().join("from-other-writer"), b"keep").unwrap();
                            }
                            std::future::ready(
                                fs::write(archive_path, &f.archive).map_err(storage_error),
                            )
                        },
                        TEST_BUNDLE,
                    )
                    .await
                    .unwrap();
                assert!(verified.host_bin.is_file());
                assert!(f.entry().join(NAME).is_file());
                assert_eq!(f.entry().join("from-other-writer").exists(), concurrent);
                assert_eq!(f.paths().staging_dir().read_dir().unwrap().count(), 0);
                assert_eq!(f.fail_fetch(Code::NetworkError).await.unwrap(), verified);
            }
        }

        #[tokio::test]
        async fn bootstrap_denies_unsigned_stage_before_reading_plist_or_publishing() {
            let f = Fixture::valid();
            let bundle = BundleVerifier {
                #[cfg(target_os = "macos")]
                assess: assess_apple_bundle,
                #[cfg(not(target_os = "macos"))]
                assess: |_| Err(UpdateError::new(Code::CodesignInvalid, "unsigned app")),
                pins: |_, _, _| panic!("must assess Apple signature before reading signed pins"),
            };
            let error = f
                .resolve(
                    |_| std::future::ready(Ok(f.raw.clone())),
                    |_, path, _| {
                        std::future::ready(fs::write(path, &f.archive).map_err(storage_error))
                    },
                    bundle,
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, Code::CodesignInvalid);
            assert!(!f.entry().exists());
            assert_eq!(f.paths().staging_dir().read_dir().unwrap().count(), 0);
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn bootstrap_rejects_cache_symlinks_and_fifo_metadata_without_following_them() {
            use std::os::unix::{ffi::OsStrExt, fs::symlink};
            let f = Fixture::valid();
            f.seed();
            let other = f.entry().with_file_name("other");
            fs::rename(f.entry(), &other).unwrap();
            symlink(&other, f.entry()).unwrap();
            assert_eq!(
                f.fail_fetch(Code::NetworkError).await.unwrap_err().code,
                Code::HostSecurityBlocked
            );
            assert!(other.join(HOST_PATH).is_file());
            fs::remove_file(f.entry()).unwrap();
            fs::rename(&other, f.entry()).unwrap();
            let manifest = f.entry().join("manifest.json");
            fs::remove_file(&manifest).unwrap();
            let fifo = std::ffi::CString::new(manifest.as_os_str().as_bytes()).unwrap();
            // SAFETY: fifo is a valid NUL-terminated pathname in the private fixture.
            assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
            assert_eq!(
                f.fail_fetch(Code::NetworkError).await.unwrap_err().code,
                Code::ManifestInvalid
            );
            assert!(!f.entry().exists());
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn bootstrap_reads_real_plist_pins_exactly_and_rejects_missing_keys() {
            let dir = tempfile::tempdir().unwrap();
            let app = dir.path().join("DDG Agents.app");
            fs::create_dir_all(app.join("Contents")).unwrap();
            let digests = format!("{}:{}", "a".repeat(64), "b".repeat(64));
            for (version, pins, passes) in [
                (Some(VERSION), Some(digests.as_str()), true),
                (Some("1.2.3"), Some(digests.as_str()), false),
                (Some(VERSION), Some("wrong"), false),
                (None, Some(digests.as_str()), false),
                (Some(VERSION), None, false),
            ] {
                fs::write(dir.path().join(PLIST_PATH), plist(version, pins)).unwrap();
                assert_eq!(verify_bundle_pins(&app, VERSION, &digests).is_ok(), passes);
            }
        }
    }
}
