//! Explicit offline-only test/example runtime. No HTTP clients or authenticators.
//! Credentials are checked against these public synthetic constants before use.
use crate::{Clock, PocState, WebError, auth::AuthClock, keys::KeyProvider, poc::PocDatabase};
use assignment_manager::{
    domain::SubmissionTarget,
    import::{AssignmentCandidate, AssignmentCandidateBatch, ExternalRecordKey, OriginKind},
    persistence::SqliteStore,
    sync::WebClassCredentialProvider,
};
use assignment_manager_application::{
    SyncBackend, SyncCounts, SyncFailure,
    credentials::{SecretText, WebClassCredentials},
};
use std::sync::{Arc, Mutex};

pub const MOODLE_URL: &str = "https://example.invalid/synthetic-calendar";
pub const UNIVERSITY_ID: &str = "synthetic-student";
pub const PASSWORD: &str = "synthetic-university-password";
#[derive(Clone, Copy, Default)]
pub enum Outcome {
    #[default]
    Success,
    Failure,
    PartialFailure,
    AuthExpiredOnce,
    AuthExpiredTwice,
}
#[derive(Clone, Default)]
pub struct Scenario {
    pub moodle: Outcome,
    pub webclass: Outcome,
    pub revision: u32,
}
#[derive(Default, Clone)]
pub struct Metrics {
    pub moodle_calls: usize,
    pub logins: usize,
    pub webclass_attempts: usize,
    pub reauths: usize,
}
/// Live controls are exclusively an in-process test seam, never an HTTP endpoint.
#[derive(Clone, Default)]
pub struct MockControl {
    pub scenario: Arc<Mutex<Scenario>>,
    pub metrics: Arc<Mutex<Metrics>>,
}
pub struct MockSyncBackend {
    control: MockControl,
}
impl MockSyncBackend {
    pub fn new(control: MockControl) -> Self {
        Self { control }
    }
}
fn import(
    store: &mut SqliteStore,
    origin: OriginKind,
    revision: u32,
) -> Result<SyncCounts, SyncFailure> {
    let batch = AssignmentCandidateBatch::try_new(vec![AssignmentCandidate {
        external_key: ExternalRecordKey {
            origin,
            scope: match origin {
                OriginKind::Moodle => "default.course-related",
                _ => "synthetic-group",
            }
            .into(),
            id: "synthetic-assignment-1".into(),
        },
        title: format!("Synthetic {origin:?} assignment v{revision}"),
        deadline: None,
        submission: match origin {
            OriginKind::Moodle => SubmissionTarget::Moodle,
            _ => SubmissionTarget::WebClass,
        },
        submission_url: None,
    }])
    .map_err(|_| SyncFailure::SourceDataInvalid)?;
    store
        .import_assignment_batch(&batch)
        .map(Into::into)
        .map_err(|_| SyncFailure::PersistenceFailed)
}
fn attempt(
    store: &mut SqliteStore,
    origin: OriginKind,
    outcome: Outcome,
    revision: u32,
) -> Result<SyncCounts, SyncFailure> {
    if matches!(outcome, Outcome::Failure) {
        return Err(SyncFailure::NetworkFailed);
    }
    let counts = import(store, origin, revision)?;
    if matches!(outcome, Outcome::PartialFailure) {
        return Err(SyncFailure::NetworkFailed);
    }
    Ok(counts)
}
impl SyncBackend for MockSyncBackend {
    type Session = usize;
    fn moodle(
        &mut self,
        url: SecretText,
        store: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure> {
        if url.expose() != MOODLE_URL {
            return Err(SyncFailure::InvalidConfiguration);
        }
        self.control.metrics.lock().unwrap().moodle_calls += 1;
        let scenario = self.control.scenario.lock().unwrap().clone();
        attempt(
            store,
            OriginKind::Moodle,
            scenario.moodle,
            scenario.revision,
        )
    }
    fn login(&mut self, credentials: WebClassCredentials) -> Result<usize, SyncFailure> {
        if credentials.university_id.expose() != UNIVERSITY_ID
            || credentials.password.expose() != PASSWORD
        {
            return Err(SyncFailure::InvalidConfiguration);
        }
        let mut metrics = self.control.metrics.lock().unwrap();
        metrics.logins += 1;
        Ok(metrics.logins)
    }
    fn webclass(
        &mut self,
        session: &mut usize,
        provider: &mut impl WebClassCredentialProvider,
        store: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure> {
        let scenario = self.control.scenario.lock().unwrap().clone();
        self.control.metrics.lock().unwrap().webclass_attempts += 1;
        if matches!(
            scenario.webclass,
            Outcome::AuthExpiredOnce | Outcome::AuthExpiredTwice
        ) {
            // Simulate a committed first group followed by AuthExpired. Reacquire
            // fresh credentials once, replace session, retry full import once.
            import(store, OriginKind::WebClass, scenario.revision)?;
            provider
                .with_credentials(|_| ())
                .ok_or(SyncFailure::CredentialUnavailable)?;
            let mut metrics = self.control.metrics.lock().unwrap();
            metrics.reauths += 1;
            metrics.webclass_attempts += 1;
            *session += 1;
            drop(metrics);
            if matches!(scenario.webclass, Outcome::AuthExpiredTwice) {
                return Err(SyncFailure::AuthenticationFailed);
            }
        }
        attempt(
            store,
            OriginKind::WebClass,
            scenario.webclass,
            scenario.revision,
        )
    }
}
/// Caller must prepare a dedicated synthetic DB and explicitly initialized vault.
/// The normal `PocState::open*` / binary always uses DisabledNetworkBackend.
pub async fn open(
    database: &PocDatabase,
    clock: impl Clock,
    auth_clock: impl AuthClock,
    provider: Arc<dyn KeyProvider>,
    backend: impl SyncBackend + Send + 'static,
) -> Result<PocState, WebError> {
    PocState::open_with_backend(database, clock, auth_clock, provider, backend).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backend_never_connects_and_rejects_credentials_outside_its_synthetic_constants() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut backend = MockSyncBackend::new(Default::default());
        let mut store = SqliteStore::open(":memory:").unwrap();
        assert_eq!(
            backend.moodle(
                SecretText::new(format!(
                    "https://{}/synthetic",
                    listener.local_addr().unwrap()
                )),
                &mut store
            ),
            Err(SyncFailure::InvalidConfiguration)
        );
        assert_eq!(
            backend.login(WebClassCredentials {
                university_id: SecretText::new("other-synthetic".into()),
                password: SecretText::new(PASSWORD.into())
            }),
            Err(SyncFailure::InvalidConfiguration)
        );
        assert_eq!(
            backend
                .moodle(SecretText::new(MOODLE_URL.into()), &mut store)
                .unwrap()
                .created,
            1
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
