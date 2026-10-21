use assignment_manager::{
    domain::{AssignmentId, CompletionState},
    persistence::SqliteStore,
    query::load_assignment_list,
};
use assignment_manager_web_server::{
    Clock, PocServer, PocState, WebError,
    poc::{PRIVATE_MARKER, PocDatabase, SOURCE_MARKER},
    router,
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
    let db = PocDatabase::prepare(&root.path().join("poc")).unwrap();
    let state = PocState::open(&db, FixedClock(NOW.parse().unwrap()))
        .await
        .unwrap();
    let app = router(state.clone(), PORT);
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
        &router(state.clone(), PORT),
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
    let db = PocDatabase::prepare(&root.path().join("poc")).unwrap();
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
    let app = router(state.clone(), PORT);
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
    for path in [
        "/api/v1/sync",
        "/api/v1/settings/status",
        "/api/v1/assignments/id/completion",
        "/",
    ] {
        assert_eq!(request(&app, path).await.0, StatusCode::NOT_FOUND);
    }
    assert_eq!(
        request(&app, "/health/live").await.1,
        json!({"status":"alive"})
    );
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
    let db2 = PocDatabase::prepare(db.path().parent().unwrap()).unwrap();
    let state = PocState::open(&db2, FixedClock(NOW.parse().unwrap()))
        .await
        .unwrap();
    let (_, after) = request(
        &router(state.clone(), PORT),
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
    let db = PocDatabase::prepare(&root.path().join("poc")).unwrap();
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
    assert!(PocDatabase::prepare(root.path()).is_err());
    assert_eq!(
        std::fs::read(root.path().join("assignment-manager.db")).unwrap(),
        bytes
    );
    assert!(!root.path().join("SYNTHETIC-ONLY").exists());
    assert!(PocDatabase::prepare(std::path::Path::new("relative")).is_err());
}

#[tokio::test]
async fn actual_listener_is_ipv4_loopback_and_graceful_shutdown_reopens() {
    let root = tempfile::tempdir().unwrap();
    let db = PocDatabase::prepare(&root.path().join("poc")).unwrap();
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
    assert!(rendered.starts_with("HTTP/1.1 200 OK"));
    assert!(rendered.contains("\"totalCount\":11"));
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
            &router(state.clone(), PORT),
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
