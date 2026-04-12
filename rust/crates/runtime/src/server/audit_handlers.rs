use super::*;
use astra_services::session_audit::{
    AuditSessionListParams, CrossSessionMutationListParams, CrossSessionRuntimePromotionListParams,
    CrossSessionStatsParams, MutationStateUpdateRequest, TurnListParams,
};
use astra_services::{EventCreateRequestData, StagedMutationState};

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

pub(super) async fn audit_mutations_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let mutations = state
        .session_audit_service
        .get_mutation_scoreboard(&user.user_id, &session_id)
        .await?;
    Ok(Json(
        serde_json::to_value(mutations).map_err(internal_error)?,
    ))
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

pub(super) async fn audit_mutation_state_handler(
    State(state): State<AppState>,
    Path((session_id, mutation_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<MutationStateUpdateRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    if !matches!(
        body.state,
        StagedMutationState::Applied | StagedMutationState::Reverted | StagedMutationState::Blocked
    ) {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "mutation state updates only allow applied, reverted, or blocked",
        ));
    }

    let existing = state
        .session_audit_service
        .get_mutation_scoreboard(&user.user_id, &session_id)
        .await?;
    let mutation = existing
        .mutations
        .iter()
        .find(|mutation| mutation.mutation_id == mutation_id)
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Mutation not found"))?;
    let note = body
        .note
        .as_deref()
        .map(str::trim)
        .filter(|note| !note.is_empty())
        .map(ToString::to_string);
    let state_label = match body.state {
        StagedMutationState::Applied => "applied",
        StagedMutationState::Reverted => "reverted",
        StagedMutationState::Blocked => "blocked",
        StagedMutationState::Pending | StagedMutationState::Ready => unreachable!(),
    };

    state
        .event_service
        .create_event(
            user.user_id.clone(),
            EventCreateRequestData {
                session_id: session_id.clone(),
                event_type: "mutation_state".to_string(),
                content: note
                    .clone()
                    .unwrap_or_else(|| format!("mutation `{mutation_id}` marked {state_label}")),
                agent_id: None,
                agent_version: None,
                parent_event_id: None,
                parent_event_ids: Some(Vec::new()),
                causal_chain_id: Some(format!("{session_id}:mutation:{mutation_id}")),
                metadata: Some(serde_json::json!({
                    "mutation_id": mutation_id,
                    "state": body.state,
                    "note": note,
                    "tool_name": mutation.tool_name,
                    "turn": mutation.turn_index,
                })),
            },
        )
        .await?;

    let updated = state
        .session_audit_service
        .get_mutation_scoreboard(&user.user_id, &session_id)
        .await?;
    let updated_mutation = updated
        .mutations
        .into_iter()
        .find(|mutation| mutation.mutation_id == mutation_id)
        .ok_or_else(|| internal_error("mutation state update did not materialize"))?;
    Ok(Json(
        serde_json::to_value(updated_mutation).map_err(internal_error)?,
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

pub(super) async fn cross_session_mutations_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<CrossSessionMutationListParams>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .session_audit_service
        .list_cross_session_mutations(&user.user_id, &params)
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
