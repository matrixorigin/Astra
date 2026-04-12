use async_trait::async_trait;
use axum::http::StatusCode;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::Value;
use sqlx::{Row, query};

use super::service::EvaluationService;
use super::types::*;
use super::utils::*;
use astra_core::{
    MatrixOneSettings, SharedPool, confidence::ConfidenceInterval, connect_matrixone,
    error_response, internal_error,
};

const MAX_EVALUATION_ROWS: i32 = 200;
const MAX_EVALUATION_DAYS: i32 = 365;
const DEFAULT_DRIFT_WINDOW_DAYS: i32 = 30;
const DRIFT_INFO_DELTA: f64 = 0.05;
const DRIFT_WARNING_DELTA: f64 = 0.10;
const DRIFT_CRITICAL_DELTA: f64 = 0.20;
const LOOP_QUALITY_THRESHOLD: f64 = 0.70;
const LOOP_DRIFT_DELTA_THRESHOLD: f64 = 0.10;
const TRUST_SLO_TARGET: f64 = 0.95;

fn clamp_eval_limit(limit: i32) -> i32 {
    limit.clamp(1, MAX_EVALUATION_ROWS)
}

fn clamp_eval_days(days: i32) -> i32 {
    days.clamp(1, MAX_EVALUATION_DAYS)
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

fn build_drift_signal(
    model: String,
    template_id: Option<String>,
    current_avg: f64,
    previous_avg: f64,
    sample_count: i64,
) -> Option<DriftSignalResponse> {
    let delta = current_avg - previous_avg;
    classify_drift_severity(delta).map(|severity| DriftSignalResponse {
        model,
        template_id,
        current_avg,
        current_avg_interval: sampled_confidence_interval(current_avg, sample_count),
        previous_avg,
        previous_avg_interval: sampled_confidence_interval(previous_avg, sample_count),
        delta,
        severity,
        sample_count,
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
    passed: bool,
    details: String,
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
        ChangeType::Selector => "selector",
        ChangeType::ContextBudget => "context_budget",
        ChangeType::Knowledge => "knowledge",
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
            passed: false,
            details: "No session quality scores available for gate validation.".into(),
        };
    }

    let baseline_end = scores_desc.len().min(window * 2);
    let baseline = &scores_desc[recent_end..baseline_end];
    let recent_avg = average_scores(recent);
    let baseline_avg = average_scores(baseline);
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

    let details = if baseline.is_empty() {
        format!(
            "{} Validated {} recent session scores with no baseline window; recent avg {:.3}, error rate {:.1}% (threshold {:.1}%).",
            if passed {
                "Gate passed."
            } else {
                "Gate failed."
            },
            recent.len(),
            recent_avg,
            error_rate * 100.0,
            error_rate_threshold * 100.0
        )
    } else {
        format!(
            "{} Validated {} recent vs {} baseline session scores; recent avg {:.3}, baseline avg {:.3}, delta {:.3} (threshold {:.3}), error rate {:.1}% (threshold {:.1}%).{}",
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
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
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

        let client = reqwest::Client::builder()
            .no_proxy()
            .default_headers(headers)
            .build()
            .map_err(internal_error)?;

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
        if model.is_some() {
            return Err(not_implemented(
                "Evaluation quality trend model filtering is not implemented yet",
            ));
        }

        let pool = self.get_pool().await.map_err(internal_error)?;
        let days = clamp_eval_days(days);

        let rows = query(
            "SELECT DATE_FORMAT(DATE(created_at), '%Y-%m-%d') AS dt, \
             AVG(score) AS avg_score, COUNT(*) AS cnt \
             FROM eval_quality_assessments \
             WHERE user_id = ? \
               AND created_at >= DATE_SUB(NOW(), INTERVAL ? DAY) \
             GROUP BY dt ORDER BY dt",
        )
        .bind(user_id)
        .bind(days)
        .fetch_all(&pool)
        .await
        .unwrap_or_default();

        let points: Vec<QualityTrendPoint> = rows
            .iter()
            .map(|r| {
                let avg_score = r.try_get("avg_score").unwrap_or(0.0);
                let count = r.try_get("cnt").unwrap_or(0);
                QualityTrendPoint {
                    date: r.try_get("dt").unwrap_or_default(),
                    avg_score,
                    avg_score_interval: sampled_confidence_interval(avg_score, count),
                    count,
                    model: None,
                }
            })
            .collect();

        let total_events: i64 = points.iter().map(|p| p.count).sum();
        let overall_avg = compute_overall_avg(&points);

        Ok(QualityTrendResponse {
            points,
            overall_avg,
            overall_avg_interval: sampled_confidence_interval(overall_avg, total_events),
            total_events,
        })
    }

    async fn detect_drift(&self, user_id: &str) -> ServiceResult<DriftDetectResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let window_days = clamp_eval_days(DEFAULT_DRIFT_WINDOW_DAYS);
        let lookback_days = window_days * 2;

        let rows = query(
            "SELECT level, \
             AVG(CASE WHEN created_at >= DATE_SUB(NOW(), INTERVAL ? DAY) THEN score END) AS current_avg, \
             AVG(CASE WHEN created_at < DATE_SUB(NOW(), INTERVAL ? DAY) \
                      AND created_at >= DATE_SUB(NOW(), INTERVAL ? DAY) THEN score END) AS previous_avg, \
             SUM(CASE WHEN created_at >= DATE_SUB(NOW(), INTERVAL ? DAY) THEN 1 ELSE 0 END) AS current_count, \
             SUM(CASE WHEN created_at < DATE_SUB(NOW(), INTERVAL ? DAY) \
                      AND created_at >= DATE_SUB(NOW(), INTERVAL ? DAY) THEN 1 ELSE 0 END) AS previous_count \
             FROM eval_quality_assessments \
             WHERE user_id = ? \
               AND created_at >= DATE_SUB(NOW(), INTERVAL ? DAY) \
             GROUP BY level \
             ORDER BY level",
        )
        .bind(window_days)
        .bind(window_days)
        .bind(lookback_days)
        .bind(window_days)
        .bind(window_days)
        .bind(lookback_days)
        .bind(user_id)
        .bind(lookback_days)
        .fetch_all(&pool)
        .await
        .unwrap_or_default();

        let signals = rows
            .iter()
            .filter_map(|row| {
                let current_count = row.try_get::<i64, _>("current_count").unwrap_or(0);
                let previous_count = row.try_get::<i64, _>("previous_count").unwrap_or(0);
                if current_count == 0 || previous_count == 0 {
                    return None;
                }

                let level = row
                    .try_get::<String, _>("level")
                    .unwrap_or_else(|_| "unknown".into());
                let current_avg = row.try_get::<f64, _>("current_avg").unwrap_or(0.0);
                let previous_avg = row.try_get::<f64, _>("previous_avg").unwrap_or(0.0);

                build_drift_signal(
                    level,
                    None,
                    current_avg,
                    previous_avg,
                    current_count.min(previous_count),
                )
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
        .unwrap_or_default();

        let gates: Vec<GateResultResponse> = rows
            .iter()
            .map(|r| {
                let sessions_tested = r.try_get("sessions_tested").unwrap_or(0);
                let error_rate = r.try_get("error_rate").unwrap_or(0.0);
                GateResultResponse {
                    gate_id: r.try_get("gate_id").unwrap_or_default(),
                    change_type: r.try_get("change_type").unwrap_or_default(),
                    change_id: r.try_get("change_id").unwrap_or_default(),
                    sessions_tested,
                    error_rate,
                    error_rate_interval: sampled_confidence_interval(error_rate, sessions_tested),
                    score_delta: r.try_get("score_delta").unwrap_or(0.0),
                    passed: r.try_get::<i8, _>("passed").unwrap_or(0) != 0,
                    created_at: r.try_get("created_at").ok(),
                }
            })
            .collect();
        let total = gates.len();
        Ok(GateHistoryResponse { gates, total })
    }

    async fn get_calibration(
        &self,
        _user_id: &str,
        _agent_id: Option<&str>,
        _days: i32,
    ) -> ServiceResult<CalibrationResponse> {
        Err(not_implemented(
            "Evaluation calibration reporting is not implemented yet",
        ))
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
        .unwrap_or_default();

        let sessions: Vec<SessionScoreResponse> = rows
            .iter()
            .map(|r| {
                let score = r.try_get("score").unwrap_or(0.0);
                SessionScoreResponse {
                    session_id: r.try_get("target_id").unwrap_or_default(),
                    score,
                    score_interval: ConfidenceInterval::exact(score.clamp(0.0, 1.0)),
                    chain_count: r.try_get("chain_count").unwrap_or(0),
                }
            })
            .collect();
        let total = sessions.len();
        Ok(SessionScoresListResponse { sessions, total })
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
            .map(|row| row.try_get::<f64, _>("score").unwrap_or(0.0))
            .collect::<Vec<_>>();
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

        let max_drift_delta = drift
            .signals
            .iter()
            .map(|signal| signal.delta.abs())
            .fold(0.0_f64, f64::max);

        let quality_action =
            if quality.total_events > 0 && quality.overall_avg < LOOP_QUALITY_THRESHOLD {
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
                value: quality.overall_avg,
                threshold: LOOP_QUALITY_THRESHOLD,
                action: quality_action,
            },
            LoopDiagnosisItem {
                metric: "drift_signal_count".into(),
                value: drift.signals.len() as f64,
                threshold: 0.0,
                action: drift_count_action,
            },
            LoopDiagnosisItem {
                metric: "drift_max_delta".into(),
                value: max_drift_delta,
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
             SUM(CASE WHEN JSON_UNQUOTE(JSON_EXTRACT(content, '$.safe_to_deliver')) = 'true' \
                  THEN 1 ELSE 0 END) AS safe_cnt \
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
        .await;

        let (total, safe) = match row {
            Ok(r) => (
                r.try_get::<i64, _>("total").unwrap_or(0),
                r.try_get::<i64, _>("safe_cnt").unwrap_or(0),
            ),
            Err(_) => (0, 0),
        };

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
             SUM(CASE WHEN JSON_UNQUOTE(JSON_EXTRACT(content, '$.safe_to_deliver')) = 'true' \
                  THEN 1 ELSE 0 END) AS safe_cnt \
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
        .unwrap_or_default();

        let agents = rows
            .iter()
            .map(|row| {
                let agent_id = row.try_get::<String, _>("agent_id").unwrap_or_default();
                let total = row.try_get::<i64, _>("total").unwrap_or(0);
                let safe = row.try_get::<i64, _>("safe_cnt").unwrap_or(0);
                let actual = trust_ratio(total, safe);
                SloEntry {
                    agent_id,
                    slo_name: "trust_ratio".into(),
                    target: TRUST_SLO_TARGET,
                    actual,
                    actual_interval: sampled_confidence_interval(actual, total),
                    met: actual >= TRUST_SLO_TARGET,
                }
            })
            .collect();

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
             SUM(CASE WHEN JSON_UNQUOTE(JSON_EXTRACT(content, '$.safe_to_deliver')) = 'true' \
                  THEN 1 ELSE 0 END) AS safe_cnt \
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
        .unwrap_or_default();

        let history = rows
            .iter()
            .map(|row| {
                let total = row.try_get::<i64, _>("total").unwrap_or(0);
                let safe = row.try_get::<i64, _>("safe_cnt").unwrap_or(0);
                let value = trust_ratio(total, safe);
                SloHistoryPoint {
                    date: row.try_get::<String, _>("dt").unwrap_or_default(),
                    value,
                    value_interval: sampled_confidence_interval(value, total),
                    target: TRUST_SLO_TARGET,
                    met: value >= TRUST_SLO_TARGET,
                }
            })
            .collect();

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

        let decision_row = query(
            "SELECT COUNT(*) AS cnt \
             FROM agent_events \
             WHERE user_id = ? \
               AND agent_id = ? AND event_type = 'llm_response' \
                 AND created_at > DATE_SUB(NOW(), INTERVAL ? DAY)",
        )
        .bind(user_id)
        .bind(agent_id)
        .bind(days)
        .fetch_one(&pool)
        .await;

        let decision = match decision_row {
            Ok(r) => DecisionMetrics {
                avg_quality: 0.0,
                total_decisions: r.try_get::<i64, _>("cnt").unwrap_or(0),
            },
            Err(_) => DecisionMetrics {
                avg_quality: 0.0,
                total_decisions: 0,
            },
        };

        let session_row = query(
            "SELECT COUNT(DISTINCT session_id) AS sess_cnt, \
             AVG(turn_count) AS avg_turns \
             FROM (SELECT session_id, COUNT(*) AS turn_count \
                    FROM agent_events \
                    WHERE user_id = ? \
                      AND agent_id = ? \
                      AND created_at > DATE_SUB(NOW(), INTERVAL ? DAY) \
                    GROUP BY session_id) sub",
        )
        .bind(user_id)
        .bind(agent_id)
        .bind(days)
        .fetch_one(&pool)
        .await;

        let session = match session_row {
            Ok(r) => SessionMetrics {
                unique_sessions: r.try_get::<i64, _>("sess_cnt").unwrap_or(0),
                avg_turns_per_session: r.try_get::<f64, _>("avg_turns").unwrap_or(0.0),
            },
            Err(_) => SessionMetrics {
                unique_sessions: 0,
                avg_turns_per_session: 0.0,
            },
        };

        let skill_row = query(
            "SELECT COUNT(*) AS total, \
             SUM(CASE WHEN execution_success = 1 THEN 1 ELSE 0 END) AS ok_cnt \
             FROM skill_selection_events \
             WHERE user_id = ? \
               AND created_at > DATE_SUB(NOW(), INTERVAL ? DAY)",
        )
        .bind(user_id)
        .bind(days)
        .fetch_one(&pool)
        .await;

        let skill = match skill_row {
            Ok(r) => {
                let total: i64 = r.try_get("total").unwrap_or(0);
                let success: i64 = r.try_get("ok_cnt").unwrap_or(0);
                let success_rate = skill_success_rate(total, success);
                SkillMetrics {
                    total_invocations: total,
                    success_count: success,
                    success_rate,
                    success_rate_interval: sampled_confidence_interval(success_rate, total),
                }
            }
            Err(_) => SkillMetrics {
                total_invocations: 0,
                success_count: 0,
                success_rate: 0.0,
                success_rate_interval: ConfidenceInterval::ZERO,
            },
        };

        Ok(ObservabilityMetricsResponse {
            agent_id: agent_id.to_string(),
            period_days: days,
            decision,
            session,
            skill,
        })
    }

    async fn memory_health(&self, user_id: &str) -> ServiceResult<MemoryHealthResponse> {
        let storage = self.memoria_get("/v1/health/storage", user_id).await?;
        let hygiene = self.memoria_get("/v1/health/hygiene", user_id).await?;

        let mem_count = storage.get("total").and_then(|v| v.as_i64()).unwrap_or(0);
        let active_count = storage
            .get("active")
            .and_then(|v| v.as_i64())
            .unwrap_or(mem_count);
        let inactive_count = storage
            .get("inactive")
            .and_then(|v| v.as_i64())
            .unwrap_or(mem_count.saturating_sub(active_count));

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
            total_memories: mem_count,
            active_memories: active_count,
            inactive_memories: inactive_count,
            stale_working_memories,
            orphaned_records,
            healthy: hygiene_issues == 0 && mem_count >= 0 && inactive_count >= 0,
        })
    }

    async fn memory_metrics(&self, user_id: &str) -> ServiceResult<MemoryMetricsResponse> {
        let storage = self.memoria_get("/v1/health/storage", user_id).await?;
        let analyze = self.memoria_get("/v1/health/analyze", user_id).await?;
        let hygiene = self.memoria_get("/v1/health/hygiene", user_id).await?;

        let total_memories = storage.get("total").and_then(|v| v.as_i64()).unwrap_or(0);
        let stale_count = hygiene
            .get("stale_working_memories")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        let mut weighted_confidence_sum = 0.0f64;
        let mut weighted_count = 0i64;
        if let Some(by_type) = analyze.as_object() {
            for stats in by_type.values() {
                let total = stats.get("total").and_then(|v| v.as_i64()).unwrap_or(0);
                let avg_conf = stats
                    .get("avg_confidence")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                weighted_confidence_sum += avg_conf * total as f64;
                weighted_count += total;
            }
        }
        let avg_confidence = if weighted_count > 0 {
            weighted_confidence_sum / weighted_count as f64
        } else {
            0.0
        };

        Ok(MemoryMetricsResponse {
            total_memories,
            avg_confidence,
            avg_confidence_interval: sampled_confidence_interval(avg_confidence, weighted_count),
            stale_count,
        })
    }

    async fn extract_training_data(
        &self,
        _user_id: &str,
        _request: TrainingDataExtractRequest,
    ) -> ServiceResult<TrainingDataExtractResponse> {
        Err(not_implemented(
            "Evaluation training data extraction is not implemented yet",
        ))
    }

    async fn export_training_data(
        &self,
        _user_id: &str,
        _dataset_id: &str,
        _format: &str,
    ) -> ServiceResult<TrainingDataExportResponse> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Training data export requires core pipeline module",
        ))
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
    fn classify_drift_severity_thresholds() {
        assert_eq!(classify_drift_severity(0.03), None);
        assert_eq!(classify_drift_severity(0.06), Some(DriftSeverity::Info));
        assert_eq!(classify_drift_severity(-0.12), Some(DriftSeverity::Warning));
        assert_eq!(classify_drift_severity(0.25), Some(DriftSeverity::Critical));
    }

    #[test]
    fn build_drift_signal_ignores_small_delta() {
        assert!(build_drift_signal("session".into(), None, 0.78, 0.75, 8).is_none());
    }

    #[test]
    fn build_drift_signal_sets_expected_fields() {
        let signal = build_drift_signal("session".into(), None, 0.55, 0.78, 12).unwrap();
        assert_eq!(signal.model, "session");
        assert_eq!(signal.template_id, None);
        assert!((signal.delta + 0.23).abs() < 1e-9);
        assert_eq!(signal.severity, DriftSeverity::Critical);
        assert_eq!(signal.sample_count, 12);
    }

    #[test]
    fn build_loop_actions_honors_dry_run_prefix() {
        let diagnoses = vec![
            LoopDiagnosisItem {
                metric: "quality_overall_avg".into(),
                value: 0.5,
                threshold: 0.7,
                action: LoopAction::Retune,
            },
            LoopDiagnosisItem {
                metric: "drift_signal_count".into(),
                value: 2.0,
                threshold: 0.0,
                action: LoopAction::Alert,
            },
            LoopDiagnosisItem {
                metric: "noop_metric".into(),
                value: 1.0,
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
    fn complement_interval_flips_bounds() {
        let interval = ConfidenceInterval::new(0.8, 0.7, 0.9);
        let complement = complement_interval(interval);
        assert_eq!(complement, ConfidenceInterval::new(0.2, 0.1, 0.3));
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
}
