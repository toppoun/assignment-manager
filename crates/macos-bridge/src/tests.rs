use super::*;
use assignment_manager::{
    domain::{CompletionState, Deadline, SubmissionTarget},
    import::OriginKind,
    persistence::{ManualAssignmentInput, SqliteStore},
};
use assignment_manager_application::{error::ApplicationError, models};
use chrono::{DateTime, NaiveDate};
use std::error::Error;

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(future)
}
fn all() -> Option<QueryOptions> {
    Some(QueryOptions {
        completion: CompletionSelectionDto::All,
        ..Default::default()
    })
}

#[test]
fn query_selections_map_to_shared_options_without_interpreting_ids_or_time() {
    use assignment_manager::{domain::CourseId, query::*};
    use assignment_manager_application::query::{AssignmentQuery, QueryClock};
    let clock =
        QueryClock::from_local(DateTime::parse_from_rfc3339("2026-10-04T12:00:00+05:45").unwrap())
            .unwrap();
    let map = |options| AssignmentQuery::from(options).options(&clock);
    assert_eq!(
        map(QueryOptions::default()).unwrap(),
        AssignmentQuery::default().options(&clock).unwrap()
    );
    for (completion, expected) in [
        (CompletionSelectionDto::All, CompletionFilter::All),
        (
            CompletionSelectionDto::Incomplete,
            CompletionFilter::State(CompletionState::Incomplete),
        ),
        (
            CompletionSelectionDto::Completed,
            CompletionFilter::State(CompletionState::Completed),
        ),
    ] {
        assert_eq!(
            map(QueryOptions {
                completion,
                ..Default::default()
            })
            .unwrap()
            .completion,
            expected
        );
    }
    for (deadline, expected) in [
        (DeadlineSelectionDto::All, DeadlineFilter::All),
        (DeadlineSelectionDto::Overdue, DeadlineFilter::Overdue),
        (DeadlineSelectionDto::WithinSevenDays, clock.seven_days),
    ] {
        assert_eq!(
            map(QueryOptions {
                deadline,
                ..Default::default()
            })
            .unwrap()
            .deadline,
            expected
        );
    }
    for (course, expected) in [
        (CourseSelectionDto::All, CourseFilter::All),
        (CourseSelectionDto::Unassigned, CourseFilter::Unassigned),
        (
            CourseSelectionDto::Course {
                id: " ID 01 ".into(),
            },
            CourseFilter::Course(CourseId::new(" ID 01 ")),
        ),
    ] {
        assert_eq!(
            map(QueryOptions {
                course,
                ..Default::default()
            })
            .unwrap()
            .course,
            expected
        );
    }
    assert_eq!(
        map(QueryOptions {
            course: CourseSelectionDto::Course { id: String::new() },
            ..Default::default()
        })
        .unwrap_err(),
        ApplicationError::InvalidQuery
    );
    for (sort, expected) in [
        (SortSelectionDto::Deadline, AssignmentSort::ByDeadline),
        (SortSelectionDto::Course, AssignmentSort::ByCourse),
        (SortSelectionDto::Title, AssignmentSort::ByTitle),
    ] {
        assert_eq!(
            map(QueryOptions {
                sort,
                ..Default::default()
            })
            .unwrap()
            .sort,
            expected
        );
    }
}

#[test]
fn deadline_conversion_preserves_calendar_date_and_exact_utc_instant() {
    let date = NaiveDate::from_ymd_opt(2028, 2, 29).unwrap();
    assert_eq!(
        DeadlineDto::from(Deadline::DateOnly(date)),
        DeadlineDto::DateOnly {
            year: 2028,
            month: 2,
            day: 29
        }
    );
    for (input, expected) in [
        (
            "2030-02-04T19:20:30.123456789+09:00",
            "2030-02-04T10:20:30.123456789Z",
        ),
        ("1960-01-01T00:00:00Z", "1960-01-01T00:00:00Z"),
    ] {
        let instant = DateTime::parse_from_rfc3339(input).unwrap().to_utc();
        assert_eq!(
            DeadlineDto::from(Deadline::DateTime(instant)),
            DeadlineDto::DateTime {
                utc_rfc3339: expected.into()
            }
        );
    }
}

#[test]
fn completion_submission_and_status_keep_every_variant() {
    assert_eq!(
        CompletionDto::from(CompletionState::Incomplete),
        CompletionDto::Incomplete
    );
    assert_eq!(
        CompletionDto::from(CompletionState::Completed),
        CompletionDto::Completed
    );
    for (core, bridge) in [
        (SubmissionTarget::Unknown, SubmissionDto::Unknown),
        (SubmissionTarget::None, SubmissionDto::None),
        (SubmissionTarget::Moodle, SubmissionDto::Moodle),
        (SubmissionTarget::WebClass, SubmissionDto::WebClass),
        (SubmissionTarget::External, SubmissionDto::External),
    ] {
        assert_eq!(SubmissionDto::from(core), bridge);
    }
    for (application, bridge) in [
        (models::DeadlineStatus::Overdue, DeadlineStatusDto::Overdue),
        (models::DeadlineStatus::Today, DeadlineStatusDto::Today),
        (
            models::DeadlineStatus::Upcoming,
            DeadlineStatusDto::Upcoming,
        ),
        (models::DeadlineStatus::None, DeadlineStatusDto::None),
    ] {
        assert_eq!(DeadlineStatusDto::from(application), bridge);
    }
}

#[test]
fn course_conversion_preserves_identity_label_and_optional_origin_without_merging() {
    for origin in [
        None,
        Some(OriginKind::Manual),
        Some(OriginKind::Moodle),
        Some(OriginKind::WebClass),
    ] {
        let course = CourseDto::from(models::CourseOption {
            id: "internal-course-id".into(),
            display_label: "Same name".into(),
            origin,
        });
        assert_eq!(course.id, "internal-course-id");
        assert_eq!(course.display_label, "Same name");
        assert_eq!(course.origin, origin.map(Into::into));
    }
    let response = AssignmentListDto::from(models::AssignmentListResponse {
        items: vec![],
        total_count: 0,
        courses: ["id-one", "id-two"]
            .into_iter()
            .map(|id| models::CourseOption {
                id: id.into(),
                display_label: "Same name".into(),
                origin: None,
            })
            .collect(),
    });
    assert_eq!(response.courses.len(), 2);
    assert_ne!(response.courses[0].id, response.courses[1].id);
}

#[test]
fn read_model_projection_has_only_safe_fields_and_keeps_rust_order() {
    let entry = |id: &str| models::AssignmentListEntry {
        id: id.into(),
        title: "Safe title".into(),
        deadline: None,
        completion: CompletionState::Incomplete,
        submission: SubmissionTarget::External,
        has_submission_url: true,
        course: None,
        status: models::DeadlineStatus::Overdue,
    };
    let response = AssignmentListDto::from(models::AssignmentListResponse {
        items: vec![entry("z"), entry("a")],
        courses: vec![],
        total_count: 3,
    });
    assert_eq!(response.total_count, 3);
    assert_eq!(
        response
            .items
            .iter()
            .map(|i| i.id.as_str())
            .collect::<Vec<_>>(),
        ["z", "a"]
    );
    assert_eq!(
        response.items[0],
        AssignmentDto {
            id: "z".into(),
            title: "Safe title".into(),
            deadline: None,
            completion: CompletionDto::Incomplete,
            submission: SubmissionDto::External,
            course: None,
            deadline_status: DeadlineStatusDto::Overdue
        }
    );
    let debug = format!("{response:?}");
    assert!(!debug.contains("url"));
    assert!(!debug.contains("has_submission"));
}

#[test]
fn every_application_error_maps_to_a_fixed_typed_error_without_cause() {
    for error in [
        ApplicationError::DataDirectory,
        ApplicationError::DatabaseStartup,
        ApplicationError::Storage,
        ApplicationError::NotFound,
        ApplicationError::InvalidAssignmentId,
        ApplicationError::InvalidQuery,
        ApplicationError::CredentialUnavailable,
        ApplicationError::CredentialReadFailed,
        ApplicationError::CredentialWriteFailed,
        ApplicationError::CredentialDeleteFailed,
        ApplicationError::SettingsReadFailed,
        ApplicationError::SettingsWriteFailed,
        ApplicationError::InvalidMoodleFeed,
        ApplicationError::InvalidCredentials,
        ApplicationError::Busy,
        ApplicationError::Unavailable,
    ] {
        let bridge = BridgeError::from(error);
        assert_eq!(bridge.to_string(), error.code());
        assert_eq!(format!("{bridge:?}"), format!("{error:?}"));
        assert!(bridge.source().is_none());
    }
}

#[test]
fn open_validates_initialization_and_returns_only_safe_categories() {
    assert!(matches!(
        block_on(MacApplication::open("relative-private-path.db".into())),
        Err(BridgeError::DataDirectory)
    ));
    let root = tempfile::tempdir().unwrap();
    let invalid = root.path().join("private-invalid.db");
    std::fs::write(&invalid, "private-sqlite-cause").unwrap();
    assert!(matches!(
        block_on(MacApplication::open(invalid.to_str().unwrap().into())),
        Err(BridgeError::DatabaseStartup)
    ));
}

#[test]
fn empty_database_opens_and_queries() {
    let root = tempfile::tempdir().unwrap();
    let app = block_on(MacApplication::open(
        root.path().join("new/data.db").to_str().unwrap().into(),
    ))
    .unwrap();
    assert_eq!(
        block_on(app.query_assignments(None)).unwrap(),
        AssignmentListDto {
            items: vec![],
            courses: vec![],
            total_count: 0
        }
    );
    block_on(app.shutdown()).unwrap();
}

#[test]
fn commit_refresh_clones_shutdown_and_reopen_preserve_application_contract() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<MacApplication>();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("data.db");
    let mut store = SqliteStore::open(&path).unwrap();
    let assignment = store
        .create_manual_assignment(ManualAssignmentInput {
            title: "test".into(),
            course_id: None,
            deadline: None,
            submission: SubmissionTarget::Unknown,
            submission_url: Some("https://example.invalid/?token=private-marker".into()),
        })
        .unwrap();
    drop(store);
    let app = block_on(MacApplication::open(path.to_str().unwrap().into())).unwrap();
    let proxy = Arc::clone(&app);
    let weak = Arc::downgrade(&app);
    assert_eq!(
        block_on(app.query_assignments(None)).unwrap().items.len(),
        1
    );
    assert_eq!(
        block_on(app.toggle_completion(String::new())),
        Err(BridgeError::InvalidAssignmentId)
    );
    assert_eq!(
        block_on(app.toggle_completion("missing".into())),
        Err(BridgeError::NotFound)
    );
    let committed = block_on(proxy.toggle_completion(assignment.id.as_str().into())).unwrap();
    assert_eq!(committed, CompletionDto::Completed);
    assert!(
        block_on(app.query_assignments(None))
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(
        block_on(app.query_assignments(all())).unwrap().items[0].completion,
        committed
    );
    // A failed later refresh must not relabel or retry this acknowledged toggle.
    block_on(proxy.shutdown()).unwrap();
    assert!(app.application.is_shutdown_complete());
    assert_eq!(
        block_on(app.query_assignments(all())),
        Err(BridgeError::Unavailable)
    );
    assert_eq!(
        block_on(app.toggle_completion(assignment.id.as_str().into())),
        Err(BridgeError::Unavailable)
    );
    assert_eq!(committed, CompletionDto::Completed);
    block_on(app.shutdown()).unwrap();
    drop(app);
    assert!(weak.upgrade().is_some());
    drop(proxy);
    assert!(weak.upgrade().is_none());
    let reopened = block_on(MacApplication::open(path.to_str().unwrap().into())).unwrap();
    assert_eq!(
        block_on(reopened.query_assignments(all())).unwrap().items[0].completion,
        committed
    );
    block_on(reopened.shutdown()).unwrap();
}

#[derive(Default)]
struct ForeignCredentials {
    records: std::sync::Mutex<std::collections::HashMap<CredentialEntry, Vec<u8>>>,
    failure: std::sync::Mutex<Option<NativeStoreError>>,
}
impl NativeCredentialStore for ForeignCredentials {
    fn read(&self, entry: CredentialEntry) -> Result<Option<Vec<u8>>, NativeStoreError> {
        if let Some(error) = *self.failure.lock().unwrap() {
            return Err(error);
        }
        Ok(self.records.lock().unwrap().get(&entry).cloned())
    }
    fn write(&self, entry: CredentialEntry, value: Vec<u8>) -> Result<(), NativeStoreError> {
        if let Some(error) = *self.failure.lock().unwrap() {
            return Err(error);
        }
        self.records.lock().unwrap().insert(entry, value);
        Ok(())
    }
    fn delete(&self, entry: CredentialEntry) -> Result<(), NativeStoreError> {
        if let Some(error) = *self.failure.lock().unwrap() {
            return Err(error);
        }
        self.records.lock().unwrap().remove(&entry);
        Ok(())
    }
}
#[derive(Default)]
struct ForeignSettings(std::sync::Mutex<Option<bool>>, std::sync::Mutex<bool>);
impl NativeSettingsStore for ForeignSettings {
    fn load_sync_on_startup(&self) -> Result<Option<bool>, NativeStoreError> {
        Ok(*self.0.lock().unwrap())
    }
    fn save_sync_on_startup(&self, enabled: bool) -> Result<(), NativeStoreError> {
        if *self.1.lock().unwrap() {
            return Err(NativeStoreError::Failed);
        }
        *self.0.lock().unwrap() = Some(enabled);
        Ok(())
    }
}
#[test]
fn native_ports_roundtrip_replace_remove_restart_and_safe_projection() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("test.db");
    let settings = Arc::new(ForeignSettings::default());
    let credentials = Arc::new(ForeignCredentials::default());
    let open = || {
        block_on(MacApplication::open_with_native_stores(
            path.to_str().unwrap().into(),
            settings.clone(),
            credentials.clone(),
        ))
        .unwrap()
    };
    let app = open();
    assert_eq!(
        block_on(app.get_settings_status()).unwrap(),
        SettingsStatusDto {
            moodle_configured: false,
            webclass_configured: false,
            sync_on_startup: false
        }
    );
    block_on(app.remove_moodle_config()).unwrap();
    block_on(app.remove_webclass_credentials()).unwrap();
    let url = "https://example.invalid/?token=PRIVATE_NATIVE_URL";
    block_on(app.save_moodle_config(url.into())).unwrap();
    block_on(
        app.save_webclass_credentials("PRIVATE_NATIVE_ID".into(), "PRIVATE_NATIVE_PASS".into()),
    )
    .unwrap();
    block_on(app.set_sync_on_startup(true)).unwrap();
    block_on(app.shutdown()).unwrap();
    let app = open();
    let status = block_on(app.get_settings_status()).unwrap();
    assert!(status.moodle_configured && status.webclass_configured && status.sync_on_startup);
    *settings.1.lock().unwrap() = true;
    assert_eq!(
        block_on(app.set_sync_on_startup(false)),
        Err(BridgeError::SettingsWriteFailed)
    );
    assert!(block_on(app.get_settings_status()).unwrap().sync_on_startup);
    assert_eq!(*settings.0.lock().unwrap(), Some(true));
    *settings.1.lock().unwrap() = false;
    block_on(app.save_moodle_config(format!("{url}_REPLACE"))).unwrap();
    block_on(app.save_webclass_credentials("PRIVATE_NEW_ID".into(), "PRIVATE_NEW_PASS".into()))
        .unwrap();
    let saved = credentials.records.lock().unwrap().clone();
    *credentials.failure.lock().unwrap() = Some(NativeStoreError::Failed);
    assert_eq!(
        block_on(app.save_moodle_config(url.into())),
        Err(BridgeError::CredentialWriteFailed)
    );
    assert_eq!(
        block_on(app.save_webclass_credentials("id".into(), "pass".into())),
        Err(BridgeError::CredentialWriteFailed)
    );
    assert_eq!(
        block_on(app.remove_webclass_credentials()),
        Err(BridgeError::CredentialDeleteFailed)
    );
    *credentials.failure.lock().unwrap() = None;
    assert_eq!(*credentials.records.lock().unwrap(), saved);
    let safe = format!(
        "{status:?} {:?} {:?}",
        settings.0.lock().unwrap(),
        block_on(app.query_assignments(None)).unwrap()
    );
    block_on(app.remove_moodle_config()).unwrap();
    assert!(
        !block_on(app.get_settings_status())
            .unwrap()
            .moodle_configured
    );
    assert!(
        block_on(app.get_settings_status())
            .unwrap()
            .webclass_configured
    );
    block_on(app.remove_webclass_credentials()).unwrap();
    assert!(
        !block_on(app.get_settings_status())
            .unwrap()
            .webclass_configured
    );
    block_on(app.shutdown()).unwrap();
    let database = std::fs::read(path).unwrap();
    for marker in ["PRIVATE_NATIVE", "PRIVATE_NEW"] {
        assert!(!safe.contains(marker));
        assert!(
            !database
                .windows(marker.len())
                .any(|b| b == marker.as_bytes())
        );
    }
}

#[test]
fn native_callbacks_corruption_and_validation_return_only_safe_errors() {
    let root = tempfile::tempdir().unwrap();
    let credentials = Arc::new(ForeignCredentials::default());
    let app = block_on(MacApplication::open_with_native_stores(
        root.path().join("test.db").to_str().unwrap().into(),
        Arc::new(ForeignSettings::default()),
        credentials.clone(),
    ))
    .unwrap();
    for input in [
        "http://example.invalid",
        "https://user:PRIVATE@example.invalid",
        "https://",
        "",
    ] {
        assert_eq!(
            block_on(app.save_moodle_config(input.into())),
            Err(BridgeError::InvalidMoodleFeed)
        );
    }
    assert_eq!(
        block_on(app.save_webclass_credentials(" ".into(), "PRIVATE_PASS".into())),
        Err(BridgeError::InvalidCredentials)
    );
    for entry in [CredentialEntry::Moodle, CredentialEntry::WebClass] {
        credentials
            .records
            .lock()
            .unwrap()
            .insert(entry, b"PRIVATE_CORRUPT".to_vec());
        assert_eq!(
            block_on(app.get_settings_status()),
            Err(BridgeError::CredentialReadFailed)
        );
        credentials.records.lock().unwrap().clear();
    }
    credentials.records.lock().unwrap().insert(
        CredentialEntry::WebClass,
        br#"{"universityId":"PRIVATE_PARTIAL_ID"}"#.to_vec(),
    );
    let error = block_on(app.get_settings_status()).unwrap_err();
    assert_eq!(error, BridgeError::CredentialReadFailed);
    assert!(!format!("{error:?} {error}").contains("PRIVATE_PARTIAL_ID"));
    credentials.records.lock().unwrap().clear();
    *credentials.failure.lock().unwrap() = Some(NativeStoreError::Unavailable);
    assert_eq!(
        block_on(app.get_settings_status()),
        Err(BridgeError::CredentialUnavailable)
    );
    block_on(app.shutdown()).unwrap();
}

#[test]
fn foreign_settings_initialization_failure_does_not_disable_local_assignments() {
    struct UnavailableSettings;
    impl NativeSettingsStore for UnavailableSettings {
        fn load_sync_on_startup(&self) -> Result<Option<bool>, NativeStoreError> {
            Err(NativeStoreError::Unavailable)
        }
        fn save_sync_on_startup(&self, _: bool) -> Result<(), NativeStoreError> {
            Err(NativeStoreError::Unavailable)
        }
    }
    let root = tempfile::tempdir().unwrap();
    let app = block_on(MacApplication::open_with_native_stores(
        root.path().join("test.db").to_str().unwrap().into(),
        Arc::new(UnavailableSettings),
        Arc::new(ForeignCredentials::default()),
    ))
    .unwrap();
    assert!(
        block_on(app.query_assignments(None))
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(
        block_on(app.get_settings_status()),
        Err(BridgeError::SettingsReadFailed)
    );
    block_on(app.shutdown()).unwrap();
}
