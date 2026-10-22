//! Shared orchestration. Fetch, parsing, discovery, import and reauth belong to core.
#[cfg(test)]
#[path = "sync_tests.rs"]
pub(crate) mod tests;
use assignment_manager::{
    moodle_http::{MoodleClient, MoodleExportKind, MoodleFeed},
    persistence::{AssignmentImportSummary, SqliteStore},
    secured_sync::SecuredSyncClients,
    sync::{
        self, MoodleSyncError, WebClassCredentialProvider, WebClassReauthSyncError,
        WebClassSyncError,
    },
    university_network::NetworkPolicy,
    webclass_auth::{HoseiAuthError, HoseiAuthenticator, HoseiCredentials},
    webclass_http::WebClassClient,
};

use crate::{
    credentials::{SecretText, WebClassCredentials},
    error::ApplicationError,
    settings::MOODLE_SCOPE,
    settings_worker::SettingsWorker,
};

/// Only fixed classifications cross IPC; never format or retain a core/backend cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncFailure {
    NetworkFailed,
    AuthenticationFailed,
    InvalidConfiguration,
    SourceDataInvalid,
    PersistenceFailed,
    CredentialUnavailable,
    Unexpected,
}

impl SyncFailure {
    pub fn code(self) -> &'static str {
        match self {
            Self::NetworkFailed => "network_failed",
            Self::AuthenticationFailed => "authentication_failed",
            Self::InvalidConfiguration => "invalid_configuration",
            Self::SourceDataInvalid => "source_data_invalid",
            Self::PersistenceFailed => "persistence_failed",
            Self::CredentialUnavailable => "credential_unavailable",
            Self::Unexpected => "unexpected",
        }
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
/// `Failed` may follow committed groups: it never means DB unchanged.
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

#[derive(Debug, Clone, PartialEq, Eq)]
/// `Completed` means the run ended, not that all sources succeeded.
pub enum SyncReport {
    Completed {
        moodle: SourceSyncResult,
        webclass: SourceSyncResult,
    },
    AlreadyRunning,
}

pub(crate) trait SyncSecrets {
    fn moodle(&self) -> Result<Option<SecretText>, ApplicationError>;
    fn webclass(&self) -> Result<(u64, Option<WebClassCredentials>), ApplicationError>;
}
impl SyncSecrets for SettingsWorker {
    fn moodle(&self) -> Result<Option<SecretText>, ApplicationError> {
        self.load_moodle_blocking()
    }
    fn webclass(&self) -> Result<(u64, Option<WebClassCredentials>), ApplicationError> {
        self.load_webclass_blocking()
    }
}

/// Trusted backend port. Synthetic substitutions remain offline test adapters.
pub trait SyncBackend {
    type Session;
    /// Run-wide resource accounting; desktop and synthetic backends need no hook.
    fn begin_run(&mut self) {}
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

/// Implementation shared by the desktop and secured factories. Fields are private.
pub struct CoreSyncBackend<F> {
    clients: F,
}
pub(crate) struct DesktopClients;
trait ClientFactory {
    fn validate_session(&self, _session: &WebClassClient) -> Result<(), SyncFailure> {
        Ok(())
    }
    fn begin_run(&mut self) {}
    fn validate_moodle(&self, _url: &str) -> Result<(), SyncFailure> {
        Ok(())
    }
    fn moodle(&self) -> Result<MoodleClient, SyncFailure>;
    fn authenticator(&self) -> HoseiAuthenticator;
}
impl ClientFactory for DesktopClients {
    fn moodle(&self) -> Result<MoodleClient, SyncFailure> {
        MoodleClient::new().map_err(|_| SyncFailure::Unexpected)
    }
    fn authenticator(&self) -> HoseiAuthenticator {
        HoseiAuthenticator::new()
    }
}
impl ClientFactory for SecuredSyncClients {
    fn validate_session(&self, session: &WebClassClient) -> Result<(), SyncFailure> {
        if self.owns_session(session) {
            Ok(())
        } else {
            Err(SyncFailure::InvalidConfiguration)
        }
    }
    fn begin_run(&mut self) {
        SecuredSyncClients::begin_run(self);
    }
    fn validate_moodle(&self, url: &str) -> Result<(), SyncFailure> {
        self.validate_moodle_url(url)
            .map_err(|_| SyncFailure::InvalidConfiguration)
    }
    fn moodle(&self) -> Result<MoodleClient, SyncFailure> {
        SecuredSyncClients::moodle(self).map_err(|_| SyncFailure::Unexpected)
    }
    fn authenticator(&self) -> HoseiAuthenticator {
        SecuredSyncClients::authenticator(self)
    }
}
impl CoreSyncBackend<DesktopClients> {
    pub(crate) fn desktop() -> Self {
        Self {
            clients: DesktopClients,
        }
    }
}
/// Web-only backend, explicitly injected by trusted Rust code. No policy means
/// no construction; default Moodle policy rejects all feeds. Normal Web binary
/// continues to use DisabledNetworkBackend. Policy changes require a new runtime.
///
/// A trusted policy is mandatory:
/// ```compile_fail
/// use assignment_manager_application::SecuredCoreSyncBackend;
/// let backend = SecuredCoreSyncBackend::new();
/// ```
pub type SecuredCoreSyncBackend = CoreSyncBackend<SecuredSyncClients>;
impl SecuredCoreSyncBackend {
    pub fn new(policy: NetworkPolicy) -> Self {
        Self {
            clients: SecuredSyncClients::new(policy),
        }
    }
}
impl<F: ClientFactory> SyncBackend for CoreSyncBackend<F> {
    type Session = WebClassClient;
    fn begin_run(&mut self) {
        self.clients.begin_run();
    }
    fn moodle(
        &mut self,
        url: SecretText,
        store: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure> {
        self.clients.validate_moodle(url.expose())?;
        let feed = MoodleFeed::new(MOODLE_SCOPE, MoodleExportKind::CourseRelated, url.expose())
            .map_err(|_| SyncFailure::InvalidConfiguration)?;
        drop(url);
        let client = self.clients.moodle()?;
        sync::sync_moodle_once(&client, &feed, store)
            .map(Into::into)
            .map_err(Into::into)
    }
    fn login(&mut self, credentials: WebClassCredentials) -> Result<WebClassClient, SyncFailure> {
        self.clients
            .authenticator()
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
        self.clients.validate_session(session)?;
        // Core owns AuthExpired -> credential callback -> login -> retry once.
        sync::sync_webclass_all_with_reauth(
            session,
            &self.clients.authenticator(),
            credentials,
            store,
        )
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

pub(crate) struct SyncService<B: SyncBackend, C> {
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
    pub fn run(&mut self) -> SyncReport {
        self.backend.begin_run();
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
        SyncReport::Completed { moodle, webclass }
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

impl SyncReport {
    /// Reload even after source failure, because earlier groups may be committed.
    pub fn should_reload(&self) -> bool {
        matches!(self, Self::Completed { moodle, webclass } if !matches!(moodle, SourceSyncResult::NotConfigured) || !matches!(webclass, SourceSyncResult::NotConfigured))
    }
}
