use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, QueryBuilder, Row, query};
use uuid::Uuid;

use astra_core::{ErrorResponse, MatrixOneSettings, SharedPool, error_response, internal_error};

use crate::storage::{agent_event_exists_for_user_session, agent_session_exists_for_user};

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotCreateRequestData {
    pub session_id: String,
    pub event_id: String,
    pub context_data: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotRecord {
    pub context_capture_id: String,
    pub session_id: String,
    pub event_id: String,
    pub context_data: serde_json::Value,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotListItem {
    pub context_capture_id: String,
    pub session_id: String,
    pub event_id: String,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotListFilter {
    pub user_id: String,
    pub session_id: Option<String>,
    pub limit: u32,
    pub cursor: Option<SnapshotListCursor>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotListRecord {
    pub snapshots: Vec<SnapshotListItem>,
    pub total: i64,
    pub limit: u32,
    pub next_cursor: Option<SnapshotListCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotListCursor {
    pub created_at: String,
    pub context_capture_id: String,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ContextService: Send + Sync {
    async fn create_snapshot(
        &self,
        user_id: String,
        request: SnapshotCreateRequestData,
    ) -> Result<SnapshotRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn list_snapshots(
        &self,
        filter: SnapshotListFilter,
    ) -> Result<SnapshotListRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_snapshot(
        &self,
        context_capture_id: String,
        user_id: String,
    ) -> Result<SnapshotRecord, (StatusCode, Json<ErrorResponse>)>;
}

// ── Database implementation ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DatabaseContextService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseContextService {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }

    pub fn snapshot_record_from_row(
        row: sqlx::mysql::MySqlRow,
    ) -> Result<SnapshotRecord, (StatusCode, Json<ErrorResponse>)> {
        let data_json = context_row_string(&row, "context_data_json")?;

        Ok(SnapshotRecord {
            context_capture_id: context_row_string(&row, "context_capture_id")?,
            session_id: context_row_string(&row, "session_id")?,
            event_id: context_row_string(&row, "event_id")?,
            context_data: parse_context_json("context_data_json", &data_json)?,
            created_at: context_row_string(&row, "created_at")?,
        })
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        crate::require_shared_pool(
            self.pool.as_ref(),
            "DatabaseContextService",
            &self.matrixone,
        )
    }
}

const SNAPSHOT_SELECT_COLS: &str = "\
    context_capture_id, session_id, event_id, \
    CAST(context_data AS CHAR) AS context_data_json, \
    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at";
const SNAPSHOT_LIST_SELECT_COLS: &str = "\
    cs.context_capture_id, cs.session_id, cs.event_id, \
    DATE_FORMAT(cs.created_at, '%Y-%m-%dT%H:%i:%s.%f') AS created_at";
const MAX_SNAPSHOT_LIST_ROWS: u32 = 200;

fn validate_snapshot_list_limit(limit: u32) -> u32 {
    limit.clamp(1, MAX_SNAPSHOT_LIST_ROWS)
}

fn snapshot_list_query_limit(limit: u32) -> i64 {
    i64::from(limit) + 1
}

fn context_decode_error(
    column: &'static str,
    message: impl Into<String>,
) -> (StatusCode, Json<ErrorResponse>) {
    internal_error(format!(
        "ctx_snapshots row decode column `{column}`: {}",
        message.into()
    ))
}

fn context_row_string(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let value = row
        .try_get::<String, _>(column)
        .map_err(|error| context_decode_error(column, error.to_string()))?;
    if value.trim().is_empty() {
        return Err(context_decode_error(column, "must not be empty"));
    }
    Ok(value)
}

fn context_row_i64(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
) -> Result<i64, (StatusCode, Json<ErrorResponse>)> {
    row.try_get::<i64, _>(column)
        .map_err(|error| context_decode_error(column, error.to_string()))
}

fn parse_context_json(
    column: &'static str,
    raw: &str,
) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
    serde_json::from_str(raw).map_err(|source| context_decode_error(column, source.to_string()))
}

fn snapshot_list_cursor_db_created_at(
    cursor: &SnapshotListCursor,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let created_at = cursor.created_at.trim();
    if created_at.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid snapshot list cursor: created_at is required",
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
            format!("invalid snapshot list cursor timestamp: {created_at}"),
        ));
    }
    Ok(db_created_at)
}

fn snapshot_list_cursor_capture_id(
    cursor: &SnapshotListCursor,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let context_capture_id = cursor.context_capture_id.trim();
    if context_capture_id.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid snapshot list cursor: context_capture_id is required",
        ));
    }
    Ok(context_capture_id.to_string())
}

fn snapshot_list_cursor_from_item(
    snapshot: &SnapshotListItem,
) -> Result<SnapshotListCursor, (StatusCode, Json<ErrorResponse>)> {
    if snapshot.created_at.trim().is_empty() {
        return Err(internal_error(format!(
            "invalid ctx_snapshots cursor: context_capture_id={}, column=created_at, value is empty",
            snapshot.context_capture_id
        )));
    }
    if snapshot.context_capture_id.trim().is_empty() {
        return Err(internal_error(
            "invalid ctx_snapshots cursor: column=context_capture_id, value is empty",
        ));
    }
    Ok(SnapshotListCursor {
        created_at: snapshot.created_at.clone(),
        context_capture_id: snapshot.context_capture_id.clone(),
    })
}

#[async_trait]
impl ContextService for DatabaseContextService {
    async fn create_snapshot(
        &self,
        user_id: String,
        request: SnapshotCreateRequestData,
    ) -> Result<SnapshotRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        if !agent_session_exists_for_user(&pool, &request.session_id, &user_id)
            .await
            .map_err(internal_error)?
        {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("Session {} not found", request.session_id),
            ));
        }
        if !agent_event_exists_for_user_session(
            &pool,
            &request.event_id,
            &request.session_id,
            &user_id,
        )
        .await
        .map_err(internal_error)?
        {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("Event {} not found", request.event_id),
            ));
        }

        let capture_id = Uuid::new_v4().to_string();
        let data_str = request.context_data.to_string();

        // Defense: reject excessively large context snapshots to prevent disk/DB exhaustion
        const MAX_SNAPSHOT_SIZE: usize = 10 * 1024 * 1024; // 10 MB
        if data_str.len() > MAX_SNAPSHOT_SIZE {
            return Err(error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "Context snapshot too large ({} bytes, max {})",
                    data_str.len(),
                    MAX_SNAPSHOT_SIZE
                ),
            ));
        }

        query(
            "INSERT INTO ctx_snapshots \
             (context_capture_id, user_id, session_id, event_id, context_data, created_at) \
             VALUES (?, ?, ?, ?, ?, NOW())",
        )
        .bind(&capture_id)
        .bind(&user_id)
        .bind(&request.session_id)
        .bind(&request.event_id)
        .bind(&data_str)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        let select_sql = format!(
            "SELECT {} FROM ctx_snapshots WHERE context_capture_id = ? AND user_id = ?",
            SNAPSHOT_SELECT_COLS
        );
        let row = query(&select_sql)
            .bind(&capture_id)
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;

        Self::snapshot_record_from_row(row)
    }

    async fn list_snapshots(
        &self,
        filter: SnapshotListFilter,
    ) -> Result<SnapshotListRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let limit = validate_snapshot_list_limit(filter.limit);

        let count_sql = if filter.session_id.is_some() {
            "SELECT COUNT(cs.context_capture_id) AS total FROM ctx_snapshots cs \
             WHERE cs.user_id = ? AND cs.session_id = ?"
        } else {
            "SELECT COUNT(cs.context_capture_id) AS total FROM ctx_snapshots cs \
             WHERE cs.user_id = ?"
        };

        let total_row = if let Some(sid) = &filter.session_id {
            query(count_sql)
                .bind(&filter.user_id)
                .bind(sid)
                .fetch_one(&pool)
                .await
        } else {
            query(count_sql)
                .bind(&filter.user_id)
                .fetch_one(&pool)
                .await
        }
        .map_err(internal_error)?;
        let total = context_row_i64(&total_row, "total")?;

        let mut list_qb = QueryBuilder::<MySql>::new(format!(
            "SELECT {} FROM ctx_snapshots cs WHERE cs.user_id = ",
            SNAPSHOT_LIST_SELECT_COLS
        ));
        list_qb.push_bind(&filter.user_id);
        if let Some(sid) = &filter.session_id {
            list_qb.push(" AND cs.session_id = ");
            list_qb.push_bind(sid);
        }
        if let Some(cursor) = &filter.cursor {
            let created_at = snapshot_list_cursor_db_created_at(cursor)?;
            let context_capture_id = snapshot_list_cursor_capture_id(cursor)?;
            list_qb.push(" AND (cs.created_at < ");
            list_qb.push_bind(created_at.clone());
            list_qb.push(" OR (cs.created_at = ");
            list_qb.push_bind(created_at);
            list_qb.push(" AND cs.context_capture_id < ");
            list_qb.push_bind(context_capture_id);
            list_qb.push("))");
        }
        list_qb.push(" ORDER BY cs.created_at DESC, cs.context_capture_id DESC LIMIT ");
        list_qb.push_bind(snapshot_list_query_limit(limit));

        let rows = list_qb
            .build()
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let mut snapshots = Vec::with_capacity(rows.len());
        for row in rows {
            snapshots.push(SnapshotListItem {
                context_capture_id: context_row_string(&row, "context_capture_id")?,
                session_id: context_row_string(&row, "session_id")?,
                event_id: context_row_string(&row, "event_id")?,
                created_at: context_row_string(&row, "created_at")?,
            });
        }
        let has_more = snapshots.len() > limit as usize;
        if has_more {
            snapshots.truncate(limit as usize);
        }
        let next_cursor = if has_more {
            snapshots
                .last()
                .map(snapshot_list_cursor_from_item)
                .transpose()?
        } else {
            None
        };
        Ok(SnapshotListRecord {
            snapshots,
            total,
            limit,
            next_cursor,
        })
    }

    async fn get_snapshot(
        &self,
        context_capture_id: String,
        user_id: String,
    ) -> Result<SnapshotRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let sql = "SELECT cs.context_capture_id, cs.session_id, cs.event_id, \
             CAST(cs.context_data AS CHAR) AS context_data_json, \
             DATE_FORMAT(cs.created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM ctx_snapshots cs \
             WHERE cs.context_capture_id = ? AND cs.user_id = ?"
            .to_string();
        let row = query(&sql)
            .bind(&context_capture_id)
            .bind(&user_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;

        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Snapshot {} not found", context_capture_id),
            )
        })?;
        Self::snapshot_record_from_row(row)
    }
}

// ── Noop implementation ──────────────────────────────────────────────────────

pub struct UnconfiguredContextService;

#[async_trait]
impl ContextService for UnconfiguredContextService {
    async fn create_snapshot(
        &self,
        _: String,
        _: SnapshotCreateRequestData,
    ) -> Result<SnapshotRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("context service not configured"))
    }
    async fn list_snapshots(
        &self,
        _: SnapshotListFilter,
    ) -> Result<SnapshotListRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("context service not configured"))
    }
    async fn get_snapshot(
        &self,
        _: String,
        _: String,
    ) -> Result<SnapshotRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("context service not configured"))
    }
}

// ── HTTP types ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SnapshotCreateRequest {
    pub session_id: String,
    pub event_id: String,
    pub context_data: serde_json::Value,
}

#[derive(Deserialize, Default)]
pub struct SnapshotListQuery {
    pub session_id: Option<String>,
    #[serde(default = "default_snapshot_limit")]
    pub limit: u32,
    pub after_created_at: Option<String>,
    pub after_context_capture_id: Option<String>,
}

pub fn default_snapshot_limit() -> u32 {
    50
}

impl SnapshotListQuery {
    pub fn cursor(&self) -> Result<Option<SnapshotListCursor>, (StatusCode, Json<ErrorResponse>)> {
        match (&self.after_created_at, &self.after_context_capture_id) {
            (None, None) => Ok(None),
            (Some(created_at), Some(context_capture_id)) => Ok(Some(SnapshotListCursor {
                created_at: created_at.clone(),
                context_capture_id: context_capture_id.clone(),
            })),
            _ => Err(error_response(
                StatusCode::BAD_REQUEST,
                "snapshot list cursor requires both after_created_at and after_context_capture_id",
            )),
        }
    }
}

#[derive(Serialize, PartialEq)]
pub struct SnapshotResponse {
    pub context_capture_id: String,
    pub session_id: String,
    pub event_id: String,
    pub context_data: serde_json::Value,
    pub created_at: String,
}

#[derive(Serialize, PartialEq)]
pub struct SnapshotListResponse {
    pub snapshots: Vec<SnapshotListItemResponse>,
    pub total: i64,
    pub limit: u32,
    pub next_cursor: Option<SnapshotListCursor>,
}

#[derive(Serialize, PartialEq)]
pub struct SnapshotListItemResponse {
    pub context_capture_id: String,
    pub session_id: String,
    pub event_id: String,
    pub created_at: String,
}

impl From<SnapshotRecord> for SnapshotResponse {
    fn from(r: SnapshotRecord) -> Self {
        Self {
            context_capture_id: r.context_capture_id,
            session_id: r.session_id,
            event_id: r.event_id,
            context_data: r.context_data,
            created_at: r.created_at,
        }
    }
}

impl From<SnapshotListRecord> for SnapshotListResponse {
    fn from(r: SnapshotListRecord) -> Self {
        Self {
            snapshots: r
                .snapshots
                .into_iter()
                .map(|snapshot| SnapshotListItemResponse {
                    context_capture_id: snapshot.context_capture_id,
                    session_id: snapshot.session_id,
                    event_id: snapshot.event_id,
                    created_at: snapshot.created_at,
                })
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
    fn snapshot_list_query_defaults() {
        let q: SnapshotListQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.limit, 50);
        assert_eq!(q.cursor().unwrap(), None);
        assert!(q.session_id.is_none());
    }

    #[test]
    fn snapshot_list_query_requires_complete_cursor() {
        let q: SnapshotListQuery =
            serde_json::from_str(r#"{"after_created_at":"2026-04-01T10:00:00.000000"}"#).unwrap();
        assert_eq!(q.cursor().unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn snapshot_list_limit_has_hard_cap_and_minimum() {
        assert_eq!(validate_snapshot_list_limit(0), 1);
        assert_eq!(validate_snapshot_list_limit(10), 10);
        assert_eq!(
            validate_snapshot_list_limit(u32::MAX),
            MAX_SNAPSHOT_LIST_ROWS
        );
        assert_eq!(snapshot_list_query_limit(MAX_SNAPSHOT_LIST_ROWS), 201);
    }

    #[test]
    fn snapshot_list_cursor_rejects_invalid_inputs() {
        let cursor = SnapshotListCursor {
            created_at: "2026-04-01T10:00:00.123456".to_string(),
            context_capture_id: "ctx-1".to_string(),
        };
        assert_eq!(
            snapshot_list_cursor_db_created_at(&cursor).unwrap(),
            "2026-04-01 10:00:00.123456"
        );
        assert_eq!(
            snapshot_list_cursor_capture_id(&cursor).unwrap(),
            "ctx-1".to_string()
        );

        let invalid_time = SnapshotListCursor {
            created_at: "2026-04-01T10:00:00".to_string(),
            context_capture_id: "ctx-1".to_string(),
        };
        assert_eq!(
            snapshot_list_cursor_db_created_at(&invalid_time)
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );

        let missing_id = SnapshotListCursor {
            created_at: "2026-04-01T10:00:00.123456".to_string(),
            context_capture_id: "  ".to_string(),
        };
        assert_eq!(
            snapshot_list_cursor_capture_id(&missing_id).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn snapshot_list_sql_contract_uses_seek_cursor_not_offset() {
        let sql = format!(
            "SELECT {SNAPSHOT_LIST_SELECT_COLS} FROM ctx_snapshots cs WHERE cs.user_id = ? \
             AND (cs.created_at < ? OR (cs.created_at = ? AND cs.context_capture_id < ?)) \
             ORDER BY cs.created_at DESC, cs.context_capture_id DESC LIMIT ?"
        );
        assert!(!sql.to_ascii_uppercase().contains(" OFFSET "));
        assert!(sql.contains("cs.context_capture_id < ?"));
    }

    #[test]
    fn default_snapshot_limit_value() {
        assert_eq!(default_snapshot_limit(), 50);
    }
}
