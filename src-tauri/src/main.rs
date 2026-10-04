#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

mod commands;
mod dto;
mod error;
mod list_query;
mod storage;

use error::ApplicationError;
use storage::DesktopState;
use tauri::Manager;

fn main() {
    let result = tauri::Builder::default()
        .setup(|app| {
            // Resolve once through the OS/Tauri API. No environment-variable paths.
            let directory = app
                .path()
                .app_data_dir()
                .map_err(|_| ApplicationError::DataDirectory);
            // Creation, open/migration and all later SQLite work run on one worker.
            // Commands wait for startup without blocking the window/event thread.
            app.manage(DesktopState::start(directory)?);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_assignments,
            commands::toggle_assignment_completion
        ])
        .run(tauri::generate_context!());

    if result.is_err() {
        // Tauri's underlying startup error can include paths; never print it.
        eprintln!("アプリを起動できませんでした。");
        std::process::exit(1);
    }
}
