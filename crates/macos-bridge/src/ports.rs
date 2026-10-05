//! Private construction adapters, replaced by native platform ports in Phase 10C.
//! They do not read files, OS credentials, tokens or environment configuration.
use assignment_manager_application::{
    credentials::{CredentialStore, SecretText, WebClassCredentials},
    error::ApplicationError,
    settings::{Settings, SettingsStore},
};

pub(crate) struct PocSettings;
impl SettingsStore for PocSettings {
    fn load_or_create(&mut self) -> Result<Settings, ApplicationError> {
        Ok(Settings::default())
    }
    fn save(&mut self, _: &Settings) -> Result<(), ApplicationError> {
        Err(ApplicationError::SettingsWriteFailed)
    }
}

pub(crate) struct NoCredentials;
impl CredentialStore for NoCredentials {
    fn load_moodle_feed_secret(&self) -> Result<Option<SecretText>, ApplicationError> {
        Ok(None)
    }
    fn save_moodle_feed_secret(&mut self, _: SecretText) -> Result<(), ApplicationError> {
        Err(ApplicationError::CredentialUnavailable)
    }
    fn delete_moodle_feed_secret(&mut self) -> Result<(), ApplicationError> {
        Err(ApplicationError::CredentialUnavailable)
    }
    fn load_webclass_credentials(&self) -> Result<Option<WebClassCredentials>, ApplicationError> {
        Ok(None)
    }
    fn save_webclass_credentials(
        &mut self,
        _: WebClassCredentials,
    ) -> Result<(), ApplicationError> {
        Err(ApplicationError::CredentialUnavailable)
    }
    fn delete_webclass_credentials(&mut self) -> Result<(), ApplicationError> {
        Err(ApplicationError::CredentialUnavailable)
    }
}
