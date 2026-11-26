use crate::swap::{is_relative_path, is_safe_segment};
use crate::{
    UpdateError, UpdateErrorCode as Code, config::UpdateChannel, integrity::decode_sha256,
    version::ReleaseTag,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    pub schema_version: u32,
    pub version: String,
    pub tag: String,
    pub channel: UpdateChannel,
    pub published_at: String,
    pub protocol_version: u32,
    pub holder_protocol_version: u32,
    pub holder_restart_required: bool,
    pub min_compatible_schema: u32,
    pub release_notes_summary: String,
    pub artifacts: Vec<ReleaseArtifact>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReleaseArtifact {
    pub platform: String,
    pub name: String,
    pub sha256: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub binaries: BTreeMap<String, BinaryDigest>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BinaryDigest {
    pub path: String,
    pub sha256: String,
}

impl ReleaseManifest {
    pub fn parse(bytes: &[u8], expected_tag: &ReleaseTag) -> Result<Self, UpdateError> {
        let manifest: Self = serde_json::from_slice(bytes).map_err(|_| {
            UpdateError::new(Code::ManifestInvalid, "Invalid release manifest JSON")
        })?;
        let mut names = HashSet::new();
        let mut platforms = HashSet::new();
        if manifest.schema_version != 1
            || manifest.version != expected_tag.version().to_string()
            || manifest.tag != expected_tag.as_str()
            || (manifest.channel == UpdateChannel::Stable) != expected_tag.version().pre.is_empty()
            || manifest.artifacts.is_empty()
            || manifest.artifacts.iter().any(|asset| {
                let mut paths = HashSet::new();
                !is_safe_segment(&asset.platform)
                    || !is_safe_segment(&asset.name)
                    || !names.insert(asset.name.as_str())
                    || !platforms.insert(asset.platform.as_str())
                    || decode_sha256(&asset.sha256).is_none()
                    || asset.binaries.iter().any(|(name, binary)| {
                        !is_safe_segment(name)
                            || !is_relative_path(&binary.path)
                            || !paths.insert(binary.path.as_str())
                            || decode_sha256(&binary.sha256).is_none()
                    })
            })
        {
            return Err(UpdateError::new(
                Code::ManifestInvalid,
                "Release manifest schema, version, channel or artifacts do not match",
            ));
        }
        Ok(manifest)
    }
}
