use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use chrono::{DateTime, Utc};
use serde_json::Value;

#[cfg(test)]
use super::http_tests::trial_tests::recovery_tests::pause_create_at;
use super::{
    AssignmentPatch, CourseMapping, DryRunCounts, DryRunReport, NotionAssignment, NotionClient,
    NotionError, PlanAction, PlannedAssignment, PreparedWrite, SyncInput, SyncKey, TrialAssignment,
    TrialRole, TrialTarget, VerificationTarget,
    state::{BindingStatus, StateError, StateStore, StateSummary, TargetLock},
    wire::{Schema, checked_page_id},
};

#[derive(Debug, PartialEq, Eq)]
pub enum ApplyError {
    Notion(NotionError),
    State(StateError),
    PlanChanged,
}

impl From<NotionError> for ApplyError {
    fn from(error: NotionError) -> Self {
        Self::Notion(error)
    }
}

impl From<StateError> for ApplyError {
    fn from(error: StateError) -> Self {
        Self::State(error)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub counts: DryRunCounts,
    pub recovered: usize,
    pub pending: usize,
    pub manual_candidates: usize,
    pub error: Option<NotionError>,
}

pub struct ApplyPreview<'a> {
    plan: &'a PreparedApply,
}

impl ApplyPreview<'_> {
    pub fn assignments(&self) -> &[PlannedAssignment] {
        self.plan.deltas.plan()
    }

    pub fn summary(&self) -> ApplyReport {
        self.plan.summary()
    }
}

pub(super) enum WriteAccess<'a> {
    #[cfg(any(test, feature = "synthetic-https-fixture"))]
    Mock,
    Trial(ApprovedTrial<'a>),
}

pub(super) struct ApprovedTrial<'a>(&'a TrialTarget);

impl ApprovedTrial<'_> {
    pub(super) fn target(&self) -> &TrialTarget {
        self.0
    }
}

#[derive(PartialEq, Eq)]
struct PreparedApply {
    schema: Schema,
    pages: Vec<Observation>,
    deltas: DryRunReport,
    manual_candidates: Vec<(SyncKey, String)>,
    recovered: Vec<(SyncKey, String)>,
    pending: usize,
    missing_bindings: usize,
}

impl PreparedApply {
    fn summary(&self) -> ApplyReport {
        let mut counts = self.deltas.counts();
        counts.deferred += self.missing_bindings;
        ApplyReport {
            counts,
            recovered: self.recovered.len(),
            pending: self.pending,
            manual_candidates: self.manual_candidates.len(),
            error: None,
        }
    }
}

#[derive(PartialEq, Eq)]
struct Observation {
    row: NotionAssignment,
    edited_at: Option<DateTime<Utc>>,
    title_content: Option<Value>,
}

impl Observation {
    fn new(schema: &Schema, page: &Value, mut row: NotionAssignment) -> Result<Self, NotionError> {
        let (edited_at, title_content) = if row.sync_key.is_some() {
            let edited_at = page["last_edited_time"]
                .as_str()
                .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
                .ok_or(NotionError::InvalidResponse)?
                .to_utc();
            (Some(edited_at), Some(schema.title_content(page)?.clone()))
        } else {
            // An unreadable manual row cannot safely rule out a duplicate candidate.
            (row.title, row.deadline) = schema.manual_title_deadline(page)?;
            (None, None)
        };
        Ok(Self {
            row,
            edited_at,
            title_content,
        })
    }

    fn agrees_with(&self, other: &Self) -> bool {
        self.row == other.row
            && self.edited_at == other.edited_at
            && self.title_content == other.title_content
    }
}

enum WriteOutcome {
    Applied,
    Conflict,
    Deferred(Option<NotionError>),
}

impl NotionClient {
    /// Compute the same reconciled plan used before apply, using an in-memory
    /// state snapshot. Counts cover the current input, including blocked rows.
    pub fn preview_verification(
        &self,
        target: &VerificationTarget,
        input: &SyncInput,
        mappings: &[CourseMapping],
        state: StateSummary,
    ) -> Result<DryRunCounts, NotionError> {
        target.verify_reader(self)?;
        let plan = self.prepare_apply(input, target.retrieve_schema(self)?, mappings, state)?;
        Ok(plan
            .deltas
            .counts_where(|key| input.assignments().contains_key(key)))
    }

    pub fn preview_trial(
        &self,
        trial: &TrialTarget,
        assignment: &TrialAssignment,
        directory: &Path,
    ) -> Result<ApplyReport, ApplyError> {
        let (input, mappings) = assignment.input()?;
        let _lock = TargetLock::acquire(directory)?;
        let state = StateStore::read(&directory.join("state.sqlite3"), trial.assignment_id())?
            .ok_or(StateError::InvalidDatabase)?;
        self.verify_trial_bot(trial, TrialRole::Read)?;
        let plan = self.prepare_apply(&input, trial.retrieve_schema(self)?, &mappings, state)?;
        Ok(plan.summary())
    }

    /// Apply the plan shown to the caller. Recheck it after interactive approval,
    /// while retaining the same target lock and state connection.
    pub fn apply_trial(
        &self,
        trial: &TrialTarget,
        assignment: &TrialAssignment,
        directory: &Path,
        approve: impl FnOnce(ApplyPreview<'_>) -> bool,
    ) -> Result<Option<ApplyReport>, ApplyError> {
        let (input, mappings) = assignment.input()?;
        let _lock = TargetLock::acquire(directory)?;
        let store = StateStore::for_apply(&directory.join("state.sqlite3"), trial.assignment_id())?;
        self.verify_trial_bot(trial, TrialRole::Write)?;
        let prepare = || -> Result<PreparedApply, ApplyError> {
            Ok(self.prepare_apply(
                &input,
                trial.retrieve_schema(self)?,
                &mappings,
                store.summary()?,
            )?)
        };
        let plan = prepare()?;
        if !approve(ApplyPreview { plan: &plan }) {
            return Ok(None);
        }
        let fresh = prepare()?;
        if fresh != plan {
            return Err(ApplyError::PlanChanged);
        }
        self.execute_apply(fresh, &store, &WriteAccess::Trial(ApprovedTrial(trial)))
            .map(Some)
    }

    #[cfg(any(test, feature = "synthetic-https-fixture"))]
    pub(super) fn apply(
        &self,
        input: &SyncInput,
        target: &super::NotionTarget,
        mappings: &[CourseMapping],
        directory: &Path,
    ) -> Result<ApplyReport, ApplyError> {
        self.require_mock_writes().map_err(ApplyError::Notion)?;
        let target_id = target.validated_id().map_err(ApplyError::Notion)?;
        let _lock = TargetLock::acquire(directory)?;
        let store = StateStore::for_apply(&directory.join("state.sqlite3"), &target_id)?;
        let state = store.summary()?;
        let plan = match self.retrieve_schema(target) {
            Ok(schema) => self.prepare_apply(input, schema, mappings, state),
            Err(error) => {
                return Ok(unavailable(input, &state, error));
            }
        };
        match plan {
            Ok(plan) => self.execute_apply(plan, &store, &WriteAccess::Mock),
            Err(error) => Ok(unavailable(input, &store.summary()?, error)),
        }
    }

    fn prepare_apply(
        &self,
        input: &SyncInput,
        schema: Schema,
        mappings: &[CourseMapping],
        mut state: StateSummary,
    ) -> Result<PreparedApply, NotionError> {
        let mut pages = self.query_assignment_pages(&schema, None, |page, row| {
            Observation::new(&schema, page, row)
        })?;
        pages.sort_unstable_by(|a, b| a.row.page_id.cmp(&b.row.page_id));
        let recovered = recoverable_intents(&state, &pages);
        for (key, page) in &recovered {
            let binding = state.bindings.get_mut(key).expect("known intent");
            binding.status = BindingStatus::Linked;
            binding.page_id = Some(page.clone());
        }
        let rows: Vec<_> = pages.iter().map(|page| page.row.clone()).collect();
        let mut deltas = self.plan_from_rows(input, &schema, &rows, mappings)?;
        let manual_candidates = deltas.defer_manual_candidates(&rows);
        deltas.defer_unreconciled(&state);
        let planned_keys: HashSet<_> = deltas.plan().iter().map(|entry| &entry.key).collect();
        let missing_bindings = state
            .bindings
            .keys()
            .filter(|key| !planned_keys.contains(key))
            .count();
        Ok(PreparedApply {
            schema,
            pages,
            deltas,
            manual_candidates,
            pending: state.pending - recovered.len(),
            recovered,
            missing_bindings,
        })
    }

    fn execute_apply(
        &self,
        plan: PreparedApply,
        store: &StateStore,
        access: &WriteAccess<'_>,
    ) -> Result<ApplyReport, ApplyError> {
        let mut report = ApplyReport {
            recovered: plan.recovered.len(),
            manual_candidates: plan.manual_candidates.len(),
            ..ApplyReport::default()
        };
        store.record_manual_candidates(&plan.manual_candidates)?;
        for (key, id) in &plan.recovered {
            store.confirm(key, id)?;
        }
        let requests: HashMap<_, _> = plan
            .deltas
            .requests()
            .iter()
            .map(|request| (request.key(), request))
            .collect();
        let by_id: HashMap<_, _> = plan
            .pages
            .iter()
            .map(|page| (page.row.page_id.as_str(), page))
            .collect();
        let mut stop_writes = false;
        for entry in plan.deltas.plan() {
            report.counts.course_relation_mismatch += usize::from(entry.course_relation_mismatch);
            let action = &entry.action;
            let outcome = match action {
                PlanAction::Deferred(_) => WriteOutcome::Deferred(None),
                PlanAction::Conflict(_) => WriteOutcome::Conflict,
                PlanAction::Unchanged { page_id } => {
                    store.confirm(&entry.key, page_id)?;
                    report.counts.unchanged += 1;
                    continue;
                }
                _ if stop_writes => WriteOutcome::Deferred(None),
                PlanAction::Create { .. } => {
                    self.apply_create(&plan.schema, store, requests[&entry.key], access)?
                }
                PlanAction::Update { page_id, patch } => {
                    store.confirm(&entry.key, page_id)?;
                    self.apply_update(
                        &plan.schema,
                        by_id[page_id.as_str()],
                        patch,
                        requests[&entry.key],
                        access,
                    )
                }
            };
            match outcome {
                WriteOutcome::Applied if matches!(action, PlanAction::Create { .. }) => {
                    report.counts.create += 1
                }
                WriteOutcome::Applied => report.counts.update += 1,
                WriteOutcome::Conflict => report.counts.conflict += 1,
                WriteOutcome::Deferred(error) => {
                    report.counts.deferred += 1;
                    if let Some(error) = error {
                        report.error.get_or_insert(error);
                        stop_writes |= stops_writes(error);
                    }
                }
            }
        }
        report.counts.deferred += plan.missing_bindings;
        report.pending = store.summary()?.pending;
        Ok(report)
    }

    fn apply_create(
        &self,
        schema: &Schema,
        store: &StateStore,
        request: &PreparedWrite,
        access: &WriteAccess<'_>,
    ) -> Result<WriteOutcome, StateError> {
        let key = request.key();
        // A fresh, fully paginated exact match reduces the race with external writers.
        // The full snapshot remains a prerequisite for reaching this path.
        let matches = match self.query_assignments(schema, Some(key)) {
            Ok(matches) => matches,
            Err(error) => return Ok(WriteOutcome::Deferred(Some(error))),
        };
        match matches.as_slice() {
            [] => {}
            [row] => {
                if !store.is_owned_by_another_key(&row.page_id, key)? {
                    store.confirm(key, &row.page_id)?;
                }
                // A new row invalidates the create plan. Plan any update in a fresh run.
                return Ok(WriteOutcome::Deferred(None));
            }
            _ => return Ok(WriteOutcome::Conflict),
        }
        if !store.reserve_create(key)? {
            return Ok(WriteOutcome::Deferred(None));
        }
        #[cfg(test)]
        pause_create_at("intent_saved");
        match self.send_write(request, access) {
            Ok(page) => {
                let id =
                    checked_page_id(&page, &schema.assignments_id, &schema.assignments_database)
                        .ok()
                        .filter(|_| schema.page_key(&page).ok().flatten().as_ref() == Some(key));
                if let Some(id) = id {
                    if !store.is_owned_by_another_key(&id, key)? {
                        #[cfg(test)]
                        pause_create_at("response_received");
                        store.confirm(key, &id)?;
                        return Ok(WriteOutcome::Applied);
                    }
                    store.mark_unknown(key)?;
                    return Ok(WriteOutcome::Deferred(Some(NotionError::Conflict)));
                }
                store.mark_unknown(key)?;
                Ok(WriteOutcome::Deferred(Some(NotionError::InvalidResponse)))
            }
            Err(error) => {
                if explicitly_rejected(error) {
                    store.release_rejected_create(key)?;
                } else {
                    store.mark_unknown(key)?;
                }
                Ok(WriteOutcome::Deferred(Some(error)))
            }
        }
    }

    fn apply_update(
        &self,
        schema: &Schema,
        baseline: &Observation,
        patch: &AssignmentPatch,
        request: &PreparedWrite,
        access: &WriteAccess<'_>,
    ) -> WriteOutcome {
        let verify = || -> Result<Observation, NotionError> {
            let page = self.retrieve_page(
                &baseline.row.page_id,
                &schema.properties.assignment_projection(),
            )?;
            let (row, more) = schema.assignment(&page)?;
            if row.page_id != baseline.row.page_id || row.sync_key.as_ref() != Some(request.key()) {
                return Err(NotionError::Conflict);
            }
            let row = self.with_complete_relation(schema, row, more)?;
            Observation::new(schema, &page, row)
        };
        match verify() {
            Ok(current) if current.agrees_with(baseline) => {}
            Ok(_) => return WriteOutcome::Deferred(Some(NotionError::Conflict)),
            Err(error) => return WriteOutcome::Deferred(Some(error)),
        }
        let failure = match self.send_write(request, access) {
            Ok(page) => {
                if let Ok((row, more)) = schema.assignment(&page)
                    && row.page_id == baseline.row.page_id
                    && row.sync_key.as_ref() == Some(request.key())
                    && !(more && patch.course_page_id.is_some())
                    && patch_matches(patch, &row)
                {
                    return WriteOutcome::Applied;
                }
                NotionError::InvalidResponse
            }
            Err(error) if explicitly_rejected(error) => return WriteOutcome::Deferred(Some(error)),
            Err(error) => error,
        };
        // Never repeat this PATCH. If the response was ambiguous, account for the
        // intended values only when another read verifies them on the same page.
        match verify() {
            Ok(current) if patch_matches(patch, &current.row) => WriteOutcome::Applied,
            _ => WriteOutcome::Deferred(Some(failure)),
        }
    }
}

fn recoverable_intents(state: &StateSummary, pages: &[Observation]) -> Vec<(SyncKey, String)> {
    let known_pages = state.page_owners();
    let mut matches: HashMap<_, Vec<_>> = HashMap::new();
    for page in pages {
        if let Some(key) = &page.row.sync_key {
            matches
                .entry(key)
                .or_default()
                .push(page.row.page_id.as_str());
        }
    }
    let mut recovered = Vec::new();
    for (key, binding) in &state.bindings {
        if binding.status == BindingStatus::Linked {
            continue;
        }
        if let Some([id]) = matches.get(key).map(Vec::as_slice)
            && binding.page_id.as_deref().is_none_or(|known| known == *id)
            && known_pages.get(id).is_none_or(|owner| *owner == key)
        {
            recovered.push((key.clone(), (*id).into()));
        }
    }
    recovered.sort_unstable();
    recovered
}

#[cfg(any(test, feature = "synthetic-https-fixture"))]
fn unavailable(input: &SyncInput, state: &StateSummary, error: NotionError) -> ApplyReport {
    let plan = super::DryRunReport::incomplete(input, error);
    let keys: HashSet<_> = plan.plan().iter().map(|entry| &entry.key).collect();
    let mut counts = plan.counts();
    counts.deferred += state
        .bindings
        .keys()
        .filter(|key| !keys.contains(key))
        .count();
    ApplyReport {
        counts,
        pending: state.pending,
        error: Some(error),
        ..ApplyReport::default()
    }
}

fn patch_matches(patch: &AssignmentPatch, current: &NotionAssignment) -> bool {
    patch
        .title
        .as_ref()
        .is_none_or(|title| title == &current.title)
        && patch
            .deadline
            .as_ref()
            .is_none_or(|deadline| deadline == &current.deadline)
        && patch
            .source
            .is_none_or(|source| Some(source) == current.source)
        && patch
            .course_page_id
            .as_ref()
            .is_none_or(|id| current.course_page_ids.as_slice() == std::slice::from_ref(id))
}

fn explicitly_rejected(error: NotionError) -> bool {
    matches!(
        error,
        NotionError::Authentication
            | NotionError::Forbidden
            | NotionError::NotFound
            | NotionError::Conflict
            | NotionError::Rejected(400)
            | NotionError::RateLimited
    )
}

fn stops_writes(error: NotionError) -> bool {
    !matches!(
        error,
        NotionError::Conflict | NotionError::NotFound | NotionError::IncompleteSnapshot
    )
}
