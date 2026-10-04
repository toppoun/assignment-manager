use tauri::State;

use crate::{
    dto::{AssignmentListDto, CompletionDto},
    error::ApplicationError,
    list_query::ListRequest,
    storage::DesktopState,
};

#[tauri::command]
pub async fn list_assignments(
    request: ListRequest,
    state: State<'_, DesktopState>,
) -> Result<AssignmentListDto, ApplicationError> {
    state.list(request).await
}

#[tauri::command]
pub async fn toggle_assignment_completion(
    assignment_id: String,
    state: State<'_, DesktopState>,
) -> Result<CompletionDto, ApplicationError> {
    state.toggle(assignment_id).await
}
