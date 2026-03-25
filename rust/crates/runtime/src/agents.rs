use mo_agent_services::agents::*;

use crate::AppState;
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use mo_agent_core::ErrorResponse;

pub async fn create_agent_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AgentCreateRequest>,
) -> Result<(StatusCode, Json<AgentResponse>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let agent = state
        .agent_service
        .create_agent(
            user.user_id,
            AgentCreateRequestData {
                name: request.name,
                agent_config: request.agent_config,
                data_source: request.data_source,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(AgentResponse::from(agent))))
}

pub async fn list_agents_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AgentListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let list = state.agent_service.list_agents(user.user_id).await?;
    Ok(Json(AgentListResponse::from(list)))
}

pub async fn get_agent_handler(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<AgentResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let agent = state
        .agent_service
        .get_agent(agent_id, user.user_id)
        .await?;
    Ok(Json(AgentResponse::from(agent)))
}

pub async fn update_agent_handler(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<AgentUpdateRequest>,
) -> Result<Json<AgentResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let agent = state
        .agent_service
        .update_agent(
            agent_id,
            user.user_id,
            AgentUpdateRequestData {
                name: request.name,
                agent_config: request.agent_config,
                data_source: request.data_source,
                is_active: request.is_active,
            },
        )
        .await?;
    Ok(Json(AgentResponse::from(agent)))
}

pub async fn delete_agent_handler(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .agent_service
        .delete_agent(agent_id, user.user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
