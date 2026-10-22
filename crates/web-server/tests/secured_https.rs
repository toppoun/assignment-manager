#![cfg(feature = "secured-synthetic")]
mod common;
use assignment_manager_web_server::{
    PocServer, WebError,
    keys::MemoryKeyProvider,
    poc::PocDatabase,
    secured_synthetic::{
        self, MOODLE_URL, Scenario, SyntheticUniversity, UNIVERSITY_ID, UNIVERSITY_PASSWORD,
    },
    vault,
};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::oneshot;

struct Http {
    client: Client,
    base: String,
    cookie: String,
    csrf: String,
}
impl Http {
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        key: Option<&str>,
    ) -> reqwest::Response {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header("origin", &self.base)
            .header("x-assignment-manager", "web-3a")
            .header("cookie", &self.cookie)
            .header("x-csrf-token", &self.csrf)
            .header("sec-fetch-site", "same-origin")
            .header("sec-fetch-mode", "same-origin");
        if let Some(body) = body {
            request = request.json(&body);
        }
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.headers()["cache-control"], "no-store");
        response
    }
    async fn login(&mut self) {
        let response = self
            .request(
                Method::POST,
                "/api/v1/auth/login",
                Some(json!({"login":"owner","password":common::PASSWORD})),
                None,
            )
            .await;
        assert_eq!(response.status(), 200);
        self.cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        self.csrf = response.json::<Value>().await.unwrap()["csrfToken"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            self.request(
                Method::POST,
                "/api/v1/auth/reauth",
                Some(json!({"password":common::PASSWORD})),
                None
            )
            .await
            .status(),
            204
        );
    }
    async fn configure(&self, moodle: bool, webclass: bool) {
        if moodle {
            assert_eq!(
                self.request(
                    Method::PUT,
                    "/api/v1/settings/moodle",
                    Some(json!({"calendarUrl":MOODLE_URL})),
                    None
                )
                .await
                .status(),
                204
            );
        }
        if webclass {
            self.save_webclass().await;
        }
    }
    async fn save_webclass(&self) {
        assert_eq!(
            self.request(
                Method::PUT,
                "/api/v1/settings/webclass",
                Some(json!({"universityId":UNIVERSITY_ID,"password":UNIVERSITY_PASSWORD})),
                None
            )
            .await
            .status(),
            204
        );
    }
    async fn sync(&self, key: &str) -> Value {
        let accepted = self
            .request(Method::POST, "/api/v1/sync", Some(json!({})), Some(key))
            .await;
        assert_eq!(accepted.status(), 202);
        let accepted: Value = accepted.json().await.unwrap();
        let path = accepted["statusUrl"].as_str().unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let response = self.request(Method::GET, path, None, None).await;
                assert_eq!(response.status(), 200);
                let run: Value = response.json().await.unwrap();
                if run["state"] != "running" {
                    return run;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }
    async fn list(&self) -> Value {
        let response = self
            .request(
                Method::GET,
                "/api/v1/assignments?completion=all",
                None,
                None,
            )
            .await;
        assert_eq!(response.status(), 200);
        response.json().await.unwrap()
    }
}
struct Running {
    http: Http,
    stop: oneshot::Sender<()>,
    serving: tokio::task::JoinHandle<Result<(), WebError>>,
    university: SyntheticUniversity,
}
async fn start(db: &PocDatabase, provider: Arc<MemoryKeyProvider>, scenario: Scenario) -> Running {
    let (state, university) = secured_synthetic::open(db, provider, scenario)
        .await
        .unwrap();
    let server = PocServer::from_synthetic_state(state, 0).await.unwrap();
    let base = format!("http://{}", server.local_addr());
    let (stop, stopping) = oneshot::channel();
    let serving = tokio::spawn(server.serve(async {
        let _ = stopping.await;
    }));
    let mut http = Http {
        client: Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap(),
        base,
        cookie: String::new(),
        csrf: String::new(),
    };
    assert_eq!(
        http.request(
            Method::POST,
            "/api/v1/sync",
            Some(json!({})),
            Some("unauthenticated")
        )
        .await
        .status(),
        401
    );
    assert_eq!(university.requests(), 0);
    http.login().await;
    Running {
        http,
        stop,
        serving,
        university,
    }
}
async fn stop(run: Running) {
    run.stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), run.serving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(run.university);
}
fn database() -> (tempfile::TempDir, PocDatabase, Arc<MemoryKeyProvider>) {
    let root = tempfile::tempdir().unwrap();
    let db = common::database(&root.path().join("secured-https")).unwrap();
    let provider = Arc::new(MemoryKeyProvider::random().unwrap());
    vault::initialize_with_provider(&db, provider.as_ref()).unwrap();
    (root, db, provider)
}
#[tokio::test]
async fn real_http_auth_vault_secured_tls_parser_sqlite_source_matrix() {
    for (scenario, moodle, webclass, m_status, w_status) in [
        (
            Scenario::BothSuccess,
            false,
            false,
            "not_configured",
            "not_configured",
        ),
        (
            Scenario::BothSuccess,
            true,
            false,
            "success",
            "not_configured",
        ),
        (
            Scenario::BothSuccess,
            false,
            true,
            "not_configured",
            "success",
        ),
        (Scenario::BothSuccess, true, true, "success", "success"),
        (Scenario::MoodleFailure, true, true, "failed", "success"),
        (Scenario::WebClassFailure, true, true, "success", "failed"),
        (Scenario::BothFailure, true, true, "failed", "failed"),
        (Scenario::BodyLimit, true, false, "failed", "not_configured"),
    ] {
        let (_root, db, provider) = database();
        let run = start(&db, provider, scenario).await;
        run.http.configure(moodle, webclass).await;
        let before = run.http.list().await["totalCount"].as_u64().unwrap();
        let result = run.http.sync("matrix").await;
        assert_eq!(result["state"], "finished");
        assert_eq!(result["report"]["moodle"]["status"], m_status);
        assert_eq!(result["report"]["webclass"]["status"], w_status);
        assert_eq!(result["shouldReload"], moodle || webclass);
        let created = u64::from(m_status == "success") + 9 * u64::from(w_status == "success");
        assert_eq!(run.http.list().await["totalCount"], before + created);
        let safe = result.to_string();
        for secret in [
            MOODLE_URL,
            UNIVERSITY_ID,
            UNIVERSITY_PASSWORD,
            "synthetic-cookie",
            "synthetic-secret",
            "synthetic-parent",
            "synthetic-saml-response",
        ] {
            assert!(!safe.contains(secret));
        }
        if !moodle && !webclass {
            assert_eq!(run.university.requests(), 0);
        }
        stop(run).await;
    }
}
#[tokio::test]
async fn https_cached_session_revision_reimport_completion_and_restart_with_memory_key() {
    let (_root, db, provider) = database();
    let run = start(&db, provider.clone(), Scenario::BothSuccess).await;
    run.http.configure(true, true).await;
    let first = run.http.sync("first").await;
    let list = run.http.list().await;
    let id = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["title"].as_str().unwrap().contains("演習課題01"))
        .unwrap()["id"]
        .as_str()
        .unwrap();
    assert_eq!(
        run.http
            .request(
                Method::PUT,
                &format!("/api/v1/assignments/{id}/completion"),
                Some(json!({"completion":"completed"})),
                None
            )
            .await
            .status(),
        200
    );
    let second = run.http.sync("second").await;
    assert_eq!(run.university.logins(), 1); // real HTTPS API reuse, no repeated SSO.
    assert_eq!(second["report"]["moodle"]["counts"]["created"], 0);
    assert_eq!(second["report"]["webclass"]["counts"]["created"], 0);
    run.http.save_webclass().await; // persisted revision changes even for same synthetic pair.
    let third = run.http.sync("revision").await;
    assert_eq!(third["report"]["webclass"]["status"], "success");
    assert_eq!(run.university.logins(), 2);
    let after = run.http.list().await;
    assert_eq!(after["totalCount"], list["totalCount"]);
    assert_eq!(
        after["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == id)
            .unwrap()["completion"],
        "completed"
    );
    let old_cookie = run.http.cookie.clone();
    stop(run).await;
    // A fresh memory key must not overwrite an existing Vault binding.
    assert!(
        secured_synthetic::open(
            &db,
            Arc::new(MemoryKeyProvider::random().unwrap()),
            Scenario::BothSuccess
        )
        .await
        .is_err()
    );
    let run = start(&db, provider, Scenario::BothSuccess).await;
    assert_eq!(
        run.http
            .client
            .get(format!("{}/api/v1/session", run.http.base))
            .header("cookie", old_cookie)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        run.http
            .request(
                Method::GET,
                &format!("/api/v1/sync/runs/{}", first["runId"].as_str().unwrap()),
                None,
                None
            )
            .await
            .status(),
        404
    );
    assert_eq!(run.http.list().await["totalCount"], after["totalCount"]);
    let restarted = run.http.sync("restart").await;
    assert_eq!(restarted["report"]["moodle"]["counts"]["created"], 0);
    assert_eq!(restarted["report"]["webclass"]["counts"]["created"], 0);
    assert_eq!(
        run.http.list().await["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == id)
            .unwrap()["completion"],
        "completed"
    );
    stop(run).await;
}
#[tokio::test]
async fn https_auth_expiry_reauth_once_and_second_expiry_stops() {
    for (scenario, expected) in [
        (Scenario::AuthExpiredOnce, "success"),
        (Scenario::AuthExpiredTwice, "failed"),
    ] {
        let (_root, db, provider) = database();
        let run = start(&db, provider, scenario).await;
        run.http.configure(false, true).await;
        let result = run.http.sync("reauth").await;
        assert_eq!(result["report"]["webclass"]["status"], expected);
        assert_eq!(run.university.logins(), 2);
        stop(run).await;
    }
}
#[tokio::test]
async fn fixed_fixture_policy_cannot_expand_to_real_hosts_or_send_other_credentials() {
    let (_root, db, provider) = database();
    let run = start(&db, provider, Scenario::BothSuccess).await;
    let policy = run
        .university
        .policy()
        .with_moodle_calendar("unapproved.example.invalid", "/calendar/export_execute.php")
        .unwrap();
    assert!(
        policy
            .validate_moodle_url("https://unapproved.example.invalid/calendar/export_execute.php")
            .is_err()
    );
    assert!(
        policy
            .validate_moodle_url("https://127.0.0.1/calendar/export_execute.php")
            .is_err()
    );
    assert!(
        policy
            .validate_moodle_url("https://moodle.synthetic.invalid/wrong?token=synthetic-secret")
            .is_err()
    );
    assert_eq!(run.http.request(Method::PUT, "/api/v1/settings/moodle", Some(json!({"calendarUrl":"https://unapproved.example.invalid/calendar/export_execute.php"})), None).await.status(), 204);
    assert_eq!(run.http.request(Method::PUT, "/api/v1/settings/webclass", Some(json!({"universityId":"other-synthetic-id","password":"other-synthetic-password"})), None).await.status(), 204);
    let result = run.http.sync("denied").await;
    assert_eq!(result["report"]["moodle"]["error"], "invalid_configuration");
    assert_eq!(result["report"]["webclass"]["status"], "failed");
    assert_eq!(run.university.requests(), 0);
    stop(run).await;
}

#[cfg(not(windows))]
#[tokio::test]
async fn normal_mac_startup_has_no_memory_or_native_keychain_fallback() {
    let (_root, db, _provider) = database();
    assert!(matches!(
        assignment_manager_web_server::PocState::open(
            &db,
            assignment_manager_web_server::SystemClock
        )
        .await,
        Err(WebError::VaultUnavailable)
    ));
    assert!(matches!(
        vault::initialize_local_key(&db),
        Err(WebError::VaultUnavailable)
    ));
}
