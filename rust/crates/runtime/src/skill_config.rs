pub use mo_agent_services::skill_config::*;

use crate::AppState;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use mo_agent_core::{ErrorResponse, error_response};

fn extract_user_id(headers: &HeaderMap) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    headers
        .get("X-User-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "Missing X-User-Id header"))
}

pub async fn validate_config_handler(
    State(state): State<AppState>,
    Path(skill_name): Path<String>,
    Query(query): Query<ValidateQuery>,
    headers: HeaderMap,
) -> Result<Json<ValidationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    state
        .skill_config_service
        .validate_config(&user_id, &skill_name, query.resource.as_deref())
        .await
}

pub async fn get_effective_config_handler(
    State(state): State<AppState>,
    Path(skill_name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ConfigResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    state
        .skill_config_service
        .get_effective_config(&user_id, &skill_name)
        .await
}

pub async fn set_setting_handler(
    State(state): State<AppState>,
    Path((skill_name, setting_name)): Path<(String, String)>,
    Query(query): Query<ScopeQuery>,
    headers: HeaderMap,
    Json(body): Json<SetSettingRequest>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;

    // Admin check for global scope
    if query.scope == "global" {
        let is_admin = headers
            .get("X-User-Role")
            .and_then(|v| v.to_str().ok())
            .map(|r| r == "mo_agent_admin")
            .unwrap_or(false);
        if !is_admin {
            return Err(error_response(
                StatusCode::FORBIDDEN,
                "Global scope requires admin role",
            ));
        }
    }

    state
        .skill_config_service
        .set_setting(
            &user_id,
            &skill_name,
            &setting_name,
            &query.scope,
            body.value,
            &state.fernet_encryptor,
        )
        .await
}

pub async fn delete_setting_handler(
    State(state): State<AppState>,
    Path((skill_name, setting_name)): Path<(String, String)>,
    Query(query): Query<ScopeQuery>,
    headers: HeaderMap,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;

    if query.scope == "global" {
        let is_admin = headers
            .get("X-User-Role")
            .and_then(|v| v.to_str().ok())
            .map(|r| r == "mo_agent_admin")
            .unwrap_or(false);
        if !is_admin {
            return Err(error_response(
                StatusCode::FORBIDDEN,
                "Global scope requires admin role",
            ));
        }
    }

    state
        .skill_config_service
        .delete_setting(&user_id, &skill_name, &setting_name, &query.scope)
        .await
}

pub async fn list_resources_handler(
    State(state): State<AppState>,
    Path(skill_name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<ResourceEntry>>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    state
        .skill_config_service
        .list_resources(&user_id, &skill_name)
        .await
}

pub async fn bind_resource_handler(
    State(state): State<AppState>,
    Path((skill_name, resource_key)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<BindResourceRequest>,
) -> Result<Json<BindResourceResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    state
        .skill_config_service
        .bind_resource(
            &user_id,
            &skill_name,
            &resource_key,
            body.bindings,
            &state.fernet_encryptor,
        )
        .await
}

pub async fn unbind_resource_handler(
    State(state): State<AppState>,
    Path((skill_name, resource_key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<UnbindResourceResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    state
        .skill_config_service
        .unbind_resource(&user_id, &skill_name, &resource_key)
        .await
}
