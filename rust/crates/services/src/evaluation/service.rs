use async_trait::async_trait;

use super::types::*;
use super::utils::ServiceResult;

#[async_trait]
pub trait EvaluationService: Send + Sync {
    async fn get_quality_trend(
        &self,
        user_id: &str,
        days: i32,
        model: Option<&str>,
    ) -> ServiceResult<QualityTrendResponse>;
    async fn detect_drift(&self, user_id: &str) -> ServiceResult<DriftDetectResponse>;
    async fn get_gate_history(
        &self,
        user_id: &str,
        limit: i32,
    ) -> ServiceResult<GateHistoryResponse>;
    async fn get_calibration(
        &self,
        user_id: &str,
        agent_id: Option<&str>,
        days: i32,
    ) -> ServiceResult<CalibrationResponse>;
    async fn get_session_scores(
        &self,
        user_id: &str,
        limit: i32,
        min_score: f64,
    ) -> ServiceResult<SessionScoresListResponse>;
    async fn record_session_quality_assessment(
        &self,
        user_id: &str,
        request: SessionQualityAssessmentRequest,
    ) -> ServiceResult<()>;
    async fn validate_gate(
        &self,
        user_id: &str,
        request: GateValidateRequest,
    ) -> ServiceResult<GateValidateResponse>;
    async fn run_drift_pipeline(&self, user_id: &str) -> ServiceResult<DriftPipelineResponse>;
    async fn run_closed_loop(
        &self,
        user_id: &str,
        days: i32,
        dry_run: bool,
    ) -> ServiceResult<ClosedLoopResponse>;
    async fn trust_report(
        &self,
        user_id: &str,
        agent_id: &str,
        days: i32,
    ) -> ServiceResult<TrustReportResponse>;
    async fn slo_dashboard(
        &self,
        user_id: &str,
        period_days: i32,
    ) -> ServiceResult<SloDashboardResponse>;
    async fn slo_history(
        &self,
        user_id: &str,
        agent_id: &str,
        days: i32,
    ) -> ServiceResult<SloHistoryResponse>;
    async fn observability_metrics(
        &self,
        user_id: &str,
        agent_id: &str,
        days: i32,
    ) -> ServiceResult<ObservabilityMetricsResponse>;
    async fn memory_health(&self, user_id: &str) -> ServiceResult<MemoryHealthResponse>;
    async fn memory_metrics(&self, user_id: &str) -> ServiceResult<MemoryMetricsResponse>;
    async fn extract_training_data(
        &self,
        user_id: &str,
        request: TrainingDataExtractRequest,
    ) -> ServiceResult<TrainingDataExtractResponse>;
    async fn export_training_data(
        &self,
        user_id: &str,
        dataset_id: &str,
        format: &str,
    ) -> ServiceResult<TrainingDataExportResponse>;
}
