//! Shared Windows Credential Manager entry construction.

use std::collections::HashMap;

use keyring_core::{Entry, api::CredentialStoreApi};

/// Local credentials survive restart and do not roam to other machines.
pub fn local_entry(service: &str, account: &str) -> keyring_core::Result<Entry> {
    windows_native_keyring_store::Store::new()?.build(
        service,
        account,
        Some(&HashMap::from([("persistence", "Local")])),
    )
}
