mod apply_tests;
#[cfg(feature = "synthetic-https-fixture")]
mod course_trial;
#[cfg(feature = "synthetic-https-fixture")]
mod moodle_integration;
mod moodle_live_create;
pub(super) mod trial_tests;

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use reqwest::Method;
use serde_json::{Value, json};

use super::{
    tests::{candidate, input, input_with_course, key},
    *,
};
use crate::domain::Deadline;

const TOKEN: &str = "synthetic-notion-secret";
const A: &str = "00000000-0000-4000-8000-000000000001";
const A_DB: &str = "00000000-0000-4000-8000-000000000002";
const C: &str = "00000000-0000-4000-8000-000000000003";
const C_DB: &str = "00000000-0000-4000-8000-000000000004";
const PAGE_A: &str = "00000000-0000-4000-8000-000000000005";
const PAGE_B: &str = "00000000-0000-4000-8000-000000000006";
const MANUAL: &str = "00000000-0000-4000-8000-000000000007";
const COURSE: &str = "00000000-0000-4000-8000-000000000008";
const OLD_COURSE: &str = "00000000-0000-4000-8000-000000000009";
const PAGE_C: &str = "00000000-0000-4000-8000-00000000000a";
const PAGE_D: &str = "00000000-0000-4000-8000-00000000000b";

type Inspect = Box<dyn FnOnce(&str, &Value) + Send>;
struct Exchange {
    method: &'static str,
    path: String,
    status: Option<u16>,
    headers: &'static str,
    body: String,
    inspect: Option<Inspect>,
}

impl Exchange {
    fn json(method: &'static str, path: impl Into<String>, body: Value) -> Self {
        Self {
            method,
            path: path.into(),
            status: Some(200),
            headers: "",
            body: body.to_string(),
            inspect: None,
        }
    }

    fn check(mut self, inspect: impl FnOnce(&str, &Value) + Send + 'static) -> Self {
        self.inspect = Some(Box::new(inspect));
        self
    }

    fn error(
        method: &'static str,
        path: impl Into<String>,
        status: u16,
        headers: &'static str,
    ) -> Self {
        let mut exchange = Self::json(method, path, json!({"message":TOKEN}));
        exchange.status = Some(status);
        exchange.headers = headers;
        exchange
    }

    fn drop_response(method: &'static str, path: impl Into<String>) -> Self {
        let mut exchange = Self::json(method, path, Value::Null);
        exchange.status = None;
        exchange
    }

    fn handle(self, mut stream: TcpStream) {
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut tokens = line.split_whitespace();
        assert_eq!(tokens.next(), Some(self.method));
        let target = tokens.next().unwrap();
        assert_eq!(target.split('?').next(), Some(self.path.as_str()));

        let mut authorization = false;
        let mut version = false;
        let mut length = 0;
        loop {
            let mut header = String::new();
            assert_ne!(reader.read_line(&mut header).unwrap(), 0);
            if header == "\r\n" {
                break;
            }
            let (name, value) = header.split_once(':').unwrap();
            match name.to_ascii_lowercase().as_str() {
                "authorization" => authorization = value.trim() == format!("Bearer {TOKEN}"),
                "notion-version" => version = value.trim() == "2026-03-11",
                "content-length" => length = value.trim().parse().unwrap(),
                _ => {}
            }
        }
        assert!(authorization && version, "missing auth or API version");
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes).unwrap();
        let body = if length == 0 {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        if let Some(inspect) = self.inspect {
            inspect(target, &body);
        }
        if let Some(status) = self.status {
            let _ = write!(
                stream,
                "HTTP/1.1 {status} Mock\r\nConnection: close\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\n\r\n{}",
                self.headers,
                self.body.len(),
                self.body
            );
        }
    }
}

struct MockServer {
    finished: mpsc::Sender<()>,
    thread: JoinHandle<()>,
}

impl MockServer {
    fn join(self) -> thread::Result<()> {
        let _ = self.finished.send(());
        self.thread.join()
    }
}

fn mock(script: Vec<Exchange>) -> (NotionClient, MockServer) {
    mock_with_timeout(script, Duration::from_secs(2))
}

fn mock_with_timeout(script: Vec<Exchange>, timeout: Duration) -> (NotionClient, MockServer) {
    serve(TcpListener::bind("127.0.0.1:0").unwrap(), script, timeout)
}

fn serve(
    listener: TcpListener,
    script: Vec<Exchange>,
    timeout: Duration,
) -> (NotionClient, MockServer) {
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (finished, test) = mpsc::channel();
    let thread = thread::spawn(move || {
        for exchange in script {
            let start = Instant::now();
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        exchange.handle(stream);
                        break;
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && start.elapsed() < Duration::from_secs(4) =>
                    {
                        thread::sleep(Duration::from_millis(2));
                    }
                    _ => panic!("expected mock request did not arrive"),
                }
            }
        }
        // Keep accepting connections until the test has finished its client calls.
        // Closing the listener earlier would hide an unexpected write attempt.
        loop {
            let finished = !matches!(
                test.recv_timeout(Duration::from_millis(2)),
                Err(mpsc::RecvTimeoutError::Timeout)
            );
            assert!(
                matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
                "unexpected extra mock request",
            );
            if finished {
                break;
            }
        }
    });
    (
        NotionClient::for_mock(address, TOKEN, timeout).unwrap(),
        MockServer { finished, thread },
    )
}

fn target() -> NotionTarget {
    NotionTarget {
        assignments_data_source_id: A.into(),
        courses_data_source_id: C.into(),
        properties: NotionPropertyIds {
            title: "title".into(),
            deadline: "due".into(),
            course: "R%3Ael".into(),
            submission_url: "url".into(),
            sync_key: "sync".into(),
            source: "orig".into(),
        },
    }
}

fn source(id: &str, database: &str, properties: Value) -> Value {
    json!({
        "object":"data_source", "id":id, "in_trash":false,
        "parent":{"type":"database_id", "database_id":database},
        "properties":properties
    })
}

fn schema_json() -> Value {
    source(
        A,
        A_DB,
        json!({
            "課題名（改名済）":{"id":"title", "type":"title","title":{}},
            "期限":{"id":"due", "type":"date","date":{}},
            "授業":{"id":"R%3Ael","type":"relation","relation":{
                "data_source_id":C,"database_id":C_DB,"type":"single_property","single_property":{}
            }},
            "提出URL":{"id":"url","type":"url","url":{}},
            "同期キー":{"id":"sync","type":"rich_text","rich_text":{}},
            "取得元":{"id":"orig","type":"select","select":{"options":[
                {"id":"option-md", "name":"Moodle"}, {"id":"option-wc", "name":"WebClass"}
            ]}},
            "状態":{"id":"private", "type":"status","status":{}}
        }),
    )
}

fn schema_exchanges(data: Value) -> Vec<Exchange> {
    vec![
        Exchange::json("GET", format!("/v1/data_sources/{A}"), data),
        Exchange::json(
            "GET",
            format!("/v1/data_sources/{C}"),
            source(C, C_DB, json!({})),
        ),
    ]
}

fn list(results: Vec<Value>, cursor: Option<&str>) -> Value {
    json!({"object":"list","results":results,"has_more":cursor.is_some(),"next_cursor":cursor})
}

fn query(results: Vec<Value>, cursor: Option<&str>) -> Exchange {
    Exchange::json(
        "POST",
        format!("/v1/data_sources/{A}/query"),
        list(results, cursor),
    )
}

fn page(id: &str, source: &str, database: &str, properties: Value) -> Value {
    json!({
        "object":"page","id":id,"in_trash":false,"properties":properties,
        "last_edited_time":"2026-11-20T00:00:00Z",
        "parent":{"type":"data_source_id","data_source_id":source,"database_id":database}
    })
}

fn text(text: &str) -> Value {
    json!({"type":"text","text":{"content":text}})
}

fn task(id: &str, key: Option<&SyncKey>, title: &str) -> Value {
    let rich_text: Vec<Value> = key.into_iter().map(|key| text(key.as_str())).collect();
    page(
        id,
        A,
        A_DB,
        json!({
            "タイトル（改名済）":{"id":"title","type":"title","title":[text(title)]},
            "同期キー":{"id":"sync","type":"rich_text","rich_text":rich_text},
            "期限":{"id":"due","type":"date","date":null},
            "取得元":{"id":"orig","type":"select","select":{"id":"option-md", "name":"Moodle"}},
            "授業":{"id":"R%3Ael","type":"relation","relation":[],"has_more":false},
            "提出URL":{"id":"url","type":"url","url":"https://example.invalid/?synthetic-secret"},
            "状態":{"id":"private","type":"status","status":{"name":"Done"}},
            "優先度":{"id":"urgency","type":"select","select":{"name":"High"}},
            "メモ":{"id":"notes","type":"rich_text","rich_text":[text("protected")]},
            "繰り返し":{"id":"repeat","type":"checkbox","checkbox":true}
        }),
    )
}

fn item_list(ids: &[&str], cursor: Option<&str>) -> Value {
    let results = ids
        .iter()
        .map(|id| {
            json!({
                "object":"property_item","type":"relation","id":"R%3Ael","relation":{"id":id}
            })
        })
        .collect();
    let mut list = list(results, cursor);
    list["type"] = json!("property_item");
    // Deliberately unusable next_url: the reader only follows the opaque cursor.
    list["property_item"] =
        json!({"id":"R%3Ael","type":"relation","next_url":"https://example.invalid/"});
    list
}

#[test]
fn http_dry_run_previews_safe_relation_patches_after_complete_pagination() {
    let a = key("a");
    let b = key("b");
    let c = key("c");
    let d = key("d");
    let (input, course_key) = input_with_course(vec![
        candidate("a", "New task"),
        candidate("b", "New title"),
        candidate("c", "Fill course"),
        candidate("d", "Keep course"),
    ]);
    let mapping = CourseMapping {
        course_key,
        page_id: COURSE.into(),
    };
    let mut manual = task(MANUAL, None, "New task");
    manual["properties"]["授業"]["relation"] = json!([{"id":OLD_COURSE}]);
    let empty = task(PAGE_C, Some(&c), "Fill course");
    let mut mismatch = task(PAGE_D, Some(&d), "Keep course");
    mismatch["properties"]["授業"]["relation"] = json!([{"id":OLD_COURSE}]);
    let mut original = task(PAGE_B, Some(&b), "Old title");
    original["properties"]["期限"]["date"] =
        json!({"start":"2026-11-20","end":null,"time_zone":null});
    original["properties"]["授業"]["relation"] = json!([{"id":COURSE}]);
    original["properties"]["授業"]["has_more"] = json!(true);

    let mut script = schema_exchanges(schema_json());
    script.push(
        query(vec![manual.clone()], Some("cursor/value")).check(|target, body| {
            assert!(target.contains("filter_properties=R%253Ael"));
            assert_eq!(body["result_type"], "page");
            assert!(body.get("start_cursor").is_none());
        }),
    );
    script.push(
        query(
            vec![original.clone(), empty.clone(), mismatch.clone()],
            None,
        )
        .check(|_, body| {
            assert_eq!(body["start_cursor"], "cursor/value");
        }),
    );
    let property_path = format!("/v1/pages/{PAGE_B}/properties/R%3Ael");
    script.push(Exchange::json(
        "GET",
        &property_path,
        item_list(&[COURSE], Some("relation page")),
    ));
    script.push(
        Exchange::json("GET", &property_path, item_list(&[OLD_COURSE], None)).check(|target, _| {
            assert!(target.contains("start_cursor=relation+page"));
        }),
    );
    script.push(Exchange::json(
        "GET",
        format!("/v1/pages/{COURSE}"),
        page(COURSE, C, C_DB, json!({})),
    ));

    let mut updated = original.clone();
    updated["properties"]["タイトル（改名済）"]["title"] = json!([text("New title")]);
    updated["properties"]["期限"]["date"] = Value::Null;
    updated["properties"]["授業"]["relation"] = json!([{"id":COURSE}, {"id":OLD_COURSE}]);
    updated["properties"]["授業"]["has_more"] = json!(false);
    let mut filled = empty;
    filled["properties"]["授業"]["relation"] = json!([{"id":COURSE}]);
    let mut created = task(PAGE_A, Some(&a), "New task");
    created["properties"]["授業"]["relation"] = json!([{"id":COURSE}]);

    // Both dry runs permit only schema, query, and property/page reads.
    script.extend(schema_exchanges(schema_json()));
    script.push(query(
        vec![manual, created, updated, filled, mismatch],
        None,
    ));
    let (client, server) = mock(script);

    let report = client.dry_run(&input, &target(), std::slice::from_ref(&mapping));
    assert_eq!(report.error(), None);
    assert_eq!(
        report.to_string(),
        "Create=1 Update=2 Unchanged=1 Conflict=0 Deferred=0 course_relation_mismatch=2"
    );
    assert_eq!(report.plan().len(), 4);
    for entry in report.plan() {
        assert_eq!(
            entry.course_relation_mismatch,
            entry.key == b || entry.key == d
        );
    }
    assert_eq!(report.requests().len(), 3);
    let request = |key| report.requests().iter().find(|r| r.key() == key).unwrap();
    let create = request(&a);
    assert_eq!(
        (create.method(), create.path()),
        (&Method::POST, "/v1/pages")
    );
    assert_eq!(
        create.body(),
        &json!({
            "parent":{"type":"data_source_id","data_source_id":A},
            "properties":{
                "title":{"title":[text("New task")]},
                "orig":{"select":{"id":"option-md"}},
                "sync":{"rich_text":[text(a.as_str())]},
                "R%3Ael":{"relation":[{"id":COURSE}]}
            }
        })
    );
    let patch = request(&b);
    assert_eq!(patch.method(), Method::PATCH);
    assert_eq!(patch.path(), format!("/v1/pages/{PAGE_B}"));
    assert_eq!(
        patch.body(),
        &json!({"properties":{
            "title":{"title":[text("New title")]},
            "due":{"date":null}
        }})
    );
    let fill = request(&c);
    assert_eq!(fill.method(), Method::PATCH);
    assert_eq!(fill.path(), format!("/v1/pages/{PAGE_C}"));
    assert_eq!(
        fill.body(),
        &json!({"properties":{
            "R%3Ael":{"relation":[{"id":COURSE}]}
        }})
    );

    let repeat = client.dry_run(&input, &target(), &[mapping]);
    assert_eq!(repeat.error(), None);
    assert_eq!(repeat.counts().unchanged, 4);
    assert_eq!(repeat.counts().course_relation_mismatch, 2);
    assert!(repeat.requests().is_empty());
    server.join().unwrap();
}

#[test]
fn http_errors_are_classified_and_reads_have_a_retry_limit() {
    let path = format!("/v1/pages/{PAGE_A}");
    let error = |code| Exchange::error("GET", &path, code, "");
    let mut script = vec![error(401), error(403), error(404), error(409)];
    script.extend([
        Exchange::error("GET", &path, 429, "Retry-After: 0\r\n"),
        Exchange::error("GET", &path, 529, "Retry-After: 0\r\n"),
        Exchange::json("GET", &path, json!({"success":true})),
        error(500),
        error(502),
        error(504),
        error(503),
        Exchange::json("GET", &path, json!({"success":true})),
        Exchange::error("GET", &path, 302, "Location: http://127.0.0.1:1/other\r\n"),
        Exchange::error("GET", &path, 429, "Retry-After: 3600\r\n"),
    ]);
    let (client, server) = mock(script);
    for expected in [
        NotionError::Authentication,
        NotionError::Forbidden,
        NotionError::NotFound,
        NotionError::Conflict,
    ] {
        let err = client.retrieve_page(PAGE_A, &[]).unwrap_err();
        assert_eq!(err, expected);
        assert!(!format!("{err:?} {err}").contains(TOKEN));
    }
    assert_eq!(client.retrieve_page(PAGE_A, &[]).unwrap()["success"], true);
    assert_eq!(
        client.retrieve_page(PAGE_A, &[]),
        Err(NotionError::Server(504))
    );
    assert_eq!(client.retrieve_page(PAGE_A, &[]).unwrap()["success"], true);
    assert_eq!(
        client.retrieve_page(PAGE_A, &[]),
        Err(NotionError::Rejected(302))
    );
    assert_eq!(
        client.retrieve_page(PAGE_A, &[]),
        Err(NotionError::RateLimited)
    );
    assert_eq!(
        client.retrieve_page(PAGE_A, &[]),
        Err(NotionError::RateLimited)
    );
    assert_eq!(client.read_counts(), (10, 5));
    server.join().unwrap();
}

#[test]
fn http_timeout_is_retried_only_to_the_read_limit() {
    let path = format!("/v1/pages/{PAGE_A}");
    let script = (0..3)
        .map(|_| {
            Exchange::drop_response("GET", &path).check(|_, _| {
                thread::sleep(Duration::from_millis(180));
            })
        })
        .collect();
    let (client, server) = mock_with_timeout(script, Duration::from_millis(150));
    assert_eq!(client.retrieve_page(PAGE_A, &[]), Err(NotionError::Timeout));
    server.join().unwrap();
}

#[test]
fn http_incomplete_query_never_turns_partial_rows_into_creates_or_updates() {
    let input = input(vec![candidate("a", "New"), candidate("b", "New")]);
    let first = || query(vec![task(PAGE_A, Some(&key("a")), "Old")], Some("next"));
    let path = format!("/v1/data_sources/{A}/query");
    let mut capped = list(vec![], None);
    capped["request_status"] =
        json!({"type":"incomplete","incomplete_reason":"query_result_limit_reached"});
    for (next, expected) in [
        (
            Exchange::error("POST", &path, 403, ""),
            NotionError::Forbidden,
        ),
        (
            Exchange::json("POST", &path, capped),
            NotionError::IncompleteSnapshot,
        ),
        (query(vec![], Some("next")), NotionError::IncompleteSnapshot),
    ] {
        let mut script = schema_exchanges(schema_json());
        script.extend([first(), next]);
        let (client, server) = mock(script);
        let result = client.dry_run(&input, &target(), &[]);
        assert_eq!(result.error(), Some(expected));
        assert_eq!(result.counts().deferred, 2);
        assert!(result.requests().is_empty());
        server.join().unwrap();
    }
}

#[test]
fn inspection_and_dry_run_use_one_complete_snapshot_with_manual_rows_and_duplicate_keys() {
    let a = key("a");
    let input = input(vec![
        candidate("a", "Candidate"),
        candidate("b", "Manual title"),
    ]);
    let mut script = schema_exchanges(inspection_schema());
    script.push(query(
        vec![
            task(MANUAL, None, "Manual title"),
            task(PAGE_A, Some(&a), "First"),
        ],
        Some("next"),
    ));
    script.push(query(vec![task(PAGE_B, Some(&a), "Second")], None));
    let (client, server) = mock(script);
    let (inspection, report) = client.inspect_dry_run(&input, &target(), &[], &[]);
    assert_eq!(inspection.error, None);
    assert_eq!(
        inspection.snapshot,
        Some(SnapshotSummary {
            managed: 2,
            manual: 1,
            duplicate_keys: 1
        })
    );
    assert_eq!(report.counts().conflict, 1);
    assert_eq!(report.counts().create, 1);
    assert_eq!(report.requests().len(), 1);
    assert_eq!(report.requests()[0].key(), &key("b"));
    assert!(!format!("{inspection} {report}").contains("Manual title"));
    server.join().unwrap();
}

#[test]
fn inspection_dry_run_retries_only_the_current_query_page_to_a_limit() {
    let input = input(vec![candidate("a", "New"), candidate("b", "Another")]);
    let path = format!("/v1/data_sources/{A}/query");
    for (last, expected) in [
        (200, None),
        (529, Some(NotionError::Overloaded)),
        (429, Some(NotionError::RateLimited)),
    ] {
        let mut script = schema_exchanges(inspection_schema());
        script.push(query(
            vec![task(PAGE_A, Some(&key("a")), "Old")],
            Some("next"),
        ));
        for status in [429, 529, last] {
            let next = if status == 200 {
                query(vec![], None)
            } else {
                Exchange::error("POST", &path, status, "Retry-After: 0\r\n")
            };
            script.push(next.check(|_, body| assert_eq!(body["start_cursor"], "next")));
        }
        let (client, server) = mock(script);
        let (inspection, report) = client.inspect_dry_run(&input, &target(), &[], &[]);
        assert_eq!(report.error(), expected);
        assert_eq!(inspection.snapshot.is_some(), expected.is_none());
        if expected.is_none() {
            assert_eq!((report.counts().create, report.counts().update), (1, 1));
        } else {
            assert_eq!(report.counts().deferred, 2);
            assert!(report.requests().is_empty());
        }
        server.join().unwrap();
    }
}

#[test]
fn inspection_dry_run_fails_closed_on_unauthorized_malformed_or_repeated_results() {
    let input = input(vec![candidate("a", "New")]);
    let path = format!("/v1/data_sources/{A}/query");
    let manual = task(MANUAL, None, "Manual");
    let mut moved = task(PAGE_A, Some(&key("a")), "Moved");
    moved["parent"]["data_source_id"] = json!(C);
    let mut trashed = task(PAGE_A, Some(&key("a")), "Trashed");
    trashed["in_trash"] = json!(true);
    for (next, error) in [
        (
            Exchange::error("POST", &path, 401, ""),
            NotionError::Authentication,
        ),
        (
            Exchange::error("POST", &path, 403, ""),
            NotionError::Forbidden,
        ),
        (
            Exchange::error("POST", &path, 404, ""),
            NotionError::NotFound,
        ),
        (
            Exchange::json(
                "POST",
                &path,
                json!({"object":"list","has_more":false,"next_cursor":null}),
            ),
            NotionError::InvalidResponse,
        ),
        (
            query(vec![], Some("bad\nnext")),
            NotionError::IncompleteSnapshot,
        ),
        (
            query(vec![manual.clone()], None),
            NotionError::IncompleteSnapshot,
        ),
        (query(vec![moved], None), NotionError::IncompleteSnapshot),
        (query(vec![trashed], None), NotionError::IncompleteSnapshot),
    ] {
        let mut script = schema_exchanges(inspection_schema());
        script.push(query(vec![manual.clone()], Some("next")));
        script.push(next);
        let (client, server) = mock(script);
        let (inspection, report) = client.inspect_dry_run(&input, &target(), &[], &[]);
        assert!(inspection.snapshot.is_none());
        assert_eq!(report.error(), Some(error));
        assert_eq!(report.counts().deferred, 1);
        assert!(report.requests().is_empty());
        assert!(!format!("{inspection} {report}").contains(TOKEN));
        server.join().unwrap();
    }
}

#[test]
fn http_relation_cannot_be_replaced_from_an_inconsistent_or_duplicate_projection() {
    let input = input(vec![candidate("a", "New"), candidate("b", "Another")]);
    let mut row = task(PAGE_A, Some(&key("a")), "Old");
    row["properties"]["授業"]["relation"] = json!([{"id":COURSE}]);
    row["properties"]["授業"]["has_more"] = json!(true);
    let path = format!("/v1/pages/{PAGE_A}/properties/R%3Ael");
    for ids in [&[][..], &[OLD_COURSE][..], &[COURSE, COURSE][..]] {
        let mut script = schema_exchanges(inspection_schema());
        script.push(query(vec![row.clone()], None));
        script.push(Exchange::json("GET", &path, item_list(ids, None)));
        let (client, server) = mock(script);
        let (inspection, report) = client.inspect_dry_run(&input, &target(), &[], &[]);
        assert!(inspection.snapshot.is_none());
        assert_eq!(report.error(), Some(NotionError::IncompleteSnapshot));
        assert_eq!(report.counts().deferred, 2);
        assert!(report.requests().is_empty());
        server.join().unwrap();
    }
}

#[test]
fn dry_run_defers_missing_known_pages_and_changed_or_unreconciled_bindings() {
    use super::state::{StateStore, test_state_dir};

    let root = test_state_dir();
    let db = root.path().join("state.sqlite3");
    StateStore::initialize(&db, A).unwrap();
    let state = StateStore::for_apply(&db, A).unwrap();
    state.confirm(&key("missing"), MANUAL).unwrap();
    state.confirm(&key("moved"), PAGE_A).unwrap();
    state.reserve_create(&key("unknown")).unwrap();
    state.mark_unknown(&key("unknown")).unwrap();
    drop(state);
    let original = std::fs::read(&db).unwrap();
    let input = input(
        ["missing", "moved", "wrong-key", "unknown", "fresh"]
            .into_iter()
            .map(|id| candidate(id, "New"))
            .collect(),
    );
    let mut script = schema_exchanges(inspection_schema());
    script.push(query(
        vec![
            task(PAGE_B, Some(&key("moved")), "Old"),
            task(PAGE_A, Some(&key("wrong-key")), "Old"),
            task(OLD_COURSE, Some(&key("unknown")), "New"),
        ],
        None,
    ));
    let (client, server) = mock(script);
    let (_, mut report) = client.inspect_dry_run(&input, &target(), &[], &[]);
    report.defer_unreconciled(&StateStore::read(&db, A).unwrap().unwrap());
    assert_eq!(
        report.counts(),
        DryRunCounts {
            create: 1,
            deferred: 4,
            ..Default::default()
        }
    );
    assert_eq!(report.requests().len(), 1);
    assert_eq!(report.requests()[0].key(), &key("fresh"));
    assert_eq!(std::fs::read(&db).unwrap(), original);
    server.join().unwrap();
}

#[test]
fn http_invalid_schema_and_unrecognized_managed_keys_fail_closed() {
    let input = input(vec![candidate("a", "New")]);
    for (property, replacement) in [
        ("同期キー", json!({"id":"sync","type":"url","url":{}})),
        (
            "授業",
            json!({"id":"R%3Ael","type":"relation","relation":{
                "data_source_id":C,"database_id":A_DB,"type":"single_property"
            }}),
        ),
        (
            "取得元",
            json!({"id":"orig","type":"select","select":{"options":[{"id":"option-md","name":"Moodle"}]}}),
        ),
    ] {
        let mut data = schema_json();
        data["properties"][property] = replacement;
        let (client, server) = mock(schema_exchanges(data));
        let result = client.dry_run(&input, &target(), &[]);
        assert_eq!(result.error(), Some(NotionError::SchemaMismatch));
        assert_eq!(result.counts().deferred, 1);
        assert!(result.requests().is_empty());
        server.join().unwrap();
    }
    let mut script = schema_exchanges(schema_json());
    let mut bad = task(PAGE_A, None, "Existing");
    bad["properties"]["同期キー"]["rich_text"] = json!([text("v2:unknown")]);
    script.push(query(vec![bad], None));
    let (client, server) = mock(script);
    let result = client.dry_run(&input, &target(), &[]);
    assert_eq!(result.error(), Some(NotionError::UnsupportedProperty));
    assert!(result.requests().is_empty());
    server.join().unwrap();
}

#[test]
fn http_duplicate_key_on_a_later_page_is_a_conflict() {
    let a = key("a");
    let input = input(vec![candidate("a", "New")]);
    let mut script = schema_exchanges(schema_json());
    script.push(query(vec![task(PAGE_A, Some(&a), "First")], Some("next")));
    script.push(query(vec![task(PAGE_B, Some(&a), "Second")], None));
    let (client, server) = mock(script);
    let report = client.dry_run(&input, &target(), &[]);
    assert_eq!(report.error(), None);
    assert_eq!(report.counts().conflict, 1);
    assert_eq!(
        report.plan()[0].action,
        PlanAction::Conflict(ConflictReason::DuplicateSyncKey)
    );
    assert!(report.requests().is_empty());
    server.join().unwrap();
}

#[test]
fn http_unreadable_relation_page_suppresses_an_update() {
    let (input, course_key) = input_with_course(vec![candidate("a", "New")]);
    let mappings = [CourseMapping {
        course_key,
        page_id: COURSE.into(),
    }];
    let mut row = task(PAGE_A, Some(&key("a")), "Old");
    row["properties"]["授業"]["has_more"] = json!(true);
    let mut script = schema_exchanges(schema_json());
    script.push(query(vec![row], None));
    script.push(Exchange::error(
        "GET",
        format!("/v1/pages/{PAGE_A}/properties/R%3Ael"),
        404,
        "",
    ));
    let (client, server) = mock(script);
    let report = client.dry_run(&input, &target(), &mappings);
    assert_eq!(report.error(), Some(NotionError::NotFound));
    assert_eq!(report.counts().deferred, 1);
    assert!(report.requests().is_empty());
    server.join().unwrap();
}

#[test]
fn http_deadline_comparison_uses_instants_and_serializes_a_changed_date() {
    let mut dated = candidate("a", "Title");
    dated.deadline = Some(Deadline::DateTime("2026-11-20T01:30:00Z".parse().unwrap()));
    let input = input(vec![dated]);
    let mut row = task(PAGE_A, Some(&key("a")), "Title");
    row["properties"]["期限"]["date"] = json!({"start":"2026-11-20T10:30:00+09:00"});
    let mut prior = row.clone();
    prior["properties"]["期限"]["date"] = json!({"start":"2026-11-20"});
    let mut script = schema_exchanges(schema_json());
    script.push(query(vec![row], None));
    script.extend(schema_exchanges(schema_json()));
    script.push(query(vec![prior], None));
    let (client, server) = mock(script);
    assert_eq!(client.dry_run(&input, &target(), &[]).counts().unchanged, 1);
    let report = client.dry_run(&input, &target(), &[]);
    assert_eq!(report.counts().update, 1);
    assert_eq!(
        report.requests()[0].body(),
        &json!({"properties":{"due":{"date":{"start":"2026-11-20T01:30:00Z"}}}})
    );
    server.join().unwrap();
}

fn inspection_schema() -> Value {
    let mut data = schema_json();
    for (name, id, kind) in [
        ("優先度", "priority", "select"),
        ("科目", "subject", "select"),
        ("種別", "category", "select"),
        ("繰り返し", "recurring", "select"),
        ("メモ", "note", "rich_text"),
    ] {
        data["properties"][name] = json!({"id":id, "type":kind, (kind):{}});
    }
    data
}

#[test]
fn inspection_reports_legacy_schema_and_pages_explicit_relation_items() {
    let mut data = inspection_schema();
    let props = data["properties"].as_object_mut().unwrap();
    props.remove("同期キー");
    props.remove("取得元");
    let mut target = target();
    target.properties.sync_key.clear();
    target.properties.source.clear();

    let mut sample = task(PAGE_A, None, TOKEN);
    sample["properties"]["授業"]["has_more"] = json!(true);
    let mut script = schema_exchanges(data);
    script.push(Exchange::json("GET", format!("/v1/pages/{PAGE_A}"), sample));
    let property = format!("/v1/pages/{PAGE_A}/properties/R%3Ael");
    script.push(Exchange::json(
        "GET",
        &property,
        item_list(&[COURSE], Some("page-2")),
    ));
    script.push(
        Exchange::json("GET", &property, item_list(&[OLD_COURSE], None))
            .check(|path, _| assert!(path.contains("start_cursor=page-2"))),
    );
    script.push(Exchange::json(
        "GET",
        format!("/v1/pages/{COURSE}"),
        page(COURSE, C, C_DB, json!({})),
    ));
    let (client, server) = mock(script);
    let mapping = CourseMapping {
        course_key: crate::import::ExternalRecordKey {
            origin: crate::import::OriginKind::Moodle,
            scope: "default.course-related".into(),
            id: "synthetic-private-course-id".into(),
        },
        page_id: COURSE.into(),
    };
    let report = client.inspect(&target, &[PAGE_A.into()], &[mapping]);
    assert_eq!(report.error, Some(NotionError::SchemaMismatch));
    assert!(report.relation_target_verified);
    assert_eq!(
        (
            report.assignment_samples,
            report.sample_relation_entries,
            report.mapped_course_pages
        ),
        (1, 2, 1)
    );
    let output = report.to_string();
    assert!(output.contains("property.sync_key=missing expected_type=rich_text"));
    assert!(output.contains("property.source=missing expected_type=select"));
    assert!(output.contains("snapshot=unavailable"));
    for private in [
        TOKEN,
        PAGE_A,
        COURSE,
        "synthetic-private-course-id",
        "https://",
    ] {
        assert!(!output.contains(private));
    }
    server.join().unwrap();
}

#[test]
fn inspection_distinguishes_configured_schema_missing_ids_and_invalid_options() {
    let ready = inspection_schema();
    let mut invalid_options = ready.clone();
    invalid_options["properties"]["取得元"]["select"]["options"] =
        json!([{"id":"option-md","name":"Moodle"}]);
    let mut script = schema_exchanges(ready.clone());
    script.push(query(vec![], None));
    script.extend(schema_exchanges(ready));
    script.extend(schema_exchanges(invalid_options));
    let (client, server) = mock(script);

    let mut target = target();
    let report = client.inspect(&target, &[], &[]);
    assert_eq!(report.error, None);
    assert_eq!(
        report.snapshot,
        Some(SnapshotSummary {
            managed: 0,
            manual: 0,
            duplicate_keys: 0
        })
    );
    let key_id = std::mem::take(&mut target.properties.sync_key);
    let report = client.inspect(&target, &[], &[]);
    assert_eq!(report.error, Some(NotionError::SchemaMismatch));
    assert!(
        report
            .to_string()
            .contains("property.sync_key=id_not_configured")
    );
    target.properties.sync_key = key_id;
    let report = client.inspect(&target, &[], &[]);
    assert_eq!(report.error, Some(NotionError::SchemaMismatch));
    assert!(
        report
            .to_string()
            .contains("property.source=invalid_options")
    );
    server.join().unwrap();
}

#[test]
fn inspection_preserves_scope_diagnostics_and_rejects_failed_page_reads() {
    let mut script = vec![
        Exchange::error("GET", format!("/v1/data_sources/{A}"), 401, ""),
        Exchange::json("GET", format!("/v1/data_sources/{A}"), inspection_schema()),
        Exchange::error("GET", format!("/v1/data_sources/{C}"), 403, ""),
    ];
    script.extend(schema_exchanges(inspection_schema()));
    let assignment = task(PAGE_A, None, "First sample");
    script.push(Exchange::json(
        "GET",
        format!("/v1/pages/{PAGE_A}"),
        assignment,
    ));
    script.push(Exchange::error(
        "GET",
        format!("/v1/pages/{PAGE_B}"),
        404,
        "",
    ));
    let (client, server) = mock(script);
    let target = target();
    assert_eq!(
        client.inspect(&target, &[], &[]).error,
        Some(NotionError::Authentication)
    );
    let forbidden = client.inspect(&target, &[], &[]);
    assert_eq!(forbidden.error, Some(NotionError::Forbidden));
    assert!(forbidden.assignments_source_verified && !forbidden.courses_source_verified);
    let incomplete = client.inspect(&target, &[PAGE_A.into(), PAGE_B.into()], &[]);
    assert_eq!(incomplete.error, Some(NotionError::NotFound));
    assert_eq!(incomplete.assignment_samples, 1);
    let input = input(vec![candidate("a", "Fixture")]);
    let plan = DryRunReport::incomplete(&input, incomplete.error.unwrap());
    assert_eq!(plan.counts().deferred, 1);
    assert!(plan.requests().is_empty());
    server.join().unwrap();
}

#[test]
fn read_transport_rejects_write_endpoints_and_other_routes_before_network() {
    let (client, server) = mock(vec![query(vec![], None)]);
    let query_path = format!("/v1/data_sources/{A}/query");
    for (method, path) in [
        (Method::POST, "/v1/pages".to_owned()),
        (Method::PATCH, format!("/v1/pages/{PAGE_A}")),
        (Method::DELETE, format!("/v1/pages/{PAGE_A}")),
        (Method::POST, format!("/v1/data_sources/{A}")),
        (Method::PATCH, format!("/v1/data_sources/{A}")),
        (Method::POST, "/v1/data_sources".to_owned()),
        (Method::POST, "/v1/search".to_owned()),
        (Method::POST, "/v1/users/me".to_owned()),
        (Method::GET, format!("/v1/users/{A}")),
        (Method::GET, query_path.clone()),
        (Method::POST, format!("{query_path}/")),
        (Method::POST, format!("/v1/data_sources/{A}/%71uery")),
        (Method::POST, "/v1/data_sources/not-a-uuid/query".to_owned()),
    ] {
        let result: Result<Value, NotionError> =
            client.read(|| client.request(method.clone(), &path, &[]));
        assert_eq!(result, Err(NotionError::MethodDisabled));
    }
    let result: Value = client
        .read(|| {
            client
                .request(Method::POST, &query_path, &[])
                .json(&json!({}))
        })
        .unwrap();
    assert_eq!(result["results"], json!([]));
    server.join().unwrap();
}
