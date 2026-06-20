//! MatrixOne-backed [`TaskStore`] for the durable session task board.
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
use serde::de::DeserializeOwned;

use std::sync::Arc;

use crate::task_mgmt::{
    InMemoryTaskStore, SESSION_TASK_STATUS_IN_PROGRESS, SESSION_TASK_STATUS_PAUSED,
    SESSION_TASK_STATUS_PENDING, SessionSubtask, SessionTask, SessionTaskStatusKind, TaskMutation,
    TaskStore,
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

struct EncodedTaskDatetimes {
    archived_at: Option<String>,
    created_at: String,
    updated_at: String,
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

fn push_optional_json<'args>(
    row: &mut sqlx::query_builder::Separated<'_, 'args, MySql, &'static str>,
    value: Option<String>,
) {
    row.push_bind(value);
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
        let encoded_datetimes = tasks[start..end]
            .iter()
            .map(|task| {
                let updated_at = to_mo_datetime(&task.updated_at, "updated_at", &task.id)?;
                Ok(EncodedTaskDatetimes {
                    archived_at: (task.status == SessionTaskStatusKind::Archived)
                        .then(|| updated_at.clone()),
                    created_at: to_mo_datetime(&task.created_at, "created_at", &task.id)?,
                    updated_at,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut builder = QueryBuilder::<MySql>::new(
            "INSERT INTO session_todos (\
                session_id, todo_id, user_id, ordinal, title, description, active_form, \
                status, owner, metadata, blocks, blocked_by, subtasks, archived_at, \
                created_at, updated_at) ",
        );
        builder.push_values(
            tasks[start..end]
                .iter()
                .zip(encoded_datetimes.iter())
                .enumerate(),
            |mut row, (offset, (task, datetimes))| {
                let encoded = encode_task_json_fields(task);
                let status_str = task.status.to_string();
                row.push_bind(session_id)
                    .push_bind(&task.id)
                    .push_bind(user_id)
                    .push_bind((start + offset) as i32)
                    .push_bind(&task.title)
                    .push_bind(&task.description)
                    .push_bind(&task.active_form)
                    .push_bind(status_str)
                    .push_bind(&task.owner);
                push_optional_json(&mut row, encoded.metadata);
                push_optional_json(&mut row, encoded.blocks);
                push_optional_json(&mut row, encoded.blocked_by);
                push_optional_json(&mut row, encoded.subtasks);
                row.push_bind(&datetimes.archived_at)
                    .push_bind(&datetimes.created_at)
                    .push_bind(&datetimes.updated_at);
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

fn peek_task_id_from_counter(row: Option<i64>, session_id: &str) -> Result<u32, String> {
    let Some(raw) = row else {
        return Ok(1);
    };
    if raw <= 0 {
        return Err(format!(
            "session_todo_counters.next_id out of range for {session_id}: {raw}"
        ));
    }
    allocated_task_id_from_counter(raw as u64, session_id)
}

/// Convert an RFC3339 timestamp (what `TaskManager` writes) into the
/// `YYYY-MM-DD HH:MM:SS.ffffff` form MatrixOne accepts for `DATETIME(6)`
/// columns. Invalid timestamps are data corruption and fail closed.
fn to_mo_datetime(rfc3339: &str, column: &'static str, task_id: &str) -> Result<String, String> {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|dt| {
            dt.with_timezone(&chrono::Utc)
                .format("%Y-%m-%d %H:%M:%S%.6f")
                .to_string()
        })
        .map_err(|e| format!("task '{task_id}' has invalid {column} timestamp '{rfc3339}': {e}"))
}

/// Pick the right [`TaskStore`] for this process: MatrixOne when a pool is
/// configured, in-memory otherwise. Call once per process (or per
/// host-binding lifecycle) — the returned store is safe to share across
/// sessions; each `TaskManager` scopes reads/writes by `session_id`.
pub fn select_task_store(
    pool: Option<astra_core::sqlx::Pool<astra_core::sqlx::MySql>>,
    user_id: impl Into<String>,
) -> Result<Arc<dyn TaskStore>, String> {
    match pool {
        Some(pool) => Ok(Arc::new(MatrixOneTaskStore::new_for_user(pool, user_id)?)),
        None => Ok(Arc::new(InMemoryTaskStore::new())),
    }
}

/// MatrixOne-backed task store. See module docs.
pub struct MatrixOneTaskStore {
    pool: Pool<MySql>,
    changed_tx: tokio::sync::broadcast::Sender<String>,
    /// User who owns rows written through this store. Threaded into
    /// every INSERT so cross-session user-scoped queries
    /// (`idx_session_todos_user_status_updated`) can find them
    /// without a join. Production construction must bind a real user_id.
    user_id: String,
}

impl MatrixOneTaskStore {
    pub fn new_for_user(pool: Pool<MySql>, user_id: impl Into<String>) -> Result<Self, String> {
        let user_id = user_id.into();
        if user_id.trim().is_empty() {
            return Err(
                "MatrixOne task store requires a non-empty user_id for durable task access"
                    .to_string(),
            );
        }
        Ok(Self {
            pool,
            changed_tx: tokio::sync::broadcast::channel(16).0,
            user_id,
        })
    }

    pub fn from_shared_for_user(
        shared: &astra_core::SharedPool,
        user_id: impl Into<String>,
    ) -> Result<Self, String> {
        Self::new_for_user(shared.get().clone(), user_id)
    }

    #[cfg(test)]
    pub fn new_for_test(pool: Pool<MySql>, user_id: impl Into<String>) -> Self {
        Self::new_for_user(pool, user_id)
            .expect("test MatrixOneTaskStore user_id must be non-empty")
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
    let decoded = (|| {
        let id: String = row.try_get("todo_id")?;
        let title: String = row.try_get("title")?;
        let description: Option<String> = row.try_get("description")?;
        let active_form: Option<String> = row.try_get("active_form")?;
        let status_str: String = row.try_get("status")?;
        let status = parse_persisted_status(&status_str)?;
        let owner: Option<String> = row.try_get("owner")?;
        let metadata: Option<serde_json::Map<String, serde_json::Value>> =
            parse_optional_json_text(optional_json_text(row, "metadata")?, "metadata")?;
        let blocks: Vec<String> =
            parse_optional_json_text(optional_json_text(row, "blocks")?, "blocks")?
                .unwrap_or_default();
        let blocked_by: Vec<String> =
            parse_optional_json_text(optional_json_text(row, "blocked_by")?, "blocked_by")?
                .unwrap_or_default();
        let subtasks: Vec<SessionSubtask> =
            parse_optional_json_text(optional_json_text(row, "subtasks")?, "subtasks")?
                .unwrap_or_default();
        // `created_at` / `updated_at` are NOT NULL DATETIME(6) columns cast to
        // CHAR in the SELECT. A NULL here means schema drift (column relaxed to
        // nullable) or a bad cast — surface it instead of silently letting an
        // empty string flow back through `to_mo_datetime` on the next save and
        // triggering the legacy-format fallback branch.
        let created_at_raw: String = row
            .try_get::<Option<String>, _>("created_at")?
            .ok_or_else(|| sqlx::Error::Decode("session_todos.created_at is NULL".into()))?;
        let updated_at_raw: String = row
            .try_get::<Option<String>, _>("updated_at")?
            .ok_or_else(|| sqlx::Error::Decode("session_todos.updated_at is NULL".into()))?;
        let created_at = from_mo_datetime(&created_at_raw, "created_at")?;
        let updated_at = from_mo_datetime(&updated_at_raw, "updated_at")?;

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
            archived_at: None, // populated on load from column below
        })
    })();

    if let Err(error) = &decoded {
        let session_id = row
            .try_get::<String, _>("session_id")
            .unwrap_or_else(|_| "<not-selected>".to_string());
        let todo_id = row
            .try_get::<String, _>("todo_id")
            .unwrap_or_else(|_| "<unavailable>".to_string());
        tracing::error!(
            session_id,
            todo_id,
            error = %error,
            "failed to decode session_todos row"
        );
    }

    decoded
}

fn optional_json_text(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
) -> Result<Option<String>, sqlx::Error> {
    if let Ok(value) = row.try_get::<Option<String>, _>(column) {
        return Ok(value);
    }
    let bytes = row.try_get::<Option<Vec<u8>>, _>(column)?;
    match bytes {
        Some(bytes) => String::from_utf8(bytes).map(Some).map_err(|error| {
            sqlx::Error::Decode(format!("{column} is not valid UTF-8: {error}").into())
        }),
        None => Ok(None),
    }
}

fn parse_optional_json_text<T>(
    value: Option<String>,
    column: &'static str,
) -> Result<Option<T>, sqlx::Error>
where
    T: DeserializeOwned,
{
    let Some(raw) = value else {
        return Ok(None);
    };
    serde_json::from_str(&raw).map(Some).map_err(|e| {
        sqlx::Error::Decode(format!("session_todos.{column} contains invalid JSON: {e}").into())
    })
}

fn from_mo_datetime(value: &str, column: &'static str) -> Result<String, sqlx::Error> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return Ok(dt.with_timezone(&chrono::Utc).to_rfc3339());
    }
    chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
        .map(|dt| chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(dt, chrono::Utc))
        .map(|dt| dt.to_rfc3339())
        .map_err(|e| {
            sqlx::Error::Decode(
                format!("session_todos.{column} contains invalid DATETIME '{value}': {e}").into(),
            )
        })
}

fn parse_persisted_status(status: &str) -> Result<SessionTaskStatusKind, sqlx::Error> {
    match status {
        "in_progress" => Ok(SessionTaskStatusKind::InProgress),
        "pending" => Ok(SessionTaskStatusKind::Pending),
        "paused" => Ok(SessionTaskStatusKind::Paused),
        "completed" => Ok(SessionTaskStatusKind::Completed),
        "failed" => Ok(SessionTaskStatusKind::Failed),
        "cancelled" => Ok(SessionTaskStatusKind::Cancelled),
        "archived" => Ok(SessionTaskStatusKind::Archived),
        "deleted" => Ok(SessionTaskStatusKind::Deleted),
        "migrated" => Ok(SessionTaskStatusKind::Migrated),
        other => Err(sqlx::Error::Decode(
            format!("session_todos.status contains invalid status '{other}'").into(),
        )),
    }
}

#[async_trait]
impl TaskStore for MatrixOneTaskStore {
    async fn load(&self, session_id: &str) -> Result<Vec<SessionTask>, String> {
        self.load_rows(session_id).await.map_err(|e| e.to_string())
    }

    /// U-8: SQL-pushdown path for open-work `active` queries.
    /// Uses `idx_session_todos_session_status_updated` so only matching
    /// rows are returned instead of shipping the whole table to Rust.
    ///
    /// **Security fix**: removed fail-open `OR status NOT IN (...)` clause.
    /// From first principles: load_active must return ONLY known active
    /// statuses (pending, in_progress, paused). Treating unknown/corrupted
    /// status values as active violates the fail-closed principle and could
    /// expose inactive tasks to orchestration logic.
    async fn load_active(&self, session_id: &str) -> Result<Vec<SessionTask>, String> {
        let rows = sqlx::query(
            "SELECT todo_id, title, description, active_form, status, owner, \
                    metadata, blocks, blocked_by, subtasks, \
                    CAST(created_at AS CHAR) AS created_at, \
                    CAST(updated_at AS CHAR) AS updated_at \
             FROM session_todos \
             WHERE session_id = ? \
               AND status IN (?, ?, ?) \
             ORDER BY ordinal ASC",
        )
        .bind(session_id)
        .bind(SESSION_TASK_STATUS_PENDING.to_string())
        .bind(SESSION_TASK_STATUS_IN_PROGRESS.to_string())
        .bind(SESSION_TASK_STATUS_PAUSED.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;

        let mut tasks = Vec::with_capacity(rows.len());
        for row in rows {
            tasks.push(row_to_task(&row).map_err(|e| e.to_string())?);
        }
        Ok(tasks)
    }

    async fn save(&self, session_id: &str, tasks: Vec<SessionTask>) -> Result<(), String> {
        // Full replace semantics: the caller computed the next state; we
        // atomically make the table match it. Transaction ensures readers
        // on the other node never see a partial update.
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;

        if let Err(e) = sqlx::query(
            "INSERT INTO session_todo_counters (session_id, next_id, version) VALUES (?, 1, 0) \
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

        if let Err(e) = sqlx::query(
            "UPDATE session_todo_counters SET version = version + 1 WHERE session_id = ?",
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

        tx.commit().await.map_err(|e| e.to_string())?;
        let _ = self.changed_tx.send(session_id.to_string());
        Ok(())
    }

    async fn mutate(&self, session_id: &str, mutation: TaskMutation) -> Result<String, String> {
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;

        if let Err(e) = sqlx::query(
            "INSERT INTO session_todo_counters (session_id, next_id, version) VALUES (?, 1, 0) \
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

        if let Err(e) = sqlx::query(
            "UPDATE session_todo_counters SET version = version + 1 WHERE session_id = ?",
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
                    "INSERT INTO session_todo_counters (session_id, next_id, version) VALUES (?, 2, 1)",
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
                    sqlx::query("UPDATE session_todo_counters SET next_id = ?, version = version + 1 WHERE session_id = ?")
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
            "INSERT INTO session_todo_counters (session_id, next_id, version) VALUES (?, ?, 1) \
             ON DUPLICATE KEY UPDATE next_id = VALUES(next_id), version = version + 1",
        )
        .bind(session_id)
        .bind(counter_bind_value(next))
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn restore_snapshot_state(
        &self,
        session_id: &str,
        tasks: Vec<SessionTask>,
        next_task_id: u32,
        expected_version: u64,
    ) -> Result<(), String> {
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;

        if expected_version > 0 {
            // CAS: atomically verify the session version hasn't changed since
            // the snapshot was captured.  Without this, a concurrent sweeper
            // auto-pause or plan-mirror mutation could be silently overwritten.
            let row: Option<(i64,)> = sqlx::query_as(
                "SELECT version FROM session_todo_counters WHERE session_id = ? FOR UPDATE",
            )
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| format!("restore_snapshot_state: version check failed: {e}"))?;
            let current_version = row.map(|(v,)| v as u64).unwrap_or(0);
            if current_version != expected_version {
                return Err(format!(
                    "restore_snapshot_state: version conflict (expected={}, current={}) — \
                     task board changed after rollback snapshot was sealed; retry with fresh state",
                    expected_version, current_version
                ));
            }
        }

        sqlx::query(
            "INSERT INTO session_todo_counters (session_id, next_id, version) VALUES (?, ?, ?) \
             ON DUPLICATE KEY UPDATE next_id = VALUES(next_id), version = version + 1",
        )
        .bind(session_id)
        .bind(counter_bind_value(next_task_id))
        .bind(expected_version as i64)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("restore_snapshot_state: counter upsert failed: {e}"))?;

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

        if let Err(e) = sqlx::query(
            "UPDATE session_todo_counters SET version = version + 1 WHERE session_id = ?",
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

        tx.commit().await.map_err(|e| e.to_string())?;
        let _ = self.changed_tx.send(session_id.to_string());
        Ok(())
    }

    async fn peek_next_task_id(&self, session_id: &str) -> Result<u32, String> {
        // Pure read — no side effect, so concurrent allocators can't
        // race the snapshot. Missing row → 1 (matches next_task_id's
        // "first allocation" semantics). If a row exists but contains
        // 0/negative, fail loudly so try_snapshot_state falls back to
        // max(existing task id) + 1 instead of capturing a corrupt
        // counter and later restoring it.
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT next_id FROM session_todo_counters WHERE session_id = ?")
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| e.to_string())?;
        peek_task_id_from_counter(row.map(|(v,)| v), session_id)
    }

    async fn get_session_version(&self, session_id: &str) -> Result<u64, String> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT version FROM session_todo_counters WHERE session_id = ?")
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| e.to_string())?;
        match row {
            Some((version,)) if version >= 0 => u64::try_from(version)
                .map_err(|_| format!("session_todo_counters.version overflow for {session_id}")),
            Some((version,)) => Err(format!(
                "session_todo_counters.version out of range for {session_id}: {version}"
            )),
            None => Ok(0),
        }
    }

    async fn load_all_sessions(&self) -> Result<Vec<(String, Vec<SessionTask>)>, String> {
        if self.user_id.trim().is_empty() {
            return Err("load_all_sessions requires a user-scoped MatrixOneTaskStore".to_string());
        }
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
             WHERE user_id = ? \
             ORDER BY session_id ASC, ordinal ASC",
        )
        .bind(&self.user_id)
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

    async fn load_open_sessions(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, Vec<SessionTask>)>, String> {
        if self.user_id.trim().is_empty() {
            return Err("load_open_sessions requires a user-scoped MatrixOneTaskStore".to_string());
        }
        let limit = i64::try_from(limit.min(200)).unwrap_or(200).max(0);
        if limit == 0 {
            return Ok(Vec::new());
        }

        let rows = sqlx::query(
            "SELECT session_id, todo_id, title, description, active_form, status, owner, \
                    metadata, blocks, blocked_by, subtasks, \
                    CAST(created_at AS CHAR) AS created_at, \
                    CAST(updated_at AS CHAR) AS updated_at \
             FROM session_todos \
             WHERE user_id = ? \
               AND status IN (?, ?, ?) \
             ORDER BY updated_at DESC, session_id ASC, ordinal ASC \
             LIMIT ?",
        )
        .bind(&self.user_id)
        .bind(SESSION_TASK_STATUS_PENDING.to_string())
        .bind(SESSION_TASK_STATUS_IN_PROGRESS.to_string())
        .bind(SESSION_TASK_STATUS_PAUSED.to_string())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;

        let mut out: Vec<(String, Vec<SessionTask>)> = Vec::new();
        for row in rows {
            let sid: String = row.try_get("session_id").map_err(|e| e.to_string())?;
            let task = row_to_task(&row).map_err(|e| e.to_string())?;
            if let Some((_, tasks)) = out.iter_mut().find(|(cur_sid, _)| cur_sid == &sid) {
                tasks.push(task);
            } else {
                out.push((sid, vec![task]));
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
    fn peek_counter_decode_distinguishes_missing_from_corrupt_zero() {
        assert_eq!(
            peek_task_id_from_counter(None, "sess-new").expect("missing counter starts at one"),
            1
        );
        assert_eq!(
            peek_task_id_from_counter(Some(7), "sess-existing")
                .expect("positive counter is the next task id"),
            7
        );

        let zero = peek_task_id_from_counter(Some(0), "sess-corrupt")
            .expect_err("persisted zero counter must not look like a missing row");
        assert!(
            zero.contains("out of range") && zero.contains("0"),
            "unexpected error: {zero}"
        );

        let negative = peek_task_id_from_counter(Some(-1), "sess-corrupt")
            .expect_err("negative counter is corrupt");
        assert!(
            negative.contains("out of range") && negative.contains("-1"),
            "unexpected error: {negative}"
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
            archived_at: None,
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
                reason: Some("waiting on dependency".into()),
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
            archived_at: None,
        };

        let encoded = encode_task_json_fields(&task);
        assert_eq!(encoded.metadata.as_deref(), Some(r#"{"priority":"high"}"#));
        assert_eq!(encoded.blocks.as_deref(), Some(r#"["task-3"]"#));
        assert_eq!(encoded.blocked_by.as_deref(), Some(r#"["task-0"]"#));
        assert_eq!(
            encoded.subtasks.as_deref(),
            Some(
                r#"[{"id":"sub-1","title":"subtask","description":null,"status":"pending","depends_on":["sub-0"],"reason":"waiting on dependency"}]"#
            )
        );
    }

    #[test]
    fn parse_optional_json_text_fails_closed_on_corrupt_task_columns() {
        let absent: Option<Vec<String>> =
            parse_optional_json_text(None, "blocks").expect("null column is empty");
        assert!(absent.is_none());

        let blocks: Option<Vec<String>> =
            parse_optional_json_text(Some(r#"["task-2"]"#.into()), "blocks")
                .expect("valid JSON array parses");
        assert_eq!(blocks, Some(vec!["task-2".to_string()]));

        let corrupt =
            parse_optional_json_text::<Vec<String>>(Some("not-json".into()), "blocked_by")
                .expect_err("invalid dependency JSON must not be treated as empty");
        let corrupt = corrupt.to_string();
        assert!(
            corrupt.contains("session_todos.blocked_by") && corrupt.contains("invalid JSON"),
            "unexpected error: {corrupt}"
        );

        let wrong_shape =
            parse_optional_json_text::<Vec<String>>(Some(r#"{"task":"task-2"}"#.into()), "blocks")
                .expect_err("wrong dependency shape must not be treated as empty");
        let wrong_shape = wrong_shape.to_string();
        assert!(
            wrong_shape.contains("session_todos.blocks") && wrong_shape.contains("invalid JSON"),
            "unexpected error: {wrong_shape}"
        );
    }

    #[test]
    fn from_mo_datetime_normalizes_matrixone_datetimes_to_rfc3339() {
        assert_eq!(
            from_mo_datetime("2026-05-29 22:57:42.599249", "updated_at")
                .expect("MatrixOne DATETIME(6) parses"),
            "2026-05-29T22:57:42.599249+00:00"
        );
        assert_eq!(
            from_mo_datetime("2026-05-29T22:57:42Z", "created_at")
                .expect("RFC3339 values remain accepted"),
            "2026-05-29T22:57:42+00:00"
        );

        let err = from_mo_datetime("not-a-time", "updated_at")
            .expect_err("bad datetime must not round-trip silently")
            .to_string();
        assert!(
            err.contains("session_todos.updated_at") && err.contains("invalid DATETIME"),
            "unexpected error: {err}"
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
