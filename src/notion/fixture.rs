//! Fixed loopback Notion fixture for the explicit CLI fixture build.

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

pub use super::apply::{ApplyError, ApplyReport};
use super::{
    NOTION_VERSION, NotionClient, NotionError, NotionPropertyIds, NotionTarget, SyncInput,
    state::{StateError, StateStore, TargetLock},
};

const ASSIGNMENTS: &str = "00000000-0000-4000-8000-000000000101";
const ASSIGNMENTS_DB: &str = "00000000-0000-4000-8000-000000000102";
const COURSES: &str = "00000000-0000-4000-8000-000000000103";
const COURSES_DB: &str = "00000000-0000-4000-8000-000000000104";
const TOKEN: &str = "fixture-notion-token";
const LIMIT: u64 = 16_384;

pub fn target() -> NotionTarget {
    NotionTarget {
        assignments_data_source_id: ASSIGNMENTS.into(),
        courses_data_source_id: COURSES.into(),
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
    Ok([
        client.apply(input, &target(), &[], state.path())?,
        client.apply(input, &target(), &[], state.path())?,
    ])
}

fn start_server(writable: bool) -> std::io::Result<(NotionClient, FixtureServer)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let client = NotionClient::for_mock(listener.local_addr()?, TOKEN, Duration::from_secs(3))
        .map_err(|_| std::io::Error::other("fixture client initialization failed"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&stop);
    let worker = thread::Builder::new()
        .name("notion-fixture".into())
        .spawn(move || {
            let mut pages = Vec::new();
            while !done.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = handle(stream, &mut pages, writable);
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

fn handle(mut stream: TcpStream, pages: &mut Vec<Value>, writable: bool) -> std::io::Result<()> {
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

    let (status, body) = if !auth || !version {
        (401, json!({"code":"unauthorized"}))
    } else if method == "GET" && path.strip_prefix("/v1/data_sources/") == Some(ASSIGNMENTS) {
        (
            200,
            data_source(
                ASSIGNMENTS,
                ASSIGNMENTS_DB,
                json!({
                    "Title":{"id":"title","type":"title","title":{}},
                    "Deadline":{"id":"due","type":"date","date":{}},
                    "Course":{"id":"course","type":"relation","relation":{
                        "type":"single_property","database_id":COURSES_DB,"data_source_id":COURSES
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
    } else if method == "GET" && path.strip_prefix("/v1/data_sources/") == Some(COURSES) {
        (200, data_source(COURSES, COURSES_DB, json!({})))
    } else if method == "POST"
        && path
            .strip_prefix("/v1/data_sources/")
            .and_then(|suffix| suffix.strip_suffix("/query"))
            == Some(ASSIGNMENTS)
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
        if request["parent"]["data_source_id"] != ASSIGNMENTS {
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
