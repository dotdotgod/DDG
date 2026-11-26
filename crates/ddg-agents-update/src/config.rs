use crate::{UpdateError, UpdateErrorCode as Code};
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

pub const DEFAULT_UPDATE_REPO: &str = "dotdotgod/DDG";
const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateChannel {
    #[default]
    Stable,
    Prerelease,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoCheckPolicy {
    #[default]
    On,
    Off,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceOrigin {
    Default,
    Config,
    Env,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoSlug(String);

impl RepoSlug {
    pub fn parse(raw: &str) -> Result<Self, UpdateError> {
        if let Some((owner, repo)) = raw.split_once('/')
            && (1..=39).contains(&owner.len())
            && !owner.starts_with('-')
            && !owner.ends_with('-')
            && !owner.contains("--")
            && owner
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            && (1..=100).contains(&repo.len())
            && !matches!(repo, "." | "..")
            && repo
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        {
            return Ok(Self(raw.to_owned()));
        }
        Err(UpdateError::new(
            Code::InvalidRepo,
            "Expected a GitHub owner/repo slug",
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    pub schema_version: u32,
    pub auto_check: AutoCheckPolicy,
    pub channel: UpdateChannel,
    pub repo: Option<String>,
    pub check_interval_secs: u64,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            auto_check: AutoCheckPolicy::On,
            channel: UpdateChannel::Stable,
            repo: None,
            check_interval_secs: 21_600,
        }
    }
}

impl UpdateConfig {
    fn normalized(mut self) -> Result<Self, UpdateError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(UpdateError::new(
                Code::ManifestInvalid,
                "Unsupported update config schema",
            ));
        }
        if let Some(repo) = &self.repo {
            RepoSlug::parse(repo)?;
        }
        self.check_interval_secs = self.check_interval_secs.clamp(900, 604_800);
        Ok(self)
    }

    pub fn load(path: &Path) -> Result<Self, UpdateError> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<Self>(&bytes)
                .map_err(|e| UpdateError::new(Code::ManifestInvalid, e.to_string()))?
                .normalized(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(UpdateError::new(Code::ManifestInvalid, e.to_string())),
        }
    }

    /// The parent directory must already exist and be trusted.
    pub fn save(&self, path: &Path) -> Result<(), UpdateError> {
        let bytes = serde_json::to_vec_pretty(&self.clone().normalized()?)
            .map_err(|e| UpdateError::new(Code::ManifestInvalid, e.to_string()))?;
        ddg_agents_os::fs::write_atomic(path, &bytes)
            .map_err(|e| UpdateError::new(Code::InstallDirNotWritable, e.to_string()))
    }

    pub fn effective_repo(&self) -> Result<(RepoSlug, SourceOrigin), UpdateError> {
        match std::env::var("DDG_UPDATE_REPO") {
            Ok(repo) => self.resolve_repo(Some(&repo)),
            Err(std::env::VarError::NotPresent) => self.resolve_repo(None),
            Err(_) => Err(UpdateError::new(
                Code::InvalidRepo,
                "DDG_UPDATE_REPO is not valid UTF-8",
            )),
        }
    }

    pub fn resolve_repo(
        &self,
        env_repo: Option<&str>,
    ) -> Result<(RepoSlug, SourceOrigin), UpdateError> {
        let (repo, origin) = match (env_repo, self.repo.as_deref()) {
            (Some(repo), _) => (repo, SourceOrigin::Env),
            (_, Some(repo)) => (repo, SourceOrigin::Config),
            _ => (DEFAULT_UPDATE_REPO, SourceOrigin::Default),
        };
        Ok((RepoSlug::parse(repo)?, origin))
    }
}
