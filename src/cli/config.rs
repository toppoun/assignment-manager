use std::{
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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Moodle {
    pub host: String,
    pub path: String,
    pub scope: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConfigError {
    Missing,
    Invalid,
    Unreadable,
}

impl Config {
    pub fn read(path: &Path) -> Result<Self, ConfigError> {
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
        Self::parse(&data)
    }

    pub fn parse(data: &[u8]) -> Result<Self, ConfigError> {
        let config: Self = serde_json::from_slice(data).map_err(|_| ConfigError::Invalid)?;
        config.target_id()?;
        if config.moodle.is_none() && !config.webclass {
            return Err(ConfigError::Invalid);
        }
        if let Some(moodle) = &config.moodle {
            let url = format!("https://{}{}", moodle.host, moodle.path);
            moodle.policy()?;
            MoodleFeed::new(&moodle.scope, MoodleExportKind::CourseRelated, &url)
                .map_err(|_| ConfigError::Invalid)?;
        }
        for mapping in &config.course_mappings {
            let key = &mapping.course_key;
            let scope_matches = match key.origin {
                OriginKind::Moodle => config.moodle.as_ref().is_some_and(|m| m.scope == key.scope),
                OriginKind::WebClass => config.webclass && key.scope == DEFAULT_COURSE_SOURCE_SCOPE,
                OriginKind::Manual => false,
            };
            if !scope_matches || key.id.trim().is_empty() || !mapping.has_valid_page_id() {
                return Err(ConfigError::Invalid);
            }
        }
        Ok(config)
    }

    pub fn target_id(&self) -> Result<String, ConfigError> {
        self.notion.validated_id().map_err(|_| ConfigError::Invalid)
    }
}

impl Moodle {
    pub fn policy(&self) -> Result<NetworkPolicy, ConfigError> {
        NetworkPolicy::default()
            .with_moodle_calendar(&self.host, &self.path)
            .map_err(|_| ConfigError::Invalid)
    }
}

pub(super) fn root() -> Result<PathBuf, ConfigError> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| path.join("AssignmentManagerNotion"))
        .ok_or(ConfigError::Missing)
}
