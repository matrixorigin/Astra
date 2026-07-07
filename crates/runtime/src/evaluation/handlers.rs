use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};

use crate::AppState;
use astra_core::{ErrorResponse, error_response};
use astra_services::evaluation::types::*;

fn extract_user_id(headers: &HeaderMap) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    headers
        .get("x-user-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "Missing X-User-Id header"))
}

pub async fn quality_trend_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<QualityTrendQuery>,
) -> Result<Json<QualityTrendResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    let resp = state
        .evaluation_service
        .get_quality_trend(&user_id, q.days, q.model.as_deref())
        .await?;
    Ok(Json(resp))
}

pub async fn drift_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<DriftDetectResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    let resp = state.evaluation_service.detect_drift(&user_id).await?;
    Ok(Json(resp))
}

pub async fn gate_history_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<GateHistoryQuery>,
) -> Result<Json<GateHistoryResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    let resp = state
        .evaluation_service
        .get_gate_history(&user_id, q.limit)
        .await?;
    Ok(Json(resp))
}

pub async fn calibration_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<CalibrationQuery>,
) -> Result<Json<CalibrationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    let resp = state
        .evaluation_service
        .get_calibration(&user_id, q.agent_id.as_deref(), q.days)
        .await?;
    Ok(Json(resp))
}

pub async fn session_scores_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SessionScoresQuery>,
) -> Result<Json<SessionScoresListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    let resp = state
        .evaluation_service
        .get_session_scores(&user_id, q.limit, q.min_score)
        .await?;
    Ok(Json(resp))
}

pub async fn gate_validate_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<GateValidateRequest>,
) -> Result<Json<GateValidateResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    let resp = state
        .evaluation_service
        .validate_gate(&user_id, request)
        .await?;
    Ok(Json(resp))
}

pub async fn drift_run_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<DriftPipelineResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    let resp = state
        .evaluation_service
        .run_drift_pipeline(&user_id)
        .await?;
    Ok(Json(resp))
}

pub async fn closed_loop_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ClosedLoopQuery>,
) -> Result<Json<ClosedLoopResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    let resp = state
        .evaluation_service
        .run_closed_loop(&user_id, q.days, q.dry_run)
        .await?;
    Ok(Json(resp))
}

pub async fn trust_report_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<TrustReportQuery>,
) -> Result<Json<TrustReportResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    let resp = state
        .evaluation_service
        .trust_report(&user_id, &q.agent_id, q.days)
        .await?;
    Ok(Json(resp))
}

pub async fn slo_dashboard_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SloDashboardQuery>,
) -> Result<Json<SloDashboardResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    let resp = state
        .evaluation_service
        .slo_dashboard(&user_id, q.period_days)
        .await?;
    Ok(Json(resp))
}

pub async fn slo_history_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
    Query(q): Query<SloHistoryQuery>,
) -> Result<Json<SloHistoryResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    let resp = state
        .evaluation_service
        .slo_history(&user_id, &agent_id, q.days)
        .await?;
    Ok(Json(resp))
}

pub async fn observability_metrics_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ObservabilityQuery>,
) -> Result<Json<ObservabilityMetricsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    let resp = state
        .evaluation_service
        .observability_metrics(&user_id, &q.agent_id, q.days)
        .await?;
    Ok(Json(resp))
}

pub async fn memory_health_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<MemoryHealthResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    let resp = state.evaluation_service.memory_health(&user_id).await?;
    Ok(Json(resp))
}

pub async fn memory_metrics_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<MemoryMetricsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    let resp = state.evaluation_service.memory_metrics(&user_id).await?;
    Ok(Json(resp))
}

pub async fn training_data_extract_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<TrainingDataExtractRequest>,
) -> Result<Json<TrainingDataExtractResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    let resp = state
        .evaluation_service
        .extract_training_data(&user_id, request)
        .await?;
    Ok(Json(resp))
}

pub async fn training_data_export_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(dataset_id): Path<String>,
    Query(q): Query<ExportQuery>,
) -> Result<Json<TrainingDataExportResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = extract_user_id(&headers)?;
    let resp = state
        .evaluation_service
        .export_training_data(&user_id, &dataset_id, &q.format)
        .await?;
    Ok(Json(resp))
}
