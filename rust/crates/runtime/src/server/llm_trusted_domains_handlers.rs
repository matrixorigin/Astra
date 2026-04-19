use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};

use astra_core::ErrorResponse;
use astra_services::llm_trusted_domains::{
    LlmTrustedDomainDeleteResponse, LlmTrustedDomainRecord, LlmTrustedDomainUpsertRequest,
    LlmTrustedDomainUpsertRequestData,
};

use crate::app_state::AppState;

pub(super) async fn list_llm_trusted_domains_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<LlmTrustedDomainRecord>>, (StatusCode, Json<ErrorResponse>)> {
    state.admin_authorizer.require_admin(&headers).await?;
    let records = state
        .llm_trusted_domain_service
        .list_trusted_domains()
        .await?;
    Ok(Json(records))
}

pub(super) async fn upsert_llm_trusted_domain_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LlmTrustedDomainUpsertRequest>,
) -> Result<Json<LlmTrustedDomainRecord>, (StatusCode, Json<ErrorResponse>)> {
    let authenticated = state.admin_authorizer.require_admin(&headers).await?;
    let request = LlmTrustedDomainUpsertRequestData {
        domain_host: body.domain_host,
        domain_port: body.domain_port,
        is_enabled: body.is_enabled,
        description: body.description,
    };
    let record = state
        .llm_trusted_domain_service
        .upsert_trusted_domain(Some(authenticated.user_id.as_str()), request)
        .await?;
    Ok(Json(record))
}

pub(super) async fn delete_llm_trusted_domain_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(domain_id): Path<String>,
) -> Result<Json<LlmTrustedDomainDeleteResponse>, (StatusCode, Json<ErrorResponse>)> {
    state.admin_authorizer.require_admin(&headers).await?;
    let response = state
        .llm_trusted_domain_service
        .delete_trusted_domain(&domain_id)
        .await?;
    Ok(Json(response))
}
