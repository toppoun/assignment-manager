use crate::{
    credentials::{CredentialStore, SecretText, WebClassCredentials},
    error::ApplicationError,
    lifecycle::WorkersLifetime,
    local_worker::{LocalWorker, Response},
    models::AssignmentListResponse,
    query::AssignmentQuery,
    settings::{SettingsStatus, SettingsStore},
    settings_worker::SettingsWorker,
    sync_service::{CoreSyncBackend, SyncBackend, SyncReport},
    sync_worker::{SyncStatus, SyncWorker},
};
use assignment_manager::domain::CompletionState;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

struct Workers {
    local: LocalWorker,
    sync: SyncWorker,
    settings: SettingsWorker,
}
struct Runtime {
    workers: Mutex<Option<Workers>>,
    lifetime: Arc<WorkersLifetime>,
    startup_checked: AtomicBool,
}
impl Runtime {
    fn close(&self) -> bool {
        let workers = self
            .workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let first = workers.is_some();
        // No admission can race after the take. Accepted requests drain normally.
        drop(workers);
        self.lifetime.close();
        first
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.close();
    }
}

/// Thread-safe use-case handle shared across all application views/windows.
///
/// `open` starts workers; database/adapter initialization occurs on those workers.
/// Initialization errors are retained as safe errors, preserving offline/local
/// operation even if the settings/credential adapter is unavailable.
///
/// `begin_shutdown` rejects new requests immediately, without joining threads.
/// `shutdown().await` observes cleanup after accepted work and adapters have dropped.
/// Dropping the final handle also initiates cleanup without blocking the caller.
#[derive(Clone)]
pub struct ApplicationHandle {
    runtime: Arc<Runtime>,
}

impl ApplicationHandle {
    pub fn open<S: SettingsStore + 'static, C: CredentialStore + 'static>(
        database: Result<PathBuf, ApplicationError>,
        initialize_settings: impl FnOnce() -> Result<(S, C), ApplicationError> + Send + 'static,
    ) -> Result<Self, ApplicationError> {
        Self::open_with_backend(database, initialize_settings, CoreSyncBackend)
    }

    /// Transport substitution for synthetic/offline tests; not a frontend API.
    pub fn open_with_backend<
        S: SettingsStore + 'static,
        C: CredentialStore + 'static,
        B: SyncBackend + Send + 'static,
    >(
        database: Result<PathBuf, ApplicationError>,
        initialize_settings: impl FnOnce() -> Result<(S, C), ApplicationError> + Send + 'static,
        backend: B,
    ) -> Result<Self, ApplicationError> {
        let lifetime = WorkersLifetime::new();
        let start = || {
            let local = LocalWorker::start(database.clone(), lifetime.guard())?;
            let settings = SettingsWorker::start(initialize_settings, lifetime.guard())?;
            let sync = SyncWorker::start(database, settings.clone(), backend, lifetime.guard())?;
            Ok(Workers {
                local,
                sync,
                settings,
            })
        };
        let workers = match start() {
            Ok(workers) => workers,
            Err(error) => {
                lifetime.close();
                return Err(error);
            }
        };
        Ok(Self {
            runtime: Arc::new(Runtime {
                workers: Mutex::new(Some(workers)),
                lifetime,
                startup_checked: AtomicBool::new(false),
            }),
        })
    }

    // Hold admission only while enqueueing, never while awaiting or doing I/O.
    fn admit<T>(
        &self,
        action: impl FnOnce(&Workers) -> Result<T, ApplicationError>,
    ) -> Result<T, ApplicationError> {
        let workers = self
            .runtime
            .workers
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        action(workers.as_ref().ok_or(ApplicationError::Unavailable)?)
    }
    pub async fn query_assignments(
        &self,
        query: AssignmentQuery,
    ) -> Result<AssignmentListResponse, ApplicationError> {
        receive(self.admit(|w| w.local.list(query))?).await
    }
    /// Returns the committed state. A later query failure does not undo this operation.
    pub async fn toggle_completion(&self, id: String) -> Result<CompletionState, ApplicationError> {
        receive(self.admit(|w| w.local.toggle(id))?).await
    }
    pub async fn get_settings_status(&self) -> Result<SettingsStatus, ApplicationError> {
        receive(self.admit(|w| w.settings.status())?).await
    }
    pub async fn save_moodle_config(&self, value: SecretText) -> Result<(), ApplicationError> {
        receive(self.admit(|w| w.settings.save_moodle(value))?).await
    }
    pub async fn remove_moodle_config(&self) -> Result<(), ApplicationError> {
        receive(self.admit(|w| w.settings.remove_moodle())?).await
    }
    pub async fn save_webclass_credentials(
        &self,
        value: WebClassCredentials,
    ) -> Result<(), ApplicationError> {
        receive(self.admit(|w| w.settings.save_webclass(value))?).await
    }
    pub async fn remove_webclass_credentials(&self) -> Result<(), ApplicationError> {
        receive(self.admit(|w| w.settings.remove_webclass())?).await
    }
    pub async fn set_sync_on_startup(&self, enabled: bool) -> Result<(), ApplicationError> {
        receive(self.admit(|w| w.settings.set_startup_sync(enabled))?).await
    }
    pub async fn sync_now(&self) -> Result<SyncReport, ApplicationError> {
        self.admit(|w| w.sync.enqueue())?
            .await
            .map_err(|_| ApplicationError::Unavailable)
    }
    pub fn get_sync_status(&self) -> Result<SyncStatus, ApplicationError> {
        self.admit(|w| w.sync.status())
    }

    /// Call after the initial local view is ready. The policy is checked once per
    /// application, including disabled/unconfigured/error cases. `None` means skip.
    /// Saving settings later never schedules a second startup run.
    pub async fn run_startup_sync(&self) -> Result<Option<SyncReport>, ApplicationError> {
        let status = self.admit(|w| {
            if self.runtime.startup_checked.swap(true, Ordering::AcqRel) {
                Ok(None)
            } else {
                w.settings.status().map(Some)
            }
        })?;
        let Some(status) = status else {
            return Ok(None);
        };
        let status = receive(status).await?;
        if !status.sync_on_startup || (!status.moodle_configured && !status.webclass_configured) {
            return Ok(None);
        }
        self.sync_now().await.map(Some)
    }
    pub fn begin_shutdown(&self) -> bool {
        self.runtime.close()
    }
    pub fn is_shutdown_complete(&self) -> bool {
        self.runtime.lifetime.is_done()
    }
    pub async fn shutdown(&self) {
        self.begin_shutdown();
        self.runtime.lifetime.wait().await;
    }
}

async fn receive<T>(response: Response<T>) -> Result<T, ApplicationError> {
    response.await.map_err(|_| ApplicationError::Unavailable)?
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
