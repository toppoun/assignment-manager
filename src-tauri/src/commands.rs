use tauri::State;

use crate::{
    dto::{AssignmentDto, CompletionDto},
    error::ApplicationError,
    storage::DesktopState,
};

#[tauri::command]
pub async fn list_assignments(
    state: State<'_, DesktopState>,
) -> Result<Vec<AssignmentDto>, ApplicationError> {
    state.list().await
}

#[tauri::command]
pub async fn toggle_assignment_completion(
    assignment_id: String,
    state: State<'_, DesktopState>,
) -> Result<CompletionDto, ApplicationError> {
    state.toggle(assignment_id).await
}
