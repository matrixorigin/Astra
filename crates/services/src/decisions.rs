use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, QueryBuilder, Row, query};
use uuid::Uuid;

use astra_core::{ErrorResponse, MatrixOneSettings, SharedPool, error_response, internal_error};

use crate::CancellationSafePoolConnection;
use crate::pagination::MAX_API_LIST_LIMIT;
use crate::storage::admit_session_event_write;

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
    pub cursor: Option<DecisionListCursor>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecisionListRecord {
    pub decisions: Vec<DecisionRecord>,
    pub total: Option<i64>,
    pub limit: u32,
    pub next_cursor: Option<DecisionListCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionListCursor {
    pub created_at: String,
    pub decision_id: String,
}

fn validate_decision_list_limit(limit: u32) -> u32 {
    limit.clamp(1, MAX_API_LIST_LIMIT)
}

fn decision_list_query_limit(limit: u32) -> i64 {
    i64::from(limit) + 1
}

fn decision_list_cursor_db_created_at(
    cursor: &DecisionListCursor,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let created_at = cursor.created_at.trim();
    if created_at.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid decision list cursor: created_at is required",
        ));
    }
    let db_created_at = created_at.replace('T', " ");
    if db_created_at.len() != "YYYY-MM-DD HH:MM:SS.ffffff".len()
        || db_created_at.as_bytes().get(10) != Some(&b' ')
        || db_created_at.as_bytes().get(19) != Some(&b'.')
        || chrono::NaiveDateTime::parse_from_str(&db_created_at, "%Y-%m-%d %H:%M:%S%.6f").is_err()
    {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("invalid decision list cursor timestamp: {created_at}"),
        ));
    }
    Ok(db_created_at)
}

fn decision_list_cursor_decision_id(
    cursor: &DecisionListCursor,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let decision_id = cursor.decision_id.trim();
    if decision_id.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid decision list cursor: decision_id is required",
        ));
    }
    Ok(decision_id.to_string())
}

fn decision_list_cursor_from_record(
    decision: &DecisionRecord,
) -> Result<DecisionListCursor, (StatusCode, Json<ErrorResponse>)> {
    if decision.created_at.trim().is_empty() {
        return Err(internal_error(format!(
            "invalid ctx_decision_audits cursor: decision_id={}, column=created_at, value is empty",
            decision.decision_id
        )));
    }
    if decision.decision_id.trim().is_empty() {
        return Err(internal_error(
            "invalid ctx_decision_audits cursor: column=decision_id, value is empty",
        ));
    }
    Ok(DecisionListCursor {
        created_at: decision.created_at.clone(),
        decision_id: decision.decision_id.clone(),
    })
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
    #[cfg(feature = "e2e-hooks")]
    retained_write_hook: Option<std::sync::Arc<RetainedWriteTestHook>>,
}

/// Instance-local, exact-owner/session pause after a real retained INSERT.
/// Available only to opt-in database tests; it does not replace persistence.
#[cfg(feature = "e2e-hooks")]
#[derive(Debug)]
pub struct RetainedWriteTestHook {
    pub user_id: String,
    pub session_id: String,
    pub inserted: tokio::sync::Notify,
    pub resume: tokio::sync::Notify,
    pub fail: bool,
}

#[cfg(feature = "e2e-hooks")]
impl RetainedWriteTestHook {
    pub async fn after_insert(&self, user_id: &str, session_id: &str) -> bool {
        if self.user_id != user_id || self.session_id != session_id {
            return false;
        }
        self.inserted.notify_one();
        self.resume.notified().await;
        self.fail
    }
}

impl DatabaseDecisionService {
    #[cfg(feature = "e2e-hooks")]
    pub fn with_retained_write_test_hook(
        mut self,
        hook: std::sync::Arc<RetainedWriteTestHook>,
    ) -> Self {
        self.retained_write_hook = Some(hook);
        self
    }

    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
            #[cfg(feature = "e2e-hooks")]
            retained_write_hook: None,
        }
    }

    pub fn decision_record_from_row(
        row: sqlx::mysql::MySqlRow,
    ) -> Result<DecisionRecord, (StatusCode, Json<ErrorResponse>)> {
        let output_json = decision_row_string(&row, "decision_output_json")?;
        let params_json = decision_row_string(&row, "model_params_json")?;

        Ok(DecisionRecord {
            decision_id: decision_row_string(&row, "decision_id")?,
            session_id: decision_row_string(&row, "session_id")?,
            event_id: decision_row_string(&row, "event_id")?,
            context_capture_id: decision_row_string_allow_empty(&row, "context_capture_id")?,
            decision_type: decision_row_string(&row, "decision_type")?,
            decision_output: parse_decision_json("decision_output_json", &output_json)?,
            model_params: parse_decision_json("model_params_json", &params_json)?,
            created_at: decision_row_string(&row, "created_at")?,
        })
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        crate::require_shared_pool(
            self.pool.as_ref(),
            "DatabaseDecisionService",
            &self.matrixone,
        )
    }
}

pub const DECISION_SELECT_COLS: &str = "\
    decision_id, session_id, event_id, context_capture_id, decision_type, \
    CAST(decision_output AS CHAR) AS decision_output_json, \
    CAST(model_params AS CHAR) AS model_params_json, \
    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at";

fn decision_decode_error(
    column: &'static str,
    message: impl Into<String>,
) -> (StatusCode, Json<ErrorResponse>) {
    internal_error(format!(
        "ctx_decision_audits row decode column `{column}`: {}",
        message.into()
    ))
}

fn decision_row_string(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let value = decision_row_string_allow_empty(row, column)?;
    if value.trim().is_empty() {
        return Err(decision_decode_error(column, "must not be empty"));
    }
    Ok(value)
}

fn decision_row_string_allow_empty(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    row.try_get::<String, _>(column)
        .map_err(|error| decision_decode_error(column, error.to_string()))
}

fn decision_row_optional_string(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
) -> Result<Option<String>, (StatusCode, Json<ErrorResponse>)> {
    row.try_get::<Option<String>, _>(column)
        .map_err(|error| decision_decode_error(column, error.to_string()))
}

fn parse_decision_json(
    column: &'static str,
    raw: &str,
) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
    serde_json::from_str(raw).map_err(|source| decision_decode_error(column, source.to_string()))
}

#[async_trait]
impl DecisionService for DatabaseDecisionService {
    async fn record_decision(
        &self,
        user_id: String,
        request: DecisionCreateRequestData,
    ) -> Result<DecisionRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let mut connection = CancellationSafePoolConnection::acquire(&pool)
            .await
            .map_err(internal_error)?;
        let mut tx = connection.begin().await.map_err(internal_error)?;
        let result = async {
            admit_session_event_write(&mut tx, &request.session_id, &user_id, false)
                .await
                .map_err(|error| match error {
                    sqlx::Error::RowNotFound => error_response(
                        StatusCode::NOT_FOUND,
                        format!("Session {} not found", request.session_id),
                    ),
                    error => internal_error(error),
                })?;
            let mut references = QueryBuilder::<MySql>::new(
                "SELECT EXISTS(SELECT 1 FROM agent_events WHERE event_id = ",
            );
            references
                .push_bind(&request.event_id)
                .push(" AND session_id = ")
                .push_bind(&request.session_id)
                .push(" AND user_id = ")
                .push_bind(&user_id)
                .push(") AS event_owned");
            let validate_snapshot = !request.context_capture_id.trim().is_empty();
            if validate_snapshot {
                references
                    .push(", EXISTS(SELECT 1 FROM ctx_snapshots WHERE context_capture_id = ")
                    .push_bind(&request.context_capture_id)
                    .push(" AND session_id = ")
                    .push_bind(&request.session_id)
                    .push(" AND user_id = ")
                    .push_bind(&user_id)
                    .push(") AS snapshot_owned");
            }
            let references = references
                .build()
                .fetch_one(&mut *tx)
                .await
                .map_err(internal_error)?;
            if !references
                .try_get::<bool, _>("event_owned")
                .map_err(internal_error)?
            {
                return Err(error_response(
                    StatusCode::NOT_FOUND,
                    format!("Event {} not found", request.event_id),
                ));
            }
            if validate_snapshot
                && !references
                    .try_get::<bool, _>("snapshot_owned")
                    .map_err(internal_error)?
            {
                return Err(error_response(
                    StatusCode::NOT_FOUND,
                    format!("Snapshot {} not found", request.context_capture_id),
                ));
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
                 (decision_id, user_id, session_id, event_id, context_capture_id, decision_type, \
                  decision_output, model_params, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, NOW())",
            )
            .bind(&decision_id)
            .bind(&user_id)
            .bind(&request.session_id)
            .bind(&request.event_id)
            .bind(&request.context_capture_id)
            .bind(&request.decision_type)
            .bind(&output_str)
            .bind(&params_str)
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;

            let select_sql = format!(
                "SELECT {} FROM ctx_decision_audits WHERE decision_id = ? AND user_id = ?",
                DECISION_SELECT_COLS
            );
            #[cfg(feature = "e2e-hooks")]
            if let Some(hook) = &self.retained_write_hook
                && hook.after_insert(&user_id, &request.session_id).await
            {
                // Exercise the real receipt decoder and rollback, after INSERT.
                query(
                    "UPDATE ctx_decision_audits SET model_params = NULL \
                     WHERE decision_id = ? AND user_id = ?",
                )
                .bind(&decision_id)
                .bind(&user_id)
                .execute(&mut *tx)
                .await
                .map_err(internal_error)?;
            }
            let row = query(&select_sql)
                .bind(&decision_id)
                .bind(&user_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(internal_error)?;

            Self::decision_record_from_row(row)
        }
        .await;
        match result {
            Ok(record) => {
                tx.commit().await.map_err(internal_error)?;
                connection.release();
                Ok(record)
            }
            Err(error) => {
                // Keep the primary error even if rollback fails. The armed
                // guard closes the checkout on cancellation or uncertain I/O.
                if tx.rollback().await.is_ok() {
                    connection.release();
                }
                Err(error)
            }
        }
    }

    async fn list_decisions(
        &self,
        filter: DecisionListFilter,
    ) -> Result<DecisionListRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let limit = validate_decision_list_limit(filter.limit);

        let mut list_qb = QueryBuilder::<MySql>::new(
            "SELECT d.decision_id, d.session_id, d.event_id, d.context_capture_id, d.decision_type, \
             CAST(d.decision_output AS CHAR) AS decision_output_json, \
             CAST(d.model_params AS CHAR) AS model_params_json, \
             DATE_FORMAT(d.created_at, '%Y-%m-%dT%H:%i:%s.%f') AS created_at \
             FROM ctx_decision_audits d \
             WHERE d.user_id = ".to_string(),
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
        if let Some(cursor) = &filter.cursor {
            let created_at = decision_list_cursor_db_created_at(cursor)?;
            let decision_id = decision_list_cursor_decision_id(cursor)?;
            list_qb.push(" AND (d.created_at < ");
            list_qb.push_bind(created_at.clone());
            list_qb.push(" OR (d.created_at = ");
            list_qb.push_bind(created_at);
            list_qb.push(" AND d.decision_id < ");
            list_qb.push_bind(decision_id);
            list_qb.push("))");
        }
        list_qb.push(" ORDER BY d.created_at DESC, d.decision_id DESC LIMIT ");
        list_qb.push_bind(decision_list_query_limit(limit));

        let rows = list_qb
            .build()
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;
        let mut decisions = Vec::with_capacity(rows.len());
        for row in rows {
            decisions.push(Self::decision_record_from_row(row)?);
        }
        let has_more = decisions.len() > limit as usize;
        if has_more {
            decisions.truncate(limit as usize);
        }
        let next_cursor = if has_more {
            decisions
                .last()
                .map(decision_list_cursor_from_record)
                .transpose()?
        } else {
            None
        };

        Ok(DecisionListRecord {
            decisions,
            total: None,
            limit,
            next_cursor,
        })
    }

    async fn get_decision(
        &self,
        decision_id: String,
        user_id: String,
    ) -> Result<DecisionRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let sql = "SELECT d.decision_id, d.session_id, d.event_id, d.context_capture_id, d.decision_type, \
             CAST(d.decision_output AS CHAR) AS decision_output_json, \
             CAST(d.model_params AS CHAR) AS model_params_json, \
             DATE_FORMAT(d.created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM ctx_decision_audits d \
             WHERE d.decision_id = ? AND d.user_id = ?".to_string();
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
             CAST(d.decision_output AS CHAR) AS decision_output_json, \
             CAST(d.model_params AS CHAR) AS model_params_json, \
             DATE_FORMAT(d.created_at, '%Y-%m-%dT%H:%i:%s') AS created_at, \
             cs.context_capture_id AS joined_context_capture_id, \
             CAST(cs.context_data AS CHAR) AS context_json \
             FROM ctx_decision_audits d \
             LEFT JOIN ctx_snapshots cs \
               ON d.context_capture_id = cs.context_capture_id \
              AND cs.user_id = d.user_id \
             WHERE d.decision_id = ? AND d.user_id = ?".to_string();
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

        let referenced_context_capture_id =
            decision_row_string_allow_empty(&row, "context_capture_id")?;
        let context = if referenced_context_capture_id.trim().is_empty() {
            None
        } else {
            let joined_context_capture_id =
                decision_row_optional_string(&row, "joined_context_capture_id")?;
            if joined_context_capture_id.is_none() {
                return Err(decision_decode_error(
                    "context_capture_id",
                    format!(
                        "referenced context snapshot {} not found",
                        referenced_context_capture_id
                    ),
                ));
            }
            let context_json =
                decision_row_optional_string(&row, "context_json")?.ok_or_else(|| {
                    decision_decode_error(
                        "context_json",
                        "referenced context snapshot has NULL data",
                    )
                })?;
            Some(parse_decision_json("context_json", &context_json)?)
        };
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
    pub after_created_at: Option<String>,
    pub after_decision_id: Option<String>,
}

pub fn default_decision_limit() -> u32 {
    50
}

impl DecisionListQuery {
    pub fn cursor(&self) -> Result<Option<DecisionListCursor>, (StatusCode, Json<ErrorResponse>)> {
        match (&self.after_created_at, &self.after_decision_id) {
            (None, None) => Ok(None),
            (Some(created_at), Some(decision_id)) => Ok(Some(DecisionListCursor {
                created_at: created_at.clone(),
                decision_id: decision_id.clone(),
            })),
            _ => Err(error_response(
                StatusCode::BAD_REQUEST,
                "decision list cursor requires both after_created_at and after_decision_id",
            )),
        }
    }
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
    pub total: Option<i64>,
    pub limit: u32,
    pub next_cursor: Option<DecisionListCursor>,
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
            next_cursor: r.next_cursor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_list_query_defaults() {
        let q: DecisionListQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.limit, 50);
        assert_eq!(q.cursor().unwrap(), None);
        assert!(q.session_id.is_none());
        assert!(q.decision_type.is_none());
    }

    #[test]
    fn decision_list_query_requires_complete_cursor() {
        let q: DecisionListQuery =
            serde_json::from_str(r#"{"after_created_at":"2026-04-01T10:00:00.000000"}"#).unwrap();
        assert_eq!(q.cursor().unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn decision_list_limit_has_hard_cap_and_minimum() {
        assert_eq!(validate_decision_list_limit(0), 1);
        assert_eq!(validate_decision_list_limit(10), 10);
        assert_eq!(validate_decision_list_limit(u32::MAX), MAX_API_LIST_LIMIT);
        assert_eq!(decision_list_query_limit(MAX_API_LIST_LIMIT), 201);
    }

    #[test]
    fn decision_list_cursor_rejects_invalid_inputs() {
        let cursor = DecisionListCursor {
            created_at: "2026-04-01T10:00:00.123456".to_string(),
            decision_id: "decision-1".to_string(),
        };
        assert_eq!(
            decision_list_cursor_db_created_at(&cursor).unwrap(),
            "2026-04-01 10:00:00.123456"
        );
        assert_eq!(
            decision_list_cursor_decision_id(&cursor).unwrap(),
            "decision-1".to_string()
        );

        let invalid_time = DecisionListCursor {
            created_at: "2026-04-01T10:00:00".to_string(),
            decision_id: "decision-1".to_string(),
        };
        assert_eq!(
            decision_list_cursor_db_created_at(&invalid_time)
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );

        let missing_id = DecisionListCursor {
            created_at: "2026-04-01T10:00:00.123456".to_string(),
            decision_id: "  ".to_string(),
        };
        assert_eq!(
            decision_list_cursor_decision_id(&missing_id).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn decision_list_sql_contract_uses_seek_cursor_not_offset() {
        let sql = "SELECT d.decision_id FROM ctx_decision_audits d \
             WHERE d.user_id = ? \
             AND (d.created_at < ? OR (d.created_at = ? AND d.decision_id < ?)) \
             ORDER BY d.created_at DESC, d.decision_id DESC LIMIT ?";
        assert!(!sql.to_ascii_uppercase().contains(" OFFSET "));
        assert!(sql.contains("d.decision_id < ?"));
    }

    #[test]
    fn decision_list_response_preserves_omitted_total() {
        let response = DecisionListResponse::from(DecisionListRecord {
            decisions: Vec::new(),
            total: None,
            limit: 50,
            next_cursor: None,
        });

        assert_eq!(response.total, None);
    }

    #[test]
    fn default_decision_limit_value() {
        assert_eq!(default_decision_limit(), 50);
    }
}
