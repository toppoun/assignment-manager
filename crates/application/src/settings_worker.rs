//! Credential/configuration ports are constructed and accessed on this worker.
use crate::{
    credentials::{CredentialStore, SecretText, WebClassCredentials},
    error::ApplicationError,
    lifecycle::WorkerGuard,
    local_worker::{Reply, Response},
    settings::{SettingsService, SettingsStatus, SettingsStore},
};
use std::{
    sync::mpsc::{self, SyncSender},
    thread,
};
use tokio::sync::oneshot;

enum Request {
    Status(Reply<SettingsStatus>),
    SaveMoodle(SecretText, Reply<()>),
    RemoveMoodle(Reply<()>),
    SaveWebclass(WebClassCredentials, Reply<()>),
    RemoveWebclass(Reply<()>),
    StartupSync(bool, Reply<()>),
    LoadMoodle(Reply<Option<SecretText>>),
    LoadWebclass(Reply<(u64, Option<WebClassCredentials>)>),
}

#[derive(Clone)]
pub(crate) struct SettingsWorker {
    requests: SyncSender<Request>,
}

impl SettingsWorker {
    pub fn start<S: SettingsStore + 'static, C: CredentialStore + 'static>(
        initialize: impl FnOnce() -> Result<(S, C), ApplicationError> + Send + 'static,
        lifetime: WorkerGuard,
    ) -> Result<Self, ApplicationError> {
        let (requests, receiver) = mpsc::sync_channel(8);
        thread::Builder::new()
            .name("assignment-settings".into())
            .spawn(move || {
                let _lifetime = lifetime;
                let mut service = initialize().and_then(|(mut storage, credentials)| {
                    Ok((
                        SettingsService::new(storage.load_or_create()?, credentials),
                        storage,
                    ))
                });
                for request in receiver {
                    macro_rules! respond {
                        ($reply:expr, $action:expr) => {
                            let _ = $reply.send(service.as_mut().map_err(|e| *e).and_then($action));
                        };
                    }
                    match request {
                        Request::Status(reply) => {
                            respond!(reply, |(service, _)| service.status());
                        }
                        Request::SaveMoodle(value, reply) => {
                            respond!(reply, |(service, _)| service.save_moodle(value));
                        }
                        Request::RemoveMoodle(reply) => {
                            respond!(reply, |(service, _)| service.remove_moodle());
                        }
                        Request::SaveWebclass(value, reply) => {
                            respond!(reply, |(service, _)| service.save_webclass(value));
                        }
                        Request::RemoveWebclass(reply) => {
                            respond!(reply, |(service, _)| service.remove_webclass());
                        }
                        Request::StartupSync(enabled, reply) => {
                            respond!(reply, |(service, storage)| service
                                .set_startup_sync(storage, enabled));
                        }
                        Request::LoadMoodle(reply) => {
                            respond!(reply, |(service, _)| service.load_moodle());
                        }
                        Request::LoadWebclass(reply) => {
                            respond!(reply, |(service, _)| service.load_webclass());
                        }
                    }
                }
            })
            .map_err(|_| ApplicationError::Unavailable)?;
        Ok(Self { requests })
    }
    fn send<T>(
        &self,
        make: impl FnOnce(Reply<T>) -> Request,
    ) -> Result<Response<T>, ApplicationError> {
        let (reply, response) = oneshot::channel();
        self.requests
            .try_send(make(reply))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => ApplicationError::Busy,
                mpsc::TrySendError::Disconnected(_) => ApplicationError::Unavailable,
            })?;
        Ok(response)
    }
    pub fn status(&self) -> Result<Response<SettingsStatus>, ApplicationError> {
        self.send(Request::Status)
    }
    pub fn save_moodle(&self, value: SecretText) -> Result<Response<()>, ApplicationError> {
        self.send(|reply| Request::SaveMoodle(value, reply))
    }
    pub fn remove_moodle(&self) -> Result<Response<()>, ApplicationError> {
        self.send(Request::RemoveMoodle)
    }
    pub fn save_webclass(
        &self,
        value: WebClassCredentials,
    ) -> Result<Response<()>, ApplicationError> {
        self.send(|reply| Request::SaveWebclass(value, reply))
    }
    pub fn remove_webclass(&self) -> Result<Response<()>, ApplicationError> {
        self.send(Request::RemoveWebclass)
    }
    pub fn set_startup_sync(&self, value: bool) -> Result<Response<()>, ApplicationError> {
        self.send(|reply| Request::StartupSync(value, reply))
    }
    pub fn load_moodle_blocking(&self) -> Result<Option<SecretText>, ApplicationError> {
        self.send(Request::LoadMoodle)?
            .blocking_recv()
            .map_err(|_| ApplicationError::Unavailable)?
    }
    pub fn load_webclass_blocking(
        &self,
    ) -> Result<(u64, Option<WebClassCredentials>), ApplicationError> {
        self.send(Request::LoadWebclass)?
            .blocking_recv()
            .map_err(|_| ApplicationError::Unavailable)?
    }
}
