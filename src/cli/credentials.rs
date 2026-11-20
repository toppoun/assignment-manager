use super::config::Config;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Moodle,
    WebClass,
    Notion,
}

impl Kind {
    pub const ALL: [Self; 3] = [Self::Moodle, Self::WebClass, Self::Notion];

    pub fn code(self) -> &'static str {
        match self {
            Self::Moodle => "moodle",
            Self::WebClass => "webclass",
            Self::Notion => "notion",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.code() == value)
    }

    pub fn enabled(self, config: &Config) -> bool {
        match self {
            Self::Moodle => config.moodle.is_some(),
            Self::WebClass => config.webclass,
            Self::Notion => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub(super) enum CredentialStatus {
    Ready,
    Disabled,
    Missing,
    Invalid,
    Unavailable,
}

impl CredentialStatus {
    pub fn code(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Disabled => "disabled",
            Self::Missing => "missing",
            Self::Invalid => "invalid",
            Self::Unavailable => "unavailable",
        }
    }

    pub fn ok(self) -> bool {
        matches!(self, Self::Ready | Self::Disabled)
    }
}

pub(super) fn diagnose(
    config: &Config,
    mut lookup: impl FnMut(Kind) -> CredentialStatus,
) -> [CredentialStatus; 3] {
    Kind::ALL.map(|kind| {
        if kind.enabled(config) {
            lookup(kind)
        } else {
            CredentialStatus::Disabled
        }
    })
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(super) fn lookup(_: &Config, _: &str, _: Kind) -> CredentialStatus {
    CredentialStatus::Unavailable
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(super) fn register(_: &Config, _: &str, _: Kind) -> Result<(), CredentialStatus> {
    Err(CredentialStatus::Unavailable)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(super) fn notion_client(
    _: &str,
) -> Result<assignment_manager::notion::NotionClient, CredentialStatus> {
    Err(CredentialStatus::Unavailable)
}

#[cfg(any(windows, target_os = "macos"))]
pub(super) use native::{lookup, notion_client, register};

#[cfg(any(windows, target_os = "macos"))]
mod native {
    use std::io::{self, IsTerminal};

    use assignment_manager::notion::NotionClient;
    #[cfg(target_os = "macos")]
    use keyring_core::api::CredentialStoreApi;
    use keyring_core::{Entry, Error};
    use serde::{Deserialize, Deserializer};
    use zeroize::Zeroizing;

    use super::{Config, CredentialStatus, Kind};

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Pair {
        #[serde(deserialize_with = "secret")]
        university_id: Zeroizing<String>,
        #[serde(deserialize_with = "secret")]
        password: Zeroizing<String>,
    }

    fn secret<'de, D: Deserializer<'de>>(de: D) -> Result<Zeroizing<String>, D::Error> {
        String::deserialize(de).map(Zeroizing::new)
    }

    fn entry(service: &str, kind: Kind) -> Result<Entry, CredentialStatus> {
        #[cfg(windows)]
        let result = assignment_manager::windows_credentials::local_entry(service, kind.code());
        #[cfg(target_os = "macos")]
        let result = apple_native_keyring_store::keychain::Store::new()
            .and_then(|store| store.build(service, kind.code(), None));
        result.map_err(|_| CredentialStatus::Unavailable)
    }

    fn target_entry(target: &str, kind: Kind) -> Result<Entry, CredentialStatus> {
        entry(
            &format!("io.assignment-manager.notion-cli.v1.{target}"),
            kind,
        )
    }

    pub(in crate::cli) fn lookup(config: &Config, target: &str, kind: Kind) -> CredentialStatus {
        let Ok(entry) = target_entry(target, kind) else {
            return CredentialStatus::Unavailable;
        };
        classify(config, kind, entry.get_secret().map(Zeroizing::new))
    }

    pub(in crate::cli) fn notion_client(target: &str) -> Result<NotionClient, CredentialStatus> {
        let entry = target_entry(target, Kind::Notion)?;
        let bytes = entry.get_secret().map(Zeroizing::new).map_err(read_error)?;
        client_from_secret(bytes)
    }

    fn client_from_secret(bytes: Zeroizing<Vec<u8>>) -> Result<NotionClient, CredentialStatus> {
        let token = std::str::from_utf8(&bytes).map_err(|_| CredentialStatus::Invalid)?;
        NotionClient::new(token).map_err(|_| CredentialStatus::Invalid)
    }

    fn read_error(error: Error) -> CredentialStatus {
        match error {
            Error::NoEntry => CredentialStatus::Missing,
            _ => CredentialStatus::Unavailable,
        }
    }

    fn classify(
        config: &Config,
        kind: Kind,
        result: keyring_core::Result<Zeroizing<Vec<u8>>>,
    ) -> CredentialStatus {
        match result {
            Ok(bytes) if valid(config, kind, &bytes) => CredentialStatus::Ready,
            Ok(_) => CredentialStatus::Invalid,
            Err(error) => read_error(error),
        }
    }

    fn valid(config: &Config, kind: Kind, bytes: &[u8]) -> bool {
        match kind {
            Kind::Moodle => config
                .moodle
                .as_ref()
                .and_then(|moodle| moodle.policy().ok())
                .zip(std::str::from_utf8(bytes).ok())
                .is_some_and(|(policy, url)| policy.validate_moodle_url(url).is_ok()),
            Kind::Notion => !bytes.is_empty() && bytes.iter().all(u8::is_ascii_graphic),
            Kind::WebClass => serde_json::from_slice::<Pair>(bytes).is_ok_and(|pair| {
                !pair.university_id.trim().is_empty() && !pair.password.trim().is_empty()
            }),
        }
    }

    pub(in crate::cli) fn register(
        config: &Config,
        target: &str,
        kind: Kind,
    ) -> Result<(), CredentialStatus> {
        if !io::stdin().is_terminal() {
            return Err(CredentialStatus::Unavailable);
        }
        let entry = target_entry(target, kind)?;
        let prompt = |label| {
            rpassword::prompt_password(label)
                .map(Zeroizing::new)
                .map_err(|_| CredentialStatus::Unavailable)
        };
        let bytes = match kind {
            Kind::Moodle => Zeroizing::new(
                prompt("Moodle calendar URL (hidden): ")?
                    .as_bytes()
                    .to_vec(),
            ),
            Kind::Notion => Zeroizing::new(prompt("Notion token (hidden): ")?.as_bytes().to_vec()),
            Kind::WebClass => {
                let id = prompt("WebClass ID (hidden): ")?;
                let password = prompt("WebClass password (hidden): ")?;
                #[derive(serde::Serialize)]
                #[serde(rename_all = "camelCase")]
                struct Encoded<'a> {
                    university_id: &'a str,
                    password: &'a str,
                }
                Zeroizing::new(
                    serde_json::to_vec(&Encoded {
                        university_id: &id,
                        password: &password,
                    })
                    .map_err(|_| CredentialStatus::Invalid)?,
                )
            }
        };
        if !valid(config, kind, &bytes) {
            return Err(CredentialStatus::Invalid);
        }
        // Replace the logical record atomically in the OS credential store.
        entry
            .set_secret(&bytes)
            .map_err(|_| CredentialStatus::Unavailable)
    }

    #[cfg(test)]
    #[test]
    fn classifies_missing_or_malformed_credentials_without_accessing_the_os_store() {
        let config = super::super::tests::config();
        assert_eq!(
            classify(&config, Kind::Notion, Err(Error::NoEntry)),
            CredentialStatus::Missing
        );
        assert_eq!(
            classify(&config, Kind::Notion, Err(Error::NoDefaultStore)),
            CredentialStatus::Unavailable
        );
        assert_eq!(
            classify(
                &config,
                Kind::WebClass,
                Ok(Zeroizing::new(b"not a credential pair".to_vec()))
            ),
            CredentialStatus::Invalid
        );
        assert!(valid(&config, Kind::Notion, b"synthetic-token"));
        assert!(!valid(&config, Kind::Notion, b"synthetic-token\r\n"));
        assert!(client_from_secret(Zeroizing::new(b"synthetic-token".to_vec())).is_ok());
        assert_eq!(
            client_from_secret(Zeroizing::new(vec![0xff])).err(),
            Some(CredentialStatus::Invalid)
        );
        assert!(valid(
            &config,
            Kind::WebClass,
            br#"{"universityId":"synthetic","password":"synthetic"}"#
        ));
    }

    #[cfg(all(test, target_os = "macos"))]
    #[test]
    #[ignore = "requires the user's unlocked macOS login Keychain"]
    fn macos_keychain_round_trip() {
        // This namespace cannot be selected by any CLI config.
        let nonce = tempfile::tempdir().unwrap();
        let name = nonce.path().file_name().unwrap().to_str().unwrap();
        let service = format!("io.assignment-manager.notion-cli.test.{name}");
        let config = super::super::tests::config();
        let kind = Kind::Notion;
        let record = entry(&service, kind).unwrap();
        assert_eq!(
            classify(&config, kind, record.get_secret().map(Zeroizing::new)),
            CredentialStatus::Missing
        );

        struct RemoveTestRecord<'a>(&'a Entry);
        impl Drop for RemoveTestRecord<'_> {
            fn drop(&mut self) {
                let _ = self.0.delete_credential();
            }
        }
        let _cleanup = RemoveTestRecord(&record);

        for (value, expected) in [
            (b"synthetic-token".as_slice(), CredentialStatus::Ready),
            (b"replacement-token", CredentialStatus::Ready),
            (b"malformed token", CredentialStatus::Invalid),
        ] {
            assert!(
                record.set_secret(value).is_ok(),
                "test Keychain save failed"
            );
            let reader = entry(&service, kind).unwrap();
            let loaded = reader.get_secret().map(Zeroizing::new);
            assert!(loaded.as_ref().is_ok_and(|bytes| bytes.as_slice() == value));
            assert_eq!(classify(&config, kind, loaded), expected);
        }

        assert!(
            record.delete_credential().is_ok(),
            "test Keychain cleanup failed"
        );
        assert!(matches!(record.get_secret(), Err(Error::NoEntry)));
    }
}
