use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{
    AssignmentPatch, CourseMapping, DryRunCounts, NotionAssignment, NotionClient, NotionError,
    NotionTarget, PlanAction, PreparedWrite, SyncInput,
    state::{BindingStatus, StateError, StateStore, StateSummary, TargetLock},
    wire::{Schema, checked_page_id},
};

#[derive(Debug, PartialEq, Eq)]
pub enum ApplyError {
    Notion(NotionError),
    State(StateError),
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
            // Manual fields are optional diagnostic input. They never generate a patch.
            if let Some((title, deadline)) = schema.manual_title_deadline(page) {
                row.title = title;
                row.deadline = deadline;
            }
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
    pub(super) fn apply(
        &self,
        input: &SyncInput,
        target: &NotionTarget,
        mappings: &[CourseMapping],
        directory: &Path,
    ) -> Result<ApplyReport, ApplyError> {
        self.require_mock_writes().map_err(ApplyError::Notion)?;
        let target_id = target.validated_id().map_err(ApplyError::Notion)?;
        let _lock = TargetLock::acquire(directory)?;
        let store = StateStore::for_apply(&directory.join("state.sqlite3"), &target_id)?;
        let mut state = store.summary()?;

        let snapshot = self.retrieve_schema(target).and_then(|schema| {
            self.query_assignment_pages(&schema, None, |page, row| {
                Observation::new(&schema, page, row)
            })
            .map(|pages| (schema, pages))
        });
        let (schema, pages) = match snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Ok(unavailable(input, &state, error));
            }
        };
        let recovered = reconcile_creates(&store, &state, &pages)?;
        if recovered != 0 {
            state = store.summary()?;
        }
        let mut report = ApplyReport {
            pending: state.pending,
            recovered,
            ..ApplyReport::default()
        };
        let rows: Vec<_> = pages.iter().map(|page| page.row.clone()).collect();
        let mut planned = match self.plan_from_rows(input, &schema, &rows, mappings) {
            Ok(plan) => plan,
            Err(error) => {
                let mut report = unavailable(input, &state, error);
                report.recovered = recovered;
                return Ok(report);
            }
        };
        planned.defer_unreconciled(&state);
        let requests: HashMap<_, _> = planned
            .requests()
            .iter()
            .map(|request| (request.key(), request))
            .collect();
        let by_id: HashMap<_, _> = pages
            .iter()
            .map(|page| (page.row.page_id.as_str(), page))
            .collect();
        let mut manuals: HashMap<&str, Vec<_>> = HashMap::new();
        for page in pages.iter().filter(|page| page.row.sync_key.is_none()) {
            manuals
                .entry(&page.row.title)
                .or_default()
                .push(&page.row.deadline);
        }
        let mut stop_writes = false;
        let mut planned_keys = HashSet::new();
        for entry in planned.plan() {
            planned_keys.insert(&entry.key);
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
                PlanAction::Create { assignment, .. } => {
                    if manuals
                        .get(assignment.title.as_str())
                        .is_some_and(|deadlines| deadlines.contains(&&assignment.deadline))
                    {
                        report.manual_candidates += 1;
                        WriteOutcome::Deferred(None)
                    } else {
                        self.apply_create(&schema, &store, requests[&entry.key])?
                    }
                }
                PlanAction::Update { page_id, patch } => {
                    store.confirm(&entry.key, page_id)?;
                    self.apply_update(
                        &schema,
                        by_id[page_id.as_str()],
                        patch,
                        requests[&entry.key],
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
        report.counts.deferred += state
            .bindings
            .keys()
            .filter(|key| !planned_keys.contains(key))
            .count();
        report.pending = store.summary()?.pending;
        Ok(report)
    }

    fn apply_create(
        &self,
        schema: &Schema,
        store: &StateStore,
        request: &PreparedWrite,
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
        match self.send_write(request) {
            Ok(page) => {
                let id =
                    checked_page_id(&page, &schema.assignments_id, &schema.assignments_database)
                        .ok()
                        .filter(|_| schema.page_key(&page).ok().flatten().as_ref() == Some(key));
                if let Some(id) = id {
                    if !store.is_owned_by_another_key(&id, key)? {
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
        let failure = match self.send_write(request) {
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

fn reconcile_creates(
    store: &StateStore,
    state: &StateSummary,
    pages: &[Observation],
) -> Result<usize, StateError> {
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
    let mut recovered = 0;
    for (key, binding) in &state.bindings {
        if binding.status == BindingStatus::Linked {
            continue;
        }
        if let Some([id]) = matches.get(key).map(Vec::as_slice)
            && binding.page_id.as_deref().is_none_or(|known| known == *id)
            && known_pages.get(id).is_none_or(|owner| *owner == key)
        {
            store.confirm(key, id)?;
            recovered += 1;
        }
    }
    Ok(recovered)
}

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
