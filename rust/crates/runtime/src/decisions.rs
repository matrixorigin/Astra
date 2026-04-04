use astra_services::decisions::*;

use crate::AppState;
use astra_core::ErrorResponse;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};

pub async fn record_decision_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<DecisionCreateRequest>,
) -> Result<(StatusCode, Json<DecisionResponse>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let decision = state
        .decision_service
        .record_decision(
            user.user_id,
            DecisionCreateRequestData {
                session_id: request.session_id,
                event_id: request.event_id,
                context_capture_id: request.context_capture_id,
                decision_type: request.decision_type,
                decision_output: request.decision_output,
                model_params: request.model_params,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(DecisionResponse::from(decision))))
}

pub async fn list_decisions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DecisionListQuery>,
) -> Result<Json<DecisionListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let list = state
        .decision_service
        .list_decisions(DecisionListFilter {
            user_id: user.user_id,
            session_id: q.session_id,
            decision_type: q.decision_type,
            limit: q.limit,
            offset: q.offset,
        })
        .await?;
    Ok(Json(DecisionListResponse::from(list)))
}

pub async fn get_decision_handler(
    State(state): State<AppState>,
    Path(decision_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<DecisionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let decision = state
        .decision_service
        .get_decision(decision_id, user.user_id)
        .await?;
    Ok(Json(DecisionResponse::from(decision)))
}

pub async fn audit_decision_handler(
    State(state): State<AppState>,
    Path(decision_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<DecisionWithContextResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let record = state
        .decision_service
        .get_decision_with_context(decision_id, user.user_id)
        .await?;
    Ok(Json(DecisionWithContextResponse::from(record)))
}
