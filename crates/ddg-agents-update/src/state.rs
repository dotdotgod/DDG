use crate::{
    UpdateError, UpdateErrorCode as Code,
    config::UpdateChannel,
    rollback::RollbackRecord,
    swap::{open_regular, storage_error},
    version::ReleaseTag,
};
use ddg_agents_os::{
    fs::{ensure_private_dir, write_atomic},
    lock::FileLock,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReleaseSummary {
    pub version: String,
    pub tag: String,
    pub repo: String,
    pub check_channel: UpdateChannel,
    pub notes: String,
    pub web_url: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FailedVersionRecord {
    pub failed_at: u64,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PendingHolderRestart {
    pub to_version: String,
    pub active_pty_count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AutoRolledBackNotice {
    pub attempted_version: String,
    pub restored_version: String,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InProgressSwap {
    pub from_version: String,
    pub to_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UpdateState {
    pub schema_version: u32,
    pub last_checked_at: Option<u64>,
    pub etag: Option<String>,
    pub cached_latest: Option<ReleaseSummary>,
    pub current_version: String,
    pub previous_version: Option<String>,
    pub rollback: Option<RollbackRecord>,
    #[serde(default)]
    pub failed_versions: BTreeMap<String, FailedVersionRecord>,
    pub pending_holder_restart: Option<PendingHolderRestart>,
    pub auto_rolled_back: Option<AutoRolledBackNotice>,
    pub in_progress: Option<InProgressSwap>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            schema_version: 1,
            last_checked_at: None,
            etag: None,
            cached_latest: None,
            current_version: env!("CARGO_PKG_VERSION").to_owned(),
            previous_version: None,
            rollback: None,
            failed_versions: BTreeMap::new(),
            pending_holder_restart: None,
            auto_rolled_back: None,
            in_progress: None,
        }
    }
}

fn unprefixed(version: &str) -> &str {
    version.strip_prefix('v').unwrap_or(version)
}

impl UpdateState {
    pub fn failed_version(&self, version: &str) -> Option<&FailedVersionRecord> {
        let key = unprefixed(version);
        self.failed_versions
            .get(key)
            .or_else(|| self.failed_versions.get(&format!("v{key}")))
    }

    pub fn should_auto_attempt(&self, version: &str) -> Result<(), UpdateError> {
        self.should_attempt(version, false)
    }

    /// This skips only the recorded-failure policy. It grants no apply consent.
    pub fn should_attempt(&self, version: &str, force: bool) -> Result<(), UpdateError> {
        if !force && let Some(failure) = self.failed_version(version) {
            return Err(UpdateError::new(
                Code::FailedVersionBlocked,
                &failure.reason,
            ));
        }
        Ok(())
    }

    /// Only call after the replacement has passed verification and startup.
    pub fn record_success(&mut self, version: &str) {
        let version = unprefixed(version);
        self.failed_versions.remove(version);
        self.failed_versions.remove(&format!("v{version}"));
        if unprefixed(&self.current_version) != version {
            self.previous_version = Some(std::mem::replace(
                &mut self.current_version,
                version.to_owned(),
            ));
        }
        self.in_progress = None;
        self.pending_holder_restart = None;
    }

    pub fn cache_pins(&self) -> BTreeSet<String> {
        std::iter::once(self.current_version.as_str())
            .chain(self.previous_version.as_deref())
            .chain(self.rollback.iter().map(|r| r.version.as_str()))
            .chain(
                self.in_progress
                    .iter()
                    .flat_map(|r| [r.from_version.as_str(), r.to_version.as_str()]),
            )
            .chain(
                self.pending_holder_restart
                    .iter()
                    .map(|r| r.to_version.as_str()),
            )
            .map(|v| unprefixed(v).to_owned())
            .collect()
    }
}

pub struct UpdateStateStore {
    dir: PathBuf,
}

/// A transaction holds update.lock even across async checks. Dropping it does not save.
pub(crate) struct LockedState {
    pub(crate) state: UpdateState,
    path: PathBuf,
    _lock: FileLock,
}

impl UpdateStateStore {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: state_dir.into(),
        }
    }

    pub fn load(&self) -> Result<UpdateState, UpdateError> {
        Ok(self.lock()?.state)
    }

    pub fn update<T>(
        &self,
        change: impl FnOnce(&mut UpdateState) -> Result<T, UpdateError>,
    ) -> Result<T, UpdateError> {
        let mut transaction = self.lock()?;
        let result = change(&mut transaction.state)?;
        transaction.save()?;
        Ok(result)
    }

    pub fn try_update_background<T>(
        &self,
        change: impl FnOnce(&mut UpdateState) -> Result<T, UpdateError>,
    ) -> Result<Option<T>, UpdateError> {
        let Some(mut transaction) = self.try_lock()? else {
            return Ok(None);
        };
        let result = change(&mut transaction.state)?;
        transaction.save()?;
        Ok(Some(result))
    }

    pub(crate) fn lock(&self) -> Result<LockedState, UpdateError> {
        self.try_lock()?.ok_or_else(|| {
            UpdateError::new(Code::LockBusy, "Another update operation holds update.lock")
        })
    }

    pub(crate) fn try_lock(&self) -> Result<Option<LockedState>, UpdateError> {
        ensure_private_dir(&self.dir).map_err(storage_error)?;
        let lock = match FileLock::try_lock_exclusive(&self.dir.join("update.lock")) {
            Ok(lock) => lock,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(err) => return Err(storage_error(err)),
        };
        let path = self.dir.join("update-state.json");
        let state = load_or_recover(&path)?;
        Ok(Some(LockedState {
            state,
            path,
            _lock: lock,
        }))
    }
}

impl LockedState {
    pub(crate) fn save(&self) -> Result<(), UpdateError> {
        save_state(&self.path, &self.state)
    }
}

fn save_state(path: &Path, state: &UpdateState) -> Result<(), UpdateError> {
    if state.schema_version != 1 || ReleaseTag::parse(&state.current_version).is_err() {
        return Err(UpdateError::new(
            Code::ManifestInvalid,
            "Invalid update state schema or current version",
        ));
    }
    let bytes = serde_json::to_vec_pretty(state).map_err(storage_error)?;
    write_atomic(path, &bytes).map_err(storage_error)
}

fn load_or_recover(path: &Path) -> Result<UpdateState, UpdateError> {
    let mut file = match open_regular(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(UpdateState::default()),
        Err(err) => return Err(storage_error(err)),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(storage_error)?;
    if let Ok(state) = serde_json::from_slice::<UpdateState>(&bytes)
        && state.schema_version == 1
        && ReleaseTag::parse(&state.current_version).is_ok()
    {
        return Ok(state);
    }

    let mut stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let backup = loop {
        let candidate = path.with_file_name(format!("update-state.json.corrupt.{stamp}.bak"));
        match candidate.symlink_metadata() {
            Err(err) if err.kind() == io::ErrorKind::NotFound => break candidate,
            Err(err) => return Err(storage_error(err)),
            Ok(_) => stamp += 1,
        }
    };
    write_atomic(&backup, &bytes).map_err(storage_error)?;
    let state = UpdateState::default();
    save_state(path, &state)?;
    Ok(state)
}
