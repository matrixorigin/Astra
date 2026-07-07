use astra_services::models::*;
use serde::{Deserialize, Serialize};

use crate::AppState;
use astra_core::{ErrorResponse, error_response};
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
    let principal = state.auth_service.current_principal(&headers).await?;
    let user = principal.user;
    let is_admin = state.admin.authorizer.require_admin(&headers).await.is_ok();
    let models = state
        .model_service
        .list_models(user.user_id, is_admin)
        .await?;
    Ok(Json(
        models
            .into_iter()
            .map(ModelListItemResponse::from)
            .collect(),
    ))
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
) -> Result<Json<MemoryModelResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let matrixone = crate::matrix_cloud_runtime::matrix_settings_from_env().map_err(|e| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("MatrixOne configuration unavailable: {e}"),
        )
    })?;
    let pool_ref = state.shared_pool.as_ref().map(|sp| sp.get());
    let resolved = resolve_memory_models(&matrixone, &state.fernet_encryptor, pool_ref)
        .await
        .map_err(|e| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Memory model resolution failed: {e}"),
            )
        })?;
    let candidate_model_names: Vec<String> = resolved
        .iter()
        .map(|model| model.model_name.clone())
        .collect();
    let candidate_thinking_capabilities: Vec<Option<String>> = resolved
        .iter()
        .map(|model| model.thinking_capability.map(|c| c.as_db_str().to_string()))
        .collect();
    let model_name = candidate_model_names.first().cloned().ok_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "No active LLM model configured.",
        )
    })?;
    Ok(Json(MemoryModelResponse {
        model_name,
        candidate_model_names,
        candidate_thinking_capabilities,
    }))
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryModelResponse {
    pub model_name: String,
    #[serde(default)]
    pub candidate_model_names: Vec<String>,
    #[serde(default)]
    pub candidate_thinking_capabilities: Vec<Option<String>>,
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
