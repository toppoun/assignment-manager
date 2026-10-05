//! Test-only fixture generator. It never accepts an existing DB/path from a caller.
//! Swift removes the returned temporary directory after closing every proxy.
use assignment_manager::{
    domain::{Deadline, SubmissionTarget},
    import::OriginKind,
    persistence::{ManualAssignmentInput, SqliteStore},
};
use chrono::{DateTime, NaiveDate};

fn main() -> Result<(), &'static str> {
    if std::env::args_os().len() != 1 {
        return Err("fixture generator accepts no arguments");
    }
    let root = tempfile::tempdir().map_err(|_| "fixture directory unavailable")?;
    let mut store = SqliteStore::open(root.path().join("seeded.db"))
        .map_err(|_| "fixture database unavailable")?;
    let moodle = store
        .create_course_with_source(
            "Same course",
            vec![],
            OriginKind::Moodle,
            "smoke",
            "moodle-private-identity",
        )
        .map_err(|_| "fixture course failed")?;
    let webclass = store
        .create_course_with_source(
            "Same course",
            vec![],
            OriginKind::WebClass,
            "smoke",
            "webclass-private-identity",
        )
        .map_err(|_| "fixture course failed")?;
    let manual = store
        .create_course("Local course", vec![])
        .map_err(|_| "fixture course failed")?;
    for (title, course_id, deadline, submission, completed) in [
        (
            "Date only",
            Some(moodle.id),
            Some(Deadline::DateOnly(
                NaiveDate::from_ymd_opt(2030, 2, 3).unwrap(),
            )),
            SubmissionTarget::Moodle,
            false,
        ),
        (
            "Date time",
            Some(webclass.id),
            Some(Deadline::DateTime(
                DateTime::parse_from_rfc3339("2030-02-04T10:20:30.123456789Z")
                    .unwrap()
                    .to_utc(),
            )),
            SubmissionTarget::WebClass,
            false,
        ),
        (
            "Completed",
            Some(manual.id),
            Some(Deadline::DateOnly(
                NaiveDate::from_ymd_opt(2030, 2, 2).unwrap(),
            )),
            SubmissionTarget::External,
            true,
        ),
        ("No deadline", None, None, SubmissionTarget::Unknown, false),
    ] {
        let assignment = store
            .create_manual_assignment(ManualAssignmentInput {
                title: title.into(),
                course_id,
                deadline,
                submission,
                submission_url: Some(
                    "https://example.invalid/submission?token=private-submission-marker".into(),
                ),
            })
            .map_err(|_| "fixture assignment failed")?;
        if completed {
            store
                .set_completion(
                    &assignment.id,
                    assignment_manager::domain::CompletionState::Completed,
                )
                .map_err(|_| "fixture completion failed")?;
        }
    }
    drop(store);
    std::fs::write(
        root.path().join("invalid.db"),
        b"invalid-sqlite-private-cause",
    )
    .map_err(|_| "invalid fixture failed")?;
    let root = root.keep();
    println!("{}", root.display());
    Ok(())
}
