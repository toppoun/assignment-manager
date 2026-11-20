use std::{
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
};

use rusqlite::Connection;

use super::*;
use crate::notion::{
    apply::{ApplyError, ApplyReport},
    state::{BindingStatus, StateError, StateStore, TargetLock},
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
    assert_eq!(apply(&client, &input, &[], &dir).counts.conflict, 1);
    assert_eq!(apply(&client, &input, &[], &dir).counts.deferred, 1);
    assert_eq!(apply(&client, &input, &[], &dir).counts.unchanged, 1);
    server.join().unwrap();
}

#[test]
fn http_apply_recovers_lost_create_response_without_resending() {
    let key = key("a");
    let dir = apply_state();
    let input = input(vec![candidate("a", "Task")]);
    let db = dir.path().join("state.sqlite3");
    let reserved = key.clone();
    let mut script = apply_exchanges(vec![]);
    script.push(exact(key.clone(), vec![]));
    script.push(
        Exchange::drop_response("POST", "/v1/pages").check(move |_, _| {
            let state = StateStore::read(&db, A).unwrap().unwrap();
            assert_eq!(
                state.bindings[&reserved].status,
                BindingStatus::CreateIntent
            );
        }),
    );
    script.extend(apply_exchanges(vec![]));
    script.extend(apply_exchanges(vec![
        task(PAGE_A, Some(&key), "Task"),
        task(PAGE_B, Some(&key), "Duplicate"),
    ]));
    script.extend(apply_exchanges(vec![task(PAGE_A, Some(&key), "Task")]));
    let (client, server) = mock(script);
    let report = apply(&client, &input, &[], &dir);
    assert_eq!(report.pending, 1);
    assert_eq!(report.counts.deferred, 1);
    assert_eq!(report.error, Some(NotionError::Transport));
    assert_eq!(apply(&client, &input, &[], &dir).counts.deferred, 1);
    let conflict = apply(&client, &input, &[], &dir);
    assert_eq!(conflict.counts.conflict, 1);
    assert_eq!(conflict.pending, 1);
    let recovered = apply(&client, &input, &[], &dir);
    assert_eq!(recovered.recovered, 1);
    assert_eq!(recovered.pending, 0);
    assert_eq!(recovered.counts.unchanged, 1);
    server.join().unwrap();
}

#[test]
fn http_apply_recovers_after_post_succeeds_but_sqlite_cannot_confirm() {
    let key = key("a");
    let dir = apply_state();
    let conn = Connection::open(dir.path().join("state.sqlite3")).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER test_confirm_failure BEFORE UPDATE ON bindings WHEN NEW.status='linked'
         BEGIN SELECT RAISE(FAIL, 'synthetic database failure'); END;",
    )
    .unwrap();
    let input = input(vec![candidate("a", "Task")]);
    let existing = task(PAGE_A, Some(&key), "Task");
    let mut script = apply_exchanges(vec![]);
    script.push(exact(key.clone(), vec![]));
    script.push(Exchange::json("POST", "/v1/pages", existing.clone()));
    script.extend(apply_exchanges(vec![existing]));
    let (client, server) = mock(script);
    assert_eq!(
        client.apply(&input, &target(), &[], dir.path()),
        Err(ApplyError::State(StateError::Database))
    );
    assert_eq!(
        open_state(dir.path())
            .binding(&key)
            .unwrap()
            .unwrap()
            .status,
        BindingStatus::CreateIntent
    );
    conn.execute_batch("DROP TRIGGER test_confirm_failure")
        .unwrap();
    let recovered = apply(&client, &input, &[], &dir);
    assert_eq!(recovered.recovered, 1);
    assert_eq!(recovered.counts.unchanged, 1);
    server.join().unwrap();
}

#[test]
fn http_apply_child_when_requested() {
    let Some(dir) = std::env::var_os("AM_TEST_NOTION_APPLY_DIR") else {
        return;
    };
    let address = std::env::var("AM_TEST_NOTION_APPLY_ADDRESS")
        .unwrap()
        .parse()
        .unwrap();
    let client = NotionClient::for_mock(address, TOKEN, Duration::from_secs(15)).unwrap();
    client
        .apply(
            &input(vec![candidate("a", "Task")]),
            &target(),
            &[],
            Path::new(&dir),
        )
        .unwrap();
}

#[test]
fn http_apply_recovers_from_process_killed_after_post_was_received() {
    let key = key("a");
    let dir = apply_state();
    let (received, arrived) = mpsc::channel();
    let (finish, hold) = mpsc::channel();
    let mut script = apply_exchanges(vec![]);
    script.push(exact(key.clone(), vec![]));
    script.push(
        Exchange::drop_response("POST", "/v1/pages").check(move |_, _| {
            received.send(()).unwrap();
            hold.recv_timeout(Duration::from_secs(5)).unwrap();
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (client, server) = serve(listener, script, Duration::from_secs(2));
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "notion::http_tests::apply_tests::http_apply_child_when_requested",
        ])
        .env("AM_TEST_NOTION_APPLY_DIR", dir.path())
        .env("AM_TEST_NOTION_APPLY_ADDRESS", address.to_string())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let arrival = arrived.recv_timeout(Duration::from_secs(5));
    if arrival.is_ok() {
        assert_eq!(
            client.apply(&SyncInput::default(), &target(), &[], dir.path()),
            Err(ApplyError::State(StateError::Busy))
        );
    }
    let _ = child.kill();
    let status = child.wait().unwrap();
    arrival.unwrap();
    assert!(!status.success());
    finish.send(()).unwrap();
    server.join().unwrap();
    assert_eq!(
        open_state(dir.path())
            .binding(&key)
            .unwrap()
            .unwrap()
            .status,
        BindingStatus::CreateIntent
    );
    let (client, server) = mock(apply_exchanges(vec![task(PAGE_A, Some(&key), "Task")]));
    let recovered = apply(&client, &input(vec![candidate("a", "Task")]), &[], &dir);
    assert_eq!(recovered.recovered, 1);
    assert_eq!(recovered.counts.unchanged, 1);
    server.join().unwrap();
}

#[test]
fn http_apply_defers_deleted_moved_trashed_rekeyed_and_manually_edited_pages() {
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
    let mut script = Vec::new();
    let cases = [
        Exchange::error("GET", format!("/v1/pages/{PAGE_A}"), 404, ""),
        get_task(moved),
        get_task(trashed),
        get_task(archived),
        get_task(revised),
        get_task(formatted),
        get_task(relinked),
        get_task(task(PAGE_A, None, "Old")),
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
    let input = input(vec![candidate("a", "New")]);
    let old = task(PAGE_A, Some(&sync_key), "Old");
    let new = task(PAGE_A, Some(&sync_key), "New");
    let path = format!("/v1/pages/{PAGE_A}");
    let mut script = apply_exchanges(vec![old.clone()]);
    script.push(get_task(old.clone()));
    script.push(Exchange::drop_response("PATCH", &path));
    script.push(get_task(new));
    script.extend(apply_exchanges(vec![old.clone()]));
    script.push(get_task(old.clone()));
    script.push(Exchange::error("PATCH", &path, 503, ""));
    script.push(get_task(task(PAGE_A, Some(&sync_key), "Manual")));
    for status in [401, 403, 404, 409, 429] {
        script.extend(apply_exchanges(vec![old.clone()]));
        script.push(get_task(old.clone()));
        script.push(Exchange::error(
            "PATCH",
            &path,
            status,
            "Retry-After: 0\r\n",
        ));
    }
    let (client, server) = mock(script);
    assert_eq!(apply(&client, &input, &[], &dir).counts.update, 1);
    for expected in [
        NotionError::Server(503),
        NotionError::Authentication,
        NotionError::Forbidden,
        NotionError::NotFound,
        NotionError::Conflict,
        NotionError::RateLimited,
    ] {
        let report = apply(&client, &input, &[], &dir);
        assert_eq!(report.counts.deferred, 1);
        assert_eq!(report.error, Some(expected));
        assert_eq!(report.pending, 0);
    }
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
    let mut script = schema_exchanges(schema_json());
    script.push(query(vec![task(PAGE_A, Some(&key), "Task")], Some("next")));
    for _ in 0..3 {
        script.push(
            Exchange::error("POST", format!("/v1/data_sources/{A}/query"), 502, "").check(
                |_, body| {
                    assert_eq!(body["start_cursor"], "next");
                },
            ),
        );
    }
    script.extend(schema_exchanges(json!({})));
    let (client, server) = mock(script);
    let input = input(vec![candidate("a", "Task"), candidate("b", "New")]);
    for expected in [NotionError::Server(502), NotionError::SchemaMismatch] {
        let report = apply(&client, &input, &[], &dir);
        assert_eq!(report.error, Some(expected));
        assert_eq!(report.pending, 1);
        assert_eq!(report.counts.deferred, 2);
    }
    server.join().unwrap();
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
        assert_eq!(real.send_write(&request), Err(NotionError::MethodDisabled));
    }
    server.join().unwrap();
}
