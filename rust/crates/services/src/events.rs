use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Acquire, MySql, QueryBuilder, Row, query};
use uuid::Uuid;

use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

use crate::pagination::clamp_api_list_pagination;

const MAX_CAUSAL_CHAIN_EVENTS: i64 = 500;

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct EventCreateRequestData {
    pub session_id: String,
    pub event_type: String,
    pub content: String,
    pub agent_id: Option<String>,
    pub agent_version: Option<String>,
    pub parent_event_id: Option<String>,
    pub parent_event_ids: Option<Vec<String>>,
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
    pub parent_event_ids: Vec<String>,
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

fn metadata_tool_name(metadata: Option<&serde_json::Value>) -> Option<String> {
    metadata
        .and_then(|v| v.get("tool_name").or_else(|| v.get("name")))
        .and_then(|v| v.as_str())
        .map(|s| s.trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
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
            parent_event_ids: Vec::new(),
            causal_chain_id: row.try_get("causal_chain_id").map_err(internal_error)?,
            metadata: serde_json::from_str(&metadata_json)
                .unwrap_or(serde_json::Value::Object(Default::default())),
            created_at: row.try_get("created_at").unwrap_or_default(),
        })
    }

    async fn hydrate_parent_event_ids<'e, E>(
        executor: E,
        records: &mut [EventRecord],
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)>
    where
        E: sqlx::Executor<'e, Database = sqlx::MySql>,
    {
        let event_ids: Vec<String> = records
            .iter()
            .map(|record| record.event_id.clone())
            .collect();
        let parent_id_map = crate::storage::load_agent_event_parent_ids(executor, &event_ids)
            .await
            .map_err(internal_error)?;
        for record in records {
            record.parent_event_ids = crate::storage::normalized_parent_event_ids(
                record.parent_event_id.as_deref(),
                parent_id_map.get(&record.event_id).map(Vec::as_slice),
            );
        }
        Ok(())
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

pub const EVENT_DETAIL_SELECT_COLS: &str = "\
    event_id, user_id, session_id, event_type, content, \
    agent_id, agent_version, parent_event_id, causal_chain_id, \
    IFNULL(CAST(`metadata` AS CHAR), '{}') AS metadata_json, \
    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at";

pub const EVENT_LIST_SELECT_COLS: &str = "\
    event_id, user_id, session_id, event_type, \
    CASE \
        WHEN content IS NULL THEN '' \
        WHEN CHAR_LENGTH(content) <= 280 THEN content \
        ELSE CONCAT(SUBSTRING(content, 1, 280), '...') \
    END AS content, \
    agent_id, \
    NULL AS agent_version, \
    parent_event_id, \
    causal_chain_id, \
    '{}' AS metadata_json, \
    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at";

#[async_trait]
impl EventService for DatabaseEventService {
    async fn create_event(
        &self,
        user_id: String,
        request: EventCreateRequestData,
    ) -> Result<EventRecord, (StatusCode, Json<ErrorResponse>)> {
        let EventCreateRequestData {
            session_id,
            event_type,
            content,
            agent_id,
            agent_version,
            parent_event_id,
            parent_event_ids,
            causal_chain_id,
            metadata,
        } = request;
        let pool = self.get_pool().await.map_err(internal_error)?;

        // Start transaction for atomicity of INSERT event + UPDATE session
        let mut conn = pool.acquire().await.map_err(internal_error)?;
        let mut tx = conn.begin().await.map_err(internal_error)?;

        let session_row = query("SELECT user_id FROM agent_sessions WHERE session_id = ?")
            .bind(&session_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal_error)?;
        let session_row = session_row.ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Session {} not found", session_id),
            )
        })?;
        let session_owner: String = session_row.try_get("user_id").map_err(internal_error)?;
        if session_owner != user_id {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("Session {} not found", session_id),
            ));
        }

        let event_id = Uuid::new_v4().to_string();
        let causal_chain_id = causal_chain_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let primary_parent_event_id = parent_event_id.clone().or_else(|| {
            parent_event_ids
                .as_ref()
                .and_then(|ids| ids.first().cloned())
        });
        let normalized_parent_event_ids = crate::storage::normalized_parent_event_ids(
            primary_parent_event_id.as_deref(),
            parent_event_ids.as_deref(),
        );
        let metadata_str = metadata
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "{}".to_string());
        let agent_id = agent_id.unwrap_or_else(|| "system".to_string());
        let agent_version = agent_version.unwrap_or_else(|| "1.0.0".to_string());

        let meta_tool_name = metadata_tool_name(metadata.as_ref());
        let meta_duration_ms = metadata
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
        .bind(&session_id)
        .bind(&user_id)
        .bind(&agent_id)
        .bind(&agent_version)
        .bind(&event_type)
        .bind(&content)
        .bind(&primary_parent_event_id)
        .bind(&causal_chain_id)
        .bind(&metadata_str)
        .bind(&meta_tool_name)
        .bind(meta_duration_ms)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;

        crate::storage::insert_agent_event_edges(
            &mut *tx,
            &event_id,
            primary_parent_event_id.as_deref(),
            &normalized_parent_event_ids,
        )
        .await
        .map_err(internal_error)?;

        // BUG FIX (Session 7875e355 diagnostic): Use COUNT(*) reconcile instead of
        // increment to prevent drift from concurrent requests or duplicate detection.
        // This matches the fix in event_ingestion.rs flush_batch().
        query(
            "UPDATE agent_sessions SET \
             event_count = (SELECT COUNT(*) FROM agent_events WHERE session_id = ?), \
             updated_at = NOW() \
             WHERE session_id = ?",
        )
        .bind(&session_id)
        .bind(&session_id)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;

        let select_sql = format!(
            "SELECT {} FROM agent_events WHERE event_id = ?",
            EVENT_DETAIL_SELECT_COLS
        );
        let row = query(&select_sql)
            .bind(&event_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(internal_error)?;

        let mut result = Self::event_record_from_row(row)?;
        result.parent_event_ids = normalized_parent_event_ids;

        tx.commit().await.map_err(internal_error)?;

        Ok(result)
    }

    async fn list_events(
        &self,
        mut filter: EventListFilter,
    ) -> Result<EventListRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        (filter.limit, filter.offset) = clamp_api_list_pagination(filter.limit, filter.offset);

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
            EVENT_LIST_SELECT_COLS
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
        Self::hydrate_parent_event_ids(&pool, &mut events).await?;

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
            EVENT_DETAIL_SELECT_COLS
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
        let mut records = vec![Self::event_record_from_row(row)?];
        Self::hydrate_parent_event_ids(&pool, &mut records).await?;
        let record = records.pop().expect("single event record");
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
            "SELECT {} FROM agent_events WHERE causal_chain_id = ? AND user_id = ? ORDER BY created_at ASC LIMIT ?",
            EVENT_DETAIL_SELECT_COLS
        );
        let rows = query(&select_sql)
            .bind(&causal_chain_id)
            .bind(&user_id)
            .bind(MAX_CAUSAL_CHAIN_EVENTS)
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            events.push(Self::event_record_from_row(row)?);
        }
        Self::hydrate_parent_event_ids(&pool, &mut events).await?;
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
        let (limit, offset) = clamp_api_list_pagination(limit, offset);

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
            EVENT_LIST_SELECT_COLS
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
        Self::hydrate_parent_event_ids(&pool, &mut events).await?;
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
            EVENT_DETAIL_SELECT_COLS
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

        let mut tx = pool.begin().await.map_err(internal_error)?;
        query("DELETE FROM agent_event_edges WHERE child_event_id = ? OR parent_event_id = ?")
            .bind(&event_id)
            .bind(&event_id)
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;
        query("DELETE FROM agent_events WHERE event_id = ?")
            .bind(&event_id)
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;
        tx.commit().await.map_err(internal_error)?;

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
    #[serde(default)]
    pub parent_event_ids: Option<Vec<String>>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_event_ids: Vec<String>,
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
            parent_event_ids: r.parent_event_ids,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pagination::{MAX_API_LIST_LIMIT, MAX_API_LIST_OFFSET, clamp_api_list_pagination};

    // --- metadata_tool_name ---

    #[test]
    fn metadata_none() {
        assert!(metadata_tool_name(None).is_none());
    }

    #[test]
    fn metadata_from_tool_name() {
        let v = serde_json::json!({"tool_name": "bash"});
        assert_eq!(metadata_tool_name(Some(&v)).unwrap(), "bash");
    }

    #[test]
    fn metadata_from_name_fallback() {
        let v = serde_json::json!({"name": "read_file"});
        assert_eq!(metadata_tool_name(Some(&v)).unwrap(), "read_file");
    }

    #[test]
    fn metadata_prefers_tool_name() {
        let v = serde_json::json!({"tool_name": "preferred", "name": "fallback"});
        assert_eq!(metadata_tool_name(Some(&v)).unwrap(), "preferred");
    }

    #[test]
    fn metadata_trims_quotes() {
        let v = serde_json::json!({"tool_name": "\"bash\""});
        assert_eq!(metadata_tool_name(Some(&v)).unwrap(), "bash");
    }

    #[test]
    fn metadata_empty_after_trim() {
        let v = serde_json::json!({"tool_name": "\"\""});
        assert!(metadata_tool_name(Some(&v)).is_none());
    }

    #[test]
    fn metadata_missing_fields() {
        let v = serde_json::json!({"other": "field"});
        assert!(metadata_tool_name(Some(&v)).is_none());
    }

    #[test]
    fn metadata_non_string() {
        let v = serde_json::json!({"tool_name": 42});
        assert!(metadata_tool_name(Some(&v)).is_none());
    }

    // --- defaults ---

    #[test]
    fn default_limits() {
        assert_eq!(default_event_limit(), 50);
        assert_eq!(default_session_event_limit(), 100);
    }

    // --- EventRecord → EventResponse conversion ---

    #[test]
    fn event_record_to_response() {
        let record = EventRecord {
            event_id: "e1".to_string(),
            user_id: "u1".to_string(),
            session_id: "s1".to_string(),
            event_type: "tool_call".to_string(),
            content: "{}".to_string(),
            agent_id: Some("a1".to_string()),
            agent_version: None,
            parent_event_id: None,
            parent_event_ids: vec!["p1".to_string(), "p2".to_string()],
            causal_chain_id: "cc1".to_string(),
            metadata: serde_json::json!({"tool_name": "bash"}),
            created_at: "2025-01-01".to_string(),
        };
        let resp = EventResponse::from(record);
        assert_eq!(resp.event_id, "e1");
        assert_eq!(resp.agent_id.as_deref(), Some("a1"));
        assert!(resp.agent_version.is_none());
        assert_eq!(
            resp.parent_event_ids,
            vec!["p1".to_string(), "p2".to_string()]
        );
    }

    #[test]
    fn event_list_record_to_response() {
        let record = EventListRecord {
            events: vec![EventRecord {
                event_id: "e1".to_string(),
                user_id: "u1".to_string(),
                session_id: "s1".to_string(),
                event_type: "t".to_string(),
                content: "c".to_string(),
                agent_id: None,
                agent_version: None,
                parent_event_id: None,
                parent_event_ids: Vec::new(),
                causal_chain_id: "cc".to_string(),
                metadata: serde_json::json!(null),
                created_at: "now".to_string(),
            }],
            total: 42,
            limit: 10,
            offset: 0,
        };
        let resp = EventListResponse::from(record);
        assert_eq!(resp.events.len(), 1);
        assert_eq!(resp.total, 42);
    }

    // --- query deserialization defaults ---

    #[test]
    fn event_list_query_defaults() {
        let q: EventListQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.limit, 50);
        assert_eq!(q.offset, 0);
    }

    #[test]
    fn session_event_query_defaults() {
        let q: SessionEventQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.limit, 100);
        assert_eq!(q.offset, 0);
    }

    #[test]
    fn list_events_paging_contract_matches_shared_clamp() {
        let (limit, offset) = clamp_api_list_pagination(u32::MAX, u32::MAX);
        assert_eq!(limit, MAX_API_LIST_LIMIT);
        assert_eq!(offset, MAX_API_LIST_OFFSET);
    }

    #[test]
    fn event_create_request_accepts_parent_event_ids() {
        let request: EventCreateRequest = serde_json::from_str(
            r#"{
                "session_id":"s1",
                "event_type":"tool_call",
                "content":"x",
                "parent_event_id":"p0",
                "parent_event_ids":["p0","p1"]
            }"#,
        )
        .unwrap();
        assert_eq!(request.parent_event_id.as_deref(), Some("p0"));
        assert_eq!(
            request.parent_event_ids.expect("parent_event_ids"),
            vec!["p0".to_string(), "p1".to_string()]
        );
    }
}
