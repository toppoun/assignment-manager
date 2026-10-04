//! Blocking credential/config work is serialized away from the UI and SQLite worker.
use std::{
    path::PathBuf,
    sync::mpsc::{self, SyncSender},
    thread,
};
use tokio::sync::oneshot;

use crate::{
    credential_store::{OsCredentialStore, SecretText, WebClassCredentials},
    error::ApplicationError,
    settings::{SettingsFile, SettingsService, SettingsStatusDto},
};

type Reply<T> = oneshot::Sender<Result<T, ApplicationError>>;
enum Request {
    Status(Reply<SettingsStatusDto>),
    SaveMoodle(SecretText, Reply<()>),
    RemoveMoodle(Reply<()>),
    SaveWebclass(WebClassCredentials, Reply<()>),
    RemoveWebclass(Reply<()>),
}

pub struct SettingsState {
    requests: SyncSender<Request>,
}

impl SettingsState {
    pub fn start(
        directory: Result<PathBuf, ApplicationError>,
        service_name: &'static str,
    ) -> Result<Self, ApplicationError> {
        let (requests, receiver) = mpsc::sync_channel(8);
        thread::Builder::new()
            .name("assignment-settings".into())
            .spawn(move || {
                let mut service = directory
                    .and_then(|directory| SettingsFile::new(&directory)?.load_or_create())
                    .and_then(|config| {
                        Ok(SettingsService::new(
                            config,
                            OsCredentialStore::new(service_name)?,
                        ))
                    });
                for request in receiver {
                    match request {
                        Request::Status(reply) => {
                            let _ = reply.send(
                                service
                                    .as_ref()
                                    .map_err(|error| *error)
                                    .and_then(SettingsService::status),
                            );
                        }
                        Request::SaveMoodle(value, reply) => {
                            let _ = reply.send(
                                service
                                    .as_mut()
                                    .map_err(|error| *error)
                                    .and_then(|service| service.save_moodle(value)),
                            );
                        }
                        Request::RemoveMoodle(reply) => {
                            let _ = reply.send(
                                service
                                    .as_mut()
                                    .map_err(|error| *error)
                                    .and_then(SettingsService::remove_moodle),
                            );
                        }
                        Request::SaveWebclass(value, reply) => {
                            let _ = reply.send(
                                service
                                    .as_mut()
                                    .map_err(|error| *error)
                                    .and_then(|service| service.save_webclass(value)),
                            );
                        }
                        Request::RemoveWebclass(reply) => {
                            let _ = reply.send(
                                service
                                    .as_mut()
                                    .map_err(|error| *error)
                                    .and_then(SettingsService::remove_webclass),
                            );
                        }
                    }
                }
            })
            .map_err(|_| ApplicationError::Unavailable)?;
        Ok(Self { requests })
    }

    async fn request<T>(
        &self,
        make: impl FnOnce(Reply<T>) -> Request,
    ) -> Result<T, ApplicationError> {
        let (reply, response) = oneshot::channel();
        self.requests
            .try_send(make(reply))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => ApplicationError::Busy,
                mpsc::TrySendError::Disconnected(_) => ApplicationError::Unavailable,
            })?;
        response.await.map_err(|_| ApplicationError::Unavailable)?
    }

    pub async fn status(&self) -> Result<SettingsStatusDto, ApplicationError> {
        self.request(Request::Status).await
    }
    pub async fn save_moodle(&self, value: SecretText) -> Result<(), ApplicationError> {
        self.request(|reply| Request::SaveMoodle(value, reply))
            .await
    }
    pub async fn remove_moodle(&self) -> Result<(), ApplicationError> {
        self.request(Request::RemoveMoodle).await
    }
    pub async fn save_webclass(&self, value: WebClassCredentials) -> Result<(), ApplicationError> {
        self.request(|reply| Request::SaveWebclass(value, reply))
            .await
    }
    pub async fn remove_webclass(&self) -> Result<(), ApplicationError> {
        self.request(Request::RemoveWebclass).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn state_is_send_sync_and_bad_config_is_retained_as_a_safe_error() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SettingsState>();
        let state = SettingsState::start(
            Err(ApplicationError::SettingsReadFailed),
            "never-accessed-test-store",
        )
        .unwrap();
        assert_eq!(
            tauri::async_runtime::block_on(state.status()),
            Err(ApplicationError::SettingsReadFailed)
        );
        assert_eq!(
            tauri::async_runtime::block_on(state.remove_moodle()),
            Err(ApplicationError::SettingsReadFailed)
        );
    }
}
