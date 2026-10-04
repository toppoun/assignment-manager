//! Offline WebClass JSON parsing and assignment candidate mapping.

use std::error::Error;
use std::fmt;

use chrono::{FixedOffset, NaiveDateTime, TimeZone, Utc};
use serde::Deserialize;

use crate::domain::{Deadline, SubmissionTarget};
use crate::import::{
    AssignmentCandidate, AssignmentCandidateBatch, ExternalRecordKey, ImportValidationError,
    OriginKind,
};

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
    InvalidEndDate {
        contents_id: String,
        value: String,
    },
    /// Import validation carries a fixed classification only, with no source data.
    Validation(ImportValidationError),
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
            Self::Validation(error) => error.fmt(f),
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
///
/// `group_id` / `contents_id` remain opaque external strings, not internal IDs.
/// `link_to` has not been established as an actual submission page, so the URL is
/// left unknown. Scores and answer timestamps do not determine local completion.
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

/// Map assignment-like records, not a complete set of source identities.
///
/// Hidden or unsupported records can exist in `contents` without becoming
/// candidates. Candidate absence must never be used to infer source disappearance.
/// Use [`map_contents_for_group`] at the fetched-group import boundary.
pub fn map_contents(
    contents: &[WebClassContent],
) -> Result<Vec<AssignmentCandidate>, WebClassMapError> {
    contents
        .iter()
        .map(map_content)
        .filter_map(|result| result.transpose())
        .collect()
}

/// Bind parsed contents to the exact group supplied to `fetch_contents_json`.
///
/// Check every source record before mapping, including hidden / unsupported
/// records. Any mismatch rejects the entire batch, with no partial candidates.
/// The comparison is exact; group IDs are never trimmed or otherwise normalized.
/// Mapping then passes through the source-independent identity / duplicate
/// validation boundary. HTTP and raw JSON parsing keep their existing roles.
/// The returned batch is still not a source-presence snapshot.
///
/// ```
/// use assignment_manager::webclass::{map_contents_for_group, parse_contents};
///
/// // After fetching raw JSON using this same requested group:
/// let requested_group = "group-1";
/// let raw = r#"[{"contents_id":"content-1","contents_kind":"Question",
///     "contents_name":"Report","group_id":"group-1","hidden_content":false}]"#;
/// let contents = parse_contents(raw)?;
/// let batch = map_contents_for_group(&contents, requested_group)?;
/// assert_eq!(batch.as_slice().len(), 1);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn map_contents_for_group(
    contents: &[WebClassContent],
    requested_group_id: &str,
) -> Result<AssignmentCandidateBatch, WebClassMapError> {
    if contents
        .iter()
        .any(|content| content.group_id != requested_group_id)
    {
        return Err(WebClassMapError::Validation(
            ImportValidationError::WebClassGroupMismatch,
        ));
    }
    AssignmentCandidateBatch::try_new(map_contents(contents)?).map_err(WebClassMapError::Validation)
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

    #[test]
    fn requested_group_matches_every_record_and_preserves_mapping() {
        let contents = parse_contents(CONTENTS_BASIC).unwrap();
        let batch = map_contents_for_group(&contents, "fixture-course-002").unwrap();
        assert_eq!(batch.as_slice(), map_contents(&contents).unwrap());
    }

    #[test]
    fn one_group_mismatch_rejects_the_whole_batch_without_partial_candidates() {
        let mut contents = parse_contents(CONTENTS_BASIC).unwrap();
        contents[2].group_id = "other-group".into();
        for records in [
            contents.clone(),
            contents.into_iter().rev().collect::<Vec<_>>(),
        ] {
            // Valid records alone would produce candidates, but none escape on error.
            assert!(!map_contents(&records[..2]).unwrap().is_empty());
            assert_eq!(
                map_contents_for_group(&records, "fixture-course-002"),
                Err(WebClassMapError::Validation(
                    ImportValidationError::WebClassGroupMismatch
                ))
            );
        }
    }

    #[test]
    fn group_validation_includes_records_that_mapping_would_filter_out() {
        let original = parse_contents(CONTENTS_BASIC).unwrap();
        for (kind, hidden) in [
            ("Question", Some(true)),
            ("Question", None),
            ("Other", Some(false)),
        ] {
            let mut contents = original.clone();
            contents[2].contents_kind = kind.into();
            contents[2].hidden_content = hidden;
            assert!(map_content(&contents[2]).unwrap().is_none());
            contents[2].group_id = "other-group".into();
            assert_eq!(
                map_contents_for_group(&contents, "fixture-course-002"),
                Err(WebClassMapError::Validation(
                    ImportValidationError::WebClassGroupMismatch
                ))
            );
        }
    }

    #[test]
    fn requested_group_comparison_and_stored_identity_remain_opaque() {
        let mut contents = parse_contents(CONTENTS_BASIC).unwrap();
        for content in &mut contents {
            content.group_id = " Group-001 ".into();
        }
        let batch = map_contents_for_group(&contents, " Group-001 ").unwrap();
        assert_eq!(batch.as_slice()[0].external_key.scope, " Group-001 ");
        for different in ["Group-001", " group-001 "] {
            assert_eq!(
                map_contents_for_group(&contents, different),
                Err(WebClassMapError::Validation(
                    ImportValidationError::WebClassGroupMismatch
                ))
            );
        }
    }

    #[test]
    fn group_import_rejects_duplicate_and_blank_candidate_identities() {
        let original = parse_contents(CONTENTS_BASIC).unwrap();
        let mut contents = original.clone();
        contents[2].contents_id = contents[0].contents_id.clone();
        assert_eq!(
            map_contents_for_group(&contents, "fixture-course-002"),
            Err(WebClassMapError::Validation(
                ImportValidationError::DuplicateExternalIdentity
            ))
        );

        contents = original.clone();
        contents[2].contents_id = "\t".into();
        assert_eq!(
            map_contents_for_group(&contents, "fixture-course-002"),
            Err(WebClassMapError::Validation(
                ImportValidationError::EmptyExternalId
            ))
        );

        contents = original;
        for content in &mut contents {
            content.group_id = " \n".into();
        }
        assert_eq!(
            map_contents_for_group(&contents, " \n"),
            Err(WebClassMapError::Validation(
                ImportValidationError::EmptyExternalScope
            ))
        );
    }

    #[test]
    fn group_validation_errors_do_not_retain_or_render_source_data() {
        let mut contents = parse_contents(CONTENTS_BASIC).unwrap();
        contents[2].group_id = "synthetic-private-group".into();
        contents[2].contents_id = "synthetic-private-id".into();
        contents[2].link_to = Some("https://example.invalid/?token=synthetic-private-token".into());
        // Mismatch is checked before even an unrelated malformed date is mapped.
        contents[2].end_date = Some("synthetic-private-response".into());
        let error = map_contents_for_group(&contents, "synthetic-requested-group").unwrap_err();
        assert_eq!(
            error,
            WebClassMapError::Validation(ImportValidationError::WebClassGroupMismatch)
        );
        assert_eq!(format!("{error:?}"), "Validation(WebClassGroupMismatch)");
        assert_eq!(
            error.to_string(),
            "WebClass response group does not match requested group"
        );
        assert!(error.source().is_none());
    }

    #[test]
    fn filtered_contents_remain_in_source_model_but_not_in_candidate_batch() {
        let mut contents = parse_contents(CONTENTS_BASIC).unwrap();
        let mut hidden = contents[0].clone();
        hidden.contents_id = "hidden-content".into();
        hidden.hidden_content = Some(true);
        let mut unsupported = contents[0].clone();
        unsupported.contents_id = "unsupported-content".into();
        unsupported.contents_kind = "Other".into();
        contents.extend([hidden, unsupported]);

        let batch = map_contents_for_group(&contents, "fixture-course-002").unwrap();
        assert_eq!(contents.len(), 5);
        assert_eq!(batch.as_slice().len(), 3);
        for observed in &contents[3..] {
            assert!(
                batch
                    .as_slice()
                    .iter()
                    .all(|candidate| candidate.external_key.id != observed.contents_id)
            );
        }
        // These identities are still observed in the source; absence cannot mean deletion.
    }

    #[test]
    fn answered_and_scored_contents_do_not_complete_or_reopen_local_assignments() {
        use crate::domain::{AssignmentId, CompletionState};

        let contents = parse_contents(CONTENTS_STATES).unwrap();
        let batch = map_contents_for_group(&contents, "fixture-course-002").unwrap();
        for candidate in batch.as_slice() {
            let mut assignment = candidate
                .clone()
                .into_assignment(AssignmentId::new("application-id"), None);
            assert_eq!(assignment.completion, CompletionState::Incomplete);
            candidate.apply_source_update(&mut assignment, None);
            assert_eq!(assignment.completion, CompletionState::Incomplete);
            assignment.completion = CompletionState::Completed;
            candidate.apply_source_update(&mut assignment, None);
            assert_eq!(assignment.completion, CompletionState::Completed);
        }
    }
}
