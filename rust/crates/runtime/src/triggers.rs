pub use mo_agent_services::triggers::*;

use crate::AppState;
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use mo_agent_core::ErrorResponse;

pub async fn create_trigger_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateTriggerRequest>,
) -> Result<Json<TriggerRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let trigger = state
        .trigger_service
        .create_trigger(
            user.user_id,
            TriggerCreateRequestData {
                trigger_type: request.trigger_type,
                name: request.name,
                agent_id: request.agent_id,
                user_input: request.user_input,
                context: request.context,
                cron_expr: request.cron_expr,
                session_id: request.session_id,
            },
        )
        .await?;
    Ok(Json(trigger))
}

pub async fn list_triggers_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<TriggerRecord>>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let triggers = state.trigger_service.list_triggers(user.user_id).await?;
    Ok(Json(triggers))
}

pub async fn delete_trigger_handler(
    State(state): State<AppState>,
    Path(trigger_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .trigger_service
        .delete_trigger(trigger_id.clone(), user.user_id)
        .await?;
    Ok(Json(
        serde_json::json!({"trigger_id": trigger_id, "deleted": true}),
    ))
}

pub async fn fire_webhook_handler(
    State(state): State<AppState>,
    Path(trigger_id): Path<String>,
    Json(request): Json<WebhookFireRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let result = state
        .trigger_service
        .fire_webhook(
            trigger_id,
            WebhookFireData {
                secret: request.secret,
                payload: request.payload,
            },
        )
        .await?;
    Ok(Json(result))
}
