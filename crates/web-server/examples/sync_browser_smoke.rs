//! Dedicated, offline synthetic smoke. No ordinary Web DB/Vault is opened.
use assignment_manager_web_server::{
    DEFAULT_PORT, PocServer, SystemClock, WebError, auth,
    keys::MemoryKeyProvider,
    poc::PocDatabase,
    synthetic::{self, MockControl, MockSyncBackend, Outcome, Scenario},
    vault,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), WebError> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let operation = args
        .first()
        .map(String::as_str)
        .ok_or(WebError::InvalidRequest)?;
    let scenario = match args.get(1).map(String::as_str).unwrap_or("both-success") {
        "both-success" | "paced" => Scenario::default(),
        "moodle-failure" => Scenario {
            moodle: Outcome::Failure,
            ..Default::default()
        },
        "webclass-partial" => Scenario {
            webclass: Outcome::PartialFailure,
            ..Default::default()
        },
        "both-failure" => Scenario {
            moodle: Outcome::Failure,
            webclass: Outcome::Failure,
            ..Default::default()
        },
        "auth-expired" => Scenario {
            webclass: Outcome::AuthExpiredOnce,
            ..Default::default()
        },
        _ => return Err(WebError::InvalidRequest),
    };
    if args.len() > 2
        || !matches!(
            operation,
            "bootstrap" | "init-vault-key" | "serve" | "ephemeral"
        )
        || (args.len() == 2 && !matches!(operation, "serve" | "ephemeral"))
    {
        return Err(WebError::InvalidRequest);
    }
    let paced = args.get(1).is_some_and(|value| value == "paced");
    let control = MockControl::default();
    *control.scenario.lock().unwrap() = scenario;
    if operation == "ephemeral" {
        // Disposable portable UI test only. Public synthetic password, random
        // memory key, new tempdir every launch; never a normal-server fallback.
        let root = tempfile::tempdir().map_err(|_| WebError::Internal)?;
        let root_path = root.path().canonicalize().map_err(|_| WebError::Internal)?;
        let db = PocDatabase::prepare(&root_path.join("web-4a-ephemeral"))?;
        auth::bootstrap_admin(
            &db,
            "synthetic-smoke",
            "synthetic-web-only-password-4a".to_owned().into(),
        )?;
        let provider = Arc::new(MemoryKeyProvider::random()?);
        vault::initialize_with_provider(&db, provider.as_ref())?;
        let state = synthetic::open(
            &db,
            SystemClock,
            auth::SystemAuthClock,
            provider,
            SmokeBackend {
                inner: MockSyncBackend::new(control),
                paced,
            },
        )
        .await?;
        run(state).await?;
        return Ok(());
    }
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/web-4a-browser-smoke");
    let db = PocDatabase::prepare(&directory)?;
    match operation {
        "bootstrap" => {
            auth::bootstrap_admin(
                &db,
                "synthetic-smoke",
                rpassword::prompt_password("Synthetic Web password (hidden): ")
                    .map_err(|_| WebError::InvalidRequest)?
                    .into(),
            )?;
        }
        "init-vault-key" => vault::initialize_local_key(&db)?,
        "serve" => {
            #[cfg(windows)]
            {
                let provider =
                    Arc::new(assignment_manager_web_server::keys::WindowsKeyProvider::new(&db)?);
                let state = synthetic::open(
                    &db,
                    SystemClock,
                    auth::SystemAuthClock,
                    provider,
                    SmokeBackend {
                        inner: MockSyncBackend::new(control),
                        paced,
                    },
                )
                .await?;
                run(state).await?;
            }
            #[cfg(not(windows))]
            {
                return Err(WebError::VaultUnavailable);
            }
        }
        _ => return Err(WebError::InvalidRequest),
    }
    Ok(())
}
async fn run(state: assignment_manager_web_server::PocState) -> Result<(), WebError> {
    let server = PocServer::from_synthetic_state(state, DEFAULT_PORT).await?;
    println!("Offline synthetic smoke: http://127.0.0.1:3000/ (Ctrl+C drains jobs)");
    server
        .serve(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}

// Manual-only pacing makes running UI and Settings 409 reproducible. Automated
// race tests use channels/Notify instead of wall-clock sleeps.
struct SmokeBackend {
    inner: MockSyncBackend,
    paced: bool,
}
impl SmokeBackend {
    fn pause(&self) {
        if self.paced {
            std::thread::sleep(std::time::Duration::from_secs(10));
        }
    }
}
impl assignment_manager_application::SyncBackend for SmokeBackend {
    type Session = usize;
    fn moodle(
        &mut self,
        url: assignment_manager_application::credentials::SecretText,
        store: &mut assignment_manager::persistence::SqliteStore,
    ) -> Result<
        assignment_manager_application::SyncCounts,
        assignment_manager_application::SyncFailure,
    > {
        self.pause();
        self.inner.moodle(url, store)
    }
    fn login(
        &mut self,
        credentials: assignment_manager_application::credentials::WebClassCredentials,
    ) -> Result<usize, assignment_manager_application::SyncFailure> {
        self.inner.login(credentials)
    }
    fn webclass(
        &mut self,
        session: &mut usize,
        provider: &mut impl assignment_manager::sync::WebClassCredentialProvider,
        store: &mut assignment_manager::persistence::SqliteStore,
    ) -> Result<
        assignment_manager_application::SyncCounts,
        assignment_manager_application::SyncFailure,
    > {
        self.pause();
        self.inner.webclass(session, provider, store)
    }
}
