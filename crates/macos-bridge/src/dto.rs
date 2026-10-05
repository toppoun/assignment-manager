use assignment_manager::{
    domain::{CompletionState, Deadline, SubmissionTarget},
    import::OriginKind,
};
use assignment_manager_application::{models, query};
use chrono::{Datelike, SecondsFormat};

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum DeadlineDto {
    DateOnly { year: i32, month: u32, day: u32 },
    DateTime { utc_rfc3339: String },
}

impl From<Deadline> for DeadlineDto {
    fn from(value: Deadline) -> Self {
        match value {
            Deadline::DateOnly(date) => Self::DateOnly {
                year: date.year(),
                month: date.month(),
                day: date.day(),
            },
            Deadline::DateTime(instant) => Self::DateTime {
                utc_rfc3339: instant.to_rfc3339_opts(SecondsFormat::AutoSi, true),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CompletionDto {
    Incomplete,
    Completed,
}
impl From<CompletionState> for CompletionDto {
    fn from(value: CompletionState) -> Self {
        match value {
            CompletionState::Incomplete => Self::Incomplete,
            CompletionState::Completed => Self::Completed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SubmissionDto {
    Unknown,
    None,
    Moodle,
    WebClass,
    External,
}
impl From<SubmissionTarget> for SubmissionDto {
    fn from(value: SubmissionTarget) -> Self {
        match value {
            SubmissionTarget::Unknown => Self::Unknown,
            SubmissionTarget::None => Self::None,
            SubmissionTarget::Moodle => Self::Moodle,
            SubmissionTarget::WebClass => Self::WebClass,
            SubmissionTarget::External => Self::External,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum OriginDto {
    Moodle,
    WebClass,
    Manual,
}
impl From<OriginKind> for OriginDto {
    fn from(value: OriginKind) -> Self {
        match value {
            OriginKind::Moodle => Self::Moodle,
            OriginKind::WebClass => Self::WebClass,
            OriginKind::Manual => Self::Manual,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CourseDto {
    pub id: String,
    pub display_label: String,
    pub origin: Option<OriginDto>,
}
impl From<models::CourseOption> for CourseDto {
    fn from(value: models::CourseOption) -> Self {
        Self {
            id: value.id,
            display_label: value.display_label,
            origin: value.origin.map(Into::into),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DeadlineStatusDto {
    Overdue,
    Today,
    Upcoming,
    None,
}
impl From<models::DeadlineStatus> for DeadlineStatusDto {
    fn from(value: models::DeadlineStatus) -> Self {
        match value {
            models::DeadlineStatus::Overdue => Self::Overdue,
            models::DeadlineStatus::Today => Self::Today,
            models::DeadlineStatus::Upcoming => Self::Upcoming,
            models::DeadlineStatus::None => Self::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AssignmentDto {
    pub id: String,
    pub title: String,
    pub deadline: Option<DeadlineDto>,
    pub completion: CompletionDto,
    pub submission: SubmissionDto,
    pub course: Option<CourseDto>,
    pub deadline_status: DeadlineStatusDto,
}
impl From<models::AssignmentListEntry> for AssignmentDto {
    fn from(value: models::AssignmentListEntry) -> Self {
        Self {
            id: value.id,
            title: value.title,
            deadline: value.deadline.map(Into::into),
            completion: value.completion.into(),
            submission: value.submission.into(),
            course: value.course.map(Into::into),
            deadline_status: value.status.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AssignmentListDto {
    pub items: Vec<AssignmentDto>,
    pub courses: Vec<CourseDto>,
    pub total_count: u64,
}
impl From<models::AssignmentListResponse> for AssignmentListDto {
    fn from(value: models::AssignmentListResponse) -> Self {
        Self {
            items: value.items.into_iter().map(Into::into).collect(),
            courses: value.courses.into_iter().map(Into::into).collect(),
            total_count: value.total_count as u64,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CompletionSelectionDto {
    All,
    Incomplete,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum CourseSelectionDto {
    All,
    Unassigned,
    Course { id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DeadlineSelectionDto {
    All,
    Overdue,
    WithinSevenDays,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SortSelectionDto {
    Deadline,
    Course,
    Title,
}

/// Transport vocabulary only; validation, clock sampling, filters and order are shared.
#[derive(Debug, Clone, uniffi::Record)]
pub struct QueryOptions {
    pub completion: CompletionSelectionDto,
    pub course: CourseSelectionDto,
    pub deadline: DeadlineSelectionDto,
    pub sort: SortSelectionDto,
}
impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            completion: CompletionSelectionDto::Incomplete,
            course: CourseSelectionDto::All,
            deadline: DeadlineSelectionDto::All,
            sort: SortSelectionDto::Deadline,
        }
    }
}
impl From<QueryOptions> for query::AssignmentQuery {
    fn from(value: QueryOptions) -> Self {
        Self {
            completion: match value.completion {
                CompletionSelectionDto::All => query::CompletionSelection::All,
                CompletionSelectionDto::Incomplete => query::CompletionSelection::Incomplete,
                CompletionSelectionDto::Completed => query::CompletionSelection::Completed,
            },
            course: match value.course {
                CourseSelectionDto::All => query::CourseSelection::All,
                CourseSelectionDto::Unassigned => query::CourseSelection::Unassigned,
                CourseSelectionDto::Course { id } => query::CourseSelection::Course { id },
            },
            deadline: match value.deadline {
                DeadlineSelectionDto::All => query::DeadlineSelection::All,
                DeadlineSelectionDto::Overdue => query::DeadlineSelection::Overdue,
                DeadlineSelectionDto::WithinSevenDays => query::DeadlineSelection::WithinSevenDays,
            },
            sort: match value.sort {
                SortSelectionDto::Deadline => query::SortSelection::Deadline,
                SortSelectionDto::Course => query::SortSelection::Course,
                SortSelectionDto::Title => query::SortSelection::Title,
            },
        }
    }
}
