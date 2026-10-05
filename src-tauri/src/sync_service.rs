//! Tauri JSON sync result adapter. Orchestration and policy are shared Rust.
use assignment_manager_application::{SourceSyncResult, SyncCounts, SyncReport};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct SyncCountsDto {
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
}
impl From<SyncCounts> for SyncCountsDto {
    fn from(value: SyncCounts) -> Self {
        Self {
            created: value.created,
            updated: value.updated,
            unchanged: value.unchanged,
        }
    }
}
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SourceSyncResultDto {
    NotConfigured,
    Success { counts: SyncCountsDto },
    Failed { error: &'static str },
}
impl From<SourceSyncResult> for SourceSyncResultDto {
    fn from(value: SourceSyncResult) -> Self {
        match value {
            SourceSyncResult::NotConfigured => Self::NotConfigured,
            SourceSyncResult::Success { counts } => Self::Success {
                counts: counts.into(),
            },
            SourceSyncResult::Failed { error } => Self::Failed {
                error: error.code(),
            },
        }
    }
}
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SyncResultDto {
    Completed {
        moodle: SourceSyncResultDto,
        webclass: SourceSyncResultDto,
    },
    AlreadyRunning,
}
impl From<SyncReport> for SyncResultDto {
    fn from(value: SyncReport) -> Self {
        match value {
            SyncReport::Completed { moodle, webclass } => Self::Completed {
                moodle: moodle.into(),
                webclass: webclass.into(),
            },
            SyncReport::AlreadyRunning => Self::AlreadyRunning,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use assignment_manager_application::SyncFailure;
    #[test]
    fn sync_wire_contract_stays_tagged_and_only_contains_safe_codes_and_counts() {
        let result = SyncReport::Completed {
            moodle: SourceSyncResult::Success {
                counts: SyncCounts {
                    created: 1,
                    updated: 2,
                    unchanged: 3,
                },
            },
            webclass: SourceSyncResult::Failed {
                error: SyncFailure::AuthenticationFailed,
            },
        };
        assert!(result.should_reload());
        assert_eq!(
            serde_json::to_value(SyncResultDto::from(result)).unwrap(),
            serde_json::json!({
                "status":"completed", "moodle":{"status":"success", "counts":{"created":1,"updated":2,"unchanged":3}},
                "webclass":{"status":"failed","error":"authentication_failed"}
            })
        );
        assert_eq!(
            serde_json::to_value(SyncResultDto::from(SyncReport::AlreadyRunning)).unwrap(),
            serde_json::json!({"status":"already_running"})
        );
        assert_eq!(
            serde_json::to_value(SourceSyncResultDto::from(SourceSyncResult::NotConfigured))
                .unwrap(),
            serde_json::json!({"status":"not_configured"})
        );
    }
}
