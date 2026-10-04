use std::{error::Error, fmt};

use assignment_manager::persistence::PersistenceError;
use serde::{Serialize, Serializer, ser::SerializeStruct};

/// Fixed, public categories. Never retain a cause, path, identity or SQL string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationError {
    DataDirectory,
    DatabaseStartup,
    Storage,
    NotFound,
    InvalidAssignmentId,
    Busy,
    Unavailable,
}

impl ApplicationError {
    fn code(self) -> &'static str {
        match self {
            Self::DataDirectory => "data_directory",
            Self::DatabaseStartup => "database_startup",
            Self::Storage => "storage",
            Self::NotFound => "not_found",
            Self::InvalidAssignmentId => "invalid_assignment_id",
            Self::Busy => "busy",
            Self::Unavailable => "unavailable",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::DataDirectory => "アプリの保存先を準備できませんでした。",
            Self::DatabaseStartup => "保存データを開けませんでした。アプリを再起動してください。",
            Self::Storage => "保存データを読み書きできませんでした。",
            Self::NotFound => "課題が見つかりません。一覧を再読み込みしてください。",
            Self::InvalidAssignmentId => "課題を指定できませんでした。",
            Self::Busy => "処理が混み合っています。少し待って再試行してください。",
            Self::Unavailable => "アプリの処理を実行できませんでした。再起動してください。",
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
        formatter.write_str(self.message())
    }
}

impl Error for ApplicationError {}

impl Serialize for ApplicationError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut object = serializer.serialize_struct("ApplicationError", 2)?;
        object.serialize_field("code", self.code())?;
        object.serialize_field("message", self.message())?;
        object.end()
    }
}

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
    fn errors_serialize_only_fixed_code_and_message_without_source_chains() {
        for error in [
            ApplicationError::DataDirectory,
            ApplicationError::DatabaseStartup,
            ApplicationError::Storage,
            ApplicationError::NotFound,
            ApplicationError::InvalidAssignmentId,
            ApplicationError::Busy,
            ApplicationError::Unavailable,
        ] {
            assert_eq!(
                serde_json::to_value(error).unwrap(),
                serde_json::json!({ "code": error.code(), "message": error.message() })
            );
            assert_eq!(error.to_string(), error.message());
            assert!(error.source().is_none());
        }
    }
}
