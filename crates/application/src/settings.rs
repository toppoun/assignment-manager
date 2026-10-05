//! Application settings policy, independent of JSON/UserDefaults and OS APIs.
use crate::{
    credentials::{CredentialStore, SecretText, WebClassCredentials, validate_moodle},
    error::ApplicationError,
};

pub const MOODLE_SCOPE: &str = "default.course-related";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub sync_on_startup: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ID, MemorySettings, PASSWORD, URL, credentials, fake};

    #[test]
    fn settings_status_and_successful_revision_changes_do_not_expose_secrets() {
        let mut service = SettingsService::new(Settings::default(), fake());
        assert_eq!(
            service.status().unwrap(),
            SettingsStatus {
                moodle_configured: false,
                webclass_configured: false,
                sync_on_startup: false
            }
        );
        service.save_moodle(SecretText::new(URL.into())).unwrap();
        service.save_webclass(credentials(ID, PASSWORD)).unwrap();
        assert_eq!(service.load_webclass().unwrap().0, 1);
        assert!(service.status().unwrap().moodle_configured);
        assert!(service.status().unwrap().webclass_configured);
        service.credentials.failure = Some(ApplicationError::CredentialWriteFailed);
        assert_eq!(
            service.save_webclass(credentials("new", "new")),
            Err(ApplicationError::CredentialWriteFailed)
        );
        service.credentials.failure = None;
        assert_eq!(service.load_webclass().unwrap().0, 1);
        assert_eq!(
            service.save_webclass(credentials("", PASSWORD)),
            Err(ApplicationError::InvalidCredentials)
        );
        assert_eq!(service.load_webclass().unwrap().0, 1);
        service.credentials.failure = Some(ApplicationError::CredentialDeleteFailed);
        assert_eq!(
            service.remove_webclass(),
            Err(ApplicationError::CredentialDeleteFailed)
        );
        service.credentials.failure = None;
        assert_eq!(service.load_webclass().unwrap().0, 1);
        service.remove_webclass().unwrap();
        assert_eq!(service.load_webclass().unwrap().0, 2);
        assert!(!service.status().unwrap().webclass_configured);
        service.remove_moodle().unwrap();
        assert!(!service.status().unwrap().moodle_configured);
        let safe = format!("{:?} {:?}", service.config, service.status().unwrap());
        for marker in [URL, ID, PASSWORD] {
            assert!(!safe.contains(marker));
        }
    }

    #[test]
    fn settings_state_changes_only_after_platform_persistence_succeeds() {
        let mut storage = MemorySettings::default();
        let mut service = SettingsService::new(storage.load_or_create().unwrap(), fake());
        service.set_startup_sync(&mut storage, true).unwrap();
        assert!(service.status().unwrap().sync_on_startup);
        assert!(storage.load_or_create().unwrap().sync_on_startup);
        storage.failure = Some(ApplicationError::SettingsWriteFailed);
        assert_eq!(
            service.set_startup_sync(&mut storage, false),
            Err(ApplicationError::SettingsWriteFailed)
        );
        assert!(service.status().unwrap().sync_on_startup);
        assert!(storage.load_or_create().unwrap().sync_on_startup);
    }

    #[test]
    fn validation_is_shared_even_when_the_platform_port_does_not_validate() {
        let mut service = SettingsService::new(Settings::default(), fake());
        for value in [
            "",
            "not a url",
            "http://example.invalid",
            "https://user:pass@example.invalid",
        ] {
            assert_eq!(
                service.save_moodle(SecretText::new(value.into())),
                Err(ApplicationError::InvalidMoodleFeed)
            );
        }
        assert!(!service.status().unwrap().moodle_configured);
        for (id, pass) in [("", "pass"), ("  ", "pass"), ("id", "")] {
            assert_eq!(
                service.save_webclass(credentials(id, pass)),
                Err(ApplicationError::InvalidCredentials)
            );
        }
        service
            .credentials
            .save_moodle_feed_secret(SecretText::new("corrupt secret".into()))
            .unwrap();
        assert_eq!(
            service.status(),
            Err(ApplicationError::CredentialReadFailed)
        );
    }
}

/// Platform persistence port. Versioning/encoding belong to the implementation.
pub trait SettingsStore {
    fn load_or_create(&mut self) -> Result<Settings, ApplicationError>;
    fn save(&mut self, settings: &Settings) -> Result<(), ApplicationError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsStatus {
    pub moodle_configured: bool,
    pub webclass_configured: bool,
    pub sync_on_startup: bool,
}

pub(crate) struct SettingsService<C> {
    config: Settings,
    credentials: C,
    webclass_revision: u64,
}

impl<C: CredentialStore> SettingsService<C> {
    pub fn new(config: Settings, credentials: C) -> Self {
        Self {
            config,
            credentials,
            webclass_revision: 0,
        }
    }
    pub fn status(&self) -> Result<SettingsStatus, ApplicationError> {
        Ok(SettingsStatus {
            moodle_configured: self.load_moodle()?.is_some(),
            webclass_configured: self.load_webclass()?.1.is_some(),
            sync_on_startup: self.config.sync_on_startup,
        })
    }
    pub fn save_moodle(&mut self, value: SecretText) -> Result<(), ApplicationError> {
        validate_moodle(&value)?;
        self.credentials.save_moodle_feed_secret(value)
    }
    pub fn remove_moodle(&mut self) -> Result<(), ApplicationError> {
        self.credentials.delete_moodle_feed_secret()
    }
    pub fn save_webclass(&mut self, value: WebClassCredentials) -> Result<(), ApplicationError> {
        value.validate()?;
        self.credentials.save_webclass_credentials(value)?;
        self.webclass_revision = self.webclass_revision.wrapping_add(1);
        Ok(())
    }
    pub fn remove_webclass(&mut self) -> Result<(), ApplicationError> {
        self.credentials.delete_webclass_credentials()?;
        self.webclass_revision = self.webclass_revision.wrapping_add(1);
        Ok(())
    }
    pub fn load_moodle(&self) -> Result<Option<SecretText>, ApplicationError> {
        let value = self.credentials.load_moodle_feed_secret()?;
        if let Some(value) = &value {
            validate_moodle(value).map_err(|_| ApplicationError::CredentialReadFailed)?;
        }
        Ok(value)
    }
    pub fn load_webclass(&self) -> Result<(u64, Option<WebClassCredentials>), ApplicationError> {
        let value = self.credentials.load_webclass_credentials()?;
        if let Some(value) = &value {
            value
                .validate()
                .map_err(|_| ApplicationError::CredentialReadFailed)?;
        }
        Ok((self.webclass_revision, value))
    }
    pub fn set_startup_sync(
        &mut self,
        storage: &mut impl SettingsStore,
        enabled: bool,
    ) -> Result<(), ApplicationError> {
        let next = Settings {
            sync_on_startup: enabled,
        };
        storage.save(&next)?;
        self.config = next;
        Ok(())
    }
}
