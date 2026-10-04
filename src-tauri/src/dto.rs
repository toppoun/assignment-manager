use assignment_manager::{
    domain::{CompletionState, Deadline, SubmissionTarget},
    import::OriginKind,
    query::{AssignmentListItem, CourseListInfo},
};
use chrono::SecondsFormat;
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignmentListDto {
    pub items: Vec<AssignmentDto>,
    /// All courses referenced by assignments, in core Course sort order.
    pub courses: Vec<CourseDto>,
    pub total_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeadlineStatusDto {
    Overdue,
    Today,
    Upcoming,
    None,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignmentDto {
    pub id: String,
    pub title: String,
    pub deadline: DeadlineDto,
    pub completion: CompletionDto,
    pub submission: SubmissionDto,
    // The initial UI needs presence only; no submission/token URL crosses IPC.
    pub has_submission_url: bool,
    pub course: Option<CourseDto>,
    pub status: DeadlineStatusDto,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum DeadlineDto {
    DateOnly(String),
    DateTime(String),
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionDto {
    Incomplete,
    Completed,
}

impl From<CompletionState> for CompletionDto {
    fn from(state: CompletionState) -> Self {
        match state {
            CompletionState::Incomplete => Self::Incomplete,
            CompletionState::Completed => Self::Completed,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionDto {
    Unknown,
    None,
    Moodle,
    WebClass,
    External,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CourseDto {
    pub id: String,
    pub display_label: String,
    pub origin: CourseOriginDto,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CourseOriginDto {
    Moodle,
    WebClass,
    Manual,
    Unbound,
}

impl From<CourseListInfo> for CourseDto {
    fn from(course: CourseListInfo) -> Self {
        Self {
            id: course.id.as_str().to_owned(),
            display_label: course.name,
            origin: match course.origin {
                Some(OriginKind::Moodle) => CourseOriginDto::Moodle,
                Some(OriginKind::WebClass) => CourseOriginDto::WebClass,
                Some(OriginKind::Manual) => CourseOriginDto::Manual,
                None => CourseOriginDto::Unbound,
            },
        }
    }
}

impl From<AssignmentListItem> for AssignmentDto {
    fn from(item: AssignmentListItem) -> Self {
        Self {
            id: item.id.as_str().to_owned(),
            title: item.title,
            deadline: match item.deadline {
                Some(Deadline::DateOnly(date)) => DeadlineDto::DateOnly(date.to_string()),
                Some(Deadline::DateTime(instant)) => {
                    DeadlineDto::DateTime(instant.to_rfc3339_opts(SecondsFormat::AutoSi, true))
                }
                None => DeadlineDto::None,
            },
            completion: item.completion.into(),
            submission: match item.submission {
                SubmissionTarget::Unknown => SubmissionDto::Unknown,
                SubmissionTarget::None => SubmissionDto::None,
                SubmissionTarget::Moodle => SubmissionDto::Moodle,
                SubmissionTarget::WebClass => SubmissionDto::WebClass,
                SubmissionTarget::External => SubmissionDto::External,
            },
            has_submission_url: item.submission_url.is_some(),
            course: item.course.map(Into::into),
            status: DeadlineStatusDto::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assignment_manager::domain::{AssignmentId, CourseId};
    use chrono::NaiveDate;
    use serde_json::json;

    fn item() -> AssignmentListItem {
        AssignmentListItem {
            id: AssignmentId::new("internal-assignment"),
            title: "レポート".into(),
            deadline: None,
            completion: CompletionState::Incomplete,
            submission: SubmissionTarget::Unknown,
            submission_url: None,
            course: None,
        }
    }

    #[test]
    fn deadline_kinds_survive_ipc_without_inventing_an_instant_for_a_date() {
        for (deadline, expected) in [
            (None, json!({ "kind": "none" })),
            (
                Some(Deadline::DateOnly(
                    NaiveDate::from_ymd_opt(2026, 10, 4).unwrap(),
                )),
                json!({ "kind": "date_only", "value": "2026-10-04" }),
            ),
            (
                Some(Deadline::DateTime(
                    "2026-10-04T03:04:05.123Z".parse().unwrap(),
                )),
                json!({ "kind": "date_time", "value": "2026-10-04T03:04:05.123Z" }),
            ),
        ] {
            let mut source = item();
            source.deadline = deadline;
            let dto = serde_json::to_value(AssignmentDto::from(source)).unwrap();
            assert_eq!(dto["deadline"], expected);
        }
    }

    #[test]
    fn completion_and_all_submission_states_remain_distinct() {
        for (target, expected) in [
            (SubmissionTarget::Unknown, "unknown"),
            (SubmissionTarget::None, "none"),
            (SubmissionTarget::Moodle, "moodle"),
            (SubmissionTarget::WebClass, "web_class"),
            (SubmissionTarget::External, "external"),
        ] {
            let mut source = item();
            source.submission = target;
            source.completion = CompletionState::Completed;
            source.submission_url = Some("https://example.invalid/?token=secret".into());
            let dto = serde_json::to_value(AssignmentDto::from(source)).unwrap();
            assert_eq!(dto["submission"], expected);
            assert_eq!(dto["completion"], "completed");
            assert_eq!(dto["hasSubmissionUrl"], true);
            assert!(!dto.to_string().contains("secret"));
            assert_eq!(dto["id"], "internal-assignment");
            assert_eq!(dto["course"], serde_json::Value::Null);
        }
        assert_eq!(
            serde_json::to_value(CompletionDto::from(CompletionState::Incomplete)).unwrap(),
            "incomplete"
        );
    }

    #[test]
    fn same_course_label_keeps_separate_ids_and_all_binding_origins() {
        for (index, origin, expected) in [
            (1, Some(OriginKind::Moodle), "moodle"),
            (2, Some(OriginKind::WebClass), "web_class"),
            (3, Some(OriginKind::Manual), "manual"),
            (4, None, "unbound"),
        ] {
            let id = format!("internal-course-{index}");
            let dto = CourseDto::from(CourseListInfo {
                id: CourseId::new(&id),
                name: "同じ表示ラベル".into(),
                origin,
            });
            let value = serde_json::to_value(dto).unwrap();
            assert_eq!(
                value,
                json!({"id": id, "displayLabel": "同じ表示ラベル", "origin": expected})
            );
        }
    }
}
