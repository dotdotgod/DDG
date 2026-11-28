use ddg_agents_proto::id::sanitize_display_text;
use ddg_agents_update::{
    ApplyOutcome, UpdateError, UpdateErrorCode as Code,
    config::{AutoCheckPolicy, UpdateConfig},
    manager::{CheckOutcome, UpdateManager, UpdatePaths},
    redact::redact_sensitive_text,
    release::validate_open_url,
    state::{AutoRolledBackNotice, ReleaseSummary, UpdateStateStore},
    version::ReleaseTag,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Banner {
    Local,
    Remote,
    Available,
}

#[derive(Clone, Debug, Default)]
pub enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    UpToDate,
    UpdateAvailable(ReleaseSummary),
    Applying(String),
    PendingHolderRestart(String),
    AutoRolledBack(AutoRolledBackNotice),
    Error(ErrorCard),
}

pub struct UpdateController {
    pub current_version: String,
    pub config: UpdateConfig,
    pub rollback_version: Option<String>,
    pub phase: UpdatePhase,
    pub active_req_id: Option<u64>,
    pub dismissed_available_version: Option<String>,
    pub quit_requested_during_apply: bool,
    pub quitting_for_update_restart: bool,
    pub applying_guard: Arc<AtomicBool>,
    pub paths: Result<UpdatePaths, UpdateError>,
}

impl Default for UpdateController {
    fn default() -> Self {
        Self {
            current_version: env!("CARGO_PKG_VERSION").into(),
            config: UpdateConfig::default(),
            rollback_version: None,
            phase: UpdatePhase::Idle,
            active_req_id: None,
            dismissed_available_version: None,
            quit_requested_during_apply: false,
            quitting_for_update_restart: false,
            applying_guard: Arc::default(),
            paths: UpdatePaths::from_environment(),
        }
    }
}

impl UpdateController {
    pub fn load(&mut self) -> Result<(), UpdateError> {
        let paths = self.paths.clone()?;
        let manager = UpdateManager::new(paths.clone());
        self.config = manager.load_config()?;
        let state = manager.load_state()?;
        self.rollback_version = rollback_version(&paths);
        if let Some(notice) = state.auto_rolled_back {
            self.phase = UpdatePhase::AutoRolledBack(notice);
        } else if let Some(pending) = state.pending_holder_restart {
            self.phase = UpdatePhase::PendingHolderRestart(pending.to_version);
        }
        Ok(())
    }

    pub fn auto_check(&self) -> bool {
        self.config.auto_check == AutoCheckPolicy::On
    }

    pub fn can_check(&self, background: bool) -> bool {
        self.active_req_id.is_none()
            && !matches!(
                self.phase,
                UpdatePhase::AutoRolledBack(_) | UpdatePhase::PendingHolderRestart(_)
            )
            && (!background || self.auto_check())
    }

    pub fn is_applying(&self) -> bool {
        self.applying_guard.load(Ordering::Acquire)
            || matches!(self.phase, UpdatePhase::Applying(_))
    }

    pub fn banner(&self, remote: bool, protocol_blocked: bool) -> Option<Banner> {
        if matches!(
            self.phase,
            UpdatePhase::Applying(_)
                | UpdatePhase::PendingHolderRestart(_)
                | UpdatePhase::AutoRolledBack(_)
        ) {
            Some(Banner::Local)
        } else if remote && !protocol_blocked {
            Some(Banner::Remote)
        } else if let UpdatePhase::UpdateAvailable(release) = &self.phase
            && self.dismissed_available_version.as_deref() != Some(&release.version)
        {
            Some(Banner::Available)
        } else {
            None
        }
    }

    pub fn dismiss_available(&mut self) {
        if let UpdatePhase::UpdateAvailable(release) = &self.phase {
            self.dismissed_available_version = Some(release.version.clone());
        }
    }

    pub fn finish_check(&mut self, outcome: Result<Option<CheckOutcome>, UpdateError>) {
        self.phase = match outcome {
            Ok(Some(CheckOutcome::UpdateAvailable(plan)))
                if ReleaseTag::parse(&plan.release.tag).is_ok_and(|tag| {
                    ReleaseTag::parse(&self.current_version).is_ok_and(|current| {
                        tag.version().cmp_precedence(current.version()).is_gt()
                    })
                }) =>
            {
                UpdatePhase::UpdateAvailable(plan.release)
            }
            Ok(Some(CheckOutcome::UpToDate { .. } | CheckOutcome::UpdateAvailable(_))) => {
                UpdatePhase::UpToDate
            }
            Ok(Some(CheckOutcome::FailedVersionBlocked { reason, .. })) => {
                UpdatePhase::Error(error_card(Code::FailedVersionBlocked, &reason, None))
            }
            Ok(Some(CheckOutcome::RateLimited { reset_at, .. })) => UpdatePhase::Error(error_card(
                Code::RateLimited,
                "GitHub rate limit reached.",
                reset_at,
            )),
            Ok(Some(CheckOutcome::Disabled) | None) => UpdatePhase::Idle,
            Err(error) => UpdatePhase::Error(card(&error)),
        };
    }

    pub fn finish_apply(&mut self, outcome: ApplyOutcome) -> bool {
        match outcome {
            ApplyOutcome::HandedOffToHelper { .. } => {
                self.quitting_for_update_restart = true;
                return true;
            }
            ApplyOutcome::PendingHolderRestart { to_version, .. } => {
                self.phase = UpdatePhase::PendingHolderRestart(to_version)
            }
            ApplyOutcome::AutoRolledBack {
                attempted_version,
                restored_version,
                reason,
            } => {
                self.phase = UpdatePhase::AutoRolledBack(AutoRolledBackNotice {
                    attempted_version,
                    restored_version,
                    reason,
                });
            }
            ApplyOutcome::Completed { .. } => self.phase = UpdatePhase::Idle,
        }
        false
    }

    pub fn acknowledge_rollback(&mut self) -> Result<(), UpdateError> {
        UpdateStateStore::new(self.paths.clone()?.state_dir).update(|state| {
            state.auto_rolled_back = None;
            Ok(())
        })?;
        self.phase = UpdatePhase::Idle;
        Ok(())
    }
}

pub fn hold_quit(applying: bool, restarting: bool) -> bool {
    applying && !restarting
}

#[derive(Clone, Debug)]
pub struct ErrorCard {
    pub code: Code,
    pub title: &'static str,
    pub message: String,
    pub can_retry: bool,
    pub reset_at: Option<u64>,
    pub copy_hint: Option<&'static str>,
}

pub fn display_error(raw: &str) -> String {
    sanitize_display_text(&redact_sensitive_text(raw), 1200)
}

pub fn card(error: &UpdateError) -> ErrorCard {
    error_card(error.code, error.message(), None)
}

fn error_card(code: Code, message: &str, reset_at: Option<u64>) -> ErrorCard {
    let (title, can_retry, rate_limit) = match code {
        Code::NetworkError => ("Network connection failed", true, false),
        Code::LockBusy => ("Another update is in progress", true, false),
        Code::RateLimited => ("GitHub rate limit", false, true),
        Code::UpdateDisabled => ("Updates are disabled", false, false),
        Code::InvalidRepo => ("Invalid release source", false, false),
        Code::DisallowedUrlOrHost => ("Address not allowed", false, false),
        Code::RedirectLimitExceeded => ("Redirect limit exceeded", false, false),
        Code::ResponseTooLarge => ("Response size limit exceeded", false, false),
        Code::AssetNotFound => ("Installer not found", false, false),
        Code::ManifestInvalid => ("Invalid release information", false, false),
        Code::ChecksumMismatch => ("File checksum mismatch", false, false),
        Code::SignatureInvalid => ("Release signature verification failed", false, false),
        Code::SignatureMissing => ("Release signature missing", false, false),
        Code::CodesignInvalid => ("App signature verification failed", false, false),
        Code::NotarizationInvalid => ("App notarization verification failed", false, false),
        Code::AppTranslocated => ("Run the app from its installed location", false, false),
        Code::InstallDirNotWritable => ("Install location is not writable", false, false),
        Code::InsufficientDiskSpace => ("Not enough disk space", false, false),
        Code::ArchiveUnsafe => ("Unsafe archive", false, false),
        Code::ConfirmationRequired => ("Confirmation required", false, false),
        Code::DowngradeDenied => ("Downgrade confirmation required", false, false),
        Code::SchemaIncompatibleDowngrade => (
            "Change blocked by an incompatible data schema",
            false,
            false,
        ),
        Code::HolderRestartRequired => ("Holder restart required", false, false),
        Code::FailedVersionBlocked => ("Automatic retry blocked for failed version", false, false),
        Code::RollbackUnavailable => ("No rollback backup available", false, false),
        Code::RollbackVerificationFailed => ("Rollback backup verification failed", false, false),
        Code::DaemonSwapFailedRolledBack => ("Update failed · Rolled back automatically", false, false),
        Code::DaemonSwapFailedRollbackFailed => ("Update and recovery failed", false, false),
        Code::ContainerUpdateUnsupported => ("Update the container image", false, false),
        Code::ProtocolIncompatible => ("Incompatible protocol version", false, false),
        Code::HostSecurityBlocked => ("Blocked by host security policy", false, false),
    };
    let message = display_error(message);
    ErrorCard {
        code,
        title,
        can_retry,
        message: if rate_limit {
            message.replace('=', "")
        } else {
            message
        },
        reset_at: reset_at.filter(|_| rate_limit),
        copy_hint: rate_limit.then_some("GITHUB_TOKEN=<token> ddg update check"),
    }
}

pub fn open_url(url: &str) -> Result<(), UpdateError> {
    validate_open_url(url)
}

pub fn container_update_command(image: &str, version: &str) -> String {
    let clean = |s: &str| {
        s.chars().filter(|c| !c.is_control() && !matches!(c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )).collect::<String>()
    };
    let reference = format!("{}:{}", clean(image), clean(version));
    format!("docker pull -- '{}'", reference.replace('\'', "'\\''"))
}

#[derive(Clone, Debug, Default)]
pub struct VersionInfo {
    pub app_version: Option<String>,
    pub platform: Option<String>,
    pub terminal_server_version: Option<String>,
    #[cfg(target_os = "macos")]
    pub holder_protocol_version: Option<u32>,
    #[cfg(target_os = "macos")]
    pub active_pty_count: Option<u32>,
    pub rollback_version: Option<String>,
}

#[derive(Default)]
pub struct RemoteMachineUpdateState {
    pub req_id: Option<u64>,
    pub phase: UpdatePhase,
    pub version: VersionInfo,
    pub dismissed_skew: Option<String>,
    pub last_change: Option<Change>,
}

#[derive(Clone, Debug)]
pub enum Change {
    LocalApply { tag: String, repo: Option<String> },
    ApplyRemote,
    Rollback,
}

pub enum UpdateResult {
    Check(Result<Option<CheckOutcome>, UpdateError>),
    Apply(Result<ApplyOutcome, UpdateError>),
}

pub(crate) fn storage(error: impl std::fmt::Display) -> UpdateError {
    UpdateError::new(Code::InstallDirNotWritable, error.to_string())
}

fn rollback_version(paths: &UpdatePaths) -> Option<String> {
    if cfg!(target_os = "macos")
        && std::env::current_exe()
            .ok()
            .as_deref()
            .and_then(enclosing_app)
            .is_some()
    {
        let raw = std::fs::read(paths.state_dir.join("app-rollback.json")).ok()?;
        #[derive(serde::Deserialize)]
        struct Backup {
            record: ddg_agents_update::rollback::RollbackRecord,
        }
        let record = serde_json::from_slice::<Backup>(&raw).ok()?.record;
        return Some(
            ReleaseTag::parse(&record.version)
                .ok()?
                .version()
                .to_string(),
        );
    }
    UpdateManager::new(paths.clone())
        .load_state()
        .ok()?
        .rollback
        .map(|r| r.version)
}

pub(crate) fn enclosing_app(path: &std::path::Path) -> Option<&std::path::Path> {
    path.ancestors().find(|p| {
        p.extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ddg_agents_update::config::UpdateChannel;

    #[test]
    fn update_controller_cards() {
        assert_eq!(Code::ALL.len(), 31);
        assert_eq!(
            error_card(Code::RateLimited, "", None).title,
            "GitHub rate limit"
        );
        for &code in Code::ALL {
            let card = error_card(code, "Bearer not-for-display", Some(123));
            assert!(!card.message.contains("not-for-display"));
            assert!(!card.title.is_empty());
            assert!(!card.message.is_empty());
            assert_eq!(
                card.can_retry,
                matches!(code, Code::NetworkError | Code::LockBusy)
            );
            assert_eq!(card.copy_hint.is_some(), code == Code::RateLimited);
            assert_eq!(card.reset_at.is_some(), code == Code::RateLimited);
        }
    }

    #[test]
    fn update_controller_banner_and_handoff() {
        let mut local = UpdateController {
            phase: UpdatePhase::UpdateAvailable(ReleaseSummary {
                version: "2.0.0".into(),
                tag: "v2.0.0".into(),
                repo: "ddg/ddg".into(),
                check_channel: UpdateChannel::Stable,
                notes: String::new(),
                web_url: String::new(),
            }),
            ..Default::default()
        };
        assert_eq!(local.banner(true, false), Some(Banner::Remote));
        assert_eq!(local.banner(true, true), Some(Banner::Available));
        local.dismiss_available();
        assert_eq!(local.banner(true, true), None);
        assert_eq!(
            UpdateController::default().dismissed_available_version,
            None
        );
        local.phase = UpdatePhase::Applying("2.0.0".into());
        assert_eq!(local.banner(true, false), Some(Banner::Local));
        assert!(local.finish_apply(ApplyOutcome::HandedOffToHelper {
            helper_pid: 42,
            to_version: "2.0.0".into()
        }));
        assert!(local.quitting_for_update_restart);
    }

    #[test]
    fn update_controller_quiet_states_and_opt_out() {
        let mut controller = UpdateController::default();
        controller.config.auto_check = AutoCheckPolicy::Off;
        assert!(!controller.can_check(true));
        controller.config.channel = UpdateChannel::Prerelease;
        assert!(!controller.can_check(true));
        assert!(controller.can_check(false));
        for phase in [
            UpdatePhase::Idle,
            UpdatePhase::Checking,
            UpdatePhase::UpToDate,
            UpdatePhase::Error(error_card(Code::NetworkError, "offline", None)),
        ] {
            controller.phase = phase;
            assert_eq!(controller.banner(false, false), None);
        }
        for phase in [
            UpdatePhase::PendingHolderRestart("v2.0.0".into()),
            UpdatePhase::AutoRolledBack(AutoRolledBackNotice {
                attempted_version: "2.0.0".into(),
                restored_version: "1.0.0".into(),
                reason: "failed".into(),
            }),
        ] {
            controller.phase = phase;
            assert_eq!(controller.banner(true, false), Some(Banner::Local));
        }
    }

    #[test]
    fn key_routing_quit_and_error_secrets() {
        for applying in [false, true] {
            for restarting in [false, true] {
                assert_eq!(hold_quit(applying, restarting), applying && !restarting);
            }
        }
        for input in [
            "GitHub_Token=secret",
            "gh_token=secret",
            "ghp_secret",
            "Authorization: secret",
            "Bearer secret",
            "https://user:secret@github.com",
        ] {
            let card = error_card(Code::RateLimited, input, None);
            assert!(!card.message.contains("secret"));
            assert!(!card.message.contains('='));
            assert_eq!(
                card.copy_hint,
                Some("GITHUB_TOKEN=<token> ddg update check")
            );
        }
        for url in [
            "file:///x",
            "smb://github.com/x",
            "https://evil.example/a/b/releases/tag/v1.2.3",
            "https://github.com/a/b/releases/tag/v1.2.3?x=1",
        ] {
            assert!(open_url(url).is_err());
        }
        assert!(open_url("https://github.com/a/b/releases/tag/v1.2.3").is_ok());
    }

    #[test]
    fn container_template_is_copy_only_and_single_line() {
        assert_eq!(
            container_update_command("image\n\r\u{202e}\u{2028}\u{2029}'x", "v1\t"),
            "docker pull -- 'image'\\''x:v1'"
        );
    }
}
