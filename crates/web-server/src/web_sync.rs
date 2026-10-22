//! Per-owner, process-local job admission. Mutexes protect memory only, never I/O.
use crate::{WebError, auth::UserId};
use assignment_manager_application::{ApplicationHandle, SourceSyncResult, SyncCounts, SyncReport};
use rand_core::{OsRng, RngCore};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Notify;

const RETENTION: Duration = Duration::from_secs(3600);
const RECORD_LIMIT: usize = 128;
const START_LIMIT: usize = 5;

#[derive(Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum SourceDto {
    NotConfigured,
    Success { counts: CountsDto },
    Failed { error: &'static str },
}
#[derive(Clone, Serialize)]
pub(crate) struct CountsDto {
    created: usize,
    updated: usize,
    unchanged: usize,
}
impl From<SourceSyncResult> for SourceDto {
    fn from(value: SourceSyncResult) -> Self {
        match value {
            SourceSyncResult::NotConfigured => Self::NotConfigured,
            SourceSyncResult::Success {
                counts:
                    SyncCounts {
                        created,
                        updated,
                        unchanged,
                    },
            } => Self::Success {
                counts: CountsDto {
                    created,
                    updated,
                    unchanged,
                },
            },
            SourceSyncResult::Failed { error } => Self::Failed {
                error: error.code(),
            },
        }
    }
}
#[derive(Clone, Serialize)]
pub(crate) struct ReportDto {
    moodle: SourceDto,
    webclass: SourceDto,
}
#[derive(Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum RunDto {
    Running {
        #[serde(rename = "runId")]
        run_id: String,
    },
    Finished {
        #[serde(rename = "runId")]
        run_id: String,
        report: ReportDto,
        #[serde(rename = "shouldReload")]
        should_reload: bool,
    },
    // No fabricated per-source results when the worker/report was lost.
    Failed {
        #[serde(rename = "runId")]
        run_id: String,
        error: &'static str,
        #[serde(rename = "shouldReload")]
        should_reload: bool,
    },
}
impl RunDto {
    fn id(&self) -> &str {
        match self {
            Self::Running { run_id }
            | Self::Finished { run_id, .. }
            | Self::Failed { run_id, .. } => run_id,
        }
    }
    fn state(&self) -> &'static str {
        match self {
            Self::Running { .. } => "running",
            Self::Finished { .. } => "finished",
            Self::Failed { .. } => "failed",
        }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AcceptedDto {
    run_id: String,
    state: &'static str,
    status_url: String,
}
impl From<&RunDto> for AcceptedDto {
    fn from(run: &RunDto) -> Self {
        Self {
            run_id: run.id().into(),
            state: run.state(),
            status_url: format!("/api/v1/sync/runs/{}", run.id()),
        }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StatusDto {
    running: bool,
    active_run_id: Option<String>,
    last_run: Option<RunDto>,
}
struct KeyRecord {
    hash: [u8; 32],
    run_id: String,
    at: Instant,
}
struct History {
    run: RunDto,
    at: Instant,
}
#[derive(Default)]
struct Admission {
    closing: bool,
    mutation: bool,
    active: Option<String>,
    history: VecDeque<History>,
    keys: VecDeque<KeyRecord>,
    starts: VecDeque<Instant>,
}
impl Admission {
    fn prune(&mut self, now: Instant) {
        self.keys.retain(|k| {
            now.duration_since(k.at) < RETENTION || self.active.as_ref() == Some(&k.run_id)
        });
        self.history
            .retain(|h| now.duration_since(h.at) < RETENTION);
        self.starts
            .retain(|at| now.duration_since(*at) < Duration::from_secs(60));
    }
    fn run(&self, id: &str) -> Option<RunDto> {
        if self.active.as_deref() == Some(id) {
            return Some(RunDto::Running { run_id: id.into() });
        }
        self.history
            .iter()
            .find(|h| h.run.id() == id)
            .map(|h| h.run.clone())
    }
}
struct Inner {
    owner: UserId,
    application: ApplicationHandle,
    admission: Mutex<Admission>,
    idle: Notify,
}
#[derive(Clone)]
pub(crate) struct WebSyncCoordinator(Arc<Inner>);
impl WebSyncCoordinator {
    pub fn new(owner: UserId, application: ApplicationHandle) -> Self {
        Self(Arc::new(Inner {
            owner,
            application,
            admission: Mutex::new(Admission::default()),
            idle: Notify::new(),
        }))
    }
    fn check(&self, user: &UserId) -> Result<(), WebError> {
        if user == &self.0.owner {
            Ok(())
        } else {
            Err(WebError::Forbidden)
        }
    }
    pub fn start(&self, user: &UserId, key: &str) -> Result<AcceptedDto, WebError> {
        self.check(user)?;
        if key.is_empty()
            || key.len() > 128
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(WebError::InvalidRequest);
        }
        let hash: [u8; 32] = Sha256::digest(key.as_bytes()).into();
        let now = Instant::now();
        let mut a = self.0.admission.lock().unwrap_or_else(|e| e.into_inner());
        if a.closing {
            return Err(WebError::Unavailable);
        }
        a.prune(now);
        if let Some(record) = a.keys.iter().find(|k| k.hash == hash) {
            return a
                .run(&record.run_id)
                .as_ref()
                .map(Into::into)
                .ok_or(WebError::NotFound);
        }
        // Never evict a live dedup record to accept new work.
        if a.keys.len() >= RECORD_LIMIT {
            return Err(WebError::RateLimited);
        }
        if let Some(id) = a.active.clone() {
            a.keys.push_back(KeyRecord {
                hash,
                run_id: id.clone(),
                at: now,
            });
            return Ok((&RunDto::Running { run_id: id }).into());
        }
        if a.mutation {
            return Err(WebError::Busy);
        }
        if a.starts.len() >= START_LIMIT {
            return Err(WebError::RateLimited);
        }
        let mut random = [0u8; 32];
        OsRng
            .try_fill_bytes(&mut random)
            .map_err(|_| WebError::Unavailable)?;
        let id: String = random.iter().map(|b| format!("{b:02x}")).collect();
        a.active = Some(id.clone());
        a.starts.push_back(now);
        a.keys.push_back(KeyRecord {
            hash,
            run_id: id.clone(),
            at: now,
        });
        let accepted = (&RunDto::Running { run_id: id.clone() }).into();
        // No await between reserving admission and transferring its guard to a
        // server-owned task. Client disconnect cannot undo this admission.
        let guard = JobGuard {
            coordinator: self.clone(),
            id,
        };
        let application = self.0.application.clone();
        tokio::spawn(async move {
            let result = tokio::spawn(async move { application.sync_now().await }).await;
            let run = match result {
                Ok(Ok(report @ SyncReport::Completed { .. })) => {
                    let should_reload = report.should_reload();
                    let SyncReport::Completed { moodle, webclass } = report else {
                        unreachable!()
                    };
                    RunDto::Finished {
                        run_id: guard.id.clone(),
                        report: ReportDto {
                            moodle: moodle.into(),
                            webclass: webclass.into(),
                        },
                        should_reload,
                    }
                }
                Ok(Ok(SyncReport::AlreadyRunning)) => RunDto::Failed {
                    run_id: guard.id.clone(),
                    error: "already_running",
                    should_reload: false,
                },
                _ => RunDto::Failed {
                    run_id: guard.id.clone(),
                    error: "job_failed",
                    should_reload: true,
                },
            };
            guard.finish(run);
        });
        Ok(accepted)
    }
    pub fn status(&self, user: &UserId) -> Result<StatusDto, WebError> {
        self.check(user)?;
        let mut a = self.0.admission.lock().unwrap_or_else(|e| e.into_inner());
        a.prune(Instant::now());
        Ok(StatusDto {
            running: a.active.is_some(),
            active_run_id: a.active.clone(),
            last_run: a.history.back().map(|h| h.run.clone()),
        })
    }
    pub fn run(&self, user: &UserId, id: &str) -> Result<RunDto, WebError> {
        self.check(user)?;
        if id.len() != 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(WebError::NotFound);
        }
        let mut a = self.0.admission.lock().unwrap_or_else(|e| e.into_inner());
        a.prune(Instant::now());
        a.run(id).ok_or(WebError::NotFound)
    }
    pub async fn mutate<F, Fut>(&self, user: &UserId, action: F) -> Result<(), WebError>
    where
        F: FnOnce(ApplicationHandle) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), assignment_manager_application::error::ApplicationError>>
            + Send,
    {
        self.check(user)?;
        {
            let mut a = self.0.admission.lock().unwrap_or_else(|e| e.into_inner());
            if a.closing {
                return Err(WebError::Unavailable);
            }
            if a.active.is_some() {
                return Err(WebError::SyncRunning);
            }
            if a.mutation {
                return Err(WebError::Busy);
            }
            a.mutation = true;
        }
        let guard = MutationGuard(self.clone());
        let application = self.0.application.clone();
        tokio::spawn(async move {
            let _guard = guard; // Held until the worker reply, even if HTTP disappears.
            action(application).await.map_err(WebError::from)
        })
        .await
        .map_err(|_| WebError::Unavailable)?
    }
    pub fn close(&self) {
        self.0
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closing = true;
    }
    pub async fn drain(&self) {
        loop {
            let notified = self.0.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let a = self.0.admission.lock().unwrap_or_else(|e| e.into_inner());
                if a.active.is_none() && !a.mutation {
                    return;
                }
            }
            notified.await;
        }
    }
}
struct JobGuard {
    coordinator: WebSyncCoordinator,
    id: String,
}
impl JobGuard {
    fn finish(self, run: RunDto) {
        let mut a = self
            .coordinator
            .0
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        a.active = None;
        a.history.push_back(History {
            run,
            at: Instant::now(),
        });
        // History cannot grow beyond the bounded key records within retention.
        while a.history.len() > RECORD_LIMIT {
            a.history.pop_front();
        }
        drop(a);
        self.coordinator.0.idle.notify_waiters();
    }
}
impl Drop for JobGuard {
    fn drop(&mut self) {
        let mut a = self
            .coordinator
            .0
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if a.active.as_ref() == Some(&self.id) {
            a.active = None;
            a.history.push_back(History {
                run: RunDto::Failed {
                    run_id: self.id.clone(),
                    error: "job_failed",
                    should_reload: true,
                },
                at: Instant::now(),
            });
        }
        drop(a);
        self.coordinator.0.idle.notify_waiters();
    }
}
struct MutationGuard(WebSyncCoordinator);
impl Drop for MutationGuard {
    fn drop(&mut self) {
        self.0
            .0
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mutation = false;
        self.0.0.idle.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn coordinator() -> WebSyncCoordinator {
        let app = ApplicationHandle::open_with_backend(
            Err(assignment_manager_application::error::ApplicationError::DatabaseStartup),
            || Ok((crate::poc::MemorySettings, crate::poc::NoCredentials)),
            crate::poc::DisabledNetworkBackend,
        )
        .unwrap();
        WebSyncCoordinator::new(UserId("owner".into()), app)
    }
    #[tokio::test]
    async fn mutation_reservation_blocks_sync_even_after_request_future_is_dropped_and_drains() {
        let coordinator = coordinator();
        let user = UserId("owner".into());
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (resume, paused) = tokio::sync::oneshot::channel();
        let request = tokio::spawn({
            let coordinator = coordinator.clone();
            let user = user.clone();
            async move {
                coordinator
                    .mutate(&user, |_| async {
                        let _ = entered.send(());
                        let _ = paused.await;
                        Ok(())
                    })
                    .await
            }
        });
        waiting.await.unwrap();
        request.abort();
        let _ = request.await;
        assert!(matches!(
            coordinator.start(&user, "during-save"),
            Err(WebError::Busy)
        ));
        let drain = tokio::spawn({
            let coordinator = coordinator.clone();
            async move { coordinator.drain().await }
        });
        tokio::task::yield_now().await;
        assert!(!drain.is_finished());
        resume.send(()).unwrap();
        drain.await.unwrap();
        assert!(coordinator.start(&user, "after-save").is_ok());
        coordinator.close();
        coordinator.drain().await;
        coordinator.0.application.shutdown().await;
    }
    #[tokio::test]
    async fn defensive_already_running_preserves_shared_no_reload_semantics() {
        let entered = Arc::new(Notify::new());
        let worker_entered = entered.clone();
        let (resume, paused) = std::sync::mpsc::channel();
        let application = ApplicationHandle::open_with_backend(
            Err(assignment_manager_application::error::ApplicationError::DatabaseStartup),
            move || {
                worker_entered.notify_one();
                paused.recv_timeout(Duration::from_secs(10)).unwrap();
                Ok((crate::poc::MemorySettings, crate::poc::NoCredentials))
            },
            crate::poc::DisabledNetworkBackend,
        )
        .unwrap();
        entered.notified().await;
        let other = tokio::spawn({
            let app = application.clone();
            async move { app.sync_now().await }
        });
        while !application.get_sync_status().unwrap().running {
            tokio::task::yield_now().await;
        }
        let user = UserId("owner".into());
        let coordinator = WebSyncCoordinator::new(user.clone(), application.clone());
        let accepted = coordinator.start(&user, "already-running").unwrap();
        coordinator.drain().await;
        assert!(matches!(
            coordinator.run(&user, &accepted.run_id).unwrap(),
            RunDto::Failed {
                error: "already_running",
                should_reload: false,
                ..
            }
        ));
        resume.send(()).unwrap();
        other.await.unwrap().unwrap();
        coordinator.close();
        application.shutdown().await;
    }
    #[tokio::test]
    async fn key_capacity_expiration_owner_binding_and_closing_are_bounded() {
        let coordinator = coordinator();
        let user = UserId("owner".into());
        assert!(matches!(
            coordinator.start(&UserId("foreign".into()), "key"),
            Err(WebError::Forbidden)
        ));
        {
            let mut a = coordinator.0.admission.lock().unwrap();
            for i in 0..RECORD_LIMIT {
                a.keys.push_back(KeyRecord {
                    hash: [0; 32],
                    run_id: format!("run{i}"),
                    at: Instant::now(),
                });
            }
        }
        assert!(matches!(
            coordinator.start(&user, "capacity"),
            Err(WebError::RateLimited)
        ));
        {
            let mut a = coordinator.0.admission.lock().unwrap();
            for key in &mut a.keys {
                key.at = Instant::now() - RETENTION;
            }
            a.prune(Instant::now());
            assert!(a.keys.is_empty());
        }
        let accepted = coordinator.start(&user, "reusable").unwrap();
        coordinator.drain().await;
        assert_eq!(
            coordinator.start(&user, "reusable").unwrap().run_id,
            accepted.run_id
        );
        {
            let mut a = coordinator.0.admission.lock().unwrap();
            for key in &mut a.keys {
                key.at = Instant::now() - RETENTION;
            }
            for history in &mut a.history {
                history.at = Instant::now() - RETENTION;
            }
        }
        assert!(matches!(
            coordinator.run(&user, &accepted.run_id),
            Err(WebError::NotFound)
        ));
        assert_ne!(
            coordinator.start(&user, "reusable").unwrap().run_id,
            accepted.run_id
        );
        coordinator.close();
        assert!(matches!(
            coordinator.start(&user, "closed"),
            Err(WebError::Unavailable)
        ));
        assert!(matches!(
            coordinator.mutate(&user, |_| async { Ok(()) }).await,
            Err(WebError::Unavailable)
        ));
        coordinator.drain().await;
        coordinator.0.application.shutdown().await;
    }
}
