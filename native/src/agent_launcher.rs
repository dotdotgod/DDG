use ddg_agents_transport::agent::{AgentProfile, AgentsConfig};
use gpui::Keystroke;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchTarget {
    Tab(String),
    Split(String),
    Room,
    Background,
}

pub struct AgentLauncher {
    pub host: String,
    pub target: usize,
    pub filter: String,
    pub selected: usize,
}

impl AgentLauncher {
    pub fn new(host: String, has_room: bool) -> Self {
        Self {
            host,
            target: if has_room { 0 } else { 2 },
            filter: String::new(),
            selected: 0,
        }
    }

    pub fn matches<'a>(&self, cfg: &'a AgentsConfig) -> Vec<&'a AgentProfile> {
        let filter = self.filter.to_lowercase();
        let mut rows: Vec<_> = cfg
            .agents
            .iter()
            .filter(|profile| {
                profile.id.contains(&filter)
                    || profile.label.to_lowercase().contains(&filter)
                    || profile.argv.join(" ").to_lowercase().contains(&filter)
            })
            .collect();
        rows.sort_by_key(|p| cfg.default_agent_id.as_deref() != Some(&p.id));
        rows
    }

    pub fn key(&mut self, key: &Keystroke, count: usize) -> Option<usize> {
        if (key.modifiers.platform || key.modifiers.control)
            && !key.modifiers.alt
            && !key.modifiers.shift
        {
            return key
                .key
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=9).contains(n) && *n <= count)
                .map(|n| n - 1);
        }
        match key.key.as_str() {
            "enter" => return (self.selected < count).then_some(self.selected),
            "up" => self.selected = self.selected.saturating_sub(1),
            "down" => self.selected = (self.selected + 1).min(count.saturating_sub(1)),
            "backspace" => {
                self.filter.pop();
                self.selected = 0;
            }
            _ if !key.modifiers.platform && !key.modifiers.control => {
                if let Some(text) = &key.key_char {
                    self.filter.extend(
                        text.chars()
                            .filter(|c| !c.is_control())
                            .take(128usize.saturating_sub(self.filter.chars().count())),
                    );
                    self.selected = 0;
                }
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launcher_modal_and_ui_prefs_number_keys() {
        let mut launcher = AgentLauncher::new("local".into(), false);
        assert_eq!(launcher.target, 2);
        assert_eq!(
            launcher.key(
                &Keystroke {
                    key: "4".into(),
                    key_char: Some("4".into()),
                    modifiers: Default::default()
                },
                4
            ),
            None
        );
        assert_eq!(launcher.filter, "4");
        assert_eq!(
            launcher.key(&Keystroke::parse("ctrl-4").unwrap(), 4),
            Some(3)
        );
    }
}
