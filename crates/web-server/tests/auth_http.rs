mod common;
use assignment_manager_web_server::{
    Clock, PocState, WebError,
    auth::{self, AuthClock},
    poc::PocDatabase,
    router,
};
use axum::{Router, body::Body, http::Request};
use chrono::{DateTime, Utc};
use common::{PASSWORD, mutation, send};
use rusqlite::Connection;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};

const PORT: u16 = 32123;
struct QueryClock;
impl Clock for QueryClock {
    fn now(&self) -> DateTime<Utc> {
        "2026-10-22T00:00:00Z".parse().unwrap()
    }
}
#[derive(Clone)]
struct TestClock(Arc<AtomicI64>);
impl AuthClock for TestClock {
    fn seconds(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
async fn fixture() -> (tempfile::TempDir, PocDatabase, PocState, Router, TestClock) {
    let root = tempfile::tempdir().unwrap();
    let db = common::database(&root.path().join("poc")).unwrap();
    let clock = TestClock(Arc::new(AtomicI64::new(1_800_000_000)));
    let state = PocState::open_with_auth_clock(&db, QueryClock, clock.clone())
        .await
        .unwrap();
    let app = router(state.clone(), PORT);
    (root, db, state, app, clock)
}
fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut req = Request::builder()
        .uri(path)
        .header("host", format!("127.0.0.1:{PORT}"));
    if let Some(cookie) = cookie {
        req = req.header("cookie", cookie);
    }
    req.body(Body::empty()).unwrap()
}
fn write(path: &str, body: &str, cookie: Option<&str>, csrf: Option<&str>) -> Request<Body> {
    let mut req = mutation(path, body, PORT);
    if let Some(value) = cookie {
        req.headers_mut().insert("cookie", value.parse().unwrap());
    }
    if let Some(value) = csrf {
        req.headers_mut()
            .insert("x-csrf-token", value.parse().unwrap());
    }
    req
}
fn login_request(login: &str, password: &str) -> Request<Body> {
    mutation(
        "/api/v1/auth/login",
        &json!({"login":login,"password":password}).to_string(),
        PORT,
    )
}
fn assignments_snapshot(db: &PocDatabase) -> Vec<u8> {
    std::fs::read(db.path()).unwrap()
}

#[test]
fn bootstrap_is_atomic_singleton_random_user_and_salted_hash_only() {
    let root = tempfile::tempdir().unwrap();
    let db = PocDatabase::prepare(&root.path().join("poc")).unwrap();
    assert!(matches!(
        auth::bootstrap_admin(&db, "owner", "short".to_owned().into()),
        Err(WebError::InvalidRequest)
    ));
    let user = auth::bootstrap_admin(&db, "owner", PASSWORD.to_owned().into()).unwrap();
    assert_eq!(
        serde_json::to_value(user).unwrap().as_str().unwrap().len(),
        64
    );
    assert!(matches!(
        auth::bootstrap_admin(
            &db,
            "changed",
            "replacement-password-only".to_owned().into()
        ),
        Err(WebError::AccountExists)
    ));
    let conn = Connection::open(auth::control_path(&db)).unwrap();
    let (login, verifier, count): (String, String, i64) = conn
        .query_row(
            "SELECT login,verifier,(SELECT count(*) FROM accounts) FROM accounts",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(login, "owner");
    assert_eq!(count, 1);
    assert!(verifier.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
    assert!(
        argon2::PasswordVerifier::verify_password(
            &argon2::Argon2::default(),
            PASSWORD.as_bytes(),
            &argon2::PasswordHash::new(&verifier).unwrap()
        )
        .is_ok()
    );
    assert!(
        !std::fs::read(auth::control_path(&db))
            .unwrap()
            .windows(PASSWORD.len())
            .any(|v| v == PASSWORD.as_bytes())
    );
    assert_eq!(
        conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        Connection::open(db.path())
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    let other = PocDatabase::prepare(&root.path().join("other")).unwrap();
    auth::bootstrap_admin(&other, "owner", PASSWORD.to_owned().into()).unwrap();
    let second: String = Connection::open(auth::control_path(&other))
        .unwrap()
        .query_row("SELECT verifier FROM accounts", [], |r| r.get(0))
        .unwrap();
    assert_ne!(verifier, second);
}

#[test]
fn bootstrap_failed_commit_leaves_no_account_and_foreign_control_schema_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let db = common::database(&root.path().join("poc")).unwrap();
    let conn = Connection::open(auth::control_path(&db)).unwrap();
    conn.execute_batch("DELETE FROM accounts; CREATE TRIGGER reject_account BEFORE INSERT ON accounts BEGIN SELECT RAISE(ABORT,'private cause'); END;").unwrap();
    assert!(matches!(
        auth::bootstrap_admin(&db, "owner", PASSWORD.to_owned().into()),
        Err(WebError::Internal)
    ));
    assert_eq!(
        conn.query_row("SELECT count(*) FROM accounts", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    conn.execute_batch("DROP TRIGGER reject_account; PRAGMA user_version=9")
        .unwrap();
    assert!(matches!(
        auth::bootstrap_admin(&db, "owner", PASSWORD.to_owned().into()),
        Err(WebError::Internal)
    ));
}

#[tokio::test]
async fn no_account_never_disables_auth_and_second_server_cannot_invalidate_first() {
    let root = tempfile::tempdir().unwrap();
    let db = PocDatabase::prepare(&root.path().join("poc")).unwrap();
    assert!(matches!(
        PocState::open(&db, QueryClock).await,
        Err(WebError::SetupRequired)
    ));
    auth::bootstrap_admin(&db, "owner", PASSWORD.to_owned().into()).unwrap();
    let state = PocState::open(&db, QueryClock).await.unwrap();
    assert!(matches!(
        PocState::open(&db, QueryClock).await,
        Err(WebError::Unavailable)
    ));
    assert!(matches!(
        auth::bootstrap_admin(&db, "other", PASSWORD.to_owned().into()),
        Err(WebError::Unavailable)
    ));
    state.shutdown().await;
}

#[tokio::test]
async fn unauthenticated_requests_and_login_failures_cannot_change_assignments() {
    let (_root, db, state, app, _) = fixture().await;
    let before = assignments_snapshot(&db);
    let mut direct = get("/api/v1/assignments", None);
    direct
        .headers_mut()
        .insert("sec-fetch-site", "none".parse().unwrap());
    direct
        .headers_mut()
        .insert("sec-fetch-mode", "navigate".parse().unwrap());
    assert_eq!(send(&app, direct).await.0, 401);
    for req in [
        get("/api/v1/assignments", None),
        get("/api/v1/session", None),
        write(
            "/api/v1/assignments/anything/completion",
            r#"{"completion":"completed"}"#,
            None,
            None,
        ),
    ] {
        let (status, _, value) = send(&app, req).await;
        assert_eq!(status, 401);
        assert_eq!(value, json!({"error":{"code":"authentication_failed"}}));
    }
    for (login, password) in [("owner", "wrong"), ("unknown", PASSWORD)] {
        let (status, _, value) = send(&app, login_request(login, password)).await;
        assert_eq!(status, 401);
        assert_eq!(value, json!({"error":{"code":"authentication_failed"}}));
    }
    Connection::open(auth::control_path(&db))
        .unwrap()
        .execute("UPDATE accounts SET status='disabled'", [])
        .unwrap();
    assert_eq!(send(&app, login_request("owner", PASSWORD)).await.0, 401);
    assert_eq!(assignments_snapshot(&db), before);
    state.shutdown().await;
}

#[tokio::test]
async fn cookie_attributes_hash_only_storage_reload_rotation_logout_and_replay() {
    let (_root, db, state, app, _) = fixture().await;
    let (status, headers, body) = send(&app, login_request("owner", PASSWORD)).await;
    assert_eq!(status, 200);
    let set = headers["set-cookie"].to_str().unwrap();
    for attr in ["HttpOnly", "SameSite=Strict", "Path=/", "Max-Age=43200"] {
        assert!(set.contains(attr));
    }
    assert!(!set.contains("Secure"));
    assert!(!set.contains("Domain="));
    assert!(!set.starts_with("__Host-"));
    let cookie = set.split(';').next().unwrap();
    let token = cookie.split_once('=').unwrap().1;
    for secret in [token, PASSWORD, "verifier", "token_hash"] {
        assert!(!body.to_string().contains(secret));
    }
    let conn = Connection::open(auth::control_path(&db)).unwrap();
    let stored: Vec<u8> = conn
        .query_row("SELECT token_hash FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored.len(), 32);
    assert_ne!(stored, token.as_bytes());
    assert!(
        !std::fs::read(auth::control_path(&db))
            .unwrap()
            .windows(token.len())
            .any(|v| v == token.as_bytes())
    );
    assert_eq!(
        send(&app, get("/api/v1/session", Some(cookie))).await.2,
        body
    );
    assert_eq!(
        send(&app, get("/api/v1/assignments", Some(cookie))).await.0,
        200
    );
    let mut req = login_request("owner", PASSWORD);
    req.headers_mut().insert("cookie", cookie.parse().unwrap());
    let (_, h, new) = send(&app, req).await;
    let rotated = h["set-cookie"].to_str().unwrap().split(';').next().unwrap();
    assert_ne!(cookie, rotated);
    assert_ne!(body["csrfToken"], new["csrfToken"]);
    assert_eq!(
        send(&app, get("/api/v1/assignments", Some(cookie))).await.0,
        401
    );
    let (s, h, _) = send(
        &app,
        write(
            "/api/v1/auth/logout",
            "{}",
            Some(rotated),
            new["csrfToken"].as_str(),
        ),
    )
    .await;
    assert_eq!(s, 200);
    assert!(h["set-cookie"].to_str().unwrap().contains("Max-Age=0"));
    for path in ["/api/v1/session", "/api/v1/assignments"] {
        assert_eq!(send(&app, get(path, Some(rotated))).await.0, 401);
    }
    assert_eq!(
        send(
            &app,
            write(
                "/api/v1/assignments/x/completion",
                r#"{"completion":"completed"}"#,
                Some(rotated),
                new["csrfToken"].as_str()
            )
        )
        .await
        .0,
        401
    );
    state.shutdown().await;
}

#[tokio::test]
async fn csrf_missing_wrong_other_session_duplicate_and_old_token_are_rejected() {
    let (_root, db, state, app, _) = fixture().await;
    let (cookie, csrf) = common::login(&app, PORT).await;
    let (other, other_csrf) = common::login(&app, PORT).await;
    let before = assignments_snapshot(&db);
    for token in [None, Some("wrong"), Some(other_csrf.as_str())] {
        for path in ["/api/v1/assignments/x/completion", "/api/v1/auth/logout"] {
            assert_eq!(
                send(&app, write(path, "{}", Some(&cookie), token)).await.0,
                403
            );
        }
    }
    let mut req = write("/api/v1/auth/logout", "{}", Some(&cookie), Some(&csrf));
    req.headers_mut()
        .append("x-csrf-token", csrf.parse().unwrap());
    assert_eq!(send(&app, req).await.0, 403);
    assert_eq!(
        send(
            &app,
            write("/api/v1/auth/logout", "{}", Some(&other), Some(&other_csrf))
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send(
            &app,
            write("/api/v1/auth/logout", "{}", Some(&other), Some(&other_csrf))
        )
        .await
        .0,
        401
    );
    assert_eq!(assignments_snapshot(&db), before);
    state.shutdown().await;
}

#[tokio::test]
async fn idle_and_absolute_expiry_are_server_enforced_and_restart_revokes_sessions() {
    let (_root, db, state, app, clock) = fixture().await;
    let (cookie, _) = common::login(&app, PORT).await;
    clock.0.fetch_add(auth::IDLE_SECONDS, Ordering::SeqCst);
    assert_eq!(
        send(&app, get("/api/v1/session", Some(&cookie))).await.0,
        401
    );
    let (cookie, _) = common::login(&app, PORT).await;
    for _ in 0..(auth::ABSOLUTE_SECONDS / 900 - 1) {
        clock.0.fetch_add(900, Ordering::SeqCst);
        assert_eq!(
            send(&app, get("/api/v1/session", Some(&cookie))).await.0,
            200
        );
    }
    clock.0.fetch_add(900, Ordering::SeqCst);
    assert_eq!(
        send(&app, get("/api/v1/assignments", Some(&cookie)))
            .await
            .0,
        401
    );
    let (cookie, _) = common::login(&app, PORT).await;
    let before = assignments_snapshot(&db);
    state.shutdown().await;
    let reopened = PocState::open_with_auth_clock(&db, QueryClock, clock)
        .await
        .unwrap();
    let app = router(reopened.clone(), PORT);
    assert_eq!(
        send(&app, get("/api/v1/session", Some(&cookie))).await.0,
        401
    );
    common::login(&app, PORT).await;
    assert_eq!(assignments_snapshot(&db), before);
    reopened.shutdown().await;
}

#[tokio::test]
async fn disabled_account_and_foreign_internal_owner_are_forbidden_before_db_access() {
    let (_root, db, state, app, _) = fixture().await;
    let (cookie, csrf) = common::login(&app, PORT).await;
    let conn = Connection::open(auth::control_path(&db)).unwrap();
    conn.execute("UPDATE accounts SET status='disabled'", [])
        .unwrap();
    assert_eq!(
        send(&app, get("/api/v1/assignments", Some(&cookie)))
            .await
            .0,
        403
    );
    conn.execute_batch("PRAGMA foreign_keys=OFF; UPDATE accounts SET status='enabled',user_id='different-internal-user'; UPDATE sessions SET user_id='different-internal-user';").unwrap();
    let before = assignments_snapshot(&db);
    assert_eq!(
        send(&app, get("/api/v1/assignments", Some(&cookie)))
            .await
            .0,
        403
    );
    assert_eq!(
        send(
            &app,
            write(
                "/api/v1/assignments/x/completion",
                r#"{"completion":"completed"}"#,
                Some(&cookie),
                Some(&csrf)
            )
        )
        .await
        .0,
        403
    );
    assert_eq!(assignments_snapshot(&db), before);
    state.shutdown().await;
}

#[tokio::test]
async fn requests_cannot_choose_user_database_or_credential_owner() {
    let (_root, db, state, app, _) = fixture().await;
    let (cookie, csrf) = common::login(&app, PORT).await;
    for query in [
        "userId=other",
        "database=other.db",
        "tenantPath=..",
        "credentialOwner=other",
    ] {
        assert_eq!(
            send(
                &app,
                get(&format!("/api/v1/assignments?{query}"), Some(&cookie))
            )
            .await
            .0,
            400
        );
    }
    assert_eq!(
        send(
            &app,
            write(
                "/api/v1/assignments/x/completion",
                r#"{"completion":"completed","userId":"other"}"#,
                Some(&cookie),
                Some(&csrf)
            )
        )
        .await
        .0,
        400
    );
    let mut req = get("/api/v1/assignments", None);
    req.headers_mut()
        .insert("x-user-id", "owner".parse().unwrap());
    assert_eq!(send(&app, req).await.0, 401);
    for malformed in [
        format!("{}=bad", auth::COOKIE_NAME),
        format!("{cookie}; {cookie}"),
    ] {
        assert_eq!(
            send(&app, get("/api/v1/session", Some(&malformed))).await.0,
            401
        );
    }
    assert_eq!(
        Connection::open(db.path())
            .unwrap()
            .query_row("SELECT count(*) FROM assignments", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        11
    );
    state.shutdown().await;
}

#[tokio::test]
async fn login_csrf_origin_metadata_json_and_body_validation_are_fail_closed() {
    let (_root, _db, state, app, _) = fixture().await;
    for (name, value, status) in [
        ("origin", None, 403),
        ("origin", Some("http://evil.example"), 403),
        ("sec-fetch-site", Some("same-site"), 403),
        ("sec-fetch-mode", Some("navigate"), 403),
        ("x-assignment-manager", None, 403),
        ("content-type", Some("text/plain"), 415),
    ] {
        let mut req = login_request("owner", PASSWORD);
        req.headers_mut().remove(name);
        if let Some(value) = value {
            req.headers_mut().insert(name, value.parse().unwrap());
        }
        assert_eq!(send(&app, req).await.0, status);
    }
    for body in [
        "{",
        "null",
        "[]",
        "{}",
        r#"{"login":"owner","password":"test","userId":"other"}"#,
        r#"{"login":"owner","login":"owner","password":"test"}"#,
    ] {
        let (status, _, value) = send(&app, mutation("/api/v1/auth/login", body, PORT)).await;
        assert_eq!(status, 400);
        assert_eq!(value, json!({"error":{"code":"invalid_request"}}));
    }
    assert_eq!(
        send(
            &app,
            mutation("/api/v1/auth/login", &"x".repeat(1025), PORT)
        )
        .await
        .0,
        413
    );
    assert_eq!(send(&app, login_request("../db", PASSWORD)).await.0, 400);
    state.shutdown().await;
}

#[tokio::test]
async fn login_rate_limit_is_global_bounded_persistent_and_recovers_after_window() {
    let (_root, db, state, app, clock) = fixture().await;
    for n in 0..5 {
        assert_eq!(
            send(&app, login_request(&format!("unknown{n}"), "wrong"))
                .await
                .0,
            401
        );
    }
    let (s, h, v) = send(&app, login_request("owner", PASSWORD)).await;
    assert_eq!(s, 429);
    assert_eq!(h["retry-after"], "60");
    assert_eq!(v, json!({"error":{"code":"rate_limited"}}));
    state.shutdown().await;
    let state = PocState::open_with_auth_clock(&db, QueryClock, clock.clone())
        .await
        .unwrap();
    let app = router(state.clone(), PORT);
    assert_eq!(send(&app, login_request("owner", PASSWORD)).await.0, 429);
    clock.0.fetch_add(60, Ordering::SeqCst);
    common::login(&app, PORT).await;
    state.shutdown().await;
}
