use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};

use crate::AppState;
use astra_core::{ErrorResponse, error_response, internal_error};
use astra_services::evaluation::types::*;
use astra_services::evaluation::{
    DatabaseEvaluationPlanStore, DatabaseEvaluationProjectionStore,
    EvaluationExperimentCreateRequest, EvaluationExperimentPrepareRequest,
    EvaluationExperimentPrepareResponse, EvaluationExperimentRecord, EvaluationPersistenceError,
    EvaluationProjectionError, EvaluationReportArtifact, EvaluationReportQuery,
    build_report_artifact, validate_report_label,
};

fn extract_user_id(headers: &HeaderMap) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    headers
        .get("x-user-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "Missing X-User-Id header"))
}

fn map_evaluation_persistence_error(
    error: EvaluationPersistenceError,
) -> (StatusCode, Json<ErrorResponse>) {
    match error {
        EvaluationPersistenceError::InvalidInput(detail) => {
            error_response(StatusCode::BAD_REQUEST, detail)
        }
        EvaluationPersistenceError::Conflict(detail) => {
            error_response(StatusCode::CONFLICT, detail)
        }
        EvaluationPersistenceError::NotFound(detail) => {
            error_response(StatusCode::NOT_FOUND, detail)
        }
        other => internal_error(other),
    }
}

fn map_evaluation_projection_error(
    error: EvaluationProjectionError,
) -> (StatusCode, Json<ErrorResponse>) {
    match error {
        EvaluationProjectionError::Persistence(error) => map_evaluation_persistence_error(error),
        EvaluationProjectionError::Execution(error) => internal_error(error),
        EvaluationProjectionError::Conflict(detail) => error_response(StatusCode::CONFLICT, detail),
    }
}

fn evaluation_pool(
    state: &AppState,
) -> Result<astra_core::SharedPool, (StatusCode, Json<ErrorResponse>)> {
    state.shared_pool.clone().ok_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "evaluation database is not configured",
        )
    })
}

pub async fn create_experiment_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<EvaluationExperimentCreateRequest>,
) -> Result<(StatusCode, Json<EvaluationExperimentRecord>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    if request.spec.adapter_profile_version.is_some() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "adapter_profile_version is server-owned; use /evaluation/experiments/prepare",
        ));
    }
    let store = DatabaseEvaluationPlanStore::new(evaluation_pool(&state)?);
    let record = store
        .register_experiment(
            &user.user_id,
            &request.spec,
            &request.submission_idempotency_key,
        )
        .await
        .map_err(map_evaluation_persistence_error)?;
    Ok((StatusCode::OK, Json(record)))
}

pub async fn prepare_experiment_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<EvaluationExperimentPrepareRequest>,
) -> Result<
    (StatusCode, Json<EvaluationExperimentPrepareResponse>),
    (StatusCode, Json<ErrorResponse>),
> {
    let user = state.auth_service.current_user(&headers).await?;
    let response = super::prepare::prepare_experiment(&state, &user.user_id, request).await?;
    Ok((StatusCode::OK, Json(response)))
}

pub async fn start_trial_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((experiment_id, trial_id)): Path<(String, String)>,
    Json(request): Json<astra_services::evaluation::EvaluationTrialStartRequest>,
) -> Result<
    (
        StatusCode,
        Json<astra_services::evaluation::EvaluationTrialStartResponse>,
    ),
    (StatusCode, Json<ErrorResponse>),
> {
    let user = state.auth_service.current_user(&headers).await?;
    let response =
        super::start::start_trial(&state, &user.user_id, &experiment_id, &trial_id, request)
            .await?;
    Ok((StatusCode::ACCEPTED, Json(response)))
}

pub async fn get_experiment_projection_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(experiment_id): Path<String>,
) -> Result<
    Json<astra_services::evaluation::EvaluationExperimentProjection>,
    (StatusCode, Json<ErrorResponse>),
> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = DatabaseEvaluationProjectionStore::new(evaluation_pool(&state)?);
    let projection = store
        .load_experiment(&user.user_id, &experiment_id)
        .await
        .map_err(map_evaluation_projection_error)?;
    Ok(Json(projection))
}

pub async fn get_experiment_report_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(experiment_id): Path<String>,
    Query(query): Query<EvaluationReportQuery>,
) -> Result<Json<EvaluationReportArtifact>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    if let Some(label) = query.baseline_label.as_deref() {
        validate_report_label("baseline_label", label)
            .map_err(|detail| error_response(StatusCode::BAD_REQUEST, detail))?;
    }
    if let Some(label) = query.candidate_label.as_deref() {
        validate_report_label("candidate_label", label)
            .map_err(|detail| error_response(StatusCode::BAD_REQUEST, detail))?;
    }
    let store = DatabaseEvaluationProjectionStore::new(evaluation_pool(&state)?);
    let projection = store
        .load_experiment(&user.user_id, &experiment_id)
        .await
        .map_err(map_evaluation_projection_error)?;
    let baseline_label = query.baseline_label.unwrap_or_else(|| {
        projection
            .experiment
            .spec
            .target
            .baseline
            .revision_id
            .clone()
    });
    let candidate_label = query.candidate_label.unwrap_or_else(|| {
        projection
            .experiment
            .spec
            .target
            .candidate
            .revision_id
            .clone()
    });
    let report = build_report_artifact(
        &user.user_id,
        &projection.experiment,
        &projection
            .trials
            .iter()
            .filter_map(|trial| trial.observation.as_ref())
            .cloned()
            .collect::<Vec<_>>(),
        &projection.unavailable_trial_ids,
        baseline_label,
        candidate_label,
    )
    .map_err(|detail| error_response(StatusCode::CONFLICT, detail))?;
    Ok(Json(report))
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
