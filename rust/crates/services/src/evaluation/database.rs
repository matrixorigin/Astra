use async_trait::async_trait;
use axum::http::StatusCode;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::Value;
use sqlx::{Row, query};
use uuid::Uuid;

use super::service::EvaluationService;
use super::types::*;
use super::utils::*;
use astra_core::{
    MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

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
        _user_id: &str,
        days: i32,
        model: Option<&str>,
    ) -> ServiceResult<QualityTrendResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let rows = if model.is_some() {
            Vec::new()
        } else {
            query(
                "SELECT DATE_FORMAT(DATE(created_at), '%Y-%m-%d') AS dt, \
                 AVG(score) AS avg_score, COUNT(*) AS cnt \
                 FROM eval_quality_assessments \
                 WHERE created_at >= DATE_SUB(NOW(), INTERVAL ? DAY) \
                 GROUP BY dt ORDER BY dt",
            )
            .bind(days)
            .fetch_all(&pool)
            .await
            .unwrap_or_default()
        };

        let points: Vec<QualityTrendPoint> = rows
            .iter()
            .map(|r| QualityTrendPoint {
                date: r.try_get("dt").unwrap_or_default(),
                avg_score: r.try_get("avg_score").unwrap_or(0.0),
                count: r.try_get("cnt").unwrap_or(0),
                model: None,
            })
            .collect();

        let total_events: i64 = points.iter().map(|p| p.count).sum();
        let overall_avg = compute_overall_avg(&points);

        Ok(QualityTrendResponse {
            points,
            overall_avg,
            total_events,
        })
    }

    async fn detect_drift(&self, _user_id: &str) -> ServiceResult<DriftDetectResponse> {
        Ok(DriftDetectResponse {
            signals: Vec::new(),
            checked_at: now_iso(),
        })
    }

    async fn get_gate_history(
        &self,
        _user_id: &str,
        limit: i32,
    ) -> ServiceResult<GateHistoryResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let rows = query(
            "SELECT gate_id, change_type, change_id, sessions_tested, \
             error_rate, score_delta, passed, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM eval_gate_results ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&pool)
        .await
        .unwrap_or_default();

        let gates: Vec<GateResultResponse> = rows
            .iter()
            .map(|r| GateResultResponse {
                gate_id: r.try_get("gate_id").unwrap_or_default(),
                change_type: r.try_get("change_type").unwrap_or_default(),
                change_id: r.try_get("change_id").unwrap_or_default(),
                sessions_tested: r.try_get("sessions_tested").unwrap_or(0),
                error_rate: r.try_get("error_rate").unwrap_or(0.0),
                score_delta: r.try_get("score_delta").unwrap_or(0.0),
                passed: r.try_get::<i8, _>("passed").unwrap_or(0) != 0,
                created_at: r.try_get("created_at").ok(),
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
        let mean_confidence = 0.0;
        let mean_quality = 0.0;
        let calibration_error = compute_calibration_error(mean_confidence, mean_quality);
        let bias = mean_confidence - mean_quality;
        let (adjustment_multiplier, adjustment_reason) =
            compute_adjustment(calibration_error, bias);

        Ok(CalibrationResponse {
            mean_confidence,
            mean_quality,
            calibration_error,
            bias,
            sample_count: 0,
            adjustment_multiplier,
            adjustment_reason,
        })
    }

    async fn get_session_scores(
        &self,
        _user_id: &str,
        limit: i32,
        min_score: f64,
    ) -> ServiceResult<SessionScoresListResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let rows = query(
            "SELECT target_id, score, COALESCE(step_count, 0) AS chain_count \
             FROM eval_quality_assessments \
             WHERE level = 'session' AND score >= ? \
             ORDER BY updated_at DESC LIMIT ?",
        )
        .bind(min_score)
        .bind(limit)
        .fetch_all(&pool)
        .await
        .unwrap_or_default();

        let sessions: Vec<SessionScoreResponse> = rows
            .iter()
            .map(|r| SessionScoreResponse {
                session_id: r.try_get("target_id").unwrap_or_default(),
                score: r.try_get("score").unwrap_or(0.0),
                chain_count: r.try_get("chain_count").unwrap_or(0),
            })
            .collect();
        let total = sessions.len();
        Ok(SessionScoresListResponse { sessions, total })
    }

    async fn validate_gate(
        &self,
        _user_id: &str,
        request: GateValidateRequest,
    ) -> ServiceResult<GateValidateResponse> {
        Ok(GateValidateResponse {
            gate_id: Uuid::new_v4().to_string(),
            change_type: request.change_type,
            change_id: request.change_id,
            sessions_tested: 0,
            error_rate: 0.0,
            score_delta: 0.0,
            passed: true,
            details: "Gate validation stub — core regression runner not yet ported".into(),
        })
    }

    async fn run_drift_pipeline(&self, _user_id: &str) -> ServiceResult<DriftPipelineResponse> {
        Ok(DriftPipelineResponse {
            run_id: Uuid::new_v4().to_string(),
            signals_detected: 0,
            signals: Vec::new(),
            started_at: now_iso(),
        })
    }

    async fn run_closed_loop(
        &self,
        _user_id: &str,
        _days: i32,
        dry_run: bool,
    ) -> ServiceResult<ClosedLoopResponse> {
        Ok(ClosedLoopResponse {
            loop_id: Uuid::new_v4().to_string(),
            dry_run,
            diagnoses: Vec::new(),
            actions_taken: Vec::new(),
        })
    }

    async fn trust_report(
        &self,
        _user_id: &str,
        agent_id: &str,
        days: i32,
    ) -> ServiceResult<TrustReportResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let row = query(
            "SELECT COUNT(*) AS total, \
             SUM(CASE WHEN JSON_UNQUOTE(JSON_EXTRACT(content, '$.safe_to_deliver')) = 'true' \
                 THEN 1 ELSE 0 END) AS safe_cnt \
             FROM agent_events \
             WHERE event_type = 'hallucination_check' \
               AND agent_id = ? \
               AND created_at > DATE_SUB(NOW(), INTERVAL ? DAY)",
        )
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
        Ok(TrustReportResponse {
            agent_id: agent_id.to_string(),
            period_days: days,
            total_checks: total,
            safe_count: safe,
            trust_ratio: ratio,
            hallucination_rate: 1.0 - ratio,
        })
    }

    async fn slo_dashboard(
        &self,
        _user_id: &str,
        period_days: i32,
    ) -> ServiceResult<SloDashboardResponse> {
        Ok(SloDashboardResponse {
            period_days,
            agents: Vec::new(),
        })
    }

    async fn slo_history(
        &self,
        _user_id: &str,
        agent_id: &str,
        days: i32,
    ) -> ServiceResult<SloHistoryResponse> {
        Ok(SloHistoryResponse {
            agent_id: agent_id.to_string(),
            days,
            history: Vec::new(),
        })
    }

    async fn observability_metrics(
        &self,
        _user_id: &str,
        agent_id: &str,
        days: i32,
    ) -> ServiceResult<ObservabilityMetricsResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let decision_row = query(
            "SELECT COUNT(*) AS cnt \
             FROM agent_events \
             WHERE agent_id = ? AND event_type = 'llm_response' \
                AND created_at > DATE_SUB(NOW(), INTERVAL ? DAY)",
        )
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
                   WHERE agent_id = ? \
                     AND created_at > DATE_SUB(NOW(), INTERVAL ? DAY) \
                   GROUP BY session_id) sub",
        )
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
             WHERE created_at > DATE_SUB(NOW(), INTERVAL ? DAY)",
        )
        .bind(days)
        .fetch_one(&pool)
        .await;

        let skill = match skill_row {
            Ok(r) => {
                let total: i64 = r.try_get("total").unwrap_or(0);
                let success: i64 = r.try_get("ok_cnt").unwrap_or(0);
                SkillMetrics {
                    total_invocations: total,
                    success_count: success,
                    success_rate: skill_success_rate(total, success),
                }
            }
            Err(_) => SkillMetrics {
                total_invocations: 0,
                success_count: 0,
                success_rate: 0.0,
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
            stale_count,
        })
    }

    async fn extract_training_data(
        &self,
        _user_id: &str,
        request: TrainingDataExtractRequest,
    ) -> ServiceResult<TrainingDataExtractResponse> {
        Ok(TrainingDataExtractResponse {
            dataset_id: Uuid::new_v4().to_string(),
            samples_extracted: 0,
            quality_threshold: request.min_quality,
            status: "stub — training pipeline not yet ported".into(),
        })
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
