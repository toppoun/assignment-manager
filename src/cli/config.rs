use std::{
    collections::HashSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use assignment_manager::{
    import::{DEFAULT_COURSE_SOURCE_SCOPE, OriginKind},
    moodle_http::{MoodleExportKind, MoodleFeed},
    notion::{CourseMapping, NotionTarget},
    university_network::NetworkPolicy,
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    pub notion: NotionTarget,
    pub moodle: Option<Moodle>,
    #[serde(default)]
    pub webclass: bool,
    #[serde(default)]
    pub course_mappings: Vec<CourseMapping>,
    #[serde(default)]
    pub sample_assignment_page_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Moodle {
    pub host: String,
    pub path: String,
    pub scope: String,
    pub windows_crl: Option<WindowsMoodleCrl>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) struct WindowsMoodleCrl {
    pub leaf_certificate: PathBuf,
    pub issuer_certificate: PathBuf,
    pub crl: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConfigError {
    Missing,
    Invalid,
    Unreadable,
}

impl Config {
    pub fn read(path: &Path) -> Result<Self, ConfigError> {
        Self::parse(&read_bytes(path)?)
    }

    pub fn parse(data: &[u8]) -> Result<Self, ConfigError> {
        let config: Self = serde_json::from_slice(data).map_err(|_| ConfigError::Invalid)?;
        config.target_id()?;
        if let Some(moodle) = &config.moodle {
            let url = format!("https://{}{}", moodle.host, moodle.path);
            moodle.policy()?;
            MoodleFeed::new(&moodle.scope, MoodleExportKind::CourseRelated, &url)
                .map_err(|_| ConfigError::Invalid)?;
        }
        let mut mapped_keys = HashSet::new();
        for mapping in config.legacy_course_mappings() {
            let key = &mapping.course_key;
            let scope_matches = match key.origin {
                OriginKind::Moodle => MoodleFeed::new(
                    &key.scope,
                    MoodleExportKind::CourseRelated,
                    "https://fixture.invalid/calendar",
                )
                .is_ok(),
                OriginKind::WebClass => key.scope == DEFAULT_COURSE_SOURCE_SCOPE,
                OriginKind::Manual => false,
            };
            if !scope_matches
                || key.id.trim().is_empty()
                || !mapping.has_valid_page_id()
                || !mapped_keys.insert(key)
            {
                return Err(ConfigError::Invalid);
            }
        }
        Ok(config)
    }

    pub fn legacy_course_mappings(&self) -> &[CourseMapping] {
        if self.notion.course_rules.is_some() {
            &[]
        } else {
            &self.course_mappings
        }
    }

    pub fn target_id(&self) -> Result<String, ConfigError> {
        self.notion.validated_id().map_err(|_| ConfigError::Invalid)
    }
}

pub(super) fn read_bytes(path: &Path) -> Result<Vec<u8>, ConfigError> {
    let file = File::open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ConfigError::Missing,
        _ => ConfigError::Unreadable,
    })?;
    if !file
        .metadata()
        .map_err(|_| ConfigError::Unreadable)?
        .is_file()
    {
        return Err(ConfigError::Invalid);
    }
    let mut data = Vec::new();
    file.take(65_537)
        .read_to_end(&mut data)
        .map_err(|_| ConfigError::Unreadable)?;
    if data.len() > 65_536 {
        return Err(ConfigError::Invalid);
    }
    Ok(data)
}

impl Moodle {
    pub fn policy(&self) -> Result<NetworkPolicy, ConfigError> {
        NetworkPolicy::default()
            .with_moodle_calendar(&self.host, &self.path)
            .map_err(|_| ConfigError::Invalid)
    }
}

pub(super) fn root() -> Result<PathBuf, ConfigError> {
    #[cfg(target_os = "macos")]
    let (variable, subdirectory) = (
        "HOME",
        "Library/Application Support/AssignmentManagerNotion",
    );
    #[cfg(not(target_os = "macos"))]
    let (variable, subdirectory) = ("LOCALAPPDATA", "AssignmentManagerNotion");

    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| path.join(subdirectory))
        .ok_or(ConfigError::Missing)
}
