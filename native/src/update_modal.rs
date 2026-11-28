use super::{Desktop, MachineView};
use crate::update_controller::{
    Banner, Change, ErrorCard, UpdatePhase, UpdateResult, card, container_update_command,
    display_error, hold_quit, open_url,
};
use ddg_agents_transport::{ConnectionState, HostTransport};
use ddg_agents_update::{
    ApplyOutcome, UpdateError, UpdateErrorCode as Code,
    config::{AutoCheckPolicy, RepoSlug, SourceOrigin, UpdateChannel},
    manager::UpdateManager,
    redact::redact_sensitive_text,
    release::{build_release_web_url, sanitize_release_notes},
    state::AutoRolledBackNotice,
    version::ReleaseTag,
};
use gpui::{prelude::*, *};
use std::sync::atomic::Ordering;

pub const UPDATE_BANNER_HEIGHT: f32 = 27.;
const AMBER: u32 = 0xe5c07b;
const BLUE: u32 = 0x8db7ff;

#[derive(Clone, Debug)]
struct Confirmation {
    target: Option<String>,
    change: Change,
    restart: bool,
    downgrade: bool,
    label: &'static str,
}

#[derive(Default)]
pub struct UpdateModal {
    inline_confirm: Option<Confirmation>,
    container_guide_expanded: bool,
    copied_hint: bool,
}

impl UpdateModal {
    pub fn has_confirmation(&self) -> bool {
        self.inline_confirm.is_some()
    }
    pub fn cancel_confirmation(&mut self) {
        self.inline_confirm = None;
    }
}

pub enum BlockedCard {
    ProtocolIncompatible {
        title: String,
        action: &'static str,
        guide: bool,
        local_old: bool,
    },
}

impl BlockedCard {
    pub fn for_machine(machine: &MachineView) -> Option<Self> {
        let ConnectionState::ProtocolIncompatible {
            local_protocol,
            remote_protocol,
            remote_version,
            message,
        } = &machine.conn_state
        else {
            return None;
        };
        let remote_old = *remote_protocol != 0 && remote_protocol < local_protocol;
        let (action, guide, local_old) = match (&machine.profile.transport, remote_old) {
            (HostTransport::Ssh { .. }, true) => ("Update Remote Host", false, false),
            (HostTransport::DockerExec { .. } | HostTransport::ContainerSsh { .. }, true) => {
                ("View Guide", true, false)
            }
            _ => ("Check for Local Updates", false, true),
        };
        let version = remote_version
            .as_ref()
            .map(|v| format!(" · Remote v{}", display_error(v)))
            .unwrap_or_default();
        Some(Self::ProtocolIncompatible {
            title: format!(
                "Incompatible protocol versions (remote p{remote_protocol} / local p{local_protocol}){version}\n{}",
                display_error(message)
            ),
            action,
            guide,
            local_old,
        })
    }
}

fn rolled_back(notice: &AutoRolledBackNotice) -> String {
    format!(
        "Could not apply v{} · Automatically rolled back to v{}",
        display_error(&notice.attempted_version),
        display_error(&notice.restored_version)
    )
}

fn phase_text(phase: &UpdatePhase) -> String {
    match phase {
        UpdatePhase::Idle => "You can check for updates.".into(),
        UpdatePhase::Checking => "Checking for updates…".into(),
        UpdatePhase::UpToDate => "You're up to date.".into(),
        UpdatePhase::UpdateAvailable(release) => format!("v{} is available", release.version),
        UpdatePhase::Applying(version) => format!("Applying {version}…"),
        UpdatePhase::PendingHolderRestart(_) => {
            "Local host (Holder) restart required · Running sessions will be affected".into()
        }
        UpdatePhase::AutoRolledBack(notice) => rolled_back(notice),
        UpdatePhase::Error(error) => error.title.into(),
    }
}

fn remote_skew(machine: &MachineView, local: &str) -> Option<String> {
    let remote = ReleaseTag::parse(machine.remote_update.version.app_version.as_deref()?).ok()?;
    let local = ReleaseTag::parse(local).ok()?;
    (machine.is_connected()
        && !matches!(machine.profile.transport, HostTransport::Local { .. })
        && remote.version() != local.version())
    .then(|| remote.version().to_string())
}

impl Desktop {
    pub(crate) fn open_update_modal(
        &mut self,
        guide: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .machine_modal()
            .is_some_and(|modal| modal.is_installing())
        {
            return;
        }
        self.active_overlay = None;
        let modal = self.update_modal.get_or_insert_with(UpdateModal::default);
        modal.container_guide_expanded |= guide;
        self.focus.focus(window);
        cx.notify();
    }

    pub(crate) fn close_update_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.update_modal = None;
        self.focus.focus(window);
        cx.notify();
    }

    pub(crate) fn check_updates(&mut self, background: bool) {
        if !self.update.can_check(background) {
            return;
        }
        let paths = match self.update.paths.clone() {
            Ok(paths) => paths,
            Err(error) => {
                self.update.phase = UpdatePhase::Error(card(&error));
                return;
            }
        };
        self.update.phase = UpdatePhase::Checking;
        self.last_local_change = None;
        self.update.active_req_id = Some(self.bridge.run_update(None, move || async move {
            UpdateResult::Check(
                UpdateManager::new(paths)
                    .check_for_updates(background)
                    .await,
            )
        }));
    }

    fn configure_update(
        &mut self,
        edit: impl FnOnce(&mut ddg_agents_update::config::UpdateConfig),
        check: bool,
    ) {
        if self.update.active_req_id.is_some() {
            return;
        }
        let mut config = self.update.config.clone();
        edit(&mut config);
        match self
            .update
            .paths
            .clone()
            .and_then(|paths| UpdateManager::new(paths).save_config(&config))
        {
            Ok(()) => {
                let source_changed = self.update.config.channel != config.channel
                    || self.update.config.repo != config.repo;
                self.update.config = config;
                if let Some(modal) = &mut self.update_modal {
                    modal.cancel_confirmation();
                }
                if check {
                    if source_changed
                        && matches!(self.update.phase, UpdatePhase::UpdateAvailable(_))
                    {
                        self.update.phase = UpdatePhase::Idle;
                    }
                    self.check_updates(true);
                }
            }
            Err(error) => self.update.phase = UpdatePhase::Error(card(&error)),
        }
    }

    pub(crate) fn hold_update_quit(&mut self) -> bool {
        if !hold_quit(
            self.update.is_applying(),
            self.update.quitting_for_update_restart,
        ) {
            return false;
        }
        self.update.quit_requested_during_apply = true;
        self.notice = Some("The app will wait for the update to finish before quitting.".into());
        true
    }

    fn confirm_change(&mut self, confirmation: Confirmation) {
        if let Some(modal) = &mut self.update_modal {
            modal.inline_confirm = Some(confirmation);
        }
    }

    fn request_rollback(&mut self, target: Option<String>) {
        self.confirm_change(Confirmation {
            target,
            change: Change::Rollback,
            restart: false,
            downgrade: false,
            label: "Roll Back to Previous Version",
        });
    }

    fn run_change(
        &mut self,
        target: Option<String>,
        change: Change,
        restart: bool,
        downgrade: bool,
    ) {
        let (profile, version) = if let Some(id) = &target {
            let Some(machine) = self.machine(id).filter(|m| {
                m.install_req_id.is_none()
                    && m.remote_update.req_id.is_none()
                    && matches!(m.profile.transport, HostTransport::Ssh { .. })
            }) else {
                return;
            };
            (
                Some(machine.profile.clone()),
                machine.remote_update.version.clone(),
            )
        } else {
            if self.update.active_req_id.is_some() {
                return;
            }
            let info = self
                .machine("local")
                .filter(|m| {
                    matches!(
                        m.profile.transport,
                        HostTransport::Local { sock_path: None }
                    )
                })
                .map(|m| m.remote_update.version.clone())
                .unwrap_or_default();
            (None, info)
        };
        let paths = match self.update.paths.clone() {
            Ok(paths) => paths,
            Err(error) => {
                self.update.phase = UpdatePhase::Error(card(&error));
                return;
            }
        };
        if let Some(modal) = &mut self.update_modal {
            modal.cancel_confirmation();
        }
        let label = match &change {
            Change::LocalApply { tag, .. } => tag.clone(),
            Change::ApplyRemote => "Update Remote Host".into(),
            Change::Rollback => "Previous Version".into(),
        };
        let operation = change.clone();
        if target.is_none() {
            self.update.applying_guard.store(true, Ordering::Release);
        }
        let applying = target.is_none().then(|| self.update.applying_guard.clone());
        let req_id = self.bridge.run_update(target.clone(), move || async move {
            let result =
                crate::update_apply::apply(paths, profile, version, operation, restart, downgrade)
                    .await;
            if let Some(applying) = applying {
                applying.store(false, Ordering::Release);
            }
            UpdateResult::Apply(result)
        });
        if let Some(id) = target {
            if let Some(machine) = self.machine_mut(&id) {
                machine.remote_update.req_id = Some(req_id);
                machine.remote_update.phase = UpdatePhase::Applying(label);
                machine.remote_update.last_change = Some(change);
            }
        } else {
            self.update.phase = UpdatePhase::Applying(label);
            self.update.active_req_id = Some(req_id);
            self.last_local_change = Some(change);
        }
    }

    pub(crate) fn finish_update(
        &mut self,
        req_id: u64,
        target: Option<String>,
        result: UpdateResult,
        cx: &mut Context<Self>,
    ) {
        let last_change = if let Some(id) = &target {
            let Some(machine) = self
                .machine_mut(id)
                .filter(|m| m.remote_update.req_id == Some(req_id))
            else {
                return;
            };
            machine.remote_update.req_id = None;
            machine.remote_update.last_change.clone()
        } else {
            if self.update.active_req_id != Some(req_id) {
                return;
            }
            self.update.active_req_id = None;
            self.update.applying_guard.store(false, Ordering::Release);
            self.last_local_change.clone()
        };
        let result = match result {
            UpdateResult::Check(result) => {
                self.update.finish_check(result);
                return;
            }
            UpdateResult::Apply(result) => result,
        };
        let result = match result {
            Ok(ApplyOutcome::PendingHolderRestart { to_version, .. }) => {
                if target.is_none() {
                    self.update.phase = UpdatePhase::PendingHolderRestart(to_version);
                }
                Err(UpdateError::new(
                    Code::HolderRestartRequired,
                    "Confirmation is required to stop running sessions.",
                ))
            }
            other => other,
        };
        if let Err(error) = &result
            && let Some(change) = last_change
        {
            let restart = error.code == Code::HolderRestartRequired;
            let downgrade = error.code == Code::DowngradeDenied;
            if (restart || downgrade) && target.as_ref().is_none_or(|id| id == &self.active_host_id)
            {
                self.confirm_change(Confirmation {
                    target: target.clone(),
                    change,
                    restart,
                    downgrade,
                    label: if restart {
                        "Stop Sessions & Apply"
                    } else {
                        "Apply Downgrade"
                    },
                });
            }
        }
        if let Some(id) = target {
            if let Some(machine) = self.machine_mut(&id) {
                let reconnect = matches!(
                    result,
                    Ok(ApplyOutcome::Completed { .. } | ApplyOutcome::AutoRolledBack { .. })
                );
                machine.remote_update.phase = match result {
                    Ok(ApplyOutcome::AutoRolledBack {
                        attempted_version,
                        restored_version,
                        reason,
                    }) => UpdatePhase::AutoRolledBack(AutoRolledBackNotice {
                        attempted_version,
                        restored_version,
                        reason,
                    }),
                    Ok(_) => UpdatePhase::UpToDate,
                    Err(error) => UpdatePhase::Error(card(&error)),
                };
                if reconnect {
                    let profile = machine.profile.clone();
                    self.bridge.add_or_update_machine(profile);
                }
            }
        } else {
            let wanted_quit = std::mem::take(&mut self.update.quit_requested_during_apply);
            if wanted_quit {
                self.notice = None;
            }
            match result {
                Ok(outcome) => {
                    if self.update.finish_apply(outcome) {
                        cx.quit();
                    } else {
                        let _ = self.update.load();
                        self.notice = Some(
                            "Rollback complete. Reopen the app to use the restored version.".into(),
                        );
                        if wanted_quit {
                            cx.quit();
                        }
                    }
                }
                Err(error) if error.code == Code::HolderRestartRequired => {
                    let version = match &self.last_local_change {
                        Some(Change::LocalApply { tag, .. }) => tag.clone(),
                        _ => String::new(),
                    };
                    self.update.phase = UpdatePhase::PendingHolderRestart(version);
                }
                Err(error) => self.update.phase = UpdatePhase::Error(card(&error)),
            }
        }
    }

    pub(crate) fn update_sidebar_row(&self, sidebar: f32, cx: &mut Context<Self>) -> Stateful<Div> {
        let remote = self.active_machine().map(|m| &m.remote_update.phase);
        let rolled_back = |phase: &UpdatePhase| {
            matches!(phase, UpdatePhase::AutoRolledBack(_))
                || matches!(phase, UpdatePhase::Error(error) if error.code == Code::DaemonSwapFailedRolledBack)
        };
        let applying = |phase: &UpdatePhase| matches!(phase, UpdatePhase::Applying(_));
        let status = if rolled_back(&self.update.phase) || remote.is_some_and(rolled_back) {
            Some(("Rolled back", AMBER))
        } else if applying(&self.update.phase) || remote.is_some_and(applying) {
            Some(("Applying", AMBER))
        } else if matches!(
            self.update.phase,
            UpdatePhase::UpdateAvailable(_) | UpdatePhase::PendingHolderRestart(_)
        ) {
            Some(("Update", BLUE))
        } else {
            None
        };
        div()
            .id("update-version")
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .px_1()
            .py_0p5()
            .cursor_pointer()
            .rounded_sm()
            .hover(|d| d.bg(rgb(0x262626)))
            .text_xs()
            .text_color(rgb(0xb8b8b8))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(format!("v{}", self.update.current_version)),
            )
            .when_some(status, |el, (text, color)| {
                el.child(
                    div()
                        .flex_none()
                        .whitespace_nowrap()
                        .text_color(rgb(color))
                        .child(if sidebar >= self.ui_scale.px(170.) {
                            format!("● {text}")
                        } else {
                            "●".into()
                        }),
                )
            })
            .on_click(cx.listener(|this, _, window, cx| this.open_update_modal(false, window, cx)))
    }

    pub(crate) fn update_banner_view(&self, blocked: bool, cx: &mut Context<Self>) -> Option<Div> {
        let remote = self
            .active_machine()
            .filter(|m| !matches!(m.profile.transport, HostTransport::Local { .. }));
        let skew = remote
            .and_then(|m| remote_skew(m, &self.update.current_version))
            .filter(|v| remote.is_some_and(|m| m.remote_update.dismissed_skew.as_ref() != Some(v)));
        let remote_rolled_back = remote.is_some_and(|m| {
            matches!(
                &m.remote_update.phase,
                UpdatePhase::AutoRolledBack(_)
                    | UpdatePhase::Error(ErrorCard {
                        code: Code::DaemonSwapFailedRolledBack,
                        ..
                    })
            )
        });
        let progress =
            remote.is_some_and(|m| m.remote_update.req_id.is_some()) || remote_rolled_back;
        let slot = self.update.banner(progress || skew.is_some(), blocked)?;
        let text = if slot == Banner::Remote {
            let machine = remote?;
            let status = match &machine.remote_update.phase {
                UpdatePhase::Error(error) if remote_rolled_back => {
                    format!("{} · {}", error.title, error.message)
                }
                phase if progress => phase_text(phase),
                _ => format!(
                    "Version mismatch (remote v{} / local v{})",
                    skew.as_deref().unwrap_or_default(),
                    self.update.current_version
                ),
            };
            format!("[{}] {status}", display_error(&machine.profile.label))
        } else if matches!(self.update.phase, UpdatePhase::Applying(_)) {
            format!(
                "{} The app will restart automatically when the update finishes.",
                phase_text(&self.update.phase)
            )
        } else {
            phase_text(&self.update.phase)
        };
        let guide = slot == Banner::Remote
            && remote.is_some_and(|m| {
                matches!(
                    m.profile.transport,
                    HostTransport::DockerExec { .. } | HostTransport::ContainerSsh { .. }
                )
            });
        let action = if guide {
            "View Guide"
        } else if slot == Banner::Remote && !progress {
            "Update Remote Host"
        } else {
            "Details"
        };
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap_2()
                .h(px(self.ui_scale.px(UPDATE_BANNER_HEIGHT)))
                .overflow_hidden()
                .px_1p5()
                .text_xs()
                .bg(rgb(if slot == Banner::Local || remote_rolled_back {
                    0x352a1b
                } else {
                    0x222b3a
                }))
                .text_color(rgb(0xf2f2f2))
                .child(div().flex_1().min_w_0().truncate().child(text))
                .child(Self::action_button(
                    cx,
                    "update-banner-details",
                    action,
                    move |this, window, cx| this.open_update_modal(guide, window, cx),
                ))
                .when(
                    slot == Banner::Available || (slot == Banner::Remote && !progress),
                    |el| {
                        el.child(Self::action_button(
                            cx,
                            "update-dismiss",
                            "×",
                            move |this, _, _| {
                                if slot == Banner::Available {
                                    this.update.dismiss_available();
                                } else if let Some(machine) = this
                                    .machines
                                    .iter_mut()
                                    .find(|m| m.profile.id == this.active_host_id)
                                {
                                    machine.remote_update.dismissed_skew = skew.clone();
                                }
                            },
                        ))
                    },
                )
                .when(
                    slot == Banner::Local
                        && matches!(self.update.phase, UpdatePhase::AutoRolledBack(_)),
                    |el| {
                        el.child(Self::action_button(
                            cx,
                            "update-ack",
                            "Confirm",
                            |this, _, _| this.acknowledge_update_rollback(),
                        ))
                    },
                ),
        )
    }

    fn acknowledge_update_rollback(&mut self) {
        match self.update.acknowledge_rollback() {
            Ok(()) => self.check_updates(true),
            Err(error) => self.notice = Some(display_error(error.message())),
        }
    }

    fn render_update_error(
        &self,
        error: &ErrorCard,
        target: Option<String>,
        retry: Option<Change>,
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_1()
            .rounded_sm()
            .bg(rgb(0x34262a))
            .child(div().font_weight(FontWeight::SEMIBOLD).child(error.title))
            .child(error.message.clone())
            .when(error.code == Code::RateLimited, |el| {
                el.child(error.reset_at.map_or_else(
                    || "Rate limit reset time: unknown".into(),
                    |at| format!("Rate limit reset time (Unix UTC): {at}"),
                ))
            })
            .when_some(error.copy_hint, |el, hint| {
                el.child(Self::action_button(
                    cx,
                    "update-copy-hint",
                    "Copy Command",
                    move |this, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(hint.into()));
                        if let Some(modal) = &mut this.update_modal {
                            modal.copied_hint = true;
                        }
                    },
                ))
            })
            .when(error.can_retry, |el| {
                el.child(Self::action_button(
                    cx,
                    "update-retry",
                    "Retry",
                    move |this, _, _| {
                        if let Some(change) = &retry {
                            if matches!(change, Change::Rollback) {
                                this.request_rollback(target.clone());
                            } else {
                                this.run_change(target.clone(), change.clone(), false, false);
                            }
                        } else {
                            this.check_updates(false);
                        }
                    },
                ))
            })
    }

    pub(crate) fn update_modal_view(&self, cx: &mut Context<Self>) -> Option<Div> {
        let modal = self.update_modal.as_ref()?;
        let busy = self.update.active_req_id.is_some();
        let config = &self.update.config;
        let section = || {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .border_t_1()
                .border_color(rgb(0x424851))
                .pt_1p5()
        };
        let row = || div().flex().flex_wrap().items_center().gap_2();
        let release = if let UpdatePhase::UpdateAvailable(release) = &self.update.phase {
            Some(release)
        } else {
            None
        };
        let mut body = div()
            .id("update-modal-scroll")
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3();
        match config.effective_repo() {
            Ok((repo, origin @ (SourceOrigin::Config | SourceOrigin::Env))) => {
                body = body.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_1()
                        .rounded_sm()
                        .bg(rgb(0x352a1b))
                        .text_color(rgb(0xf1d6a8))
                        .child(format!(
                            "[Custom Source] {} · Source: {}",
                            repo.as_str(),
                            if origin == SourceOrigin::Config {
                                "Settings"
                            } else {
                                "Environment variable"
                            }
                        ))
                        .when(origin == SourceOrigin::Config && !busy, |e| {
                            e.child(Self::action_button(
                                cx,
                                "update-reset-source",
                                "Restore Default Source",
                                |this, _, _| this.configure_update(|c| c.repo = None, true),
                            ))
                        }),
                );
            }
            Err(error) => {
                body = body.child(self.render_update_error(&card(&error), None, None, cx))
            }
            Ok(_) => {}
        }
        let local = section().child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
            "Local app · v{} · {}",
            self.update.current_version,
            phase_text(&self.update.phase)
        )));
        let local = match &self.update.phase {
            UpdatePhase::Error(error) => local.child(self.render_update_error(
                error,
                None,
                self.last_local_change.clone(),
                cx,
            )),
            UpdatePhase::AutoRolledBack(notice) => local
                .child(display_error(&notice.reason))
                .child(Self::action_button(
                    cx,
                    "update-modal-ack",
                    "Confirm",
                    |this, _, _| this.acknowledge_update_rollback(),
                )),
            UpdatePhase::PendingHolderRestart(tag) => {
                let change = self
                    .last_local_change
                    .clone()
                    .unwrap_or_else(|| Change::LocalApply {
                        tag: tag.clone(),
                        repo: None,
                    });
                local.child(Self::action_button(
                    cx,
                    "update-holder",
                    "Confirm Session Restart",
                    move |this, _, _| {
                        this.confirm_change(Confirmation {
                            target: None,
                            change: change.clone(),
                            restart: true,
                            downgrade: false,
                            label: "Stop Sessions & Apply",
                        });
                    },
                ))
            }
            _ => local,
        };
        body = body.child(local.when_some(release, |el, release| {
            let url = RepoSlug::parse(&release.repo)
                .and_then(|repo| {
                    ReleaseTag::parse(&release.tag).map(|tag| build_release_web_url(&repo, &tag))
                })
                .ok();
            let change = Change::LocalApply {
                tag: release.tag.clone(),
                repo: Some(release.repo.clone()),
            };
            el.child(
                div()
                    .text_xs()
                    .text_color(rgb(0xc9cdd4))
                    .child(config_channel(config.channel)),
            )
            .children(
                redact_sensitive_text(&sanitize_release_notes(&release.notes))
                    .lines()
                    .map(|line| div().text_sm().child(line.to_owned())),
            )
            .child(
                div()
                    .text_sm()
                    .child("The app restarts automatically after download and verification."),
            )
            .child(
                row()
                    .when_some(url, |el, url| {
                        el.child(Self::action_button(
                            cx,
                            "update-notes",
                            "Open Release Notes ↗",
                            move |_, _, cx| {
                                if open_url(&url).is_ok() {
                                    cx.open_url(&url);
                                }
                            },
                        ))
                    })
                    .child(Self::action_button(
                        cx,
                        "update-apply",
                        "Apply Update",
                        move |this, _, _| this.run_change(None, change.clone(), false, false),
                    )),
            )
        }));
        body = body.child(
            section()
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Settings"))
                .when(!busy, |el| {
                    el.child(
                        row()
                            .child("Auto-check")
                            .child(Self::action_button(
                                cx,
                                "update-auto",
                                if self.update.auto_check() {
                                    "● On"
                                } else {
                                    "Off"
                                },
                                |this, _, _| {
                                    this.configure_update(
                                        |c| {
                                            c.auto_check = if c.auto_check == AutoCheckPolicy::On {
                                                AutoCheckPolicy::Off
                                            } else {
                                                AutoCheckPolicy::On
                                            }
                                        },
                                        true,
                                    );
                                },
                            ))
                            .child("Channel")
                            .children([UpdateChannel::Stable, UpdateChannel::Prerelease].map(
                                |channel| {
                                    let label = config_channel(channel);
                                    Self::action_button(
                                        cx,
                                        label,
                                        if channel == config.channel {
                                            format!("● {label}")
                                        } else {
                                            label.into()
                                        },
                                        move |this, _, _| {
                                            if this.update.config.channel != channel {
                                                this.configure_update(
                                                    |c| c.channel = channel,
                                                    true,
                                                );
                                            }
                                        },
                                    )
                                },
                            ))
                            .when(self.update.can_check(false), |el| {
                                el.child(Self::action_button(
                                    cx,
                                    "update-check-now",
                                    "Check Now",
                                    |this, _, _| this.check_updates(false),
                                ))
                            }),
                    )
                    .when_some(
                        self.update.rollback_version.as_ref(),
                        |el, version| {
                            el.child(Self::action_button(
                                cx,
                                "update-rollback",
                                format!("Roll Back to v{}", display_error(version)),
                                |this, _, _| this.request_rollback(None),
                            ))
                        },
                    )
                })
                .when(busy, |el| {
                    el.child("Update settings cannot be changed while an update is in progress.")
                }),
        );
        if let Some(machine) = self.active_machine() {
            let remote = &machine.remote_update;
            let id = &machine.profile.id;
            match &machine.profile.transport {
                HostTransport::Ssh { .. } => {
                    let apply_id = id.clone();
                    let rollback_id = id.clone();
                    let version = remote
                        .version
                        .app_version
                        .as_deref()
                        .unwrap_or("Unknown");
                    let mut part =
                        section().child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                            "Remote Host · {} · {}",
                            display_error(&machine.profile.label),
                            display_error(version)
                        )));
                    if let Some(runtime) = remote
                        .version
                        .platform
                        .as_ref()
                        .or(remote.version.terminal_server_version.as_ref())
                    {
                        part = part.child(div().text_xs().child(display_error(runtime)));
                    }
                    if let Some(rollback) = &remote.version.rollback_version {
                        part = part.child(
                            div()
                                .text_xs()
                                .child(format!("Previous backup: v{}", display_error(rollback))),
                        );
                    }
                    body = body.child(
                        part.when(
                            machine.install_req_id.is_none() && remote.req_id.is_none(),
                            |el| {
                                el.child(
                                    row()
                                        .child(Self::action_button(
                                            cx,
                                            "update-remote",
                                            "Update Remote Host",
                                            move |this, _, _| {
                                                this.run_change(
                                                    Some(apply_id.clone()),
                                                    Change::ApplyRemote,
                                                    false,
                                                    false,
                                                )
                                            },
                                        ))
                                        .child(Self::action_button(
                                            cx,
                                            "rollback-remote",
                                            "Roll Back Remote Host",
                                            move |this, _, _| {
                                                this.request_rollback(Some(rollback_id.clone()))
                                            },
                                        )),
                                )
                            },
                        )
                        .when_some(
                            match &remote.phase {
                                UpdatePhase::Idle | UpdatePhase::Error(_) => None,
                                phase => Some(phase_text(phase)),
                            },
                            |el, text| el.child(text),
                        )
                        .when_some(
                            if let UpdatePhase::Error(error) = &remote.phase {
                                Some(error)
                            } else {
                                None
                            },
                            |el, error| {
                                el.child(self.render_update_error(
                                    error,
                                    Some(id.clone()),
                                    remote.last_change.clone(),
                                    cx,
                                ))
                            },
                        ),
                    );
                }
                HostTransport::DockerExec { .. } | HostTransport::ContainerSsh { .. } => {
                    let command = container_update_command(
                        "<image>",
                        release.map_or("<tag>", |r| r.tag.as_str()),
                    );
                    body = body.child(
                        section()
                            .child("Update the container image, then recreate the container.")
                            .child(Self::action_button(
                                cx,
                                "update-container-guide",
                                if modal.container_guide_expanded {
                                    "▴ Hide Guide"
                                } else {
                                    "▾ View Guide"
                                },
                                |this, _, _| {
                                    if let Some(modal) = &mut this.update_modal {
                                        modal.container_guide_expanded =
                                            !modal.container_guide_expanded;
                                    }
                                },
                            ))
                            .when(modal.container_guide_expanded, |el| {
                                el.child(
                                    div()
                                        .id("container-update-command")
                                        .overflow_x_scroll()
                                        .whitespace_nowrap()
                                        .text_sm()
                                        .p_1()
                                        .bg(rgb(0x15181c))
                                        .child(command.clone()),
                                )
                                .child(Self::action_button(
                                    cx,
                                    "update-container-copy",
                                    "Copy",
                                    move |this, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            command.clone(),
                                        ));
                                        if let Some(modal) = &mut this.update_modal {
                                            modal.copied_hint = true;
                                        }
                                    },
                                ))
                            }),
                    );
                }
                HostTransport::Local { .. } => {}
            }
        }
        let confirmation = modal.inline_confirm.clone().map(|confirm| {
            div()
                .flex()
                .flex_none()
                .flex_col()
                .gap_2()
                .p_1p5()
                .rounded_sm()
                .bg(rgb(0x352a1b))
                .text_color(rgb(0xf1d6a8))
                .child("⚠ Confirmation required · Enter will not run this. Click the button.")
                .child(
                    row()
                        .child(Self::action_button(
                            cx,
                            "update-cancel",
                            "Cancel (Esc)",
                            |this, _, _| {
                                if let Some(modal) = &mut this.update_modal {
                                    modal.cancel_confirmation();
                                }
                            },
                        ))
                        .child(Self::action_button(
                            cx,
                            "update-confirm",
                            confirm.label,
                            move |this, _, _| {
                                if this
                                    .update_modal
                                    .as_ref()
                                    .is_some_and(UpdateModal::has_confirmation)
                                {
                                    this.run_change(
                                        confirm.target.clone(),
                                        confirm.change.clone(),
                                        confirm.restart,
                                        confirm.downgrade,
                                    );
                                }
                            },
                        )),
                )
        });
        if modal.copied_hint {
            body = body.child(div().text_xs().child("Copied to clipboard."));
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .p_1p5()
                .bg(rgba(0x00000088))
                .child(
                    div()
                        .w(px(self.ui_scale.px(442.)))
                        .debug_selector(|| "update-modal-panel".into())
                        .max_w_full()
                        .max_h_full()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_1p5()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(0x424851))
                        .bg(rgb(0x202328))
                        .child(
                            div()
                                .flex()
                                .flex_none()
                                .items_center()
                                .justify_between()
                                .child(div().font_weight(FontWeight::SEMIBOLD).child("Update"))
                                .child(Self::action_button(
                                    cx,
                                    "update-close",
                                    "Close ×",
                                    |this, window, cx| this.close_update_modal(window, cx),
                                )),
                        )
                        .child(body)
                        .children(confirmation),
                ),
        )
    }
}

fn config_channel(channel: UpdateChannel) -> &'static str {
    match channel {
        UpdateChannel::Stable => "stable",
        UpdateChannel::Prerelease => "prerelease",
    }
}
