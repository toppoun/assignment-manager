use std::path::Path;

use reqwest::Method;

use crate::{
    notion::{
        AssignmentPatch, CourseMapping, DryRunCounts, MoodleCreateTarget, NotionClient, PlanAction,
        PreparedWrite, SyncInput, SyncKey, TrialRole, TrialTarget,
        state::{BindingStatus, StateStore, StateSummary, TargetLock},
    },
    webclass_auth::synthetic::MOODLE_TRIAL_UPDATE_UID,
};

use super::{
    ApplyError, ApplyReport, ApprovedTrial, Observation, PreparedApply, WriteAccess, WriteOutcome,
    moodle_create::{other_managed_revision, selected},
};

pub struct MoodleUpdatePreview {
    pub counts: DryRunCounts,
    pub existing_rows: usize,
    pub other_managed_revision: String,
    pub can_update: bool,
}

fn selected_update<'a>(
    target: &MoodleCreateTarget,
    input: &'a SyncInput,
    mappings: &'a [CourseMapping],
) -> Result<(&'a SyncKey, &'a CourseMapping), ApplyError> {
    let (key, mapping) =
        selected(target, input, mappings).map_err(|_| ApplyError::UpdateBlocked)?;
    if SyncKey::fixture_moodle(MOODLE_TRIAL_UPDATE_UID)
        .as_ref()
        .ok()
        != Some(key)
    {
        return Err(ApplyError::UpdateBlocked);
    }
    Ok((key, mapping))
}

fn linked_state(
    directory: &Path,
    trial: &TrialTarget,
    key: &SyncKey,
) -> Result<(StateSummary, String), ApplyError> {
    let state = StateStore::read(&directory.join("state.sqlite3"), trial.assignment_id())?
        .ok_or(ApplyError::UpdateBlocked)?;
    let page_id = state
        .bindings
        .get(key)
        .filter(|binding| binding.status == BindingStatus::Linked)
        .and_then(|binding| binding.page_id.clone())
        .ok_or(ApplyError::UpdateBlocked)?;
    Ok((state, page_id))
}

fn update_request<'a>(
    plan: &'a PreparedApply,
    key: &SyncKey,
    page_id: &str,
) -> Result<(&'a Observation, &'a AssignmentPatch, &'a PreparedWrite), ApplyError> {
    let [request] = plan.deltas.requests() else {
        return Err(ApplyError::UpdateBlocked);
    };
    if request.method() != Method::PATCH
        || request.key() != key
        || request.path().strip_prefix("/v1/pages/") != Some(page_id)
    {
        return Err(ApplyError::UpdateBlocked);
    }
    let entry = plan
        .deltas
        .plan()
        .iter()
        .find(|entry| &entry.key == key)
        .ok_or(ApplyError::UpdateBlocked)?;
    let PlanAction::Update {
        page_id: planned_page,
        patch,
    } = &entry.action
    else {
        return Err(ApplyError::UpdateBlocked);
    };
    // A one-field delta could be the result of an earlier partial update.
    if planned_page != page_id
        || entry.course_relation_mismatch
        || patch.title.is_none()
        || !matches!(patch.deadline, Some(Some(_)))
        || patch.source.is_some()
        || patch.course_page_id.is_some()
    {
        return Err(ApplyError::UpdateBlocked);
    }
    let baseline = plan
        .pages
        .iter()
        .find(|page| page.row.page_id == page_id && page.row.sync_key.as_ref() == Some(key))
        .ok_or(ApplyError::UpdateBlocked)?;
    Ok((baseline, patch, request))
}

impl NotionClient {
    pub fn preview_moodle_update(
        &self,
        target: &MoodleCreateTarget,
        input: &SyncInput,
        mappings: &[CourseMapping],
        directory: &Path,
    ) -> Result<MoodleUpdatePreview, ApplyError> {
        let (key, mapping) = selected_update(target, input, mappings)?;
        let trial = target.trial();
        let (state, page_id) = linked_state(directory, trial, key)?;
        self.verify_trial_bot(trial, TrialRole::Read)?;
        let plan = self.prepare_apply(
            input,
            trial.retrieve_schema(self)?,
            std::slice::from_ref(mapping),
            state,
        )?;
        let counts = plan.deltas.counts_where(|candidate| candidate == key);
        let can_update = update_request(&plan, key, &page_id).is_ok();
        if can_update || (counts.unchanged == 1 && counts.course_relation_mismatch == 0) {
            self.verify_moodle_course(&plan.schema, mapping)?;
        }
        Ok(MoodleUpdatePreview {
            counts,
            existing_rows: plan.pages.len(),
            other_managed_revision: other_managed_revision(&plan, key),
            can_update,
        })
    }

    pub fn apply_moodle_update(
        &self,
        target: &MoodleCreateTarget,
        input: &SyncInput,
        mappings: &[CourseMapping],
        directory: &Path,
        approve: impl FnOnce() -> bool,
    ) -> Result<Option<ApplyReport>, ApplyError> {
        let (key, mapping) = selected_update(target, input, mappings)?;
        let trial = target.trial();
        let _lock = TargetLock::acquire(directory)?;
        let (state, page_id) = linked_state(directory, trial, key)?;
        self.verify_trial_bot(trial, TrialRole::Write)?;
        let mappings = std::slice::from_ref(mapping);
        let plan = self.prepare_apply(input, trial.retrieve_schema(self)?, mappings, state)?;
        update_request(&plan, key, &page_id)?;
        self.verify_moodle_course(&plan.schema, mapping)?;
        if !approve() {
            return Ok(None);
        }
        let (state, linked_page) = linked_state(directory, trial, key)?;
        if linked_page != page_id {
            return Err(ApplyError::PlanChanged);
        }
        let fresh = self.prepare_apply(input, trial.retrieve_schema(self)?, mappings, state)?;
        if fresh != plan {
            return Err(ApplyError::PlanChanged);
        }
        let (baseline, patch, request) = update_request(&fresh, key, &page_id)?;
        self.verify_moodle_course(&fresh.schema, mapping)?;
        let access = WriteAccess::MoodleUpdate {
            approval: ApprovedTrial(trial),
            key,
            page_id: &page_id,
            properties: [
                &fresh.schema.properties.title,
                &fresh.schema.properties.deadline,
            ],
        };
        let outcome = self.apply_update(&fresh.schema, baseline, patch, request, &access);
        let mut report = ApplyReport::default();
        match outcome {
            WriteOutcome::Applied => report.counts.update = 1,
            WriteOutcome::Conflict => report.counts.conflict = 1,
            WriteOutcome::Deferred(error) => {
                report.counts.deferred = 1;
                report.error = error;
            }
        }
        Ok(Some(report))
    }
}
