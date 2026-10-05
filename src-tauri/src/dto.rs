use assignment_manager::{
    domain::{CompletionState, Deadline, SubmissionTarget},
    import::OriginKind,
};
use assignment_manager_application::models::{
    AssignmentListEntry, AssignmentListResponse, CourseOption, DeadlineStatus,
};
use assignment_manager_application::settings::SettingsStatus;
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

impl From<CourseOption> for CourseDto {
    fn from(course: CourseOption) -> Self {
        Self {
            id: course.id,
            display_label: course.display_label,
            origin: match course.origin {
                Some(OriginKind::Moodle) => CourseOriginDto::Moodle,
                Some(OriginKind::WebClass) => CourseOriginDto::WebClass,
                Some(OriginKind::Manual) => CourseOriginDto::Manual,
                None => CourseOriginDto::Unbound,
            },
        }
    }
}

impl From<AssignmentListEntry> for AssignmentDto {
    fn from(item: AssignmentListEntry) -> Self {
        Self {
            id: item.id,
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
            has_submission_url: item.has_submission_url,
            course: item.course.map(Into::into),
            status: item.status.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use serde_json::json;

    fn item() -> AssignmentListEntry {
        AssignmentListEntry {
            id: "internal-assignment".into(),
            title: "レポート".into(),
            deadline: None,
            completion: CompletionState::Incomplete,
            submission: SubmissionTarget::Unknown,
            has_submission_url: false,
            status: DeadlineStatus::None,
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
            source.has_submission_url = true;
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
            let dto = CourseDto::from(CourseOption {
                id: id.clone(),
                display_label: "同じ表示ラベル".into(),
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

impl From<AssignmentListResponse> for AssignmentListDto {
    fn from(value: AssignmentListResponse) -> Self {
        Self {
            items: value.items.into_iter().map(Into::into).collect(),
            courses: value.courses.into_iter().map(Into::into).collect(),
            total_count: value.total_count,
        }
    }
}
impl From<DeadlineStatus> for DeadlineStatusDto {
    fn from(value: DeadlineStatus) -> Self {
        match value {
            DeadlineStatus::Overdue => Self::Overdue,
            DeadlineStatus::Today => Self::Today,
            DeadlineStatus::Upcoming => Self::Upcoming,
            DeadlineStatus::None => Self::None,
        }
    }
}
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SettingsStatusDto {
    pub moodle_configured: bool,
    pub webclass_configured: bool,
    pub sync_on_startup: bool,
}
impl From<SettingsStatus> for SettingsStatusDto {
    fn from(value: SettingsStatus) -> Self {
        Self {
            moodle_configured: value.moodle_configured,
            webclass_configured: value.webclass_configured,
            sync_on_startup: value.sync_on_startup,
        }
    }
}
