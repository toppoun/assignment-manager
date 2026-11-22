use std::{fmt::Write as _, path::Path};

use aws_lc_rs::digest::{Context, SHA256};
use reqwest::Method;

use crate::{import::OriginKind, webclass_auth::synthetic::MOODLE_SCOPE};

use super::{ApplyError, ApplyReport, ApprovedTrial, PreparedApply, WriteAccess, WriteOutcome};
use crate::notion::{
    CourseMapping, DryRunCounts, MoodleCreateTarget, NotionClient, NotionError, PreparedWrite,
    SyncInput, SyncKey, TrialRole, UniversitySource,
    state::{BindingStatus, StateStore, TargetLock},
    wire::{Schema, checked_page_id, uuid},
};

pub struct MoodleCreatePreview {
    pub counts: DryRunCounts,
    pub existing_rows: usize,
    pub other_managed_revision: String,
    pub pending: usize,
    pub can_create: bool,
}

fn selected<'a>(
    target: &MoodleCreateTarget,
    input: &'a SyncInput,
    mappings: &'a [CourseMapping],
) -> Result<(&'a SyncKey, &'a CourseMapping), ApplyError> {
    let (key, assignment) = input
        .assignments()
        .first_key_value()
        .ok_or(ApplyError::CreateBlocked)?;
    if input.assignments().len() != 1
        || assignment.source != UniversitySource::Moodle
        || assignment.deadline.is_none()
        || assignment.submission_url.is_some()
    {
        return Err(ApplyError::CreateBlocked);
    }
    let course = assignment
        .course_key
        .as_ref()
        .ok_or(ApplyError::CreateBlocked)?;
    let mut matched = mappings
        .iter()
        .filter(|mapping| &mapping.course_key == course);
    let mapping = matched.next().ok_or(ApplyError::CreateBlocked)?;
    let expected_page = target
        .course_page(&course.id)
        .ok_or(ApplyError::CreateBlocked)?;
    if course.origin != OriginKind::Moodle
        || course.scope != MOODLE_SCOPE
        || matched.next().is_some()
        || uuid(&mapping.page_id).as_deref() != Some(expected_page)
    {
        return Err(ApplyError::CreateBlocked);
    }
    Ok((key, mapping))
}

fn create_request<'a>(
    plan: &'a PreparedApply,
    key: &SyncKey,
) -> Result<&'a PreparedWrite, ApplyError> {
    let [request] = plan.deltas.requests() else {
        return Err(ApplyError::CreateBlocked);
    };
    if request.method() != Method::POST || request.key() != key {
        return Err(ApplyError::CreateBlocked);
    }
    Ok(request)
}

fn other_managed_revision(plan: &PreparedApply, key: &SyncKey) -> String {
    let mut digest = Context::new(&SHA256);
    // Pages in the complete snapshot are sorted by page ID.
    for page in &plan.pages {
        if let Some(time) = page
            .edited_at
            .filter(|_| page.row.sync_key.as_ref() != Some(key))
        {
            digest.update(page.row.page_id.as_bytes());
            digest.update(time.to_rfc3339().as_bytes());
            digest.update(b"\n");
        }
    }
    let mut text = String::new();
    for byte in digest.finish().as_ref() {
        write!(&mut text, "{byte:02x}").expect("writing to String cannot fail");
    }
    text
}

impl NotionClient {
    pub fn preview_moodle_create(
        &self,
        target: &MoodleCreateTarget,
        input: &SyncInput,
        mappings: &[CourseMapping],
        directory: &Path,
    ) -> Result<MoodleCreatePreview, ApplyError> {
        let (key, mapping) = selected(target, input, mappings)?;
        let trial = target.trial();
        let state = StateStore::read(&directory.join("state.sqlite3"), trial.assignment_id())?;
        let empty_state = state
            .as_ref()
            .is_some_and(|state| state.bindings.is_empty());
        let pending = state.as_ref().map_or(0, |state| state.pending);
        self.verify_trial_bot(trial, TrialRole::Read)?;
        let plan = self.prepare_apply(
            input,
            trial.retrieve_schema(self)?,
            std::slice::from_ref(mapping),
            state.unwrap_or_default(),
        )?;
        let counts = plan.deltas.counts_where(|candidate| candidate == key);
        if counts.unchanged == 1 {
            self.verify_moodle_course(&plan.schema, mapping)?;
        }
        Ok(MoodleCreatePreview {
            counts,
            existing_rows: plan.pages.len(),
            other_managed_revision: other_managed_revision(&plan, key),
            pending,
            can_create: empty_state && create_request(&plan, key).is_ok(),
        })
    }

    pub fn apply_moodle_create(
        &self,
        target: &MoodleCreateTarget,
        input: &SyncInput,
        mappings: &[CourseMapping],
        directory: &Path,
        approve: impl FnOnce() -> bool,
    ) -> Result<Option<ApplyReport>, ApplyError> {
        let (key, mapping) = selected(target, input, mappings)?;
        let trial = target.trial();
        let _lock = TargetLock::acquire(directory)?;
        let store = StateStore::for_apply(&directory.join("state.sqlite3"), trial.assignment_id())?;
        let state = store.summary()?;
        // A prior attempt blocks this trial even if a different fixture UID is selected.
        if !state.bindings.is_empty() {
            return Err(ApplyError::CreateBlocked);
        }
        self.verify_trial_bot(trial, TrialRole::Write)?;
        let mapping = std::slice::from_ref(mapping);
        let plan = self.prepare_apply(input, trial.retrieve_schema(self)?, mapping, state)?;
        create_request(&plan, key)?;
        if !approve() {
            return Ok(None);
        }
        let fresh = self.prepare_apply(
            input,
            trial.retrieve_schema(self)?,
            mapping,
            store.summary()?,
        )?;
        if fresh != plan {
            return Err(ApplyError::PlanChanged);
        }
        if !self.query_assignments(&fresh.schema, Some(key))?.is_empty() {
            return Err(ApplyError::PlanChanged);
        }
        let access = WriteAccess::MoodleCreate {
            approval: ApprovedTrial(trial),
            key,
        };
        let outcome = self.send_reserved_create(
            &fresh.schema,
            &store,
            create_request(&fresh, key)?,
            &access,
        )?;
        let mut report = ApplyReport::default();
        match outcome {
            WriteOutcome::Applied => report.counts.create = 1,
            WriteOutcome::Conflict => report.counts.conflict = 1,
            WriteOutcome::Deferred(error) => {
                report.counts.deferred = 1;
                report.error = error;
            }
        }
        report.pending = store.summary()?.pending;
        Ok(Some(report))
    }

    /// Recover only an existing pending binding, using a verified reader and no Notion mutations.
    pub fn reconcile_moodle_create(
        &self,
        target: &MoodleCreateTarget,
        input: &SyncInput,
        mappings: &[CourseMapping],
        directory: &Path,
    ) -> Result<(), ApplyError> {
        let (key, mapping) = selected(target, input, mappings)?;
        let trial = target.trial();
        let _lock = TargetLock::acquire(directory)?;
        let store = StateStore::for_apply(&directory.join("state.sqlite3"), trial.assignment_id())?;
        let state = store.summary()?;
        if state.bindings.len() != 1
            || state
                .bindings
                .get(key)
                .is_none_or(|binding| binding.status == BindingStatus::Linked)
        {
            return Err(ApplyError::CreateBlocked);
        }
        self.verify_trial_bot(trial, TrialRole::Read)?;
        let plan = self.prepare_apply(
            input,
            trial.retrieve_schema(self)?,
            std::slice::from_ref(mapping),
            state,
        )?;
        let [(recovered_key, page)] = plan.recovered.as_slice() else {
            return Err(ApplyError::CreateBlocked);
        };
        let counts = plan.deltas.counts_where(|candidate| candidate == key);
        if recovered_key != key || counts.unchanged != 1 || counts.course_relation_mismatch != 0 {
            return Err(ApplyError::CreateBlocked);
        }
        self.verify_moodle_course(&plan.schema, mapping)?;
        store.confirm(key, page)?;
        Ok(())
    }

    fn verify_moodle_course(
        &self,
        schema: &Schema,
        mapping: &CourseMapping,
    ) -> Result<(), NotionError> {
        let id = uuid(&mapping.page_id).ok_or(NotionError::Configuration)?;
        let page = self.retrieve_page(&id, &[])?;
        if checked_page_id(&page, &schema.courses_id, &schema.courses_database)? != id {
            return Err(NotionError::InvalidResponse);
        }
        Ok(())
    }
}
