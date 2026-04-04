use async_trait::async_trait;

use super::service::EvaluationService;
use super::types::*;
use super::utils::ServiceResult;
use astra_core::internal_error;

pub struct UnconfiguredEvaluationService;

#[async_trait]
impl EvaluationService for UnconfiguredEvaluationService {
    async fn get_quality_trend(
        &self,
        _: &str,
        _: i32,
        _: Option<&str>,
    ) -> ServiceResult<QualityTrendResponse> {
        Err(internal_error("evaluation service not configured"))
    }
    async fn detect_drift(&self, _: &str) -> ServiceResult<DriftDetectResponse> {
        Err(internal_error("evaluation service not configured"))
    }
    async fn get_gate_history(&self, _: &str, _: i32) -> ServiceResult<GateHistoryResponse> {
        Err(internal_error("evaluation service not configured"))
    }
    async fn get_calibration(
        &self,
        _: &str,
        _: Option<&str>,
        _: i32,
    ) -> ServiceResult<CalibrationResponse> {
        Err(internal_error("evaluation service not configured"))
    }
    async fn get_session_scores(
        &self,
        _: &str,
        _: i32,
        _: f64,
    ) -> ServiceResult<SessionScoresListResponse> {
        Err(internal_error("evaluation service not configured"))
    }
    async fn validate_gate(
        &self,
        _: &str,
        _: GateValidateRequest,
    ) -> ServiceResult<GateValidateResponse> {
        Err(internal_error("evaluation service not configured"))
    }
    async fn run_drift_pipeline(&self, _: &str) -> ServiceResult<DriftPipelineResponse> {
        Err(internal_error("evaluation service not configured"))
    }
    async fn run_closed_loop(&self, _: &str, _: i32, _: bool) -> ServiceResult<ClosedLoopResponse> {
        Err(internal_error("evaluation service not configured"))
    }
    async fn trust_report(&self, _: &str, _: &str, _: i32) -> ServiceResult<TrustReportResponse> {
        Err(internal_error("evaluation service not configured"))
    }
    async fn slo_dashboard(&self, _: &str, _: i32) -> ServiceResult<SloDashboardResponse> {
        Err(internal_error("evaluation service not configured"))
    }
    async fn slo_history(&self, _: &str, _: &str, _: i32) -> ServiceResult<SloHistoryResponse> {
        Err(internal_error("evaluation service not configured"))
    }
    async fn observability_metrics(
        &self,
        _: &str,
        _: &str,
        _: i32,
    ) -> ServiceResult<ObservabilityMetricsResponse> {
        Err(internal_error("evaluation service not configured"))
    }
    async fn memory_health(&self, _: &str) -> ServiceResult<MemoryHealthResponse> {
        Err(internal_error("evaluation service not configured"))
    }
    async fn memory_metrics(&self, _: &str) -> ServiceResult<MemoryMetricsResponse> {
        Err(internal_error("evaluation service not configured"))
    }
    async fn extract_training_data(
        &self,
        _: &str,
        _: TrainingDataExtractRequest,
    ) -> ServiceResult<TrainingDataExtractResponse> {
        Err(internal_error("evaluation service not configured"))
    }
    async fn export_training_data(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> ServiceResult<TrainingDataExportResponse> {
        Err(internal_error("evaluation service not configured"))
    }
}
