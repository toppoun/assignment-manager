//! Tauri JSON request vocabulary. Query semantics live in the shared application.

use assignment_manager_application::query as application;
use serde::Deserialize;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListRequest {
    pub completion: CompletionSelection,
    pub course: CourseSelection,
    pub deadline: DeadlineSelection,
    pub sort: SortSelection,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionSelection {
    All,
    #[default]
    Incomplete,
    Completed,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CourseSelection {
    #[default]
    All,
    Unassigned,
    Course {
        id: String,
    },
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeadlineSelection {
    #[default]
    All,
    Overdue,
    WithinSevenDays,
    Today,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortSelection {
    #[default]
    Deadline,
    Course,
    Title,
}

impl From<ListRequest> for application::AssignmentQuery {
    fn from(request: ListRequest) -> Self {
        Self {
            completion: match request.completion {
                CompletionSelection::All => application::CompletionSelection::All,
                CompletionSelection::Incomplete => application::CompletionSelection::Incomplete,
                CompletionSelection::Completed => application::CompletionSelection::Completed,
            },
            course: match request.course {
                CourseSelection::All => application::CourseSelection::All,
                CourseSelection::Unassigned => application::CourseSelection::Unassigned,
                CourseSelection::Course { id } => application::CourseSelection::Course { id },
            },
            deadline: match request.deadline {
                DeadlineSelection::All => application::DeadlineSelection::All,
                DeadlineSelection::Overdue => application::DeadlineSelection::Overdue,
                DeadlineSelection::WithinSevenDays => {
                    application::DeadlineSelection::WithinSevenDays
                }
                DeadlineSelection::Today => application::DeadlineSelection::Today,
            },
            sort: match request.sort {
                SortSelection::Deadline => application::SortSelection::Deadline,
                SortSelection::Course => application::SortSelection::Course,
                SortSelection::Title => application::SortSelection::Title,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ApplicationError;
    use application::QueryClock;
    use assignment_manager::{
        domain::{CompletionState, CourseId},
        query::{AssignmentSort, CompletionFilter, CourseFilter, DeadlineFilter},
    };
    use chrono::DateTime;
    use serde_json::json;

    fn clock() -> QueryClock {
        QueryClock::from_local(DateTime::parse_from_rfc3339("2026-10-04T12:00:00+05:45").unwrap())
            .unwrap()
    }

    #[test]
    fn typed_requests_reject_sql_unknown_fields_and_clock_input() {
        for value in [
            json!({"completion":"incomplete","course":{"kind":"all"},"deadline":"all","sort":"deadline","now":"secret"}),
            json!({"completion":"incomplete","course":{"kind":"all"},"deadline":"all","sort":"SELECT secret"}),
            json!({"completion":"incomplete","course":{"kind":"course","id":"internal-id","scope":"secret"},"deadline":"all","sort":"title"}),
        ] {
            assert!(serde_json::from_value::<ListRequest>(value).is_err());
        }
    }

    #[test]
    fn every_selection_maps_to_core_contract_and_opaque_course_id_is_preserved() {
        let clock = clock();
        for (value, expected) in [
            ("all", CompletionFilter::All),
            (
                "incomplete",
                CompletionFilter::State(CompletionState::Incomplete),
            ),
            (
                "completed",
                CompletionFilter::State(CompletionState::Completed),
            ),
        ] {
            let request: ListRequest = serde_json::from_value(json!({"completion":value,"course":{"kind":"all"},"deadline":"all","sort":"deadline"})).unwrap();
            assert_eq!(
                application::AssignmentQuery::from(request)
                    .options(&clock)
                    .unwrap()
                    .completion,
                expected
            );
        }
        for (value, expected) in [
            ("deadline", AssignmentSort::ByDeadline),
            ("course", AssignmentSort::ByCourse),
            ("title", AssignmentSort::ByTitle),
        ] {
            let request: ListRequest = serde_json::from_value(json!({"completion":"all","course":{"kind":"course","id":" ID 01 "},"deadline":"within_seven_days","sort":value})).unwrap();
            let options = application::AssignmentQuery::from(request)
                .options(&clock)
                .unwrap();
            assert_eq!(options.sort, expected);
            assert_eq!(
                options.course,
                CourseFilter::Course(CourseId::new(" ID 01 "))
            );
            assert_eq!(options.deadline, clock.seven_days);
        }
        for (selection, expected) in [
            (DeadlineSelection::All, DeadlineFilter::All),
            (DeadlineSelection::Overdue, DeadlineFilter::Overdue),
            (DeadlineSelection::Today, clock.today),
        ] {
            assert_eq!(
                application::AssignmentQuery::from(ListRequest {
                    deadline: selection,
                    ..Default::default()
                })
                .options(&clock)
                .unwrap()
                .deadline,
                expected
            );
        }
        assert_eq!(
            application::AssignmentQuery::from(ListRequest {
                course: CourseSelection::Unassigned,
                ..Default::default()
            })
            .options(&clock)
            .unwrap()
            .course,
            CourseFilter::Unassigned
        );
        assert_eq!(
            application::AssignmentQuery::from(ListRequest {
                course: CourseSelection::Course { id: String::new() },
                ..Default::default()
            })
            .options(&clock)
            .unwrap_err(),
            ApplicationError::InvalidQuery
        );
    }
}
