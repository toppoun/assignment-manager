//! Platform credential port. Secrets are neither read models nor configuration.
use assignment_manager::moodle_http::{MoodleExportKind, MoodleFeed};
use zeroize::Zeroizing;

use crate::{error::ApplicationError, settings::MOODLE_SCOPE};

/// An owned, short-lived secret. No Debug, Clone or serialization implementation.
///
/// ```compile_fail
/// use assignment_manager_application::credentials::SecretText;
/// let secret = SecretText::new("password".into());
/// let _ = format!("{secret:?}");
/// ```
/// ```compile_fail
/// use assignment_manager_application::credentials::SecretText;
/// let secret = SecretText::new("password".into());
/// let _ = secret.clone();
/// ```
pub struct SecretText(Zeroizing<String>);

impl SecretText {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }
    /// Only credential adapters and the authentication request may expose this.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

pub struct WebClassCredentials {
    pub university_id: SecretText,
    pub password: SecretText,
}

impl WebClassCredentials {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.university_id.expose().trim().is_empty() || self.password.expose().is_empty() {
            return Err(ApplicationError::InvalidCredentials);
        }
        Ok(())
    }
}

pub fn validate_moodle(secret: &SecretText) -> Result<(), ApplicationError> {
    MoodleFeed::new(
        MOODLE_SCOPE,
        MoodleExportKind::CourseRelated,
        secret.expose(),
    )
    .map(|_| ())
    .map_err(|_| ApplicationError::InvalidMoodleFeed)
}

/// Implemented by platform adapters, initialized/accessed on the settings worker.
/// Missing entries return `None`; unavailable/corrupt entries return safe errors.
/// Replacement must preserve the previous entry if writing fails.
pub trait CredentialStore {
    fn load_moodle_feed_secret(&self) -> Result<Option<SecretText>, ApplicationError>;
    fn save_moodle_feed_secret(&mut self, value: SecretText) -> Result<(), ApplicationError>;
    fn delete_moodle_feed_secret(&mut self) -> Result<(), ApplicationError>;
    fn load_webclass_credentials(&self) -> Result<Option<WebClassCredentials>, ApplicationError>;
    fn save_webclass_credentials(
        &mut self,
        value: WebClassCredentials,
    ) -> Result<(), ApplicationError>;
    fn delete_webclass_credentials(&mut self) -> Result<(), ApplicationError>;
}
