use super::*;
use astra_services::session_audit::{
    AuditSessionListParams, CrossSessionRuntimePromotionListParams, CrossSessionStatsParams,
    TurnListParams,
};

pub(super) async fn audit_summary_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let summary = state
        .session_audit_service
        .get_summary(&user.user_id, &session_id)
        .await?;
    Ok(Json(serde_json::to_value(summary).map_err(internal_error)?))
}

pub(super) async fn audit_turns_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Query(params): Query<TurnListParams>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let turns = state
        .session_audit_service
        .list_turns(&user.user_id, &session_id, &params)
        .await?;
    Ok(Json(serde_json::to_value(turns).map_err(internal_error)?))
}

pub(super) async fn audit_turn_detail_handler(
    State(state): State<AppState>,
    Path((session_id, turn)): Path<(String, u32)>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let detail = state
        .session_audit_service
        .get_turn_detail(&user.user_id, &session_id, turn)
        .await?;
    Ok(Json(serde_json::to_value(detail).map_err(internal_error)?))
}

pub(super) async fn audit_context_traces_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let traces = state
        .session_audit_service
        .list_context_traces(&user.user_id, &session_id)
        .await?;
    Ok(Json(serde_json::to_value(traces).map_err(internal_error)?))
}

pub(super) async fn audit_tools_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let tools = state
        .session_audit_service
        .get_tool_analytics(&user.user_id, &session_id)
        .await?;
    Ok(Json(serde_json::to_value(tools).map_err(internal_error)?))
}

pub(super) async fn audit_errors_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let errors = state
        .session_audit_service
        .list_errors(&user.user_id, &session_id)
        .await?;
    Ok(Json(serde_json::to_value(errors).map_err(internal_error)?))
}

pub(super) async fn audit_runtime_promotions_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let promotions = state
        .session_audit_service
        .list_session_runtime_promotions(&user.user_id, &session_id)
        .await?;
    Ok(Json(
        serde_json::to_value(promotions).map_err(internal_error)?,
    ))
}

// ── Cross-session handlers ───────────────────────────────────────────────────

pub(super) async fn list_sessions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<AuditSessionListParams>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .session_audit_service
        .list_sessions(&user.user_id, &params)
        .await?;
    Ok(Json(serde_json::to_value(result).map_err(internal_error)?))
}

pub(super) async fn cross_session_stats_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<CrossSessionStatsParams>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .session_audit_service
        .get_cross_session_stats(&user.user_id, &params)
        .await?;
    Ok(Json(serde_json::to_value(result).map_err(internal_error)?))
}

pub(super) async fn cross_session_tools_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<CrossSessionStatsParams>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .session_audit_service
        .get_cross_session_tools(&user.user_id, &params)
        .await?;
    Ok(Json(serde_json::to_value(result).map_err(internal_error)?))
}

pub(super) async fn cross_session_runtime_promotions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<CrossSessionRuntimePromotionListParams>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .session_audit_service
        .list_cross_session_runtime_promotions(&user.user_id, &params)
        .await?;
    Ok(Json(serde_json::to_value(result).map_err(internal_error)?))
}
