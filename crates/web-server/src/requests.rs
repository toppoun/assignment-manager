//! Transport vocabulary only. Filtering and ordering belong to the application.
use crate::WebError;
use assignment_manager_application::query as app;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompletionRequest {
    pub completion: DesiredCompletion,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DesiredCompletion {
    Incomplete,
    Completed,
}
impl From<DesiredCompletion> for assignment_manager::domain::CompletionState {
    fn from(value: DesiredCompletion) -> Self {
        match value {
            DesiredCompletion::Incomplete => Self::Incomplete,
            DesiredCompletion::Completed => Self::Completed,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ListRequest {
    completion: Option<Completion>,
    deadline: Option<Deadline>,
    sort: Option<Sort>,
    course_mode: Option<CourseMode>,
    course_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Completion {
    All,
    Incomplete,
    Completed,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Deadline {
    All,
    Overdue,
    Today,
    WithinSevenDays,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Sort {
    Deadline,
    Course,
    Title,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CourseMode {
    All,
    Unassigned,
    Course,
}

impl ListRequest {
    pub fn into_query(self) -> Result<app::AssignmentQuery, WebError> {
        let mut query = app::AssignmentQuery::default();
        if let Some(value) = self.completion {
            query.completion = match value {
                Completion::All => app::CompletionSelection::All,
                Completion::Incomplete => app::CompletionSelection::Incomplete,
                Completion::Completed => app::CompletionSelection::Completed,
            };
        }
        if let Some(value) = self.deadline {
            query.deadline = match value {
                Deadline::All => app::DeadlineSelection::All,
                Deadline::Overdue => app::DeadlineSelection::Overdue,
                Deadline::Today => app::DeadlineSelection::Today,
                Deadline::WithinSevenDays => app::DeadlineSelection::WithinSevenDays,
            };
        }
        if let Some(value) = self.sort {
            query.sort = match value {
                Sort::Deadline => app::SortSelection::Deadline,
                Sort::Course => app::SortSelection::Course,
                Sort::Title => app::SortSelection::Title,
            };
        }
        query.course = match (self.course_mode, self.course_id) {
            (None | Some(CourseMode::All), None) => app::CourseSelection::All,
            (Some(CourseMode::Unassigned), None) => app::CourseSelection::Unassigned,
            (Some(CourseMode::Course), Some(id))
                if !id.trim().is_empty()
                    && id.len() <= 256
                    && !id.chars().any(char::is_control) =>
            {
                app::CourseSelection::Course { id }
            }
            _ => return Err(WebError::InvalidQuery),
        };
        Ok(query)
    }
}
