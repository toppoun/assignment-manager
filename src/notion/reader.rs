use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    CourseMapping, DeferredReason, NotionAssignment, NotionClient, NotionError, NotionSnapshot,
    NotionTarget, PlanAction, PlannedAssignment, PreparedWrite, SyncInput, SyncKey,
    course_rules::CourseRules,
    plan::plan_sync_with_rules,
    plan_sync,
    state::{BindingStatus, StateSummary},
    wire::{Schema, checked_page_id, notion_deadline_matches, relation_id, uuid},
};

const MAX_LIST_PAGES: usize = 200;
const MAX_LIST_ITEMS: usize = 10_000;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DryRunCounts {
    pub create: usize,
    pub update: usize,
    pub unchanged: usize,
    pub conflict: usize,
    pub deferred: usize,
    #[serde(default)]
    pub course_relation_mismatch: usize,
    #[serde(default)]
    pub course_rules: CoursePlanCounts,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoursePlanCounts {
    pub unconfigured: usize,
    pub duplicates: usize,
    pub invalid: usize,
    pub invalid_relation: usize,
    pub unverified_term: usize,
    pub excluded: usize,
}

impl fmt::Display for CoursePlanCounts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "course_rule_unconfigured={} course_rule_duplicate={} course_rule_invalid={} course_rule_invalid_relation={} course_rule_unverified_term={} course_rule_excluded={}",
            self.unconfigured,
            self.duplicates,
            self.invalid,
            self.invalid_relation,
            self.unverified_term,
            self.excluded
        )
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct DryRunReport {
    plan: Vec<PlannedAssignment>,
    requests: Vec<PreparedWrite>,
    error: Option<NotionError>,
}

impl DryRunReport {
    pub fn incomplete(input: &SyncInput, error: NotionError) -> Self {
        Self {
            plan: plan_sync(input, NotionSnapshot::Incomplete, &[]),
            requests: Vec::new(),
            error: Some(error),
        }
    }

    pub fn plan(&self) -> &[PlannedAssignment] {
        &self.plan
    }
    pub fn requests(&self) -> &[PreparedWrite] {
        &self.requests
    }
    pub fn error(&self) -> Option<NotionError> {
        self.error
    }

    /// Require a prior binding to agree with the complete snapshot before
    /// proposing a write or marking an assignment unchanged.
    pub fn defer_unreconciled(&mut self, state: &StateSummary) {
        let known_pages = state.page_owners();
        let mut blocked = HashSet::new();
        for row in &mut self.plan {
            let binding = state.bindings.get(&row.key);
            let needs_reconciliation = match &row.action {
                PlanAction::Create { .. } => binding.is_some(),
                PlanAction::Update { page_id, .. } | PlanAction::Unchanged { page_id } => {
                    binding.is_some_and(|binding| {
                        binding.status != BindingStatus::Linked
                            || binding.page_id.as_ref() != Some(page_id)
                    }) || known_pages
                        .get(page_id.as_str())
                        .is_some_and(|key| *key != &row.key)
                }
                _ => false,
            };
            if needs_reconciliation {
                row.action = PlanAction::Deferred(DeferredReason::PendingReconciliation);
                blocked.insert(&row.key);
            }
        }
        self.requests
            .retain(|request| !blocked.contains(request.key()));
    }

    pub fn counts(&self) -> DryRunCounts {
        self.counts_where(|_| true)
    }

    pub fn counts_for(&self, input: &SyncInput) -> DryRunCounts {
        self.counts_where(|key| input.assignments().contains_key(key))
    }

    pub(super) fn retain_input(&mut self, input: &SyncInput) {
        self.plan
            .retain(|entry| input.assignments().contains_key(&entry.key));
        self.requests
            .retain(|request| input.assignments().contains_key(request.key()));
    }

    pub(super) fn append(&mut self, mut other: Self) {
        self.plan.append(&mut other.plan);
        self.requests.append(&mut other.requests);
    }

    pub(super) fn counts_where(&self, include: impl Fn(&SyncKey) -> bool) -> DryRunCounts {
        let mut counts = DryRunCounts::default();
        for entry in self.plan.iter().filter(|entry| include(&entry.key)) {
            counts.course_relation_mismatch += usize::from(entry.course_relation_mismatch);
            match entry.action {
                PlanAction::Deferred(DeferredReason::CourseUnconfigured) => {
                    counts.course_rules.unconfigured += 1
                }
                PlanAction::Deferred(DeferredReason::CourseRuleDuplicate) => {
                    counts.course_rules.duplicates += 1
                }
                PlanAction::Deferred(DeferredReason::CourseRuleInvalid) => {
                    counts.course_rules.invalid += 1
                }
                PlanAction::Deferred(DeferredReason::CourseRuleInvalidRelation) => {
                    counts.course_rules.invalid_relation += 1
                }
                PlanAction::Deferred(DeferredReason::CourseTermUnverified) => {
                    counts.course_rules.unverified_term += 1
                }
                PlanAction::Excluded => counts.course_rules.excluded += 1,
                _ => {}
            }
            *match entry.action {
                PlanAction::Create { .. } => &mut counts.create,
                PlanAction::Update { .. } => &mut counts.update,
                PlanAction::Unchanged { .. } => &mut counts.unchanged,
                PlanAction::Conflict(_) => &mut counts.conflict,
                PlanAction::Deferred(_) => &mut counts.deferred,
                PlanAction::Excluded => continue,
            } += 1;
        }
        counts
    }

    pub(super) fn defer_manual_candidates(
        &mut self,
        rows: &[NotionAssignment],
    ) -> Vec<(SyncKey, String)> {
        let mut manuals = HashMap::<&str, Vec<&NotionAssignment>>::new();
        for row in rows.iter().filter(|row| row.sync_key.is_none()) {
            manuals.entry(&row.title).or_default().push(row);
        }
        let mut matches = Vec::new();
        let mut blocked = HashSet::new();
        for row in &mut self.plan {
            let PlanAction::Create { assignment, .. } = &row.action else {
                continue;
            };
            let before = matches.len();
            if let Some(pages) = manuals.get(assignment.title.as_str()) {
                matches.extend(
                    pages
                        .iter()
                        .filter(|page| {
                            notion_deadline_matches(&page.deadline, &assignment.deadline)
                        })
                        .map(|page| (row.key.clone(), page.page_id.clone())),
                );
            }
            if matches.len() != before {
                blocked.insert(&row.key);
                row.action = PlanAction::Deferred(DeferredReason::ManualCandidate);
            }
        }
        self.requests
            .retain(|request| !blocked.contains(request.key()));
        matches
    }
}

impl fmt::Display for DryRunReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let counts = self.counts();
        write!(
            f,
            "Create={} Update={} Unchanged={} Conflict={} Deferred={} course_relation_mismatch={}",
            counts.create,
            counts.update,
            counts.unchanged,
            counts.conflict,
            counts.deferred,
            counts.course_relation_mismatch
        )?;
        if let Some(error) = self.error {
            write!(f, " ({error})")?;
        }
        Ok(())
    }
}

impl NotionClient {
    /// Read the target and construct request previews. This accepts no state store
    /// and never dispatches mutations. A read/projection error invalidates the
    /// snapshot as a whole, including any pages collected before that error.
    pub fn dry_run(
        &self,
        input: &SyncInput,
        target: &NotionTarget,
        mappings: &[CourseMapping],
    ) -> DryRunReport {
        match self.plan_complete(input, target, mappings) {
            Ok(report) => report,
            Err(error) => DryRunReport::incomplete(input, error),
        }
    }

    fn plan_complete(
        &self,
        input: &SyncInput,
        target: &NotionTarget,
        mappings: &[CourseMapping],
    ) -> Result<DryRunReport, NotionError> {
        let schema = self.retrieve_schema(target)?;
        if let Some(config) = &target.course_rules {
            let rules = self.read_course_rules(config, &schema)?;
            let rows = self.query_assignments_with_manual_fields(&schema)?;
            return Self::plan_from_course_rules(input, &schema, &rows, &rules);
        }
        let rows = self.query_assignments(&schema, None)?;
        self.plan_from_rows(input, &schema, &rows, mappings)
    }

    pub(super) fn plan_from_rows(
        &self,
        input: &SyncInput,
        schema: &Schema,
        rows: &[NotionAssignment],
        mappings: &[CourseMapping],
    ) -> Result<DryRunReport, NotionError> {
        self.plan_from_rows_with_course_check(input, schema, rows, mappings, |id| {
            let page = self.retrieve_page(id, &[])?;
            if checked_page_id(&page, &schema.courses_id, &schema.courses_database)? != id {
                return Err(NotionError::InvalidResponse);
            }
            Ok(())
        })
    }

    pub(super) fn plan_from_rows_with_course_check(
        &self,
        input: &SyncInput,
        schema: &Schema,
        rows: &[NotionAssignment],
        mappings: &[CourseMapping],
        verify_course: impl Fn(&str) -> Result<(), NotionError>,
    ) -> Result<DryRunReport, NotionError> {
        if mappings.iter().any(|mapping| !mapping.has_valid_page_id()) {
            return Err(NotionError::Configuration);
        }
        let plan = plan_sync(input, NotionSnapshot::Complete(rows), mappings);
        let mut verified = HashSet::new();
        for entry in &plan {
            let relation = match &entry.action {
                PlanAction::Create { course_page_id, .. } => course_page_id.as_deref(),
                PlanAction::Update { patch, .. } => patch.course_page_id.as_deref(),
                _ => None,
            };
            if let Some(id) = relation.filter(|id| verified.insert(*id)) {
                verify_course(id)?;
            }
        }
        Self::prepare_report(schema, plan)
    }

    pub(super) fn plan_from_course_rules(
        input: &SyncInput,
        schema: &Schema,
        rows: &[NotionAssignment],
        rules: &CourseRules,
    ) -> Result<DryRunReport, NotionError> {
        let plan = plan_sync_with_rules(input, NotionSnapshot::Complete(rows), rules);
        let mut report = Self::prepare_report(schema, plan)?;
        report.defer_manual_candidates(rows);
        Ok(report)
    }

    pub(super) fn prepare_report(
        schema: &Schema,
        plan: Vec<PlannedAssignment>,
    ) -> Result<DryRunReport, NotionError> {
        let mut requests = Vec::new();
        for entry in &plan {
            if let Some(request) = schema.prepare(entry)? {
                requests.push(request);
            }
        }
        Ok(DryRunReport {
            plan,
            requests,
            error: None,
        })
    }

    pub(super) fn retrieve_schema(&self, target: &NotionTarget) -> Result<Schema, NotionError> {
        let ids = target.ids()?;
        let assignments = self.get_data_source(&ids.0)?;
        let courses = self.get_data_source(&ids.1)?;
        Schema::validate(target, ids, &assignments, &courses)
    }

    pub(super) fn get_data_source(&self, id: &str) -> Result<Value, NotionError> {
        let path = format!("/v1/data_sources/{id}");
        self.read(|| self.request(Method::GET, &path, &[]))
    }

    pub(super) fn retrieve_page(
        &self,
        id: &str,
        properties: &[&str],
    ) -> Result<Value, NotionError> {
        let id = uuid(id).ok_or(NotionError::Configuration)?;
        let path = format!("/v1/pages/{id}");
        let filters = property_filters(properties);
        self.read(|| self.request(Method::GET, &path, &filters))
    }

    pub(super) fn query_assignments(
        &self,
        schema: &Schema,
        key: Option<&SyncKey>,
    ) -> Result<Vec<NotionAssignment>, NotionError> {
        self.query_assignment_pages(schema, key, |_, row| Ok(row))
    }

    pub(super) fn query_assignments_with_manual_fields(
        &self,
        schema: &Schema,
    ) -> Result<Vec<NotionAssignment>, NotionError> {
        self.query_assignment_pages(schema, None, |page, mut row| {
            if row.sync_key.is_none() {
                (row.title, row.deadline) = schema.manual_title_deadline(page)?;
            }
            Ok(row)
        })
    }

    pub(super) fn query_course_pages<T>(
        &self,
        schema: &Schema,
        projection: &[&str],
        observe: impl Fn(&str, &Value) -> Result<T, NotionError>,
    ) -> Result<HashMap<String, T>, NotionError> {
        self.query_data_source_pages(
            &schema.courses_id,
            &schema.courses_database,
            projection,
            observe,
        )
    }

    pub(super) fn query_data_source_pages<T>(
        &self,
        id: &str,
        database: &str,
        projection: &[&str],
        observe: impl Fn(&str, &Value) -> Result<T, NotionError>,
    ) -> Result<HashMap<String, T>, NotionError> {
        self.query_filtered_data_source_pages(id, database, projection, None, observe)
    }

    pub(super) fn query_filtered_data_source_pages<T>(
        &self,
        id: &str,
        database: &str,
        projection: &[&str],
        filter: Option<&Value>,
        observe: impl Fn(&str, &Value) -> Result<T, NotionError>,
    ) -> Result<HashMap<String, T>, NotionError> {
        let path = format!("/v1/data_sources/{id}/query");
        let mut pages = HashMap::new();
        paginate(
            |cursor| {
                let mut body = json!({"page_size":100, "result_type":"page"});
                if let Some(cursor) = cursor {
                    body["start_cursor"] = json!(cursor);
                }
                if let Some(filter) = filter {
                    body["filter"] = filter.clone();
                }
                self.read(|| {
                    self.request(Method::POST, &path, &property_filters(projection))
                        .json(&body)
                })
            },
            |page| {
                let id = checked_page_id(page, id, database)?;
                let observed = observe(&id, page)?;
                if pages.insert(id, observed).is_some() {
                    return Err(NotionError::IncompleteSnapshot);
                }
                Ok(())
            },
        )?;
        Ok(pages)
    }

    pub(super) fn query_assignment_pages<T>(
        &self,
        schema: &Schema,
        key: Option<&SyncKey>,
        observe: impl Fn(&Value, NotionAssignment) -> Result<T, NotionError>,
    ) -> Result<Vec<T>, NotionError> {
        let path = format!("/v1/data_sources/{}/query", schema.assignments_id);
        let props = &schema.properties;
        let projection = props.assignment_projection();
        let filters = property_filters(&projection);
        let mut seen_pages = HashSet::new();
        paginate(
            |cursor| {
                let mut body = json!({"page_size":100, "result_type":"page"});
                if let Some(cursor) = cursor {
                    body["start_cursor"] = json!(cursor);
                }
                if let Some(key) = key {
                    body["filter"] =
                        json!({"property":props.sync_key, "rich_text":{"equals":key.as_str()}});
                }
                self.read(|| self.request(Method::POST, &path, &filters).json(&body))
            },
            |page| {
                let (row, more_relation) = schema.assignment(page)?;
                if let Some(key) = key
                    && row.sync_key.as_ref() != Some(key)
                {
                    return Err(NotionError::InvalidResponse);
                }
                if !seen_pages.insert(row.page_id.clone()) {
                    return Err(NotionError::IncompleteSnapshot);
                }
                observe(
                    page,
                    self.with_complete_relation(schema, row, more_relation)?,
                )
            },
        )
    }

    pub(super) fn with_complete_relation(
        &self,
        schema: &Schema,
        mut row: NotionAssignment,
        more: bool,
    ) -> Result<NotionAssignment, NotionError> {
        if row.sync_key.is_some() {
            row.course_page_ids = self.complete_relation(
                &row.page_id,
                &schema.properties.course,
                row.course_page_ids,
                more,
            )?;
        }
        Ok(row)
    }

    pub(super) fn complete_relation(
        &self,
        page: &str,
        property: &str,
        links: Vec<String>,
        more: bool,
    ) -> Result<Vec<String>, NotionError> {
        if links.iter().collect::<HashSet<_>>().len() != links.len() {
            return Err(NotionError::IncompleteSnapshot);
        }
        if !more {
            return Ok(links);
        }
        let full = self.retrieve_relation(page, property)?;
        let unique: HashSet<_> = full.iter().collect();
        if unique.len() != full.len()
            || full.len() <= links.len()
            || links.iter().any(|link| !unique.contains(link))
        {
            return Err(NotionError::IncompleteSnapshot);
        }
        Ok(full)
    }

    fn retrieve_relation(&self, page: &str, property: &str) -> Result<Vec<String>, NotionError> {
        let path = format!("/v1/pages/{page}/properties/{property}");
        paginate(
            |cursor| {
                let mut query = vec![("page_size", "100")];
                if let Some(cursor) = cursor {
                    query.push(("start_cursor", cursor));
                }
                let list: Value = self.read(|| self.request(Method::GET, &path, &query))?;
                let metadata = &list["property_item"];
                if list["type"] != "property_item"
                    || metadata["id"] != property
                    || metadata["type"] != "relation"
                {
                    return Err(NotionError::InvalidResponse);
                }
                Ok(list)
            },
            |item| {
                if item["object"] != "property_item"
                    || item["id"] != property
                    || item["type"] != "relation"
                {
                    return Err(NotionError::InvalidResponse);
                }
                relation_id(&item["relation"])
            },
        )
    }
}

pub(super) fn property_filters<'a>(ids: &[&'a str]) -> Vec<(&'static str, &'a str)> {
    ids.iter().map(|id| ("filter_properties", *id)).collect()
}

pub(super) fn paginate<T>(
    mut fetch: impl FnMut(Option<&str>) -> Result<Value, NotionError>,
    mut convert: impl FnMut(&Value) -> Result<T, NotionError>,
) -> Result<Vec<T>, NotionError> {
    let mut cursor = None;
    let mut seen = HashSet::new();
    let mut results = Vec::new();
    for _ in 0..MAX_LIST_PAGES {
        let page = fetch(cursor.as_deref())?;
        if page["object"] != "list" {
            return Err(NotionError::InvalidResponse);
        }
        if page
            .get("request_status")
            .is_some_and(|status| status["type"] != "complete")
        {
            return Err(NotionError::IncompleteSnapshot);
        }
        let items = page["results"]
            .as_array()
            .ok_or(NotionError::InvalidResponse)?;
        if results.len() + items.len() >= MAX_LIST_ITEMS {
            return Err(NotionError::IncompleteSnapshot);
        }
        for item in items {
            results.push(convert(item)?);
        }
        match (page["has_more"].as_bool(), page.get("next_cursor")) {
            (Some(false), Some(Value::Null)) => return Ok(results),
            (Some(true), Some(Value::String(next)))
                if !next.trim().is_empty()
                    && next.len() <= 4096
                    && !next.bytes().any(|byte| byte.is_ascii_control())
                    && seen.insert(next.clone()) =>
            {
                cursor = Some(next.clone());
            }
            _ => return Err(NotionError::IncompleteSnapshot),
        }
    }
    Err(NotionError::IncompleteSnapshot)
}
