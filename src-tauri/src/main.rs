#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

mod commands;
mod credential_store;
mod dto;
mod error;
mod list_query;
mod settings;
mod settings_worker;
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
            let config_directory = app
                .path()
                .app_config_dir()
                .map_err(|_| ApplicationError::SettingsReadFailed);
            app.manage(settings_worker::SettingsState::start(
                config_directory,
                "io.assignment-manager.desktop",
            )?);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_assignments,
            commands::toggle_assignment_completion,
            commands::get_settings_status,
            commands::save_moodle_feed,
            commands::remove_moodle_feed,
            commands::save_webclass_credentials,
            commands::remove_webclass_credentials
        ])
        .run(tauri::generate_context!());

    if result.is_err() {
        // Tauri's underlying startup error can include paths; never print it.
        eprintln!("アプリを起動できませんでした。");
        std::process::exit(1);
    }
}
