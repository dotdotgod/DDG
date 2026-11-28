//! Built-in agent launch presets shared by the host, CLI, and desktop app.

use crate::agent::{AgentProfile, AgentsConfig, MAX_AGENT_PROFILES};
use ddg_agents_proto::BUILTIN_AGENT_PRESET_BINS;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub argv: &'static [&'static str],
    pub detect_bin: &'static str,
    pub icon: &'static str,
}

impl AgentPreset {
    const fn new(id: &'static str, label: &'static str, argv: &'static [&'static str]) -> Self {
        Self {
            id,
            label,
            argv,
            detect_bin: argv[0],
            icon: id,
        }
    }

    pub fn to_profile(&self) -> AgentProfile {
        AgentProfile {
            id: self.id.into(),
            label: self.label.into(),
            argv: self.argv.iter().map(|&arg| arg.into()).collect(),
            default_cwd: None,
            env: Default::default(),
            icon: Some(self.icon.into()),
            host_overrides: Default::default(),
        }
    }
}

pub static BUILTIN_AGENT_PRESETS: [AgentPreset; 5] = [
    AgentPreset::new("pi", "Pi", &[BUILTIN_AGENT_PRESET_BINS[0]]),
    AgentPreset::new(
        "antigravity",
        "Antigravity",
        &[BUILTIN_AGENT_PRESET_BINS[1]],
    ),
    AgentPreset::new("codex", "Codex", &[BUILTIN_AGENT_PRESET_BINS[2]]),
    AgentPreset::new("opencode", "OpenCode", &[BUILTIN_AGENT_PRESET_BINS[3]]),
    AgentPreset::new(
        "claude-code",
        "Claude Code",
        &[BUILTIN_AGENT_PRESET_BINS[4]],
    ),
];

pub fn find_agent_preset(id: &str) -> Option<&'static AgentPreset> {
    BUILTIN_AGENT_PRESETS
        .iter()
        .find(|preset| preset.id.eq_ignore_ascii_case(id))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutoSeedOutcome {
    pub added_ids: Vec<String>,
    pub seeded_ids_changed: bool,
}

/// Merge in memory only; the caller owns any history reservation and config save.
pub fn merge_auto_detected_presets(
    config: &mut AgentsConfig,
    seeded_ids: &mut BTreeSet<String>,
    found_preset_ids: &[String],
) -> AutoSeedOutcome {
    let mut outcome = AutoSeedOutcome::default();
    for id in found_preset_ids {
        let Some(preset) = find_agent_preset(id) else {
            continue;
        };
        if config.find(preset.id).is_none() {
            if seeded_ids.contains(preset.id) || config.agents.len() >= MAX_AGENT_PROFILES {
                continue;
            }
            config.agents.push(preset.to_profile());
            outcome.added_ids.push(preset.id.into());
        }
        outcome.seeded_ids_changed |= seeded_ids.insert(preset.id.into());
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentHostOverride;
    use crate::programs::ProgramsConfig;
    use ddg_agents_proto::host::validate_agent_profile_slug;

    // UT-PRESET-01
    #[test]
    fn preset_catalog_matches_allowlist_icons_and_profile_contract() {
        let programs = ProgramsConfig::default();
        let icons = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../native/assets/icons");
        let program_icons = icons.join("programs");
        let icons = if program_icons.is_dir() {
            program_icons
        } else {
            icons
        };
        assert_eq!(find_agent_preset("PI").unwrap().id, "pi");
        let mut ids = BTreeSet::new();
        let mut config = AgentsConfig::default();
        for ((preset, bin), expected_bin) in BUILTIN_AGENT_PRESETS
            .iter()
            .zip(BUILTIN_AGENT_PRESET_BINS)
            .zip(["pi", "agy", "codex", "opencode", "claude"])
        {
            assert_eq!(bin, expected_bin);
            validate_agent_profile_slug(preset.id).unwrap();
            assert!(ids.insert(preset.id.to_ascii_lowercase()));
            assert_eq!(preset.detect_bin, bin);
            assert_eq!(preset.argv, [bin]);
            assert!(!bin.contains(".."));
            assert!(
                bin.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            );
            let program = programs
                .programs
                .iter()
                .find(|p| p.id == preset.id)
                .unwrap();
            assert_eq!(program.label, preset.label);
            assert_eq!(program.icon, preset.icon);
            let path = icons.join(format!("{}.svg", preset.icon));
            let svg = std::fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("could not read {}: {err}", path.display()));
            assert!(
                svg.contains("<svg") && svg.contains("</svg>"),
                "{}",
                path.display()
            );
            let profile = preset.to_profile();
            assert_eq!(profile.id, preset.id);
            assert_eq!(profile.label, preset.label);
            assert_eq!(profile.argv, [bin]);
            assert_eq!(profile.icon.as_deref(), Some(preset.icon));
            assert!(
                profile.default_cwd.is_none()
                    && profile.env.is_empty()
                    && profile.host_overrides.is_empty()
            );
            config.agents.push(profile);
        }
        assert_eq!(BUILTIN_AGENT_PRESETS.len(), 5);
        assert_eq!(
            ids,
            ["pi", "antigravity", "codex", "opencode", "claude-code"]
                .map(String::from)
                .into()
        );
        config.validate().unwrap();
    }

    // UT-PRESET-02
    #[test]
    fn preset_merge_preserves_profiles_defaults_history_and_capacity() {
        let found = ["pi", "codex", "unknown-cli", "pi"].map(String::from);
        let mut config = AgentsConfig::default();
        let mut seeded = BTreeSet::new();
        let outcome = merge_auto_detected_presets(&mut config, &mut seeded, &found);
        assert_eq!(outcome.added_ids, ["pi", "codex"]);
        assert!(outcome.seeded_ids_changed);
        assert_eq!(seeded, ["pi".into(), "codex".into()].into());
        assert!(config.default_agent_id.is_none());

        config.agents.pop();
        let pi = &mut config.agents[0];
        pi.argv = vec!["custom-pi".into()];
        pi.env.insert("MODEL".into(), "custom".into());
        pi.host_overrides.insert(
            "remote".into(),
            AgentHostOverride {
                argv: Some(vec!["remote-pi".into()]),
                cwd: None,
                env: Default::default(),
            },
        );
        config.default_agent_id = Some("pi".into());
        let original = config.clone();
        seeded.remove("pi");
        let outcome = merge_auto_detected_presets(&mut config, &mut seeded, &found);
        assert!(outcome.added_ids.is_empty() && outcome.seeded_ids_changed);
        assert_eq!(config, original);
        assert_eq!(seeded, ["pi".into(), "codex".into()].into());
        assert!(!merge_auto_detected_presets(&mut config, &mut seeded, &found).seeded_ids_changed);

        for i in 1..MAX_AGENT_PROFILES - 1 {
            let mut profile = original.agents[0].clone();
            profile.id = format!("custom-{i}");
            config.agents.push(profile);
        }
        seeded.remove("pi");
        let found = ["agy", "opencode", "claude-code", "pi"].map(String::from);
        let outcome = merge_auto_detected_presets(&mut config, &mut seeded, &found);
        assert_eq!(outcome.added_ids, ["opencode"]);
        assert!(outcome.seeded_ids_changed);
        assert_eq!(config.agents.len(), MAX_AGENT_PROFILES);
        assert_eq!(
            seeded,
            ["pi".into(), "codex".into(), "opencode".into()].into()
        );
        assert_eq!(config.agents[0], original.agents[0]);
        assert_eq!(config.default_agent_id, original.default_agent_id);
        config.validate().unwrap();
    }
}
