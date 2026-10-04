fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "list_assignments",
            "toggle_assignment_completion",
            "get_settings_status",
            "save_moodle_feed",
            "remove_moodle_feed",
            "save_webclass_credentials",
            "remove_webclass_credentials",
            "set_startup_sync",
            "sync_now",
        ]),
    ))
    .expect("could not build desktop application resources");
}
