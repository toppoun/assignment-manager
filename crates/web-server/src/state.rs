use crate::{
    WebError,
    dto::AssignmentListDto,
    poc::{DisabledNetworkBackend, MemorySettings, NoCredentials, PocDatabase},
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
        let (auth, owner) = crate::auth::AuthService::open(database, auth_clock).await?;
        let application = ApplicationHandle::open_with_backend(
            Ok(database.path().to_owned()),
            || Ok((MemorySettings, NoCredentials)),
            DisabledNetworkBackend,
        )
        .map_err(WebError::from)?;
        let state = Self(Arc::new(Inner {
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
    }
    /// Called after the HTTP server has drained its accepted requests.
    pub async fn shutdown(&self) {
        self.begin_shutdown();
        self.0.tenant.application.shutdown().await;
        self.0.auth.close().await;
    }
    pub(crate) fn auth(&self) -> &crate::auth::AuthService {
        &self.0.auth
    }
}
