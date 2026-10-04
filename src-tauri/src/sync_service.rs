//! Desktop orchestration only. Fetch, parsing, discovery, import and reauth belong to core.
#[cfg(test)]
#[path = "sync_tests.rs"]
pub(crate) mod tests;
use assignment_manager::{
    moodle_http::{MoodleClient, MoodleExportKind, MoodleFeed},
    persistence::{AssignmentImportSummary, SqliteStore},
    sync::{
        self, MoodleSyncError, WebClassCredentialProvider, WebClassReauthSyncError,
        WebClassSyncError,
    },
    webclass_auth::{HoseiAuthError, HoseiAuthenticator, HoseiCredentials},
    webclass_http::WebClassClient,
};
use serde::Serialize;

use crate::{
    credential_store::{SecretText, WebClassCredentials},
    error::ApplicationError,
    settings::MOODLE_SCOPE,
    settings_worker::SettingsState,
};

/// Only fixed classifications cross IPC; never format or retain a core/backend cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncFailure {
    NetworkFailed,
    AuthenticationFailed,
    InvalidConfiguration,
    SourceDataInvalid,
    PersistenceFailed,
    CredentialUnavailable,
    Unexpected,
}

impl From<MoodleSyncError> for SyncFailure {
    fn from(error: MoodleSyncError) -> Self {
        match error {
            MoodleSyncError::Fetch => Self::NetworkFailed,
            MoodleSyncError::Parse | MoodleSyncError::Validation => Self::SourceDataInvalid,
            MoodleSyncError::Persistence => Self::PersistenceFailed,
        }
    }
}
impl From<WebClassSyncError> for SyncFailure {
    fn from(error: WebClassSyncError) -> Self {
        match error {
            WebClassSyncError::AuthExpired => Self::AuthenticationFailed,
            WebClassSyncError::Fetch => Self::NetworkFailed,
            WebClassSyncError::Parse
            | WebClassSyncError::CourseValidation
            | WebClassSyncError::Map
            | WebClassSyncError::Validation => Self::SourceDataInvalid,
            WebClassSyncError::CoursePersistence | WebClassSyncError::Persistence => {
                Self::PersistenceFailed
            }
        }
    }
}
impl From<WebClassReauthSyncError> for SyncFailure {
    fn from(error: WebClassReauthSyncError) -> Self {
        match error {
            WebClassReauthSyncError::Sync(error) => error.into(),
            WebClassReauthSyncError::CredentialsUnavailable => Self::CredentialUnavailable,
            WebClassReauthSyncError::AuthenticationFailed => Self::AuthenticationFailed,
        }
    }
}
impl From<HoseiAuthError> for SyncFailure {
    fn from(error: HoseiAuthError) -> Self {
        match error {
            HoseiAuthError::Timeout(_) | HoseiAuthError::Request(_) => Self::NetworkFailed,
            HoseiAuthError::ClientInitialization => Self::Unexpected,
            _ => Self::AuthenticationFailed,
        }
    }
}
impl From<ApplicationError> for SyncFailure {
    fn from(error: ApplicationError) -> Self {
        match error {
            ApplicationError::CredentialUnavailable | ApplicationError::CredentialReadFailed => {
                Self::CredentialUnavailable
            }
            ApplicationError::SettingsReadFailed
            | ApplicationError::InvalidMoodleFeed
            | ApplicationError::InvalidCredentials => Self::InvalidConfiguration,
            ApplicationError::DataDirectory
            | ApplicationError::DatabaseStartup
            | ApplicationError::Storage => Self::PersistenceFailed,
            _ => Self::Unexpected,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SyncCounts {
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
}
impl From<AssignmentImportSummary> for SyncCounts {
    fn from(summary: AssignmentImportSummary) -> Self {
        Self {
            created: summary.created,
            updated: summary.updated,
            unchanged: summary.unchanged,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SourceSyncResult {
    NotConfigured,
    Success { counts: SyncCounts },
    Failed { error: SyncFailure },
}
impl SourceSyncResult {
    fn from_result(result: Result<SyncCounts, SyncFailure>) -> Self {
        match result {
            Ok(counts) => Self::Success { counts },
            Err(error) => Self::Failed { error },
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SyncResultDto {
    Completed {
        moodle: SourceSyncResult,
        webclass: SourceSyncResult,
    },
    AlreadyRunning,
}

pub trait SyncSecrets {
    fn moodle(&self) -> Result<Option<SecretText>, ApplicationError>;
    fn webclass(&self) -> Result<(u64, Option<WebClassCredentials>), ApplicationError>;
}
impl SyncSecrets for SettingsState {
    fn moodle(&self) -> Result<Option<SecretText>, ApplicationError> {
        self.load_moodle_blocking()
    }
    fn webclass(&self) -> Result<(u64, Option<WebClassCredentials>), ApplicationError> {
        self.load_webclass_blocking()
    }
}

/// Test seam replaces transport, never production credentials or core behavior.
pub trait SyncBackend {
    type Session;
    fn moodle(
        &mut self,
        url: SecretText,
        store: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure>;
    fn login(&mut self, credentials: WebClassCredentials) -> Result<Self::Session, SyncFailure>;
    fn webclass(
        &mut self,
        session: &mut Self::Session,
        credentials: &mut impl WebClassCredentialProvider,
        store: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure>;
}

pub struct CoreSyncBackend;
impl SyncBackend for CoreSyncBackend {
    type Session = WebClassClient;
    fn moodle(
        &mut self,
        url: SecretText,
        store: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure> {
        let feed = MoodleFeed::new(MOODLE_SCOPE, MoodleExportKind::CourseRelated, url.expose())
            .map_err(|_| SyncFailure::InvalidConfiguration)?;
        drop(url);
        let client = MoodleClient::new().map_err(|_| SyncFailure::Unexpected)?;
        sync::sync_moodle_once(&client, &feed, store)
            .map(Into::into)
            .map_err(Into::into)
    }
    fn login(&mut self, credentials: WebClassCredentials) -> Result<WebClassClient, SyncFailure> {
        HoseiAuthenticator::new()
            .login(HoseiCredentials::new(
                credentials.university_id.expose(),
                credentials.password.expose(),
            ))
            .map_err(Into::into)
    }
    fn webclass(
        &mut self,
        session: &mut WebClassClient,
        credentials: &mut impl WebClassCredentialProvider,
        store: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure> {
        // Core owns AuthExpired -> credential callback -> login -> retry once.
        sync::sync_webclass_all_with_reauth(session, &HoseiAuthenticator::new(), credentials, store)
            .map(|summary| SyncCounts {
                created: summary.assignments_created,
                updated: summary.assignments_updated,
                unchanged: summary.assignments_unchanged,
            })
            .map_err(Into::into)
    }
}

struct CredentialProvider<'a, C>(&'a C);
impl<C: SyncSecrets> WebClassCredentialProvider for CredentialProvider<'_, C> {
    fn with_credentials<R>(
        &mut self,
        use_credentials: impl FnOnce(HoseiCredentials<'_>) -> R,
    ) -> Option<R> {
        let (_, credentials) = self.0.webclass().ok()?;
        let credentials = credentials?;
        Some(use_credentials(HoseiCredentials::new(
            credentials.university_id.expose(),
            credentials.password.expose(),
        )))
        // Owned zeroizing buffers drop before the callback result leaves this method.
    }
}

pub struct SyncService<B: SyncBackend, C> {
    backend: B,
    credentials: C,
    store: Result<SqliteStore, ApplicationError>,
    session: Option<(u64, B::Session)>,
}
impl<B: SyncBackend, C: SyncSecrets> SyncService<B, C> {
    pub fn new(backend: B, credentials: C, store: Result<SqliteStore, ApplicationError>) -> Self {
        Self {
            backend,
            credentials,
            store,
            session: None,
        }
    }
    pub fn run(&mut self) -> SyncResultDto {
        // Secrets are loaded per source just before use, not retained across runs.
        let moodle = match self.credentials.moodle() {
            Ok(None) => SourceSyncResult::NotConfigured,
            Err(error) => SourceSyncResult::Failed {
                error: error.into(),
            },
            Ok(Some(url)) => SourceSyncResult::from_result(
                self.store
                    .as_mut()
                    .map_err(|error| (*error).into())
                    .and_then(|store| self.backend.moodle(url, store)),
            ),
        };
        // Always attempt the second source, even after the first source's failure.
        let webclass = match self.credentials.webclass() {
            Ok((_, None)) => {
                self.session = None;
                SourceSyncResult::NotConfigured
            }
            Err(error) => {
                self.session = None;
                SourceSyncResult::Failed {
                    error: error.into(),
                }
            }
            Ok((revision, Some(credentials))) => {
                SourceSyncResult::from_result(self.run_webclass(revision, credentials))
            }
        };
        SyncResultDto::Completed { moodle, webclass }
    }
    fn run_webclass(
        &mut self,
        revision: u64,
        credentials: WebClassCredentials,
    ) -> Result<SyncCounts, SyncFailure> {
        let store = self
            .store
            .as_mut()
            .map_err(|error| SyncFailure::from(*error))?;
        if self.session.as_ref().map(|(current, _)| *current) != Some(revision) {
            // Drop old cookies before attempting login with a replacement credential.
            self.session = None;
            self.session = Some((revision, self.backend.login(credentials)?));
        } else {
            drop(credentials);
        }
        let (_, session) = self.session.as_mut().ok_or(SyncFailure::Unexpected)?;
        self.backend
            .webclass(session, &mut CredentialProvider(&self.credentials), store)
    }
}
