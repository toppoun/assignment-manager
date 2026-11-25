use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
};

use chrono::Utc;
use reqwest::Method;
use serde::{Deserialize, Serialize};

use super::{ApplyError, NotionClient, NotionError, PreparedApply, WriteAccess, must_stop_writes};
use crate::notion::{
    DeferredReason, DryRunCounts, DryRunReport, NotionPropertyIds, NotionSnapshot, NotionTarget,
    PlanAction, PreparedWrite, SyncInput, UniversitySource, live_assignment_read_target,
    plan::plan_sync_with_rules,
    state::{RunIssue, RunOutcome, StateError, StateStore, TargetLock},
    trial::PRODUCTION_ASSIGNMENTS,
    wire::{notion_deadline_matches, uuid},
};

const SOURCES: [UniversitySource; 2] = [UniversitySource::Moodle, UniversitySource::WebClass];

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeLimits {
    pub max_creates: usize,
    pub max_updates: usize,
}

#[derive(Serialize)]
pub struct RecurringPolicy {
    writer_bot_id: String,
    moodle: Option<ChangeLimits>,
    webclass: Option<ChangeLimits>,
    moodle_feed: Option<[String; 3]>,
}

impl RecurringPolicy {
    pub fn new(
        writer_bot_id: &str,
        moodle: Option<(ChangeLimits, [String; 3])>,
        webclass: Option<ChangeLimits>,
    ) -> Result<Self, NotionError> {
        let (moodle, moodle_feed) = moodle.unzip();
        if moodle.is_none() && webclass.is_none() {
            return Err(NotionError::Configuration);
        }
        Ok(Self {
            writer_bot_id: uuid(writer_bot_id).ok_or(NotionError::Configuration)?,
            moodle,
            webclass,
            moodle_feed,
        })
    }

    fn limits(&self, source: UniversitySource) -> Option<ChangeLimits> {
        match source {
            UniversitySource::Moodle => self.moodle,
            UniversitySource::WebClass => self.webclass,
        }
    }
}

pub struct RecurringInputs {
    pub moodle: Result<SyncInput, &'static str>,
    pub webclass: Result<SyncInput, &'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecurringMode {
    Preview,
    Apply,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Success,
    Failed,
    Blocked,
    Partial,
    Busy,
}

#[derive(Serialize)]
struct SourceReport {
    acquisition: &'static str,
    candidates: Option<usize>,
    application: &'static str,
    planned: Option<DryRunCounts>,
    applied: Option<DryRunCounts>,
    error: Option<&'static str>,
}

impl Default for SourceReport {
    fn default() -> Self {
        Self {
            acquisition: "not_attempted",
            candidates: None,
            application: "not_attempted",
            planned: None,
            applied: None,
            error: None,
        }
    }
}

#[derive(Serialize)]
pub struct RecurringReport {
    schema_version: u8,
    mode: RecurringMode,
    status: Status,
    error: Option<&'static str>,
    pending_creates: Option<usize>,
    recovered_creates: Option<usize>,
    moodle: SourceReport,
    webclass: SourceReport,
}

impl RecurringReport {
    pub fn blocked(mode: RecurringMode, error: &'static str) -> Self {
        Self {
            schema_version: 1,
            mode,
            status: Status::Blocked,
            error: Some(error),
            pending_creates: None,
            recovered_creates: None,
            moodle: SourceReport::default(),
            webclass: SourceReport::default(),
        }
    }

    pub fn exit_code(&self) -> u8 {
        match self.status {
            Status::Success => 0,
            Status::Failed => 1,
            Status::Blocked => 2,
            Status::Partial => 3,
            Status::Busy => 4,
        }
    }

    fn source(&mut self, source: UniversitySource) -> &mut SourceReport {
        match source {
            UniversitySource::Moodle => &mut self.moodle,
            UniversitySource::WebClass => &mut self.webclass,
        }
    }

    fn finish(&mut self) {
        let sources = [&self.moodle, &self.webclass];
        let succeeded = sources
            .iter()
            .any(|source| matches!(source.application, "planned" | "applied"));
        let partial = sources.iter().any(|source| source.application == "partial");
        let failed = sources.iter().any(|source| source.error.is_some());
        if self.error.is_some() {
            if partial || (succeeded && self.mode == RecurringMode::Apply) {
                self.status = Status::Partial;
            }
        } else if partial || (succeeded && failed) {
            self.status = Status::Partial;
        } else if succeeded {
            self.status = Status::Success;
        } else if sources.iter().any(|source| source.application == "blocked") {
            self.status = Status::Blocked;
        } else {
            self.status = Status::Failed;
        }
    }
}

pub(in crate::notion) struct ApprovedRecurring<'a> {
    writer: &'a NotionClient,
    requests: &'a [PreparedWrite],
    properties: &'a NotionPropertyIds,
    source: UniversitySource,
}

#[cfg(all(test, target_os = "macos"))]
mod verification;
#[cfg(all(test, target_os = "macos"))]
pub(in crate::notion) use verification::{RecurringVerification, VerificationSeed};

enum RecurringTarget {
    Production,
    #[cfg(all(test, target_os = "macos"))]
    Verification(crate::notion::SyncKey),
}

impl RecurringTarget {
    fn id(&self, reader: &NotionClient, target: &NotionTarget) -> Result<String, &'static str> {
        reader.require_course_setup_origin().map_err(notion_code)?;
        match self {
            Self::Production => {
                live_assignment_read_target(target, true).map_err(|_| "target_mismatch")
            }
            #[cfg(all(test, target_os = "macos"))]
            Self::Verification(_) => Ok(crate::notion::CourseSetupVerification::new(target)
                .map_err(|_| "target_mismatch")?
                .assignments_id()
                .to_owned()),
        }
    }

    fn fingerprint(
        &self,
        target: &NotionTarget,
        policy: &RecurringPolicy,
    ) -> Result<String, &'static str> {
        match self {
            Self::Production => digest(&(target, policy)),
            #[cfg(all(test, target_os = "macos"))]
            Self::Verification(key) => digest(&("verification", key.as_str(), target, policy)),
        }
    }

    fn check_writer(
        &self,
        writer: &NotionClient,
        target: &NotionTarget,
        policy: &RecurringPolicy,
    ) -> Result<(), &'static str> {
        match self {
            Self::Production => writer
                .check_production_assignment_writer(target, &policy.writer_bot_id)
                .map(|_| ())
                .map_err(|errors| notion_code(errors[0].error)),
            #[cfg(all(test, target_os = "macos"))]
            Self::Verification(_) => {
                if policy.writer_bot_id != crate::notion::trial::MOODLE_VERIFICATION_WRITER {
                    return Err("target_mismatch");
                }
                writer
                    .check_verification_assignment_writer(target)
                    .map_err(notion_code)
            }
        }
    }

    fn permits_input(&self, input: &SyncInput, source: UniversitySource) -> bool {
        input.assignments().iter().all(|(_key, row)| {
            row.source == source
                && match self {
                    Self::Production => true,
                    #[cfg(all(test, target_os = "macos"))]
                    Self::Verification(sample) => {
                        source == UniversitySource::Moodle && _key == sample
                    }
                }
        })
    }

    fn access<'a>(&'a self, approval: ApprovedRecurring<'a>) -> WriteAccess<'a> {
        match self {
            Self::Production => WriteAccess::ProductionRecurring(approval),
            #[cfg(all(test, target_os = "macos"))]
            Self::Verification(key) => WriteAccess::VerificationRecurring(approval, key),
        }
    }
}

impl ApprovedRecurring<'_> {
    pub(super) fn source(&self) -> UniversitySource {
        self.source
    }

    pub(in crate::notion) fn permits(
        &self,
        writer: &NotionClient,
        request: &PreparedWrite,
    ) -> bool {
        self.permits_target(writer, request, PRODUCTION_ASSIGNMENTS)
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(in crate::notion) fn permits_verification(
        &self,
        writer: &NotionClient,
        request: &PreparedWrite,
        sample: &crate::notion::SyncKey,
    ) -> bool {
        request.key() == sample
            && self.permits_target(
                writer,
                request,
                crate::notion::trial::MOODLE_VERIFICATION_ASSIGNMENTS,
            )
    }

    fn permits_target(
        &self,
        writer: &NotionClient,
        request: &PreparedWrite,
        target_id: &str,
    ) -> bool {
        if !std::ptr::eq(self.writer, writer)
            || request.target_id != target_id
            || !self.requests.contains(request)
        {
            return false;
        }
        let Some(body) = request.body().as_object() else {
            return false;
        };
        let Some(fields) = body
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .filter(|fields| !fields.is_empty())
        else {
            return false;
        };
        let ids = self.properties;
        match *request.method() {
            Method::POST => {
                body.len() == 2
                    && body["parent"]["type"] == "data_source_id"
                    && [&ids.title, &ids.source, &ids.sync_key, &ids.course]
                        .into_iter()
                        .all(|id| fields.contains_key(id))
                    && fields.keys().all(|id| {
                        [
                            &ids.title,
                            &ids.deadline,
                            &ids.source,
                            &ids.sync_key,
                            &ids.course,
                        ]
                        .contains(&id)
                    })
            }
            Method::PATCH => {
                body.len() == 1
                    && fields
                        .keys()
                        .all(|id| id == &ids.title || id == &ids.deadline)
            }
            _ => false,
        }
    }
}

impl NotionClient {
    /// Revoke only the recurring write approval in the production state file.
    /// This operation does not need a client or contact any network service.
    pub fn revoke_production_recurring(
        target: &NotionTarget,
        directory: &Path,
    ) -> Result<(), &'static str> {
        let id = live_assignment_read_target(target, true).map_err(|_| "target_mismatch")?;
        revoke_recurring(directory, &id)
    }

    /// Save approval only after the caller has obtained explicit operator confirmation.
    pub fn approve_production_recurring(
        &self,
        writer: &NotionClient,
        target: &NotionTarget,
        policy: &RecurringPolicy,
        directory: &Path,
    ) -> Result<(), &'static str> {
        self.approve_recurring(
            writer,
            target,
            policy,
            directory,
            &RecurringTarget::Production,
        )
    }

    fn approve_recurring(
        &self,
        writer: &NotionClient,
        target: &NotionTarget,
        policy: &RecurringPolicy,
        directory: &Path,
        destination: &RecurringTarget,
    ) -> Result<(), &'static str> {
        let id = destination.id(self, target)?;
        let _lock = TargetLock::acquire(directory).map_err(state_code)?;
        let store =
            StateStore::for_apply(&directory.join("state.sqlite3"), &id).map_err(state_code)?;
        let schema = self.retrieve_schema(target).map_err(notion_code)?;
        let rule_schema = self
            .retrieve_course_rule_schema(rules_target(target)?, &schema)
            .map_err(notion_code)?;
        destination.check_writer(writer, target, policy)?;
        if writer.retrieve_schema(target).map_err(notion_code)? != schema {
            return Err("target_mismatch");
        }
        let approved = format!(
            "{}:{}",
            destination.fingerprint(target, policy)?,
            digest(&(&schema, &rule_schema))?
        );
        store.approve_recurring(&approved).map_err(state_code)
    }

    pub fn run_production_recurring(
        &self,
        target: &NotionTarget,
        policy: &RecurringPolicy,
        directory: &Path,
        mode: RecurringMode,
        load_writer: impl FnOnce() -> Result<NotionClient, &'static str>,
        acquire: impl FnOnce(&BTreeMap<String, String>) -> RecurringInputs,
    ) -> RecurringReport {
        self.recurring_report(
            target,
            policy,
            directory,
            mode,
            &RecurringTarget::Production,
            (load_writer, acquire),
        )
    }

    fn recurring_report(
        &self,
        target: &NotionTarget,
        policy: &RecurringPolicy,
        directory: &Path,
        mode: RecurringMode,
        destination: &RecurringTarget,
        operations: (
            impl FnOnce() -> Result<NotionClient, &'static str>,
            impl FnOnce(&BTreeMap<String, String>) -> RecurringInputs,
        ),
    ) -> RecurringReport {
        let mut report = RecurringReport::blocked(mode, "not_started");
        report.error = None;
        for source in SOURCES {
            if policy.limits(source).is_none() {
                report.source(source).acquisition = "disabled";
            }
        }
        if let Err(error) = self.run_recurring(
            target,
            policy,
            directory,
            destination,
            operations,
            &mut report,
        ) {
            report.error = Some(error);
            if error == "target_busy" {
                report.status = Status::Busy;
            }
        }
        report.finish();
        report
    }

    fn run_recurring(
        &self,
        target: &NotionTarget,
        policy: &RecurringPolicy,
        directory: &Path,
        destination: &RecurringTarget,
        (load_writer, acquire): (
            impl FnOnce() -> Result<NotionClient, &'static str>,
            impl FnOnce(&BTreeMap<String, String>) -> RecurringInputs,
        ),
        report: &mut RecurringReport,
    ) -> Result<(), &'static str> {
        let id = destination.id(self, target)?;
        let _lock = TargetLock::acquire(directory).map_err(state_code)?;
        let path = directory.join("state.sqlite3");
        let store = match report.mode {
            RecurringMode::Apply => Some(StateStore::for_apply(&path, &id).map_err(state_code)?),
            RecurringMode::Preview => StateStore::read_only(&path, &id).map_err(state_code)?,
        };
        let approved = if let (RecurringMode::Apply, Some(store)) = (report.mode, &store) {
            let approval = store
                .recurring_approval()
                .map_err(state_code)?
                .ok_or("write_not_approved")?;
            let (configured, schema) = approval.split_once(':').ok_or("policy_changed")?;
            if configured != destination.fingerprint(target, policy)? {
                return Err("policy_changed");
            }
            Some(schema.to_owned())
        } else {
            None
        };

        let schema = self.retrieve_schema(target).map_err(notion_code)?;
        let config = rules_target(target)?;
        let rule_schema = self
            .retrieve_course_rule_schema(config, &schema)
            .map_err(notion_code)?;
        if let Some(approved) = approved
            && approved != digest(&(&schema, &rule_schema))?
        {
            return Err("policy_changed");
        }
        let before = self
            .read_course_rules_with_schema(config, &schema, &rule_schema)
            .map_err(notion_code)?;
        let invalid_webclass_rules = before.by_key.iter().any(|(key, entry)| {
            key.origin == crate::import::OriginKind::WebClass
                && matches!(
                    entry.resolution,
                    Err(DeferredReason::CourseRuleDuplicate
                        | DeferredReason::CourseRuleInvalid
                        | DeferredReason::CourseRuleInvalidRelation
                        | DeferredReason::CourseTermUnverified)
                )
        });
        let scopes = if invalid_webclass_rules {
            BTreeMap::new()
        } else {
            before.webclass_scopes()
        };
        let RecurringInputs {
            moodle,
            mut webclass,
        } = acquire(&scopes);
        if invalid_webclass_rules {
            webclass = Err("invalid_course_rules");
        }
        let mut inputs = Vec::new();
        for (source, input) in SOURCES.into_iter().zip([moodle, webclass]) {
            if policy.limits(source).is_none() {
                continue;
            }
            let result = report.source(source);
            match input {
                Ok(input) if destination.permits_input(&input, source) => {
                    result.acquisition = "success";
                    result.candidates = Some(input.assignments().len());
                    inputs.push((source, input));
                }
                failed => {
                    result.acquisition = "failed";
                    result.error = Some(failed.err().unwrap_or("invalid_source_data"));
                }
            }
        }
        if inputs.is_empty() {
            if report.mode == RecurringMode::Apply
                && let Some(store) = store.as_ref()
            {
                store
                    .record_run(
                        Utc::now(),
                        RunOutcome::Failed,
                        Some(RunIssue::Acquisition),
                        0,
                    )
                    .map_err(state_code)?;
            }
            return Ok(());
        }
        if self.retrieve_schema(target).map_err(notion_code)? != schema
            || self
                .retrieve_course_rule_schema(config, &schema)
                .map_err(notion_code)?
                != rule_schema
        {
            return Err("target_mismatch");
        }
        let rules = self
            .read_course_rules_with_schema(config, &schema, &rule_schema)
            .map_err(notion_code)?;
        // A change isolated to one source's rules need not block the other source.
        inputs.retain(|(source, _)| {
            let origin = match source {
                UniversitySource::Moodle => crate::import::OriginKind::Moodle,
                UniversitySource::WebClass => crate::import::OriginKind::WebClass,
            };
            let changed =
                [&before, &rules]
                    .into_iter()
                    .zip([&rules, &before])
                    .any(|(left, right)| {
                        left.by_key.iter().any(|(key, entry)| {
                            key.origin == origin && right.by_key.get(key) != Some(entry)
                        })
                    });
            if changed {
                let result = report.source(*source);
                result.application = "blocked";
                result.error = Some("course_rules_changed");
            }
            !changed
        });

        let state = store
            .as_ref()
            .map(StateStore::summary)
            .transpose()
            .map_err(state_code)?
            .unwrap_or_default();
        let fresh = self
            .prepare_plan(schema, state, |schema, rows| {
                let mut seen = HashSet::new();
                if rows
                    .iter()
                    .filter_map(|row| row.sync_key.as_ref())
                    .any(|key| !seen.insert(key))
                {
                    return Err(NotionError::Conflict);
                }
                let mut all = DryRunReport::default();
                for (source, input) in &inputs {
                    let plan = plan_sync_with_rules(input, NotionSnapshot::Complete(rows), &rules);
                    match Self::prepare_report(schema, plan) {
                        Ok(mut plan) => {
                            plan.retain_input(input);
                            all.append(plan);
                        }
                        Err(error) => {
                            let result = report.source(*source);
                            result.application = "blocked";
                            result.error = Some(notion_code(error));
                        }
                    }
                }
                Ok(all)
            })
            .map_err(notion_code)?;

        let origins = store
            .as_ref()
            .map(StateStore::recurring_origins)
            .transpose()
            .map_err(state_code)?
            .unwrap_or_default();
        report.pending_creates = Some(fresh.pending);
        let mut pending = Vec::new();
        for key in &fresh.unresolved_keys {
            let source = inputs
                .iter()
                .find_map(|(source, input)| {
                    input.assignments().contains_key(key).then_some(*source)
                })
                .or_else(|| origins.get(key).copied())
                .ok_or("unattributed_pending_create")?;
            pending.push(source);
        }

        let mut candidates = Vec::new();
        for (source, input) in &inputs {
            let result = report.source(*source);
            if result.error.is_some() {
                continue;
            }
            let counts = fresh.deltas.counts_for(input);
            result.planned = Some(counts);
            let limits = policy.limits(*source).expect("configured input");
            let unsafe_patch = fresh.deltas.plan().iter().any(|entry| {
                input.assignments().contains_key(&entry.key)
                    && matches!(&entry.action, PlanAction::Update { patch, .. }
                    if patch.source.is_some() || patch.course_page_id.is_some())
            });
            let error = if counts.create > limits.max_creates || counts.update > limits.max_updates
            {
                Some("change_limit")
            } else if pending.contains(source) {
                Some("reconciliation_required")
            } else if unsafe_patch {
                Some("protected_field_change")
            } else if has_cross_source_duplicate(&fresh, input, &inputs, &rules) {
                Some("potential_duplicate")
            } else {
                None
            };
            if let Some(error) = error {
                result.application = "blocked";
                result.error = Some(error);
            } else {
                candidates.push((*source, input));
            }
        }

        if report.mode == RecurringMode::Preview {
            for (source, _) in candidates {
                let result = report.source(source);
                result.application = "planned";
                if result
                    .planned
                    .is_some_and(|counts| counts.deferred > 0 || counts.conflict > 0)
                {
                    result.error = Some("deferred_items");
                }
            }
            return Ok(());
        }
        let store = store.as_ref().expect("apply requires a state database");
        for (key, page) in &fresh.recovered {
            store.confirm(key, page).map_err(state_code)?;
        }
        report.recovered_creates = Some(fresh.recovered.len());
        let mut shared_error = None;
        if !candidates.is_empty() {
            let writer = load_writer()?;
            destination.check_writer(&writer, target, policy)?;
            if writer.retrieve_schema(target).map_err(notion_code)? != fresh.schema {
                return Err("target_mismatch");
            }
            for (source, input) in candidates {
                let result = report.source(source);
                result.application = "partial";
                let plan = scope_plan(&fresh, input);
                let access = destination.access(ApprovedRecurring {
                    writer: &writer,
                    requests: plan.deltas.requests(),
                    properties: &fresh.schema.properties,
                    source,
                });
                let applied = writer
                    .execute_apply(&plan, store, &access)
                    .map_err(apply_code)?;
                result.applied = Some(applied.counts);
                if let Some(error) = applied.error {
                    result.error = Some(notion_code(error));
                    if must_stop_writes(&access, error) {
                        shared_error = result.error;
                        break;
                    }
                } else if applied.counts.conflict > 0 || applied.counts.deferred > 0 {
                    result.error = Some("deferred_items");
                } else {
                    result.application = "applied";
                }
            }
        }
        report.pending_creates = Some(store.summary().map_err(state_code)?.pending);
        report.error = shared_error;
        report.finish();
        let now = Utc::now();
        for source in SOURCES {
            let result = report.source(source);
            store
                .record_progress(
                    source,
                    (result.acquisition == "success").then_some(now),
                    (result.application == "applied").then_some(now),
                )
                .map_err(state_code)?;
        }
        let (outcome, issue) = match report.status {
            Status::Success => (RunOutcome::Success, None),
            Status::Partial => (
                RunOutcome::Partial,
                Some(if shared_error.is_some() {
                    RunIssue::Notion
                } else {
                    RunIssue::Acquisition
                }),
            ),
            _ => (RunOutcome::Failed, Some(RunIssue::Reconciliation)),
        };
        let deferred = [&report.moodle, &report.webclass]
            .into_iter()
            .map(|result| result.applied.map_or(0, |counts| counts.deferred))
            .sum();
        store
            .record_run(now, outcome, issue, deferred)
            .map_err(state_code)?;
        Ok(())
    }
}

fn revoke_recurring(directory: &Path, id: &str) -> Result<(), &'static str> {
    if !directory.try_exists().map_err(|_| "state_error")? {
        return Err("state_missing_or_invalid");
    }
    let _lock = TargetLock::acquire(directory).map_err(state_code)?;
    StateStore::for_apply(&directory.join("state.sqlite3"), id)
        .and_then(|store| store.revoke_recurring_approval())
        .map_err(state_code)
}

fn scope_plan(full: &PreparedApply, input: &SyncInput) -> PreparedApply {
    let mut deltas = full.deltas.clone();
    deltas.retain_input(input);
    let updates: HashSet<_> = deltas
        .plan()
        .iter()
        .filter_map(|entry| match &entry.action {
            PlanAction::Update { page_id, .. } => Some(page_id.as_str()),
            _ => None,
        })
        .collect();
    let pages = full
        .pages
        .iter()
        .filter(|page| updates.contains(page.row.page_id.as_str()))
        .cloned()
        .collect();
    PreparedApply {
        schema: full.schema.clone(),
        pages,
        deltas,
        manual_candidates: full
            .manual_candidates
            .iter()
            .filter(|(key, _)| input.assignments().contains_key(key))
            .cloned()
            .collect(),
        recovered: Vec::new(),
        pending: 0,
        unresolved_keys: Vec::new(),
        missing_bindings: 0,
    }
}

fn has_cross_source_duplicate(
    plan: &PreparedApply,
    input: &SyncInput,
    inputs: &[(UniversitySource, SyncInput)],
    rules: &crate::notion::course_rules::CourseRules,
) -> bool {
    let mut other = HashMap::<_, Vec<_>>::new();
    for assignment in inputs
        .iter()
        .flat_map(|(_, input)| input.assignments().values())
    {
        if let Ok(course) = rules.resolve(assignment) {
            other
                .entry((course, assignment.title.as_str()))
                .or_default()
                .push((assignment.source, &assignment.deadline));
        }
    }
    for page in &plan.pages {
        let row = &page.row;
        if let (Some(_), Some(source), [course]) =
            (&row.sync_key, row.source, row.course_page_ids.as_slice())
        {
            other
                .entry((course.as_str(), row.title.as_str()))
                .or_default()
                .push((source, &row.deadline));
        }
    }
    plan.deltas.plan().iter().any(|entry| {
        let PlanAction::Create {
            assignment,
            course_page_id: Some(course),
        } = &entry.action
        else {
            return false;
        };
        input.assignments().contains_key(&entry.key)
            && other
                .get(&(course.as_str(), assignment.title.as_str()))
                .is_some_and(|matches| {
                    matches.iter().any(|(source, deadline)| {
                        *source != assignment.source
                            && notion_deadline_matches(deadline, &assignment.deadline)
                    })
                })
    })
}

fn rules_target(target: &NotionTarget) -> Result<&crate::notion::CourseRulesTarget, &'static str> {
    target.course_rules.as_ref().ok_or("target_mismatch")
}

fn digest(data: &impl Serialize) -> Result<String, &'static str> {
    use std::fmt::Write as _;
    let encoded = serde_json::to_vec(data).map_err(|_| "configuration_invalid")?;
    let hash = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &encoded);
    let mut output = String::from("v1-");
    for byte in hash.as_ref() {
        write!(output, "{byte:02x}").expect("writing to a string");
    }
    Ok(output)
}

fn state_code(error: StateError) -> &'static str {
    match error {
        StateError::Busy => "target_busy",
        StateError::InvalidDatabase => "state_missing_or_invalid",
        _ => "state_error",
    }
}

fn notion_code(error: NotionError) -> &'static str {
    match error {
        NotionError::Authentication => "notion_authentication",
        NotionError::Forbidden => "notion_access_denied",
        NotionError::CredentialMismatch => "writer_mismatch",
        NotionError::IncompleteSnapshot => "incomplete_snapshot",
        NotionError::Conflict => "notion_conflict",
        NotionError::SchemaMismatch | NotionError::MethodDisabled => "target_mismatch",
        NotionError::RateLimited | NotionError::Overloaded => "notion_rate_limited",
        NotionError::Timeout | NotionError::Transport => "notion_network_failed",
        NotionError::Server(_) => "notion_server_unavailable",
        _ => "notion_invalid_response",
    }
}

fn apply_code(error: ApplyError) -> &'static str {
    match error {
        ApplyError::State(error) => state_code(error),
        ApplyError::Notion(error) => notion_code(error),
        _ => "plan_changed",
    }
}
