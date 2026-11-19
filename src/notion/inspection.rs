//! GET-only diagnostics. Explicitly selected pages never form a complete snapshot.

use std::{
    collections::{BTreeSet, HashSet},
    fmt,
};

use serde_json::{Map, Value};

use super::{
    CourseMapping, NotionClient, NotionError, NotionTarget,
    wire::{
        checked_page_id, data_source_parent, page_relations, relation_targets, source_options, uuid,
    },
};

const MAX_ASSIGNMENT_SAMPLES: usize = 64;
const MAX_COURSE_MAPPINGS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyStatus {
    Ready,
    Missing,
    TypeMismatch,
    InvalidOptions,
    IdNotConfigured,
    Ambiguous,
}

impl PropertyStatus {
    pub fn code(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Missing => "missing",
            Self::TypeMismatch => "type_mismatch",
            Self::InvalidOptions => "invalid_options",
            Self::IdNotConfigured => "id_not_configured",
            Self::Ambiguous => "ambiguous",
        }
    }
}

#[derive(Debug)]
pub struct PropertyFinding {
    pub role: &'static str,
    pub expected_type: &'static str,
    pub status: PropertyStatus,
}

#[derive(Debug, Default)]
pub struct InspectionReport {
    pub properties: Vec<PropertyFinding>,
    pub assignments_source_verified: bool,
    pub courses_source_verified: bool,
    pub relation_target_verified: bool,
    pub assignment_samples: usize,
    pub sample_relation_entries: usize,
    pub mapped_course_pages: usize,
    pub error: Option<NotionError>,
}

impl fmt::Display for InspectionReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let check = |verified| if verified { "verified" } else { "unverified" };
        let relation = if self.relation_target_verified {
            "verified"
        } else if self.assignments_source_verified && self.courses_source_verified {
            "mismatch"
        } else {
            "unverified"
        };
        writeln!(
            f,
            "assignments_source={} courses_source={} relation_target={}",
            check(self.assignments_source_verified),
            check(self.courses_source_verified),
            relation
        )?;
        for property in &self.properties {
            writeln!(
                f,
                "property.{}={} expected_type={}",
                property.role,
                property.status.code(),
                property.expected_type
            )?;
        }
        write!(
            f,
            "assignment_samples={} sample_relation_entries={} mapped_course_pages={} snapshot=unavailable_get_only",
            self.assignment_samples, self.sample_relation_entries, self.mapped_course_pages
        )
    }
}

impl NotionClient {
    /// Validate schema and GET only the explicitly named pages. A page sample
    /// cannot prove the absence of a sync key in the data source.
    pub fn inspect(
        &self,
        target: &NotionTarget,
        assignment_samples: &[String],
        mappings: &[CourseMapping],
    ) -> InspectionReport {
        let mut report = InspectionReport::default();
        report.error = self
            .inspect_into(target, assignment_samples, mappings, &mut report)
            .err();
        report
    }

    fn inspect_into(
        &self,
        target: &NotionTarget,
        assignment_samples: &[String],
        mappings: &[CourseMapping],
        report: &mut InspectionReport,
    ) -> Result<(), NotionError> {
        if assignment_samples.len() > MAX_ASSIGNMENT_SAMPLES || mappings.len() > MAX_COURSE_MAPPINGS
        {
            return Err(NotionError::Configuration);
        }
        let (assignment_id, course_id) = target.ids()?;
        let samples = page_ids(assignment_samples.iter().map(String::as_str))?;
        let mapped_courses = page_ids(mappings.iter().map(|mapping| mapping.page_id.as_str()))?;

        let assignment = self.get_data_source(&assignment_id)?;
        let assignment_db = data_source_parent(&assignment, &assignment_id)?;
        report.assignments_source_verified = true;
        let properties = assignment["properties"]
            .as_object()
            .ok_or(NotionError::SchemaMismatch)?;
        let (findings, relation) = schema_findings(target, properties);
        report.properties = findings;

        let course = self.get_data_source(&course_id)?;
        let course_db = data_source_parent(&course, &course_id)?;
        report.courses_source_verified = true;
        if !relation.is_some_and(|relation| relation_targets(relation, &course_id, &course_db)) {
            return Err(NotionError::SchemaMismatch);
        }
        report.relation_target_verified = true;

        // A legacy schema can still be inspected before the proposed columns
        // exist. Its diagnostic result remains a mismatch after successful reads.
        for id in samples {
            let page = self.retrieve_page(&id, &[&target.properties.course])?;
            if checked_page_id(&page, &assignment_id, &assignment_db)? != id {
                return Err(NotionError::InvalidResponse);
            }
            let (links, more) = page_relations(&page, &target.properties.course)?;
            let links = if more {
                self.retrieve_relation(&id, &target.properties.course)?
            } else {
                links
            };
            if links.iter().collect::<HashSet<_>>().len() != links.len() {
                return Err(NotionError::InvalidResponse);
            }
            report.assignment_samples += 1;
            report.sample_relation_entries += links.len();
        }
        for id in mapped_courses {
            let page = self.retrieve_page(&id, &["title"])?;
            if checked_page_id(&page, &course_id, &course_db)? != id {
                return Err(NotionError::InvalidResponse);
            }
            report.mapped_course_pages += 1;
        }

        if report
            .properties
            .iter()
            .any(|property| property.status != PropertyStatus::Ready)
        {
            return Err(NotionError::SchemaMismatch);
        }
        Ok(())
    }
}

fn page_ids<'a>(ids: impl Iterator<Item = &'a str>) -> Result<BTreeSet<String>, NotionError> {
    ids.map(|id| uuid(id).ok_or(NotionError::Configuration))
        .collect()
}

fn schema_findings<'a>(
    target: &NotionTarget,
    props: &'a Map<String, Value>,
) -> (Vec<PropertyFinding>, Option<&'a Value>) {
    let mut findings = Vec::new();
    let mut relation = None;
    let configured = [
        "title",
        "deadline",
        "course",
        "submission_url",
        "sync_key",
        "source",
    ];
    for (role, (id, kind)) in configured.into_iter().zip(target.properties.fields()) {
        let (finding, prop) = if id.is_empty() {
            let name = if role == "sync_key" {
                "同期キー"
            } else {
                "取得元"
            };
            describe(role, kind, props.get(name).into_iter(), false)
        } else {
            describe(
                role,
                kind,
                props.values().filter(|prop| prop["id"] == id),
                true,
            )
        };
        if role == "course" {
            relation = prop;
        }
        findings.push(finding);
    }

    for (role, name, kind) in [
        ("status", "状態", "status"),
        ("priority", "優先度", "select"),
        ("subject", "科目", "select"),
        ("category", "種別", "select"),
        ("recurrence", "繰り返し", "select"),
        ("notes", "メモ", "rich_text"),
    ] {
        findings.push(describe(role, kind, props.get(name).into_iter(), true).0);
    }
    (findings, relation)
}

fn describe<'a>(
    role: &'static str,
    kind: &'static str,
    mut matches: impl Iterator<Item = &'a Value>,
    configured: bool,
) -> (PropertyFinding, Option<&'a Value>) {
    let prop = matches.next();
    let status = match prop {
        None => PropertyStatus::Missing,
        Some(_) if matches.next().is_some() => PropertyStatus::Ambiguous,
        Some(prop) if prop["type"] != kind || !prop[kind].is_object() => {
            PropertyStatus::TypeMismatch
        }
        Some(prop) if role == "source" && source_options(&prop[kind]).is_err() => {
            PropertyStatus::InvalidOptions
        }
        Some(_) if !configured => PropertyStatus::IdNotConfigured,
        Some(_) => PropertyStatus::Ready,
    };
    let valid = prop
        .filter(|_| status == PropertyStatus::Ready)
        .map(|prop| &prop[kind]);
    (
        PropertyFinding {
            role,
            expected_type: kind,
            status,
        },
        valid,
    )
}
