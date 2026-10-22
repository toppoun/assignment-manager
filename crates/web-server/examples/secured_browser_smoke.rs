//! Ephemeral secured HTTPS Browser smoke. Synthetic credentials only.
use assignment_manager_web_server::{
    DEFAULT_PORT, PocServer, WebError, auth,
    keys::MemoryKeyProvider,
    poc::PocDatabase,
    secured_synthetic::{self, Scenario},
    vault,
};
use std::sync::Arc;
#[tokio::main]
async fn main() -> Result<(), WebError> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let scenario = match args.as_slice() {
        [] => Scenario::BothSuccess,
        [s] if s == "both-success" => Scenario::BothSuccess,
        [s] if s == "moodle-failure" => Scenario::MoodleFailure,
        [s] if s == "webclass-failure" => Scenario::WebClassFailure,
        [s] if s == "both-failure" => Scenario::BothFailure,
        [s] if s == "auth-expired" => Scenario::AuthExpiredOnce,
        [s] if s == "body-limit" => Scenario::BodyLimit,
        [s] if s == "paced" => Scenario::Paced,
        _ => return Err(WebError::InvalidRequest),
    };
    let root = tempfile::tempdir().map_err(|_| WebError::Internal)?;
    let path = root.path().canonicalize().map_err(|_| WebError::Internal)?;
    let db = PocDatabase::prepare(&path.join("web-4b3-ephemeral"))?;
    auth::bootstrap_admin(
        &db,
        "synthetic-smoke",
        "synthetic-web-only-password-4b3".to_owned().into(),
    )?;
    let provider = Arc::new(MemoryKeyProvider::random()?);
    vault::initialize_with_provider(&db, provider.as_ref())?;
    let (state, university) = secured_synthetic::open(&db, provider, scenario).await?;
    let server = PocServer::from_synthetic_state(state, DEFAULT_PORT).await?;
    println!("Offline secured synthetic smoke: http://127.0.0.1:3000/ (Ctrl+C drains jobs)");
    let result = server
        .serve(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    drop(university);
    result
}
