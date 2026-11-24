//! Read-only schema diagnostics and complete assignment snapshots.

use std::{
    collections::{BTreeSet, HashSet},
    fmt,
};

use serde_json::{Map, Value};

use super::{
    CourseMapping, CourseRuleSummary, DryRunReport, NotionAssignment, NotionClient, NotionError,
    NotionTarget, SyncInput,
    apply::reconcile_snapshot,
    course_rules::{CourseRules, VerificationCourse, VerificationCourseRule},
    live_assignment_read_target,
    state::StateSummary,
    trial::MANAGED_PROPERTIES,
    wire::{
        Schema, checked_page_id, data_source_parent, page_property, page_relations,
        relation_targets, source_options, text, uuid,
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
    pub course_rules_configured: bool,
    pub course_rules: Option<CourseRuleSummary>,
    pub snapshot: Option<SnapshotSummary>,
    pub recoverable_creates: usize,
    pub unresolved_creates: usize,
    pub error: Option<NotionError>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SnapshotSummary {
    pub managed: usize,
    pub manual: usize,
    pub duplicate_keys: usize,
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
            "assignment_samples={} sample_relation_entries={} mapped_course_pages={}",
            self.assignment_samples, self.sample_relation_entries, self.mapped_course_pages
        )?;
        if let Some(rules) = &self.course_rules {
            write!(
                f,
                "\ncourse_rules=complete rows={} confirmed={} unset={} excluded={} duplicate_keys={} invalid_rows={} invalid_relations={} unverified_terms={}",
                rules.rows,
                rules.confirmed,
                rules.unset,
                rules.excluded,
                rules.duplicate_keys,
                rules.invalid_rows,
                rules.invalid_relations,
                rules.unverified_terms
            )?;
        } else if self.course_rules_configured {
            write!(f, "\ncourse_rules=unavailable")?;
        }
        match &self.snapshot {
            Some(snapshot) => write!(
                f,
                "\nsnapshot=complete managed={} manual={} duplicate_keys={}",
                snapshot.managed, snapshot.manual, snapshot.duplicate_keys
            ),
            None => write!(f, "\nsnapshot=unavailable"),
        }
    }
}

impl NotionClient {
    /// Build the live dry run and its course diagnostics from the same validated rules.
    pub fn inspect_live_assignment_dry_run(
        &self,
        input: &SyncInput,
        target: &NotionTarget,
        assignment_samples: &[String],
        mut state: StateSummary,
        production: bool,
    ) -> Result<(InspectionReport, DryRunReport, Vec<VerificationCourseRule>), NotionError> {
        live_assignment_read_target(target, production)?;
        let mut inspection = InspectionReport::default();
        let (schema, rows, rules) =
            self.inspect_snapshot(target, assignment_samples, &[], &mut inspection)?;
        let rules = rules.ok_or(NotionError::Configuration)?;
        let mut plan = Self::plan_from_course_rules(input, &schema, &rows, &rules)?;
        inspection.recoverable_creates = reconcile_snapshot(&mut state, &rows).len();
        inspection.unresolved_creates = state.pending;
        plan.defer_unreconciled(&state);
        let titles = self.query_course_pages(&schema, &["title"], |_, page| {
            text(page_property(page, "title", "title")?)
        })?;
        let report = rules
            .by_key
            .into_iter()
            .map(|(key, rule)| {
                let destination = match rule.resolution {
                    Ok(course) => Ok(VerificationCourse {
                        title: titles
                            .get(&course.page_id)
                            .ok_or(NotionError::IncompleteSnapshot)?
                            .clone(),
                        term: course.term,
                        page_id: course.page_id,
                    }),
                    Err(problem) => Err(problem),
                };
                Ok(VerificationCourseRule {
                    key,
                    source_name: rule.name,
                    destination,
                })
            })
            .collect::<Result<_, NotionError>>()?;
        Ok((inspection, plan, report))
    }

    /// Audit the schema and selected pages, then read the complete assignment list.
    pub fn inspect(
        &self,
        target: &NotionTarget,
        assignment_samples: &[String],
        mappings: &[CourseMapping],
    ) -> InspectionReport {
        let mut report = InspectionReport::default();
        report.error = self
            .inspect_snapshot(target, assignment_samples, mappings, &mut report)
            .err();
        report
    }

    /// Plan against the same validated snapshot reported by the inspection.
    pub fn inspect_dry_run(
        &self,
        input: &SyncInput,
        target: &NotionTarget,
        assignment_samples: &[String],
        mappings: &[CourseMapping],
    ) -> (InspectionReport, DryRunReport) {
        let mut inspection = InspectionReport::default();
        let result = self
            .inspect_snapshot(target, assignment_samples, mappings, &mut inspection)
            .and_then(|(schema, rows, rules)| match rules {
                Some(rules) => Self::plan_from_course_rules(input, &schema, &rows, &rules),
                None => self.plan_from_rows(input, &schema, &rows, mappings),
            });
        let plan = result.unwrap_or_else(|error| {
            inspection.error = Some(error);
            DryRunReport::incomplete(input, error)
        });
        (inspection, plan)
    }

    fn inspect_snapshot(
        &self,
        target: &NotionTarget,
        assignment_samples: &[String],
        mappings: &[CourseMapping],
        report: &mut InspectionReport,
    ) -> Result<(Schema, Vec<NotionAssignment>, Option<CourseRules>), NotionError> {
        report.course_rules_configured = target.course_rules.is_some();
        let mappings = if report.course_rules_configured {
            &[]
        } else {
            mappings
        };
        let schema = self.inspect_schema(target, assignment_samples, mappings, report)?;
        let rules = target
            .course_rules
            .as_ref()
            .map(|config| self.read_course_rules(config, &schema))
            .transpose()?;
        report.course_rules = rules.as_ref().map(|rules| rules.summary);
        let rows = if rules.is_some() {
            self.query_assignments_with_manual_fields(&schema)?
        } else {
            self.query_assignments(&schema, None)?
        };
        let mut keys = HashSet::new();
        let mut duplicates = HashSet::new();
        let mut managed = 0;
        for key in rows.iter().filter_map(|row| row.sync_key.as_ref()) {
            managed += 1;
            if !keys.insert(key) {
                duplicates.insert(key);
            }
        }
        report.snapshot = Some(SnapshotSummary {
            managed,
            manual: rows.len() - managed,
            duplicate_keys: duplicates.len(),
        });
        Ok((schema, rows, rules))
    }

    fn inspect_schema(
        &self,
        target: &NotionTarget,
        assignment_samples: &[String],
        mappings: &[CourseMapping],
        report: &mut InspectionReport,
    ) -> Result<Schema, NotionError> {
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
            let links = self.complete_relation(&id, &target.properties.course, links, more)?;
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
        Schema::validate(target, (assignment_id, course_id), &assignment, &course)
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

    for (role, name, kind) in MANAGED_PROPERTIES.into_iter().chain([
        ("subject", "科目", "select"),
        ("category", "種別", "select"),
    ]) {
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
