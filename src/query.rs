//! Read-only application queries for local assignment display.
//!
//! Callers receive domain values and course display information without reading
//! SQLite tables or interpreting source identities. No sync metadata is included.

use std::cmp::Ordering;

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, Timelike, Utc};

use crate::domain::{AssignmentId, CompletionState, CourseId, Deadline, SubmissionTarget};
use crate::import::OriginKind;
use crate::persistence::{PersistenceError, SqliteStore};

/// The saved display information for one internal course, not a merged real-world class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CourseListInfo {
    pub id: CourseId,
    /// The currently saved display label, not a guarantee of an official course name.
    /// Moodle discovery initially uses a provisional CATEGORIES source label.
    pub name: String,
    /// Origin of the course binding; `None` means the course has no source binding.
    /// This is independent of the assignment's submission target or source.
    pub origin: Option<OriginKind>,
}

/// One assignment with everything needed for the initial assignment list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentListItem {
    pub id: AssignmentId,
    pub title: String,
    pub deadline: Option<Deadline>,
    pub completion: CompletionState,
    pub submission: SubmissionTarget,
    /// The saved submission/answer page, if known.
    pub submission_url: Option<String>,
    /// `None` only for assignments without a course relation.
    /// Group by `course.id`, never by name, origin, schedule or string similarity.
    pub course: Option<CourseListInfo>,
}

/// Load all assignments and their course display information from one SQLite snapshot.
///
/// Results use ascending, case-sensitive binary order of the opaque internal
/// [`AssignmentId`] string, explicitly ordered in SQL. This is not deadline or
/// display priority. Completed assignments, missing deadlines and unassigned
/// assignments are all retained; an empty database returns an empty vector.
/// Courses without assignments do not appear in this assignment list.
///
/// Course identity and origin are preserved exactly; Moodle and WebClass courses
/// are never merged, even when their saved names match. The flat list leaves
/// presentation layout to callers without requiring any course/source lookups.
///
/// Failures reuse the safe [`PersistenceError`] boundary: only fixed categories,
/// with no row values, paths, SQL or driver causes. Invalid displayed data, course
/// relations or course source metadata fail the whole query, never a partial list.
///
/// ```
/// use assignment_manager::persistence::SqliteStore;
/// use assignment_manager::query::load_assignment_list;
///
/// let store = SqliteStore::open(":memory:")?;
/// let assignments = load_assignment_list(&store)?;
/// assert!(assignments.is_empty());
/// # Ok::<(), assignment_manager::persistence::PersistenceError>(())
/// ```
pub fn load_assignment_list(
    store: &SqliteStore,
) -> Result<Vec<AssignmentListItem>, PersistenceError> {
    store.list_assignments_with_courses()
}

/// Caller-supplied clock/calendar context; queries never read the system clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryTimeContext {
    /// Exact reference instant for DateTime deadlines.
    pub now: DateTime<Utc>,
    /// Current calendar date in the caller's chosen timezone, for DateOnly deadlines.
    pub local_date: NaiveDate,
    /// Calendar offset for mixed deadline sorting. The caller supplies an offset
    /// consistent with its local calendar (e.g. UTC+09:00 for Japan).
    /// This is a fixed-offset calendar, not a timezone/DST database.
    pub utc_offset: FixedOffset,
}

/// Local completion selection, reusing the existing domain state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CompletionFilter {
    #[default]
    All,
    State(CompletionState),
}

/// Exact internal course selection; display labels never establish identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CourseFilter {
    #[default]
    All,
    Course(CourseId),
    Unassigned,
}

/// Deadline selection without converting a DateOnly deadline to an instant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DeadlineFilter {
    #[default]
    All,
    /// Incomplete assignments only: DateOnly < local_date, or DateTime < now.
    /// Today and the exact reference instant are not overdue. Combining this
    /// with Completed selects nothing because filters are intersected.
    Overdue,
    /// Inclusive bounds: DateOnly uses from.local_date..=until.local_date;
    /// DateTime uses from.now..=until.now. Offsets of these bounds are unused;
    /// callers supply both dates in the same chosen local calendar.
    /// Reversed dates or instants make the entire window empty.
    /// Completed assignments are eligible unless the completion filter excludes them.
    DueWithin {
        from: QueryTimeContext,
        until: QueryTimeContext,
    },
}

/// Ascending application order; every mode breaks ties by internal AssignmentId.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AssignmentSort {
    /// Earliest calendar day first, with DateTime days using the context's offset.
    /// Within a day, exact instants come first in chronological order, followed
    /// by DateOnly deadlines (which do not expire at midnight). Missing deadlines
    /// are last. No DateOnly value is assigned an artificial timestamp.
    #[default]
    ByDeadline,
    /// Saved course label, then CourseId, then AssignmentId; unassigned items last.
    /// Labels use Rust string order, with no locale rules or cross-source merging.
    ByCourse,
    /// Rust lexical title order, then AssignmentId.
    ByTitle,
}

/// Intersected filters and an ascending sort, applied to the Phase 8A read model.
/// Defaults retain all assignments and sort by deadline, with no deadline last.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssignmentListOptions {
    pub completion: CompletionFilter,
    pub course: CourseFilter,
    pub deadline: DeadlineFilter,
    pub sort: AssignmentSort,
}

/// Load the complete validated read model, then filter and sort in memory.
///
/// No further database queries, source identity interpretation or schedule checks
/// are needed. An invalid stored row still fails the complete load, even if a
/// filter would exclude it. Errors retain the existing safe persistence boundary.
/// Use DueWithin with from.now = time.now and from.local_date = time.local_date
/// to select upcoming deadlines; the caller chooses the inclusive end bounds.
/// Missing deadlines match only DeadlineFilter::All.
///
/// ```
/// use assignment_manager::domain::CompletionState;
/// use assignment_manager::persistence::SqliteStore;
/// use assignment_manager::query::{
///     AssignmentListOptions, CompletionFilter, DeadlineFilter, QueryTimeContext,
///     query_assignments,
/// };
/// use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
///
/// let store = SqliteStore::open(":memory:")?;
/// let time = QueryTimeContext {
///     now: "2026-10-03T15:30:00Z".parse::<DateTime<Utc>>().unwrap(),
///     local_date: NaiveDate::from_ymd_opt(2026, 10, 4).unwrap(),
///     utc_offset: FixedOffset::east_opt(9 * 3600).unwrap(),
/// };
/// let until = QueryTimeContext {
///     now: time.now + chrono::Duration::days(7),
///     local_date: time.local_date + chrono::Duration::days(7),
///     ..time
/// };
/// let options = AssignmentListOptions {
///     completion: CompletionFilter::State(CompletionState::Incomplete),
///     deadline: DeadlineFilter::DueWithin { from: time, until },
///     ..Default::default()
/// };
/// assert!(query_assignments(&store, &options, time)?.is_empty());
/// # Ok::<(), assignment_manager::persistence::PersistenceError>(())
/// ```
pub fn query_assignments(
    store: &SqliteStore,
    options: &AssignmentListOptions,
    time: QueryTimeContext,
) -> Result<Vec<AssignmentListItem>, PersistenceError> {
    let mut items = load_assignment_list(store)?;
    items.retain(|item| matches_filters(item, options, time));
    items.sort_by(|a, b| {
        let order = match options.sort {
            AssignmentSort::ByDeadline => {
                compare_deadlines(a.deadline.as_ref(), b.deadline.as_ref(), time.utc_offset)
            }
            AssignmentSort::ByCourse => match (&a.course, &b.course) {
                (Some(a), Some(b)) => a
                    .name
                    .cmp(&b.name)
                    .then_with(|| a.id.as_str().cmp(b.id.as_str())),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            },
            AssignmentSort::ByTitle => a.title.cmp(&b.title),
        };
        order.then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
    Ok(items)
}

fn matches_filters(
    item: &AssignmentListItem,
    options: &AssignmentListOptions,
    time: QueryTimeContext,
) -> bool {
    let completion_matches = match options.completion {
        CompletionFilter::All => true,
        CompletionFilter::State(state) => item.completion == state,
    };
    let course_matches = match &options.course {
        CourseFilter::All => true,
        CourseFilter::Course(id) => item.course.as_ref().is_some_and(|course| course.id == *id),
        CourseFilter::Unassigned => item.course.is_none(),
    };
    let deadline_matches = match options.deadline {
        DeadlineFilter::All => true,
        DeadlineFilter::Overdue => {
            item.completion == CompletionState::Incomplete
                && match &item.deadline {
                    Some(Deadline::DateOnly(date)) => *date < time.local_date,
                    Some(Deadline::DateTime(instant)) => *instant < time.now,
                    None => false,
                }
        }
        DeadlineFilter::DueWithin { from, until } => {
            from.now <= until.now
                && from.local_date <= until.local_date
                && match &item.deadline {
                    Some(Deadline::DateOnly(date)) => {
                        from.local_date <= *date && *date <= until.local_date
                    }
                    Some(Deadline::DateTime(instant)) => {
                        from.now <= *instant && *instant <= until.now
                    }
                    None => false,
                }
        }
    };
    completion_matches && course_matches && deadline_matches
}

fn compare_deadlines(a: Option<&Deadline>, b: Option<&Deadline>, offset: FixedOffset) -> Ordering {
    match (a, b) {
        (Some(Deadline::DateOnly(a)), Some(Deadline::DateOnly(b))) => a.cmp(b),
        (Some(Deadline::DateTime(a)), Some(Deadline::DateTime(b))) => a.cmp(b),
        (Some(Deadline::DateOnly(a)), Some(Deadline::DateTime(b))) => {
            i64::from(a.num_days_from_ce())
                .cmp(&local_calendar_day(b, offset))
                .then(Ordering::Greater)
        }
        (Some(Deadline::DateTime(a)), Some(Deadline::DateOnly(b))) => local_calendar_day(a, offset)
            .cmp(&i64::from(b.num_days_from_ce()))
            .then(Ordering::Less),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn local_calendar_day(instant: &DateTime<Utc>, offset: FixedOffset) -> i64 {
    // Day arithmetic avoids overflowing Chrono's local date representation near
    // its minimum/maximum instants when a nonzero offset is supplied.
    let local_seconds =
        i64::from(instant.time().num_seconds_from_midnight()) + i64::from(offset.local_minus_utc());
    i64::from(instant.date_naive().num_days_from_ce()) + local_seconds.div_euclid(86_400)
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, NaiveDate, Utc};

    use super::*;
    use crate::domain::Assignment;
    use crate::import::ExternalRecordKey;
    use crate::persistence::ManualAssignmentInput;

    fn input(course_id: Option<CourseId>) -> ManualAssignmentInput {
        ManualAssignmentInput {
            title: "Report 日本語".into(),
            deadline: None,
            course_id,
            submission: SubmissionTarget::Unknown,
            submission_url: None,
        }
    }

    fn expected_item(assignment: Assignment, course: Option<CourseListInfo>) -> AssignmentListItem {
        AssignmentListItem {
            id: assignment.id,
            title: assignment.title,
            deadline: assignment.deadline,
            completion: assignment.completion,
            submission: assignment.submission,
            submission_url: assignment.submission_url,
            course,
        }
    }

    #[test]
    fn empty_database_returns_an_empty_list() {
        let store = SqliteStore::open(":memory:").unwrap();
        assert!(load_assignment_list(&store).unwrap().is_empty());
    }

    #[test]
    fn discovered_courses_without_assignments_do_not_appear() {
        let mut store = SqliteStore::open(":memory:").unwrap();
        let discovered = store
            .create_course_with_source("2026-DB", vec![], OriginKind::Moodle, "feed", "2026-DB")
            .unwrap();
        store.create_course("Manual course", vec![]).unwrap();
        assert!(load_assignment_list(&store).unwrap().is_empty());
        assert_eq!(store.get_course(&discovered.id).unwrap(), Some(discovered));
    }

    #[test]
    fn unassigned_items_preserve_completion_deadlines_and_all_submission_targets() {
        let mut store = SqliteStore::open(":memory:").unwrap();
        let deadlines = [
            None,
            Some(Deadline::DateOnly(
                NaiveDate::from_ymd_opt(2026, 10, 4).unwrap(),
            )),
            Some(Deadline::DateTime(
                "2026-10-04T14:59:00.123456789Z"
                    .parse::<DateTime<Utc>>()
                    .unwrap(),
            )),
        ];
        let mut expected = Vec::new();
        for (index, submission) in [
            SubmissionTarget::Unknown,
            SubmissionTarget::None,
            SubmissionTarget::Moodle,
            SubmissionTarget::WebClass,
            SubmissionTarget::External,
        ]
        .into_iter()
        .enumerate()
        {
            let mut value = input(None);
            value.deadline = deadlines[index % deadlines.len()].clone();
            value.submission = submission;
            value.submission_url = match submission {
                SubmissionTarget::Unknown | SubmissionTarget::None => None,
                _ => Some(format!("https://submit.example.invalid/{index}")),
            };
            let mut assignment = store.create_manual_assignment(value).unwrap();
            if index == 0 {
                store
                    .set_completion(&assignment.id, CompletionState::Completed)
                    .unwrap();
                assignment.completion = CompletionState::Completed;
            }
            expected.push(expected_item(assignment, None));
        }
        expected.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        assert_eq!(load_assignment_list(&store).unwrap(), expected);
    }

    #[test]
    fn course_ids_and_origins_survive_identical_labels_without_merging() {
        let mut store = SqliteStore::open(":memory:").unwrap();
        // Identical names/schedules/source strings never establish shared identity.
        let moodle = store
            .create_course_with_source(
                "データベース",
                vec![],
                OriginKind::Moodle,
                "default",
                "same-external-id",
            )
            .unwrap();
        let webclass = store
            .create_course_with_source(
                "データベース",
                vec![],
                OriginKind::WebClass,
                "default",
                "same-external-id",
            )
            .unwrap();
        let manual = store
            .create_course_with_source(
                "データベース",
                vec![],
                OriginKind::Manual,
                "default",
                "same-external-id",
            )
            .unwrap();
        let unbound = store.create_course("データベース", vec![]).unwrap();
        assert_ne!(moodle.id, webclass.id);
        let mut expected = Vec::new();
        for (course, origin) in [
            (&moodle, Some(OriginKind::Moodle)),
            (&webclass, Some(OriginKind::WebClass)),
            (&manual, Some(OriginKind::Manual)),
            (&unbound, None),
            (&moodle, Some(OriginKind::Moodle)),
        ] {
            let assignment = store
                .create_manual_assignment(input(Some(course.id.clone())))
                .unwrap();
            // The displayed origin belongs to the course, not the assignment binding.
            store
                .bind_assignment_source(
                    &assignment.id,
                    &ExternalRecordKey {
                        origin: OriginKind::Manual,
                        scope: "manual-import".into(),
                        id: assignment.id.as_str().to_owned(),
                    },
                )
                .unwrap();
            expected.push(expected_item(
                assignment,
                Some(CourseListInfo {
                    id: course.id.clone(),
                    name: course.name.clone(),
                    origin,
                }),
            ));
        }
        expected.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        let items = load_assignment_list(&store).unwrap();
        assert_eq!(items, expected);
        let moodle_courses: Vec<_> = items
            .iter()
            .filter_map(|item| item.course.as_ref())
            .filter(|course| course.id == moodle.id)
            .collect();
        assert_eq!(moodle_courses.len(), 2);
        assert_eq!(moodle_courses[0], moodle_courses[1]);
    }

    fn date(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, day).unwrap()
    }

    fn time() -> QueryTimeContext {
        QueryTimeContext {
            now: "2026-10-03T15:30:00Z".parse().unwrap(),
            local_date: date(4),
            utc_offset: FixedOffset::east_opt(9 * 3600).unwrap(),
        }
    }

    fn create_item(
        store: &mut SqliteStore,
        title: &str,
        deadline: Option<Deadline>,
        completion: CompletionState,
        course_id: Option<CourseId>,
    ) -> Assignment {
        let mut value = input(course_id);
        value.title = title.into();
        value.deadline = deadline;
        let mut assignment = store.create_manual_assignment(value).unwrap();
        store.set_completion(&assignment.id, completion).unwrap();
        assignment.completion = completion;
        assignment
    }

    fn titles(items: &[AssignmentListItem]) -> Vec<&str> {
        items.iter().map(|item| item.title.as_str()).collect()
    }

    fn assert_selected_titles(items: &[AssignmentListItem], expected: &[&str]) {
        let mut actual = titles(items);
        let mut expected = expected.to_vec();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
    }

    #[test]
    fn query_defaults_include_all_states_and_empty_databases() {
        let mut store = SqliteStore::open(":memory:").unwrap();
        let mut options = AssignmentListOptions::default();
        assert!(
            query_assignments(&store, &options, time())
                .unwrap()
                .is_empty()
        );
        assert_eq!(options.sort, AssignmentSort::ByDeadline);
        for (title, state) in [
            ("incomplete", CompletionState::Incomplete),
            ("completed", CompletionState::Completed),
        ] {
            create_item(&mut store, title, None, state, None);
        }
        assert_selected_titles(
            &query_assignments(&store, &options, time()).unwrap(),
            &["incomplete", "completed"],
        );
        for (title, state) in [
            ("incomplete", CompletionState::Incomplete),
            ("completed", CompletionState::Completed),
        ] {
            options.completion = CompletionFilter::State(state);
            assert_selected_titles(
                &query_assignments(&store, &options, time()).unwrap(),
                &[title],
            );
        }
    }

    #[test]
    fn course_filters_use_exact_ids_and_intersect_with_completion() {
        let mut store = SqliteStore::open(":memory:").unwrap();
        let moodle = store
            .create_course_with_source("Same label", vec![], OriginKind::Moodle, "feed", "key")
            .unwrap();
        let webclass = store
            .create_course_with_source("Same label", vec![], OriginKind::WebClass, "default", "key")
            .unwrap();
        for (title, course_id, state) in [
            (
                "moodle-incomplete",
                Some(moodle.id.clone()),
                CompletionState::Incomplete,
            ),
            (
                "moodle-completed",
                Some(moodle.id.clone()),
                CompletionState::Completed,
            ),
            (
                "webclass",
                Some(webclass.id.clone()),
                CompletionState::Incomplete,
            ),
            ("unassigned", None, CompletionState::Incomplete),
        ] {
            create_item(&mut store, title, None, state, course_id);
        }
        let mut options = AssignmentListOptions::default();
        assert_eq!(
            query_assignments(&store, &options, time()).unwrap().len(),
            4
        );
        for (filter, expected) in [
            (
                CourseFilter::Course(moodle.id.clone()),
                vec!["moodle-incomplete", "moodle-completed"],
            ),
            (CourseFilter::Course(webclass.id), vec!["webclass"]),
            (CourseFilter::Unassigned, vec!["unassigned"]),
            (CourseFilter::Course(CourseId::new("missing")), vec![]),
        ] {
            options.course = filter;
            assert_selected_titles(
                &query_assignments(&store, &options, time()).unwrap(),
                &expected,
            );
        }
        options.course = CourseFilter::Course(moodle.id);
        options.completion = CompletionFilter::State(CompletionState::Completed);
        assert_selected_titles(
            &query_assignments(&store, &options, time()).unwrap(),
            &["moodle-completed"],
        );
    }

    #[test]
    fn overdue_is_strictly_past_and_incomplete_with_no_midnight_expiry() {
        let mut store = SqliteStore::open(":memory:").unwrap();
        let reference = time();
        for (title, deadline, completion) in [
            (
                "yesterday",
                Some(Deadline::DateOnly(date(3))),
                CompletionState::Incomplete,
            ),
            (
                "today",
                Some(Deadline::DateOnly(date(4))),
                CompletionState::Incomplete,
            ),
            (
                "tomorrow",
                Some(Deadline::DateOnly(date(5))),
                CompletionState::Incomplete,
            ),
            (
                "before",
                Some(Deadline::DateTime(
                    reference.now - chrono::Duration::nanoseconds(1),
                )),
                CompletionState::Incomplete,
            ),
            (
                "now",
                Some(Deadline::DateTime(reference.now)),
                CompletionState::Incomplete,
            ),
            (
                "after",
                Some(Deadline::DateTime(
                    reference.now + chrono::Duration::nanoseconds(1),
                )),
                CompletionState::Incomplete,
            ),
            (
                "completed-date",
                Some(Deadline::DateOnly(date(3))),
                CompletionState::Completed,
            ),
            (
                "completed-instant",
                Some(Deadline::DateTime(
                    reference.now - chrono::Duration::days(1),
                )),
                CompletionState::Completed,
            ),
            ("no-deadline", None, CompletionState::Incomplete),
        ] {
            create_item(&mut store, title, deadline, completion, None);
        }
        let mut options = AssignmentListOptions {
            deadline: DeadlineFilter::Overdue,
            ..Default::default()
        };
        assert_selected_titles(
            &query_assignments(&store, &options, reference).unwrap(),
            &["yesterday", "before"],
        );
        options.completion = CompletionFilter::State(CompletionState::Incomplete);
        assert_eq!(
            query_assignments(&store, &options, reference)
                .unwrap()
                .len(),
            2
        );
        options.completion = CompletionFilter::State(CompletionState::Completed);
        assert!(
            query_assignments(&store, &options, reference)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn date_only_uses_the_supplied_local_date_when_utc_is_previous_or_next_day() {
        let mut store = SqliteStore::open(":memory:").unwrap();
        for (title, day) in [("yesterday", 3), ("today", 4), ("tomorrow", 5)] {
            create_item(
                &mut store,
                title,
                Some(Deadline::DateOnly(date(day))),
                CompletionState::Incomplete,
                None,
            );
        }
        for reference in [
            time(),
            QueryTimeContext {
                now: "2026-10-05T01:00:00Z".parse().unwrap(),
                local_date: date(4),
                utc_offset: FixedOffset::west_opt(9 * 3600).unwrap(),
            },
        ] {
            assert_ne!(reference.now.date_naive(), reference.local_date);
            let mut options = AssignmentListOptions {
                deadline: DeadlineFilter::Overdue,
                ..Default::default()
            };
            assert_selected_titles(
                &query_assignments(&store, &options, reference).unwrap(),
                &["yesterday"],
            );
            let until = QueryTimeContext {
                now: reference.now + chrono::Duration::hours(1),
                ..reference
            };
            options.deadline = DeadlineFilter::DueWithin {
                from: reference,
                until,
            };
            assert_selected_titles(
                &query_assignments(&store, &options, reference).unwrap(),
                &["today"],
            );
        }
    }

    #[test]
    fn due_within_has_inclusive_separate_date_and_instant_bounds() {
        let mut store = SqliteStore::open(":memory:").unwrap();
        let from = time();
        let until = QueryTimeContext {
            now: from.now + chrono::Duration::days(7),
            local_date: date(11),
            ..from
        };
        for (title, deadline, completion) in [
            (
                "date-before",
                Some(Deadline::DateOnly(date(3))),
                CompletionState::Incomplete,
            ),
            (
                "date-from",
                Some(Deadline::DateOnly(date(4))),
                CompletionState::Incomplete,
            ),
            (
                "date-until",
                Some(Deadline::DateOnly(date(11))),
                CompletionState::Completed,
            ),
            (
                "date-after",
                Some(Deadline::DateOnly(date(12))),
                CompletionState::Incomplete,
            ),
            (
                "instant-before",
                Some(Deadline::DateTime(
                    from.now - chrono::Duration::nanoseconds(1),
                )),
                CompletionState::Incomplete,
            ),
            (
                "instant-from",
                Some(Deadline::DateTime(from.now)),
                CompletionState::Incomplete,
            ),
            (
                "instant-until",
                Some(Deadline::DateTime(until.now)),
                CompletionState::Incomplete,
            ),
            (
                "instant-after",
                Some(Deadline::DateTime(
                    until.now + chrono::Duration::nanoseconds(1),
                )),
                CompletionState::Incomplete,
            ),
            ("no-deadline", None, CompletionState::Incomplete),
        ] {
            create_item(&mut store, title, deadline, completion, None);
        }
        let mut options = AssignmentListOptions {
            deadline: DeadlineFilter::DueWithin { from, until },
            ..Default::default()
        };
        assert_selected_titles(
            &query_assignments(&store, &options, from).unwrap(),
            &["date-from", "date-until", "instant-from", "instant-until"],
        );
        options.completion = CompletionFilter::State(CompletionState::Incomplete);
        assert_selected_titles(
            &query_assignments(&store, &options, from).unwrap(),
            &["date-from", "instant-from", "instant-until"],
        );
        options.completion = CompletionFilter::All;
        options.deadline = DeadlineFilter::DueWithin { from, until: from };
        assert_selected_titles(
            &query_assignments(&store, &options, from).unwrap(),
            &["date-from", "instant-from"],
        );
        for (from, until) in [
            (until, from),
            (
                from,
                QueryTimeContext {
                    now: from.now - chrono::Duration::seconds(1),
                    ..until
                },
            ),
            (
                from,
                QueryTimeContext {
                    local_date: date(3),
                    ..until
                },
            ),
        ] {
            options.deadline = DeadlineFilter::DueWithin { from, until };
            assert!(
                query_assignments(&store, &options, time())
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn default_deadline_sort_uses_local_days_exact_times_then_date_only_then_missing() {
        let mut store = SqliteStore::open(":memory:").unwrap();
        for (title, deadline) in [
            ("no-deadline", None),
            ("next-date", Some(Deadline::DateOnly(date(5)))),
            ("today-date", Some(Deadline::DateOnly(date(4)))),
            (
                "today-late",
                Some(Deadline::DateTime("2026-10-04T14:00:00Z".parse().unwrap())),
            ),
            ("previous-date", Some(Deadline::DateOnly(date(3)))),
            (
                "today-early",
                Some(Deadline::DateTime("2026-10-03T15:00:00Z".parse().unwrap())),
            ),
            (
                "previous-instant",
                Some(Deadline::DateTime("2026-10-03T14:59:59Z".parse().unwrap())),
            ),
            (
                "next-instant",
                Some(Deadline::DateTime("2026-10-04T15:00:00Z".parse().unwrap())),
            ),
        ] {
            create_item(
                &mut store,
                title,
                deadline,
                CompletionState::Incomplete,
                None,
            );
        }
        let before = load_assignment_list(&store).unwrap();
        let items = query_assignments(&store, &AssignmentListOptions::default(), time()).unwrap();
        assert_eq!(
            titles(&items),
            [
                "previous-instant",
                "previous-date",
                "today-early",
                "today-late",
                "today-date",
                "next-instant",
                "next-date",
                "no-deadline"
            ]
        );
        assert_eq!(load_assignment_list(&store).unwrap(), before);
    }

    #[test]
    fn all_sort_modes_break_equal_keys_by_assignment_id() {
        let mut store = SqliteStore::open(":memory:").unwrap();
        let mut expected = Vec::new();
        for _ in 0..4 {
            expected.push(
                create_item(
                    &mut store,
                    "Same title",
                    Some(Deadline::DateOnly(date(4))),
                    CompletionState::Incomplete,
                    None,
                )
                .id,
            );
        }
        expected.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        for sort in [
            AssignmentSort::ByDeadline,
            AssignmentSort::ByCourse,
            AssignmentSort::ByTitle,
        ] {
            let options = AssignmentListOptions {
                sort,
                ..Default::default()
            };
            let items = query_assignments(&store, &options, time()).unwrap();
            assert_eq!(
                items.iter().map(|item| &item.id).collect::<Vec<_>>(),
                expected.iter().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn course_sort_keeps_same_name_courses_separate_and_unassigned_last() {
        let mut store = SqliteStore::open(":memory:").unwrap();
        let moodle = store
            .create_course_with_source("Same", vec![], OriginKind::Moodle, "feed", "key")
            .unwrap();
        let webclass = store
            .create_course_with_source("Same", vec![], OriginKind::WebClass, "default", "key")
            .unwrap();
        let early = store.create_course("A", vec![]).unwrap();
        let late = store.create_course("Z", vec![]).unwrap();
        for course_id in [
            None,
            Some(late.id.clone()),
            Some(moodle.id.clone()),
            Some(webclass.id.clone()),
            Some(early.id.clone()),
            Some(moodle.id.clone()),
        ] {
            create_item(
                &mut store,
                "Same title",
                None,
                CompletionState::Incomplete,
                course_id,
            );
        }
        let options = AssignmentListOptions {
            sort: AssignmentSort::ByCourse,
            ..Default::default()
        };
        let items = query_assignments(&store, &options, time()).unwrap();
        let mut same_ids = [moodle.id.clone(), moodle.id, webclass.id];
        same_ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let expected = vec![
            Some(early.id),
            Some(same_ids[0].clone()),
            Some(same_ids[1].clone()),
            Some(same_ids[2].clone()),
            Some(late.id),
            None,
        ];
        assert_eq!(
            items
                .iter()
                .map(|item| item.course.as_ref().map(|course| course.id.clone()))
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            items
                .iter()
                .filter_map(|item| item.course.as_ref())
                .filter(|course| course.origin == Some(OriginKind::Moodle))
                .count(),
            2
        );
    }

    #[test]
    fn title_sort_uses_rust_unicode_string_order() {
        let mut store = SqliteStore::open(":memory:").unwrap();
        for title in ["課題", "a", "é", "A", " Report", "レポート"] {
            create_item(&mut store, title, None, CompletionState::Incomplete, None);
        }
        let options = AssignmentListOptions {
            sort: AssignmentSort::ByTitle,
            ..Default::default()
        };
        assert_eq!(
            titles(&query_assignments(&store, &options, time()).unwrap()),
            [" Report", "A", "a", "é", "レポート", "課題"]
        );
    }

    #[test]
    fn mixed_deadline_comparison_handles_extreme_instants_and_negative_offsets() {
        for (instant, calendar_date, offset, expected) in [
            (
                DateTime::<Utc>::MIN_UTC,
                NaiveDate::MIN,
                FixedOffset::west_opt(3600).unwrap(),
                Ordering::Less,
            ),
            (
                DateTime::<Utc>::MAX_UTC,
                NaiveDate::MAX,
                FixedOffset::east_opt(3600).unwrap(),
                Ordering::Greater,
            ),
            (
                "2026-10-05T01:00:00Z".parse().unwrap(),
                date(4),
                FixedOffset::west_opt(9 * 3600).unwrap(),
                Ordering::Less,
            ),
        ] {
            let exact = Deadline::DateTime(instant);
            let day = Deadline::DateOnly(calendar_date);
            assert_eq!(
                compare_deadlines(Some(&exact), Some(&day), offset),
                expected
            );
            assert_eq!(
                compare_deadlines(Some(&day), Some(&exact), offset),
                expected.reverse()
            );
        }
    }
}
