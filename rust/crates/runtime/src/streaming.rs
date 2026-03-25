pub use mo_agent_services::streaming::*;

use crate::AppState;
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use mo_agent_core::ErrorResponse;

pub async fn stream_chat_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<StreamChatRequest>,
) -> Result<Json<StreamChatResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .streaming_service
        .stream_chat(
            user.user_id,
            StreamChatRequestData {
                session_id: request.session_id,
                message: request.message,
                context: request.context,
                max_candidates: request.max_candidates,
            },
        )
        .await?;
    Ok(Json(result))
}
