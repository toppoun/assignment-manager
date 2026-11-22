use std::{fmt, path::Path};

use reqwest::Method;

use super::{
    ApplyError, ApplyReport, ApprovedTrial, Observation, PreparedApply, WriteAccess, WriteOutcome,
};
use crate::notion::{
    MoodlePreviewMappings, NotionClient, NotionError, NotionTarget, PlanAction, PreparedWrite,
    SyncAssignment, SyncInput, SyncKey, TrialRole, TrialTarget, VerificationTarget,
    state::{BindingStatus, StateStore, TargetLock},
    trial::MOODLE_VERIFICATION_WRITER,
    wire::{Schema, checked_page_id, page_property, text},
};

/// Real Moodle input can write only to the fixed verification pair, with the
/// existing verification writer. Synthetic trials use a different entry point.
pub struct MoodleLiveTarget(TrialTarget);

impl MoodleLiveTarget {
    pub fn new(notion: NotionTarget) -> Result<Self, super::NotionError> {
        Ok(Self(TrialTarget::from_verification(
            VerificationTarget::moodle_integration(notion)?,
            MOODLE_VERIFICATION_WRITER,
        )?))
    }

    pub fn trial(&self) -> &TrialTarget {
        &self.0
    }
}

pub struct MoodleLiveCreateChoices<'a> {
    pub candidate_count: usize,
    pub existing_rows: usize,
    pub eligible: Vec<&'a SyncAssignment>,
}

#[derive(Default, PartialEq, Eq)]
pub struct MoodleLiveCreateResult {
    pub report: ApplyReport,
    verification: Option<MoodleLiveCreateVerification>,
}

impl fmt::Debug for MoodleLiveCreateResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MoodleLiveCreateResult")
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

#[derive(PartialEq, Eq)]
struct MoodleLiveCreateVerification {
    schema: Schema,
    existing: Vec<Observation>,
    key: SyncKey,
    page_id: String,
    course_page_id: String,
    assignment: SyncAssignment,
}

impl NotionClient {
    /// Evaluate all candidates but dispatch only the one chosen and approved
    /// for this run. A pending intent blocks new creates until reconciliation.
    pub fn create_one_live_moodle(
        &self,
        target: &MoodleLiveTarget,
        input: &SyncInput,
        mappings: &MoodlePreviewMappings<'_>,
        directory: &Path,
        choose: impl FnOnce(MoodleLiveCreateChoices<'_>) -> Option<usize>,
        approve: impl FnOnce(&SyncAssignment, usize) -> bool,
    ) -> Result<Option<MoodleLiveCreateResult>, ApplyError> {
        if !mappings.coverage(input)?.ready() {
            return Err(ApplyError::CreateBlocked);
        }
        let trial = target.trial();
        let _lock = TargetLock::acquire(directory)?;
        let store = StateStore::for_apply(&directory.join("state.sqlite3"), trial.assignment_id())?;
        let state = store.summary()?;
        if state.pending != 0 {
            return Err(ApplyError::CreateBlocked);
        }
        self.verify_trial_bot(trial, TrialRole::Write)?;
        let plan =
            self.prepare_moodle_plan(trial.retrieve_schema(self)?, input, mappings, state, true)?;
        let counts = plan.deltas.counts_for(input);
        if counts.conflict != 0 || counts.deferred != 0 || counts.course_relation_mismatch != 0 {
            return Err(ApplyError::CreateBlocked);
        }
        let eligible: Vec<_> = plan
            .deltas
            .plan()
            .iter()
            .filter_map(|item| match &item.action {
                PlanAction::Create {
                    assignment,
                    course_page_id: Some(course_page_id),
                } => Some((&item.key, assignment, course_page_id)),
                _ => None,
            })
            .collect();
        if eligible.is_empty() {
            return Ok(Some(MoodleLiveCreateResult::default()));
        }
        let candidate_count = input.assignments().len();
        let Some(index) = choose(MoodleLiveCreateChoices {
            candidate_count,
            existing_rows: plan.pages.len(),
            eligible: eligible
                .iter()
                .map(|(_, assignment, _)| *assignment)
                .collect(),
        }) else {
            return Ok(None);
        };
        let &(key, assignment, course_page_id) =
            eligible.get(index).ok_or(ApplyError::CreateBlocked)?;
        selected_request(&plan, key)?;
        if !approve(assignment, candidate_count - 1) {
            return Ok(None);
        }
        let state = store.summary()?;
        if state.pending != 0 {
            return Err(ApplyError::PlanChanged);
        }
        let fresh =
            self.prepare_moodle_plan(trial.retrieve_schema(self)?, input, mappings, state, true)?;
        if fresh != plan || !self.query_assignments(&fresh.schema, Some(key))?.is_empty() {
            return Err(ApplyError::PlanChanged);
        }
        let access = WriteAccess::MoodleCreate {
            approval: ApprovedTrial(trial),
            key,
        };
        let outcome = self.send_reserved_create(
            &fresh.schema,
            &store,
            selected_request(&fresh, key)?,
            &access,
        )?;
        let mut report = ApplyReport::default();
        let verification = match outcome {
            WriteOutcome::Applied => {
                report.counts.create = 1;
                let binding = store
                    .binding(key)?
                    .ok_or(super::StateError::InvalidTransition)?;
                let page_id = binding
                    .page_id
                    .filter(|_| binding.status == BindingStatus::Linked)
                    .ok_or(super::StateError::InvalidTransition)?;
                Some(MoodleLiveCreateVerification {
                    schema: fresh.schema,
                    existing: fresh.pages,
                    key: key.clone(),
                    page_id,
                    course_page_id: course_page_id.clone(),
                    assignment: assignment.clone(),
                })
            }
            WriteOutcome::Deferred(Some(error)) => {
                report.counts.deferred = 1;
                report.error = Some(error);
                None
            }
            WriteOutcome::Conflict | WriteOutcome::Deferred(None) => {
                return Err(ApplyError::PlanChanged);
            }
        };
        report.pending = store.summary()?.pending;
        Ok(Some(MoodleLiveCreateResult {
            report,
            verification,
        }))
    }

    /// Compare the in-memory pre-create snapshot and all five expected fields.
    /// No Moodle fetch or Notion mutation is possible through this method.
    pub fn verify_one_live_moodle(
        &self,
        target: &MoodleLiveTarget,
        result: &MoodleLiveCreateResult,
    ) -> Result<(usize, usize), NotionError> {
        let expected = result
            .verification
            .as_ref()
            .ok_or(NotionError::Configuration)?;
        let trial = target.trial();
        self.verify_trial_bot(trial, TrialRole::Read)?;
        let schema = trial.retrieve_schema(self)?;
        if schema != expected.schema {
            return Err(NotionError::SchemaMismatch);
        }
        let course = self.retrieve_page(&expected.course_page_id, &["title"])?;
        let category = expected
            .assignment
            .course_key
            .as_ref()
            .ok_or(NotionError::Configuration)?;
        if checked_page_id(&course, &schema.courses_id, &schema.courses_database)?
            != expected.course_page_id
            || text(page_property(&course, "title", "title")?)? != category.id
        {
            return Err(NotionError::CourseMappingOutsideTarget);
        }
        let mut pages = self.query_assignment_pages(&schema, None, |page, row| {
            Observation::new(&schema, page, row)
        })?;
        let mut matches = pages
            .iter()
            .enumerate()
            .filter(|(_, page)| page.row.sync_key.as_ref() == Some(&expected.key));
        let (Some((index, _)), None) = (matches.next(), matches.next()) else {
            return Err(NotionError::Conflict);
        };
        let created = pages.remove(index).row;
        if created.page_id != expected.page_id
            || created.source != Some(expected.assignment.source)
            || created.title != expected.assignment.title
            || created.deadline != expected.assignment.deadline
            || created.course_page_ids != [expected.course_page_id.as_str()]
        {
            return Err(NotionError::Conflict);
        }
        pages.sort_unstable_by(|a, b| a.row.page_id.cmp(&b.row.page_id));
        if pages != expected.existing || pages.iter().any(|page| page.edited_at.is_none()) {
            return Err(NotionError::Conflict);
        }
        Ok((pages.len(), pages.len() + 1))
    }

    /// Attach an unresolved intent only when the current source and the unique
    /// matching Notion page still agree. This path has no write capability for Notion.
    pub fn reconcile_one_live_moodle(
        &self,
        target: &MoodleLiveTarget,
        input: &SyncInput,
        mappings: &MoodlePreviewMappings<'_>,
        directory: &Path,
    ) -> Result<(), ApplyError> {
        if !mappings.coverage(input)?.ready() {
            return Err(ApplyError::CreateBlocked);
        }
        let trial = target.trial();
        let _lock = TargetLock::acquire(directory)?;
        let store = StateStore::for_apply(&directory.join("state.sqlite3"), trial.assignment_id())?;
        let state = store.summary()?;
        if state.pending != 1
            || state.bindings.iter().any(|(key, binding)| {
                binding.status != BindingStatus::Linked && !input.assignments().contains_key(key)
            })
        {
            return Err(ApplyError::CreateBlocked);
        }
        self.verify_trial_bot(trial, TrialRole::Read)?;
        let plan =
            self.prepare_moodle_plan(trial.retrieve_schema(self)?, input, mappings, state, true)?;
        let [(key, page)] = plan.recovered.as_slice() else {
            return Err(ApplyError::CreateBlocked);
        };
        let counts = plan.deltas.counts_where(|candidate| candidate == key);
        if counts.unchanged != 1 || counts.course_relation_mismatch != 0 {
            return Err(ApplyError::CreateBlocked);
        }
        store.confirm(key, page)?;
        Ok(())
    }
}

fn selected_request<'a>(
    plan: &'a PreparedApply,
    key: &SyncKey,
) -> Result<&'a PreparedWrite, ApplyError> {
    let projection = plan.schema.properties.assignment_projection();
    plan.deltas
        .requests()
        .iter()
        .find(|request| {
            request.key() == key
                && request.method() == Method::POST
                && request.body()["properties"]
                    .as_object()
                    .is_some_and(|properties| {
                        properties.len() == projection.len()
                            && projection.iter().all(|id| properties.contains_key(*id))
                    })
        })
        .ok_or(ApplyError::CreateBlocked)
}
