//! Single-flight admission. No pending sync-run queue and no cancellation on reply loss.
use crate::{
    error::ApplicationError,
    lifecycle::WorkerGuard,
    local_worker::open_store,
    sync_service::{SyncBackend, SyncReport, SyncSecrets, SyncService},
};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{self, SyncSender},
    },
    thread,
};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncStatus {
    pub running: bool,
    pub last_report: Option<SyncReport>,
}

struct FlightGuard {
    status: Arc<Mutex<SyncStatus>>,
    active: bool,
}
impl FlightGuard {
    fn complete(mut self, report: &SyncReport) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        status.last_report = Some(report.clone());
        status.running = false;
        self.active = false;
    }
}
impl Drop for FlightGuard {
    fn drop(&mut self) {
        if self.active {
            self.status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .running = false;
        }
    }
}

struct Request {
    reply: oneshot::Sender<SyncReport>,
    flight: FlightGuard,
}
pub(crate) struct SyncWorker {
    requests: SyncSender<Request>,
    status: Arc<Mutex<SyncStatus>>,
}

impl SyncWorker {
    pub fn start<B: SyncBackend + Send + 'static, C: SyncSecrets + Send + 'static>(
        path: Result<PathBuf, ApplicationError>,
        credentials: C,
        backend: B,
        lifetime: WorkerGuard,
    ) -> Result<Self, ApplicationError> {
        let (requests, receiver) = mpsc::sync_channel::<Request>(1);
        let status = Arc::new(Mutex::new(SyncStatus::default()));
        thread::Builder::new()
            .name("assignment-sync".into())
            .spawn(move || {
                let _lifetime = lifetime;
                let mut service = SyncService::new(backend, credentials, path.and_then(open_store));
                for request in receiver {
                    let report = service.run();
                    request.flight.complete(&report);
                    let _ = request.reply.send(report);
                }
                // Store, session and the final settings sender drop before the guard.
            })
            .map_err(|_| ApplicationError::Unavailable)?;
        Ok(Self { requests, status })
    }
    pub fn enqueue(&self) -> Result<oneshot::Receiver<SyncReport>, ApplicationError> {
        let (reply, response) = oneshot::channel();
        let mut status = self
            .status
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        if status.running {
            let _ = reply.send(SyncReport::AlreadyRunning);
            return Ok(response);
        }
        status.running = true;
        drop(status);
        self.requests
            .try_send(Request {
                reply,
                flight: FlightGuard {
                    status: Arc::clone(&self.status),
                    active: true,
                },
            })
            .map_err(|_| ApplicationError::Unavailable)?;
        Ok(response)
    }
    pub fn status(&self) -> Result<SyncStatus, ApplicationError> {
        self.status
            .lock()
            .map(|status| status.clone())
            .map_err(|_| ApplicationError::Unavailable)
    }
}
