//! Course setup plans against a complete Notion snapshot. Writes are limited
//! to the verified course and rule data sources, or the loopback fixture.

use std::{
    collections::{BTreeSet, HashMap},
    fmt::Write as _,
    path::Path,
};

use aws_lc_rs::digest::{SHA256, digest};
use reqwest::Method;
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    import::{DEFAULT_COURSE_SOURCE_SCOPE, ExternalRecordKey, OriginKind},
    moodle::{MoodleEvent, single_course_category},
    moodle_http::{MoodleExportKind, MoodleFeed},
    sync::AcquiredWebClassCourses,
    webclass::WebClassCourse,
};

use super::{
    ApplyError, PreparedWrite,
    apply::{WriteAccess, explicitly_rejected},
    state::{StateStore, TargetLock},
    trial::{
        MOODLE_VERIFICATION_ASSIGNMENTS, MOODLE_VERIFICATION_COURSES, MOODLE_VERIFICATION_WRITER,
    },
    wire::{checked_page_id, page_relations, relation_value, text_item, uuid},
};

pub(super) const VERIFICATION_RULES: &str = "42285934-7e84-81a9-9770-000be500f527";
const SETUP_STATE_FILE: &str = "course-setup.sqlite3";

pub struct CourseSetupVerification(());

impl CourseSetupVerification {
    pub fn new(target: &NotionTarget) -> Result<Self, NotionError> {
        let (assignments, courses) = target.ids()?;
        let rules = target
            .course_rules
            .as_ref()
            .ok_or(NotionError::Configuration)?;
        if assignments != MOODLE_VERIFICATION_ASSIGNMENTS
            || courses != MOODLE_VERIFICATION_COURSES
            || uuid(&rules.data_source_id).as_deref() != Some(VERIFICATION_RULES)
        {
            return Err(NotionError::MethodDisabled);
        }
        rules.setup.as_ref().ok_or(NotionError::Configuration)?;
        Ok(Self(()))
    }

    pub fn assignments_id(&self) -> &'static str {
        MOODLE_VERIFICATION_ASSIGNMENTS
    }
    pub fn rules_id(&self) -> &'static str {
        VERIFICATION_RULES
    }
    pub fn writer_bot_id(&self) -> &'static str {
        MOODLE_VERIFICATION_WRITER
    }
}

pub(super) struct ApprovedCourseSetup<'a>(&'a NotionClient);

impl ApprovedCourseSetup<'_> {
    pub(super) fn permits(&self, client: &NotionClient, request: &PreparedWrite) -> bool {
        std::ptr::eq(self.0, client)
            && request.method() == Method::POST
            && request.path() == "/v1/pages"
            && matches!(
                request.target_id.as_str(),
                MOODLE_VERIFICATION_COURSES | VERIFICATION_RULES
            )
            && request.body()["parent"]["type"] == "data_source_id"
            && request
                .body()
                .as_object()
                .is_some_and(|fields| fields.len() == 2 && request.body()["properties"].is_object())
    }
}
use super::{
    CourseRulesTarget, CourseSetupProperties, DeferredReason, NotionClient, NotionError,
    NotionTarget, SyncKey,
    course_rules::{CourseRuleSchema, CourseRules, valid_term},
    state::{BindingStatus, StateSummary},
    wire::{Schema, page_property, text, title_value},
};

pub struct CourseSetupInput<'a> {
    term: &'a str,
    candidates: Vec<Candidate<'a>>,
}

struct Candidate<'a> {
    key: ExternalRecordKey,
    name: &'a str,
    comparison_name: String,
    webclass: Option<&'a WebClassCourse>,
    rule_key: SyncKey,
    course_key: Option<SyncKey>,
}

impl<'a> Candidate<'a> {
    fn new(
        key: ExternalRecordKey,
        name: &'a str,
        webclass: Option<&'a WebClassCourse>,
    ) -> Result<Self, NotionError> {
        // This journal has its own target and task-free key namespaces.
        let journal_key = |kind| {
            SyncKey::derive(&ExternalRecordKey {
                scope: format!("course-setup/{kind}/{}", key.scope),
                ..key.clone()
            })
            .map_err(|_| NotionError::Configuration)
        };
        let rule_key = journal_key("rule")?;
        let course_key = webclass.map(|_| journal_key("course")).transpose()?;
        Ok(Self {
            key,
            name,
            comparison_name: comparison_name(name),
            webclass,
            rule_key,
            course_key,
        })
    }
}

impl<'a> CourseSetupInput<'a> {
    pub fn new(
        term: &'a str,
        webclass: &'a AcquiredWebClassCourses,
        moodle: &'a [MoodleEvent],
        feed: &MoodleFeed,
    ) -> Result<Self, NotionError> {
        if !valid_course_setup_term(term) || feed.export_kind() != MoodleExportKind::CourseRelated {
            return Err(NotionError::Configuration);
        }
        let mut candidates = Vec::new();
        for course in webclass.records() {
            if format!("{}:{}", course.year, course.semester) == term {
                let key = ExternalRecordKey {
                    origin: OriginKind::WebClass,
                    scope: DEFAULT_COURSE_SOURCE_SCOPE.into(),
                    id: course.group_id.clone(),
                };
                candidates.push(Candidate::new(key, &course.group_name, Some(course))?);
            }
        }
        candidates.sort_unstable_by(|left, right| left.key.id.cmp(&right.key.id));
        let categories: BTreeSet<_> = moodle
            .iter()
            .filter(|event| !event.cancelled)
            .filter_map(|event| single_course_category(event, feed))
            .collect();
        for category in categories {
            let key = ExternalRecordKey {
                origin: OriginKind::Moodle,
                scope: feed.scope().into(),
                id: category.into(),
            };
            candidates.push(Candidate::new(key, category, None)?);
        }
        Ok(Self { term, candidates })
    }
}

pub fn valid_course_setup_term(term: &str) -> bool {
    valid_term(term) && fits_text(term)
}

fn fits_text(value: &str) -> bool {
    value.encode_utf16().count() <= 2_000
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CourseSetupCounts {
    pub create_courses: usize,
    pub create_rules: usize,
    pub reuse_courses: usize,
    pub reuse_rules: usize,
    pub held: usize,
}

/// Local terminal preview. Names and external identities are for user review.
#[derive(Serialize)]
pub struct CourseSetupPreview {
    pub plan_id: String,
    pub counts: CourseSetupCounts,
    pub webclass_courses: usize,
    pub moodle_categories: usize,
    pub items: Vec<CourseSetupPreviewItem>,
    pub existing_courses_without_group_id: Vec<ExistingCoursePreview>,
}

#[derive(Serialize)]
pub struct CourseSetupPreviewItem {
    source: &'static str,
    name: String,
    scope: String,
    external_id: String,
    term: String,
    day_of_week: Option<String>,
    class_order: Option<String>,
    course_action: &'static str,
    rule_action: &'static str,
    reason: &'static str,
    existing_rule_id: Option<String>,
    proposed_rule: Option<CourseSetupRulePreview>,
    existing_course_candidates: Vec<ExistingCoursePreview>,
    other_source_name_matches: Vec<String>,
}

#[derive(Serialize)]
struct CourseSetupRulePreview {
    status: &'static str,
    relation: &'static str,
    course_page_id: Option<String>,
}

#[derive(Serialize)]
pub struct ExistingCoursePreview {
    page_id: String,
    name: String,
    term: Option<String>,
    group_id: Option<String>,
    matching_moodle_categories: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
enum Destination {
    Unset,
    Ambiguous,
    Existing(String),
    NewCourse,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
enum Action {
    Hold(&'static str),
    ReuseRule { page_id: String, waiting: bool },
    CreateRule(Destination),
}

fn counts(plan: &[Action]) -> CourseSetupCounts {
    let mut counts = CourseSetupCounts::default();
    for action in plan {
        match action {
            Action::Hold(_) => counts.held += 1,
            Action::ReuseRule { waiting, .. } => {
                counts.reuse_rules += 1;
                counts.held += usize::from(*waiting);
            }
            Action::CreateRule(destination) => {
                counts.create_rules += 1;
                match destination {
                    Destination::Unset => {}
                    Destination::Ambiguous => counts.held += 1,
                    Destination::Existing(_) => counts.reuse_courses += 1,
                    Destination::NewCourse => counts.create_courses += 1,
                }
            }
        }
    }
    counts
}

struct CoursePage {
    group_id: String,
    title: String,
    term: String,
    comparison_name: String,
}

struct CourseSetupSnapshot {
    rules: CourseRules,
    courses: HashMap<String, CoursePage>,
}

#[derive(Serialize)]
struct SetupTarget<'a> {
    rules: &'a CourseRulesTarget,
    properties: &'a CourseSetupProperties,
    schema: Schema,
    rule_schema: CourseRuleSchema,
}

impl NotionClient {
    pub fn preview_course_setup(
        &self,
        target: &NotionTarget,
        input: &CourseSetupInput<'_>,
    ) -> Result<CourseSetupPreview, NotionError> {
        let target = self.setup_target(target)?;
        let snapshot = self.course_setup_snapshot(&target)?;
        let plan = snapshot.plan(input, &StateSummary::default());
        snapshot.preview(&target, input, &plan)
    }

    pub fn preview_verified_course_setup(
        &self,
        configured: &NotionTarget,
        input: &CourseSetupInput<'_>,
        directory: &Path,
    ) -> Result<CourseSetupPreview, ApplyError> {
        let verification = CourseSetupVerification::new(configured)?;
        self.require_course_setup_origin()?;
        let target = self.setup_target(configured)?;
        let state = StateStore::read(&directory.join(SETUP_STATE_FILE), verification.rules_id())?
            .unwrap_or_default();
        let snapshot = self.course_setup_snapshot(&target)?;
        let plan = snapshot.plan(input, &state);
        Ok(snapshot.preview(&target, input, &plan)?)
    }

    pub fn apply_verified_course_setup(
        &self,
        writer: &NotionClient,
        configured: &NotionTarget,
        input: &CourseSetupInput<'_>,
        directory: &Path,
        expected_plan_id: &str,
    ) -> Result<CourseSetupCounts, ApplyError> {
        let verification = CourseSetupVerification::new(configured)?;
        self.require_course_setup_origin()?;
        writer.require_course_setup_origin()?;
        writer.verify_bot(verification.writer_bot_id())?;
        let access = WriteAccess::CourseSetup(ApprovedCourseSetup(writer));
        self.apply_checked_course_setup(writer, configured, input, directory, &access, |preview| {
            if preview.plan_id != expected_plan_id {
                return Err(ApplyError::PlanChanged);
            }
            Ok(true)
        })?
        .ok_or(ApplyError::PlanChanged)
    }

    #[cfg(feature = "synthetic-https-fixture")]
    pub(super) fn apply_course_setup(
        &self,
        configured: &NotionTarget,
        input: &CourseSetupInput<'_>,
        directory: &Path,
        approve: impl FnOnce(CourseSetupCounts) -> bool,
    ) -> Result<Option<CourseSetupCounts>, ApplyError> {
        // This guard runs before accessing either Notion or the state directory.
        self.require_mock_writes()?;
        self.apply_checked_course_setup(
            self,
            configured,
            input,
            directory,
            &WriteAccess::Mock,
            |preview| Ok(approve(preview.counts)),
        )
    }

    fn apply_checked_course_setup(
        &self,
        writer: &NotionClient,
        configured: &NotionTarget,
        input: &CourseSetupInput<'_>,
        directory: &Path,
        access: &WriteAccess<'_>,
        approve: impl FnOnce(&CourseSetupPreview) -> Result<bool, ApplyError>,
    ) -> Result<Option<CourseSetupCounts>, ApplyError> {
        let _lock = TargetLock::acquire(directory)?;
        let target = self.setup_target(configured)?;
        let path = directory.join(SETUP_STATE_FILE);
        let state = StateStore::read(&path, &target.rule_schema.id)?.unwrap_or_default();
        let snapshot = self.course_setup_snapshot(&target)?;
        let plan = snapshot.plan(input, &state);
        let preview = snapshot.preview(&target, input, &plan)?;
        if !approve(&preview)? {
            return Ok(None);
        }
        // Validate both schemata and reread the complete snapshots after approval.
        let refreshed = self.setup_target(configured)?;
        let snapshot = self.course_setup_snapshot(&refreshed)?;
        let fresh_plan = snapshot.plan(input, &state);
        if snapshot.preview(&refreshed, input, &fresh_plan)?.plan_id != preview.plan_id {
            return Err(ApplyError::PlanChanged);
        }
        if preview.counts.create_rules == 0 && state.pending == 0 {
            return Ok(Some(preview.counts));
        }
        StateStore::initialize(&path, &target.rule_schema.id)?;
        let store = StateStore::for_apply(&path, &target.rule_schema.id)?;
        for (candidate, action) in input.candidates.iter().zip(plan) {
            match action {
                Action::Hold(_) => {}
                Action::ReuseRule { page_id, .. } => {
                    reconcile(&store, &candidate.rule_key, &page_id)?
                }
                Action::CreateRule(destination) => {
                    self.check_new_rule(&target, candidate)?;
                    if candidate.webclass.is_some() {
                        let courses = self.read_setup_courses(&target)?;
                        if choose_course_destination(
                            candidate,
                            input.term,
                            &courses,
                            &state,
                            &state.page_owners(),
                        ) != Action::CreateRule(destination.clone())
                        {
                            return Err(ApplyError::PlanChanged);
                        }
                    }
                    let is_new_course = destination == Destination::NewCourse;
                    let course = writer.setup_course_destination(
                        &target,
                        input.term,
                        candidate,
                        destination,
                        &store,
                        access,
                    )?;
                    if is_new_course {
                        // The course POST has completed. A rule may have been
                        // added by another writer while it was in flight.
                        self.check_new_rule(&target, candidate)?;
                    }
                    let props = target.rule_properties(input.term, candidate, course.as_deref())?;
                    let request = PreparedWrite::create_page(
                        &target.rule_schema.id,
                        candidate.rule_key.clone(),
                        props,
                    );
                    writer.setup_post(&store, &request, access, |page| {
                        target.verify_rule(page, input.term, candidate, course.as_deref())
                    })?;
                }
            }
        }
        Ok(Some(preview.counts))
    }

    fn setup_target<'a>(&self, target: &'a NotionTarget) -> Result<SetupTarget<'a>, NotionError> {
        let rules = target
            .course_rules
            .as_ref()
            .ok_or(NotionError::Configuration)?;
        let properties = rules.setup.as_ref().ok_or(NotionError::Configuration)?;
        let schema = self.retrieve_schema(target)?;
        let rule_schema = self.retrieve_course_rule_schema(rules, &schema)?;
        Ok(SetupTarget {
            rules,
            properties,
            schema,
            rule_schema,
        })
    }

    fn course_setup_snapshot(
        &self,
        target: &SetupTarget<'_>,
    ) -> Result<CourseSetupSnapshot, NotionError> {
        let rules =
            self.read_course_rules_with_schema(target.rules, &target.schema, &target.rule_schema)?;
        Ok(CourseSetupSnapshot {
            rules,
            courses: self.read_setup_courses(target)?,
        })
    }

    fn read_setup_courses(
        &self,
        target: &SetupTarget<'_>,
    ) -> Result<HashMap<String, CoursePage>, NotionError> {
        self.query_course_pages(
            &target.schema,
            &[
                "title",
                &target.rules.course_term,
                &target.properties.group_id,
            ],
            |_, page| {
                let title = text(page_property(page, "title", "title")?)?;
                Ok(CoursePage {
                    comparison_name: comparison_name(&title),
                    title,
                    term: text(page_property(page, &target.rules.course_term, "rich_text")?)?,
                    group_id: text(page_property(
                        page,
                        &target.properties.group_id,
                        "rich_text",
                    )?)?,
                })
            },
        )
    }

    fn check_new_rule(
        &self,
        target: &SetupTarget<'_>,
        candidate: &Candidate<'_>,
    ) -> Result<(), ApplyError> {
        let props = target.rules;
        let filter = json!({"property":props.external_id,"rich_text":{"equals":candidate.key.id}});
        let rows = self.query_filtered_data_source_pages(
            &target.rule_schema.id,
            &target.rule_schema.database,
            &[&props.source, &props.scope, &props.external_id],
            Some(&filter),
            |_, page| {
                Ok(read_text(page, &props.external_id)? == candidate.key.id
                    && read_text(page, &props.scope)? == candidate.key.scope
                    && page_property(page, &props.source, "select")?["id"]
                        == target.source_option(candidate))
            },
        )?;
        if rows.into_values().any(|matches| matches) {
            return Err(ApplyError::PlanChanged);
        }
        Ok(())
    }

    fn setup_course_destination(
        &self,
        target: &SetupTarget<'_>,
        term: &str,
        candidate: &Candidate<'_>,
        destination: Destination,
        store: &StateStore,
        access: &WriteAccess<'_>,
    ) -> Result<Option<String>, ApplyError> {
        match destination {
            Destination::Unset | Destination::Ambiguous => Ok(None),
            Destination::Existing(id) => {
                let key = candidate
                    .course_key
                    .as_ref()
                    .ok_or(NotionError::Configuration)?;
                reconcile(store, key, &id)?;
                Ok(Some(id))
            }
            Destination::NewCourse => {
                let course = candidate.webclass.ok_or(NotionError::Configuration)?;
                let key = candidate
                    .course_key
                    .as_ref()
                    .ok_or(NotionError::Configuration)?;
                let props = target.course_properties(term, course)?;
                let request =
                    PreparedWrite::create_page(&target.schema.courses_id, key.clone(), props);
                self.setup_post(store, &request, access, |page| {
                    let id = checked_page_id(
                        page,
                        &target.schema.courses_id,
                        &target.schema.courses_database,
                    )?;
                    if read_text(page, &target.properties.group_id)? != course.group_id
                        || read_text(page, &target.rules.course_term)? != term
                    {
                        return Err(NotionError::InvalidResponse);
                    }
                    Ok(id)
                })
                .map(Some)
            }
        }
    }

    fn setup_post(
        &self,
        store: &StateStore,
        request: &PreparedWrite,
        access: &WriteAccess<'_>,
        verify: impl FnOnce(&Value) -> Result<String, NotionError>,
    ) -> Result<String, ApplyError> {
        let key = request.key();
        if !store.reserve_create(key)? {
            return Err(ApplyError::CreateBlocked);
        }
        let response = match self.send_write(request, access) {
            Ok(page) => verify(&page),
            Err(error) if explicitly_rejected(error) => {
                store.release_rejected_create(key)?;
                return Err(error.into());
            }
            Err(error) => Err(error),
        };
        let id = match response {
            Ok(id) if !store.is_owned_by_another_key(&id, key)? => id,
            failure => {
                store.mark_unknown(key)?;
                return Err(failure.err().unwrap_or(NotionError::Conflict).into());
            }
        };
        store.confirm(key, &id)?;
        Ok(id)
    }
}

fn choose_action(
    candidate: &Candidate<'_>,
    term: &str,
    rules: &CourseRules,
    courses: &HashMap<String, CoursePage>,
    state: &StateSummary,
    owners: &HashMap<&str, &SyncKey>,
) -> Action {
    let rule = rules.by_key.get(&candidate.key);
    if !binding_agrees(
        state,
        owners,
        &candidate.rule_key,
        rule.map(|rule| rule.page_id.as_str()),
    ) {
        return Action::Hold("rule_requires_reconciliation");
    }
    if let Some(rule) = rule {
        let waiting = match &rule.resolution {
            Ok(course) if course.term == term => false,
            Err(DeferredReason::CourseExcluded) => false,
            Err(DeferredReason::CourseUnconfigured) => candidate.webclass.is_some(),
            Ok(_) => return Action::Hold("existing_rule_term_mismatch"),
            Err(DeferredReason::CourseRuleDuplicate) => return Action::Hold("duplicate_rule"),
            Err(_) => return Action::Hold("invalid_existing_rule"),
        };
        return Action::ReuseRule {
            page_id: rule.page_id.clone(),
            waiting,
        };
    }
    if title_value(candidate.name).is_err()
        || !fits_text(&candidate.key.id)
        || !fits_text(&candidate.key.scope)
    {
        return Action::Hold("invalid_source_fields");
    }
    choose_course_destination(candidate, term, courses, state, owners)
}

fn choose_course_destination(
    candidate: &Candidate<'_>,
    term: &str,
    courses: &HashMap<String, CoursePage>,
    state: &StateSummary,
    owners: &HashMap<&str, &SyncKey>,
) -> Action {
    let Some(course_key) = &candidate.course_key else {
        return Action::CreateRule(Destination::Unset);
    };
    let mut exact = courses
        .iter()
        .filter(|(_, page)| page.group_id == candidate.key.id);
    let first = exact.next();
    if exact.next().is_some() {
        return Action::Hold("duplicate_group_id");
    }
    if !binding_agrees(state, owners, course_key, first.map(|(id, _)| id.as_str())) {
        return Action::Hold("course_requires_reconciliation");
    }
    if let Some((id, page)) = first {
        return if page.term == term {
            Action::CreateRule(Destination::Existing(id.clone()))
        } else {
            Action::Hold("existing_course_term_mismatch")
        };
    }
    if courses.values().any(|page| {
        page.comparison_name == candidate.comparison_name
            && (page.term == term || !valid_term(&page.term))
    }) {
        return Action::CreateRule(Destination::Ambiguous);
    }
    Action::CreateRule(Destination::NewCourse)
}

// Name comparisons can suggest a review or prevent creation. They cannot link a page.
fn comparison_name(name: &str) -> String {
    name.chars()
        .filter(|ch| !ch.is_whitespace())
        .map(|ch| {
            if matches!(ch, '\u{ff01}'..='\u{ff5e}') {
                char::from_u32(u32::from(ch) - 0xfee0).unwrap_or(ch)
            } else {
                ch
            }
        })
        .flat_map(char::to_lowercase)
        .collect()
}

impl CourseSetupSnapshot {
    fn plan(&self, input: &CourseSetupInput<'_>, state: &StateSummary) -> Vec<Action> {
        let owners = state.page_owners();
        input
            .candidates
            .iter()
            .map(|candidate| {
                choose_action(
                    candidate,
                    input.term,
                    &self.rules,
                    &self.courses,
                    state,
                    &owners,
                )
            })
            .collect()
    }

    fn preview(
        &self,
        target: &SetupTarget<'_>,
        input: &CourseSetupInput<'_>,
        plan: &[Action],
    ) -> Result<CourseSetupPreview, NotionError> {
        let mut pages: Vec<_> = self.courses.iter().collect();
        pages.sort_unstable_by(|(id, course), (other_id, other)| {
            (&course.title, id).cmp(&(&other.title, other_id))
        });
        let moodle: Vec<_> = input
            .candidates
            .iter()
            .filter(|candidate| candidate.webclass.is_none())
            .collect();
        let describe_course = |id: &str, page: &CoursePage| ExistingCoursePreview {
            page_id: id.to_owned(),
            name: page.title.clone(),
            term: present(&page.term),
            group_id: present(&page.group_id),
            matching_moodle_categories: moodle
                .iter()
                .filter(|candidate| candidate.comparison_name == page.comparison_name)
                .map(|candidate| candidate.name.to_owned())
                .collect(),
        };
        let items = input
            .candidates
            .iter()
            .zip(plan)
            .map(|(candidate, action)| {
                let rule = self.rules.by_key.get(&candidate.key);
                let resolved = rule.and_then(|rule| rule.resolution.as_ref().ok());
                let (course_action, rule_action, reason, proposed_rule) = match action {
                    Action::Hold(reason) => ("hold", "hold", *reason, None),
                    Action::ReuseRule { waiting: true, .. } => {
                        ("hold", "reuse", "user_confirmation_required", None)
                    }
                    Action::ReuseRule { .. } => match rule.map(|rule| &rule.resolution) {
                        Some(Ok(_)) => ("reuse", "reuse", "confirmed_rule", None),
                        Some(Err(DeferredReason::CourseExcluded)) => {
                            ("none", "reuse", "excluded_rule", None)
                        }
                        _ => ("none", "reuse", "user_confirmation_required", None),
                    },
                    Action::CreateRule(destination) => {
                        let (action, reason, status, relation, course_page_id) = match destination {
                            Destination::NewCourse => {
                                ("create", "new_group", "確定", "new_course", None)
                            }
                            Destination::Existing(id) => (
                                "reuse",
                                "exact_group_and_term",
                                "確定",
                                "existing_course",
                                Some(id.clone()),
                            ),
                            Destination::Ambiguous => {
                                ("hold", "existing_name_match", "未設定", "pending", None)
                            }
                            Destination::Unset => (
                                "none",
                                "moodle_user_confirmation",
                                "未設定",
                                "pending",
                                None,
                            ),
                        };
                        (
                            action,
                            "create",
                            reason,
                            Some(CourseSetupRulePreview {
                                status,
                                relation,
                                course_page_id,
                            }),
                        )
                    }
                };
                CourseSetupPreviewItem {
                    source: if candidate.webclass.is_some() {
                        "HOPPII"
                    } else {
                        "Moodle"
                    },
                    name: candidate.name.to_owned(),
                    scope: candidate.key.scope.clone(),
                    external_id: candidate.key.id.clone(),
                    term: input.term.to_owned(),
                    day_of_week: candidate
                        .webclass
                        .and_then(|course| present(&course.day_of_week)),
                    class_order: candidate
                        .webclass
                        .and_then(|course| present(&course.class_order)),
                    course_action,
                    rule_action,
                    reason,
                    existing_rule_id: rule
                        .filter(|rule| {
                            rule.resolution.as_ref().err()
                                != Some(&DeferredReason::CourseRuleDuplicate)
                        })
                        .map(|rule| rule.page_id.clone()),
                    proposed_rule,
                    existing_course_candidates: pages
                        .iter()
                        .filter(|(id, page)| {
                            resolved.is_some_and(|course| &course.page_id == *id)
                                || (candidate.webclass.is_some()
                                    && page.group_id == candidate.key.id)
                                || page.comparison_name == candidate.comparison_name
                        })
                        .map(|(id, page)| describe_course(id, page))
                        .collect(),
                    other_source_name_matches: input
                        .candidates
                        .iter()
                        .filter(|other| {
                            other.key.origin != candidate.key.origin
                                && other.comparison_name == candidate.comparison_name
                        })
                        .map(|other| other.name.to_owned())
                        .collect(),
                }
            })
            .collect();
        let mut preview = CourseSetupPreview {
            plan_id: String::new(),
            counts: counts(plan),
            webclass_courses: input.candidates.len() - moodle.len(),
            moodle_categories: moodle.len(),
            items,
            existing_courses_without_group_id: pages
                .iter()
                .filter(|(_, page)| page.group_id.trim().is_empty())
                .map(|(id, page)| describe_course(id, page))
                .collect(),
        };
        let bytes = serde_json::to_vec(&(target, input.term, plan, &preview))
            .map_err(|_| NotionError::Configuration)?;
        let mut tagged = b"assignment-manager/course-setup-plan/v1\0".to_vec();
        tagged.extend(bytes);
        for byte in digest(&SHA256, &tagged).as_ref() {
            write!(preview.plan_id, "{byte:02x}").expect("writing to String");
        }
        Ok(preview)
    }
}

fn present(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_owned())
}

fn binding_agrees(
    state: &StateSummary,
    owners: &HashMap<&str, &SyncKey>,
    key: &SyncKey,
    page: Option<&str>,
) -> bool {
    state.bindings.get(key).is_none_or(|binding| {
        page.is_some()
            && binding.status != BindingStatus::Reconcile
            && binding
                .page_id
                .as_deref()
                .is_none_or(|known| Some(known) == page)
    }) && page
        .and_then(|page| owners.get(page))
        .is_none_or(|owner| *owner == key)
}

fn reconcile(store: &StateStore, key: &SyncKey, page: &str) -> Result<(), ApplyError> {
    if let Some(binding) = store.binding(key)?
        && (binding.status != BindingStatus::Linked || binding.page_id.as_deref() != Some(page))
    {
        store.confirm(key, page)?;
    }
    Ok(())
}

fn read_text(page: &Value, id: &str) -> Result<String, NotionError> {
    text(page_property(page, id, "rich_text")?)
}

fn rich(value: &str) -> Value {
    json!({"rich_text":[text_item(value)]})
}

impl SetupTarget<'_> {
    fn course_properties(&self, term: &str, course: &WebClassCourse) -> Result<Value, NotionError> {
        let mut values = json!({
            "title": title_value(&course.group_name)?,
            (&self.rules.course_term): rich(term),
            (&self.properties.group_id): rich(&course.group_id),
        });
        if let Some(year) = &self.properties.year {
            values[year] = json!({"number":course.year});
        }
        for (property, value) in [
            (&self.properties.day, &course.day_of_week),
            (&self.properties.period, &course.class_order),
        ] {
            if let Some(id) = property
                && !value.trim().is_empty()
                && fits_text(value)
            {
                values[id] = rich(value);
            }
        }
        Ok(values)
    }

    fn rule_properties(
        &self,
        term: &str,
        candidate: &Candidate<'_>,
        course: Option<&str>,
    ) -> Result<Value, NotionError> {
        let mut values = json!({
            "title": title_value(candidate.name)?,
            (&self.rules.source): {"select":{"id":self.source_option(candidate)}},
            (&self.rules.scope): rich(&candidate.key.scope),
            (&self.rules.external_id): rich(&candidate.key.id),
            (&self.rules.term): rich(term),
            (&self.rules.status): {"select":{"id":self.status_option(course)}},
        });
        if let Some(id) = course {
            values[&self.rules.course] = relation_value(id)?;
        }
        Ok(values)
    }

    fn verify_rule(
        &self,
        page: &Value,
        term: &str,
        candidate: &Candidate<'_>,
        course: Option<&str>,
    ) -> Result<String, NotionError> {
        let id = checked_page_id(page, &self.rule_schema.id, &self.rule_schema.database)?;
        let (links, more) = page_relations(page, &self.rules.course)?;
        if more
            || !links.iter().map(String::as_str).eq(course)
            || read_text(page, &self.rules.scope)? != candidate.key.scope
            || read_text(page, &self.rules.external_id)? != candidate.key.id
            || read_text(page, &self.rules.term)? != term
            || page_property(page, &self.rules.source, "select")?["id"]
                != self.source_option(candidate)
            || page_property(page, &self.rules.status, "select")?["id"]
                != self.status_option(course)
        {
            return Err(NotionError::InvalidResponse);
        }
        Ok(id)
    }

    fn source_option(&self, candidate: &Candidate<'_>) -> &str {
        if candidate.webclass.is_some() {
            &self.rule_schema.hoppii
        } else {
            &self.rule_schema.moodle
        }
    }

    fn status_option(&self, course: Option<&str>) -> &str {
        if course.is_some() {
            &self.rule_schema.confirmed
        } else {
            &self.rule_schema.unset
        }
    }
}

#[cfg(test)]
#[test]
fn course_setup_write_access_rejects_assignment_parents_and_other_destinations() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let client = NotionClient::for_mock(
        listener.local_addr().unwrap(),
        "fixture",
        std::time::Duration::from_millis(100),
    )
    .unwrap();
    let access = WriteAccess::CourseSetup(ApprovedCourseSetup(&client));
    let key = SyncKey::derive(&ExternalRecordKey {
        origin: OriginKind::WebClass,
        scope: "default".into(),
        id: "group".into(),
    })
    .unwrap();
    for id in [
        MOODLE_VERIFICATION_ASSIGNMENTS,
        "00000000-0000-4000-8000-000000000001",
    ] {
        let request = PreparedWrite::create_page(id, key.clone(), json!({}));
        assert_eq!(
            client.send_write(&request, &access),
            Err(NotionError::MethodDisabled)
        );
    }
}
