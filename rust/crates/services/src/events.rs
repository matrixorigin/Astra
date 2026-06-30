use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Acquire, MySql, QueryBuilder, query};
use uuid::Uuid;

use astra_core::{ErrorResponse, MatrixOneSettings, SharedPool, error_response, internal_error};

use crate::db_row::RowExt as EventDbRow;
use crate::pagination::MAX_API_LIST_LIMIT;
use crate::storage::{agent_session_exists_for_user, bump_agent_session_event_count};
use astra_core::canonical_names::metadata_tool_name;

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
    pub cursor: Option<EventListCursor>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventListRecord {
    pub events: Vec<EventRecord>,
    pub total: i64,
    pub limit: u32,
    pub next_cursor: Option<EventListCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventListCursor {
    pub created_at: String,
    pub event_id: String,
}

fn validate_event_list_limit(limit: u32) -> u32 {
    limit.clamp(1, MAX_API_LIST_LIMIT)
}

fn event_list_query_limit(limit: u32) -> i64 {
    i64::from(limit) + 1
}

fn event_list_cursor_db_created_at(
    cursor: &EventListCursor,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let created_at = cursor.created_at.trim();
    if created_at.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid event list cursor: created_at is required",
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
            format!("invalid event list cursor timestamp: {created_at}"),
        ));
    }
    Ok(db_created_at)
}

fn event_list_cursor_event_id(
    cursor: &EventListCursor,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let event_id = cursor.event_id.trim();
    if event_id.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid event list cursor: event_id is required",
        ));
    }
    Ok(event_id.to_string())
}

fn event_list_cursor_from_record(
    event: &EventRecord,
) -> Result<EventListCursor, (StatusCode, Json<ErrorResponse>)> {
    if event.created_at.trim().is_empty() {
        return Err(internal_error(format!(
            "invalid agent_events cursor: event_id={}, column=created_at, value is empty",
            event.event_id
        )));
    }
    if event.event_id.trim().is_empty() {
        return Err(internal_error(
            "invalid agent_events cursor: column=event_id, value is empty",
        ));
    }
    Ok(EventListCursor {
        created_at: event.created_at.clone(),
        event_id: event.event_id.clone(),
    })
}

fn event_row_string(
    row: &impl EventDbRow,
    column: &str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    row.string_column(column)
        .map_err(|e| internal_error(format!("agent_events decode column `{column}`: {e}")))
}

fn event_row_optional_string(
    row: &impl EventDbRow,
    column: &str,
) -> Result<Option<String>, (StatusCode, Json<ErrorResponse>)> {
    row.optional_string_column(column)
        .map_err(|e| internal_error(format!("agent_events decode column `{column}`: {e}")))
}

fn parse_event_metadata_json(
    metadata_json: &str,
) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
    serde_json::from_str(metadata_json)
        .map_err(|e| internal_error(format!("agent_events metadata JSON decode failed: {e}")))
}

fn event_count_total(row: &impl EventDbRow) -> Result<i64, (StatusCode, Json<ErrorResponse>)> {
    row.i64_column("total")
        .map_err(|e| internal_error(format!("agent_events decode column `total`: {e}")))
}

fn agent_session_event_count(
    row: &impl EventDbRow,
) -> Result<i64, (StatusCode, Json<ErrorResponse>)> {
    row.i64_column("event_count")
        .map_err(|e| internal_error(format!("agent_sessions decode column `event_count`: {e}")))
}

fn event_list_can_use_session_summary(filter: &EventListFilter) -> bool {
    filter.session_id.is_some()
        && filter.event_type.is_none()
        && filter.agent_id.is_none()
        && filter.causal_chain_id.is_none()
}

fn push_event_list_filters<'a>(qb: &mut QueryBuilder<'a, MySql>, filter: &'a EventListFilter) {
    qb.push_bind(&filter.user_id);
    if let Some(sid) = &filter.session_id {
        qb.push(" AND session_id = ");
        qb.push_bind(sid);
    }
    if let Some(et) = &filter.event_type {
        qb.push(" AND event_type = ");
        qb.push_bind(et);
    }
    if let Some(aid) = &filter.agent_id {
        qb.push(" AND agent_id = ");
        qb.push_bind(aid);
    }
    if let Some(ccid) = &filter.causal_chain_id {
        qb.push(" AND causal_chain_id = ");
        qb.push_bind(ccid);
    }
}

async fn count_events_from_agent_events(
    pool: &sqlx::Pool<MySql>,
    filter: &EventListFilter,
) -> Result<i64, (StatusCode, Json<ErrorResponse>)> {
    let mut count_qb = QueryBuilder::<MySql>::new(
        "SELECT COUNT(event_id) AS total FROM agent_events WHERE user_id = ",
    );
    push_event_list_filters(&mut count_qb, filter);
    let total_row = count_qb
        .build()
        .fetch_one(pool)
        .await
        .map_err(internal_error)?;
    event_count_total(&total_row)
}

async fn list_events_total(
    pool: &sqlx::Pool<MySql>,
    filter: &EventListFilter,
) -> Result<i64, (StatusCode, Json<ErrorResponse>)> {
    if !event_list_can_use_session_summary(filter) {
        return count_events_from_agent_events(pool, filter).await;
    }

    let session_id = filter.session_id.as_deref().expect("checked above");
    let session_row = query(
        "SELECT event_count FROM agent_sessions WHERE session_id = ? AND user_id = ? LIMIT 1",
    )
    .bind(session_id)
    .bind(&filter.user_id)
    .fetch_optional(pool)
    .await
    .map_err(internal_error)?;
    match session_row {
        Some(row) => agent_session_event_count(&row),
        None => count_events_from_agent_events(pool, filter).await,
    }
}

fn decode_event_record_from_row(
    row: &impl EventDbRow,
) -> Result<EventRecord, (StatusCode, Json<ErrorResponse>)> {
    let metadata_json = event_row_string(row, "metadata_json")?;

    Ok(EventRecord {
        event_id: event_row_string(row, "event_id")?,
        user_id: event_row_string(row, "user_id")?,
        session_id: event_row_string(row, "session_id")?,
        event_type: event_row_string(row, "event_type")?,
        content: event_row_string(row, "content")?,
        agent_id: event_row_optional_string(row, "agent_id")?,
        agent_version: event_row_optional_string(row, "agent_version")?,
        parent_event_id: event_row_optional_string(row, "parent_event_id")?,
        parent_event_ids: Vec::new(),
        causal_chain_id: event_row_string(row, "causal_chain_id")?,
        metadata: parse_event_metadata_json(&metadata_json)?,
        created_at: event_row_string(row, "created_at")?,
    })
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
        cursor: Option<EventListCursor>,
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
        decode_event_record_from_row(&row)
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
        let mut parent_id_map = std::collections::HashMap::new();
        let Some(user_id) = records.first().map(|record| record.user_id.as_str()) else {
            return Ok(());
        };
        if records.iter().any(|record| record.user_id != user_id) {
            return Err(internal_error(
                "cannot hydrate parent event ids for mixed-owner event records",
            ));
        }
        crate::storage::load_agent_event_parent_ids(executor, user_id, &event_ids)
            .await
            .map(|loaded| parent_id_map.extend(loaded))
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
        crate::require_shared_pool(self.pool.as_ref(), "DatabaseEventService", &self.matrixone)
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
    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%f') AS created_at";

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

        if !agent_session_exists_for_user(&mut *tx, &session_id, &user_id)
            .await
            .map_err(internal_error)?
        {
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

        let insert_result = query(
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
            &user_id,
            &session_id,
            &event_id,
            primary_parent_event_id.as_deref(),
            &normalized_parent_event_ids,
        )
        .await
        .map_err(internal_error)?;

        let event_count_delta =
            crate::storage::rows_affected_to_i64(insert_result.rows_affected(), "create_event")
                .map_err(internal_error)?;
        bump_agent_session_event_count(
            &mut *tx,
            &session_id,
            &user_id,
            event_count_delta,
            Some(&event_id),
        )
        .await
        .map_err(internal_error)?;

        let select_sql = format!(
            "SELECT {} FROM agent_events WHERE event_id = ? AND user_id = ?",
            EVENT_DETAIL_SELECT_COLS
        );
        let row = query(&select_sql)
            .bind(&event_id)
            .bind(&user_id)
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
        filter: EventListFilter,
    ) -> Result<EventListRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let limit = validate_event_list_limit(filter.limit);

        let total = list_events_total(&pool, &filter).await?;

        let mut list_qb = QueryBuilder::<MySql>::new(format!(
            "SELECT {} FROM agent_events WHERE user_id = ",
            EVENT_LIST_SELECT_COLS
        ));
        push_event_list_filters(&mut list_qb, &filter);
        if let Some(cursor) = &filter.cursor {
            let created_at = event_list_cursor_db_created_at(cursor)?;
            let event_id = event_list_cursor_event_id(cursor)?;
            list_qb.push(" AND (created_at < ");
            list_qb.push_bind(created_at.clone());
            list_qb.push(" OR (created_at = ");
            list_qb.push_bind(created_at);
            list_qb.push(" AND event_id < ");
            list_qb.push_bind(event_id);
            list_qb.push("))");
        }
        list_qb.push(" ORDER BY created_at DESC, event_id DESC LIMIT ");
        list_qb.push_bind(event_list_query_limit(limit));

        let rows = list_qb
            .build()
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            events.push(Self::event_record_from_row(row)?);
        }
        let has_more = events.len() > limit as usize;
        if has_more {
            events.truncate(limit as usize);
        }
        Self::hydrate_parent_event_ids(&pool, &mut events).await?;
        let next_cursor = if has_more {
            events
                .last()
                .map(event_list_cursor_from_record)
                .transpose()?
        } else {
            None
        };

        Ok(EventListRecord {
            events,
            total,
            limit,
            next_cursor,
        })
    }

    async fn get_event(
        &self,
        event_id: String,
        user_id: String,
    ) -> Result<EventRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let select_sql = format!(
            "SELECT {} FROM agent_events WHERE event_id = ? AND user_id = ?",
            EVENT_DETAIL_SELECT_COLS
        );
        let row = query(&select_sql)
            .bind(&event_id)
            .bind(&user_id)
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
        Ok(records.pop().expect("single event record"))
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
        cursor: Option<EventListCursor>,
    ) -> Result<EventListRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let limit = validate_event_list_limit(limit);

        let session_row = query(
            "SELECT event_count FROM agent_sessions WHERE session_id = ? AND user_id = ? LIMIT 1",
        )
        .bind(&session_id)
        .bind(&user_id)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;
        let Some(session_row) = session_row else {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("Session {} not found", session_id),
            ));
        };
        let total = agent_session_event_count(&session_row)?;

        let mut list_qb = QueryBuilder::<MySql>::new(format!(
            "SELECT {} FROM agent_events WHERE session_id = ",
            EVENT_LIST_SELECT_COLS
        ));
        list_qb.push_bind(&session_id);
        list_qb.push(" AND user_id = ");
        list_qb.push_bind(&user_id);
        if let Some(cursor) = &cursor {
            let created_at = event_list_cursor_db_created_at(cursor)?;
            let event_id = event_list_cursor_event_id(cursor)?;
            list_qb.push(" AND (created_at > ");
            list_qb.push_bind(created_at.clone());
            list_qb.push(" OR (created_at = ");
            list_qb.push_bind(created_at);
            list_qb.push(" AND event_id > ");
            list_qb.push_bind(event_id);
            list_qb.push("))");
        }
        list_qb.push(" ORDER BY created_at ASC, event_id ASC LIMIT ");
        list_qb.push_bind(event_list_query_limit(limit));

        let rows = list_qb
            .build()
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            events.push(Self::event_record_from_row(row)?);
        }
        let has_more = events.len() > limit as usize;
        if has_more {
            events.truncate(limit as usize);
        }
        Self::hydrate_parent_event_ids(&pool, &mut events).await?;
        let next_cursor = if has_more {
            events
                .last()
                .map(event_list_cursor_from_record)
                .transpose()?
        } else {
            None
        };
        Ok(EventListRecord {
            events,
            total,
            limit,
            next_cursor,
        })
    }

    async fn delete_event(
        &self,
        event_id: String,
        user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let select_sql = format!(
            "SELECT {} FROM agent_events WHERE event_id = ? AND user_id = ?",
            EVENT_DETAIL_SELECT_COLS
        );
        let row = query(&select_sql)
            .bind(&event_id)
            .bind(&user_id)
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

        let mut tx = pool.begin().await.map_err(internal_error)?;
        query(
            "DELETE FROM agent_event_edges
             WHERE user_id = ? AND (child_event_id = ? OR parent_event_id = ?)",
        )
        .bind(&user_id)
        .bind(&event_id)
        .bind(&event_id)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;
        let delete_result = query("DELETE FROM agent_events WHERE event_id = ? AND user_id = ?")
            .bind(&event_id)
            .bind(&user_id)
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;
        if delete_result.rows_affected() > 0 {
            bump_agent_session_event_count(&mut *tx, &record.session_id, &user_id, -1, None)
                .await
                .map_err(internal_error)?;
        }
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
        _: Option<EventListCursor>,
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

#[derive(Deserialize)]
pub struct EventListQuery {
    pub session_id: Option<String>,
    pub event_type: Option<String>,
    pub agent_id: Option<String>,
    pub causal_chain_id: Option<String>,
    #[serde(default = "default_event_limit")]
    pub limit: u32,
    pub after_created_at: Option<String>,
    pub after_event_id: Option<String>,
}

impl Default for EventListQuery {
    fn default() -> Self {
        Self {
            session_id: None,
            event_type: None,
            agent_id: None,
            causal_chain_id: None,
            limit: default_event_limit(),
            after_created_at: None,
            after_event_id: None,
        }
    }
}

pub fn default_event_limit() -> u32 {
    50
}

#[derive(Deserialize)]
pub struct SessionEventQuery {
    #[serde(default = "default_session_event_limit")]
    pub limit: u32,
    pub after_created_at: Option<String>,
    pub after_event_id: Option<String>,
}

impl Default for SessionEventQuery {
    fn default() -> Self {
        Self {
            limit: default_session_event_limit(),
            after_created_at: None,
            after_event_id: None,
        }
    }
}

pub fn default_session_event_limit() -> u32 {
    100
}

fn event_cursor_from_query_parts(
    after_created_at: &Option<String>,
    after_event_id: &Option<String>,
) -> Result<Option<EventListCursor>, (StatusCode, Json<ErrorResponse>)> {
    match (after_created_at, after_event_id) {
        (None, None) => Ok(None),
        (Some(created_at), Some(event_id)) => Ok(Some(EventListCursor {
            created_at: created_at.clone(),
            event_id: event_id.clone(),
        })),
        _ => Err(error_response(
            StatusCode::BAD_REQUEST,
            "event list cursor requires both after_created_at and after_event_id",
        )),
    }
}

impl EventListQuery {
    pub fn cursor(&self) -> Result<Option<EventListCursor>, (StatusCode, Json<ErrorResponse>)> {
        event_cursor_from_query_parts(&self.after_created_at, &self.after_event_id)
    }
}

impl SessionEventQuery {
    pub fn cursor(&self) -> Result<Option<EventListCursor>, (StatusCode, Json<ErrorResponse>)> {
        event_cursor_from_query_parts(&self.after_created_at, &self.after_event_id)
    }
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
    pub next_cursor: Option<EventListCursor>,
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
            next_cursor: r.next_cursor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- metadata_tool_name ---

    #[test]
    fn metadata_none() {
        assert!(metadata_tool_name(None).is_none());
    }

    #[test]
    fn metadata_from_tool_name() {
        let v = serde_json::json!({"tool_name": " bash "});
        assert_eq!(metadata_tool_name(Some(&v)).unwrap(), "bash");
    }

    #[test]
    fn metadata_name_is_not_a_tool_name_alias() {
        let v = serde_json::json!({"name": "read_file"});
        assert!(metadata_tool_name(Some(&v)).is_none());
    }

    #[test]
    fn metadata_ignores_ambiguous_name_when_tool_name_exists() {
        let v = serde_json::json!({"tool_name": "preferred", "name": "read_file"});
        assert_eq!(metadata_tool_name(Some(&v)).unwrap(), "preferred");
    }

    #[test]
    fn metadata_trims_quotes() {
        let v = serde_json::json!({"tool_name": " \"bash\" "});
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

    #[test]
    fn event_metadata_json_decode_fails_loudly() {
        assert!(parse_event_metadata_json(r#"{"ok":true}"#).is_ok());
        assert!(parse_event_metadata_json("{not-json").is_err());
    }

    fn list_filter_for_summary_fast_path() -> EventListFilter {
        EventListFilter {
            user_id: "user-1".to_string(),
            session_id: Some("session-1".to_string()),
            event_type: None,
            agent_id: None,
            causal_chain_id: None,
            limit: 50,
            cursor: None,
        }
    }

    #[test]
    fn event_list_summary_fast_path_requires_unfiltered_session_scope() {
        assert!(event_list_can_use_session_summary(
            &list_filter_for_summary_fast_path()
        ));

        let mut without_session = list_filter_for_summary_fast_path();
        without_session.session_id = None;
        assert!(!event_list_can_use_session_summary(&without_session));

        let mut with_event_type = list_filter_for_summary_fast_path();
        with_event_type.event_type = Some("tool_call".to_string());
        assert!(!event_list_can_use_session_summary(&with_event_type));

        let mut with_agent = list_filter_for_summary_fast_path();
        with_agent.agent_id = Some("agent-1".to_string());
        assert!(!event_list_can_use_session_summary(&with_agent));

        let mut with_causal_chain = list_filter_for_summary_fast_path();
        with_causal_chain.causal_chain_id = Some("chain-1".to_string());
        assert!(!event_list_can_use_session_summary(&with_causal_chain));
    }

    struct FakeEventDbRow {
        failed_column: Option<&'static str>,
        metadata_json: &'static str,
    }

    impl FakeEventDbRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                metadata_json: r#"{"tool_name":"bash"}"#,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_metadata_json(metadata_json: &'static str) -> Self {
            Self {
                metadata_json,
                ..Self::complete()
            }
        }

        fn fail_if_needed(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl EventDbRow for FakeEventDbRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "event_id" => "event-1",
                "user_id" => "user-1",
                "session_id" => "session-1",
                "event_type" => "tool_call",
                "content" => r#"{"cmd":"pwd"}"#,
                "causal_chain_id" => "chain-1",
                "metadata_json" => self.metadata_json,
                "created_at" => "2026-06-26T12:00:00",
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "agent_id" => Some("agent-1".to_string()),
                "agent_version" => Some("1.2.3".to_string()),
                "parent_event_id" => None,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.fail_if_needed(column)?;
            match column {
                "total" => Ok(9),
                "event_count" => Ok(7),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }
    }

    fn assert_internal_decode_error(
        result: Result<impl std::fmt::Debug, (StatusCode, Json<ErrorResponse>)>,
        column: &str,
    ) {
        let (status, Json(body)) = result.unwrap_err();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            body.detail.contains(&format!("decode column `{column}`")),
            "error should identify failed column: {:?}",
            body.detail
        );
    }

    #[test]
    fn event_record_row_decode_preserves_database_values() {
        let record = decode_event_record_from_row(&FakeEventDbRow::complete()).unwrap();

        assert_eq!(record.event_id, "event-1");
        assert_eq!(record.user_id, "user-1");
        assert_eq!(record.session_id, "session-1");
        assert_eq!(record.event_type, "tool_call");
        assert_eq!(record.content, r#"{"cmd":"pwd"}"#);
        assert_eq!(record.agent_id.as_deref(), Some("agent-1"));
        assert_eq!(record.agent_version.as_deref(), Some("1.2.3"));
        assert_eq!(record.parent_event_id, None);
        assert_eq!(record.parent_event_ids, Vec::<String>::new());
        assert_eq!(record.causal_chain_id, "chain-1");
        assert_eq!(record.metadata, serde_json::json!({"tool_name":"bash"}));
        assert_eq!(record.created_at, "2026-06-26T12:00:00");
    }

    #[test]
    fn event_record_row_decode_fails_loudly_on_missing_columns() {
        for column in [
            "event_id",
            "user_id",
            "session_id",
            "event_type",
            "content",
            "agent_id",
            "agent_version",
            "parent_event_id",
            "causal_chain_id",
            "metadata_json",
            "created_at",
        ] {
            assert_internal_decode_error(
                decode_event_record_from_row(&FakeEventDbRow::fail_on(column)),
                column,
            );
        }
    }

    #[test]
    fn event_record_row_decode_fails_loudly_on_invalid_metadata_json() {
        let (status, Json(body)) =
            decode_event_record_from_row(&FakeEventDbRow::with_metadata_json("{not-json"))
                .unwrap_err();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            body.detail.contains("metadata JSON decode failed"),
            "error should identify metadata JSON decode failure: {:?}",
            body.detail
        );
    }

    #[test]
    fn event_count_total_preserves_value_and_fails_loudly() {
        assert_eq!(event_count_total(&FakeEventDbRow::complete()).unwrap(), 9);

        let (status, Json(body)) =
            event_count_total(&FakeEventDbRow::fail_on("total")).unwrap_err();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            body.detail.contains("decode column `total`"),
            "error should identify total decode failure: {:?}",
            body.detail
        );
    }

    #[test]
    fn agent_session_event_count_preserves_summary_and_fails_loudly() {
        assert_eq!(
            agent_session_event_count(&FakeEventDbRow::complete()).unwrap(),
            7
        );

        let (status, Json(body)) =
            agent_session_event_count(&FakeEventDbRow::fail_on("event_count")).unwrap_err();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            body.detail
                .contains("agent_sessions decode column `event_count`"),
            "error should identify agent_sessions.event_count decode failure: {:?}",
            body.detail
        );
    }

    #[test]
    fn event_row_optional_string_does_not_swallow_decode_errors() {
        for column in ["agent_id", "agent_version", "parent_event_id"] {
            assert_internal_decode_error(
                event_row_optional_string(&FakeEventDbRow::fail_on(column), column),
                column,
            );
        }
    }

    #[test]
    fn event_row_string_does_not_default_missing_required_values() {
        for column in ["created_at", "metadata_json"] {
            assert!(
                event_row_string(&FakeEventDbRow::fail_on(column), column).is_err(),
                "required event column should not default: {column}"
            );
        }
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
            next_cursor: Some(EventListCursor {
                created_at: "2026-04-01T10:00:00.123456".to_string(),
                event_id: "e1".to_string(),
            }),
        };
        let resp = EventListResponse::from(record);
        assert_eq!(resp.events.len(), 1);
        assert_eq!(resp.total, 42);
        assert_eq!(
            resp.next_cursor
                .as_ref()
                .map(|cursor| cursor.event_id.as_str()),
            Some("e1")
        );
    }

    // --- query deserialization defaults ---

    #[test]
    fn event_list_query_defaults() {
        let q: EventListQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.limit, 50);
        assert_eq!(q.cursor().unwrap(), None);
    }

    #[test]
    fn session_event_query_defaults() {
        let q: SessionEventQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.limit, 100);
        assert_eq!(q.cursor().unwrap(), None);
    }

    #[test]
    fn event_list_query_requires_complete_cursor() {
        let q: EventListQuery =
            serde_json::from_str(r#"{"after_created_at":"2026-04-01T10:00:00.000000"}"#).unwrap();
        assert_eq!(q.cursor().unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn event_list_limit_has_hard_cap_and_minimum() {
        assert_eq!(validate_event_list_limit(0), 1);
        assert_eq!(validate_event_list_limit(10), 10);
        assert_eq!(validate_event_list_limit(u32::MAX), MAX_API_LIST_LIMIT);
        assert_eq!(event_list_query_limit(MAX_API_LIST_LIMIT), 201);
    }

    #[test]
    fn event_list_cursor_rejects_invalid_inputs() {
        let cursor = EventListCursor {
            created_at: "2026-04-01T10:00:00.123456".to_string(),
            event_id: "event-1".to_string(),
        };
        assert_eq!(
            event_list_cursor_db_created_at(&cursor).unwrap(),
            "2026-04-01 10:00:00.123456"
        );
        assert_eq!(
            event_list_cursor_event_id(&cursor).unwrap(),
            "event-1".to_string()
        );

        let invalid_time = EventListCursor {
            created_at: "2026-04-01T10:00:00".to_string(),
            event_id: "event-1".to_string(),
        };
        assert_eq!(
            event_list_cursor_db_created_at(&invalid_time)
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );

        let missing_event_id = EventListCursor {
            created_at: "2026-04-01T10:00:00.123456".to_string(),
            event_id: "  ".to_string(),
        };
        assert_eq!(
            event_list_cursor_event_id(&missing_event_id).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn event_list_sql_contract_uses_seek_cursor_not_offset() {
        let desc_sql = format!(
            "SELECT {EVENT_LIST_SELECT_COLS} FROM agent_events WHERE user_id = ? \
             AND (created_at < ? OR (created_at = ? AND event_id < ?)) \
             ORDER BY created_at DESC, event_id DESC LIMIT ?"
        );
        let asc_sql = format!(
            "SELECT {EVENT_LIST_SELECT_COLS} FROM agent_events WHERE session_id = ? AND user_id = ? \
             AND (created_at > ? OR (created_at = ? AND event_id > ?)) \
             ORDER BY created_at ASC, event_id ASC LIMIT ?"
        );
        assert!(!desc_sql.to_ascii_uppercase().contains(" OFFSET "));
        assert!(!asc_sql.to_ascii_uppercase().contains(" OFFSET "));
        assert!(desc_sql.contains("event_id < ?"));
        assert!(asc_sql.contains("event_id > ?"));
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

    /// P1-C: delete_event must decrement session event_count by the actual deleted row.
    /// P2-A: get_event and delete_event must return 404 (not 403) for
    /// non-owner access to prevent IDOR information leakage.
    /// P2-E: EventListQuery::default() must use the same limit as serde deserialization.
    #[test]
    fn event_list_query_default_matches_serde() {
        let q = EventListQuery::default();
        assert_eq!(
            q.limit, 50,
            "EventListQuery::default().limit must be 50 (matching serde default)"
        );
    }

    /// P2-E: SessionEventQuery::default() must use the same limit as serde deserialization.
    #[test]
    fn session_event_query_default_matches_serde() {
        let q = SessionEventQuery::default();
        assert_eq!(
            q.limit, 100,
            "SessionEventQuery::default().limit must be 100 (matching serde default)"
        );
    }
}
