use super::super::trial_tests::{READ_BOT, WRITE_BOT, bot, snapshot};
use super::*;
use crate::notion::MoodleCreateTarget;
use crate::webclass_auth::synthetic::Scenario;

mod update;

fn moodle_fixture() -> SyncInput {
    acquire_moodle_fixture(Scenario::MoodleReview)
}

fn acquire_moodle_fixture(scenario: Scenario) -> SyncInput {
    use crate::{
        moodle_http::{MoodleExportKind, MoodleFeed},
        secured_sync::SecuredSyncClients,
        sync::acquire_moodle,
        webclass_auth::synthetic::{MOODLE_SCOPE, MOODLE_URL, SyntheticUniversity},
    };

    let university = SyntheticUniversity::start(scenario);
    let mut clients = SecuredSyncClients::new(university.policy());
    clients.begin_run();
    let feed = MoodleFeed::new(MOODLE_SCOPE, MoodleExportKind::CourseRelated, MOODLE_URL).unwrap();
    let acquired = acquire_moodle(&clients.moodle().unwrap(), &feed).unwrap();
    SyncInput::from_acquisitions(Some(&acquired), None).unwrap()
}

fn moodle_pages(source: &SyncInput) -> Vec<Value> {
    assert_eq!(source.assignments().len(), 3);
    source
        .assignments()
        .iter()
        .zip([PAGE_A, PAGE_B, PAGE_C])
        .map(|((key, entry), id)| {
            let mut page = task(id, Some(key), &entry.title);
            let Some(Deadline::DateTime(due)) = entry.deadline else {
                panic!("fixture deadline")
            };
            page["properties"]["期限"]["date"] =
                json!({"start":due.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)});
            page
        })
        .collect()
}

fn create_fixture() -> (MoodleCreateTarget, SyncInput, [CourseMapping; 1], Value) {
    let trial = TrialTarget::new(target(), A, READ_BOT, WRITE_BOT).unwrap();
    let all = moodle_fixture();
    let pages = moodle_pages(&all);
    let input = all
        .select_fixture_moodle("review-submit@example.invalid")
        .unwrap();
    let (key, assignment) = input.assignments().first_key_value().unwrap();
    let mut created = pages
        .into_iter()
        .find(|page| {
            page["properties"]["同期キー"]["rich_text"][0]["text"]["content"] == key.as_str()
        })
        .unwrap();
    created["properties"]["授業"]["relation"] = json!([{"id":COURSE}]);
    let mappings = [CourseMapping {
        course_key: assignment.course_key.clone().unwrap(),
        page_id: COURSE.into(),
    }];
    (
        MoodleCreateTarget::for_mock(trial, COURSE),
        input,
        mappings,
        created,
    )
}

fn create_snapshot(rows: Vec<Value>) -> Vec<Exchange> {
    let mut exchanges = snapshot(rows);
    exchanges.push(get_course());
    exchanges
}

#[test]
fn create_trial_selects_one_fixture_and_never_updates_other_pages() {
    let (target, input, mappings, created) = create_fixture();
    let dir = apply_state();
    let key = input.assignments().first_key_value().unwrap().0.clone();
    let existing = task(PAGE_D, Some(&super::key("phase3d")), "Prior trial");
    let assignment = &input.assignments()[&key];
    let expected_body = json!({
        "parent":{"type":"data_source_id","data_source_id":A},
        "properties":{
            "title":{"title":[text(&assignment.title)]},
            "due":{"date":created["properties"]["期限"]["date"]},
            "orig":{"select":{"id":"option-md"}},
            "sync":{"rich_text":[text(key.as_str())]},
            "R%3Ael":{"relation":[{"id":COURSE}]}
        }
    });
    let mut script = vec![bot(READ_BOT)];
    script.extend(create_snapshot(vec![existing.clone()]));
    script.push(bot(WRITE_BOT));
    script.extend(create_snapshot(vec![existing.clone()]));
    script.extend(create_snapshot(vec![existing.clone()]));
    script.push(exact(key.clone(), vec![]));
    script.push(
        Exchange::json("POST", "/v1/pages", created.clone()).check(move |_, body| {
            assert_eq!(body, &expected_body);
        }),
    );
    script.push(bot(READ_BOT));
    script.extend(create_snapshot(vec![existing, created]));
    let (client, server) = mock(script);

    let before = client
        .preview_moodle_create(&target, &input, &mappings, dir.path())
        .unwrap();
    assert!(before.can_create);
    assert_eq!(before.counts.create, 1);
    let applied = client
        .apply_moodle_create(&target, &input, &mappings, dir.path(), || {
            assert!(matches!(
                TargetLock::acquire(dir.path()),
                Err(StateError::Busy)
            ));
            true
        })
        .unwrap()
        .unwrap();
    assert_eq!(applied.counts.create, 1);
    assert_eq!(applied.counts.update, 0);
    assert_eq!(applied.pending, 0);

    let db = dir.path().join("state.sqlite3");
    let saved = std::fs::read(&db).unwrap();
    let after = client
        .preview_moodle_create(&target, &input, &mappings, dir.path())
        .unwrap();
    assert_eq!(before.existing_rows + 1, after.existing_rows);
    assert_eq!(before.other_managed_revision, after.other_managed_revision);
    assert_eq!(after.counts.create, 0);
    assert_eq!(after.counts.unchanged, 1);
    assert!(!after.can_create);
    assert_eq!(std::fs::read(db).unwrap(), saved);
    let another = moodle_fixture()
        .select_fixture_moodle("review-close@example.invalid")
        .unwrap();
    assert_eq!(
        client.apply_moodle_create(&target, &another, &mappings, dir.path(), || true),
        Err(ApplyError::CreateBlocked)
    );
    server.join().unwrap();
}

#[test]
fn create_trial_rejects_bulk_wrong_writer_and_existing_keys_before_approval() {
    let (target, input, mappings, existing) = create_fixture();
    let path = apply_state();
    let state = path.path().join("state.sqlite3");
    let saved = std::fs::read(&state).unwrap();
    assert!(
        moodle_fixture()
            .select_fixture_moodle("missing@example.invalid")
            .is_err()
    );
    let mut other = existing.clone();
    other["id"] = json!(PAGE_D);
    let mut script = vec![bot(READ_BOT), bot(WRITE_BOT)];
    script.extend(snapshot(vec![existing.clone(), other]));
    script.push(bot(WRITE_BOT));
    script.extend(snapshot(vec![existing]));
    let (client, server) = mock(script);
    let check = |input: &SyncInput| {
        client.apply_moodle_create(&target, input, &mappings, path.path(), || {
            panic!("must not offer approval")
        })
    };
    assert_eq!(check(&moodle_fixture()), Err(ApplyError::CreateBlocked));
    assert_eq!(
        check(&input),
        Err(ApplyError::Notion(NotionError::CredentialMismatch))
    );
    assert_eq!(check(&input), Err(ApplyError::CreateBlocked));
    assert_eq!(check(&input), Err(ApplyError::CreateBlocked));
    assert_eq!(std::fs::read(state).unwrap(), saved);
    server.join().unwrap();
}

#[test]
fn create_trial_stops_if_the_key_appears_after_approval() {
    let (target, input, mappings, existing) = create_fixture();
    let dir = apply_state();
    let key = input.assignments().first_key_value().unwrap().0.clone();
    let db = dir.path().join("state.sqlite3");
    let saved = std::fs::read(&db).unwrap();
    let mut script = vec![bot(WRITE_BOT)];
    script.extend(create_snapshot(vec![]));
    script.push(bot(WRITE_BOT));
    script.extend(create_snapshot(vec![]));
    script.extend(create_snapshot(vec![]));
    script.push(exact(key, vec![existing]));
    let (client, server) = mock(script);
    assert_eq!(
        client.apply_moodle_create(&target, &input, &mappings, dir.path(), || false),
        Ok(None)
    );
    assert_eq!(
        client.apply_moodle_create(&target, &input, &mappings, dir.path(), || true),
        Err(ApplyError::PlanChanged)
    );
    assert_eq!(std::fs::read(db).unwrap(), saved);
    server.join().unwrap();
}

#[test]
fn create_trial_keeps_an_unknown_write_pending_and_recovers_only_with_a_verified_reader() {
    let (target, input, mappings, created) = create_fixture();
    let dir = apply_state();
    let key = input.assignments().first_key_value().unwrap().0;
    let mut script = vec![bot(WRITE_BOT)];
    script.extend(create_snapshot(vec![]));
    script.extend(create_snapshot(vec![]));
    script.push(exact(key.clone(), vec![]));
    script.push(Exchange::error("POST", "/v1/pages", 500, ""));
    script.push(bot(READ_BOT));
    script.extend(create_snapshot(vec![]));
    let mut changed = created.clone();
    changed["properties"]["タイトル（改名済）"]["title"] = json!([text("Different title")]);
    script.push(bot(READ_BOT));
    script.extend(snapshot(vec![changed]));
    script.push(bot(READ_BOT));
    script.extend(create_snapshot(vec![created]));
    let (client, server) = mock(script);

    let report = client
        .apply_moodle_create(&target, &input, &mappings, dir.path(), || true)
        .unwrap()
        .unwrap();
    assert_eq!(report.error, Some(NotionError::Server(500)));
    assert_eq!(report.pending, 1);
    assert_eq!(
        client.apply_moodle_create(&target, &input, &mappings, dir.path(), || true),
        Err(ApplyError::CreateBlocked)
    );
    for _ in 0..2 {
        assert_eq!(
            client.reconcile_moodle_create(&target, &input, &mappings, dir.path()),
            Err(ApplyError::CreateBlocked)
        );
        assert_eq!(
            open_state(dir.path()).binding(key).unwrap().unwrap().status,
            state::BindingStatus::Unknown
        );
    }
    client
        .reconcile_moodle_create(&target, &input, &mappings, dir.path())
        .unwrap();
    assert_eq!(
        open_state(dir.path()).binding(key).unwrap().unwrap().status,
        state::BindingStatus::Linked
    );
    server.join().unwrap();
}

#[test]
fn create_trial_requires_the_fixed_verification_pair_and_a_distinct_writer() {
    let mut verified = target();
    verified.assignments_data_source_id = "41e85934-7e84-817e-b1d2-000b68726503".into();
    verified.courses_data_source_id = "41e85934-7e84-81df-902b-000bbece9bf7".into();
    assert!(MoodleCreateTarget::new(verified, WRITE_BOT).is_ok());
    let mut wrong = target();
    wrong.assignments_data_source_id = "41e85934-7e84-817e-b1d2-000b68726503".into();
    assert!(MoodleCreateTarget::new(wrong, WRITE_BOT).is_err());
    let mut same = target();
    same.assignments_data_source_id = "41e85934-7e84-817e-b1d2-000b68726503".into();
    same.courses_data_source_id = "41e85934-7e84-81df-902b-000bbece9bf7".into();
    assert!(MoodleCreateTarget::new(same, "41e85934-7e84-81d8-9891-00275cb3050f").is_err());
}

#[test]
fn moodle_partial_write_reconciles_after_reopening_state_and_never_creates_twice() {
    let input = moodle_fixture();
    let keys: Vec<_> = input.assignments().keys().cloned().collect();
    let pages = moodle_pages(&input);
    let dir = apply_state();
    let path = dir.path().join("state.sqlite3");
    let mut first = apply_exchanges(vec![]);
    first.push(exact(keys[0].clone(), vec![]));
    first.push(Exchange::json("POST", "/v1/pages", pages[0].clone()));
    first.push(exact(keys[1].clone(), vec![]));
    first.push(Exchange::error("POST", "/v1/pages", 500, ""));
    let (client, server) = mock(first);
    let partial = apply(&client, &input, &[], &dir);
    assert_eq!(partial.counts.create, 1);
    assert_eq!(partial.counts.deferred, 2);
    assert_eq!(partial.pending, 1);
    assert_eq!(partial.error, Some(NotionError::Server(500)));
    server.join().unwrap();
    drop(client);

    let input = moodle_fixture();
    assert!(keys.iter().eq(input.assignments().keys()));
    let verification = VerificationTarget::new(target(), A, C, READ_BOT).unwrap();
    let state = || StateStore::read(&path, A).unwrap().unwrap();
    let saved = std::fs::read(&path).unwrap();
    let mut script = vec![bot(READ_BOT)];
    script.extend(snapshot(pages[..1].to_vec()));
    let (client, server) = mock(script);
    let preview = client
        .preview_verification(&verification, &input, &[], state())
        .unwrap();
    assert_eq!(
        (preview.create, preview.unchanged, preview.deferred),
        (1, 1, 1)
    );
    assert_eq!(std::fs::read(&path).unwrap(), saved);
    server.join().unwrap();

    let mut script = apply_exchanges(pages[..2].to_vec());
    script.push(exact(keys[2].clone(), vec![]));
    script.push(Exchange::json("POST", "/v1/pages", pages[2].clone()));
    script.extend(apply_exchanges(pages.clone()));
    let (client, server) = mock(script);
    let resumed = apply(&client, &input, &[], &dir);
    assert_eq!(
        (
            resumed.counts.create,
            resumed.counts.unchanged,
            resumed.recovered,
            resumed.pending
        ),
        (1, 2, 1, 0)
    );
    assert_eq!(resumed.error, None);
    let repeat = apply(&client, &input, &[], &dir);
    assert_eq!(
        (
            repeat.counts.create,
            repeat.counts.update,
            repeat.counts.unchanged
        ),
        (0, 0, 3)
    );
    for (key, page) in keys.iter().zip([PAGE_A, PAGE_B, PAGE_C]) {
        assert_eq!(state().bindings[key].page_id.as_deref(), Some(page));
    }
    server.join().unwrap();
}

#[test]
fn moodle_preview_blocks_duplicate_and_manual_candidates_and_ignores_absent_tasks() {
    let input = moodle_fixture();
    let mut pages = moodle_pages(&input);
    let mut duplicate = pages[0].clone();
    duplicate["id"] = json!(PAGE_D);
    let mut manual = pages.pop().unwrap();
    manual["id"] = json!(MANUAL);
    manual["properties"]["同期キー"]["rich_text"] = json!([]);
    let absent = task(
        PAGE_C,
        Some(&key("absent-from-moodle")),
        "Absent from this feed",
    );
    pages.extend([duplicate, manual, absent]);

    let dir = apply_state();
    let path = dir.path().join("state.sqlite3");
    let before = std::fs::read(&path).unwrap();
    let mut script = vec![bot(READ_BOT)];
    script.extend(snapshot(pages));
    let (client, server) = mock(script);
    let counts = client
        .preview_verification(
            &VerificationTarget::new(target(), A, C, READ_BOT).unwrap(),
            &input,
            &[],
            StateStore::read(&path, A).unwrap().unwrap(),
        )
        .unwrap();
    assert_eq!(
        counts,
        DryRunCounts {
            unchanged: 1,
            conflict: 1,
            deferred: 1,
            ..Default::default()
        }
    );
    assert_eq!(std::fs::read(path).unwrap(), before);
    server.join().unwrap();
}

#[test]
fn moodle_updates_title_and_due_while_preserving_manual_properties_and_course() {
    let input = moodle_fixture();
    let latest = moodle_pages(&input);
    let mut prior = latest.clone();
    prior[0]["properties"]["タイトル（改名済）"]["title"] = json!([text("Former title")]);
    prior[0]["properties"]["期限"]["date"] = json!({"start":"2026-11-20T00:00:00Z"});
    let course_key = input
        .assignments()
        .values()
        .next()
        .unwrap()
        .course_key
        .clone()
        .unwrap();
    let course_index = input
        .assignments()
        .values()
        .enumerate()
        .skip(1)
        .find_map(|(index, entry)| {
            (entry.course_key.as_ref() != Some(&course_key)).then_some(index)
        })
        .unwrap();
    let other = input
        .assignments()
        .values()
        .nth(course_index)
        .unwrap()
        .course_key
        .clone()
        .unwrap();
    let mappings = [
        CourseMapping {
            course_key: course_key.clone(),
            page_id: PAGE_D.into(),
        },
        CourseMapping {
            course_key: other,
            page_id: COURSE.into(),
        },
    ];
    // The current Moodle course maps to a different page than the existing Relation.
    prior[0]["properties"]["授業"]["relation"] = json!([{"id":OLD_COURSE}]);
    let mut expected = latest.clone();
    expected[0]["properties"]["授業"]["relation"] =
        prior[0]["properties"]["授業"]["relation"].clone();
    for (index, entry) in input.assignments().values().enumerate().skip(1) {
        let course = if entry.course_key.as_ref() == Some(&course_key) {
            PAGE_D
        } else {
            COURSE
        };
        expected[index]["properties"]["授業"]["relation"] = json!([{"id":course}]);
        if index != course_index {
            prior[index] = expected[index].clone();
        }
    }
    let mut script = apply_exchanges(prior.clone());
    script.push(get_course());
    script.push(get_task(prior[0].clone()));
    script.push(patch_task(
        expected[0].clone(),
        json!({
            "title":{"title": latest[0]["properties"]["タイトル（改名済）"]["title"]},
            "due":{"date": latest[0]["properties"]["期限"]["date"]},
        }),
    ));
    let id = expected[course_index]["id"].as_str().unwrap();
    let path = format!("/v1/pages/{id}");
    script.push(Exchange::json("GET", &path, prior[course_index].clone()));
    script.push(
        Exchange::json("PATCH", path, expected[course_index].clone()).check(|_, body| {
            assert_eq!(
                body,
                &json!({"properties":{"R%3Ael":{"relation":[{"id":COURSE}]}}})
            );
        }),
    );
    let (client, server) = mock(script);
    let report = apply(&client, &input, &mappings, &apply_state());
    assert_eq!(
        (
            report.counts.update,
            report.counts.unchanged,
            report.counts.course_relation_mismatch
        ),
        (2, 1, 1)
    );
    assert_eq!(report.error, None);
    server.join().unwrap();
}
