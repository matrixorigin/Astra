//! REST handlers for the resource governance admin API (Phase 5.2).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use serde_json::json;

use astra_core::ErrorResponse;

use crate::app_state::AppState;

/// GET /resources/usage — current user's resource usage for today.
pub(super) async fn get_resource_usage_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let usage = state.resource_governor.get_usage(&user.user_id).await;
    let limits = state.resource_governor.get_limits(&user.user_id).await;
    Ok(Json(json!({
        "usage": usage,
        "limits": limits,
    })))
}

/// GET /resources/limits — current user's effective limits.
pub(super) async fn get_resource_limits_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let limits = state.resource_governor.get_limits(&user.user_id).await;
    Ok(Json(json!({ "limits": limits })))
}

/// PUT /admin/resources/limits/{user_id} — admin override of per-user limits.
pub(super) async fn set_resource_limits_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Json(limits): Json<astra_services::resource_governor::ResourceLimits>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    state.admin_authorizer.require_admin(&headers).await?;
    state
        .resource_governor
        .set_limits(&user_id, limits.clone())
        .await;
    Ok(Json(json!({
        "status": "ok",
        "user_id": user_id,
        "limits": limits,
    })))
}
