use std::{
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, SyncSender},
    thread::{self, JoinHandle},
};

use assignment_manager::{actions, domain::AssignmentId, persistence::SqliteStore};
use chrono::Local;
use tokio::sync::oneshot;

use crate::{
    dto::{AssignmentListDto, CompletionDto},
    error::ApplicationError,
    list_query::{ListRequest, QueryClock, list_from_store},
};

const DATABASE_FILENAME: &str = "assignment-manager.db";
const QUEUE_CAPACITY: usize = 32;
type Reply<T> = oneshot::Sender<Result<T, ApplicationError>>;

enum Request {
    List(ListRequest, Reply<AssignmentListDto>),
    Toggle(String, Reply<CompletionDto>),
}

/// Send/Sync command state contains only a bounded request sender, never a Connection.
/// One worker exclusively owns SqliteStore (rusqlite Connection is Send, not Sync).
pub struct DesktopState {
    requests: SyncSender<Request>,
}

impl DesktopState {
    pub fn start(directory: Result<PathBuf, ApplicationError>) -> Result<Self, ApplicationError> {
        Self::start_worker(directory).map(|(state, _worker)| state)
    }

    fn start_worker(
        directory: Result<PathBuf, ApplicationError>,
    ) -> Result<(Self, JoinHandle<()>), ApplicationError> {
        let (requests, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let worker = thread::Builder::new()
            .name("assignment-store".into())
            .spawn(move || {
                // Initialize exactly once, before any queued command. A startup failure
                // is retained as a fixed error; commands never retry open/migrations.
                let mut store = directory.and_then(|path| open_app_store(&path));
                for request in receiver {
                    match request {
                        Request::List(request, reply) => {
                            let result = match &store {
                                Ok(store) => QueryClock::from_local(Local::now())
                                    .and_then(|clock| list_from_store(store, &request, &clock)),
                                Err(error) => Err(*error),
                            };
                            let _ = reply.send(result);
                        }
                        Request::Toggle(id, reply) => {
                            let result = match &mut store {
                                Ok(store) => actions::toggle_assignment_completion(
                                    store,
                                    &AssignmentId::new(id),
                                )
                                .map(Into::into)
                                .map_err(Into::into),
                                Err(error) => Err(*error),
                            };
                            let _ = reply.send(result);
                        }
                    }
                }
                // Dropping the final sender stops the worker and closes the store.
            })
            .map_err(|_| ApplicationError::Unavailable)?;
        Ok((Self { requests }, worker))
    }

    fn send(&self, request: Request) -> Result<(), ApplicationError> {
        self.requests
            .try_send(request)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => ApplicationError::Busy,
                mpsc::TrySendError::Disconnected(_) => ApplicationError::Unavailable,
            })
    }

    pub async fn list(&self, request: ListRequest) -> Result<AssignmentListDto, ApplicationError> {
        let (reply, response) = oneshot::channel();
        self.send(Request::List(request, reply))?;
        response.await.map_err(|_| ApplicationError::Unavailable)?
    }

    pub async fn toggle(&self, id: String) -> Result<CompletionDto, ApplicationError> {
        if id.is_empty() {
            return Err(ApplicationError::InvalidAssignmentId);
        }
        let (reply, response) = oneshot::channel();
        self.send(Request::Toggle(id, reply))?;
        response.await.map_err(|_| ApplicationError::Unavailable)?
    }
}

fn database_path(directory: &Path) -> Result<PathBuf, ApplicationError> {
    if !directory.is_absolute() {
        return Err(ApplicationError::DataDirectory);
    }
    Ok(directory.join(DATABASE_FILENAME))
}

fn open_app_store(directory: &Path) -> Result<SqliteStore, ApplicationError> {
    let path = database_path(directory)?;
    fs::create_dir_all(directory).map_err(|_| ApplicationError::DataDirectory)?;
    // The core alone owns schema v3 and migrations.
    SqliteStore::open(path).map_err(|_| ApplicationError::DatabaseStartup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::list_query::{CompletionSelection, time_context};
    use assignment_manager::{
        domain::{Deadline, SubmissionTarget},
        persistence::ManualAssignmentInput,
    };
    use chrono::{DateTime, Utc};

    fn all_request() -> ListRequest {
        ListRequest {
            completion: CompletionSelection::All,
            ..Default::default()
        }
    }

    fn stop(state: DesktopState, worker: JoinHandle<()>) {
        drop(state);
        worker.join().unwrap();
    }

    #[test]
    fn state_meets_tauri_thread_safety_without_unsafe_impls() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DesktopState>();
    }

    #[test]
    fn fixed_filename_and_nested_directory_creation_use_the_given_app_directory() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("app").join("data");
        assert_eq!(
            database_path(&directory).unwrap(),
            directory.join(DATABASE_FILENAME)
        );
        assert!(open_app_store(&directory).is_ok());
        assert!(directory.join(DATABASE_FILENAME).is_file());
        assert_eq!(
            database_path(Path::new("relative-data")).unwrap_err(),
            ApplicationError::DataDirectory
        );
    }

    #[test]
    fn local_calendar_and_offset_come_from_the_same_clock_sample() {
        for timestamp in ["2026-10-04T00:30:00+05:45", "2026-10-03T23:30:00-07:00"] {
            let local = DateTime::parse_from_rfc3339(timestamp).unwrap();
            let time = time_context(local);
            assert_eq!(time.now, local.with_timezone(&Utc));
            assert_eq!(time.local_date, local.date_naive());
            assert_eq!(time.utc_offset, *local.offset());
        }
    }

    #[test]
    fn worker_lists_all_by_deadline_and_toggle_survives_worker_restart() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("app-data");
        let mut seed = open_app_store(&directory).unwrap();
        let mut ids = Vec::new();
        for deadline in [
            None,
            Some(Deadline::DateOnly("2026-10-04".parse().unwrap())),
        ] {
            ids.push(
                seed.create_manual_assignment(ManualAssignmentInput {
                    title: "テスト課題".into(),
                    deadline,
                    course_id: None,
                    submission: SubmissionTarget::Unknown,
                    submission_url: None,
                })
                .unwrap()
                .id
                .as_str()
                .to_owned(),
            );
        }
        drop(seed);
        let (state, worker) = DesktopState::start_worker(Ok(directory.clone())).unwrap();
        tauri::async_runtime::block_on(async {
            let items = state.list(all_request()).await.unwrap().items;
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].id, ids[1]);
            assert_eq!(items[1].id, ids[0]);
            assert_eq!(
                state.toggle(ids[1].clone()).await.unwrap(),
                CompletionDto::Completed
            );
            assert_eq!(
                state.list(all_request()).await.unwrap().items[0].completion,
                CompletionDto::Completed
            );
            let incomplete = state.list(ListRequest::default()).await.unwrap();
            assert_eq!(incomplete.total_count, 2);
            assert_eq!(incomplete.items.len(), 1);
            assert_eq!(incomplete.items[0].id, ids[0]);
            assert_eq!(
                state
                    .toggle("unknown-internal-id".into())
                    .await
                    .unwrap_err(),
                ApplicationError::NotFound
            );
            assert_eq!(
                state.toggle(String::new()).await.unwrap_err(),
                ApplicationError::InvalidAssignmentId
            );
        });
        stop(state, worker);
        let (state, worker) = DesktopState::start_worker(Ok(directory)).unwrap();
        tauri::async_runtime::block_on(async {
            assert_eq!(
                state.list(all_request()).await.unwrap().items[0].completion,
                CompletionDto::Completed
            );
            assert_eq!(
                state.toggle(ids[1].clone()).await.unwrap(),
                CompletionDto::Incomplete
            );
        });
        stop(state, worker);
    }

    #[test]
    fn empty_database_is_a_successful_empty_list() {
        let root = tempfile::tempdir().unwrap();
        let (state, worker) = DesktopState::start_worker(Ok(root.path().join("app"))).unwrap();
        assert!(
            tauri::async_runtime::block_on(state.list(all_request()))
                .unwrap()
                .items
                .is_empty()
        );
        stop(state, worker);
    }

    #[test]
    fn startup_failures_are_safe_retained_errors_and_never_reopen_per_command() {
        let root = tempfile::tempdir().unwrap();
        let blocked = root.path().join("private-data-directory");
        fs::write(&blocked, "a file prevents directory creation").unwrap();
        let (state, worker) = DesktopState::start_worker(Ok(blocked.clone())).unwrap();
        assert_eq!(
            tauri::async_runtime::block_on(state.list(all_request())).unwrap_err(),
            ApplicationError::DataDirectory
        );
        // Repairing the path does not cause a command to open a different connection.
        fs::remove_file(&blocked).unwrap();
        fs::create_dir(&blocked).unwrap();
        assert_eq!(
            tauri::async_runtime::block_on(state.list(all_request())).unwrap_err(),
            ApplicationError::DataDirectory
        );
        stop(state, worker);

        let bad_database = root.path().join(DATABASE_FILENAME);
        fs::write(&bad_database, "invalid SQLite content containing a secret").unwrap();
        let (state, worker) = DesktopState::start_worker(Ok(root.path().to_owned())).unwrap();
        let error = tauri::async_runtime::block_on(state.list(all_request())).unwrap_err();
        assert_eq!(error, ApplicationError::DatabaseStartup);
        let serialized = serde_json::to_string(&error).unwrap();
        assert!(!serialized.contains("secret"));
        assert!(!serialized.contains("private-data-directory"));
        assert!(!serialized.contains(&root.path().to_string_lossy().to_string()));
        stop(state, worker);

        let (state, worker) =
            DesktopState::start_worker(Err(ApplicationError::DataDirectory)).unwrap();
        assert_eq!(
            tauri::async_runtime::block_on(state.list(all_request())).unwrap_err(),
            ApplicationError::DataDirectory
        );
        stop(state, worker);
    }
}
