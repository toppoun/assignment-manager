use std::collections::HashSet;

use chrono::{DateTime, NaiveDate, SecondsFormat};
use reqwest::Method;
use serde_json::{Map, Value, json};

use crate::domain::Deadline;

use super::{
    NotionAssignment, NotionError, PlanAction, PlannedAssignment, SyncKey, UniversitySource,
};

#[derive(Clone)]
/// Property IDs returned by the API for the preconfigured assignment schema.
/// Renaming a column does not change these IDs.
pub struct NotionPropertyIds {
    /// Notion title (the built-in property ID is always `title`).
    pub title: String,
    /// Date, accepting a single calendar date or an RFC 3339 instant.
    pub deadline: String,
    /// Configure a source-owned Relation column.
    pub course: String,
    /// URL column verified during schema validation and omitted from writes.
    pub submission_url: String,
    /// Rich text carrying the canonical versioned key, empty for manual tasks.
    pub sync_key: String,
    /// Select with existing Moodle and WebClass options, written by option ID.
    pub source: String,
}

pub struct NotionTarget {
    pub assignments_data_source_id: String,
    pub courses_data_source_id: String,
    pub properties: NotionPropertyIds,
}

pub(super) struct Schema {
    pub assignments_id: String,
    pub assignments_database: String,
    pub courses_id: String,
    pub courses_database: String,
    pub properties: NotionPropertyIds,
    moodle_option: String,
    webclass_option: String,
}

pub(super) fn uuid(value: &str) -> Option<String> {
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

fn property_id(id: &str) -> bool {
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
    fn fields(&self) -> [(&str, &str); 6] {
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
    pub(super) fn ids(&self) -> Result<(String, String), NotionError> {
        let mut seen = HashSet::new();
        if self.properties.title != "title"
            || self
                .properties
                .fields()
                .into_iter()
                .any(|(id, _)| !property_id(id) || !seen.insert(id))
        {
            return Err(NotionError::Configuration);
        }
        let assignments =
            uuid(&self.assignments_data_source_id).ok_or(NotionError::Configuration)?;
        let courses = uuid(&self.courses_data_source_id).ok_or(NotionError::Configuration)?;
        if assignments == courses {
            return Err(NotionError::Configuration);
        }
        Ok((assignments, courses))
    }
}

fn object_id(value: &Value, field: &str) -> Option<String> {
    uuid(value[field].as_str()?)
}

fn data_source_parent(data: &Value, id: &str) -> Result<String, NotionError> {
    if data["object"] != "data_source"
        || object_id(data, "id").as_deref() != Some(id)
        || data["in_trash"].as_bool() != Some(false)
        || data["parent"]["type"] != "database_id"
    {
        return Err(NotionError::SchemaMismatch);
    }
    object_id(&data["parent"], "database_id").ok_or(NotionError::SchemaMismatch)
}

fn property<'a>(properties: &'a Map<String, Value>, id: &str, kind: &str) -> Option<&'a Value> {
    let mut matches = properties
        .values()
        .filter(|prop| prop["id"].as_str() == Some(id));
    let prop = matches.next()?;
    (matches.next().is_none() && prop["type"].as_str() == Some(kind)).then_some(prop)
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
        let properties = data["properties"]
            .as_object()
            .ok_or(NotionError::SchemaMismatch)?;
        for (id, kind) in target.properties.fields() {
            let prop = property(properties, id, kind).ok_or(NotionError::SchemaMismatch)?;
            if !prop[kind].is_object() {
                return Err(NotionError::SchemaMismatch);
            }
        }
        let relation = &property(properties, &target.properties.course, "relation")
            .ok_or(NotionError::SchemaMismatch)?["relation"];
        if object_id(relation, "data_source_id").as_deref() != Some(&courses_id)
            || object_id(relation, "database_id").as_deref() != Some(&courses_database)
            || !matches!(
                relation["type"].as_str(),
                Some("single_property" | "dual_property")
            )
        {
            return Err(NotionError::SchemaMismatch);
        }
        let select = &property(properties, &target.properties.source, "select")
            .ok_or(NotionError::SchemaMismatch)?["select"];
        let options = select["options"]
            .as_array()
            .ok_or(NotionError::SchemaMismatch)?;
        let mut option_ids = HashSet::new();
        for option in options {
            let id = option["id"]
                .as_str()
                .filter(|id| !id.trim().is_empty())
                .ok_or(NotionError::SchemaMismatch)?;
            if !option_ids.insert(id) {
                return Err(NotionError::SchemaMismatch);
            }
        }
        let option_id = |name| {
            let mut named = options
                .iter()
                .filter(|option| option["name"].as_str() == Some(name));
            let first = named.next()?;
            named
                .next()
                .is_none()
                .then(|| first["id"].as_str().map(str::to_owned))
                .flatten()
        };
        let moodle_option = option_id("Moodle").ok_or(NotionError::SchemaMismatch)?;
        let webclass_option = option_id("WebClass").ok_or(NotionError::SchemaMismatch)?;
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
        let p = property(
            page["properties"]
                .as_object()
                .ok_or(NotionError::InvalidResponse)?,
            &self.properties.course,
            "relation",
        )
        .ok_or(NotionError::InvalidResponse)?;
        let relation = p["relation"]
            .as_array()
            .ok_or(NotionError::InvalidResponse)?;
        let has_more = p["has_more"]
            .as_bool()
            .ok_or(NotionError::InvalidResponse)?;
        if !has_more {
            row.course_page_ids = relation.iter().map(relation_id).collect::<Result<_, _>>()?;
        }
        Ok((row, has_more))
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
                course_page_id,
            } => {
                props.insert(ids.title.clone(), title_value(&assignment.title)?);
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
        || parent["type"] != "data_source_id"
        || object_id(parent, "data_source_id").as_deref() != Some(source)
        || object_id(parent, "database_id").as_deref() != Some(database)
    {
        return Err(NotionError::IncompleteSnapshot);
    }
    Ok(id)
}

fn page_property<'a>(page: &'a Value, id: &str, kind: &str) -> Result<&'a Value, NotionError> {
    let properties = page["properties"]
        .as_object()
        .ok_or(NotionError::InvalidResponse)?;
    property(properties, id, kind)
        .and_then(|prop| prop.get(kind))
        .ok_or(NotionError::InvalidResponse)
}

fn text(value: &Value) -> Result<String, NotionError> {
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

pub(super) fn relation_id(value: &Value) -> Result<String, NotionError> {
    object_id(value, "id").ok_or(NotionError::InvalidResponse)
}

fn text_item(text: &str) -> Value {
    json!({"type":"text", "text":{"content":text}})
}

fn title_value(title: &str) -> Result<Value, NotionError> {
    // Conservative size unit for supplementary Unicode characters.
    if title.trim().is_empty() || title.encode_utf16().count() > 2_000 {
        return Err(NotionError::InvalidWrite);
    }
    Ok(json!({"title":[text_item(title)]}))
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

fn relation_value(id: &str) -> Result<Value, NotionError> {
    let id = uuid(id).ok_or(NotionError::InvalidWrite)?;
    Ok(json!({"relation":[{"id":id}]}))
}

/// A request preview with no authorization data or send method.
pub struct PreparedWrite {
    key: SyncKey,
    method: Method,
    path: String,
    body: Value,
}

impl PreparedWrite {
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
