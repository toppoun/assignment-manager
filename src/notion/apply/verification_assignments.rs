use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
};

use serde_json::{Value, json};

use super::{ApplyError, ApplyReport, WriteAccess};
use crate::notion::{
    CourseSetupVerification, DryRunReport, NotionClient, NotionError, NotionTarget, PlanAction,
    PreparedWrite, SyncInput, live_assignment_read_target,
    state::{StateStore, TargetLock},
    trial::{
        MOODLE_VERIFICATION_READER, MOODLE_VERIFICATION_WRITER, PRODUCTION_ASSIGNMENTS,
        PRODUCTION_COURSES, PRODUCTION_READER,
    },
    wire::{Schema, notion_deadline_matches, page_property, page_relations, uuid},
};

#[derive(PartialEq, Eq)]
pub struct ProtectedManualAssignment {
    id: String,
    observation: Value,
}

pub struct ProductionAssignmentWriter<'a>(&'a NotionClient);

#[derive(Debug, PartialEq, Eq)]
pub struct ProductionWriterCheckError {
    pub stage: &'static str,
    pub endpoint: Option<String>,
    pub error: NotionError,
}

pub(in crate::notion) struct ApprovedProductionCreates<'a> {
    writer: &'a NotionClient,
    requests: &'a [PreparedWrite],
    protected: &'a ProtectedManualAssignment,
}

impl ApprovedProductionCreates<'_> {
    pub(in crate::notion) fn permits(
        &self,
        writer: &NotionClient,
        request: &PreparedWrite,
    ) -> bool {
        std::ptr::eq(self.writer, writer)
            && request.method() == reqwest::Method::POST
            && request.target_id == PRODUCTION_ASSIGNMENTS
            && request.body()["parent"]["type"] == "data_source_id"
            && self.requests.contains(request)
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct AssignmentReadback {
    pub matched: usize,
    pub deadlines_present: usize,
    pub deadlines_at_minute_precision: usize,
    pub urls_present: usize,
    pub statuses: BTreeMap<String, usize>,
    pub field_mismatches: BTreeMap<&'static str, usize>,
}

impl NotionClient {
    pub fn check_production_assignment_writer(
        &self,
        target: &NotionTarget,
        writer_bot: &str,
    ) -> Result<ProductionAssignmentWriter<'_>, Vec<ProductionWriterCheckError>> {
        let mismatch = || {
            vec![ProductionWriterCheckError {
                stage: "expected_bot",
                endpoint: None,
                error: NotionError::CredentialMismatch,
            }]
        };
        let expected = uuid(writer_bot).ok_or_else(mismatch)?;
        if expected != self.production_assignment_writer_bot_id(target)? {
            return Err(mismatch());
        }
        Ok(ProductionAssignmentWriter(self))
    }

    pub fn production_assignment_writer_bot_id(
        &self,
        target: &NotionTarget,
    ) -> Result<String, Vec<ProductionWriterCheckError>> {
        let target_error = |error| {
            vec![ProductionWriterCheckError {
                stage: "target",
                endpoint: None,
                error,
            }]
        };
        let assignments_id = live_assignment_read_target(target, true).map_err(target_error)?;
        self.require_course_setup_origin().map_err(target_error)?;
        let bot_error = |error| {
            vec![ProductionWriterCheckError {
                stage: "bot_identity",
                endpoint: Some("/v1/users/me".into()),
                error,
            }]
        };
        let id = self.workspace_bot_id().map_err(bot_error)?;
        if [
            PRODUCTION_READER,
            MOODLE_VERIFICATION_READER,
            MOODLE_VERIFICATION_WRITER,
        ]
        .contains(&id.as_str())
        {
            return Err(bot_error(NotionError::CredentialMismatch));
        }
        let read_source = |stage, source_id: &str| {
            self.get_data_source(source_id)
                .map_err(|error| ProductionWriterCheckError {
                    stage,
                    endpoint: Some(format!("/v1/data_sources/{source_id}")),
                    error,
                })
        };
        let assignments = read_source("assignments_data_source", &assignments_id);
        let courses = read_source("courses_data_source", PRODUCTION_COURSES);
        let (Ok(assignments), Ok(courses)) = (&assignments, &courses) else {
            return Err([assignments.err(), courses.err()]
                .into_iter()
                .flatten()
                .collect());
        };
        Schema::validate(
            target,
            (assignments_id, PRODUCTION_COURSES.into()),
            assignments,
            courses,
        )
        .map_err(|error| {
            vec![ProductionWriterCheckError {
                stage: "schema_and_relation",
                endpoint: None,
                error,
            }]
        })?;
        Ok(id)
    }

    /// The only unkeyed row expected before and after the first production sync.
    pub fn protected_production_assignment(
        &self,
        target: &NotionTarget,
    ) -> Result<ProtectedManualAssignment, NotionError> {
        live_assignment_read_target(target, true)?;
        self.require_course_setup_origin()?;
        let schema = self.retrieve_schema(target)?;
        let data = self.get_data_source(&schema.assignments_id)?;
        let status_id = data["properties"]["状態"]["id"]
            .as_str()
            .ok_or(NotionError::SchemaMismatch)?;
        let filter = json!({"property":schema.properties.sync_key, "rich_text":{"is_empty":true}});
        let mut manual = self
            .query_filtered_data_source_pages(
                &schema.assignments_id,
                &schema.assignments_database,
                &[],
                Some(&filter),
                |id, page| {
                    let (title, deadline) = schema.manual_title_deadline(page)?;
                    let (courses, more) = page_relations(page, &schema.properties.course)?;
                    let status = page_property(page, status_id, "status")?;
                    if schema.page_key(page)?.is_some()
                        || !title.is_empty()
                        || deadline.is_some()
                        || more
                        || !courses.is_empty()
                        || status["name"] != "完了"
                        || page["last_edited_time"].as_str().is_none()
                    {
                        return Err(NotionError::Conflict);
                    }
                    Ok(ProtectedManualAssignment {
                        id: id.to_owned(),
                        observation: json!([page["last_edited_time"], page["properties"]]),
                    })
                },
            )?
            .into_values();
        let protected = manual.next().ok_or(NotionError::IncompleteSnapshot)?;
        if manual.next().is_some() {
            return Err(NotionError::Conflict);
        }
        Ok(protected)
    }

    pub fn check_verification_assignment_writer(
        &self,
        target: &NotionTarget,
    ) -> Result<(), NotionError> {
        let target = CourseSetupVerification::new(target)?;
        self.require_course_setup_origin()?;
        self.verify_bot(target.writer_bot_id())
    }

    /// Apply the displayed plan to the verification assignments data source.
    pub fn apply_verified_assignments(
        &self,
        writer: &NotionClient,
        target: &NotionTarget,
        input: &SyncInput,
        expected: &DryRunReport,
        directory: &Path,
    ) -> Result<ApplyReport, ApplyError> {
        let verification = CourseSetupVerification::new(target)?;
        self.require_course_setup_origin()?;
        writer.check_verification_assignment_writer(target)?;
        self.apply_live_assignments(
            target,
            input,
            expected,
            directory,
            &WriteAccess::VerificationAssignments {
                writer,
                properties: &target.properties,
            },
            verification.assignments_id(),
        )
    }

    pub fn apply_production_first_sync(
        &self,
        writer: &ProductionAssignmentWriter<'_>,
        target: &NotionTarget,
        input: &SyncInput,
        expected: &DryRunReport,
        directory: &Path,
        protected: &ProtectedManualAssignment,
    ) -> Result<ApplyReport, ApplyError> {
        let assignments_id = live_assignment_read_target(target, true)?;
        self.require_course_setup_origin()?;
        let counts = expected.counts_for(input);
        let candidates = input.assignments().len();
        let actions = (counts.create, counts.update, counts.unchanged);
        if candidates == 0 || (actions != (candidates, 0, 0) && actions != (0, 0, candidates)) {
            return Err(ApplyError::PlanChanged);
        }
        self.apply_live_assignments(
            target,
            input,
            expected,
            directory,
            &WriteAccess::ProductionFirstSync(ApprovedProductionCreates {
                writer: writer.0,
                requests: expected.requests(),
                protected,
            }),
            &assignments_id,
        )
    }

    fn apply_live_assignments(
        &self,
        target: &NotionTarget,
        input: &SyncInput,
        expected: &DryRunReport,
        directory: &Path,
        access: &WriteAccess<'_>,
        assignments_id: &str,
    ) -> Result<ApplyReport, ApplyError> {
        let writer = match access {
            WriteAccess::ProductionFirstSync(approval) => approval.writer,
            WriteAccess::VerificationAssignments { writer, .. } => writer,
            _ => return Err(ApplyError::PlanChanged),
        };
        let rules_config = target
            .course_rules
            .as_ref()
            .ok_or(NotionError::Configuration)?;
        let _lock = TargetLock::acquire(directory)?;
        if let WriteAccess::ProductionFirstSync(approval) = access
            && &self.protected_production_assignment(target)? != approval.protected
        {
            return Err(ApplyError::PlanChanged);
        }
        let state_path = directory.join("state.sqlite3");
        let state = StateStore::read(&state_path, assignments_id)?.unwrap_or_default();
        let schema = self.retrieve_schema(target)?;
        if schema != writer.retrieve_schema(target)? {
            return Err(ApplyError::PlanChanged);
        }
        let rules = self.read_course_rules(rules_config, &schema)?;
        let mut fresh = self.prepare_plan(schema, state, |schema, rows| {
            Self::plan_from_course_rules(input, schema, rows, &rules)
        })?;
        let counts = fresh.deltas.counts_for(input);
        if fresh.deltas != *expected
            || fresh.pending != 0
            || counts.create + counts.update + counts.unchanged != input.assignments().len()
            || fresh.deltas.plan().iter().any(|entry| {
                matches!(&entry.action, PlanAction::Update { patch, .. }
                    if patch.source.is_some() || patch.course_page_id.is_some())
            })
        {
            return Err(ApplyError::PlanChanged);
        }
        if counts.create == 0 && counts.update == 0 && fresh.recovered.is_empty() {
            return Ok(ApplyReport {
                counts,
                ..Default::default()
            });
        }

        fresh.deltas.retain_input(input);
        fresh.missing_bindings = 0;
        StateStore::initialize(&state_path, assignments_id)?;
        let store = StateStore::for_apply(&state_path, assignments_id)?;
        writer.execute_apply(fresh, &store, access)
    }

    /// Compare the current rows to the original acquisition, and inspect optional
    /// fields the source does not provide to this sync path.
    pub fn verify_synced_assignments(
        &self,
        target: &NotionTarget,
        input: &SyncInput,
    ) -> Result<AssignmentReadback, NotionError> {
        CourseSetupVerification::new(target)?;
        self.read_back_assignments(target, input)
    }

    pub fn verify_production_assignments(
        &self,
        target: &NotionTarget,
        input: &SyncInput,
        protected: &ProtectedManualAssignment,
    ) -> Result<AssignmentReadback, NotionError> {
        if &self.protected_production_assignment(target)? != protected {
            return Err(NotionError::Conflict);
        }
        self.read_back_assignments(target, input)
    }

    fn read_back_assignments(
        &self,
        target: &NotionTarget,
        input: &SyncInput,
    ) -> Result<AssignmentReadback, NotionError> {
        let rules_config = target
            .course_rules
            .as_ref()
            .ok_or(NotionError::Configuration)?;
        let schema = self.retrieve_schema(target)?;
        let rules = self.read_course_rules(rules_config, &schema)?;
        let data = self.get_data_source(&schema.assignments_id)?;
        let status_id = data["properties"]["状態"]["id"]
            .as_str()
            .ok_or(NotionError::SchemaMismatch)?;
        let properties = &schema.properties;
        let mut projection = properties.assignment_projection().to_vec();
        projection.extend([properties.submission_url.as_str(), status_id]);
        let read_pages = |projection: &[&str]| {
            self.query_data_source_pages(
                &schema.assignments_id,
                &schema.assignments_database,
                projection,
                |_, page| {
                    let (row, more) = schema.assignment(page)?;
                    let url = page_property(page, &properties.submission_url, "url")?;
                    let status = page_property(page, status_id, "status")?;
                    let url_present = match url {
                        serde_json::Value::Null => false,
                        serde_json::Value::String(url) if !url.is_empty() => true,
                        _ => return Err(NotionError::InvalidResponse),
                    };
                    let status = if status.is_null() {
                        None
                    } else {
                        Some(
                            status["name"]
                                .as_str()
                                .ok_or(NotionError::InvalidResponse)?
                                .to_owned(),
                        )
                    };
                    Ok((row, more, url_present, status))
                },
            )
        };
        let pages = match read_pages(&projection) {
            Err(NotionError::Rejected(400)) => read_pages(&[])?,
            result => result?,
        };
        let mut keys = HashSet::new();
        let mut report = AssignmentReadback::default();
        for (row, more, url, status) in pages.into_values() {
            let Some(key) = row.sync_key.as_ref() else {
                continue;
            };
            if !keys.insert(key.clone()) {
                return Err(NotionError::Conflict);
            }
            let Some(source) = input.assignments().get(key) else {
                continue;
            };
            let course = rules.resolve(source).map_err(|_| NotionError::Conflict)?;
            let deadline_matches = notion_deadline_matches(&row.deadline, &source.deadline);
            report.deadlines_at_minute_precision +=
                usize::from(deadline_matches && row.deadline != source.deadline);
            for (field, mismatch) in [
                ("title", row.title != source.title),
                ("deadline", !deadline_matches),
                ("source", row.source != Some(source.source)),
                ("course", more || row.course_page_ids.as_slice() != [course]),
            ] {
                if mismatch {
                    *report.field_mismatches.entry(field).or_default() += 1;
                }
            }
            report.matched += 1;
            report.deadlines_present += usize::from(row.deadline.is_some());
            report.urls_present += usize::from(url);
            if let Some(status) = status {
                *report.statuses.entry(status).or_default() += 1;
            }
        }
        if report.matched != input.assignments().len() {
            return Err(NotionError::IncompleteSnapshot);
        }
        Ok(report)
    }
}
