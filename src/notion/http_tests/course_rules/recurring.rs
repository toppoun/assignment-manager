use std::path::Path;

use super::*;
use crate::notion::{
    ChangeLimits, PlanHash, RecurringInputs, RecurringMode, RecurringPolicy,
    state::{BindingStatus, RunIssue, RunOutcome, StateStore, TargetLock, test_state_dir},
    trial::{PRODUCTION_ASSIGNMENTS, PRODUCTION_COURSES, PRODUCTION_RULES},
};

const GROUP: &str = "recurring-group";
const CATEGORY: &str = "Recurring Moodle category";

fn production_target() -> NotionTarget {
    let mut target = rules_target();
    target.assignments_data_source_id = PRODUCTION_ASSIGNMENTS.into();
    target.courses_data_source_id = PRODUCTION_COURSES.into();
    target.course_rules.as_mut().unwrap().data_source_id = PRODUCTION_RULES.into();
    target
}

fn local_mock(mut script: Vec<Exchange>) -> (NotionClient, MockServer) {
    for exchange in &mut script {
        for (from, to) in [
            (A, PRODUCTION_ASSIGNMENTS),
            (C, PRODUCTION_COURSES),
            (RULES, PRODUCTION_RULES),
        ] {
            exchange.path = exchange.path.replace(from, to);
            exchange.body = exchange.body.replace(from, to);
        }
    }
    mock(script)
}

fn policy(moodle_limit: usize) -> RecurringPolicy {
    policy_for(
        moodle_limit,
        Some(ChangeLimits {
            max_creates: 3,
            max_updates: 3,
        }),
    )
}

fn policy_for(moodle_limit: usize, webclass: Option<ChangeLimits>) -> RecurringPolicy {
    RecurringPolicy::new(
        &fake_id(0x500),
        Some((
            ChangeLimits {
                max_creates: moodle_limit,
                max_updates: 3,
            },
            [
                "moodle.fixture.invalid".into(),
                "/calendar".into(),
                FEED.into(),
            ],
        )),
        webclass,
    )
    .unwrap()
}

fn writer_checks() -> Vec<Exchange> {
    let mut exchanges = vec![trial_tests::bot(&fake_id(0x500))];
    for _ in 0..2 {
        exchanges.extend(schemas().into_iter().take(2));
    }
    exchanges
}

fn state_directory() -> tempfile::TempDir {
    let directory = test_state_dir();
    StateStore::initialize(
        &directory.path().join("state.sqlite3"),
        PRODUCTION_ASSIGNMENTS,
    )
    .unwrap();
    directory
}

fn approve(directory: &Path, policy: &RecurringPolicy) {
    let (reader, reads) = local_mock(schemas());
    let (writer, writes) = local_mock(writer_checks());
    reader
        .approve_production_recurring(&writer, &production_target(), policy, directory)
        .unwrap();
    reads.join().unwrap();
    writes.join().unwrap();
}

fn rules_snapshot() -> Vec<Exchange> {
    rules_snapshot_with_course("人工知能", 0x101)
}

fn rules_snapshot_with_course(moodle_title: &str, hoppii_course: usize) -> Vec<Exchange> {
    let mut script = schemas();
    script.extend([
        rules_query(
            vec![
                rule(0, "moodle-option", FEED, CATEGORY, &fake_id(0x100)),
                rule(
                    1,
                    "hoppii-option",
                    DEFAULT_COURSE_SOURCE_SCOPE,
                    GROUP,
                    &fake_id(hoppii_course),
                ),
            ],
            None,
        ),
        courses_query(vec![named_course(0, TERM, moodle_title), course(1, TERM)]),
    ]);
    script
}

#[test]
fn title_migration_keeps_page_key_and_protected_fields_through_lost_patch_and_rename() {
    let directory = state_directory();
    let policy = policy_for(0, None);
    approve(directory.path(), &policy);
    let key = key("moodle-0");
    let before = managed(UniversitySource::Moodle, PAGE_A, Some("Moodle task 0"));
    let mut after = before.clone();
    after["properties"]["タイトル（改名済）"]["title"] = json!([text("【人工知能】Moodle task 0")]);
    let manual = task(MANUAL, None, "【人工知能】Moodle task 0");
    let rows = || vec![before.clone(), manual.clone()];
    let (reader, preview_mock) = local_mock(reader_script(rows()));
    let preview = reader.run_production_recurring(
        &production_target(),
        &policy,
        directory.path(),
        RecurringMode::Preview,
        || panic!("preview does not load the writer"),
        |_| successful_inputs(),
    );
    assert_eq!(preview.exit_code(), 0);
    let preview = json!(preview);
    assert_eq!(preview["moodle"]["planned"]["update"], 1);
    assert_eq!(preview["moodle"]["planned"]["create"], 0);
    assert!(!preview.to_string().contains("Moodle task 0"));
    preview_mock.join().unwrap();

    let path = format!("/v1/pages/{PAGE_A}");
    let mut writes = writer_checks();
    writes.extend([
        Exchange::json("GET", &path, before.clone()),
        Exchange::drop_response("PATCH", &path).check(|_, body| {
            assert_eq!(
                *body,
                json!({"properties":{"title":{"title":[text("【人工知能】Moodle task 0")]}}})
            );
        }),
        Exchange::json("GET", &path, after.clone()),
    ]);
    let (exit, report) = apply(
        directory.path(),
        &policy,
        successful_inputs(),
        reader_script(rows()),
        Some(writes),
    );
    assert_eq!(exit, 0, "{report}");
    assert_eq!(report["moodle"]["planned"], preview["moodle"]["planned"]);
    assert_eq!(report["moodle"]["applied"]["update"], 1);
    let store = StateStore::for_apply(
        &directory.path().join("state.sqlite3"),
        PRODUCTION_ASSIGNMENTS,
    )
    .unwrap();
    assert_eq!(
        store.binding(&key).unwrap().unwrap().page_id.as_deref(),
        Some(PAGE_A)
    );
    let mut preserved = after.clone();
    preserved["properties"]["タイトル（改名済）"] =
        before["properties"]["タイトル（改名済）"].clone();
    assert_eq!(preserved, before);

    let (exit, report) = apply(
        directory.path(),
        &policy,
        successful_inputs(),
        reader_script(vec![after.clone(), manual.clone()]),
        Some(writer_checks()),
    );
    assert_eq!(exit, 0, "{report}");
    assert_eq!(report["moodle"]["applied"]["unchanged"], 1);
    let renamed_snapshot = || rules_snapshot_with_course("人工知能演習", 0x101);
    let mut reader = renamed_snapshot();
    reader.extend(renamed_snapshot());
    reader.push(query(vec![after.clone(), manual], None));
    let mut renamed = after.clone();
    renamed["properties"]["タイトル（改名済）"]["title"] =
        json!([text("【人工知能演習】Moodle task 0")]);
    let mut writes = writer_checks();
    writes.extend([
        Exchange::json("GET", &path, after),
        Exchange::json("PATCH", &path, renamed).check(|_, body| {
            assert_eq!(
                *body,
                json!({"properties":{"title":{"title":[text("【人工知能演習】Moodle task 0")]}}})
            );
        }),
    ]);
    let (exit, report) = apply(
        directory.path(),
        &policy,
        successful_inputs(),
        reader,
        Some(writes),
    );
    assert_eq!(exit, 0, "{report}");
    assert_eq!(report["moodle"]["applied"]["update"], 1);
    assert_eq!(
        store.binding(&key).unwrap().unwrap().page_id.as_deref(),
        Some(PAGE_A)
    );
}

#[test]
fn recurring_stops_when_the_formal_course_title_changes_during_acquisition() {
    let directory = state_directory();
    let policy = policy_for(3, None);
    approve(directory.path(), &policy);
    let mut reader = rules_snapshot();
    reader.extend(rules_snapshot_with_course("人工知能演習", 0x101));
    reader.push(query(vec![], None));
    let (exit, report) = apply(directory.path(), &policy, successful_inputs(), reader, None);
    assert_eq!(exit, 2, "{report}");
    assert_eq!(report["moodle"]["error"], "course_rules_changed");
}

#[test]
fn cross_lms_holds_legacy_current_and_possible_former_titles_in_the_same_course() {
    let directory = state_directory();
    let policy = policy_for(3, None);
    approve(directory.path(), &policy);
    for title in [
        "Moodle task 0",
        "【人工知能】Moodle task 0",
        "【改名前】Moodle task 0",
    ] {
        let mut old_web = managed(UniversitySource::WebClass, PAGE_B, Some(title));
        old_web["properties"]["授業"]["relation"] = json!([{"id":fake_id(0x100)}]);
        let (exit, report) = apply(
            directory.path(),
            &policy,
            successful_inputs(),
            reader_script(vec![old_web]),
            None,
        );
        assert_eq!(exit, 2, "{report}");
        assert_eq!(report["moodle"]["error"], "potential_duplicate");
    }
    let other_course = managed(
        UniversitySource::WebClass,
        PAGE_B,
        Some("【プログラミング2】Moodle task 0"),
    );
    let distinct_title = managed(
        UniversitySource::WebClass,
        PAGE_C,
        Some("【人工知能】【補足】Moodle task 0"),
    );
    let mut distinct_title = distinct_title;
    distinct_title["properties"]["同期キー"]["rich_text"] =
        json!([text(key("other-web").as_str())]);
    distinct_title["properties"]["授業"]["relation"] = json!([{"id":fake_id(0x100)}]);
    let mut writes = writer_checks();
    writes.extend([
        query(vec![], None),
        create(managed(UniversitySource::Moodle, PAGE_A, None)),
    ]);
    let (exit, report) = apply(
        directory.path(),
        &policy,
        successful_inputs(),
        reader_script(vec![other_course, distinct_title]),
        Some(writes),
    );
    assert_eq!(exit, 0, "{report}");
    assert_eq!(report["moodle"]["applied"]["create"], 1);
}

#[test]
fn cross_lms_holds_both_acquired_candidates_with_the_same_display_title() {
    let directory = state_directory();
    let policy = policy(3);
    approve(directory.path(), &policy);
    let snapshot = || rules_snapshot_with_course("人工知能", 0x100);
    let mut reader = snapshot();
    reader.extend(snapshot());
    reader.push(query(vec![], None));
    let mut inputs = successful_inputs();
    inputs
        .webclass
        .as_mut()
        .unwrap()
        .assignments
        .values_mut()
        .next()
        .unwrap()
        .title = "【人工知能】Moodle task 0".into();
    let (exit, report) = apply(directory.path(), &policy, inputs, reader, None);
    assert_eq!(exit, 2, "{report}");
    assert_eq!(report["moodle"]["error"], "potential_duplicate");
    assert_eq!(report["webclass"]["error"], "potential_duplicate");
}

#[test]
fn title_only_migration_stops_at_the_update_limit() {
    let directory = state_directory();
    let policy = RecurringPolicy::new(
        &fake_id(0x500),
        Some((
            ChangeLimits {
                max_creates: 5,
                max_updates: 0,
            },
            [
                "moodle.fixture.invalid".into(),
                "/calendar".into(),
                FEED.into(),
            ],
        )),
        None,
    )
    .unwrap();
    approve(directory.path(), &policy);
    let row = managed(UniversitySource::Moodle, PAGE_A, Some("Moodle task 0"));
    let (exit, report) = apply(
        directory.path(),
        &policy,
        successful_inputs(),
        reader_script(vec![row]),
        None,
    );
    assert_eq!(exit, 2, "{report}");
    assert_eq!(report["moodle"]["error"], "change_limit");
    assert_eq!(report["moodle"]["planned"]["update"], 1);
    assert_eq!(report["moodle"]["planned"]["create"], 0);
}

#[test]
fn title_migration_defers_a_concurrent_edit_and_retries_on_the_same_key() {
    let directory = state_directory();
    let policy = policy_for(3, None);
    approve(directory.path(), &policy);
    let before = managed(UniversitySource::Moodle, PAGE_A, Some("Moodle task 0"));
    let mut user_edit = before.clone();
    user_edit["last_edited_time"] = json!("2026-11-20T00:00:01Z");
    user_edit["properties"]["メモ"]["rich_text"] = json!([text("New note")]);
    user_edit["properties"]["タイトル（改名済）"]["title"] = json!([text("Manually edited title")]);
    let path = format!("/v1/pages/{PAGE_A}");
    let mut writes = writer_checks();
    writes.push(Exchange::json("GET", &path, user_edit.clone()));
    let (exit, report) = apply(
        directory.path(),
        &policy,
        successful_inputs(),
        reader_script(vec![before]),
        Some(writes),
    );
    assert_eq!(exit, 3, "{report}");
    assert_eq!(report["moodle"]["error"], "notion_conflict");
    assert_eq!(report["moodle"]["applied"]["update"], 0);
    let mut after = user_edit.clone();
    after["properties"]["タイトル（改名済）"]["title"] = json!([text("【人工知能】Moodle task 0")]);
    let mut writes = writer_checks();
    writes.extend([
        Exchange::json("GET", &path, user_edit.clone()),
        Exchange::json("PATCH", &path, after).check(|_, body| {
            assert_eq!(
                *body,
                json!({"properties":{"title":{"title":[text("【人工知能】Moodle task 0")]}}})
            );
        }),
    ]);
    let (exit, report) = apply(
        directory.path(),
        &policy,
        successful_inputs(),
        reader_script(vec![user_edit]),
        Some(writes),
    );
    assert_eq!(exit, 0, "{report}");
    assert_eq!(report["moodle"]["applied"]["update"], 1);
}

#[test]
fn title_migration_stops_for_duplicate_keys_and_preserves_the_existing_page_owner() {
    let directory = state_directory();
    let policy = policy_for(3, None);
    approve(directory.path(), &policy);
    let first = managed(UniversitySource::Moodle, PAGE_A, Some("Moodle task 0"));
    let duplicate = managed(
        UniversitySource::Moodle,
        PAGE_B,
        Some("【人工知能】Moodle task 0"),
    );
    let (exit, report) = apply(
        directory.path(),
        &policy,
        successful_inputs(),
        reader_script(vec![first.clone(), duplicate]),
        None,
    );
    assert_eq!(exit, 2, "{report}");
    assert_eq!(report["error"], "notion_conflict");

    let owner = key("other-owner");
    let store = StateStore::for_apply(
        &directory.path().join("state.sqlite3"),
        PRODUCTION_ASSIGNMENTS,
    )
    .unwrap();
    store.confirm(&owner, PAGE_A).unwrap();
    let (exit, report) = apply(
        directory.path(),
        &policy,
        successful_inputs(),
        reader_script(vec![first]),
        Some(writer_checks()),
    );
    assert_eq!(exit, 3, "{report}");
    assert_eq!(report["moodle"]["applied"]["deferred"], 1);
    assert_eq!(report["moodle"]["applied"]["update"], 0);
    assert!(store.binding(&key("moodle-0")).unwrap().is_none());
    assert_eq!(
        store.binding(&owner).unwrap().unwrap().page_id.as_deref(),
        Some(PAGE_A)
    );
}

fn reader_script(snapshot: Vec<Value>) -> Vec<Exchange> {
    let mut script = rules_snapshot();
    script.extend(rules_snapshot());
    script.push(query(snapshot, None));
    script
}

fn successful_inputs() -> RecurringInputs {
    RecurringInputs {
        moodle: Ok(mixed_input(&[], &[CATEGORY])),
        webclass: Ok(mixed_input(&[(GROUP, "2")], &[])),
    }
}

fn managed(source: UniversitySource, id: &str, title_override: Option<&str>) -> Value {
    let RecurringInputs { moodle, webclass } = successful_inputs();
    let (input, course, course_name, option, name) = match source {
        UniversitySource::Moodle => (moodle.unwrap(), 0x100, "人工知能", "option-md", "Moodle"),
        UniversitySource::WebClass => (
            webclass.unwrap(),
            0x101,
            "プログラミング2",
            "option-wc",
            "WebClass",
        ),
    };
    let (key, data) = input.assignments().iter().next().unwrap();
    let display = format!("【{course_name}】{}", data.title);
    let mut row = task(id, Some(key), title_override.unwrap_or(&display));
    row["properties"]["授業"]["relation"] = json!([{"id":fake_id(course)}]);
    row["properties"]["取得元"]["select"] = json!({"id":option,"name":name});
    row
}

fn create(row: Value) -> Exchange {
    let expected_title = row["properties"]["タイトル（改名済）"]["title"].clone();
    Exchange::json("POST", "/v1/pages", row).check(move |_, body| {
        assert_eq!(body["parent"]["data_source_id"], PRODUCTION_ASSIGNMENTS);
        let fields = body["properties"].as_object().unwrap();
        assert_eq!(fields.len(), 4);
        assert_eq!(fields["title"]["title"], expected_title);
        assert!(
            ["title", "sync", "orig", "R%3Ael"]
                .iter()
                .all(|id| fields.contains_key(*id))
        );
    })
}

fn apply(
    directory: &Path,
    policy: &RecurringPolicy,
    inputs: RecurringInputs,
    reader: Vec<Exchange>,
    writer: Option<Vec<Exchange>>,
) -> (u8, Value) {
    run_mode(
        RecurringMode::Apply,
        directory,
        policy,
        inputs,
        reader,
        writer,
    )
}

fn run_mode(
    mode: RecurringMode,
    directory: &Path,
    policy: &RecurringPolicy,
    inputs: RecurringInputs,
    reader: Vec<Exchange>,
    writer: Option<Vec<Exchange>>,
) -> (u8, Value) {
    let (client, server) = local_mock(reader);
    let writer = writer.map(local_mock);
    let (writer, writer_server) = match writer {
        Some((writer, mock)) => (Some(writer), Some(mock)),
        None => (None, None),
    };
    let report = client.run_production_recurring(
        &production_target(),
        policy,
        directory,
        mode,
        || {
            assert!(matches!(
                TargetLock::acquire(directory),
                Err(crate::notion::state::StateError::Busy)
            ));
            Ok(writer.expect("no write path expected"))
        },
        |scopes| {
            assert!(matches!(
                TargetLock::acquire(directory),
                Err(crate::notion::state::StateError::Busy)
            ));
            assert_eq!(scopes.get(GROUP).map(String::as_str), Some(TERM));
            inputs
        },
    );
    server.join().unwrap();
    if let Some(server) = writer_server {
        server.join().unwrap();
    }
    (report.exit_code(), json!(report))
}

#[test]
fn recurring_sync_updates_only_title_and_deadline_and_is_idempotent() {
    let directory = state_directory();
    let policy = policy(3);
    approve(directory.path(), &policy);
    let manual = task(MANUAL, None, "Independent manual assignment");
    let before = managed(UniversitySource::Moodle, PAGE_A, Some("Old title"));
    let mut after = managed(UniversitySource::Moodle, PAGE_A, None);
    let inputs = || {
        let mut input = successful_inputs();
        input
            .moodle
            .as_mut()
            .unwrap()
            .assignments
            .values_mut()
            .next()
            .unwrap()
            .deadline = Some(Deadline::DateOnly(
            chrono::NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(),
        ));
        input
    };
    after["properties"]["期限"]["date"] = json!({"start":"2026-12-01"});
    for property in ["状態", "メモ", "提出URL"] {
        assert_eq!(
            before["properties"][property],
            after["properties"][property]
        );
    }
    let web = managed(UniversitySource::WebClass, PAGE_B, None);
    let mut writes = writer_checks();
    writes.extend([
        Exchange::json("GET", format!("/v1/pages/{PAGE_A}"), before.clone()),
        Exchange::json("PATCH", format!("/v1/pages/{PAGE_A}"), after.clone()).check(|_, body| {
            assert_eq!(
                body["properties"],
                json!({
                    "title":{"title":[text("【人工知能】Moodle task 0")]},
                    "due":{"date":{"start":"2026-12-01"}}
                })
            );
        }),
        query(vec![], None),
        create(web.clone()),
    ]);
    let (exit, report) = apply(
        directory.path(),
        &policy,
        inputs(),
        reader_script(vec![manual.clone(), before]),
        Some(writes),
    );
    assert_eq!(exit, 0, "{report}");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["moodle"]["applied"]["update"], 1);
    assert_eq!(report["webclass"]["applied"]["create"], 1);
    let (exit, report) = apply(
        directory.path(),
        &policy,
        inputs(),
        reader_script(vec![manual, after, web]),
        Some(writer_checks()),
    );
    assert_eq!(exit, 0, "{report}");
    assert_eq!(report["moodle"]["applied"]["unchanged"], 1);
    assert_eq!(report["webclass"]["applied"]["unchanged"], 1);
}

#[test]
fn recurring_sync_applies_moodle_when_hoppii_needs_reauthentication() {
    let directory = state_directory();
    let policy = policy(3);
    approve(directory.path(), &policy);
    let mut inputs = successful_inputs();
    inputs.webclass = Err("reauth_required");
    let mut writes = writer_checks();
    writes.extend([
        query(vec![], None),
        create(managed(UniversitySource::Moodle, PAGE_A, None)),
    ]);
    let (exit, report) = apply(
        directory.path(),
        &policy,
        inputs,
        reader_script(vec![]),
        Some(writes),
    );
    assert_eq!(exit, 3, "{report}");
    assert_eq!(report["moodle"]["applied"]["create"], 1);
    assert_eq!(report["webclass"]["error"], "reauth_required");
    assert_eq!(report["webclass"]["candidates"], Value::Null);
    assert_eq!(report["webclass"]["applied"], Value::Null);
}

#[test]
fn recurring_sync_distinguishes_empty_success_from_both_source_failures() {
    let directory = state_directory();
    let policy = policy(3);
    approve(directory.path(), &policy);
    let (exit, report) = apply(
        directory.path(),
        &policy,
        RecurringInputs {
            moodle: Err("network_failed"),
            webclass: Ok(SyncInput::default()),
        },
        reader_script(vec![managed(UniversitySource::WebClass, PAGE_B, None)]),
        Some(writer_checks()),
    );
    assert_eq!(exit, 3, "{report}");
    assert_eq!(report["webclass"]["candidates"], 0);
    assert_eq!(report["webclass"]["application"], "applied");
    assert_eq!(report["moodle"]["candidates"], Value::Null);
    let (exit, report) = apply(
        directory.path(),
        &policy,
        RecurringInputs {
            moodle: Err("network_failed"),
            webclass: Err("access_denied"),
        },
        rules_snapshot(),
        None,
    );
    assert_eq!(exit, 1, "{report}");
    assert_eq!(report["status"], "failed");
    assert_eq!(report["webclass"]["candidates"], Value::Null);
}

#[test]
fn recurring_sync_blocks_unapproved_changed_and_concurrent_runs_before_requests() {
    let directory = state_directory();
    let initial = policy(3);
    let attempt = |policy: &RecurringPolicy| {
        let (reader, server) = local_mock(vec![]);
        let report = reader.run_production_recurring(
            &production_target(),
            policy,
            directory.path(),
            RecurringMode::Apply,
            || panic!("must not read writer"),
            |_| panic!("must not fetch"),
        );
        server.join().unwrap();
        (report.exit_code(), json!(report))
    };
    let (exit, report) = attempt(&initial);
    assert_eq!(exit, 2);
    assert_eq!(report["error"], "write_not_approved");
    approve(directory.path(), &initial);
    let (exit, report) = attempt(&policy(4));
    assert_eq!(exit, 2);
    assert_eq!(report["error"], "policy_changed");
    let _lock = TargetLock::acquire(directory.path()).unwrap();
    let (exit, report) = attempt(&initial);
    assert_eq!(exit, 4);
    assert_eq!(report["status"], "busy");
}

#[test]
fn recurring_revocation_preserves_recovery_and_requires_approval_for_restored_policy() {
    let directory = state_directory();
    let target = production_target();
    let policy = policy_for(5, None);
    approve(directory.path(), &policy);
    let inputs = successful_inputs();
    let moodle = inputs.moodle.unwrap();
    let pending = moodle.assignments().keys().next().unwrap();
    let webclass = inputs.webclass.unwrap();
    let linked = webclass.assignments().keys().next().unwrap();
    let path = directory.path().join("state.sqlite3");
    let timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    {
        let _lock = TargetLock::acquire(directory.path()).unwrap();
        let store = StateStore::for_apply(&path, PRODUCTION_ASSIGNMENTS).unwrap();
        assert!(
            store
                .reserve_recurring_create(pending, UniversitySource::Moodle)
                .unwrap()
        );
        store.mark_unknown(pending).unwrap();
        store.confirm(linked, PAGE_B).unwrap();
        store
            .record_progress(UniversitySource::WebClass, Some(timestamp), Some(timestamp))
            .unwrap();
        store
            .record_run(
                timestamp,
                RunOutcome::Partial,
                Some(RunIssue::Reconciliation),
                1,
            )
            .unwrap();
        assert_eq!(
            NotionClient::revoke_production_recurring(&target, directory.path()),
            Err("target_busy")
        );
        assert!(store.recurring_approval().unwrap().is_some());
    }
    for _ in 0..2 {
        NotionClient::revoke_production_recurring(&target, directory.path()).unwrap();
    }
    let store = StateStore::for_apply(&path, PRODUCTION_ASSIGNMENTS).unwrap();
    assert!(store.recurring_approval().unwrap().is_none());
    assert_eq!(
        store.binding(pending).unwrap().unwrap().status,
        BindingStatus::Unknown
    );
    assert_eq!(
        store.binding(linked).unwrap().unwrap().page_id.as_deref(),
        Some(PAGE_B)
    );
    assert_eq!(
        store.recurring_origins().unwrap().get(pending),
        Some(&UniversitySource::Moodle)
    );
    let preserved = store.summary().unwrap();
    assert_eq!(preserved.pending, 1);
    assert_eq!(preserved.last_fetch, Some(timestamp));
    assert_eq!(preserved.last_apply, Some(timestamp));
    let last = preserved.last_run.unwrap();
    assert_eq!(last.finished_at, timestamp);
    assert_eq!(last.outcome, RunOutcome::Partial);
    assert_eq!(last.issue, Some(RunIssue::Reconciliation));

    let (reader, server) = local_mock(vec![]);
    let report = reader.run_production_recurring(
        &target,
        &policy,
        directory.path(),
        RecurringMode::Apply,
        || panic!("revoked policy must not load Writer"),
        |_| panic!("revoked policy must not fetch"),
    );
    server.join().unwrap();
    assert_eq!(report.exit_code(), 2);
    assert_eq!(json!(report)["error"], "write_not_approved");
}

#[test]
fn recurring_moodle_only_reports_webclass_disabled() {
    let directory = state_directory();
    let policy = policy_for(5, None);
    approve(directory.path(), &policy);
    let mut writes = writer_checks();
    writes.extend([
        query(vec![], None),
        create(managed(UniversitySource::Moodle, PAGE_A, None)),
    ]);
    let (exit, report) = apply(
        directory.path(),
        &policy,
        RecurringInputs {
            moodle: successful_inputs().moodle,
            webclass: Err("disabled"),
        },
        reader_script(vec![]),
        Some(writes),
    );
    assert_eq!(exit, 0, "{report}");
    assert_eq!(report["moodle"]["applied"]["create"], 1);
    assert_eq!(report["webclass"]["acquisition"], "disabled");
    assert_eq!(report["webclass"]["candidates"], Value::Null);
}

#[test]
fn recurring_sync_stops_only_the_source_over_its_limits() {
    let directory = state_directory();
    let policy = policy(0);
    approve(directory.path(), &policy);
    let mut writes = writer_checks();
    writes.extend([
        query(vec![], None),
        create(managed(UniversitySource::WebClass, PAGE_B, None)),
    ]);
    let (exit, report) = apply(
        directory.path(),
        &policy,
        successful_inputs(),
        reader_script(vec![]),
        Some(writes),
    );
    assert_eq!(exit, 3, "{report}");
    assert_eq!(report["moodle"]["error"], "change_limit");
    assert_eq!(report["moodle"]["planned"]["create"], 1);
    assert_eq!(report["webclass"]["applied"]["create"], 1);
}

#[test]
fn recurring_sync_holds_a_manual_match_without_touching_the_manual_page() {
    let directory = state_directory();
    let policy = policy(3);
    approve(directory.path(), &policy);
    let mut writes = writer_checks();
    writes.extend([
        query(vec![], None),
        create(managed(UniversitySource::WebClass, PAGE_B, None)),
    ]);
    let manual = task(MANUAL, None, "Moodle task 0");
    let (exit, report) = apply(
        directory.path(),
        &policy,
        successful_inputs(),
        reader_script(vec![manual]),
        Some(writes),
    );
    assert_eq!(exit, 3, "{report}");
    assert_eq!(report["moodle"]["applied"]["deferred"], 1);
    assert_eq!(report["moodle"]["applied"]["create"], 0);
    assert_eq!(report["webclass"]["applied"]["create"], 1);
}

#[test]
fn recurring_sync_never_writes_from_an_incomplete_notion_snapshot() {
    let directory = state_directory();
    let policy = policy(3);
    approve(directory.path(), &policy);
    let mut reads = reader_script(vec![]);
    reads.pop();
    let mut incomplete = list(vec![], None);
    incomplete["request_status"] = json!({"type":"incomplete"});
    reads.push(Exchange::json(
        "POST",
        format!("/v1/data_sources/{A}/query"),
        incomplete,
    ));
    let (exit, report) = apply(directory.path(), &policy, successful_inputs(), reads, None);
    assert_eq!(exit, 2, "{report}");
    assert_eq!(report["error"], "incomplete_snapshot");
    assert_eq!(report["moodle"]["applied"], Value::Null);
    assert_eq!(report["webclass"]["applied"], Value::Null);
}

#[test]
fn recurring_sync_preview_needs_neither_writer_nor_approval() {
    let directory = test_state_dir();
    let (reader, server) = local_mock(reader_script(vec![]));
    let policy = policy(3);
    let report = reader.run_production_recurring(
        &production_target(),
        &policy,
        directory.path(),
        RecurringMode::Preview,
        || panic!("preview must not read the writer"),
        |_| successful_inputs(),
    );
    assert_eq!(report.exit_code(), 0);
    let report = json!(report);
    assert_eq!(report["moodle"]["planned"]["create"], 1);
    assert_eq!(report["webclass"]["planned"]["create"], 1);
    assert_eq!(report["moodle"]["applied"], Value::Null);
    assert!(!directory.path().join("state.sqlite3").exists());
    server.join().unwrap();
}

#[test]
fn recurring_detailed_preview_shows_updates_creates_and_holds_from_the_same_snapshot() {
    let directory = test_state_dir();
    let policy = policy_for(0, None);
    let mut existing = managed(UniversitySource::Moodle, PAGE_A, Some("Moodle task 0"));
    existing["properties"]["期限"]["date"] = json!({"start":"2026-11-28"});
    let (reader, server) = local_mock(reader_script(vec![
        existing,
        task(MANUAL, None, "【人工知能】Moodle task 2"),
    ]));
    let mut input = mixed_input(&[], &[CATEGORY; 3]);
    input
        .assignments
        .get_mut(&key("moodle-0"))
        .unwrap()
        .deadline = Some(Deadline::DateOnly("2026-12-01".parse().unwrap()));
    let report = reader.run_production_recurring(
        &production_target(),
        &policy,
        directory.path(),
        RecurringMode::PreviewWithPlan,
        || panic!("preview must not load a writer"),
        |_| RecurringInputs {
            moodle: Ok(input),
            webclass: Err("disabled"),
        },
    );
    assert_eq!(report.exit_code(), 2);
    let report = json!(report);
    assert_eq!(report["mode"], "preview");
    assert_eq!(report["moodle"]["error"], "change_limit");
    assert_eq!(report["moodle"]["planned"]["create"], 1);
    assert_eq!(report["moodle"]["planned"]["update"], 1);
    assert_eq!(report["moodle"]["planned"]["deferred"], 1);
    assert!(report["moodle"]["applied"].is_null());
    let moodle = &report["plan"]["sources"]["moodle"];
    assert_eq!(moodle["limits"], json!({"max_creates":0,"max_updates":3}));
    let entries = moodle["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    let action = |kind| {
        entries
            .iter()
            .find(|entry| entry["action"] == kind)
            .unwrap()
    };
    let update = action("update");
    assert_eq!(update["page_id"], PAGE_A);
    assert_eq!(
        update["title"],
        json!({"before":"Moodle task 0","after":"【人工知能】Moodle task 0"})
    );
    assert_eq!(
        update["deadline"],
        json!({"before":{"DateOnly":"2026-11-28"},"after":{"DateOnly":"2026-12-01"}})
    );
    assert_eq!(update["properties"], json!(["due", "title"]));
    assert_eq!(action("create")["title"], "【人工知能】Moodle task 1");
    assert_eq!(action("deferred")["reason"], "manual_candidate");
    assert!(!report.to_string().contains("synthetic-secret"));
    assert!(!directory.path().join("state.sqlite3").exists());
    server.join().unwrap();
}

#[test]
fn recurring_detailed_preview_reports_pending_intent_without_recovering_or_writing() {
    let directory = state_directory();
    let policy = policy_for(3, None);
    let state = directory.path().join("state.sqlite3");
    let store = StateStore::for_apply(&state, PRODUCTION_ASSIGNMENTS).unwrap();
    let key = key("moodle-0");
    assert!(store.reserve_create(&key).unwrap());
    drop(store);
    let before = std::fs::read(&state).unwrap();
    let (reader, server) = local_mock(reader_script(vec![]));
    let report = reader.run_production_recurring(
        &production_target(),
        &policy,
        directory.path(),
        RecurringMode::PreviewWithPlan,
        || panic!("preview must not load a writer"),
        |_| successful_inputs(),
    );
    assert_eq!(report.exit_code(), 2);
    let report = json!(report);
    assert_eq!(report["moodle"]["error"], "reconciliation_required");
    assert_eq!(report["pending_creates"], 1);
    assert_eq!(
        report["plan"]["unresolved_creates"],
        json!([{"source":"moodle","sync_key":key.as_str()}])
    );
    assert_eq!(
        report["plan"]["sources"]["moodle"]["entries"][0]["reason"],
        "pending_reconciliation"
    );
    assert_eq!(std::fs::read(&state).unwrap(), before);
    server.join().unwrap();
}

#[test]
fn recurring_apply_replans_after_preview_and_checks_the_new_counts_against_limits() {
    let directory = state_directory();
    let policy = policy_for(1, None);
    approve(directory.path(), &policy);
    let (reader, server) = local_mock(reader_script(vec![]));
    let preview = reader.run_production_recurring(
        &production_target(),
        &policy,
        directory.path(),
        RecurringMode::PreviewWithPlan,
        || panic!("preview must not load a writer"),
        |_| successful_inputs(),
    );
    assert_eq!(preview.exit_code(), 0);
    let preview = json!(preview);
    assert_eq!(preview["moodle"]["planned"]["create"], 1);
    assert_eq!(preview["moodle"]["planned"]["update"], 0);
    server.join().unwrap();

    let (exit, applied) = apply(
        directory.path(),
        &policy,
        RecurringInputs {
            moodle: Ok(mixed_input(&[], &[CATEGORY; 3])),
            webclass: Err("disabled"),
        },
        reader_script(vec![managed(
            UniversitySource::Moodle,
            PAGE_A,
            Some("Edited in Notion"),
        )]),
        None,
    );
    assert_eq!(exit, 2);
    assert_eq!(applied["moodle"]["error"], "change_limit");
    assert_eq!(applied["moodle"]["planned"]["create"], 2);
    assert_eq!(applied["moodle"]["planned"]["update"], 1);
    assert!(applied["moodle"]["applied"].is_null());
    assert!(applied.get("plan").is_none());
}

fn confirmed_hash(
    directory: &Path,
    policy: &RecurringPolicy,
    inputs: RecurringInputs,
    rows: Vec<Value>,
) -> PlanHash {
    let (_, preview) = run_mode(
        RecurringMode::PreviewWithPlan,
        directory,
        policy,
        inputs,
        reader_script(rows),
        None,
    );
    preview["plan"]["hash"]
        .as_str()
        .unwrap_or_else(|| panic!("{preview}"))
        .parse()
        .unwrap()
}

#[test]
fn plan_hash_is_order_independent_and_matching_apply_and_repeat_use_the_same_plan() {
    let directory = state_directory();
    let dir = directory.path();
    let policy = policy(3);
    approve(dir, &policy);
    let old = managed(UniversitySource::Moodle, PAGE_A, Some("Moodle task 0"));
    let manual = task(MANUAL, None, "Independent task");
    let before = vec![old.clone(), manual.clone()];
    let mut reversed = before.clone();
    reversed.reverse();
    let hash = confirmed_hash(dir, &policy, successful_inputs(), before);
    assert_eq!(
        hash,
        confirmed_hash(dir, &policy, successful_inputs(), reversed.clone())
    );
    let path = format!("/v1/pages/{PAGE_A}");
    let migrated = managed(UniversitySource::Moodle, PAGE_A, None);
    let created = managed(UniversitySource::WebClass, PAGE_B, None);
    let mut writes = writer_checks();
    writes.extend([
        Exchange::json("GET", &path, old),
        Exchange::json("PATCH", &path, migrated.clone()).check(|_, body| {
            assert_eq!(
                body["properties"],
                json!({"title":{"title":[text("【人工知能】Moodle task 0")]}})
            );
        }),
        query(vec![], None),
        create(created.clone()),
    ]);
    let (exit, report) = run_mode(
        RecurringMode::ApplyWithPlanHash(hash),
        dir,
        &policy,
        successful_inputs(),
        reader_script(reversed),
        Some(writes),
    );
    assert_eq!(exit, 0, "{report}");
    assert_eq!(report["moodle"]["applied"]["update"], 1);
    assert_eq!(report["webclass"]["applied"]["create"], 1);
    assert!(report.get("plan").is_none());
    let rows = vec![migrated, created, manual];
    let repeat = confirmed_hash(dir, &policy, successful_inputs(), rows.clone());
    assert_ne!(hash, repeat);
    let (exit, report) = run_mode(
        RecurringMode::ApplyWithPlanHash(repeat),
        dir,
        &policy,
        successful_inputs(),
        reader_script(rows),
        Some(writer_checks()),
    );
    assert_eq!(exit, 0, "{report}");
    for source in ["moodle", "webclass"] {
        assert_eq!(report[source]["applied"]["unchanged"], 1);
        assert_eq!(report[source]["applied"]["update"], 0);
        assert_eq!(report[source]["applied"]["create"], 0);
    }
}

#[test]
fn plan_hash_rejects_value_page_candidate_and_source_drift_before_any_writer_or_state_update() {
    let directory = state_directory();
    let dir = directory.path();
    let policy = policy(3);
    approve(dir, &policy);
    let state = dir.join("state.sqlite3");
    let bytes = std::fs::read(&state).unwrap();
    let old = managed(UniversitySource::Moodle, PAGE_A, Some("Moodle task 0"));
    let hash = confirmed_hash(dir, &policy, successful_inputs(), vec![old.clone()]);
    let blocked = |inputs, rows| {
        let (exit, report) = run_mode(
            RecurringMode::ApplyWithPlanHash(hash),
            dir,
            &policy,
            inputs,
            reader_script(rows),
            None,
        );
        assert_eq!(exit, 2, "{report}");
        assert_eq!(report["status"], "blocked");
        assert_eq!(report["error"], "plan_hash_mismatch");
        assert!(report["moodle"]["applied"].is_null() && report["webclass"]["applied"].is_null());
        assert_eq!(std::fs::read(&state).unwrap(), bytes);
    };
    let mut title = successful_inputs();
    // Drift in the second source must stop the first source's update as well.
    title
        .webclass
        .as_mut()
        .unwrap()
        .assignments
        .values_mut()
        .next()
        .unwrap()
        .title = "Changed WebClass task".into();
    blocked(title, vec![old.clone()]);
    let mut deadline = successful_inputs();
    deadline
        .moodle
        .as_mut()
        .unwrap()
        .assignments
        .values_mut()
        .next()
        .unwrap()
        .deadline = Some(Deadline::DateOnly("2026-12-01".parse().unwrap()));
    blocked(deadline, vec![old.clone()]);
    blocked(
        successful_inputs(),
        vec![managed(
            UniversitySource::Moodle,
            PAGE_C,
            Some("Moodle task 0"),
        )],
    );
    // The resulting PATCH is identical here. The observed pre-update title differs.
    blocked(
        successful_inputs(),
        vec![managed(
            UniversitySource::Moodle,
            PAGE_A,
            Some("Edited in Notion"),
        )],
    );
    let mut extra = successful_inputs();
    extra.moodle = Ok(mixed_input(&[], &[CATEGORY; 2]));
    blocked(extra, vec![old.clone()]);
    let mut missing_source = successful_inputs();
    missing_source.webclass = Err("reauth_required");
    blocked(missing_source, vec![old.clone()]);
    blocked(
        successful_inputs(),
        vec![old, task(MANUAL, None, "【プログラミング2】WebClass task")],
    );
}

#[test]
fn plan_hash_covers_unresolved_intent_status_and_recovery_without_persisting_on_mismatch() {
    let directory = state_directory();
    let dir = directory.path();
    let policy = policy_for(3, None);
    approve(dir, &policy);
    let state = dir.join("state.sqlite3");
    let open = || StateStore::for_apply(&state, PRODUCTION_ASSIGNMENTS).unwrap();
    let key = key("moodle-0");
    let initial = confirmed_hash(dir, &policy, successful_inputs(), vec![]);
    assert!(
        open()
            .reserve_recurring_create(&key, UniversitySource::Moodle)
            .unwrap()
    );
    let pending = confirmed_hash(dir, &policy, successful_inputs(), vec![]);
    assert_ne!(initial, pending);
    open().mark_unknown(&key).unwrap();
    let unknown = confirmed_hash(dir, &policy, successful_inputs(), vec![]);
    assert_ne!(pending, unknown);

    let rows = vec![managed(UniversitySource::Moodle, PAGE_A, None)];
    let before = std::fs::read(&state).unwrap();
    let (exit, report) = run_mode(
        RecurringMode::ApplyWithPlanHash(unknown),
        dir,
        &policy,
        successful_inputs(),
        reader_script(rows.clone()),
        None,
    );
    assert_eq!(exit, 2, "{report}");
    assert_eq!(report["error"], "plan_hash_mismatch");
    assert_eq!(std::fs::read(&state).unwrap(), before);
    assert_eq!(
        open().binding(&key).unwrap().unwrap().status,
        BindingStatus::Unknown
    );
    let recover = confirmed_hash(dir, &policy, successful_inputs(), rows.clone());
    let (exit, report) = run_mode(
        RecurringMode::ApplyWithPlanHash(recover),
        dir,
        &policy,
        successful_inputs(),
        reader_script(rows),
        Some(writer_checks()),
    );
    assert_eq!(exit, 0, "{report}");
    assert_eq!(report["recovered_creates"], 1);
    assert_eq!(report["moodle"]["applied"]["unchanged"], 1);
    assert_eq!(
        open().binding(&key).unwrap().unwrap().status,
        BindingStatus::Linked
    );
}

#[test]
fn matching_plan_hash_still_requires_approval_and_does_not_override_limits() {
    let directory = state_directory();
    let dir = directory.path();
    let policy = policy_for(0, None);
    let hash = confirmed_hash(dir, &policy, successful_inputs(), vec![]);
    assert_ne!(
        hash,
        confirmed_hash(dir, &policy_for(1, None), successful_inputs(), vec![])
    );
    let mode = RecurringMode::ApplyWithPlanHash(hash);
    let (exit, report) = run_mode(mode, dir, &policy, successful_inputs(), vec![], None);
    assert_eq!(exit, 2, "{report}");
    assert_eq!(report["error"], "write_not_approved");
    approve(dir, &policy);
    let (exit, report) = run_mode(
        mode,
        dir,
        &policy,
        successful_inputs(),
        reader_script(vec![]),
        None,
    );
    assert_eq!(exit, 2, "{report}");
    assert_eq!(report["moodle"]["error"], "change_limit");
    assert!(report["moodle"]["applied"].is_null());
}

#[test]
fn matching_plan_hash_still_rechecks_the_page_and_creation_key_before_writing() {
    let directory = state_directory();
    let dir = directory.path();
    let policy = policy(3);
    approve(dir, &policy);
    let old = managed(UniversitySource::Moodle, PAGE_A, Some("Moodle task 0"));
    let hash = confirmed_hash(dir, &policy, successful_inputs(), vec![old.clone()]);
    let mut writes = writer_checks();
    writes.extend([
        Exchange::json(
            "GET",
            format!("/v1/pages/{PAGE_A}"),
            managed(UniversitySource::Moodle, PAGE_A, Some("Concurrent edit")),
        ),
        query(
            vec![managed(UniversitySource::WebClass, PAGE_B, None)],
            None,
        ),
    ]);
    let (exit, report) = run_mode(
        RecurringMode::ApplyWithPlanHash(hash),
        dir,
        &policy,
        successful_inputs(),
        reader_script(vec![old]),
        Some(writes),
    );
    assert_eq!(exit, 3, "{report}");
    assert_eq!(report["moodle"]["error"], "notion_conflict");
    assert_eq!(report["moodle"]["applied"]["update"], 0);
    assert_eq!(report["webclass"]["applied"]["create"], 0);
}

#[test]
fn guarded_apply_blocks_conflicting_snapshots_and_total_acquisition_failure_without_state_updates()
{
    let directory = state_directory();
    let dir = directory.path();
    let policy = policy(3);
    approve(dir, &policy);
    let hash = confirmed_hash(dir, &policy, successful_inputs(), vec![]);
    let state = dir.join("state.sqlite3");
    let before = std::fs::read(&state).unwrap();
    let duplicates = vec![
        managed(UniversitySource::Moodle, PAGE_A, None),
        managed(UniversitySource::Moodle, PAGE_B, None),
    ];
    let (_, preview) = run_mode(
        RecurringMode::PreviewWithPlan,
        dir,
        &policy,
        successful_inputs(),
        reader_script(duplicates.clone()),
        None,
    );
    assert!(preview["plan"]["hash"].is_null());
    let (exit, report) = run_mode(
        RecurringMode::ApplyWithPlanHash(hash),
        dir,
        &policy,
        successful_inputs(),
        reader_script(duplicates),
        None,
    );
    assert_eq!(exit, 2, "{report}");
    assert_eq!(report["error"], "notion_conflict");
    let (exit, report) = run_mode(
        RecurringMode::ApplyWithPlanHash(hash),
        dir,
        &policy,
        RecurringInputs {
            moodle: Err("fetch_failed"),
            webclass: Err("reauth_required"),
        },
        rules_snapshot(),
        None,
    );
    assert_eq!(exit, 2, "{report}");
    assert_eq!(report["error"], "plan_hash_mismatch");
    assert_eq!(std::fs::read(&state).unwrap(), before);
}

#[test]
fn recurring_sync_blocks_a_changed_writer_for_both_sources() {
    let directory = state_directory();
    let policy = policy(3);
    approve(directory.path(), &policy);
    let mut writer = vec![trial_tests::bot(&fake_id(0x501))];
    writer.extend(schemas().into_iter().take(2));
    let (exit, report) = apply(
        directory.path(),
        &policy,
        successful_inputs(),
        reader_script(vec![]),
        Some(writer),
    );
    assert_eq!(exit, 2, "{report}");
    assert_eq!(report["error"], "writer_mismatch");
    assert_eq!(report["moodle"]["applied"], Value::Null);
    assert_eq!(report["webclass"]["applied"], Value::Null);
}

#[test]
fn recurring_sync_recovers_after_a_lost_create_response_without_reposting() {
    let directory = state_directory();
    let policy = policy(3);
    approve(directory.path(), &policy);
    let mut writes = writer_checks();
    writes.extend([
        query(vec![], None),
        Exchange::drop_response("POST", "/v1/pages"),
    ]);
    let (exit, report) = apply(
        directory.path(),
        &policy,
        successful_inputs(),
        reader_script(vec![]),
        Some(writes),
    );
    assert_eq!(exit, 3, "{report}");
    assert_eq!(report["pending_creates"], 1);
    assert_eq!(report["webclass"]["applied"], Value::Null);

    let mut still_missing = successful_inputs();
    still_missing.moodle = Err("network_failed");
    let mut writes = writer_checks();
    let web = managed(UniversitySource::WebClass, PAGE_B, None);
    writes.extend([query(vec![], None), create(web.clone())]);
    let (exit, report) = apply(
        directory.path(),
        &policy,
        still_missing,
        reader_script(vec![]),
        Some(writes),
    );
    assert_eq!(exit, 3, "{report}");
    assert_eq!(report["pending_creates"], 1);
    assert_eq!(report["webclass"]["applied"]["create"], 1);

    let rows = vec![managed(UniversitySource::Moodle, PAGE_A, None), web];
    let (exit, report) = apply(
        directory.path(),
        &policy,
        successful_inputs(),
        reader_script(rows),
        Some(writer_checks()),
    );
    assert_eq!(exit, 0, "{report}");
    assert_eq!(report["pending_creates"], 0);
    assert_eq!(report["recovered_creates"], 1);
    assert_eq!(report["moodle"]["applied"]["unchanged"], 1);
}
