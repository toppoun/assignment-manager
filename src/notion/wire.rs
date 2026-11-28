use std::collections::HashSet;

use chrono::{DateTime, NaiveDate, SecondsFormat};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::domain::Deadline;

use super::{
    NotionAssignment, NotionError, PlanAction, PlannedAssignment, SyncKey, UniversitySource,
};

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Property IDs returned by the API for the preconfigured assignment schema.
/// Renaming a column does not change these IDs.
pub struct NotionPropertyIds {
    /// Notion title (the built-in property ID is always `title`).
    pub title: String,
    /// Date, accepting a single calendar date or an RFC 3339 instant.
    pub deadline: String,
    /// Relation column populated from a verified mapping when empty or on creation.
    pub course: String,
    /// URL column verified during schema validation and omitted from writes.
    pub submission_url: String,
    /// Rich text carrying the canonical versioned key, empty for manual tasks.
    #[serde(default)]
    pub sync_key: String,
    /// Select with existing Moodle and WebClass options, written by option ID.
    #[serde(default)]
    pub source: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NotionTarget {
    pub assignments_data_source_id: String,
    pub courses_data_source_id: String,
    pub properties: NotionPropertyIds,
    #[serde(default)]
    pub course_rules: Option<CourseRulesTarget>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CourseRulesTarget {
    pub data_source_id: String,
    /// Academic term property on the existing courses data source.
    pub course_term: String,
    pub source: String,
    pub scope: String,
    pub external_id: String,
    pub term: String,
    pub course: String,
    pub status: String,
    #[serde(default)]
    pub setup: Option<CourseSetupProperties>,
}

/// Properties on the existing courses data source used for course setup.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CourseSetupProperties {
    pub group_id: String,
    pub year: Option<String>,
    pub day: Option<String>,
    pub period: Option<String>,
}

impl CourseSetupProperties {
    fn fields(&self) -> impl Iterator<Item = (&str, &'static str)> {
        [
            (Some(self.group_id.as_str()), "rich_text"),
            (self.year.as_deref(), "number"),
            (self.day.as_deref(), "rich_text"),
            (self.period.as_deref(), "rich_text"),
        ]
        .into_iter()
        .filter_map(|(id, kind)| id.map(|id| (id, kind)))
    }
}

impl CourseRulesTarget {
    pub(super) fn fields(&self) -> [(&str, &'static str); 7] {
        [
            ("title", "title"),
            (&self.source, "select"),
            (&self.scope, "rich_text"),
            (&self.external_id, "rich_text"),
            (&self.term, "rich_text"),
            (&self.course, "relation"),
            (&self.status, "select"),
        ]
    }
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub(super) struct Schema {
    pub assignments_id: String,
    pub assignments_database: String,
    pub courses_id: String,
    pub courses_database: String,
    pub properties: NotionPropertyIds,
    moodle_option: String,
    webclass_option: String,
}

/// Canonical form of an ID used as a local credential namespace or API target.
pub fn uuid(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let valid = match bytes.len() {
        32 => bytes.iter().all(u8::is_ascii_hexdigit),
        36 => bytes.iter().enumerate().all(|(index, c)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        }),
        _ => false,
    };
    if !valid {
        return None;
    }
    let hex = value.replace('-', "").to_ascii_lowercase();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

pub(super) fn property_id(id: &str) -> bool {
    // The API returns property IDs pre-encoded. Keep escapes in the path and
    // reject any ID that could become a URL dot segment.
    let mut bytes = id.bytes();
    let mut non_dot = false;
    while let Some(c) = bytes.next() {
        if c == b'%' {
            let Some(pair) = bytes.next().zip(bytes.next()) else {
                return false;
            };
            if !pair.0.is_ascii_hexdigit() || !pair.1.is_ascii_hexdigit() {
                return false;
            }
            non_dot |= !(pair.0 == b'2' && pair.1.eq_ignore_ascii_case(&b'e'));
        } else if c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'~' | b'.') {
            non_dot |= c != b'.';
        } else {
            return false;
        }
    }
    non_dot && id.len() <= 256
}

impl NotionPropertyIds {
    pub(super) fn assignment_projection(&self) -> [&str; 5] {
        [
            &self.sync_key,
            &self.title,
            &self.deadline,
            &self.source,
            &self.course,
        ]
    }

    pub(super) fn fields(&self) -> [(&str, &'static str); 6] {
        [
            (&self.title, "title"),
            (&self.deadline, "date"),
            (&self.course, "relation"),
            (&self.submission_url, "url"),
            (&self.sync_key, "rich_text"),
            (&self.source, "select"),
        ]
    }
}

impl NotionTarget {
    /// Canonical assignment data source ID after checking local configuration.
    pub fn validated_id(&self) -> Result<String, NotionError> {
        self.ids().map(|(assignments, _)| assignments)
    }

    pub(super) fn ids(&self) -> Result<(String, String), NotionError> {
        let mut seen = HashSet::new();
        if self.properties.title != "title"
            || self
                .properties
                .fields()
                .into_iter()
                .enumerate()
                .any(|(index, (id, _))| {
                    (index < 4 || !id.is_empty()) && (!property_id(id) || !seen.insert(id))
                })
        {
            return Err(NotionError::Configuration);
        }
        let assignments =
            uuid(&self.assignments_data_source_id).ok_or(NotionError::Configuration)?;
        let courses = uuid(&self.courses_data_source_id).ok_or(NotionError::Configuration)?;
        if assignments == courses {
            return Err(NotionError::Configuration);
        }
        if let Some(rules) = &self.course_rules {
            let id = uuid(&rules.data_source_id).ok_or(NotionError::Configuration)?;
            let mut seen = HashSet::new();
            if id == assignments
                || id == courses
                || rules.course_term == "title"
                || !property_id(&rules.course_term)
                || rules
                    .fields()
                    .into_iter()
                    .any(|(id, _)| !property_id(id) || !seen.insert(id))
            {
                return Err(NotionError::Configuration);
            }
            if let Some(setup) = &rules.setup {
                let mut seen = HashSet::from(["title", rules.course_term.as_str()]);
                if setup
                    .fields()
                    .any(|(id, _)| !property_id(id) || !seen.insert(id))
                {
                    return Err(NotionError::Configuration);
                }
            }
        }
        Ok((assignments, courses))
    }
}

pub(super) fn object_id(value: &Value, field: &str) -> Option<String> {
    uuid(value[field].as_str()?)
}

pub(super) fn data_source_parent(data: &Value, id: &str) -> Result<String, NotionError> {
    if data["object"] != "data_source"
        || object_id(data, "id").as_deref() != Some(id)
        || data["in_trash"].as_bool() != Some(false)
        || data["parent"]["type"] != "database_id"
    {
        return Err(NotionError::SchemaMismatch);
    }
    object_id(&data["parent"], "database_id").ok_or(NotionError::SchemaMismatch)
}

pub(super) fn property<'a>(
    properties: &'a Map<String, Value>,
    id: &str,
    kind: &str,
) -> Option<&'a Value> {
    let mut matches = properties
        .values()
        .filter(|prop| prop["id"].as_str() == Some(id));
    let prop = matches.next()?;
    (matches.next().is_none() && prop["type"].as_str() == Some(kind)).then_some(prop)
}

pub(super) fn relation_targets(relation: &Value, courses: &str, database: &str) -> bool {
    object_id(relation, "data_source_id").as_deref() == Some(courses)
        && object_id(relation, "database_id").as_deref() == Some(database)
        && matches!(
            relation["type"].as_str(),
            Some("single_property" | "dual_property")
        )
}

pub(super) fn source_options(select: &Value) -> Result<(String, String), NotionError> {
    let [moodle, webclass] = select_option_ids(select, ["Moodle", "WebClass"])?;
    Ok((moodle.to_owned(), webclass.to_owned()))
}

pub(super) fn select_option_ids<'a, const N: usize>(
    select: &'a Value,
    names: [&str; N],
) -> Result<[&'a str; N], NotionError> {
    let options = select["options"]
        .as_array()
        .ok_or(NotionError::SchemaMismatch)?;
    let mut option_ids = HashSet::new();
    let mut required = [""; N];
    for option in options {
        let id = option["id"]
            .as_str()
            .filter(|id| !id.trim().is_empty())
            .ok_or(NotionError::SchemaMismatch)?;
        if !option_ids.insert(id) {
            return Err(NotionError::SchemaMismatch);
        }
        if let Some(index) = names
            .iter()
            .position(|name| Some(*name) == option["name"].as_str())
        {
            if !required[index].is_empty() {
                return Err(NotionError::SchemaMismatch);
            }
            required[index] = id;
        }
    }
    required
        .iter()
        .all(|id| !id.is_empty())
        .then_some(required)
        .ok_or(NotionError::SchemaMismatch)
}

pub(super) fn page_relations(page: &Value, id: &str) -> Result<(Vec<String>, bool), NotionError> {
    let props = page["properties"]
        .as_object()
        .ok_or(NotionError::InvalidResponse)?;
    let prop = property(props, id, "relation").ok_or(NotionError::InvalidResponse)?;
    let relation = prop["relation"]
        .as_array()
        .ok_or(NotionError::InvalidResponse)?;
    let has_more = prop["has_more"]
        .as_bool()
        .ok_or(NotionError::InvalidResponse)?;
    let links = relation.iter().map(relation_id).collect::<Result<_, _>>()?;
    Ok((links, has_more))
}

impl Schema {
    pub(super) fn validate(
        target: &NotionTarget,
        ids: (String, String),
        data: &Value,
        related: &Value,
    ) -> Result<Self, NotionError> {
        let (assignments_id, courses_id) = ids;
        let assignments_database = data_source_parent(data, &assignments_id)?;
        let courses_database = data_source_parent(related, &courses_id)?;
        if let Some(rules) = &target.course_rules {
            let properties = related["properties"]
                .as_object()
                .ok_or(NotionError::SchemaMismatch)?;
            if !property(properties, &rules.course_term, "rich_text")
                .is_some_and(|prop| prop["rich_text"].is_object())
            {
                return Err(NotionError::SchemaMismatch);
            }
            if let Some(setup) = &rules.setup {
                for (id, kind) in [("title", "title")].into_iter().chain(setup.fields()) {
                    if !property(properties, id, kind).is_some_and(|prop| prop[kind].is_object()) {
                        return Err(NotionError::SchemaMismatch);
                    }
                }
            }
        }
        let properties = data["properties"]
            .as_object()
            .ok_or(NotionError::SchemaMismatch)?;
        for (id, kind) in target.properties.fields() {
            if id.is_empty() {
                return Err(NotionError::SchemaMismatch);
            }
            let prop = property(properties, id, kind).ok_or(NotionError::SchemaMismatch)?;
            if !prop[kind].is_object() {
                return Err(NotionError::SchemaMismatch);
            }
        }
        let relation = &property(properties, &target.properties.course, "relation")
            .ok_or(NotionError::SchemaMismatch)?["relation"];
        if !relation_targets(relation, &courses_id, &courses_database) {
            return Err(NotionError::SchemaMismatch);
        }
        let select = &property(properties, &target.properties.source, "select")
            .ok_or(NotionError::SchemaMismatch)?["select"];
        let (moodle_option, webclass_option) = source_options(select)?;
        Ok(Self {
            assignments_id,
            assignments_database,
            courses_id,
            courses_database,
            properties: target.properties.clone(),
            moodle_option,
            webclass_option,
        })
    }

    fn source_value(&self, source: UniversitySource) -> Value {
        let id = match source {
            UniversitySource::Moodle => &self.moodle_option,
            UniversitySource::WebClass => &self.webclass_option,
        };
        json!({"select":{"id":id}})
    }

    pub(super) fn page_key(&self, page: &Value) -> Result<Option<SyncKey>, NotionError> {
        let prop = page_property(page, &self.properties.sync_key, "rich_text")?;
        let key = text(prop)?;
        if key.is_empty() {
            Ok(None)
        } else {
            key.parse()
                .map(Some)
                .map_err(|_| NotionError::UnsupportedProperty)
        }
    }

    /// Return a projection and whether Relation must be retrieved separately.
    pub(super) fn assignment(&self, page: &Value) -> Result<(NotionAssignment, bool), NotionError> {
        let page_id = checked_page_id(page, &self.assignments_id, &self.assignments_database)?;
        let sync_key = self.page_key(page)?;
        let mut row = NotionAssignment {
            page_id,
            sync_key,
            source: None,
            title: String::new(),
            deadline: None,
            course_page_ids: Vec::new(),
        };
        if row.sync_key.is_none() {
            return Ok((row, false));
        }
        row.title = text(page_property(page, &self.properties.title, "title")?)?;
        row.deadline = read_deadline(page_property(page, &self.properties.deadline, "date")?)?;
        let source = page_property(page, &self.properties.source, "select")?;
        if !source.is_null() {
            row.source = Some(match source["id"].as_str() {
                Some(id) if id == self.moodle_option => UniversitySource::Moodle,
                Some(id) if id == self.webclass_option => UniversitySource::WebClass,
                _ => return Err(NotionError::UnsupportedProperty),
            });
        }
        let (links, has_more) = page_relations(page, &self.properties.course)?;
        row.course_page_ids = links;
        Ok((row, has_more))
    }

    pub(super) fn manual_title_deadline(
        &self,
        page: &Value,
    ) -> Result<(String, Option<Deadline>), NotionError> {
        Ok((
            text(page_property(page, &self.properties.title, "title")?)?,
            read_deadline(page_property(page, &self.properties.deadline, "date")?)?,
        ))
    }

    pub(super) fn read_manual_comparison(
        &self,
        page: &Value,
        row: &mut NotionAssignment,
    ) -> Result<(), NotionError> {
        (row.title, row.deadline) = self.manual_title_deadline(page)?;
        // Missing or partial relations cannot rule out a possible duplicate.
        if let Ok((courses, false)) = page_relations(page, &self.properties.course) {
            row.course_page_ids = courses;
        }
        Ok(())
    }

    pub(super) fn title_content<'a>(&self, page: &'a Value) -> Result<&'a Value, NotionError> {
        page_property(page, &self.properties.title, "title")
    }

    pub(super) fn prepare(
        &self,
        entry: &PlannedAssignment,
    ) -> Result<Option<PreparedWrite>, NotionError> {
        let mut props = Map::new();
        let ids = &self.properties;
        let request = match &entry.action {
            PlanAction::Create {
                assignment,
                display_title,
                course_page_id,
            } => {
                props.insert(ids.title.clone(), title_value(display_title)?);
                props.insert(ids.source.clone(), self.source_value(assignment.source));
                props.insert(
                    ids.sync_key.clone(),
                    json!({"rich_text":[text_item(entry.key.as_str())]}),
                );
                if let Some(deadline) = assignment.deadline.as_ref() {
                    props.insert(ids.deadline.clone(), date_value(Some(deadline)));
                }
                if let Some(id) = course_page_id {
                    props.insert(ids.course.clone(), relation_value(id)?);
                }
                PreparedWrite {
                    target_id: self.assignments_id.clone(),
                    key: entry.key.clone(),
                    method: Method::POST,
                    path: "/v1/pages".into(),
                    body: json!({
                        "parent":{"type":"data_source_id", "data_source_id":self.assignments_id},
                        "properties":props,
                    }),
                }
            }
            PlanAction::Update { page_id, patch } => {
                if let Some(title) = &patch.title {
                    props.insert(ids.title.clone(), title_value(title)?);
                }
                if let Some(source) = patch.source {
                    props.insert(ids.source.clone(), self.source_value(source));
                }
                if let Some(ref deadline) = patch.deadline {
                    props.insert(ids.deadline.clone(), date_value(deadline.as_ref()));
                }
                if let Some(id) = &patch.course_page_id {
                    props.insert(ids.course.clone(), relation_value(id)?);
                }
                let id = uuid(page_id).ok_or(NotionError::InvalidWrite)?;
                PreparedWrite {
                    target_id: self.assignments_id.clone(),
                    key: entry.key.clone(),
                    method: Method::PATCH,
                    path: format!("/v1/pages/{id}"),
                    body: json!({"properties":props}),
                }
            }
            _ => return Ok(None),
        };
        Ok(Some(request))
    }
}

pub(super) fn checked_page_id(
    page: &Value,
    source: &str,
    database: &str,
) -> Result<String, NotionError> {
    if page["object"] != "page" {
        return Err(NotionError::InvalidResponse);
    }
    let id = object_id(page, "id").ok_or(NotionError::InvalidResponse)?;
    let parent = &page["parent"];
    if page["in_trash"].as_bool() != Some(false)
        || ["is_archived", "archived"].iter().any(|flag| {
            page.get(flag)
                .is_some_and(|value| value.as_bool() != Some(false))
        })
        || parent["type"] != "data_source_id"
        || object_id(parent, "data_source_id").as_deref() != Some(source)
        || object_id(parent, "database_id").as_deref() != Some(database)
    {
        return Err(NotionError::IncompleteSnapshot);
    }
    Ok(id)
}

pub(super) fn page_property<'a>(
    page: &'a Value,
    id: &str,
    kind: &str,
) -> Result<&'a Value, NotionError> {
    let properties = page["properties"]
        .as_object()
        .ok_or(NotionError::InvalidResponse)?;
    property(properties, id, kind)
        .and_then(|prop| prop.get(kind))
        .ok_or(NotionError::InvalidResponse)
}

pub(super) fn text(value: &Value) -> Result<String, NotionError> {
    let mut result = String::new();
    for part in value.as_array().ok_or(NotionError::InvalidResponse)? {
        if part["type"] != "text" {
            return Err(NotionError::UnsupportedProperty);
        }
        result.push_str(
            part["text"]["content"]
                .as_str()
                .ok_or(NotionError::InvalidResponse)?,
        );
    }
    Ok(result)
}

fn read_deadline(value: &Value) -> Result<Option<Deadline>, NotionError> {
    if value.is_null() {
        return Ok(None);
    }
    if !value["end"].is_null() || !value["time_zone"].is_null() {
        return Err(NotionError::UnsupportedProperty);
    }
    let start = value["start"]
        .as_str()
        .ok_or(NotionError::InvalidResponse)?;
    if let Ok(date) = NaiveDate::parse_from_str(start, "%Y-%m-%d") {
        return Ok(Some(Deadline::DateOnly(date)));
    }
    let instant =
        DateTime::parse_from_rfc3339(start).map_err(|_| NotionError::UnsupportedProperty)?;
    Ok(Some(Deadline::DateTime(instant.to_utc())))
}

/// The Notion Date property may round a time down to minute precision.
pub(super) fn notion_deadline_matches(
    stored: &Option<Deadline>,
    source: &Option<Deadline>,
) -> bool {
    stored == source
        || matches!((stored, source), (Some(Deadline::DateTime(stored)), Some(Deadline::DateTime(source)))
            if stored.timestamp_subsec_nanos() == 0
                && stored.timestamp() == source.timestamp().div_euclid(60) * 60)
}

pub(super) fn relation_id(value: &Value) -> Result<String, NotionError> {
    object_id(value, "id").ok_or(NotionError::InvalidResponse)
}

pub(super) fn text_item(text: &str) -> Value {
    json!({"type":"text", "text":{"content":text}})
}

pub(super) fn title_value(title: &str) -> Result<Value, NotionError> {
    if !valid_title(title) {
        return Err(NotionError::InvalidWrite);
    }
    Ok(json!({"title":[text_item(title)]}))
}

pub(super) fn valid_title(title: &str) -> bool {
    // Conservative size unit for supplementary Unicode characters.
    !title.trim().is_empty() && title.encode_utf16().count() <= 2_000
}

fn date_value(deadline: Option<&Deadline>) -> Value {
    let date = deadline.map(|deadline| match deadline {
        Deadline::DateOnly(date) => json!({"start":date.format("%Y-%m-%d").to_string()}),
        Deadline::DateTime(instant) => {
            json!({"start":instant.to_rfc3339_opts(SecondsFormat::AutoSi, true)})
        }
    });
    json!({"date":date})
}

pub(super) fn relation_value(id: &str) -> Result<Value, NotionError> {
    let id = uuid(id).ok_or(NotionError::InvalidWrite)?;
    Ok(json!({"relation":[{"id":id}]}))
}

/// A request preview with no authorization data or send method.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedWrite {
    pub(super) target_id: String,
    key: SyncKey,
    method: Method,
    path: String,
    body: Value,
}

impl PreparedWrite {
    pub(super) fn create_page(target_id: &str, key: SyncKey, properties: Value) -> Self {
        Self {
            target_id: target_id.to_owned(),
            key,
            method: Method::POST,
            path: "/v1/pages".into(),
            body: json!({"parent":{"type":"data_source_id","data_source_id":target_id},"properties":properties}),
        }
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(super) fn verification_patch(key: SyncKey, page_id: &str, properties: Value) -> Self {
        Self {
            target_id: super::trial::MOODLE_VERIFICATION_ASSIGNMENTS.to_owned(),
            key,
            method: Method::PATCH,
            path: format!("/v1/pages/{page_id}"),
            body: json!({"properties": properties}),
        }
    }

    pub fn key(&self) -> &SyncKey {
        &self.key
    }
    pub fn method(&self) -> &Method {
        &self.method
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn body(&self) -> &Value {
        &self.body
    }
}
