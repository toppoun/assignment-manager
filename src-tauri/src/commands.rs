use tauri::State;

use crate::{
    credential_store::{SecretText, WebClassCredentials},
    dto::{AssignmentListDto, CompletionDto},
    error::ApplicationError,
    list_query::ListRequest,
    settings::SettingsStatusDto,
    settings_worker::SettingsState,
    storage::DesktopState,
    sync_service::SyncResultDto,
    sync_worker::SyncState,
};

#[tauri::command]
pub async fn list_assignments(
    request: ListRequest,
    state: State<'_, DesktopState>,
) -> Result<AssignmentListDto, ApplicationError> {
    state.list(request).await
}

#[tauri::command]
pub async fn sync_now(state: State<'_, SyncState>) -> Result<SyncResultDto, ApplicationError> {
    state.sync_now().await
}

#[tauri::command]
pub async fn set_startup_sync(
    enabled: bool,
    state: State<'_, SettingsState>,
) -> Result<(), ApplicationError> {
    state.set_startup_sync(enabled).await
}

#[tauri::command]
pub async fn get_settings_status(
    state: State<'_, SettingsState>,
) -> Result<SettingsStatusDto, ApplicationError> {
    state.status().await
}

#[tauri::command]
pub async fn save_moodle_feed(
    secret_url: SecretText,
    state: State<'_, SettingsState>,
) -> Result<(), ApplicationError> {
    state.save_moodle(secret_url).await
}

#[tauri::command]
pub async fn remove_moodle_feed(state: State<'_, SettingsState>) -> Result<(), ApplicationError> {
    state.remove_moodle().await
}

#[tauri::command]
pub async fn save_webclass_credentials(
    credentials: WebClassCredentials,
    state: State<'_, SettingsState>,
) -> Result<(), ApplicationError> {
    state.save_webclass(credentials).await
}

#[tauri::command]
pub async fn remove_webclass_credentials(
    state: State<'_, SettingsState>,
) -> Result<(), ApplicationError> {
    state.remove_webclass().await
}

#[tauri::command]
pub async fn toggle_assignment_completion(
    assignment_id: String,
    state: State<'_, DesktopState>,
) -> Result<CompletionDto, ApplicationError> {
    state.toggle(assignment_id).await
}
