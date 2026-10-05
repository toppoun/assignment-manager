#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

mod commands;
mod credential_store;
mod dto;
mod error;
mod list_query;
mod settings;
mod storage;
mod sync_service;

use error::ApplicationError;
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
            let config_directory = app
                .path()
                .app_config_dir()
                .map_err(|_| ApplicationError::SettingsReadFailed);
            app.manage(storage::open_application(
                directory,
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
            commands::remove_webclass_credentials,
            commands::set_startup_sync,
            commands::sync_now,
            commands::run_startup_sync
        ])
        .build(tauri::generate_context!());

    if let Ok(app) = result {
        app.run(|app, event| storage::handle_exit(app, &event));
    } else {
        // Tauri's underlying startup error can include paths; never print it.
        eprintln!("アプリを起動できませんでした。");
        std::process::exit(1);
    }
}
