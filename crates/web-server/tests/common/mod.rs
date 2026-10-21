#![allow(dead_code)]
use assignment_manager_web_server::{
    PocState, WebError,
    auth::{COOKIE_NAME, bootstrap_admin},
    poc::PocDatabase,
    router,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
    middleware::{self, Next},
    response::Response,
};
use serde_json::{Value, json};
use std::path::Path;
use tower::ServiceExt;
pub const PASSWORD: &str = "synthetic-web-only-password-3A";
pub fn database(path: &Path) -> Result<PocDatabase, WebError> {
    let db = PocDatabase::prepare(path)?;
    match bootstrap_admin(&db, "owner", PASSWORD.to_owned().into()) {
        Ok(_) | Err(WebError::AccountExists) => {}
        Err(e) => return Err(e),
    }
    Ok(db)
}
pub fn mutation(path: &str, body: &str, port: u16) -> Request<Body> {
    Request::builder()
        .uri(path)
        .method(if path.contains("/completion") {
            "PUT"
        } else {
            "POST"
        })
        .header("host", format!("127.0.0.1:{port}"))
        .header("origin", format!("http://127.0.0.1:{port}"))
        .header("x-assignment-manager", "web-3a")
        .header("content-type", "application/json")
        .header("sec-fetch-site", "same-origin")
        .header("sec-fetch-mode", "same-origin")
        .body(Body::from(body.to_owned()))
        .unwrap()
}
pub async fn login(app: &Router, port: u16) -> (String, String) {
    let response = app
        .clone()
        .oneshot(mutation(
            "/api/v1/auth/login",
            &json!({"login":"owner","password":PASSWORD}).to_string(),
            port,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert!(cookie.starts_with(COOKIE_NAME));
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    (cookie, body["csrfToken"].as_str().unwrap().to_owned())
}
// Existing Web-2 semantics tests use a real HTTP login before adding its proof
// to requests. Auth-specific tests always exercise the unwrapped router.
pub async fn authenticated_router(state: PocState, port: u16) -> Router {
    let app = router(state, port);
    let (cookie, csrf) = login(&app, port).await;
    app.layer(middleware::from_fn(
        move |mut req: Request<Body>, next: Next| {
            let cookie = cookie.clone();
            let csrf = csrf.clone();
            async move {
                req.headers_mut().insert("cookie", cookie.parse().unwrap());
                req.headers_mut()
                    .insert("x-csrf-token", csrf.parse().unwrap());
                next.run(req).await
            }
        },
    ))
}
pub async fn send(app: &Router, req: Request<Body>) -> (u16, axum::http::HeaderMap, Value) {
    let response: Response = app.clone().oneshot(req).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    assert_eq!(headers["cache-control"], "no-store");
    assert!(!headers.contains_key("access-control-allow-origin"));
    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    (
        status,
        headers,
        if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        },
    )
}

pub fn keys(
    db: &PocDatabase,
) -> std::sync::Arc<assignment_manager_web_server::keys::MemoryKeyProvider> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static KEYS: OnceLock<
        Mutex<
            HashMap<
                std::path::PathBuf,
                Arc<assignment_manager_web_server::keys::MemoryKeyProvider>,
            >,
        >,
    > = OnceLock::new();
    let mut keys = KEYS.get_or_init(Default::default).lock().unwrap();
    keys.entry(db.path().to_owned())
        .or_insert_with(|| {
            Arc::new(assignment_manager_web_server::keys::MemoryKeyProvider::random().unwrap())
        })
        .clone()
}
pub async fn open(
    db: &PocDatabase,
    clock: impl assignment_manager_web_server::Clock,
) -> Result<PocState, WebError> {
    open_with_auth_clock(
        db,
        clock,
        assignment_manager_web_server::auth::SystemAuthClock,
    )
    .await
}
pub async fn open_with_auth_clock(
    db: &PocDatabase,
    clock: impl assignment_manager_web_server::Clock,
    auth_clock: impl assignment_manager_web_server::auth::AuthClock,
) -> Result<PocState, WebError> {
    // Existing live state must be allowed to report the process-lock conflict.
    let provider = keys(db);
    assignment_manager_web_server::vault::initialize_with_provider(db, provider.as_ref())?;
    PocState::open_with_key_provider(db, clock, auth_clock, provider).await
}
pub async fn server(
    db: &PocDatabase,
    clock: impl assignment_manager_web_server::Clock,
    port: u16,
) -> Result<assignment_manager_web_server::PocServer, WebError> {
    let provider = keys(db);
    assignment_manager_web_server::vault::initialize_with_provider(db, provider.as_ref())?;
    assignment_manager_web_server::PocServer::start_with_key_provider(db, clock, port, provider)
        .await
}
