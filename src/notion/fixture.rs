//! Fixed loopback Notion fixture for the explicit CLI fixture build.

mod course_setup;
pub use course_setup::{CourseSetupCounts, exercise_course_setup};

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use serde_json::{Value, json};

use crate::{
    import::{ExternalRecordKey, OriginKind},
    webclass_auth::synthetic::MOODLE_SCOPE,
};

pub use super::apply::{ApplyError, ApplyReport};
use super::{
    CourseMapping, CourseRulesTarget, NOTION_VERSION, NotionClient, NotionError, NotionPropertyIds,
    NotionTarget, SyncInput, VerificationTarget,
    state::{StateError, StateStore, TargetLock},
};

const ASSIGNMENTS: &str = "00000000-0000-4000-8000-000000000101";
const ASSIGNMENTS_DB: &str = "00000000-0000-4000-8000-000000000102";
const COURSES: &str = "00000000-0000-4000-8000-000000000103";
const COURSES_DB: &str = "00000000-0000-4000-8000-000000000104";
const RULES: &str = "00000000-0000-4000-8000-000000000110";
const RULES_DB: &str = "00000000-0000-4000-8000-000000000111";
const HOPPII_GROUPS: [&str; 3] = [
    "fixture-course-001",
    "fixture-course-002",
    "fixture-course-003",
];
const FIXTURE_TERM: &str = "2026:2";
const READER: &str = "00000000-0000-4000-8000-000000000105";
const MOODLE_COURSES: [(&str, &str); 2] = [
    ("照合用の授業", "00000000-0000-4000-8000-000000000106"),
    ("別の合成授業", "00000000-0000-4000-8000-000000000107"),
];
const TOKEN: &str = "fixture-notion-token";
const LIMIT: u64 = 16_384;

pub fn target() -> NotionTarget {
    NotionTarget {
        assignments_data_source_id: ASSIGNMENTS.into(),
        courses_data_source_id: COURSES.into(),
        course_rules: None,
        properties: NotionPropertyIds {
            title: "title".into(),
            deadline: "due".into(),
            course: "course".into(),
            submission_url: "url".into(),
            sync_key: "sync".into(),
            source: "source".into(),
        },
    }
}

pub fn course_rules_target() -> NotionTarget {
    let mut target = target();
    target.course_rules = Some(CourseRulesTarget {
        data_source_id: RULES.into(),
        course_term: "term".into(),
        source: "source".into(),
        scope: "scope".into(),
        external_id: "external".into(),
        term: "term".into(),
        course: "course".into(),
        status: "status".into(),
        setup: None,
    });
    target
}

pub fn verification_target() -> VerificationTarget {
    VerificationTarget::new(target(), ASSIGNMENTS, COURSES, READER).expect("fixed fixture IDs")
}

pub fn moodle_mappings() -> [CourseMapping; 2] {
    MOODLE_COURSES.map(|(course, page)| CourseMapping {
        course_key: ExternalRecordKey {
            origin: OriginKind::Moodle,
            scope: MOODLE_SCOPE.into(),
            id: course.into(),
        },
        page_id: page.into(),
    })
}

pub struct FixtureServer {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

pub fn start() -> std::io::Result<(NotionClient, FixtureServer)> {
    start_server(false)
}

/// Exercise creation and reconciliation against the same in-memory service.
/// The mock, its synthetic token and its state file belong to this exercise.
pub fn exercise_apply(input: &SyncInput, directory: &Path) -> Result<[ApplyReport; 2], ApplyError> {
    let _lock = TargetLock::acquire(directory)?;
    let state = tempfile::tempdir_in(directory).map_err(|_| StateError::Io)?;
    let (client, _server) =
        start_server(true).map_err(|_| ApplyError::Notion(NotionError::Configuration))?;
    StateStore::initialize(&state.path().join("state.sqlite3"), ASSIGNMENTS)?;
    let target = course_rules_target();
    Ok([
        client.apply(input, &target, &[], state.path())?,
        client.apply(input, &target, &[], state.path())?,
    ])
}

#[cfg(test)]
#[test]
fn university_keys_reconcile_the_same_notion_pages_with_independent_state() {
    use crate::{
        domain::Deadline,
        moodle_http::{MoodleExportKind, MoodleFeed},
        secured_sync::SecuredSyncClients,
        sync::{WebClassAcquisition, acquire_moodle, acquire_webclass_once},
        webclass_auth::{
            HoseiCredentials,
            synthetic::{
                MOODLE_URL, Scenario, SyntheticUniversity, UNIVERSITY_ID, UNIVERSITY_PASSWORD,
            },
        },
    };

    let acquire = || {
        let university = SyntheticUniversity::start(Scenario::BothSuccess);
        let mut clients = SecuredSyncClients::new(university.policy());
        clients.begin_run();
        let feed = MoodleFeed::new(
            "default.course-related",
            MoodleExportKind::CourseRelated,
            MOODLE_URL,
        )
        .unwrap();
        let moodle = acquire_moodle(&clients.moodle().unwrap(), &feed).unwrap();
        let session = clients
            .authenticator()
            .login(HoseiCredentials::new(UNIVERSITY_ID, UNIVERSITY_PASSWORD))
            .unwrap();
        let webclass = acquire_webclass_once(&session);
        assert_eq!(webclass.result(), Ok(()));
        SyncInput::from_acquisitions(
            Some(&moodle),
            Some(&WebClassAcquisition::new(vec![webclass], Ok(()))),
        )
        .unwrap()
    };

    let (client, _server) = start_server(true).unwrap();
    let directory = super::state::test_state_dir();
    let target = course_rules_target();
    let target_id = target.validated_id().unwrap();
    let first = acquire();
    let second = acquire();
    assert_eq!(first.assignments().len(), 10);
    assert_eq!(first.assignments(), second.assignments());
    let preview = client.dry_run(&first, &target, &[]);
    assert_eq!(preview.counts().create, 10);
    // Fixed UTF-8 byte-level vectors run identically on every host.
    for (key, title, due, course, display) in [
        (
            "v1:b7e92444ae37a18bb36c597fd2e6bd2ba0999bff4ece1984c465aba44cb977aa",
            "演習課題01",
            "2026-09-27T14:59:00Z",
            "Java2-2026",
            "【合成授業 1】演習課題01",
        ),
        (
            "v1:f2683d509d2cc28d68ced0815d6c3cd4012d5ef6fa592db1c56ef3c3ef6aefd1",
            "演習課題A",
            "2026-10-31T14:59:59Z",
            "fixture-course-002",
            "【合成授業 2】演習課題A",
        ),
    ] {
        let key = key.parse().unwrap();
        let assignment = &first.assignments()[&key];
        let request = preview.requests().iter().find(|r| r.key() == &key).unwrap();
        assert_eq!(
            request.body()["properties"]["title"]["title"][0]["text"]["content"],
            display
        );
        assert_eq!(assignment.title, title);
        assert_eq!(
            assignment.deadline,
            Some(Deadline::DateTime(due.parse().unwrap()))
        );
        assert_eq!(assignment.course_key.as_ref().unwrap().id, course);
        assert_eq!(assignment.submission_url, None);
    }

    let initial_dir = directory.path().join("端末 A");
    std::fs::create_dir(&initial_dir).unwrap();
    let initial_state = initial_dir.join("state.sqlite3");
    StateStore::initialize(&initial_state, &target_id).unwrap();
    let created = client.apply(&first, &target, &[], &initial_dir).unwrap();
    assert_eq!(created.error, None);
    assert_eq!(created.counts.create, 10);
    let existing = StateStore::read(&initial_state, &target_id)
        .unwrap()
        .unwrap();
    assert_eq!(existing.bindings.len(), 10);

    let fresh_dir = directory.path().join("端末 B");
    std::fs::create_dir(&fresh_dir).unwrap();
    let fresh_state = fresh_dir.join("state.sqlite3");
    StateStore::initialize(&fresh_state, &target_id).unwrap();
    let empty = StateStore::read(&fresh_state, &target_id).unwrap().unwrap();
    assert!(empty.bindings.is_empty());
    let reconciled = client.apply(&second, &target, &[], &fresh_dir).unwrap();
    assert_eq!(reconciled.error, None);
    assert_eq!(reconciled.counts.unchanged, 10);
    assert_eq!(reconciled.counts.create, 0);
    assert_eq!(reconciled.counts.update, 0);
    assert_eq!(reconciled.pending, 0);
    let matched = StateStore::read(&fresh_state, &target_id).unwrap().unwrap();
    assert_eq!(matched.page_owners(), existing.page_owners());
}

fn start_server(writable: bool) -> std::io::Result<(NotionClient, FixtureServer)> {
    start_server_with_setup(writable, Vec::new(), None)
}

#[derive(Default)]
struct SetupFixture {
    courses: Vec<Value>,
    rules: Vec<Value>,
    #[cfg(test)]
    fail_next: Option<SetupFault>,
    #[cfg(test)]
    verification: bool,
    #[cfg(test)]
    late_rule: Option<Value>,
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum SetupFault {
    RejectRule,
    DropCourseBeforeCommit,
    DropCourseResponse,
    DropRuleResponse,
}

fn start_server_with_setup(
    writable: bool,
    mut pages: Vec<Value>,
    mut setup: Option<SetupFixture>,
) -> std::io::Result<(NotionClient, FixtureServer)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let client = NotionClient::for_mock(listener.local_addr()?, TOKEN, Duration::from_secs(3))
        .map_err(|_| std::io::Error::other("fixture client initialization failed"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&stop);
    let worker = thread::Builder::new()
        .name("notion-fixture".into())
        .spawn(move || {
            while !done.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = handle(stream, &mut pages, writable, setup.as_mut());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        })?;
    Ok((
        client,
        FixtureServer {
            stop,
            worker: Some(worker),
        },
    ))
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn handle(
    mut stream: TcpStream,
    pages: &mut Vec<Value>,
    writable: bool,
    setup: Option<&mut SetupFixture>,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let mut reader = BufReader::new((&mut stream).take(LIMIT));
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let mut words = first.split_whitespace();
    let method = words.next().unwrap_or_default();
    let path = words
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default();
    let mut auth = false;
    let mut version = false;
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("authorization") {
                auth = value.trim().strip_prefix("Bearer ") == Some(TOKEN);
            }
            if name.eq_ignore_ascii_case("notion-version") {
                version = value.trim() == NOTION_VERSION;
            }
            if name.eq_ignore_ascii_case("content-length") {
                length = match value.trim().parse::<usize>() {
                    Ok(value) if value <= LIMIT as usize => value,
                    _ => return Ok(()),
                };
            }
        }
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    let [assignments_id, courses_id, rules_id] = setup
        .as_ref()
        .map_or([ASSIGNMENTS, COURSES, RULES], |setup| setup.source_ids());
    #[cfg(test)]
    let reader_id = if setup.as_ref().is_some_and(|setup| setup.verification) {
        super::trial::MOODLE_VERIFICATION_WRITER
    } else {
        READER
    };
    #[cfg(not(test))]
    let reader_id = READER;

    let (status, body) = if !auth || !version {
        (401, json!({"code":"unauthorized"}))
    } else if method == "GET" && path == "/v1/users/me" {
        (
            200,
            json!({"object":"user","id":reader_id,"type":"bot","bot":{"owner":{
                "type":"workspace","workspace":true
            }}}),
        )
    } else if let Some(id) = path
        .strip_prefix("/v1/pages/")
        .filter(|id| method == "GET" && MOODLE_COURSES.iter().any(|(_, page)| id == page))
    {
        (
            200,
            json!({"object":"page","id":id,"in_trash":false,"parent":{
                "type":"data_source_id","data_source_id":COURSES,"database_id":COURSES_DB
            }}),
        )
    } else if method == "GET" && path.strip_prefix("/v1/data_sources/") == Some(assignments_id) {
        (
            200,
            data_source(
                assignments_id,
                ASSIGNMENTS_DB,
                json!({
                    "Title":{"id":"title","type":"title","title":{}},
                    "Deadline":{"id":"due","type":"date","date":{}},
                    "Course":{"id":"course","type":"relation","relation":{
                        "type":"single_property","database_id":COURSES_DB,"data_source_id":courses_id
                    }},
                    "Submission":{"id":"url","type":"url","url":{}},
                    "Sync key":{"id":"sync","type":"rich_text","rich_text":{}},
                    "Source":{"id":"source","type":"select","select":{"options":[
                        {"id":"moodle","name":"Moodle"},{"id":"webclass","name":"WebClass"}
                    ]}},
                    "状態":{"id":"status","type":"status","status":{}},
                    "優先度":{"id":"priority","type":"select","select":{}},
                    "科目":{"id":"subject","type":"select","select":{}},
                    "種別":{"id":"category","type":"select","select":{}},
                    "繰り返し":{"id":"recurrence","type":"select","select":{}},
                    "メモ":{"id":"notes","type":"rich_text","rich_text":{}}
                }),
            ),
        )
    } else if method == "GET" && path.strip_prefix("/v1/data_sources/") == Some(courses_id) {
        (
            200,
            data_source(
                courses_id,
                COURSES_DB,
                json!({
                    "Title":{"id":"title","type":"title","title":{}},
                    "Term":{"id":"term","type":"rich_text","rich_text":{}},
                    "Group":{"id":"group","type":"rich_text","rich_text":{}},
                    "Year":{"id":"year","type":"number","number":{}},
                    "Day":{"id":"day","type":"rich_text","rich_text":{}},
                    "Period":{"id":"period","type":"rich_text","rich_text":{}}
                }),
            ),
        )
    } else if method == "GET" && path.strip_prefix("/v1/data_sources/") == Some(rules_id) {
        (
            200,
            data_source(
                rules_id,
                RULES_DB,
                json!({
                    "Title":{"id":"title","type":"title","title":{}},
                    "Source":{"id":"source","type":"select","select":{"options":[
                        {"id":"hoppii","name":"HOPPII"},{"id":"moodle","name":"Moodle"}
                    ]}},
                    "Scope":{"id":"scope","type":"rich_text","rich_text":{}},
                    "External ID":{"id":"external","type":"rich_text","rich_text":{}},
                    "Term":{"id":"term","type":"rich_text","rich_text":{}},
                    "Course":{"id":"course","type":"relation","relation":{
                        "type":"single_property","data_source_id":courses_id,"database_id":COURSES_DB
                    }},
                    "Status":{"id":"status","type":"select","select":{"options":[
                        {"id":"unset","name":"未設定"},{"id":"confirmed","name":"確定"},{"id":"excluded","name":"対象外"}
                    ]}}
                }),
            ),
        )
    } else if method == "POST" && path == format!("/v1/data_sources/{rules_id}/query") {
        if let Some(setup) = setup {
            #[cfg(test)]
            if setup.late_rule.is_some() {
                let query: Value = serde_json::from_slice(&payload)?;
                if query.get("filter").is_some()
                    && let Some(rule) = setup.late_rule.take()
                {
                    setup.rules.push(rule);
                }
            }
            (200, fixture_list(&setup.rules))
        } else {
            let groups = HOPPII_GROUPS.into_iter().enumerate().map(|(index, group)| {
                rule_page(
                    index,
                    "hoppii",
                    crate::import::DEFAULT_COURSE_SOURCE_SCOPE,
                    group,
                    index,
                )
            });
            let rules: Vec<_> = groups
                .chain([rule_page(3, "moodle", MOODLE_SCOPE, "Java2-2026", 0)])
                .collect();
            (200, fixture_list(&rules))
        }
    } else if method == "POST" && path == format!("/v1/data_sources/{courses_id}/query") {
        if let Some(setup) = setup {
            (200, fixture_list(&setup.courses))
        } else {
            let courses: Vec<_> = (0..HOPPII_GROUPS.len())
                .map(|index| {
                    fixture_page(
                        0x120 + index,
                        COURSES,
                        COURSES_DB,
                        json!({
                            "Term": rich("term", FIXTURE_TERM),
                            "Title": {"id":"title","type":"title","title":[
                                super::wire::text_item(&format!("合成授業 {}", index + 1))
                            ]}
                        }),
                    )
                })
                .collect();
            (200, fixture_list(&courses))
        }
    } else if method == "POST"
        && path
            .strip_prefix("/v1/data_sources/")
            .and_then(|suffix| suffix.strip_suffix("/query"))
            == Some(assignments_id)
    {
        let query: Value = serde_json::from_slice(&payload)?;
        let key = query["filter"]["rich_text"]["equals"].as_str();
        let results: Vec<_> = pages
            .iter()
            .filter(|page| {
                key.is_none_or(|key| {
                    page["properties"]["sync"]["rich_text"][0]["text"]["content"] == key
                })
            })
            .collect();
        (
            200,
            json!({"object":"list","results":results,"has_more":false,"next_cursor":null}),
        )
    } else if writable && method == "POST" && path == "/v1/pages" {
        let request: Value = serde_json::from_slice(&payload)?;
        if let Some(setup) = setup {
            let Some(response) = setup.create(&request) else {
                return Ok(()); // Simulate a committed POST whose response was lost.
            };
            response
        } else if request["parent"]["data_source_id"] != ASSIGNMENTS {
            (400, json!({"code":"validation_error"}))
        } else {
            let page = created_page(&request, pages.len());
            pages.push(page.clone());
            (200, page)
        }
    } else {
        (405, json!({"code":"invalid_request"}))
    };
    let body = body.to_string();
    write!(
        stream,
        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

impl SetupFixture {
    fn source_ids(&self) -> [&'static str; 3] {
        #[cfg(test)]
        if self.verification {
            return [
                super::trial::MOODLE_VERIFICATION_ASSIGNMENTS,
                super::trial::MOODLE_VERIFICATION_COURSES,
                super::course_setup::VERIFICATION_RULES,
            ];
        }
        [ASSIGNMENTS, COURSES, RULES]
    }

    fn create(&mut self, request: &Value) -> Option<(u16, Value)> {
        let target = request["parent"]["data_source_id"].as_str();
        let [_, courses, rules] = self.source_ids();
        #[cfg(test)]
        let drop_response = match (&self.fail_next, target) {
            (Some(SetupFault::RejectRule), id) if id == Some(rules) => {
                self.fail_next = None;
                return Some((400, json!({"code":"validation_error"})));
            }
            (Some(SetupFault::DropCourseBeforeCommit), id) if id == Some(courses) => {
                self.fail_next = None;
                return None;
            }
            (Some(SetupFault::DropCourseResponse), id)
            | (Some(SetupFault::DropRuleResponse), id)
                if id
                    == Some(match self.fail_next {
                        Some(SetupFault::DropCourseResponse) => courses,
                        _ => rules,
                    }) =>
            {
                self.fail_next = None;
                true
            }
            _ => false,
        };
        #[cfg(not(test))]
        let drop_response = false;
        let (rows, base, id, database) = match target {
            Some(id) if id == courses => (&mut self.courses, 0x2000, courses, COURSES_DB),
            Some(id) if id == rules => (&mut self.rules, 0x4000, rules, RULES_DB),
            _ => return Some((400, json!({"code":"validation_error"}))),
        };
        let mut properties = json!({});
        for (id, property) in request["properties"].as_object()? {
            let (kind, value) = property.as_object()?.iter().next()?;
            properties[id] = json!({"id":id,"type":kind,(kind):value});
        }
        if target == Some(rules) {
            if properties.get("course").is_none() {
                properties["course"] = json!({"id":"course","type":"relation","relation":[]});
            }
            properties["course"]["has_more"] = json!(false);
        }
        let page = fixture_page(base + rows.len(), id, database, properties);
        rows.push(page.clone());
        (!drop_response).then_some((200, page))
    }
}

fn fixture_list(pages: &[Value]) -> Value {
    json!({"object":"list","results":pages,"has_more":false,"next_cursor":null})
}

fn fixture_id(index: usize) -> String {
    format!("00000000-0000-4000-8000-{index:012x}")
}

fn fixture_page(index: usize, source: &str, database: &str, properties: Value) -> Value {
    json!({"object":"page","id":fixture_id(index),"in_trash":false,"properties":properties,"parent":{
        "type":"data_source_id","data_source_id":source,"database_id":database
    }})
}

fn rich(id: &str, text: &str) -> Value {
    json!({"id":id,"type":"rich_text","rich_text":[{"type":"text","text":{"content":text}}]})
}

fn rule_page(index: usize, source: &str, scope: &str, external: &str, course: usize) -> Value {
    fixture_page(
        0x140 + index,
        RULES,
        RULES_DB,
        json!({
            "Title":{"id":"title","type":"title","title":[{"type":"text","text":{"content":external}}]},
            "Source":{"id":"source","type":"select","select":{"id":source}},
            "Scope":rich("scope", scope),
            "External ID":rich("external", external),
            "Term":rich("term", FIXTURE_TERM),
            "Course":{"id":"course","type":"relation","relation":[{"id":fixture_id(0x120 + course)}],"has_more":false},
            "Status":{"id":"status","type":"select","select":{"id":"confirmed"}}
        }),
    )
}

fn created_page(request: &Value, index: usize) -> Value {
    let mut properties = json!({});
    for (id, kind, default) in [
        ("title", "title", json!([])),
        ("due", "date", Value::Null),
        ("course", "relation", json!([])),
        ("sync", "rich_text", json!([])),
        ("source", "select", Value::Null),
    ] {
        let mut property = json!({"id":id,"type":kind});
        property[kind] = request["properties"][id]
            .get(kind)
            .cloned()
            .unwrap_or(default);
        if kind == "relation" {
            property["has_more"] = Value::Bool(false);
        }
        properties[id] = property;
    }
    json!({
        "object":"page","id":format!("00000000-0000-4000-8000-{:012x}", 0x1000 + index),
        "in_trash":false,"last_edited_time":"2026-03-11T00:00:00Z",
        "parent":{"type":"data_source_id","data_source_id":ASSIGNMENTS,"database_id":ASSIGNMENTS_DB},
        "properties":properties
    })
}

fn data_source(id: &str, database: &str, properties: Value) -> Value {
    json!({
        "object":"data_source","id":id,"in_trash":false,
        "parent":{"type":"database_id","database_id":database},"properties":properties
    })
}
