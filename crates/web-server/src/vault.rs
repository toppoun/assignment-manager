//! Owner-bound AES-256-GCM CredentialStore, used exclusively by SettingsWorker.
use crate::{
    WebError,
    auth::UserId,
    control::{ControlDb, SharedControl},
    keys::{KeyProvider, VaultKey},
    poc::PocDatabase,
};
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{AeadInOut, KeyInit},
};
use assignment_manager_application::{
    credentials::{CredentialStore, SecretText, WebClassCredentials, validate_moodle},
    error::ApplicationError,
};
use rand_core::{OsRng, RngCore};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::sync::Arc;
use zeroize::Zeroizing;

const VERSION: u8 = 1;
const HEADER: usize = 1 + 16 + 12;
const CHECK: &[u8] = b"assignment-manager-web-vault-key-check-v1";
pub const URL_LIMIT: usize = 2048;
pub const ID_LIMIT: usize = 256;
pub const PASSWORD_LIMIT: usize = 1024;

#[derive(Clone, Copy)]
enum Kind {
    Moodle,
    Webclass,
    Binding,
}
impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Moodle => "moodle",
            Self::Webclass => "webclass",
            Self::Binding => "binding",
        }
    }
}
fn aad(owner: &UserId, kind: Kind, key: &VaultKey) -> Vec<u8> {
    let mut bytes = b"assignment-manager.web.vault\0".to_vec();
    bytes.push(VERSION);
    bytes.extend_from_slice(&key.id);
    bytes.extend_from_slice(&(owner.0.len() as u64).to_be_bytes());
    bytes.extend_from_slice(owner.0.as_bytes());
    bytes.extend_from_slice(kind.name().as_bytes());
    bytes
}
fn decrypt(
    key: &VaultKey,
    owner: &UserId,
    kind: Kind,
    envelope: &[u8],
) -> Result<Zeroizing<Vec<u8>>, WebError> {
    if envelope.len() < HEADER + 16
        || envelope.len() > 4096
        || envelope[0] != VERSION
        || envelope[1..17] != key.id
    {
        return Err(WebError::VaultUnavailable);
    }
    let cipher =
        Aes256Gcm::new_from_slice(key.bytes.as_ref()).map_err(|_| WebError::VaultUnavailable)?;
    // In-place decrypt ensures even a failed authentication's temporary buffer
    // is zeroized. The maintained cipher's zeroize feature clears key state.
    let mut plaintext = Zeroizing::new(envelope[HEADER..].to_vec());
    let nonce: [u8; 12] = envelope[17..HEADER]
        .try_into()
        .map_err(|_| WebError::VaultUnavailable)?;
    cipher
        .decrypt_in_place(&Nonce::from(nonce), &aad(owner, kind, key), &mut *plaintext)
        .map_err(|_| WebError::VaultUnavailable)?;
    Ok(plaintext)
}
fn encrypt(
    db: &Connection,
    key: &VaultKey,
    owner: &UserId,
    kind: Kind,
    plaintext: &[u8],
) -> Result<Vec<u8>, WebError> {
    let mut nonce = [0; 12];
    // Persisted ledger survives replacement/deletion/restart. Random collision
    // or exhausted per-key budget fails without ever reusing a committed nonce.
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM vault_nonces WHERE key_id=?1",
            [&key.id[..]],
            |r| r.get(0),
        )
        .map_err(|_| WebError::VaultUnavailable)?;
    if count >= 1_000_000 {
        return Err(WebError::VaultUnavailable);
    }
    let mut reserved = false;
    for _ in 0..4 {
        OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| WebError::VaultUnavailable)?;
        let changed = db
            .execute(
                "INSERT INTO vault_nonces (key_id,nonce) VALUES (?1,?2) ON CONFLICT DO NOTHING",
                params![&key.id[..], &nonce[..]],
            )
            .map_err(|_| WebError::VaultUnavailable)?;
        if changed == 1 {
            reserved = true;
            break;
        }
    }
    if !reserved {
        return Err(WebError::VaultUnavailable);
    }
    let cipher =
        Aes256Gcm::new_from_slice(key.bytes.as_ref()).map_err(|_| WebError::VaultUnavailable)?;
    let mut ciphertext = Zeroizing::new(plaintext.to_vec());
    cipher
        .encrypt_in_place(
            &Nonce::from(nonce),
            &aad(owner, kind, key),
            &mut *ciphertext,
        )
        .map_err(|_| WebError::VaultUnavailable)?;
    let mut result = Vec::with_capacity(HEADER + ciphertext.len());
    result.push(VERSION);
    result.extend_from_slice(&key.id);
    result.extend_from_slice(&nonce);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}
fn owner(db: &Connection) -> Result<UserId, WebError> {
    db.query_row("SELECT user_id FROM accounts WHERE singleton=1", [], |r| {
        Ok(UserId(r.get(0)?))
    })
    .map_err(|_| WebError::SetupRequired)
}
fn verify_binding(db: &Connection, key: &VaultKey, user: &UserId) -> Result<(), WebError> {
    let (stored_owner, envelope): (String, Vec<u8>) = db
        .query_row(
            "SELECT owner,envelope FROM vault_binding WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| WebError::VaultUnavailable)?;
    if stored_owner != user.0 || *decrypt(key, user, Kind::Binding, &envelope)? != CHECK {
        return Err(WebError::VaultUnavailable);
    }
    Ok(())
}
fn bind(db: &mut Connection, provider: &dyn KeyProvider) -> Result<(), WebError> {
    let user = owner(db)?;
    let key = provider.load()?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| WebError::VaultUnavailable)?;
    let exists: bool = tx
        .query_row("SELECT EXISTS(SELECT 1 FROM vault_binding)", [], |r| {
            r.get(0)
        })
        .map_err(|_| WebError::VaultUnavailable)?;
    if exists {
        return verify_binding(&tx, &key, &user);
    }
    let records: i64 = tx
        .query_row(
            "SELECT (SELECT count(*) FROM web_credentials)+(SELECT count(*) FROM vault_nonces)",
            [],
            |r| r.get(0),
        )
        .map_err(|_| WebError::VaultUnavailable)?;
    if records != 0 {
        return Err(WebError::VaultUnavailable);
    }
    let envelope = encrypt(&tx, &key, &user, Kind::Binding, CHECK)?;
    tx.execute(
        "INSERT INTO vault_binding VALUES (1,?1,?2)",
        params![user.0, envelope],
    )
    .map_err(|_| WebError::VaultUnavailable)?;
    tx.commit().map_err(|_| WebError::VaultUnavailable)
}
/// Explicit management operation/test setup. Existing bindings are verified and
/// never overwritten. Missing/wrong keys cannot reinitialize an existing vault.
pub fn initialize_with_provider(
    database: &PocDatabase,
    provider: &dyn KeyProvider,
) -> Result<(), WebError> {
    let mut control = ControlDb::open(database)?;
    bind(&mut control.connection, provider)
}
pub fn initialize_local_key(database: &PocDatabase) -> Result<(), WebError> {
    let mut control = ControlDb::open(database)?;
    #[cfg(windows)]
    {
        let provider = crate::keys::WindowsKeyProvider::new(database)?;
        let occupied: bool = control.connection.query_row("SELECT EXISTS(SELECT 1 FROM vault_binding) OR EXISTS(SELECT 1 FROM web_credentials) OR EXISTS(SELECT 1 FROM vault_nonces)", [], |r| r.get(0)).map_err(|_| WebError::VaultUnavailable)?;
        if !occupied {
            owner(&control.connection)?;
            provider.create_if_missing()?;
        }
        bind(&mut control.connection, &provider)
    }
    #[cfg(not(windows))]
    {
        let _ = &mut control;
        Err(WebError::VaultUnavailable)
    }
}

pub(crate) struct WebVault {
    control: SharedControl,
    owner: UserId,
    provider: Arc<dyn KeyProvider>,
}
impl WebVault {
    pub fn new(
        control: SharedControl,
        owner: UserId,
        provider: Arc<dyn KeyProvider>,
    ) -> Result<Self, ApplicationError> {
        let vault = Self {
            control,
            owner,
            provider,
        };
        vault
            .access(|_, _| Ok(()))
            .map_err(|_| ApplicationError::CredentialUnavailable)?;
        Ok(vault)
    }
    fn access<T>(
        &self,
        f: impl FnOnce(&mut Connection, &VaultKey) -> Result<T, WebError>,
    ) -> Result<T, WebError> {
        let key = self.provider.load()?;
        self.control.with(|db| {
            if owner(db)? != self.owner {
                return Err(WebError::Forbidden);
            }
            verify_binding(db, &key, &self.owner)?;
            f(db, &key)
        })
    }
    fn read(
        db: &Connection,
        key: &VaultKey,
        user: &UserId,
        kind: Kind,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, WebError> {
        let bytes: Option<Vec<u8>> = db
            .query_row(
                "SELECT envelope FROM web_credentials WHERE owner=?1 AND kind=?2",
                params![user.0, kind.name()],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| WebError::VaultUnavailable)?;
        bytes
            .map(|v| {
                let plaintext = decrypt(key, user, kind, &v)?;
                validate_payload(kind, &plaintext)?;
                Ok(plaintext)
            })
            .transpose()
    }
    fn write(&self, kind: Kind, value: Option<&[u8]>) -> Result<(), WebError> {
        self.access(|db,key| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_| WebError::VaultUnavailable)?;
            // Corruption is an error, including on replace/delete. Repair is an
            // explicit future operator procedure, never silent credential loss.
            Self::read(&tx,key,&self.owner,kind)?;
            if let Some(value) = value {
                validate_payload(kind, value)?;
                let envelope = encrypt(&tx,key,&self.owner,kind,value)?;
                tx.execute("INSERT INTO web_credentials (owner,kind,envelope) VALUES (?1,?2,?3) ON CONFLICT(owner,kind) DO UPDATE SET envelope=excluded.envelope",params![self.owner.0,kind.name(),envelope]).map_err(|_| WebError::VaultUnavailable)?;
            } else {
                tx.execute("DELETE FROM web_credentials WHERE owner=?1 AND kind=?2",params![self.owner.0,kind.name()]).map_err(|_| WebError::VaultUnavailable)?;
            }
            tx.commit().map_err(|_| WebError::VaultUnavailable)
        })
    }
}
fn moodle(bytes: &[u8]) -> Result<SecretText, WebError> {
    let value = std::str::from_utf8(bytes).map_err(|_| WebError::VaultUnavailable)?;
    if value.len() > URL_LIMIT {
        return Err(WebError::VaultUnavailable);
    }
    let secret = SecretText::new(value.to_owned());
    validate_moodle(&secret).map_err(|_| WebError::VaultUnavailable)?;
    Ok(secret)
}
fn webclass(bytes: &[u8]) -> Result<WebClassCredentials, WebError> {
    if bytes.len() < 4 {
        return Err(WebError::VaultUnavailable);
    }
    let length = u32::from_be_bytes(
        bytes[..4]
            .try_into()
            .map_err(|_| WebError::VaultUnavailable)?,
    ) as usize;
    if length > ID_LIMIT || length > bytes.len() - 4 || bytes.len() - 4 - length > PASSWORD_LIMIT {
        return Err(WebError::VaultUnavailable);
    }
    let text = |value| {
        std::str::from_utf8(value)
            .map(|v| SecretText::new(v.to_owned()))
            .map_err(|_| WebError::VaultUnavailable)
    };
    let value = WebClassCredentials {
        university_id: text(&bytes[4..4 + length])?,
        password: text(&bytes[4 + length..])?,
    };
    value.validate().map_err(|_| WebError::VaultUnavailable)?;
    Ok(value)
}
fn validate_payload(kind: Kind, bytes: &[u8]) -> Result<(), WebError> {
    match kind {
        Kind::Moodle => {
            moodle(bytes)?;
        }
        Kind::Webclass => {
            webclass(bytes)?;
        }
        Kind::Binding => return Err(WebError::VaultUnavailable),
    }
    Ok(())
}
impl CredentialStore for WebVault {
    fn load_moodle_feed_secret(&self) -> Result<Option<SecretText>, ApplicationError> {
        self.access(|db, key| {
            Self::read(db, key, &self.owner, Kind::Moodle)?
                .map(|v| moodle(&v))
                .transpose()
        })
        .map_err(|_| ApplicationError::CredentialReadFailed)
    }
    fn load_webclass_credentials(&self) -> Result<Option<WebClassCredentials>, ApplicationError> {
        self.access(|db, key| {
            Self::read(db, key, &self.owner, Kind::Webclass)?
                .map(|v| webclass(&v))
                .transpose()
        })
        .map_err(|_| ApplicationError::CredentialReadFailed)
    }
    fn save_moodle_feed_secret(&mut self, value: SecretText) -> Result<(), ApplicationError> {
        validate_moodle(&value)?;
        self.write(Kind::Moodle, Some(value.expose().as_bytes()))
            .map_err(|_| ApplicationError::CredentialWriteFailed)
    }
    fn save_webclass_credentials(
        &mut self,
        value: WebClassCredentials,
    ) -> Result<(), ApplicationError> {
        value.validate()?;
        let mut bytes = Zeroizing::new(Vec::new());
        bytes.extend_from_slice(&(value.university_id.expose().len() as u32).to_be_bytes());
        bytes.extend_from_slice(value.university_id.expose().as_bytes());
        bytes.extend_from_slice(value.password.expose().as_bytes());
        self.write(Kind::Webclass, Some(&bytes))
            .map_err(|_| ApplicationError::CredentialWriteFailed)
    }
    fn delete_moodle_feed_secret(&mut self) -> Result<(), ApplicationError> {
        self.write(Kind::Moodle, None)
            .map_err(|_| ApplicationError::CredentialDeleteFailed)
    }
    fn delete_webclass_credentials(&mut self) -> Result<(), ApplicationError> {
        self.write(Kind::Webclass, None)
            .map_err(|_| ApplicationError::CredentialDeleteFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::MemoryKeyProvider;
    const URL: &str = "https://example.invalid/calendar?token=SYNTHETIC_VAULT_3B";
    const ID: &str = "synthetic-vault-id-3b";
    const PASSWORD: &str = "synthetic-vault-password-3b";
    fn credentials() -> WebClassCredentials {
        WebClassCredentials {
            university_id: SecretText::new(ID.into()),
            password: SecretText::new(PASSWORD.into()),
        }
    }
    fn fixture() -> (tempfile::TempDir, PocDatabase, Arc<MemoryKeyProvider>) {
        let root = tempfile::tempdir().unwrap();
        let db = PocDatabase::prepare(&root.path().join("poc")).unwrap();
        crate::auth::bootstrap_admin(&db, "owner", "synthetic-web-password-3b".to_owned().into())
            .unwrap();
        let keys = Arc::new(MemoryKeyProvider::random().unwrap());
        initialize_with_provider(&db, keys.as_ref()).unwrap();
        (root, db, keys)
    }
    fn open(db: &PocDatabase, keys: Arc<dyn KeyProvider>) -> WebVault {
        let control = ControlDb::open(db).unwrap();
        let user = owner(&control.connection).unwrap();
        WebVault::new(SharedControl::new(control), user, keys).unwrap()
    }
    fn record(vault: &WebVault, kind: Kind) -> Vec<u8> {
        vault
            .control
            .with(|db| {
                Ok(db
                    .query_row(
                        "SELECT envelope FROM web_credentials WHERE kind=?1",
                        [kind.name()],
                        |r| r.get(0),
                    )
                    .unwrap())
            })
            .unwrap()
    }
    #[test]
    fn roundtrip_random_nonce_and_plaintext_absent_from_database_and_journal() {
        let (_root, db, keys) = fixture();
        let mut vault = open(&db, keys);
        vault
            .save_moodle_feed_secret(SecretText::new(URL.into()))
            .unwrap();
        let first = record(&vault, Kind::Moodle);
        vault
            .save_moodle_feed_secret(SecretText::new(URL.into()))
            .unwrap();
        let second = record(&vault, Kind::Moodle);
        assert_ne!(&first[17..HEADER], &second[17..HEADER]);
        assert_ne!(first, second);
        vault.save_webclass_credentials(credentials()).unwrap();
        assert_eq!(
            vault.load_moodle_feed_secret().unwrap().unwrap().expose(),
            URL
        );
        let loaded = vault.load_webclass_credentials().unwrap().unwrap();
        assert_eq!(loaded.university_id.expose(), ID);
        assert_eq!(loaded.password.expose(), PASSWORD);
        for entry in std::fs::read_dir(db.path().parent().unwrap()).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file()
                && entry.path().extension().is_none_or(|v| v != "lock")
            {
                let bytes = std::fs::read(entry.path()).unwrap();
                for secret in [URL, ID, PASSWORD] {
                    assert!(!bytes.windows(secret.len()).any(|v| v == secret.as_bytes()));
                }
            }
        }
        vault
            .control
            .with(|db| {
                assert_eq!(
                    db.query_row("SELECT count(*) FROM vault_nonces", [], |r| r
                        .get::<_, i64>(0))
                        .unwrap(),
                    4
                );
                assert!(
                    db.execute(
                        "INSERT INTO vault_nonces SELECT * FROM vault_nonces LIMIT 1",
                        []
                    )
                    .is_err()
                );
                Ok(())
            })
            .unwrap();
    }
    #[test]
    fn aad_rejects_owner_kind_version_key_and_tag_substitutions() {
        let (_root, db, keys) = fixture();
        let key = keys.load().unwrap();
        let mut vault = open(&db, keys);
        vault
            .save_moodle_feed_secret(SecretText::new(URL.into()))
            .unwrap();
        let bytes = record(&vault, Kind::Moodle);
        assert!(
            decrypt(
                &key,
                &UserId("different-owner".into()),
                Kind::Moodle,
                &bytes
            )
            .is_err()
        );
        assert!(decrypt(&key, &vault.owner, Kind::Webclass, &bytes).is_err());
        for index in [0, 1, 17, HEADER, bytes.len() - 1] {
            let mut corrupt = bytes.clone();
            corrupt[index] ^= 0x80;
            assert!(decrypt(&key, &vault.owner, Kind::Moodle, &corrupt).is_err());
        }
        for length in [0, 1, 16, HEADER, HEADER + 15] {
            assert!(decrypt(&key, &vault.owner, Kind::Moodle, &bytes[..length]).is_err());
        }
        let wrong = VaultKey {
            id: key.id,
            bytes: Zeroizing::new([123; 32]),
        };
        assert!(decrypt(&wrong, &vault.owner, Kind::Moodle, &bytes).is_err());
    }
    #[test]
    fn persistence_replace_failure_delete_and_restart_preserve_contract() {
        let (_root, db, keys) = fixture();
        {
            let mut vault = open(&db, keys.clone());
            vault
                .save_moodle_feed_secret(SecretText::new(URL.into()))
                .unwrap();
            vault.save_webclass_credentials(credentials()).unwrap();
        }
        {
            let mut vault = open(&db, keys.clone());
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
            vault
                .save_moodle_feed_secret(SecretText::new(format!("{URL}-replaced")))
                .unwrap();
            vault
                .save_webclass_credentials(WebClassCredentials {
                    university_id: SecretText::new("synthetic-new-id".into()),
                    password: SecretText::new("synthetic-new-password".into()),
                })
                .unwrap();
        }
        {
            let mut vault = open(&db, keys.clone());
            assert_eq!(
                vault.load_moodle_feed_secret().unwrap().unwrap().expose(),
                format!("{URL}-replaced")
            );
            assert_eq!(
                vault
                    .load_webclass_credentials()
                    .unwrap()
                    .unwrap()
                    .university_id
                    .expose(),
                "synthetic-new-id"
            );
            let before = record(&vault, Kind::Webclass);
            vault.control.with(|db| { db.execute_batch("CREATE TRIGGER reject_replace BEFORE UPDATE ON web_credentials BEGIN SELECT RAISE(ABORT,'private failure'); END;").unwrap(); Ok(()) }).unwrap();
            let error = vault.save_webclass_credentials(credentials()).unwrap_err();
            assert_eq!(error, ApplicationError::CredentialWriteFailed);
            assert_eq!(record(&vault, Kind::Webclass), before);
            assert_eq!(error.to_string(), "credential_write_failed");
            assert!(!format!("{error:?}").contains(PASSWORD));
            vault
                .control
                .with(|db| {
                    db.execute_batch("DROP TRIGGER reject_replace;").unwrap();
                    Ok(())
                })
                .unwrap();
            for _ in 0..2 {
                vault.delete_moodle_feed_secret().unwrap();
                vault.delete_webclass_credentials().unwrap();
            }
        }
        let vault = open(&db, keys);
        assert!(vault.load_moodle_feed_secret().unwrap().is_none());
        assert!(vault.load_webclass_credentials().unwrap().is_none());
    }
    #[test]
    fn corrupt_record_is_never_missing_or_silently_overwritten_or_deleted() {
        let (_root, db, keys) = fixture();
        let mut vault = open(&db, keys);
        vault
            .save_moodle_feed_secret(SecretText::new(URL.into()))
            .unwrap();
        let good = record(&vault, Kind::Moodle);
        for corrupt in [
            vec![],
            vec![1; HEADER],
            {
                let mut c = good.clone();
                c[0] = 99;
                c
            },
            {
                let mut c = good.clone();
                c[HEADER] ^= 1;
                c
            },
        ] {
            vault
                .control
                .with(|db| {
                    db.execute("UPDATE web_credentials SET envelope=?1", [&corrupt])
                        .unwrap();
                    Ok(())
                })
                .unwrap();
            assert!(vault.load_moodle_feed_secret().is_err());
            assert!(
                vault
                    .save_moodle_feed_secret(SecretText::new(URL.into()))
                    .is_err()
            );
            assert!(vault.delete_moodle_feed_secret().is_err());
            assert_eq!(record(&vault, Kind::Moodle), corrupt);
        }
        // Even authenticated plaintext with invalid structure is corrupt.
        vault
            .control
            .with(|db| {
                let key = vault.provider.load()?;
                let invalid = encrypt(db, &key, &vault.owner, Kind::Webclass, b"partial-id")?;
                db.execute(
                    "INSERT INTO web_credentials VALUES (?1,'webclass',?2)",
                    params![vault.owner.0, invalid],
                )
                .unwrap();
                Ok(())
            })
            .unwrap();
        assert!(vault.load_webclass_credentials().is_err());
        assert!(vault.save_webclass_credentials(credentials()).is_err());
    }
    #[test]
    fn missing_wrong_key_fail_closed_without_reset_including_an_empty_vault() {
        let (_root, db, keys) = fixture();
        assert!(initialize_with_provider(&db, &MemoryKeyProvider::missing()).is_err());
        assert!(initialize_with_provider(&db, &MemoryKeyProvider::random().unwrap()).is_err());
        {
            let mut vault = open(&db, keys.clone());
            vault.save_webclass_credentials(credentials()).unwrap();
        }
        let bytes = std::fs::read(crate::auth::control_path(&db)).unwrap();
        for wrong in [
            MemoryKeyProvider::missing(),
            MemoryKeyProvider::random().unwrap(),
        ] {
            assert!(initialize_with_provider(&db, &wrong).is_err());
            let control = ControlDb::open(&db).unwrap();
            let user = owner(&control.connection).unwrap();
            assert!(WebVault::new(SharedControl::new(control), user, Arc::new(wrong)).is_err());
        }
        assert_eq!(
            std::fs::read(crate::auth::control_path(&db)).unwrap(),
            bytes
        );
        let vault = open(&db, keys);
        assert!(vault.load_webclass_credentials().unwrap().is_some());
    }
    #[test]
    fn storage_owner_change_and_ciphertext_kind_swap_are_rejected() {
        let (_root, db, keys) = fixture();
        let mut vault = open(&db, keys);
        vault
            .save_moodle_feed_secret(SecretText::new(URL.into()))
            .unwrap();
        vault.save_webclass_credentials(credentials()).unwrap();
        let moodle = record(&vault, Kind::Moodle);
        let webclass = record(&vault, Kind::Webclass);
        vault
            .control
            .with(|db| {
                db.execute(
                    "UPDATE web_credentials SET envelope=?1 WHERE kind='webclass'",
                    [&moodle],
                )
                .unwrap();
                db.execute(
                    "UPDATE web_credentials SET envelope=?1 WHERE kind='moodle'",
                    [&webclass],
                )
                .unwrap();
                Ok(())
            })
            .unwrap();
        assert!(vault.load_webclass_credentials().is_err());
        assert!(vault.load_moodle_feed_secret().is_err());
        let alien = WebVault {
            control: vault.control.clone(),
            owner: UserId("different-owner".into()),
            provider: vault.provider.clone(),
        };
        assert!(alien.load_moodle_feed_secret().is_err());
        assert!(alien.load_webclass_credentials().is_err());
    }
}
