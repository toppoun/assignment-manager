//! Offline WebClass JSON parsing and assignment candidate mapping.

use std::error::Error;
use std::fmt;

use chrono::{FixedOffset, NaiveDateTime, TimeZone, Utc};
use serde::Deserialize;

use crate::domain::{Deadline, SubmissionTarget};
use crate::import::{AssignmentCandidate, ExternalRecordKey, OriginKind};

/// WebClass course data. These external fields are not application course IDs or schedule entries.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WebClassCourse {
    pub group_id: String,
    pub group_name: String,
    pub year: i32,
    pub semester: String,
    pub day_of_week: String,
    pub class_order: String,
    /// Retained as the API's PHP-serialized string; no schedule inference is made here.
    pub children: String,
    /// The fixtures contain both null and an empty string.
    pub group_name_en: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WebClassScore {
    pub max_score: Option<i64>,
    pub answer_datetime: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WebClassContent {
    pub contents_id: String,
    pub contents_kind: String,
    pub contents_name: String,
    pub group_id: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub link_to: Option<String>,
    #[serde(default)]
    pub scores: Vec<WebClassScore>,
    pub hidden_content: Option<bool>,
}

/// Unknown fields are ignored, while malformed JSON and missing required fields fail parsing.
pub fn parse_courses(json: &str) -> Result<Vec<WebClassCourse>, serde_json::Error> {
    serde_json::from_str(json)
}

/// Unknown contents kinds are retained so classification can make an explicit decision.
pub fn parse_contents(json: &str) -> Result<Vec<WebClassContent>, serde_json::Error> {
    serde_json::from_str(json)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebClassMapError {
    InvalidEndDate { contents_id: String, value: String },
}

impl fmt::Display for WebClassMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndDate { contents_id, value } => {
                write!(
                    f,
                    "WebClass content {contents_id}: invalid end_date: {value}"
                )
            }
        }
    }
}

impl Error for WebClassMapError {}

fn deadline(content: &WebClassContent) -> Result<Option<Deadline>, WebClassMapError> {
    let Some(value) = content
        .end_date
        .as_deref()
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let parsed = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S").map_err(|_| {
        WebClassMapError::InvalidEndDate {
            contents_id: content.contents_id.clone(),
            value: value.to_owned(),
        }
    })?;
    let jst = FixedOffset::east_opt(9 * 60 * 60).expect("+09:00 is a valid fixed offset");
    let datetime = jst
        .from_local_datetime(&parsed)
        .single()
        .expect("fixed offset is unambiguous");
    Ok(Some(Deadline::DateTime(datetime.with_timezone(&Utc))))
}

/// Only `Question` is confirmed assignment-like by the supplied fixtures.
pub fn map_content(
    content: &WebClassContent,
) -> Result<Option<AssignmentCandidate>, WebClassMapError> {
    // Validate a non-empty end_date even when the kind is unsupported or hidden.
    let deadline = deadline(content)?;
    // Conservatively require an explicit `hidden_content = false` for visibility.
    // The supplied fixtures do not establish server behavior for true or absent values.
    if content.contents_kind != "Question" || content.hidden_content != Some(false) {
        return Ok(None);
    }

    Ok(Some(AssignmentCandidate {
        external_key: ExternalRecordKey {
            origin: OriginKind::WebClass,
            scope: content.group_id.clone(),
            id: content.contents_id.clone(),
        },
        title: content.contents_name.clone(),
        deadline,
        submission: SubmissionTarget::WebClass,
        submission_url: None,
    }))
}

pub fn map_contents(
    contents: &[WebClassContent],
) -> Result<Vec<AssignmentCandidate>, WebClassMapError> {
    contents
        .iter()
        .map(map_content)
        .filter_map(|result| result.transpose())
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::*;

    const COURSES_BASIC: &str = include_str!("../fixtures/webclass/courses_basic.json");
    const COURSES_EDGE: &str = include_str!("../fixtures/webclass/courses_edge_cases.json");
    const CONTENTS_BASIC: &str = include_str!("../fixtures/webclass/contents_basic.json");
    const CONTENTS_STATES: &str = include_str!("../fixtures/webclass/contents_states.json");
    const CONTENTS_EMPTY: &str = include_str!("../fixtures/webclass/contents_empty.json");

    fn utc(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn courses_basic_keep_fixture_types_and_values() {
        let courses = parse_courses(COURSES_BASIC).unwrap();
        assert_eq!(courses.len(), 3);
        assert_eq!(courses[0].group_id, "fixture-course-001");
        assert_eq!(courses[0].group_name, "線形代数");
        assert_eq!(courses[0].year, 2026);
        assert_eq!(courses[0].semester, "2");
        assert_eq!(courses[0].day_of_week, "1");
        assert_eq!(courses[0].class_order, "4");
        assert_eq!(courses[1].day_of_week, "2");
        assert_eq!(courses[2].class_order, "2");
    }

    #[test]
    fn courses_edge_cases_preserve_zero_children_null_and_empty_string() {
        let courses = parse_courses(COURSES_EDGE).unwrap();
        assert_eq!(courses.len(), 3);
        assert_eq!(courses[0].day_of_week, "0");
        assert_eq!(courses[0].class_order, "0");
        assert_eq!(courses[0].children, "");
        assert_eq!(courses[0].group_name_en, None);
        assert_eq!(
            courses[1].children,
            "a:1:{i:0;a:2:{s:11:\"day_of_week\";i:1;s:11:\"class_order\";i:2;}}"
        );
        assert_eq!(courses[2].group_name_en.as_deref(), Some(""));
        // No Course or ScheduleEntry conversion is performed from the zero values.
    }

    #[test]
    fn contents_basic_map_question_and_jst_deadline() {
        let contents = parse_contents(CONTENTS_BASIC).unwrap();
        assert_eq!(contents.len(), 3);
        let content = &contents[0];
        assert_eq!(content.contents_kind, "Question");
        assert_eq!(content.contents_name, "演習課題A");
        assert_eq!(content.group_id, "fixture-course-002");
        assert_eq!(content.contents_id, "fixture-content-001");
        assert_eq!(content.start_date.as_deref(), Some("2026-09-01 09:00:00"));
        assert_eq!(content.end_date.as_deref(), Some("2026-10-31 23:59:59"));
        assert_eq!(content.link_to, None);
        assert_eq!(content.hidden_content, Some(false));

        let candidates = map_contents(&contents).unwrap();
        assert_eq!(candidates.len(), 3);
        let candidate = &candidates[0];
        assert_eq!(candidate.title, "演習課題A");
        assert_eq!(candidate.external_key.origin, OriginKind::WebClass);
        assert_eq!(candidate.external_key.scope, content.group_id);
        assert_eq!(candidate.external_key.id, content.contents_id);
        assert_eq!(
            candidate.deadline,
            Some(Deadline::DateTime(utc("2026-10-31T14:59:59Z")))
        );
        assert_eq!(candidate.submission, SubmissionTarget::WebClass);
        assert_eq!(candidate.submission_url, None);
    }

    #[test]
    fn contents_states_do_not_change_candidate_mapping() {
        let contents = parse_contents(CONTENTS_STATES).unwrap();
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0].scores[0].answer_datetime, None);
        assert_eq!(contents[0].scores[0].max_score, None);
        assert_eq!(
            contents[1].scores[0].answer_datetime.as_deref(),
            Some("2026-09-20 10:20:30")
        );
        assert_eq!(contents[1].scores[0].max_score, Some(3));
        assert_eq!(
            contents[2].scores[0].answer_datetime.as_deref(),
            Some("2026-09-21 11:22:33")
        );
        assert_eq!(contents[2].scores[0].max_score, Some(0));

        let candidates = map_contents(&contents).unwrap();
        assert_eq!(candidates.len(), 3);
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.deadline == candidates[0].deadline)
        );
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.submission == SubmissionTarget::WebClass)
        );
        let mut changed_score = contents[0].clone();
        changed_score.scores = contents[2].scores.clone();
        assert_eq!(map_content(&contents[0]), map_content(&changed_score));
        // AssignmentCandidate has no local CompletionState field to infer from scores.
    }

    #[test]
    fn empty_fixture_has_no_contents_or_candidates() {
        let contents = parse_contents(CONTENTS_EMPTY).unwrap();
        assert!(contents.is_empty());
        assert!(map_contents(&contents).unwrap().is_empty());
    }

    #[test]
    fn synthetic_unknown_kind_and_unknown_fields_are_accepted_but_not_mapped() {
        let json = r#"[{"contents_id":"other-1","contents_kind":"Other","contents_name":"通知","group_id":"group-1","end_date":"2026-10-06 09:00:59","scores":[],"hidden_content":false,"extra":{"anything":true}}]"#;
        let contents = parse_contents(json).unwrap();
        assert_eq!(contents[0].contents_kind, "Other");
        assert!(map_contents(&contents).unwrap().is_empty());
    }

    #[test]
    fn malformed_json_and_missing_identity_fields_fail() {
        assert!(parse_courses("[{ ").is_err());
        assert!(parse_contents("[{ ").is_err());
        assert!(parse_courses(r#"[{"group_name":"講義","year":2026,"semester":"2","day_of_week":"1","class_order":"1","children":""}]"#).is_err());
        assert!(
            parse_contents(
                r#"[{"contents_kind":"Question","contents_name":"課題","group_id":"group-1"}]"#
            )
            .is_err()
        );
        assert!(
            parse_contents(
                r#"[{"contents_id":"c-1","contents_kind":"Question","contents_name":"課題"}]"#
            )
            .is_err()
        );
    }

    #[test]
    fn malformed_nonempty_end_date_fails_mapping() {
        let json = r#"[{"contents_id":"c-1","contents_kind":"Question","contents_name":"課題","group_id":"group-1","end_date":"2026-13-06 09:00:59"}]"#;
        let contents = parse_contents(json).unwrap();
        assert_eq!(
            map_contents(&contents),
            Err(WebClassMapError::InvalidEndDate {
                contents_id: "c-1".into(),
                value: "2026-13-06 09:00:59".into(),
            })
        );
    }

    #[test]
    fn synthetic_empty_end_date_keeps_no_deadline() {
        let json = r#"[{"contents_id":"c-1","contents_kind":"Question","contents_name":"課題","group_id":"group-1","end_date":"","hidden_content":false}]"#;
        let contents = parse_contents(json).unwrap();
        assert_eq!(contents[0].end_date.as_deref(), Some(""));
        assert_eq!(map_contents(&contents).unwrap()[0].deadline, None);
    }

    #[test]
    fn synthetic_hidden_content_is_conservatively_excluded() {
        let json = r#"[{"contents_id":"c-1","contents_kind":"Question","contents_name":"課題","group_id":"group-1","hidden_content":true},{"contents_id":"c-2","contents_kind":"Question","contents_name":"課題","group_id":"group-1"}]"#;
        let contents = parse_contents(json).unwrap();
        assert_eq!(contents[0].hidden_content, Some(true));
        assert_eq!(contents[1].hidden_content, None);
        assert!(map_contents(&contents).unwrap().is_empty());
    }

    #[test]
    fn synthetic_jst_morning_maps_to_utc_midnight() {
        let json = r#"[{"contents_id":"c-1","contents_kind":"Question","contents_name":"課題","group_id":"group-1","end_date":"2026-10-06 09:00:59","hidden_content":false}]"#;
        let contents = parse_contents(json).unwrap();
        assert_eq!(
            map_contents(&contents).unwrap()[0].deadline,
            Some(Deadline::DateTime(utc("2026-10-06T00:00:59Z")))
        );
    }
}
