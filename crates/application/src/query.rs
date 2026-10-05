//! Application query vocabulary and projections from one validated DB snapshot.

use std::collections::HashSet;

use assignment_manager::{
    domain::{CompletionState, CourseId},
    persistence::SqliteStore,
    query::{
        AssignmentListOptions, AssignmentSort, CompletionFilter, CourseFilter, DeadlineFilter,
        QueryTimeContext, filter_sort_assignment_list, load_assignment_list,
    },
};
use chrono::{DateTime, Days, Duration, Offset, TimeZone, Utc};

use crate::{
    error::ApplicationError,
    models::{AssignmentListEntry, AssignmentListResponse, DeadlineStatus},
};

#[derive(Debug, Default, Clone)]
pub struct AssignmentQuery {
    pub completion: CompletionSelection,
    pub course: CourseSelection,
    pub deadline: DeadlineSelection,
    pub sort: SortSelection,
}

#[derive(Debug, Default, Clone, Copy)]
pub enum CompletionSelection {
    All,
    #[default]
    Incomplete,
    Completed,
}

#[derive(Debug, Default, Clone)]
pub enum CourseSelection {
    #[default]
    All,
    Unassigned,
    Course {
        id: String,
    },
}

#[derive(Debug, Default, Clone, Copy)]
pub enum DeadlineSelection {
    #[default]
    All,
    Overdue,
    WithinSevenDays,
    Today,
}

#[derive(Debug, Default, Clone, Copy)]
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

impl AssignmentQuery {
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

pub(crate) fn list_from_store(
    store: &SqliteStore,
    request: &AssignmentQuery,
    clock: &QueryClock,
) -> Result<AssignmentListResponse, ApplicationError> {
    let options = request.options(clock)?;
    let snapshot = load_assignment_list(store)?;
    Ok(response_from_snapshot(snapshot, &options, clock))
}

fn response_from_snapshot(
    snapshot: Vec<assignment_manager::query::AssignmentListItem>,
    options: &AssignmentListOptions,
    clock: &QueryClock,
) -> AssignmentListResponse {
    // Every projection below uses this snapshot and this clock, never another read.
    let select = |options: &AssignmentListOptions| {
        let mut items = snapshot.clone();
        filter_sort_assignment_list(&mut items, options, clock.time);
        items
    };
    let all = select(&AssignmentListOptions {
        sort: AssignmentSort::ByCourse,
        ..Default::default()
    });
    let total_count = all.len();
    let mut seen = HashSet::new();
    let courses = all
        .into_iter()
        .filter_map(|item| item.course)
        .filter(|course| seen.insert(course.id.as_str().to_owned()))
        .map(Into::into)
        .collect();
    let membership = |deadline| -> HashSet<String> {
        select(&AssignmentListOptions {
            completion: CompletionFilter::State(CompletionState::Incomplete),
            deadline,
            ..Default::default()
        })
        .into_iter()
        .map(|item| item.id.as_str().to_owned())
        .collect()
    };
    let overdue = membership(DeadlineFilter::Overdue);
    let today = membership(clock.today);
    let upcoming = membership(clock.seven_days);
    let items = select(options)
        .into_iter()
        .map(|item| {
            let status = if overdue.contains(item.id.as_str()) {
                DeadlineStatus::Overdue
            } else if today.contains(item.id.as_str()) {
                DeadlineStatus::Today
            } else if upcoming.contains(item.id.as_str()) {
                DeadlineStatus::Upcoming
            } else {
                DeadlineStatus::None
            };
            let mut entry = AssignmentListEntry::from(item);
            entry.status = status;
            entry
        })
        .collect();
    AssignmentListResponse {
        items,
        courses,
        total_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assignment_manager::{
        domain::{Deadline, SubmissionTarget},
        import::OriginKind,
        persistence::ManualAssignmentInput,
    };

    fn clock() -> QueryClock {
        QueryClock::from_local(DateTime::parse_from_rfc3339("2026-10-04T12:00:00+05:45").unwrap())
            .unwrap()
    }

    fn all_request() -> AssignmentQuery {
        AssignmentQuery {
            completion: CompletionSelection::All,
            ..Default::default()
        }
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
                DeadlineStatus::Overdue,
            ),
            (
                Some(Deadline::DateOnly("2026-10-04".parse().unwrap())),
                false,
                DeadlineStatus::Today,
            ),
            (
                Some(Deadline::DateTime(
                    clock.time.now - Duration::nanoseconds(1),
                )),
                false,
                DeadlineStatus::Overdue,
            ),
            (
                Some(Deadline::DateTime(clock.time.now)),
                false,
                DeadlineStatus::Today,
            ),
            (
                Some(Deadline::DateOnly("2026-10-11".parse().unwrap())),
                false,
                DeadlineStatus::Upcoming,
            ),
            (
                Some(Deadline::DateTime(clock.time.now + Duration::days(7))),
                false,
                DeadlineStatus::Upcoming,
            ),
            (
                Some(Deadline::DateTime(
                    clock.time.now + Duration::days(7) + Duration::nanoseconds(1),
                )),
                false,
                DeadlineStatus::None,
            ),
            (None, false, DeadlineStatus::None),
            (
                Some(Deadline::DateOnly("2026-10-03".parse().unwrap())),
                true,
                DeadlineStatus::None,
            ),
            (
                Some(Deadline::DateOnly("2026-10-04".parse().unwrap())),
                true,
                DeadlineStatus::None,
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
            &AssignmentQuery {
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
        assert_ne!(result.courses[0].origin, result.courses[1].origin);
    }

    #[test]
    fn second_connection_commit_between_load_and_projection_cannot_mix_response_snapshots() {
        use std::{sync::mpsc, thread};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("snapshot.db");
        let mut reader = SqliteStore::open(&path).unwrap();
        let course = reader.create_course("original course", vec![]).unwrap();
        let item = reader
            .create_manual_assignment(ManualAssignmentInput {
                title: "original assignment".into(),
                deadline: Some(Deadline::DateOnly(clock().time.local_date)),
                course_id: Some(course.id.clone()),
                submission: SubmissionTarget::Unknown,
                submission_url: Some("https://example.invalid/?token=PRIVATE_URL".into()),
            })
            .unwrap();
        let snapshot = load_assignment_list(&reader).unwrap();
        let (loaded, read_signal) = mpsc::channel();
        let (committed, commit_signal) = mpsc::channel();
        // Deterministic interleaving at the real load/projection boundary; no sleeps
        // and no production test hooks. Projection has no further access to the DB.
        let response = thread::spawn(move || {
            loaded.send(()).unwrap();
            commit_signal.recv().unwrap();
            let clock = clock();
            response_from_snapshot(snapshot, &all_request().options(&clock).unwrap(), &clock)
        });
        let old_id = item.id.clone();
        let writer = thread::spawn(move || {
            read_signal.recv().unwrap();
            let mut store = SqliteStore::open(path).unwrap();
            let course = store.create_course("new course", vec![]).unwrap();
            store
                .create_manual_assignment(ManualAssignmentInput {
                    title: "new assignment".into(),
                    deadline: None,
                    course_id: Some(course.id),
                    submission: SubmissionTarget::Unknown,
                    submission_url: None,
                })
                .unwrap();
            assignment_manager::actions::toggle_assignment_completion(&mut store, &old_id).unwrap();
            committed.send(()).unwrap();
        });
        let response = response.join().unwrap();
        writer.join().unwrap();
        assert_eq!(response.total_count, 1);
        assert_eq!(response.items.len(), 1);
        assert_eq!(response.courses.len(), 1);
        assert_eq!(response.courses[0].id, course.id.as_str());
        assert_eq!(
            response.items[0].course.as_ref().unwrap(),
            &response.courses[0]
        );
        assert_eq!(response.items[0].completion, CompletionState::Incomplete);
        assert_eq!(response.items[0].status, DeadlineStatus::Today);
        assert!(response.items[0].has_submission_url);
        assert!(!format!("{response:?}").contains("PRIVATE_URL"));
        let fresh = list_from_store(&reader, &all_request(), &clock()).unwrap();
        assert_eq!(fresh.total_count, 2);
        assert_eq!(fresh.courses.len(), 2);
        let old = fresh
            .items
            .iter()
            .find(|e| e.id == item.id.as_str())
            .unwrap();
        assert_eq!(old.completion, CompletionState::Completed);
        assert_eq!(old.status, DeadlineStatus::None);
    }

    #[test]
    fn application_defaults_and_invalid_course_are_independent_of_transport() {
        let options = AssignmentQuery::default().options(&clock()).unwrap();
        assert_eq!(
            options.completion,
            CompletionFilter::State(CompletionState::Incomplete)
        );
        assert_eq!(options.deadline, DeadlineFilter::All);
        assert_eq!(options.sort, AssignmentSort::ByDeadline);
        assert_eq!(
            AssignmentQuery {
                course: CourseSelection::Course { id: String::new() },
                ..Default::default()
            }
            .options(&clock())
            .unwrap_err(),
            ApplicationError::InvalidQuery
        );
    }
}
