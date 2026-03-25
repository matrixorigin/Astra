pub use mo_agent_services::data_versioning::*;

use crate::AppState;
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use mo_agent_core::ErrorResponse;

pub async fn create_checkpoint_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateCheckpointRequest>,
) -> Result<(StatusCode, Json<CheckpointResponse>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let checkpoint = state
        .data_versioning_service
        .create_checkpoint(
            user.user_id,
            CreateCheckpointData {
                name: request.name,
                description: request.description,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(checkpoint)))
}

pub async fn list_checkpoints_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<CheckpointResponse>>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let checkpoints = state
        .data_versioning_service
        .list_checkpoints(user.user_id)
        .await?;
    Ok(Json(checkpoints))
}

pub async fn get_events_at_checkpoint_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<EventAtCheckpoint>>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let events = state
        .data_versioning_service
        .get_events_at_checkpoint(user.user_id, name)
        .await?;
    Ok(Json(events))
}

pub async fn get_causal_chain_handler(
    State(state): State<AppState>,
    Path(event_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<LineageNode>>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let chain = state
        .data_versioning_service
        .get_causal_chain(user.user_id, event_id)
        .await?;
    Ok(Json(chain))
}

pub async fn trace_upstream_handler(
    State(state): State<AppState>,
    Path(event_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<LineageNode>>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let chain = state
        .data_versioning_service
        .trace_upstream(user.user_id, event_id)
        .await?;
    Ok(Json(chain))
}

pub async fn sandbox_checkpoint_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SandboxCheckpointRequest>,
) -> Result<(StatusCode, Json<CheckpointResponse>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let checkpoint = state
        .data_versioning_service
        .sandbox_checkpoint(
            user.user_id,
            name,
            SandboxCheckpointData {
                checkpoint_name: request.checkpoint_name,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(checkpoint)))
}

pub async fn sandbox_restore_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SandboxCheckpointRequest>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .data_versioning_service
        .sandbox_restore(
            user.user_id,
            name,
            SandboxCheckpointData {
                checkpoint_name: request.checkpoint_name,
            },
        )
        .await?;
    Ok(Json(result))
}
