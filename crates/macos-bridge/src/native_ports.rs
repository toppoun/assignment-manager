//! Synchronous foreign callbacks execute only on the shared settings worker.
//! Swift owns OS I/O; application owns validation, revision and session policy.
use assignment_manager_application::{
    credentials::{CredentialStore, SecretText, WebClassCredentials},
    error::ApplicationError,
    settings::{Settings, SettingsStore},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum CredentialEntry {
    Moodle,
    WebClass,
}

/// Contains no platform status, message, secret or underlying cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum NativeStoreError {
    #[error("unavailable")]
    Unavailable,
    #[error("failed")]
    Failed,
}
impl From<uniffi::UnexpectedUniFFICallbackError> for NativeStoreError {
    fn from(_: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Failed
    }
}

/// Private credential adapter transport, never a Settings/UI read model.
/// Owned byte buffers avoid generated secret records with Debug/Equatable.
/// Missing is None; write must replace a whole entry without delete-first.
#[uniffi::export(foreign)]
pub trait NativeCredentialStore: Send + Sync {
    fn read(&self, entry: CredentialEntry) -> Result<Option<Vec<u8>>, NativeStoreError>;
    fn write(&self, entry: CredentialEntry, value: Vec<u8>) -> Result<(), NativeStoreError>;
    fn delete(&self, entry: CredentialEntry) -> Result<(), NativeStoreError>;
}

#[uniffi::export(foreign)]
pub trait NativeSettingsStore: Send + Sync {
    /// None delegates the default to shared Rust Settings.
    fn load_sync_on_startup(&self) -> Result<Option<bool>, NativeStoreError>;
    fn save_sync_on_startup(&self, enabled: bool) -> Result<(), NativeStoreError>;
}

pub(crate) struct NativeSettings(pub Arc<dyn NativeSettingsStore>);
impl SettingsStore for NativeSettings {
    fn load_or_create(&mut self) -> Result<Settings, ApplicationError> {
        let mut settings = Settings::default();
        if let Some(enabled) = self
            .0
            .load_sync_on_startup()
            .map_err(|_| ApplicationError::SettingsReadFailed)?
        {
            settings.sync_on_startup = enabled;
        }
        Ok(settings)
    }
    fn save(&mut self, settings: &Settings) -> Result<(), ApplicationError> {
        self.0
            .save_sync_on_startup(settings.sync_on_startup)
            .map_err(|_| ApplicationError::SettingsWriteFailed)
    }
}

pub(crate) struct NativeCredentials(pub Arc<dyn NativeCredentialStore>);
fn map_error(error: NativeStoreError, operation: ApplicationError) -> ApplicationError {
    match error {
        NativeStoreError::Unavailable => ApplicationError::CredentialUnavailable,
        NativeStoreError::Failed => operation,
    }
}
// Encoding belongs to this private transport adapter; secret validation stays shared.
// No Debug/Clone; decoding storage errors never returns the offending input.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredWebClass {
    #[serde(deserialize_with = "decode_secret")]
    university_id: SecretText,
    #[serde(deserialize_with = "decode_secret")]
    password: SecretText,
}
fn decode_secret<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<SecretText, D::Error> {
    String::deserialize(deserializer).map(SecretText::new)
}
impl CredentialStore for NativeCredentials {
    fn load_moodle_feed_secret(&self) -> Result<Option<SecretText>, ApplicationError> {
        self.0
            .read(CredentialEntry::Moodle)
            .map_err(|e| map_error(e, ApplicationError::CredentialReadFailed))?
            .map(|bytes| {
                let bytes = Zeroizing::new(bytes);
                let value = std::str::from_utf8(&bytes)
                    .map_err(|_| ApplicationError::CredentialReadFailed)?;
                Ok(SecretText::new(value.to_owned()))
            })
            .transpose()
    }
    fn save_moodle_feed_secret(&mut self, value: SecretText) -> Result<(), ApplicationError> {
        self.0
            .write(CredentialEntry::Moodle, value.expose().as_bytes().to_vec())
            .map_err(|e| map_error(e, ApplicationError::CredentialWriteFailed))
    }
    fn delete_moodle_feed_secret(&mut self) -> Result<(), ApplicationError> {
        self.0
            .delete(CredentialEntry::Moodle)
            .map_err(|e| map_error(e, ApplicationError::CredentialDeleteFailed))
    }
    fn load_webclass_credentials(&self) -> Result<Option<WebClassCredentials>, ApplicationError> {
        self.0
            .read(CredentialEntry::WebClass)
            .map_err(|e| map_error(e, ApplicationError::CredentialReadFailed))?
            .map(|bytes| {
                let bytes = Zeroizing::new(bytes);
                let stored: StoredWebClass = serde_json::from_slice(&bytes)
                    .map_err(|_| ApplicationError::CredentialReadFailed)?;
                Ok(WebClassCredentials {
                    university_id: stored.university_id,
                    password: stored.password,
                })
            })
            .transpose()
    }
    fn save_webclass_credentials(
        &mut self,
        value: WebClassCredentials,
    ) -> Result<(), ApplicationError> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Encoded<'a> {
            university_id: &'a str,
            password: &'a str,
        }
        let bytes = serde_json::to_vec(&Encoded {
            university_id: value.university_id.expose(),
            password: value.password.expose(),
        })
        .map_err(|_| ApplicationError::CredentialWriteFailed)?;
        self.0
            .write(CredentialEntry::WebClass, bytes)
            .map_err(|e| map_error(e, ApplicationError::CredentialWriteFailed))
    }
    fn delete_webclass_credentials(&mut self) -> Result<(), ApplicationError> {
        self.0
            .delete(CredentialEntry::WebClass)
            .map_err(|e| map_error(e, ApplicationError::CredentialDeleteFailed))
    }
}
