//! PoC-owned synthetic storage and inert platform ports. No native stores/network.
use crate::WebError;
use assignment_manager::{
    domain::{CompletionState, CourseId, Deadline, SubmissionTarget},
    import::OriginKind,
    persistence::{ManualAssignmentInput, SqliteStore},
    sync::WebClassCredentialProvider,
};
#[cfg(test)]
use assignment_manager_application::credentials::CredentialStore;
use assignment_manager_application::{
    SyncBackend, SyncCounts, SyncFailure,
    credentials::{SecretText, WebClassCredentials},
    error::ApplicationError,
    settings::{Settings, SettingsStore},
};
use chrono::{DateTime, NaiveDate, Utc};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const MARKER: &str = "assignment-manager-web-poc-v1\n";
const MARKER_FILE: &str = "SYNTHETIC-ONLY";
const DATABASE: &str = "assignment-manager.db";
pub const PRIVATE_MARKER: &str = "SYNTHETIC_PRIVATE_WEB1";
pub const SOURCE_MARKER: &str = "SYNTHETIC_SOURCE_WEB1";

/// Created only by the PoC factory. No HTTP-controlled paths are accepted.
pub struct PocDatabase {
    path: PathBuf,
}
impl PocDatabase {
    pub fn default_directory() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("web-poc")
    }
    /// New directories seed once. Existing directories require our exact marker
    /// and regular database; incomplete/foreign directories fail without writes.
    /// Tests may supply their own isolated temporary parent; the binary cannot.
    pub fn prepare(directory: &Path) -> Result<Self, WebError> {
        if !directory.is_absolute() {
            return Err(WebError::Internal);
        }
        reject_links(directory)?;
        let parent = directory.parent().ok_or(WebError::Internal)?;
        fs::create_dir_all(parent).map_err(|_| WebError::Internal)?;
        reject_links(parent)?;
        match fs::create_dir(directory) {
            Ok(()) => {
                // Publish the database only after every fixture operation succeeds.
                let temporary =
                    tempfile::NamedTempFile::new_in(directory).map_err(|_| WebError::Internal)?;
                seed(temporary.path())?;
                temporary
                    .as_file()
                    .sync_all()
                    .map_err(|_| WebError::Internal)?;
                temporary
                    .persist_noclobber(directory.join(DATABASE))
                    .map_err(|_| WebError::Internal)?;
                let mut marker = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(directory.join(MARKER_FILE))
                    .map_err(|_| WebError::Internal)?;
                marker
                    .write_all(MARKER.as_bytes())
                    .map_err(|_| WebError::Internal)?;
                marker.sync_all().map_err(|_| WebError::Internal)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                regular_file(&directory.join(MARKER_FILE))?;
                regular_file(&directory.join(DATABASE))?;
                let marker = directory.join(MARKER_FILE);
                if fs::metadata(&marker).map_err(|_| WebError::Internal)?.len()
                    != MARKER.len() as u64
                    || fs::read(marker).map_err(|_| WebError::Internal)? != MARKER.as_bytes()
                {
                    return Err(WebError::Internal);
                }
            }
            Err(_) => return Err(WebError::Internal),
        }
        Ok(Self {
            path: directory.join(DATABASE),
        })
    }
    /// Local setup/test access only. It is never part of a transport DTO.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) fn reject_links(path: &Path) -> Result<(), WebError> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(WebError::Internal);
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if metadata.file_attributes() & 0x400 != 0 {
                        return Err(WebError::Internal);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(WebError::Internal),
        }
    }
    Ok(())
}
fn regular_file(path: &Path) -> Result<(), WebError> {
    reject_links(path)?;
    if !fs::metadata(path)
        .map_err(|_| WebError::Internal)?
        .is_file()
    {
        return Err(WebError::Internal);
    }
    Ok(())
}

fn instant(value: &str) -> Result<Deadline, WebError> {
    value
        .parse::<DateTime<Utc>>()
        .map(Deadline::DateTime)
        .map_err(|_| WebError::Internal)
}
fn date(day: u32) -> Result<Deadline, WebError> {
    NaiveDate::from_ymd_opt(2026, 10, day)
        .map(Deadline::DateOnly)
        .ok_or(WebError::Internal)
}

fn seed(path: &Path) -> Result<(), WebError> {
    let mut store = SqliteStore::open(path).map_err(|_| WebError::Internal)?;
    let moodle = store
        .create_course_with_source(
            "合成Course",
            vec![],
            OriginKind::Moodle,
            "poc.course-related",
            SOURCE_MARKER,
        )
        .map_err(|_| WebError::Internal)?;
    let webclass = store
        .create_course_with_source(
            "合成Course",
            vec![],
            OriginKind::WebClass,
            "default",
            SOURCE_MARKER,
        )
        .map_err(|_| WebError::Internal)?;
    // Dates are intentionally fixed so runs never depend on a real university feed.
    let records: [(&str, Option<CourseId>, Option<Deadline>, bool); 11] = [
        (
            "あ・日時ちょうど",
            Some(moodle.id.clone()),
            Some(instant("2026-10-21T15:00:00.123456789Z")?),
            false,
        ),
        (
            "い・日付のみ",
            Some(moodle.id.clone()),
            Some(date(22)?),
            false,
        ),
        (
            "う・期限切れ",
            Some(webclass.id.clone()),
            Some(instant("2026-10-21T14:59:59.999999999Z")?),
            false,
        ),
        (
            "え・7日後",
            Some(webclass.id.clone()),
            Some(instant("2026-10-28T15:00:00.123456789Z")?),
            false,
        ),
        ("お・期限なし", None, None, false),
        (
            "か・完了済み",
            Some(webclass.id.clone()),
            Some(date(21)?),
            true,
        ),
        ("き・日付7日後", None, Some(date(29)?), false),
        (
            "く・期間外",
            Some(moodle.id.clone()),
            Some(instant("2026-10-28T15:00:00.123456790Z")?),
            false,
        ),
        (
            "け・今日の最後",
            Some(moodle.id.clone()),
            Some(instant("2026-10-22T14:59:59.999999999Z")?),
            false,
        ),
        (
            "こ・翌日開始",
            Some(webclass.id.clone()),
            Some(instant("2026-10-22T15:00:00Z")?),
            false,
        ),
        ("さ・日付期限切れ", None, Some(date(21)?), false),
    ];
    for (index, (title, course_id, deadline, completed)) in records.into_iter().enumerate() {
        let submission = match course_id.as_ref() {
            Some(id) if *id == moodle.id => SubmissionTarget::Moodle,
            Some(_) => SubmissionTarget::WebClass,
            None => SubmissionTarget::Unknown,
        };
        let assignment = store
            .create_manual_assignment(ManualAssignmentInput {
                title: title.into(),
                deadline,
                course_id,
                submission,
                submission_url: (index == 0)
                    .then(|| format!("https://example.invalid/submit?token={PRIVATE_MARKER}")),
            })
            .map_err(|_| WebError::Internal)?;
        if completed {
            store
                .set_completion(&assignment.id, CompletionState::Completed)
                .map_err(|_| WebError::Internal)?;
        }
    }
    Ok(())
}

pub(crate) struct MemorySettings;
impl SettingsStore for MemorySettings {
    fn load_or_create(&mut self) -> Result<Settings, ApplicationError> {
        Ok(Settings::default())
    }
    fn save(&mut self, _: &Settings) -> Result<(), ApplicationError> {
        Err(ApplicationError::SettingsWriteFailed)
    }
}
#[cfg(test)]
pub(crate) struct NoCredentials;
#[cfg(test)]
impl CredentialStore for NoCredentials {
    fn load_moodle_feed_secret(&self) -> Result<Option<SecretText>, ApplicationError> {
        Ok(None)
    }
    fn load_webclass_credentials(&self) -> Result<Option<WebClassCredentials>, ApplicationError> {
        Ok(None)
    }
    fn save_moodle_feed_secret(&mut self, _: SecretText) -> Result<(), ApplicationError> {
        Err(ApplicationError::CredentialWriteFailed)
    }
    fn save_webclass_credentials(
        &mut self,
        _: WebClassCredentials,
    ) -> Result<(), ApplicationError> {
        Err(ApplicationError::CredentialWriteFailed)
    }
    fn delete_moodle_feed_secret(&mut self) -> Result<(), ApplicationError> {
        Err(ApplicationError::CredentialDeleteFailed)
    }
    fn delete_webclass_credentials(&mut self) -> Result<(), ApplicationError> {
        Err(ApplicationError::CredentialDeleteFailed)
    }
}
pub(crate) struct DisabledNetworkBackend;
impl SyncBackend for DisabledNetworkBackend {
    type Session = ();
    fn moodle(&mut self, _: SecretText, _: &mut SqliteStore) -> Result<SyncCounts, SyncFailure> {
        Err(SyncFailure::Unexpected)
    }
    fn login(&mut self, _: WebClassCredentials) -> Result<(), SyncFailure> {
        Err(SyncFailure::Unexpected)
    }
    fn webclass(
        &mut self,
        _: &mut (),
        _: &mut impl WebClassCredentialProvider,
        _: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure> {
        Err(SyncFailure::Unexpected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inert_ports_and_backend_cannot_send_network_requests_or_acquire_credentials() {
        struct NeverSupply;
        impl WebClassCredentialProvider for NeverSupply {
            fn with_credentials<R>(
                &mut self,
                _: impl FnOnce(assignment_manager::webclass_auth::HoseiCredentials<'_>) -> R,
            ) -> Option<R> {
                panic!("disabled backend must never acquire credentials")
            }
        }
        let tripwire = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        tripwire.set_nonblocking(true).unwrap();
        let mut backend = DisabledNetworkBackend;
        let mut store = SqliteStore::open(":memory:").unwrap();
        let url = format!(
            "http://{}/?token={PRIVATE_MARKER}",
            tripwire.local_addr().unwrap()
        );
        assert_eq!(
            backend.moodle(SecretText::new(url), &mut store),
            Err(SyncFailure::Unexpected)
        );
        assert_eq!(
            backend.login(WebClassCredentials {
                university_id: SecretText::new("synthetic-id".into()),
                password: SecretText::new(PRIVATE_MARKER.into()),
            }),
            Err(SyncFailure::Unexpected)
        );
        assert_eq!(
            backend.webclass(&mut (), &mut NeverSupply, &mut store),
            Err(SyncFailure::Unexpected)
        );
        assert_eq!(
            tripwire.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let mut credentials = NoCredentials;
        assert!(credentials.load_moodle_feed_secret().unwrap().is_none());
        assert!(credentials.load_webclass_credentials().unwrap().is_none());
        assert_eq!(
            credentials.save_moodle_feed_secret(SecretText::new(PRIVATE_MARKER.into())),
            Err(ApplicationError::CredentialWriteFailed)
        );
        assert!(!MemorySettings.load_or_create().unwrap().sync_on_startup);
    }
}
