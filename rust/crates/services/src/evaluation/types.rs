use astra_core::confidence::ConfidenceInterval;
use serde::{Deserialize, Serialize};

// ── Enums ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DriftSeverity {
    Critical,
    Warning,
    Info,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeType {
    Prompt,
    Skill,
    Config,
    Selector,
    ContextBudget,
    Knowledge,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoopAction {
    Retune,
    Alert,
    NoOp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Jsonl,
    Csv,
    Parquet,
}

// ── Request types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct QualityTrendQuery {
    #[serde(default = "default_days")]
    pub days: i32,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GateHistoryQuery {
    #[serde(default = "default_limit")]
    pub limit: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CalibrationQuery {
    pub agent_id: Option<String>,
    #[serde(default = "default_days")]
    pub days: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionScoresQuery {
    #[serde(default = "default_limit")]
    pub limit: i32,
    #[serde(default)]
    pub min_score: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GateValidateRequest {
    pub change_type: ChangeType,
    pub change_id: String,
    pub change_content: serde_json::Value,
    #[serde(default = "default_golden_count")]
    pub golden_session_count: i32,
    #[serde(default = "default_error_threshold")]
    pub error_rate_threshold: f64,
    #[serde(default = "default_score_regression")]
    pub score_regression_threshold: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClosedLoopQuery {
    #[serde(default = "default_days")]
    pub days: i32,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrustReportQuery {
    pub agent_id: String,
    #[serde(default = "default_days")]
    pub days: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SloDashboardQuery {
    #[serde(default = "default_days")]
    pub period_days: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SloHistoryQuery {
    #[serde(default = "default_days")]
    pub days: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObservabilityQuery {
    pub agent_id: String,
    #[serde(default = "default_days")]
    pub days: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrainingDataExtractRequest {
    #[serde(default = "default_days")]
    pub days: i32,
    #[serde(default = "default_min_quality")]
    pub min_quality: f64,
    #[serde(default = "default_extract_limit")]
    pub max_samples: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExportQuery {
    #[serde(default = "default_export_format")]
    pub format: String,
}

fn default_days() -> i32 {
    30
}
fn default_limit() -> i32 {
    50
}
fn default_golden_count() -> i32 {
    50
}
fn default_error_threshold() -> f64 {
    0.05
}
fn default_score_regression() -> f64 {
    -0.1
}
fn default_min_quality() -> f64 {
    0.7
}
fn default_extract_limit() -> i32 {
    1000
}
fn default_export_format() -> String {
    "jsonl".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ──────────────────────────────────────────────────────────
    // default value functions
    // ──────────────────────────────────────────────────────────

    #[test]
    fn default_values() {
        assert_eq!(default_days(), 30);
        assert_eq!(default_limit(), 50);
        assert_eq!(default_golden_count(), 50);
        assert!((default_error_threshold() - 0.05).abs() < 1e-9);
        assert!((default_score_regression() - (-0.1)).abs() < 1e-9);
        assert!((default_min_quality() - 0.7).abs() < 1e-9);
        assert_eq!(default_extract_limit(), 1000);
        assert_eq!(default_export_format(), "jsonl");
    }

    // ──────────────────────────────────────────────────────────
    // serde enum round-trips
    // ──────────────────────────────────────────────────────────

    #[test]
    fn drift_severity_roundtrip() {
        let json = serde_json::to_string(&DriftSeverity::Critical).unwrap();
        assert_eq!(json, r#""critical""#);
        let parsed: DriftSeverity = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, DriftSeverity::Critical);
    }

    #[test]
    fn change_type_roundtrip() {
        let json = serde_json::to_string(&ChangeType::Prompt).unwrap();
        assert_eq!(json, r#""prompt""#);
        let parsed: ChangeType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ChangeType::Prompt);
    }

    #[test]
    fn loop_action_roundtrip() {
        let json = serde_json::to_string(&LoopAction::NoOp).unwrap();
        assert_eq!(json, r#""no_op""#);
        let parsed: LoopAction = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, LoopAction::NoOp);
    }

    #[test]
    fn export_format_roundtrip() {
        for variant in [
            ExportFormat::Jsonl,
            ExportFormat::Csv,
            ExportFormat::Parquet,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let parsed: ExportFormat = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    // ──────────────────────────────────────────────────────────
    // query deserialization with defaults
    // ──────────────────────────────────────────────────────────

    #[test]
    fn quality_trend_query_defaults() {
        let q: QualityTrendQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.days, 30);
        assert!(q.model.is_none());
    }

    #[test]
    fn gate_history_query_defaults() {
        let q: GateHistoryQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.limit, 50);
    }

    #[test]
    fn session_scores_query_defaults() {
        let q: SessionScoresQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.limit, 50);
        assert_eq!(q.min_score, 0.0);
    }

    #[test]
    fn gate_validate_request_defaults() {
        let json = r#"{"change_type":"prompt","change_id":"c1","change_content":{}}"#;
        let req: GateValidateRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.golden_session_count, 50);
        assert!((req.error_rate_threshold - 0.05).abs() < 1e-9);
        assert!((req.score_regression_threshold - (-0.1)).abs() < 1e-9);
    }

    #[test]
    fn export_query_defaults() {
        let q: ExportQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.format, "jsonl");
    }

    #[test]
    fn training_data_extract_defaults() {
        let q: TrainingDataExtractRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(q.days, 30);
        assert!((q.min_quality - 0.7).abs() < 1e-9);
        assert_eq!(q.max_samples, 1000);
    }
}

// ── Response types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityTrendPoint {
    pub date: String,
    pub avg_score: f64,
    pub avg_score_interval: ConfidenceInterval,
    pub count: i64,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QualityTrendResponse {
    pub points: Vec<QualityTrendPoint>,
    pub overall_avg: f64,
    pub overall_avg_interval: ConfidenceInterval,
    pub total_events: i64,
    pub noise_filtered_overall_avg: f64,
    pub noise_filtered_overall_avg_interval: ConfidenceInterval,
    pub noise_filtered_total_events: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriftSignalResponse {
    pub model: String,
    pub template_id: Option<String>,
    pub current_avg: f64,
    pub current_avg_interval: ConfidenceInterval,
    pub previous_avg: f64,
    pub previous_avg_interval: ConfidenceInterval,
    pub delta: f64,
    pub noise_filtered_current_avg: f64,
    pub noise_filtered_current_avg_interval: ConfidenceInterval,
    pub noise_filtered_previous_avg: f64,
    pub noise_filtered_previous_avg_interval: ConfidenceInterval,
    pub noise_filtered_delta: f64,
    pub noise_filtered_sample_count: i64,
    pub severity: DriftSeverity,
    pub sample_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriftDetectResponse {
    pub signals: Vec<DriftSignalResponse>,
    pub checked_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GateResultResponse {
    pub gate_id: String,
    pub change_type: String,
    pub change_id: String,
    pub sessions_tested: i64,
    pub error_rate: f64,
    pub error_rate_interval: ConfidenceInterval,
    pub score_delta: f64,
    pub passed: bool,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GateHistoryResponse {
    pub gates: Vec<GateResultResponse>,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalibrationResponse {
    pub mean_confidence: f64,
    pub mean_confidence_interval: ConfidenceInterval,
    pub mean_quality: f64,
    pub mean_quality_interval: ConfidenceInterval,
    pub calibration_error: f64,
    pub bias: f64,
    pub sample_count: i64,
    pub adjustment_multiplier: f64,
    pub adjustment_reason: String,
    pub noise_filtered_mean_confidence: f64,
    pub noise_filtered_mean_confidence_interval: ConfidenceInterval,
    pub noise_filtered_mean_quality: f64,
    pub noise_filtered_mean_quality_interval: ConfidenceInterval,
    pub noise_filtered_calibration_error: f64,
    pub noise_filtered_bias: f64,
    pub noise_filtered_sample_count: i64,
    pub noise_filtered_adjustment_multiplier: f64,
    pub noise_filtered_adjustment_reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionScoreResponse {
    pub session_id: String,
    pub score: f64,
    pub score_interval: ConfidenceInterval,
    pub chain_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionScoresListResponse {
    pub sessions: Vec<SessionScoreResponse>,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct GateValidateResponse {
    pub gate_id: String,
    pub change_type: ChangeType,
    pub change_id: String,
    pub sessions_tested: i64,
    pub error_rate: f64,
    pub error_rate_interval: ConfidenceInterval,
    pub score_delta: f64,
    pub passed: bool,
    pub details: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriftPipelineResponse {
    pub run_id: String,
    pub signals_detected: usize,
    pub signals: Vec<DriftSignalResponse>,
    pub started_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoopDiagnosisItem {
    pub metric: String,
    pub value: f64,
    pub threshold: f64,
    pub action: LoopAction,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClosedLoopResponse {
    pub loop_id: String,
    pub dry_run: bool,
    pub diagnoses: Vec<LoopDiagnosisItem>,
    pub actions_taken: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrustReportResponse {
    pub agent_id: String,
    pub period_days: i32,
    pub total_checks: i64,
    pub safe_count: i64,
    pub trust_ratio: f64,
    pub trust_ratio_interval: ConfidenceInterval,
    pub hallucination_rate: f64,
    pub hallucination_rate_interval: ConfidenceInterval,
}

#[derive(Debug, Clone, Serialize)]
pub struct SloEntry {
    pub agent_id: String,
    pub slo_name: String,
    pub target: f64,
    pub actual: f64,
    pub actual_interval: ConfidenceInterval,
    pub met: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SloDashboardResponse {
    pub period_days: i32,
    pub agents: Vec<SloEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SloHistoryPoint {
    pub date: String,
    pub value: f64,
    pub value_interval: ConfidenceInterval,
    pub target: f64,
    pub met: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SloHistoryResponse {
    pub agent_id: String,
    pub days: i32,
    pub history: Vec<SloHistoryPoint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecisionMetrics {
    pub avg_quality: f64,
    pub avg_quality_interval: ConfidenceInterval,
    pub noise_filtered_avg_quality: f64,
    pub noise_filtered_avg_quality_interval: ConfidenceInterval,
    pub total_decisions: i64,
    pub total_quality_samples: i64,
    pub noise_filtered_quality_samples: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionMetrics {
    pub unique_sessions: i64,
    pub avg_turns_per_session: f64,
    pub noise_filtered_avg_turns_per_session: f64,
    pub noise_filtered_session_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillMetrics {
    pub total_invocations: i64,
    pub success_count: i64,
    pub success_rate: f64,
    pub success_rate_interval: ConfidenceInterval,
}

#[derive(Debug, Clone, Serialize)]
pub struct ObservabilityMetricsResponse {
    pub agent_id: String,
    pub period_days: i32,
    pub decision: DecisionMetrics,
    pub session: SessionMetrics,
    pub skill: SkillMetrics,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryHealthResponse {
    pub total_memories: i64,
    pub active_memories: i64,
    pub inactive_memories: i64,
    pub stale_working_memories: i64,
    pub orphaned_records: i64,
    pub healthy: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryMetricsResponse {
    pub total_memories: i64,
    pub avg_confidence: f64,
    pub avg_confidence_interval: ConfidenceInterval,
    pub noise_filtered_avg_confidence: f64,
    pub noise_filtered_avg_confidence_interval: ConfidenceInterval,
    pub noise_filtered_confidence_samples: i64,
    pub stale_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrainingDataExtractResponse {
    pub dataset_id: String,
    pub samples_extracted: i64,
    pub quality_threshold: f64,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrainingDataExportResponse {
    pub dataset_id: String,
    pub format: String,
    pub status: String,
    pub message: String,
    pub samples_exported: i64,
    pub content_type: String,
    pub content: String,
}
