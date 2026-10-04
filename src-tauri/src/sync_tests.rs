use super::*;
use crate::{
    credential_store::tests::{FakeBackend, ID, PASSWORD, URL, credentials, fake},
    credential_store::{CredentialStore, CredentialVault},
    dto::CompletionDto,
    list_query::{CompletionSelection, ListRequest, QueryClock, list_from_store},
    settings::{NonSecretSettings, SettingsService},
};
use assignment_manager::{
    domain::SubmissionTarget,
    import::{AssignmentCandidate, AssignmentCandidateBatch, ExternalRecordKey, OriginKind},
};

impl<C: CredentialStore> SyncSecrets for SettingsService<C> {
    fn moodle(&self) -> Result<Option<SecretText>, ApplicationError> {
        self.load_moodle()
    }
    fn webclass(&self) -> Result<(u64, Option<WebClassCredentials>), ApplicationError> {
        self.load_webclass()
    }
}

pub(crate) fn secrets(
    moodle: bool,
    webclass: bool,
) -> SettingsService<CredentialVault<FakeBackend>> {
    let mut service = SettingsService::new(NonSecretSettings::default(), fake());
    if moodle {
        service.save_moodle(SecretText::new(URL.into())).unwrap();
    }
    if webclass {
        service.save_webclass(credentials(ID, PASSWORD)).unwrap();
    }
    service
}
pub(crate) fn import(store: &mut SqliteStore) -> Result<SyncCounts, SyncFailure> {
    let batch = AssignmentCandidateBatch::try_new(vec![AssignmentCandidate {
        external_key: ExternalRecordKey {
            origin: OriginKind::Moodle,
            scope: MOODLE_SCOPE.into(),
            id: "synthetic-assignment".into(),
        },
        title: "同期した課題".into(),
        deadline: None,
        submission: SubmissionTarget::Unknown,
        submission_url: None,
    }])
    .unwrap();
    store
        .import_assignment_batch(&batch)
        .map(Into::into)
        .map_err(|_| SyncFailure::PersistenceFailed)
}

#[derive(Default)]
struct FakeNetwork {
    moodle_calls: usize,
    login_calls: usize,
    webclass_calls: usize,
    reauth_calls: usize,
    moodle_failure: Option<SyncFailure>,
    login_failure: Option<SyncFailure>,
    webclass_failure: Option<SyncFailure>,
    expired: bool,
    reauth_failure: bool,
}
impl SyncBackend for FakeNetwork {
    type Session = usize;
    fn moodle(
        &mut self,
        _url: SecretText,
        store: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure> {
        self.moodle_calls += 1;
        match self.moodle_failure {
            Some(error) => Err(error),
            None => import(store),
        }
    }
    fn login(&mut self, _credentials: WebClassCredentials) -> Result<usize, SyncFailure> {
        self.login_calls += 1;
        match self.login_failure {
            Some(error) => Err(error),
            None => Ok(self.login_calls),
        }
    }
    fn webclass(
        &mut self,
        session: &mut usize,
        provider: &mut impl WebClassCredentialProvider,
        store: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure> {
        self.webclass_calls += 1;
        // Core HTTP mocks separately verify the protocol/retry limit behind this callback.
        if self.expired {
            provider
                .with_credentials(|_| {
                    self.reauth_calls += 1;
                })
                .ok_or(SyncFailure::CredentialUnavailable)?;
            if self.reauth_failure {
                return Err(SyncFailure::AuthenticationFailed);
            }
            self.expired = false;
            *session += 1;
        }
        match self.webclass_failure {
            Some(error) => Err(error),
            None => import(store),
        }
    }
}
fn service(
    moodle: bool,
    webclass: bool,
) -> SyncService<FakeNetwork, SettingsService<CredentialVault<FakeBackend>>> {
    SyncService::new(
        FakeNetwork::default(),
        secrets(moodle, webclass),
        Ok(SqliteStore::open(":memory:").unwrap()),
    )
}
fn report(result: SyncResultDto) -> (SourceSyncResult, SourceSyncResult) {
    match result {
        SyncResultDto::Completed { moodle, webclass } => (moodle, webclass),
        _ => panic!("not completed"),
    }
}

#[test]
fn configured_source_selection_never_fetches_unconfigured_sources() {
    for (moodle, webclass) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut service = service(moodle, webclass);
        let (m, w) = report(service.run());
        assert_eq!(service.backend.moodle_calls, usize::from(moodle));
        assert_eq!(service.backend.login_calls, usize::from(webclass));
        assert_eq!(service.backend.webclass_calls, usize::from(webclass));
        assert_eq!(matches!(m, SourceSyncResult::NotConfigured), !moodle);
        assert_eq!(matches!(w, SourceSyncResult::NotConfigured), !webclass);
    }
}
#[test]
fn source_failures_are_independent_and_committed_data_stays_visible() {
    let mut service = service(true, true);
    service.backend.moodle_failure = Some(SyncFailure::NetworkFailed);
    let (moodle, webclass) = report(service.run());
    assert_eq!(
        moodle,
        SourceSyncResult::Failed {
            error: SyncFailure::NetworkFailed
        }
    );
    assert!(matches!(webclass, SourceSyncResult::Success { .. }));
    service.backend.moodle_failure = None;
    service.backend.webclass_failure = Some(SyncFailure::SourceDataInvalid);
    let (moodle, webclass) = report(service.run());
    assert!(matches!(moodle, SourceSyncResult::Success { .. }));
    assert_eq!(
        webclass,
        SourceSyncResult::Failed {
            error: SyncFailure::SourceDataInvalid
        }
    );
    let clock = QueryClock::from_local(chrono::Local::now()).unwrap();
    assert_eq!(
        list_from_store(
            service.store.as_ref().unwrap(),
            &ListRequest::default(),
            &clock
        )
        .unwrap()
        .items
        .len(),
        1
    );
}
#[test]
fn initial_login_session_reuse_expired_reauth_and_failed_reauth() {
    let mut service = service(false, true);
    service.run();
    service.run();
    assert_eq!(service.backend.login_calls, 1);
    assert_eq!(service.backend.webclass_calls, 2);
    service.backend.expired = true;
    let (_, webclass) = report(service.run());
    assert!(matches!(webclass, SourceSyncResult::Success { .. }));
    assert_eq!(service.backend.reauth_calls, 1);
    service.backend.expired = true;
    service.backend.reauth_failure = true;
    let (_, webclass) = report(service.run());
    assert_eq!(
        webclass,
        SourceSyncResult::Failed {
            error: SyncFailure::AuthenticationFailed
        }
    );
    assert_eq!(service.backend.reauth_calls, 2);
    assert_eq!(service.backend.login_calls, 1);
}
#[test]
fn replacement_and_removal_invalidate_session_and_never_cache_moodle_url() {
    let mut service = service(true, true);
    service.run();
    service
        .credentials
        .save_webclass(credentials("replacement-id", "replacement-password"))
        .unwrap();
    service.run();
    assert_eq!(service.backend.login_calls, 2);
    service
        .credentials
        .save_webclass(credentials("replacement-id-2", "password-2"))
        .unwrap();
    service.backend.login_failure = Some(SyncFailure::AuthenticationFailed);
    let (_, webclass) = report(service.run());
    assert_eq!(
        webclass,
        SourceSyncResult::Failed {
            error: SyncFailure::AuthenticationFailed
        }
    );
    assert!(service.session.is_none());
    assert_eq!(service.backend.webclass_calls, 2);
    service.credentials.remove_webclass().unwrap();
    service.credentials.remove_moodle().unwrap();
    assert_eq!(
        report(service.run()),
        (
            SourceSyncResult::NotConfigured,
            SourceSyncResult::NotConfigured
        )
    );
    assert!(service.session.is_none());
    assert_eq!(service.backend.moodle_calls, 3);
}
#[test]
fn stable_url_rotation_keeps_identity_and_local_completion_on_reimport_and_reopen() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("assignment-manager.db");
    let mut service = SyncService::new(
        FakeNetwork::default(),
        secrets(true, false),
        Ok(SqliteStore::open(&path).unwrap()),
    );
    service.run();
    let clock = QueryClock::from_local(chrono::Local::now()).unwrap();
    let item = list_from_store(
        service.store.as_ref().unwrap(),
        &ListRequest::default(),
        &clock,
    )
    .unwrap()
    .items
    .remove(0);
    assignment_manager::actions::toggle_assignment_completion(
        service.store.as_mut().unwrap(),
        &assignment_manager::domain::AssignmentId::new(item.id.clone()),
    )
    .unwrap();
    service
        .credentials
        .save_moodle(SecretText::new(
            "https://example.invalid/?token=ROTATED".into(),
        ))
        .unwrap();
    let (moodle, _) = report(service.run());
    assert!(matches!(
        moodle,
        SourceSyncResult::Success {
            counts: SyncCounts {
                created: 0,
                unchanged: 1,
                ..
            }
        }
    ));
    let safe = format!("{moodle:?} {}", serde_json::to_string(&moodle).unwrap());
    for marker in [
        URL,
        ID,
        PASSWORD,
        MOODLE_SCOPE,
        "ROTATED",
        "synthetic-assignment",
    ] {
        assert!(!safe.contains(marker));
    }
    drop(service);
    let store = SqliteStore::open(&path).unwrap();
    let all = ListRequest {
        completion: CompletionSelection::All,
        ..Default::default()
    };
    let items = list_from_store(&store, &all, &clock).unwrap().items;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, item.id);
    assert_eq!(items[0].completion, CompletionDto::Completed);
}
#[test]
fn safe_core_error_mapping_covers_fetch_data_persistence_and_auth() {
    use WebClassSyncError::*;
    for error in [Parse, CourseValidation, Map, Validation] {
        assert_eq!(SyncFailure::from(error), SyncFailure::SourceDataInvalid);
    }
    for error in [Persistence, CoursePersistence] {
        assert_eq!(SyncFailure::from(error), SyncFailure::PersistenceFailed);
    }
    assert_eq!(SyncFailure::from(Fetch), SyncFailure::NetworkFailed);
    assert_eq!(
        SyncFailure::from(AuthExpired),
        SyncFailure::AuthenticationFailed
    );
    assert_eq!(
        SyncFailure::from(MoodleSyncError::Fetch),
        SyncFailure::NetworkFailed
    );
    assert_eq!(
        SyncFailure::from(MoodleSyncError::Parse),
        SyncFailure::SourceDataInvalid
    );
    assert_eq!(
        SyncFailure::from(MoodleSyncError::Validation),
        SyncFailure::SourceDataInvalid
    );
    assert_eq!(
        SyncFailure::from(MoodleSyncError::Persistence),
        SyncFailure::PersistenceFailed
    );
    assert_eq!(
        SyncFailure::from(WebClassReauthSyncError::AuthenticationFailed),
        SyncFailure::AuthenticationFailed
    );
    assert_eq!(
        SyncFailure::from(WebClassReauthSyncError::CredentialsUnavailable),
        SyncFailure::CredentialUnavailable
    );
    assert_eq!(
        SyncFailure::from(WebClassReauthSyncError::Sync(AuthExpired)),
        SyncFailure::AuthenticationFailed
    );
}
#[test]
fn database_startup_error_never_logs_in_and_credential_callback_handles_missing() {
    let mut service = SyncService::new(
        FakeNetwork::default(),
        secrets(true, true),
        Err(ApplicationError::DatabaseStartup),
    );
    let expected = SourceSyncResult::Failed {
        error: SyncFailure::PersistenceFailed,
    };
    let (m, w) = report(service.run());
    assert_eq!(m, expected);
    assert_eq!(w, expected);
    assert_eq!(service.backend.login_calls, 0);
    assert_eq!(service.backend.moodle_calls, 0);
    let missing = secrets(false, false);
    assert!(
        CredentialProvider(&missing)
            .with_credentials(|_| panic!("must not be called"))
            .is_none()
    );
}

#[test]
fn initial_auth_failure_and_failed_credential_replacement_preserve_independent_sync() {
    let mut service = service(true, true);
    service.backend.login_failure = Some(SyncFailure::AuthenticationFailed);
    let (moodle, webclass) = report(service.run());
    assert!(matches!(moodle, SourceSyncResult::Success { .. }));
    assert_eq!(
        webclass,
        SourceSyncResult::Failed {
            error: SyncFailure::AuthenticationFailed
        }
    );
    assert!(service.session.is_none());
    service.backend.login_failure = None;
    service.run();
    assert_eq!(service.backend.login_calls, 2);
    assert_eq!(
        service.credentials.save_webclass(credentials("", PASSWORD)),
        Err(ApplicationError::InvalidCredentials)
    );
    service.run();
    assert_eq!(service.backend.login_calls, 2);
}

#[test]
fn credential_read_errors_are_source_local_and_invalidate_cached_session() {
    struct ErrorSecrets {
        inner: SettingsService<CredentialVault<FakeBackend>>,
        fail_moodle: bool,
        fail_webclass: bool,
    }
    impl SyncSecrets for ErrorSecrets {
        fn moodle(&self) -> Result<Option<SecretText>, ApplicationError> {
            if self.fail_moodle {
                Err(ApplicationError::CredentialReadFailed)
            } else {
                self.inner.moodle()
            }
        }
        fn webclass(&self) -> Result<(u64, Option<WebClassCredentials>), ApplicationError> {
            if self.fail_webclass {
                Err(ApplicationError::CredentialReadFailed)
            } else {
                self.inner.webclass()
            }
        }
    }
    let credentials = ErrorSecrets {
        inner: secrets(true, true),
        fail_moodle: false,
        fail_webclass: false,
    };
    let mut service = SyncService::new(
        FakeNetwork::default(),
        credentials,
        Ok(SqliteStore::open(":memory:").unwrap()),
    );
    service.run();
    assert!(service.session.is_some());
    service.credentials.fail_webclass = true;
    let (moodle, webclass) = report(service.run());
    assert!(matches!(moodle, SourceSyncResult::Success { .. }));
    assert_eq!(
        webclass,
        SourceSyncResult::Failed {
            error: SyncFailure::CredentialUnavailable
        }
    );
    assert!(service.session.is_none());
    assert!(
        CredentialProvider(&service.credentials)
            .with_credentials(|_| panic!("not available"))
            .is_none()
    );
    service.credentials.fail_moodle = true;
    service.credentials.fail_webclass = false;
    let (moodle, webclass) = report(service.run());
    assert_eq!(
        moodle,
        SourceSyncResult::Failed {
            error: SyncFailure::CredentialUnavailable
        }
    );
    assert!(matches!(webclass, SourceSyncResult::Success { .. }));
    assert_eq!(service.backend.login_calls, 2);
}
