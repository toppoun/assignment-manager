mod common;
use assignment_manager::{
    domain::{AssignmentId, CompletionState},
    persistence::SqliteStore,
    query::load_assignment_list,
};
use assignment_manager_web_server::{
    Clock, PocServer, PocState, WebError,
    poc::{PRIVATE_MARKER, PocDatabase, SOURCE_MARKER},
};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration as StdDuration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

const NOW: &str = "2026-10-21T15:00:00.123456789Z";
const PORT: u16 = 32123;
struct FixedClock(DateTime<Utc>);
impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}
async fn fixture() -> (tempfile::TempDir, PocDatabase, PocState, Router) {
    let root = tempfile::tempdir().unwrap();
    let db = common::database(&root.path().join("poc")).unwrap();
    let state = PocState::open(&db, FixedClock(NOW.parse().unwrap()))
        .await
        .unwrap();
    let app = common::authenticated_router(state.clone(), PORT).await;
    (root, db, state, app)
}
async fn request(app: &Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("host", format!("127.0.0.1:{PORT}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert!(
        !response
            .headers()
            .contains_key("access-control-allow-origin")
    );
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
fn titles(value: &Value) -> Vec<&str> {
    value["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["title"].as_str().unwrap())
        .collect()
}
fn item<'a>(value: &'a Value, title: &str) -> &'a Value {
    value["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["title"] == title)
        .unwrap()
}

#[tokio::test]
async fn defaults_and_dto_preserve_core_order_identity_and_safe_fields() {
    let (_root, db, state, app) = fixture().await;
    let (status, body) = request(&app, "/api/v1/assignments").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["totalCount"], 11);
    assert_eq!(body["evaluatedAt"], NOW);
    assert_eq!(body["timeZone"], "Asia/Tokyo");
    assert_eq!(
        titles(&body),
        [
            "う・期限切れ",
            "さ・日付期限切れ",
            "あ・日時ちょうど",
            "け・今日の最後",
            "い・日付のみ",
            "こ・翌日開始",
            "え・7日後",
            "く・期間外",
            "き・日付7日後",
            "お・期限なし"
        ]
    );
    let courses = body["courses"].as_array().unwrap();
    assert_eq!(courses.len(), 2);
    assert_eq!(courses[0]["displayLabel"], courses[1]["displayLabel"]);
    assert_ne!(courses[0]["id"], courses[1]["id"]);
    assert_ne!(courses[0]["origin"], courses[1]["origin"]);
    let instant = item(&body, "あ・日時ちょうど");
    assert_eq!(instant["deadline"], json!({"kind":"date_time","value":NOW}));
    assert_eq!(instant["hasSubmissionUrl"], true);
    assert_eq!(instant["submission"], "moodle");
    assert_eq!(item(&body, "う・期限切れ")["submission"], "web_class");
    assert_eq!(
        item(&body, "い・日付のみ")["deadline"],
        json!({"kind":"date_only","value":"2026-10-22"})
    );
    assert_eq!(
        item(&body, "お・期限なし")["deadline"],
        json!({"kind":"none"})
    );
    assert!(item(&body, "お・期限なし")["course"].is_null());
    let mut keys: Vec<_> = instant
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "completion",
            "course",
            "deadline",
            "hasSubmissionUrl",
            "id",
            "status",
            "submission",
            "title"
        ]
    );
    let rendered = body.to_string();
    for private in [
        PRIVATE_MARKER,
        SOURCE_MARKER,
        "https://",
        "token=",
        "submissionUrl",
        "external_id",
        "password",
        "cookie",
        db.path().to_str().unwrap(),
    ] {
        assert!(!rendered.contains(private), "unexpected private field");
    }
    state.shutdown().await;
}

#[tokio::test]
async fn filters_sorts_and_course_options_follow_application() {
    let (_root, _db, state, app) = fixture().await;
    let (_, all) = request(&app, "/api/v1/assignments?completion=all&sort=title").await;
    assert_eq!(all["items"].as_array().unwrap().len(), 11);
    let mut expected = titles(&all);
    expected.sort();
    assert_eq!(titles(&all), expected);
    let (_, completed) = request(&app, "/api/v1/assignments?completion=completed").await;
    assert_eq!(titles(&completed), ["か・完了済み"]);
    assert_eq!(completed["items"][0]["status"], "none");
    assert_eq!(completed["totalCount"], 11);
    assert_eq!(completed["courses"].as_array().unwrap().len(), 2);
    let (_, unassigned) =
        request(&app, "/api/v1/assignments?courseMode=unassigned&sort=title").await;
    assert_eq!(
        titles(&unassigned),
        ["お・期限なし", "き・日付7日後", "さ・日付期限切れ"]
    );
    for course in all["courses"].as_array().unwrap() {
        let url = format!(
            "/api/v1/assignments?completion=all&courseMode=course&courseId={}",
            course["id"].as_str().unwrap()
        );
        let (_, selected) = request(&app, &url).await;
        assert_eq!(selected["courses"], all["courses"]);
        assert!(
            selected["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v["course"]["id"] == course["id"])
        );
        assert!(!selected["items"].as_array().unwrap().is_empty());
    }
    let (_, course_order) = request(
        &app,
        "/api/v1/assignments?completion=all&courseMode=all&deadline=all&sort=course",
    )
    .await;
    let ids: Vec<_> = course_order["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["course"]["id"].as_str())
        .collect();
    let first_unassigned = ids.iter().position(Option::is_none).unwrap();
    assert!(ids[first_unassigned..].iter().all(Option::is_none));
    assert!(ids[..first_unassigned].windows(2).all(|w| w[0] <= w[1]));
    let (_, empty) = request(
        &app,
        "/api/v1/assignments?courseMode=course&courseId=missing",
    )
    .await;
    assert_eq!(titles(&empty).len(), 0);
    assert_eq!(empty["totalCount"], 11);
    assert_eq!(empty["courses"], all["courses"]);
    state.shutdown().await;
}

#[tokio::test]
async fn jst_boundaries_inclusive_windows_and_evaluated_at_share_one_sample() {
    let (_root, db, state, app) = fixture().await;
    let (_, overdue) = request(&app, "/api/v1/assignments?deadline=overdue").await;
    assert_eq!(titles(&overdue), ["う・期限切れ", "さ・日付期限切れ"]);
    assert!(
        overdue["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["status"] == "overdue")
    );
    let (_, today) = request(&app, "/api/v1/assignments?deadline=today").await;
    assert_eq!(
        titles(&today),
        ["あ・日時ちょうど", "け・今日の最後", "い・日付のみ"]
    );
    assert!(
        today["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["status"] == "today")
    );
    let (_, week) = request(&app, "/api/v1/assignments?deadline=within_seven_days").await;
    assert_eq!(
        titles(&week),
        [
            "あ・日時ちょうど",
            "け・今日の最後",
            "い・日付のみ",
            "こ・翌日開始",
            "え・7日後",
            "き・日付7日後"
        ]
    );
    assert_eq!(item(&week, "え・7日後")["status"], "upcoming");
    state.shutdown().await;
    let before = "2026-10-21T14:59:59.999999999Z".parse().unwrap();
    let state = PocState::open(&db, FixedClock(before)).await.unwrap();
    let (_, today) = request(
        &common::authenticated_router(state.clone(), PORT).await,
        "/api/v1/assignments?deadline=today&completion=all",
    )
    .await;
    assert_eq!(today["evaluatedAt"], "2026-10-21T14:59:59.999999999Z");
    assert_eq!(today["items"].as_array().unwrap().len(), 3); // DateTime + DateOnly incomplete + completed date.
    assert!(!titles(&today).contains(&"い・日付のみ"));
    assert_eq!(item(&today, "う・期限切れ")["status"], "today"); // Exactly now is not overdue.
    state.shutdown().await;
}

struct AdvancingClock {
    calls: Arc<AtomicUsize>,
}
impl Clock for AdvancingClock {
    fn now(&self) -> DateTime<Utc> {
        NOW.parse::<DateTime<Utc>>().unwrap()
            + Duration::days(self.calls.fetch_add(1, Ordering::SeqCst) as i64)
    }
}
#[tokio::test]
async fn one_runtime_multiple_gets_sample_once_each_and_ignore_os_calendar() {
    let root = tempfile::tempdir().unwrap();
    let db = common::database(&root.path().join("poc")).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let state = PocState::open(
        &db,
        AdvancingClock {
            calls: Arc::clone(&calls),
        },
    )
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1); // Actual readiness query.
    let app = common::authenticated_router(state.clone(), PORT).await;
    let (_, first) = request(&app, "/api/v1/assignments?deadline=today").await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(first["evaluatedAt"], "2026-10-22T15:00:00.123456789Z");
    assert_eq!(titles(&first), ["こ・翌日開始"]);
    assert_eq!(first["items"][0]["status"], "overdue"); // Same JST day, earlier than now.
    let (_, second) = request(&app, "/api/v1/assignments?deadline=today").await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(second["evaluatedAt"], "2026-10-23T15:00:00.123456789Z");
    assert!(titles(&second).is_empty());
    let (one, two) = tokio::join!(
        request(&app, "/api/v1/assignments"),
        request(&app, "/api/v1/assignments?completion=all")
    );
    assert_eq!(one.0, StatusCode::OK);
    assert_eq!(two.0, StatusCode::OK);
    state.begin_shutdown();
    let (status, body) = request(&app, "/api/v1/assignments").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, json!({"error":{"code":"unavailable"}}));
    state.shutdown().await;
}

#[tokio::test]
async fn malformed_queries_never_echo_input_or_accept_client_clocks() {
    let (_root, _db, state, app) = fixture().await;
    for query in [
        "unknown=SYNTHETIC_PRIVATE_WEB1",
        "completion=all&completion=all",
        "deadline=all&deadline=all",
        "sort=title&sort=course",
        "courseMode=all&courseMode=all",
        "courseId=x&courseId=y",
        "completion=no",
        "deadline=no",
        "sort=descending",
        "courseMode=no",
        "courseMode=course",
        "courseId=x",
        "courseMode=all&courseId=x",
        "courseMode=unassigned&courseId=x",
        "courseMode=course&courseId=",
        "courseMode=course&courseId=+",
        "courseMode=course&courseId=%00",
        "completion=",
        "now=2026-01-01",
        "local_date=2026-01-01",
        "utc_offset=9",
        "timeZone=UTC",
        "courseMode=course&courseId=%GG",
        "courseMode=course&courseId=%",
        "sort[]=title",
        "comple%74ion=all&completion=all",
        "completion=%FF",
    ] {
        let (status, body) = request(&app, &format!("/api/v1/assignments?{query}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(body, json!({"error":{"code":"invalid_query"}}));
    }
    let (status, _) = request(
        &app,
        &format!(
            "/api/v1/assignments?courseMode=course&courseId={}",
            "x".repeat(257)
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request(&app, &format!("/api/v1/assignments?{}", "x".repeat(2049))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    state.shutdown().await;
}

#[tokio::test]
async fn host_origin_methods_bodies_and_absent_routes_are_guarded() {
    let (_root, _db, state, app) = fixture().await;
    for host in [
        None,
        Some("evil.example:32123"),
        Some("127.0.0.1:1"),
        Some("127.0.0.1.evil.example:32123"),
        Some("192.168.1.2:32123"),
    ] {
        let mut builder = Request::builder().uri("/api/v1/assignments");
        if let Some(host) = host {
            builder = builder.header("host", host);
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    for origin in [
        "null",
        "https://127.0.0.1:32123",
        "http://evil.example",
        "http://127.0.0.1:1",
        "http://localhost:32123",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/assignments")
                    .header("host", format!("127.0.0.1:{PORT}"))
                    .header("origin", origin)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-origin")
        );
    }
    for host in [format!("127.0.0.1:{PORT}"), format!("localhost:{PORT}")] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health/live")
                    .header("host", &host)
                    .header("origin", format!("http://{host}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    for method in ["POST", "PUT", "DELETE", "OPTIONS", "HEAD"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/api/v1/assignments")
                    .header("host", format!("127.0.0.1:{PORT}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-methods")
        );
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/assignments")
                .header("host", format!("127.0.0.1:{PORT}"))
                .header("content-type", "application/json")
                .body(Body::from("{PRIVATE invalid json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"error":{"code":"invalid_request"}})
    );
    for path in ["/api/v1/sync", "/api/v1/settings/status"] {
        assert_eq!(request(&app, path).await.0, StatusCode::NOT_FOUND);
    }
    assert_eq!(
        request(&app, "/api/v1/assignments/id/completion").await.0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        request(&app, "/health/live").await.1,
        json!({"status":"alive"})
    );
    state.shutdown().await;
}

fn put_request(id: &str, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(format!("/api/v1/assignments/{id}/completion"))
        .header("host", format!("127.0.0.1:{PORT}"))
        .header("origin", format!("http://127.0.0.1:{PORT}"))
        .header("sec-fetch-site", "same-origin")
        .header("sec-fetch-mode", "same-origin")
        .header("x-assignment-manager", "web-3a")
        .header("content-type", "application/json")
        .body(body.into())
        .unwrap()
}
async fn send_json(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["content-type"], "application/json");
    assert!(
        !response
            .headers()
            .contains_key("access-control-allow-origin")
    );
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(!text.contains(PRIVATE_MARKER));
    assert!(!text.contains(SOURCE_MARKER));
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn put(app: &Router, id: &str, completion: &str) -> (StatusCode, Value) {
    send_json(
        app,
        put_request(id, json!({"completion":completion}).to_string()),
    )
    .await
}

// Test-only raw connection verifies *all* v3 tables, timestamps and source keys.
fn db_snapshot(
    db: &PocDatabase,
    include_completion: bool,
) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    [
        "courses",
        "assignments",
        "assignment_sources",
        "course_sources",
        "course_schedule",
        "sync_scopes",
    ]
    .into_iter()
    .map(|table| {
        let columns = if table == "assignments" && !include_completion {
            "id,course_id,title,deadline_kind,deadline_value,submission,submission_url,created_at"
        } else {
            "*"
        };
        let mut stmt = conn
            .prepare(&format!("SELECT {columns} FROM {table} ORDER BY 1,2"))
            .unwrap();
        let count = stmt.column_count();
        stmt.query_map([], |row| (0..count).map(|i| row.get(i)).collect())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    })
    .collect()
}

#[tokio::test]
async fn put_is_idempotent_commits_before_ack_and_preserves_every_other_field_across_restart() {
    use assignment_manager::import::{ExternalRecordKey, OriginKind};
    let (_root, db, state, app) = fixture().await;
    let (_, before) = request(&app, "/api/v1/assignments?completion=all").await;
    let id = item(&before, "い・日付のみ")["id"].as_str().unwrap();
    let mut store = SqliteStore::open(db.path()).unwrap();
    let key = ExternalRecordKey {
        origin: OriginKind::Manual,
        scope: "synthetic".into(),
        id: SOURCE_MARKER.into(),
    };
    store
        .bind_assignment_source(&AssignmentId::new(id), &key)
        .unwrap();
    drop(store);
    let unrelated = db_snapshot(&db, false);
    assert_eq!(
        put(&app, id, "completed").await,
        (StatusCode::OK, json!({"id":id,"completion":"completed"}))
    );
    let committed = db_snapshot(&db, true);
    // Drop a successful response body, then retry the same request: no toggle or timestamp change.
    let lost = app
        .clone()
        .oneshot(put_request(id, r#"{"completion":"completed"}"#))
        .await
        .unwrap();
    assert_eq!(lost.status(), StatusCode::OK);
    drop(lost);
    assert_eq!(put(&app, id, "completed").await.0, StatusCode::OK);
    assert_eq!(db_snapshot(&db, true), committed);
    assert_eq!(db_snapshot(&db, false), unrelated);
    let (_, completed) = request(&app, "/api/v1/assignments?completion=completed").await;
    assert_eq!(item(&completed, "い・日付のみ")["completion"], "completed");
    let (_, incomplete) = request(&app, "/api/v1/assignments").await;
    assert!(!titles(&incomplete).contains(&"い・日付のみ"));
    let (_, after) = request(&app, "/api/v1/assignments?completion=all").await;
    assert_eq!(after["courses"], before["courses"]);
    assert_eq!(after["totalCount"], 11);
    state.shutdown().await;
    let reopened = common::database(db.path().parent().unwrap()).unwrap();
    let state = PocState::open(&reopened, FixedClock(NOW.parse().unwrap()))
        .await
        .unwrap();
    let app = common::authenticated_router(state.clone(), PORT).await;
    assert_eq!(db_snapshot(&reopened, true), committed);
    assert_eq!(
        item(
            &request(&app, "/api/v1/assignments?completion=all").await.1,
            "い・日付のみ"
        )["completion"],
        "completed"
    );
    for _ in 0..2 {
        assert_eq!(
            put(&app, id, "incomplete").await,
            (StatusCode::OK, json!({"id":id,"completion":"incomplete"}))
        );
    }
    assert_eq!(db_snapshot(&db, false), unrelated);
    state.shutdown().await;
    let store = SqliteStore::open(db.path()).unwrap();
    assert_eq!(
        store
            .find_assignment_by_external_key(&key)
            .unwrap()
            .unwrap()
            .completion,
        CompletionState::Incomplete
    );
}

#[tokio::test]
async fn completion_input_errors_are_fixed_and_never_mutate_or_echo_secrets() {
    let (_root, db, state, app) = fixture().await;
    let (_, all) = request(&app, "/api/v1/assignments?completion=all").await;
    let id = all["items"][0]["id"].as_str().unwrap();
    let before = db_snapshot(&db, true);
    for body in [
        "{PRIVATE invalid",
        "null",
        "[]",
        "{}",
        r#"{"completion":null}"#,
        r#"{"completion":"all"}"#,
        r#"{"completion":"completed","secret":"SYNTHETIC_PRIVATE_WEB1"}"#,
        r#"{"completion":"completed","completion":"incomplete"}"#,
    ] {
        assert_eq!(
            send_json(&app, put_request(id, body)).await,
            (
                StatusCode::BAD_REQUEST,
                json!({"error":{"code":"invalid_request"}})
            )
        );
    }
    for invalid in [
        "%20",
        "%00",
        "%FF",
        "%2F",
        "%5C",
        ".",
        "..",
        &"x".repeat(257),
    ] {
        assert_eq!(
            put(&app, invalid, "completed").await.0,
            StatusCode::BAD_REQUEST,
            "{invalid}"
        );
    }
    assert_eq!(
        put(&app, "unknown-synthetic-id", "completed").await,
        (StatusCode::NOT_FOUND, json!({"error":{"code":"not_found"}}))
    );
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/jsonp"),
        Some("application/problem+json"),
        Some("application/json; charset=latin1"),
    ] {
        let mut req = put_request(id, r#"{"completion":"completed"}"#);
        req.headers_mut().remove("content-type");
        if let Some(value) = content_type {
            req.headers_mut()
                .insert("content-type", value.parse().unwrap());
        }
        assert_eq!(
            send_json(&app, req).await,
            (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                json!({"error":{"code":"unsupported_media_type"}})
            )
        );
    }
    for declared in [false, true] {
        let mut req = put_request(id, "x".repeat(1025));
        if declared {
            req.headers_mut()
                .insert("content-length", "1025".parse().unwrap());
        }
        assert_eq!(
            send_json(&app, req).await,
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                json!({"error":{"code":"payload_too_large"}})
            )
        );
    }
    assert_eq!(db_snapshot(&db, true), before);
    state.shutdown().await;
}

#[tokio::test]
async fn unsafe_requests_require_exact_origin_json_and_non_simple_header() {
    let (_root, db, state, app) = fixture().await;
    let id = request(&app, "/api/v1/assignments").await.1["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let before = db_snapshot(&db, true);
    for (header, value, expected) in [
        ("host", Some("evil.example:32123"), 400),
        ("host", None, 400),
        ("origin", None, 403),
        ("origin", Some("null"), 403),
        ("origin", Some("http://localhost:32123"), 403),
        ("origin", Some("http://evil.example"), 403),
        ("sec-fetch-site", Some("cross-site"), 403),
        ("sec-fetch-site", Some("same-site"), 403),
        ("sec-fetch-site", Some("none"), 403),
        ("sec-fetch-mode", Some("navigate"), 403),
        ("x-assignment-manager", None, 403),
        ("x-assignment-manager", Some("invalid"), 403),
    ] {
        let mut req = put_request(&id, r#"{"completion":"completed"}"#);
        req.headers_mut().remove(header);
        if let Some(value) = value {
            req.headers_mut().insert(header, value.parse().unwrap());
        }
        assert_eq!(send_json(&app, req).await.0.as_u16(), expected, "{header}");
    }
    for header in [
        "host",
        "origin",
        "sec-fetch-site",
        "sec-fetch-mode",
        "content-type",
        "x-assignment-manager",
    ] {
        let mut req = put_request(&id, r#"{"completion":"completed"}"#);
        let value = req.headers()[header].clone();
        req.headers_mut().append(header, value);
        assert!(
            !send_json(&app, req).await.0.is_success(),
            "duplicate {header}"
        );
    }
    assert_eq!(db_snapshot(&db, true), before);
    // CLI compatibility: Origin and custom header remain mandatory without Fetch Metadata.
    let mut req = put_request(&id, r#"{"completion":"completed"}"#);
    req.headers_mut().remove("sec-fetch-site");
    req.headers_mut().remove("sec-fetch-mode");
    assert_eq!(send_json(&app, req).await.0, StatusCode::OK);
    state.begin_shutdown();
    assert_eq!(
        put(&app, &id, "incomplete").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    state.shutdown().await;
}

#[tokio::test]
async fn failed_commit_is_rolled_back_with_fixed_error_and_no_database_path_or_cause() {
    let (_root, db, state, app) = fixture().await;
    let id = request(&app, "/api/v1/assignments").await.1["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    conn.execute_batch("CREATE TRIGGER reject_completion BEFORE UPDATE ON assignments BEGIN SELECT RAISE(ABORT, 'SYNTHETIC_PRIVATE_WEB1 SQL cause'); END;").unwrap();
    let before = db_snapshot(&db, true);
    let (status, body) = put(&app, &id, "completed").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, json!({"error":{"code":"internal_error"}}));
    assert!(!body.to_string().contains(db.path().to_str().unwrap()));
    assert_eq!(db_snapshot(&db, true), before);
    conn.execute_batch("DROP TRIGGER reject_completion")
        .unwrap();
    assert_eq!(put(&app, &id, "completed").await.0, StatusCode::OK);
    state.shutdown().await;
}

#[tokio::test]
async fn static_allowlist_has_safe_types_and_no_desktop_or_filesystem_access() {
    let (_root, _db, state, app) = fixture().await;
    for (path, content_type) in [
        ("/", "text/html; charset=utf-8"),
        ("/styles.css", "text/css; charset=utf-8"),
        ("/web_app.js", "text/javascript; charset=utf-8"),
        ("/http_api.js", "text/javascript; charset=utf-8"),
        ("/list_view.js", "text/javascript; charset=utf-8"),
        ("/list_tasks.js", "text/javascript; charset=utf-8"),
        ("/presentation.js", "text/javascript; charset=utf-8"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("host", format!("127.0.0.1:{PORT}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], content_type);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert!(
            response.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
        let body = to_bytes(response.into_body(), 65536).await.unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("__TAURI__"));
        if path == "/" {
            assert!(body.contains("合成データを使ったローカルデモ"));
            assert!(!body.contains("id=\"sync-now\""));
            assert!(!body.contains("id=\"show-settings\""));
        }
    }
    for path in [
        "/../Cargo.toml",
        "/%2e%2e/Cargo.toml",
        "/frontend/../Cargo.toml",
        "/styles.css/..",
        "/Cargo.lock",
        "/Cargo.toml",
        "/.git/config",
        "/src/main.rs",
        "/assignment-manager.db",
        "/target/web-poc/assignment-manager.db",
        "/SYNTHETIC-ONLY",
        "/app.js",
        "/tauri_api.js",
        "/settings_view.js",
        "/sync.js",
    ] {
        assert_eq!(request(&app, path).await.0, StatusCode::NOT_FOUND, "{path}");
    }
    state.shutdown().await;
}

#[tokio::test]
async fn restart_preserves_synthetic_database_and_completion_without_reseeding() {
    let (_root, db, state, app) = fixture().await;
    let (_, before) = request(&app, "/api/v1/assignments?completion=all").await;
    let target = item(&before, "い・日付のみ")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    state.shutdown().await;
    let mut store = SqliteStore::open(db.path()).unwrap();
    store
        .set_completion(&AssignmentId::new(&target), CompletionState::Completed)
        .unwrap();
    drop(store);
    let db2 = common::database(db.path().parent().unwrap()).unwrap();
    let state = PocState::open(&db2, FixedClock(NOW.parse().unwrap()))
        .await
        .unwrap();
    let (_, after) = request(
        &common::authenticated_router(state.clone(), PORT).await,
        "/api/v1/assignments?completion=all",
    )
    .await;
    assert_eq!(after["totalCount"], 11);
    assert_eq!(after["courses"], before["courses"]);
    assert_eq!(item(&after, "い・日付のみ")["id"], target);
    assert_eq!(item(&after, "い・日付のみ")["completion"], "completed");
    state.shutdown().await;
}

#[tokio::test]
async fn readiness_failure_never_listens_and_has_no_path_or_driver_cause() {
    let root = tempfile::tempdir().unwrap();
    let db = common::database(&root.path().join("poc")).unwrap();
    std::fs::write(db.path(), b"SYNTHETIC_PRIVATE_WEB1 invalid sqlite").unwrap();
    let reserved = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);
    match PocServer::start(&db, FixedClock(NOW.parse().unwrap()), port).await {
        Err(error) => assert_eq!(error, WebError::Internal),
        Ok(_) => panic!("invalid database became ready"),
    }
    let _still_available =
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
}

#[test]
fn foreign_or_incomplete_directories_are_rejected_without_touching_database() {
    let root = tempfile::tempdir().unwrap();
    let bytes = b"DO_NOT_OPEN_PRIVATE_DESKTOP_DATABASE";
    std::fs::write(root.path().join("assignment-manager.db"), bytes).unwrap();
    assert!(common::database(root.path()).is_err());
    assert_eq!(
        std::fs::read(root.path().join("assignment-manager.db")).unwrap(),
        bytes
    );
    assert!(!root.path().join("SYNTHETIC-ONLY").exists());
    assert!(common::database(std::path::Path::new("relative")).is_err());
}

#[tokio::test]
async fn actual_listener_is_ipv4_loopback_and_graceful_shutdown_reopens() {
    let root = tempfile::tempdir().unwrap();
    let db = common::database(&root.path().join("poc")).unwrap();
    let server = PocServer::start(&db, FixedClock(NOW.parse().unwrap()), 0)
        .await
        .unwrap();
    let address = server.local_addr();
    assert_eq!(address.ip(), std::net::Ipv4Addr::LOCALHOST);
    let (send, stop) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stop.await;
    }));
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client.write_all(format!("GET /api/v1/assignments?completion=all HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(StdDuration::from_secs(5), client.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    let rendered = String::from_utf8(response).unwrap();
    assert!(rendered.starts_with("HTTP/1.1 401 Unauthorized"));
    assert!(rendered.contains("authentication_failed"));
    send.send(()).unwrap();
    tokio::time::timeout(StdDuration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
    let state = PocState::open(&db, FixedClock(NOW.parse().unwrap()))
        .await
        .unwrap();
    assert_eq!(
        request(
            &common::authenticated_router(state.clone(), PORT).await,
            "/api/v1/assignments?completion=all"
        )
        .await
        .1["totalCount"],
        11
    );
    state.shutdown().await;
    let store = SqliteStore::open(db.path()).unwrap();
    assert_eq!(load_assignment_list(&store).unwrap().len(), 11);
}
