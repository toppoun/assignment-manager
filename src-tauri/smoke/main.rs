//! A separate, synthetic-data app for manual UI smoke tests; no production DB override.
#![forbid(unsafe_code)]

#[path = "../src/commands.rs"]
mod commands;
#[path = "../src/dto.rs"]
mod dto;
#[path = "../src/error.rs"]
mod error;
#[path = "../src/list_query.rs"]
mod list_query;
#[path = "../src/storage.rs"]
mod storage;

use std::path::PathBuf;

use assignment_manager::{
    actions::toggle_assignment_completion,
    domain::{Deadline, SubmissionTarget},
    import::OriginKind,
    persistence::{ManualAssignmentInput, SqliteStore},
};
use chrono::{Days, Duration, Local, Utc};
use tauri::{Manager, utils::config::AppDirectoriesOverride};

fn seed(directory: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    // This executable has its own fixed DB path and explicit test root. Reopen preserves toggles.
    let path = directory.join("data").join("assignment-manager.db");
    if path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().ok_or("invalid smoke directory")?)?;
    let mut store = SqliteStore::open(path)?;
    let moodle = store.create_course_with_source(
        "データベース",
        vec![],
        OriginKind::Moodle,
        "smoke",
        "synthetic-moodle",
    )?;
    let webclass = store.create_course_with_source(
        "データベース",
        vec![],
        OriginKind::WebClass,
        "default",
        "synthetic-webclass",
    )?;
    let manual = store.create_course("線形代数", vec![])?;
    let local = Local::now();
    let today = local.date_naive();
    let now = local.with_timezone(&Utc);
    for (title, course_id, deadline, completed) in [
        (
            "SQL演習レポート",
            Some(moodle.id.clone()),
            Some(Deadline::DateOnly(today - Days::new(1))),
            false,
        ),
        (
            "正規化の演習",
            Some(moodle.id),
            Some(Deadline::DateOnly(today + Days::new(3))),
            false,
        ),
        (
            "データモデルの小テスト",
            Some(webclass.id.clone()),
            Some(Deadline::DateOnly(today)),
            false,
        ),
        (
            "ER図の提出",
            Some(webclass.id.clone()),
            Some(Deadline::DateTime(now + Duration::hours(2))),
            false,
        ),
        (
            "提出済みのレポート",
            Some(webclass.id),
            Some(Deadline::DateOnly(today - Days::new(2))),
            true,
        ),
        ("行列の復習", Some(manual.id), None, false),
        ("学習メモ", None, None, false),
    ] {
        let assignment = store.create_manual_assignment(ManualAssignmentInput {
            title: title.into(),
            course_id,
            deadline,
            submission: SubmissionTarget::Unknown,
            submission_url: None,
        })?;
        if completed {
            toggle_assignment_completion(&mut store, &assignment.id)?;
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("provide an absolute, dedicated smoke-test directory")?;
    seed(&directory)?;
    let mut context = tauri::generate_context!();
    context.config_mut().identifier = "io.assignment-manager.list-smoke".into();
    // DB and webview data both stay in the supplied test root, outside production data.
    context.config_mut().app.app_directories_override =
        Some(AppDirectoriesOverride::Root(directory.join("data")));
    context.config_mut().app.windows[0].title = "Assignment Manager — Smoke".into();
    tauri::Builder::default()
        .setup(|app| {
            let directory = app
                .path()
                .app_data_dir()
                .map_err(|_| error::ApplicationError::DataDirectory);
            app.manage(storage::DesktopState::start(directory)?);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_assignments,
            commands::toggle_assignment_completion
        ])
        .run(context)?;
    Ok(())
}
