pub use mo_agent_services::replay::*;

use crate::AppState;
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use mo_agent_core::ErrorResponse;

pub async fn replay_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ReplaySessionRequest>,
) -> Result<Json<ReplayResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .replay_service
        .replay_session(
            user.user_id,
            session_id,
            ReplaySessionRequestData {
                sandbox_name: request.sandbox_name,
                mock_mode: request.mock_mode.unwrap_or(true),
            },
        )
        .await?;
    Ok(Json(result))
}

pub async fn compare_replay_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ComparisonResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .replay_service
        .compare_replay(user.user_id, session_id)
        .await?;
    Ok(Json(result))
}
