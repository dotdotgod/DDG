use crate::{UpdateError, UpdateErrorCode as Code, config::UpdateChannel};
use semver::Version;
use std::cmp::Ordering;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseTag {
    raw: String,
    version: Version,
}

impl ReleaseTag {
    pub fn parse(raw: &str) -> Result<Self, UpdateError> {
        let invalid = || UpdateError::new(Code::ManifestInvalid, "Invalid SemVer release tag");
        if raw.contains(['/', '?', '#'])
            || raw.contains("..")
            || raw.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(invalid());
        }
        let version =
            Version::parse(raw.strip_prefix('v').unwrap_or(raw)).map_err(|_| invalid())?;
        Ok(Self {
            raw: raw.to_owned(),
            version,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
    pub fn version(&self) -> &Version {
        &self.version
    }
}

/// Prerelease opt-in includes stable releases. SemVer build metadata does not
/// affect precedence; equal-precedence tags do not offer an update.
pub fn compare_and_select_release<'a>(
    current: &Version,
    tags: &'a [ReleaseTag],
    channel: UpdateChannel,
    allow_downgrade: bool,
    schema_compatible: impl FnOnce(&ReleaseTag) -> bool,
) -> Result<Option<&'a ReleaseTag>, UpdateError> {
    let Some(target) = tags
        .iter()
        .filter(|tag| channel == UpdateChannel::Prerelease || tag.version.pre.is_empty())
        .max_by(|a, b| {
            a.version
                .cmp_precedence(&b.version)
                .then_with(|| a.raw.cmp(&b.raw))
        })
    else {
        return Ok(None);
    };
    if target.version.cmp_precedence(current) == Ordering::Equal {
        return Ok(None);
    }
    validate_version_change(
        current,
        target.version(),
        schema_compatible(target),
        allow_downgrade,
    )?;
    Ok(Some(target))
}

fn validate_version_change(
    current: &Version,
    target: &Version,
    schema_compatible: bool,
    allow_downgrade: bool,
) -> Result<(), UpdateError> {
    let downgrade = target.cmp_precedence(current).is_lt();
    if !schema_compatible {
        let code = if downgrade {
            Code::SchemaIncompatibleDowngrade
        } else {
            Code::ManifestInvalid
        };
        return Err(UpdateError::new(
            code,
            "Target is incompatible with the data schema",
        ));
    }
    if downgrade && !allow_downgrade {
        return Err(UpdateError::new(
            Code::DowngradeDenied,
            "Downgrade requires explicit consent",
        ));
    }
    Ok(())
}

/// Call at the coordinator boundary even when selection already ran in the UI.
pub fn validate_apply_consent(
    current: &Version,
    target: &Version,
    schema_compatible: bool,
    holder_incompatible: bool,
    active_pty_count: u32,
    allow_downgrade: bool,
    restart_sessions: bool,
) -> Result<(), UpdateError> {
    validate_version_change(current, target, schema_compatible, allow_downgrade)?;
    if holder_incompatible && active_pty_count > 0 && !restart_sessions {
        return Err(UpdateError::new(
            Code::HolderRestartRequired,
            "Active sessions require explicit restart consent",
        ));
    }
    Ok(())
}
