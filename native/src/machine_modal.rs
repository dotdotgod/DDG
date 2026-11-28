//! Add / Edit Machine modal state, SSH command paste parsing, and validation.

use ddg_agents_transport::{
    DialErrorKind, HostProfile, HostTransport, parse_docker_input, parse_ssh_input,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MachineTab {
    Ssh,
    Docker,
    ContainerSsh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModalField {
    SshInput,
    Label,
    Port,
    IdentityFile,
    KnownHostsFile,
    DockerContainer,
    DockerSshHost,
    ContainerHost,
    ContainerUser,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestState {
    Idle,
    Running {
        req_id: u64,
    },
    Installing {
        req_id: u64,
        stage: String,
    },
    Success {
        host_id: String,
    },
    Failed {
        kind: DialErrorKind,
        message: String,
        copy_hint: Option<String>,
    },
}

pub struct MachineModal {
    pub editing_id: Option<String>,
    pub tab: MachineTab,
    pub active_field: ModalField,
    pub show_advanced: bool,
    pub ssh_input: String,
    pub label: String,
    pub auto_filled_label: Option<String>,
    pub port: String,
    pub identity_file: String,
    pub known_hosts_file: String,
    pub docker_container: String,
    pub docker_ssh_host: String,
    pub container_host: String,
    pub container_user: String,
    pub expected_host_id: Option<String>,
    pub ssh_candidates: Vec<String>,
    pub docker_candidates: Vec<String>,
    pub test_state: TestState,
    pub error: Option<String>,
    pub copied_hint: bool,
}

impl MachineModal {
    pub fn new_add() -> Self {
        Self {
            editing_id: None,
            tab: MachineTab::Ssh,
            active_field: ModalField::SshInput,
            show_advanced: false,
            ssh_input: String::new(),
            label: String::new(),
            auto_filled_label: None,
            port: String::new(),
            identity_file: String::new(),
            known_hosts_file: String::new(),
            docker_container: String::new(),
            docker_ssh_host: String::new(),
            container_host: "127.0.0.1".into(),
            container_user: "ddg".into(),
            expected_host_id: None,
            ssh_candidates: Vec::new(),
            docker_candidates: Vec::new(),
            test_state: TestState::Idle,
            error: None,
            copied_hint: false,
        }
    }

    pub fn from_profile(profile: &HostProfile) -> Self {
        let mut m = Self::new_add();
        m.editing_id = Some(profile.id.clone());
        m.label = profile.label.clone();
        m.expected_host_id = profile.expected_host_id.clone();
        match &profile.transport {
            HostTransport::Local { .. } => {}
            HostTransport::Ssh {
                target,
                port,
                known_hosts_file,
                identity_file,
            } => {
                m.tab = MachineTab::Ssh;
                m.active_field = ModalField::SshInput;
                m.ssh_input = target.clone();
                if let Some(p) = port {
                    m.port = p.to_string();
                    m.show_advanced = true;
                }
                if let Some(id) = identity_file {
                    m.identity_file = id.clone();
                    m.show_advanced = true;
                }
                if let Some(kh) = known_hosts_file {
                    m.known_hosts_file = kh.clone();
                    m.show_advanced = true;
                }
            }
            HostTransport::DockerExec {
                container,
                ssh_host,
                known_hosts_file,
                identity_file,
            } => {
                m.tab = MachineTab::Docker;
                m.active_field = ModalField::DockerContainer;
                m.docker_container = container.clone();
                if let Some(h) = ssh_host {
                    m.docker_ssh_host = h.clone();
                }
                if let Some(id) = identity_file {
                    m.identity_file = id.clone();
                    m.show_advanced = true;
                }
                if let Some(kh) = known_hosts_file {
                    m.known_hosts_file = kh.clone();
                    m.show_advanced = true;
                }
            }
            HostTransport::ContainerSsh {
                host,
                port,
                user,
                known_hosts_file,
                identity_file,
            } => {
                m.tab = MachineTab::ContainerSsh;
                m.active_field = ModalField::ContainerHost;
                m.container_host = host.clone();
                m.port = port.to_string();
                m.container_user = user.clone();
                if let Some(id) = identity_file {
                    m.identity_file = id.clone();
                    m.show_advanced = true;
                }
                if let Some(kh) = known_hosts_file {
                    m.known_hosts_file = kh.clone();
                    m.show_advanced = true;
                }
            }
        }
        m
    }

    pub fn supports_remote_install(&self) -> bool {
        matches!(self.tab, MachineTab::Ssh | MachineTab::ContainerSsh)
    }

    pub fn is_installing(&self) -> bool {
        matches!(self.test_state, TestState::Installing { .. })
    }

    pub fn is_busy(&self) -> bool {
        matches!(
            self.test_state,
            TestState::Running { .. } | TestState::Installing { .. }
        )
    }

    pub fn select_ssh_candidate(&mut self, host: &str) {
        self.ssh_input = host.to_owned();
        self.select_candidate_label(host);
    }

    pub fn select_docker_candidate(&mut self, container: &str) {
        self.docker_container = container.to_owned();
        self.select_candidate_label(container);
    }

    fn select_candidate_label(&mut self, candidate: &str) {
        if self.label.trim().is_empty()
            || self.auto_filled_label.as_deref() == Some(self.label.as_str())
        {
            self.label = candidate.to_owned();
            self.auto_filled_label = Some(candidate.to_owned());
        }
        self.error = None;
        self.test_state = TestState::Idle;
    }

    pub fn switch_tab(&mut self, tab: MachineTab) {
        if self.tab == tab || self.is_busy() {
            return;
        }
        self.tab = tab;
        self.error = None;
        self.test_state = TestState::Idle;
        self.copied_hint = false;
        self.active_field = match tab {
            MachineTab::Ssh => ModalField::SshInput,
            MachineTab::Docker => ModalField::DockerContainer,
            MachineTab::ContainerSsh => {
                if self.port.trim().is_empty() {
                    self.port = "2222".into();
                }
                ModalField::ContainerHost
            }
        };
    }

    pub fn visible_fields(&self) -> Vec<ModalField> {
        let mut fields = match self.tab {
            MachineTab::Ssh => vec![ModalField::SshInput, ModalField::Label],
            MachineTab::Docker => vec![
                ModalField::DockerContainer,
                ModalField::DockerSshHost,
                ModalField::Label,
            ],
            MachineTab::ContainerSsh => vec![
                ModalField::ContainerHost,
                ModalField::Port,
                ModalField::ContainerUser,
                ModalField::Label,
            ],
        };
        if self.show_advanced {
            if self.tab == MachineTab::Ssh {
                fields.push(ModalField::Port);
            }
            fields.push(ModalField::IdentityFile);
            fields.push(ModalField::KnownHostsFile);
        }
        fields
    }

    pub fn cycle_field(&mut self, reverse: bool) {
        let fields = self.visible_fields();
        if fields.is_empty() {
            return;
        }
        let pos = fields
            .iter()
            .position(|f| *f == self.active_field)
            .unwrap_or(0);
        let next = if reverse {
            (pos + fields.len() - 1) % fields.len()
        } else {
            (pos + 1) % fields.len()
        };
        self.active_field = fields[next];
    }

    pub fn active_value_mut(&mut self) -> &mut String {
        match self.active_field {
            ModalField::SshInput => &mut self.ssh_input,
            ModalField::Label => {
                self.auto_filled_label = None;
                &mut self.label
            }
            ModalField::Port => &mut self.port,
            ModalField::IdentityFile => &mut self.identity_file,
            ModalField::KnownHostsFile => &mut self.known_hosts_file,
            ModalField::DockerContainer => &mut self.docker_container,
            ModalField::DockerSshHost => &mut self.docker_ssh_host,
            ModalField::ContainerHost => &mut self.container_host,
            ModalField::ContainerUser => &mut self.container_user,
        }
    }

    /// Shows a live parsed summary when the user types or pastes a full `ssh ...` command.
    pub fn ssh_command_preview(&self) -> Option<Result<String, String>> {
        if self.tab != MachineTab::Ssh {
            return None;
        }
        let trimmed = self.ssh_input.trim();
        if !trimmed.contains(char::is_whitespace) {
            return None;
        }
        match parse_ssh_input(trimmed) {
            Ok(HostTransport::Ssh {
                target,
                port,
                known_hosts_file,
                identity_file,
            }) => {
                let mut parts = vec![format!("Target {target}")];
                if let Some(p) = port {
                    parts.push(format!("Port {p}"));
                }
                if let Some(id) = identity_file {
                    parts.push(format!("Key {id}"));
                }
                if let Some(kh) = known_hosts_file {
                    parts.push(format!("known_hosts {kh}"));
                }
                Some(Ok(format!("Parsed command: {}", parts.join(" · "))))
            }
            Ok(_) => None,
            Err(e) => Some(Err(e)),
        }
    }

    pub fn build_profile(&self, existing_ids: &[String]) -> Result<HostProfile, String> {
        let opt_str = |s: &str| {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_owned())
        };
        let adv_identity = opt_str(&self.identity_file);
        let adv_known_hosts = opt_str(&self.known_hosts_file);

        let (transport, default_label) = match self.tab {
            MachineTab::Ssh => {
                let HostTransport::Ssh {
                    target,
                    mut port,
                    mut known_hosts_file,
                    mut identity_file,
                } = parse_ssh_input(&self.ssh_input)?
                else {
                    return Err("invalid ssh transport".into());
                };
                if let Some(p_str) = opt_str(&self.port) {
                    let p: u16 = p_str
                        .parse()
                        .map_err(|_| format!("Invalid port number: {p_str}"))?;
                    port = Some(p);
                }
                if adv_identity.is_some() {
                    identity_file = adv_identity;
                }
                if adv_known_hosts.is_some() {
                    known_hosts_file = adv_known_hosts;
                }
                let def_label = target.clone();
                (
                    HostTransport::Ssh {
                        target,
                        port,
                        known_hosts_file,
                        identity_file,
                    },
                    def_label,
                )
            }
            MachineTab::Docker => {
                let container = parse_docker_input(&self.docker_container)?;
                let ssh_host = opt_str(&self.docker_ssh_host);
                let def_label = container.clone();
                (
                    HostTransport::DockerExec {
                        container,
                        ssh_host,
                        known_hosts_file: adv_known_hosts,
                        identity_file: adv_identity,
                    },
                    def_label,
                )
            }
            MachineTab::ContainerSsh => {
                let host = self.container_host.trim().to_owned();
                let user = self.container_user.trim().to_owned();
                let port: u16 = self
                    .port
                    .trim()
                    .parse()
                    .map_err(|_| "Enter a valid port number (e.g., 2222)".to_owned())?;
                let def_label = format!("{host}:{port}");
                (
                    HostTransport::ContainerSsh {
                        host,
                        port,
                        user,
                        known_hosts_file: adv_known_hosts,
                        identity_file: adv_identity,
                    },
                    def_label,
                )
            }
        };

        let label = opt_str(&self.label).unwrap_or(default_label);
        let id = match &self.editing_id {
            Some(existing) => existing.clone(),
            None => unique_host_id(&label, existing_ids),
        };
        let expected_host_id = match &self.test_state {
            TestState::Success { host_id } => Some(host_id.clone()),
            _ => self.expected_host_id.clone(),
        };

        let profile = HostProfile {
            id,
            label,
            transport,
            expected_host_id,
        };
        profile.validate()?;
        Ok(profile)
    }

    pub fn build_copy_hint(&self, kind: DialErrorKind) -> Option<String> {
        match kind {
            DialErrorKind::HostKeyMismatch => {
                let profile = self.build_profile(&[]).ok()?;
                match profile.transport {
                    HostTransport::Ssh { target, port, .. } => Some(match port {
                        Some(p) => format!("ssh -p {p} {target} exit"),
                        None => format!("ssh {target} exit"),
                    }),
                    HostTransport::ContainerSsh {
                        host, port, user, ..
                    } => Some(format!("ssh -p {port} {user}@{host} exit")),
                    HostTransport::DockerExec {
                        ssh_host: Some(host),
                        ..
                    } => Some(format!("ssh {host} exit")),
                    _ => None,
                }
            }
            DialErrorKind::AuthRequired => Some("ssh-add -l".into()),
            _ => None,
        }
    }
}

fn unique_host_id(label: &str, existing: &[String]) -> String {
    let mut base: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while base.contains("--") {
        base = base.replace("--", "-");
    }
    let base = base.trim_matches('-').to_owned();
    let base = if base.is_empty() {
        "machine".into()
    } else {
        base
    };
    if !existing.iter().any(|x| x == &base) {
        return base;
    }
    for n in 2..1000 {
        let cand = format!("{base}-{n}");
        if !existing.iter().any(|x| x == &cand) {
            return cand;
        }
    }
    format!("{base}-{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chips_replace_auto_labels_but_preserve_manually_edited_labels() {
        let mut m = MachineModal::new_add();
        m.label = "   ".into();
        m.error = Some("old error".into());
        m.test_state = TestState::Success {
            host_id: "old".into(),
        };
        m.select_ssh_candidate("gpu-one");
        assert_eq!(m.ssh_input, "gpu-one");
        assert_eq!(m.label, "gpu-one");
        assert!(m.error.is_none());
        assert_eq!(m.test_state, TestState::Idle);
        m.select_ssh_candidate("gpu-two");
        assert_eq!(m.label, "gpu-two");

        m.switch_tab(MachineTab::Docker);
        m.select_docker_candidate("dev-one");
        assert_eq!(m.label, "dev-one");
        m.select_docker_candidate("dev-two");
        assert_eq!(m.label, "dev-two");
        m.active_field = ModalField::Label;
        // Re-typing even the same label makes it user-owned.
        *m.active_value_mut() = "dev-two".into();
        assert_eq!(m.auto_filled_label, None);
        m.select_docker_candidate("dev-three");
        assert_eq!(m.docker_container, "dev-three");
        assert_eq!(m.label, "dev-two");
        m.select_ssh_candidate("gpu-three");
        assert_eq!(m.label, "dev-two");
    }

    #[test]
    fn only_ssh_tabs_support_install_and_in_flight_operations_are_busy() {
        let mut m = MachineModal::new_add();
        assert!(m.supports_remote_install());
        assert!(!m.is_busy());
        m.switch_tab(MachineTab::Docker);
        assert!(!m.supports_remote_install());
        m.switch_tab(MachineTab::ContainerSsh);
        assert!(m.supports_remote_install());
        m.test_state = TestState::Running { req_id: 1 };
        assert!(m.is_busy());
        m.test_state = TestState::Installing {
            req_id: 2,
            stage: "upload".into(),
        };
        assert!(m.is_busy());
        m.switch_tab(MachineTab::Docker);
        assert_eq!(m.tab, MachineTab::ContainerSsh);
        m.test_state = TestState::Success {
            host_id: "verified".into(),
        };
        assert!(!m.is_busy());
        assert_eq!(
            m.build_profile(&[]).unwrap().expected_host_id.as_deref(),
            Some("verified")
        );
    }

    #[test]
    fn builds_profile_from_pasted_ssh_command() {
        let mut m = MachineModal::new_add();
        m.ssh_input = "ssh -i /tmp/id_ed25519 -p 2222 ubuntu@10.0.0.5".into();
        let preview = m.ssh_command_preview().unwrap().unwrap();
        assert!(preview.contains("ubuntu@10.0.0.5"));
        assert!(preview.contains("2222"));

        let prof = m.build_profile(&["local".into()]).unwrap();
        assert_eq!(prof.label, "ubuntu@10.0.0.5");
        assert_eq!(
            prof.transport,
            HostTransport::Ssh {
                target: "ubuntu@10.0.0.5".into(),
                port: Some(2222),
                known_hosts_file: None,
                identity_file: Some("/tmp/id_ed25519".into()),
            }
        );
    }

    #[test]
    fn builds_profile_from_pasted_docker_exec_command() {
        let mut m = MachineModal::new_add();
        m.switch_tab(MachineTab::Docker);
        m.docker_container = "docker exec -it ddg-dev zsh".into();
        let prof = m.build_profile(&["local".into()]).unwrap();
        assert_eq!(prof.label, "ddg-dev");
        assert_eq!(
            prof.transport,
            HostTransport::DockerExec {
                container: "ddg-dev".into(),
                ssh_host: None,
                known_hosts_file: None,
                identity_file: None,
            }
        );
    }
}
