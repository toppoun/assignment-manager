use std::path::Path;

use reqwest::Method;

use super::{ApplyError, ApplyReport, ApprovedTrial, PreparedApply, WriteAccess, WriteOutcome};
use crate::notion::{
    MoodlePreviewMappings, NotionClient, NotionTarget, PlanAction, PreparedWrite, SyncAssignment,
    SyncInput, SyncKey, TrialRole, TrialTarget, VerificationTarget,
    state::{BindingStatus, StateStore, TargetLock},
    trial::MOODLE_VERIFICATION_WRITER,
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
    pub eligible: Vec<&'a SyncAssignment>,
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
    ) -> Result<Option<ApplyReport>, ApplyError> {
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
                PlanAction::Create { assignment, .. } => Some((&item.key, assignment)),
                _ => None,
            })
            .collect();
        if eligible.is_empty() {
            return Ok(Some(ApplyReport::default()));
        }
        let candidate_count = input.assignments().len();
        let Some(index) = choose(MoodleLiveCreateChoices {
            candidate_count,
            eligible: eligible.iter().map(|(_, assignment)| *assignment).collect(),
        }) else {
            return Ok(None);
        };
        let &(key, assignment) = eligible.get(index).ok_or(ApplyError::CreateBlocked)?;
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
        match outcome {
            WriteOutcome::Applied => report.counts.create = 1,
            WriteOutcome::Deferred(Some(error)) => {
                report.counts.deferred = 1;
                report.error = Some(error);
            }
            WriteOutcome::Conflict | WriteOutcome::Deferred(None) => {
                return Err(ApplyError::PlanChanged);
            }
        }
        report.pending = store.summary()?.pending;
        Ok(Some(report))
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
