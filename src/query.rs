//! Read-only application queries for local assignment display.
//!
//! Callers receive domain values and course display information without reading
//! SQLite tables or interpreting source identities. No sync metadata is included.

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
}
