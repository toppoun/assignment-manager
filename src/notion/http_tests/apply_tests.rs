#[cfg(feature = "synthetic-https-fixture")]
mod moodle;

use std::path::Path;

use rusqlite::Connection;

use super::*;
use crate::notion::{
    apply::{ApplyError, ApplyReport},
    state::{StateError, StateStore, TargetLock},
};

fn exact(expected: SyncKey, rows: Vec<Value>) -> Exchange {
    query(rows, None).check(move |_, body| {
        assert_eq!(
            body["filter"],
            json!({
                "property":"sync","rich_text":{"equals":expected.as_str()}
            })
        );
    })
}

fn apply_exchanges(rows: Vec<Value>) -> Vec<Exchange> {
    let mut script = schema_exchanges(schema_json());
    script.push(query(rows, None).check(|_, body| assert!(body.get("filter").is_none())));
    script
}

fn apply_state() -> tempfile::TempDir {
    let dir = state::test_state_dir();
    StateStore::initialize(&dir.path().join("state.sqlite3"), A).unwrap();
    dir
}

fn open_state(dir: &Path) -> StateStore {
    StateStore::for_apply(&dir.join("state.sqlite3"), A).unwrap()
}

fn apply(
    client: &NotionClient,
    input: &SyncInput,
    mappings: &[CourseMapping],
    dir: &tempfile::TempDir,
) -> ApplyReport {
    client
        .apply(input, &target(), mappings, dir.path())
        .unwrap()
}

fn get_task(response: Value) -> Exchange {
    Exchange::json("GET", format!("/v1/pages/{PAGE_A}"), response)
}

fn patch_task(response: Value, properties: Value) -> Exchange {
    Exchange::json("PATCH", format!("/v1/pages/{PAGE_A}"), response).check(move |_, body| {
        assert_eq!(body, &json!({"properties":properties}));
    })
}

fn get_course() -> Exchange {
    Exchange::json(
        "GET",
        format!("/v1/pages/{COURSE}"),
        page(COURSE, C, C_DB, json!({})),
    )
}

#[test]
fn http_apply_creates_once_and_updates_only_the_changed_properties() {
    let key = key("a");
    let dir = apply_state();
    let mut initial = candidate("a", "Initial");
    initial.deadline = Some(Deadline::DateOnly("2026-11-20".parse().unwrap()));
    let mut renamed = initial.clone();
    renamed.title = "Renamed".into();
    let mut rescheduled = renamed.clone();
    rescheduled.deadline = Some(Deadline::DateOnly("2026-12-01".parse().unwrap()));
    let cleared = candidate("a", "Renamed");
    let mut initial_page = task(PAGE_A, Some(&key), "Initial");
    initial_page["properties"]["期限"]["date"] = json!({"start":"2026-11-20"});
    initial_page["children"] = json!([{"type":"paragraph","content":"private body"}]);
    let mut renamed_page = initial_page.clone();
    renamed_page["properties"]["タイトル（改名済）"]["title"] = json!([text("Renamed")]);
    let mut rescheduled_page = renamed_page.clone();
    rescheduled_page["properties"]["期限"]["date"] = json!({"start":"2026-12-01"});
    let mut cleared_page = rescheduled_page.clone();
    cleared_page["properties"]["期限"]["date"] = Value::Null;

    let mut script = apply_exchanges(vec![task(MANUAL, None, "Manual")]);
    script.push(exact(key.clone(), vec![]));
    let sync_key = key.clone();
    script.push(
        Exchange::json("POST", "/v1/pages", initial_page.clone()).check(move |_, body| {
            assert_eq!(
                body,
                &json!({
                    "parent":{"type":"data_source_id","data_source_id":A},
                    "properties":{
                        "title":{"title":[text("Initial")]},
                        "due":{"date":{"start":"2026-11-20"}},
                        "orig":{"select":{"id":"option-md"}},
                        "sync":{"rich_text":[text(sync_key.as_str())]}
                    }
                })
            );
        }),
    );
    script.extend(apply_exchanges(vec![initial_page.clone()]));
    for (prior, next, properties) in [
        (
            initial_page,
            renamed_page.clone(),
            json!({"title":{"title":[text("Renamed")]}}),
        ),
        (
            renamed_page,
            rescheduled_page.clone(),
            json!({"due":{"date":{"start":"2026-12-01"}}}),
        ),
        (
            rescheduled_page,
            cleared_page.clone(),
            json!({"due":{"date":null}}),
        ),
    ] {
        script.extend(apply_exchanges(vec![prior.clone()]));
        script.push(get_task(prior));
        script.push(patch_task(next, properties));
    }
    script.extend(apply_exchanges(vec![cleared_page]));
    let (client, server) = mock(script);
    let first = apply(&client, &input(vec![initial.clone()]), &[], &dir);
    assert_eq!(first.counts.create, 1);
    assert_eq!(first.pending, 0);
    assert_eq!(
        apply(&client, &input(vec![initial]), &[], &dir)
            .counts
            .unchanged,
        1
    );
    for changed in [renamed, rescheduled, cleared.clone()] {
        assert_eq!(
            apply(&client, &input(vec![changed]), &[], &dir)
                .counts
                .update,
            1
        );
    }
    assert_eq!(
        apply(&client, &input(vec![cleared]), &[], &dir)
            .counts
            .unchanged,
        1
    );
    server.join().unwrap();
    assert_eq!(
        open_state(dir.path())
            .binding(&key)
            .unwrap()
            .unwrap()
            .page_id
            .as_deref(),
        Some(PAGE_A)
    );
}

#[test]
fn http_apply_sets_only_verified_mapped_relations_and_preserves_populated_relations() {
    let key = key("a");
    let dir = apply_state();
    let existing = task(PAGE_A, Some(&key), "Task");
    let mut filled = existing.clone();
    filled["properties"]["授業"]["relation"] = json!([{"id":COURSE}]);
    let mut populated = existing.clone();
    populated["properties"]["授業"]["relation"] = json!([{"id":OLD_COURSE}]);
    let mut renamed = populated.clone();
    renamed["properties"]["タイトル（改名済）"]["title"] = json!([text("Changed")]);
    let mut script = apply_exchanges(vec![]);
    script.push(get_course());
    script.push(exact(key.clone(), vec![]));
    script.push(
        Exchange::json("POST", "/v1/pages", filled.clone()).check(|_, body| {
            assert_eq!(
                body["properties"]["R%3Ael"],
                json!({"relation":[{"id":COURSE}]})
            );
            assert!(body["properties"].get("url").is_none());
        }),
    );
    script.extend(apply_exchanges(vec![existing.clone()]));
    script.push(get_course());
    script.push(get_task(existing));
    script.push(patch_task(
        filled,
        json!({"R%3Ael":{"relation":[{"id":COURSE}]}}),
    ));
    script.extend(apply_exchanges(vec![populated.clone()]));
    script.push(get_task(populated));
    script.push(patch_task(
        renamed,
        json!({"title":{"title":[text("Changed")]}}),
    ));

    let (client, server) = mock(script);
    let (same, course_key) = input_with_course(vec![candidate("a", "Task")]);
    let mappings = [CourseMapping {
        course_key,
        page_id: COURSE.into(),
    }];
    assert_eq!(apply(&client, &same, &mappings, &dir).counts.create, 1);
    assert_eq!(apply(&client, &same, &mappings, &dir).counts.update, 1);
    let (changed, _) = input_with_course(vec![candidate("a", "Changed")]);
    let report = apply(&client, &changed, &mappings, &dir);
    assert_eq!(report.counts.update, 1);
    assert_eq!(report.counts.course_relation_mismatch, 1);
    server.join().unwrap();
}

#[test]
fn http_apply_reports_manual_candidates_and_rechecks_the_key_before_post() {
    let key = key("a");
    let dir = apply_state();
    let input = input(vec![candidate("a", "Task")]);
    let existing = task(PAGE_A, Some(&key), "Task");
    let mut script = apply_exchanges(vec![task(MANUAL, None, "Task")]);
    script.extend(apply_exchanges(vec![]));
    script.push(exact(
        key.clone(),
        vec![existing.clone(), task(PAGE_B, Some(&key), "Duplicate")],
    ));
    script.extend(apply_exchanges(vec![]));
    script.push(exact(key.clone(), vec![existing.clone()]));
    script.extend(apply_exchanges(vec![existing]));
    let (client, server) = mock(script);
    let report = apply(&client, &input, &[], &dir);
    assert_eq!(report.manual_candidates, 1);
    assert_eq!(report.counts.deferred, 1);
    let db = Connection::open(dir.path().join("state.sqlite3")).unwrap();
    let pair: (String, String) = db
        .query_row(
            "SELECT sync_key,page_id FROM manual_candidates",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(pair, (key.as_str().into(), MANUAL.into()));
    assert!(open_state(dir.path()).binding(&key).unwrap().is_none());
    assert_eq!(apply(&client, &input, &[], &dir).counts.conflict, 1);
    assert_eq!(apply(&client, &input, &[], &dir).counts.deferred, 1);
    assert_eq!(apply(&client, &input, &[], &dir).counts.unchanged, 1);
    server.join().unwrap();
}

#[test]
fn http_apply_defers_deleted_moved_trashed_rekeyed_and_manually_edited_pages() {
    let changed_key = key("changed-key");
    let key = key("a");
    let dir = apply_state();
    let input = input(vec![candidate("a", "New")]);
    let prior = task(PAGE_A, Some(&key), "Old");
    let mut moved = prior.clone();
    moved["parent"]["data_source_id"] = json!(C);
    let mut trashed = prior.clone();
    trashed["in_trash"] = json!(true);
    let mut archived = prior.clone();
    archived["is_archived"] = json!(true);
    let mut revised = prior.clone();
    revised["last_edited_time"] = json!("2026-11-20T00:00:01Z");
    let mut formatted = prior.clone();
    formatted["properties"]["タイトル（改名済）"]["title"][0]["annotations"]["bold"] = json!(true);
    let mut relinked = prior.clone();
    relinked["properties"]["授業"]["relation"] = json!([{"id":OLD_COURSE}]);
    let mut rescheduled = prior.clone();
    rescheduled["properties"]["期限"]["date"] = json!({"start":"2026-12-20"});
    let mut script = Vec::new();
    let cases = [
        Exchange::error("GET", format!("/v1/pages/{PAGE_A}"), 404, ""),
        get_task(moved),
        get_task(trashed),
        get_task(archived),
        get_task(revised),
        get_task(formatted),
        get_task(relinked),
        get_task(rescheduled),
        get_task(task(PAGE_A, None, "Old")),
        get_task(task(PAGE_A, Some(&changed_key), "Old")),
        get_task(task(PAGE_A, Some(&key), "Manual edit")),
    ];
    let count = cases.len();
    for exchange in cases {
        script.extend(apply_exchanges(vec![prior.clone()]));
        script.push(exchange);
    }
    script.extend(apply_exchanges(vec![]));
    script.extend(apply_exchanges(vec![task(PAGE_B, Some(&key), "Other")]));
    let (client, server) = mock(script);
    for _ in 0..count + 2 {
        let report = apply(&client, &input, &[], &dir);
        assert_eq!(report.counts.deferred, 1);
        assert_eq!(report.counts.update + report.counts.create, 0);
    }
    server.join().unwrap();
}

#[test]
fn http_apply_checks_ambiguous_patch_by_reading_intended_changes_once() {
    let sync_key = key("a");
    let dir = apply_state();
    let mut changed = candidate("a", "New");
    changed.deadline = Some(Deadline::DateOnly("2026-12-20".parse().unwrap()));
    let (input, course_key) = input_with_course(vec![changed]);
    let mappings = [CourseMapping {
        course_key,
        page_id: COURSE.into(),
    }];
    let mut old = task(PAGE_A, Some(&sync_key), "Old");
    old["properties"]["授業"]["relation"] = json!([{"id":OLD_COURSE}]);
    old["children"] = json!([{"type":"paragraph","content":"private body"}]);
    let mut only_title = old.clone();
    only_title["properties"]["タイトル（改名済）"]["title"] = json!([text("New")]);
    let mut new = only_title.clone();
    new["properties"]["期限"]["date"] = json!({"start":"2026-12-20"});
    let rows = || vec![old.clone(), task(MANUAL, None, "Manual task")];
    let path = format!("/v1/pages/{PAGE_A}");
    let check_patch = |_: &str, body: &Value| {
        assert_eq!(
            body,
            &json!({
                "properties":{
                    "title":{"title":[text("New")]},
                    "due":{"date":{"start":"2026-12-20"}}
                }
            })
        );
    };
    let mut script = Vec::new();
    for (patch, actual) in [
        (Exchange::drop_response("PATCH", &path), new.clone()),
        (Exchange::error("PATCH", &path, 503, ""), new),
        (Exchange::error("PATCH", &path, 503, ""), only_title),
    ] {
        script.extend(apply_exchanges(rows()));
        script.push(get_task(old.clone()));
        script.push(patch.check(check_patch));
        script.push(get_task(actual));
    }
    for status in [401, 403, 404, 409, 429] {
        script.extend(apply_exchanges(rows()));
        script.push(get_task(old.clone()));
        script
            .push(Exchange::error("PATCH", &path, status, "Retry-After: 0\r\n").check(check_patch));
    }
    let (client, server) = mock(script);
    for _ in 0..2 {
        let report = apply(&client, &input, &mappings, &dir);
        assert_eq!(report.counts.update, 1);
        assert_eq!(report.counts.course_relation_mismatch, 1);
        assert_eq!(report.counts.deferred, 0);
        assert_eq!(report.error, None);
    }
    for expected in [
        NotionError::Server(503),
        NotionError::Authentication,
        NotionError::Forbidden,
        NotionError::NotFound,
        NotionError::Conflict,
        NotionError::RateLimited,
    ] {
        let report = apply(&client, &input, &mappings, &dir);
        assert_eq!(report.counts.deferred, 1);
        assert_eq!(report.counts.update, 0);
        assert_eq!(report.counts.course_relation_mismatch, 1);
        assert_eq!(report.error, Some(expected));
        assert_eq!(report.pending, 0);
        assert!(!format!("{report:?}").contains(TOKEN));
    }
    let state = StateStore::read(&dir.path().join("state.sqlite3"), A)
        .unwrap()
        .unwrap();
    assert_eq!(state.bindings.len(), 1);
    assert_eq!(state.bindings[&sync_key].page_id.as_deref(), Some(PAGE_A));
    server.join().unwrap();
}

#[test]
fn http_apply_respects_retry_after_after_mutations_and_retains_ambiguous_creates() {
    let key = key("a");
    let input = input(vec![candidate("a", "Task")]);
    let dir = apply_state();
    let mut script = apply_exchanges(vec![]);
    script.push(exact(key.clone(), vec![]));
    script.push(Exchange::error(
        "POST",
        "/v1/pages",
        429,
        "Retry-After: 1\r\n",
    ));
    script.extend(apply_exchanges(vec![]));
    script.push(exact(key, vec![]));
    script.push(Exchange::error(
        "POST",
        "/v1/pages",
        529,
        "Retry-After: 3600\r\n",
    ));
    let (client, server) = mock(script);
    let first = apply(&client, &input, &[], &dir);
    assert_eq!(first.error, Some(NotionError::RateLimited));
    assert_eq!(first.pending, 0);
    let retry = Instant::now();
    let second = apply(&client, &input, &[], &dir);
    assert!(retry.elapsed() >= Duration::from_millis(900));
    assert_eq!(second.error, Some(NotionError::Overloaded));
    assert_eq!(second.pending, 1);
    let cooled = apply(&client, &input, &[], &dir);
    assert_eq!(cooled.counts.deferred, 1);
    assert_eq!(cooled.error, Some(NotionError::Overloaded));
    assert_eq!(cooled.pending, 1);
    server.join().unwrap();
}

#[test]
fn http_apply_does_not_write_after_incomplete_pagination_or_schema_mismatch() {
    let key = key("a");
    let dir = apply_state();
    open_state(dir.path()).reserve_create(&key).unwrap();
    let db = dir.path().join("state.sqlite3");
    let before = std::fs::read(&db).unwrap();
    let existing = task(PAGE_A, Some(&key), "Task");
    let first = || query(vec![existing.clone()], Some("next"));
    let path = format!("/v1/data_sources/{A}/query");
    let mut script = schema_exchanges(schema_json());
    script.push(first());
    for _ in 0..3 {
        script.push(Exchange::error("POST", &path, 502, "").check(|_, body| {
            assert_eq!(body["start_cursor"], "next");
        }));
    }
    let mut capped = list(vec![existing.clone()], None);
    capped["request_status"] =
        json!({"type":"incomplete","incomplete_reason":"query_result_limit_reached"});
    script.extend(schema_exchanges(schema_json()));
    script.push(Exchange::json("POST", &path, capped));
    for next in [
        query(vec![], Some("next")),
        query(vec![existing.clone()], None),
        Exchange::error("POST", &path, 401, ""),
        Exchange::error("POST", &path, 403, ""),
    ] {
        script.extend(schema_exchanges(schema_json()));
        script.push(first());
        script.push(next.check(|_, body| assert_eq!(body["start_cursor"], "next")));
    }
    script.extend(schema_exchanges(json!({})));
    let (client, server) = mock(script);
    let input = input(vec![candidate("a", "Needs update"), candidate("b", "New")]);
    for expected in [
        NotionError::Server(502),
        NotionError::IncompleteSnapshot,
        NotionError::IncompleteSnapshot,
        NotionError::IncompleteSnapshot,
        NotionError::Authentication,
        NotionError::Forbidden,
        NotionError::SchemaMismatch,
    ] {
        let report = apply(&client, &input, &[], &dir);
        assert_eq!(report.error, Some(expected));
        assert_eq!(report.pending, 1);
        assert_eq!(
            report.counts,
            DryRunCounts {
                deferred: 2,
                ..DryRunCounts::default()
            }
        );
        assert!(!format!("{report:?}").contains(TOKEN));
        assert_eq!(std::fs::read(&db).unwrap(), before);
    }
    server.join().unwrap();
}

#[test]
fn http_apply_does_not_write_when_the_relation_read_fails() {
    let sync_key = key("a");
    let dir = apply_state();
    let db = dir.path().join("state.sqlite3");
    let before = std::fs::read(&db).unwrap();
    let row = task(PAGE_A, Some(&sync_key), "Old");
    let mut overflow = row.clone();
    overflow["properties"]["授業"]["has_more"] = json!(true);
    let mut script = apply_exchanges(vec![overflow]);
    script.push(Exchange::error(
        "GET",
        format!("/v1/pages/{PAGE_A}/properties/R%3Ael"),
        403,
        "",
    ));
    script.extend(apply_exchanges(vec![row]));
    script.push(Exchange::error(
        "GET",
        format!("/v1/pages/{COURSE}"),
        404,
        "",
    ));
    let (client, server) = mock(script);
    let (source, course_key) =
        input_with_course(vec![candidate("a", "New"), candidate("b", "New")]);
    let mappings = [CourseMapping {
        course_key,
        page_id: COURSE.into(),
    }];
    for error in [NotionError::Forbidden, NotionError::NotFound] {
        let report = apply(&client, &source, &mappings, &dir);
        assert_eq!(report.error, Some(error));
        assert_eq!(
            report.counts,
            DryRunCounts {
                deferred: 2,
                ..DryRunCounts::default()
            }
        );
        assert_eq!(std::fs::read(&db).unwrap(), before);
    }
    server.join().unwrap();
}

#[test]
fn http_apply_retries_only_the_incomplete_query_page_within_retry_after_limits() {
    let dir = apply_state();
    let db = dir.path().join("state.sqlite3");
    let before = std::fs::read(&db).unwrap();
    let source = input(vec![candidate("a", "New"), candidate("b", "New")]);
    let path = format!("/v1/data_sources/{A}/query");
    for (status, error) in [
        (429, NotionError::RateLimited),
        (529, NotionError::Overloaded),
    ] {
        for (delays, waited) in [
            (
                &[
                    "Retry-After: 1\r\n",
                    "Retry-After: 0\r\n",
                    "Retry-After: 0\r\n",
                ][..],
                true,
            ),
            (&["Retry-After: 3600\r\n"][..], false),
        ] {
            let mut script = schema_exchanges(schema_json());
            script.push(query(
                vec![task(PAGE_A, Some(&key("a")), "Old")],
                Some("next"),
            ));
            for &delay in delays {
                script.push(
                    Exchange::error("POST", &path, status, delay).check(|_, body| {
                        assert_eq!(body["start_cursor"], "next");
                    }),
                );
            }
            let (client, server) = mock(script);
            let started = Instant::now();
            let report = apply(&client, &source, &[], &dir);
            if waited {
                assert!(started.elapsed() >= Duration::from_secs(1));
            }
            assert_eq!(report.error, Some(error));
            assert_eq!(
                report.counts,
                DryRunCounts {
                    deferred: 2,
                    ..DryRunCounts::default()
                }
            );
            assert_eq!(std::fs::read(&db).unwrap(), before);
            server.join().unwrap();
        }
    }
}

#[test]
fn http_apply_checks_destination_lock_and_database_before_network() {
    let dir = apply_state();
    let initial = input(vec![candidate("a", "Task")]);
    let real = NotionClient::new(TOKEN).unwrap();
    let bogus = dir.path().join("must-not-be-created");
    assert_eq!(
        real.apply(&initial, &target(), &[], &bogus),
        Err(ApplyError::Notion(NotionError::MethodDisabled))
    );
    assert!(!bogus.exists());
    assert!(matches!(
        NotionClient::for_mock("192.0.2.1:1234".parse().unwrap(), TOKEN, Duration::ZERO),
        Err(NotionError::Configuration)
    ));
    let (client, server) = mock(vec![]);
    let lock = TargetLock::acquire(dir.path()).unwrap();
    assert_eq!(
        client.apply(&initial, &target(), &[], dir.path()),
        Err(ApplyError::State(StateError::Busy))
    );
    drop(lock);
    let mut wrong_target = target();
    wrong_target.assignments_data_source_id = PAGE_B.into();
    let path = dir.path().join("state.sqlite3");
    let original = std::fs::read(&path).unwrap();
    assert_eq!(
        client.apply(&initial, &wrong_target, &[], dir.path()),
        Err(ApplyError::State(StateError::InvalidDatabase))
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    let db = Connection::open(&path).unwrap();
    let page_size: i64 = db
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .unwrap();
    let bindings_page: i64 = db
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name='bindings'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(db);
    // Damage a table B-tree page, leaving the database and application headers intact.
    let mut corrupt = original;
    corrupt[usize::try_from((bindings_page - 1) * page_size).unwrap()] = 0;
    std::fs::write(&path, &corrupt).unwrap();
    assert_eq!(
        client.apply(&initial, &target(), &[], dir.path()),
        Err(ApplyError::State(StateError::InvalidDatabase))
    );
    assert_eq!(std::fs::read(&path).unwrap(), corrupt);
    std::fs::write(&path, b"broken SQLite").unwrap();
    assert_eq!(
        client.apply(&initial, &target(), &[], dir.path()),
        Err(ApplyError::State(StateError::InvalidDatabase))
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"broken SQLite");
    server.join().unwrap();
}

#[test]
fn http_apply_distinguishes_explicit_create_rejection_from_server_or_invalid_success() {
    let key = key("a");
    let source = input(vec![candidate("a", "Task")]);
    for (exchange, error, pending) in [
        (
            Exchange::error("POST", "/v1/pages", 400, ""),
            NotionError::Rejected(400),
            0,
        ),
        (
            Exchange::error("POST", "/v1/pages", 401, ""),
            NotionError::Authentication,
            0,
        ),
        (
            Exchange::error("POST", "/v1/pages", 403, ""),
            NotionError::Forbidden,
            0,
        ),
        (
            Exchange::error("POST", "/v1/pages", 404, ""),
            NotionError::NotFound,
            0,
        ),
        (
            Exchange::error("POST", "/v1/pages", 409, ""),
            NotionError::Conflict,
            0,
        ),
        (
            Exchange::error("POST", "/v1/pages", 500, ""),
            NotionError::Server(500),
            1,
        ),
        (
            Exchange::json("POST", "/v1/pages", task(PAGE_A, None, "Manual")),
            NotionError::InvalidResponse,
            1,
        ),
    ] {
        let dir = apply_state();
        let mut script = apply_exchanges(vec![]);
        script.push(exact(key.clone(), vec![]));
        script.push(exchange);
        let (client, server) = mock(script);
        let report = apply(&client, &source, &[], &dir);
        assert_eq!(report.error, Some(error));
        assert_eq!(report.counts.deferred, 1);
        assert_eq!(report.pending, pending);
        assert!(!format!("{report:?}").contains(TOKEN));
        server.join().unwrap();
    }
}

#[test]
fn http_apply_preserves_partial_success_when_an_independent_page_changes() {
    let (existing, new) = if key("a") < key("b") {
        ("a", "b")
    } else {
        ("b", "a")
    };
    let existing_key = key(existing);
    let new_key = key(new);
    let source = input(vec![
        candidate(existing, "Desired title"),
        candidate(new, "Create"),
    ]);
    let mut script = apply_exchanges(vec![task(PAGE_A, Some(&existing_key), "Old title")]);
    script.push(get_task(task(PAGE_A, Some(&existing_key), "Manual edit")));
    script.push(exact(new_key.clone(), vec![]));
    script.push(Exchange::json(
        "POST",
        "/v1/pages",
        task(PAGE_B, Some(&new_key), "Create"),
    ));
    let (client, server) = mock(script);
    let report = apply(&client, &source, &[], &apply_state());
    assert_eq!(report.counts.create, 1);
    assert_eq!(report.counts.deferred, 1);
    assert_eq!(report.error, Some(NotionError::Conflict));
    server.join().unwrap();
}

#[test]
fn write_transport_itself_blocks_prepared_post_and_patch_for_the_real_origin() {
    let (mock, server) = mock(schema_exchanges(schema_json()));
    let schema = mock.retrieve_schema(&target()).unwrap();
    let real = NotionClient::new(TOKEN).unwrap();
    let input = input(vec![candidate("a", "New")]);
    let (existing, _) = schema
        .assignment(&task(PAGE_A, Some(&key("a")), "Old"))
        .unwrap();
    for rows in [&[][..], &[existing]] {
        let plan = plan_sync(&input, NotionSnapshot::Complete(rows), &[]);
        let request = schema.prepare(&plan[0]).unwrap().unwrap();
        assert_eq!(
            real.send_write(&request, &super::super::apply::WriteAccess::Mock),
            Err(NotionError::MethodDisabled)
        );
    }
    server.join().unwrap();
}
