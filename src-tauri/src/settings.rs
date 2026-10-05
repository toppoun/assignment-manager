//! Non-secret, versioned product configuration. No URLs or credential fields.
use std::{
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::error::ApplicationError;

use assignment_manager_application::settings::{MOODLE_SCOPE, Settings, SettingsStore};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NonSecretSettings {
    version: u32,
    moodle_scope: String,
    moodle_export_kind: ExportKind,
    #[serde(default)]
    sync_on_startup: bool,
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
            sync_on_startup: false,
        }
    }
}

impl NonSecretSettings {
    fn validate(&self) -> Result<(), ApplicationError> {
        if self.version != 1
            || self.moodle_scope != MOODLE_SCOPE
            || self.moodle_export_kind != ExportKind::CourseRelated
        {
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

    fn load_config(&self) -> Result<NonSecretSettings, ApplicationError> {
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

    fn write(&self, settings: &NonSecretSettings) -> Result<(), ApplicationError> {
        settings.validate()?;
        // Do not overwrite an existing file that became corrupt after startup.
        self.read()?;
        let parent = self
            .path
            .parent()
            .ok_or(ApplicationError::SettingsWriteFailed)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|_| ApplicationError::SettingsWriteFailed)?;
        serde_json::to_writer_pretty(&mut temporary, settings)
            .map_err(|_| ApplicationError::SettingsWriteFailed)?;
        temporary
            .write_all(b"\n")
            .map_err(|_| ApplicationError::SettingsWriteFailed)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|_| ApplicationError::SettingsWriteFailed)?;
        temporary
            .persist(&self.path)
            .map_err(|_| ApplicationError::SettingsWriteFailed)?;
        Ok(())
    }
}

impl SettingsStore for SettingsFile {
    fn load_or_create(&mut self) -> Result<Settings, ApplicationError> {
        self.load_config().map(|config| Settings {
            sync_on_startup: config.sync_on_startup,
        })
    }
    fn save(&mut self, settings: &Settings) -> Result<(), ApplicationError> {
        self.write(&NonSecretSettings {
            sync_on_startup: settings.sync_on_startup,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_config_defaults_off_and_startup_setting_is_atomic_and_persistent() {
        let root = tempfile::tempdir().unwrap();
        let mut file = SettingsFile::new(root.path()).unwrap();
        let old = r#"{"version":1,"moodleScope":"default.course-related","moodleExportKind":"course_related"}"#;
        fs::write(&file.path, old).unwrap();
        assert!(!file.load_or_create().unwrap().sync_on_startup);
        assert_eq!(fs::read_to_string(&file.path).unwrap(), old);
        file.save(&Settings {
            sync_on_startup: true,
        })
        .unwrap();
        assert!(file.load_or_create().unwrap().sync_on_startup);
        fs::write(&file.path, "broken").unwrap();
        assert_eq!(
            file.save(&Settings::default()),
            Err(ApplicationError::SettingsReadFailed)
        );
        assert_eq!(fs::read_to_string(&file.path).unwrap(), "broken");
    }
    #[test]
    fn non_secret_config_roundtrips_with_stable_scope_and_export_kind() {
        let root = tempfile::tempdir().unwrap();
        let file = SettingsFile::new(&root.path().join("nested")).unwrap();
        let initial = file.load_config().unwrap();
        assert_eq!(initial, file.load_config().unwrap());
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
                file.load_config(),
                Err(ApplicationError::SettingsReadFailed)
            );
            assert_eq!(fs::read_to_string(&file.path).unwrap(), content);
        }
        fs::remove_file(&file.path).unwrap();
        fs::create_dir(&file.path).unwrap();
        assert_eq!(
            file.load_config(),
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
            file.load_config(),
            Err(ApplicationError::SettingsReadFailed | ApplicationError::SettingsWriteFailed)
        ));
    }
}
