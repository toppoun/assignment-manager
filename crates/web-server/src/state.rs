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
    application: ApplicationHandle,
    clock: Arc<dyn Clock>,
    accepting: AtomicBool,
}
/// Clones share exactly one application runtime. No connection or worker escapes.
#[derive(Clone)]
pub struct PocState(Arc<Inner>);

impl PocState {
    /// Await a real SQLite query before returning a ready state.
    pub async fn open(database: &PocDatabase, clock: impl Clock) -> Result<Self, WebError> {
        let application = ApplicationHandle::open_with_backend(
            Ok(database.path().to_owned()),
            || Ok((MemorySettings, NoCredentials)),
            DisabledNetworkBackend,
        )
        .map_err(WebError::from)?;
        let state = Self(Arc::new(Inner {
            application,
            clock: Arc::new(clock),
            accepting: AtomicBool::new(true),
        }));
        if let Err(error) = state.query(AssignmentQuery::default()).await {
            state.shutdown().await;
            return Err(error);
        }
        Ok(state)
    }
    pub(crate) async fn query(
        &self,
        query: AssignmentQuery,
    ) -> Result<AssignmentListDto, WebError> {
        let now = self.0.clock.now(); // One sample supplies both query and evaluatedAt.
        let jst = FixedOffset::east_opt(9 * 3600).ok_or(WebError::Internal)?;
        let clock = QueryClock::from_local(now.with_timezone(&jst)).map_err(WebError::from)?;
        let response = self
            .0
            .application
            .query_assignments_with_clock(query, clock)
            .await
            .map_err(WebError::from)?;
        Ok(AssignmentListDto::new(response, now))
    }
    pub(crate) fn accepting(&self) -> bool {
        self.0.accepting.load(Ordering::Acquire)
    }
    /// Close HTTP admission, while requests already admitted may still finish.
    pub fn begin_shutdown(&self) {
        self.0.accepting.store(false, Ordering::Release);
    }
    /// Called after the HTTP server has drained its accepted requests.
    pub async fn shutdown(&self) {
        self.begin_shutdown();
        self.0.application.shutdown().await;
    }
}
