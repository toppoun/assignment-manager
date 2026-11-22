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

const MOODLE_VERIFICATION_ASSIGNMENTS: &str = "41e85934-7e84-817e-b1d2-000b68726503";
pub(super) const MOODLE_VERIFICATION_COURSES: &str = "41e85934-7e84-81df-902b-000bbece9bf7";
const MOODLE_VERIFICATION_READER: &str = "41e85934-7e84-81d8-9891-00275cb3050f";
#[cfg(any(windows, test, feature = "synthetic-https-fixture"))]
pub(super) const MOODLE_VERIFICATION_WRITER: &str = "41e85934-7e84-81b1-a761-002702fbd8d2";
pub(super) const MOODLE_TRIAL_COURSES: [(&str, &str); 2] = [
    ("照合用の授業", "41e85934-7e84-81c4-8ae6-ec4fcaea84d0"),
    ("別の合成授業", "41e85934-7e84-8172-93b3-f4885b1efc7b"),
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
    verification: VerificationTarget,
    write_bot: String,
}

/// A separately selected pair of verification data sources and its reader bot.
pub struct VerificationTarget {
    notion: NotionTarget,
    assignments: String,
    courses: String,
    read_bot: String,
}

#[cfg(feature = "synthetic-https-fixture")]
pub struct MoodleCreateTarget {
    trial: TrialTarget,
    course_pages: [(&'static str, &'static str); 2],
}

#[cfg(feature = "synthetic-https-fixture")]
impl MoodleCreateTarget {
    pub fn new(notion: NotionTarget, writer: &str) -> Result<Self, NotionError> {
        let verification = VerificationTarget::moodle_integration(notion)?;
        let trial = TrialTarget::from_verification(verification, writer)?;
        Ok(Self {
            trial,
            course_pages: MOODLE_TRIAL_COURSES,
        })
    }

    pub fn trial(&self) -> &TrialTarget {
        &self.trial
    }

    pub(super) fn course_page(&self, category: &str) -> Option<&str> {
        self.course_pages
            .iter()
            .find_map(|(name, page)| (*name == category).then_some(*page))
    }

    #[cfg(test)]
    pub(super) fn for_mock(trial: TrialTarget, course_page: &'static str) -> Self {
        Self {
            trial,
            course_pages: [("照合用の授業", course_page), ("別の合成授業", course_page)],
        }
    }
}

impl TrialTarget {
    pub fn new(
        notion: NotionTarget,
        selected: &str,
        read_bot: &str,
        write_bot: &str,
    ) -> Result<Self, NotionError> {
        let courses = notion.courses_data_source_id.clone();
        let verification = VerificationTarget::new(notion, selected, &courses, read_bot)?;
        Self::from_verification(verification, write_bot)
    }

    pub(super) fn from_verification(
        verification: VerificationTarget,
        write_bot: &str,
    ) -> Result<Self, NotionError> {
        let write_bot = uuid(write_bot).ok_or(NotionError::Configuration)?;
        if verification.read_bot == write_bot {
            return Err(NotionError::Configuration);
        }
        Ok(Self {
            verification,
            write_bot,
        })
    }

    pub fn assignment_id(&self) -> &str {
        self.verification.assignment_id()
    }

    pub fn bot_id(&self, role: TrialRole) -> &str {
        match role {
            TrialRole::Read => &self.verification.read_bot,
            TrialRole::Write => &self.write_bot,
        }
    }

    pub(super) fn retrieve_schema(&self, client: &NotionClient) -> Result<Schema, NotionError> {
        self.verification.retrieve_schema(client)
    }
}

impl VerificationTarget {
    pub fn moodle_integration(notion: NotionTarget) -> Result<Self, NotionError> {
        Self::new(
            notion,
            MOODLE_VERIFICATION_ASSIGNMENTS,
            MOODLE_VERIFICATION_COURSES,
            MOODLE_VERIFICATION_READER,
        )
    }

    pub fn new(
        notion: NotionTarget,
        selected_assignments: &str,
        selected_courses: &str,
        read_bot: &str,
    ) -> Result<Self, NotionError> {
        let (assignments, courses) = notion.ids()?;
        if uuid(selected_assignments).as_ref() != Some(&assignments)
            || uuid(selected_courses).as_ref() != Some(&courses)
            || PRODUCTION_DATA_SOURCES.contains(&assignments.as_str())
            || PRODUCTION_DATA_SOURCES.contains(&courses.as_str())
        {
            return Err(NotionError::MethodDisabled);
        }
        if notion.properties.sync_key.is_empty() || notion.properties.source.is_empty() {
            return Err(NotionError::Configuration);
        }
        let read_bot = uuid(read_bot).ok_or(NotionError::Configuration)?;
        Ok(Self {
            notion,
            assignments,
            courses,
            read_bot,
        })
    }

    pub fn assignment_id(&self) -> &str {
        &self.assignments
    }

    pub(super) fn verify_reader(&self, client: &NotionClient) -> Result<(), NotionError> {
        client.verify_bot(&self.read_bot)
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
        self.verify_bot(target.bot_id(role))
    }

    fn verify_bot(&self, expected: &str) -> Result<(), NotionError> {
        let user: Value = self.read(|| self.request(Method::GET, "/v1/users/me", &[]))?;
        let owner = &user["bot"]["owner"];
        if user["object"] != "user"
            || user["type"] != "bot"
            || owner["type"] != "workspace"
            || owner["workspace"] != true
            || object_id(&user, "id").as_deref() != Some(expected)
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
