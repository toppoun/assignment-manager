use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use reqwest::Method;
use rusqlite::Connection;
use serde_json::{Value, json};

use super::{
    mock_apply::{self, ApplyError, CreateOutcome},
    tests::{candidate, input, key, moodle},
    *,
};
use crate::{domain::Deadline, sync::SourceCourse};

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

fn mock(script: Vec<Exchange>) -> (NotionClient, JoinHandle<()>) {
    mock_with_timeout(script, Duration::from_secs(2))
}

fn mock_with_timeout(script: Vec<Exchange>, timeout: Duration) -> (NotionClient, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let client = NotionClient::for_mock(listener.local_addr().unwrap(), TOKEN, timeout).unwrap();
    let server = thread::spawn(move || {
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
        // A hidden extra send may be queued by the time the last handler exits.
        assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    });
    (client, server)
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
fn http_dry_run_previews_writes_after_complete_pagination_and_reconverges() {
    let a = key("a");
    let b = key("b");
    let mut second = candidate("b", "New title");
    second.deadline = None;
    let mut acquired = moodle(vec![candidate("a", "New task"), second]);
    let assignment_key = acquired.assignments().batch().as_slice()[1]
        .external_key
        .clone();
    let mut course_key = assignment_key.clone();
    course_key.id = "external-course".into();
    acquired.courses.insert(
        assignment_key,
        SourceCourse {
            external_key: course_key.clone(),
            name: "Course".into(),
        },
    );
    let input = SyncInput::from_acquisitions(Some(&acquired), None).unwrap();
    let mapping = CourseMapping {
        course_key,
        page_id: COURSE.into(),
    };
    let manual = task(MANUAL, None, "New task");
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
    script.push(query(vec![original.clone()], None).check(|_, body| {
        assert_eq!(body["start_cursor"], "cursor/value");
    }));
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
    updated["properties"]["授業"]["has_more"] = json!(false);
    let expected_patch = json!({"properties":{
        "title":{"title":[text("New title")]},
        "due":{"date":null},
        "R%3Ael":{"relation":[{"id":COURSE}]}
    }});
    script.push(
        Exchange::json("PATCH", format!("/v1/pages/{PAGE_B}"), updated.clone()).check(
            move |_, body| {
                assert_eq!(body, &expected_patch);
            },
        ),
    );
    script.extend(schema_exchanges(schema_json()));
    script.push(query(
        vec![manual, task(PAGE_A, Some(&a), "New task"), updated],
        None,
    ));
    let (client, server) = mock(script);
    let db = Connection::open_in_memory().unwrap();
    mock_apply::initialize(&db).unwrap();

    let report = client.dry_run(&input, &target(), std::slice::from_ref(&mapping));
    assert_eq!(report.error(), None);
    assert_eq!(
        report.to_string(),
        "Create=1 Update=1 Unchanged=0 Conflict=0 Deferred=0"
    );
    assert_eq!(report.plan().len(), 2);
    let count: i64 = db
        .query_row("SELECT count(*) FROM notion_creation", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
    let create = report
        .requests()
        .iter()
        .find(|r| r.method() == Method::POST)
        .unwrap();
    assert_eq!(
        create.body(),
        &json!({
            "parent":{"type":"data_source_id","data_source_id":A},
            "properties":{
                "title":{"title":[text("New task")]},
                "orig":{"select":{"id":"option-md"}},
                "sync":{"rich_text":[text(a.as_str())]}
            }
        })
    );
    let patch = report
        .requests()
        .iter()
        .find(|r| r.method() == Method::PATCH)
        .unwrap();
    // Dispatch is an explicit operation available only inside this mock test.
    let returned = client.send_mock_write(patch).unwrap();
    for managed in ["状態", "優先度", "メモ", "繰り返し", "提出URL"] {
        assert_eq!(
            returned["properties"][managed],
            original["properties"][managed]
        );
    }

    let repeat = client.dry_run(&input, &target(), &[mapping]);
    assert_eq!(repeat.counts().unchanged, 2);
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
        Exchange::error("GET", &path, 429, "Retry-After: 61\r\n"),
        Exchange::error("GET", &path, 302, "Location: http://127.0.0.1:1/other\r\n"),
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
        Err(NotionError::RateLimited)
    );
    assert_eq!(
        client.retrieve_page(PAGE_A, &[]),
        Err(NotionError::Rejected(302))
    );
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
    let input = input(vec![candidate("a", "New")]);
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
    let report = client.dry_run(&input, &target(), &[]);
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

struct JournalPath(PathBuf);

impl JournalPath {
    fn new() -> Self {
        let mut bytes = [0; 16];
        aws_lc_rs::rand::fill(&mut bytes).unwrap();
        let path = std::env::temp_dir().join(format!(
            "assignment-manager-notion-{:032x}.sqlite",
            u128::from_ne_bytes(bytes)
        ));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        Self(path)
    }

    fn open(&self) -> Connection {
        Connection::open(&self.0).unwrap()
    }
}

impl Drop for JournalPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn journal_page(path: &Path, key: &SyncKey) -> Option<String> {
    let db = Connection::open(path).unwrap();
    db.query_row(
        "SELECT page_id FROM notion_creation WHERE data_source=?1 AND sync_key=?2",
        [A, key.as_str()],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn http_mock_creation_recovers_across_restart_without_resending_ambiguous_posts() {
    let journal = JournalPath::new();
    let a = key("a");
    let b = key("b");
    let input = input(vec![candidate("a", "Task A"), candidate("b", "Task B")]);
    let remote_a = task(PAGE_A, Some(&a), "Task A");
    let remote_b = task(PAGE_B, Some(&b), "Task B");
    let mut script = schema_exchanges(schema_json());
    script.push(query(vec![], None));
    script.extend(schema_exchanges(schema_json()));
    script.push(exact(a.clone(), vec![]));
    let journal_path = journal.0.clone();
    let post_a_key = a.clone();
    script.push(
        Exchange::drop_response("POST", "/v1/pages").check(move |_, body| {
            assert_eq!(
                body["properties"]["sync"]["rich_text"],
                json!([text(post_a_key.as_str())])
            );
            // A separate SQLite connection can read the committed intent before POST.
            assert_eq!(journal_page(&journal_path, &post_a_key), None);
        }),
    );
    script.push(exact(b.clone(), vec![]));
    script.push(Exchange::json("POST", "/v1/pages", remote_b));
    // Search initially returns zero although the original POST was processed.
    script.push(exact(a.clone(), vec![]));
    script.push(exact(a.clone(), vec![remote_a.clone()]));
    // Previously known pages that become inaccessible, moved or trashed defer.
    script.push(Exchange::error(
        "GET",
        format!("/v1/pages/{PAGE_B}"),
        404,
        "",
    ));
    let mut moved = remote_a.clone();
    moved["parent"] = json!({"type":"data_source_id","data_source_id":C,"database_id":C_DB});
    script.push(Exchange::json("GET", format!("/v1/pages/{PAGE_A}"), moved));
    let mut trashed = remote_a;
    trashed["in_trash"] = json!(true);
    script.push(Exchange::json(
        "GET",
        format!("/v1/pages/{PAGE_A}"),
        trashed,
    ));

    let (client, server) = mock(script);
    let db = journal.open();
    mock_apply::initialize(&db).unwrap();
    let preview = client.dry_run(&input, &target(), &[]);
    assert_eq!(preview.counts().create, 2);
    let for_key = |key: &SyncKey| preview.requests().iter().find(|r| r.key() == key).unwrap();
    let schema = client.retrieve_schema(&target()).unwrap();

    assert_eq!(
        mock_apply::create_once(&client, &schema, &db, for_key(&a)).unwrap(),
        CreateOutcome::Deferred
    );
    assert_eq!(
        mock_apply::create_once(&client, &schema, &db, for_key(&b)).unwrap(),
        CreateOutcome::Created
    );
    drop(db);
    let db = journal.open();
    assert_eq!(
        mock_apply::create_once(&client, &schema, &db, for_key(&a)).unwrap(),
        CreateOutcome::Deferred
    );
    assert_eq!(journal_page(&journal.0, &a), None);
    assert_eq!(
        mock_apply::create_once(&client, &schema, &db, for_key(&a)).unwrap(),
        CreateOutcome::Confirmed
    );
    assert_eq!(journal_page(&journal.0, &a).as_deref(), Some(PAGE_A));
    assert_eq!(journal_page(&journal.0, &b).as_deref(), Some(PAGE_B));
    for key in [&b, &a, &a] {
        assert_eq!(
            mock_apply::create_once(&client, &schema, &db, for_key(key)).unwrap(),
            CreateOutcome::Deferred
        );
    }
    server.join().unwrap();
}

#[test]
fn http_mock_create_rechecks_matches_before_post() {
    let a = key("a");
    let input = input(vec![candidate("a", "Task A")]);
    let mut script = schema_exchanges(schema_json());
    let mut incomplete = list(vec![], None);
    incomplete["request_status"] = json!({"type":"incomplete"});
    script.push(Exchange::json(
        "POST",
        format!("/v1/data_sources/{A}/query"),
        incomplete,
    ));
    let existing = task(PAGE_A, Some(&a), "Task A");
    script.push(exact(
        a.clone(),
        vec![existing.clone(), task(PAGE_B, Some(&a), "Duplicate")],
    ));
    script.push(exact(a.clone(), vec![existing]));
    let (client, server) = mock(script);
    let schema = client.retrieve_schema(&target()).unwrap();
    let plan = plan_sync(&input, NotionSnapshot::Complete(&[]), &[]);
    let request = schema.prepare(&plan[0]).unwrap().unwrap();
    let db = Connection::open_in_memory().unwrap();
    mock_apply::initialize(&db).unwrap();

    let result = mock_apply::create_once(&client, &schema, &db, &request);
    assert!(matches!(
        result,
        Err(ApplyError::Read(NotionError::IncompleteSnapshot))
    ));
    assert_eq!(
        mock_apply::create_once(&client, &schema, &db, &request).unwrap(),
        CreateOutcome::Conflict
    );
    assert_eq!(
        mock_apply::create_once(&client, &schema, &db, &request).unwrap(),
        CreateOutcome::Confirmed
    );
    server.join().unwrap();
}

#[test]
fn http_mock_write_errors_are_reconciled_before_any_further_creation() {
    let a = key("a");
    let input = input(vec![candidate("a", "Task A")]);
    for status in [429, 529, 503] {
        let mut script = schema_exchanges(schema_json());
        script.push(exact(a.clone(), vec![]));
        script.push(Exchange::error(
            "POST",
            "/v1/pages",
            status,
            "Retry-After: 0\r\n",
        ));
        script.push(exact(a.clone(), vec![task(PAGE_A, Some(&a), "Task A")]));
        let (client, server) = mock(script);
        let schema = client.retrieve_schema(&target()).unwrap();
        let plan = plan_sync(&input, NotionSnapshot::Complete(&[]), &[]);
        let request = schema.prepare(&plan[0]).unwrap().unwrap();
        let db = Connection::open_in_memory().unwrap();
        mock_apply::initialize(&db).unwrap();
        assert_eq!(
            mock_apply::create_once(&client, &schema, &db, &request).unwrap(),
            CreateOutcome::Deferred
        );
        assert_eq!(
            mock_apply::create_once(&client, &schema, &db, &request).unwrap(),
            CreateOutcome::Confirmed
        );
        server.join().unwrap();
    }
}
