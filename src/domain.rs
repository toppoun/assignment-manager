use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

/// An application ID, independent of identifiers supplied by external services.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentId(String);

impl AssignmentId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An application ID, independent of identifiers supplied by external services.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CourseId(String);

impl CourseId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Deadline {
    DateOnly(NaiveDate),
    DateTime(DateTime<Utc>),
}

/// The user's local completion state; external submission and grading states are separate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompletionState {
    Incomplete,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubmissionTarget {
    /// The submission destination has not been determined.
    Unknown,
    /// It is known that no submission destination or action is required.
    None,
    Moodle,
    WebClass,
    External,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub id: AssignmentId,
    pub course_id: Option<CourseId>,
    pub title: String,
    pub deadline: Option<Deadline>,
    pub submission: SubmissionTarget,
    /// The actual page where the user can submit or answer, not a source detail page.
    pub submission_url: Option<String>,
    pub completion: CompletionState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Course {
    pub id: CourseId,
    pub name: String,
    pub schedule: Vec<ScheduleEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassSlot {
    pub day: DayOfWeek,
    pub period: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScheduleEntry {
    Timetabled(ClassSlot),
    OnDemand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DayOfWeek {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_only_deadline_keeps_a_calendar_date() {
        let date = NaiveDate::from_ymd_opt(2026, 10, 2).unwrap();
        let deadline = Deadline::DateOnly(date);

        assert!(matches!(deadline, Deadline::DateOnly(value) if value == date));
    }

    #[test]
    fn datetime_deadline_keeps_an_exact_utc_instant() {
        let instant = DateTime::parse_from_rfc3339("2026-10-02T23:59:00+09:00")
            .unwrap()
            .with_timezone(&Utc);
        let deadline = Deadline::DateTime(instant);

        assert!(matches!(deadline, Deadline::DateTime(value) if value == instant));
        assert_eq!(instant.to_rfc3339(), "2026-10-02T14:59:00+00:00");
    }

    #[test]
    fn unknown_and_no_submission_are_distinct() {
        assert_ne!(SubmissionTarget::Unknown, SubmissionTarget::None);
    }

    #[test]
    fn completion_is_independent_of_submission_target() {
        let mut assignment = Assignment {
            id: AssignmentId::new("assignment-1"),
            course_id: None,
            title: "Essay".into(),
            deadline: None,
            submission: SubmissionTarget::Moodle,
            submission_url: None,
            completion: CompletionState::Incomplete,
        };

        assert_eq!(assignment.completion, CompletionState::Incomplete);
        assignment.completion = CompletionState::Completed;
        assert_eq!(assignment.completion, CompletionState::Completed);
        assert_eq!(assignment.submission, SubmissionTarget::Moodle);
    }

    #[test]
    fn assignment_and_course_can_be_constructed() {
        let course = Course {
            id: CourseId::new("course-1"),
            name: "Mathematics".into(),
            schedule: vec![ScheduleEntry::Timetabled(ClassSlot {
                day: DayOfWeek::Monday,
                period: 2,
            })],
        };
        let assignment = Assignment {
            id: AssignmentId::new("assignment-1"),
            course_id: Some(course.id.clone()),
            title: "Problem set".into(),
            deadline: None,
            submission: SubmissionTarget::Unknown,
            submission_url: None,
            completion: CompletionState::Incomplete,
        };

        assert_eq!(assignment.course_id.as_ref(), Some(&course.id));
        assert_eq!(
            course.schedule[0],
            ScheduleEntry::Timetabled(ClassSlot {
                day: DayOfWeek::Monday,
                period: 2,
            })
        );
    }

    #[test]
    fn course_can_be_on_demand() {
        let course = Course {
            id: CourseId::new("course-ondemand"),
            name: "On-demand course".into(),
            schedule: vec![ScheduleEntry::OnDemand],
        };

        assert_eq!(course.schedule, vec![ScheduleEntry::OnDemand]);
    }
}
