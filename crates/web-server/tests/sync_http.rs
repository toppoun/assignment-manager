mod common;
use assignment_manager_web_server::{PocState, SystemClock, auth, poc::PocDatabase, router};
use axum::{
    Router,
    body::Body,
    http::{Method, Request},
};
use common::send;
use serde_json::{Value, json};
const PORT: u16 = 32441;
fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut request = Request::builder()
        .uri(path)
        .header("host", format!("127.0.0.1:{PORT}"));
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    request.body(Body::empty()).unwrap()
}
fn write(path: &str, body: Value, cookie: Option<&str>, csrf: Option<&str>) -> Request<Body> {
    let mut request = common::mutation(path, &body.to_string(), PORT);
    if path.starts_with("/api/v1/settings/") {
        *request.method_mut() = Method::PUT;
    }
    if let Some(cookie) = cookie {
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
    }
    if let Some(csrf) = csrf {
        request
            .headers_mut()
            .insert("x-csrf-token", csrf.parse().unwrap());
    }
    request
}
fn post(key: &str, cookie: &str, csrf: &str) -> Request<Body> {
    let mut request = write("/api/v1/sync", json!({}), Some(cookie), Some(csrf));
    request
        .headers_mut()
        .insert("idempotency-key", key.parse().unwrap());
    request
}
async fn wait_finished(app: &Router, path: &str, cookie: &str) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (status, _, run) = send(app, get(path, Some(cookie))).await;
            assert_eq!(status, 200);
            if run["state"] != "running" {
                return run;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn sync_auth_boundaries_disabled_backend_and_restart_do_not_fabricate_success() {
    let root = tempfile::tempdir().unwrap();
    let db = common::database(&root.path().join("poc")).unwrap();
    let state = common::open(&db, SystemClock).await.unwrap();
    let app = router(state.clone(), PORT);
    assert_eq!(
        send(&app, write("/api/v1/sync", json!({}), None, None))
            .await
            .0,
        401
    );
    assert_eq!(send(&app, get("/api/v1/sync/status", None)).await.0, 401);
    assert_eq!(
        send(
            &app,
            get(&format!("/api/v1/sync/runs/{}", "a".repeat(64)), None)
        )
        .await
        .0,
        401
    );
    let (cookie, csrf) = common::login(&app, PORT).await;
    let mut bad = post("key", &cookie, "wrong");
    assert_eq!(send(&app, bad).await.0, 403);
    for (header, value, expected) in [
        ("origin", "https://evil.invalid", 403),
        ("host", "evil.invalid", 400),
        ("sec-fetch-site", "same-site", 403),
        ("content-type", "text/plain", 415),
        ("x-assignment-manager", "wrong", 403),
    ] {
        bad = post("key", &cookie, &csrf);
        bad.headers_mut().insert(header, value.parse().unwrap());
        assert_eq!(send(&app, bad).await.0, expected);
    }
    for key in ["", "bad key", "bad/key", &"x".repeat(129)] {
        assert_eq!(send(&app, post(key, &cookie, &csrf)).await.0, 400);
    }
    assert_eq!(
        send(
            &app,
            write("/api/v1/sync", json!({}), Some(&cookie), Some(&csrf))
        )
        .await
        .0,
        400
    );
    let mut duplicate = post("key", &cookie, &csrf);
    duplicate
        .headers_mut()
        .append("idempotency-key", "other".parse().unwrap());
    assert_eq!(send(&app, duplicate).await.0, 400);
    let mut unknown = post("key", &cookie, &csrf);
    *unknown.body_mut() = Body::from("{\"owner\":\"other\"}");
    assert_eq!(send(&app, unknown).await.0, 400);
    let (_, _, empty) = send(&app, get("/api/v1/sync/status", Some(&cookie))).await;
    assert_eq!(
        empty,
        json!({"running":false,"activeRunId":null,"lastRun":null})
    );
    let (status, _, start) = send(&app, post("original", &cookie, &csrf)).await;
    assert_eq!(status, 202);
    assert_eq!(start["runId"].as_str().unwrap().len(), 64);
    let path = start["statusUrl"].as_str().unwrap();
    let run = wait_finished(&app, path, &cookie).await;
    assert_eq!(run["state"], "finished");
    assert_eq!(run["shouldReload"], false);
    assert_eq!(
        run["report"],
        json!({"moodle":{"status":"not_configured"},"webclass":{"status":"not_configured"}})
    );
    assert_eq!(
        send(&app, post("original", &cookie, &csrf)).await.2["runId"],
        start["runId"]
    );
    // A configured ordinary runtime still cannot construct/use an HTTP client.
    assert_eq!(
        send(
            &app,
            write(
                "/api/v1/auth/reauth",
                json!({"password":common::PASSWORD}),
                Some(&cookie),
                Some(&csrf)
            )
        )
        .await
        .0,
        204
    );
    assert_eq!(
        send(
            &app,
            write(
                "/api/v1/settings/moodle",
                json!({"calendarUrl":"https://example.invalid/synthetic-calendar"}),
                Some(&cookie),
                Some(&csrf)
            )
        )
        .await
        .0,
        204
    );
    let (_, _, disabled) = send(&app, post("disabled-network", &cookie, &csrf)).await;
    let disabled = wait_finished(&app, disabled["statusUrl"].as_str().unwrap(), &cookie).await;
    assert_eq!(
        disabled["report"]["moodle"],
        json!({"status":"failed","error":"unexpected"})
    );
    let conn = rusqlite::Connection::open(auth::control_path(&db)).unwrap();
    conn.execute("UPDATE accounts SET status='disabled'", [])
        .unwrap();
    assert_eq!(send(&app, post("disabled", &cookie, &csrf)).await.0, 403);
    assert_eq!(send(&app, get(path, Some(&cookie))).await.0, 403);
    conn.execute("UPDATE accounts SET status='enabled'", [])
        .unwrap();
    assert_eq!(
        send(
            &app,
            write("/api/v1/auth/logout", json!({}), Some(&cookie), Some(&csrf))
        )
        .await
        .0,
        200
    );
    assert_eq!(send(&app, post("logout", &cookie, &csrf)).await.0, 401);
    assert_eq!(send(&app, get(path, Some(&cookie))).await.0, 401);
    state.shutdown().await;
    let reopened = common::open(&db, SystemClock).await.unwrap();
    let app = router(reopened.clone(), PORT);
    assert_eq!(send(&app, get(path, Some(&cookie))).await.0, 401);
    let (new_cookie, new_csrf) = common::login(&app, PORT).await;
    assert_eq!(send(&app, get(path, Some(&new_cookie))).await.0, 404);
    assert_eq!(
        send(&app, get("/api/v1/sync/status", Some(&new_cookie)))
            .await
            .2,
        empty
    );
    assert_ne!(
        send(&app, post("original", &new_cookie, &new_csrf)).await.2["runId"],
        start["runId"]
    );
    reopened.shutdown().await;
}
#[tokio::test]
async fn foreign_owner_cannot_start_or_read_runs() {
    let root = tempfile::tempdir().unwrap();
    let db = common::database(&root.path().join("poc")).unwrap();
    let state = common::open(&db, SystemClock).await.unwrap();
    let app = router(state.clone(), PORT);
    let (cookie, csrf) = common::login(&app, PORT).await;
    let (_, _, start) = send(&app, post("owner", &cookie, &csrf)).await;
    let conn = rusqlite::Connection::open(auth::control_path(&db)).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=OFF; UPDATE accounts SET user_id='foreign'; UPDATE sessions SET user_id='foreign';").unwrap();
    for path in ["/api/v1/sync/status", start["statusUrl"].as_str().unwrap()] {
        assert_eq!(send(&app, get(path, Some(&cookie))).await.0, 403);
    }
    assert_eq!(send(&app, post("foreign", &cookie, &csrf)).await.0, 403);
    state.shutdown().await;
}

#[cfg(feature = "synthetic-sync")]
mod mock {
    use super::*;
    use assignment_manager::{persistence::SqliteStore, sync::WebClassCredentialProvider};
    use assignment_manager_application::{
        SyncBackend, SyncCounts, SyncFailure,
        credentials::{SecretText, WebClassCredentials},
    };
    use assignment_manager_web_server::{
        synthetic::{self, MockControl, MockSyncBackend, Outcome, Scenario},
        vault,
    };
    use std::sync::{Arc, Mutex, mpsc};
    use tokio::sync::Notify;
    async fn fixture(
        backend: impl SyncBackend + Send + 'static,
    ) -> (
        tempfile::TempDir,
        PocDatabase,
        PocState,
        Router,
        String,
        String,
    ) {
        let root = tempfile::tempdir().unwrap();
        let db = common::database(&root.path().join("poc")).unwrap();
        let provider = common::keys(&db);
        vault::initialize_with_provider(&db, provider.as_ref()).unwrap();
        let state = synthetic::open(&db, SystemClock, auth::SystemAuthClock, provider, backend)
            .await
            .unwrap();
        let app = router(state.clone(), PORT);
        let (cookie, csrf) = common::login(&app, PORT).await;
        assert_eq!(
            send(
                &app,
                write(
                    "/api/v1/auth/reauth",
                    json!({"password":common::PASSWORD}),
                    Some(&cookie),
                    Some(&csrf)
                )
            )
            .await
            .0,
            204
        );
        (root, db, state, app, cookie, csrf)
    }
    async fn configure(app: &Router, cookie: &str, csrf: &str, moodle: bool, webclass: bool) {
        for (enabled, path, body) in [
            (
                moodle,
                "/api/v1/settings/moodle",
                json!({"calendarUrl":synthetic::MOODLE_URL}),
            ),
            (
                webclass,
                "/api/v1/settings/webclass",
                json!({"universityId":synthetic::UNIVERSITY_ID,"password":synthetic::PASSWORD}),
            ),
        ] {
            if enabled {
                assert_eq!(
                    send(app, write(path, body, Some(cookie), Some(csrf)))
                        .await
                        .0,
                    204
                );
            }
        }
    }
    async fn run(app: &Router, cookie: &str, csrf: &str, key: &str) -> Value {
        let (status, _, start) = send(app, post(key, cookie, csrf)).await;
        assert_eq!(status, 202);
        wait_finished(app, start["statusUrl"].as_str().unwrap(), cookie).await
    }
    #[tokio::test]
    async fn scenarios_safe_source_results_partial_commit_and_not_configured() {
        for (moodle, webclass, m, w) in [
            (true, false, Outcome::Success, Outcome::Success),
            (false, true, Outcome::Success, Outcome::Success),
            (true, true, Outcome::Success, Outcome::Success),
            (true, true, Outcome::Failure, Outcome::Success),
            (true, true, Outcome::Success, Outcome::PartialFailure),
            (true, true, Outcome::PartialFailure, Outcome::Failure),
            (false, false, Outcome::Success, Outcome::Success),
        ] {
            let control = MockControl::default();
            *control.scenario.lock().unwrap() = Scenario {
                moodle: m,
                webclass: w,
                revision: 0,
            };
            let (_root, _db, state, app, cookie, csrf) =
                fixture(MockSyncBackend::new(control.clone())).await;
            configure(&app, &cookie, &csrf, moodle, webclass).await;
            let report = run(&app, &cookie, &csrf, "scenario").await;
            assert_eq!(report["state"], "finished");
            assert_eq!(report["shouldReload"], moodle || webclass);
            for (source, enabled, outcome) in [("moodle", moodle, m), ("webclass", webclass, w)] {
                let expected = if !enabled {
                    json!({"status":"not_configured"})
                } else if matches!(outcome, Outcome::Success) {
                    json!({"status":"success","counts":{"created":1,"updated":0,"unchanged":0}})
                } else {
                    json!({"status":"failed","error":"network_failed"})
                };
                assert_eq!(report["report"][source], expected);
            }
            let (_, _, list) = send(
                &app,
                get("/api/v1/assignments?completion=all", Some(&cookie)),
            )
            .await;
            let expected = 11
                + usize::from(moodle && !matches!(m, Outcome::Failure))
                + usize::from(webclass && !matches!(w, Outcome::Failure));
            assert_eq!(list["totalCount"], expected);
            for marker in [
                synthetic::MOODLE_URL,
                synthetic::UNIVERSITY_ID,
                synthetic::PASSWORD,
                "Cookie",
                "SAMLResponse",
                "cause",
                "https:",
            ] {
                assert!(!report.to_string().contains(marker));
            }
            state.shutdown().await;
        }
    }
    #[tokio::test]
    async fn stable_import_completion_counts_session_revision_and_auth_expired_retry() {
        let control = MockControl::default();
        let (_root, _db, state, app, cookie, csrf) =
            fixture(MockSyncBackend::new(control.clone())).await;
        configure(&app, &cookie, &csrf, true, true).await;
        let first = run(&app, &cookie, &csrf, "first").await;
        assert_eq!(first["report"]["webclass"]["counts"]["created"], 1);
        let list = send(
            &app,
            get("/api/v1/assignments?completion=all", Some(&cookie)),
        )
        .await
        .2;
        let target = list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["title"].as_str().unwrap().starts_with("Synthetic Moodle"))
            .unwrap()["id"]
            .as_str()
            .unwrap();
        assert_eq!(
            send(
                &app,
                write(
                    &format!("/api/v1/assignments/{target}/completion"),
                    json!({"completion":"completed"}),
                    Some(&cookie),
                    Some(&csrf)
                )
            )
            .await
            .0,
            200
        );
        control.scenario.lock().unwrap().revision = 1;
        let second = run(&app, &cookie, &csrf, "second").await;
        assert_eq!(second["report"]["moodle"]["counts"]["updated"], 1);
        assert_eq!(control.metrics.lock().unwrap().logins, 1);
        let list = send(
            &app,
            get("/api/v1/assignments?completion=all", Some(&cookie)),
        )
        .await
        .2;
        assert_eq!(list["totalCount"], 13);
        assert_eq!(
            list["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["id"] == target)
                .unwrap()["completion"],
            "completed"
        );
        configure(&app, &cookie, &csrf, false, true).await; // revision increments after persisted replacement
        let third = run(&app, &cookie, &csrf, "third").await;
        assert_eq!(third["report"]["moodle"]["counts"]["unchanged"], 1);
        assert_eq!(control.metrics.lock().unwrap().logins, 2);
        control.scenario.lock().unwrap().webclass = Outcome::AuthExpiredOnce;
        let fourth = run(&app, &cookie, &csrf, "fourth").await;
        assert_eq!(fourth["report"]["webclass"]["status"], "success");
        assert_eq!(fourth["report"]["webclass"]["counts"]["unchanged"], 1);
        assert_eq!(control.metrics.lock().unwrap().reauths, 1);
        control.scenario.lock().unwrap().webclass = Outcome::AuthExpiredTwice;
        let fifth = run(&app, &cookie, &csrf, "fifth").await;
        assert_eq!(
            fifth["report"]["webclass"],
            json!({"status":"failed","error":"authentication_failed"})
        );
        assert_eq!(control.metrics.lock().unwrap().reauths, 2); // exactly one per run
        assert_eq!(send(&app, post("sixth", &cookie, &csrf)).await.0, 429);
        assert_eq!(
            send(&app, post("first", &cookie, &csrf)).await.2["runId"],
            first["runId"]
        );
        state.shutdown().await;
    }
    #[tokio::test]
    async fn successful_delete_then_reconfigure_discards_cached_session() {
        let control = MockControl::default();
        let (_root, _db, state, app, cookie, csrf) =
            fixture(MockSyncBackend::new(control.clone())).await;
        configure(&app, &cookie, &csrf, false, true).await;
        run(&app, &cookie, &csrf, "before-delete").await;
        let mut request = write(
            "/api/v1/settings/webclass",
            json!({}),
            Some(&cookie),
            Some(&csrf),
        );
        *request.method_mut() = Method::DELETE;
        assert_eq!(send(&app, request).await.0, 204);
        let missing = run(&app, &cookie, &csrf, "after-delete").await;
        assert_eq!(missing["report"]["webclass"]["status"], "not_configured");
        assert_eq!(missing["shouldReload"], false);
        configure(&app, &cookie, &csrf, false, true).await;
        run(&app, &cookie, &csrf, "after-save").await;
        assert_eq!(control.metrics.lock().unwrap().logins, 2);
        state.shutdown().await;
    }
    #[tokio::test]
    async fn actual_http_disconnect_after_admission_keeps_job_alive() {
        use tokio::io::AsyncWriteExt;
        let control = MockControl::default();
        let entered = Arc::new(Notify::new());
        let (resume, receiver) = mpsc::channel();
        let (_root, _db, state, app, cookie, csrf) = fixture(Paused {
            backend: MockSyncBackend::new(control.clone()),
            entered: entered.clone(),
            resume: Mutex::new(receiver),
        })
        .await;
        configure(&app, &cookie, &csrf, true, false).await;
        let server =
            assignment_manager_web_server::PocServer::from_synthetic_state(state.clone(), 0)
                .await
                .unwrap();
        let address = server.local_addr();
        let (stop, signal) = tokio::sync::oneshot::channel();
        let serving = tokio::spawn(server.serve(async {
            let _ = signal.await;
        }));
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket.write_all(format!("POST /api/v1/sync HTTP/1.1\r\nHost: {address}\r\nOrigin: http://{address}\r\nCookie: {cookie}\r\nX-CSRF-Token: {csrf}\r\nX-Assignment-Manager: web-3a\r\nIdempotency-Key: disconnected\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}").as_bytes()).await.unwrap();
        entered.notified().await;
        drop(socket);
        let status = send(&app, get("/api/v1/sync/status", Some(&cookie)))
            .await
            .2;
        assert_eq!(status["running"], true);
        resume.send(()).unwrap();
        let path = format!(
            "/api/v1/sync/runs/{}",
            status["activeRunId"].as_str().unwrap()
        );
        assert_eq!(
            wait_finished(&app, &path, &cookie).await["report"]["moodle"]["status"],
            "success"
        );
        stop.send(()).unwrap();
        serving.await.unwrap().unwrap();
        assert_eq!(control.metrics.lock().unwrap().moodle_calls, 1);
    }
    struct Paused {
        backend: MockSyncBackend,
        entered: Arc<Notify>,
        resume: Mutex<mpsc::Receiver<()>>,
    }
    impl SyncBackend for Paused {
        type Session = usize;
        fn moodle(
            &mut self,
            url: SecretText,
            store: &mut SqliteStore,
        ) -> Result<SyncCounts, SyncFailure> {
            self.entered.notify_one();
            self.resume
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            self.backend.moodle(url, store)
        }
        fn login(&mut self, c: WebClassCredentials) -> Result<usize, SyncFailure> {
            self.backend.login(c)
        }
        fn webclass(
            &mut self,
            s: &mut usize,
            p: &mut impl WebClassCredentialProvider,
            store: &mut SqliteStore,
        ) -> Result<SyncCounts, SyncFailure> {
            self.backend.webclass(s, p, store)
        }
    }
    #[tokio::test]
    async fn concurrent_tabs_response_drop_settings_conflicts_and_shutdown_drain() {
        use tower::ServiceExt;
        let control = MockControl::default();
        let entered = Arc::new(Notify::new());
        let (resume, receiver) = mpsc::channel();
        let (_root, db, state, app, cookie, csrf) = fixture(Paused {
            backend: MockSyncBackend::new(control.clone()),
            entered: entered.clone(),
            resume: Mutex::new(receiver),
        })
        .await;
        configure(&app, &cookie, &csrf, true, true).await;
        // Discard the complete response without reading it. The server task owns the job.
        drop(
            app.clone()
                .oneshot(post("lost-response", &cookie, &csrf))
                .await
                .unwrap(),
        );
        entered.notified().await;
        let status = send(&app, get("/api/v1/sync/status", Some(&cookie)))
            .await
            .2;
        assert_eq!(status["running"], true);
        let id = status["activeRunId"].as_str().unwrap();
        let path = format!("/api/v1/sync/runs/{id}");
        assert_eq!(
            send(&app, get(&path, Some(&cookie))).await.2,
            json!({"runId":id,"state":"running"})
        );
        let (tab2, csrf2) = common::login(&app, PORT).await;
        let (a, b) = tokio::join!(
            send(&app, post("lost-response", &cookie, &csrf)),
            send(&app, post("other-key", &tab2, &csrf2))
        );
        assert_eq!(a.0, 202);
        assert_eq!(b.0, 202);
        assert_eq!(a.2["runId"], id);
        assert_eq!(b.2["runId"], id);
        for (path, body) in [
            (
                "/api/v1/settings/moodle",
                json!({"calendarUrl":synthetic::MOODLE_URL}),
            ),
            (
                "/api/v1/settings/webclass",
                json!({"universityId":synthetic::UNIVERSITY_ID,"password":synthetic::PASSWORD}),
            ),
        ] {
            for method in [Method::PUT, Method::DELETE] {
                let mut req = write(
                    path,
                    if method == Method::DELETE {
                        json!({})
                    } else {
                        body.clone()
                    },
                    Some(&cookie),
                    Some(&csrf),
                );
                *req.method_mut() = method;
                let rejected = send(&app, req).await;
                assert_eq!(rejected.0, 409);
                assert_eq!(rejected.2["error"]["code"], "sync_running");
            }
        }
        assert_eq!(
            send(
                &app,
                write("/api/v1/auth/logout", json!({}), Some(&cookie), Some(&csrf))
            )
            .await
            .0,
            200
        );
        assert_eq!(send(&app, get(&path, Some(&cookie))).await.0, 401);
        state.begin_shutdown();
        assert_eq!(send(&app, post("shutdown", &tab2, &csrf2)).await.0, 503);
        let draining = tokio::spawn({
            let state = state.clone();
            async move { state.shutdown().await }
        });
        tokio::task::yield_now().await;
        assert!(!draining.is_finished());
        resume.send(()).unwrap();
        draining.await.unwrap();
        assert_eq!(control.metrics.lock().unwrap().moodle_calls, 1);
        assert_eq!(control.metrics.lock().unwrap().logins, 1);
        // Job drain preceded application/control shutdown: both commits survive restart.
        let reopened = common::open(&db, SystemClock).await.unwrap();
        let app = router(reopened.clone(), PORT);
        let (cookie, _) = common::login(&app, PORT).await;
        assert_eq!(
            send(
                &app,
                get("/api/v1/assignments?completion=all", Some(&cookie))
            )
            .await
            .2["totalCount"],
            13
        );
        assert_eq!(send(&app, get(&path, Some(&cookie))).await.0, 404);
        reopened.shutdown().await;
    }
    struct Panics;
    impl SyncBackend for Panics {
        type Session = ();
        fn moodle(
            &mut self,
            _: SecretText,
            _: &mut SqliteStore,
        ) -> Result<SyncCounts, SyncFailure> {
            panic!("synthetic worker panic")
        }
        fn login(&mut self, _: WebClassCredentials) -> Result<(), SyncFailure> {
            unreachable!()
        }
        fn webclass(
            &mut self,
            _: &mut (),
            _: &mut impl WebClassCredentialProvider,
            _: &mut SqliteStore,
        ) -> Result<SyncCounts, SyncFailure> {
            unreachable!()
        }
    }
    #[tokio::test]
    async fn worker_panic_finishes_job_and_releases_settings_admission() {
        let (_root, _db, state, app, cookie, csrf) = fixture(Panics).await;
        configure(&app, &cookie, &csrf, true, false).await;
        let run = run(&app, &cookie, &csrf, "panic").await;
        assert_eq!(run["state"], "failed");
        assert_eq!(run["error"], "job_failed");
        assert_eq!(run["shouldReload"], true);
        assert_eq!(
            send(&app, get("/api/v1/sync/status", Some(&cookie)))
                .await
                .2["running"],
            false
        );
        configure(&app, &cookie, &csrf, true, false).await;
        state.shutdown().await;
    }
}

// The core's isolated CA/DNS seam is intentionally unavailable to this crate.
// This half of the split E2E verifies the genuine secured backend through the
// authenticated API/Vault/coordinator/worker boundary, with denied URLs only.
#[cfg(feature = "synthetic-sync")]
#[tokio::test]
async fn secured_backend_through_api_vault_coordinator_fails_closed_without_network() {
    use assignment_manager::university_network::NetworkPolicy;
    use assignment_manager_application::SecuredCoreSyncBackend;
    use assignment_manager_web_server::{synthetic, vault};
    let root = tempfile::tempdir().unwrap();
    let db = common::database(&root.path().join("secured-poc")).unwrap();
    let provider = common::keys(&db);
    vault::initialize_with_provider(&db, provider.as_ref()).unwrap();
    let state = synthetic::open(
        &db,
        SystemClock,
        auth::SystemAuthClock,
        provider,
        SecuredCoreSyncBackend::new(NetworkPolicy::default()),
    )
    .await
    .unwrap();
    let app = router(state.clone(), PORT);
    let (cookie, csrf) = common::login(&app, PORT).await;
    let (_, _, start) = send(&app, post("secured-empty", &cookie, &csrf)).await;
    let run = wait_finished(&app, start["statusUrl"].as_str().unwrap(), &cookie).await;
    assert_eq!(run["shouldReload"], false);
    assert_eq!(run["report"]["moodle"]["status"], "not_configured");
    assert_eq!(
        send(
            &app,
            write(
                "/api/v1/auth/reauth",
                json!({"password":common::PASSWORD}),
                Some(&cookie),
                Some(&csrf)
            )
        )
        .await
        .0,
        204
    );
    let trap = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    trap.set_nonblocking(true).unwrap();
    let url = format!(
        "https://127.0.0.1:{}/private?token=synthetic-secured-secret",
        trap.local_addr().unwrap().port()
    );
    assert_eq!(
        send(
            &app,
            write(
                "/api/v1/settings/moodle",
                json!({"calendarUrl":url}),
                Some(&cookie),
                Some(&csrf)
            )
        )
        .await
        .0,
        204
    );
    let (_, _, start) = send(&app, post("secured-denied", &cookie, &csrf)).await;
    let run = wait_finished(&app, start["statusUrl"].as_str().unwrap(), &cookie).await;
    assert_eq!(run["state"], "finished");
    assert_eq!(run["shouldReload"], true);
    assert_eq!(
        run["report"]["moodle"],
        json!({"status":"failed","error":"invalid_configuration"})
    );
    assert_eq!(run["report"]["webclass"]["status"], "not_configured");
    assert!(!run.to_string().contains("synthetic-secured-secret"));
    assert!(!run.to_string().contains("127.0.0.1"));
    assert_eq!(
        trap.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let control = rusqlite::Connection::open(auth::control_path(&db)).unwrap();
    let payload: Vec<u8> = control
        .query_row(
            "SELECT envelope FROM web_credentials WHERE kind='moodle'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !payload
            .windows("synthetic-secured-secret".len())
            .any(|w| w == b"synthetic-secured-secret")
    );
    state.shutdown().await;
}
