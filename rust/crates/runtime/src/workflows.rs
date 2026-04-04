use astra_services::workflows::*;

use crate::AppState;
use astra_core::ErrorResponse;
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};

pub async fn list_workflows_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<WorkflowDefRecord>>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let workflows = state.workflow_service.list_workflows().await?;
    Ok(Json(workflows))
}

pub async fn get_workflow_run_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<WorkflowRunRecord>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let run = state.workflow_service.get_workflow_run(run_id).await?;
    Ok(Json(run))
}

pub async fn resolve_workflow_wait_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    Json(result): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let response = state
        .workflow_service
        .resolve_workflow_wait(run_id, result)
        .await?;
    Ok(Json(response))
}
