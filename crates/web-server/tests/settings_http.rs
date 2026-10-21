mod common;
use assignment_manager_web_server::{
    Clock, PocState,
    auth::{self, AuthClock},
    poc::PocDatabase,
    router,
};
use axum::{
    Router,
    body::Body,
    http::{Method, Request},
};
use chrono::{DateTime, Utc};
use common::{PASSWORD, send};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
const PORT: u16 = 32123;
const MOODLE: &str = "/api/v1/settings/moodle";
const WEBCLASS: &str = "/api/v1/settings/webclass";
const STATUS: &str = "/api/v1/settings/status";
const REAUTH: &str = "/api/v1/auth/reauth";
const URL: &str = "https://example.invalid/calendar?token=SYNTHETIC_HTTP_3B";
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
    let state = common::open_with_auth_clock(&db, QueryClock, clock.clone())
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
fn write(
    method: Method,
    path: &str,
    body: &str,
    cookie: Option<&str>,
    csrf: Option<&str>,
) -> Request<Body> {
    let mut req = common::mutation(path, body, PORT);
    *req.method_mut() = method;
    if let Some(cookie) = cookie {
        req.headers_mut().insert("cookie", cookie.parse().unwrap());
    }
    if let Some(csrf) = csrf {
        req.headers_mut()
            .insert("x-csrf-token", csrf.parse().unwrap());
    }
    req
}
async fn reauth(app: &Router, cookie: &str, csrf: &str) {
    assert_eq!(
        send(
            app,
            write(
                Method::POST,
                REAUTH,
                &json!({"password":PASSWORD}).to_string(),
                Some(cookie),
                Some(csrf)
            )
        )
        .await
        .0,
        204
    );
}
fn payload(path: &str) -> Value {
    if path == MOODLE {
        json!({"calendarUrl":URL})
    } else {
        json!({"universityId":"synthetic-id-3b","password":"synthetic-password-3b"})
    }
}

#[tokio::test]
async fn all_settings_and_reauth_require_sessions_csrf_origin_host_and_recent_proof() {
    let (_root, _db, state, app, _clock) = fixture().await;
    assert_eq!(send(&app, get(STATUS, None)).await.0, 401);
    for path in [MOODLE, WEBCLASS] {
        for method in [Method::PUT, Method::DELETE] {
            assert_eq!(
                send(&app, write(method, path, "{}", None, None)).await.0,
                401
            );
        }
    }
    assert_eq!(
        send(&app, write(Method::POST, REAUTH, "{}", None, None))
            .await
            .0,
        401
    );
    let (cookie, csrf) = common::login(&app, PORT).await;
    for path in [MOODLE, WEBCLASS] {
        for method in [Method::PUT, Method::DELETE] {
            let body = if method == Method::PUT {
                payload(path)
            } else {
                json!({})
            }
            .to_string();
            let (status, _, value) = send(
                &app,
                write(method.clone(), path, &body, Some(&cookie), Some(&csrf)),
            )
            .await;
            assert_eq!(status, 403);
            assert_eq!(value["error"]["code"], "reauth_required");
            for token in [None, Some("wrong")] {
                assert_eq!(
                    send(
                        &app,
                        write(method.clone(), path, &body, Some(&cookie), token)
                    )
                    .await
                    .0,
                    403
                );
            }
        }
    }
    reauth(&app, &cookie, &csrf).await;
    for path in [MOODLE, WEBCLASS, REAUTH] {
        for (name, value, expected) in [
            ("origin", Some("http://evil.example"), 403),
            ("origin", None, 403),
            ("host", Some("evil.example"), 400),
            ("sec-fetch-site", Some("same-site"), 403),
            ("sec-fetch-mode", Some("navigate"), 403),
            ("x-assignment-manager", None, 403),
            ("content-type", Some("text/plain"), 415),
        ] {
            let mut req = write(
                if path == REAUTH {
                    Method::POST
                } else {
                    Method::PUT
                },
                path,
                "{}",
                Some(&cookie),
                Some(&csrf),
            );
            req.headers_mut().remove(name);
            if let Some(value) = value {
                req.headers_mut().insert(name, value.parse().unwrap());
            }
            assert_eq!(send(&app, req).await.0, expected, "{path} {name}");
        }
    }
    state.shutdown().await;
}

#[tokio::test]
async fn safe_flags_204_atomic_pair_replace_delete_and_restart() {
    let (_root, db, state, app, clock) = fixture().await;
    let (cookie, csrf) = common::login(&app, PORT).await;
    assert_eq!(
        send(&app, get(STATUS, Some(&cookie))).await.2,
        json!({"moodleConfigured":false,"webclassConfigured":false})
    );
    reauth(&app, &cookie, &csrf).await;
    let tripwire = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    tripwire.set_nonblocking(true).unwrap();
    for path in [MOODLE, WEBCLASS] {
        let (status, headers, value) = send(
            &app,
            write(
                Method::PUT,
                path,
                &payload(path).to_string(),
                Some(&cookie),
                Some(&csrf),
            ),
        )
        .await;
        assert_eq!(status, 204);
        assert!(value.is_null());
        assert_eq!(headers["cache-control"], "no-store");
    }
    // No login/fetch is attempted even for a URL pointing at a local tripwire.
    assert_eq!(send(&app,write(Method::PUT,MOODLE,&json!({"calendarUrl":format!("https://{}/calendar?token=synthetic",tripwire.local_addr().unwrap())}).to_string(),Some(&cookie),Some(&csrf))).await.0,204);
    assert_eq!(
        tripwire.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    for path in [MOODLE, WEBCLASS] {
        assert_eq!(
            send(
                &app,
                write(
                    Method::PUT,
                    path,
                    &payload(path).to_string(),
                    Some(&cookie),
                    Some(&csrf)
                )
            )
            .await
            .0,
            204
        );
    }
    let expected = json!({"moodleConfigured":true,"webclassConfigured":true});
    assert_eq!(send(&app, get(STATUS, Some(&cookie))).await.2, expected);
    let conn = Connection::open(auth::control_path(&db)).unwrap();
    let before: Vec<u8> = conn
        .query_row(
            "SELECT envelope FROM web_credentials WHERE kind='webclass'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute_batch("CREATE TRIGGER reject_replace BEFORE UPDATE ON web_credentials BEGIN SELECT RAISE(ABORT,'private SQL cause'); END").unwrap();
    let (status, _, error) = send(
        &app,
        write(
            Method::PUT,
            WEBCLASS,
            &json!({"universityId":"synthetic-replacement","password":"synthetic-new"}).to_string(),
            Some(&cookie),
            Some(&csrf),
        ),
    )
    .await;
    assert_eq!(status, 503);
    assert_eq!(error, json!({"error":{"code":"vault_unavailable"}}));
    assert_eq!(
        conn.query_row(
            "SELECT envelope FROM web_credentials WHERE kind='webclass'",
            [],
            |r| r.get::<_, Vec<u8>>(0)
        )
        .unwrap(),
        before
    );
    conn.execute_batch("DROP TRIGGER reject_replace").unwrap();
    drop(conn);
    state.shutdown().await;
    let state = common::open_with_auth_clock(&db, QueryClock, clock)
        .await
        .unwrap();
    let app = router(state.clone(), PORT);
    assert_eq!(send(&app, get(STATUS, Some(&cookie))).await.0, 401);
    let (cookie, csrf) = common::login(&app, PORT).await;
    assert_eq!(send(&app, get(STATUS, Some(&cookie))).await.2, expected);
    assert_eq!(
        send(
            &app,
            write(Method::DELETE, MOODLE, "{}", Some(&cookie), Some(&csrf))
        )
        .await
        .2["error"]["code"],
        "reauth_required"
    );
    reauth(&app, &cookie, &csrf).await;
    for path in [MOODLE, WEBCLASS] {
        for _ in 0..2 {
            assert_eq!(
                send(
                    &app,
                    write(Method::DELETE, path, "{}", Some(&cookie), Some(&csrf))
                )
                .await
                .0,
                204
            );
        }
    }
    assert_eq!(
        send(&app, get(STATUS, Some(&cookie))).await.2,
        json!({"moodleConfigured":false,"webclassConfigured":false})
    );
    assert_eq!(
        send(
            &app,
            write(
                Method::POST,
                "/api/v1/auth/logout",
                "{}",
                Some(&cookie),
                Some(&csrf)
            )
        )
        .await
        .0,
        200
    );
    assert_eq!(send(&app, get(STATUS, Some(&cookie))).await.0, 401);
    assert_eq!(
        send(
            &app,
            write(
                Method::PUT,
                MOODLE,
                &payload(MOODLE).to_string(),
                Some(&cookie),
                Some(&csrf)
            )
        )
        .await
        .0,
        401
    );
    state.shutdown().await;
}

#[tokio::test]
async fn shared_validation_strict_json_lengths_body_limits_and_no_owner_selector() {
    let (_root, _db, state, app, _) = fixture().await;
    let (cookie, csrf) = common::login(&app, PORT).await;
    reauth(&app, &cookie, &csrf).await;
    for value in [
        "",
        "not a URL",
        "http://example.invalid",
        "https://user:pass@example.invalid",
    ] {
        assert_eq!(
            send(
                &app,
                write(
                    Method::PUT,
                    MOODLE,
                    &json!({"calendarUrl":value}).to_string(),
                    Some(&cookie),
                    Some(&csrf)
                )
            )
            .await
            .0,
            400
        );
    }
    for (id, password) in [("", "p"), ("  ", "p"), ("id", "")] {
        assert_eq!(
            send(
                &app,
                write(
                    Method::PUT,
                    WEBCLASS,
                    &json!({"universityId":id,"password":password}).to_string(),
                    Some(&cookie),
                    Some(&csrf)
                )
            )
            .await
            .0,
            400
        );
    }
    for (path, body) in [
        (
            MOODLE,
            r#"{"calendarUrl":"https://example.invalid","userId":"other"}"#.into(),
        ),
        (
            MOODLE,
            r#"{"calendarUrl":"https://example.invalid","calendarUrl":"https://example.invalid"}"#
                .into(),
        ),
        (
            WEBCLASS,
            r#"{"universityId":"id","password":"a","password":"b"}"#.into(),
        ),
        (
            WEBCLASS,
            r#"{"universityId":"id","password":"p","database":"other.db"}"#.into(),
        ),
        (
            WEBCLASS,
            json!({"universityId":"x".repeat(257),"password":"p"}).to_string(),
        ),
        (
            WEBCLASS,
            json!({"universityId":"id","password":"x".repeat(1025)}).to_string(),
        ),
        (
            MOODLE,
            json!({"calendarUrl":format!("https://example.invalid/{}","x".repeat(2048))})
                .to_string(),
        ),
    ] {
        assert_eq!(
            send(
                &app,
                write(Method::PUT, path, &body, Some(&cookie), Some(&csrf))
            )
            .await
            .0,
            400
        );
    }
    for body in ["{", "null", "[]", "{}"] {
        assert_eq!(
            send(
                &app,
                write(Method::PUT, MOODLE, body, Some(&cookie), Some(&csrf))
            )
            .await
            .0,
            400
        );
    }
    // No Content-Length: exercises streamed extractor limits as well as headers.
    assert_eq!(
        send(
            &app,
            write(
                Method::PUT,
                MOODLE,
                &"x".repeat(4097),
                Some(&cookie),
                Some(&csrf)
            )
        )
        .await
        .0,
        413
    );
    assert_eq!(
        send(
            &app,
            write(
                Method::POST,
                REAUTH,
                &"x".repeat(1025),
                Some(&cookie),
                Some(&csrf)
            )
        )
        .await
        .0,
        413
    );
    for query in [
        "userId=other",
        "database=other.db",
        "tenantPath=..",
        "credentialOwner=other",
    ] {
        assert_eq!(
            send(&app, get(&format!("{STATUS}?{query}"), Some(&cookie)))
                .await
                .0,
            400
        );
        assert_eq!(
            send(
                &app,
                write(
                    Method::PUT,
                    &format!("{MOODLE}?{query}"),
                    &payload(MOODLE).to_string(),
                    Some(&cookie),
                    Some(&csrf)
                )
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
                Method::DELETE,
                MOODLE,
                r#"{"owner":"other"}"#,
                Some(&cookie),
                Some(&csrf)
            )
        )
        .await
        .0,
        400
    );
    state.shutdown().await;
}

#[tokio::test]
async fn reauth_is_session_bound_expires_at_five_minutes_and_shares_login_limit() {
    let (_root, _db, state, app, clock) = fixture().await;
    let (cookie, csrf) = common::login(&app, PORT).await;
    let (other, other_csrf) = common::login(&app, PORT).await;
    let wrong = write(
        Method::POST,
        REAUTH,
        r#"{"password":"synthetic-wrong-password"}"#,
        Some(&cookie),
        Some(&csrf),
    );
    assert_eq!(send(&app, wrong).await.2["error"]["code"], "reauth_failed");
    reauth(&app, &cookie, &csrf).await;
    assert_eq!(
        send(
            &app,
            write(
                Method::PUT,
                MOODLE,
                &payload(MOODLE).to_string(),
                Some(&other),
                Some(&other_csrf)
            )
        )
        .await
        .2["error"]["code"],
        "reauth_required"
    );
    clock
        .0
        .fetch_add(auth::REAUTH_SECONDS - 1, Ordering::SeqCst);
    assert_eq!(
        send(
            &app,
            write(
                Method::PUT,
                MOODLE,
                &payload(MOODLE).to_string(),
                Some(&cookie),
                Some(&csrf)
            )
        )
        .await
        .0,
        204
    );
    clock.0.fetch_add(1, Ordering::SeqCst);
    assert_eq!(
        send(
            &app,
            write(Method::DELETE, MOODLE, "{}", Some(&cookie), Some(&csrf))
        )
        .await
        .2["error"]["code"],
        "reauth_required"
    );
    for _ in 0..5 {
        assert_eq!(
            send(
                &app,
                write(
                    Method::POST,
                    REAUTH,
                    r#"{"password":"synthetic-wrong"}"#,
                    Some(&cookie),
                    Some(&csrf)
                )
            )
            .await
            .0,
            403
        );
    }
    let (status, headers, _) = send(
        &app,
        write(
            Method::POST,
            REAUTH,
            &json!({"password":PASSWORD}).to_string(),
            Some(&cookie),
            Some(&csrf),
        ),
    )
    .await;
    assert_eq!(status, 429);
    assert_eq!(headers["retry-after"], "60");
    assert_eq!(
        send(
            &app,
            common::mutation(
                "/api/v1/auth/login",
                &json!({"login":"owner","password":PASSWORD}).to_string(),
                PORT
            )
        )
        .await
        .0,
        429
    );
    clock.0.fetch_add(60, Ordering::SeqCst);
    reauth(&app, &cookie, &csrf).await;
    state.shutdown().await;
}

#[tokio::test]
async fn disabled_and_other_authenticated_internal_user_cannot_read_or_mutate_vault() {
    let (_root, db, state, app, _) = fixture().await;
    let (cookie, csrf) = common::login(&app, PORT).await;
    reauth(&app, &cookie, &csrf).await;
    assert_eq!(
        send(
            &app,
            write(
                Method::PUT,
                MOODLE,
                &payload(MOODLE).to_string(),
                Some(&cookie),
                Some(&csrf)
            )
        )
        .await
        .0,
        204
    );
    let conn = Connection::open(auth::control_path(&db)).unwrap();
    let before: Vec<u8> = conn
        .query_row("SELECT envelope FROM web_credentials", [], |r| r.get(0))
        .unwrap();
    conn.execute("UPDATE accounts SET status='disabled'", [])
        .unwrap();
    for req in [
        get(STATUS, Some(&cookie)),
        write(Method::DELETE, MOODLE, "{}", Some(&cookie), Some(&csrf)),
    ] {
        assert_eq!(send(&app, req).await.0, 403);
    }
    // Trusted adversarial DB seam simulates a verified foreign internal identity.
    // Request selectors can never create this identity or switch the runtime.
    conn.execute_batch("PRAGMA foreign_keys=OFF; UPDATE accounts SET status='enabled',user_id='other-internal-user'; UPDATE sessions SET user_id='other-internal-user'").unwrap();
    for req in [
        get(STATUS, Some(&cookie)),
        write(
            Method::PUT,
            MOODLE,
            &payload(MOODLE).to_string(),
            Some(&cookie),
            Some(&csrf),
        ),
        write(Method::DELETE, MOODLE, "{}", Some(&cookie), Some(&csrf)),
        write(
            Method::POST,
            REAUTH,
            &json!({"password":PASSWORD}).to_string(),
            Some(&cookie),
            Some(&csrf),
        ),
    ] {
        assert_eq!(send(&app, req).await.0, 403);
    }
    assert_eq!(
        conn.query_row("SELECT envelope FROM web_credentials", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        before
    );
    state.shutdown().await;
}

#[tokio::test]
async fn corruption_status_and_replace_fail_with_fixed_error_without_disclosing_inputs() {
    let (_root, db, state, app, _) = fixture().await;
    let (cookie, csrf) = common::login(&app, PORT).await;
    reauth(&app, &cookie, &csrf).await;
    assert_eq!(
        send(
            &app,
            write(
                Method::PUT,
                MOODLE,
                &payload(MOODLE).to_string(),
                Some(&cookie),
                Some(&csrf)
            )
        )
        .await
        .0,
        204
    );
    Connection::open(auth::control_path(&db))
        .unwrap()
        .execute("UPDATE web_credentials SET envelope=x'01'", [])
        .unwrap();
    for req in [
        get(STATUS, Some(&cookie)),
        write(
            Method::PUT,
            MOODLE,
            &payload(MOODLE).to_string(),
            Some(&cookie),
            Some(&csrf),
        ),
        write(Method::DELETE, MOODLE, "{}", Some(&cookie), Some(&csrf)),
    ] {
        let (s, _, body) = send(&app, req).await;
        assert_eq!(s, 503);
        assert_eq!(body, json!({"error":{"code":"vault_unavailable"}}));
    }
    state.shutdown().await;
}

#[tokio::test]
async fn startup_with_missing_or_wrong_key_refuses_existing_records_and_right_key_recovers() {
    let (_root, db, state, app, clock) = fixture().await;
    let (cookie, csrf) = common::login(&app, PORT).await;
    reauth(&app, &cookie, &csrf).await;
    assert_eq!(
        send(
            &app,
            write(
                Method::PUT,
                MOODLE,
                &payload(MOODLE).to_string(),
                Some(&cookie),
                Some(&csrf)
            )
        )
        .await
        .0,
        204
    );
    state.shutdown().await;
    let conn = Connection::open(auth::control_path(&db)).unwrap();
    let before: Vec<u8> = conn
        .query_row("SELECT envelope FROM web_credentials", [], |r| r.get(0))
        .unwrap();
    for provider in [
        assignment_manager_web_server::keys::MemoryKeyProvider::missing(),
        assignment_manager_web_server::keys::MemoryKeyProvider::random().unwrap(),
    ] {
        assert!(
            PocState::open_with_key_provider(&db, QueryClock, clock.clone(), Arc::new(provider))
                .await
                .is_err()
        );
        assert_eq!(
            conn.query_row("SELECT envelope FROM web_credentials", [], |r| r
                .get::<_, Vec<u8>>(0))
                .unwrap(),
            before
        );
    }
    let state = common::open_with_auth_clock(&db, QueryClock, clock)
        .await
        .unwrap();
    let app = router(state.clone(), PORT);
    let (cookie, _) = common::login(&app, PORT).await;
    assert_eq!(
        send(&app, get(STATUS, Some(&cookie))).await.2,
        flags(true, false)
    );
    state.shutdown().await;
}

fn flags(moodle: bool, webclass: bool) -> Value {
    json!({"moodleConfigured":moodle,"webclassConfigured":webclass})
}
