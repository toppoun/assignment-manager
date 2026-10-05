//! Thin macOS transport boundary. Workers, storage and policy belong to application/core.
//! No handwritten FFI or OS API implementations. Native sync is not exported yet.

mod dto;
mod error;
mod native_ports;
mod ports;

pub use dto::*;
pub use error::BridgeError;
pub use native_ports::{
    CredentialEntry, NativeCredentialStore, NativeSettingsStore, NativeStoreError,
};

use assignment_manager_application::{
    ApplicationHandle,
    credentials::{SecretText, WebClassCredentials},
    query::AssignmentQuery,
};
use std::{path::PathBuf, sync::Arc};

uniffi::setup_scaffolding!();

/// UniFFI owns an Arc of this object; the handle owns the shared Rust worker lifetime.
/// Dropping the last proxy initiates nonblocking cleanup. Prefer awaiting shutdown.
#[derive(uniffi::Object)]
pub struct MacApplication {
    application: ApplicationHandle,
}

#[uniffi::export]
impl MacApplication {
    #[uniffi::constructor]
    pub async fn open_with_native_stores(
        database_path: String,
        settings: Arc<dyn NativeSettingsStore>,
        credentials: Arc<dyn NativeCredentialStore>,
    ) -> Result<Arc<Self>, BridgeError> {
        let application = ApplicationHandle::open(Ok(PathBuf::from(database_path)), move || {
            Ok((
                native_ports::NativeSettings(settings),
                native_ports::NativeCredentials(credentials),
            ))
        })?;
        if let Err(error) = application
            .query_assignments(AssignmentQuery::default())
            .await
        {
            application.shutdown().await;
            return Err(error.into());
        }
        Ok(Arc::new(Self { application }))
    }

    pub async fn get_settings_status(&self) -> Result<SettingsStatusDto, BridgeError> {
        self.application
            .get_settings_status()
            .await
            .map(Into::into)
            .map_err(Into::into)
    }
    pub async fn save_moodle_config(&self, calendar_url: String) -> Result<(), BridgeError> {
        self.application
            .save_moodle_config(SecretText::new(calendar_url))
            .await
            .map_err(Into::into)
    }
    pub async fn remove_moodle_config(&self) -> Result<(), BridgeError> {
        self.application
            .remove_moodle_config()
            .await
            .map_err(Into::into)
    }
    pub async fn save_webclass_credentials(
        &self,
        university_id: String,
        password: String,
    ) -> Result<(), BridgeError> {
        self.application
            .save_webclass_credentials(WebClassCredentials {
                university_id: SecretText::new(university_id),
                password: SecretText::new(password),
            })
            .await
            .map_err(Into::into)
    }
    pub async fn remove_webclass_credentials(&self) -> Result<(), BridgeError> {
        self.application
            .remove_webclass_credentials()
            .await
            .map_err(Into::into)
    }
    pub async fn set_sync_on_startup(&self, enabled: bool) -> Result<(), BridgeError> {
        self.application
            .set_sync_on_startup(enabled)
            .await
            .map_err(Into::into)
    }
    /// The native shell supplies an absolute database path. Validate worker initialization
    /// before returning a usable proxy; shared ApplicationHandle::open remains unchanged.
    #[uniffi::constructor]
    pub async fn open(database_path: String) -> Result<Arc<Self>, BridgeError> {
        let application = ApplicationHandle::open(Ok(PathBuf::from(database_path)), || {
            Ok((ports::PocSettings, ports::NoCredentials))
        })?;
        if let Err(error) = application
            .query_assignments(AssignmentQuery::default())
            .await
        {
            application.shutdown().await;
            return Err(error.into());
        }
        Ok(Arc::new(Self { application }))
    }

    /// Omitted options use the shared default: incomplete, all courses/deadlines,
    /// deadline sort. Swift never filters or sorts this response.
    #[uniffi::method(default(options = None))]
    pub async fn query_assignments(
        &self,
        options: Option<QueryOptions>,
    ) -> Result<AssignmentListDto, BridgeError> {
        self.application
            .query_assignments(options.map(Into::into).unwrap_or_default())
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Commit acknowledgement only. Refresh is a separate call with its own outcome.
    pub async fn toggle_completion(&self, id: String) -> Result<CompletionDto, BridgeError> {
        self.application
            .toggle_completion(id)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Reject new requests across every proxy, drain accepted work, await worker cleanup.
    /// Result permits UniFFI to report an unexpected panic through Swift throws.
    pub async fn shutdown(&self) -> Result<(), BridgeError> {
        self.application.shutdown().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
