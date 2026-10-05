pub use assignment_manager_application::error::ApplicationError;
use serde::{Serialize, Serializer, ser::SerializeStruct};
use std::{error::Error, fmt};

/// Tauri wire adapter: stable code + localized message, no backend causes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandError(pub ApplicationError);
impl From<ApplicationError> for CommandError {
    fn from(value: ApplicationError) -> Self {
        Self(value)
    }
}
impl CommandError {
    fn code(self) -> &'static str {
        self.0.code()
    }

    fn message(self) -> &'static str {
        match self.0 {
            ApplicationError::DataDirectory => "アプリの保存先を準備できませんでした。",
            ApplicationError::DatabaseStartup => {
                "保存データを開けませんでした。アプリを再起動してください。"
            }
            ApplicationError::Storage => "保存データを読み書きできませんでした。",
            ApplicationError::NotFound => "課題が見つかりません。一覧を再読み込みしてください。",
            ApplicationError::InvalidAssignmentId => "課題を指定できませんでした。",
            ApplicationError::InvalidQuery => "一覧の条件を指定できませんでした。",
            ApplicationError::CredentialUnavailable => "OSの認証情報ストアを利用できませんでした。",
            ApplicationError::CredentialReadFailed => "保存済みの認証情報を確認できませんでした。",
            ApplicationError::CredentialWriteFailed => "認証情報を保存できませんでした。",
            ApplicationError::CredentialDeleteFailed => "認証情報を削除できませんでした。",
            ApplicationError::SettingsReadFailed => {
                "設定ファイルを読み込めませんでした。ファイルは変更していません。"
            }
            ApplicationError::SettingsWriteFailed => "設定ファイルを保存できませんでした。",
            ApplicationError::InvalidMoodleFeed => {
                "Moodle URLはユーザー名・passwordを含まないHTTPS URLを入力してください。"
            }
            ApplicationError::InvalidCredentials => "大学IDとpasswordの両方を入力してください。",
            ApplicationError::Busy => "処理が混み合っています。少し待って再試行してください。",
            ApplicationError::Unavailable => {
                "アプリの処理を実行できませんでした。再起動してください。"
            }
        }
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}
impl Error for CommandError {}
impl Serialize for CommandError {
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
            let error = CommandError::from(error);
            assert_eq!(
                serde_json::to_value(error).unwrap(),
                serde_json::json!({ "code": error.code(), "message": error.message() })
            );
            assert_eq!(error.to_string(), error.message());
            assert!(error.source().is_none());
        }
    }
}
