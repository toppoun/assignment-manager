//! The only OS credential boundary. No network clients, SQL, or public secret DTOs.
use std::{collections::HashMap, fmt};

use assignment_manager::moodle_http::{MoodleExportKind, MoodleFeed};
use keyring_core::{Entry, Error as BackendError, api::CredentialStoreApi};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use zeroize::Zeroizing;

use crate::{error::ApplicationError, settings::MOODLE_SCOPE};

/// Deliberately not Serialize or Clone. IPC inputs and loaded secrets redact Debug.
pub struct SecretText(Zeroizing<String>);

impl SecretText {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl<'de> Deserialize<'de> for SecretText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)
            .map(Self::new)
            .map_err(|_| D::Error::custom("invalid secret input"))
    }
}

#[derive(Debug)]
pub struct WebClassCredentials {
    pub university_id: SecretText,
    pub password: SecretText,
}

impl<'de> Deserialize<'de> for WebClassCredentials {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Input {
            university_id: SecretText,
            password: SecretText,
        }
        let input = Input::deserialize(deserializer)
            .map_err(|_| D::Error::custom("invalid WebClass credentials input"))?;
        Ok(Self {
            university_id: input.university_id,
            password: input.password,
        })
    }
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
    // Existing constructor validates HTTPS, host and userinfo without making a request.
    MoodleFeed::new(
        MOODLE_SCOPE,
        MoodleExportKind::CourseRelated,
        secret.expose(),
    )
    .map(|_| ())
    .map_err(|_| ApplicationError::InvalidMoodleFeed)
}

pub trait CredentialStore {
    fn load_moodle_feed_secret(&self) -> Result<Option<SecretText>, ApplicationError>;
    fn save_moodle_feed_secret(&mut self, secret: SecretText) -> Result<(), ApplicationError>;
    fn delete_moodle_feed_secret(&mut self) -> Result<(), ApplicationError>;
    fn load_webclass_credentials(&self) -> Result<Option<WebClassCredentials>, ApplicationError>;
    fn save_webclass_credentials(
        &mut self,
        value: WebClassCredentials,
    ) -> Result<(), ApplicationError>;
    fn delete_webclass_credentials(&mut self) -> Result<(), ApplicationError>;
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Record {
    Moodle,
    WebClass,
}

trait SecretBackend {
    fn read(&self, record: Record) -> Result<Option<Zeroizing<Vec<u8>>>, ApplicationError>;
    fn write(&mut self, record: Record, value: &[u8]) -> Result<(), ApplicationError>;
    fn delete(&mut self, record: Record) -> Result<(), ApplicationError>;
}

pub struct CredentialVault<B> {
    backend: B,
}

impl<B: SecretBackend> CredentialStore for CredentialVault<B> {
    fn load_moodle_feed_secret(&self) -> Result<Option<SecretText>, ApplicationError> {
        self.backend
            .read(Record::Moodle)?
            .map(|bytes| {
                let value = std::str::from_utf8(&bytes)
                    .map_err(|_| ApplicationError::CredentialReadFailed)?;
                let secret = SecretText::new(value.to_owned());
                validate_moodle(&secret).map_err(|_| ApplicationError::CredentialReadFailed)?;
                Ok(secret)
            })
            .transpose()
    }

    fn save_moodle_feed_secret(&mut self, secret: SecretText) -> Result<(), ApplicationError> {
        validate_moodle(&secret)?;
        self.backend
            .write(Record::Moodle, secret.expose().as_bytes())
    }

    fn delete_moodle_feed_secret(&mut self) -> Result<(), ApplicationError> {
        self.backend.delete(Record::Moodle)
    }

    fn load_webclass_credentials(&self) -> Result<Option<WebClassCredentials>, ApplicationError> {
        self.backend
            .read(Record::WebClass)?
            .map(|bytes| {
                let value: WebClassCredentials = serde_json::from_slice(&bytes)
                    .map_err(|_| ApplicationError::CredentialReadFailed)?;
                value
                    .validate()
                    .map_err(|_| ApplicationError::CredentialReadFailed)?;
                Ok(value)
            })
            .transpose()
    }

    fn save_webclass_credentials(
        &mut self,
        value: WebClassCredentials,
    ) -> Result<(), ApplicationError> {
        value.validate()?;
        // One write replaces one logical record. Neither field is credential metadata.
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Encoded<'a> {
            university_id: &'a str,
            password: &'a str,
        }
        let encoded = Zeroizing::new(
            serde_json::to_vec(&Encoded {
                university_id: value.university_id.expose(),
                password: value.password.expose(),
            })
            .map_err(|_| ApplicationError::CredentialWriteFailed)?,
        );
        self.backend.write(Record::WebClass, &encoded)
    }

    fn delete_webclass_credentials(&mut self) -> Result<(), ApplicationError> {
        self.backend.delete(Record::WebClass)
    }
}

pub struct NativeBackend {
    moodle: Entry,
    webclass: Entry,
}

pub type OsCredentialStore = CredentialVault<NativeBackend>;

impl OsCredentialStore {
    /// Called on the dedicated settings worker, with a fixed app-owned service.
    pub fn new(service: &str) -> Result<Self, ApplicationError> {
        #[cfg(windows)]
        let store = windows_native_keyring_store::Store::new();
        #[cfg(target_os = "macos")]
        let store = apple_native_keyring_store::keychain::Store::new();
        #[cfg(not(any(windows, target_os = "macos")))]
        let store: keyring_core::Result<std::sync::Arc<keyring_core::CredentialStore>> =
            Err(BackendError::NoDefaultStore);
        let store = store.map_err(|_| ApplicationError::CredentialUnavailable)?;
        // Windows Local persistence survives restart without credential roaming.
        let modifiers = if cfg!(windows) {
            HashMap::from([("persistence", "Local")])
        } else {
            HashMap::new()
        };
        let build = |account| {
            store
                .build(service, account, Some(&modifiers))
                .map_err(|_| ApplicationError::CredentialUnavailable)
        };
        Ok(Self {
            backend: NativeBackend {
                moodle: build("moodle-feed-v1")?,
                webclass: build("webclass-credentials-v1")?,
            },
        })
    }
}

#[derive(Clone, Copy)]
enum Operation {
    Read,
    Write,
    Delete,
}

fn backend_error(error: BackendError, operation: Operation) -> ApplicationError {
    match error {
        BackendError::NoStorageAccess(_) | BackendError::NoDefaultStore => {
            ApplicationError::CredentialUnavailable
        }
        // Never format/retain library errors: some variants contain raw bytes or strings.
        _ => match operation {
            Operation::Read => ApplicationError::CredentialReadFailed,
            Operation::Write => ApplicationError::CredentialWriteFailed,
            Operation::Delete => ApplicationError::CredentialDeleteFailed,
        },
    }
}

impl NativeBackend {
    fn entry(&self, record: Record) -> &Entry {
        match record {
            Record::Moodle => &self.moodle,
            Record::WebClass => &self.webclass,
        }
    }
}

impl SecretBackend for NativeBackend {
    fn read(&self, record: Record) -> Result<Option<Zeroizing<Vec<u8>>>, ApplicationError> {
        match self.entry(record).get_secret() {
            Ok(value) => Ok(Some(Zeroizing::new(value))),
            Err(BackendError::NoEntry) => Ok(None),
            Err(error) => Err(backend_error(error, Operation::Read)),
        }
    }

    fn write(&mut self, record: Record, value: &[u8]) -> Result<(), ApplicationError> {
        // Direct OS replacement: do not delete the previous entry first.
        self.entry(record)
            .set_secret(value)
            .map_err(|error| backend_error(error, Operation::Write))
    }

    fn delete(&mut self, record: Record) -> Result<(), ApplicationError> {
        match self.entry(record).delete_credential() {
            Ok(()) | Err(BackendError::NoEntry) => Ok(()),
            Err(error) => Err(backend_error(error, Operation::Delete)),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub const URL: &str = "https://example.invalid/calendar?token=PRIVATE_MOODLE_8D1";
    pub const ID: &str = "PRIVATE_UNIVERSITY_ID_8D1";
    pub const PASSWORD: &str = "PRIVATE_PASSWORD_8D1";

    #[derive(Default)]
    pub struct FakeBackend {
        records: HashMap<Record, Zeroizing<Vec<u8>>>,
        pub failure: Option<ApplicationError>,
    }

    impl SecretBackend for FakeBackend {
        fn read(&self, record: Record) -> Result<Option<Zeroizing<Vec<u8>>>, ApplicationError> {
            if let Some(error) = self.failure {
                return Err(error);
            }
            Ok(self.records.get(&record).cloned())
        }
        fn write(&mut self, record: Record, value: &[u8]) -> Result<(), ApplicationError> {
            if let Some(error) = self.failure {
                return Err(error);
            }
            self.records.insert(record, Zeroizing::new(value.to_vec()));
            Ok(())
        }
        fn delete(&mut self, record: Record) -> Result<(), ApplicationError> {
            if let Some(error) = self.failure {
                return Err(error);
            }
            self.records.remove(&record);
            Ok(())
        }
    }

    pub fn fake() -> CredentialVault<FakeBackend> {
        CredentialVault {
            backend: FakeBackend::default(),
        }
    }
    pub fn credentials(id: &str, password: &str) -> WebClassCredentials {
        WebClassCredentials {
            university_id: SecretText::new(id.into()),
            password: SecretText::new(password.into()),
        }
    }

    #[test]
    fn fake_roundtrip_replace_and_idempotent_remove_use_two_records() {
        let mut vault = fake();
        assert!(vault.load_moodle_feed_secret().unwrap().is_none());
        assert!(vault.load_webclass_credentials().unwrap().is_none());
        vault
            .save_moodle_feed_secret(SecretText::new(URL.into()))
            .unwrap();
        vault
            .save_webclass_credentials(credentials(ID, PASSWORD))
            .unwrap();
        assert_eq!(vault.backend.records.len(), 2);
        assert_eq!(
            vault.load_moodle_feed_secret().unwrap().unwrap().expose(),
            URL
        );
        let loaded = vault.load_webclass_credentials().unwrap().unwrap();
        assert_eq!(loaded.university_id.expose(), ID);
        assert_eq!(loaded.password.expose(), PASSWORD);
        let replacement = "https://example.invalid/changed?token=ROTATED";
        vault
            .save_moodle_feed_secret(SecretText::new(replacement.into()))
            .unwrap();
        vault
            .save_webclass_credentials(credentials("REPLACEMENT_ID", "REPLACEMENT_PASSWORD"))
            .unwrap();
        assert_eq!(
            vault.load_moodle_feed_secret().unwrap().unwrap().expose(),
            replacement
        );
        assert_eq!(
            vault
                .load_webclass_credentials()
                .unwrap()
                .unwrap()
                .university_id
                .expose(),
            "REPLACEMENT_ID"
        );
        assert_eq!(MOODLE_SCOPE, "default.course-related");
        for _ in 0..2 {
            vault.delete_moodle_feed_secret().unwrap();
            vault.delete_webclass_credentials().unwrap();
        }
        assert!(vault.load_moodle_feed_secret().unwrap().is_none());
        assert!(vault.load_webclass_credentials().unwrap().is_none());
    }

    #[test]
    fn failed_replacement_and_validation_preserve_previous_secrets() {
        let mut vault = fake();
        vault
            .save_moodle_feed_secret(SecretText::new(URL.into()))
            .unwrap();
        vault
            .save_webclass_credentials(credentials(ID, PASSWORD))
            .unwrap();
        vault.backend.failure = Some(ApplicationError::CredentialWriteFailed);
        assert_eq!(
            vault.save_moodle_feed_secret(SecretText::new(URL.into())),
            Err(ApplicationError::CredentialWriteFailed)
        );
        assert_eq!(
            vault.save_webclass_credentials(credentials("new", "new")),
            Err(ApplicationError::CredentialWriteFailed)
        );
        vault.backend.failure = None;
        for invalid in [
            "",
            "not a url",
            "http://example.invalid",
            "https://user:pass@example.invalid",
        ] {
            assert_eq!(
                vault.save_moodle_feed_secret(SecretText::new(invalid.into())),
                Err(ApplicationError::InvalidMoodleFeed)
            );
        }
        for (id, password) in [("", "pass"), ("   ", "pass"), ("id", "")] {
            assert_eq!(
                vault.save_webclass_credentials(credentials(id, password)),
                Err(ApplicationError::InvalidCredentials)
            );
        }
        assert_eq!(
            vault.load_moodle_feed_secret().unwrap().unwrap().expose(),
            URL
        );
        assert_eq!(
            vault
                .load_webclass_credentials()
                .unwrap()
                .unwrap()
                .password
                .expose(),
            PASSWORD
        );
    }

    #[test]
    fn corrupt_or_partial_records_are_read_errors_not_missing() {
        let mut vault = fake();
        for bad in [
            b"{}".as_slice(),
            br#"{"universityId":"id"}"#,
            br#"{"universityId":"id","password":""}"#,
        ] {
            vault
                .backend
                .records
                .insert(Record::WebClass, Zeroizing::new(bad.to_vec()));
            assert!(matches!(
                vault.load_webclass_credentials(),
                Err(ApplicationError::CredentialReadFailed)
            ));
        }
        vault
            .backend
            .records
            .insert(Record::Moodle, Zeroizing::new(vec![255]));
        assert!(matches!(
            vault.load_moodle_feed_secret(),
            Err(ApplicationError::CredentialReadFailed)
        ));
    }

    #[test]
    fn debug_and_backend_errors_never_expose_literal_secrets() {
        let debug = format!(
            "{:?} {:?}",
            SecretText::new(URL.into()),
            credentials(ID, PASSWORD)
        );
        for marker in [URL, ID, PASSWORD] {
            assert!(!debug.contains(marker));
        }
        for (operation, expected) in [
            (Operation::Read, ApplicationError::CredentialReadFailed),
            (Operation::Write, ApplicationError::CredentialWriteFailed),
            (Operation::Delete, ApplicationError::CredentialDeleteFailed),
        ] {
            let error = backend_error(
                BackendError::Invalid(ID.into(), format!("{URL} {PASSWORD}")),
                operation,
            );
            assert_eq!(error, expected);
            let public = format!(
                "{error} {error:?} {}",
                serde_json::to_string(&error).unwrap()
            );
            for marker in [URL, ID, PASSWORD] {
                assert!(!public.contains(marker));
            }
        }
        assert_eq!(
            backend_error(
                BackendError::NoStorageAccess(Box::new(std::io::Error::other(PASSWORD))),
                Operation::Read
            ),
            ApplicationError::CredentialUnavailable
        );
        assert_eq!(
            backend_error(
                BackendError::BadEncoding(PASSWORD.as_bytes().to_vec()),
                Operation::Read
            ),
            ApplicationError::CredentialReadFailed
        );
    }

    #[test]
    fn malformed_ipc_credentials_do_not_echo_secret_values_or_field_names() {
        let bad = serde_json::json!({"universityId":ID,"password":PASSWORD, (URL): PASSWORD});
        assert!(bad.get(URL).is_some());
        let error = serde_json::from_value::<WebClassCredentials>(bad)
            .unwrap_err()
            .to_string();
        for marker in [URL, ID, PASSWORD] {
            assert!(!error.contains(marker));
        }
        let secret_error = serde_json::from_value::<SecretText>(serde_json::json!({(PASSWORD):ID}))
            .unwrap_err()
            .to_string();
        for marker in [URL, ID, PASSWORD] {
            assert!(!secret_error.contains(marker));
        }
    }
}
