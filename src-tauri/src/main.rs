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
mod sync_service;
mod sync_worker;

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
            // Creation, open/migration and local list/completion work run on this worker.
            // Commands wait for startup without blocking the window/event thread.
            app.manage(DesktopState::start(directory.clone())?);
            let config_directory = app
                .path()
                .app_config_dir()
                .map_err(|_| ApplicationError::SettingsReadFailed);
            let settings = settings_worker::SettingsState::start(
                config_directory,
                "io.assignment-manager.desktop",
            )?;
            app.manage(sync_worker::SyncState::start(directory, settings.clone())?);
            app.manage(settings);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_assignments,
            commands::toggle_assignment_completion,
            commands::get_settings_status,
            commands::save_moodle_feed,
            commands::remove_moodle_feed,
            commands::save_webclass_credentials,
            commands::remove_webclass_credentials,
            commands::set_startup_sync,
            commands::sync_now
        ])
        .run(tauri::generate_context!());

    if result.is_err() {
        // Tauri's underlying startup error can include paths; never print it.
        eprintln!("アプリを起動できませんでした。");
        std::process::exit(1);
    }
}
