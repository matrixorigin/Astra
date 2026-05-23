pub use astra_services::jobs::*;

use crate::AppState;
use astra_core::ErrorResponse;
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};

pub async fn submit_job_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<JobSubmitRequest>,
) -> Result<Json<JobResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let job = state
        .job_service
        .submit_job(
            user.user_id,
            JobSubmitRequestData {
                job_type: request.job_type,
                inputs: request.inputs,
                gpu_required: request.gpu_required,
                timeout_seconds: request.timeout_seconds,
                conda_env: request.conda_env,
            },
        )
        .await?;
    Ok(Json(JobResponse::from(job)))
}

pub async fn get_job_handler(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<JobResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let job = state.job_service.get_job(job_id).await?;
    Ok(Json(JobResponse::from(job)))
}

pub async fn cancel_job_handler(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let job = state.job_service.cancel_job(job_id).await?;
    Ok(Json(
        serde_json::json!({"job_id": job.job_id, "status": "cancelled"}),
    ))
}

pub async fn job_webhook_handler(
    State(state): State<AppState>,
    Json(request): Json<JobWebhookRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let result = state
        .job_service
        .job_webhook(JobWebhookData {
            job_id: request.job_id,
            status: request.status,
            result: request.result,
            error: request.error,
        })
        .await?;
    Ok(Json(result))
}
