use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};
use uuid::Uuid;

use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

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
    pub offset: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotListRecord {
    pub snapshots: Vec<SnapshotListItem>,
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
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
        let data_json: String = row
            .try_get("context_data_json")
            .unwrap_or_else(|_| "{}".to_string());

        Ok(SnapshotRecord {
            context_capture_id: row.try_get("context_capture_id").map_err(internal_error)?,
            session_id: row.try_get("session_id").map_err(internal_error)?,
            event_id: row.try_get("event_id").map_err(internal_error)?,
            context_data: serde_json::from_str(&data_json)
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

const SNAPSHOT_SELECT_COLS: &str = "\
    context_capture_id, session_id, event_id, \
    IFNULL(CAST(context_data AS CHAR), '{}') AS context_data_json, \
    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at";
const SNAPSHOT_LIST_SELECT_COLS: &str = "\
    cs.context_capture_id, cs.session_id, cs.event_id, \
    DATE_FORMAT(cs.created_at, '%Y-%m-%dT%H:%i:%s') AS created_at";
const MAX_SNAPSHOT_LIST_ROWS: u32 = 200;

#[async_trait]
impl ContextService for DatabaseContextService {
    async fn create_snapshot(
        &self,
        user_id: String,
        request: SnapshotCreateRequestData,
    ) -> Result<SnapshotRecord, (StatusCode, Json<ErrorResponse>)> {
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

        let capture_id = Uuid::new_v4().to_string();
        let data_str = request.context_data.to_string();

        query(
            "INSERT INTO ctx_snapshots \
             (context_capture_id, session_id, event_id, context_data, created_at) \
             VALUES (?, ?, ?, ?, NOW())",
        )
        .bind(&capture_id)
        .bind(&request.session_id)
        .bind(&request.event_id)
        .bind(&data_str)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        let select_sql = format!(
            "SELECT {} FROM ctx_snapshots WHERE context_capture_id = ?",
            SNAPSHOT_SELECT_COLS
        );
        let row = query(&select_sql)
            .bind(&capture_id)
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
        let limit = filter.limit.min(MAX_SNAPSHOT_LIST_ROWS);

        let count_sql = if filter.session_id.is_some() {
            "SELECT COUNT(cs.context_capture_id) AS total FROM ctx_snapshots cs \
             JOIN agent_sessions s ON cs.session_id = s.session_id \
             WHERE s.user_id = ? AND cs.session_id = ?"
        } else {
            "SELECT COUNT(cs.context_capture_id) AS total FROM ctx_snapshots cs \
             JOIN agent_sessions s ON cs.session_id = s.session_id \
             WHERE s.user_id = ?"
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
        let total = total_row.try_get::<i64, _>("total").unwrap_or(0);

        let list_sql = if filter.session_id.is_some() {
            format!(
                "SELECT {} \
                 FROM ctx_snapshots cs \
                 JOIN agent_sessions s ON cs.session_id = s.session_id \
                 WHERE s.user_id = ? AND cs.session_id = ? \
                 ORDER BY cs.created_at DESC LIMIT ? OFFSET ?",
                SNAPSHOT_LIST_SELECT_COLS
            )
        } else {
            format!(
                "SELECT {} \
                 FROM ctx_snapshots cs \
                 JOIN agent_sessions s ON cs.session_id = s.session_id \
                 WHERE s.user_id = ? \
                 ORDER BY cs.created_at DESC LIMIT ? OFFSET ?",
                SNAPSHOT_LIST_SELECT_COLS
            )
        };

        let rows = if let Some(sid) = &filter.session_id {
            query(&list_sql)
                .bind(&filter.user_id)
                .bind(sid)
                .bind(i64::from(limit))
                .bind(i64::from(filter.offset))
                .fetch_all(&pool)
                .await
        } else {
            query(&list_sql)
                .bind(&filter.user_id)
                .bind(i64::from(limit))
                .bind(i64::from(filter.offset))
                .fetch_all(&pool)
                .await
        }
        .map_err(internal_error)?;

        let mut snapshots = Vec::with_capacity(rows.len());
        for row in rows {
            snapshots.push(SnapshotListItem {
                context_capture_id: row.try_get("context_capture_id").map_err(internal_error)?,
                session_id: row.try_get("session_id").map_err(internal_error)?,
                event_id: row.try_get("event_id").map_err(internal_error)?,
                created_at: row.try_get("created_at").unwrap_or_default(),
            });
        }
        Ok(SnapshotListRecord {
            snapshots,
            total,
            limit,
            offset: filter.offset,
        })
    }

    async fn get_snapshot(
        &self,
        context_capture_id: String,
        user_id: String,
    ) -> Result<SnapshotRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let sql = "SELECT cs.context_capture_id, cs.session_id, cs.event_id, \
             IFNULL(CAST(cs.context_data AS CHAR), '{}') AS context_data_json, \
             DATE_FORMAT(cs.created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM ctx_snapshots cs \
             JOIN agent_sessions s ON cs.session_id = s.session_id \
             WHERE cs.context_capture_id = ? AND s.user_id = ?"
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
    #[serde(default)]
    pub offset: u32,
}

pub fn default_snapshot_limit() -> u32 {
    50
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
    pub offset: u32,
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
            offset: r.offset,
        }
    }
}
