//! Safe application read models. No transport tags, localized text or raw URLs.
use assignment_manager::{
    domain::{CompletionState, Deadline, SubmissionTarget},
    import::OriginKind,
    query::{AssignmentListItem, CourseListInfo},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentListResponse {
    pub items: Vec<AssignmentListEntry>,
    pub courses: Vec<CourseOption>,
    pub total_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadlineStatus {
    Overdue,
    Today,
    Upcoming,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CourseOption {
    pub id: String,
    pub display_label: String,
    pub origin: Option<OriginKind>,
}

impl From<CourseListInfo> for CourseOption {
    fn from(value: CourseListInfo) -> Self {
        Self {
            id: value.id.as_str().to_owned(),
            display_label: value.name,
            origin: value.origin,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentListEntry {
    pub id: String,
    pub title: String,
    pub deadline: Option<Deadline>,
    pub completion: CompletionState,
    pub submission: SubmissionTarget,
    pub has_submission_url: bool,
    pub course: Option<CourseOption>,
    pub status: DeadlineStatus,
}

impl From<AssignmentListItem> for AssignmentListEntry {
    fn from(value: AssignmentListItem) -> Self {
        Self {
            id: value.id.as_str().to_owned(),
            title: value.title,
            deadline: value.deadline,
            completion: value.completion,
            submission: value.submission,
            has_submission_url: value.submission_url.is_some(),
            course: value.course.map(Into::into),
            status: DeadlineStatus::None,
        }
    }
}
