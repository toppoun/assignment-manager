use super::*;
use crate::notion::{
    state::{StateError, StateStore, TargetLock},
    trial::PRODUCTION_DATA_SOURCES,
};

pub(in crate::notion) mod recovery_tests;

const READ_BOT: &str = "00000000-0000-4000-8000-000000000011";
const WRITE_BOT: &str = "00000000-0000-4000-8000-000000000012";

fn trial() -> TrialTarget {
    TrialTarget::new(target(), A, READ_BOT, WRITE_BOT).unwrap()
}

fn assignment() -> TrialAssignment {
    TrialAssignment {
        title: "Synthetic task".into(),
        deadline: None,
        course_page_id: None,
    }
}

fn bot(id: &str) -> Exchange {
    Exchange::json(
        "GET",
        "/v1/users/me",
        json!({
            "object":"user", "type":"bot", "id":id,
            "bot":{"owner":{"type":"workspace","workspace":true}},
        }),
    )
}

fn trial_schema() -> Value {
    let mut schema = schema_json();
    schema["properties"]["優先度"] = json!({"id":"urgency","type":"select","select":{}});
    schema["properties"]["繰り返し"] = json!({"id":"repeat","type":"select","select":{}});
    schema["properties"]["メモ"] = json!({"id":"notes","type":"rich_text","rich_text":{}});
    schema
}

fn snapshot(rows: Vec<Value>) -> Vec<Exchange> {
    let mut script = schema_exchanges(trial_schema());
    script.push(query(rows, None));
    script
}

fn directory() -> tempfile::TempDir {
    let dir = state::test_state_dir();
    StateStore::initialize(&dir.path().join("state.sqlite3"), A).unwrap();
    dir
}

#[test]
fn rejects_both_production_ids_in_either_role_and_requires_explicit_selection() {
    for id in PRODUCTION_DATA_SOURCES {
        let alternate_format = id.replace('-', "").to_ascii_uppercase();
        let mut config = target();
        config.assignments_data_source_id = alternate_format.clone();
        assert_eq!(
            TrialTarget::new(config, id, READ_BOT, WRITE_BOT).err(),
            Some(NotionError::MethodDisabled)
        );
        let mut config = target();
        config.courses_data_source_id = alternate_format;
        assert_eq!(
            TrialTarget::new(config, A, READ_BOT, WRITE_BOT).err(),
            Some(NotionError::MethodDisabled)
        );
    }
    assert!(TrialTarget::new(target(), "", READ_BOT, WRITE_BOT).is_err());
    assert!(TrialTarget::new(target(), C, READ_BOT, WRITE_BOT).is_err());
    assert!(TrialTarget::new(target(), A, READ_BOT, READ_BOT).is_err());
}

#[test]
fn a_trial_credential_must_match_its_expected_bot_before_loading_the_target() {
    let target = trial();
    let assignment = assignment();
    let dir = directory();
    let (client, server) = mock(vec![bot(READ_BOT), bot(WRITE_BOT)]);
    assert_eq!(
        client
            .apply_trial(&target, &assignment, dir.path(), |_| panic!("no plan"))
            .err(),
        Some(ApplyError::Notion(NotionError::CredentialMismatch))
    );
    assert_eq!(
        client.preview_trial(&target, &assignment, dir.path()).err(),
        Some(ApplyError::Notion(NotionError::CredentialMismatch))
    );
    server.join().unwrap();
}

#[test]
fn a_trial_dry_run_and_declined_plan_leave_the_state_database_identical() {
    let target = trial();
    let assignment = assignment();
    let dir = directory();
    let path = dir.path().join("state.sqlite3");
    let before = std::fs::read(&path).unwrap();
    let mut script = vec![bot(READ_BOT)];
    script.extend(snapshot(vec![]));
    script.push(bot(WRITE_BOT));
    script.extend(snapshot(vec![]));
    let (client, server) = mock(script);
    assert_eq!(
        client
            .preview_trial(&target, &assignment, dir.path())
            .unwrap()
            .counts
            .create,
        1
    );
    let result = client.apply_trial(&target, &assignment, dir.path(), |preview| {
        assert_eq!(preview.summary().counts.create, 1);
        assert!(matches!(
            preview.assignments()[0].action,
            PlanAction::Create { .. }
        ));
        assert!(matches!(
            TargetLock::acquire(dir.path()),
            Err(StateError::Busy)
        ));
        false
    });
    assert_eq!(result, Ok(None));
    assert_eq!(std::fs::read(path).unwrap(), before);
    server.join().unwrap();
}

#[test]
fn a_trial_with_a_changed_snapshot_after_approval_holds_the_write() {
    let target = trial();
    let assignment = assignment();
    let dir = directory();
    let path = dir.path().join("state.sqlite3");
    let before = std::fs::read(&path).unwrap();
    let mut script = vec![bot(WRITE_BOT)];
    script.extend(snapshot(vec![]));
    script.extend(snapshot(vec![task(MANUAL, None, &assignment.title)]));
    let (client, server) = mock(script);
    assert_eq!(
        client.apply_trial(&target, &assignment, dir.path(), |_| true),
        Err(ApplyError::PlanChanged)
    );
    assert_eq!(std::fs::read(path).unwrap(), before);
    server.join().unwrap();
}

#[test]
fn a_trial_with_an_incomplete_snapshot_after_approval_holds_the_write() {
    let target = trial();
    let assignment = assignment();
    let (input, _) = assignment.input().unwrap();
    let key = input.assignments().first_key_value().unwrap().0;
    let dir = directory();
    let path = dir.path().join("state.sqlite3");
    let before = std::fs::read(&path).unwrap();
    let old = task(PAGE_A, Some(key), "Old title");
    let mut script = vec![bot(WRITE_BOT)];
    script.extend(snapshot(vec![old.clone()]));
    script.extend(schema_exchanges(trial_schema()));
    script.push(query(vec![old], Some("next")));
    script.push(
        Exchange::error("POST", format!("/v1/data_sources/{A}/query"), 403, "")
            .check(|_, body| assert_eq!(body["start_cursor"], "next")),
    );
    let (client, server) = mock(script);
    assert_eq!(
        client.apply_trial(&target, &assignment, dir.path(), |preview| {
            assert_eq!(preview.summary().counts.update, 1);
            true
        }),
        Err(ApplyError::Notion(NotionError::Forbidden))
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    server.join().unwrap();
}

#[test]
fn a_trial_requires_the_protected_schema_and_a_complete_readable_snapshot() {
    let target = trial();
    let assignment = assignment();
    let dir = directory();
    let mut incomplete = task(MANUAL, None, &assignment.title);
    incomplete["properties"]
        .as_object_mut()
        .unwrap()
        .remove("期限");
    let mut script = vec![bot(WRITE_BOT)];
    script.extend(schema_exchanges(schema_json()));
    script.push(bot(WRITE_BOT));
    let mut aliased = trial_schema();
    aliased["properties"]["優先度"]["id"] = json!("orig");
    script.extend(schema_exchanges(aliased));
    script.push(bot(WRITE_BOT));
    script.extend(snapshot(vec![incomplete]));
    let (client, server) = mock(script);
    for error in [
        NotionError::SchemaMismatch,
        NotionError::SchemaMismatch,
        NotionError::InvalidResponse,
    ] {
        assert_eq!(
            client.apply_trial(&target, &assignment, dir.path(), |_| panic!(
                "incomplete plan"
            )),
            Err(ApplyError::Notion(error))
        );
    }
    server.join().unwrap();
}

#[test]
fn approved_trial_creates_exactly_once_and_updates_using_the_same_key() {
    let target = trial();
    let mut assignment = assignment();
    let (input, _) = assignment.input().unwrap();
    let key = input.assignments().first_key_value().unwrap().0;
    let page = task(PAGE_A, Some(key), &assignment.title);
    let mut updated = task(PAGE_A, Some(key), "Edited");
    updated["properties"]["期限"]["date"] = json!({"start":"2026-11-22"});
    let mut script = vec![bot(WRITE_BOT)];
    script.extend(snapshot(vec![]));
    script.extend(snapshot(vec![]));
    let expected_key = key.as_str().to_owned();
    script.push(query(vec![], None).check(move |_, body| {
        assert_eq!(body["filter"]["rich_text"]["equals"], expected_key);
    }));
    script.push(
        Exchange::json("POST", "/v1/pages", page.clone()).check(|_, body| {
            assert_eq!(
                body["parent"],
                json!({"type":"data_source_id", "data_source_id":A})
            );
            assert_eq!(body["properties"].as_object().unwrap().len(), 3);
        }),
    );
    for _ in 0..2 {
        script.push(bot(WRITE_BOT));
        script.extend(snapshot(vec![page.clone()]));
        script.extend(snapshot(vec![page.clone()]));
    }
    script.push(Exchange::json("GET", format!("/v1/pages/{PAGE_A}"), page));
    script.push(
        Exchange::json("PATCH", format!("/v1/pages/{PAGE_A}"), updated).check(|_, body| {
            assert_eq!(
                body,
                &json!({"properties":{
                    "title":{"title":[text("Edited")]},
                    "due":{"date":{"start":"2026-11-22"}}
                }})
            );
        }),
    );
    let (client, server) = mock(script);
    let dir = directory();
    assert_eq!(
        client
            .apply_trial(&target, &assignment, dir.path(), |_| true)
            .unwrap()
            .unwrap()
            .counts
            .create,
        1
    );
    assert_eq!(
        client
            .apply_trial(&target, &assignment, dir.path(), |_| true)
            .unwrap()
            .unwrap()
            .counts
            .unchanged,
        1
    );
    assignment.title = "Edited".into();
    assignment.deadline = Some(Deadline::DateOnly("2026-11-22".parse().unwrap()));
    assert_eq!(
        client
            .apply_trial(&target, &assignment, dir.path(), |_| true)
            .unwrap()
            .unwrap()
            .counts
            .update,
        1
    );
    let state = StateStore::for_apply(&dir.path().join("state.sqlite3"), A).unwrap();
    assert_eq!(
        state.binding(key).unwrap().unwrap().page_id.as_deref(),
        Some(PAGE_A)
    );
    server.join().unwrap();
}
