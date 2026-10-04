//! All OS credential access stays on this thread, including sync credential reads.
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
    StartupSync(bool, Reply<()>),
    LoadMoodle(Reply<Option<SecretText>>),
    LoadWebclass(Reply<(u64, Option<WebClassCredentials>)>),
}

#[derive(Clone)]
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
                let mut service = directory.and_then(|directory| {
                    let file = SettingsFile::new(&directory)?;
                    let config = file.load_or_create()?;
                    Ok((
                        file,
                        SettingsService::new(config, OsCredentialStore::new(service_name)?),
                    ))
                });
                for request in receiver {
                    macro_rules! respond {
                        ($reply:expr, $action:expr) => {
                            let _ = $reply
                                .send(service.as_mut().map_err(|error| *error).and_then($action));
                        };
                    }
                    match request {
                        Request::Status(reply) => {
                            respond!(reply, |(_, service)| service.status());
                        }
                        Request::SaveMoodle(value, reply) => {
                            respond!(reply, |(_, service)| service.save_moodle(value));
                        }
                        Request::RemoveMoodle(reply) => {
                            respond!(reply, |(_, service)| service.remove_moodle());
                        }
                        Request::SaveWebclass(value, reply) => {
                            respond!(reply, |(_, service)| service.save_webclass(value));
                        }
                        Request::RemoveWebclass(reply) => {
                            respond!(reply, |(_, service)| service.remove_webclass());
                        }
                        Request::StartupSync(enabled, reply) => {
                            respond!(reply, |(file, service)| service
                                .set_startup_sync(file, enabled));
                        }
                        Request::LoadMoodle(reply) => {
                            respond!(reply, |(_, service)| service.load_moodle());
                        }
                        Request::LoadWebclass(reply) => {
                            respond!(reply, |(_, service)| service.load_webclass());
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
    ) -> Result<oneshot::Receiver<Result<T, ApplicationError>>, ApplicationError> {
        let (reply, response) = oneshot::channel();
        self.requests
            .try_send(make(reply))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => ApplicationError::Busy,
                mpsc::TrySendError::Disconnected(_) => ApplicationError::Unavailable,
            })?;
        Ok(response)
    }

    async fn request<T>(
        &self,
        make: impl FnOnce(Reply<T>) -> Request,
    ) -> Result<T, ApplicationError> {
        self.send(make)?
            .await
            .map_err(|_| ApplicationError::Unavailable)?
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
    pub async fn set_startup_sync(&self, enabled: bool) -> Result<(), ApplicationError> {
        self.request(|reply| Request::StartupSync(enabled, reply))
            .await
    }

    // Only the dedicated network thread calls these. OS access still runs above.
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
        assert_eq!(
            state.load_moodle_blocking().unwrap_err(),
            ApplicationError::SettingsReadFailed
        );
        assert!(matches!(
            state.load_webclass_blocking(),
            Err(ApplicationError::SettingsReadFailed)
        ));
    }
}
