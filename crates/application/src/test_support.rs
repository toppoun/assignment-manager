use crate::{
    credentials::{CredentialStore, SecretText, WebClassCredentials},
    error::ApplicationError,
    settings::{Settings, SettingsStore},
};
use zeroize::Zeroizing;

pub const URL: &str = "https://example.invalid/calendar?token=PRIVATE_MOODLE_8D1";
pub const ID: &str = "PRIVATE_UNIVERSITY_ID_8D1";
pub const PASSWORD: &str = "PRIVATE_PASSWORD_8D1";

#[derive(Default)]
pub struct FakeCredentials {
    moodle: Option<Zeroizing<String>>,
    webclass: Option<(Zeroizing<String>, Zeroizing<String>)>,
    pub failure: Option<ApplicationError>,
}
pub fn fake() -> FakeCredentials {
    FakeCredentials::default()
}
pub fn credentials(id: &str, password: &str) -> WebClassCredentials {
    WebClassCredentials {
        university_id: SecretText::new(id.into()),
        password: SecretText::new(password.into()),
    }
}
impl CredentialStore for FakeCredentials {
    fn load_moodle_feed_secret(&self) -> Result<Option<SecretText>, ApplicationError> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        Ok(self.moodle.as_ref().map(|s| SecretText::new(s.to_string())))
    }
    fn save_moodle_feed_secret(&mut self, value: SecretText) -> Result<(), ApplicationError> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        self.moodle = Some(Zeroizing::new(value.expose().into()));
        Ok(())
    }
    fn delete_moodle_feed_secret(&mut self) -> Result<(), ApplicationError> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        self.moodle = None;
        Ok(())
    }
    fn load_webclass_credentials(&self) -> Result<Option<WebClassCredentials>, ApplicationError> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        Ok(self
            .webclass
            .as_ref()
            .map(|(id, pass)| credentials(id, pass)))
    }
    fn save_webclass_credentials(
        &mut self,
        value: WebClassCredentials,
    ) -> Result<(), ApplicationError> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        self.webclass = Some((
            Zeroizing::new(value.university_id.expose().into()),
            Zeroizing::new(value.password.expose().into()),
        ));
        Ok(())
    }
    fn delete_webclass_credentials(&mut self) -> Result<(), ApplicationError> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        self.webclass = None;
        Ok(())
    }
}

#[derive(Default)]
pub struct MemorySettings {
    pub value: Settings,
    pub failure: Option<ApplicationError>,
}
impl SettingsStore for MemorySettings {
    fn load_or_create(&mut self) -> Result<Settings, ApplicationError> {
        Ok(self.value.clone())
    }
    fn save(&mut self, settings: &Settings) -> Result<(), ApplicationError> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        self.value = settings.clone();
        Ok(())
    }
}

pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(future)
}
