//! Read the authoritative course rules before planning any assignment changes.

use std::collections::{BTreeMap, HashMap, hash_map::Entry};

use serde::Serialize;
use serde_json::Value;

use crate::{
    import::{DEFAULT_COURSE_SOURCE_SCOPE, ExternalRecordKey, OriginKind},
    moodle_http::{MoodleExportKind, MoodleFeed},
};

use super::{
    CourseRulesTarget, CourseSetupVerification, DeferredReason, NotionClient, NotionError,
    NotionTarget, SyncAssignment, UniversitySource,
    trial::{PRODUCTION_ASSIGNMENTS, PRODUCTION_COURSES, PRODUCTION_RULES},
    wire::{
        Schema, data_source_parent, page_property, page_relations, property, relation_targets,
        select_option_ids, text, uuid,
    },
};

/// Validate the explicitly selected live assignment read scope.
pub fn live_assignment_read_target(
    target: &NotionTarget,
    production: bool,
) -> Result<String, NotionError> {
    if !production {
        return Ok(CourseSetupVerification::new(target)?
            .assignments_id()
            .into());
    }
    let (assignments, courses) = target.ids()?;
    let rules = target
        .course_rules
        .as_ref()
        .ok_or(NotionError::Configuration)?;
    if assignments != PRODUCTION_ASSIGNMENTS
        || courses != PRODUCTION_COURSES
        || uuid(&rules.data_source_id).as_deref() != Some(PRODUCTION_RULES)
    {
        return Err(NotionError::MethodDisabled);
    }
    Ok(assignments)
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CourseRuleSummary {
    pub rows: usize,
    pub confirmed: usize,
    pub unset: usize,
    pub excluded: usize,
    pub duplicate_keys: usize,
    pub invalid_rows: usize,
    pub invalid_relations: usize,
    pub unverified_terms: usize,
}

impl CourseRuleSummary {
    pub fn has_issues(&self) -> bool {
        self.unset
            + self.duplicate_keys
            + self.invalid_rows
            + self.invalid_relations
            + self.unverified_terms
            != 0
    }
}

pub(super) struct ConfirmedCourse {
    pub(super) page_id: String,
    pub(super) term: String,
}

type RuleResult = Result<ConfirmedCourse, DeferredReason>;

pub(super) struct CourseRuleEntry {
    pub(super) page_id: String,
    pub(super) name: String,
    pub(super) resolution: RuleResult,
}

pub struct VerificationCourseRule {
    pub key: ExternalRecordKey,
    pub source_name: String,
    pub destination: Result<VerificationCourse, DeferredReason>,
}

pub struct VerificationCourse {
    pub title: String,
    pub term: String,
    pub page_id: String,
}

#[derive(PartialEq, Eq, Serialize)]
pub(super) struct CourseRuleSchema {
    pub(super) id: String,
    pub(super) database: String,
    pub(super) hoppii: String,
    pub(super) moodle: String,
    pub(super) unset: String,
    pub(super) confirmed: String,
    excluded: String,
}

pub(super) struct CourseRules {
    pub(super) by_key: HashMap<ExternalRecordKey, CourseRuleEntry>,
    pub summary: CourseRuleSummary,
}

impl CourseRules {
    pub(super) fn resolve(&self, assignment: &SyncAssignment) -> Result<&str, DeferredReason> {
        let rule = assignment
            .course_key
            .as_ref()
            .and_then(|key| self.by_key.get(key))
            .ok_or(DeferredReason::CourseUnconfigured)?
            .resolution
            .as_ref()
            .map_err(|reason| *reason)?;
        if assignment.source == UniversitySource::WebClass
            && assignment.course_term.as_deref() != Some(rule.term.as_str())
        {
            return Err(DeferredReason::CourseTermUnverified);
        }
        // A unique confirmed Moodle rule asserts that this category belongs to
        // one term within the feed. The ICS supplies no independent term value.
        Ok(&rule.page_id)
    }
}

/// The year and source-native semester token, as in `2026:2`.
pub(super) fn valid_term(term: &str) -> bool {
    term.split_once(':').is_some_and(|(year, semester)| {
        year.len() == 4
            && year.bytes().all(|byte| byte.is_ascii_digit())
            && !year.starts_with('0')
            && !semester.is_empty()
            && semester.trim() == semester
            && !semester.contains(':')
            && !semester.chars().any(char::is_control)
    })
}

impl NotionClient {
    /// Select content requests from the approved live Notion data sources.
    pub fn live_webclass_scopes(
        &self,
        target: &NotionTarget,
        production: bool,
    ) -> Result<BTreeMap<String, String>, NotionError> {
        live_assignment_read_target(target, production)?;
        let config = target
            .course_rules
            .as_ref()
            .ok_or(NotionError::Configuration)?;
        let schema = self.retrieve_schema(target)?;
        let rules = self.read_course_rules(config, &schema)?;
        Ok(rules
            .by_key
            .into_iter()
            .filter_map(|(key, rule)| {
                if key.origin != OriginKind::WebClass {
                    return None;
                }
                rule.resolution.ok().map(|course| (key.id, course.term))
            })
            .collect())
    }

    pub(super) fn read_course_rules(
        &self,
        target: &CourseRulesTarget,
        schema: &Schema,
    ) -> Result<CourseRules, NotionError> {
        let rules = self.retrieve_course_rule_schema(target, schema)?;
        self.read_course_rules_with_schema(target, schema, &rules)
    }

    pub(super) fn retrieve_course_rule_schema(
        &self,
        target: &CourseRulesTarget,
        schema: &Schema,
    ) -> Result<CourseRuleSchema, NotionError> {
        let id = uuid(&target.data_source_id).ok_or(NotionError::Configuration)?;
        let data = self.get_data_source(&id)?;
        let database = data_source_parent(&data, &id)?;
        let properties = data["properties"]
            .as_object()
            .ok_or(NotionError::SchemaMismatch)?;
        let fields = target.fields();
        for (id, kind) in fields {
            if !property(properties, id, kind).is_some_and(|prop| prop[kind].is_object()) {
                return Err(NotionError::SchemaMismatch);
            }
        }
        let relation =
            property(properties, &target.course, "relation").ok_or(NotionError::SchemaMismatch)?;
        if !relation_targets(
            &relation["relation"],
            &schema.courses_id,
            &schema.courses_database,
        ) {
            return Err(NotionError::SchemaMismatch);
        }
        let select = |id| {
            property(properties, id, "select")
                .map(|prop| &prop["select"])
                .ok_or(NotionError::SchemaMismatch)
        };
        let [hoppii, moodle] = select_option_ids(select(&target.source)?, ["HOPPII", "Moodle"])?;
        let [unset, confirmed, excluded] =
            select_option_ids(select(&target.status)?, ["未設定", "確定", "対象外"])?;
        Ok(CourseRuleSchema {
            id,
            database,
            hoppii: hoppii.to_owned(),
            moodle: moodle.to_owned(),
            unset: unset.to_owned(),
            confirmed: confirmed.to_owned(),
            excluded: excluded.to_owned(),
        })
    }

    pub(super) fn read_course_rules_with_schema(
        &self,
        target: &CourseRulesTarget,
        schema: &Schema,
        rules: &CourseRuleSchema,
    ) -> Result<CourseRules, NotionError> {
        let projection = target.fields().map(|(id, _)| id);
        let rows = self.query_data_source_pages(
            &rules.id,
            &rules.database,
            &projection,
            |page_id, page| {
                let source = page_property(page, &target.source, "select")?;
                let scope = text(page_property(page, &target.scope, "rich_text")?)?;
                let external_id = text(page_property(page, &target.external_id, "rich_text")?)?;
                let origin = match source["id"].as_str() {
                    Some(source)
                        if source == rules.hoppii && scope == DEFAULT_COURSE_SOURCE_SCOPE =>
                    {
                        OriginKind::WebClass
                    }
                    Some(source)
                        if source == rules.moodle
                            && MoodleFeed::new(
                                &scope,
                                MoodleExportKind::CourseRelated,
                                "https://fixture.invalid/calendar",
                            )
                            .is_ok() =>
                    {
                        OriginKind::Moodle
                    }
                    _ => return Ok(None),
                };
                if external_id.trim().is_empty() {
                    return Ok(None);
                }
                let key = ExternalRecordKey {
                    origin,
                    scope,
                    id: external_id,
                };
                let name = text(page_property(page, "title", "title")?)?;
                let status = page_property(page, &target.status, "select")?;
                let resolution = match status["id"].as_str() {
                    _ if name.trim().is_empty() => Err(DeferredReason::CourseRuleInvalid),
                    None if status.is_null() => Err(DeferredReason::CourseUnconfigured),
                    Some(status) if status == rules.unset => {
                        Err(DeferredReason::CourseUnconfigured)
                    }
                    Some(status) if status == rules.excluded => Err(DeferredReason::CourseExcluded),
                    Some(status) if status == rules.confirmed => confirmed_course(page, target)?,
                    _ => Err(DeferredReason::CourseRuleInvalid),
                };
                Ok(Some((
                    key,
                    CourseRuleEntry {
                        page_id: page_id.to_owned(),
                        name,
                        resolution,
                    },
                )))
            },
        )?;

        let mut summary = CourseRuleSummary {
            rows: rows.len(),
            ..Default::default()
        };
        let mut by_key = HashMap::new();
        for row in rows.into_values() {
            let Some((key, rule)) = row else {
                summary.invalid_rows += 1;
                continue;
            };
            // The v1 assignment identity has no academic term. Multiple rows
            // for an external key remain ambiguous even across different terms.
            match by_key.entry(key) {
                Entry::Vacant(entry) => {
                    entry.insert(rule);
                }
                Entry::Occupied(entry) => {
                    entry.into_mut().resolution = Err(DeferredReason::CourseRuleDuplicate);
                }
            }
        }

        if by_key.values().any(|rule| rule.resolution.is_ok()) {
            let terms = self.query_course_pages(schema, &[&target.course_term], |_, page| {
                text(page_property(page, &target.course_term, "rich_text")?)
            })?;
            for rule in by_key.values_mut() {
                if let Ok(course) = &rule.resolution {
                    rule.resolution = match terms.get(&course.page_id) {
                        None => Err(DeferredReason::CourseRuleInvalidRelation),
                        Some(term) if term != &course.term => {
                            Err(DeferredReason::CourseTermUnverified)
                        }
                        Some(_) => continue,
                    };
                }
            }
        }
        for rule in by_key.values() {
            match &rule.resolution {
                Ok(_) => summary.confirmed += 1,
                Err(DeferredReason::CourseUnconfigured) => summary.unset += 1,
                Err(DeferredReason::CourseExcluded) => summary.excluded += 1,
                Err(DeferredReason::CourseRuleDuplicate) => summary.duplicate_keys += 1,
                Err(DeferredReason::CourseRuleInvalidRelation) => summary.invalid_relations += 1,
                Err(DeferredReason::CourseTermUnverified) => summary.unverified_terms += 1,
                Err(_) => summary.invalid_rows += 1,
            }
        }
        Ok(CourseRules { by_key, summary })
    }
}

fn confirmed_course(page: &Value, target: &CourseRulesTarget) -> Result<RuleResult, NotionError> {
    let term = text(page_property(page, &target.term, "rich_text")?)?;
    if !valid_term(&term) {
        return Ok(Err(DeferredReason::CourseTermUnverified));
    }
    // Exactly one complete Relation is required. `has_more` already proves
    // this Relation cannot consist of a single destination.
    let Ok((mut links, false)) = page_relations(page, &target.course) else {
        return Ok(Err(DeferredReason::CourseRuleInvalidRelation));
    };
    if links.len() != 1 {
        return Ok(Err(DeferredReason::CourseRuleInvalidRelation));
    }
    let page_id = links.remove(0);
    Ok(Ok(ConfirmedCourse { page_id, term }))
}
