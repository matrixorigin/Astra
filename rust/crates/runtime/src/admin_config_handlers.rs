//! HTTP handlers for server-wide admin configuration (`/admin/config`).
//!
//! All routes require `astra_admin` role (via [`AdminAuthorizer::require_admin`]).

use crate::AppState;
use astra_core::{ErrorResponse, error_response, internal_error};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct AdminConfigEntry {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct AdminConfigListResponse {
    pub entries: Vec<AdminConfigEntry>,
}

#[derive(Debug, Serialize)]
pub struct AdminConfigGetResponse {
    pub key: String,
    pub value: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AdminConfigSetRequest {
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct AdminConfigDeleteResponse {
    pub deleted: bool,
}

pub async fn list_admin_config_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AdminConfigListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _admin = state.admin_authorizer.require_admin(&headers).await?;
    let rows = state
        .admin_config_service
        .list()
        .await
        .map_err(internal_error)?;
    Ok(Json(AdminConfigListResponse {
        entries: rows
            .into_iter()
            .map(|(key, value)| AdminConfigEntry { key, value })
            .collect(),
    }))
}

pub async fn get_admin_config_handler(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: HeaderMap,
) -> Result<Json<AdminConfigGetResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _admin = state.admin_authorizer.require_admin(&headers).await?;
    let value = state
        .admin_config_service
        .get(&key)
        .await
        .map_err(internal_error)?;
    Ok(Json(AdminConfigGetResponse { key, value }))
}

pub async fn set_admin_config_handler(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: HeaderMap,
    Json(request): Json<AdminConfigSetRequest>,
) -> Result<Json<AdminConfigEntry>, (StatusCode, Json<ErrorResponse>)> {
    let admin = state.admin_authorizer.require_admin(&headers).await?;
    state
        .admin_config_service
        .set(&key, &request.value, Some(&admin.user_id))
        .await
        .map_err(|e| error_response(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(AdminConfigEntry {
        key,
        value: request.value,
    }))
}

pub async fn delete_admin_config_handler(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: HeaderMap,
) -> Result<Json<AdminConfigDeleteResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _admin = state.admin_authorizer.require_admin(&headers).await?;
    let deleted = state
        .admin_config_service
        .unset(&key)
        .await
        .map_err(internal_error)?;
    Ok(Json(AdminConfigDeleteResponse { deleted }))
}
