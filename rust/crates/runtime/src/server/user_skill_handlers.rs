//! Personal skill HTTP endpoints for web-agent skill state.

use super::*;
use astra_services::{
    ActivateUserSkillVersion, CreateUserSkillSource, DatabasePersonalSkillStore, InstallUserSkill,
    PersonalSkillError, RecordUserSkillEvaluation, SubmitUserSkillVersion,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(super) struct ListUserSkillsQuery {
    prefix: Option<String>,
    owner_user_id: Option<String>,
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
        PersonalSkillError::InvalidStatus { .. }
        | PersonalSkillError::VersionQuarantined { .. } => {
            error_response(StatusCode::BAD_REQUEST, error.to_string())
        }
        PersonalSkillError::VersionNotFound { .. } => {
            error_response(StatusCode::NOT_FOUND, error.to_string())
        }
        other => error_response(StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
    }
}

fn requested_owner(
    authenticated_user_id: &str,
    requested_owner_user_id: Option<&str>,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    match requested_owner_user_id {
        Some(owner) if owner != authenticated_user_id => Err(error_response(
            StatusCode::FORBIDDEN,
            "cannot access another user's personal skills",
        )),
        Some(owner) => Ok(owner.to_string()),
        None => Ok(authenticated_user_id.to_string()),
    }
}

pub(super) async fn list_user_skills_handler(
    State(state): State<AppState>,
    Query(query): Query<ListUserSkillsQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<astra_services::UserSkillSourceRecord>>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let owner_user_id = requested_owner(&user.user_id, query.owner_user_id.as_deref())?;
    let store = require_personal_skill_store(&state)?;
    store
        .list_sources(&owner_user_id, query.prefix.as_deref())
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

pub(super) async fn install_user_skill_handler(
    State(state): State<AppState>,
    Path(skill_name): Path<String>,
    headers: HeaderMap,
    Json(request): Json<InstallUserSkill>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_personal_skill_store(&state)?;
    store
        .install_skill(&user.user_id, &skill_name, request)
        .await
        .map_err(map_personal_skill_error)?;
    Ok(Json(serde_json::json!({ "ok": true })))
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
    let versions = store
        .list_versions(&user.user_id, &skill_name)
        .await
        .map_err(map_personal_skill_error)?;
    let owns_version = versions.iter().any(|version| {
        version.source_id == request.source_id && version.version_id == request.version_id
    });
    if !owns_version {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            format!("skill version not found for {skill_name}"),
        ));
    }
    store
        .record_evaluation(request)
        .await
        .map(|record| (StatusCode::CREATED, Json(record)))
        .map_err(map_personal_skill_error)
}
