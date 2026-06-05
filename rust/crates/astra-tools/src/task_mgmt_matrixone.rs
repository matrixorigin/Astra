//! MatrixOne-backed [`TaskStore`] for the session scratchpad.
//!
//! Authoritative store for the Tier 1 task board, per Plan
//! `docs/plans/task-system-design.md` §2.1. Each edge and cloud loop host
//! reads/writes the same `session_todos` rows for a given `session_id`, so a
//! task created on the CLI is immediately visible to a cloud-resumed turn.
//!
//! # Sizing assumption
//!
//! [`TASK_SOFT_CAP`] encodes the Tier 1 "dozens of rows" budget as a concrete
//! number. Callers should treat a failing `debug_assert!(len < TASK_SOFT_CAP)`
//! in [`MatrixOneTaskStore::save`] as a signal to revisit the
//! delete-all / insert-all strategy in favour of incremental upserts.
//!
//! This impl uses a simple **load-all / replace-all** strategy that matches
//! the [`TaskStore`] trait semantics: a session's todo vec is small (dozens
//! of rows at most), so a full rewrite on every mutation keeps the MO access
//! pattern and the in-memory logic in perfect sync with no partial-update
//! complexity. CLAUDE.md §5 compliance: no `WHERE` on JSON columns; every
//! read is column-scoped; single (session_id, status, updated_at) index
//! covers the only non-PK query the manager makes (not used yet but kept
//! for future list-filtering).

use astra_core::sqlx::{self, MySql, MySqlConnection, Pool, QueryBuilder, Row};
use async_trait::async_trait;
use serde_json::{Value, json};

use std::sync::Arc;

use crate::task_mgmt::{
    InMemoryTaskStore, SESSION_TASK_STATUS_ARCHIVED, SESSION_TASK_STATUS_CANCELLED,
    SESSION_TASK_STATUS_COMPLETED, SESSION_TASK_STATUS_FAILED, SESSION_TASK_STATUS_IN_PROGRESS,
    SESSION_TASK_STATUS_PENDING, SessionSubtask, SessionTask, SessionTaskStatusKind, TaskMutation,
    TaskStore, prefix_summary,
};

/// Soft cap on the number of rows a single `session_todos` full-replace
/// is expected to handle. Above this the delete-all / insert-all strategy
/// stops being cheap and callers should migrate to incremental upserts.
///
/// Tier 1 plan assumption: a session holds "dozens of rows"; 256 leaves
/// ample headroom before the debug_assert in [`MatrixOneTaskStore::save`]
/// trips.
pub const TASK_SOFT_CAP: usize = 256;
const INSERT_BATCH_ROWS: usize = 100;

const EXHAUSTED_COUNTER_SENTINEL: u64 = u32::MAX as u64 + 1;

// MatrixOne rejects `LAST_INSERT_ID(expr)` with error 20203 "invalid
// argument function last_insert_id, bad value" — in ALL positions, not
// just VALUES. So the MySQL "stash pre-increment via LAST_INSERT_ID"
// idiom is unusable on MO. Instead we do SELECT…FOR UPDATE inside an
// explicit transaction: the row lock gives us the same atomicity the
// MySQL idiom relied on, at the cost of a second round-trip per call.
//
// See `next_task_id` below for the orchestration.

fn counter_bind_value(next: u32) -> i64 {
    i64::from(next)
}

struct EncodedTaskJsonFields {
    metadata: Option<String>,
    blocks: Option<String>,
    blocked_by: Option<String>,
    subtasks: Option<String>,
}

fn encode_task_json_fields(task: &SessionTask) -> EncodedTaskJsonFields {
    EncodedTaskJsonFields {
        metadata: task
            .metadata
            .as_ref()
            .map(|metadata| serde_json::to_string(metadata).unwrap_or_else(|_| "{}".into())),
        blocks: (!task.blocks.is_empty())
            .then(|| serde_json::to_string(&task.blocks).unwrap_or_else(|_| "[]".into())),
        blocked_by: (!task.blocked_by.is_empty())
            .then(|| serde_json::to_string(&task.blocked_by).unwrap_or_else(|_| "[]".into())),
        subtasks: (!task.subtasks.is_empty())
            .then(|| serde_json::to_string(&task.subtasks).unwrap_or_else(|_| "[]".into())),
    }
}

async fn insert_session_tasks(
    executor: &mut MySqlConnection,
    session_id: &str,
    user_id: &str,
    tasks: &[SessionTask],
) -> Result<(), String> {
    if tasks.is_empty() {
        return Ok(());
    }

    for (start, end) in task_insert_batch_ranges(tasks.len()) {
        let mut builder = QueryBuilder::<MySql>::new(
            "INSERT INTO session_todos (\
                session_id, todo_id, user_id, ordinal, title, description, active_form, \
                status, owner, metadata, blocks, blocked_by, subtasks, archived_at, \
                created_at, updated_at) ",
        );
        builder.push_values(
            tasks[start..end].iter().enumerate(),
            |mut row, (offset, task)| {
                let encoded = encode_task_json_fields(task);
                let archived_at = (task.status == SessionTaskStatusKind::Archived)
                    .then(|| to_mo_datetime(&task.updated_at));
                let status_str = task.status.to_string();
                row.push_bind(session_id)
                    .push_bind(&task.id)
                    .push_bind(user_id)
                    .push_bind((start + offset) as i32)
                    .push_bind(&task.title)
                    .push_bind(&task.description)
                    .push_bind(&task.active_form)
                    .push_bind(status_str)
                    .push_bind(&task.owner)
                    .push_bind(encoded.metadata)
                    .push_bind(encoded.blocks)
                    .push_bind(encoded.blocked_by)
                    .push_bind(encoded.subtasks)
                    .push_bind(archived_at)
                    .push_bind(to_mo_datetime(&task.created_at))
                    .push_bind(to_mo_datetime(&task.updated_at));
            },
        );
        builder
            .build()
            .execute(&mut *executor)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn task_insert_batch_ranges(total: usize) -> Vec<(usize, usize)> {
    (0..total)
        .step_by(INSERT_BATCH_ROWS)
        .map(|start| (start, (start + INSERT_BATCH_ROWS).min(total)))
        .collect()
}

fn counter_after_allocation(current: u32) -> u64 {
    u64::from(current) + 1
}

fn allocated_task_id_from_counter(raw: u64, session_id: &str) -> Result<u32, String> {
    u32::try_from(raw)
        .map_err(|_| format!("session_todo_counters.next_id overflow for {session_id}"))
}

fn locked_counter_advance(raw: i64, session_id: &str) -> Result<(u32, u64), String> {
    if raw <= 0 {
        return Err(format!(
            "session_todo_counters.next_id out of range for {session_id}: {raw}"
        ));
    }
    let current = allocated_task_id_from_counter(raw as u64, session_id)?;
    let next_stored = counter_after_allocation(current);
    if next_stored > EXHAUSTED_COUNTER_SENTINEL {
        return Err(format!("session_todo_counters exhausted for {session_id}"));
    }
    Ok((current, next_stored))
}

/// Convert an RFC3339 timestamp (what `TaskManager` writes) into the
/// `YYYY-MM-DD HH:MM:SS.ffffff` form MatrixOne accepts for `DATETIME(6)`
/// columns. Falls back to stripping the `T` + timezone offset if parsing
/// fails, so callers can recover from legacy-format inputs without panicking.
fn to_mo_datetime(rfc3339: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(rfc3339) {
        return dt
            .with_timezone(&chrono::Utc)
            .format("%Y-%m-%d %H:%M:%S%.6f")
            .to_string();
    }
    rfc3339
        .replacen('T', " ", 1)
        .split(&['+', 'Z'][..])
        .next()
        .unwrap_or(rfc3339)
        .to_string()
}

/// Pick the right [`TaskStore`] for this process: MatrixOne when a pool is
/// configured, in-memory otherwise. Call once per process (or per
/// host-binding lifecycle) — the returned store is safe to share across
/// sessions; each `TaskManager` scopes reads/writes by `session_id`.
pub fn select_task_store(
    pool: Option<astra_core::sqlx::Pool<astra_core::sqlx::MySql>>,
) -> Arc<dyn TaskStore> {
    match pool {
        Some(pool) => Arc::new(MatrixOneTaskStore::new(pool)),
        None => Arc::new(InMemoryTaskStore::new()),
    }
}

/// MatrixOne-backed task store. See module docs.
pub struct MatrixOneTaskStore {
    pool: Pool<MySql>,
    changed_tx: tokio::sync::broadcast::Sender<String>,
    /// User who owns rows written through this store. Threaded into
    /// every INSERT so cross-session user-scoped queries
    /// (`idx_session_todos_user_status_updated`) can find them
    /// without a join. Empty string means the store is
    /// "anonymous" — only test paths should construct that shape.
    user_id: String,
}

impl MatrixOneTaskStore {
    pub fn new(pool: Pool<MySql>) -> Self {
        Self {
            pool,
            changed_tx: tokio::sync::broadcast::channel(16).0,
            user_id: String::new(),
        }
    }

    pub fn from_shared(shared: &astra_core::SharedPool) -> Self {
        Self::new(shared.get().clone())
    }

    /// Bind the user that owns subsequent writes. Must be set before
    /// any `save()` / `mutate()` so the user_id column has a real
    /// owner for cross-session queries.
    pub fn with_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = user_id.into();
        self
    }

    async fn load_rows(&self, session_id: &str) -> Result<Vec<SessionTask>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT todo_id, title, description, active_form, status, owner, \
                    metadata, blocks, blocked_by, subtasks, \
                    CAST(created_at AS CHAR) AS created_at, \
                    CAST(updated_at AS CHAR) AS updated_at \
             FROM session_todos \
             WHERE session_id = ? \
             ORDER BY ordinal ASC",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        let mut tasks = Vec::with_capacity(rows.len());
        for row in rows {
            tasks.push(row_to_task(&row)?);
        }
        Ok(tasks)
    }
}

fn row_to_task(row: &sqlx::mysql::MySqlRow) -> Result<SessionTask, sqlx::Error> {
    let id: String = row.try_get("todo_id")?;
    let title: String = row.try_get("title")?;
    let description: Option<String> = row.try_get("description").ok().flatten();
    let active_form: Option<String> = row.try_get("active_form").ok().flatten();
    let status_str: String = row.try_get("status")?;
    let status = SessionTaskStatusKind::from_status_str(&status_str);
    let owner: Option<String> = row.try_get("owner").ok().flatten();
    let metadata: Option<serde_json::Map<String, serde_json::Value>> = row
        .try_get::<Option<String>, _>("metadata")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());
    let blocks: Vec<String> = row
        .try_get::<Option<String>, _>("blocks")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let blocked_by: Vec<String> = row
        .try_get::<Option<String>, _>("blocked_by")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let subtasks: Vec<SessionSubtask> = row
        .try_get::<Option<String>, _>("subtasks")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    // `created_at` / `updated_at` are NOT NULL DATETIME(6) columns cast to
    // CHAR in the SELECT. A NULL here means schema drift (column relaxed to
    // nullable) or a bad cast — surface it instead of silently letting an
    // empty string flow back through `to_mo_datetime` on the next save and
    // triggering the legacy-format fallback branch.
    let created_at: String = row
        .try_get::<Option<String>, _>("created_at")?
        .ok_or_else(|| sqlx::Error::Decode("session_todos.created_at is NULL".into()))?;
    let updated_at: String = row
        .try_get::<Option<String>, _>("updated_at")?
        .ok_or_else(|| sqlx::Error::Decode("session_todos.updated_at is NULL".into()))?;

    Ok(SessionTask {
        id,
        title,
        description,
        status,
        subtasks,
        created_at,
        updated_at,
        active_form,
        owner,
        metadata,
        blocks,
        blocked_by,
    })
}

#[async_trait]
impl TaskStore for MatrixOneTaskStore {
    async fn load(&self, session_id: &str) -> Result<Vec<SessionTask>, String> {
        self.load_rows(session_id).await.map_err(|e| e.to_string())
    }

    /// U-8: SQL-pushdown path for active-only queries.
    /// Uses `idx_session_todos_session_status_updated` so only matching
    /// rows are returned instead of shipping the whole table to Rust.
    async fn load_active(&self, session_id: &str) -> Result<Vec<SessionTask>, String> {
        let rows = sqlx::query(
            "SELECT todo_id, title, description, active_form, status, owner, \
                    metadata, blocks, blocked_by, subtasks, \
                    CAST(created_at AS CHAR) AS created_at, \
                    CAST(updated_at AS CHAR) AS updated_at \
             FROM session_todos \
             WHERE session_id = ? AND status IN (?, ?) \
             ORDER BY ordinal ASC",
        )
        .bind(session_id)
        .bind(SESSION_TASK_STATUS_PENDING.to_string())
        .bind(SESSION_TASK_STATUS_IN_PROGRESS.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;

        let mut tasks = Vec::with_capacity(rows.len());
        for row in rows {
            tasks.push(row_to_task(&row).map_err(|e| e.to_string())?);
        }
        Ok(tasks)
    }

    async fn archive(&self, session_id: &str, args: &Value) -> Result<String, String> {
        if let Some(task_id) = args.get("task_id").and_then(Value::as_str) {
            let status_row: Option<(String,)> = if self.user_id.is_empty() {
                sqlx::query_as(
                    "SELECT status FROM session_todos \
                     WHERE session_id = ? AND todo_id = ? LIMIT 1",
                )
                .bind(session_id)
                .bind(task_id)
                .fetch_optional(&self.pool)
                .await
            } else {
                sqlx::query_as(
                    "SELECT status FROM session_todos \
                     WHERE user_id = ? AND session_id = ? AND todo_id = ? LIMIT 1",
                )
                .bind(&self.user_id)
                .bind(session_id)
                .bind(task_id)
                .fetch_optional(&self.pool)
                .await
            }
            .map_err(|e| e.to_string())?;

            let Some((status,)) = status_row else {
                return Ok(prefix_summary(
                    format!("Refused: task #{task_id} not found in session {session_id}"),
                    json!({
                        "success": false,
                        "task_id": task_id,
                        "session_id": session_id,
                        "message": format!(
                            "Task '{}' was not found in session '{}'",
                            task_id, session_id
                        ),
                    })
                    .to_string(),
                ));
            };
            let status_kind = SessionTaskStatusKind::from_status_str(&status);
            if status_kind == SessionTaskStatusKind::Archived {
                return Ok(prefix_summary(
                    format!("Refused: task #{task_id} is already archived"),
                    json!({
                        "success": false,
                        "task_id": task_id,
                        "session_id": session_id,
                        "previous_status": status,
                        "message": format!("Task '{}' is already archived", task_id),
                    })
                    .to_string(),
                ));
            }
            if !status_kind.can_be_archived() {
                return Ok(prefix_summary(
                    format!(
                        "Refused: task #{task_id} is '{status}' — only completed, failed, or cancelled tasks can be archived"
                    ),
                    json!({
                        "success": false,
                        "task_id": task_id,
                        "session_id": session_id,
                        "previous_status": status,
                        "message": format!(
                            "Task '{}' must be completed, failed, or cancelled before it can be archived",
                            task_id
                        ),
                    })
                    .to_string(),
                ));
            }

            let result = if self.user_id.is_empty() {
                sqlx::query(
                    "UPDATE session_todos \
                     SET status = ?, archived_at = NOW(6), updated_at = NOW(6) \
                     WHERE session_id = ? AND todo_id = ? \
                       AND status IN (?, ?, ?)",
                )
                .bind(SESSION_TASK_STATUS_ARCHIVED.to_string())
                .bind(session_id)
                .bind(task_id)
                .bind(SESSION_TASK_STATUS_COMPLETED.to_string())
                .bind(SESSION_TASK_STATUS_FAILED.to_string())
                .bind(SESSION_TASK_STATUS_CANCELLED.to_string())
                .execute(&self.pool)
                .await
            } else {
                sqlx::query(
                    "UPDATE session_todos \
                     SET status = ?, archived_at = NOW(6), updated_at = NOW(6) \
                     WHERE user_id = ? AND session_id = ? AND todo_id = ? \
                       AND status IN (?, ?, ?)",
                )
                .bind(SESSION_TASK_STATUS_ARCHIVED.to_string())
                .bind(&self.user_id)
                .bind(session_id)
                .bind(task_id)
                .bind(SESSION_TASK_STATUS_COMPLETED.to_string())
                .bind(SESSION_TASK_STATUS_FAILED.to_string())
                .bind(SESSION_TASK_STATUS_CANCELLED.to_string())
                .execute(&self.pool)
                .await
            }
            .map_err(|e| e.to_string())?;

            if result.rows_affected() == 0 {
                return Ok(prefix_summary(
                    format!(
                        "Refused: task #{task_id} changed before it could be archived; reload and try again"
                    ),
                    json!({
                        "success": false,
                        "task_id": task_id,
                        "session_id": session_id,
                        "message": format!(
                            "Task '{}' changed before archive could be applied; reload and try again",
                            task_id
                        ),
                    })
                    .to_string(),
                ));
            }

            return Ok(prefix_summary(
                format!("Archived task #{task_id} (was {status})"),
                json!({
                    "success": true,
                    "task_id": task_id,
                    "session_id": session_id,
                    "previous_status": status,
                    "status": SESSION_TASK_STATUS_ARCHIVED,
                    "message": format!("Task '{}' archived", task_id),
                })
                .to_string(),
            ));
        }

        let days_raw = args
            .get("older_than_days")
            .and_then(Value::as_u64)
            .unwrap_or(30);
        let days = i64::try_from(days_raw)
            .map_err(|_| format!("older_than_days is too large: {days_raw}"))?;
        let result = if self.user_id.is_empty() {
            sqlx::query(
                "UPDATE session_todos \
                 SET status = ?, archived_at = NOW(6), updated_at = NOW(6) \
                 WHERE session_id = ? AND status = ? \
                   AND updated_at < DATE_SUB(NOW(6), INTERVAL ? DAY)",
            )
            .bind(SESSION_TASK_STATUS_ARCHIVED.to_string())
            .bind(session_id)
            .bind(SESSION_TASK_STATUS_COMPLETED.to_string())
            .bind(days)
            .execute(&self.pool)
            .await
        } else {
            sqlx::query(
                "UPDATE session_todos \
                 SET status = ?, archived_at = NOW(6), updated_at = NOW(6) \
                 WHERE user_id = ? AND status = ? \
                   AND updated_at < DATE_SUB(NOW(6), INTERVAL ? DAY)",
            )
            .bind(SESSION_TASK_STATUS_ARCHIVED.to_string())
            .bind(&self.user_id)
            .bind(SESSION_TASK_STATUS_COMPLETED.to_string())
            .bind(days)
            .execute(&self.pool)
            .await
        }
        .map_err(|e| e.to_string())?;

        let scope = if self.user_id.is_empty() {
            format!("session {session_id}")
        } else {
            format!("user {}", self.user_id)
        };
        Ok(prefix_summary(
            format!(
                "Archived {} completed task(s) older than {days} days for {scope}",
                result.rows_affected()
            ),
            json!({
                "success": true,
                "archived": result.rows_affected(),
                "older_than_days": days,
                "scope": scope,
                "message": format!(
                    "Archived {} completed task(s) older than {} days for {}",
                    result.rows_affected(),
                    days,
                    scope
                ),
            })
            .to_string(),
        ))
    }

    async fn save(&self, session_id: &str, tasks: Vec<SessionTask>) -> Result<(), String> {
        // Full replace semantics: the caller computed the next state; we
        // atomically make the table match it. Transaction ensures readers
        // on the other node never see a partial update.
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;

        if let Err(e) = sqlx::query("DELETE FROM session_todos WHERE session_id = ?")
            .bind(session_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())
        {
            if let Err(rollback_err) = tx.rollback().await {
                return Err(format!("{e}; rollback failed: {rollback_err}"));
            }
            return Err(e);
        }

        // Full-replace is justified only while task counts stay small (per
        // module docs: "dozens of rows at most"). Break loudly in debug
        // builds if a session ever pushes past a soft cap so we catch the
        // design assumption breaking before it becomes a perf problem.
        debug_assert!(
            tasks.len() < TASK_SOFT_CAP,
            "session_todos full-replace exceeded soft cap ({} rows); revisit incremental upserts",
            tasks.len()
        );

        if let Err(e) = insert_session_tasks(&mut tx, session_id, &self.user_id, &tasks).await {
            if let Err(rollback_err) = tx.rollback().await {
                return Err(format!("{e}; rollback failed: {rollback_err}"));
            }
            return Err(e);
        }

        tx.commit().await.map_err(|e| e.to_string())?;
        let _ = self.changed_tx.send(session_id.to_string());
        Ok(())
    }

    async fn mutate(&self, session_id: &str, mutation: TaskMutation) -> Result<String, String> {
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;

        if let Err(e) = sqlx::query(
            "INSERT INTO session_todo_counters (session_id, next_id) VALUES (?, 1) \
             ON DUPLICATE KEY UPDATE next_id = next_id",
        )
        .bind(session_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())
        {
            if let Err(rollback_err) = tx.rollback().await {
                return Err(format!("{e}; rollback failed: {rollback_err}"));
            }
            return Err(e);
        }

        let raw_next: i64 = match sqlx::query_as::<_, (i64,)>(
            "SELECT next_id FROM session_todo_counters WHERE session_id = ? FOR UPDATE",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await
        .map(|(next,)| next)
        .map_err(|e| e.to_string())
        {
            Ok(next) => next,
            Err(e) => {
                if let Err(rollback_err) = tx.rollback().await {
                    return Err(format!("{e}; rollback failed: {rollback_err}"));
                }
                return Err(e);
            }
        };
        if raw_next <= 0 {
            let err = format!("session_todo_counters.next_id out of range for {session_id}");
            if let Err(rollback_err) = tx.rollback().await {
                return Err(format!("{err}; rollback failed: {rollback_err}"));
            }
            return Err(err);
        }
        let next = match allocated_task_id_from_counter(raw_next as u64, session_id) {
            Ok(next) => next,
            Err(e) => {
                if let Err(rollback_err) = tx.rollback().await {
                    return Err(format!("{e}; rollback failed: {rollback_err}"));
                }
                return Err(e);
            }
        };

        let rows = match sqlx::query(
            "SELECT todo_id, title, description, active_form, status, owner, \
                    metadata, blocks, blocked_by, subtasks, \
                    CAST(created_at AS CHAR) AS created_at, \
                    CAST(updated_at AS CHAR) AS updated_at \
             FROM session_todos \
             WHERE session_id = ? \
             ORDER BY ordinal ASC \
             FOR UPDATE",
        )
        .bind(session_id)
        .fetch_all(&mut *tx)
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                let err = e.to_string();
                if let Err(rollback_err) = tx.rollback().await {
                    return Err(format!("{err}; rollback failed: {rollback_err}"));
                }
                return Err(err);
            }
        };
        let mut tasks = Vec::with_capacity(rows.len());
        for row in rows {
            match row_to_task(&row).map_err(|e| e.to_string()) {
                Ok(task) => tasks.push(task),
                Err(e) => {
                    if let Err(rollback_err) = tx.rollback().await {
                        return Err(format!("{e}; rollback failed: {rollback_err}"));
                    }
                    return Err(e);
                }
            }
        }

        let result = match mutation(tasks, next) {
            Ok(result) => result,
            Err(e) => {
                if let Err(rollback_err) = tx.rollback().await {
                    return Err(format!("{e}; rollback failed: {rollback_err}"));
                }
                return Err(e);
            }
        };

        if let Some(next_task_id) = result.next_task_id
            && let Err(e) =
                sqlx::query("UPDATE session_todo_counters SET next_id = ? WHERE session_id = ?")
                    .bind(counter_bind_value(next_task_id))
                    .bind(session_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| e.to_string())
        {
            if let Err(rollback_err) = tx.rollback().await {
                return Err(format!("{e}; rollback failed: {rollback_err}"));
            }
            return Err(e);
        }

        if let Err(e) = sqlx::query("DELETE FROM session_todos WHERE session_id = ?")
            .bind(session_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())
        {
            if let Err(rollback_err) = tx.rollback().await {
                return Err(format!("{e}; rollback failed: {rollback_err}"));
            }
            return Err(e);
        }

        if let Err(e) =
            insert_session_tasks(&mut tx, session_id, &self.user_id, &result.tasks).await
        {
            if let Err(rollback_err) = tx.rollback().await {
                return Err(format!("{e}; rollback failed: {rollback_err}"));
            }
            return Err(e);
        }

        tx.commit().await.map_err(|e| e.to_string())?;
        let _ = self.changed_tx.send(session_id.to_string());
        Ok(result.response)
    }

    fn subscribe(&self) -> Option<tokio::sync::broadcast::Receiver<String>> {
        Some(self.changed_tx.subscribe())
    }

    async fn next_task_id(&self, session_id: &str) -> Result<u32, String> {
        // Atomic read-and-increment via `SELECT … FOR UPDATE` inside a
        // transaction. Two concurrent hosts (edge + cloud) hitting the
        // same session_id get serialised on the row lock — only one
        // sees `current` at a time, and the one that doesn't find a
        // row inserts it. The MySQL `LAST_INSERT_ID(expr)` idiom we
        // previously used is NOT supported by MatrixOne (err 20203);
        // the row lock gives us the same atomicity at the cost of one
        // extra round-trip.
        //
        // Semantics:
        //   - First call for a new session: no row → INSERT next_id=2,
        //     return 1 (the id we reserved).
        //   - Subsequent call: SELECT FOR UPDATE holds the row,
        //     UPDATE bumps next_id, return pre-increment value.
        //
        // Exhaustion: the counter column is BIGINT so allocating
        // u32::MAX persists u32::MAX + 1 as a non-wrapping exhausted
        // sentinel. Later callers reject that sentinel before returning
        // an id.
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;

        let existing: Option<(i64,)> = match sqlx::query_as(
            "SELECT next_id FROM session_todo_counters WHERE session_id = ? FOR UPDATE",
        )
        .bind(session_id)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(v) => v,
            Err(e) => {
                let err = e.to_string();
                if let Err(rollback_err) = tx.rollback().await {
                    return Err(format!("{err}; rollback failed: {rollback_err}"));
                }
                return Err(err);
            }
        };

        let current: u32 = match existing {
            None => {
                // First allocation: seed counter to 2 (reserves id 1).
                if let Err(e) = sqlx::query(
                    "INSERT INTO session_todo_counters (session_id, next_id) VALUES (?, 2)",
                )
                .bind(session_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())
                {
                    if let Err(rollback_err) = tx.rollback().await {
                        return Err(format!("{e}; rollback failed: {rollback_err}"));
                    }
                    return Err(e);
                }
                1
            }
            Some((raw,)) => {
                let (current, next_stored) = match locked_counter_advance(raw, session_id) {
                    Ok(v) => v,
                    Err(e) => {
                        if let Err(rollback_err) = tx.rollback().await {
                            return Err(format!("{e}; rollback failed: {rollback_err}"));
                        }
                        return Err(e);
                    }
                };
                // Persist the bump (may store the exhausted sentinel;
                // `allocated_task_id_from_counter` rejects it on next read).
                if let Err(e) =
                    sqlx::query("UPDATE session_todo_counters SET next_id = ? WHERE session_id = ?")
                        .bind(next_stored as i64)
                        .bind(session_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| e.to_string())
                {
                    if let Err(rollback_err) = tx.rollback().await {
                        return Err(format!("{e}; rollback failed: {rollback_err}"));
                    }
                    return Err(e);
                }
                current
            }
        };
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(current)
    }

    async fn set_next_task_id(&self, session_id: &str, next: u32) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO session_todo_counters (session_id, next_id) VALUES (?, ?) \
             ON DUPLICATE KEY UPDATE next_id = VALUES(next_id)",
        )
        .bind(session_id)
        .bind(counter_bind_value(next))
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn peek_next_task_id(&self, session_id: &str) -> Result<u32, String> {
        // Pure read — no side effect, so concurrent allocators can't
        // race the snapshot. Missing row → 1 (matches next_task_id's
        // "first allocation" semantics). A `next_id` of 0 is treated
        // the same as "no row" for the same reason.
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT next_id FROM session_todo_counters WHERE session_id = ?")
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| e.to_string())?;
        let raw = row.map(|(v,)| v).unwrap_or(0);
        if raw == 0 {
            return Ok(1);
        }
        if raw < 0 {
            return Err(format!(
                "session_todo_counters.next_id out of range for {session_id}"
            ));
        }
        allocated_task_id_from_counter(raw as u64, session_id)
    }

    async fn load_all_sessions(&self) -> Result<Vec<(String, Vec<SessionTask>)>, String> {
        // One SQL round-trip across the whole table — the multi-
        // session board is a low-frequency power-user surface so
        // we favour simplicity over a per-session fan-out. Order
        // by `(session_id, ordinal)` so the group grouping below
        // is a single linear pass instead of a HashMap build.
        let rows = sqlx::query(
            "SELECT session_id, todo_id, title, description, active_form, status, owner, \
                    metadata, blocks, blocked_by, subtasks, \
                    CAST(created_at AS CHAR) AS created_at, \
                    CAST(updated_at AS CHAR) AS updated_at \
             FROM session_todos \
             ORDER BY session_id ASC, ordinal ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;

        let mut out: Vec<(String, Vec<SessionTask>)> = Vec::new();
        for row in rows {
            let sid: String = row.try_get("session_id").map_err(|e| e.to_string())?;
            let task = row_to_task(&row).map_err(|e| e.to_string())?;
            match out.last_mut() {
                Some((cur_sid, tasks)) if cur_sid == &sid => {
                    tasks.push(task);
                }
                _ => {
                    out.push((sid, vec![task]));
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_bind_value_preserves_full_u32_range() {
        assert_eq!(counter_bind_value(1), 1);
        assert_eq!(counter_bind_value(u32::MAX), 4_294_967_295);
    }

    #[test]
    fn counter_after_allocation_uses_exhausted_sentinel_instead_of_wrapping() {
        assert_eq!(
            counter_after_allocation(u32::MAX),
            4_294_967_296,
            "allocating the final u32 task id must store an exhausted sentinel, not wrap to 0/-1"
        );
    }

    #[test]
    fn allocated_task_id_rejects_exhausted_counter_sentinel() {
        let err = allocated_task_id_from_counter(4_294_967_296, "sess-overflow")
            .expect_err("exhausted sentinel is not a valid task id");
        assert!(
            err.contains("session_todo_counters.next_id overflow"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn locked_counter_raw_validation_rejects_invalid_values_before_update() {
        assert!(
            locked_counter_advance(-1, "sess-bad")
                .expect_err("negative counter requires rollback")
                .contains("out of range")
        );
        assert!(
            locked_counter_advance(0, "sess-bad")
                .expect_err("zero counter requires rollback")
                .contains("out of range")
        );
        assert!(
            locked_counter_advance(EXHAUSTED_COUNTER_SENTINEL as i64, "sess-bad")
                .expect_err("exhausted sentinel requires rollback")
                .contains("overflow")
        );
    }

    #[test]
    fn encode_task_json_fields_omits_empty_optional_columns() {
        let task = SessionTask {
            id: "task-1".into(),
            title: "test".into(),
            description: None,
            status: "pending".into(),
            subtasks: vec![],
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: "2025-01-01T00:00:00Z".into(),
            active_form: None,
            owner: None,
            metadata: None,
            blocks: vec![],
            blocked_by: vec![],
        };

        let encoded = encode_task_json_fields(&task);
        assert_eq!(encoded.metadata, None);
        assert_eq!(encoded.blocks, None);
        assert_eq!(encoded.blocked_by, None);
        assert_eq!(encoded.subtasks, None);
    }

    #[test]
    fn encode_task_json_fields_serializes_present_values() {
        let task = SessionTask {
            id: "task-2".into(),
            title: "test".into(),
            description: Some("desc".into()),
            status: "in_progress".into(),
            subtasks: vec![SessionSubtask {
                id: "sub-1".into(),
                title: "subtask".into(),
                description: None,
                status: "pending".into(),
                depends_on: vec!["sub-0".into()],
                owner: None,
            }],
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: "2025-01-01T00:00:01Z".into(),
            active_form: Some("testing".into()),
            owner: Some("agent".into()),
            metadata: Some(serde_json::Map::from_iter([(
                "priority".into(),
                serde_json::Value::String("high".into()),
            )])),
            blocks: vec!["task-3".into()],
            blocked_by: vec!["task-0".into()],
        };

        let encoded = encode_task_json_fields(&task);
        assert_eq!(encoded.metadata.as_deref(), Some(r#"{"priority":"high"}"#));
        assert_eq!(encoded.blocks.as_deref(), Some(r#"["task-3"]"#));
        assert_eq!(encoded.blocked_by.as_deref(), Some(r#"["task-0"]"#));
        assert_eq!(
            encoded.subtasks.as_deref(),
            Some(
                r#"[{"id":"sub-1","title":"subtask","description":null,"status":"pending","depends_on":["sub-0"]}]"#
            )
        );
    }

    #[test]
    fn task_insert_batch_ranges_split_large_payloads() {
        assert_eq!(task_insert_batch_ranges(0), Vec::<(usize, usize)>::new());
        assert_eq!(task_insert_batch_ranges(1), vec![(0, 1)]);
        assert_eq!(task_insert_batch_ranges(100), vec![(0, 100)]);
        assert_eq!(
            task_insert_batch_ranges(250),
            vec![(0, 100), (100, 200), (200, 250)]
        );
    }
}
