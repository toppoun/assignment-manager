use super::*;
use crate::notion::trial::{MOODLE_TRIAL_COURSES, MOODLE_VERIFICATION_COURSES as COURSE_SOURCE};

const CATEGORIES: [&str; 6] = [
    "Course A", "Course B", "Course C", "D/E-秋", "Course E", "Course F",
];
const PAGE_IDS: [&str; 6] = [PAGE_A, PAGE_B, PAGE_C, PAGE_D, COURSE, OLD_COURSE];

fn trial_config() -> NotionTarget {
    let mut config = target();
    config.assignments_data_source_id = "41e85934-7e84-817e-b1d2-000b68726503".into();
    config.courses_data_source_id = COURSE_SOURCE.into();
    config
}

fn trial() -> CourseCreationTrial {
    CourseCreationTrial::for_mock(trial_config(), CATEGORIES)
}

fn page(id: &str, name: &str) -> Value {
    json!({
        "object":"page", "id":id, "in_trash":false,
        "last_edited_time":"2026-01-01T00:00:00.000Z",
        "parent":{"type":"data_source_id", "database_id":C_DB, "data_source_id":COURSE_SOURCE},
        "properties":{"授業名":{"id":"title", "type":"title",
            "title":[{"type":"text", "text":{"content":name}}]}},
    })
}

fn baseline() -> Vec<Value> {
    MOODLE_TRIAL_COURSES
        .iter()
        .enumerate()
        .map(|(index, (_, id))| page(id, &format!("Existing trial course {}", index + 1)))
        .collect()
}

fn bot_exchange(id: &str) -> Exchange {
    Exchange::json(
        "GET",
        "/v1/users/me",
        json!({
            "object":"user", "id": id, "type":"bot",
            "bot":{"owner":{"type":"workspace", "workspace":true}}
        }),
    )
}

fn course_schema() -> Exchange {
    Exchange::json(
        "GET",
        format!("/v1/data_sources/{COURSE_SOURCE}"),
        source(
            COURSE_SOURCE,
            C_DB,
            json!({"授業名":{"id":"title", "type":"title", "title":{}}}),
        ),
    )
}

fn course_query(rows: &[Value], more: bool) -> Exchange {
    Exchange::json("POST", format!("/v1/data_sources/{COURSE_SOURCE}/query"), json!({
        "object":"list", "results":rows, "has_more":more, "next_cursor": if more { Some("cursor") } else { None }
    })).check(|target, body| {
        assert!(target.ends_with("?filter_properties=title"));
        assert_eq!(*body, json!({"page_size":100}));
    })
}

fn inspection(trial: &CourseCreationTrial, role: TrialRole, rows: &[Value]) -> Vec<Exchange> {
    vec![
        bot_exchange(trial.target().bot_id(role)),
        course_schema(),
        course_query(rows, false),
    ]
}

fn create(index: usize) -> Exchange {
    Exchange::json("POST", "/v1/pages", page(PAGE_IDS[index], CATEGORIES[index])).check(move |target, body| {
        assert_eq!(target, "/v1/pages");
        assert_eq!(*body, json!({
            "parent":{"type":"data_source_id", "data_source_id":COURSE_SOURCE},
            "properties":{"title":{"title":[{"type":"text", "text":{"content":CATEGORIES[index]}}]}},
        }));
    })
}

#[test]
fn course_trial_creates_exact_titles_only_and_reads_back_six_distinct_mappings() {
    let trial = trial();
    let directory = tempfile::tempdir().unwrap();
    let journal = directory.path().join("journal");
    let mut rows = baseline();
    let mut script = inspection(&trial, TrialRole::Write, &rows);
    for index in 0..CATEGORIES.len() {
        script.push(course_query(&rows, false));
        script.push(create(index));
        rows.push(page(PAGE_IDS[index], CATEGORIES[index]));
    }
    script.extend(inspection(&trial, TrialRole::Read, &rows));
    rows[0]["last_edited_time"] = json!("2026-01-02T00:00:00.000Z");
    script.extend(inspection(&trial, TrialRole::Read, &rows));
    let (client, server) = mock(script);
    assert_eq!(trial.create(&client, &journal, || true), Ok(true));
    let snapshot = trial.inspect(&client, TrialRole::Read).unwrap();
    assert!(snapshot.existing_ordinals().eq(1..=6));
    let mappings = trial.verified_mappings(&snapshot, &journal).unwrap();
    for (index, mapping) in mappings.iter().enumerate() {
        assert_eq!(mapping.course_key.id, CATEGORIES[index]);
        assert_eq!(mapping.course_key.origin, crate::import::OriginKind::Moodle);
        assert_eq!(mapping.course_key.scope, "default.course-related");
        assert_eq!(mapping.page_id, PAGE_IDS[index]);
    }
    let changed = trial.inspect(&client, TrialRole::Read).unwrap();
    assert_eq!(
        trial.verified_mappings(&changed, &journal).err(),
        Some(CourseTrialError::PlanChanged),
    );
    server.join().unwrap();
}

#[test]
fn course_trial_rejects_wrong_bot_schema_or_incomplete_inventory() {
    let trial = trial();
    let read_bot = || bot_exchange(trial.target().bot_id(TrialRole::Read));
    let cases = [
        (vec![bot_exchange(PAGE_A)], NotionError::CredentialMismatch),
        (
            vec![
                read_bot(),
                Exchange::json(
                    "GET",
                    format!("/v1/data_sources/{COURSE_SOURCE}"),
                    source(
                        COURSE_SOURCE,
                        C_DB,
                        json!({"別名":{"type":"title", "id":"title", "title":{}}}),
                    ),
                ),
            ],
            NotionError::SchemaMismatch,
        ),
        (
            vec![read_bot(), course_schema(), course_query(&baseline(), true)],
            NotionError::IncompleteSnapshot,
        ),
    ];
    for (script, error) in cases {
        let (client, server) = mock(script);
        assert_eq!(trial.inspect(&client, TrialRole::Read).err(), Some(error));
        server.join().unwrap();
    }
}

#[test]
fn course_trial_stops_before_confirmation_for_existing_or_different_spelling() {
    let trial = trial();
    for name in [CATEGORIES[0], "course A", "Course A "] {
        let mut rows = baseline();
        rows.push(page(PAGE_IDS[0], name));
        let (client, server) = mock(inspection(&trial, TrialRole::Write, &rows));
        let directory = tempfile::tempdir().unwrap();
        let journal = directory.path().join("journal");
        assert_eq!(
            trial.create(&client, &journal, || panic!(
                "must stop before confirmation"
            )),
            Err(CourseTrialError::Notion(NotionError::Conflict)),
        );
        assert!(!journal.exists());
        server.join().unwrap();
    }
}

#[test]
fn course_trial_cancels_cleanly_and_rejects_changes_after_confirmation() {
    let trial = trial();
    let rows = baseline();
    let directory = tempfile::tempdir().unwrap();
    let journal = directory.path().join("journal");
    let (client, server) = mock(inspection(&trial, TrialRole::Write, &rows));
    assert_eq!(trial.create(&client, &journal, || false), Ok(false));
    assert!(!journal.exists());
    server.join().unwrap();

    let mut script = inspection(&trial, TrialRole::Write, &rows);
    let mut concurrent_rows = rows;
    concurrent_rows.push(page(PAGE_IDS[0], CATEGORIES[0]));
    script.push(course_query(&concurrent_rows, false));
    let (client, server) = mock(script);
    assert_eq!(
        trial.create(&client, &journal, || true),
        Err(CourseTrialError::PlanChanged)
    );
    let journal_text = std::fs::read_to_string(&journal).unwrap();
    assert!(journal_text.starts_with("baseline "));
    assert_eq!(journal_text.lines().count(), 1);
    server.join().unwrap();
}

#[test]
fn course_trial_uses_read_only_recovery_after_partial_success_or_lost_response() {
    let trial = trial();
    let directory = tempfile::tempdir().unwrap();
    let journal = directory.path().join("journal");
    let mut rows = baseline();
    let mut script = inspection(&trial, TrialRole::Write, &rows);
    script.push(course_query(&rows, false));
    script.push(create(0));
    rows.push(page(PAGE_IDS[0], CATEGORIES[0]));
    script.push(course_query(&rows, false));
    script.push(Exchange::drop_response("POST", "/v1/pages"));
    // The server committed the second page without returning a usable response.
    rows.push(page(PAGE_IDS[1], CATEGORIES[1]));
    script.extend(inspection(&trial, TrialRole::Read, &rows));
    let (client, server) = mock(script);
    assert_eq!(
        trial.create(&client, &journal, || true),
        Err(CourseTrialError::ResultUnconfirmed(NotionError::Transport)),
    );
    let journal_text = std::fs::read_to_string(&journal).unwrap();
    let (baseline, advances) = journal_text.split_once('\n').unwrap();
    assert!(baseline.starts_with("baseline "));
    assert_eq!(
        advances,
        format!("attempt 1\nconfirmed 1 {}\nattempt 2\n", PAGE_IDS[0]),
    );
    assert_eq!(
        trial.create(&client, &journal, || panic!(
            "a second run must stop locally"
        )),
        Err(CourseTrialError::AlreadyStarted),
    );
    let snapshot = trial.inspect(&client, TrialRole::Read).unwrap();
    assert!(snapshot.existing_ordinals().eq([1, 2]));
    assert_eq!(
        trial.verified_mappings(&snapshot, &journal).err(),
        Some(CourseTrialError::Notion(NotionError::IncompleteSnapshot)),
    );
    server.join().unwrap();
}

#[test]
fn course_trial_refuses_duplicate_titles_wrong_membership_or_changed_confirmed_page() {
    let trial = trial();
    let mut rows = baseline();
    rows.push(page(PAGE_IDS[0], CATEGORIES[0]));
    rows.push(page(PAGE_IDS[1], CATEGORIES[0]));
    let (client, server) = mock(inspection(&trial, TrialRole::Read, &rows));
    assert_eq!(
        trial.inspect(&client, TrialRole::Read).err(),
        Some(NotionError::Conflict)
    );
    server.join().unwrap();

    rows[3]["parent"]["data_source_id"] = json!(C);
    let (client, server) = mock(inspection(&trial, TrialRole::Read, &rows));
    assert_eq!(
        trial.inspect(&client, TrialRole::Read).err(),
        Some(NotionError::IncompleteSnapshot)
    );
    server.join().unwrap();

    rows.truncate(2);
    rows.extend(
        PAGE_IDS
            .iter()
            .zip(CATEGORIES)
            .map(|(id, name)| page(id, name)),
    );
    let (client, server) = mock(inspection(&trial, TrialRole::Read, &rows));
    let directory = tempfile::tempdir().unwrap();
    let journal = directory.path().join("journal");
    let snapshot = trial.inspect(&client, TrialRole::Read).unwrap();
    std::fs::write(
        &journal,
        format!(
            "baseline {}\nattempt 1\nconfirmed 1 {}\n",
            snapshot.baseline_revision(),
            PAGE_IDS[1]
        ),
    )
    .unwrap();
    assert_eq!(
        trial.verified_mappings(&snapshot, &journal).err(),
        Some(CourseTrialError::PlanChanged),
    );
    server.join().unwrap();
}
