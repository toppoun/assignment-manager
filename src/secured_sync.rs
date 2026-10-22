//! Immutable Web client factory. A factory belongs to exactly one application
//! runtime; replace/drain that runtime when operator policy changes. Credential
//! revision remains independently owned by the application SettingsService.
use std::sync::Arc;

use crate::{
    moodle_http::{MoodleClient, MoodleHttpError},
    university_network::{NetworkError, NetworkPolicy, RunBudget},
    webclass_auth::HoseiAuthenticator,
};

/// No desktop fallback, default constructor, policy mutation, CA or DNS injection.
/// Cached clients share the run budget, reset without rebuilding their cookie jar.
pub struct SecuredSyncClients {
    policy: NetworkPolicy,
    run: Arc<RunBudget>,
}
impl SecuredSyncClients {
    pub fn new(policy: NetworkPolicy) -> Self {
        Self {
            policy,
            run: Arc::new(RunBudget::default()),
        }
    }
    /// Called once by SyncService before either source, including unconfigured runs.
    /// Caller must finish the preceding run before beginning another.
    pub fn begin_run(&mut self) {
        self.run.begin();
    }
    /// Check original secret syntax and exact host/path before any HTTP construction.
    pub fn validate_moodle_url(&self, secret: &str) -> Result<(), NetworkError> {
        self.policy.validate_moodle_url(secret)
    }
    pub fn moodle(&self) -> Result<MoodleClient, MoodleHttpError> {
        let mut client = MoodleClient::for_web(self.policy.clone())?;
        client.set_run_budget(Arc::clone(&self.run));
        Ok(client)
    }
    /// Reject sessions created under a different runtime/policy lifecycle.
    pub fn owns_session(&self, session: &crate::webclass_http::WebClassClient) -> bool {
        session.shares_run(&self.run)
    }
    /// The exact same factory is used for initial login and AuthExpired recovery.
    pub fn authenticator(&self) -> HoseiAuthenticator {
        let mut authenticator = HoseiAuthenticator::for_web(self.policy.clone());
        authenticator.set_run_budget(Arc::clone(&self.run));
        authenticator
    }
}
