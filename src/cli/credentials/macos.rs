use security_framework::{
    base::Result as KeychainResult,
    item::{ItemClass, ItemSearchOptions, Reference, SearchResult},
    os::macos::{
        keychain::{KeychainUserInteractionLock, SecKeychain, SecPreferencesDomain},
        keychain_item::SecKeychainItem,
    },
};

use super::CredentialStatus;

pub(super) fn suppress_interaction_if(
    unattended: bool,
) -> Result<Option<KeychainUserInteractionLock>, CredentialStatus> {
    // The keyring backend uses SecKeychainFindGenericPassword. Its UI setting
    // is process-wide, so only change it during the CLI's synchronous read,
    // before constructing a network client or starting other work.
    if !unattended
        || !SecKeychain::user_interaction_allowed().map_err(|_| CredentialStatus::Unavailable)?
    {
        return Ok(None);
    }
    SecKeychain::disable_user_interaction()
        .map(Some)
        .map_err(|_| CredentialStatus::Unavailable)
}

pub(super) struct Registration<'a> {
    keychain: SecKeychain,
    service: &'a str,
    account: &'a str,
    existing: Option<SecKeychainItem>,
}

impl<'a> Registration<'a> {
    pub(super) fn open(service: &'a str, account: &'a str) -> Result<Self, CredentialStatus> {
        // Match the keyring backend's user-domain default keychain and item identity.
        let keychain = SecKeychain::default_for_domain(SecPreferencesDomain::User)
            .map_err(|_| CredentialStatus::Unavailable)?;
        let found = ItemSearchOptions::new()
            .keychains(std::slice::from_ref(&keychain))
            .class(ItemClass::generic_password())
            .service(service)
            .account(account)
            .load_refs(true)
            .search();
        let existing = match found {
            Ok(mut items) if items.len() == 1 => match items.pop() {
                Some(SearchResult::Ref(Reference::KeychainItem(item))) => Some(item),
                _ => return Err(CredentialStatus::Unavailable),
            },
            Err(error)
                if matches!(
                    apple_native_keyring_store::keychain::decode_error(error),
                    keyring_core::Error::NoEntry
                ) =>
            {
                None
            }
            _ => return Err(CredentialStatus::Unavailable),
        };
        Ok(Self {
            keychain,
            service,
            account,
            existing,
        })
    }

    pub(super) fn exists(&self) -> bool {
        self.existing.is_some()
    }

    pub(super) fn save(self, secret: &[u8]) -> KeychainResult<()> {
        // A separate add path prevents silently overwriting a concurrently added
        // record when no confirmation was requested.
        match self.existing {
            Some(mut item) => item.set_password(secret),
            None => self
                .keychain
                .add_generic_password(self.service, self.account, secret),
        }
    }
}
