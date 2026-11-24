use std::fmt::Write as _;

use crate::{
    moodle::{MoodleEvent, parse_calendar},
    moodle_http::{MoodleExportKind, MoodleFeed},
    sync::AcquiredWebClassCourses,
    webclass::{WebClassCourse, parse_courses},
};

use super::{
    ApplyError, MOODLE_SCOPE, NotionError, SetupFixture, StateError, course_rules_target,
    start_server_with_setup,
};
use crate::notion::{CourseSetupProperties, NotionTarget, course_setup::CourseSetupInput};

pub use crate::notion::course_setup::CourseSetupCounts;

#[cfg(test)]
mod tests;

fn setup_target() -> NotionTarget {
    let mut target = course_rules_target();
    target.course_rules.as_mut().expect("fixture rules").setup = Some(CourseSetupProperties {
        group_id: "group".into(),
        year: Some("year".into()),
        day: Some("day".into()),
        period: Some("period".into()),
    });
    target
}

fn setup_sources() -> Result<(AcquiredWebClassCourses, Vec<MoodleEvent>, MoodleFeed), NotionError> {
    let records = parse_courses(include_str!(
        "../../../fixtures/webclass/courses_basic.json"
    ))
    .map_err(|_| NotionError::Configuration)?;
    let sample = records.first().ok_or(NotionError::Configuration)?;
    let courses = (1..=13)
        .map(|index| WebClassCourse {
            group_id: format!("setup-group-{index:02}"),
            group_name: format!("合成授業 {index:02}"),
            year: if index <= 12 { 2026 } else { 2027 },
            semester: if index <= 12 { "2".into() } else { "1".into() },
            ..sample.clone()
        })
        .collect();
    let courses =
        AcquiredWebClassCourses::from_records(courses).map_err(|_| NotionError::Configuration)?;
    let mut ics = "BEGIN:VCALENDAR\nVERSION:2.0\n".to_owned();
    for index in 1..=8 {
        let categories = match index {
            7 => "CATEGORIES:合成カテゴリ 01".into(),
            8 => "CATEGORIES:曖昧 A,曖昧 B".into(),
            _ => format!("CATEGORIES:合成カテゴリ {index:02}"),
        };
        // The feed may discover a course from a start event with no task candidate.
        writeln!(ics, "BEGIN:VEVENT\nUID:setup-{index}@example.invalid\nSUMMARY:合成活動の受験可能期間の開始\nDTSTART:20261201T000000Z\n{categories}\nEND:VEVENT").expect("write to String");
    }
    ics.push_str("END:VCALENDAR\n");
    let events = parse_calendar(&ics).map_err(|_| NotionError::Configuration)?;
    let feed = MoodleFeed::new(
        MOODLE_SCOPE,
        MoodleExportKind::CourseRelated,
        "https://moodle.synthetic.invalid/calendar",
    )
    .map_err(|_| NotionError::Configuration)?;
    Ok((courses, events, feed))
}

/// Use a fresh loopback Notion and temporary state. The second apply in the
/// same invocation observes the pages and state produced by the first one.
pub fn exercise_course_setup(
    term: &str,
    apply: bool,
    mut preview: impl FnMut(&'static str, CourseSetupCounts),
) -> Result<(), ApplyError> {
    let (courses, events, feed) = setup_sources()?;
    let input = CourseSetupInput::new(term, &courses, &events, &feed)?;
    let target = setup_target();
    let (client, _server) =
        start_server_with_setup(apply, Vec::new(), Some(SetupFixture::default()))
            .map_err(|_| NotionError::Configuration)?;
    if !apply {
        preview(
            "preview",
            client.preview_course_setup(&target, &input)?.counts,
        );
        return Ok(());
    }
    let state = tempfile::tempdir().map_err(|_| StateError::Io)?;
    let directory = state.path().canonicalize().map_err(|_| StateError::Io)?;
    for pass in ["initial", "repeat"] {
        client.apply_course_setup(&target, &input, &directory, |plan| {
            preview(pass, plan);
            true
        })?;
    }
    Ok(())
}
