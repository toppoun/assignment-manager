use std::path::Path;

use super::*;
use crate::notion::{
    ChangeLimits, RecurringInputs, RecurringMode, RecurringPolicy,
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
                    &fake_id(0x101),
                ),
            ],
            None,
        ),
        courses_query(vec![course(0, TERM), course(1, TERM)]),
    ]);
    script
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
    let (input, course, option, name) = match source {
        UniversitySource::Moodle => (moodle.unwrap(), 0x100, "option-md", "Moodle"),
        UniversitySource::WebClass => (webclass.unwrap(), 0x101, "option-wc", "WebClass"),
    };
    let (key, data) = input.assignments().iter().next().unwrap();
    let mut row = task(id, Some(key), title_override.unwrap_or(&data.title));
    row["properties"]["授業"]["relation"] = json!([{"id":fake_id(course)}]);
    row["properties"]["取得元"]["select"] = json!({"id":option,"name":name});
    row
}

fn create(row: Value) -> Exchange {
    Exchange::json("POST", "/v1/pages", row).check(|_, body| {
        assert_eq!(body["parent"]["data_source_id"], PRODUCTION_ASSIGNMENTS);
        let fields = body["properties"].as_object().unwrap();
        assert_eq!(fields.len(), 4);
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
        RecurringMode::Apply,
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
                    "title":{"title":[text("Moodle task 0")]},
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
