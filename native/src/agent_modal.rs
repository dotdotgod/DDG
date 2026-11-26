use ddg_agents_transport::{
    agent::{AgentConfigError, AgentHostOverride, AgentProfile, AgentsConfig, ConfigStamp},
    split_safe_tokens,
};
use std::path::PathBuf;

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

pub struct AgentSettingsModal {
    pub path: PathBuf,
    pub config: AgentsConfig,
    stamp: ConfigStamp,
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

impl AgentSettingsModal {
    pub fn load(path: PathBuf) -> Self {
        let (config, stamp, blocked, recovery, error) = match AgentsConfig::load(&path) {
            Ok((config, stamp)) => (config, stamp, false, None, None),
            Err(AgentConfigError::Corrupt { backup, stamp }) => (
                AgentsConfig::default(),
                stamp,
                true,
                Some(backup),
                Some("agents.json 형식 오류 — 원본은 변경하지 않았습니다".into()),
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
            config,
            stamp,
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
            if let Some(overridden) = self.host_override() {
                overridden.argv = override_argv;
            }
            Ok(())
        });
        if result.is_err() {
            self.error = Some("실행 명령어 형식이 올바르지 않습니다".into());
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
            label: "새 에이전트".into(),
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
        let result = if overwrite {
            AgentsConfig::load(&self.path)
                .and_then(|(_, stamp)| self.config.save(&self.path, &stamp))
        } else {
            self.config.save(&self.path, &self.stamp)
        };
        match result {
            Ok(stamp) => {
                self.stamp = stamp;
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

    pub fn reset(&mut self) -> bool {
        if self.confirm != Some(Confirm::Reset) || self.recovery.is_none() {
            return false;
        }
        match AgentsConfig::default().save(&self.path, &self.stamp) {
            Ok(_) => {
                *self = Self::load(self.path.clone());
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
