//! Web JSON projection of safe application models; no Tauri dependency.
use assignment_manager::{
    domain::{CompletionState, Deadline, SubmissionTarget},
    import::OriginKind,
};
use assignment_manager_application::models::{
    AssignmentListEntry, AssignmentListResponse, CourseOption, DeadlineStatus,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AssignmentListDto {
    items: Vec<AssignmentDto>,
    courses: Vec<CourseDto>,
    total_count: usize,
    evaluated_at: String,
    time_zone: &'static str,
}
impl AssignmentListDto {
    pub fn new(value: AssignmentListResponse, evaluated_at: DateTime<Utc>) -> Self {
        Self {
            items: value.items.into_iter().map(Into::into).collect(),
            courses: value.courses.into_iter().map(Into::into).collect(),
            total_count: value.total_count,
            evaluated_at: evaluated_at.to_rfc3339_opts(SecondsFormat::AutoSi, true),
            time_zone: "Asia/Tokyo",
        }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AssignmentDto {
    id: String,
    title: String,
    deadline: DeadlineDto,
    completion: CompletionDto,
    submission: SubmissionDto,
    has_submission_url: bool,
    course: Option<CourseDto>,
    status: StatusDto,
}
#[derive(Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum DeadlineDto {
    DateOnly(String),
    DateTime(String),
    None,
}
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionDto {
    Incomplete,
    Completed,
}

#[derive(Serialize)]
pub(crate) struct CompletionResponse {
    pub id: String,
    pub completion: CompletionDto,
}
impl From<CompletionState> for CompletionDto {
    fn from(value: CompletionState) -> Self {
        match value {
            CompletionState::Incomplete => Self::Incomplete,
            CompletionState::Completed => Self::Completed,
        }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SubmissionDto {
    Unknown,
    None,
    Moodle,
    WebClass,
    External,
}
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum StatusDto {
    Overdue,
    Today,
    Upcoming,
    None,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CourseDto {
    id: String,
    display_label: String,
    origin: OriginDto,
}
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum OriginDto {
    Moodle,
    WebClass,
    Manual,
    Unbound,
}

impl From<CourseOption> for CourseDto {
    fn from(value: CourseOption) -> Self {
        Self {
            id: value.id,
            display_label: value.display_label,
            origin: match value.origin {
                Some(OriginKind::Moodle) => OriginDto::Moodle,
                Some(OriginKind::WebClass) => OriginDto::WebClass,
                Some(OriginKind::Manual) => OriginDto::Manual,
                None => OriginDto::Unbound,
            },
        }
    }
}
impl From<AssignmentListEntry> for AssignmentDto {
    fn from(value: AssignmentListEntry) -> Self {
        Self {
            id: value.id,
            title: value.title,
            deadline: match value.deadline {
                Some(Deadline::DateOnly(date)) => DeadlineDto::DateOnly(date.to_string()),
                Some(Deadline::DateTime(instant)) => {
                    DeadlineDto::DateTime(instant.to_rfc3339_opts(SecondsFormat::AutoSi, true))
                }
                None => DeadlineDto::None,
            },
            completion: match value.completion {
                CompletionState::Incomplete => CompletionDto::Incomplete,
                CompletionState::Completed => CompletionDto::Completed,
            },
            submission: match value.submission {
                SubmissionTarget::Unknown => SubmissionDto::Unknown,
                SubmissionTarget::None => SubmissionDto::None,
                SubmissionTarget::Moodle => SubmissionDto::Moodle,
                SubmissionTarget::WebClass => SubmissionDto::WebClass,
                SubmissionTarget::External => SubmissionDto::External,
            },
            has_submission_url: value.has_submission_url,
            course: value.course.map(Into::into),
            status: match value.status {
                DeadlineStatus::Overdue => StatusDto::Overdue,
                DeadlineStatus::Today => StatusDto::Today,
                DeadlineStatus::Upcoming => StatusDto::Upcoming,
                DeadlineStatus::None => StatusDto::None,
            },
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SettingsStatusDto {
    pub moodle_configured: bool,
    pub webclass_configured: bool,
}
