use super::trial_tests::{READ_BOT, bot, snapshot};
use super::*;
use crate::{
    import::{ExternalRecordKey, OriginKind},
    moodle_http::{MoodleExportKind, MoodleFeed},
    secured_sync::SecuredSyncClients,
    sync::{MoodleAcquisition, MoodleAcquisitionError, acquire_moodle_with_diagnostic},
    webclass_auth::synthetic::{MOODLE_URL, Scenario, SyntheticUniversity},
};

fn acquire(scenario: Scenario) -> Result<MoodleAcquisition, MoodleAcquisitionError> {
    let university = SyntheticUniversity::start(scenario);
    let mut clients = SecuredSyncClients::new(university.policy());
    clients.begin_run();
    let feed = MoodleFeed::new(
        MOODLE_INTEGRATION_SCOPE,
        MoodleExportKind::CourseRelated,
        MOODLE_URL,
    )
    .unwrap();
    acquire_moodle_with_diagnostic(&clients.moodle().unwrap(), &feed)
}

fn input_fixture() -> SyncInput {
    SyncInput::from_acquisitions(Some(&acquire(Scenario::MoodleIntegration).unwrap()), None)
        .unwrap()
}

fn target_fixture() -> VerificationTarget {
    VerificationTarget::new(target(), A, C, READ_BOT).unwrap()
}

fn mapping(category: &str, page_id: &str) -> CourseMapping {
    CourseMapping {
        course_key: ExternalRecordKey {
            origin: OriginKind::Moodle,
            scope: MOODLE_INTEGRATION_SCOPE.into(),
            id: category.into(),
        },
        page_id: page_id.into(),
    }
}

fn mappings_fixture() -> [CourseMapping; 2] {
    [
        mapping("統合試験 A", COURSE),
        mapping("統合試験 B", OLD_COURSE),
    ]
}

fn course_query(pages: Vec<Value>, cursor: Option<&str>) -> Exchange {
    Exchange::json(
        "POST",
        format!("/v1/data_sources/{C}/query"),
        list(pages, cursor),
    )
}

fn scoped_snapshot(rows: Vec<Value>) -> Vec<Exchange> {
    let mut script = vec![bot(READ_BOT)];
    script.extend(snapshot(rows));
    script.insert(
        3,
        course_query(vec![page(COURSE, C, C_DB, json!({}))], Some("more")),
    );
    script.insert(
        4,
        course_query(vec![page(OLD_COURSE, C, C_DB, json!({}))], None)
            .check(|_, body| assert_eq!(body["start_cursor"], "more")),
    );
    script
}

fn stored_rows(input: &SyncInput) -> Vec<Value> {
    input
        .assignments()
        .iter()
        .zip([PAGE_A, PAGE_B, PAGE_C, PAGE_D])
        .map(|((key, entry), id)| {
            let mut row = task(id, Some(key), &entry.title);
            let Some(Deadline::DateTime(due)) = entry.deadline else {
                panic!("fixture deadline")
            };
            row["properties"]["期限"]["date"] =
                json!({"start":due.to_rfc3339_opts(chrono::SecondsFormat::Secs,true)});
            let category = &entry.course_key.as_ref().unwrap().id;
            let course = if category == "統合試験 A" {
                COURSE
            } else {
                OLD_COURSE
            };
            row["properties"]["授業"]["relation"] = json!([{"id":course}]);
            row
        })
        .collect()
}

#[test]
fn live_moodle_plan_scopes_courses_and_handles_independent_deltas_and_repeats() {
    let input = input_fixture();
    let entries = mappings_fixture();
    let mappings = MoodlePreviewMappings::new(&entries).unwrap();
    let target = target_fixture();
    let matching = stored_rows(&input);
    let mut partial = matching[1..].to_vec();
    partial[0]["properties"]["タイトル（改名済）"]["title"] = json!([text("Old title")]);
    partial[1]["properties"]["期限"]["date"] = json!({"start":"2026-11-01T00:00:00Z"});
    partial.push(task(MANUAL, None, "Manual record"));
    let mut script = scoped_snapshot(partial);
    script.extend(scoped_snapshot(matching.clone()));
    let (client, server) = mock(script);

    let coverage = mappings.coverage(&input).unwrap();
    assert_eq!(
        (
            coverage.candidates,
            coverage.courses,
            coverage.timed_deadlines
        ),
        (4, 2, 4)
    );
    assert!(coverage.ready());
    let dir = state::test_state_dir();
    let path = dir.path().join("state.sqlite3");
    state::StateStore::initialize(&path, A).unwrap();
    {
        let state = state::StateStore::for_apply(&path, A).unwrap();
        for (key, row) in input.assignments().keys().zip(&matching).skip(1) {
            state.confirm(key, row["id"].as_str().unwrap()).unwrap();
        }
    }
    let saved = std::fs::read(&path).unwrap();
    let state = || state::StateStore::read(&path, A).unwrap().unwrap();
    let report = client
        .preview_moodle_integration(&target, &input, &mappings, state())
        .unwrap();
    let counts = report.counts_for(&input);
    assert_eq!(
        counts,
        DryRunCounts {
            create: 1,
            update: 2,
            unchanged: 1,
            ..Default::default()
        }
    );
    let patches: Vec<_> = report
        .requests()
        .iter()
        .filter(|r| r.method() == Method::PATCH)
        .collect();
    assert_eq!(patches.len(), 2);
    for property in ["title", "due"] {
        assert!(patches.iter().any(|patch| {
            let fields = patch.body()["properties"].as_object().unwrap();
            fields.len() == 1 && fields.contains_key(property)
        }));
    }
    let repeated = client
        .preview_moodle_integration(&target, &input, &mappings, state())
        .unwrap();
    assert_eq!(
        repeated.counts_for(&input),
        DryRunCounts {
            unchanged: 4,
            ..Default::default()
        }
    );
    assert!(repeated.requests().is_empty());
    assert_eq!(std::fs::read(path).unwrap(), saved);
    server.join().unwrap();
}

#[test]
fn live_moodle_plan_reports_unmapped_courses_without_sending_notion_requests() {
    let mut input = input_fixture();
    let entries = [mapping("統合試験 A", COURSE)];
    let mappings = MoodlePreviewMappings::new(&entries).unwrap();
    let coverage = mappings.coverage(&input).unwrap();
    assert_eq!(
        (
            coverage.unmapped_courses,
            coverage.unmapped_candidates,
            coverage.unclassified_candidates
        ),
        (1, 2, 0)
    );
    let (client, server) = mock(vec![]);
    assert!(matches!(
        client.preview_moodle_integration(&target_fixture(), &input, &mappings, Default::default()),
        Err(NotionError::Configuration)
    ));
    server.join().unwrap();
    input
        .assignments
        .values_mut()
        .find(|entry| entry.course_key.as_ref().unwrap().id == "統合試験 A")
        .unwrap()
        .course_key = None;
    let entries = mappings_fixture();
    let mappings = MoodlePreviewMappings::new(&entries).unwrap();
    let coverage = mappings.coverage(&input).unwrap();
    assert_eq!(
        (
            coverage.unmapped_courses,
            coverage.unmapped_candidates,
            coverage.unclassified_candidates
        ),
        (0, 0, 1)
    );
    assert!(!coverage.ready());
}

#[test]
fn live_moodle_plan_rejects_duplicate_or_trial_only_mappings() {
    assert!(
        MoodlePreviewMappings::new(&[
            mapping("統合試験 A", COURSE),
            mapping("統合試験 A", OLD_COURSE)
        ])
        .is_err()
    );
    assert!(
        MoodlePreviewMappings::new(&[
            mapping("統合試験 A", COURSE),
            mapping("統合試験 B", &COURSE.replace('-', ""))
        ])
        .is_err()
    );
    assert!(MoodlePreviewMappings::new(&[mapping("照合用の授業", COURSE)]).is_err());
    assert!(
        MoodlePreviewMappings::new(&[mapping(
            "統合試験 A",
            "41e85934-7e84-81c4-8ae6-ec4fcaea84d0"
        )])
        .is_err()
    );
}

#[test]
fn live_moodle_plan_never_fetches_a_mapped_page_outside_the_verified_courses() {
    let input = input_fixture();
    for unused_mapping in [false, true] {
        let mut entries = Vec::from(mappings_fixture());
        let mut pages = vec![page(COURSE, C, C_DB, json!({}))];
        if unused_mapping {
            entries.push(mapping("今回未取得の合成授業", MANUAL));
            pages.push(page(OLD_COURSE, C, C_DB, json!({})));
        }
        let mappings = MoodlePreviewMappings::new(&entries).unwrap();
        assert_eq!(
            mappings.coverage(&input).unwrap().unused_mappings,
            usize::from(unused_mapping)
        );
        let mut script = vec![bot(READ_BOT)];
        script.extend(snapshot(vec![]).into_iter().take(2));
        script.push(course_query(pages, None));
        let (client, server) = mock(script);
        assert!(matches!(
            client.preview_moodle_integration(
                &target_fixture(),
                &input,
                &mappings,
                Default::default()
            ),
            Err(NotionError::CourseMappingOutsideTarget)
        ));
        assert_eq!(client.read_counts(), (4, 0));
        server.join().unwrap();
    }
}

#[test]
fn live_moodle_plan_rejects_duplicate_moodle_uids_and_conflicting_notion_rows() {
    let error = acquire(Scenario::MoodleDuplicate).unwrap_err();
    assert_eq!(error.diagnostic.code(), "moodle_uid_duplicate");
    let input = input_fixture();
    let entries = mappings_fixture();
    let mappings = MoodlePreviewMappings::new(&entries).unwrap();
    let mut rows = stored_rows(&input);
    let mut duplicate = rows[0].clone();
    duplicate["id"] = json!(MANUAL);
    rows.push(duplicate);
    let (client, server) = mock(scoped_snapshot(rows));
    let report = client
        .preview_moodle_integration(&target_fixture(), &input, &mappings, Default::default())
        .unwrap();
    assert_eq!(
        report.counts_for(&input),
        DryRunCounts {
            conflict: 1,
            unchanged: 3,
            ..Default::default()
        }
    );
    assert!(report.requests().is_empty());
    server.join().unwrap();
}

#[test]
fn live_moodle_plan_discards_snapshots_after_a_notion_read_failure() {
    let input = input_fixture();
    let entries = mappings_fixture();
    let mappings = MoodlePreviewMappings::new(&entries).unwrap();
    let mut script = scoped_snapshot(vec![]);
    script.pop();
    script.push(query(stored_rows(&input), Some("next")));
    script.push(
        Exchange::error("POST", format!("/v1/data_sources/{A}/query"), 403, "")
            .check(|_, body| assert_eq!(body["start_cursor"], "next")),
    );
    let (client, server) = mock(script);
    assert!(matches!(
        client.preview_moodle_integration(&target_fixture(), &input, &mappings, Default::default()),
        Err(NotionError::Forbidden)
    ));
    server.join().unwrap();
}
