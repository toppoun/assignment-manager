//! One network worker/SQLite connection. Admission is single-flight, with no pending-run queue.
use crate::{
    error::ApplicationError,
    settings_worker::SettingsState,
    storage::open_app_store,
    sync_service::{CoreSyncBackend, SyncBackend, SyncResultDto, SyncSecrets, SyncService},
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
};
use tokio::sync::oneshot;

struct FlightGuard(Arc<AtomicBool>);
impl Drop for FlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct Request {
    reply: oneshot::Sender<SyncResultDto>,
    _flight: FlightGuard,
}
pub struct SyncState {
    requests: SyncSender<Request>,
    running: Arc<AtomicBool>,
}

impl SyncState {
    pub fn start(
        directory: Result<PathBuf, ApplicationError>,
        settings: SettingsState,
    ) -> Result<Self, ApplicationError> {
        Self::start_with_backend(directory, settings, CoreSyncBackend)
    }
    pub fn start_with_backend<B: SyncBackend + Send + 'static, C: SyncSecrets + Send + 'static>(
        directory: Result<PathBuf, ApplicationError>,
        credentials: C,
        backend: B,
    ) -> Result<Self, ApplicationError> {
        let (requests, receiver) = mpsc::sync_channel::<Request>(1);
        Self::spawn(directory, credentials, backend, requests, receiver)
            .map(|(state, _worker)| state)
    }

    fn spawn<B: SyncBackend + Send + 'static, C: SyncSecrets + Send + 'static>(
        directory: Result<PathBuf, ApplicationError>,
        credentials: C,
        backend: B,
        requests: SyncSender<Request>,
        receiver: mpsc::Receiver<Request>,
    ) -> Result<(Self, thread::JoinHandle<()>), ApplicationError> {
        let running = Arc::new(AtomicBool::new(false));
        let worker = thread::Builder::new()
            .name("assignment-sync".into())
            .spawn(move || {
                // Same path helper/migrations as the list worker; opened exactly once here.
                let store = directory.and_then(|path| open_app_store(&path));
                let mut service = SyncService::new(backend, credentials, store);
                for request in receiver {
                    let result = service.run();
                    // Completion/cancellation of a command does not cancel an in-flight import.
                    // Release only after all network/DB work, including on unwinding.
                    drop(request._flight);
                    let _ = request.reply.send(result);
                }
            })
            .map_err(|_| ApplicationError::Unavailable)?;
        Ok((Self { requests, running }, worker))
    }
    pub async fn sync_now(&self) -> Result<SyncResultDto, ApplicationError> {
        if self
            .running
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Ok(SyncResultDto::AlreadyRunning);
        }
        let flight = FlightGuard(Arc::clone(&self.running));
        let (reply, response) = oneshot::channel();
        self.requests
            .try_send(Request {
                reply,
                _flight: flight,
            })
            .map_err(|_| ApplicationError::Unavailable)?;
        response.await.map_err(|_| ApplicationError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        credential_store::{SecretText, WebClassCredentials},
        dto::CompletionDto,
        list_query::{CompletionSelection, ListRequest},
        storage::DesktopState,
        sync_service::{
            SourceSyncResult, SyncCounts, SyncFailure,
            tests::{import, secrets},
        },
    };
    use assignment_manager::{persistence::SqliteStore, sync::WebClassCredentialProvider};
    use std::time::Duration;

    struct PausedNetwork {
        started: mpsc::Sender<()>,
        resume: mpsc::Receiver<()>,
        fail: bool,
    }
    impl SyncBackend for PausedNetwork {
        type Session = ();
        fn moodle(
            &mut self,
            _url: SecretText,
            store: &mut SqliteStore,
        ) -> Result<SyncCounts, SyncFailure> {
            self.started.send(()).unwrap();
            self.resume.recv_timeout(Duration::from_secs(15)).unwrap();
            let fail = self.fail;
            self.fail = true;
            if fail {
                Err(SyncFailure::NetworkFailed)
            } else {
                import(store)
            }
        }
        fn login(&mut self, _credentials: WebClassCredentials) -> Result<(), SyncFailure> {
            panic!("not configured")
        }
        fn webclass(
            &mut self,
            _: &mut (),
            _: &mut impl WebClassCredentialProvider,
            _: &mut SqliteStore,
        ) -> Result<SyncCounts, SyncFailure> {
            panic!("not configured")
        }
    }
    #[test]
    fn single_flight_network_wait_keeps_list_and_toggle_available_then_reload_preserves_completion()
    {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SyncState>();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_owned();
        // Seed through the same fake import used by subsequent syncs.
        let mut store = open_app_store(&path).unwrap();
        import(&mut store).unwrap();
        drop(store);
        let (list, list_thread) = DesktopState::start_worker(Ok(path.clone())).unwrap();
        let (started, start_signal) = mpsc::channel();
        let (resume, resume_signal) = mpsc::channel();
        let (requests, receiver) = mpsc::sync_channel(1);
        let (sync, sync_thread) = SyncState::spawn(
            Ok(path.clone()),
            secrets(true, false),
            PausedNetwork {
                started,
                resume: resume_signal,
                fail: false,
            },
            requests,
            receiver,
        )
        .unwrap();
        let sync = Arc::new(sync);
        let first = Arc::clone(&sync);
        let command = thread::spawn(move || tauri::async_runtime::block_on(first.sync_now()));
        start_signal.recv_timeout(Duration::from_secs(5)).unwrap();
        for _ in 0..5 {
            assert_eq!(
                tauri::async_runtime::block_on(sync.sync_now()).unwrap(),
                SyncResultDto::AlreadyRunning
            );
        }
        let (local_reply, local_response) = mpsc::channel();
        let local_command = thread::spawn(move || {
            let items = tauri::async_runtime::block_on(list.list(ListRequest::default()))
                .unwrap()
                .items;
            assert_eq!(items.len(), 1);
            let id = items[0].id.clone();
            assert_eq!(
                tauri::async_runtime::block_on(list.toggle(id.clone())).unwrap(),
                CompletionDto::Completed
            );
            local_reply.send((list, id)).unwrap();
        });
        // Must finish while the HTTP stand-in is still waiting; no DB worker monopolization.
        let (list, id) = local_response.recv_timeout(Duration::from_secs(5)).unwrap();
        local_command.join().unwrap();
        resume.send(()).unwrap();
        assert!(matches!(
            command.join().unwrap().unwrap(),
            SyncResultDto::Completed {
                moodle: SourceSyncResult::Success { .. },
                ..
            }
        ));
        assert!(start_signal.try_recv().is_err()); // repeated requests were not queued
        let all = ListRequest {
            completion: CompletionSelection::All,
            ..Default::default()
        };
        let items = tauri::async_runtime::block_on(list.list(all.clone()))
            .unwrap()
            .items;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, id);
        assert_eq!(items[0].completion, CompletionDto::Completed);
        // A failed later run must leave the existing local list operational.
        let later = Arc::clone(&sync);
        let command = thread::spawn(move || tauri::async_runtime::block_on(later.sync_now()));
        start_signal.recv_timeout(Duration::from_secs(5)).unwrap();
        resume.send(()).unwrap();
        assert!(matches!(
            command.join().unwrap().unwrap(),
            SyncResultDto::Completed {
                moodle: SourceSyncResult::Failed {
                    error: SyncFailure::NetworkFailed
                },
                ..
            }
        ));
        assert_eq!(
            tauri::async_runtime::block_on(list.list(all.clone()))
                .unwrap()
                .items[0]
                .completion,
            CompletionDto::Completed
        );
        drop(sync);
        sync_thread.join().unwrap();
        drop(list);
        list_thread.join().unwrap();
        let (list, worker) = DesktopState::start_worker(Ok(path)).unwrap();
        assert_eq!(
            tauri::async_runtime::block_on(list.list(all))
                .unwrap()
                .items[0]
                .completion,
            CompletionDto::Completed
        );
        assert_eq!(
            tauri::async_runtime::block_on(list.toggle(id)).unwrap(),
            CompletionDto::Incomplete
        );
        drop(list);
        worker.join().unwrap();
    }

    #[test]
    fn dropped_command_receiver_does_not_admit_a_second_run_until_network_finishes() {
        let root = tempfile::tempdir().unwrap();
        let (started, start_signal) = mpsc::channel();
        let (resume, resume_signal) = mpsc::channel();
        let (requests, receiver) = mpsc::sync_channel(1);
        let (sync, worker) = SyncState::spawn(
            Ok(root.path().into()),
            secrets(true, false),
            PausedNetwork {
                started,
                resume: resume_signal,
                fail: true,
            },
            requests,
            receiver,
        )
        .unwrap();
        sync.running.store(true, Ordering::Release);
        let (reply, response) = oneshot::channel();
        sync.requests
            .send(Request {
                reply,
                _flight: FlightGuard(Arc::clone(&sync.running)),
            })
            .unwrap();
        start_signal.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(response);
        assert_eq!(
            tauri::async_runtime::block_on(sync.sync_now()).unwrap(),
            SyncResultDto::AlreadyRunning
        );
        resume.send(()).unwrap();
        let running = Arc::clone(&sync.running);
        drop(sync);
        worker.join().unwrap();
        assert!(!running.load(Ordering::Acquire));
    }
}
