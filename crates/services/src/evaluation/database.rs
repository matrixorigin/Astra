use async_trait::async_trait;
use axum::http::StatusCode;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::Value;
use sqlx::{Row, query};
use std::time::Duration;

use super::service::EvaluationService;
use super::types::*;
use super::utils::*;
use astra_core::{
    MatrixOneSettings, SharedPool, confidence::ConfidenceInterval, error_response, internal_error,
};

const MAX_EVALUATION_ROWS: i32 = 200;
const MAX_EVALUATION_DAYS: i32 = 365;
const MAX_EXTRACT_SAMPLES: i32 = 1000;
const DEFAULT_DRIFT_WINDOW_DAYS: i32 = 30;
const DRIFT_INFO_DELTA: f64 = 0.05;
const DRIFT_WARNING_DELTA: f64 = 0.10;
const DRIFT_CRITICAL_DELTA: f64 = 0.20;
const LOOP_QUALITY_THRESHOLD: f64 = 0.70;
const LOOP_DRIFT_DELTA_THRESHOLD: f64 = 0.10;
const TRUST_SLO_TARGET: f64 = 0.95;
const ZERO_IQR_NOISE_BAND: f64 = 0.05;
const SESSION_QUALITY_LEVEL: &str = "session";
const MEMORIA_CONNECT_TIMEOUT_SECS: u64 = 10;
const MEMORIA_REQUEST_TIMEOUT_SECS: u64 = 30;
const UPSERT_SESSION_QUALITY_ASSESSMENT_SQL: &str = "INSERT INTO eval_quality_assessments \
     (assessment_id, user_id, target_id, score, step_count, level) \
     VALUES (?, ?, ?, ?, ?, ?) \
     ON DUPLICATE KEY UPDATE \
       user_id = VALUES(user_id), \
       target_id = VALUES(target_id), \
       score = VALUES(score), \
       step_count = VALUES(step_count), \
       level = VALUES(level), \
       updated_at = CURRENT_TIMESTAMP(6)";

trait EvaluationRow {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error>;
    fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error>;
    fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error>;
    fn i8_column(&self, column: &str) -> Result<i8, sqlx::Error>;
    fn f64_column(&self, column: &str) -> Result<f64, sqlx::Error>;
    fn optional_f64_column(&self, column: &str) -> Result<Option<f64>, sqlx::Error>;
}

impl EvaluationRow for sqlx::mysql::MySqlRow {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
        self.try_get(column)
    }

    fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
        self.try_get(column)
    }

    fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
        self.try_get(column)
    }

    fn i8_column(&self, column: &str) -> Result<i8, sqlx::Error> {
        self.try_get(column)
    }

    fn f64_column(&self, column: &str) -> Result<f64, sqlx::Error> {
        self.try_get(column)
    }

    fn optional_f64_column(&self, column: &str) -> Result<Option<f64>, sqlx::Error> {
        self.try_get(column)
    }
}

fn evaluation_decode_error(
    context: &str,
    column: &str,
    error: impl std::fmt::Display,
) -> (StatusCode, axum::Json<astra_core::ErrorResponse>) {
    internal_error(format!(
        "evaluation {context} decode column `{column}`: {error}"
    ))
}

fn evaluation_row_string(
    row: &impl EvaluationRow,
    context: &str,
    column: &str,
) -> ServiceResult<String> {
    row.string_column(column)
        .map_err(|error| evaluation_decode_error(context, column, error))
}

fn evaluation_required_non_empty_string(
    row: &impl EvaluationRow,
    context: &str,
    column: &str,
) -> ServiceResult<String> {
    let value = evaluation_row_string(row, context, column)?;
    if value.trim().is_empty() {
        return Err(evaluation_decode_error(
            context,
            column,
            "expected non-empty string",
        ));
    }
    Ok(value)
}

fn evaluation_optional_non_empty_string(
    row: &impl EvaluationRow,
    context: &str,
    column: &str,
) -> ServiceResult<Option<String>> {
    let value = row
        .optional_string_column(column)
        .map_err(|error| evaluation_decode_error(context, column, error))?;
    if matches!(value.as_deref(), Some(value) if value.trim().is_empty()) {
        return Err(evaluation_decode_error(
            context,
            column,
            "expected optional string to be non-empty when present",
        ));
    }
    Ok(value)
}

fn evaluation_row_i64(row: &impl EvaluationRow, context: &str, column: &str) -> ServiceResult<i64> {
    row.i64_column(column)
        .map_err(|error| evaluation_decode_error(context, column, error))
}

fn evaluation_row_non_negative_i64(
    row: &impl EvaluationRow,
    context: &str,
    column: &str,
) -> ServiceResult<i64> {
    let value = evaluation_row_i64(row, context, column)?;
    if value < 0 {
        return Err(evaluation_decode_error(
            context,
            column,
            format!("expected non-negative integer, got {value}"),
        ));
    }
    Ok(value)
}

fn evaluation_row_bool_i8(
    row: &impl EvaluationRow,
    context: &str,
    column: &str,
) -> ServiceResult<bool> {
    let value = row
        .i8_column(column)
        .map_err(|error| evaluation_decode_error(context, column, error))?;
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(evaluation_decode_error(
            context,
            column,
            format!("expected boolean 0 or 1, got {value}"),
        )),
    }
}

fn evaluation_row_f64(row: &impl EvaluationRow, context: &str, column: &str) -> ServiceResult<f64> {
    row.f64_column(column)
        .map_err(|error| evaluation_decode_error(context, column, error))
}

fn evaluation_finite_f64(
    row: &impl EvaluationRow,
    context: &str,
    column: &str,
) -> ServiceResult<f64> {
    let value = evaluation_row_f64(row, context, column)?;
    if !value.is_finite() {
        return Err(evaluation_decode_error(
            context,
            column,
            format!("expected finite number, got {value}"),
        ));
    }
    Ok(value)
}

fn evaluation_score(row: &impl EvaluationRow, context: &str, column: &str) -> ServiceResult<f64> {
    let score = evaluation_finite_f64(row, context, column)?;
    if !(0.0..=1.0).contains(&score) {
        return Err(evaluation_decode_error(
            context,
            column,
            format!("expected score in 0..=1, got {score}"),
        ));
    }
    Ok(score)
}

fn evaluation_optional_score(
    row: &impl EvaluationRow,
    context: &str,
    column: &str,
) -> ServiceResult<Option<f64>> {
    let score = row
        .optional_f64_column(column)
        .map_err(|error| evaluation_decode_error(context, column, error))?;
    match score {
        Some(score) if !score.is_finite() || !(0.0..=1.0).contains(&score) => {
            Err(evaluation_decode_error(
                context,
                column,
                format!("expected optional score in 0..=1 when present, got {score}"),
            ))
        }
        score => Ok(score),
    }
}

fn evaluation_non_negative_pair(
    row: &impl EvaluationRow,
    context: &str,
    total_column: &str,
    part_column: &str,
) -> ServiceResult<(i64, i64)> {
    let total = evaluation_row_non_negative_i64(row, context, total_column)?;
    let part = evaluation_row_non_negative_i64(row, context, part_column)?;
    if part > total {
        return Err(evaluation_decode_error(
            context,
            part_column,
            format!("expected `{part_column}` <= `{total_column}`, got {part} > {total}"),
        ));
    }
    Ok((total, part))
}

fn quality_trend_point_from_row(
    row: &impl EvaluationRow,
    model: Option<String>,
) -> ServiceResult<QualityTrendPoint> {
    let context = "quality_trend_row";
    let avg_score = evaluation_score(row, context, "avg_score")?;
    let count = evaluation_row_non_negative_i64(row, context, "cnt")?;
    Ok(QualityTrendPoint {
        date: evaluation_required_non_empty_string(row, context, "dt")?,
        avg_score,
        avg_score_interval: sampled_confidence_interval(avg_score, count),
        count,
        model,
    })
}

fn quality_trend_score_from_row(row: &impl EvaluationRow) -> ServiceResult<f64> {
    evaluation_score(row, "quality_trend_score_row", "score")
}

fn drift_score_from_row(row: &impl EvaluationRow) -> ServiceResult<(String, String, f64)> {
    let context = "drift_score_row";
    let level = evaluation_required_non_empty_string(row, context, "level")?;
    let window_bucket = evaluation_required_non_empty_string(row, context, "window_bucket")?;
    if !matches!(window_bucket.as_str(), "current" | "previous") {
        return Err(evaluation_decode_error(
            context,
            "window_bucket",
            format!("expected `current` or `previous`, got `{window_bucket}`"),
        ));
    }
    let score = evaluation_score(row, context, "score")?;
    Ok((level, window_bucket, score))
}

fn gate_result_from_row(row: &impl EvaluationRow) -> ServiceResult<GateResultResponse> {
    let context = "gate_result_row";
    let sessions_tested = evaluation_row_non_negative_i64(row, context, "sessions_tested")?;
    let error_rate = evaluation_score(row, context, "error_rate")?;
    let score_delta = evaluation_finite_f64(row, context, "score_delta")?;
    Ok(GateResultResponse {
        gate_id: evaluation_required_non_empty_string(row, context, "gate_id")?,
        change_type: evaluation_required_non_empty_string(row, context, "change_type")?,
        change_id: evaluation_required_non_empty_string(row, context, "change_id")?,
        sessions_tested,
        error_rate,
        error_rate_interval: sampled_confidence_interval(error_rate, sessions_tested),
        score_delta,
        score_delta_interval: sampled_value_interval(score_delta, sessions_tested),
        passed: evaluation_row_bool_i8(row, context, "passed")?,
        created_at: evaluation_optional_non_empty_string(row, context, "created_at")?,
    })
}

fn calibration_sample_from_row(row: &impl EvaluationRow) -> ServiceResult<(f64, f64)> {
    let context = "calibration_sample_row";
    Ok((
        evaluation_score(row, context, "confidence")?,
        evaluation_score(row, context, "quality_score")?,
    ))
}

fn session_score_from_row(row: &impl EvaluationRow) -> ServiceResult<SessionScoreResponse> {
    let context = "session_score_row";
    let score = evaluation_score(row, context, "score")?;
    Ok(SessionScoreResponse {
        session_id: evaluation_required_non_empty_string(row, context, "target_id")?,
        score,
        score_interval: ConfidenceInterval::exact(score),
        chain_count: evaluation_row_non_negative_i64(row, context, "chain_count")?,
    })
}

fn gate_validation_score_from_row(row: &impl EvaluationRow) -> ServiceResult<f64> {
    evaluation_score(row, "gate_validation_score_row", "score")
}

fn trust_count_pair_from_row(row: &impl EvaluationRow, context: &str) -> ServiceResult<(i64, i64)> {
    evaluation_non_negative_pair(row, context, "total", "safe_cnt")
}

fn slo_entry_from_row(row: &impl EvaluationRow) -> ServiceResult<SloEntry> {
    let context = "slo_dashboard_row";
    let (total, safe) = trust_count_pair_from_row(row, context)?;
    let actual = trust_ratio(total, safe);
    Ok(SloEntry {
        agent_id: evaluation_required_non_empty_string(row, context, "agent_id")?,
        slo_name: "trust_ratio".into(),
        target: TRUST_SLO_TARGET,
        actual,
        actual_interval: sampled_confidence_interval(actual, total),
        met: actual >= TRUST_SLO_TARGET,
    })
}

fn slo_history_point_from_row(row: &impl EvaluationRow) -> ServiceResult<SloHistoryPoint> {
    let context = "slo_history_row";
    let (total, safe) = trust_count_pair_from_row(row, context)?;
    let value = trust_ratio(total, safe);
    Ok(SloHistoryPoint {
        date: evaluation_required_non_empty_string(row, context, "dt")?,
        value,
        value_interval: sampled_confidence_interval(value, total),
        target: TRUST_SLO_TARGET,
        met: value >= TRUST_SLO_TARGET,
    })
}

fn observability_turn_count_from_row(row: &impl EvaluationRow) -> ServiceResult<i64> {
    evaluation_row_non_negative_i64(row, "observability_turn_count_row", "turn_count")
}

fn observability_quality_score_from_row(row: &impl EvaluationRow) -> ServiceResult<f64> {
    evaluation_score(row, "observability_quality_row", "session_quality")
}

fn skill_metrics_from_row(row: &impl EvaluationRow) -> ServiceResult<SkillMetrics> {
    let context = "skill_metrics_row";
    let (total, success) = evaluation_non_negative_pair(row, context, "total", "ok_cnt")?;
    let success_rate = skill_success_rate(total, success);
    Ok(SkillMetrics {
        total_invocations: total,
        success_count: success,
        success_rate,
        success_rate_interval: sampled_confidence_interval(success_rate, total),
    })
}

fn training_sample_from_row(row: &impl EvaluationRow) -> ServiceResult<ExtractedTrainingSample> {
    let context = "training_sample_row";
    Ok(ExtractedTrainingSample {
        session_id: evaluation_required_non_empty_string(row, context, "session_id")?,
        quality_score: evaluation_score(row, context, "quality_score")?,
        step_count: evaluation_row_non_negative_i64(row, context, "step_count")?,
        avg_confidence: evaluation_optional_score(row, context, "avg_confidence")?,
        trace_count: evaluation_row_non_negative_i64(row, context, "trace_count")?,
        quality_updated_at: evaluation_optional_non_empty_string(
            row,
            context,
            "quality_updated_at",
        )?,
        latest_context_trace_at: evaluation_optional_non_empty_string(
            row,
            context,
            "latest_context_trace_at",
        )?,
    })
}

fn training_dataset_export_from_row(row: &impl EvaluationRow) -> ServiceResult<(String, i64)> {
    let context = "training_dataset_export_row";
    Ok((
        evaluation_required_non_empty_string(row, context, "dataset_json")?,
        evaluation_row_non_negative_i64(row, context, "sample_count")?,
    ))
}

fn clamp_eval_limit(limit: i32) -> i32 {
    limit.clamp(1, MAX_EVALUATION_ROWS)
}

fn clamp_eval_days(days: i32) -> i32 {
    days.clamp(1, MAX_EVALUATION_DAYS)
}

fn clamp_extract_limit(limit: i32) -> i32 {
    limit.clamp(1, MAX_EXTRACT_SAMPLES)
}

fn memoria_connect_timeout() -> Duration {
    Duration::from_secs(MEMORIA_CONNECT_TIMEOUT_SECS)
}

fn memoria_request_timeout() -> Duration {
    Duration::from_secs(MEMORIA_REQUEST_TIMEOUT_SECS)
}

fn memoria_http_client(headers: HeaderMap) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(memoria_connect_timeout())
        .timeout(memoria_request_timeout())
        .default_headers(headers)
        .build()
}

fn session_quality_assessment_id(session_id: &str) -> String {
    format!("session:{session_id}")
}

fn classify_drift_severity(delta: f64) -> Option<DriftSeverity> {
    let magnitude = delta.abs();
    if magnitude >= DRIFT_CRITICAL_DELTA {
        Some(DriftSeverity::Critical)
    } else if magnitude >= DRIFT_WARNING_DELTA {
        Some(DriftSeverity::Warning)
    } else if magnitude >= DRIFT_INFO_DELTA {
        Some(DriftSeverity::Info)
    } else {
        None
    }
}

fn average_score(scores: &[f64]) -> f64 {
    if scores.is_empty() {
        0.0
    } else {
        scores.iter().sum::<f64>() / scores.len() as f64
    }
}

fn build_drift_signal(
    model: String,
    template_id: Option<String>,
    current_scores: &[f64],
    previous_scores: &[f64],
) -> Option<DriftSignalResponse> {
    if current_scores.is_empty() || previous_scores.is_empty() {
        return None;
    }
    let current_avg = average_score(current_scores);
    let previous_avg = average_score(previous_scores);
    let delta = current_avg - previous_avg;
    let current_noise_filtered = noise_filtered_average(current_scores);
    let previous_noise_filtered = noise_filtered_average(previous_scores);
    let noise_filtered_delta = current_noise_filtered.average - previous_noise_filtered.average;
    classify_drift_severity(noise_filtered_delta).map(|severity| DriftSignalResponse {
        model,
        template_id,
        current_avg,
        current_avg_interval: sampled_confidence_interval(current_avg, current_scores.len() as i64),
        previous_avg,
        previous_avg_interval: sampled_confidence_interval(
            previous_avg,
            previous_scores.len() as i64,
        ),
        delta,
        delta_interval: sampled_value_interval(
            delta,
            current_scores.len().min(previous_scores.len()) as i64,
        ),
        noise_filtered_current_avg: current_noise_filtered.average,
        noise_filtered_current_avg_interval: sampled_confidence_interval(
            current_noise_filtered.average,
            current_noise_filtered.sample_count,
        ),
        noise_filtered_previous_avg: previous_noise_filtered.average,
        noise_filtered_previous_avg_interval: sampled_confidence_interval(
            previous_noise_filtered.average,
            previous_noise_filtered.sample_count,
        ),
        noise_filtered_delta,
        noise_filtered_delta_interval: sampled_value_interval(
            noise_filtered_delta,
            current_noise_filtered
                .sample_count
                .min(previous_noise_filtered.sample_count),
        ),
        noise_filtered_sample_count: current_noise_filtered
            .sample_count
            .min(previous_noise_filtered.sample_count),
        severity,
        sample_count: current_scores.len().min(previous_scores.len()) as i64,
    })
}

fn build_loop_actions(diagnoses: &[LoopDiagnosisItem], dry_run: bool) -> Vec<String> {
    diagnoses
        .iter()
        .filter_map(|diagnosis| match diagnosis.action {
            LoopAction::NoOp => None,
            LoopAction::Retune => Some(format!(
                "{}retune:{}",
                if dry_run { "dry_run:" } else { "" },
                diagnosis.metric
            )),
            LoopAction::Alert => Some(format!(
                "{}alert:{}",
                if dry_run { "dry_run:" } else { "" },
                diagnosis.metric
            )),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
struct GateValidationSummary {
    sessions_tested: i64,
    error_rate: f64,
    score_delta: f64,
    score_delta_interval: ValueInterval,
    passed: bool,
    details: String,
}

#[derive(Debug, Clone, PartialEq)]
struct CalibrationSummary {
    mean_confidence: f64,
    mean_confidence_interval: ConfidenceInterval,
    mean_quality: f64,
    mean_quality_interval: ConfidenceInterval,
    calibration_error: f64,
    calibration_error_interval: ValueInterval,
    bias: f64,
    bias_interval: ValueInterval,
    sample_count: i64,
    adjustment_multiplier: f64,
    adjustment_multiplier_interval: ValueInterval,
    adjustment_reason: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ExtractedTrainingDataRequest {
    days: i32,
    min_quality: f64,
    max_samples: i32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ExtractedTrainingSample {
    session_id: String,
    quality_score: f64,
    step_count: i64,
    avg_confidence: Option<f64>,
    trace_count: i64,
    quality_updated_at: Option<String>,
    latest_context_trace_at: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ExtractedTrainingDataset {
    schema_version: u32,
    extracted_at: String,
    request: ExtractedTrainingDataRequest,
    samples: Vec<ExtractedTrainingSample>,
}

#[derive(Debug, Clone, PartialEq)]
struct NoiseFilteredAverage {
    average: f64,
    sample_count: i64,
}

fn average_scores(scores: &[f64]) -> f64 {
    if scores.is_empty() {
        0.0
    } else {
        scores.iter().sum::<f64>() / scores.len() as f64
    }
}

fn sampled_confidence_interval(point: f64, sample_count: i64) -> ConfidenceInterval {
    if sample_count <= 0 {
        return ConfidenceInterval::ZERO;
    }
    let margin = (0.5 / (sample_count as f64).sqrt()).clamp(0.05, 0.25);
    ConfidenceInterval::symmetric(point.clamp(0.0, 1.0), margin)
}

fn sampled_value_interval(point: f64, sample_count: i64) -> ValueInterval {
    if sample_count <= 0 {
        return ValueInterval::ZERO;
    }
    let margin = (0.5 / (sample_count as f64).sqrt()).clamp(0.05, 0.25);
    ValueInterval::new(point, point - margin, point + margin)
}

fn confidence_to_value_interval(interval: ConfidenceInterval) -> ValueInterval {
    ValueInterval::new(interval.point, interval.lower, interval.upper)
}

fn absolute_value_interval(interval: ValueInterval) -> ValueInterval {
    let lower = if interval.lower <= 0.0 && interval.upper >= 0.0 {
        0.0
    } else {
        interval.lower.abs().min(interval.upper.abs())
    };
    let upper = interval.lower.abs().max(interval.upper.abs());
    ValueInterval::new(interval.point.abs(), lower, upper)
}

fn adjustment_multiplier_interval(
    calibration_error_interval: ValueInterval,
    bias_interval: ValueInterval,
    point: f64,
) -> ValueInterval {
    let mut lower = f64::INFINITY;
    let mut upper = f64::NEG_INFINITY;
    for error in [
        calibration_error_interval.lower,
        calibration_error_interval.point,
        calibration_error_interval.upper,
    ] {
        for bias in [
            bias_interval.lower,
            bias_interval.point,
            bias_interval.upper,
        ] {
            let candidate = compute_adjustment(error.abs(), bias).0;
            lower = lower.min(candidate);
            upper = upper.max(candidate);
        }
    }
    if !lower.is_finite() || !upper.is_finite() {
        ValueInterval::exact(point)
    } else {
        ValueInterval::new(point, lower, upper)
    }
}

fn numeric_mean_interval(samples: &[f64]) -> NumericInterval {
    if samples.is_empty() {
        return NumericInterval::ZERO;
    }
    let mean = average_scores(samples);
    if samples.len() == 1 {
        return NumericInterval::exact(mean);
    }
    let variance = samples
        .iter()
        .map(|value| {
            let delta = *value - mean;
            delta * delta
        })
        .sum::<f64>()
        / (samples.len() - 1) as f64;
    let std_error = variance.max(0.0).sqrt() / (samples.len() as f64).sqrt();
    let margin = 1.96 * std_error;
    NumericInterval::new(mean, mean - margin, mean + margin)
}

fn noise_filtered_indices(values: &[f64]) -> Vec<usize> {
    if values.is_empty() {
        return Vec::new();
    }
    if values.len() < 5 {
        return (0..values.len()).collect();
    }

    let mut sorted: Vec<(usize, f64)> = values.iter().copied().enumerate().collect();
    sorted.sort_by(|left, right| left.1.total_cmp(&right.1));
    let sorted_values: Vec<f64> = sorted.iter().map(|(_, value)| *value).collect();
    let q1 = percentile(&sorted_values, 0.25);
    let q3 = percentile(&sorted_values, 0.75);
    let iqr = q3 - q1;
    if iqr <= f64::EPSILON {
        let median = percentile(&sorted_values, 0.5);
        let mut indices: Vec<usize> = sorted
            .iter()
            .filter(|(_, value)| (*value - median).abs() <= ZERO_IQR_NOISE_BAND)
            .map(|(index, _)| *index)
            .collect();
        if indices.len() >= 3 && indices.len() < values.len() {
            indices.sort_unstable();
            return indices;
        }
        return (0..values.len()).collect();
    }

    let lower = q1 - 1.5 * iqr;
    let upper = q3 + 1.5 * iqr;
    let mut indices: Vec<usize> = sorted
        .into_iter()
        .filter(|(_, value)| *value >= lower && *value <= upper)
        .map(|(index, _)| index)
        .collect();
    if indices.len() < 3 {
        return (0..values.len()).collect();
    }

    indices.sort_unstable();
    indices
}

fn complement_interval(interval: ConfidenceInterval) -> ConfidenceInterval {
    ConfidenceInterval::new(
        1.0 - interval.point,
        1.0 - interval.upper,
        1.0 - interval.lower,
    )
}

fn change_type_label(change_type: &ChangeType) -> &'static str {
    match change_type {
        ChangeType::Prompt => "prompt",
        ChangeType::Skill => "skill",
        ChangeType::Config => "config",
        ChangeType::ToolSurface => "tool_surface",
        ChangeType::ContextBudget => "context_budget",
        ChangeType::Knowledge => "knowledge",
    }
}

fn summarize_calibration(
    mean_confidence: f64,
    mean_quality: f64,
    sample_count: i64,
) -> CalibrationSummary {
    if sample_count <= 0 {
        return CalibrationSummary {
            mean_confidence: 0.0,
            mean_confidence_interval: ConfidenceInterval::ZERO,
            mean_quality: 0.0,
            mean_quality_interval: ConfidenceInterval::ZERO,
            calibration_error: 0.0,
            calibration_error_interval: ValueInterval::ZERO,
            bias: 0.0,
            bias_interval: ValueInterval::ZERO,
            sample_count: 0,
            adjustment_multiplier: 1.0,
            adjustment_multiplier_interval: ValueInterval::ZERO,
            adjustment_reason: "No session calibration samples available.".into(),
        };
    }

    let mean_confidence = mean_confidence.clamp(0.0, 1.0);
    let mean_quality = mean_quality.clamp(0.0, 1.0);
    let calibration_error = compute_calibration_error(mean_confidence, mean_quality);
    let bias = mean_confidence - mean_quality;
    let (adjustment_multiplier, adjustment_reason) = compute_adjustment(calibration_error, bias);
    let calibration_error_interval = sampled_value_interval(calibration_error, sample_count);
    let bias_interval = sampled_value_interval(bias, sample_count);

    CalibrationSummary {
        mean_confidence,
        mean_confidence_interval: sampled_confidence_interval(mean_confidence, sample_count),
        mean_quality,
        mean_quality_interval: sampled_confidence_interval(mean_quality, sample_count),
        calibration_error,
        calibration_error_interval,
        bias,
        bias_interval,
        sample_count,
        adjustment_multiplier,
        adjustment_multiplier_interval: adjustment_multiplier_interval(
            calibration_error_interval,
            bias_interval,
            adjustment_multiplier,
        ),
        adjustment_reason,
    }
}

fn training_dataset_status(sample_count: usize) -> &'static str {
    if sample_count == 0 { "empty" } else { "ready" }
}

fn normalize_export_format(
    format: &str,
) -> Result<ExportFormat, (StatusCode, axum::Json<astra_core::ErrorResponse>)> {
    match format.trim().to_ascii_lowercase().as_str() {
        "jsonl" => Ok(ExportFormat::Jsonl),
        "csv" => Ok(ExportFormat::Csv),
        "parquet" => Ok(ExportFormat::Parquet),
        other => Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("Unsupported export format: {other}"),
        )),
    }
}

fn csv_escape(value: &str) -> String {
    if value.contains([',', '"', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn render_training_dataset_jsonl(
    dataset: &ExtractedTrainingDataset,
) -> Result<String, serde_json::Error> {
    let lines = dataset
        .samples
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(lines.join("\n"))
}

fn render_training_dataset_csv(dataset: &ExtractedTrainingDataset) -> String {
    let mut lines = vec![
        "session_id,quality_score,step_count,avg_confidence,trace_count,quality_updated_at,latest_context_trace_at".to_string(),
    ];
    lines.extend(dataset.samples.iter().map(|sample| {
        [
            csv_escape(&sample.session_id),
            sample.quality_score.to_string(),
            sample.step_count.to_string(),
            sample
                .avg_confidence
                .map(|value| value.to_string())
                .unwrap_or_default(),
            sample.trace_count.to_string(),
            csv_escape(sample.quality_updated_at.as_deref().unwrap_or_default()),
            csv_escape(
                sample
                    .latest_context_trace_at
                    .as_deref()
                    .unwrap_or_default(),
            ),
        ]
        .join(",")
    }));
    lines.join("\n")
}

fn percentile(sorted_scores: &[f64], fraction: f64) -> f64 {
    if sorted_scores.is_empty() {
        return 0.0;
    }
    if sorted_scores.len() == 1 {
        return sorted_scores[0];
    }

    let position = fraction.clamp(0.0, 1.0) * (sorted_scores.len() - 1) as f64;
    let lower_idx = position.floor() as usize;
    let upper_idx = position.ceil() as usize;
    if lower_idx == upper_idx {
        sorted_scores[lower_idx]
    } else {
        let lower = sorted_scores[lower_idx];
        let upper = sorted_scores[upper_idx];
        lower + (upper - lower) * (position - lower_idx as f64)
    }
}

fn noise_filtered_average(scores: &[f64]) -> NoiseFilteredAverage {
    if scores.is_empty() {
        return NoiseFilteredAverage {
            average: 0.0,
            sample_count: 0,
        };
    }
    let filtered_indices = noise_filtered_indices(scores);
    let filtered: Vec<f64> = filtered_indices
        .iter()
        .map(|index| scores[*index])
        .collect();

    NoiseFilteredAverage {
        average: average_scores(&filtered),
        sample_count: filtered.len() as i64,
    }
}

fn summarize_decision_metrics(total_decisions: i64, quality_scores: &[f64]) -> DecisionMetrics {
    let total_quality_samples = quality_scores.len() as i64;
    let avg_quality = average_scores(quality_scores);
    let noise_filtered = noise_filtered_average(quality_scores);
    DecisionMetrics {
        avg_quality,
        avg_quality_interval: sampled_confidence_interval(avg_quality, total_quality_samples),
        noise_filtered_avg_quality: noise_filtered.average,
        noise_filtered_avg_quality_interval: sampled_confidence_interval(
            noise_filtered.average,
            noise_filtered.sample_count,
        ),
        total_decisions,
        total_quality_samples,
        noise_filtered_quality_samples: noise_filtered.sample_count,
    }
}

fn summarize_session_metrics(turn_counts: &[f64]) -> SessionMetrics {
    let filtered_indices = noise_filtered_indices(turn_counts);
    let filtered_turn_counts: Vec<f64> = filtered_indices
        .iter()
        .map(|index| turn_counts[*index])
        .collect();
    let noise_filtered = noise_filtered_average(turn_counts);
    SessionMetrics {
        unique_sessions: turn_counts.len() as i64,
        avg_turns_per_session: average_scores(turn_counts),
        avg_turns_per_session_interval: numeric_mean_interval(turn_counts),
        noise_filtered_avg_turns_per_session: noise_filtered.average,
        noise_filtered_avg_turns_per_session_interval: numeric_mean_interval(&filtered_turn_counts),
        noise_filtered_session_count: noise_filtered.sample_count,
    }
}

fn weighted_average_scores(samples: &[(f64, i64)]) -> f64 {
    let total_weight: i64 = samples.iter().map(|(_, weight)| (*weight).max(0)).sum();
    if total_weight <= 0 {
        return 0.0;
    }
    samples
        .iter()
        .map(|(value, weight)| *value * (*weight).max(0) as f64)
        .sum::<f64>()
        / total_weight as f64
}

fn weighted_percentile(sorted_samples: &[(f64, i64)], fraction: f64) -> f64 {
    if sorted_samples.is_empty() {
        return 0.0;
    }
    let total_weight: i64 = sorted_samples
        .iter()
        .map(|(_, weight)| (*weight).max(0))
        .sum();
    if total_weight <= 0 {
        return sorted_samples
            .last()
            .map(|(value, _)| *value)
            .unwrap_or(0.0);
    }
    let target = fraction.clamp(0.0, 1.0) * (total_weight - 1) as f64;
    let mut cumulative = 0.0;
    for (value, weight) in sorted_samples {
        cumulative += (*weight).max(0) as f64;
        if cumulative > target {
            return *value;
        }
    }
    sorted_samples
        .last()
        .map(|(value, _)| *value)
        .unwrap_or(0.0)
}

fn weighted_noise_filtered_average(samples: &[(f64, i64)]) -> NoiseFilteredAverage {
    let normalized: Vec<(f64, i64)> = samples
        .iter()
        .copied()
        .filter(|(_, weight)| *weight > 0)
        .collect();
    if normalized.is_empty() {
        return NoiseFilteredAverage {
            average: 0.0,
            sample_count: 0,
        };
    }

    let raw = NoiseFilteredAverage {
        average: weighted_average_scores(&normalized),
        sample_count: normalized.iter().map(|(_, weight)| *weight).sum(),
    };
    if normalized.len() < 5 {
        return raw;
    }

    let mut sorted = normalized;
    sorted.sort_by(|left, right| left.0.total_cmp(&right.0));
    let q1 = weighted_percentile(&sorted, 0.25);
    let q3 = weighted_percentile(&sorted, 0.75);
    let iqr = q3 - q1;

    let filtered: Vec<(f64, i64)> = if iqr <= f64::EPSILON {
        let median = weighted_percentile(&sorted, 0.5);
        let retained: Vec<(f64, i64)> = sorted
            .iter()
            .copied()
            .filter(|(value, _)| (*value - median).abs() <= ZERO_IQR_NOISE_BAND)
            .collect();
        if retained.len() >= 3 && retained.len() < sorted.len() {
            retained
        } else {
            return raw;
        }
    } else {
        let lower = q1 - 1.5 * iqr;
        let upper = q3 + 1.5 * iqr;
        let retained: Vec<(f64, i64)> = sorted
            .into_iter()
            .filter(|(value, _)| *value >= lower && *value <= upper)
            .collect();
        if retained.len() >= 3 {
            retained
        } else {
            return raw;
        }
    };

    NoiseFilteredAverage {
        average: weighted_average_scores(&filtered),
        sample_count: filtered.iter().map(|(_, weight)| *weight).sum(),
    }
}

fn memory_confidence_samples(analyze: &Value) -> Vec<(f64, i64)> {
    analyze
        .as_object()
        .map(|by_type| {
            by_type
                .values()
                .filter_map(|stats| {
                    Some((
                        stats.get("avg_confidence").and_then(|v| v.as_f64())?,
                        stats.get("total").and_then(|v| v.as_i64())?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn summarize_memory_metrics(
    total_memories: i64,
    stale_count: i64,
    confidence_samples: &[(f64, i64)],
) -> MemoryMetricsResponse {
    let weighted_count: i64 = confidence_samples
        .iter()
        .map(|(_, weight)| (*weight).max(0))
        .sum();
    let avg_confidence = weighted_average_scores(confidence_samples);
    let noise_filtered = weighted_noise_filtered_average(confidence_samples);

    MemoryMetricsResponse {
        total_memories,
        avg_confidence,
        avg_confidence_interval: sampled_confidence_interval(avg_confidence, weighted_count),
        noise_filtered_avg_confidence: noise_filtered.average,
        noise_filtered_avg_confidence_interval: sampled_confidence_interval(
            noise_filtered.average,
            noise_filtered.sample_count,
        ),
        noise_filtered_confidence_samples: noise_filtered.sample_count,
        stale_count,
    }
}

fn summarize_calibration_samples(samples: &[(f64, f64)]) -> CalibrationSummary {
    let mean_confidence = average_scores(
        &samples
            .iter()
            .map(|(confidence, _)| *confidence)
            .collect::<Vec<_>>(),
    );
    let mean_quality = average_scores(
        &samples
            .iter()
            .map(|(_, quality)| *quality)
            .collect::<Vec<_>>(),
    );
    summarize_calibration(mean_confidence, mean_quality, samples.len() as i64)
}

fn noise_filtered_calibration_samples(samples: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let gaps: Vec<f64> = samples
        .iter()
        .map(|(confidence, quality)| (confidence - quality).abs())
        .collect();
    noise_filtered_indices(&gaps)
        .into_iter()
        .map(|index| samples[index])
        .collect()
}

fn normalize_model_filter(model: Option<&str>) -> Option<String> {
    model.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn quality_trend_query(model: Option<&str>) -> &'static str {
    if model.is_some() {
        "SELECT DATE_FORMAT(DATE(qa.created_at), '%Y-%m-%d') AS dt, \
         AVG(qa.score) AS avg_score, COUNT(*) AS cnt \
         FROM eval_quality_assessments qa \
         WHERE qa.user_id = ? \
           AND qa.level = 'session' \
           AND qa.created_at >= DATE_SUB(NOW(), INTERVAL ? DAY) \
           AND EXISTS ( \
               SELECT 1 FROM agent_events e \
               WHERE e.user_id = qa.user_id \
                 AND e.session_id = qa.target_id \
                 AND e.llm_model_used = ? \
           ) \
         GROUP BY dt ORDER BY dt"
    } else {
        "SELECT DATE_FORMAT(DATE(qa.created_at), '%Y-%m-%d') AS dt, \
         AVG(qa.score) AS avg_score, COUNT(*) AS cnt \
         FROM eval_quality_assessments qa \
         WHERE qa.user_id = ? \
           AND qa.created_at >= DATE_SUB(NOW(), INTERVAL ? DAY) \
         GROUP BY dt ORDER BY dt"
    }
}

fn quality_trend_scores_query(model: Option<&str>) -> &'static str {
    if model.is_some() {
        "SELECT qa.score AS score \
         FROM eval_quality_assessments qa \
         WHERE qa.user_id = ? \
           AND qa.level = 'session' \
           AND qa.created_at >= DATE_SUB(NOW(), INTERVAL ? DAY) \
           AND EXISTS ( \
               SELECT 1 FROM agent_events e \
               WHERE e.user_id = qa.user_id \
                 AND e.session_id = qa.target_id \
                 AND e.llm_model_used = ? \
           ) \
         ORDER BY qa.created_at"
    } else {
        "SELECT qa.score AS score \
         FROM eval_quality_assessments qa \
         WHERE qa.user_id = ? \
           AND qa.created_at >= DATE_SUB(NOW(), INTERVAL ? DAY) \
         ORDER BY qa.created_at"
    }
}

fn summarize_gate_validation(
    scores_desc: &[f64],
    golden_session_count: i32,
    error_rate_threshold: f64,
    score_regression_threshold: f64,
) -> GateValidationSummary {
    let window = clamp_eval_limit(golden_session_count) as usize;
    let error_rate_threshold = error_rate_threshold.clamp(0.0, 1.0);
    let recent_end = scores_desc.len().min(window);
    let recent = &scores_desc[..recent_end];
    if recent.is_empty() {
        return GateValidationSummary {
            sessions_tested: 0,
            error_rate: 0.0,
            score_delta: 0.0,
            score_delta_interval: ValueInterval::ZERO,
            passed: false,
            details: "No session quality scores available for gate validation.".into(),
        };
    }

    let baseline_end = scores_desc.len().min(window * 2);
    let baseline = &scores_desc[recent_end..baseline_end];
    let recent_filtered = noise_filtered_average(recent);
    let baseline_filtered = noise_filtered_average(baseline);
    let recent_avg = recent_filtered.average;
    let baseline_avg = baseline_filtered.average;
    let error_count = recent
        .iter()
        .filter(|score| **score < LOOP_QUALITY_THRESHOLD)
        .count();
    let error_rate = error_count as f64 / recent.len() as f64;
    let score_delta = if baseline.is_empty() {
        0.0
    } else {
        recent_avg - baseline_avg
    };
    let score_delta_interval = if baseline.is_empty() {
        ValueInterval::ZERO
    } else {
        sampled_value_interval(
            score_delta,
            recent_filtered
                .sample_count
                .min(baseline_filtered.sample_count),
        )
    };

    let error_ok = error_rate <= error_rate_threshold;
    let score_ok = baseline.is_empty() || score_delta >= score_regression_threshold;
    let passed = error_ok && score_ok;

    let mut reasons = Vec::new();
    if !error_ok {
        reasons.push(format!(
            "error rate {:.1}% exceeded {:.1}%",
            error_rate * 100.0,
            error_rate_threshold * 100.0
        ));
    }
    if !score_ok {
        reasons.push(format!(
            "score delta {:.3} below {:.3}",
            score_delta, score_regression_threshold
        ));
    }

    let filtering_note = if baseline.is_empty() {
        if recent_filtered.sample_count < recent.len() as i64 {
            format!(
                " Noise filter kept {} of {} recent scores.",
                recent_filtered.sample_count,
                recent.len()
            )
        } else {
            String::new()
        }
    } else if recent_filtered.sample_count < recent.len() as i64
        || baseline_filtered.sample_count < baseline.len() as i64
    {
        format!(
            " Noise filter kept {} of {} recent and {} of {} baseline scores.",
            recent_filtered.sample_count,
            recent.len(),
            baseline_filtered.sample_count,
            baseline.len()
        )
    } else {
        String::new()
    };

    let details = if baseline.is_empty() {
        format!(
            "{} Validated {} recent session scores with no baseline window; recent avg {:.3}, error rate {:.1}% (threshold {:.1}%).{}",
            if passed {
                "Gate passed."
            } else {
                "Gate failed."
            },
            recent.len(),
            recent_avg,
            error_rate * 100.0,
            error_rate_threshold * 100.0,
            filtering_note
        )
    } else {
        format!(
            "{} Validated {} recent vs {} baseline session scores; recent avg {:.3}, baseline avg {:.3}, delta {:.3} (threshold {:.3}), error rate {:.1}% (threshold {:.1}%).{}{}",
            if passed {
                "Gate passed."
            } else {
                "Gate failed."
            },
            recent.len(),
            baseline.len(),
            recent_avg,
            baseline_avg,
            score_delta,
            score_regression_threshold,
            error_rate * 100.0,
            error_rate_threshold * 100.0,
            filtering_note,
            if reasons.is_empty() {
                String::new()
            } else {
                format!(" Reasons: {}.", reasons.join("; "))
            }
        )
    };

    GateValidationSummary {
        sessions_tested: recent.len() as i64,
        error_rate,
        score_delta,
        score_delta_interval,
        passed,
        details,
    }
}

#[derive(Clone, Debug)]
pub struct DatabaseEvaluationService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
    memoria_base_url: Option<String>,
    memoria_master_key: Option<String>,
}

impl DatabaseEvaluationService {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
            memoria_base_url: None,
            memoria_master_key: None,
        }
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    pub fn with_memoria_config(
        mut self,
        base_url: impl Into<String>,
        master_key: Option<String>,
    ) -> Self {
        self.memoria_base_url = Some(base_url.into());
        self.memoria_master_key = master_key.filter(|key| !key.is_empty());
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        crate::require_shared_pool(
            self.pool.as_ref(),
            "DatabaseEvaluationService",
            &self.matrixone,
        )
    }

    async fn memoria_get(&self, endpoint: &str, user_id: &str) -> ServiceResult<Value> {
        let base_url = self.memoria_base_url.as_deref().ok_or_else(|| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Memoria not configured on server",
            )
        })?;
        let master_key = self.memoria_master_key.as_deref().ok_or_else(|| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Memoria not configured on server",
            )
        })?;

        let url = format!("{}{}", base_url.trim_end_matches('/'), endpoint);
        let mut headers = HeaderMap::new();
        headers.insert(
            "X-User-Id",
            HeaderValue::from_str(user_id).map_err(internal_error)?,
        );
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {master_key}")).map_err(internal_error)?,
        );

        let client = memoria_http_client(headers).map_err(internal_error)?;

        let response = client.get(url).send().await.map_err(internal_error)?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(error_response(
                StatusCode::BAD_GATEWAY,
                format!("Memoria request failed ({status}): {body}"),
            ));
        }

        response.json::<Value>().await.map_err(internal_error)
    }
}

#[async_trait]
impl EvaluationService for DatabaseEvaluationService {
    async fn get_quality_trend(
        &self,
        user_id: &str,
        days: i32,
        model: Option<&str>,
    ) -> ServiceResult<QualityTrendResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let days = clamp_eval_days(days);
        let normalized_model = normalize_model_filter(model);

        let mut trend_query = query(quality_trend_query(normalized_model.as_deref()))
            .bind(user_id)
            .bind(days);
        if let Some(ref model) = normalized_model {
            trend_query = trend_query.bind(model);
        }
        let rows = trend_query.fetch_all(&pool).await.map_err(internal_error)?;

        let mut scores_query = query(quality_trend_scores_query(normalized_model.as_deref()))
            .bind(user_id)
            .bind(days);
        if let Some(ref model) = normalized_model {
            scores_query = scores_query.bind(model);
        }
        let raw_scores: Vec<f64> = scores_query
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?
            .into_iter()
            .map(|row| quality_trend_score_from_row(&row))
            .collect::<ServiceResult<_>>()?;
        let noise_filtered = noise_filtered_average(&raw_scores);

        let points: Vec<QualityTrendPoint> = rows
            .iter()
            .map(|row| quality_trend_point_from_row(row, normalized_model.clone()))
            .collect::<ServiceResult<_>>()?;

        let total_events: i64 = points.iter().map(|p| p.count).sum();
        let overall_avg = compute_overall_avg(&points);

        Ok(QualityTrendResponse {
            points,
            overall_avg,
            overall_avg_interval: sampled_confidence_interval(overall_avg, total_events),
            total_events,
            noise_filtered_overall_avg: noise_filtered.average,
            noise_filtered_overall_avg_interval: sampled_confidence_interval(
                noise_filtered.average,
                noise_filtered.sample_count,
            ),
            noise_filtered_total_events: noise_filtered.sample_count,
        })
    }

    async fn detect_drift(&self, user_id: &str) -> ServiceResult<DriftDetectResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let window_days = clamp_eval_days(DEFAULT_DRIFT_WINDOW_DAYS);
        let lookback_days = window_days * 2;

        let rows = query(
            "SELECT level, \
             CASE WHEN created_at >= DATE_SUB(NOW(), INTERVAL ? DAY) THEN 'current' ELSE 'previous' END AS window_bucket, \
             score \
             FROM eval_quality_assessments \
             WHERE user_id = ? \
               AND created_at >= DATE_SUB(NOW(), INTERVAL ? DAY) \
             ORDER BY level, created_at DESC",
        )
        .bind(window_days)
        .bind(user_id)
        .bind(lookback_days)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let mut scores_by_level = std::collections::BTreeMap::<String, (Vec<f64>, Vec<f64>)>::new();
        for row in &rows {
            let (level, window_bucket, score) = drift_score_from_row(row)?;
            let entry = scores_by_level.entry(level).or_default();
            if window_bucket == "current" {
                entry.0.push(score);
            } else {
                entry.1.push(score);
            }
        }

        let signals = scores_by_level
            .into_iter()
            .filter_map(|(level, (current_scores, previous_scores))| {
                build_drift_signal(level, None, &current_scores, &previous_scores)
            })
            .collect();

        Ok(DriftDetectResponse {
            signals,
            checked_at: now_iso(),
        })
    }

    async fn get_gate_history(
        &self,
        user_id: &str,
        limit: i32,
    ) -> ServiceResult<GateHistoryResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let limit = clamp_eval_limit(limit);

        let rows = query(
            "SELECT gate_id, change_type, change_id, sessions_tested, \
             error_rate, score_delta, passed, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM eval_gate_results \
             WHERE user_id = ? \
             ORDER BY created_at DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let gates: Vec<GateResultResponse> = rows
            .iter()
            .map(gate_result_from_row)
            .collect::<ServiceResult<_>>()?;
        let total = gates.len();
        Ok(GateHistoryResponse { gates, total })
    }

    async fn get_calibration(
        &self,
        user_id: &str,
        agent_id: Option<&str>,
        days: i32,
    ) -> ServiceResult<CalibrationResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let days = clamp_eval_days(days);

        let sql = if agent_id.is_some() {
            "SELECT CAST(ca.confidence AS DOUBLE) AS confidence, \
                    CAST(ca.quality_score AS DOUBLE) AS quality_score \
             FROM eval_calibration_assessments ca \
             INNER JOIN eval_quality_assessments qa \
               ON qa.user_id = ca.user_id \
              AND qa.target_id = ca.session_id \
              AND qa.level = 'session' \
             WHERE ca.user_id = ? \
               AND ca.agent_id = ? \
               AND ca.created_at >= DATE_SUB(NOW(), INTERVAL ? DAY)"
        } else {
            "SELECT CAST(ca.confidence AS DOUBLE) AS confidence, \
                    CAST(ca.quality_score AS DOUBLE) AS quality_score \
             FROM eval_calibration_assessments ca \
             INNER JOIN eval_quality_assessments qa \
               ON qa.user_id = ca.user_id \
              AND qa.target_id = ca.session_id \
              AND qa.level = 'session' \
             WHERE ca.user_id = ? \
               AND ca.created_at >= DATE_SUB(NOW(), INTERVAL ? DAY)"
        };

        let mut query_builder = query(sql).bind(user_id);
        if let Some(agent) = agent_id {
            query_builder = query_builder.bind(agent);
        }
        query_builder = query_builder.bind(days);

        let rows = query_builder
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let samples: Vec<(f64, f64)> = rows
            .iter()
            .map(calibration_sample_from_row)
            .collect::<ServiceResult<_>>()?;

        let summary = summarize_calibration_samples(&samples);
        let noise_filtered_summary =
            summarize_calibration_samples(&noise_filtered_calibration_samples(&samples));

        Ok(CalibrationResponse {
            mean_confidence: summary.mean_confidence,
            mean_confidence_interval: summary.mean_confidence_interval,
            mean_quality: summary.mean_quality,
            mean_quality_interval: summary.mean_quality_interval,
            calibration_error: summary.calibration_error,
            calibration_error_interval: summary.calibration_error_interval,
            bias: summary.bias,
            bias_interval: summary.bias_interval,
            sample_count: summary.sample_count,
            adjustment_multiplier: summary.adjustment_multiplier,
            adjustment_multiplier_interval: summary.adjustment_multiplier_interval,
            adjustment_reason: summary.adjustment_reason,
            noise_filtered_mean_confidence: noise_filtered_summary.mean_confidence,
            noise_filtered_mean_confidence_interval: noise_filtered_summary
                .mean_confidence_interval,
            noise_filtered_mean_quality: noise_filtered_summary.mean_quality,
            noise_filtered_mean_quality_interval: noise_filtered_summary.mean_quality_interval,
            noise_filtered_calibration_error: noise_filtered_summary.calibration_error,
            noise_filtered_calibration_error_interval: noise_filtered_summary
                .calibration_error_interval,
            noise_filtered_bias: noise_filtered_summary.bias,
            noise_filtered_bias_interval: noise_filtered_summary.bias_interval,
            noise_filtered_sample_count: noise_filtered_summary.sample_count,
            noise_filtered_adjustment_multiplier: noise_filtered_summary.adjustment_multiplier,
            noise_filtered_adjustment_multiplier_interval: noise_filtered_summary
                .adjustment_multiplier_interval,
            noise_filtered_adjustment_reason: noise_filtered_summary.adjustment_reason,
        })
    }

    async fn get_session_scores(
        &self,
        user_id: &str,
        limit: i32,
        min_score: f64,
    ) -> ServiceResult<SessionScoresListResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let limit = clamp_eval_limit(limit);

        let rows = query(
            "SELECT target_id, score, COALESCE(step_count, 0) AS chain_count \
             FROM eval_quality_assessments \
             WHERE user_id = ? \
               AND level = 'session' AND score >= ? \
             ORDER BY updated_at DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(min_score)
        .bind(limit)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let sessions: Vec<SessionScoreResponse> = rows
            .iter()
            .map(session_score_from_row)
            .collect::<ServiceResult<_>>()?;
        let total = sessions.len();
        Ok(SessionScoresListResponse { sessions, total })
    }

    async fn record_session_quality_assessment(
        &self,
        user_id: &str,
        request: SessionQualityAssessmentRequest,
    ) -> ServiceResult<()> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let assessment_id = session_quality_assessment_id(&request.session_id);
        let score = request.score.clamp(0.0, 1.0);
        let step_count = request.step_count.max(0);

        query(UPSERT_SESSION_QUALITY_ASSESSMENT_SQL)
            .bind(&assessment_id)
            .bind(user_id)
            .bind(&request.session_id)
            .bind(score)
            .bind(step_count)
            .bind(SESSION_QUALITY_LEVEL)
            .execute(&pool)
            .await
            .map_err(internal_error)?;

        Ok(())
    }

    async fn validate_gate(
        &self,
        user_id: &str,
        request: GateValidateRequest,
    ) -> ServiceResult<GateValidateResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let sample_limit = clamp_eval_limit(request.golden_session_count) * 2;
        let rows = query(
            "SELECT score \
             FROM eval_quality_assessments \
             WHERE user_id = ? \
               AND level = 'session' \
             ORDER BY updated_at DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(sample_limit)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let scores = rows
            .iter()
            .map(gate_validation_score_from_row)
            .collect::<ServiceResult<Vec<_>>>()?;
        let summary = summarize_gate_validation(
            &scores,
            request.golden_session_count,
            request.error_rate_threshold,
            request.score_regression_threshold,
        );
        let gate_id = uuid::Uuid::now_v7().to_string();
        let change_type = request.change_type.clone();

        query(
            "INSERT INTO eval_gate_results \
             (gate_id, user_id, change_type, change_id, sessions_tested, error_rate, score_delta, passed) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&gate_id)
        .bind(user_id)
        .bind(change_type_label(&change_type))
        .bind(&request.change_id)
        .bind(summary.sessions_tested)
        .bind(summary.error_rate)
        .bind(summary.score_delta)
        .bind(if summary.passed { 1_i8 } else { 0_i8 })
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        Ok(GateValidateResponse {
            gate_id,
            change_type,
            change_id: request.change_id,
            sessions_tested: summary.sessions_tested,
            error_rate: summary.error_rate,
            error_rate_interval: sampled_confidence_interval(
                summary.error_rate,
                summary.sessions_tested,
            ),
            score_delta: summary.score_delta,
            score_delta_interval: summary.score_delta_interval,
            passed: summary.passed,
            details: summary.details,
        })
    }

    async fn run_drift_pipeline(&self, user_id: &str) -> ServiceResult<DriftPipelineResponse> {
        let drift = self.detect_drift(user_id).await?;
        let started_at = drift.checked_at.clone();
        let run_id = format!("drift-{}", started_at.replace(['-', 'T', ':'], ""));
        Ok(DriftPipelineResponse {
            run_id,
            signals_detected: drift.signals.len(),
            signals: drift.signals,
            started_at,
        })
    }

    async fn run_closed_loop(
        &self,
        user_id: &str,
        days: i32,
        dry_run: bool,
    ) -> ServiceResult<ClosedLoopResponse> {
        let days = clamp_eval_days(days);
        let quality = self.get_quality_trend(user_id, days, None).await?;
        let drift = self.detect_drift(user_id).await?;

        let (max_drift_delta, max_drift_delta_interval) =
            drift
                .signals
                .iter()
                .fold((0.0_f64, ValueInterval::ZERO), |best, signal| {
                    let candidate = signal.noise_filtered_delta.abs();
                    if candidate > best.0 {
                        (
                            candidate,
                            absolute_value_interval(signal.noise_filtered_delta_interval),
                        )
                    } else {
                        best
                    }
                });

        let quality_signal = if quality.noise_filtered_total_events > 0 {
            quality.noise_filtered_overall_avg
        } else {
            quality.overall_avg
        };
        let quality_signal_interval = if quality.noise_filtered_total_events > 0 {
            confidence_to_value_interval(quality.noise_filtered_overall_avg_interval)
        } else {
            confidence_to_value_interval(quality.overall_avg_interval)
        };

        let quality_action =
            if quality.noise_filtered_total_events > 0 && quality_signal < LOOP_QUALITY_THRESHOLD {
                LoopAction::Retune
            } else {
                LoopAction::NoOp
            };
        let drift_count_action = if drift.signals.iter().any(|signal| {
            matches!(
                signal.severity,
                DriftSeverity::Critical | DriftSeverity::Warning
            )
        }) {
            LoopAction::Alert
        } else if !drift.signals.is_empty() {
            LoopAction::Retune
        } else {
            LoopAction::NoOp
        };
        let drift_delta_action = if max_drift_delta >= DRIFT_CRITICAL_DELTA {
            LoopAction::Alert
        } else if max_drift_delta >= LOOP_DRIFT_DELTA_THRESHOLD {
            LoopAction::Retune
        } else {
            LoopAction::NoOp
        };

        let diagnoses = vec![
            LoopDiagnosisItem {
                metric: "quality_overall_avg".into(),
                value: quality_signal,
                value_interval: quality_signal_interval,
                threshold: LOOP_QUALITY_THRESHOLD,
                action: quality_action,
            },
            LoopDiagnosisItem {
                metric: "drift_signal_count".into(),
                value: drift.signals.len() as f64,
                value_interval: ValueInterval::exact(drift.signals.len() as f64),
                threshold: 0.0,
                action: drift_count_action,
            },
            LoopDiagnosisItem {
                metric: "drift_max_delta".into(),
                value: max_drift_delta,
                value_interval: max_drift_delta_interval,
                threshold: LOOP_DRIFT_DELTA_THRESHOLD,
                action: drift_delta_action,
            },
        ];

        let loop_id = format!("loop-{}", now_iso().replace(['-', 'T', ':'], ""));
        let actions_taken = build_loop_actions(&diagnoses, dry_run);

        Ok(ClosedLoopResponse {
            loop_id,
            dry_run,
            diagnoses,
            actions_taken,
        })
    }

    async fn trust_report(
        &self,
        user_id: &str,
        agent_id: &str,
        days: i32,
    ) -> ServiceResult<TrustReportResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let days = clamp_eval_days(days);

        let row = query(
            "SELECT COUNT(*) AS total, \
             CAST(COALESCE(SUM(CASE WHEN JSON_UNQUOTE(JSON_EXTRACT(content, '$.safe_to_deliver')) = 'true' \
                  THEN 1 ELSE 0 END), 0) AS SIGNED) AS safe_cnt \
             FROM agent_events \
             WHERE user_id = ? \
               AND event_type = 'hallucination_check' \
               AND agent_id = ? \
               AND created_at > DATE_SUB(NOW(), INTERVAL ? DAY)",
        )
        .bind(user_id)
        .bind(agent_id)
        .bind(days)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;

        let (total, safe) = trust_count_pair_from_row(&row, "trust_report_row")?;

        let ratio = trust_ratio(total, safe);
        let trust_ratio_interval = sampled_confidence_interval(ratio, total);
        Ok(TrustReportResponse {
            agent_id: agent_id.to_string(),
            period_days: days,
            total_checks: total,
            safe_count: safe,
            trust_ratio: ratio,
            trust_ratio_interval,
            hallucination_rate: 1.0 - ratio,
            hallucination_rate_interval: complement_interval(trust_ratio_interval),
        })
    }

    async fn slo_dashboard(
        &self,
        user_id: &str,
        period_days: i32,
    ) -> ServiceResult<SloDashboardResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let period_days = clamp_eval_days(period_days);

        let rows = query(
            "SELECT agent_id, COUNT(*) AS total, \
             CAST(COALESCE(SUM(CASE WHEN JSON_UNQUOTE(JSON_EXTRACT(content, '$.safe_to_deliver')) = 'true' \
                  THEN 1 ELSE 0 END), 0) AS SIGNED) AS safe_cnt \
             FROM agent_events \
             WHERE user_id = ? \
               AND event_type = 'hallucination_check' \
               AND agent_id IS NOT NULL \
               AND created_at > DATE_SUB(NOW(), INTERVAL ? DAY) \
             GROUP BY agent_id \
             ORDER BY agent_id",
        )
        .bind(user_id)
        .bind(period_days)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let agents = rows
            .iter()
            .map(slo_entry_from_row)
            .collect::<ServiceResult<_>>()?;

        Ok(SloDashboardResponse {
            period_days,
            agents,
        })
    }

    async fn slo_history(
        &self,
        user_id: &str,
        agent_id: &str,
        days: i32,
    ) -> ServiceResult<SloHistoryResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let days = clamp_eval_days(days);

        let rows = query(
            "SELECT DATE_FORMAT(DATE(created_at), '%Y-%m-%d') AS dt, \
             COUNT(*) AS total, \
             CAST(COALESCE(SUM(CASE WHEN JSON_UNQUOTE(JSON_EXTRACT(content, '$.safe_to_deliver')) = 'true' \
                  THEN 1 ELSE 0 END), 0) AS SIGNED) AS safe_cnt \
             FROM agent_events \
             WHERE user_id = ? \
               AND agent_id = ? \
               AND event_type = 'hallucination_check' \
               AND created_at > DATE_SUB(NOW(), INTERVAL ? DAY) \
             GROUP BY dt \
             ORDER BY dt",
        )
        .bind(user_id)
        .bind(agent_id)
        .bind(days)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let history = rows
            .iter()
            .map(slo_history_point_from_row)
            .collect::<ServiceResult<_>>()?;

        Ok(SloHistoryResponse {
            agent_id: agent_id.to_string(),
            days,
            history,
        })
    }

    async fn observability_metrics(
        &self,
        user_id: &str,
        agent_id: &str,
        days: i32,
    ) -> ServiceResult<ObservabilityMetricsResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let days = clamp_eval_days(days);

        let session_turn_rows = query(
            "SELECT COUNT(*) AS turn_count \
             FROM agent_events \
             WHERE user_id = ? \
               AND agent_id = ? \
               AND event_type = 'llm_response' \
               AND created_at > DATE_SUB(NOW(), INTERVAL ? DAY) \
             GROUP BY session_id",
        )
        .bind(user_id)
        .bind(agent_id)
        .bind(days)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;
        let turn_counts_raw: Vec<i64> = session_turn_rows
            .into_iter()
            .map(|row| observability_turn_count_from_row(&row))
            .collect::<ServiceResult<_>>()?;
        let total_decisions = turn_counts_raw.iter().sum();
        let turn_counts: Vec<f64> = turn_counts_raw.iter().map(|count| *count as f64).collect();
        let session = summarize_session_metrics(&turn_counts);

        let decision_quality_rows = query(
            "SELECT MAX(CAST(qa.score AS DOUBLE)) AS session_quality \
             FROM eval_quality_assessments qa \
             WHERE qa.user_id = ? \
               AND qa.level = 'session' \
               AND qa.updated_at >= DATE_SUB(NOW(), INTERVAL ? DAY) \
               AND qa.updated_at = ( \
                   SELECT MAX(q2.updated_at) \
                   FROM eval_quality_assessments q2 \
                   WHERE q2.user_id = qa.user_id \
                     AND q2.level = 'session' \
                     AND q2.target_id = qa.target_id \
               ) \
               AND EXISTS ( \
                   SELECT 1 \
                   FROM agent_events ev \
                   WHERE ev.user_id = qa.user_id \
                     AND ev.session_id = qa.target_id \
                     AND ev.agent_id = ? \
                     AND ev.event_type = 'llm_response' \
                     AND ev.created_at > DATE_SUB(NOW(), INTERVAL ? DAY) \
               ) \
             GROUP BY qa.target_id",
        )
        .bind(user_id)
        .bind(days)
        .bind(agent_id)
        .bind(days)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;
        let decision_quality_scores: Vec<f64> = decision_quality_rows
            .into_iter()
            .map(|row| observability_quality_score_from_row(&row))
            .collect::<ServiceResult<_>>()?;
        let decision = summarize_decision_metrics(total_decisions, &decision_quality_scores);

        let skill_row = query(
            "SELECT COUNT(*) AS total, \
             CAST(COALESCE(SUM(CASE WHEN execution_success = 1 THEN 1 ELSE 0 END), 0) AS SIGNED) AS ok_cnt \
             FROM skill_selection_events \
             WHERE user_id = ? \
               AND created_at > DATE_SUB(NOW(), INTERVAL ? DAY)",
        )
        .bind(user_id)
        .bind(days)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;

        let skill = skill_metrics_from_row(&skill_row)?;

        Ok(ObservabilityMetricsResponse {
            agent_id: agent_id.to_string(),
            period_days: days,
            decision,
            session,
            skill,
        })
    }

    async fn memory_health(&self, user_id: &str) -> ServiceResult<MemoryHealthResponse> {
        let analyze = self.memoria_get("/v1/health/analyze", user_id).await?;
        let hygiene = self.memoria_get("/v1/health/hygiene", user_id).await?;

        let semantic_total = analyze
            .pointer("/semantic/total")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let profile_total = analyze
            .pointer("/profile/total")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let total_memories = semantic_total + profile_total;

        // /storage may fail (Memoria #179); use it for active/inactive split, fallback to 0.
        let (active_memories, inactive_count) =
            match self.memoria_get("/v1/health/storage", user_id).await {
                Ok(s) => (
                    s.get("active").and_then(|v| v.as_i64()).unwrap_or(0),
                    s.get("inactive").and_then(|v| v.as_i64()).unwrap_or(0),
                ),
                Err(_) => (total_memories, 0),
            };

        let hygiene_issues: i64 = [
            "inactive_memories",
            "stale_working_memories",
            "orphan_memory_entity_links",
            "orphan_entity_links",
            "orphan_graph_nodes",
        ]
        .iter()
        .map(|key| hygiene.get(*key).and_then(|v| v.as_i64()).unwrap_or(0))
        .sum();
        let stale_working_memories = hygiene
            .get("stale_working_memories")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let orphaned_records: i64 = [
            "orphan_memory_entity_links",
            "orphan_entity_links",
            "orphan_graph_nodes",
        ]
        .iter()
        .map(|key| hygiene.get(*key).and_then(|v| v.as_i64()).unwrap_or(0))
        .sum();

        Ok(MemoryHealthResponse {
            total_memories,
            active_memories,
            inactive_memories: inactive_count,
            stale_working_memories,
            orphaned_records,
            healthy: hygiene_issues == 0,
        })
    }

    async fn memory_metrics(&self, user_id: &str) -> ServiceResult<MemoryMetricsResponse> {
        let analyze = self.memoria_get("/v1/health/analyze", user_id).await?;
        let hygiene = self.memoria_get("/v1/health/hygiene", user_id).await?;

        let total_memories = analyze
            .pointer("/semantic/total")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            + analyze
                .pointer("/profile/total")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
        let stale_count = hygiene
            .get("stale_working_memories")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let confidence_samples = memory_confidence_samples(&analyze);

        Ok(summarize_memory_metrics(
            total_memories,
            stale_count,
            &confidence_samples,
        ))
    }

    async fn extract_training_data(
        &self,
        user_id: &str,
        request: TrainingDataExtractRequest,
    ) -> ServiceResult<TrainingDataExtractResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let days = clamp_eval_days(request.days);
        let min_quality = request.min_quality.clamp(0.0, 1.0);
        let max_samples = clamp_extract_limit(request.max_samples);
        let sql = "SELECT qa.target_id AS session_id, \
                   MAX(CAST(qa.score AS DOUBLE)) AS quality_score, \
                   MAX(COALESCE(qa.step_count, 0)) AS step_count, \
                   AVG(CAST(ca.confidence AS DOUBLE)) AS avg_confidence, \
                   COUNT(ev.event_id) AS trace_count, \
                   DATE_FORMAT(MAX(qa.updated_at), '%Y-%m-%dT%H:%i:%s') AS quality_updated_at, \
                   DATE_FORMAT(MAX(ev.created_at), '%Y-%m-%dT%H:%i:%s') AS latest_context_trace_at \
            FROM eval_quality_assessments qa \
            LEFT JOIN eval_calibration_assessments ca \
              ON ca.session_id = qa.target_id \
             AND ca.user_id = qa.user_id \
             AND ca.created_at >= DATE_SUB(NOW(), INTERVAL ? DAY) \
            LEFT JOIN agent_events ev \
              ON ev.session_id = qa.target_id \
             AND ev.user_id = qa.user_id \
             AND ev.event_type = 'context_trace_signal' \
             AND ev.created_at >= DATE_SUB(NOW(), INTERVAL ? DAY) \
            WHERE qa.user_id = ? \
              AND qa.level = 'session' \
              AND qa.updated_at >= DATE_SUB(NOW(), INTERVAL ? DAY) \
              AND qa.score >= ? \
              AND qa.updated_at = ( \
                  SELECT MAX(q2.updated_at) \
                  FROM eval_quality_assessments q2 \
                  WHERE q2.user_id = qa.user_id \
                    AND q2.level = 'session' \
                    AND q2.target_id = qa.target_id \
              ) \
            GROUP BY qa.target_id \
            ORDER BY quality_score DESC, MAX(qa.updated_at) DESC \
            LIMIT ?";
        let rows = query(sql)
            .bind(days)
            .bind(days)
            .bind(user_id)
            .bind(days)
            .bind(min_quality)
            .bind(max_samples)
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let samples = rows
            .iter()
            .map(training_sample_from_row)
            .collect::<ServiceResult<Vec<_>>>()?;
        let dataset_id = uuid::Uuid::now_v7().to_string();
        let status = training_dataset_status(samples.len()).to_string();
        let request_payload = ExtractedTrainingDataRequest {
            days,
            min_quality,
            max_samples,
        };
        let dataset_payload = ExtractedTrainingDataset {
            schema_version: 1,
            extracted_at: now_iso(),
            request: request_payload.clone(),
            samples,
        };

        query(
            "INSERT INTO eval_training_datasets \
             (dataset_id, user_id, request_json, dataset_json, sample_count, quality_threshold, status) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&dataset_id)
        .bind(user_id)
        .bind(serde_json::to_string(&request_payload).map_err(internal_error)?)
        .bind(serde_json::to_string(&dataset_payload).map_err(internal_error)?)
        .bind(dataset_payload.samples.len() as i64)
        .bind(min_quality)
        .bind(&status)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        Ok(TrainingDataExtractResponse {
            dataset_id,
            samples_extracted: dataset_payload.samples.len() as i64,
            quality_threshold: min_quality,
            status,
        })
    }

    async fn export_training_data(
        &self,
        user_id: &str,
        dataset_id: &str,
        format: &str,
    ) -> ServiceResult<TrainingDataExportResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let export_format = normalize_export_format(format)?;
        if matches!(export_format, ExportFormat::Parquet) {
            return Err(error_response(
                StatusCode::NOT_IMPLEMENTED,
                "Parquet export is not implemented yet for evaluation training datasets",
            ));
        }

        let row = query(
            "SELECT dataset_json, sample_count \
             FROM eval_training_datasets \
             WHERE dataset_id = ? AND user_id = ?",
        )
        .bind(dataset_id)
        .bind(user_id)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let Some(row) = row else {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("Training dataset {dataset_id} not found"),
            ));
        };

        let (dataset_json, samples_exported) = training_dataset_export_from_row(&row)?;
        let dataset: ExtractedTrainingDataset =
            serde_json::from_str(&dataset_json).map_err(internal_error)?;
        let (normalized_format, content_type, content) = match export_format {
            ExportFormat::Jsonl => (
                "jsonl".to_string(),
                "application/x-ndjson".to_string(),
                render_training_dataset_jsonl(&dataset).map_err(internal_error)?,
            ),
            ExportFormat::Csv => (
                "csv".to_string(),
                "text/csv".to_string(),
                render_training_dataset_csv(&dataset),
            ),
            ExportFormat::Parquet => unreachable!(),
        };
        let status = if samples_exported == 0 {
            "empty".to_string()
        } else {
            "exported".to_string()
        };

        Ok(TrainingDataExportResponse {
            dataset_id: dataset_id.to_string(),
            format: normalized_format.clone(),
            status: status.clone(),
            message: if samples_exported == 0 {
                format!("Dataset {dataset_id} has no samples to export as {normalized_format}.")
            } else {
                format!(
                    "Exported {samples_exported} samples from dataset {dataset_id} as {normalized_format}."
                )
            },
            samples_exported,
            content_type,
            content,
        })
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeQualityTrendRow {
        failed_column: Option<&'static str>,
        empty_column: Option<&'static str>,
        avg_score: f64,
        count: i64,
        score: f64,
    }

    impl FakeQualityTrendRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                empty_column: None,
                avg_score: 0.75,
                count: 8,
                score: 0.82,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn empty_on(column: &'static str) -> Self {
            Self {
                empty_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_avg_score(avg_score: f64) -> Self {
            Self {
                avg_score,
                ..Self::complete()
            }
        }

        fn with_count(count: i64) -> Self {
            Self {
                count,
                ..Self::complete()
            }
        }

        fn with_score(score: f64) -> Self {
            Self {
                score,
                ..Self::complete()
            }
        }

        fn maybe_fail(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }

        fn text(&self, column: &str, value: &'static str) -> String {
            if self.empty_column == Some(column) {
                String::new()
            } else {
                value.to_string()
            }
        }
    }

    impl EvaluationRow for FakeQualityTrendRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.maybe_fail(column)?;
            match column {
                "dt" => Ok(self.text(column, "2026-06-26")),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.maybe_fail(column)?;
            match column {
                "dt" => Ok(Some(self.text(column, "2026-06-26"))),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.maybe_fail(column)?;
            match column {
                "cnt" => Ok(self.count),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn i8_column(&self, column: &str) -> Result<i8, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn f64_column(&self, column: &str) -> Result<f64, sqlx::Error> {
            self.maybe_fail(column)?;
            match column {
                "avg_score" => Ok(self.avg_score),
                "score" => Ok(self.score),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn optional_f64_column(&self, column: &str) -> Result<Option<f64>, sqlx::Error> {
            self.maybe_fail(column)?;
            match column {
                "avg_score" => Ok(Some(self.avg_score)),
                "score" => Ok(Some(self.score)),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }
    }

    #[derive(Default)]
    struct FakeEvaluationRow {
        failed_column: Option<&'static str>,
        strings: std::collections::BTreeMap<&'static str, Option<String>>,
        i64s: std::collections::BTreeMap<&'static str, i64>,
        i8s: std::collections::BTreeMap<&'static str, i8>,
        f64s: std::collections::BTreeMap<&'static str, f64>,
        optional_f64s: std::collections::BTreeMap<&'static str, Option<f64>>,
    }

    impl FakeEvaluationRow {
        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::default()
            }
        }

        fn string(mut self, column: &'static str, value: impl Into<String>) -> Self {
            self.strings.insert(column, Some(value.into()));
            self
        }

        fn optional_string_none(mut self, column: &'static str) -> Self {
            self.strings.insert(column, None);
            self
        }

        fn i64(mut self, column: &'static str, value: i64) -> Self {
            self.i64s.insert(column, value);
            self
        }

        fn i8(mut self, column: &'static str, value: i8) -> Self {
            self.i8s.insert(column, value);
            self
        }

        fn f64(mut self, column: &'static str, value: f64) -> Self {
            self.f64s.insert(column, value);
            self.optional_f64s.insert(column, Some(value));
            self
        }

        fn maybe_fail(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl EvaluationRow for FakeEvaluationRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.maybe_fail(column)?;
            match self.strings.get(column) {
                Some(Some(value)) => Ok(value.clone()),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.maybe_fail(column)?;
            self.strings
                .get(column)
                .cloned()
                .ok_or_else(|| sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.maybe_fail(column)?;
            self.i64s
                .get(column)
                .copied()
                .ok_or_else(|| sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn i8_column(&self, column: &str) -> Result<i8, sqlx::Error> {
            self.maybe_fail(column)?;
            self.i8s
                .get(column)
                .copied()
                .ok_or_else(|| sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn f64_column(&self, column: &str) -> Result<f64, sqlx::Error> {
            self.maybe_fail(column)?;
            self.f64s
                .get(column)
                .copied()
                .ok_or_else(|| sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn optional_f64_column(&self, column: &str) -> Result<Option<f64>, sqlx::Error> {
            self.maybe_fail(column)?;
            self.optional_f64s
                .get(column)
                .copied()
                .ok_or_else(|| sqlx::Error::ColumnNotFound(column.to_string()))
        }
    }

    fn assert_evaluation_internal_error_mentions(
        result: ServiceResult<impl std::fmt::Debug>,
        needle: &str,
    ) {
        let (status, axum::Json(body)) = result.expect_err("decode should fail");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            body.detail.contains(needle),
            "evaluation decode error should identify `{needle}`: {:?}",
            body.detail
        );
    }

    #[test]
    fn quality_trend_row_decode_preserves_values_and_fails_loudly() {
        let point =
            quality_trend_point_from_row(&FakeQualityTrendRow::complete(), Some("glm".into()))
                .unwrap();
        assert_eq!(point.date, "2026-06-26");
        assert_eq!(point.avg_score, 0.75);
        assert_eq!(point.count, 8);
        assert_eq!(point.model.as_deref(), Some("glm"));

        for column in ["dt", "avg_score", "cnt"] {
            assert_evaluation_internal_error_mentions(
                quality_trend_point_from_row(&FakeQualityTrendRow::fail_on(column), None),
                column,
            );
        }

        assert_evaluation_internal_error_mentions(
            quality_trend_point_from_row(&FakeQualityTrendRow::empty_on("dt"), None),
            "expected non-empty string",
        );
        assert_evaluation_internal_error_mentions(
            quality_trend_point_from_row(&FakeQualityTrendRow::with_count(-1), None),
            "non-negative integer",
        );
        for avg_score in [-0.1, 1.1] {
            assert_evaluation_internal_error_mentions(
                quality_trend_point_from_row(&FakeQualityTrendRow::with_avg_score(avg_score), None),
                "avg_score",
            );
        }
    }

    #[test]
    fn quality_trend_score_row_decode_preserves_values_and_fails_loudly() {
        assert_eq!(
            quality_trend_score_from_row(&FakeQualityTrendRow::complete()).unwrap(),
            0.82
        );

        assert_evaluation_internal_error_mentions(
            quality_trend_score_from_row(&FakeQualityTrendRow::fail_on("score")),
            "score",
        );
        for score in [-0.1, 1.1] {
            assert_evaluation_internal_error_mentions(
                quality_trend_score_from_row(&FakeQualityTrendRow::with_score(score)),
                "score",
            );
        }
    }

    #[test]
    fn remaining_evaluation_row_decoders_preserve_values() {
        let drift_row = FakeEvaluationRow::default()
            .string("level", "session")
            .string("window_bucket", "current")
            .f64("score", 0.77);
        assert_eq!(
            drift_score_from_row(&drift_row).unwrap(),
            ("session".to_string(), "current".to_string(), 0.77)
        );

        let gate_row = FakeEvaluationRow::default()
            .string("gate_id", "gate-1")
            .string("change_type", "tool_surface")
            .string("change_id", "change-1")
            .string("created_at", "2026-06-26T12:00:00")
            .i64("sessions_tested", 5)
            .f64("error_rate", 0.2)
            .f64("score_delta", -0.08)
            .i8("passed", 1);
        let gate = gate_result_from_row(&gate_row).unwrap();
        assert_eq!(gate.gate_id, "gate-1");
        assert_eq!(gate.sessions_tested, 5);
        assert_eq!(gate.score_delta, -0.08);
        assert!(gate.passed);
        assert_eq!(gate.created_at.as_deref(), Some("2026-06-26T12:00:00"));

        let calibration_row = FakeEvaluationRow::default()
            .f64("confidence", 0.62)
            .f64("quality_score", 0.71);
        assert_eq!(
            calibration_sample_from_row(&calibration_row).unwrap(),
            (0.62, 0.71)
        );

        let session_row = FakeEvaluationRow::default()
            .string("target_id", "session-1")
            .f64("score", 0.9)
            .i64("chain_count", 3);
        let session = session_score_from_row(&session_row).unwrap();
        assert_eq!(session.session_id, "session-1");
        assert_eq!(session.score, 0.9);
        assert_eq!(session.chain_count, 3);

        let slo_row = FakeEvaluationRow::default()
            .string("agent_id", "agent-1")
            .i64("total", 5)
            .i64("safe_cnt", 4);
        let slo = slo_entry_from_row(&slo_row).unwrap();
        assert_eq!(slo.agent_id, "agent-1");
        assert_eq!(slo.actual, 0.8);
        assert!(!slo.met);

        let history_row = FakeEvaluationRow::default()
            .string("dt", "2026-06-26")
            .i64("total", 2)
            .i64("safe_cnt", 1);
        let history = slo_history_point_from_row(&history_row).unwrap();
        assert_eq!(history.date, "2026-06-26");
        assert_eq!(history.value, 0.5);

        let observability_turn_row = FakeEvaluationRow::default().i64("turn_count", 9);
        assert_eq!(
            observability_turn_count_from_row(&observability_turn_row).unwrap(),
            9
        );
        let observability_quality_row = FakeEvaluationRow::default().f64("session_quality", 0.86);
        assert_eq!(
            observability_quality_score_from_row(&observability_quality_row).unwrap(),
            0.86
        );

        let skill_row = FakeEvaluationRow::default()
            .i64("total", 10)
            .i64("ok_cnt", 7);
        let skill = skill_metrics_from_row(&skill_row).unwrap();
        assert_eq!(skill.total_invocations, 10);
        assert_eq!(skill.success_count, 7);
        assert_eq!(skill.success_rate, 0.7);

        let training_row = FakeEvaluationRow::default()
            .string("session_id", "session-2")
            .f64("quality_score", 0.93)
            .i64("step_count", 4)
            .f64("avg_confidence", 0.81)
            .i64("trace_count", 6)
            .string("quality_updated_at", "2026-06-26T12:00:00")
            .optional_string_none("latest_context_trace_at");
        let sample = training_sample_from_row(&training_row).unwrap();
        assert_eq!(sample.session_id, "session-2");
        assert_eq!(sample.avg_confidence, Some(0.81));
        assert_eq!(sample.latest_context_trace_at, None);

        let export_row = FakeEvaluationRow::default()
            .string("dataset_json", "{\"schema_version\":1,\"samples\":[]}")
            .i64("sample_count", 0);
        assert_eq!(
            training_dataset_export_from_row(&export_row).unwrap(),
            ("{\"schema_version\":1,\"samples\":[]}".to_string(), 0)
        );
    }

    #[test]
    fn remaining_evaluation_row_decoders_fail_loudly() {
        assert_evaluation_internal_error_mentions(
            drift_score_from_row(&FakeEvaluationRow::fail_on("level")),
            "level",
        );
        assert_evaluation_internal_error_mentions(
            drift_score_from_row(
                &FakeEvaluationRow::default()
                    .string("level", "session")
                    .string("window_bucket", "stale")
                    .f64("score", 0.7),
            ),
            "window_bucket",
        );
        assert_evaluation_internal_error_mentions(
            gate_result_from_row(
                &FakeEvaluationRow::default()
                    .string("gate_id", "gate-1")
                    .string("change_type", "tool_surface")
                    .string("change_id", "change-1")
                    .string("created_at", "2026-06-26T12:00:00")
                    .i64("sessions_tested", 5)
                    .f64("error_rate", 0.2)
                    .f64("score_delta", -0.08)
                    .i8("passed", 2),
            ),
            "passed",
        );
        assert_evaluation_internal_error_mentions(
            calibration_sample_from_row(
                &FakeEvaluationRow::default()
                    .f64("confidence", 1.2)
                    .f64("quality_score", 0.71),
            ),
            "confidence",
        );
        assert_evaluation_internal_error_mentions(
            session_score_from_row(
                &FakeEvaluationRow::default()
                    .string("target_id", "")
                    .f64("score", 0.9)
                    .i64("chain_count", 3),
            ),
            "non-empty string",
        );
        assert_evaluation_internal_error_mentions(
            trust_count_pair_from_row(
                &FakeEvaluationRow::default()
                    .i64("total", 1)
                    .i64("safe_cnt", 2),
                "trust_report_row",
            ),
            "safe_cnt",
        );
        assert_evaluation_internal_error_mentions(
            training_sample_from_row(
                &FakeEvaluationRow::default()
                    .string("session_id", "session-2")
                    .f64("quality_score", 0.93)
                    .i64("step_count", 4)
                    .f64("avg_confidence", 1.1)
                    .i64("trace_count", 6)
                    .string("quality_updated_at", "2026-06-26T12:00:00")
                    .optional_string_none("latest_context_trace_at"),
            ),
            "avg_confidence",
        );
        assert_evaluation_internal_error_mentions(
            training_sample_from_row(
                &FakeEvaluationRow::default()
                    .string("session_id", "session-2")
                    .f64("quality_score", 0.93)
                    .i64("step_count", 4)
                    .f64("avg_confidence", 0.8)
                    .i64("trace_count", 6)
                    .string("quality_updated_at", "")
                    .optional_string_none("latest_context_trace_at"),
            ),
            "optional string",
        );
        assert_evaluation_internal_error_mentions(
            training_dataset_export_from_row(
                &FakeEvaluationRow::default()
                    .string("dataset_json", "{}")
                    .i64("sample_count", -1),
            ),
            "non-negative integer",
        );
    }

    // ── clamp helpers ───────────────────────────────────────────────────

    #[test]
    fn clamp_eval_limit_within_range() {
        assert_eq!(clamp_eval_limit(50), 50);
    }

    #[test]
    fn clamp_eval_limit_too_low() {
        assert_eq!(clamp_eval_limit(0), 1);
        assert_eq!(clamp_eval_limit(-10), 1);
    }

    #[test]
    fn clamp_eval_limit_too_high() {
        assert_eq!(clamp_eval_limit(999), MAX_EVALUATION_ROWS);
    }

    #[test]
    fn clamp_eval_days_within_range() {
        assert_eq!(clamp_eval_days(30), 30);
    }

    #[test]
    fn clamp_eval_days_boundaries() {
        assert_eq!(clamp_eval_days(0), 1);
        assert_eq!(clamp_eval_days(-5), 1);
        assert_eq!(clamp_eval_days(999), MAX_EVALUATION_DAYS);
        assert_eq!(clamp_eval_days(1), 1);
        assert_eq!(clamp_eval_days(365), 365);
    }

    #[test]
    fn clamp_extract_limit_boundaries() {
        assert_eq!(clamp_extract_limit(0), 1);
        assert_eq!(clamp_extract_limit(500), 500);
        assert_eq!(clamp_extract_limit(5000), MAX_EXTRACT_SAMPLES);
    }

    #[test]
    fn classify_drift_severity_thresholds() {
        assert_eq!(classify_drift_severity(0.03), None);
        assert_eq!(classify_drift_severity(0.06), Some(DriftSeverity::Info));
        assert_eq!(classify_drift_severity(-0.12), Some(DriftSeverity::Warning));
        assert_eq!(classify_drift_severity(0.25), Some(DriftSeverity::Critical));
    }

    #[test]
    fn build_drift_signal_ignores_small_delta() {
        assert!(
            build_drift_signal(
                "session".into(),
                None,
                &[0.78, 0.79, 0.77, 0.78, 0.78],
                &[0.75, 0.76, 0.74, 0.75, 0.75],
            )
            .is_none()
        );
    }

    #[test]
    fn build_drift_signal_sets_expected_fields() {
        let signal = build_drift_signal(
            "session".into(),
            None,
            &[0.55, 0.56, 0.54, 0.55, 0.55],
            &[0.78, 0.79, 0.77, 0.78, 0.78],
        )
        .unwrap();
        assert_eq!(signal.model, "session");
        assert_eq!(signal.template_id, None);
        assert!((signal.delta + 0.23).abs() < 1e-9);
        assert!((signal.noise_filtered_delta + 0.23).abs() < 1e-9);
        assert!((signal.delta_interval.point - signal.delta).abs() < 1e-9);
        assert!(
            (signal.noise_filtered_delta_interval.point - signal.noise_filtered_delta).abs() < 1e-9
        );
        assert_eq!(signal.severity, DriftSeverity::Critical);
        assert_eq!(signal.sample_count, 5);
        assert_eq!(signal.noise_filtered_sample_count, 5);
    }

    #[test]
    fn build_drift_signal_uses_noise_filtered_delta_for_severity() {
        assert!(
            build_drift_signal(
                "session".into(),
                None,
                &[0.20, 0.79, 0.80, 0.81, 0.82],
                &[0.79, 0.80, 0.81, 0.82, 0.83],
            )
            .is_none()
        );
    }

    #[test]
    fn build_loop_actions_honors_dry_run_prefix() {
        let diagnoses = vec![
            LoopDiagnosisItem {
                metric: "quality_overall_avg".into(),
                value: 0.5,
                value_interval: ValueInterval::exact(0.5),
                threshold: 0.7,
                action: LoopAction::Retune,
            },
            LoopDiagnosisItem {
                metric: "drift_signal_count".into(),
                value: 2.0,
                value_interval: ValueInterval::exact(2.0),
                threshold: 0.0,
                action: LoopAction::Alert,
            },
            LoopDiagnosisItem {
                metric: "noop_metric".into(),
                value: 1.0,
                value_interval: ValueInterval::exact(1.0),
                threshold: 1.0,
                action: LoopAction::NoOp,
            },
        ];

        assert_eq!(
            build_loop_actions(&diagnoses, true),
            vec![
                "dry_run:retune:quality_overall_avg".to_string(),
                "dry_run:alert:drift_signal_count".to_string(),
            ]
        );
        assert_eq!(
            build_loop_actions(&diagnoses, false),
            vec![
                "retune:quality_overall_avg".to_string(),
                "alert:drift_signal_count".to_string(),
            ]
        );
    }

    // ── DatabaseEvaluationService builder ───────────────────────────────

    #[test]
    fn new_service_has_no_pool_or_memoria() {
        let settings = MatrixOneSettings {
            host: String::new(),
            port: 0,
            user: String::new(),
            password: String::new(),
            database: String::new(),
            db_pool_max_connections: 1,
            db_pool_min_connections: 1,
            db_pool_acquire_timeout_secs: 5,
            db_pool_idle_timeout_secs: 60,
            db_pool_max_lifetime_secs: 300,
        };
        let svc = DatabaseEvaluationService::new(settings);
        assert!(svc.pool.is_none());
        assert!(svc.memoria_base_url.is_none());
        assert!(svc.memoria_master_key.is_none());
    }

    #[test]
    fn with_memoria_config_filters_empty_key() {
        let settings = MatrixOneSettings {
            host: String::new(),
            port: 0,
            user: String::new(),
            password: String::new(),
            database: String::new(),
            db_pool_max_connections: 1,
            db_pool_min_connections: 1,
            db_pool_acquire_timeout_secs: 5,
            db_pool_idle_timeout_secs: 60,
            db_pool_max_lifetime_secs: 300,
        };
        let svc = DatabaseEvaluationService::new(settings)
            .with_memoria_config("http://localhost:8080", Some("".to_string()));
        assert_eq!(svc.memoria_base_url, Some("http://localhost:8080".into()));
        // Empty key should be filtered to None
        assert!(svc.memoria_master_key.is_none());
    }

    #[test]
    fn with_memoria_config_keeps_valid_key() {
        let settings = MatrixOneSettings {
            host: String::new(),
            port: 0,
            user: String::new(),
            password: String::new(),
            database: String::new(),
            db_pool_max_connections: 1,
            db_pool_min_connections: 1,
            db_pool_acquire_timeout_secs: 5,
            db_pool_idle_timeout_secs: 60,
            db_pool_max_lifetime_secs: 300,
        };
        let svc = DatabaseEvaluationService::new(settings)
            .with_memoria_config("http://localhost:8080", Some("secret123".to_string()));
        assert_eq!(svc.memoria_master_key, Some("secret123".into()));
    }

    #[test]
    fn with_memoria_config_none_key() {
        let settings = MatrixOneSettings {
            host: String::new(),
            port: 0,
            user: String::new(),
            password: String::new(),
            database: String::new(),
            db_pool_max_connections: 1,
            db_pool_min_connections: 1,
            db_pool_acquire_timeout_secs: 5,
            db_pool_idle_timeout_secs: 60,
            db_pool_max_lifetime_secs: 300,
        };
        let svc = DatabaseEvaluationService::new(settings)
            .with_memoria_config("http://localhost:8080", None);
        assert!(svc.memoria_master_key.is_none());
    }

    // ── memoria_get error paths (no DB needed) ──────────────────────────

    #[tokio::test]
    async fn memoria_get_fails_without_config() {
        let settings = MatrixOneSettings {
            host: String::new(),
            port: 0,
            user: String::new(),
            password: String::new(),
            database: String::new(),
            db_pool_max_connections: 1,
            db_pool_min_connections: 1,
            db_pool_acquire_timeout_secs: 5,
            db_pool_idle_timeout_secs: 60,
            db_pool_max_lifetime_secs: 300,
        };
        let svc = DatabaseEvaluationService::new(settings);
        let result = svc.memoria_get("/v1/health/storage", "user1").await;
        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn memoria_get_fails_without_master_key() {
        let settings = MatrixOneSettings {
            host: String::new(),
            port: 0,
            user: String::new(),
            password: String::new(),
            database: String::new(),
            db_pool_max_connections: 1,
            db_pool_min_connections: 1,
            db_pool_acquire_timeout_secs: 5,
            db_pool_idle_timeout_secs: 60,
            db_pool_max_lifetime_secs: 300,
        };
        let svc = DatabaseEvaluationService::new(settings)
            .with_memoria_config("http://localhost:9999", None);
        let result = svc.memoria_get("/v1/health/storage", "user1").await;
        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    // ── UnconfiguredEvaluationService ───────────────────────────────────

    #[tokio::test]
    async fn unconfigured_service_all_methods_error() {
        use super::super::noop::UnconfiguredEvaluationService;
        use super::super::service::EvaluationService;
        let svc = UnconfiguredEvaluationService;

        assert!(svc.get_quality_trend("u", 30, None).await.is_err());
        assert!(svc.detect_drift("u").await.is_err());
        assert!(svc.get_gate_history("u", 50).await.is_err());
        assert!(svc.get_calibration("u", None, 30).await.is_err());
        assert!(svc.get_session_scores("u", 50, 0.0).await.is_err());
        assert!(svc.trust_report("u", "a", 30).await.is_err());
        assert!(svc.slo_dashboard("u", 30).await.is_err());
        assert!(svc.slo_history("u", "a", 30).await.is_err());
        assert!(svc.observability_metrics("u", "a", 30).await.is_err());
        assert!(svc.memory_health("u").await.is_err());
        assert!(svc.memory_metrics("u").await.is_err());
    }

    // ── evaluation/utils pure functions ──────────────────────────────────

    #[test]
    fn compute_overall_avg_empty() {
        use super::super::utils::compute_overall_avg;
        assert_eq!(compute_overall_avg(&[]), 0.0);
    }

    #[test]
    fn compute_overall_avg_weighted() {
        use super::super::types::QualityTrendPoint;
        use super::super::utils::compute_overall_avg;
        let points = vec![
            QualityTrendPoint {
                date: "2024-01-01".into(),
                avg_score: 0.8,
                avg_score_interval: ConfidenceInterval::exact(0.8),
                count: 10,
                model: None,
            },
            QualityTrendPoint {
                date: "2024-01-02".into(),
                avg_score: 0.6,
                avg_score_interval: ConfidenceInterval::exact(0.6),
                count: 10,
                model: None,
            },
        ];
        let avg = compute_overall_avg(&points);
        assert!((avg - 0.7).abs() < 0.001);
    }

    #[test]
    fn compute_overall_avg_zero_counts() {
        use super::super::types::QualityTrendPoint;
        use super::super::utils::compute_overall_avg;
        let points = vec![QualityTrendPoint {
            date: "2024-01-01".into(),
            avg_score: 0.9,
            avg_score_interval: ConfidenceInterval::exact(0.9),
            count: 0,
            model: None,
        }];
        assert_eq!(compute_overall_avg(&points), 0.0);
    }

    #[test]
    fn sampled_confidence_interval_zero_samples_is_zero() {
        let interval = sampled_confidence_interval(0.8, 0);
        assert_eq!(interval, ConfidenceInterval::ZERO);
    }

    #[test]
    fn sampled_confidence_interval_clamps_bounds() {
        let interval = sampled_confidence_interval(0.9, 4);
        assert_eq!(interval.point, 0.9);
        assert!(interval.lower >= 0.0);
        assert!(interval.upper <= 1.0);
    }

    #[test]
    fn sampled_value_interval_zero_samples_is_zero() {
        let interval = sampled_value_interval(-0.2, 0);
        assert_eq!(interval.point, ValueInterval::ZERO.point);
        assert_eq!(interval.lower, ValueInterval::ZERO.lower);
        assert_eq!(interval.upper, ValueInterval::ZERO.upper);
    }

    #[test]
    fn sampled_value_interval_supports_negative_points() {
        let interval = sampled_value_interval(-0.2, 16);
        assert!((interval.point + 0.2).abs() < 0.0001);
        assert!(interval.lower < 0.0);
        assert!(interval.upper > interval.point);
    }

    #[test]
    fn absolute_value_interval_clamps_crossing_zero_lower_bound() {
        let interval = absolute_value_interval(ValueInterval::new(-0.1, -0.3, 0.2));
        assert!((interval.point - 0.1).abs() < 0.0001);
        assert_eq!(interval.lower, 0.0);
        assert!((interval.upper - 0.3).abs() < 0.0001);
    }

    #[test]
    fn adjustment_multiplier_interval_spans_interval_corners() {
        let interval = adjustment_multiplier_interval(
            ValueInterval::new(0.2, 0.1, 0.3),
            ValueInterval::new(0.2, 0.1, 0.3),
            0.8,
        );
        assert!((interval.point - 0.8).abs() < 0.0001);
        assert!(interval.lower <= interval.point);
        assert!(interval.upper >= interval.point);
    }

    #[test]
    fn numeric_mean_interval_zero_samples_is_zero() {
        assert_eq!(
            numeric_mean_interval(&[]).point,
            NumericInterval::ZERO.point
        );
        assert_eq!(
            numeric_mean_interval(&[]).lower,
            NumericInterval::ZERO.lower
        );
        assert_eq!(
            numeric_mean_interval(&[]).upper,
            NumericInterval::ZERO.upper
        );
    }

    #[test]
    fn numeric_mean_interval_preserves_unbounded_scale() {
        let interval = numeric_mean_interval(&[6.0, 8.0, 10.0, 12.0]);
        assert!((interval.point - 9.0).abs() < 0.0001);
        assert!(interval.upper > 1.0);
        assert!(interval.lower >= 0.0);
    }

    #[test]
    fn complement_interval_flips_bounds() {
        let interval = ConfidenceInterval::new(0.8, 0.7, 0.9);
        let complement = complement_interval(interval);
        assert_eq!(complement, ConfidenceInterval::new(0.2, 0.1, 0.3));
    }

    #[test]
    fn training_dataset_status_matches_sample_count() {
        assert_eq!(training_dataset_status(0), "empty");
        assert_eq!(training_dataset_status(3), "ready");
    }

    #[test]
    fn normalize_model_filter_trims_and_drops_empty_values() {
        assert_eq!(normalize_model_filter(None), None);
        assert_eq!(normalize_model_filter(Some("   ")), None);
        assert_eq!(
            normalize_model_filter(Some(" gpt-4.1 ")),
            Some("gpt-4.1".into())
        );
    }

    #[test]
    fn quality_trend_query_without_model_uses_unfiltered_assessments() {
        let sql = quality_trend_query(None);
        assert!(sql.contains("FROM eval_quality_assessments qa"));
        assert!(!sql.contains("qa.level = 'session'"));
        assert!(!sql.contains("agent_events e"));
    }

    #[test]
    fn quality_trend_query_with_model_filters_session_assessments() {
        let sql = quality_trend_query(Some("gpt-4"));
        assert!(sql.contains("qa.level = 'session'"));
        assert!(sql.contains("EXISTS ("));
        assert!(sql.contains("e.session_id = qa.target_id"));
        assert!(sql.contains("e.llm_model_used = ?"));
    }

    #[test]
    fn session_quality_assessment_id_is_stable() {
        assert_eq!(
            session_quality_assessment_id("sess-123"),
            "session:sess-123".to_string()
        );
    }

    #[test]
    fn session_quality_assessment_upsert_query_updates_existing_rows() {
        assert!(
            UPSERT_SESSION_QUALITY_ASSESSMENT_SQL
                .contains("(assessment_id, user_id, target_id, score, step_count, level)")
        );
        assert!(
            !UPSERT_SESSION_QUALITY_ASSESSMENT_SQL.contains("session_id"),
            "session quality uses target_id with level='session'; do not add a duplicate required session_id column"
        );
        assert!(UPSERT_SESSION_QUALITY_ASSESSMENT_SQL.contains("ON DUPLICATE KEY UPDATE"));
        assert!(
            UPSERT_SESSION_QUALITY_ASSESSMENT_SQL.contains("updated_at = CURRENT_TIMESTAMP(6)")
        );
    }

    #[test]
    fn quality_trend_scores_query_with_model_filters_session_assessments() {
        let sql = quality_trend_scores_query(Some("gpt-4"));
        assert!(sql.contains("SELECT qa.score AS score"));
        assert!(sql.contains("qa.level = 'session'"));
        assert!(sql.contains("e.session_id = qa.target_id"));
    }

    #[test]
    fn noise_filtered_average_empty_scores_returns_zero() {
        assert_eq!(
            noise_filtered_average(&[]),
            NoiseFilteredAverage {
                average: 0.0,
                sample_count: 0,
            }
        );
    }

    #[test]
    fn noise_filtered_average_small_sample_keeps_raw_scores() {
        let filtered = noise_filtered_average(&[0.7, 0.8, 0.6, 0.9]);
        assert_eq!(filtered.sample_count, 4);
        assert!((filtered.average - 0.75).abs() < 1e-9);
    }

    #[test]
    fn noise_filtered_average_drops_iqr_outlier() {
        let filtered = noise_filtered_average(&[0.2, 0.79, 0.8, 0.81, 0.82]);
        assert_eq!(filtered.sample_count, 4);
        assert!((filtered.average - 0.805).abs() < 0.0001);
    }

    #[test]
    fn noise_filtered_average_keeps_flat_distribution() {
        let filtered = noise_filtered_average(&[0.7, 0.7, 0.7, 0.7, 0.7]);
        assert_eq!(filtered.sample_count, 5);
        assert!((filtered.average - 0.7).abs() < 1e-9);
    }

    #[test]
    fn summarize_decision_metrics_uses_filtered_quality_companion() {
        let metrics = summarize_decision_metrics(12, &[0.2, 0.79, 0.8, 0.81, 0.82]);
        assert_eq!(metrics.total_decisions, 12);
        assert_eq!(metrics.total_quality_samples, 5);
        assert_eq!(metrics.noise_filtered_quality_samples, 4);
        assert!(metrics.noise_filtered_avg_quality > metrics.avg_quality);
        assert!((metrics.noise_filtered_avg_quality - 0.805).abs() < 0.0001);
    }

    #[test]
    fn summarize_session_metrics_filters_outlier_session_turns() {
        let metrics = summarize_session_metrics(&[6.0, 7.0, 8.0, 9.0, 60.0]);
        assert_eq!(metrics.unique_sessions, 5);
        assert_eq!(metrics.noise_filtered_session_count, 4);
        assert!(metrics.noise_filtered_avg_turns_per_session < metrics.avg_turns_per_session);
        assert!((metrics.noise_filtered_avg_turns_per_session - 7.5).abs() < 0.0001);
        assert!(
            (metrics.avg_turns_per_session_interval.point - metrics.avg_turns_per_session).abs()
                < 0.0001
        );
        assert!(
            (metrics.noise_filtered_avg_turns_per_session_interval.point
                - metrics.noise_filtered_avg_turns_per_session)
                .abs()
                < 0.0001
        );
        assert!(metrics.avg_turns_per_session_interval.upper > 1.0);
    }

    #[test]
    fn weighted_noise_filtered_average_small_sample_keeps_all_weights() {
        let filtered = weighted_noise_filtered_average(&[(0.8, 4), (0.6, 8)]);
        assert_eq!(filtered.sample_count, 12);
        assert!((filtered.average - 0.6666666667).abs() < 0.0001);
    }

    #[test]
    fn weighted_noise_filtered_average_drops_confidence_outlier_bucket() {
        let filtered = weighted_noise_filtered_average(&[
            (0.79, 10),
            (0.80, 10),
            (0.81, 10),
            (0.82, 10),
            (0.10, 1),
        ]);
        assert_eq!(filtered.sample_count, 40);
        assert!((filtered.average - 0.805).abs() < 0.0001);
    }

    #[test]
    fn summarize_memory_metrics_uses_weighted_filtered_confidence() {
        let metrics = summarize_memory_metrics(
            41,
            3,
            &[(0.79, 10), (0.80, 10), (0.81, 10), (0.82, 10), (0.10, 1)],
        );
        assert_eq!(metrics.total_memories, 41);
        assert_eq!(metrics.stale_count, 3);
        assert_eq!(metrics.noise_filtered_confidence_samples, 40);
        assert!(metrics.noise_filtered_avg_confidence > metrics.avg_confidence);
        assert!((metrics.noise_filtered_avg_confidence - 0.805).abs() < 0.0001);
    }

    #[test]
    fn noise_filtered_calibration_samples_drop_gap_outlier() {
        let filtered = noise_filtered_calibration_samples(&[
            (0.80, 0.79),
            (0.81, 0.80),
            (0.82, 0.81),
            (0.83, 0.82),
            (0.20, 0.90),
        ]);
        assert_eq!(filtered.len(), 4);
        assert!(!filtered.contains(&(0.20, 0.90)));
    }

    fn sample_training_dataset() -> ExtractedTrainingDataset {
        ExtractedTrainingDataset {
            schema_version: 1,
            extracted_at: "2026-04-12T00:00:00".into(),
            request: ExtractedTrainingDataRequest {
                days: 30,
                min_quality: 0.8,
                max_samples: 10,
            },
            samples: vec![ExtractedTrainingSample {
                session_id: "sess-1".into(),
                quality_score: 0.9,
                step_count: 4,
                avg_confidence: Some(0.85),
                trace_count: 2,
                quality_updated_at: Some("2026-04-12T00:00:00".into()),
                latest_context_trace_at: Some("2026-04-12T00:01:00".into()),
            }],
        }
    }

    #[test]
    fn normalize_export_format_rejects_unknown() {
        let err = normalize_export_format("xml").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn render_training_dataset_jsonl_outputs_one_line_per_sample() {
        let dataset = sample_training_dataset();
        let jsonl = render_training_dataset_jsonl(&dataset).unwrap();
        assert_eq!(jsonl.lines().count(), 1);
        assert!(jsonl.contains("\"session_id\":\"sess-1\""));
    }

    #[test]
    fn render_training_dataset_csv_outputs_header_and_rows() {
        let dataset = sample_training_dataset();
        let csv = render_training_dataset_csv(&dataset);
        assert!(csv.starts_with("session_id,quality_score,step_count"));
        assert!(csv.contains("sess-1,0.9,4,0.85,2"));
    }

    #[test]
    fn summarize_calibration_empty_samples_returns_default() {
        let summary = summarize_calibration(0.9, 0.7, 0);
        assert_eq!(summary.sample_count, 0);
        assert_eq!(summary.mean_confidence_interval, ConfidenceInterval::ZERO);
        assert_eq!(summary.mean_quality_interval, ConfidenceInterval::ZERO);
        assert_eq!(summary.adjustment_multiplier, 1.0);
        assert!(
            summary
                .adjustment_reason
                .contains("No session calibration samples")
        );
    }

    #[test]
    fn summarize_calibration_derives_bias_and_intervals() {
        let summary = summarize_calibration(0.9, 0.7, 16);
        assert_eq!(summary.sample_count, 16);
        assert!((summary.calibration_error - 0.2).abs() < 0.001);
        assert!((summary.bias - 0.2).abs() < 0.001);
        assert!(summary.mean_confidence_interval.lower <= summary.mean_confidence);
        assert!(summary.mean_quality_interval.upper >= summary.mean_quality);
        assert!(
            (summary.calibration_error_interval.point - summary.calibration_error).abs() < 0.001
        );
        assert!((summary.bias_interval.point - summary.bias).abs() < 0.001);
        assert!(summary.adjustment_multiplier < 1.0);
        assert!(
            (summary.adjustment_multiplier_interval.point - summary.adjustment_multiplier).abs()
                < 0.001
        );
        assert!(summary.adjustment_reason.contains("Overconfident"));
    }

    #[test]
    fn trust_ratio_zero_checks() {
        use super::super::utils::trust_ratio;
        assert_eq!(trust_ratio(0, 0), 1.0);
    }

    #[test]
    fn trust_ratio_all_safe() {
        use super::super::utils::trust_ratio;
        assert_eq!(trust_ratio(100, 100), 1.0);
    }

    #[test]
    fn trust_ratio_partial() {
        use super::super::utils::trust_ratio;
        assert!((trust_ratio(10, 7) - 0.7).abs() < 0.001);
    }

    #[test]
    fn skill_success_rate_zero() {
        use super::super::utils::skill_success_rate;
        assert_eq!(skill_success_rate(0, 0), 0.0);
    }

    #[test]
    fn skill_success_rate_half() {
        use super::super::utils::skill_success_rate;
        assert!((skill_success_rate(10, 5) - 0.5).abs() < 0.001);
    }

    #[test]
    fn compute_calibration_error_perfect() {
        use super::super::utils::compute_calibration_error;
        assert_eq!(compute_calibration_error(0.8, 0.8), 0.0);
    }

    #[test]
    fn compute_calibration_error_gap() {
        use super::super::utils::compute_calibration_error;
        assert!((compute_calibration_error(0.9, 0.7) - 0.2).abs() < 0.001);
    }

    #[test]
    fn compute_adjustment_well_calibrated() {
        use super::super::utils::compute_adjustment;
        let (mult, reason) = compute_adjustment(0.03, 0.0);
        assert_eq!(mult, 1.0);
        assert!(reason.contains("Well calibrated"));
    }

    #[test]
    fn compute_adjustment_overconfident() {
        use super::super::utils::compute_adjustment;
        let (mult, reason) = compute_adjustment(0.2, 0.15);
        assert!(mult < 1.0);
        assert!(reason.contains("Overconfident"));
    }

    #[test]
    fn compute_adjustment_underconfident() {
        use super::super::utils::compute_adjustment;
        let (mult, reason) = compute_adjustment(0.2, -0.15);
        assert!(mult > 1.0);
        assert!(reason.contains("Underconfident"));
    }

    // ── evaluation/types serde ──────────────────────────────────────────

    #[test]
    fn drift_severity_serde_roundtrip() {
        use super::super::types::DriftSeverity;
        let json = serde_json::to_string(&DriftSeverity::Critical).unwrap();
        assert_eq!(json, r#""critical""#);
        let back: DriftSeverity = serde_json::from_str(&json).unwrap();
        assert_eq!(back, DriftSeverity::Critical);
    }

    #[test]
    fn change_type_serde() {
        use super::super::types::ChangeType;
        let json = serde_json::to_string(&ChangeType::ContextBudget).unwrap();
        assert_eq!(json, r#""context_budget""#);
    }

    #[test]
    fn loop_action_serde() {
        use super::super::types::LoopAction;
        let json = serde_json::to_string(&LoopAction::NoOp).unwrap();
        assert_eq!(json, r#""no_op""#);
    }

    #[test]
    fn export_format_serde() {
        use super::super::types::ExportFormat;
        let json = serde_json::to_string(&ExportFormat::Parquet).unwrap();
        assert_eq!(json, r#""parquet""#);
    }

    #[test]
    fn quality_trend_query_defaults() {
        use super::super::types::QualityTrendQuery;
        let q: QualityTrendQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.days, 30);
        assert!(q.model.is_none());
    }

    #[test]
    fn gate_validate_request_defaults() {
        use super::super::types::GateValidateRequest;
        let json = r#"{"change_type":"prompt","change_id":"c1","change_content":{}}"#;
        let req: GateValidateRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.golden_session_count, 50);
        assert!((req.error_rate_threshold - 0.05).abs() < 0.001);
        assert!((req.score_regression_threshold - (-0.1)).abs() < 0.001);
    }

    #[test]
    fn summarize_gate_validation_rejects_missing_scores() {
        let summary = summarize_gate_validation(&[], 50, 0.05, -0.1);
        assert_eq!(summary.sessions_tested, 0);
        assert!(!summary.passed);
        assert!(summary.details.contains("No session quality scores"));
    }

    #[test]
    fn summarize_gate_validation_passes_stable_recent_window() {
        let summary = summarize_gate_validation(&[0.91, 0.88, 0.85, 0.84], 2, 0.2, -0.1);
        assert_eq!(summary.sessions_tested, 2);
        assert!(summary.passed);
        assert!((summary.error_rate - 0.0).abs() < 1e-9);
        assert!((summary.score_delta - 0.05).abs() < 1e-9);
    }

    #[test]
    fn summarize_gate_validation_uses_noise_filtered_score_delta() {
        let summary = summarize_gate_validation(
            &[0.83, 0.82, 0.81, 0.80, 0.20, 0.82, 0.81, 0.80, 0.79, 0.80],
            5,
            0.25,
            -0.05,
        );
        assert_eq!(summary.sessions_tested, 5);
        assert!(summary.passed);
        assert!(summary.score_delta > 0.0);
        assert!((summary.score_delta_interval.point - summary.score_delta).abs() < 0.0001);
        assert!(summary.details.contains("Noise filter kept 4 of 5 recent"));
    }

    #[test]
    fn summarize_gate_validation_fails_on_error_rate() {
        let summary = summarize_gate_validation(&[0.65, 0.60, 0.91, 0.90], 2, 0.25, -0.5);
        assert_eq!(summary.sessions_tested, 2);
        assert!(!summary.passed);
        assert!((summary.error_rate - 1.0).abs() < 1e-9);
        assert!(summary.details.contains("error rate"));
    }

    #[test]
    fn summarize_gate_validation_fails_on_score_regression() {
        let summary = summarize_gate_validation(&[0.72, 0.70, 0.95, 0.92], 2, 1.0, -0.1);
        assert_eq!(summary.sessions_tested, 2);
        assert!(!summary.passed);
        assert!(summary.score_delta < -0.2);
        assert!(summary.details.contains("score delta"));
    }

    #[test]
    fn training_data_extract_defaults() {
        use super::super::types::TrainingDataExtractRequest;
        let req: TrainingDataExtractRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(req.days, 30);
        assert!((req.min_quality - 0.7).abs() < 0.001);
        assert_eq!(req.max_samples, 1000);
    }

    #[test]
    fn not_implemented_helper() {
        use super::super::utils::not_implemented;
        let (status, _) = not_implemented("test");
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    }

    #[test]
    fn memoria_http_client_timeout_policy_is_bounded() {
        assert!(
            memoria_connect_timeout() <= memoria_request_timeout(),
            "connect timeout cannot exceed the full request timeout"
        );
        assert!(
            memoria_request_timeout() <= std::time::Duration::from_secs(60),
            "Memoria calls must stay bounded so evaluation handlers cannot hang indefinitely"
        );
        memoria_http_client(HeaderMap::new()).expect("Memoria HTTP client builder");
    }
}
