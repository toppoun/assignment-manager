//! Non-secret, versioned product configuration. No URLs or credential fields.
use std::{
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    credential_store::{CredentialStore, SecretText, WebClassCredentials},
    error::ApplicationError,
};

pub const MOODLE_SCOPE: &str = "default.course-related";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NonSecretSettings {
    version: u32,
    moodle_scope: String,
    moodle_export_kind: ExportKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExportKind {
    CourseRelated,
}

impl Default for NonSecretSettings {
    fn default() -> Self {
        Self {
            version: 1,
            moodle_scope: MOODLE_SCOPE.into(),
            moodle_export_kind: ExportKind::CourseRelated,
        }
    }
}

impl NonSecretSettings {
    fn validate(&self) -> Result<(), ApplicationError> {
        if self != &Self::default() {
            return Err(ApplicationError::SettingsReadFailed);
        }
        Ok(())
    }
}

pub struct SettingsFile {
    path: PathBuf,
}

impl SettingsFile {
    pub fn new(directory: &Path) -> Result<Self, ApplicationError> {
        if !directory.is_absolute() {
            return Err(ApplicationError::SettingsReadFailed);
        }
        Ok(Self {
            path: directory.join("settings.json"),
        })
    }

    fn read(&self) -> Result<Option<NonSecretSettings>, ApplicationError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(ApplicationError::SettingsReadFailed),
        };
        let settings: NonSecretSettings =
            serde_json::from_slice(&bytes).map_err(|_| ApplicationError::SettingsReadFailed)?;
        settings.validate()?;
        Ok(Some(settings))
    }

    pub fn load_or_create(&self) -> Result<NonSecretSettings, ApplicationError> {
        if let Some(settings) = self.read()? {
            return Ok(settings);
        }
        let settings = NonSecretSettings::default();
        let parent = self
            .path
            .parent()
            .ok_or(ApplicationError::SettingsWriteFailed)?;
        fs::create_dir_all(parent).map_err(|_| ApplicationError::SettingsWriteFailed)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|_| ApplicationError::SettingsWriteFailed)?;
        serde_json::to_writer_pretty(&mut temporary, &settings)
            .map_err(|_| ApplicationError::SettingsWriteFailed)?;
        temporary
            .write_all(b"\n")
            .map_err(|_| ApplicationError::SettingsWriteFailed)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|_| ApplicationError::SettingsWriteFailed)?;
        // Same-directory atomic creation, never overwrites a concurrent/broken file.
        match temporary.persist_noclobber(&self.path) {
            Ok(_) => Ok(settings),
            Err(error) if error.error.kind() == ErrorKind::AlreadyExists => {
                self.read()?.ok_or(ApplicationError::SettingsReadFailed)
            }
            Err(_) => Err(ApplicationError::SettingsWriteFailed),
        }
    }
}

/// Only these flags cross IPC. Loaded secrets are immediately dropped after inspection.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SettingsStatusDto {
    pub moodle_configured: bool,
    pub webclass_configured: bool,
}

pub struct SettingsService<C> {
    config: NonSecretSettings,
    credentials: C,
}

impl<C: CredentialStore> SettingsService<C> {
    pub fn new(config: NonSecretSettings, credentials: C) -> Self {
        Self {
            config,
            credentials,
        }
    }

    pub fn status(&self) -> Result<SettingsStatusDto, ApplicationError> {
        self.config.validate()?;
        Ok(SettingsStatusDto {
            moodle_configured: self.credentials.load_moodle_feed_secret()?.is_some(),
            webclass_configured: self.credentials.load_webclass_credentials()?.is_some(),
        })
    }

    pub fn save_moodle(&mut self, secret: SecretText) -> Result<(), ApplicationError> {
        self.credentials.save_moodle_feed_secret(secret)
    }
    pub fn remove_moodle(&mut self) -> Result<(), ApplicationError> {
        self.credentials.delete_moodle_feed_secret()
    }
    pub fn save_webclass(&mut self, value: WebClassCredentials) -> Result<(), ApplicationError> {
        self.credentials.save_webclass_credentials(value)
    }
    pub fn remove_webclass(&mut self) -> Result<(), ApplicationError> {
        self.credentials.delete_webclass_credentials()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential_store::tests::{ID, PASSWORD, URL, credentials, fake};

    #[test]
    fn non_secret_config_roundtrips_with_stable_scope_and_export_kind() {
        let root = tempfile::tempdir().unwrap();
        let file = SettingsFile::new(&root.path().join("nested")).unwrap();
        let initial = file.load_or_create().unwrap();
        assert_eq!(initial, file.load_or_create().unwrap());
        assert_eq!(initial.moodle_scope, MOODLE_SCOPE);
        assert_eq!(initial.moodle_export_kind, ExportKind::CourseRelated);
        assert_eq!(
            fs::read_dir(file.path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn malformed_unknown_or_changed_identity_config_is_preserved() {
        let root = tempfile::tempdir().unwrap();
        let file = SettingsFile::new(root.path()).unwrap();
        for content in [
            "broken",
            "{}",
            r#"{"version":99,"moodleScope":"default.course-related","moodleExportKind":"course_related"}"#,
            r#"{"version":1,"moodleScope":"token-secret.course-related","moodleExportKind":"course_related"}"#,
            r#"{"version":1,"moodleScope":"default.course-related","moodleExportKind":"course_related","url":"secret"}"#,
        ] {
            fs::write(&file.path, content).unwrap();
            assert_eq!(
                file.load_or_create(),
                Err(ApplicationError::SettingsReadFailed)
            );
            assert_eq!(fs::read_to_string(&file.path).unwrap(), content);
        }
        fs::remove_file(&file.path).unwrap();
        fs::create_dir(&file.path).unwrap();
        assert_eq!(
            file.load_or_create(),
            Err(ApplicationError::SettingsReadFailed)
        );
        assert!(matches!(
            SettingsFile::new(Path::new("relative")),
            Err(ApplicationError::SettingsReadFailed)
        ));
    }

    #[test]
    fn config_directory_failure_is_fixed_and_does_not_create_a_config() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("file");
        fs::write(&parent, "not a directory").unwrap();
        let file = SettingsFile::new(&parent.join("nested")).unwrap();
        assert!(matches!(
            file.load_or_create(),
            Err(ApplicationError::SettingsReadFailed | ApplicationError::SettingsWriteFailed)
        ));
    }

    #[test]
    fn literal_secrets_never_reach_config_sqlite_status_errors_or_debug() {
        let root = tempfile::tempdir().unwrap();
        let file = SettingsFile::new(root.path()).unwrap();
        let config = file.load_or_create().unwrap();
        let mut service = SettingsService::new(config.clone(), fake());
        assert_eq!(
            service.status().unwrap(),
            SettingsStatusDto {
                moodle_configured: false,
                webclass_configured: false
            }
        );
        let db_path = root.path().join("assignment-manager.db");
        let db = assignment_manager::persistence::SqliteStore::open(&db_path).unwrap();
        service.save_moodle(SecretText::new(URL.into())).unwrap();
        service.save_webclass(credentials(ID, PASSWORD)).unwrap();
        let dto = service.status().unwrap();
        assert_eq!(
            serde_json::to_value(&dto).unwrap(),
            serde_json::json!({"moodleConfigured":true,"webclassConfigured":true})
        );
        drop(db);
        let representations = [
            fs::read(&file.path).unwrap(),
            fs::read(db_path).unwrap(),
            serde_json::to_vec(&dto).unwrap(),
            format!("{config:?} {dto:?} {:?}", credentials(ID, PASSWORD)).into_bytes(),
        ];
        for bytes in representations {
            for marker in [URL, ID, PASSWORD] {
                assert!(
                    !bytes
                        .windows(marker.len())
                        .any(|window| window == marker.as_bytes())
                );
            }
        }
        service.remove_moodle().unwrap();
        assert_eq!(
            service.status().unwrap(),
            SettingsStatusDto {
                moodle_configured: false,
                webclass_configured: true
            }
        );
        service.remove_webclass().unwrap();
        assert!(!service.status().unwrap().webclass_configured);
        assert_eq!(config, file.load_or_create().unwrap());
    }
}
