//! Tauri transport adapter: input conversion -> shared handle -> JSON DTO.
use crate::{
    credential_store::{SecretInput, WebClassCredentialsInput},
    dto::{AssignmentListDto, CompletionDto, SettingsStatusDto},
    error::CommandError,
    list_query::ListRequest,
    sync_service::SyncResultDto,
};
use assignment_manager_application::ApplicationHandle;
use tauri::State;

#[tauri::command]
pub async fn list_assignments(
    request: ListRequest,
    state: State<'_, ApplicationHandle>,
) -> Result<AssignmentListDto, CommandError> {
    state
        .query_assignments(request.into())
        .await
        .map(Into::into)
        .map_err(Into::into)
}
#[tauri::command]
pub async fn toggle_assignment_completion(
    assignment_id: String,
    state: State<'_, ApplicationHandle>,
) -> Result<CompletionDto, CommandError> {
    state
        .toggle_completion(assignment_id)
        .await
        .map(Into::into)
        .map_err(Into::into)
}
#[tauri::command]
pub async fn get_settings_status(
    state: State<'_, ApplicationHandle>,
) -> Result<SettingsStatusDto, CommandError> {
    state
        .get_settings_status()
        .await
        .map(Into::into)
        .map_err(Into::into)
}
#[tauri::command]
pub async fn save_moodle_feed(
    secret_url: SecretInput,
    state: State<'_, ApplicationHandle>,
) -> Result<(), CommandError> {
    state
        .save_moodle_config(secret_url.0)
        .await
        .map_err(Into::into)
}
#[tauri::command]
pub async fn remove_moodle_feed(state: State<'_, ApplicationHandle>) -> Result<(), CommandError> {
    state.remove_moodle_config().await.map_err(Into::into)
}
#[tauri::command]
pub async fn save_webclass_credentials(
    credentials: WebClassCredentialsInput,
    state: State<'_, ApplicationHandle>,
) -> Result<(), CommandError> {
    state
        .save_webclass_credentials(credentials.0)
        .await
        .map_err(Into::into)
}
#[tauri::command]
pub async fn remove_webclass_credentials(
    state: State<'_, ApplicationHandle>,
) -> Result<(), CommandError> {
    state
        .remove_webclass_credentials()
        .await
        .map_err(Into::into)
}
#[tauri::command]
pub async fn set_startup_sync(
    enabled: bool,
    state: State<'_, ApplicationHandle>,
) -> Result<(), CommandError> {
    state.set_sync_on_startup(enabled).await.map_err(Into::into)
}
#[tauri::command]
pub async fn sync_now(state: State<'_, ApplicationHandle>) -> Result<SyncResultDto, CommandError> {
    state.sync_now().await.map(Into::into).map_err(Into::into)
}
#[tauri::command]
pub async fn run_startup_sync(
    state: State<'_, ApplicationHandle>,
) -> Result<Option<SyncResultDto>, CommandError> {
    state
        .run_startup_sync()
        .await
        .map(|report| report.map(Into::into))
        .map_err(Into::into)
}
