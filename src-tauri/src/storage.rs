//! Tauri path/native adapter construction and exit lifecycle. No DB ownership.
use crate::{credential_store::OsCredentialStore, error::ApplicationError, settings::SettingsFile};
use assignment_manager_application::ApplicationHandle;
#[cfg(feature = "smoke-test")]
use assignment_manager_application::SyncBackend;
use std::path::PathBuf;
use tauri::Manager;

fn database_path(
    directory: Result<PathBuf, ApplicationError>,
) -> Result<PathBuf, ApplicationError> {
    let directory = directory?;
    if !directory.is_absolute() {
        return Err(ApplicationError::DataDirectory);
    }
    Ok(directory.join("assignment-manager.db"))
}

// This module is also compiled by the separate smoke executable, which uses
// the synthetic constructor instead of this production constructor.
#[cfg_attr(feature = "smoke-test", allow(dead_code))]
pub fn open_application(
    directory: Result<PathBuf, ApplicationError>,
    config: Result<PathBuf, ApplicationError>,
    service: &'static str,
) -> Result<ApplicationHandle, ApplicationError> {
    ApplicationHandle::open(database_path(directory), move || {
        Ok((
            SettingsFile::new(&config?)?,
            OsCredentialStore::new(service)?,
        ))
    })
}

#[cfg(feature = "smoke-test")]
// The production executable never selects the synthetic backend.
#[allow(dead_code)]
pub fn open_with_backend<B: SyncBackend + Send + 'static>(
    directory: Result<PathBuf, ApplicationError>,
    config: Result<PathBuf, ApplicationError>,
    service: &'static str,
    backend: B,
) -> Result<ApplicationHandle, ApplicationError> {
    ApplicationHandle::open_with_backend(
        database_path(directory),
        move || {
            Ok((
                SettingsFile::new(&config?)?,
                OsCredentialStore::new(service)?,
            ))
        },
        backend,
    )
}

/// Prevent the first exit until accepted jobs/resources finish, without UI joins.
pub fn handle_exit(app: &tauri::AppHandle, event: &tauri::RunEvent) {
    if let tauri::RunEvent::ExitRequested { api, .. } = event {
        let application = app.state::<ApplicationHandle>();
        if !application.is_shutdown_complete() {
            api.prevent_exit();
            if application.begin_shutdown() {
                let application = application.inner().clone();
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    application.shutdown().await;
                    app.exit(0);
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adapter_uses_fixed_filename_and_passes_path_failures_as_safe_categories() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            database_path(Ok(root.path().into())).unwrap(),
            root.path().join("assignment-manager.db")
        );
        assert_eq!(
            database_path(Ok(PathBuf::from("relative"))),
            Err(ApplicationError::DataDirectory)
        );
        assert_eq!(
            database_path(Err(ApplicationError::DataDirectory)),
            Err(ApplicationError::DataDirectory)
        );
    }
    #[test]
    fn bad_config_adapter_never_touches_native_store_and_local_query_remains_available() {
        let root = tempfile::tempdir().unwrap();
        let application = open_application(
            Ok(root.path().into()),
            Err(ApplicationError::SettingsReadFailed),
            "never-accessed-test-store",
        )
        .unwrap();
        tauri::async_runtime::block_on(async {
            assert!(
                application
                    .query_assignments(Default::default())
                    .await
                    .unwrap()
                    .items
                    .is_empty()
            );
            assert_eq!(
                application.get_settings_status().await,
                Err(ApplicationError::SettingsReadFailed)
            );
            application.shutdown().await;
        });
    }
}
