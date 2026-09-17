//! Personal skill HTTP endpoints for web-agent skill state.

use super::*;
use astra_services::{
    ActivateUserSkillVersion, CreateUserSkillSource, DatabasePersonalSkillStore,
    PersonalSkillError, RecordUserSkillEvaluation, SubmitUserSkillVersion,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListUserSkillsQuery {
    prefix: Option<String>,
}

fn require_personal_skill_store(
    state: &AppState,
) -> Result<DatabasePersonalSkillStore, (StatusCode, Json<ErrorResponse>)> {
    state
        .shared_pool
        .clone()
        .map(DatabasePersonalSkillStore::new)
        .ok_or_else(|| error_response(StatusCode::SERVICE_UNAVAILABLE, "database not configured"))
}

fn map_personal_skill_error(error: PersonalSkillError) -> (StatusCode, Json<ErrorResponse>) {
    match error {
        PersonalSkillError::InvalidStatus { .. } => {
            error_response(StatusCode::BAD_REQUEST, error.to_string())
        }
        PersonalSkillError::VersionNotActivatable { .. } => {
            error_response(StatusCode::CONFLICT, error.to_string())
        }
        PersonalSkillError::VersionNotFound { .. }
        | PersonalSkillError::SessionNotActive { .. }
        | PersonalSkillError::RunNotFound { .. } => {
            error_response(StatusCode::NOT_FOUND, error.to_string())
        }
        PersonalSkillError::InvalidActiveProjection { .. } => {
            error_response(StatusCode::CONFLICT, error.to_string())
        }
        other => error_response(StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
    }
}

pub(super) async fn list_user_skills_handler(
    State(state): State<AppState>,
    Query(query): Query<ListUserSkillsQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<astra_services::UserSkillSourceRecord>>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_personal_skill_store(&state)?;
    store
        .list_sources(&user.user_id, query.prefix.as_deref())
        .await
        .map(Json)
        .map_err(map_personal_skill_error)
}

pub(super) async fn create_user_skill_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateUserSkillSource>,
) -> Result<
    (StatusCode, Json<astra_services::UserSkillSourceRecord>),
    (StatusCode, Json<ErrorResponse>),
> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_personal_skill_store(&state)?;
    store
        .create_source(&user.user_id, request)
        .await
        .map(|record| (StatusCode::CREATED, Json(record)))
        .map_err(map_personal_skill_error)
}

pub(super) async fn list_user_skill_versions_handler(
    State(state): State<AppState>,
    Path(skill_name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<astra_services::UserSkillVersionRecord>>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_personal_skill_store(&state)?;
    store
        .list_versions(&user.user_id, &skill_name)
        .await
        .map(Json)
        .map_err(map_personal_skill_error)
}

pub(super) async fn submit_user_skill_version_handler(
    State(state): State<AppState>,
    Path(skill_name): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SubmitUserSkillVersion>,
) -> Result<
    (StatusCode, Json<astra_services::UserSkillVersionRecord>),
    (StatusCode, Json<ErrorResponse>),
> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_personal_skill_store(&state)?;
    store
        .submit_version(&user.user_id, &skill_name, request)
        .await
        .map(|record| (StatusCode::CREATED, Json(record)))
        .map_err(map_personal_skill_error)
}

pub(super) async fn activate_user_skill_handler(
    State(state): State<AppState>,
    Path(skill_name): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ActivateUserSkillVersion>,
) -> Result<Json<astra_services::UserSkillVersionRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_personal_skill_store(&state)?;
    store
        .activate_version(
            &user.user_id,
            &request.session_id,
            &skill_name,
            &request.version_id,
        )
        .await
        .map(Json)
        .map_err(map_personal_skill_error)
}

pub(super) async fn record_user_skill_evaluation_handler(
    State(state): State<AppState>,
    Path(skill_name): Path<String>,
    headers: HeaderMap,
    Json(request): Json<RecordUserSkillEvaluation>,
) -> Result<
    (StatusCode, Json<astra_services::UserSkillEvaluationRecord>),
    (StatusCode, Json<ErrorResponse>),
> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_personal_skill_store(&state)?;
    store
        .record_evaluation(&user.user_id, &skill_name, request)
        .await
        .map(|record| (StatusCode::CREATED, Json(record)))
        .map_err(map_personal_skill_error)
}
