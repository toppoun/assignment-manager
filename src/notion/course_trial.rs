use std::{
    collections::HashSet,
    fmt::Write as _,
    fs::OpenOptions,
    io::{Read, Write},
    path::Path,
};

use aws_lc_rs::digest::{Context, SHA256, digest};
use reqwest::Method;
use serde_json::{Value, json};

use crate::import::{ExternalRecordKey, OriginKind};

use super::{
    CourseMapping, MOODLE_INTEGRATION_SCOPE, MoodlePreviewMappings, NotionClient, NotionError,
    NotionTarget, TrialRole, TrialTarget, VerificationTarget,
    trial::{MOODLE_TRIAL_COURSES, MOODLE_VERIFICATION_COURSES},
    wire::{checked_page_id, data_source_parent, page_property, text, uuid},
};

const WRITER: &str = "41e85934-7e84-81b1-a761-002702fbd8d2";
const TITLE: &str = "授業名";
const COURSE_COUNT: usize = 6;
// Fingerprint the ordered, exact UTF-8 categories without putting those labels in Git.
const APPROVED_CATEGORIES_SHA256: [u8; 32] = [
    0xa4, 0x5a, 0x12, 0x8c, 0x48, 0xb4, 0x4a, 0x0a, 0xae, 0xda, 0x0c, 0xfd, 0x74, 0xaf, 0xa4, 0x50,
    0x07, 0xf5, 0x2c, 0xba, 0x55, 0xec, 0x50, 0xeb, 0x37, 0xd6, 0x35, 0xf3, 0x08, 0xdb, 0x36, 0x29,
];

pub struct CourseCreationTrial {
    target: TrialTarget,
    categories: [String; COURSE_COUNT],
}

#[derive(PartialEq, Eq)]
pub struct CourseSnapshot {
    database: String,
    baseline_pages: [[u8; 32]; MOODLE_TRIAL_COURSES.len()],
    pages: [Option<String>; COURSE_COUNT],
}

impl CourseSnapshot {
    pub fn total_courses(&self) -> usize {
        MOODLE_TRIAL_COURSES.len() + self.existing_ordinals().count()
    }

    pub fn existing_ordinals(&self) -> impl Iterator<Item = usize> + '_ {
        self.pages
            .iter()
            .enumerate()
            .filter_map(|(index, id)| id.as_ref().map(|_| index + 1))
    }

    pub(super) fn baseline_revision(&self) -> String {
        let mut revision = String::with_capacity(64);
        for byte in digest(&SHA256, self.baseline_pages.as_flattened()).as_ref() {
            write!(&mut revision, "{byte:02x}").expect("writing to a String cannot fail");
        }
        revision
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CourseTrialError {
    Notion(NotionError),
    Journal,
    AlreadyStarted,
    PlanChanged,
    ResultUnconfirmed(NotionError),
}

impl From<NotionError> for CourseTrialError {
    fn from(error: NotionError) -> Self {
        Self::Notion(error)
    }
}

impl CourseCreationTrial {
    pub fn new(notion: NotionTarget, categories: Vec<String>) -> Result<Self, NotionError> {
        let categories: [String; COURSE_COUNT] = categories
            .try_into()
            .map_err(|_| NotionError::Configuration)?;
        if categories.iter().any(|name| name.contains(['\r', '\n']))
            || digest(&SHA256, categories.join("\n").as_bytes()).as_ref()
                != APPROVED_CATEGORIES_SHA256
        {
            return Err(NotionError::Configuration);
        }
        let target = TrialTarget::from_verification(
            VerificationTarget::moodle_integration(notion)?,
            WRITER,
        )?;
        Ok(Self { target, categories })
    }

    pub fn target(&self) -> &TrialTarget {
        &self.target
    }

    #[cfg(test)]
    pub(super) fn for_mock(notion: NotionTarget, categories: [&str; COURSE_COUNT]) -> Self {
        let target = TrialTarget::from_verification(
            VerificationTarget::moodle_integration(notion).unwrap(),
            WRITER,
        )
        .unwrap();
        Self {
            target,
            categories: categories.map(str::to_owned),
        }
    }

    pub fn inspect(
        &self,
        client: &NotionClient,
        role: TrialRole,
    ) -> Result<CourseSnapshot, NotionError> {
        client.verify_trial_bot(&self.target, role)?;
        let schema = client.get_data_source(MOODLE_VERIFICATION_COURSES)?;
        let database = data_source_parent(&schema, MOODLE_VERIFICATION_COURSES)?;
        let title = &schema["properties"][TITLE];
        if title["type"] != "title" || title["id"] != "title" || !title["title"].is_object() {
            return Err(NotionError::SchemaMismatch);
        }
        self.read_snapshot(client, &database)
    }

    pub fn create(
        &self,
        client: &NotionClient,
        journal_path: &Path,
        confirm: impl FnOnce() -> bool,
    ) -> Result<bool, CourseTrialError> {
        if journal_path
            .try_exists()
            .map_err(|_| CourseTrialError::Journal)?
        {
            return Err(CourseTrialError::AlreadyStarted);
        }
        let mut expected = self.inspect(client, TrialRole::Write)?;
        if expected.existing_ordinals().next().is_some() {
            return Err(NotionError::Conflict.into());
        }
        if !confirm() {
            return Ok(false);
        }
        let mut journal = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(journal_path)
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::AlreadyExists => CourseTrialError::AlreadyStarted,
                _ => CourseTrialError::Journal,
            })?;

        record(
            &mut journal,
            format_args!("baseline {}", expected.baseline_revision()),
        )?;
        for (index, title) in self.categories.iter().enumerate() {
            if self.read_snapshot(client, &expected.database)? != expected {
                return Err(CourseTrialError::PlanChanged);
            }
            record(&mut journal, format_args!("attempt {}", index + 1))?;
            let response = client
                .create_verification_course(title)
                .map_err(CourseTrialError::ResultUnconfirmed)?;
            let (id, actual_title) = validated_course(&response, &expected.database)
                .map_err(CourseTrialError::ResultUnconfirmed)?;
            if actual_title != *title
                || expected
                    .pages
                    .iter()
                    .flatten()
                    .any(|previous| previous == &id)
                || MOODLE_TRIAL_COURSES.iter().any(|(_, prior)| *prior == id)
            {
                return Err(CourseTrialError::ResultUnconfirmed(
                    NotionError::InvalidResponse,
                ));
            }
            record(&mut journal, format_args!("confirmed {} {id}", index + 1))?;
            expected.pages[index] = Some(id);
        }
        Ok(true)
    }

    pub fn verified_mappings(
        &self,
        snapshot: &CourseSnapshot,
        journal_path: &Path,
    ) -> Result<Vec<CourseMapping>, CourseTrialError> {
        let confirmed = confirmed_pages(journal_path, &snapshot.baseline_revision())?;
        let mut mappings = Vec::with_capacity(COURSE_COUNT);
        for ((title, observed), confirmed_id) in
            self.categories.iter().zip(&snapshot.pages).zip(confirmed)
        {
            let observed = observed.as_ref().ok_or(NotionError::IncompleteSnapshot)?;
            if confirmed_id.as_ref().is_some_and(|id| id != observed) {
                return Err(CourseTrialError::PlanChanged);
            }
            mappings.push(CourseMapping {
                course_key: ExternalRecordKey {
                    origin: OriginKind::Moodle,
                    scope: MOODLE_INTEGRATION_SCOPE.into(),
                    id: title.clone(),
                },
                page_id: observed.clone(),
            });
        }
        MoodlePreviewMappings::new(&mappings)?;
        Ok(mappings)
    }

    fn read_snapshot(
        &self,
        client: &NotionClient,
        database: &str,
    ) -> Result<CourseSnapshot, NotionError> {
        let path = format!("/v1/data_sources/{MOODLE_VERIFICATION_COURSES}/query");
        let page: Value = client.read(|| {
            client
                .request(Method::POST, &path, &[("filter_properties", "title")])
                .json(&json!({"page_size": 100}))
        })?;
        // The approved scope is two existing courses plus six additions. A
        // second page can only be an unexpected inventory, so fail closed.
        if page["object"] != "list"
            || page["has_more"] != false
            || page.get("next_cursor") != Some(&Value::Null)
        {
            return Err(NotionError::IncompleteSnapshot);
        }
        let rows = page["results"]
            .as_array()
            .ok_or(NotionError::InvalidResponse)?;
        let mut snapshot = CourseSnapshot {
            database: database.into(),
            baseline_pages: Default::default(),
            pages: Default::default(),
        };
        let mut seen = HashSet::new();
        for row in rows {
            let (id, title) = validated_course(row, database)?;
            if !seen.insert(id.clone()) {
                return Err(NotionError::Conflict);
            }
            if let Some(index) = MOODLE_TRIAL_COURSES.iter().position(|(_, old)| *old == id) {
                if self.categories.contains(&title) {
                    return Err(NotionError::Conflict);
                }
                let timestamp = row["last_edited_time"]
                    .as_str()
                    .filter(|value| chrono::DateTime::parse_from_rfc3339(value).is_ok())
                    .ok_or(NotionError::InvalidResponse)?;
                let mut page_revision = Context::new(&SHA256);
                page_revision.update(timestamp.as_bytes());
                page_revision.update(&[0]);
                page_revision.update(title.as_bytes());
                snapshot.baseline_pages[index].copy_from_slice(page_revision.finish().as_ref());
                continue;
            }
            let index = self
                .categories
                .iter()
                .position(|name| *name == title)
                .ok_or(NotionError::Conflict)?;
            if snapshot.pages[index].replace(id).is_some() {
                return Err(NotionError::Conflict);
            }
        }
        if !MOODLE_TRIAL_COURSES
            .iter()
            .all(|(_, id)| seen.contains(*id))
        {
            return Err(NotionError::IncompleteSnapshot);
        }
        Ok(snapshot)
    }
}

fn validated_course(page: &Value, database: &str) -> Result<(String, String), NotionError> {
    let id = checked_page_id(page, MOODLE_VERIFICATION_COURSES, database)?;
    if page["properties"][TITLE]["id"] != "title" {
        return Err(NotionError::SchemaMismatch);
    }
    let title = text(page_property(page, "title", "title")?)?;
    Ok((id, title))
}

fn record(
    journal: &mut std::fs::File,
    line: std::fmt::Arguments<'_>,
) -> Result<(), CourseTrialError> {
    writeln!(journal, "{line}")
        .and_then(|()| journal.sync_all())
        .map_err(|_| CourseTrialError::Journal)
}

fn confirmed_pages(
    path: &Path,
    baseline: &str,
) -> Result<[Option<String>; COURSE_COUNT], CourseTrialError> {
    let mut contents = String::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(4097).read_to_string(&mut contents))
        .map_err(|_| CourseTrialError::Journal)?;
    if contents.len() > 4096 || (!contents.is_empty() && !contents.ends_with('\n')) {
        return Err(CourseTrialError::Journal);
    }
    let mut confirmed = std::array::from_fn(|_| None);
    let mut lines = contents.lines();
    let initial = lines
        .next()
        .and_then(|line| line.strip_prefix("baseline "))
        .ok_or(CourseTrialError::Journal)?;
    if initial != baseline {
        return Err(CourseTrialError::PlanChanged);
    }
    for (index, page) in confirmed.iter_mut().enumerate() {
        let Some(attempt) = lines.next() else { break };
        let ordinal = index + 1;
        if attempt != format!("attempt {ordinal}") {
            return Err(CourseTrialError::Journal);
        }
        let Some(success) = lines.next() else { break };
        *page = Some(
            success
                .strip_prefix(&format!("confirmed {ordinal} "))
                .and_then(uuid)
                .ok_or(CourseTrialError::Journal)?,
        );
    }
    if lines.next().is_some() {
        return Err(CourseTrialError::Journal);
    }
    Ok(confirmed)
}
