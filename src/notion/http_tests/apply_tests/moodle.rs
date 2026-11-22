use super::super::trial_tests::{READ_BOT, bot, snapshot};
use super::*;

fn moodle_fixture() -> SyncInput {
    use crate::{
        moodle_http::{MoodleExportKind, MoodleFeed},
        secured_sync::SecuredSyncClients,
        sync::acquire_moodle,
        webclass_auth::synthetic::{MOODLE_SCOPE, MOODLE_URL, Scenario, SyntheticUniversity},
    };

    let university = SyntheticUniversity::start(Scenario::MoodleReview);
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
