use astra_services::events::*;

use crate::AppState;
use astra_core::ErrorResponse;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};

pub async fn create_event_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<EventCreateRequest>,
) -> Result<(StatusCode, Json<EventResponse>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let event = state
        .event_service
        .create_event(
            user.user_id,
            EventCreateRequestData {
                session_id: request.session_id,
                event_type: request.event_type,
                content: request.content,
                agent_id: request.agent_id,
                agent_version: request.agent_version,
                parent_event_id: request.parent_event_id,
                causal_chain_id: request.causal_chain_id,
                metadata: request.metadata,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(EventResponse::from(event))))
}

pub async fn list_events_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<EventListQuery>,
) -> Result<Json<EventListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let list = state
        .event_service
        .list_events(EventListFilter {
            user_id: user.user_id,
            session_id: q.session_id,
            event_type: q.event_type,
            agent_id: q.agent_id,
            causal_chain_id: q.causal_chain_id,
            limit: q.limit,
            offset: q.offset,
        })
        .await?;
    Ok(Json(EventListResponse::from(list)))
}

pub async fn get_event_handler(
    State(state): State<AppState>,
    Path(event_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<EventResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let event = state
        .event_service
        .get_event(event_id, user.user_id)
        .await?;
    Ok(Json(EventResponse::from(event)))
}

pub async fn get_causal_chain_handler(
    State(state): State<AppState>,
    Path(causal_chain_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<EventResponse>>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let events = state
        .event_service
        .get_causal_chain(causal_chain_id, user.user_id)
        .await?;
    Ok(Json(events.into_iter().map(EventResponse::from).collect()))
}

pub async fn get_session_events_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<SessionEventQuery>,
) -> Result<Json<EventListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let list = state
        .event_service
        .get_session_events(session_id, user.user_id, q.limit, q.offset)
        .await?;
    Ok(Json(EventListResponse::from(list)))
}

pub async fn delete_event_handler(
    State(state): State<AppState>,
    Path(event_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .event_service
        .delete_event(event_id, user.user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
