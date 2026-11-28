use crate::ui_scale::UiPrefs;
use ddg_agents_proto::host::PresetDetectStatus;
use ddg_agents_transport::{
    PresetDetectOutcome,
    agent::{AgentConfigError, AgentHostOverride, AgentProfile, AgentsConfig, ConfigStamp},
    presets::{AgentPreset, BUILTIN_AGENT_PRESETS, find_agent_preset},
    split_safe_tokens,
};
use std::{collections::BTreeSet, io, path::PathBuf, time::Duration};

#[derive(Clone, PartialEq, Eq)]
pub enum Field {
    Id,
    Name,
    Command,
    Icon,
    EnvKey,
    EnvValue(String),
    HostCommand,
    HostValue(String),
}

#[derive(PartialEq, Eq)]
pub enum Confirm {
    Delete,
    Reset,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresetDetectionState {
    Loading,
    Unsupported,
    Unavailable(String),
    Loaded {
        found: Vec<String>,
        incomplete: bool,
    },
}

pub struct AgentSettingsModal {
    pub path: PathBuf,
    pub config: AgentsConfig,
    stamp: ConfigStamp,
    presets_at_risk: BTreeSet<String>,
    pub detect_host: String,
    pub detection: PresetDetectionState,
    detect_request_id: Option<u64>,
    pub selected: usize,
    pub command: String,
    pub host_command: String,
    pub field: Field,
    pub env_key: String,
    pub reveal_env: bool,
    pub host: Option<String>,
    pub confirm: Option<Confirm>,
    pub recovery: Option<PathBuf>,
    pub error: Option<String>,
    pub conflict: bool,
    pub blocked: bool,
}

pub fn argv_text(args: &[String]) -> String {
    args.iter()
        .map(|s| format!("'{}'", s.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn registered_preset_ids(config: &AgentsConfig) -> BTreeSet<String> {
    BUILTIN_AGENT_PRESETS
        .iter()
        .filter(|preset| config.find(preset.id).is_some())
        .map(|preset| preset.id.to_owned())
        .collect()
}

impl AgentSettingsModal {
    pub fn load(path: PathBuf) -> Self {
        let (config, stamp, blocked, recovery, error) = match AgentsConfig::load(&path) {
            Ok((config, stamp)) => (config, stamp, false, None, None),
            Err(AgentConfigError::Corrupt { backup, stamp }) => (
                AgentsConfig::default(),
                stamp,
                true,
                Some(backup),
                Some("Invalid agents.json format — original file was not modified".into()),
            ),
            Err(error) => (
                AgentsConfig::default(),
                ConfigStamp::default(),
                true,
                None,
                Some(error.to_string()),
            ),
        };
        let mut modal = Self {
            path,
            presets_at_risk: registered_preset_ids(&config),
            config,
            stamp,
            detect_host: "local".into(),
            detection: PresetDetectionState::Unavailable("Detection has not run".into()),
            detect_request_id: None,
            selected: 0,
            command: String::new(),
            host_command: String::new(),
            field: Field::Id,
            env_key: String::new(),
            reveal_env: false,
            host: None,
            confirm: None,
            recovery,
            error,
            conflict: false,
            blocked,
        };
        modal.select(0);
        modal
    }

    pub fn begin_preset_detection(&mut self, host_id: String, request_id: u64) {
        self.detect_host = host_id;
        self.detect_request_id = Some(request_id);
        self.detection = PresetDetectionState::Loading;
    }

    /// The caller must also verify the current connection identity.
    pub fn apply_preset_detection(
        &mut self,
        host_id: &str,
        request_id: u64,
        result: Result<PresetDetectOutcome, String>,
    ) -> bool {
        if self.detect_host != host_id || self.detect_request_id != Some(request_id) {
            return false;
        }
        self.detect_request_id = None;
        self.detection = match result {
            Err(reason) => PresetDetectionState::Unavailable(reason),
            Ok(PresetDetectOutcome::Unsupported) => PresetDetectionState::Unsupported,
            Ok(PresetDetectOutcome::Detected(items)) => {
                let mut found = Vec::new();
                let mut incomplete = false;
                for preset in &BUILTIN_AGENT_PRESETS {
                    match items
                        .iter()
                        .find(|item| item.id == preset.id)
                        .map(|item| item.status)
                    {
                        Some(PresetDetectStatus::Found) => found.push(preset.id.to_owned()),
                        Some(PresetDetectStatus::NotFound) => {}
                        Some(PresetDetectStatus::Unknown) | None => incomplete = true,
                    }
                }
                if found.is_empty() && incomplete {
                    PresetDetectionState::Unavailable(
                        "Could not determine which presets are installed".into(),
                    )
                } else {
                    PresetDetectionState::Loaded { found, incomplete }
                }
            }
        };
        true
    }

    pub fn add_preset(&mut self, preset: &AgentPreset) -> bool {
        if self.blocked
            || !matches!(&self.detection, PresetDetectionState::Loaded { found, .. } if found.iter().any(|id| id == preset.id))
            || self.config.find(preset.id).is_some()
            || !self.commit_fields()
        {
            return false;
        }
        self.config.agents.push(preset.to_profile());
        self.select(self.config.agents.len() - 1);
        self.field = Field::Command;
        self.error = None;
        true
    }

    pub fn select(&mut self, index: usize) {
        self.selected = index;
        self.command = self
            .profile()
            .map(|p| argv_text(&p.argv))
            .unwrap_or_default();
        self.reload_host_command();
        self.confirm = None;
        self.reveal_env = false;
        self.field = Field::Id;
    }

    pub fn reload_host_command(&mut self) {
        self.host_command = self
            .host
            .as_ref()
            .and_then(|host| self.profile()?.host_overrides.get(host)?.argv.as_ref())
            .map(|argv| argv_text(argv))
            .unwrap_or_default();
    }

    pub fn profile(&self) -> Option<&AgentProfile> {
        self.config.agents.get(self.selected)
    }
    pub fn profile_mut(&mut self) -> Option<&mut AgentProfile> {
        self.config.agents.get_mut(self.selected)
    }

    pub fn host_override(&mut self) -> Option<&mut AgentHostOverride> {
        let host = self.host.clone()?;
        Some(
            self.profile_mut()?
                .host_overrides
                .entry(host)
                .or_insert_with(|| AgentHostOverride {
                    argv: None,
                    cwd: None,
                    env: Default::default(),
                }),
        )
    }

    pub fn field_mut(&mut self) -> Option<&mut String> {
        match self.field.clone() {
            Field::Command => Some(&mut self.command),
            Field::HostCommand => Some(&mut self.host_command),
            Field::Id => Some(&mut self.profile_mut()?.id),
            Field::Name => Some(&mut self.profile_mut()?.label),
            Field::Icon => Some(self.profile_mut()?.icon.get_or_insert_with(String::new)),
            Field::EnvKey => Some(&mut self.env_key),
            Field::EnvValue(key) => self.profile_mut()?.env.get_mut(&key),
            Field::HostValue(key) => self.host_override()?.env.get_mut(&key)?.as_mut(),
        }
    }

    pub fn update_value(&mut self, backspace: bool, text: Option<&str>) {
        let old_id = self.profile().map(|p| p.id.clone());
        if let Some(value) = self.field_mut() {
            if backspace {
                value.pop();
            }
            if let Some(text) = text {
                value.extend(
                    text.chars()
                        .filter(|c| !c.is_control())
                        .take(8192usize.saturating_sub(value.chars().count())),
                );
            }
        }
        if self.field == Field::Id && self.config.default_agent_id == old_id && old_id.is_some() {
            self.config.default_agent_id = self.profile().map(|p| p.id.clone());
        }
        self.error = None;
    }

    pub fn commit_fields(&mut self) -> bool {
        if self.profile().is_none() {
            return true;
        }
        let result = split_safe_tokens(&self.command).and_then(|argv| {
            let override_argv = if self.host_command.trim().is_empty() {
                None
            } else {
                Some(split_safe_tokens(&self.host_command)?)
            };
            let profile = self.profile_mut().unwrap();
            profile.argv = argv;
            if profile.icon.as_deref() == Some("") {
                profile.icon = None;
            }
            if override_argv.is_some() {
                if let Some(overridden) = self.host_override() {
                    overridden.argv = override_argv;
                }
            } else if let Some(overridden) = self.host.as_ref().and_then(|host| {
                self.config.agents[self.selected]
                    .host_overrides
                    .get_mut(host)
            }) {
                overridden.argv = None;
            }
            Ok(())
        });
        if result.is_err() {
            self.error = Some("Invalid launch command format".into());
        }
        result.is_ok()
    }

    pub fn add(&mut self) {
        if !self.commit_fields() {
            return;
        }
        let id = (1..)
            .map(|i| format!("agent-{i}"))
            .find(|id| self.config.find(id).is_none())
            .unwrap();
        self.config.agents.push(AgentProfile {
            id,
            label: "New Agent".into(),
            argv: vec![],
            default_cwd: None,
            env: Default::default(),
            icon: None,
            host_overrides: Default::default(),
        });
        self.select(self.config.agents.len() - 1);
        self.field = Field::Command;
    }

    pub fn delete(&mut self) {
        let Some(old) = self.profile().map(|p| p.id.clone()) else {
            return;
        };
        if let Some(preset) = find_agent_preset(&old) {
            self.presets_at_risk.insert(preset.id.to_owned());
        }
        self.config.agents.remove(self.selected);
        if self.config.default_agent_id.as_deref() == Some(&old) {
            self.config.default_agent_id = None;
        }
        self.select(
            self.selected
                .min(self.config.agents.len().saturating_sub(1)),
        );
    }

    pub fn save(&mut self, overwrite: bool) -> Option<AgentsConfig> {
        if self.blocked || !self.commit_fields() {
            return None;
        }
        match self.save_config(overwrite) {
            Ok(stamp) => {
                self.stamp = stamp;
                self.presets_at_risk = registered_preset_ids(&self.config);
                Some(self.config.clone())
            }
            Err(AgentConfigError::Conflict) => {
                self.conflict = true;
                None
            }
            Err(error) => {
                self.error = Some(error.to_string());
                None
            }
        }
    }

    fn save_config(&self, overwrite: bool) -> Result<ConfigStamp, AgentConfigError> {
        let mut removed = self.presets_at_risk.clone();
        let stamp = if overwrite {
            let (current, stamp) = AgentsConfig::load(&self.path)?;
            removed.extend(registered_preset_ids(&current));
            stamp
        } else {
            self.stamp.clone()
        };
        removed.retain(|id| self.config.find(id).is_none());
        if !removed.is_empty() {
            self.config.validate()?;
            if !overwrite && AgentsConfig::load(&self.path)?.1 != stamp {
                return Err(AgentConfigError::Conflict);
            }
            UiPrefs::reserve_preset_ids(
                &self.path.with_file_name("ui.json"),
                Duration::ZERO,
                |ids| {
                    ids.extend(removed);
                },
            )
            .map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!("could not reserve preset history in ui.json: {err}"),
                )
            })?;
        }
        self.config.save(&self.path, &stamp)
    }

    pub fn reset(&mut self) -> bool {
        if self.confirm != Some(Confirm::Reset) || self.recovery.is_none() {
            return false;
        }
        match AgentsConfig::default().save(&self.path, &self.stamp) {
            Ok(_) => {
                let mut fresh = Self::load(self.path.clone());
                fresh.detect_host.clone_from(&self.detect_host);
                fresh.detection.clone_from(&self.detection);
                fresh.detect_request_id = self.detect_request_id;
                *self = fresh;
                true
            }
            Err(error) => {
                self.error = Some(error.to_string());
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn detections(found: &[&str], unknown: &[&str]) -> PresetDetectOutcome {
        use ddg_agents_proto::host::{AgentPresetDetection, PresetDetectStatus::*};
        PresetDetectOutcome::Detected(
            ddg_agents_transport::presets::BUILTIN_AGENT_PRESETS
                .iter()
                .map(|preset| AgentPresetDetection {
                    id: preset.id.into(),
                    status: if found.contains(&preset.id) {
                        Found
                    } else if unknown.contains(&preset.id) {
                        Unknown
                    } else {
                        NotFound
                    },
                })
                .collect(),
        )
    }

    // UT-NATIVE-02 + R2.
    #[test]
    fn modal_detection_is_independent_stale_safe_and_only_adds_confirmed_drafts() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("agents.json");
        let pi = find_agent_preset("pi").unwrap();
        let codex = find_agent_preset("codex").unwrap();
        let mut initial = AgentsConfig::default();
        initial.agents.push(pi.to_profile());
        initial.default_agent_id = Some("pi".into());
        initial.save(&agents, &Default::default()).unwrap();
        let bytes = std::fs::read(&agents).unwrap();
        let mut modal = AgentSettingsModal::load(agents.clone());
        modal.host = Some("override-host".into());
        modal.host_command = "pi --custom".into();
        modal.begin_preset_detection("remote".into(), 1);
        assert_eq!(modal.detection, PresetDetectionState::Loading);
        assert!(!modal.add_preset(codex));
        assert!(!modal.apply_preset_detection("local", 1, Ok(detections(&["codex"], &[]))));
        modal.begin_preset_detection("remote".into(), 2);
        assert!(!modal.apply_preset_detection("remote", 1, Ok(detections(&["codex"], &[]))));
        assert!(modal.apply_preset_detection("remote", 2, Ok(PresetDetectOutcome::Unsupported)));
        assert_eq!(modal.detection, PresetDetectionState::Unsupported);
        modal.begin_preset_detection("remote".into(), 3);
        assert!(modal.apply_preset_detection("remote", 3, Ok(detections(&[], &["opencode"]))));
        assert!(matches!(
            modal.detection,
            PresetDetectionState::Unavailable(_)
        ));
        modal.begin_preset_detection("remote".into(), 4);
        assert!(modal.apply_preset_detection("remote", 4, Ok(detections(&[], &[]))));
        assert_eq!(
            modal.detection,
            PresetDetectionState::Loaded {
                found: vec![],
                incomplete: false
            }
        );
        modal.begin_preset_detection("remote".into(), 5);
        assert!(modal.apply_preset_detection(
            "remote",
            5,
            Ok(detections(&["pi", "codex"], &["opencode"]))
        ));
        assert_eq!(
            modal.detection,
            PresetDetectionState::Loaded {
                found: vec!["pi".into(), "codex".into()],
                incomplete: true,
            }
        );
        assert!(!modal.add_preset(pi));
        modal.command = "'invalid".into();
        assert!(!modal.add_preset(codex));
        assert!(modal.error.is_some());
        assert_eq!(modal.config, initial);
        modal.command = "pi --edited".into();
        modal.host_command = "'invalid override".into();
        assert!(!modal.add_preset(codex));
        assert_eq!(modal.config, initial);
        modal.host_command = "pi --custom".into();
        assert!(modal.add_preset(codex));
        assert_eq!(modal.host.as_deref(), Some("override-host"));
        assert!(modal.host_command.is_empty());
        assert_eq!(modal.detect_host, "remote");
        assert!(modal.field == Field::Command);
        assert_eq!(modal.profile(), Some(&codex.to_profile()));
        assert_eq!(modal.config.default_agent_id.as_deref(), Some("pi"));
        let previous = modal.config.find("pi").unwrap();
        assert_eq!(previous.argv, ["pi", "--edited"]);
        assert_eq!(
            previous.host_overrides["override-host"]
                .argv
                .as_ref()
                .unwrap(),
            &["pi", "--custom"]
        );
        assert!(modal.commit_fields());
        assert!(modal.profile().unwrap().host_overrides.is_empty());
        drop(modal); // Cancel.
        assert_eq!(std::fs::read(&agents).unwrap(), bytes);
        assert!(!dir.path().join("ui.json").exists());
    }

    #[test]
    fn modal_deletion_and_overwrite_reserve_before_removing_preset_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("custom-agents.json");
        let ui = dir.path().join("ui.json");
        let mut config = AgentsConfig::default();
        config
            .agents
            .push(find_agent_preset("pi").unwrap().to_profile());
        let stamp = config.save(&agents, &Default::default()).unwrap();
        let mut canceled = AgentSettingsModal::load(agents.clone());
        canceled.delete();
        drop(canceled);
        assert!(!ui.exists());
        assert_eq!(AgentsConfig::load(&agents).unwrap().0, config);

        let mut modal = AgentSettingsModal::load(agents.clone());
        modal.delete();
        std::fs::write(&ui, b"{").unwrap();
        assert!(modal.save(false).is_none());
        assert_eq!(AgentsConfig::load(&agents).unwrap().0, config);
        assert_eq!(std::fs::read(&ui).unwrap(), b"{");
        std::fs::remove_file(&ui).unwrap();

        // Deliberate overwrite also removes a preset added since the modal opened.
        config
            .agents
            .push(find_agent_preset("codex").unwrap().to_profile());
        config.save(&agents, &stamp).unwrap();
        assert!(modal.save(false).is_none());
        assert!(modal.conflict);
        assert!(!ui.exists());
        assert!(modal.save(true).is_some());
        assert!(AgentsConfig::load(&agents).unwrap().0.agents.is_empty());
        assert_eq!(
            UiPrefs::load(&ui).unwrap().seeded_preset_ids,
            ["pi".into(), "codex".into()].into()
        );
    }

    #[test]
    fn launcher_modal_and_ui_prefs_conflict_then_recovery() {
        let dir = std::env::temp_dir().join(format!("ddg-agent-modal-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("agents.json");
        let mut modal = AgentSettingsModal::load(path.clone());
        let config = AgentsConfig::default();
        config.save(&path, &ConfigStamp::default()).unwrap();
        modal.add();
        modal.command = "echo 'draft'".into();
        assert!(modal.save(false).is_none());
        assert!(modal.conflict);
        assert_eq!(modal.command, "echo 'draft'");
        assert!(modal.save(true).is_some());
        std::fs::write(&path, b"{").unwrap();
        let mut broken = AgentSettingsModal::load(path.clone());
        assert!(broken.blocked);
        assert!(broken.recovery.as_ref().unwrap().exists());
        assert!(broken.save(false).is_none());
        assert!(!broken.reset());
        broken.confirm = Some(Confirm::Reset);
        assert!(broken.reset());
        assert_eq!(broken.config, config);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
