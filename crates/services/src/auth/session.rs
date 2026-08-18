use crate::pagination::MAX_API_LIST_LIMIT;
use crate::session_lifecycle::{SessionTableDeleteOutcome, hard_delete_session};
use crate::storage::{log_session_audit, session_record_from_row};
use astra_core::{ErrorResponse, MatrixOneSettings, SharedPool, error_response, internal_error};
use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{MySql, QueryBuilder, Row, query};
use uuid::Uuid;

const MAX_SESSION_ACTIVITY_ROWS: u32 = 200;
const SESSION_LIST_CURSOR_FILTER_SQL: &str = " AND (COALESCE(updated_at, created_at) < ";
const SESSION_LIST_CURSOR_TIE_SQL: &str = " OR (COALESCE(updated_at, created_at) = ";
const SESSION_LIST_ORDER_SQL: &str =
    " ORDER BY COALESCE(updated_at, created_at) DESC, session_id DESC LIMIT ";
const SESSION_ACTIVITY_SELECT_SQL: &str = "SELECT log_id, action, \
        IFNULL(CAST(details AS CHAR), 'null') AS details_json, \
        DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%f') AS created_at \
     FROM auth_audit_logs \
     WHERE user_id = ? AND resource_type = 'session' AND resource_id = ? \
     ORDER BY created_at DESC, log_id DESC LIMIT ?";
const SESSION_ACTIVITY_SELECT_AFTER_SQL: &str = "SELECT log_id, action, \
        IFNULL(CAST(details AS CHAR), 'null') AS details_json, \
        DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%f') AS created_at \
     FROM auth_audit_logs \
     WHERE user_id = ? AND resource_type = 'session' AND resource_id = ? \
       AND (created_at < ? OR (created_at = ? AND log_id < ?)) \
     ORDER BY created_at DESC, log_id DESC LIMIT ?";

#[async_trait]
pub trait SessionService: Send + Sync {
    async fn create_session(
        &self,
        user_id: String,
        request: SessionCreateRequestData,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn list_sessions(
        &self,
        filter: SessionListFilter,
    ) -> Result<SessionListRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_session(
        &self,
        session_id: String,
        user_id: String,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn update_session(
        &self,
        session_id: String,
        user_id: String,
        request: SessionUpdateRequestData,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn delete_session(
        &self,
        session_id: String,
        user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)>;

    async fn get_session_activity(
        &self,
        session_id: String,
        user_id: String,
        limit: u32,
        cursor: Option<SessionActivityCursor>,
    ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionCreateRequestData {
    pub agent_id: Option<String>,
    pub title: Option<String>,
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionUpdateRequestData {
    pub title: Option<String>,
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
    pub metadata_patch: Option<serde_json::Map<String, serde_json::Value>>,
    pub status: Option<String>,
}

fn apply_metadata_patch(
    metadata: &mut serde_json::Map<String, serde_json::Value>,
    patch: serde_json::Map<String, serde_json::Value>,
) {
    for (key, value) in patch {
        if value.is_null() {
            metadata.remove(&key);
        } else {
            metadata.insert(key, value);
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionListFilter {
    pub user_id: String,
    pub agent_id: Option<String>,
    pub status: Option<String>,
    pub limit: u32,
    pub cursor: Option<SessionListCursor>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionRecord {
    pub session_id: String,
    pub user_id: String,
    pub agent_id: Option<String>,
    pub title: Option<String>,
    pub metadata: serde_json::Map<String, serde_json::Value>,
    pub status: String,
    pub event_count: i64,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub ended_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionListRecord {
    pub sessions: Vec<SessionRecord>,
    pub total: Option<i64>,
    pub limit: u32,
    pub next_cursor: Option<SessionListCursor>,
}

/// Cursor for session list pagination.
///
/// The `updated_at` field carries the value of `COALESCE(updated_at, created_at)` from the
/// database — it is the *ordering key*, not strictly the `updated_at` column.  When a session
/// has never been updated the cursor will contain its `created_at` timestamp.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListCursor {
    pub updated_at: String,
    pub session_id: String,
}

fn validate_session_list_limit(limit: u32) -> u32 {
    limit.clamp(1, MAX_API_LIST_LIMIT)
}

fn session_list_query_limit(limit: u32) -> i64 {
    i64::from(limit) + 1
}

fn session_list_cursor_db_updated_at(
    cursor: &SessionListCursor,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let updated_at = cursor.updated_at.trim();
    if updated_at.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid session list cursor: updated_at is required",
        ));
    }
    let db_updated_at = updated_at.replace('T', " ");
    if db_updated_at.len() != "YYYY-MM-DD HH:MM:SS.ffffff".len()
        || db_updated_at.as_bytes().get(10) != Some(&b' ')
        || db_updated_at.as_bytes().get(19) != Some(&b'.')
        || chrono::NaiveDateTime::parse_from_str(&db_updated_at, "%Y-%m-%d %H:%M:%S%.6f").is_err()
    {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("invalid session list cursor timestamp: {updated_at}"),
        ));
    }
    Ok(db_updated_at)
}

fn session_list_cursor_session_id(
    cursor: &SessionListCursor,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let session_id = cursor.session_id.trim();
    if session_id.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid session list cursor: session_id is required",
        ));
    }
    Ok(session_id.to_string())
}

fn session_list_cursor_from_row(
    row: &sqlx::mysql::MySqlRow,
) -> Result<SessionListCursor, (StatusCode, Json<ErrorResponse>)> {
    let updated_at: String = row.try_get("cursor_updated_at").map_err(internal_error)?;
    let session_id: String = row.try_get("session_id").map_err(internal_error)?;
    if updated_at.trim().is_empty() {
        return Err(internal_error(format!(
            "invalid agent_sessions cursor: session_id={session_id}, column=cursor_updated_at, value is empty"
        )));
    }
    if session_id.trim().is_empty() {
        return Err(internal_error(
            "invalid agent_sessions cursor: column=session_id, value is empty",
        ));
    }
    Ok(SessionListCursor {
        updated_at,
        session_id,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionActivityEntryRecord {
    pub log_id: String,
    pub action: String,
    pub details: serde_json::Value,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionActivityRecord {
    pub session_id: String,
    pub activities: Vec<SessionActivityEntryRecord>,
    pub total: i64,
    pub limit: u32,
    pub next_cursor: Option<SessionActivityCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivityCursor {
    pub created_at: String,
    pub log_id: String,
}

fn validate_session_activity_limit(limit: u32) -> u32 {
    limit.clamp(1, MAX_SESSION_ACTIVITY_ROWS)
}

fn session_activity_query_limit(limit: u32) -> i64 {
    i64::from(limit) + 1
}

fn session_activity_cursor_db_created_at(
    cursor: &SessionActivityCursor,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let created_at = cursor.created_at.trim();
    if created_at.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid session activity cursor: created_at is required",
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
            format!("invalid session activity cursor timestamp: {created_at}"),
        ));
    }
    Ok(db_created_at)
}

fn session_activity_cursor_log_id(
    cursor: &SessionActivityCursor,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let log_id = cursor.log_id.trim();
    if log_id.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid session activity cursor: log_id is required",
        ));
    }
    Ok(log_id.to_string())
}

fn session_activity_cursor_from_entry(
    entry: &SessionActivityEntryRecord,
) -> Result<SessionActivityCursor, (StatusCode, Json<ErrorResponse>)> {
    if entry.created_at.trim().is_empty() {
        return Err(internal_error(format!(
            "invalid auth_audit_logs cursor: log_id={}, column=created_at, value is empty",
            entry.log_id
        )));
    }
    if entry.log_id.trim().is_empty() {
        return Err(internal_error(
            "invalid auth_audit_logs cursor: column=log_id, value is empty",
        ));
    }
    Ok(SessionActivityCursor {
        created_at: entry.created_at.clone(),
        log_id: entry.log_id.clone(),
    })
}

fn parse_session_activity_details(
    log_id: &str,
    details_json: Option<String>,
) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
    let Some(details_json) = details_json else {
        return Ok(serde_json::Value::Null);
    };
    serde_json::from_str(&details_json).map_err(|source| {
        internal_error(format!(
            "invalid auth session activity details JSON: log_id={log_id}: {source}"
        ))
    })
}

fn session_activity_entry_from_row(
    row: sqlx::mysql::MySqlRow,
) -> Result<SessionActivityEntryRecord, (StatusCode, Json<ErrorResponse>)> {
    let log_id: String = row.try_get("log_id").map_err(internal_error)?;
    let action: String = row.try_get("action").map_err(internal_error)?;
    let details = parse_session_activity_details(
        &log_id,
        row.try_get::<Option<String>, _>("details_json")
            .map_err(internal_error)?,
    )?;
    let created_at: String = row.try_get("created_at").map_err(internal_error)?;
    Ok(SessionActivityEntryRecord {
        log_id,
        action,
        details,
        created_at,
    })
}

#[derive(Clone, Debug)]
pub struct DatabaseSessionService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

#[derive(Clone, Debug, PartialEq)]
struct SessionDeletionOutcome {
    session: SessionRecord,
    database_rows_deleted: u64,
    session_references_cleared: u64,
    workspace_cleanup_debts_enqueued: u64,
    database_tables_deleted: Vec<SessionTableDeleteOutcome>,
    local_bytes_freed: u64,
    workspaces_removed: u64,
    database_delete_ms: u64,
    local_artifact_delete_ms: u64,
    workspace_delete_ms: u64,
    total_delete_ms: u64,
    cleanup_errors: Vec<String>,
}

impl DatabaseSessionService {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }

    async fn fetch_session_for_user(
        &self,
        pool: &sqlx::Pool<MySql>,
        session_id: &str,
        user_id: &str,
    ) -> Result<Option<SessionRecord>, (StatusCode, Json<ErrorResponse>)> {
        query(
            "SELECT session_id, user_id, agent_id, title, status, event_count, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at, \
             DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s') AS updated_at, \
             DATE_FORMAT(ended_at, '%Y-%m-%dT%H:%i:%s') AS ended_at, \
             IFNULL(CAST(`metadata` AS CHAR), '{}') AS metadata_json \
             FROM agent_sessions WHERE session_id = ? AND user_id = ? LIMIT 1",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(internal_error)?
        .map(session_record_from_row)
        .transpose()
    }

    async fn delete_session_lifecycle(
        &self,
        pool: &sqlx::Pool<MySql>,
        session_id: &str,
        user_id: &str,
    ) -> Result<SessionDeletionOutcome, (StatusCode, Json<ErrorResponse>)> {
        let existing = self
            .fetch_session_for_user(pool, session_id, user_id)
            .await?
            .ok_or_else(|| {
                error_response(
                    StatusCode::NOT_FOUND,
                    format!("Session {session_id} 不存在"),
                )
            })?;

        let hard_delete = hard_delete_session(pool, session_id, user_id)
            .await
            .map_err(internal_error)?;

        Ok(SessionDeletionOutcome {
            session: existing,
            database_rows_deleted: hard_delete.database_rows_deleted,
            session_references_cleared: hard_delete.session_references_cleared,
            workspace_cleanup_debts_enqueued: hard_delete.workspace_cleanup_debts_enqueued,
            database_tables_deleted: hard_delete.database_tables_deleted,
            local_bytes_freed: hard_delete.local_bytes_freed,
            workspaces_removed: hard_delete.workspaces_removed,
            database_delete_ms: hard_delete.database_delete_ms,
            local_artifact_delete_ms: hard_delete.local_artifact_delete_ms,
            workspace_delete_ms: hard_delete.workspace_delete_ms,
            total_delete_ms: hard_delete.total_delete_ms,
            cleanup_errors: hard_delete.cleanup_errors,
        })
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        crate::require_shared_pool(
            self.pool.as_ref(),
            "DatabaseSessionService",
            &self.matrixone,
        )
    }
}

#[async_trait]
impl SessionService for DatabaseSessionService {
    async fn create_session(
        &self,
        user_id: String,
        request: SessionCreateRequestData,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let session_id = Uuid::new_v4().to_string();
        let title = request
            .title
            .unwrap_or_else(|| format!("Session {}", Utc::now().format("%Y-%m-%d %H:%M")));
        let metadata = serde_json::Value::Object(request.metadata.unwrap_or_default()).to_string();

        query(
            "INSERT INTO agent_sessions \
             (session_id, user_id, agent_id, title, status, event_count, created_at, updated_at, last_active_at, `metadata`) \
             VALUES (?, ?, ?, ?, 'active', 0, NOW(), NOW(), NOW(), ?)",
        )
        .bind(&session_id)
        .bind(&user_id)
        .bind(&request.agent_id)
        .bind(&title)
        .bind(metadata)
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        let record = self
            .fetch_session_for_user(&pool, &session_id, &user_id)
            .await?
            .ok_or_else(|| internal_error("failed to read created session"))?;
        let details = serde_json::json!({
            "title": record.title,
            "agent_id": record.agent_id,
        });
        log_session_audit(&pool, &user_id, "session_create", &session_id, details).await;
        Ok(record)
    }

    async fn list_sessions(
        &self,
        filter: SessionListFilter,
    ) -> Result<SessionListRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let limit = validate_session_list_limit(filter.limit);

        let total = None;

        let mut list_query = QueryBuilder::<MySql>::new(
            "SELECT session_id, user_id, agent_id, title, status, event_count, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at, \
             DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s') AS updated_at, \
             DATE_FORMAT(ended_at, '%Y-%m-%dT%H:%i:%s') AS ended_at, \
             DATE_FORMAT(COALESCE(updated_at, created_at), '%Y-%m-%dT%H:%i:%s.%f') AS cursor_updated_at, \
             IFNULL(CAST(`metadata` AS CHAR), '{}') AS metadata_json \
             FROM agent_sessions WHERE user_id = ",
        );
        list_query.push_bind(&filter.user_id);
        if let Some(agent_id) = &filter.agent_id {
            list_query.push(" AND agent_id = ");
            list_query.push_bind(agent_id);
        }
        if let Some(status) = &filter.status {
            list_query.push(" AND status = ");
            list_query.push_bind(status);
        }
        if let Some(cursor) = &filter.cursor {
            let updated_at = session_list_cursor_db_updated_at(cursor)?;
            let session_id = session_list_cursor_session_id(cursor)?;
            list_query.push(SESSION_LIST_CURSOR_FILTER_SQL);
            list_query.push_bind(updated_at.clone());
            list_query.push(SESSION_LIST_CURSOR_TIE_SQL);
            list_query.push_bind(updated_at);
            list_query.push(" AND session_id < ");
            list_query.push_bind(session_id);
            list_query.push("))");
        }
        list_query.push(SESSION_LIST_ORDER_SQL);
        list_query.push_bind(session_list_query_limit(limit));

        let rows = list_query
            .build()
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let cursor = session_list_cursor_from_row(&row)?;
            let session = session_record_from_row(row)?;
            entries.push((session, cursor));
        }
        let has_more = entries.len() > limit as usize;
        if has_more {
            entries.truncate(limit as usize);
        }
        let next_cursor = if has_more {
            entries.last().map(|(_, cursor)| cursor.clone())
        } else {
            None
        };
        let sessions = entries
            .into_iter()
            .map(|(session, _)| session)
            .collect::<Vec<_>>();

        Ok(SessionListRecord {
            sessions,
            total,
            limit,
            next_cursor,
        })
    }

    async fn get_session(
        &self,
        session_id: String,
        user_id: String,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let session = self
            .fetch_session_for_user(&pool, &session_id, &user_id)
            .await?
            .ok_or_else(|| {
                error_response(
                    StatusCode::NOT_FOUND,
                    format!("Session {session_id} 不存在"),
                )
            })?;

        Ok(session)
    }

    async fn update_session(
        &self,
        session_id: String,
        user_id: String,
        request: SessionUpdateRequestData,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let existing = self
            .fetch_session_for_user(&pool, &session_id, &user_id)
            .await?
            .ok_or_else(|| {
                error_response(
                    StatusCode::NOT_FOUND,
                    format!("Session {session_id} 不存在"),
                )
            })?;

        let SessionUpdateRequestData {
            title,
            metadata,
            metadata_patch,
            status,
        } = request;

        if metadata.is_some() && metadata_patch.is_some() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "metadata and metadata_patch are mutually exclusive",
            ));
        }

        if title.is_none() && metadata.is_none() && metadata_patch.is_none() && status.is_none() {
            return Ok(existing);
        }

        let mut tx = pool.begin().await.map_err(internal_error)?;
        let locked = query(
            "SELECT IFNULL(CAST(`metadata` AS CHAR), '{}') AS metadata_json \
             FROM agent_sessions WHERE session_id = ? AND user_id = ? FOR UPDATE",
        )
        .bind(&session_id)
        .bind(&user_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("Session {session_id} 不存在"),
            )
        })?;

        let metadata_patch_for_audit = metadata_patch.clone();
        let metadata = match (metadata, metadata_patch) {
            (Some(metadata), None) => Some(metadata),
            (None, Some(patch)) => {
                let raw = locked
                    .try_get::<String, _>("metadata_json")
                    .map_err(internal_error)?;
                let value: serde_json::Value =
                    serde_json::from_str(&raw).map_err(internal_error)?;
                let mut current = value.as_object().cloned().ok_or_else(|| {
                    internal_error("persisted session metadata must be a JSON object")
                })?;
                apply_metadata_patch(&mut current, patch);
                Some(current)
            }
            (None, None) => None,
            (Some(_), Some(_)) => unreachable!("validated above"),
        };

        let mut update_query =
            QueryBuilder::<MySql>::new("UPDATE agent_sessions SET updated_at = NOW()");
        if let Some(title) = &title {
            update_query.push(", title = ");
            update_query.push_bind(title);
        }
        if let Some(metadata) = &metadata {
            update_query.push(", `metadata` = ");
            update_query.push_bind(serde_json::Value::Object(metadata.clone()).to_string());
        }
        if let Some(status) = &status {
            update_query.push(", status = ");
            update_query.push_bind(status);
            if status == "ended" {
                update_query.push(", ended_at = NOW()");
            }
        }
        update_query.push(" WHERE session_id = ");
        update_query.push_bind(&session_id);
        update_query.push(" AND user_id = ");
        update_query.push_bind(&user_id);
        // A delete intent is irreversible: the foreground hard-delete may be
        // retried by maintenance after this request returns, so do not let an
        // ordinary session update revive or alter a session being deleted.
        update_query.push(" AND delete_requested_at IS NULL");

        let rows_affected = update_query
            .build()
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?
            .rows_affected();
        if rows_affected == 0 {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("Session {session_id} 不存在"),
            ));
        }
        tx.commit().await.map_err(internal_error)?;

        let updated = self
            .fetch_session_for_user(&pool, &session_id, &user_id)
            .await?
            .ok_or_else(|| internal_error("failed to read updated session"))?;
        let details = serde_json::json!({
            "title": title,
            "status": status,
            "metadata": metadata,
            "metadata_patch": metadata_patch_for_audit,
        });
        log_session_audit(&pool, &user_id, "session_update", &session_id, details).await;
        Ok(updated)
    }

    async fn delete_session(
        &self,
        session_id: String,
        user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let outcome = self
            .delete_session_lifecycle(&pool, &session_id, &user_id)
            .await?;

        let details = serde_json::json!({
            "title": outcome.session.title,
            "database_rows_deleted": outcome.database_rows_deleted,
            "session_references_cleared": outcome.session_references_cleared,
            "workspace_cleanup_debts_enqueued": outcome.workspace_cleanup_debts_enqueued,
            "database_tables_deleted": outcome.database_tables_deleted,
            "local_bytes_freed": outcome.local_bytes_freed,
            "workspaces_removed": outcome.workspaces_removed,
            "database_delete_ms": outcome.database_delete_ms,
            "local_artifact_delete_ms": outcome.local_artifact_delete_ms,
            "workspace_delete_ms": outcome.workspace_delete_ms,
            "total_delete_ms": outcome.total_delete_ms,
            "cleanup_errors": outcome.cleanup_errors,
        });
        log_session_audit(&pool, &user_id, "session_delete", &session_id, details).await;
        Ok(())
    }

    async fn get_session_activity(
        &self,
        session_id: String,
        user_id: String,
        limit: u32,
        cursor: Option<SessionActivityCursor>,
    ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.fetch_session_for_user(&pool, &session_id, &user_id)
            .await?
            .ok_or_else(|| {
                error_response(
                    StatusCode::NOT_FOUND,
                    format!("Session {session_id} 不存在"),
                )
            })?;
        let limit = validate_session_activity_limit(limit);

        let count_row = query(
            "SELECT COUNT(*) as cnt FROM auth_audit_logs \
             WHERE user_id = ? AND resource_type = 'session' AND resource_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;
        let total: i64 = count_row.try_get("cnt").map_err(internal_error)?;

        let select_query = if let Some(cursor) = &cursor {
            let created_at = session_activity_cursor_db_created_at(cursor)?;
            let log_id = session_activity_cursor_log_id(cursor)?;
            let mut query = query(SESSION_ACTIVITY_SELECT_AFTER_SQL)
                .bind(&user_id)
                .bind(&session_id)
                .bind(created_at.clone())
                .bind(created_at)
                .bind(log_id);
            query = query.bind(session_activity_query_limit(limit));
            query
        } else {
            let mut query = query(SESSION_ACTIVITY_SELECT_SQL)
                .bind(&user_id)
                .bind(&session_id);
            query = query.bind(session_activity_query_limit(limit));
            query
        };

        let rows = select_query
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let mut activities = rows
            .into_iter()
            .map(session_activity_entry_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = activities.len() > limit as usize;
        if has_more {
            activities.truncate(limit as usize);
        }
        let next_cursor = if has_more {
            activities
                .last()
                .map(session_activity_cursor_from_entry)
                .transpose()?
        } else {
            None
        };

        Ok(SessionActivityRecord {
            session_id,
            activities,
            total,
            limit,
            next_cursor,
        })
    }
}

#[derive(Clone, Debug)]
pub struct UnconfiguredSessionService;

#[async_trait]
impl SessionService for UnconfiguredSessionService {
    async fn create_session(
        &self,
        _user_id: String,
        _request: SessionCreateRequestData,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Session service not configured",
        ))
    }

    async fn list_sessions(
        &self,
        _filter: SessionListFilter,
    ) -> Result<SessionListRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Session service not configured",
        ))
    }

    async fn get_session(
        &self,
        _session_id: String,
        _user_id: String,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Session service not configured",
        ))
    }

    async fn update_session(
        &self,
        _session_id: String,
        _user_id: String,
        _request: SessionUpdateRequestData,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Session service not configured",
        ))
    }

    async fn delete_session(
        &self,
        _session_id: String,
        _user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Session service not configured",
        ))
    }

    async fn get_session_activity(
        &self,
        _session_id: String,
        _user_id: String,
        _limit: u32,
        _cursor: Option<SessionActivityCursor>,
    ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Session service not configured",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_list_limit_has_hard_cap_and_minimum() {
        assert_eq!(validate_session_list_limit(0), 1);
        assert_eq!(validate_session_list_limit(10), 10);
        assert_eq!(validate_session_list_limit(u32::MAX), MAX_API_LIST_LIMIT);
    }

    #[test]
    fn session_list_cursor_rejects_incomplete_values() {
        let cursor = SessionListCursor {
            updated_at: "2026-04-01T10:00:00.123456".to_string(),
            session_id: "session-1".to_string(),
        };
        assert_eq!(
            session_list_cursor_db_updated_at(&cursor).unwrap(),
            "2026-04-01 10:00:00.123456"
        );
        assert_eq!(
            session_list_cursor_session_id(&cursor).unwrap(),
            "session-1"
        );

        let invalid_time = SessionListCursor {
            updated_at: "2026-04-01T10:00:00".to_string(),
            session_id: "session-1".to_string(),
        };
        assert_eq!(
            session_list_cursor_db_updated_at(&invalid_time)
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );

        let missing_session_id = SessionListCursor {
            updated_at: "2026-04-01T10:00:00.123456".to_string(),
            session_id: "  ".to_string(),
        };
        assert_eq!(
            session_list_cursor_session_id(&missing_session_id)
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn session_list_sql_contract_uses_seek_cursor_not_offset() {
        let order_sql = SESSION_LIST_ORDER_SQL.to_ascii_uppercase();
        let cursor_sql = format!(
            "{}?{}? AND session_id < ?))",
            SESSION_LIST_CURSOR_FILTER_SQL, SESSION_LIST_CURSOR_TIE_SQL
        )
        .to_ascii_uppercase();
        assert!(!order_sql.contains(" OFFSET "));
        assert!(!cursor_sql.contains(" OFFSET "));
        assert!(order_sql.contains("SESSION_ID DESC"));
        assert!(cursor_sql.contains("SESSION_ID < ?"));
    }

    #[test]
    fn session_activity_limit_has_hard_cap_and_minimum() {
        assert_eq!(validate_session_activity_limit(0), 1);
        assert_eq!(validate_session_activity_limit(10), 10);
        assert_eq!(
            validate_session_activity_limit(u32::MAX),
            MAX_SESSION_ACTIVITY_ROWS
        );
    }

    #[test]
    fn session_activity_cursor_rejects_incomplete_values() {
        let cursor = SessionActivityCursor {
            created_at: "2026-04-01T10:00:00.123456".to_string(),
            log_id: "log-1".to_string(),
        };
        assert_eq!(
            session_activity_cursor_db_created_at(&cursor).unwrap(),
            "2026-04-01 10:00:00.123456"
        );
        assert_eq!(session_activity_cursor_log_id(&cursor).unwrap(), "log-1");

        let invalid_time = SessionActivityCursor {
            created_at: "2026-04-01T10:00:00".to_string(),
            log_id: "log-1".to_string(),
        };
        assert_eq!(
            session_activity_cursor_db_created_at(&invalid_time)
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );

        let missing_log_id = SessionActivityCursor {
            created_at: "2026-04-01T10:00:00.123456".to_string(),
            log_id: "  ".to_string(),
        };
        assert_eq!(
            session_activity_cursor_log_id(&missing_log_id)
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn session_activity_sql_contract_uses_seek_cursor_not_offset() {
        let base_sql = SESSION_ACTIVITY_SELECT_SQL.to_ascii_uppercase();
        let cursor_sql = SESSION_ACTIVITY_SELECT_AFTER_SQL.to_ascii_uppercase();
        assert!(!base_sql.contains(" OFFSET "));
        assert!(!cursor_sql.contains(" OFFSET "));
        assert!(cursor_sql.contains("CREATED_AT < ?"));
        assert!(cursor_sql.contains("LOG_ID < ?"));
        assert!(cursor_sql.contains("ORDER BY CREATED_AT DESC, LOG_ID DESC"));
    }

    #[test]
    fn session_activity_details_reject_invalid_json() {
        assert_eq!(
            parse_session_activity_details("log-1", None).unwrap(),
            serde_json::Value::Null
        );

        let parsed = parse_session_activity_details("log-1", Some(r#"{"ok":true}"#.into()))
            .expect("valid audit details JSON");
        assert_eq!(parsed["ok"], serde_json::Value::Bool(true));

        let error = parse_session_activity_details("log-1", Some("{bad".into()))
            .expect_err("invalid audit details JSON should be rejected");
        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            error
                .1
                .0
                .detail
                .contains("invalid auth session activity details JSON"),
            "unexpected error: {:?}",
            error.1.0
        );
    }

    #[test]
    fn metadata_patch_preserves_unrelated_keys_and_deletes_null_values() {
        let mut metadata = serde_json::Map::from_iter([
            ("current_model".to_string(), serde_json::json!("qwen3")),
            (
                "workspace_selection".to_string(),
                serde_json::json!({"kind": "server_sandbox"}),
            ),
        ]);
        let patch = serde_json::Map::from_iter([
            ("current_model".to_string(), serde_json::json!("gpt-5.6")),
            ("workspace_selection".to_string(), serde_json::Value::Null),
        ]);

        apply_metadata_patch(&mut metadata, patch);

        assert_eq!(
            metadata.get("current_model"),
            Some(&serde_json::json!("gpt-5.6"))
        );
        assert!(!metadata.contains_key("workspace_selection"));
    }
}
