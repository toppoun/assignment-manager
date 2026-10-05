use std::{error::Error, fmt};

use assignment_manager::persistence::PersistenceError;

/// Fixed, public categories. Never retain a cause, path, identity or SQL string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationError {
    DataDirectory,
    DatabaseStartup,
    Storage,
    NotFound,
    InvalidAssignmentId,
    InvalidQuery,
    CredentialUnavailable,
    CredentialReadFailed,
    CredentialWriteFailed,
    CredentialDeleteFailed,
    SettingsReadFailed,
    SettingsWriteFailed,
    InvalidMoodleFeed,
    InvalidCredentials,
    Busy,
    Unavailable,
}

impl ApplicationError {
    pub fn code(self) -> &'static str {
        match self {
            Self::DataDirectory => "data_directory",
            Self::DatabaseStartup => "database_startup",
            Self::Storage => "storage",
            Self::NotFound => "not_found",
            Self::InvalidAssignmentId => "invalid_assignment_id",
            Self::InvalidQuery => "invalid_query",
            Self::CredentialUnavailable => "credential_unavailable",
            Self::CredentialReadFailed => "credential_read_failed",
            Self::CredentialWriteFailed => "credential_write_failed",
            Self::CredentialDeleteFailed => "credential_delete_failed",
            Self::SettingsReadFailed => "settings_read_failed",
            Self::SettingsWriteFailed => "settings_write_failed",
            Self::InvalidMoodleFeed => "invalid_moodle_feed",
            Self::InvalidCredentials => "invalid_credentials",
            Self::Busy => "busy",
            Self::Unavailable => "unavailable",
        }
    }
}

impl From<PersistenceError> for ApplicationError {
    fn from(error: PersistenceError) -> Self {
        match error {
            PersistenceError::NotFound => Self::NotFound,
            _ => Self::Storage,
        }
    }
}

impl fmt::Display for ApplicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl Error for ApplicationError {}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_core_error_crosses_only_the_fixed_application_boundary() {
        use PersistenceError::*;
        for cause in [
            Open,
            Configure,
            Migration,
            UnsupportedSchemaVersion,
            Database,
            InvalidStoredData,
            Constraint,
            IdGenerationFailed,
            InvalidExternalIdentity,
            ObservationValidation,
            CourseResolutionValidation,
            EntityAlreadyBound,
            ExternalIdentityAlreadyBound,
        ] {
            assert_eq!(ApplicationError::from(cause), ApplicationError::Storage);
        }
        assert_eq!(ApplicationError::from(NotFound), ApplicationError::NotFound);
    }

    #[test]
    fn application_errors_have_only_fixed_codes_and_no_cause_or_localized_payload() {
        for error in [
            ApplicationError::DataDirectory,
            ApplicationError::DatabaseStartup,
            ApplicationError::Storage,
            ApplicationError::NotFound,
            ApplicationError::InvalidAssignmentId,
            ApplicationError::InvalidQuery,
            ApplicationError::CredentialUnavailable,
            ApplicationError::CredentialReadFailed,
            ApplicationError::CredentialWriteFailed,
            ApplicationError::CredentialDeleteFailed,
            ApplicationError::SettingsReadFailed,
            ApplicationError::SettingsWriteFailed,
            ApplicationError::InvalidMoodleFeed,
            ApplicationError::InvalidCredentials,
            ApplicationError::Busy,
            ApplicationError::Unavailable,
        ] {
            assert_eq!(error.to_string(), error.code());
            assert!(
                error
                    .code()
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_')
            );
            assert!(error.source().is_none());
        }
    }
}
