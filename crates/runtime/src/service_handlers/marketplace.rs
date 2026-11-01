pub use astra_services::marketplace::*;

use crate::AppState;
use astra_core::ErrorResponse;
use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
};

pub async fn install_skill_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<InstallRequest>,
) -> Result<Json<InstallationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .marketplace_service
        .install_skill(
            user.user_id,
            InstallRequestData {
                skill_name: request.skill_name,
            },
        )
        .await?;
    Ok(Json(result))
}

pub async fn uninstall_skill_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<InstallRequest>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .marketplace_service
        .uninstall_skill(
            user.user_id,
            InstallRequestData {
                skill_name: request.skill_name,
            },
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn upgrade_skill_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<InstallRequest>,
) -> Result<Json<InstallationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .marketplace_service
        .upgrade_skill(
            user.user_id,
            InstallRequestData {
                skill_name: request.skill_name,
            },
        )
        .await?;
    Ok(Json(result))
}

pub async fn rollback_skill_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<InstallRequest>,
) -> Result<Json<InstallationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .marketplace_service
        .rollback_skill(
            user.user_id,
            InstallRequestData {
                skill_name: request.skill_name,
            },
        )
        .await?;
    Ok(Json(result))
}

pub async fn list_installed_handler(
    State(state): State<AppState>,
    Query(query): Query<ListInstalledQuery>,
    headers: HeaderMap,
) -> Result<Json<InstalledListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let cursor = query.cursor()?;
    let result = state
        .marketplace_service
        .list_installed(user.user_id, query.limit, cursor)
        .await?;
    Ok(Json(result))
}
