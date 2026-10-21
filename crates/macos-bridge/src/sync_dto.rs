use assignment_manager_application::{
    SourceSyncResult, SyncCounts, SyncFailure, SyncReport, SyncStatus,
};

/// Fixed classifications only: no backend cause, identity, URL, or session crosses FFI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SyncFailureDto {
    NetworkFailed,
    AuthenticationFailed,
    InvalidConfiguration,
    SourceDataInvalid,
    PersistenceFailed,
    CredentialUnavailable,
    Unexpected,
}
impl From<SyncFailure> for SyncFailureDto {
    fn from(value: SyncFailure) -> Self {
        match value {
            SyncFailure::NetworkFailed => Self::NetworkFailed,
            SyncFailure::AuthenticationFailed => Self::AuthenticationFailed,
            SyncFailure::InvalidConfiguration => Self::InvalidConfiguration,
            SyncFailure::SourceDataInvalid => Self::SourceDataInvalid,
            SyncFailure::PersistenceFailed => Self::PersistenceFailed,
            SyncFailure::CredentialUnavailable => Self::CredentialUnavailable,
            SyncFailure::Unexpected => Self::Unexpected,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct SyncCountsDto {
    pub created: u64,
    pub updated: u64,
    pub unchanged: u64,
}
impl From<SyncCounts> for SyncCountsDto {
    fn from(value: SyncCounts) -> Self {
        Self {
            created: value.created as u64,
            updated: value.updated as u64,
            unchanged: value.unchanged as u64,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum SourceSyncResultDto {
    NotConfigured,
    Success { counts: SyncCountsDto },
    Failed { error: SyncFailureDto },
}
impl From<SourceSyncResult> for SourceSyncResultDto {
    fn from(value: SourceSyncResult) -> Self {
        match value {
            SourceSyncResult::NotConfigured => Self::NotConfigured,
            SourceSyncResult::Success { counts } => Self::Success {
                counts: counts.into(),
            },
            SourceSyncResult::Failed { error } => Self::Failed {
                error: error.into(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum SyncReportDto {
    Completed {
        moodle: SourceSyncResultDto,
        webclass: SourceSyncResultDto,
        should_reload: bool,
    },
    AlreadyRunning,
}
impl From<SyncReport> for SyncReportDto {
    fn from(value: SyncReport) -> Self {
        // Failed can include committed rows. The reload policy belongs to application.
        let should_reload = value.should_reload();
        match value {
            SyncReport::Completed { moodle, webclass } => Self::Completed {
                moodle: moodle.into(),
                webclass: webclass.into(),
                should_reload,
            },
            SyncReport::AlreadyRunning => Self::AlreadyRunning,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SyncStatusDto {
    pub running: bool,
    pub last_report: Option<SyncReportDto>,
}
impl From<SyncStatus> for SyncStatusDto {
    fn from(value: SyncStatus) -> Self {
        Self {
            running: value.running,
            last_report: value.last_report.map(Into::into),
        }
    }
}
