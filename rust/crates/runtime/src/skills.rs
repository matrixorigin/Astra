pub use astra_services::skills::*;

use crate::AppState;
use astra_core::ErrorResponse;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};

pub async fn register_skill_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<RegisterSkillRequest>,
) -> Result<(StatusCode, Json<SkillRecord>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let skill = state
        .skill_service
        .register_skill(
            user.user_id,
            SkillRegisterRequestData {
                skill_id: request.skill_id,
                skill_name: request.skill_name,
                skill_version: request.skill_version,
                skill_code: request.skill_code,
                description: request.description,
                metadata: request.metadata,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(skill)))
}

pub async fn list_skills_handler(
    State(state): State<AppState>,
    Query(query): Query<SkillListQuery>,
    headers: HeaderMap,
) -> Result<Json<SkillListRecord>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let result = state
        .skill_service
        .list_skills(query.limit, query.offset)
        .await?;
    Ok(Json(result))
}

pub async fn get_skill_status_handler(
    State(state): State<AppState>,
    Query(query): Query<SkillStatusQuery>,
    headers: HeaderMap,
) -> Result<Json<SkillStatusRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .skill_service
        .get_skill_status(user.user_id, query.per_group)
        .await?;
    Ok(Json(result))
}

pub async fn publish_skill_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<PublishSkillRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .skill_service
        .publish_skill(
            user.user_id,
            SkillPublishRequestData {
                name: request.name,
                version: request.version,
                description: request.description,
                triggers: request.triggers,
                dependencies: request.dependencies,
                manifest: request.manifest,
                category: request.category,
                priority: request.priority,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(result)))
}

pub async fn get_skill_info_handler(
    State(state): State<AppState>,
    Path(skill_name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SkillInfoRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let info = state
        .skill_service
        .get_skill_info(skill_name, user.user_id)
        .await?;
    Ok(Json(info))
}

pub async fn get_skill_handler(
    State(state): State<AppState>,
    Path(skill_id): Path<String>,
    Query(query): Query<SkillGetQuery>,
    headers: HeaderMap,
) -> Result<Json<SkillRecord>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let skill = state
        .skill_service
        .get_skill(skill_id, query.version)
        .await?;
    Ok(Json(skill))
}

pub async fn list_skill_versions_handler(
    State(state): State<AppState>,
    Path(skill_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<SkillVersionRecord>>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let name = skill_id.split('@').next().unwrap_or(&skill_id).to_string();
    let versions = state.skill_service.list_skill_versions(name).await?;
    Ok(Json(versions))
}

pub async fn unpublish_skill_handler(
    State(state): State<AppState>,
    Path(skill_name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .skill_service
        .unpublish_skill(user.user_id, skill_name)
        .await?;
    Ok(Json(result))
}
