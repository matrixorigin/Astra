pub use astra_services::marketplace::*;

use crate::AppState;
use astra_core::ErrorResponse;
use axum::{
    Json,
    extract::{Path, Query, State},
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
    let limit = query.limit.unwrap_or(50);
    let offset = query.offset.unwrap_or(0);
    let result = state
        .marketplace_service
        .list_installed(user.user_id, limit, offset)
        .await?;
    Ok(Json(result))
}

pub async fn save_credential_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CredentialRequest>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .marketplace_service
        .save_credential(
            user.user_id,
            CredentialRequestData {
                skill_name: request.skill_name,
                credential_name: request.credential_name,
                value: request.value,
            },
            &state.fernet_encryptor,
        )
        .await?;
    Ok(Json(result))
}

pub async fn delete_credential_handler(
    State(state): State<AppState>,
    Query(params): Query<DeleteCredentialQuery>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .marketplace_service
        .delete_credential(user.user_id, params.skill_name, params.credential_name)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn publish_skill_handler(
    State(state): State<AppState>,
    Path(skill_name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .marketplace_service
        .publish_skill(user.user_id, skill_name)
        .await?;
    Ok(Json(result))
}

pub async fn deprecate_skill_handler(
    State(state): State<AppState>,
    Path(skill_name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .marketplace_service
        .deprecate_skill(user.user_id, skill_name)
        .await?;
    Ok(Json(result))
}

// ── Marketplace stats handlers (Phase 3) ─────────────────────────────────────

pub use astra_services::marketplace_stats::{
    QualityReportData, SkillMarketplaceStats, SkillSearchQuery, SkillSearchResponse,
};

pub async fn submit_quality_report_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(report): Json<QualityReportData>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    // Require authentication to prevent anonymous quality report spam
    let _user = state.auth_service.current_user(&headers).await?;
    state
        .marketplace_stats_service
        .submit_quality_report(report)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_skill_stats_handler(
    State(state): State<AppState>,
    Path(skill_name): Path<String>,
) -> Result<Json<SkillMarketplaceStats>, (StatusCode, Json<ErrorResponse>)> {
    let stats = state
        .marketplace_stats_service
        .get_skill_stats(skill_name)
        .await?;
    Ok(Json(stats))
}

pub async fn search_marketplace_handler(
    State(state): State<AppState>,
    Query(query): Query<SkillSearchQuery>,
) -> Result<Json<SkillSearchResponse>, (StatusCode, Json<ErrorResponse>)> {
    let results = state
        .marketplace_stats_service
        .search_ranked(query)
        .await?;
    Ok(Json(results))
}
