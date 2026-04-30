use astra_services::models::*;

use crate::AppState;
use astra_core::ErrorResponse;
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
    let admin = state.admin_authorizer.require_admin(&headers).await?;

    // Auto-infer thinking_mode at load time. Priority order:
    //   1. Explicit `quirks.thinking_mode` in the request body (highest).
    //   2. Tags: "thinking" → "controllable", "reasoning" (without "thinking") → "native".
    let quirks = {
        let mut q = request.quirks.unwrap_or_default();
        if q.thinking_mode.is_none() {
            let has_thinking = request
                .tags
                .iter()
                .any(|t| t.eq_ignore_ascii_case("thinking"));
            let has_reasoning = request
                .tags
                .iter()
                .any(|t| t.eq_ignore_ascii_case("reasoning"));
            q.thinking_mode = if has_thinking {
                Some("controllable".into())
            } else if has_reasoning {
                Some("native".into())
            } else {
                None
            };
        }
        Some(q)
    };

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
                quirks,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(ModelResponse::from(model))))
}

pub async fn list_models_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ModelListItemResponse>>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let is_admin = state.admin_authorizer.require_admin(&headers).await.is_ok();
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

pub async fn update_model_handler(
    State(state): State<AppState>,
    Path(model_name): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ModelUpdateRequest>,
) -> Result<Json<ModelResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _admin = state.admin_authorizer.require_admin(&headers).await?;
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
    let _admin = state.admin_authorizer.require_admin(&headers).await?;
    state.model_service.delete_model(model_name).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn check_model_handler(
    State(state): State<AppState>,
    Path(model_name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ModelResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _admin = state.admin_authorizer.require_admin(&headers).await?;
    let model = state.model_service.check_model(model_name).await?;
    Ok(Json(ModelResponse::from(model)))
}
