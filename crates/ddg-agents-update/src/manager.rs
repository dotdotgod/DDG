//! Update discovery and planning. Process handoff lives in the host coordinator.

use crate::{
    UpdateError,
    config::UpdateConfig,
    state::{ReleaseSummary, UpdateState, UpdateStateStore},
    swap::storage_error,
};
use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdatePaths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub dist_dir: PathBuf,
}

impl UpdatePaths {
    pub fn from_environment() -> Result<Self, UpdateError> {
        use ddg_agents_os::paths;
        Ok(Self {
            config_dir: paths::config_dir().map_err(storage_error)?,
            state_dir: paths::state_dir().map_err(storage_error)?,
            dist_dir: paths::dist_dir().map_err(storage_error)?,
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("update.json")
    }
    pub fn rollback_dir(&self) -> PathBuf {
        self.state_dir.join("rollback/prev")
    }
    pub fn staging_dir(&self) -> PathBuf {
        self.state_dir.join("staging")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckOutcome {
    Disabled,
    RateLimited {
        reset_at: Option<u64>,
        cached: Option<ReleaseSummary>,
    },
    UpToDate {
        current: String,
        checked_at: u64,
    },
    UpdateAvailable(UpdatePlan),
    FailedVersionBlocked {
        version: String,
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdatePlan {
    pub current: String,
    pub release: ReleaseSummary,
}

/// Non-wire events for GUI or plain-text stderr. Do not serialize as CLI JSON.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateProgressEvent {
    Checking,
    Downloading { received: u64, total: Option<u64> },
    Verifying,
    Staging,
    Applying,
    RollingBack,
}

pub struct UpdateManager {
    paths: UpdatePaths,
    store: UpdateStateStore,
}

impl UpdateManager {
    pub fn new(paths: UpdatePaths) -> Self {
        let store = UpdateStateStore::new(paths.state_dir.clone());
        Self { paths, store }
    }

    pub fn paths(&self) -> &UpdatePaths {
        &self.paths
    }

    pub fn load_config(&self) -> Result<UpdateConfig, UpdateError> {
        UpdateConfig::load(&self.paths.config_file())
    }

    pub fn save_config(&self, config: &UpdateConfig) -> Result<(), UpdateError> {
        ddg_agents_os::fs::ensure_private_dir(&self.paths.config_dir).map_err(storage_error)?;
        config.save(&self.paths.config_file())
    }

    pub fn load_state(&self) -> Result<UpdateState, UpdateError> {
        self.store.load()
    }

    pub fn gc_dist_cache(&self) -> Result<(), UpdateError> {
        let tx = self.store.lock()?;
        crate::cache::gc_dist_cache(&self.paths.dist_dir, &tx.state.cache_pins())
    }
}

#[cfg(feature = "network")]
pub use network::{LocalUpdateOptions, LocalUpdatePlan};

#[cfg(feature = "network")]
mod network {
    use super::*;
    use crate::{
        UpdateErrorCode as Code,
        config::{AutoCheckPolicy, RepoSlug, SourceOrigin, UpdateChannel},
        github::{self, FetchResult},
        integrity::{RELEASE_PUBKEYS, verify_manifest},
        manifest::{ReleaseArtifact, ReleaseManifest},
        release::{GitHubRelease, build_release_web_url, sanitize_release_notes},
        version::{ReleaseTag, validate_apply_consent},
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct LocalUpdateOptions {
        pub platform: String,
        pub force: bool,
        pub allow_downgrade: bool,
        pub restart_sessions: bool,
        pub active_pty_count: u32,
        pub holder_protocol_version: u32,
        pub data_schema_version: u32,
        /// The coordinator's compatibility check against the target's readable schema.
        pub schema_compatible: bool,
        pub container: bool,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct LocalUpdatePlan {
        pub update: UpdatePlan,
        pub source: SourceOrigin,
        pub manifest: ReleaseManifest,
        pub artifact: ReleaseArtifact,
        pub asset_url: String,
    }

    impl UpdateManager {
        /// Automatic checks honor opt-out and return None when update.lock is busy.
        /// Explicit checks may run with auto-check off and surface LockBusy.
        pub async fn check_for_updates(
            &self,
            background: bool,
        ) -> Result<Option<CheckOutcome>, UpdateError> {
            let config = self.load_config()?;
            if background && config.auto_check == AutoCheckPolicy::Off {
                return Ok(Some(CheckOutcome::Disabled));
            }
            let mut tx = if background {
                let Some(tx) = self.store.try_lock()? else {
                    return Ok(None);
                };
                tx
            } else {
                self.store.lock()?
            };
            let (repo, _) = config.effective_repo()?;
            let cached = tx
                .state
                .cached_latest
                .as_ref()
                .and_then(|summary| validated_cache(summary, &repo, config.channel));
            let etag = cached.as_ref().and(tx.state.etag.as_deref());
            let response = github::auto_check(AutoCheckPolicy::On, &repo, etag).await;
            let (latest, etag) = match response {
                Ok(FetchResult::Disabled) => return Ok(Some(CheckOutcome::Disabled)),
                Ok(FetchResult::NotModified { etag }) => (cached, etag),
                Ok(FetchResult::Modified { etag, body }) => {
                    (latest_release(&repo, config.channel, &body)?, etag)
                }
                Err(err) if err.code == Code::RateLimited => {
                    return Ok(Some(CheckOutcome::RateLimited {
                        reset_at: err.rate_limit_reset_at,
                        cached,
                    }));
                }
                Err(err) => return Err(err),
            };
            let checked_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let outcome = check_outcome(&tx.state, latest.as_ref(), checked_at)?;
            tx.state.last_checked_at = Some(checked_at);
            tx.state.etag = etag;
            tx.state.cached_latest = latest;
            tx.save()?;
            Ok(Some(outcome))
        }

        /// Fetch and verify the selected manifest. Force skips the failed-version
        /// record only. The apply coordinator still rechecks all live facts.
        pub async fn plan_local_update(
            &self,
            target: &ReleaseTag,
            options: &LocalUpdateOptions,
        ) -> Result<LocalUpdatePlan, UpdateError> {
            if options.container {
                return Err(UpdateError::new(
                    Code::ContainerUpdateUnsupported,
                    "In-place updates are unsupported for containers",
                ));
            }
            let tx = self.store.lock()?;
            tx.state
                .should_attempt(&target.version().to_string(), options.force)?;
            let config = self.load_config()?;
            if config.channel == UpdateChannel::Stable && !target.version().pre.is_empty() {
                return Err(UpdateError::new(
                    Code::ManifestInvalid,
                    "Select the prerelease channel before applying a prerelease",
                ));
            }
            let current = ReleaseTag::parse(&tx.state.current_version)?;
            validate_apply_consent(
                current.version(),
                target.version(),
                options.schema_compatible,
                false,
                0,
                options.allow_downgrade,
                options.restart_sessions,
            )?;
            let (repo, source) = config.effective_repo()?;
            const TAG_ENCODE: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS.add(b'+');
            let tag = percent_encoding::utf8_percent_encode(target.as_str(), TAG_ENCODE);
            let prefix = format!(
                "https://github.com/{}/releases/download/{tag}",
                repo.as_str()
            );
            let bytes = fetch_manifest_bytes(&format!("{prefix}/manifest.json")).await?;
            let signature = if RELEASE_PUBKEYS.is_empty() {
                None
            } else {
                match fetch_manifest_bytes(&format!("{prefix}/manifest.json.sig")).await {
                    Ok(signature) => Some(signature),
                    Err(err) if err.code == Code::AssetNotFound => None,
                    Err(err) => return Err(err),
                }
            };
            let manifest = verify_manifest(&bytes, signature.as_deref(), target)?;
            validate_plan(&current, target, &manifest, options)?;
            let artifact = manifest
                .artifacts
                .iter()
                .find(|artifact| artifact.platform == options.platform)
                .ok_or_else(|| {
                    UpdateError::new(Code::AssetNotFound, "No artifact for this platform")
                })?
                .clone();
            let update = UpdatePlan {
                current: tx.state.current_version.clone(),
                release: summary(
                    &repo,
                    config.channel,
                    target,
                    sanitize_release_notes(&manifest.release_notes_summary),
                ),
            };
            let asset_url = format!("{prefix}/{}", artifact.name);
            Ok(LocalUpdatePlan {
                update,
                source,
                manifest,
                artifact,
                asset_url,
            })
        }
    }

    async fn fetch_manifest_bytes(url: &str) -> Result<Vec<u8>, UpdateError> {
        match github::fetch_json(url, None).await? {
            FetchResult::Modified { body, .. } => Ok(body),
            _ => Err(UpdateError::new(
                Code::NetworkError,
                "Unexpected response while fetching release metadata",
            )),
        }
    }

    fn summary(
        repo: &RepoSlug,
        channel: UpdateChannel,
        tag: &ReleaseTag,
        notes: String,
    ) -> ReleaseSummary {
        ReleaseSummary {
            version: tag.version().to_string(),
            tag: tag.as_str().to_owned(),
            repo: repo.as_str().to_owned(),
            check_channel: channel,
            notes,
            web_url: build_release_web_url(repo, tag),
        }
    }

    fn validated_cache(
        cached: &ReleaseSummary,
        repo: &RepoSlug,
        channel: UpdateChannel,
    ) -> Option<ReleaseSummary> {
        if cached.repo != repo.as_str() || cached.check_channel != channel {
            return None;
        }
        let tag = ReleaseTag::parse(&cached.tag).ok()?;
        if channel == UpdateChannel::Stable && !tag.version().pre.is_empty() {
            return None;
        }
        Some(summary(
            repo,
            channel,
            &tag,
            sanitize_release_notes(&cached.notes),
        ))
    }

    fn latest_release(
        repo: &RepoSlug,
        channel: UpdateChannel,
        bytes: &[u8],
    ) -> Result<Option<ReleaseSummary>, UpdateError> {
        let releases: Vec<GitHubRelease> = serde_json::from_slice(bytes)
            .map_err(|_| UpdateError::new(Code::ManifestInvalid, "Invalid GitHub release list"))?;
        let best = releases
            .iter()
            .filter(|release| !release.draft)
            .filter_map(|release| Some((release.tag().ok()?, release)))
            .filter(|(tag, release)| {
                channel == UpdateChannel::Prerelease
                    || (!release.prerelease && tag.version().pre.is_empty())
            })
            .max_by(|(a, _), (b, _)| {
                a.version()
                    .cmp_precedence(b.version())
                    .then_with(|| a.as_str().cmp(b.as_str()))
            });
        Ok(best.map(|(tag, release)| summary(repo, channel, &tag, release.notes())))
    }

    fn check_outcome(
        state: &UpdateState,
        release: Option<&ReleaseSummary>,
        checked_at: u64,
    ) -> Result<CheckOutcome, UpdateError> {
        if let Some(release) = release
            && ReleaseTag::parse(&release.tag)?
                .version()
                .cmp_precedence(ReleaseTag::parse(&state.current_version)?.version())
                .is_gt()
        {
            if let Err(error) = state.should_auto_attempt(&release.version) {
                return Ok(CheckOutcome::FailedVersionBlocked {
                    version: release.version.clone(),
                    reason: error.message().to_owned(),
                });
            }
            return Ok(CheckOutcome::UpdateAvailable(UpdatePlan {
                current: state.current_version.clone(),
                release: release.clone(),
            }));
        }
        Ok(CheckOutcome::UpToDate {
            current: state.current_version.clone(),
            checked_at,
        })
    }

    fn validate_plan(
        current: &ReleaseTag,
        target: &ReleaseTag,
        manifest: &ReleaseManifest,
        options: &LocalUpdateOptions,
    ) -> Result<(), UpdateError> {
        validate_apply_consent(
            current.version(),
            target.version(),
            options.schema_compatible
                && options.data_schema_version >= manifest.min_compatible_schema,
            manifest.holder_restart_required
                || manifest.holder_protocol_version != options.holder_protocol_version,
            options.active_pty_count,
            options.allow_downgrade,
            options.restart_sessions,
        )
    }
}
