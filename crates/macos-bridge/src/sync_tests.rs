use super::*;
use assignment_manager::{
    import::{AssignmentCandidate, AssignmentCandidateBatch, ExternalRecordKey},
    sync::WebClassCredentialProvider,
};
use assignment_manager_application::{
    SourceSyncResult, SyncBackend, SyncCounts, SyncFailure, SyncReport,
};
use std::sync::{Mutex, mpsc};

#[derive(Default)]
struct Calls {
    moodle: usize,
    login: usize,
    webclass: usize,
    session_drops: usize,
    fail_moodle: bool,
    fail_webclass_after_commit: bool,
}
struct Session(Arc<Mutex<Calls>>);
impl Drop for Session {
    fn drop(&mut self) {
        self.0.lock().unwrap().session_drops += 1;
    }
}
struct Network {
    calls: Arc<Mutex<Calls>>,
    pause: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
}
fn import_source(store: &mut SqliteStore, origin: OriginKind) -> Result<SyncCounts, SyncFailure> {
    let batch = AssignmentCandidateBatch::try_new(vec![AssignmentCandidate {
        external_key: ExternalRecordKey {
            origin,
            scope: "default.course-related".into(),
            id: "synthetic-record".into(),
        },
        title: "Synthetic assignment".into(),
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
impl SyncBackend for Network {
    type Session = Session;
    fn moodle(
        &mut self,
        url: SecretText,
        store: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure> {
        assert!(url.expose().starts_with("https://example.invalid/"));
        if let Some((entered, release)) = self.pause.take() {
            entered.send(()).unwrap();
            release.recv().unwrap();
        }
        let mut calls = self.calls.lock().unwrap();
        calls.moodle += 1;
        if calls.fail_moodle {
            return Err(SyncFailure::NetworkFailed);
        }
        import_source(store, OriginKind::Moodle)
    }
    fn login(&mut self, credentials: WebClassCredentials) -> Result<Session, SyncFailure> {
        assert!(
            credentials
                .university_id
                .expose()
                .starts_with("SYNTHETIC_ID")
        );
        assert_eq!(credentials.password.expose(), "SYNTHETIC_PASSWORD");
        self.calls.lock().unwrap().login += 1;
        Ok(Session(self.calls.clone()))
    }
    fn webclass(
        &mut self,
        _session: &mut Session,
        _provider: &mut impl WebClassCredentialProvider,
        store: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure> {
        let mut calls = self.calls.lock().unwrap();
        calls.webclass += 1;
        let counts = import_source(store, OriginKind::WebClass)?;
        if calls.fail_webclass_after_commit {
            Err(SyncFailure::NetworkFailed)
        } else {
            Ok(counts)
        }
    }
}
fn app(
    path: &std::path::Path,
    settings: Arc<ForeignSettings>,
    credentials: Arc<ForeignCredentials>,
    calls: Arc<Mutex<Calls>>,
    pause: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
) -> Arc<MacApplication> {
    let application = ApplicationHandle::open_with_backend(
        Ok(path.into()),
        move || {
            Ok((
                native_ports::NativeSettings(settings),
                native_ports::NativeCredentials(credentials),
            ))
        },
        Network { calls, pause },
    )
    .unwrap();
    block_on(application.query_assignments(AssignmentQuery::default())).unwrap();
    Arc::new(MacApplication { application })
}
fn configure(app: &MacApplication, moodle: bool, webclass: bool) {
    if moodle {
        block_on(app.save_moodle_config("https://example.invalid/?token=SYNTHETIC_URL".into()))
            .unwrap();
    }
    if webclass {
        block_on(app.save_webclass_credentials("SYNTHETIC_ID".into(), "SYNTHETIC_PASSWORD".into()))
            .unwrap();
    }
}

#[test]
fn bridge_sync_uses_foreign_ports_for_all_source_combinations_and_safe_status() {
    for (moodle, webclass) in [(false, false), (true, false), (false, true), (true, true)] {
        let root = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Calls::default()));
        let app = app(
            &root.path().join("test.db"),
            Arc::new(ForeignSettings::default()),
            Arc::new(ForeignCredentials::default()),
            calls.clone(),
            None,
        );
        configure(&app, moodle, webclass);
        let report = block_on(app.sync_now()).unwrap();
        let SyncReportDto::Completed {
            moodle: m,
            webclass: w,
            should_reload,
        } = &report
        else {
            panic!("not completed");
        };
        assert_eq!(*m == SourceSyncResultDto::NotConfigured, !moodle);
        assert_eq!(*w == SourceSyncResultDto::NotConfigured, !webclass);
        assert_eq!(*should_reload, moodle || webclass);
        let calls = calls.lock().unwrap();
        assert_eq!(
            (calls.moodle, calls.login, calls.webclass),
            (
                usize::from(moodle),
                usize::from(webclass),
                usize::from(webclass)
            )
        );
        drop(calls);
        let status = block_on(app.get_sync_status()).unwrap();
        assert_eq!(
            status,
            SyncStatusDto {
                running: false,
                last_report: Some(report)
            }
        );
        assert!(!format!("{status:?}").contains("SYNTHETIC"));
        block_on(app.shutdown()).unwrap();
        assert_eq!(block_on(app.sync_now()), Err(BridgeError::Unavailable));
        assert_eq!(
            block_on(app.get_sync_status()),
            Err(BridgeError::Unavailable)
        );
    }
}

#[test]
fn bridge_partial_commit_url_rotation_completion_revision_and_missing_credentials() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("test.db");
    let credentials = Arc::new(ForeignCredentials::default());
    let calls = Arc::new(Mutex::new(Calls::default()));
    let app = app(
        &path,
        Arc::new(ForeignSettings::default()),
        credentials.clone(),
        calls.clone(),
        None,
    );
    configure(&app, true, true);
    block_on(app.sync_now()).unwrap();
    let initial = block_on(app.query_assignments(all())).unwrap();
    let id = initial.items[0].id.clone();
    block_on(app.toggle_completion(id.clone())).unwrap();
    block_on(app.save_moodle_config("https://example.invalid/?token=SYNTHETIC_REPLACEMENT".into()))
        .unwrap();
    calls.lock().unwrap().fail_webclass_after_commit = true;
    let report = block_on(app.sync_now()).unwrap();
    assert!(matches!(
        report,
        SyncReportDto::Completed {
            moodle: SourceSyncResultDto::Success { .. },
            webclass: SourceSyncResultDto::Failed {
                error: SyncFailureDto::NetworkFailed
            },
            should_reload: true
        }
    ));
    let reloaded = block_on(app.query_assignments(all())).unwrap();
    assert_eq!(
        initial.items.iter().map(|i| &i.id).collect::<Vec<_>>(),
        reloaded.items.iter().map(|i| &i.id).collect::<Vec<_>>()
    );
    assert_eq!(
        reloaded
            .items
            .iter()
            .find(|i| i.id == id)
            .unwrap()
            .completion,
        CompletionDto::Completed
    );
    assert_eq!(calls.lock().unwrap().login, 1);
    *credentials.failure.lock().unwrap() = Some(NativeStoreError::Failed);
    assert_eq!(
        block_on(
            app.save_webclass_credentials("SYNTHETIC_ID_NEW".into(), "SYNTHETIC_PASSWORD".into())
        ),
        Err(BridgeError::CredentialWriteFailed)
    );
    *credentials.failure.lock().unwrap() = None;
    block_on(app.sync_now()).unwrap();
    assert_eq!(calls.lock().unwrap().login, 1);
    block_on(app.save_webclass_credentials("SYNTHETIC_ID_NEW".into(), "SYNTHETIC_PASSWORD".into()))
        .unwrap();
    block_on(app.sync_now()).unwrap();
    assert_eq!(calls.lock().unwrap().login, 2);
    assert_eq!(calls.lock().unwrap().session_drops, 1);
    block_on(app.remove_webclass_credentials()).unwrap();
    assert!(matches!(
        block_on(app.sync_now()).unwrap(),
        SyncReportDto::Completed {
            webclass: SourceSyncResultDto::NotConfigured,
            ..
        }
    ));
    assert_eq!(calls.lock().unwrap().session_drops, 2);
    block_on(app.remove_moodle_config()).unwrap();
    let empty = block_on(app.sync_now()).unwrap();
    assert_eq!(
        empty,
        SyncReportDto::Completed {
            moodle: SourceSyncResultDto::NotConfigured,
            webclass: SourceSyncResultDto::NotConfigured,
            should_reload: false
        }
    );
    let safe = format!("{report:?} {empty:?} {reloaded:?}");
    assert!(
        !safe.contains("SYNTHETIC_URL")
            && !safe.contains("SYNTHETIC_ID")
            && !safe.contains("SYNTHETIC_PASSWORD")
    );
    block_on(app.shutdown()).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    for marker in [
        "SYNTHETIC_URL",
        "SYNTHETIC_REPLACEMENT",
        "SYNTHETIC_ID",
        "SYNTHETIC_PASSWORD",
    ] {
        assert!(!bytes.windows(marker.len()).any(|b| b == marker.as_bytes()));
    }
    let reopened = block_on(MacApplication::open(path.to_str().unwrap().into())).unwrap();
    assert_eq!(
        block_on(reopened.query_assignments(all()))
            .unwrap()
            .items
            .iter()
            .find(|i| i.id == id)
            .unwrap()
            .completion,
        CompletionDto::Completed
    );
    block_on(reopened.shutdown()).unwrap();
}

#[test]
fn bridge_failed_sources_still_request_reload_including_committed_webclass_group() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Calls {
        fail_moodle: true,
        fail_webclass_after_commit: true,
        ..Default::default()
    }));
    let app = app(
        &root.path().join("test.db"),
        Arc::new(ForeignSettings::default()),
        Arc::new(ForeignCredentials::default()),
        calls,
        None,
    );
    configure(&app, true, true);
    assert!(matches!(
        block_on(app.sync_now()).unwrap(),
        SyncReportDto::Completed {
            moodle: SourceSyncResultDto::Failed { .. },
            webclass: SourceSyncResultDto::Failed { .. },
            should_reload: true
        }
    ));
    assert_eq!(
        block_on(app.query_assignments(all())).unwrap().items.len(),
        1
    );
    block_on(app.shutdown()).unwrap();
}

#[test]
fn bridge_single_flight_is_not_queued_and_local_queries_remain_available() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Calls::default()));
    let (entered, entered_rx) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let app = app(
        &root.path().join("test.db"),
        Arc::new(ForeignSettings::default()),
        Arc::new(ForeignCredentials::default()),
        calls.clone(),
        Some((entered, release_rx)),
    );
    configure(&app, true, false);
    let other = app.clone();
    let first = std::thread::spawn(move || block_on(other.sync_now()));
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(block_on(app.get_sync_status()).unwrap().running);
    for _ in 0..3 {
        assert_eq!(
            block_on(app.sync_now()).unwrap(),
            SyncReportDto::AlreadyRunning
        );
    }
    assert!(
        block_on(app.query_assignments(all()))
            .unwrap()
            .items
            .is_empty()
    );
    release.send(()).unwrap();
    assert!(matches!(
        first.join().unwrap().unwrap(),
        SyncReportDto::Completed { .. }
    ));
    assert!(!block_on(app.get_sync_status()).unwrap().running);
    assert_eq!(calls.lock().unwrap().moodle, 1);
    block_on(app.shutdown()).unwrap();
}

#[test]
fn bridge_startup_disabled_enabled_unconfigured_and_once_across_proxies() {
    for (enabled, configured) in [(false, true), (true, false), (true, true)] {
        let root = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Calls::default()));
        let settings = Arc::new(ForeignSettings::default());
        *settings.0.lock().unwrap() = Some(enabled);
        let app = app(
            &root.path().join("test.db"),
            settings,
            Arc::new(ForeignCredentials::default()),
            calls.clone(),
            None,
        );
        configure(&app, configured, false);
        assert_eq!(
            block_on(app.run_startup_sync()).unwrap().is_some(),
            enabled && configured
        );
        block_on(app.set_sync_on_startup(true)).unwrap();
        assert_eq!(block_on(app.clone().run_startup_sync()).unwrap(), None);
        assert_eq!(
            calls.lock().unwrap().moodle,
            usize::from(enabled && configured)
        );
        block_on(app.shutdown()).unwrap();
    }
}

#[test]
fn bridge_sync_error_mapping_and_reload_match_shared_contract_for_every_category() {
    for (shared, expected) in [
        (SyncFailure::NetworkFailed, SyncFailureDto::NetworkFailed),
        (
            SyncFailure::AuthenticationFailed,
            SyncFailureDto::AuthenticationFailed,
        ),
        (
            SyncFailure::InvalidConfiguration,
            SyncFailureDto::InvalidConfiguration,
        ),
        (
            SyncFailure::SourceDataInvalid,
            SyncFailureDto::SourceDataInvalid,
        ),
        (
            SyncFailure::PersistenceFailed,
            SyncFailureDto::PersistenceFailed,
        ),
        (
            SyncFailure::CredentialUnavailable,
            SyncFailureDto::CredentialUnavailable,
        ),
        (SyncFailure::Unexpected, SyncFailureDto::Unexpected),
    ] {
        let report = SyncReport::Completed {
            moodle: SourceSyncResult::Failed { error: shared },
            webclass: SourceSyncResult::NotConfigured,
        };
        assert_eq!(
            SyncReportDto::from(report),
            SyncReportDto::Completed {
                moodle: SourceSyncResultDto::Failed { error: expected },
                webclass: SourceSyncResultDto::NotConfigured,
                should_reload: true
            }
        );
    }
}
