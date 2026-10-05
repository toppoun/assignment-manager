use crate::{
    error::ApplicationError,
    lifecycle::WorkerGuard,
    models::AssignmentListResponse,
    query::{AssignmentQuery, QueryClock, list_from_store},
};
use assignment_manager::{
    actions,
    domain::{AssignmentId, CompletionState},
    persistence::SqliteStore,
};
use chrono::Local;
use std::{
    fs,
    path::PathBuf,
    sync::mpsc::{self, SyncSender},
    thread,
};
use tokio::sync::oneshot;

pub(crate) type Reply<T> = oneshot::Sender<Result<T, ApplicationError>>;
pub(crate) type Response<T> = oneshot::Receiver<Result<T, ApplicationError>>;
enum Request {
    List(AssignmentQuery, Reply<AssignmentListResponse>),
    Toggle(String, Reply<CompletionState>),
}

pub(crate) struct LocalWorker {
    requests: SyncSender<Request>,
}

/// Portable path preparation. The shell resolves the absolute database path.
pub(crate) fn open_store(path: PathBuf) -> Result<SqliteStore, ApplicationError> {
    if !path.is_absolute() {
        return Err(ApplicationError::DataDirectory);
    }
    let parent = path.parent().ok_or(ApplicationError::DataDirectory)?;
    fs::create_dir_all(parent).map_err(|_| ApplicationError::DataDirectory)?;
    SqliteStore::open(path).map_err(|_| ApplicationError::DatabaseStartup)
}

impl LocalWorker {
    pub fn start(
        path: Result<PathBuf, ApplicationError>,
        lifetime: WorkerGuard,
    ) -> Result<Self, ApplicationError> {
        let (requests, receiver) = mpsc::sync_channel(32);
        thread::Builder::new()
            .name("assignment-store".into())
            .spawn(move || {
                // Declared first: the store drops before completion is signalled.
                let _lifetime = lifetime;
                let mut store = path.and_then(open_store);
                for request in receiver {
                    match request {
                        Request::List(query, reply) => {
                            let result = store.as_ref().map_err(|e| *e).and_then(|store| {
                                // Exactly one clock sample per response.
                                QueryClock::from_local(Local::now())
                                    .and_then(|clock| list_from_store(store, &query, &clock))
                            });
                            let _ = reply.send(result);
                        }
                        Request::Toggle(id, reply) => {
                            let result = store.as_mut().map_err(|e| *e).and_then(|store| {
                                actions::toggle_assignment_completion(store, &AssignmentId::new(id))
                                    .map_err(Into::into)
                            });
                            // Commit acknowledgement only. A later refresh is a separate operation.
                            let _ = reply.send(result);
                        }
                    }
                }
            })
            .map_err(|_| ApplicationError::Unavailable)?;
        Ok(Self { requests })
    }
    pub fn list(
        &self,
        query: AssignmentQuery,
    ) -> Result<Response<AssignmentListResponse>, ApplicationError> {
        let (reply, response) = oneshot::channel();
        self.send(Request::List(query, reply))?;
        Ok(response)
    }
    pub fn toggle(&self, id: String) -> Result<Response<CompletionState>, ApplicationError> {
        if id.is_empty() {
            return Err(ApplicationError::InvalidAssignmentId);
        }
        let (reply, response) = oneshot::channel();
        self.send(Request::Toggle(id, reply))?;
        Ok(response)
    }
    fn send(&self, request: Request) -> Result<(), ApplicationError> {
        self.requests
            .try_send(request)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => ApplicationError::Busy,
                mpsc::TrySendError::Disconnected(_) => ApplicationError::Unavailable,
            })
    }
}
