use assignment_manager_application::error::ApplicationError;

/// Fixed categories only: no message, cause, path, URL, SQL or external identity payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum BridgeError {
    #[error("data_directory")]
    DataDirectory,
    #[error("database_startup")]
    DatabaseStartup,
    #[error("storage")]
    Storage,
    #[error("not_found")]
    NotFound,
    #[error("invalid_assignment_id")]
    InvalidAssignmentId,
    #[error("invalid_query")]
    InvalidQuery,
    #[error("credential_unavailable")]
    CredentialUnavailable,
    #[error("credential_read_failed")]
    CredentialReadFailed,
    #[error("credential_write_failed")]
    CredentialWriteFailed,
    #[error("credential_delete_failed")]
    CredentialDeleteFailed,
    #[error("settings_read_failed")]
    SettingsReadFailed,
    #[error("settings_write_failed")]
    SettingsWriteFailed,
    #[error("invalid_moodle_feed")]
    InvalidMoodleFeed,
    #[error("invalid_credentials")]
    InvalidCredentials,
    #[error("busy")]
    Busy,
    #[error("unavailable")]
    Unavailable,
}

impl From<ApplicationError> for BridgeError {
    fn from(value: ApplicationError) -> Self {
        match value {
            ApplicationError::DataDirectory => Self::DataDirectory,
            ApplicationError::DatabaseStartup => Self::DatabaseStartup,
            ApplicationError::Storage => Self::Storage,
            ApplicationError::NotFound => Self::NotFound,
            ApplicationError::InvalidAssignmentId => Self::InvalidAssignmentId,
            ApplicationError::InvalidQuery => Self::InvalidQuery,
            ApplicationError::CredentialUnavailable => Self::CredentialUnavailable,
            ApplicationError::CredentialReadFailed => Self::CredentialReadFailed,
            ApplicationError::CredentialWriteFailed => Self::CredentialWriteFailed,
            ApplicationError::CredentialDeleteFailed => Self::CredentialDeleteFailed,
            ApplicationError::SettingsReadFailed => Self::SettingsReadFailed,
            ApplicationError::SettingsWriteFailed => Self::SettingsWriteFailed,
            ApplicationError::InvalidMoodleFeed => Self::InvalidMoodleFeed,
            ApplicationError::InvalidCredentials => Self::InvalidCredentials,
            ApplicationError::Busy => Self::Busy,
            ApplicationError::Unavailable => Self::Unavailable,
        }
    }
}
