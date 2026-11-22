//! Windows-only, in-memory review of the validated Moodle acquisition.

mod windows;

use std::collections::{BTreeMap, HashMap};

use assignment_manager::{domain::Deadline, moodle::MoodleEventKind, sync::MoodleAcquisition};
use chrono::FixedOffset;

pub(super) use windows::confirm_fetch;

pub(super) fn review(acquired: MoodleAcquisition) -> bool {
    let mut categories = Categories::from_acquisition(&acquired);
    categories.print_totals();
    let action = windows::show_categories(&mut categories);
    categories.print_mapping_totals();
    match action {
        Ok(CategoryAction::Finish) => return true,
        Ok(CategoryAction::ReviewAssignments) => (),
        Err(()) => {
            println!("review diagnostic=category_window_unavailable");
            return false;
        }
    }

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

struct Categories {
    rows: Vec<Category>,
    unclassified: usize,
}

struct Category {
    name: String,
    candidates: usize,
    choice: CategoryChoice,
}

enum CategoryAction {
    Finish,
    ReviewAssignments,
}

#[derive(Clone, Copy)]
#[repr(usize)]
enum CategoryChoice {
    Pending,
    Existing,
    NewPage,
    Ambiguous,
}

const CATEGORY_CHOICES: [(CategoryChoice, &str); 4] = [
    (CategoryChoice::Pending, "未確認"),
    (CategoryChoice::Existing, "正しい検証用授業ページがある"),
    (CategoryChoice::NewPage, "新しい検証用授業ページが必要"),
    (CategoryChoice::Ambiguous, "対応が曖昧"),
];

#[derive(Debug, Default, PartialEq, Eq)]
struct CategoryCounts {
    pending: usize,
    existing: usize,
    new_pages: usize,
    ambiguous: usize,
}

impl Categories {
    fn from_acquisition(acquired: &MoodleAcquisition) -> Self {
        Self::from_names(
            acquired
                .assignments()
                .batch()
                .as_slice()
                .iter()
                .map(|item| {
                    acquired
                        .course_for(&item.external_key)
                        .map(|course| course.name.as_str())
                }),
        )
    }

    fn from_names<'a>(names: impl IntoIterator<Item = Option<&'a str>>) -> Self {
        let mut groups = BTreeMap::new();
        let mut unclassified = 0;
        for name in names {
            if let Some(name) = name {
                *groups.entry(name).or_insert(0) += 1;
            } else {
                unclassified += 1;
            }
        }
        let rows = groups
            .into_iter()
            .map(|(name, candidates)| Category {
                name: name.to_owned(),
                candidates,
                choice: CategoryChoice::Pending,
            })
            .collect();
        Self { rows, unclassified }
    }

    fn classified(&self) -> usize {
        self.rows.iter().map(|row| row.candidates).sum()
    }

    fn mapping_counts(&self) -> CategoryCounts {
        let mut counts = CategoryCounts::default();
        for row in &self.rows {
            match row.choice {
                CategoryChoice::Pending => counts.pending += 1,
                CategoryChoice::Existing => counts.existing += 1,
                CategoryChoice::NewPage => counts.new_pages += 1,
                CategoryChoice::Ambiguous => counts.ambiguous += 1,
            }
        }
        counts
    }

    fn print_totals(&self) {
        let classified = self.classified();
        println!(
            "moodle_categories candidates={} distinct={} classified={classified} unclassified={}",
            classified + self.unclassified,
            self.rows.len(),
            self.unclassified,
        );
        // Print the distribution in descending order, independent of the category names.
        let mut sizes: Vec<_> = self.rows.iter().map(|row| row.candidates).collect();
        sizes.sort_unstable_by(|a, b| b.cmp(a));
        let distribution: Vec<_> = sizes.iter().map(usize::to_string).collect();
        println!("moodle_category_sizes counts=[{}]", distribution.join(","));
    }

    fn print_mapping_totals(&self) {
        let counts = self.mapping_counts();
        println!(
            "moodle_category_mapping existing={} new_pages={} ambiguous={} pending={}",
            counts.existing, counts.new_pages, counts.ambiguous, counts.pending,
        );
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
    fn categories_group_exact_values_and_track_mapping_choices_separately() {
        let mut categories = Categories::from_names([
            Some("ゼミ"),
            Some("ゼミ"),
            Some("ゼミＡ"),
            Some("ゼミA"),
            Some("講義"),
            None,
        ]);
        assert_eq!(categories.rows.len(), 4);
        assert_eq!(categories.classified(), 5);
        assert_eq!(categories.unclassified, 1);
        assert_eq!(categories.rows[0].name, "ゼミ");
        assert_eq!(categories.rows[0].candidates, 2);
        categories.rows[0].choice = CategoryChoice::Existing;
        categories.rows[1].choice = CategoryChoice::NewPage;
        categories.rows[2].choice = CategoryChoice::Ambiguous;
        assert_eq!(
            categories.mapping_counts(),
            CategoryCounts {
                pending: 1,
                existing: 1,
                new_pages: 1,
                ambiguous: 1,
            }
        );
    }

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
        let categories = Categories::from_acquisition(&acquired);
        let review = Review::from_acquisition(&acquired).unwrap();
        assert_eq!(university.requests(), 1);
        assert_eq!(categories.rows.len(), 2);
        assert_eq!(categories.classified(), 3);
        assert_eq!(categories.unclassified, 0);
        assert!(categories.rows.iter().any(|row| row.candidates == 2));
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
