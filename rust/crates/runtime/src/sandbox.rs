pub use astra_services::sandbox::*;

use crate::AppState;
use astra_core::ErrorResponse;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};

pub async fn create_sandbox_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateSandboxRequest>,
) -> Result<(StatusCode, Json<SandboxRecord>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let sandbox = state
        .sandbox_service
        .create_sandbox(
            user.user_id,
            SandboxCreateRequestData {
                name: request.name,
                description: request.description,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(sandbox)))
}

pub async fn list_sandboxes_handler(
    State(state): State<AppState>,
    Query(query): Query<SandboxListQuery>,
    headers: HeaderMap,
) -> Result<Json<SandboxListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let sandboxes = state
        .sandbox_service
        .list_sandboxes(user.user_id, query.pattern)
        .await?;
    let total = sandboxes.len();
    Ok(Json(SandboxListResponse { sandboxes, total }))
}

pub async fn get_sandbox_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SandboxRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let sandbox = state
        .sandbox_service
        .get_sandbox(name, user.user_id)
        .await?;
    Ok(Json(sandbox))
}

pub async fn delete_sandbox_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .sandbox_service
        .delete_sandbox(name, user.user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
