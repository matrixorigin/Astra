use astra_services::workflows::WorkflowListItem;

use crate::AppState;
use astra_core::ErrorResponse;
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};

pub async fn list_workflows_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<WorkflowListItem>>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let workflows = state.workflow_service.list_workflows().await?;
    Ok(Json(workflows))
}
