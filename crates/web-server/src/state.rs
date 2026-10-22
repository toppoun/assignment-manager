use crate::{
    WebError,
    dto::AssignmentListDto,
    poc::{DisabledNetworkBackend, MemorySettings, PocDatabase},
};
use assignment_manager_application::{
    ApplicationHandle,
    query::{AssignmentQuery, QueryClock},
};
use chrono::{DateTime, FixedOffset, Utc};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// A server-side test seam, never populated from HTTP clock parameters.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> DateTime<Utc>;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

struct Inner {
    tenant: TenantRuntime,
    auth: crate::auth::AuthService,
    clock: Arc<dyn Clock>,
    accepting: AtomicBool,
    sync: crate::web_sync::WebSyncCoordinator,
}
/// Immutable owner-to-runtime binding. One account, one three-worker runtime.
struct TenantRuntime {
    owner: crate::auth::UserId,
    application: ApplicationHandle,
}
impl TenantRuntime {
    fn for_user(&self, user: &crate::auth::UserId) -> Result<&ApplicationHandle, WebError> {
        if user == &self.owner {
            Ok(&self.application)
        } else {
            Err(WebError::Forbidden)
        }
    }
}
/// Clones share exactly one application runtime. No connection or worker escapes.
#[derive(Clone)]
pub struct PocState(Arc<Inner>);

impl PocState {
    /// Await a real SQLite query before returning a ready state.
    pub async fn open(database: &PocDatabase, clock: impl Clock) -> Result<Self, WebError> {
        Self::open_with_auth_clock(database, clock, crate::auth::SystemAuthClock).await
    }
    /// Trusted clock seam for timeout integration tests, never HTTP input.
    pub async fn open_with_auth_clock(
        database: &PocDatabase,
        clock: impl Clock,
        auth_clock: impl crate::auth::AuthClock,
    ) -> Result<Self, WebError> {
        let provider = crate::keys::local_provider(database)?;
        Self::open_with_key_provider(database, clock, auth_clock, provider).await
    }
    /// Synthetic tests inject an isolated memory provider; never touches OS entries.
    pub async fn open_with_key_provider(
        database: &PocDatabase,
        clock: impl Clock,
        auth_clock: impl crate::auth::AuthClock,
        provider: Arc<dyn crate::keys::KeyProvider>,
    ) -> Result<Self, WebError> {
        Self::open_with_backend(
            database,
            clock,
            auth_clock,
            provider,
            DisabledNetworkBackend,
        )
        .await
    }
    pub(crate) async fn open_with_backend<
        B: assignment_manager_application::SyncBackend + Send + 'static,
    >(
        database: &PocDatabase,
        clock: impl Clock,
        auth_clock: impl crate::auth::AuthClock,
        provider: Arc<dyn crate::keys::KeyProvider>,
        backend: B,
    ) -> Result<Self, WebError> {
        let (auth, owner) = crate::auth::AuthService::open(database, auth_clock).await?;
        let control = auth.control();
        let vault_owner = owner.clone();
        let application = match ApplicationHandle::open_with_backend(
            Ok(database.path().to_owned()),
            move || {
                Ok((
                    MemorySettings,
                    crate::vault::WebVault::new(control, vault_owner, provider)?,
                ))
            },
            backend,
        ) {
            Ok(application) => application,
            Err(error) => {
                auth.close().await;
                return Err(error.into());
            }
        };
        let sync = crate::web_sync::WebSyncCoordinator::new(owner.clone(), application.clone());
        let state = Self(Arc::new(Inner {
            sync,
            tenant: TenantRuntime {
                owner: owner.clone(),
                application,
            },
            auth,
            clock: Arc::new(clock),
            accepting: AtomicBool::new(true),
        }));
        if let Err(error) = state.query(&owner, AssignmentQuery::default()).await {
            state.shutdown().await;
            return Err(error);
        }
        if let Err(error) = state.settings_status(&owner).await {
            state.shutdown().await;
            return Err(error);
        }
        Ok(state)
    }
    pub(crate) async fn query(
        &self,
        user: &crate::auth::UserId,
        query: AssignmentQuery,
    ) -> Result<AssignmentListDto, WebError> {
        let now = self.0.clock.now(); // One sample supplies both query and evaluatedAt.
        let jst = FixedOffset::east_opt(9 * 3600).ok_or(WebError::Internal)?;
        let clock = QueryClock::from_local(now.with_timezone(&jst)).map_err(WebError::from)?;
        let response = self
            .0
            .tenant
            .for_user(user)?
            .query_assignments_with_clock(query, clock)
            .await
            .map_err(WebError::from)?;
        Ok(AssignmentListDto::new(response, now))
    }
    pub(crate) fn check_owner(&self, user: &crate::auth::UserId) -> Result<(), WebError> {
        self.0.tenant.for_user(user).map(|_| ())
    }
    pub(crate) async fn settings_status(
        &self,
        user: &crate::auth::UserId,
    ) -> Result<crate::dto::SettingsStatusDto, WebError> {
        let status = self.0.tenant.for_user(user)?.get_settings_status().await?;
        Ok(crate::dto::SettingsStatusDto {
            moodle_configured: status.moodle_configured,
            webclass_configured: status.webclass_configured,
        })
    }
    pub(crate) async fn save_moodle(
        &self,
        user: &crate::auth::UserId,
        secret: assignment_manager_application::credentials::SecretText,
    ) -> Result<(), WebError> {
        self.0
            .sync
            .mutate(user, move |app| async move {
                app.save_moodle_config(secret).await
            })
            .await
    }
    pub(crate) async fn remove_moodle(&self, user: &crate::auth::UserId) -> Result<(), WebError> {
        self.0
            .sync
            .mutate(user, |app| async move { app.remove_moodle_config().await })
            .await
    }
    pub(crate) async fn save_webclass(
        &self,
        user: &crate::auth::UserId,
        credentials: assignment_manager_application::credentials::WebClassCredentials,
    ) -> Result<(), WebError> {
        self.0
            .sync
            .mutate(user, move |app| async move {
                app.save_webclass_credentials(credentials).await
            })
            .await
    }
    pub(crate) async fn remove_webclass(&self, user: &crate::auth::UserId) -> Result<(), WebError> {
        self.0
            .sync
            .mutate(user, |app| async move {
                app.remove_webclass_credentials().await
            })
            .await
    }
    pub(crate) fn sync(&self) -> &crate::web_sync::WebSyncCoordinator {
        &self.0.sync
    }
    pub(crate) fn accepting(&self) -> bool {
        self.0.accepting.load(Ordering::Acquire)
    }
    pub(crate) async fn set_completion(
        &self,
        user: &crate::auth::UserId,
        id: String,
        desired: assignment_manager::domain::CompletionState,
    ) -> Result<crate::dto::CompletionResponse, WebError> {
        let completion = self
            .0
            .tenant
            .for_user(user)?
            .set_completion(id.clone(), desired)
            .await?;
        Ok(crate::dto::CompletionResponse {
            id,
            completion: completion.into(),
        })
    }
    /// Close HTTP admission, while requests already admitted may still finish.
    pub fn begin_shutdown(&self) {
        self.0.accepting.store(false, Ordering::Release);
        self.0.sync.close();
    }
    /// Called after the HTTP server has drained its accepted requests.
    pub async fn shutdown(&self) {
        self.begin_shutdown();
        self.0.sync.drain().await;
        self.0.tenant.application.shutdown().await;
        self.0.auth.close().await;
    }
    pub(crate) fn auth(&self) -> &crate::auth::AuthService {
        &self.0.auth
    }
}
