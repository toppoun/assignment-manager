//! Key custody is separate from ciphertext persistence. No implicit key creation.
use crate::{WebError, poc::PocDatabase};
use rand_core::{OsRng, RngCore};
use std::sync::Mutex;
use zeroize::Zeroizing;

/// No Debug/Clone/serialization: only a key provider and the vault see raw keys.
pub struct VaultKey {
    pub(crate) id: [u8; 16],
    pub(crate) bytes: Zeroizing<[u8; 32]>,
}
impl VaultKey {
    /// Server-side adapter entry point for a future secret mount/KMS provider.
    /// The caller owns custody; neither part may come from a browser request.
    pub fn from_parts(id: [u8; 16], bytes: Zeroizing<[u8; 32]>) -> Self {
        Self { id, bytes }
    }
    fn generate() -> Result<Self, WebError> {
        let mut key = Self {
            id: [0; 16],
            bytes: Zeroizing::new([0; 32]),
        };
        OsRng
            .try_fill_bytes(&mut key.id)
            .map_err(|_| WebError::Unavailable)?;
        OsRng
            .try_fill_bytes(key.bytes.as_mut())
            .map_err(|_| WebError::Unavailable)?;
        Ok(key)
    }
}

/// Replace with an operator secret mount/KMS adapter before Linux/VPS deployment.
/// A provider must fail on missing keys; loading must never generate a replacement.
pub trait KeyProvider: Send + Sync + 'static {
    fn load(&self) -> Result<VaultKey, WebError>;
}

/// Isolated synthetic test seam; never selected by the executable.
pub struct MemoryKeyProvider(Mutex<Option<VaultKey>>);
impl MemoryKeyProvider {
    pub fn random() -> Result<Self, WebError> {
        Ok(Self(Mutex::new(Some(VaultKey::generate()?))))
    }
    pub fn missing() -> Self {
        Self(Mutex::new(None))
    }
}
impl KeyProvider for MemoryKeyProvider {
    fn load(&self) -> Result<VaultKey, WebError> {
        let guard = self.0.lock().map_err(|_| WebError::Unavailable)?;
        let key = guard.as_ref().ok_or(WebError::VaultUnavailable)?;
        Ok(VaultKey {
            id: key.id,
            bytes: Zeroizing::new(*key.bytes),
        })
    }
}

#[cfg(windows)]
pub struct WindowsKeyProvider {
    entry: keyring_core::Entry,
}
#[cfg(windows)]
impl WindowsKeyProvider {
    pub fn new(database: &PocDatabase) -> Result<Self, WebError> {
        use keyring_core::api::CredentialStoreApi;
        use sha2::{Digest, Sha256};
        use std::collections::HashMap;
        let path =
            std::fs::canonicalize(database.path().parent().ok_or(WebError::VaultUnavailable)?)
                .map_err(|_| WebError::VaultUnavailable)?
                .join("web-control.db");
        // Installation identity only: never a university ID, password or token URL.
        let hash = Sha256::digest(path.as_os_str().as_encoded_bytes());
        let account: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        let store =
            windows_native_keyring_store::Store::new().map_err(|_| WebError::VaultUnavailable)?;
        let entry = store
            .build(
                "org.assignmentmanager.web.vault.keys.v1",
                &account,
                Some(&HashMap::from([("persistence", "Local")])),
            )
            .map_err(|_| WebError::VaultUnavailable)?;
        Ok(Self { entry })
    }
    fn existing(&self) -> Result<Option<VaultKey>, WebError> {
        let raw = match self.entry.get_secret() {
            Ok(raw) => Zeroizing::new(raw),
            Err(keyring_core::Error::NoEntry) => return Ok(None),
            Err(_) => return Err(WebError::VaultUnavailable),
        };
        if raw.len() != 49 || raw[0] != 1 {
            return Err(WebError::VaultUnavailable);
        }
        let mut key = VaultKey {
            id: [0; 16],
            bytes: Zeroizing::new([0; 32]),
        };
        key.id.copy_from_slice(&raw[1..17]);
        key.bytes.copy_from_slice(&raw[17..]);
        Ok(Some(key))
    }
    /// Called only under the control DB's exclusive process lock and only when
    /// there is no binding or ciphertext. An orphan key is reused, never replaced.
    pub(crate) fn create_if_missing(&self) -> Result<(), WebError> {
        if self.existing()?.is_some() {
            return Ok(());
        }
        let key = VaultKey::generate()?;
        let mut raw = Zeroizing::new(Vec::with_capacity(49));
        raw.push(1);
        raw.extend_from_slice(&key.id);
        raw.extend_from_slice(key.bytes.as_ref());
        self.entry
            .set_secret(&raw)
            .map_err(|_| WebError::VaultUnavailable)
    }
}
#[cfg(windows)]
impl KeyProvider for WindowsKeyProvider {
    fn load(&self) -> Result<VaultKey, WebError> {
        self.existing()?.ok_or(WebError::VaultUnavailable)
    }
}

pub(crate) fn local_provider(
    database: &PocDatabase,
) -> Result<std::sync::Arc<dyn KeyProvider>, WebError> {
    #[cfg(windows)]
    {
        Ok(std::sync::Arc::new(WindowsKeyProvider::new(database)?))
    }
    #[cfg(not(windows))]
    {
        let _ = database;
        Err(WebError::VaultUnavailable)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    #[ignore = "Explicit Windows OS-store smoke; creates only an isolated temporary entry"]
    fn windows_key_persists_across_provider_and_database_reopen_without_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let db = PocDatabase::prepare(&root.path().join("poc")).unwrap();
        crate::auth::bootstrap_admin(
            &db,
            "owner",
            "synthetic-windows-test-password".to_owned().into(),
        )
        .unwrap();
        let provider = WindowsKeyProvider::new(&db).unwrap();
        assert!(provider.load().is_err());
        crate::vault::initialize_local_key(&db).unwrap();
        let first = provider.load().unwrap();
        drop(provider);
        let provider = WindowsKeyProvider::new(&db).unwrap();
        crate::vault::initialize_local_key(&db).unwrap();
        let reopened = provider.load().unwrap();
        assert!(first.id == reopened.id && first.bytes.as_ref() == reopened.bytes.as_ref());
        provider.entry.delete_credential().unwrap();
        assert!(provider.load().is_err());
        // Existing binding must never cause automatic replacement key generation.
        assert!(crate::vault::initialize_local_key(&db).is_err());
        assert!(provider.load().is_err());
    }
}
