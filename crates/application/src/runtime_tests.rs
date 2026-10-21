use super::*;
use crate::{
    query::{CompletionSelection, CourseSelection},
    sync_service::{SourceSyncResult, SyncCounts, SyncFailure, tests::import},
    test_support::{FakeCredentials, MemorySettings, URL, block_on, fake},
};
use assignment_manager::{
    domain::SubmissionTarget,
    persistence::{ManualAssignmentInput, SqliteStore},
    sync::WebClassCredentialProvider,
};
use std::{
    sync::{atomic::AtomicUsize, mpsc},
    thread,
    time::Duration,
};

struct PausedNetwork {
    started: mpsc::Sender<()>,
    resume: mpsc::Receiver<()>,
    fail: bool,
}
impl SyncBackend for PausedNetwork {
    type Session = ();
    fn moodle(
        &mut self,
        _: SecretText,
        store: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure> {
        self.started.send(()).unwrap();
        self.resume.recv_timeout(Duration::from_secs(15)).unwrap();
        let fail = self.fail;
        self.fail = true;
        if fail {
            Err(SyncFailure::NetworkFailed)
        } else {
            import(store)
        }
    }
    fn login(&mut self, _: WebClassCredentials) -> Result<(), SyncFailure> {
        panic!("not configured")
    }
    fn webclass(
        &mut self,
        _: &mut (),
        _: &mut impl WebClassCredentialProvider,
        _: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure> {
        panic!("not configured")
    }
}

fn all() -> AssignmentQuery {
    AssignmentQuery {
        completion: CompletionSelection::All,
        ..Default::default()
    }
}

#[test]
fn explicit_clock_reaches_worker_without_changing_desktop_query_or_lifetime() {
    use crate::query::{DeadlineSelection, QueryClock};
    use assignment_manager::domain::Deadline;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("clock.db");
    let mut store = SqliteStore::open(&path).unwrap();
    for deadline in [
        Deadline::DateOnly(chrono::NaiveDate::from_ymd_opt(2001, 1, 2).unwrap()),
        Deadline::DateTime("2001-01-01T15:00:00.123456789Z".parse().unwrap()),
    ] {
        store
            .create_manual_assignment(ManualAssignmentInput {
                title: "clock fixture".into(),
                deadline: Some(deadline),
                course_id: None,
                submission: SubmissionTarget::Unknown,
                submission_url: None,
            })
            .unwrap();
    }
    drop(store);
    let app =
        ApplicationHandle::open(Ok(path), || Ok((MemorySettings::default(), fake()))).unwrap();
    let today = || AssignmentQuery {
        deadline: DeadlineSelection::Today,
        ..all()
    };
    let clock = |value| {
        QueryClock::from_local(chrono::DateTime::parse_from_rfc3339(value).unwrap()).unwrap()
    };
    let jst = block_on(
        app.query_assignments_with_clock(today(), clock("2001-01-02T00:00:00.123456789+09:00")),
    )
    .unwrap();
    assert_eq!(jst.total_count, 2);
    assert_eq!(jst.items.len(), 2);
    assert!(
        jst.items
            .iter()
            .all(|item| item.status == crate::models::DeadlineStatus::Today)
    );
    // The same instant with a different calendar proves the supplied clock is used.
    let utc = block_on(
        app.clone()
            .query_assignments_with_clock(today(), clock("2001-01-01T15:00:00.123456789Z")),
    )
    .unwrap();
    assert_eq!(utc.items.len(), 1);
    assert_eq!(
        block_on(app.query_assignments(all())).unwrap().total_count,
        2
    );
    block_on(app.shutdown());
    assert_eq!(
        block_on(app.query_assignments_with_clock(all(), clock("2001-01-01T00:00:00Z"))),
        Err(ApplicationError::Unavailable)
    );
}
fn configured() -> FakeCredentials {
    use crate::credentials::CredentialStore;
    let mut credentials = fake();
    credentials
        .save_moodle_feed_secret(SecretText::new(URL.into()))
        .unwrap();
    credentials
}

#[test]
fn single_flight_network_wait_keeps_query_toggle_available_and_completion_survives_sync_restart() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ApplicationHandle>();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("app.db");
    let mut store = SqliteStore::open(&path).unwrap();
    import(&mut store).unwrap();
    drop(store);
    let (started, signal) = mpsc::channel();
    let (resume, paused) = mpsc::channel();
    let app = ApplicationHandle::open_with_backend(
        Ok(path.clone()),
        || Ok((MemorySettings::default(), configured())),
        PausedNetwork {
            started,
            resume: paused,
            fail: false,
        },
    )
    .unwrap();
    let first = app.clone();
    let command = thread::spawn(move || block_on(first.sync_now()));
    signal.recv_timeout(Duration::from_secs(5)).unwrap();
    for _ in 0..5 {
        assert_eq!(
            block_on(app.sync_now()).unwrap(),
            SyncReport::AlreadyRunning
        );
    }
    assert!(app.get_sync_status().unwrap().running);
    let local = app.clone();
    let (reply, response) = mpsc::channel();
    thread::spawn(move || {
        let id = block_on(local.query_assignments(all()))
            .unwrap()
            .items
            .remove(0)
            .id;
        assert_eq!(
            block_on(local.toggle_completion(id.clone())).unwrap(),
            CompletionState::Completed
        );
        reply.send(id).unwrap();
    });
    let id = response.recv_timeout(Duration::from_secs(5)).unwrap(); // network still paused
    resume.send(()).unwrap();
    let result = command.join().unwrap().unwrap();
    assert!(result.should_reload());
    assert!(matches!(
        result,
        SyncReport::Completed {
            moodle: SourceSyncResult::Success { .. },
            ..
        }
    ));
    assert!(!app.get_sync_status().unwrap().running);
    assert_eq!(app.get_sync_status().unwrap().last_report, Some(result));
    assert!(signal.try_recv().is_err()); // no queued runs
    let item = block_on(app.query_assignments(all()))
        .unwrap()
        .items
        .remove(0);
    assert_eq!(item.id, id);
    assert_eq!(item.completion, CompletionState::Completed);
    let later = app.clone();
    let command = thread::spawn(move || block_on(later.sync_now()));
    signal.recv_timeout(Duration::from_secs(5)).unwrap();
    resume.send(()).unwrap();
    assert!(matches!(
        command.join().unwrap().unwrap(),
        SyncReport::Completed {
            moodle: SourceSyncResult::Failed { .. },
            ..
        }
    ));
    assert_eq!(
        block_on(app.query_assignments(all())).unwrap().items[0].completion,
        CompletionState::Completed
    );
    block_on(app.shutdown());
    let reopened =
        ApplicationHandle::open(Ok(path), || Ok((MemorySettings::default(), fake()))).unwrap();
    assert_eq!(
        block_on(reopened.query_assignments(all())).unwrap().items[0].completion,
        CompletionState::Completed
    );
    assert_eq!(
        block_on(reopened.toggle_completion(id)).unwrap(),
        CompletionState::Incomplete
    );
    block_on(reopened.shutdown());
}

#[test]
fn lost_reply_keeps_single_flight_and_shutdown_drains_before_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let (started, signal) = mpsc::channel();
    let (resume, paused) = mpsc::channel();
    let app = ApplicationHandle::open_with_backend(
        Ok(root.path().join("app.db")),
        || Ok((MemorySettings::default(), configured())),
        PausedNetwork {
            started,
            resume: paused,
            fail: false,
        },
    )
    .unwrap();
    // Admit without retaining the result receiver, exactly like a cancelled caller.
    let response = app.admit(|w| w.sync.enqueue()).unwrap();
    drop(response);
    signal.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        block_on(app.sync_now()).unwrap(),
        SyncReport::AlreadyRunning
    );
    let other_view = app.clone();
    assert!(app.begin_shutdown());
    assert!(!other_view.begin_shutdown());
    assert!(!app.is_shutdown_complete());
    assert_eq!(
        block_on(other_view.query_assignments(all())),
        Err(ApplicationError::Unavailable)
    );
    assert_eq!(
        block_on(other_view.sync_now()),
        Err(ApplicationError::Unavailable)
    );
    resume.send(()).unwrap();
    block_on(app.shutdown());
    assert!(app.is_shutdown_complete());
    let store = SqliteStore::open(root.path().join("app.db")).unwrap();
    assert_eq!(
        assignment_manager::query::load_assignment_list(&store)
            .unwrap()
            .len(),
        1
    );
}

struct CountingNetwork(Arc<AtomicUsize>);
impl SyncBackend for CountingNetwork {
    type Session = ();
    fn moodle(
        &mut self,
        _: SecretText,
        store: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure> {
        self.0.fetch_add(1, Ordering::SeqCst);
        import(store)
    }
    fn login(&mut self, _: WebClassCredentials) -> Result<(), SyncFailure> {
        panic!("not configured")
    }
    fn webclass(
        &mut self,
        _: &mut (),
        _: &mut impl WebClassCredentialProvider,
        _: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure> {
        panic!("not configured")
    }
}

#[test]
fn startup_policy_is_once_across_clones_including_off_unconfigured_and_later_settings_changes() {
    for (enabled, configured_source) in [(false, true), (true, false), (true, true)] {
        let root = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let app = ApplicationHandle::open_with_backend(
            Ok(root.path().join("app.db")),
            move || {
                Ok((
                    MemorySettings {
                        value: crate::settings::Settings {
                            sync_on_startup: enabled,
                        },
                        ..Default::default()
                    },
                    if configured_source {
                        configured()
                    } else {
                        fake()
                    },
                ))
            },
            CountingNetwork(Arc::clone(&calls)),
        )
        .unwrap();
        block_on(app.query_assignments(all())).unwrap();
        let view = app.clone();
        let first = app.clone();
        let first = thread::spawn(move || block_on(first.run_startup_sync()));
        let second = block_on(view.run_startup_sync()).unwrap();
        let first = first.join().unwrap().unwrap();
        assert_eq!(
            usize::from(first.is_some()) + usize::from(second.is_some()),
            usize::from(enabled && configured_source)
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            usize::from(enabled && configured_source)
        );
        block_on(app.set_sync_on_startup(true)).unwrap();
        block_on(app.save_moodle_config(SecretText::new(URL.into()))).unwrap();
        assert_eq!(block_on(view.run_startup_sync()).unwrap(), None);
        block_on(app.shutdown());
    }
}

#[test]
fn startup_error_is_safe_consumes_once_and_does_not_disable_offline_query() {
    let root = tempfile::tempdir().unwrap();
    let app = ApplicationHandle::open(Ok(root.path().join("app.db")), || {
        Err::<(MemorySettings, FakeCredentials), _>(ApplicationError::SettingsReadFailed)
    })
    .unwrap();
    assert!(
        block_on(app.query_assignments(all()))
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(
        block_on(app.run_startup_sync()),
        Err(ApplicationError::SettingsReadFailed)
    );
    assert_eq!(block_on(app.run_startup_sync()).unwrap(), None);
    assert_eq!(
        block_on(app.get_settings_status()),
        Err(ApplicationError::SettingsReadFailed)
    );
    block_on(app.shutdown());
}

#[test]
fn toggle_acknowledges_commit_independently_of_later_invalid_query_and_shutdown() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("app.db");
    let mut store = SqliteStore::open(&path).unwrap();
    let id = store
        .create_manual_assignment(ManualAssignmentInput {
            title: "local".into(),
            deadline: None,
            course_id: None,
            submission: SubmissionTarget::Unknown,
            submission_url: None,
        })
        .unwrap()
        .id;
    drop(store);
    let app = ApplicationHandle::open(Ok(path.clone()), || Ok((MemorySettings::default(), fake())))
        .unwrap();
    assert_eq!(
        block_on(app.toggle_completion(id.as_str().into())).unwrap(),
        CompletionState::Completed
    );
    assert_eq!(
        block_on(app.query_assignments(AssignmentQuery {
            course: CourseSelection::Course { id: String::new() },
            ..all()
        })),
        Err(ApplicationError::InvalidQuery)
    );
    assert_eq!(
        block_on(app.toggle_completion(String::new())),
        Err(ApplicationError::InvalidAssignmentId)
    );
    assert_eq!(
        block_on(app.toggle_completion("missing".into())),
        Err(ApplicationError::NotFound)
    );
    block_on(app.shutdown());
    assert_eq!(
        SqliteStore::open(path)
            .unwrap()
            .get_assignment(&id)
            .unwrap()
            .unwrap()
            .completion,
        CompletionState::Completed
    );
}

#[test]
fn failed_initialization_is_retained_even_after_path_is_repaired() {
    let root = tempfile::tempdir().unwrap();
    let blocked = root.path().join("blocked");
    std::fs::write(&blocked, "private").unwrap();
    let app = ApplicationHandle::open(Ok(blocked.join("app.db")), || {
        Ok((MemorySettings::default(), fake()))
    })
    .unwrap();
    assert_eq!(
        block_on(app.query_assignments(all())),
        Err(ApplicationError::DataDirectory)
    );
    std::fs::remove_file(&blocked).unwrap();
    std::fs::create_dir(&blocked).unwrap();
    assert_eq!(
        block_on(app.query_assignments(all())),
        Err(ApplicationError::DataDirectory)
    );
    block_on(app.shutdown());
}

#[test]
fn settings_ports_are_initialized_accessed_and_dropped_on_worker_before_shutdown_completes() {
    use crate::credentials::CredentialStore;
    use crate::settings::{Settings, SettingsStore};
    struct ThreadBoundSettings {
        owner: thread::ThreadId,
        dropped: mpsc::Sender<thread::ThreadId>,
    }
    impl SettingsStore for ThreadBoundSettings {
        fn load_or_create(&mut self) -> Result<Settings, ApplicationError> {
            assert_eq!(thread::current().id(), self.owner);
            Ok(Settings::default())
        }
        fn save(&mut self, _: &Settings) -> Result<(), ApplicationError> {
            assert_eq!(thread::current().id(), self.owner);
            Ok(())
        }
    }
    impl Drop for ThreadBoundSettings {
        fn drop(&mut self) {
            self.dropped.send(thread::current().id()).unwrap();
        }
    }
    struct ThreadBoundCredentials {
        owner: thread::ThreadId,
    }
    impl CredentialStore for ThreadBoundCredentials {
        fn load_moodle_feed_secret(&self) -> Result<Option<SecretText>, ApplicationError> {
            assert_eq!(thread::current().id(), self.owner);
            Ok(None)
        }
        fn load_webclass_credentials(
            &self,
        ) -> Result<Option<WebClassCredentials>, ApplicationError> {
            assert_eq!(thread::current().id(), self.owner);
            Ok(None)
        }
        fn save_moodle_feed_secret(&mut self, _: SecretText) -> Result<(), ApplicationError> {
            assert_eq!(thread::current().id(), self.owner);
            Ok(())
        }
        fn save_webclass_credentials(
            &mut self,
            _: WebClassCredentials,
        ) -> Result<(), ApplicationError> {
            assert_eq!(thread::current().id(), self.owner);
            Ok(())
        }
        fn delete_moodle_feed_secret(&mut self) -> Result<(), ApplicationError> {
            assert_eq!(thread::current().id(), self.owner);
            Ok(())
        }
        fn delete_webclass_credentials(&mut self) -> Result<(), ApplicationError> {
            assert_eq!(thread::current().id(), self.owner);
            Ok(())
        }
    }
    let root = tempfile::tempdir().unwrap();
    let (initialized, ready) = mpsc::channel();
    let (dropped, drop_signal) = mpsc::channel();
    let caller = thread::current().id();
    let app = ApplicationHandle::open(Ok(root.path().join("app.db")), move || {
        let owner = thread::current().id();
        initialized.send(owner).unwrap();
        Ok((
            ThreadBoundSettings { owner, dropped },
            ThreadBoundCredentials { owner },
        ))
    })
    .unwrap();
    let owner = ready.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_ne!(owner, caller);
    block_on(app.get_settings_status()).unwrap();
    block_on(app.set_sync_on_startup(true)).unwrap();
    block_on(app.save_moodle_config(SecretText::new(URL.into()))).unwrap();
    block_on(app.save_webclass_credentials(crate::test_support::credentials("id", "pass")))
        .unwrap();
    block_on(app.remove_moodle_config()).unwrap();
    block_on(app.remove_webclass_credentials()).unwrap();
    block_on(app.sync_now()).unwrap(); // sync's reads also happen on settings worker
    block_on(app.shutdown());
    assert_eq!(drop_signal.try_recv().unwrap(), owner);
    assert!(app.is_shutdown_complete());
}

#[test]
fn final_handle_drop_closes_workers_without_explicit_shutdown_or_blocking_join() {
    let root = tempfile::tempdir().unwrap();
    let app = ApplicationHandle::open(Ok(root.path().join("app.db")), || {
        Ok((MemorySettings::default(), fake()))
    })
    .unwrap();
    block_on(app.query_assignments(all())).unwrap();
    let lifetime = Arc::clone(&app.runtime.lifetime);
    let other_view = app.clone();
    drop(app);
    assert!(!lifetime.is_done());
    drop(other_view);
    block_on(lifetime.wait());
    assert!(lifetime.is_done());
}

#[test]
fn empty_database_and_invalid_database_content_have_safe_distinct_results() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("private-path.db");
    std::fs::write(&path, "invalid SQLite PRIVATE_SECRET").unwrap();
    let app =
        ApplicationHandle::open(Ok(path), || Ok((MemorySettings::default(), fake()))).unwrap();
    let error = block_on(app.query_assignments(all())).unwrap_err();
    assert_eq!(error, ApplicationError::DatabaseStartup);
    assert_eq!(error.code(), "database_startup");
    block_on(app.shutdown());
    let app = ApplicationHandle::open(Ok(root.path().join("empty.db")), || {
        Ok((MemorySettings::default(), fake()))
    })
    .unwrap();
    assert!(
        block_on(app.query_assignments(all()))
            .unwrap()
            .items
            .is_empty()
    );
    block_on(app.shutdown());
}
