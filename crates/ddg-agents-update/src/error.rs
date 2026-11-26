use serde::{Deserialize, Serialize};

macro_rules! update_codes {
    ($($code:ident),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum UpdateErrorCode { $($code),+ }

        impl UpdateErrorCode {
            pub const ALL: &'static [UpdateErrorCode] = &[$(Self::$code),+];
        }
    };
}

update_codes!(
    UpdateDisabled,
    InvalidRepo,
    DisallowedUrlOrHost,
    RedirectLimitExceeded,
    NetworkError,
    RateLimited,
    ResponseTooLarge,
    AssetNotFound,
    ManifestInvalid,
    ChecksumMismatch,
    SignatureInvalid,
    SignatureMissing,
    CodesignInvalid,
    NotarizationInvalid,
    AppTranslocated,
    InstallDirNotWritable,
    InsufficientDiskSpace,
    ArchiveUnsafe,
    LockBusy,
    ConfirmationRequired,
    DowngradeDenied,
    SchemaIncompatibleDowngrade,
    HolderRestartRequired,
    FailedVersionBlocked,
    RollbackUnavailable,
    RollbackVerificationFailed,
    DaemonSwapFailedRolledBack,
    DaemonSwapFailedRollbackFailed,
    ContainerUpdateUnsupported,
    ProtocolIncompatible,
    HostSecurityBlocked,
);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ApplyOutcome {
    Completed {
        from_version: String,
        to_version: String,
        tokens_preserved: bool,
    },
    HandedOffToHelper {
        helper_pid: u32,
        to_version: String,
    },
    PendingHolderRestart {
        to_version: String,
        active_pty_count: u32,
    },
    AutoRolledBack {
        attempted_version: String,
        restored_version: String,
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UpdateError {
    pub code: UpdateErrorCode,
    message: String,
    #[cfg(feature = "network")]
    #[serde(skip)]
    pub(crate) rate_limit_reset_at: Option<u64>,
}

impl UpdateError {
    pub fn new(code: UpdateErrorCode, message: impl AsRef<str>) -> Self {
        Self {
            code,
            message: crate::redact::redact_sensitive_text(message.as_ref()),
            #[cfg(feature = "network")]
            rate_limit_reset_at: None,
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for UpdateError {}
