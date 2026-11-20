use std::collections::HashSet;

use reqwest::Method;
use serde::Deserialize;
use serde_json::Value;

use crate::{
    domain::{Deadline, SubmissionTarget},
    import::{AssignmentCandidate, ExternalRecordKey, OriginKind},
};

use super::{
    CourseMapping, NotionClient, NotionError, NotionTarget, SyncInput, UniversitySource,
    wire::{Schema, object_id, property_id, uuid},
};

// Production「大学課題管理」and its course data source. Apply this list to
// both roles in the pair, independent of the caller's local configuration.
pub(super) const PRODUCTION_DATA_SOURCES: [&str; 2] = [
    "41d85934-7e84-811c-a9bb-000b8b646c3f",
    "41d85934-7e84-815d-bab8-000b9dbad959",
];

pub(super) const MANAGED_PROPERTIES: [(&str, &str, &str); 4] = [
    ("status", "状態", "status"),
    ("priority", "優先度", "select"),
    ("recurrence", "繰り返し", "select"),
    ("notes", "メモ", "rich_text"),
];

#[derive(Clone, Copy)]
pub enum TrialRole {
    Read,
    Write,
}

/// A validated, explicitly selected pair of trial data sources.
pub struct TrialTarget {
    notion: NotionTarget,
    assignments: String,
    courses: String,
    read_bot: String,
    write_bot: String,
}

impl TrialTarget {
    pub fn new(
        notion: NotionTarget,
        selected: &str,
        read_bot: &str,
        write_bot: &str,
    ) -> Result<Self, NotionError> {
        let (assignments, courses) = notion.ids()?;
        if uuid(selected).as_ref() != Some(&assignments)
            || PRODUCTION_DATA_SOURCES.contains(&assignments.as_str())
            || PRODUCTION_DATA_SOURCES.contains(&courses.as_str())
        {
            return Err(NotionError::MethodDisabled);
        }
        if notion.properties.sync_key.is_empty() || notion.properties.source.is_empty() {
            return Err(NotionError::Configuration);
        }
        let read_bot = uuid(read_bot).ok_or(NotionError::Configuration)?;
        let write_bot = uuid(write_bot).ok_or(NotionError::Configuration)?;
        if read_bot == write_bot {
            return Err(NotionError::Configuration);
        }
        Ok(Self {
            notion,
            assignments,
            courses,
            read_bot,
            write_bot,
        })
    }

    pub fn assignment_id(&self) -> &str {
        &self.assignments
    }

    pub fn bot_id(&self, role: TrialRole) -> &str {
        match role {
            TrialRole::Read => &self.read_bot,
            TrialRole::Write => &self.write_bot,
        }
    }

    pub(super) fn retrieve_schema(&self, client: &NotionClient) -> Result<Schema, NotionError> {
        let assignments = client.get_data_source(&self.assignments)?;
        let courses = client.get_data_source(&self.courses)?;
        let schema = Schema::validate(
            &self.notion,
            (self.assignments.clone(), self.courses.clone()),
            &assignments,
            &courses,
        )?;
        let mut property_ids: HashSet<_> = schema.properties.fields().map(|(id, _)| id).into();
        if schema.assignments_database == schema.courses_database
            || MANAGED_PROPERTIES.iter().any(|(_, name, kind)| {
                let prop = &assignments["properties"][name];
                prop["type"] != *kind
                    || !prop[kind].is_object()
                    || prop["id"]
                        .as_str()
                        .is_none_or(|id| !property_id(id) || !property_ids.insert(id))
            })
        {
            return Err(NotionError::SchemaMismatch);
        }
        Ok(schema)
    }
}

impl NotionClient {
    pub(super) fn verify_trial_bot(
        &self,
        target: &TrialTarget,
        role: TrialRole,
    ) -> Result<(), NotionError> {
        let user: Value = self.read(|| self.request(Method::GET, "/v1/users/me", &[]))?;
        let owner = &user["bot"]["owner"];
        if user["object"] != "user"
            || user["type"] != "bot"
            || owner["type"] != "workspace"
            || owner["workspace"] != true
            || object_id(&user, "id").as_deref() != Some(target.bot_id(role))
        {
            return Err(NotionError::CredentialMismatch);
        }
        Ok(())
    }
}

/// One stable, synthetic input. Editing or removing its deadline preserves its key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrialAssignment {
    pub title: String,
    pub deadline: Option<Deadline>,
    pub course_page_id: Option<String>,
}

impl TrialAssignment {
    pub(super) fn input(&self) -> Result<(SyncInput, Vec<CourseMapping>), NotionError> {
        if self.title.trim().is_empty()
            || self.title.chars().any(char::is_control)
            || self.title.encode_utf16().count() > 2_000
        {
            return Err(NotionError::Configuration);
        }
        let key = |id: &str| ExternalRecordKey {
            origin: OriginKind::Moodle,
            scope: "assignment-manager.phase-3d.synthetic".into(),
            id: id.into(),
        };
        let mappings = self
            .course_page_id
            .as_deref()
            .map(|page_id| {
                Ok(CourseMapping {
                    course_key: key("course"),
                    page_id: uuid(page_id).ok_or(NotionError::Configuration)?,
                })
            })
            .transpose()?
            .into_iter()
            .collect::<Vec<_>>();
        let mut input = SyncInput::default();
        input
            .insert(
                &AssignmentCandidate {
                    external_key: key("assignment"),
                    title: self.title.clone(),
                    deadline: self.deadline.clone(),
                    submission: SubmissionTarget::Unknown,
                    submission_url: None,
                },
                UniversitySource::Moodle,
                mappings.first().map(|mapping| &mapping.course_key),
            )
            .map_err(|_| NotionError::Configuration)?;
        Ok((input, mappings))
    }
}
