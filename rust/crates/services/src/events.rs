use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Acquire, MySql, QueryBuilder, Row, query};
use uuid::Uuid;

use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct EventCreateRequestData {
    pub session_id: String,
    pub event_type: String,
    pub content: String,
    pub agent_id: Option<String>,
    pub agent_version: Option<String>,
    pub parent_event_id: Option<String>,
    pub causal_chain_id: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventRecord {
    pub event_id: String,
    pub user_id: String,
    pub session_id: String,
    pub event_type: String,
    pub content: String,
    pub agent_id: Option<String>,
    pub agent_version: Option<String>,
    pub parent_event_id: Option<String>,
    pub causal_chain_id: String,
    pub metadata: serde_json::Value,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventListFilter {
    pub user_id: String,
    pub session_id: Option<String>,
    pub event_type: Option<String>,
    pub agent_id: Option<String>,
    pub causal_chain_id: Option<String>,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventListRecord {
    pub events: Vec<EventRecord>,
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait EventService: Send + Sync {
    async fn create_event(
        &self,
        user_id: String,
        request: EventCreateRequestData,
    ) -> Result<EventRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn list_events(
        &self,
        filter: EventListFilter,
    ) -> Result<EventListRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_event(
        &self,
        event_id: String,
        user_id: String,
    ) -> Result<EventRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_causal_chain(
        &self,
        causal_chain_id: String,
        user_id: String,
    ) -> Result<Vec<EventRecord>, (StatusCode, Json<ErrorResponse>)>;

    async fn get_session_events(
        &self,
        session_id: String,
        user_id: String,
        limit: u32,
        offset: u32,
    ) -> Result<EventListRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn delete_event(
        &self,
        event_id: String,
        user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)>;
}

// ── Database implementation ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DatabaseEventService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseEventService {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }

    pub fn event_record_from_row(
        row: sqlx::mysql::MySqlRow,
    ) -> Result<EventRecord, (StatusCode, Json<ErrorResponse>)> {
        let metadata_json: String = row
            .try_get("metadata_json")
            .unwrap_or_else(|_| "{}".to_string());

        Ok(EventRecord {
            event_id: row.try_get("event_id").map_err(internal_error)?,
            user_id: row.try_get("user_id").map_err(internal_error)?,
            session_id: row.try_get("session_id").map_err(internal_error)?,
            event_type: row.try_get("event_type").map_err(internal_error)?,
            content: row.try_get("content").map_err(internal_error)?,
            agent_id: row.try_get("agent_id").ok(),
            agent_version: row.try_get("agent_version").ok(),
            parent_event_id: row.try_get("parent_event_id").ok(),
            causal_chain_id: row.try_get("causal_chain_id").map_err(internal_error)?,
            metadata: serde_json::from_str(&metadata_json)
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

pub const EVENT_SELECT_COLS: &str = "\
    event_id, user_id, session_id, event_type, content, \
    agent_id, agent_version, parent_event_id, causal_chain_id, \
    IFNULL(CAST(`metadata` AS CHAR), '{}') AS metadata_json, \
    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at";

#[async_trait]
impl EventService for DatabaseEventService {
    async fn create_event(
        &self,
        user_id: String,
        request: EventCreateRequestData,
    ) -> Result<EventRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        // Start transaction for atomicity of INSERT event + UPDATE session
        let mut conn = pool.acquire().await.map_err(internal_error)?;
        let mut tx = conn.begin().await.map_err(internal_error)?;

        let session_row = query("SELECT user_id FROM agent_sessions WHERE session_id = ?")
            .bind(&request.session_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal_error)?;
        let session_row = session_row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Session {} not found", request.session_id),
            )
        })?;
        let session_owner: String = session_row.try_get("user_id").map_err(internal_error)?;
        if session_owner != user_id {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("Session {} not found", request.session_id),
            ));
        }

        let event_id = Uuid::new_v4().to_string();
        let causal_chain_id = request
            .causal_chain_id
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let metadata_str = request
            .metadata
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "{}".to_string());
        let agent_id = request.agent_id.unwrap_or_else(|| "system".to_string());
        let agent_version = request.agent_version.unwrap_or_else(|| "1.0.0".to_string());

        let meta_tool_name = request
            .metadata
            .as_ref()
            .and_then(|v| v.get("tool_name"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let meta_duration_ms = request
            .metadata
            .as_ref()
            .and_then(|v| v.get("duration_ms"))
            .and_then(|v| v.as_i64())
            .map(|v| v as i32);

        query(
            "INSERT INTO agent_events \
             (event_id, session_id, user_id, agent_id, agent_version, event_type, content, \
              parent_event_id, causal_chain_id, `metadata`, meta_tool_name, meta_duration_ms, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
        )
        .bind(&event_id)
        .bind(&request.session_id)
        .bind(&user_id)
        .bind(&agent_id)
        .bind(&agent_version)
        .bind(&request.event_type)
        .bind(&request.content)
        .bind(&request.parent_event_id)
        .bind(&causal_chain_id)
        .bind(&metadata_str)
        .bind(&meta_tool_name)
        .bind(meta_duration_ms)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;

        query(
            "UPDATE agent_sessions SET event_count = event_count + 1, updated_at = NOW() WHERE session_id = ?",
        )
        .bind(&request.session_id)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;

        let select_sql = format!(
            "SELECT {} FROM agent_events WHERE event_id = ?",
            EVENT_SELECT_COLS
        );
        let row = query(&select_sql)
            .bind(&event_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(internal_error)?;

        let result = Self::event_record_from_row(row)?;

        tx.commit().await.map_err(internal_error)?;

        Ok(result)
    }

    async fn list_events(
        &self,
        filter: EventListFilter,
    ) -> Result<EventListRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let mut count_qb = QueryBuilder::<MySql>::new(
            "SELECT COUNT(event_id) AS total FROM agent_events WHERE user_id = ",
        );
        count_qb.push_bind(&filter.user_id);
        if let Some(sid) = &filter.session_id {
            count_qb.push(" AND session_id = ");
            count_qb.push_bind(sid);
        }
        if let Some(et) = &filter.event_type {
            count_qb.push(" AND event_type = ");
            count_qb.push_bind(et);
        }
        if let Some(aid) = &filter.agent_id {
            count_qb.push(" AND agent_id = ");
            count_qb.push_bind(aid);
        }
        if let Some(ccid) = &filter.causal_chain_id {
            count_qb.push(" AND causal_chain_id = ");
            count_qb.push_bind(ccid);
        }
        let total_row = count_qb
            .build()
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;
        let total = total_row.try_get::<i64, _>("total").unwrap_or(0);

        let mut list_qb = QueryBuilder::<MySql>::new(format!(
            "SELECT {} FROM agent_events WHERE user_id = ",
            EVENT_SELECT_COLS
        ));
        list_qb.push_bind(&filter.user_id);
        if let Some(sid) = &filter.session_id {
            list_qb.push(" AND session_id = ");
            list_qb.push_bind(sid);
        }
        if let Some(et) = &filter.event_type {
            list_qb.push(" AND event_type = ");
            list_qb.push_bind(et);
        }
        if let Some(aid) = &filter.agent_id {
            list_qb.push(" AND agent_id = ");
            list_qb.push_bind(aid);
        }
        if let Some(ccid) = &filter.causal_chain_id {
            list_qb.push(" AND causal_chain_id = ");
            list_qb.push_bind(ccid);
        }
        list_qb.push(" ORDER BY created_at DESC LIMIT ");
        list_qb.push_bind(i64::from(filter.limit));
        list_qb.push(" OFFSET ");
        list_qb.push_bind(i64::from(filter.offset));

        let rows = list_qb
            .build()
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            events.push(Self::event_record_from_row(row)?);
        }

        Ok(EventListRecord {
            events,
            total,
            limit: filter.limit,
            offset: filter.offset,
        })
    }

    async fn get_event(
        &self,
        event_id: String,
        user_id: String,
    ) -> Result<EventRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let select_sql = format!(
            "SELECT {} FROM agent_events WHERE event_id = ?",
            EVENT_SELECT_COLS
        );
        let row = query(&select_sql)
            .bind(&event_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;

        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Event {} not found", event_id),
            )
        })?;
        let record = Self::event_record_from_row(row)?;
        if record.user_id != user_id {
            return Err(error_response(StatusCode::FORBIDDEN, "Permission denied"));
        }
        Ok(record)
    }

    async fn get_causal_chain(
        &self,
        causal_chain_id: String,
        user_id: String,
    ) -> Result<Vec<EventRecord>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let select_sql = format!(
            "SELECT {} FROM agent_events WHERE causal_chain_id = ? AND user_id = ? ORDER BY created_at ASC",
            EVENT_SELECT_COLS
        );
        let rows = query(&select_sql)
            .bind(&causal_chain_id)
            .bind(&user_id)
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            events.push(Self::event_record_from_row(row)?);
        }
        Ok(events)
    }

    async fn get_session_events(
        &self,
        session_id: String,
        user_id: String,
        limit: u32,
        offset: u32,
    ) -> Result<EventListRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let session_row = query("SELECT user_id FROM agent_sessions WHERE session_id = ?")
            .bind(&session_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;
        let session_row = session_row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Session {} not found", session_id),
            )
        })?;
        let owner: String = session_row.try_get("user_id").map_err(internal_error)?;
        if owner != user_id {
            return Err(error_response(StatusCode::FORBIDDEN, "Permission denied"));
        }

        let count_row =
            query("SELECT COUNT(event_id) AS total FROM agent_events WHERE session_id = ?")
                .bind(&session_id)
                .fetch_one(&pool)
                .await
                .map_err(internal_error)?;
        let total = count_row.try_get::<i64, _>("total").unwrap_or(0);

        let select_sql = format!(
            "SELECT {} FROM agent_events WHERE session_id = ? ORDER BY created_at ASC LIMIT ? OFFSET ?",
            EVENT_SELECT_COLS
        );
        let rows = query(&select_sql)
            .bind(&session_id)
            .bind(i64::from(limit))
            .bind(i64::from(offset))
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            events.push(Self::event_record_from_row(row)?);
        }
        Ok(EventListRecord {
            events,
            total,
            limit,
            offset,
        })
    }

    async fn delete_event(
        &self,
        event_id: String,
        user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let select_sql = format!(
            "SELECT {} FROM agent_events WHERE event_id = ?",
            EVENT_SELECT_COLS
        );
        let row = query(&select_sql)
            .bind(&event_id)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?;
        let row = row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Event {} not found", event_id),
            )
        })?;
        let record = Self::event_record_from_row(row)?;
        if record.user_id != user_id {
            return Err(error_response(StatusCode::FORBIDDEN, "Permission denied"));
        }

        query("DELETE FROM agent_events WHERE event_id = ?")
            .bind(&event_id)
            .execute(&pool)
            .await
            .map_err(internal_error)?;

        Ok(())
    }
}

// ── Noop implementation ──────────────────────────────────────────────────────

pub struct UnconfiguredEventService;

#[async_trait]
impl EventService for UnconfiguredEventService {
    async fn create_event(
        &self,
        _: String,
        _: EventCreateRequestData,
    ) -> Result<EventRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("event service not configured"))
    }
    async fn list_events(
        &self,
        _: EventListFilter,
    ) -> Result<EventListRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("event service not configured"))
    }
    async fn get_event(
        &self,
        _: String,
        _: String,
    ) -> Result<EventRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("event service not configured"))
    }
    async fn get_causal_chain(
        &self,
        _: String,
        _: String,
    ) -> Result<Vec<EventRecord>, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("event service not configured"))
    }
    async fn get_session_events(
        &self,
        _: String,
        _: String,
        _: u32,
        _: u32,
    ) -> Result<EventListRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("event service not configured"))
    }
    async fn delete_event(
        &self,
        _: String,
        _: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        Err(internal_error("event service not configured"))
    }
}

// ── HTTP types ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct EventCreateRequest {
    pub session_id: String,
    pub event_type: String,
    pub content: String,
    pub agent_id: Option<String>,
    pub agent_version: Option<String>,
    pub parent_event_id: Option<String>,
    pub causal_chain_id: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Deserialize, Default)]
pub struct EventListQuery {
    pub session_id: Option<String>,
    pub event_type: Option<String>,
    pub agent_id: Option<String>,
    pub causal_chain_id: Option<String>,
    #[serde(default = "default_event_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

pub fn default_event_limit() -> u32 {
    50
}

#[derive(Deserialize, Default)]
pub struct SessionEventQuery {
    #[serde(default = "default_session_event_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

pub fn default_session_event_limit() -> u32 {
    100
}

#[derive(Serialize, PartialEq)]
pub struct EventResponse {
    pub event_id: String,
    pub user_id: String,
    pub session_id: String,
    pub event_type: String,
    pub content: String,
    pub agent_id: Option<String>,
    pub agent_version: Option<String>,
    pub parent_event_id: Option<String>,
    pub causal_chain_id: String,
    pub metadata: serde_json::Value,
    pub created_at: String,
}

#[derive(Serialize, PartialEq)]
pub struct EventListResponse {
    pub events: Vec<EventResponse>,
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
}

impl From<EventRecord> for EventResponse {
    fn from(r: EventRecord) -> Self {
        Self {
            event_id: r.event_id,
            user_id: r.user_id,
            session_id: r.session_id,
            event_type: r.event_type,
            content: r.content,
            agent_id: r.agent_id,
            agent_version: r.agent_version,
            parent_event_id: r.parent_event_id,
            causal_chain_id: r.causal_chain_id,
            metadata: r.metadata,
            created_at: r.created_at,
        }
    }
}

impl From<EventListRecord> for EventListResponse {
    fn from(r: EventListRecord) -> Self {
        Self {
            events: r.events.into_iter().map(EventResponse::from).collect(),
            total: r.total,
            limit: r.limit,
            offset: r.offset,
        }
    }
}
