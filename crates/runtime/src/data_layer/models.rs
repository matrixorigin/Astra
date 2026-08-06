use astra_services::models::*;
use serde::{Deserialize, Serialize};

use crate::AppState;
use astra_core::{ErrorResponse, error_response, internal_error};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};

pub async fn create_model_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ModelCreateRequest>,
) -> Result<(StatusCode, Json<ModelResponse>), (StatusCode, Json<ErrorResponse>)> {
    let admin = state.admin.authorizer.require_admin(&headers).await?;

    let model = state
        .model_service
        .create_model(
            admin.user_id,
            ModelCreateRequestData {
                name: request.name,
                provider: request.provider,
                api_key: request.api_key,
                base_url: request.base_url,
                description: request.description,
                context_window: request.context_window,
                max_completion_tokens: request.max_completion_tokens,
                input_modalities: request.input_modalities,
                output_modalities: request.output_modalities,
                supported_parameters: request.supported_parameters,
                pricing: request.pricing,
                architecture: request.architecture,
                tags: request.tags,
                quirks: request.quirks,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(ModelResponse::from(model))))
}

pub async fn list_models_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ModelListItemResponse>>, (StatusCode, Json<ErrorResponse>)> {
    let catalog = effective_model_catalog(&state, &headers).await?;
    Ok(Json(
        catalog
            .offerings
            .into_iter()
            .map(ModelListItemResponse::from)
            .collect(),
    ))
}

struct EffectiveModelCatalog {
    declared: Vec<DeclaredModelAccess>,
    offerings: Vec<ModelListItem>,
    provider_default_offering_id: Option<String>,
}

async fn effective_model_catalog(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<EffectiveModelCatalog, (StatusCode, Json<ErrorResponse>)> {
    let principal = state.auth_service.current_principal(headers).await?;
    if principal.is_edge_registration() {
        let catalog = state
            .auth_service
            .external_catalog_by_scope(&principal)
            .await?;
        return Ok(EffectiveModelCatalog {
            declared: vec![DeclaredModelAccess {
                id: "this-device".to_string(),
                kind: ModelAccessKind::ThisDevice,
                label: "This device".to_string(),
                execution_placement: ModelExecutionPlacement::Edge,
                availability: ModelAccessAvailability::Ready,
            }],
            offerings: catalog
                .models
                .into_iter()
                .map(ModelListItem::from)
                .collect(),
            provider_default_offering_id: catalog.default_model_id,
        });
    }
    let user = principal.user;
    let is_admin = state.admin.authorizer.require_admin(headers).await.is_ok();
    let offerings = state
        .model_service
        .list_models(user.user_id, is_admin)
        .await?;
    Ok(EffectiveModelCatalog {
        declared: vec![DeclaredModelAccess {
            id: "self-hosted".to_string(),
            kind: ModelAccessKind::SelfHosted,
            label: "Self-hosted".to_string(),
            execution_placement: ModelExecutionPlacement::Server,
            availability: ModelAccessAvailability::Ready,
        }],
        offerings,
        provider_default_offering_id: None,
    })
}

pub async fn get_model_access_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ModelAccessProjectionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let catalog = effective_model_catalog(&state, &headers).await?;
    let projection = project_effective_model_access(catalog, chrono::Utc::now().to_rfc3339())
        .map_err(internal_error)?;
    Ok(Json(projection))
}

fn project_effective_model_access(
    catalog: EffectiveModelCatalog,
    observed_at: String,
) -> astra_services::service_error::ServiceResult<ModelAccessProjectionResponse> {
    let offerings = catalog
        .offerings
        .into_iter()
        .filter(|offering| offering.is_active)
        .map(ModelListItemResponse::from)
        .collect();
    project_model_access_with_default(
        catalog.declared,
        offerings,
        catalog.provider_default_offering_id,
        observed_at,
    )
}

pub async fn get_model_handler(
    State(state): State<AppState>,
    Path(model_name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ModelResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let model = state.model_service.get_model(model_name).await?;
    Ok(Json(ModelResponse::from(model)))
}

pub async fn get_memory_model_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<MemoryInferenceOfferingsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let matrixone = crate::matrix_cloud_runtime::matrix_settings_from_env().map_err(|e| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("MatrixOne configuration unavailable: {e}"),
        )
    })?;
    let pool_ref = state.shared_pool.as_ref().map(|sp| sp.get());
    let resolved = resolve_memory_offerings(&matrixone, &state.fernet_encryptor, pool_ref)
        .await
        .map_err(|e| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Memory model resolution failed: {e}"),
            )
        })?;
    let offerings = resolved
        .into_iter()
        .map(|offering| MemoryInferenceOfferingResponse {
            offering_id: offering.offering_id,
            model_name: offering.model.model_name,
            thinking_capability: offering.model.thinking_capability,
        })
        .collect::<Vec<_>>();
    if offerings.is_empty() {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "No active LLM model configured.",
        ));
    }
    Ok(Json(MemoryInferenceOfferingsResponse { offerings }))
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryInferenceOfferingsResponse {
    /// Ordered candidates. The first entry is the current governed default;
    /// later entries are explicit failover candidates for optional memory
    /// inference only.
    pub offerings: Vec<MemoryInferenceOfferingResponse>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryInferenceOfferingResponse {
    pub offering_id: String,
    /// Display/diagnostic fact; clients must never use it to select a route.
    pub model_name: String,
    pub thinking_capability: Option<ThinkingCapability>,
}

pub async fn update_model_handler(
    State(state): State<AppState>,
    Path(model_name): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ModelUpdateRequest>,
) -> Result<Json<ModelResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _admin = state.admin.authorizer.require_admin(&headers).await?;
    let model = state
        .model_service
        .update_model(
            model_name,
            ModelUpdateRequestData {
                api_key: request.api_key,
                base_url: request.base_url,
                provider: request.provider,
                description: request.description,
                context_window: request.context_window,
                max_completion_tokens: request.max_completion_tokens,
                input_modalities: request.input_modalities,
                output_modalities: request.output_modalities,
                supported_parameters: request.supported_parameters,
                pricing: request.pricing,
                architecture: request.architecture,
                tags: request.tags,
                is_active: request.is_active,
                quirks: request.quirks,
            },
        )
        .await?;
    Ok(Json(ModelResponse::from(model)))
}

pub async fn delete_model_handler(
    State(state): State<AppState>,
    Path(model_name): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let _admin = state.admin.authorizer.require_admin(&headers).await?;
    state.model_service.delete_model(model_name).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn check_model_handler(
    State(state): State<AppState>,
    Path(model_name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ModelResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _admin = state.admin.authorizer.require_admin(&headers).await?;
    let model = state.model_service.check_model(model_name).await?;
    Ok(Json(ModelResponse::from(model)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offering(id: &str, name: &str) -> ModelListItem {
        ModelListItem {
            offering_id: id.to_string(),
            access_id: "this-device".to_string(),
            access_kind: ModelAccessKind::ThisDevice,
            access_label: "This device".to_string(),
            execution_placement: ModelExecutionPlacement::Edge,
            name: name.to_string(),
            provider: "moi".to_string(),
            description: None,
            is_active: true,
            context_window: 128_000,
            max_completion_tokens: None,
            architecture: None,
            thinking_capability: None,
        }
    }

    #[test]
    fn model_access_uses_external_catalog_default_instead_of_catalog_order() {
        let projection = project_effective_model_access(
            EffectiveModelCatalog {
                declared: vec![DeclaredModelAccess {
                    id: "this-device".to_string(),
                    kind: ModelAccessKind::ThisDevice,
                    label: "This device".to_string(),
                    execution_placement: ModelExecutionPlacement::Edge,
                    availability: ModelAccessAvailability::Ready,
                }],
                offerings: vec![
                    offering("model-alpha", "Alpha"),
                    offering("model-beta", "Beta"),
                ],
                provider_default_offering_id: Some("model-beta".to_string()),
            },
            "2026-08-06T00:00:00Z".to_string(),
        )
        .expect("external catalog default must be projected");

        assert_eq!(
            projection.default_offering_id.as_deref(),
            Some("model-beta")
        );
    }
}
