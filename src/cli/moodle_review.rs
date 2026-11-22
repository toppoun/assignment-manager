//! Windows-only, in-memory review of the validated Moodle acquisition.

mod windows;

use std::collections::HashMap;

use assignment_manager::{domain::Deadline, moodle::MoodleEventKind, sync::MoodleAcquisition};
use chrono::FixedOffset;

pub(super) use windows::confirm_fetch;

pub(super) fn review(acquired: MoodleAcquisition) -> bool {
    let Some(mut review) = Review::from_acquisition(&acquired) else {
        println!("review diagnostic=review_metadata_invalid");
        return false;
    };
    drop(acquired);
    match windows::show(&mut review) {
        Ok(Some(counts)) => {
            println!(
                "review result=completed matched={} extra={} missing={} duplicates={} deadline_diff={} course_diff={} uid_duplicates=0 potential_repeated_activity_groups={}",
                counts.matched,
                counts.extra,
                counts.missing,
                counts.duplicates,
                counts.deadline_diff,
                counts.course_diff,
                review.repeated_groups,
            );
            true
        }
        Ok(None) => {
            println!("review result=cancelled");
            false
        }
        Err(()) => {
            println!("review diagnostic=review_window_unavailable");
            false
        }
    }
}

struct Row {
    course: Option<String>,
    activity: String,
    kind: MoodleEventKind,
    deadline_jst: String,
    possible_activity_events: usize,
    choice: Choice,
}

impl Row {
    fn kind_label(&self) -> &'static str {
        match self.kind {
            MoodleEventKind::SubmissionDeadline => "提出期限",
            MoodleEventKind::AvailabilityEnd => "小テストの受験可能期間終了",
            _ => unreachable!("review rows contain validated deadline kinds"),
        }
    }
}

struct Review {
    rows: Vec<Row>,
    repeated_groups: usize,
}

impl Review {
    fn from_acquisition(acquired: &MoodleAcquisition) -> Option<Self> {
        let jst = FixedOffset::east_opt(9 * 60 * 60)?;
        let items = acquired.assignments().batch().as_slice();
        let mut groups = HashMap::new();
        for item in items {
            if let Some(course) = acquired.course_for(&item.external_key) {
                *groups
                    .entry((course.name.as_str(), item.title.as_str()))
                    .or_insert(0) += 1;
            }
        }
        let rows = items
            .iter()
            .map(|item| {
                let kind = acquired.event_kind_for(&item.external_key)?;
                if !matches!(
                    kind,
                    MoodleEventKind::SubmissionDeadline | MoodleEventKind::AvailabilityEnd
                ) {
                    return None;
                }
                let Some(Deadline::DateTime(deadline)) = &item.deadline else {
                    return None;
                };
                let course = acquired
                    .course_for(&item.external_key)
                    .map(|course| course.name.as_str());
                Some(Row {
                    course: course.map(str::to_owned),
                    activity: item.title.clone(),
                    kind,
                    deadline_jst: deadline
                        .with_timezone(&jst)
                        .format("%Y-%m-%d %H:%M:%S JST")
                        .to_string(),
                    possible_activity_events: course
                        .and_then(|course| groups.get(&(course, item.title.as_str())).copied())
                        .unwrap_or(1),
                    choice: Choice::Pending,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            rows,
            repeated_groups: groups.values().filter(|&&size| size > 1).count(),
        })
    }

    fn counts(&self, missing: usize) -> Counts {
        let mut counts = Counts {
            missing,
            ..Counts::default()
        };
        for row in &self.rows {
            match row.choice {
                Choice::Pending => counts.pending += 1,
                Choice::Matched => counts.matched += 1,
                Choice::Extra => counts.extra += 1,
                Choice::Duplicate => counts.duplicates += 1,
                Choice::Deadline => counts.deadline_diff += 1,
                Choice::Course => counts.course_diff += 1,
                Choice::DeadlineAndCourse => {
                    counts.deadline_diff += 1;
                    counts.course_diff += 1;
                }
            }
        }
        counts
    }

    fn finish(&self, missing: Option<usize>) -> Option<Counts> {
        let counts = self.counts(missing?);
        (counts.pending == 0).then_some(counts)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
enum Choice {
    Pending,
    Matched,
    Extra,
    Duplicate,
    Deadline,
    Course,
    DeadlineAndCourse,
}

const CHOICES: [(Choice, &str); 7] = [
    (Choice::Pending, "未確認"),
    (Choice::Matched, "一致"),
    (Choice::Extra, "候補余り"),
    (Choice::Duplicate, "同一活動の重複"),
    (Choice::Deadline, "期限差"),
    (Choice::Course, "授業差"),
    (Choice::DeadlineAndCourse, "期限差と授業差"),
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    pending: usize,
    matched: usize,
    extra: usize,
    missing: usize,
    duplicates: usize,
    deadline_diff: usize,
    course_diff: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_requires_a_choice_for_each_row_and_an_explicit_missing_count() {
        let mut review = Review {
            rows: CHOICES
                .iter()
                .map(|(choice, _)| Row {
                    course: None,
                    activity: String::new(),
                    kind: MoodleEventKind::SubmissionDeadline,
                    deadline_jst: String::new(),
                    possible_activity_events: 1,
                    choice: *choice,
                })
                .collect(),
            repeated_groups: 0,
        };
        assert_eq!(review.finish(Some(2)), None);
        review.rows.remove(0);
        assert_eq!(review.finish(None), None);
        assert_eq!(
            review.finish(Some(2)),
            Some(Counts {
                pending: 0,
                matched: 1,
                extra: 1,
                missing: 2,
                duplicates: 1,
                deadline_diff: 2,
                course_diff: 2,
            })
        );
    }

    #[cfg(feature = "synthetic-https-fixture")]
    #[test]
    fn review_uses_one_acquisition_and_flags_repeated_activities_in_the_same_course() {
        use assignment_manager::{
            moodle_http::{MoodleExportKind, MoodleFeed},
            secured_sync::SecuredSyncClients,
            sync::acquire_moodle_with_diagnostic,
            webclass_auth::synthetic::{MOODLE_URL, Scenario, SyntheticUniversity},
        };

        let university = SyntheticUniversity::start(Scenario::MoodleReview);
        let mut clients = SecuredSyncClients::new(university.policy());
        clients.begin_run();
        let feed = MoodleFeed::new(
            "default.course-related",
            MoodleExportKind::CourseRelated,
            MOODLE_URL,
        )
        .unwrap();
        let acquired = acquire_moodle_with_diagnostic(&clients.moodle().unwrap(), &feed).unwrap();
        let review = Review::from_acquisition(&acquired).unwrap();
        assert_eq!(university.requests(), 1);
        assert_eq!(review.rows.len(), 3);
        assert_eq!(review.repeated_groups, 1);
        assert_eq!(review.rows[0].possible_activity_events, 2);
        assert_eq!(review.rows[1].possible_activity_events, 2);
        assert_eq!(review.rows[2].possible_activity_events, 1);
        assert_eq!(review.rows[0].kind, MoodleEventKind::SubmissionDeadline);
        assert_eq!(review.rows[1].kind, MoodleEventKind::AvailabilityEnd);
        assert_eq!(review.rows[0].deadline_jst, "2026-11-24 00:00:00 JST");
        assert_eq!(review.rows[1].deadline_jst, "2026-11-24 11:00:00 JST");
    }
}
