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
#[cfg_attr(not(windows), allow(dead_code))]
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

#[cfg(not(windows))]
pub(super) fn lookup(_: &Config, _: &str, _: Kind) -> CredentialStatus {
    CredentialStatus::Unavailable
}

#[cfg(not(windows))]
pub(super) fn register(_: &Config, _: &str, _: Kind) -> Result<(), CredentialStatus> {
    Err(CredentialStatus::Unavailable)
}

#[cfg(windows)]
pub(super) use windows::{lookup, register};

#[cfg(windows)]
mod windows {
    use std::io::{self, IsTerminal};

    use assignment_manager::windows_credentials::local_entry;
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

    fn entry(target: &str, kind: Kind) -> Result<Entry, CredentialStatus> {
        let service = format!("io.assignment-manager.notion-cli.v1.{target}");
        local_entry(&service, kind.code()).map_err(|_| CredentialStatus::Unavailable)
    }

    pub(in crate::cli) fn lookup(config: &Config, target: &str, kind: Kind) -> CredentialStatus {
        let Ok(entry) = entry(target, kind) else {
            return CredentialStatus::Unavailable;
        };
        classify(config, kind, entry.get_secret().map(Zeroizing::new))
    }

    fn classify(
        config: &Config,
        kind: Kind,
        result: keyring_core::Result<Zeroizing<Vec<u8>>>,
    ) -> CredentialStatus {
        match result {
            Ok(bytes) if valid(config, kind, &bytes) => CredentialStatus::Ready,
            Ok(_) => CredentialStatus::Invalid,
            Err(Error::NoEntry) => CredentialStatus::Missing,
            Err(_) => CredentialStatus::Unavailable,
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
        let entry = entry(target, kind)?;
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
            classify(
                &config,
                Kind::WebClass,
                Ok(Zeroizing::new(b"not a credential pair".to_vec()))
            ),
            CredentialStatus::Invalid
        );
        assert!(valid(&config, Kind::Notion, b"synthetic-token"));
        assert!(!valid(&config, Kind::Notion, b"synthetic-token\r\n"));
        assert!(valid(
            &config,
            Kind::WebClass,
            br#"{"universityId":"synthetic","password":"synthetic"}"#
        ));
    }
}
