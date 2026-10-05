//! Phase 10B synthetic fixture. Creates only its own temporary DB; no input paths.
use assignment_manager::{
    domain::{CompletionState, Deadline, SubmissionTarget},
    import::OriginKind,
    persistence::{ManualAssignmentInput, SqliteStore},
};
use chrono::{Duration, Local};

fn main() -> Result<(), &'static str> {
    if std::env::args_os().len() != 1 {
        return Err("fixture generator accepts no arguments");
    }
    let root = tempfile::tempdir().map_err(|_| "fixture directory unavailable")?;
    let mut store = SqliteStore::open(root.path().join("assignment-manager.db"))
        .map_err(|_| "fixture database unavailable")?;
    let moodle = store
        .create_course_with_source(
            "情報基礎",
            vec![],
            OriginKind::Moodle,
            "smoke",
            "moodle-test",
        )
        .map_err(|_| "fixture course unavailable")?;
    let webclass = store
        .create_course_with_source(
            "情報基礎",
            vec![],
            OriginKind::WebClass,
            "smoke",
            "webclass-test",
        )
        .map_err(|_| "fixture course unavailable")?;
    let manual = store
        .create_course("自主学習", vec![])
        .map_err(|_| "fixture course unavailable")?;
    let now = Local::now();
    for (title, course, deadline, submission, completed) in [
        (
            "01 レポート（期限切れ）",
            Some(moodle.id.clone()),
            Some(Deadline::DateOnly(now.date_naive() - Duration::days(1))),
            SubmissionTarget::Moodle,
            false,
        ),
        (
            "02 本日の課題",
            Some(moodle.id),
            Some(Deadline::DateOnly(now.date_naive())),
            SubmissionTarget::Moodle,
            false,
        ),
        (
            "03 小テスト（日時）",
            Some(webclass.id.clone()),
            Some(Deadline::DateTime((now + Duration::hours(2)).to_utc())),
            SubmissionTarget::WebClass,
            false,
        ),
        (
            "04 来週の課題",
            Some(webclass.id.clone()),
            Some(Deadline::DateOnly(now.date_naive() + Duration::days(3))),
            SubmissionTarget::WebClass,
            false,
        ),
        (
            "05 資料を読む",
            Some(webclass.id),
            None,
            SubmissionTarget::None,
            false,
        ),
        (
            "06 完了済みの復習",
            Some(manual.id.clone()),
            Some(Deadline::DateOnly(now.date_naive() - Duration::days(2))),
            SubmissionTarget::External,
            true,
        ),
        (
            "07 長い課題タイトルの表示を確認するための自主学習・参考資料を読み、考察をまとめて次回の授業に備える",
            Some(manual.id),
            None,
            SubmissionTarget::Unknown,
            false,
        ),
        (
            "08 未所属の提出課題",
            None,
            Some(Deadline::DateTime((now + Duration::days(1)).to_utc())),
            SubmissionTarget::External,
            false,
        ),
    ] {
        let assignment = store
            .create_manual_assignment(ManualAssignmentInput {
                title: title.into(),
                course_id: course,
                deadline,
                submission,
                submission_url: None,
            })
            .map_err(|_| "fixture assignment unavailable")?;
        if completed {
            store
                .set_completion(&assignment.id, CompletionState::Completed)
                .map_err(|_| "fixture completion unavailable")?;
        }
    }
    drop(store);
    println!("{}", root.keep().display());
    Ok(())
}
