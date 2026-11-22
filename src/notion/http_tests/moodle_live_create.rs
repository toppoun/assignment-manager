use std::path::Path;

use super::{trial_tests::bot, *};
use crate::{
    import::{ExternalRecordKey, OriginKind},
    notion::state::{BindingStatus, StateError, StateStore},
};
use chrono::NaiveDate;

const ASSIGNMENTS: &str = "41e85934-7e84-817e-b1d2-000b68726503";
const COURSES: &str = "41e85934-7e84-81df-902b-000bbece9bf7";
const NEW_PAGE: &str = "00000000-0000-4000-8000-000000000020";
const COURSE_IDS: [&str; 6] = [COURSE, OLD_COURSE, PAGE_A, PAGE_B, PAGE_C, PAGE_D];

fn real_target() -> MoodleLiveTarget {
    let mut config = target();
    config.assignments_data_source_id = ASSIGNMENTS.into();
    config.courses_data_source_id = COURSES.into();
    MoodleLiveTarget::new(config).unwrap()
}

fn data(count: usize) -> (SyncInput, Vec<CourseMapping>) {
    let entries: Vec<_> = COURSE_IDS
        .iter()
        .enumerate()
        .map(|(index, page_id)| CourseMapping {
            course_key: ExternalRecordKey {
                origin: OriginKind::Moodle,
                scope: MOODLE_INTEGRATION_SCOPE.into(),
                id: format!("Synthetic course {index}"),
            },
            page_id: (*page_id).into(),
        })
        .collect();
    let mut input = SyncInput::default();
    for index in 0..count {
        let mut row = candidate(
            &format!("live-{index}@example.invalid"),
            &format!("Synthetic task {index}"),
        );
        row.external_key.scope = MOODLE_INTEGRATION_SCOPE.into();
        row.deadline = Some(Deadline::DateOnly(
            NaiveDate::from_ymd_opt(2026, 11, 30).unwrap(),
        ));
        input
            .insert(
                &row,
                UniversitySource::Moodle,
                Some(&entries[index % entries.len()].course_key),
            )
            .unwrap();
    }
    (input, entries)
}

fn state_directory() -> tempfile::TempDir {
    let directory = state::test_state_dir();
    let path = directory.path().join("state.sqlite3");
    StateStore::initialize(&path, ASSIGNMENTS).unwrap();
    let original = std::fs::read(&path).unwrap();
    StateStore::initialize(&path, ASSIGNMENTS).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);
    directory
}

fn existing(id: &str, key: Option<&SyncKey>, name: &str, course: &str) -> Value {
    let mut row = task(id, key, name);
    row["parent"]["data_source_id"] = json!(ASSIGNMENTS);
    row["properties"]["期限"]["date"] = json!({"start":"2026-11-30"});
    row["properties"]["授業"]["relation"] = json!([{"id":course}]);
    row
}

fn full_plan(rows: Vec<Value>, entries: &[CourseMapping]) -> Vec<Exchange> {
    let mut assignments = schema_json();
    assignments["id"] = json!(ASSIGNMENTS);
    assignments["properties"]["授業"]["relation"]["data_source_id"] = json!(COURSES);
    let properties = assignments["properties"].as_object_mut().unwrap();
    for (id, label, kind) in super::super::trial::MANAGED_PROPERTIES {
        properties.insert(
            label.into(),
            json!({"id":format!("managed_{id}"),"type":kind, (kind):{}}),
        );
    }
    let courses = entries
        .iter()
        .map(|entry| {
            page(
                &entry.page_id,
                COURSES,
                C_DB,
                json!({
                    "授業名":{"id":"title","type":"title","title":[text(&entry.course_key.id)]}
                }),
            )
        })
        .collect();
    vec![
        Exchange::json(
            "GET",
            format!("/v1/data_sources/{ASSIGNMENTS}"),
            assignments,
        ),
        Exchange::json(
            "GET",
            format!("/v1/data_sources/{COURSES}"),
            source(COURSES, C_DB, json!({})),
        ),
        Exchange::json(
            "POST",
            format!("/v1/data_sources/{COURSES}/query"),
            list(courses, None),
        ),
        assignment_rows(rows),
    ]
}

fn assignment_rows(rows: Vec<Value>) -> Exchange {
    Exchange::json(
        "POST",
        format!("/v1/data_sources/{ASSIGNMENTS}/query"),
        list(rows, None),
    )
}

fn exact_key(rows: Vec<Value>, key: &SyncKey) -> Exchange {
    let key = key.as_str().to_owned();
    assignment_rows(rows).check(move |_, body| {
        assert_eq!(
            body["filter"],
            json!({"property":"sync","rich_text":{"equals":key}})
        );
    })
}

fn postcheck(
    target: &MoodleLiveTarget,
    rows: Vec<Value>,
    entries: &[CourseMapping],
    course: &CourseMapping,
) -> Vec<Exchange> {
    let mut script = vec![bot(target.trial().bot_id(TrialRole::Read))];
    script.extend(full_plan(vec![], entries).into_iter().take(2));
    script.push(Exchange::json(
        "GET",
        format!("/v1/pages/{}", course.page_id),
        page(
            &course.page_id,
            COURSES,
            C_DB,
            json!({
                "授業名":{"id":"title","type":"title","title":[text(&course.course_key.id)]}
            }),
        ),
    ));
    script.push(assignment_rows(rows));
    script
}

fn assert_intent(path: &Path, key: &SyncKey, status: BindingStatus, id: Option<&str>) {
    let snapshot = StateStore::read(&path.join("state.sqlite3"), ASSIGNMENTS)
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.bindings.len(), 1);
    assert_eq!(snapshot.bindings[key].status, status);
    assert_eq!(snapshot.bindings[key].page_id.as_deref(), id);
}

#[test]
fn live_create_evaluates_twelve_but_posts_only_the_selected_task_and_holds_the_rest() {
    let (mut input, entries) = data(12);
    let target = real_target();
    let mappings = MoodlePreviewMappings::new(&entries).unwrap();
    let directory = state_directory();
    let title = "Synthetic task 7";
    let (selected, _) = input
        .assignments()
        .iter()
        .find(|(_, row)| row.title == title)
        .unwrap();
    let selected = selected.clone();
    let course = entries[1].page_id.clone();
    let manual = existing(MANUAL, None, "Protected manual task", &course);
    let older_key = key("older-synthetic");
    let older = existing(
        "00000000-0000-4000-8000-000000000021",
        Some(&older_key),
        "Previous synthetic task",
        &course,
    );
    let prior = vec![manual, older];
    let created = existing(NEW_PAGE, Some(&selected), title, &course);
    let mut script = vec![bot(target.trial().bot_id(TrialRole::Write))];
    script.extend(full_plan(prior.clone(), &entries));
    script.extend(full_plan(prior.clone(), &entries));
    script.push(exact_key(vec![], &selected));
    let expected_key = selected.clone();
    let expected_course = course.clone();
    let sqlite_directory = directory.path().to_owned();
    script.push(
        Exchange::json("POST", "/v1/pages", created.clone()).check(move |_, body| {
            assert_intent(
                &sqlite_directory,
                &expected_key,
                BindingStatus::CreateIntent,
                None,
            );
            assert_eq!(
                *body,
                json!({
                    "parent":{"type":"data_source_id","data_source_id":ASSIGNMENTS},
                    "properties":{
                        "title":{"title":[text(title)]},
                        "sync":{"rich_text":[text(expected_key.as_str())]},
                        "due":{"date":{"start":"2026-11-30"}},
                        "orig":{"select":{"id":"option-md"}},
                        "R%3Ael":{"relation":[{"id":expected_course}]}
                    }
                })
            );
        }),
    );
    let mut after = prior;
    after.push(created);
    script.extend(postcheck(&target, after.clone(), &entries, &entries[1]));
    let mut changed = after.clone();
    changed[0]["last_edited_time"] = json!("2026-11-22T04:00:00Z");
    script.extend(postcheck(&target, changed, &entries, &entries[1]));
    for _ in 0..2 {
        script.push(bot(target.trial().bot_id(TrialRole::Write)));
        script.extend(full_plan(after.clone(), &entries));
    }
    let (client, server) = mock(script);
    let result = client
        .create_one_live_moodle(
            &target,
            &input,
            &mappings,
            directory.path(),
            |choices| {
                assert_eq!(choices.candidate_count, 12);
                assert_eq!(choices.eligible.len(), 12);
                assert_eq!(choices.existing_rows, 2);
                choices.eligible.iter().position(|row| row.title == title)
            },
            |row, held| {
                assert_eq!((row.title.as_str(), held), (title, 11));
                true
            },
        )
        .unwrap()
        .unwrap();
    let report = &result.report;
    assert_eq!(
        (report.counts.create, report.counts.update, report.pending),
        (1, 0, 0)
    );
    assert_intent(
        directory.path(),
        &selected,
        BindingStatus::Linked,
        Some(NEW_PAGE),
    );
    assert_eq!(client.verify_one_live_moodle(&target, &result), Ok((2, 3)));
    assert_eq!(
        client.verify_one_live_moodle(&target, &result),
        Err(NotionError::Conflict)
    );

    assert!(
        client
            .create_one_live_moodle(
                &target,
                &input,
                &mappings,
                directory.path(),
                |choices| {
                    assert_eq!(choices.eligible.len(), 11);
                    assert!(choices.eligible.iter().all(|row| row.title != title));
                    None
                },
                |_, _| panic!("cancelled selection must not reach confirmation"),
            )
            .unwrap()
            .is_none()
    );
    input.assignments.retain(|key, _| key == &selected);
    let report = client
        .create_one_live_moodle(
            &target,
            &input,
            &mappings,
            directory.path(),
            |_| panic!("the only current candidate already exists"),
            |_, _| panic!("there is nothing to create"),
        )
        .unwrap()
        .unwrap();
    assert_eq!(report.report.counts.create, 0);
    assert_intent(
        directory.path(),
        &selected,
        BindingStatus::Linked,
        Some(NEW_PAGE),
    );
    server.join().unwrap();
}

#[test]
fn live_create_never_writes_on_changed_key_unreadable_notion_or_unmapped_input() {
    let (mut input, entries) = data(2);
    let target = real_target();
    let mappings = MoodlePreviewMappings::new(&entries).unwrap();
    let directory = state_directory();
    let key = input.assignments().first_key_value().unwrap().0.clone();
    let verify_writer = || bot(target.trial().bot_id(TrialRole::Write));
    let full = || full_plan(vec![], &entries);
    let mut script = vec![verify_writer()];
    script.extend(full());
    script.extend(full());
    script.push(exact_key(
        vec![existing(NEW_PAGE, Some(&key), "Concurrent task", COURSE)],
        &key,
    ));
    script.push(verify_writer());
    let mut incomplete = full();
    incomplete.pop();
    incomplete.push(Exchange::error(
        "POST",
        format!("/v1/data_sources/{ASSIGNMENTS}/query"),
        403,
        "",
    ));
    script.extend(incomplete);
    script.push(verify_writer());
    let (_, mut mismapped) = data(0);
    mismapped[0].course_key.id.push('!');
    script.extend(full_plan(vec![], &mismapped).into_iter().take(3));
    let (client, server) = mock(script);
    let create = |input: &SyncInput| {
        client.create_one_live_moodle(
            &target,
            input,
            &mappings,
            directory.path(),
            |_| Some(0),
            |_, _| true,
        )
    };
    assert_eq!(create(&input), Err(ApplyError::PlanChanged));
    assert_eq!(
        create(&input),
        Err(ApplyError::Notion(NotionError::Forbidden))
    );
    assert_eq!(
        create(&input),
        Err(ApplyError::Notion(NotionError::CourseMappingOutsideTarget))
    );
    input
        .assignments
        .values_mut()
        .next()
        .unwrap()
        .course_key
        .as_mut()
        .unwrap()
        .id
        .push('!');
    assert_eq!(create(&input), Err(ApplyError::CreateBlocked));
    let saved = StateStore::read(&directory.path().join("state.sqlite3"), ASSIGNMENTS)
        .unwrap()
        .unwrap();
    assert!(saved.bindings.is_empty());
    server.join().unwrap();
}

#[test]
fn live_create_unknown_response_and_failed_sqlite_confirmation_reconcile_without_resending() {
    for lost_response in [true, false] {
        let (input, entries) = data(1);
        let key = input.assignments().first_key_value().unwrap().0.clone();
        let target = real_target();
        let mappings = MoodlePreviewMappings::new(&entries).unwrap();
        let directory = state_directory();
        let database = directory.path().join("state.sqlite3");
        if !lost_response {
            rusqlite::Connection::open(&database).unwrap().execute_batch(
                "CREATE TRIGGER mock_failed_confirmation BEFORE UPDATE ON bindings
                WHEN NEW.status='linked' BEGIN SELECT RAISE(FAIL, 'mock failed confirmation'); END;",
            ).unwrap();
        }
        let mut script = vec![bot(target.trial().bot_id(TrialRole::Write))];
        script.extend(full_plan(vec![], &entries));
        script.extend(full_plan(vec![], &entries));
        script.push(exact_key(vec![], &key));
        let created = existing(NEW_PAGE, Some(&key), "Synthetic task 0", COURSE);
        script.push(if lost_response {
            Exchange::drop_response("POST", "/v1/pages")
        } else {
            Exchange::json("POST", "/v1/pages", created.clone())
        });
        script.push(bot(target.trial().bot_id(TrialRole::Read)));
        script.extend(full_plan(vec![], &entries));
        script.push(bot(target.trial().bot_id(TrialRole::Read)));
        script.extend(full_plan(vec![created], &entries));
        let (client, server) = mock(script);
        let create = || {
            client.create_one_live_moodle(
                &target,
                &input,
                &mappings,
                directory.path(),
                |_| Some(0),
                |_, _| true,
            )
        };
        let result = create();
        let pending = if lost_response {
            let report = result.unwrap().unwrap().report;
            assert_eq!(
                (report.counts.create, report.pending, report.error),
                (0, 1, Some(NotionError::Transport))
            );
            BindingStatus::Unknown
        } else {
            assert_eq!(result, Err(ApplyError::State(StateError::Database)));
            rusqlite::Connection::open(&database)
                .unwrap()
                .execute_batch("DROP TRIGGER mock_failed_confirmation;")
                .unwrap();
            BindingStatus::CreateIntent
        };
        assert_intent(directory.path(), &key, pending, None);
        assert_eq!(create(), Err(ApplyError::CreateBlocked));
        let reconcile =
            || client.reconcile_one_live_moodle(&target, &input, &mappings, directory.path());
        assert_eq!(reconcile(), Err(ApplyError::CreateBlocked));
        assert_intent(directory.path(), &key, pending, None);
        assert_eq!(reconcile(), Ok(()));
        assert_intent(
            directory.path(),
            &key,
            BindingStatus::Linked,
            Some(NEW_PAGE),
        );
        server.join().unwrap();
    }
}
