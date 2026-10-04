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
    fn code(self) -> &'static str {
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

    fn message(self) -> &'static str {
        match self {
            Self::DataDirectory => "アプリの保存先を準備できませんでした。",
            Self::DatabaseStartup => "保存データを開けませんでした。アプリを再起動してください。",
            Self::Storage => "保存データを読み書きできませんでした。",
            Self::NotFound => "課題が見つかりません。一覧を再読み込みしてください。",
            Self::InvalidAssignmentId => "課題を指定できませんでした。",
            Self::InvalidQuery => "一覧の条件を指定できませんでした。",
            Self::CredentialUnavailable => "OSの認証情報ストアを利用できませんでした。",
            Self::CredentialReadFailed => "保存済みの認証情報を確認できませんでした。",
            Self::CredentialWriteFailed => "認証情報を保存できませんでした。",
            Self::CredentialDeleteFailed => "認証情報を削除できませんでした。",
            Self::SettingsReadFailed => {
                "設定ファイルを読み込めませんでした。ファイルは変更していません。"
            }
            Self::SettingsWriteFailed => "設定ファイルを保存できませんでした。",
            Self::InvalidMoodleFeed => {
                "Moodle URLはユーザー名・passwordを含まないHTTPS URLを入力してください。"
            }
            Self::InvalidCredentials => "大学IDとpasswordの両方を入力してください。",
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
            assert_eq!(
                serde_json::to_value(error).unwrap(),
                serde_json::json!({ "code": error.code(), "message": error.message() })
            );
            assert_eq!(error.to_string(), error.message());
            assert!(error.source().is_none());
        }
    }
}
