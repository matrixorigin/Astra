use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Acquire, MySql, QueryBuilder, Row, query};
use uuid::Uuid;

use astra_core::{
    ERROR_CODE_SESSION_DELETED, ERROR_CODE_SESSION_DELETING, ERROR_CODE_SESSION_FOREIGN_OWNER,
    ERROR_CODE_SESSION_WRITE_CONFLICT, ErrorResponse, MatrixOneSettings, SharedPool,
    error_response, error_response_coded, internal_error, is_duplicate_key_error,
};

use crate::db_row::RowExt as EventDbRow;
use crate::pagination::MAX_API_LIST_LIMIT;
use crate::storage::{
    SessionWriteAdmission, add_agent_session_event_count_or_create, bump_agent_session_event_count,
    classify_session_write_admission,
};
use crate::sync_outbox::{sync_outbox_canonical_payload_hash, sync_outbox_stable_event_id};
use astra_core::canonical_names::{
    metadata_duration_ms, metadata_tool_call_id, metadata_tool_name,
};

const MAX_CAUSAL_CHAIN_EVENTS: i64 = 500;

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventIngestionSource {
    Client,
    SyncOutbox,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventCreateRequestData {
    pub ingestion_source: EventIngestionSource,
    pub event_id: Option<String>,
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
    pub causal_chain_id: Option<String>,
    pub metadata: serde_json::Value,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventCreateOutcome {
    pub record: EventRecord,
    pub idempotent_replay: bool,
}

impl EventCreateOutcome {
    pub fn created(record: EventRecord) -> Self {
        Self {
            record,
            idempotent_replay: false,
        }
    }

    pub fn replayed(record: EventRecord) -> Self {
        Self {
            record,
            idempotent_replay: true,
        }
    }
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
    pub total: Option<i64>,
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

fn normalize_client_event_id(
    event_id: Option<String>,
) -> Result<Option<String>, (StatusCode, Json<ErrorResponse>)> {
    let Some(event_id) = event_id else {
        return Ok(None);
    };
    let event_id = event_id.trim().to_string();
    if event_id.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "event_id must not be empty when provided",
        ));
    }
    if event_id.len() > crate::storage::AGENT_EVENT_ID_LEN {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!(
                "event_id length {} exceeds maximum {}",
                event_id.len(),
                crate::storage::AGENT_EVENT_ID_LEN
            ),
        ));
    }
    Ok(Some(event_id))
}

fn normalize_required_event_field(
    field: &'static str,
    value: String,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("{field} must not be empty"),
        ));
    }
    Ok(value)
}

fn sync_outbox_payload_hash(metadata: &serde_json::Value) -> Option<&str> {
    metadata
        .get("sync_outbox")
        .and_then(|value| value.get("payload_hash"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn sync_outbox_content_payload_hash(
    content: &str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let payload: serde_json::Value = serde_json::from_str(content).map_err(|error| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("sync_outbox event content must be canonical JSON payload: {error}"),
        )
    })?;
    Ok(sync_outbox_canonical_payload_hash(&payload))
}

fn verified_sync_outbox_payload_hash(
    content: &str,
    metadata: Option<&serde_json::Value>,
) -> Result<Option<String>, (StatusCode, Json<ErrorResponse>)> {
    let Some(declared_hash) = metadata.and_then(sync_outbox_payload_hash) else {
        return Ok(None);
    };
    let computed_hash = sync_outbox_content_payload_hash(content)?;
    if computed_hash != declared_hash {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "sync_outbox payload_hash must match the canonical event content",
        ));
    }
    Ok(Some(computed_hash))
}

fn verified_sync_outbox_payload_hash_for_source(
    source: EventIngestionSource,
    content: &str,
    metadata: Option<&serde_json::Value>,
) -> Result<Option<String>, (StatusCode, Json<ErrorResponse>)> {
    match source {
        EventIngestionSource::Client => {
            if metadata.and_then(sync_outbox_payload_hash).is_some() {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "sync_outbox metadata is reserved for /sync/outbox/events",
                ));
            }
            Ok(None)
        }
        EventIngestionSource::SyncOutbox => {
            let verified = verified_sync_outbox_payload_hash(content, metadata)?;
            if verified.is_none() {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "sync_outbox ingestion requires metadata.sync_outbox.payload_hash",
                ));
            }
            Ok(verified)
        }
    }
}

fn verify_sync_outbox_event_identity(
    content: &str,
    payload_hash: &str,
    event_id: &str,
    session_id: &str,
    event_type: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let event: crate::session_journal::JournalEvent =
        serde_json::from_str(content).map_err(|error| {
            error_response(
                StatusCode::BAD_REQUEST,
                format!("sync_outbox event content must be a serialized JournalEvent: {error}"),
            )
        })?;
    let expected_id = sync_outbox_stable_event_id(&event, payload_hash).map_err(internal_error)?;
    if expected_id != event_id {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "sync_outbox event_id must match the stable JournalEvent identity",
        ));
    }
    if event.session_id.as_deref().unwrap_or("") != session_id {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "sync_outbox session_id must match the serialized JournalEvent session_id",
        ));
    }
    let journal_event_type = serde_json::to_value(&event.event_type)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| format!("{:?}", event.event_type));
    if journal_event_type != event_type {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "sync_outbox event_type must match the serialized JournalEvent event_type",
        ));
    }
    Ok(())
}

fn duplicate_event_matches_request(
    existing: &EventRecord,
    session_id: &str,
    event_type: &str,
    content: &str,
    metadata: Option<&serde_json::Value>,
    verified_sync_payload_hash: Option<&str>,
) -> bool {
    if existing.session_id != session_id || existing.event_type != event_type {
        return false;
    }
    if let Some(incoming_hash) = verified_sync_payload_hash {
        return sync_outbox_content_payload_hash(&existing.content)
            .ok()
            .as_deref()
            == Some(incoming_hash);
    }
    existing.session_id == session_id
        && existing.event_type == event_type
        && existing.content == content
        && existing.metadata == metadata.cloned().unwrap_or_else(|| serde_json::json!({}))
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

async fn list_events_total(
    pool: &sqlx::Pool<MySql>,
    filter: &EventListFilter,
) -> Result<Option<i64>, (StatusCode, Json<ErrorResponse>)> {
    if !event_list_can_use_session_summary(filter) {
        return Ok(None);
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
        Some(row) => agent_session_event_count(&row).map(Some),
        None => Ok(None),
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
        causal_chain_id: event_row_optional_string(row, "causal_chain_id")?,
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
    ) -> Result<EventCreateOutcome, (StatusCode, Json<ErrorResponse>)>;

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
    ) -> Result<EventCreateOutcome, (StatusCode, Json<ErrorResponse>)> {
        let EventCreateRequestData {
            ingestion_source,
            event_id,
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
        let session_id = normalize_required_event_field("session_id", session_id)?;
        let event_type = normalize_required_event_field("event_type", event_type)?;

        // Start transaction for atomicity of INSERT event + UPDATE session
        let mut conn = pool.acquire().await.map_err(internal_error)?;
        let mut tx = conn.begin().await.map_err(internal_error)?;

        let client_event_id = normalize_client_event_id(event_id)?;
        let sync_outbox_ingestion = ingestion_source == EventIngestionSource::SyncOutbox;
        let verified_sync_payload_hash = verified_sync_outbox_payload_hash_for_source(
            ingestion_source,
            &content,
            metadata.as_ref(),
        )?;
        if sync_outbox_ingestion {
            let Some(event_id) = client_event_id.as_deref() else {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "sync_outbox ingestion requires a stable event_id",
                ));
            };
            let Some(payload_hash) = verified_sync_payload_hash.as_deref() else {
                return Err(internal_error(
                    "sync_outbox ingestion reached identity verification without payload hash",
                ));
            };
            verify_sync_outbox_event_identity(
                &content,
                payload_hash,
                event_id,
                &session_id,
                &event_type,
            )?;
        }
        ensure_event_session_requirement(&mut tx, &session_id, &user_id, sync_outbox_ingestion)
            .await?;
        if let Some(existing_id) = client_event_id.as_deref() {
            let select_sql = format!(
                "SELECT {} FROM agent_events WHERE event_id = ? AND user_id = ?",
                EVENT_DETAIL_SELECT_COLS
            );
            if let Some(row) = query(&select_sql)
                .bind(existing_id)
                .bind(&user_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(internal_error)?
            {
                let existing = Self::event_record_from_row(row)?;
                if duplicate_event_matches_request(
                    &existing,
                    &session_id,
                    &event_type,
                    &content,
                    metadata.as_ref(),
                    verified_sync_payload_hash.as_deref(),
                ) {
                    if sync_outbox_ingestion {
                        repair_sync_event_session_summary(&mut tx, &session_id, &user_id).await?;
                    }
                    tx.commit().await.map_err(internal_error)?;
                    return Ok(EventCreateOutcome::replayed(existing));
                }
                return Err(error_response(
                    StatusCode::CONFLICT,
                    format!("event_id {existing_id} already exists with a different payload hash"),
                ));
            }
        }

        let client_supplied_event_id = client_event_id.is_some();
        let event_id = client_event_id.unwrap_or_else(|| Uuid::new_v4().to_string());
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

        let tool_call_id = metadata_tool_call_id(metadata.as_ref());
        let meta_tool_name = metadata_tool_name(metadata.as_ref());
        let meta_duration_ms = metadata_duration_ms(metadata.as_ref());

        let insert_result = match query(
            "INSERT INTO agent_events \
             (event_id, session_id, user_id, agent_id, agent_version, event_type, content, \
              parent_event_id, causal_chain_id, `metadata`, tool_call_id, meta_tool_name, \
              meta_duration_ms, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
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
        .bind(&tool_call_id)
        .bind(&meta_tool_name)
        .bind(meta_duration_ms)
        .execute(&mut *tx)
        .await
        {
            Ok(result) => result,
            Err(error) if client_supplied_event_id && is_duplicate_key_error(&error) => {
                let select_sql = format!(
                    "SELECT {} FROM agent_events WHERE event_id = ? AND user_id = ?",
                    EVENT_DETAIL_SELECT_COLS
                );
                if let Some(row) = query(&select_sql)
                    .bind(&event_id)
                    .bind(&user_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(internal_error)?
                {
                    let existing = Self::event_record_from_row(row)?;
                    if duplicate_event_matches_request(
                        &existing,
                        &session_id,
                        &event_type,
                        &content,
                        metadata.as_ref(),
                        verified_sync_payload_hash.as_deref(),
                    ) {
                        if sync_outbox_ingestion {
                            repair_sync_event_session_summary(&mut tx, &session_id, &user_id)
                                .await?;
                        }
                        tx.commit().await.map_err(internal_error)?;
                        return Ok(EventCreateOutcome::replayed(existing));
                    }
                }
                return Err(error_response(
                    StatusCode::CONFLICT,
                    format!("event_id {event_id} already exists with a different payload"),
                ));
            }
            Err(error) => return Err(internal_error(error)),
        };

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
        // Deletion can win between the admission check above and this write.
        // That is a refused write, not an internal fault, so the caller learns
        // the session is going away instead of seeing a 500.
        let summary_write = bump_agent_session_event_count(
            &mut *tx,
            &session_id,
            &user_id,
            event_count_delta,
            Some(&event_id),
        )
        .await;
        map_session_summary_write(&mut tx, &session_id, &user_id, summary_write).await?;

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

        Ok(EventCreateOutcome::created(result))
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
            total: Some(total),
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
            let summary_write =
                bump_agent_session_event_count(&mut *tx, &record.session_id, &user_id, -1, None)
                    .await;
            map_session_summary_write(&mut tx, &record.session_id, &user_id, summary_write).await?;
        }
        tx.commit().await.map_err(internal_error)?;

        Ok(())
    }
}

/// Map a refused session write onto the owner-visible boundary error.
///
/// `Writable` means the refusal did not come from session state (a concurrent
/// writer took the row): retryable conflict, not an internal fault.
fn session_write_rejection(
    admission: SessionWriteAdmission,
    session_id: &str,
) -> (StatusCode, Json<ErrorResponse>) {
    match admission {
        SessionWriteAdmission::Deleting => error_response_coded(
            StatusCode::CONFLICT,
            format!("Session {session_id} is being deleted"),
            ERROR_CODE_SESSION_DELETING,
        ),
        SessionWriteAdmission::Fenced => error_response_coded(
            StatusCode::CONFLICT,
            format!("Session {session_id} was deleted"),
            ERROR_CODE_SESSION_DELETED,
        ),
        SessionWriteAdmission::ForeignOwner => error_response_coded(
            StatusCode::CONFLICT,
            format!("session_id {session_id} is owned by another user"),
            ERROR_CODE_SESSION_FOREIGN_OWNER,
        ),
        SessionWriteAdmission::Missing => error_response(
            StatusCode::NOT_FOUND,
            format!("Session {session_id} not found"),
        ),
        SessionWriteAdmission::Writable => error_response_coded(
            StatusCode::CONFLICT,
            format!("Session {session_id} write lost a concurrent race; retry"),
            ERROR_CODE_SESSION_WRITE_CONFLICT,
        ),
    }
}

/// Resolve why a session summary write was refused and turn it into a boundary
/// error. Only call this after a helper signalled [`sqlx::Error::RowNotFound`].
async fn reject_refused_session_write(
    tx: &mut sqlx::Transaction<'_, MySql>,
    session_id: &str,
    user_id: &str,
) -> (StatusCode, Json<ErrorResponse>) {
    match classify_session_write_admission(tx, session_id, user_id).await {
        Ok(admission) => session_write_rejection(admission, session_id),
        Err(error) => internal_error(format!(
            "session {session_id} write refusal could not be classified: {error}"
        )),
    }
}

/// Map a session summary write result, classifying refusals instead of
/// surfacing them as internal faults.
async fn map_session_summary_write(
    tx: &mut sqlx::Transaction<'_, MySql>,
    session_id: &str,
    user_id: &str,
    result: Result<(), sqlx::Error>,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    match result {
        Ok(()) => Ok(()),
        Err(sqlx::Error::RowNotFound) => {
            Err(reject_refused_session_write(tx, session_id, user_id).await)
        }
        Err(error) => Err(internal_error(error)),
    }
}

async fn ensure_event_session_requirement(
    tx: &mut sqlx::Transaction<'_, MySql>,
    session_id: &str,
    user_id: &str,
    sync_outbox_ingestion: bool,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if sync_outbox_ingestion {
        return ensure_sync_event_session_header(tx, session_id, user_id).await;
    }
    let admission = classify_session_write_admission(tx, session_id, user_id)
        .await
        .map_err(internal_error)?;
    match admission {
        SessionWriteAdmission::Writable => Ok(()),
        // A session id owned by somebody else must stay indistinguishable from
        // one that never existed on this non-sync path.
        SessionWriteAdmission::ForeignOwner => Err(error_response(
            StatusCode::NOT_FOUND,
            format!("Session {session_id} not found"),
        )),
        rejected => Err(session_write_rejection(rejected, session_id)),
    }
}

async fn ensure_sync_event_session_header(
    tx: &mut sqlx::Transaction<'_, MySql>,
    session_id: &str,
    user_id: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    match add_agent_session_event_count_or_create(&mut **tx, session_id, user_id, 0, None).await {
        Ok(()) => Ok(()),
        Err(sqlx::Error::RowNotFound) => {
            // A no-op upsert also reports zero rows for a live session whose
            // summary already holds these values, so writability decides.
            let admission = classify_session_write_admission(tx, session_id, user_id)
                .await
                .map_err(internal_error)?;
            if admission.is_writable() {
                return Ok(());
            }
            // This path creates the header on demand, so "no row anywhere" is a
            // lost race rather than a missing session: keep it retryable.
            if admission == SessionWriteAdmission::Missing {
                return Err(error_response_coded(
                    StatusCode::CONFLICT,
                    format!("Session {session_id} header could not be created; retry"),
                    ERROR_CODE_SESSION_WRITE_CONFLICT,
                ));
            }
            Err(session_write_rejection(admission, session_id))
        }
        Err(error) => Err(internal_error(error)),
    }
}

async fn repair_sync_event_session_summary(
    tx: &mut sqlx::Transaction<'_, MySql>,
    session_id: &str,
    user_id: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let count_row = query(
        "SELECT COUNT(*) AS event_count FROM agent_events WHERE session_id = ? AND user_id = ?",
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(internal_error)?;
    let event_count = count_row
        .try_get::<i64, _>("event_count")
        .map_err(|e| internal_error(format!("sync outbox session summary count decode: {e}")))?;
    let latest_row = query(
        "SELECT event_id FROM agent_events \
         WHERE session_id = ? AND user_id = ? \
         ORDER BY created_at DESC, event_id DESC LIMIT 1",
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(internal_error)?;
    let last_event_id = latest_row
        .as_ref()
        .map(|row| event_row_string(row, "event_id"))
        .transpose()?;
    // Same deletion fence as every other session summary writer: a replayed
    // sync event must not rewrite the summary of a session that is going away.
    let result = query(
        "UPDATE agent_sessions \
         SET event_count = ?, last_event_id = ?, updated_at = NOW(6), last_active_at = NOW(6) \
         WHERE session_id = ? AND user_id = ? AND status <> 'deleting'",
    )
    .bind(event_count)
    .bind(last_event_id)
    .bind(session_id)
    .bind(user_id)
    .execute(&mut **tx)
    .await
    .map_err(internal_error)?;
    if result.rows_affected() == 0 {
        let admission = classify_session_write_admission(tx, session_id, user_id)
            .await
            .map_err(internal_error)?;
        if !admission.is_writable() {
            return Err(session_write_rejection(admission, session_id));
        }
    }
    Ok(())
}

// ── Noop implementation ──────────────────────────────────────────────────────

pub struct UnconfiguredEventService;

#[async_trait]
impl EventService for UnconfiguredEventService {
    async fn create_event(
        &self,
        _: String,
        _: EventCreateRequestData,
    ) -> Result<EventCreateOutcome, (StatusCode, Json<ErrorResponse>)> {
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
#[serde(deny_unknown_fields)]
pub struct EventCreateRequest {
    pub event_id: Option<String>,
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
    pub causal_chain_id: Option<String>,
    pub metadata: serde_json::Value,
    pub created_at: String,
}

#[derive(Serialize, PartialEq)]
pub struct EventListResponse {
    pub events: Vec<EventResponse>,
    pub total: Option<i64>,
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

    // --- session write rejection ---

    #[test]
    fn refused_session_writes_are_reported_as_typed_client_errors() {
        // A refused write is a fact about session state, never an internal
        // fault: deletion races must stay 4xx with a machine-readable code so
        // clients can tell "retry later" from "never again".
        for (admission, expected_status, expected_code) in [
            (
                SessionWriteAdmission::Deleting,
                StatusCode::CONFLICT,
                Some(ERROR_CODE_SESSION_DELETING),
            ),
            (
                SessionWriteAdmission::Fenced,
                StatusCode::CONFLICT,
                Some(ERROR_CODE_SESSION_DELETED),
            ),
            (
                SessionWriteAdmission::ForeignOwner,
                StatusCode::CONFLICT,
                Some(ERROR_CODE_SESSION_FOREIGN_OWNER),
            ),
            (
                SessionWriteAdmission::Writable,
                StatusCode::CONFLICT,
                Some(ERROR_CODE_SESSION_WRITE_CONFLICT),
            ),
            (SessionWriteAdmission::Missing, StatusCode::NOT_FOUND, None),
        ] {
            let (status, body) = session_write_rejection(admission, "session-1");
            assert_eq!(status, expected_status, "{admission:?} status");
            assert_eq!(
                body.0.error_code.as_deref(),
                expected_code,
                "{admission:?} error code"
            );
            assert!(
                body.0.detail.contains("session-1"),
                "{admission:?} detail must name the session: {}",
                body.0.detail
            );
        }
    }

    #[test]
    fn only_a_live_owner_session_admits_writes() {
        assert!(SessionWriteAdmission::Writable.is_writable());
        for refused in [
            SessionWriteAdmission::Deleting,
            SessionWriteAdmission::Fenced,
            SessionWriteAdmission::ForeignOwner,
            SessionWriteAdmission::Missing,
        ] {
            assert!(!refused.is_writable(), "{refused:?} must not admit writes");
        }
    }

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
        with_event_type.event_type = Some("tool_call_completed".to_string());
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
                "event_type" => "tool_call_completed",
                "content" => r#"{"cmd":"pwd"}"#,
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
                "causal_chain_id" => Some("chain-1".to_string()),
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
        assert_eq!(record.event_type, "tool_call_completed");
        assert_eq!(record.content, r#"{"cmd":"pwd"}"#);
        assert_eq!(record.agent_id.as_deref(), Some("agent-1"));
        assert_eq!(record.agent_version.as_deref(), Some("1.2.3"));
        assert_eq!(record.parent_event_id, None);
        assert_eq!(record.parent_event_ids, Vec::<String>::new());
        assert_eq!(record.causal_chain_id.as_deref(), Some("chain-1"));
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
    fn agent_session_event_count_preserves_value_and_fails_loudly() {
        assert_eq!(
            agent_session_event_count(&FakeEventDbRow::complete()).unwrap(),
            7
        );

        let (status, Json(body)) =
            agent_session_event_count(&FakeEventDbRow::fail_on("event_count")).unwrap_err();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            body.detail.contains("decode column `event_count`"),
            "error should identify event_count decode failure: {:?}",
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
            event_type: "tool_call_completed".to_string(),
            content: "{}".to_string(),
            agent_id: Some("a1".to_string()),
            agent_version: None,
            parent_event_id: None,
            parent_event_ids: vec!["p1".to_string(), "p2".to_string()],
            causal_chain_id: Some("cc1".to_string()),
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
                causal_chain_id: Some("cc".to_string()),
                metadata: serde_json::json!(null),
                created_at: "now".to_string(),
            }],
            total: Some(42),
            limit: 10,
            next_cursor: Some(EventListCursor {
                created_at: "2026-04-01T10:00:00.123456".to_string(),
                event_id: "e1".to_string(),
            }),
        };
        let resp = EventListResponse::from(record);
        assert_eq!(resp.events.len(), 1);
        assert_eq!(resp.total, Some(42));
        assert_eq!(
            resp.next_cursor
                .as_ref()
                .map(|cursor| cursor.event_id.as_str()),
            Some("e1")
        );
    }

    #[test]
    fn event_list_record_to_response_preserves_omitted_total() {
        let response = EventListResponse::from(EventListRecord {
            events: Vec::new(),
            total: None,
            limit: 50,
            next_cursor: None,
        });

        assert_eq!(response.total, None);
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
    fn client_event_id_is_bounded_by_agent_event_column() {
        let valid = normalize_client_event_id(Some("sync_evt_123".to_string()))
            .unwrap()
            .unwrap();
        assert_eq!(valid, "sync_evt_123");

        let too_long = "x".repeat(crate::storage::AGENT_EVENT_ID_LEN + 1);
        assert_eq!(
            normalize_client_event_id(Some(too_long)).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn duplicate_event_match_uses_server_verified_sync_outbox_payload_hash() {
        let existing_content = r#"{"a":1,"b":2}"#.to_string();
        let incoming_content = r#"{"b":2,"a":1}"#;
        let incoming_hash = sync_outbox_content_payload_hash(incoming_content).unwrap();
        let existing = EventRecord {
            event_id: "sync_evt_1".to_string(),
            user_id: "user-a".to_string(),
            session_id: "session-a".to_string(),
            event_type: "sync_marker".to_string(),
            content: existing_content,
            agent_id: None,
            agent_version: None,
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: None,
            metadata: serde_json::json!({
                "sync_outbox": {
                    "payload_hash": "sha256:same"
                }
            }),
            created_at: "2026-07-08T00:00:00.000000".to_string(),
        };

        assert!(duplicate_event_matches_request(
            &existing,
            "session-a",
            "sync_marker",
            incoming_content,
            Some(&serde_json::json!({
                "sync_outbox": {
                    "payload_hash": incoming_hash.clone()
                }
            })),
            Some(&incoming_hash),
        ));
        let different_content_hash = sync_outbox_content_payload_hash(r#"{"a":1,"b":3}"#).unwrap();
        assert!(!duplicate_event_matches_request(
            &existing,
            "session-a",
            "sync_marker",
            r#"{"a":1,"b":3}"#,
            Some(&serde_json::json!({
                "sync_outbox": {
                    "payload_hash": different_content_hash.clone()
                }
            })),
            Some(&different_content_hash),
        ));
    }

    #[test]
    fn sync_outbox_metadata_is_not_client_authority() {
        let metadata = serde_json::json!({
            "sync_outbox": {
                "payload_hash": sync_outbox_content_payload_hash("{}").unwrap()
            }
        });

        assert_eq!(
            verified_sync_outbox_payload_hash_for_source(
                EventIngestionSource::Client,
                "{}",
                Some(&metadata)
            )
            .unwrap_err()
            .0,
            StatusCode::BAD_REQUEST
        );
        assert!(
            verified_sync_outbox_payload_hash_for_source(
                EventIngestionSource::SyncOutbox,
                "{}",
                Some(&metadata)
            )
            .unwrap()
            .is_some()
        );
        assert_eq!(
            verified_sync_outbox_payload_hash_for_source(
                EventIngestionSource::SyncOutbox,
                "{}",
                None
            )
            .unwrap_err()
            .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn sync_outbox_identity_requires_stable_event_id_and_matching_headers() {
        let event = crate::session_journal::JournalEvent::config_change(
            Some("session-a"),
            "model",
            "gpt-5",
        );
        let content = serde_json::to_string(&event).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&content).unwrap();
        let payload_hash = sync_outbox_canonical_payload_hash(&payload);
        let event_id = sync_outbox_stable_event_id(&event, &payload_hash).unwrap();

        verify_sync_outbox_event_identity(
            &content,
            &payload_hash,
            &event_id,
            "session-a",
            "config_change",
        )
        .unwrap();
        assert_eq!(
            verify_sync_outbox_event_identity(
                &content,
                &payload_hash,
                "sync_evt_wrong",
                "session-a",
                "config_change",
            )
            .unwrap_err()
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            verify_sync_outbox_event_identity(
                &content,
                &payload_hash,
                &event_id,
                "session-b",
                "config_change",
            )
            .unwrap_err()
            .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn sync_outbox_payload_hash_is_verified_against_content() {
        let content = r#"{"b":2,"a":1}"#;
        let hash = sync_outbox_content_payload_hash(content).unwrap();
        assert_eq!(
            verified_sync_outbox_payload_hash(
                content,
                Some(&serde_json::json!({
                    "sync_outbox": {
                        "payload_hash": hash.clone()
                    }
                })),
            )
            .unwrap()
            .as_deref(),
            Some(hash.as_str())
        );

        assert_eq!(
            verified_sync_outbox_payload_hash(
                content,
                Some(&serde_json::json!({
                    "sync_outbox": {
                        "payload_hash": "sha256:forged"
                    }
                })),
            )
            .unwrap_err()
            .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn duplicate_event_match_never_replays_across_session_or_type() {
        let existing = EventRecord {
            event_id: "sync_evt_1".to_string(),
            user_id: "user-a".to_string(),
            session_id: "session-a".to_string(),
            event_type: "sync_marker".to_string(),
            content: "{}".to_string(),
            agent_id: None,
            agent_version: None,
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: None,
            metadata: serde_json::json!({
                "sync_outbox": {
                    "payload_hash": "sha256:same"
                }
            }),
            created_at: "2026-07-08T00:00:00.000000".to_string(),
        };
        let metadata = serde_json::json!({
            "sync_outbox": {
                "payload_hash": "sha256:same"
            }
        });

        assert!(!duplicate_event_matches_request(
            &existing,
            "session-b",
            "sync_marker",
            "{}",
            Some(&metadata),
            Some("sha256:same"),
        ));
        assert!(!duplicate_event_matches_request(
            &existing,
            "session-a",
            "other_event",
            "{}",
            Some(&metadata),
            Some("sha256:same"),
        ));
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
                "event_type":"tool_call_completed",
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

    #[test]
    fn event_create_request_rejects_unknown_fields() {
        let result = serde_json::from_str::<EventCreateRequest>(
            r#"{
                "session_id":"s1",
                "event_type":"tool_call_completed",
                "content":"x",
                "ignored_by_business_logic":"must not be accepted"
            }"#,
        );

        match result {
            Ok(_) => panic!("unknown EventCreateRequest fields must be rejected"),
            Err(err) => assert!(
                err.to_string().contains("unknown field"),
                "unexpected error: {err}"
            ),
        }
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
