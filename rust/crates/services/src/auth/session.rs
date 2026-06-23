use crate::pagination::{MAX_API_LIST_OFFSET, clamp_api_list_pagination};
use crate::storage::{log_session_audit, session_record_from_row};
use astra_core::{ErrorResponse, MatrixOneSettings, SharedPool, error_response, internal_error};
use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use chrono::Utc;
use sqlx::{MySql, QueryBuilder, Row, query};
use uuid::Uuid;

const MAX_SESSION_ACTIVITY_ROWS: u32 = 200;
const SESSION_DELETE_OWNER_MISMATCH_PREFIX: &str = "session_delete_owner_mismatch:";

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
        offset: u32,
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
    pub status: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionListFilter {
    pub user_id: String,
    pub agent_id: Option<String>,
    pub status: Option<String>,
    pub limit: u32,
    pub offset: u32,
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
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
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
}

#[derive(Clone, Debug)]
pub struct DatabaseSessionService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
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
        mut filter: SessionListFilter,
    ) -> Result<SessionListRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        (filter.limit, filter.offset) = clamp_api_list_pagination(filter.limit, filter.offset);

        let mut count_query = QueryBuilder::<MySql>::new(
            "SELECT COUNT(session_id) AS total FROM agent_sessions WHERE user_id = ",
        );
        count_query.push_bind(&filter.user_id);
        if let Some(agent_id) = &filter.agent_id {
            count_query.push(" AND agent_id = ");
            count_query.push_bind(agent_id);
        }
        if let Some(status) = &filter.status {
            count_query.push(" AND status = ");
            count_query.push_bind(status);
        }

        let total_row = count_query
            .build()
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;
        let total = total_row.try_get::<i64, _>("total").unwrap_or(0);

        let mut list_query = QueryBuilder::<MySql>::new(
            "SELECT session_id, user_id, agent_id, title, status, event_count, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at, \
             DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s') AS updated_at, \
             DATE_FORMAT(ended_at, '%Y-%m-%dT%H:%i:%s') AS ended_at, \
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
        list_query.push(" ORDER BY updated_at DESC LIMIT ");
        list_query.push_bind(i64::from(filter.limit));
        list_query.push(" OFFSET ");
        list_query.push_bind(i64::from(filter.offset));

        let rows = list_query
            .build()
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;
        let mut sessions = Vec::with_capacity(rows.len());
        for row in rows {
            sessions.push(session_record_from_row(row)?);
        }

        Ok(SessionListRecord {
            sessions,
            total,
            limit: filter.limit,
            offset: filter.offset,
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
            status,
        } = request;

        if title.is_none() && metadata.is_none() && status.is_none() {
            return Ok(existing);
        }

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

        let rows_affected = update_query
            .build()
            .execute(&pool)
            .await
            .map_err(internal_error)?
            .rows_affected();
        if rows_affected == 0 {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                format!("Session {session_id} 不存在"),
            ));
        }

        let updated = self
            .fetch_session_for_user(&pool, &session_id, &user_id)
            .await?
            .ok_or_else(|| internal_error("failed to read updated session"))?;
        let details = serde_json::json!({
            "title": title,
            "status": status,
            "metadata": metadata,
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
        let existing = self
            .fetch_session_for_user(&pool, &session_id, &user_id)
            .await?
            .ok_or_else(|| {
                error_response(
                    StatusCode::NOT_FOUND,
                    format!("Session {session_id} 不存在"),
                )
            })?;

        let mut tx = pool.begin().await.map_err(internal_error)?;
        hard_delete_session_rows(&mut tx, &session_id, &user_id)
            .await
            .map_err(map_hard_delete_session_error)?;
        tx.commit().await.map_err(internal_error)?;

        let details = serde_json::json!({ "title": existing.title });
        log_session_audit(&pool, &user_id, "session_delete", &session_id, details).await;
        Ok(())
    }

    async fn get_session_activity(
        &self,
        session_id: String,
        user_id: String,
        limit: u32,
        offset: u32,
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
        let limit = limit.min(MAX_SESSION_ACTIVITY_ROWS);
        let offset = offset.min(MAX_API_LIST_OFFSET);

        let count_row = query(
            "SELECT COUNT(*) as cnt FROM auth_audit_logs \
             WHERE resource_type = 'session' AND resource_id = ?",
        )
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;
        let total: i64 = count_row.try_get("cnt").unwrap_or(0);

        let rows = query(
            "SELECT log_id, action, details, created_at FROM auth_audit_logs \
             WHERE resource_type = 'session' AND resource_id = ? \
             ORDER BY created_at DESC LIMIT ? OFFSET ?",
        )
        .bind(&session_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let activities = rows
            .iter()
            .map(|row| {
                let details_str: String = row.try_get("details").unwrap_or_default();
                let details = serde_json::from_str(&details_str).unwrap_or(serde_json::Value::Null);
                SessionActivityEntryRecord {
                    log_id: row.try_get("log_id").unwrap_or_default(),
                    action: row.try_get("action").unwrap_or_default(),
                    details,
                    created_at: row.try_get::<String, _>("created_at").unwrap_or_default(),
                }
            })
            .collect();

        Ok(SessionActivityRecord {
            session_id,
            activities,
            total,
        })
    }
}

async fn delete_session_rows_1(
    tx: &mut sqlx::Transaction<'_, MySql>,
    label: &'static str,
    statement: &'static str,
    session_id: &str,
) -> Result<u64, String> {
    query(statement)
        .bind(session_id)
        .execute(&mut **tx)
        .await
        .map(|result| result.rows_affected())
        .map_err(|source| format!("delete_session.{label}: {source}"))
}

async fn delete_session_rows_session_user(
    tx: &mut sqlx::Transaction<'_, MySql>,
    label: &'static str,
    statement: &'static str,
    session_id: &str,
    user_id: &str,
) -> Result<u64, String> {
    query(statement)
        .bind(session_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await
        .map(|result| result.rows_affected())
        .map_err(|source| format!("delete_session.{label}: {source}"))
}

async fn delete_session_rows_session_user_twice(
    tx: &mut sqlx::Transaction<'_, MySql>,
    label: &'static str,
    statement: &'static str,
    session_id: &str,
    user_id: &str,
) -> Result<u64, String> {
    query(statement)
        .bind(session_id)
        .bind(user_id)
        .bind(session_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await
        .map(|result| result.rows_affected())
        .map_err(|source| format!("delete_session.{label}: {source}"))
}

async fn ensure_session_delete_owner_consistency(
    tx: &mut sqlx::Transaction<'_, MySql>,
    session_id: &str,
    user_id: &str,
) -> Result<(), String> {
    for (label, statement) in [
        (
            "agent_events",
            "SELECT COUNT(*) AS c FROM agent_events WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "agent_runs",
            "SELECT COUNT(*) AS c FROM agent_runs WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "agent_run_events",
            "SELECT COUNT(*) AS c FROM agent_run_events WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "run_checkpoints",
            "SELECT COUNT(*) AS c FROM run_checkpoints WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "run_display_projections",
            "SELECT COUNT(*) AS c FROM run_display_projections WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "session_tool_output_batches",
            "SELECT COUNT(*) AS c FROM session_tool_output_batches WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "session_tool_outputs",
            "SELECT COUNT(*) AS c FROM session_tool_outputs WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "session_device_lease_events",
            "SELECT COUNT(*) AS c FROM session_device_lease_events WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "session_device_leases",
            "SELECT COUNT(*) AS c FROM session_device_leases WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "session_transcript_items",
            "SELECT COUNT(*) AS c FROM session_transcript_items WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "transcript_pages",
            "SELECT COUNT(*) AS c FROM transcript_pages WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "prompt_request_records",
            "SELECT COUNT(*) AS c FROM prompt_request_records WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "session_state_revisions",
            "SELECT COUNT(*) AS c FROM session_state_revisions WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "context_manifests",
            "SELECT COUNT(*) AS c FROM context_manifests WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "session_artifacts_grants",
            "SELECT COUNT(*) AS c FROM session_artifacts_grants WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "session_artifacts",
            "SELECT COUNT(*) AS c FROM session_artifacts WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "session_state_item_events",
            "SELECT COUNT(*) AS c FROM session_state_item_events WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "session_state_items",
            "SELECT COUNT(*) AS c FROM session_state_items WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "session_delegations",
            "SELECT COUNT(*) AS c FROM session_delegations WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "session_plan_todos",
            "SELECT COUNT(*) AS c FROM session_plan_todos WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "session_history_chunks",
            "SELECT COUNT(*) AS c FROM session_history_chunks WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "harness_snapshots",
            "SELECT COUNT(*) AS c FROM harness_snapshots WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "skill_selection_events",
            "SELECT COUNT(*) AS c FROM skill_selection_events WHERE session_id = ? AND COALESCE(user_id, '') <> ?",
        ),
        (
            "session_sync_log",
            "SELECT COUNT(*) AS c FROM session_sync_log WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "agent_tasks",
            "SELECT COUNT(*) AS c FROM agent_tasks WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "plans",
            "SELECT COUNT(*) AS c FROM plans WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "session_checkpoints",
            "SELECT COUNT(*) AS c FROM session_checkpoints WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "session_todos",
            "SELECT COUNT(*) AS c FROM session_todos WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "task_contracts",
            "SELECT COUNT(*) AS c FROM task_contracts WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "skill_installations",
            "SELECT COUNT(*) AS c FROM skill_installations WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "wf_triggers",
            "SELECT COUNT(*) AS c FROM wf_triggers WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "eval_user_feedback",
            "SELECT COUNT(*) AS c FROM eval_user_feedback WHERE session_id = ? AND user_id <> ?",
        ),
        (
            "team_snapshots",
            "SELECT COUNT(*) AS c FROM team_snapshots WHERE session_id = ? AND user_id <> ?",
        ),
    ] {
        let row = query(statement)
            .bind(session_id)
            .bind(user_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(|source| format!("delete_session.{label}.owner_check: {source}"))?;
        let mismatches = row.try_get::<i64, _>("c").unwrap_or(0);
        if mismatches > 0 {
            return Err(format!(
                "{SESSION_DELETE_OWNER_MISMATCH_PREFIX}{label}:{mismatches}"
            ));
        }
    }
    Ok(())
}

async fn hard_delete_session_rows(
    tx: &mut sqlx::Transaction<'_, MySql>,
    session_id: &str,
    user_id: &str,
) -> Result<u64, String> {
    let mut deleted = 0_u64;
    ensure_session_delete_owner_consistency(tx, session_id, user_id).await?;

    for (label, statement) in [(
        "user_skill_evaluations",
        "DELETE FROM user_skill_evaluations
             WHERE run_id IN (SELECT run_id FROM agent_runs WHERE session_id = ? AND user_id = ?)",
    )] {
        deleted +=
            delete_session_rows_session_user(tx, label, statement, session_id, user_id).await?;
    }

    deleted += query(
        "DELETE FROM agent_event_edges
         WHERE child_event_id IN (SELECT event_id FROM agent_events WHERE session_id = ? AND user_id = ?)
            OR parent_event_id IN (SELECT event_id FROM agent_events WHERE session_id = ? AND user_id = ?)",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(session_id)
    .bind(user_id)
    .execute(&mut **tx)
    .await
    .map(|result| result.rows_affected())
    .map_err(|source| format!("delete_session.agent_event_edges: {source}"))?;

    for (label, statement) in [
        (
            "session_state_item_events",
            "DELETE FROM session_state_item_events
             WHERE (session_id = ? AND user_id = ?)
                OR item_id IN (
                    SELECT item_id FROM session_state_items
                    WHERE origin_session_id = ? AND user_id = ?
                )",
        ),
        (
            "session_state_items",
            "DELETE FROM session_state_items
             WHERE (session_id = ? AND user_id = ?)
                OR (origin_session_id = ? AND user_id = ?)",
        ),
        (
            "session_history_chunks",
            "DELETE FROM session_history_chunks
             WHERE (session_id = ? AND user_id = ?)
                OR (source_session_id = ? AND user_id = ?)",
        ),
    ] {
        deleted +=
            delete_session_rows_session_user_twice(tx, label, statement, session_id, user_id)
                .await?;
    }

    for (label, statement) in [
        (
            "context_manifest_items",
            "DELETE FROM context_manifest_items
             WHERE manifest_id IN (
                 SELECT manifest_id FROM context_manifests
                 WHERE session_id = ? AND user_id = ?
             )",
        ),
        (
            "prompt_deltas",
            "DELETE FROM prompt_deltas
             WHERE request_id IN (
                 SELECT request_id FROM prompt_request_records
                 WHERE session_id = ? AND user_id = ?
             )",
        ),
        (
            "plan_step_runs",
            "DELETE FROM plan_step_runs
             WHERE plan_id IN (
                 SELECT plan_id FROM plans
                 WHERE session_id = ? AND user_id = ?
             )",
        ),
        (
            "task_verification_results",
            "DELETE FROM task_verification_results
             WHERE contract_id IN (
                 SELECT contract_id FROM task_contracts
                 WHERE session_id = ? AND user_id = ?
             )",
        ),
    ] {
        deleted +=
            delete_session_rows_session_user(tx, label, statement, session_id, user_id).await?;
    }

    for (label, statement) in [
        (
            "context_manifests",
            "DELETE FROM context_manifests WHERE session_id = ? AND user_id = ?",
        ),
        (
            "session_artifacts_grants",
            "DELETE FROM session_artifacts_grants WHERE session_id = ? AND user_id = ?",
        ),
        (
            "session_artifacts",
            "DELETE FROM session_artifacts WHERE session_id = ? AND user_id = ?",
        ),
        (
            "session_tool_outputs",
            "DELETE FROM session_tool_outputs WHERE session_id = ? AND user_id = ?",
        ),
        (
            "session_tool_output_batches",
            "DELETE FROM session_tool_output_batches WHERE session_id = ? AND user_id = ?",
        ),
        (
            "session_device_lease_events",
            "DELETE FROM session_device_lease_events WHERE session_id = ? AND user_id = ?",
        ),
        (
            "session_device_leases",
            "DELETE FROM session_device_leases WHERE session_id = ? AND user_id = ?",
        ),
        (
            "session_transcript_items",
            "DELETE FROM session_transcript_items WHERE session_id = ? AND user_id = ?",
        ),
        (
            "transcript_pages",
            "DELETE FROM transcript_pages WHERE session_id = ? AND user_id = ?",
        ),
        (
            "prompt_request_records",
            "DELETE FROM prompt_request_records WHERE session_id = ? AND user_id = ?",
        ),
        (
            "session_state_revisions",
            "DELETE FROM session_state_revisions WHERE session_id = ? AND user_id = ?",
        ),
        (
            "session_delegations",
            "DELETE FROM session_delegations WHERE session_id = ? AND user_id = ?",
        ),
        (
            "session_todos",
            "DELETE FROM session_todos WHERE session_id = ? AND user_id = ?",
        ),
        (
            "session_plan_todos",
            "DELETE FROM session_plan_todos WHERE session_id = ? AND user_id = ?",
        ),
        (
            "harness_snapshots",
            "DELETE FROM harness_snapshots WHERE session_id = ? AND user_id = ?",
        ),
        (
            "skill_selection_events",
            "DELETE FROM skill_selection_events WHERE session_id = ? AND user_id = ?",
        ),
        // skill_selector_turn_metrics — table was removed in PR #337
        // (tool selector subsystem deletion); the cleanup statement
        // remained and started failing with "no such table" once a
        // session deletion ran against a fresh schema. Drop the
        // stale cleanup entry.
        (
            "session_sync_log",
            "DELETE FROM session_sync_log WHERE session_id = ? AND user_id = ?",
        ),
        (
            "agent_tasks",
            "DELETE FROM agent_tasks WHERE session_id = ? AND user_id = ?",
        ),
        (
            "plans",
            "DELETE FROM plans WHERE session_id = ? AND user_id = ?",
        ),
        (
            "session_checkpoints",
            "DELETE FROM session_checkpoints WHERE session_id = ? AND user_id = ?",
        ),
        (
            "task_contracts",
            "DELETE FROM task_contracts WHERE session_id = ? AND user_id = ?",
        ),
        (
            "skill_installations",
            "DELETE FROM skill_installations WHERE session_id = ? AND user_id = ?",
        ),
        (
            "wf_triggers",
            "DELETE FROM wf_triggers WHERE session_id = ? AND user_id = ?",
        ),
        (
            "eval_user_feedback",
            "DELETE FROM eval_user_feedback WHERE session_id = ? AND user_id = ?",
        ),
        (
            "team_snapshots",
            "DELETE FROM team_snapshots WHERE session_id = ? AND user_id = ?",
        ),
    ] {
        deleted +=
            delete_session_rows_session_user(tx, label, statement, session_id, user_id).await?;
    }

    for (label, statement) in [
        (
            "ctx_snapshots",
            "DELETE FROM ctx_snapshots WHERE session_id = ?",
        ),
        (
            "ctx_decision_audits",
            "DELETE FROM ctx_decision_audits WHERE session_id = ?",
        ),
        (
            "session_todo_counters",
            "DELETE FROM session_todo_counters WHERE session_id = ?",
        ),
        (
            "conversation_log",
            "DELETE FROM conversation_log WHERE session_id = ?",
        ),
    ] {
        deleted += delete_session_rows_1(tx, label, statement, session_id).await?;
    }

    for (label, statement) in [
        (
            "agent_run_events",
            "DELETE FROM agent_run_events WHERE session_id = ? AND user_id = ?",
        ),
        (
            "run_checkpoints",
            "DELETE FROM run_checkpoints WHERE session_id = ? AND user_id = ?",
        ),
        (
            "run_display_projections",
            "DELETE FROM run_display_projections WHERE session_id = ? AND user_id = ?",
        ),
        (
            "agent_runs",
            "DELETE FROM agent_runs WHERE session_id = ? AND user_id = ?",
        ),
        (
            "agent_events",
            "DELETE FROM agent_events WHERE session_id = ? AND user_id = ?",
        ),
        (
            "agent_sessions",
            "DELETE FROM agent_sessions WHERE session_id = ? AND user_id = ?",
        ),
    ] {
        deleted +=
            delete_session_rows_session_user(tx, label, statement, session_id, user_id).await?;
    }

    Ok(deleted)
}

fn map_hard_delete_session_error(error: String) -> (StatusCode, Json<ErrorResponse>) {
    if error.starts_with(SESSION_DELETE_OWNER_MISMATCH_PREFIX) {
        return error_response(
            StatusCode::CONFLICT,
            "Session ownership is inconsistent; deletion blocked",
        );
    }
    internal_error(error)
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
        _offset: u32,
    ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Session service not configured",
        ))
    }
}
