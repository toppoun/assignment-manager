//! Desktop request vocabulary and presentation derived solely from core queries.

use std::collections::HashSet;

use assignment_manager::{
    domain::{CompletionState, CourseId},
    persistence::SqliteStore,
    query::{
        AssignmentListOptions, AssignmentSort, CompletionFilter, CourseFilter, DeadlineFilter,
        QueryTimeContext, query_assignments,
    },
};
use chrono::{DateTime, Days, Duration, Offset, TimeZone, Utc};
use serde::Deserialize;

use crate::{
    dto::{AssignmentDto, AssignmentListDto, CourseDto, DeadlineStatusDto},
    error::ApplicationError,
};

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

/// One clock sample, with today's actual OS timezone boundaries (including DST).
pub struct QueryClock {
    pub time: QueryTimeContext,
    pub today: DeadlineFilter,
    pub seven_days: DeadlineFilter,
}

pub fn time_context<T: TimeZone>(local: DateTime<T>) -> QueryTimeContext {
    QueryTimeContext {
        now: local.with_timezone(&Utc),
        local_date: local.date_naive(),
        utc_offset: local.offset().fix(),
    }
}

impl QueryClock {
    pub fn from_local<T: TimeZone>(local: DateTime<T>) -> Result<Self, ApplicationError> {
        let time = time_context(local.clone());
        let next_date = time
            .local_date
            .checked_add_days(Days::new(1))
            .ok_or(ApplicationError::Unavailable)?;
        let timezone = local.timezone();
        let midnight = |date: chrono::NaiveDate| {
            let value = date
                .and_hms_opt(0, 0, 0)
                .ok_or(ApplicationError::Unavailable)?;
            timezone
                .from_local_datetime(&value)
                .earliest()
                .map(|instant| instant.with_timezone(&Utc))
                .ok_or(ApplicationError::Unavailable)
        };
        let start = midnight(time.local_date)?;
        let end = midnight(next_date)?
            .checked_sub_signed(Duration::nanoseconds(1))
            .ok_or(ApplicationError::Unavailable)?;
        let seven_days = DeadlineFilter::DueWithin {
            from: time,
            until: QueryTimeContext {
                now: time
                    .now
                    .checked_add_signed(Duration::days(7))
                    .ok_or(ApplicationError::Unavailable)?,
                local_date: time
                    .local_date
                    .checked_add_days(Days::new(7))
                    .ok_or(ApplicationError::Unavailable)?,
                ..time
            },
        };
        Ok(Self {
            time,
            today: DeadlineFilter::DueWithin {
                from: QueryTimeContext { now: start, ..time },
                until: QueryTimeContext { now: end, ..time },
            },
            seven_days,
        })
    }
}

impl ListRequest {
    pub fn options(&self, clock: &QueryClock) -> Result<AssignmentListOptions, ApplicationError> {
        Ok(AssignmentListOptions {
            completion: match self.completion {
                CompletionSelection::All => CompletionFilter::All,
                CompletionSelection::Incomplete => {
                    CompletionFilter::State(CompletionState::Incomplete)
                }
                CompletionSelection::Completed => {
                    CompletionFilter::State(CompletionState::Completed)
                }
            },
            course: match &self.course {
                CourseSelection::All => CourseFilter::All,
                CourseSelection::Unassigned => CourseFilter::Unassigned,
                CourseSelection::Course { id } if !id.is_empty() => {
                    CourseFilter::Course(CourseId::new(id))
                }
                CourseSelection::Course { .. } => return Err(ApplicationError::InvalidQuery),
            },
            deadline: match self.deadline {
                DeadlineSelection::All => DeadlineFilter::All,
                DeadlineSelection::Overdue => DeadlineFilter::Overdue,
                DeadlineSelection::WithinSevenDays => clock.seven_days,
                DeadlineSelection::Today => clock.today,
            },
            sort: match self.sort {
                SortSelection::Deadline => AssignmentSort::ByDeadline,
                SortSelection::Course => AssignmentSort::ByCourse,
                SortSelection::Title => AssignmentSort::ByTitle,
            },
        })
    }
}

pub fn list_from_store(
    store: &SqliteStore,
    request: &ListRequest,
    clock: &QueryClock,
) -> Result<AssignmentListDto, ApplicationError> {
    let options = request.options(clock)?;
    // Unfiltered metadata uses core Course sort: label, ID tie-break, unassigned last.
    // Keep choices available even when completion/deadline filters remove every item.
    let all = query_assignments(
        store,
        &AssignmentListOptions {
            sort: AssignmentSort::ByCourse,
            ..Default::default()
        },
        clock.time,
    )?;
    let total_count = all.len();
    let mut seen = HashSet::new();
    let courses: Vec<CourseDto> = all
        .into_iter()
        .filter_map(|item| item.course)
        .filter(|course| seen.insert(course.id.as_str().to_owned()))
        .map(Into::into)
        .collect();

    let items = query_assignments(store, &options, clock.time)?;
    // No comparisons of deadlines here: badges use membership in existing core filters.
    let membership = |deadline| -> Result<HashSet<String>, ApplicationError> {
        Ok(query_assignments(
            store,
            &AssignmentListOptions {
                completion: CompletionFilter::State(CompletionState::Incomplete),
                deadline,
                ..Default::default()
            },
            clock.time,
        )?
        .into_iter()
        .map(|item| item.id.as_str().to_owned())
        .collect())
    };
    let overdue = membership(DeadlineFilter::Overdue)?;
    let today = membership(clock.today)?;
    let upcoming = membership(clock.seven_days)?;
    let items = items
        .into_iter()
        .map(|item| {
            let status = if overdue.contains(item.id.as_str()) {
                DeadlineStatusDto::Overdue
            } else if today.contains(item.id.as_str()) {
                DeadlineStatusDto::Today
            } else if upcoming.contains(item.id.as_str()) {
                DeadlineStatusDto::Upcoming
            } else {
                DeadlineStatusDto::None
            };
            let mut dto = AssignmentDto::from(item);
            dto.status = status;
            dto
        })
        .collect();
    Ok(AssignmentListDto {
        items,
        courses,
        total_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use assignment_manager::{
        domain::{Deadline, SubmissionTarget},
        import::OriginKind,
        persistence::ManualAssignmentInput,
    };
    use serde_json::json;

    fn clock() -> QueryClock {
        QueryClock::from_local(DateTime::parse_from_rfc3339("2026-10-04T12:00:00+05:45").unwrap())
            .unwrap()
    }

    fn all_request() -> ListRequest {
        ListRequest {
            completion: CompletionSelection::All,
            ..Default::default()
        }
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
            assert_eq!(request.options(&clock).unwrap().completion, expected);
        }
        for (value, expected) in [
            ("deadline", AssignmentSort::ByDeadline),
            ("course", AssignmentSort::ByCourse),
            ("title", AssignmentSort::ByTitle),
        ] {
            let request: ListRequest = serde_json::from_value(json!({"completion":"all","course":{"kind":"course","id":" ID 01 "},"deadline":"within_seven_days","sort":value})).unwrap();
            let options = request.options(&clock).unwrap();
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
                ListRequest {
                    deadline: selection,
                    ..Default::default()
                }
                .options(&clock)
                .unwrap()
                .deadline,
                expected
            );
        }
        assert_eq!(
            ListRequest {
                course: CourseSelection::Unassigned,
                ..Default::default()
            }
            .options(&clock)
            .unwrap()
            .course,
            CourseFilter::Unassigned
        );
        assert_eq!(
            ListRequest {
                course: CourseSelection::Course { id: String::new() },
                ..Default::default()
            }
            .options(&clock)
            .unwrap_err(),
            ApplicationError::InvalidQuery
        );
    }

    #[test]
    fn local_clock_windows_are_inclusive_and_not_fixed_to_japan() {
        let clock = clock();
        assert_eq!(clock.time.utc_offset.local_minus_utc(), 5 * 3600 + 45 * 60);
        let DeadlineFilter::DueWithin { from, until } = clock.today else {
            panic!()
        };
        assert_eq!(from.now.to_rfc3339(), "2026-10-03T18:15:00+00:00");
        assert_eq!(
            until.now + Duration::nanoseconds(1),
            from.now + Duration::days(1)
        );
        assert_eq!(from.local_date, clock.time.local_date);
        assert_eq!(until.local_date, clock.time.local_date);
        let DeadlineFilter::DueWithin { from, until } = clock.seven_days else {
            panic!()
        };
        assert_eq!(from, clock.time);
        assert_eq!(until.now, from.now + Duration::days(7));
        assert_eq!(until.local_date, from.local_date + Days::new(7));
    }

    #[test]
    fn status_membership_respects_date_only_instants_completion_and_inclusive_bounds() {
        let clock = clock();
        let mut store = SqliteStore::open(":memory:").unwrap();
        let mut expected = Vec::new();
        for (deadline, completed, status) in [
            (
                Some(Deadline::DateOnly("2026-10-03".parse().unwrap())),
                false,
                DeadlineStatusDto::Overdue,
            ),
            (
                Some(Deadline::DateOnly("2026-10-04".parse().unwrap())),
                false,
                DeadlineStatusDto::Today,
            ),
            (
                Some(Deadline::DateTime(
                    clock.time.now - Duration::nanoseconds(1),
                )),
                false,
                DeadlineStatusDto::Overdue,
            ),
            (
                Some(Deadline::DateTime(clock.time.now)),
                false,
                DeadlineStatusDto::Today,
            ),
            (
                Some(Deadline::DateOnly("2026-10-11".parse().unwrap())),
                false,
                DeadlineStatusDto::Upcoming,
            ),
            (
                Some(Deadline::DateTime(clock.time.now + Duration::days(7))),
                false,
                DeadlineStatusDto::Upcoming,
            ),
            (
                Some(Deadline::DateTime(
                    clock.time.now + Duration::days(7) + Duration::nanoseconds(1),
                )),
                false,
                DeadlineStatusDto::None,
            ),
            (None, false, DeadlineStatusDto::None),
            (
                Some(Deadline::DateOnly("2026-10-03".parse().unwrap())),
                true,
                DeadlineStatusDto::None,
            ),
            (
                Some(Deadline::DateOnly("2026-10-04".parse().unwrap())),
                true,
                DeadlineStatusDto::None,
            ),
        ] {
            let assignment = store
                .create_manual_assignment(ManualAssignmentInput {
                    title: "課題".into(),
                    deadline,
                    course_id: None,
                    submission: SubmissionTarget::Unknown,
                    submission_url: None,
                })
                .unwrap();
            if completed {
                assignment_manager::actions::toggle_assignment_completion(
                    &mut store,
                    &assignment.id,
                )
                .unwrap();
            }
            expected.push((assignment.id.as_str().to_owned(), status));
        }
        let result = list_from_store(&store, &all_request(), &clock).unwrap();
        assert_eq!(result.total_count, expected.len());
        for (id, status) in expected {
            let item = result.items.iter().find(|item| item.id == id).unwrap();
            assert_eq!(item.status, status);
            assert_eq!(
                serde_json::to_value(item).unwrap()["status"],
                serde_json::to_value(status).unwrap()
            );
        }
    }

    #[test]
    fn filtered_empty_keeps_total_and_all_courses_in_core_course_order() {
        let mut store = SqliteStore::open(":memory:").unwrap();
        let mut course_ids = Vec::new();
        for origin in [OriginKind::Moodle, OriginKind::WebClass] {
            let course = store
                .create_course_with_source("同じ表示名", vec![], origin, "test", "synthetic-id")
                .unwrap();
            store
                .create_manual_assignment(ManualAssignmentInput {
                    title: "課題".into(),
                    deadline: None,
                    course_id: Some(course.id.clone()),
                    submission: SubmissionTarget::Unknown,
                    submission_url: None,
                })
                .unwrap();
            course_ids.push(course.id.as_str().to_owned());
        }
        course_ids.sort();
        let result = list_from_store(
            &store,
            &ListRequest {
                completion: CompletionSelection::Completed,
                ..Default::default()
            },
            &clock(),
        )
        .unwrap();
        assert!(result.items.is_empty());
        assert_eq!(result.total_count, 2);
        assert_eq!(
            result
                .courses
                .iter()
                .map(|course| &course.id)
                .collect::<Vec<_>>(),
            course_ids.iter().collect::<Vec<_>>()
        );
        assert_ne!(
            serde_json::to_value(&result.courses[0]).unwrap()["origin"],
            serde_json::to_value(&result.courses[1]).unwrap()["origin"]
        );
    }
}
