use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, QueryBuilder, Row, query};
use uuid::Uuid;

use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct DecisionCreateRequestData {
    pub session_id: String,
    pub event_id: String,
    pub context_capture_id: String,
    pub decision_type: String,
    pub decision_output: serde_json::Value,
    pub model_params: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecisionRecord {
    pub decision_id: String,
    pub session_id: String,
    pub event_id: String,
    pub context_capture_id: String,
    pub decision_type: String,
    pub decision_output: serde_json::Value,
    pub model_params: serde_json::Value,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecisionWithContextRecord {
    pub decision: DecisionRecord,
    pub context: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecisionListFilter {
    pub user_id: String,
    pub session_id: Option<String>,
    pub decision_type: Option<String>,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecisionListRecord {
    pub decisions: Vec<DecisionRecord>,
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait DecisionService: Send + Sync {
    async fn record_decision(
        &self,
        user_id: String,
        request: DecisionCreateRequestData,
    ) -> Result<DecisionRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn list_decisions(
        &self,
        filter: DecisionListFilter,
    ) -> Result<DecisionListRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_decision(
        &self,
        decision_id: String,
        user_id: String,
    ) -> Result<DecisionRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_decision_with_context(
        &self,
        decision_id: String,
        user_id: String,
    ) -> Result<DecisionWithContextRecord, (StatusCode, Json<ErrorResponse>)>;
}

// ── Database implementation ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DatabaseDecisionService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseDecisionService {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }

    pub fn decision_record_from_row(
        row: sqlx::mysql::MySqlRow,
    ) -> Result<DecisionRecord, (StatusCode, Json<ErrorResponse>)> {
        let output_json: String = row
            .try_get("decision_output_json")
            .unwrap_or_else(|_| "{}".to_string());
        let params_json: String = row
            .try_get("model_params_json")
            .unwrap_or_else(|_| "{}".to_string());

        Ok(DecisionRecord {
            decision_id: row.try_get("decision_id").map_err(internal_error)?,
            session_id: row.try_get("session_id").map_err(internal_error)?,
            event_id: row.try_get("event_id").map_err(internal_error)?,
            context_capture_id: row.try_get("context_capture_id").map_err(internal_error)?,
            decision_type: row.try_get("decision_type").map_err(internal_error)?,
            decision_output: serde_json::from_str(&output_json)
                .unwrap_or(serde_json::Value::Object(Default::default())),
            model_params: serde_json::from_str(&params_json)
                .unwrap_or(serde_json::Value::Object(Default::default())),
            created_at: row.try_get("created_at").unwrap_or_default(),
        })
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

pub const DECISION_SELECT_COLS: &str = "\
    decision_id, session_id, event_id, context_capture_id, decision_type, \
    IFNULL(CAST(decision_output AS CHAR), '{}') AS decision_output_json, \
    IFNULL(CAST(model_params AS CHAR), '{}') AS model_params_json, \
    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at";

#[async_trait]
impl DecisionService for DatabaseDecisionService {
    async fn record_decision(
        &self,
        user_id: String,
        request: DecisionCreateRequestData,
    ) -> Result<DecisionRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let session_row = query("SELECT user_id FROM agent_sessions WHERE session_id = ?")
            .bind(&request.session_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;
        let session_row = session_row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Session {} not found", request.session_id),
            )
        })?;
        let owner: String = session_row.try_get("user_id").map_err(internal_error)?;
        if owner != user_id {
            return Err(error_response(StatusCode::FORBIDDEN, "Permission denied"));
        }

        let decision_id = Uuid::new_v4().to_string();
        let output_str = request.decision_output.to_string();
        let params_str = request
            .model_params
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "{}".to_string());

        query(
            "INSERT INTO ctx_decision_audits \
             (decision_id, session_id, event_id, context_capture_id, decision_type, \
              decision_output, model_params, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, NOW())",
        )
        .bind(&decision_id)
        .bind(&request.session_id)
        .bind(&request.event_id)
        .bind(&request.context_capture_id)
        .bind(&request.decision_type)
        .bind(&output_str)
        .bind(&params_str)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        let select_sql = format!(
            "SELECT {} FROM ctx_decision_audits WHERE decision_id = ?",
            DECISION_SELECT_COLS
        );
        let row = query(&select_sql)
            .bind(&decision_id)
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;

        Self::decision_record_from_row(row)
    }

    async fn list_decisions(
        &self,
        filter: DecisionListFilter,
    ) -> Result<DecisionListRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let mut count_qb = QueryBuilder::<MySql>::new(
            "SELECT COUNT(d.decision_id) AS total FROM ctx_decision_audits d \
             JOIN agent_sessions s ON d.session_id = s.session_id \
             WHERE s.user_id = ",
        );
        count_qb.push_bind(&filter.user_id);
        if let Some(sid) = &filter.session_id {
            count_qb.push(" AND d.session_id = ");
            count_qb.push_bind(sid);
        }
        if let Some(dt) = &filter.decision_type {
            count_qb.push(" AND d.decision_type = ");
            count_qb.push_bind(dt);
        }
        let total_row = count_qb
            .build()
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;
        let total = total_row.try_get::<i64, _>("total").unwrap_or(0);

        let mut list_qb = QueryBuilder::<MySql>::new(
            "SELECT d.decision_id, d.session_id, d.event_id, d.context_capture_id, d.decision_type, \
             IFNULL(CAST(d.decision_output AS CHAR), '{}') AS decision_output_json, \
             IFNULL(CAST(d.model_params AS CHAR), '{}') AS model_params_json, \
             DATE_FORMAT(d.created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM ctx_decision_audits d \
             JOIN agent_sessions s ON d.session_id = s.session_id \
             WHERE s.user_id = ".to_string(),
        );
        list_qb.push_bind(&filter.user_id);
        if let Some(sid) = &filter.session_id {
            list_qb.push(" AND d.session_id = ");
            list_qb.push_bind(sid);
        }
        if let Some(dt) = &filter.decision_type {
            list_qb.push(" AND d.decision_type = ");
            list_qb.push_bind(dt);
        }
        list_qb.push(" ORDER BY d.created_at DESC LIMIT ");
        list_qb.push_bind(i64::from(filter.limit));
        list_qb.push(" OFFSET ");
        list_qb.push_bind(i64::from(filter.offset));

        let rows = list_qb
            .build()
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;
        let mut decisions = Vec::with_capacity(rows.len());
        for row in rows {
            decisions.push(Self::decision_record_from_row(row)?);
        }

        Ok(DecisionListRecord {
            decisions,
            total,
            limit: filter.limit,
            offset: filter.offset,
        })
    }

    async fn get_decision(
        &self,
        decision_id: String,
        user_id: String,
    ) -> Result<DecisionRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let sql = "SELECT d.decision_id, d.session_id, d.event_id, d.context_capture_id, d.decision_type, \
             IFNULL(CAST(d.decision_output AS CHAR), '{}') AS decision_output_json, \
             IFNULL(CAST(d.model_params AS CHAR), '{}') AS model_params_json, \
             DATE_FORMAT(d.created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM ctx_decision_audits d \
             JOIN agent_sessions s ON d.session_id = s.session_id \
             WHERE d.decision_id = ? AND s.user_id = ?".to_string();
        let row = query(&sql)
            .bind(&decision_id)
            .bind(&user_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;

        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Decision {} not found", decision_id),
            )
        })?;
        Self::decision_record_from_row(row)
    }

    async fn get_decision_with_context(
        &self,
        decision_id: String,
        user_id: String,
    ) -> Result<DecisionWithContextRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let sql = "SELECT d.decision_id, d.session_id, d.event_id, d.context_capture_id, d.decision_type, \
             IFNULL(CAST(d.decision_output AS CHAR), '{}') AS decision_output_json, \
             IFNULL(CAST(d.model_params AS CHAR), '{}') AS model_params_json, \
             DATE_FORMAT(d.created_at, '%Y-%m-%dT%H:%i:%s') AS created_at, \
             IFNULL(CAST(cs.context_data AS CHAR), '{}') AS context_json \
             FROM ctx_decision_audits d \
             JOIN agent_sessions s ON d.session_id = s.session_id \
             LEFT JOIN ctx_snapshots cs ON d.context_capture_id = cs.context_capture_id \
             WHERE d.decision_id = ? AND s.user_id = ?".to_string();
        let row = query(&sql)
            .bind(&decision_id)
            .bind(&user_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;

        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Decision {} not found", decision_id),
            )
        })?;

        let context_json: String = row
            .try_get("context_json")
            .unwrap_or_else(|_| "{}".to_string());
        let context: Option<serde_json::Value> = serde_json::from_str(&context_json).ok();

        let decision = Self::decision_record_from_row(row)?;

        Ok(DecisionWithContextRecord { decision, context })
    }
}

// ── Noop implementation ──────────────────────────────────────────────────────

pub struct UnconfiguredDecisionService;

#[async_trait]
impl DecisionService for UnconfiguredDecisionService {
    async fn record_decision(
        &self,
        _: String,
        _: DecisionCreateRequestData,
    ) -> Result<DecisionRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("decision service not configured"))
    }
    async fn list_decisions(
        &self,
        _: DecisionListFilter,
    ) -> Result<DecisionListRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("decision service not configured"))
    }
    async fn get_decision(
        &self,
        _: String,
        _: String,
    ) -> Result<DecisionRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("decision service not configured"))
    }
    async fn get_decision_with_context(
        &self,
        _: String,
        _: String,
    ) -> Result<DecisionWithContextRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("decision service not configured"))
    }
}

// ── HTTP types ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct DecisionCreateRequest {
    pub session_id: String,
    pub event_id: String,
    pub context_capture_id: String,
    pub decision_type: String,
    pub decision_output: serde_json::Value,
    pub model_params: Option<serde_json::Value>,
}

#[derive(Deserialize, Default)]
pub struct DecisionListQuery {
    pub session_id: Option<String>,
    pub decision_type: Option<String>,
    #[serde(default = "default_decision_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

pub fn default_decision_limit() -> u32 {
    50
}

#[derive(Serialize, PartialEq)]
pub struct DecisionResponse {
    pub decision_id: String,
    pub session_id: String,
    pub event_id: String,
    pub context_capture_id: String,
    pub decision_type: String,
    pub decision_output: serde_json::Value,
    pub model_params: serde_json::Value,
    pub created_at: String,
}

#[derive(Serialize, PartialEq)]
pub struct DecisionWithContextResponse {
    pub decision_id: String,
    pub session_id: String,
    pub event_id: String,
    pub context_capture_id: String,
    pub decision_type: String,
    pub decision_output: serde_json::Value,
    pub model_params: serde_json::Value,
    pub context: Option<serde_json::Value>,
    pub created_at: String,
}

#[derive(Serialize, PartialEq)]
pub struct DecisionListResponse {
    pub decisions: Vec<DecisionResponse>,
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
}

impl From<DecisionRecord> for DecisionResponse {
    fn from(r: DecisionRecord) -> Self {
        Self {
            decision_id: r.decision_id,
            session_id: r.session_id,
            event_id: r.event_id,
            context_capture_id: r.context_capture_id,
            decision_type: r.decision_type,
            decision_output: r.decision_output,
            model_params: r.model_params,
            created_at: r.created_at,
        }
    }
}

impl From<DecisionWithContextRecord> for DecisionWithContextResponse {
    fn from(r: DecisionWithContextRecord) -> Self {
        Self {
            decision_id: r.decision.decision_id,
            session_id: r.decision.session_id,
            event_id: r.decision.event_id,
            context_capture_id: r.decision.context_capture_id,
            decision_type: r.decision.decision_type,
            decision_output: r.decision.decision_output,
            model_params: r.decision.model_params,
            context: r.context,
            created_at: r.decision.created_at,
        }
    }
}

impl From<DecisionListRecord> for DecisionListResponse {
    fn from(r: DecisionListRecord) -> Self {
        Self {
            decisions: r
                .decisions
                .into_iter()
                .map(DecisionResponse::from)
                .collect(),
            total: r.total,
            limit: r.limit,
            offset: r.offset,
        }
    }
}
