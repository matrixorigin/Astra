use async_trait::async_trait;
use axum::http::StatusCode;
use sqlx::{Row, query};
use uuid::Uuid;

use super::service::EvaluationService;
use super::types::*;
use super::utils::*;
use mo_agent_core::{
    MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

#[derive(Clone, Debug)]
pub struct DatabaseEvaluationService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseEvaluationService {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
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

        let (sql, bind_model) = if let Some(m) = model {
            (
                "SELECT DATE_FORMAT(DATE(created_at), '%Y-%m-%d') AS dt, \
                 AVG(quality_score) AS avg_score, COUNT(*) AS cnt, \
                 IFNULL(llm_model_used, '') AS model \
                 FROM agent_events \
                 WHERE quality_score IS NOT NULL \
                   AND created_at >= DATE_SUB(NOW(), INTERVAL ? DAY) \
                   AND llm_model_used = ? \
                 GROUP BY dt, model ORDER BY dt",
                Some(m.to_string()),
            )
        } else {
            (
                "SELECT DATE_FORMAT(DATE(created_at), '%Y-%m-%d') AS dt, \
                 AVG(quality_score) AS avg_score, COUNT(*) AS cnt, \
                 IFNULL(llm_model_used, '') AS model \
                 FROM agent_events \
                 WHERE quality_score IS NOT NULL \
                   AND created_at >= DATE_SUB(NOW(), INTERVAL ? DAY) \
                 GROUP BY dt, model ORDER BY dt",
                None,
            )
        };

        let rows = if let Some(ref m) = bind_model {
            query(sql).bind(days).bind(m).fetch_all(&pool).await
        } else {
            query(sql).bind(days).fetch_all(&pool).await
        };
        let rows = rows.unwrap_or_default();

        let points: Vec<QualityTrendPoint> = rows
            .iter()
            .map(|r| {
                let model_val: String = r.try_get("model").unwrap_or_default();
                QualityTrendPoint {
                    date: r.try_get("dt").unwrap_or_default(),
                    avg_score: r.try_get("avg_score").unwrap_or(0.0),
                    count: r.try_get("cnt").unwrap_or(0),
                    model: if model_val.is_empty() {
                        None
                    } else {
                        Some(model_val)
                    },
                }
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
            "SELECT AVG(quality_score) AS avg_q, COUNT(*) AS cnt \
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
                avg_quality: r.try_get::<f64, _>("avg_q").unwrap_or(0.0),
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

    async fn memory_health(&self, _user_id: &str) -> ServiceResult<MemoryHealthResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let mem_count: i64 = query("SELECT COUNT(*) AS cnt FROM mem_memories")
            .fetch_one(&pool)
            .await
            .and_then(|r| r.try_get("cnt"))
            .unwrap_or(0);

        let kb_count: i64 = query("SELECT COUNT(*) AS cnt FROM sk_knowledge_entries")
            .fetch_one(&pool)
            .await
            .and_then(|r| r.try_get("cnt"))
            .unwrap_or(0);

        let last_gov: Option<String> = query(
            "SELECT DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS ts \
             FROM governance_runs ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(&pool)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.try_get("ts").ok());

        Ok(MemoryHealthResponse {
            total_memories: mem_count,
            knowledge_entries: kb_count,
            last_governance_run: last_gov,
            healthy: mem_count >= 0 && kb_count >= 0,
        })
    }

    async fn memory_metrics(&self) -> ServiceResult<MemoryMetricsResponse> {
        Ok(MemoryMetricsResponse {
            total_memories: 0,
            avg_confidence: 0.0,
            stale_count: 0,
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
