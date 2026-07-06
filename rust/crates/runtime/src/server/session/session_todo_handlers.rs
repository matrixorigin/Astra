//! Session todos REST surface.
//!
//! Wraps `astra_tools::task_mgmt::TaskManager` over `MatrixOneTaskStore`
//! so edge clients (CLI, web agent) never connect to MO directly. The
//! TaskManager business logic (cycle detection, parent reconciliation,
//! id allocation) lives on the server; clients send the raw action +
//! args from the LLM `task_board` tool and receive the rendered string output.
//!
//! Endpoints:
//! - `POST /sessions/{session_id}/todos:execute` — run a TaskManager
//!   action (create/update/list/get/stop/archive) and return its string
//!   output. Internal action `fork_copy` copies a parent task board
//!   into a newly forked child session.
//! - `GET /sessions/{session_id}/todos` — load the full task list.
//!
//! User isolation: every request resolves the user via the auth header
//! and verifies the session belongs to that user before touching
//! `session_todos`. We do NOT trust the client-supplied `session_id`
//! to skip ownership checks.

use super::*;
use astra_tools::task_mgmt::{
    TaskManager, TaskStore, normalize_title, prepare_task_snapshot_for_fork,
};
use astra_tools::task_mgmt_matrixone::MatrixOneTaskStore;
use sqlx::Row;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::time::{Duration, sleep};

const SESSION_TODO_OWNER_LOCK_SQL: &str =
    "SELECT 1 FROM agent_sessions WHERE session_id = ? AND user_id = ? FOR UPDATE";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecuteTodoRequest {
    /// `task_board` tool action: `create | update | list | get | stop | archive`.
    /// Internal callers may also use `fork_copy`; it is not advertised
    /// in the model-facing task schema.
    pub action: String,
    /// Action arguments — same shape the LLM emits to the `task_board` tool.
    /// Unknown fields are rejected by action-specific validation.
    #[serde(default)]
    pub args: serde_json::Value,
    /// Required for `action=create`. Provides HTTP retry idempotency for
    /// cloud/edge calls so a connection drop after mutation cannot create
    /// duplicate tasks on retry.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct ExecuteTodoResponse {
    /// Rendered output (success summary + optional JSON body, OR
    /// `Error: ...` prefix on failure). Mirrors what the local
    /// TaskManager returns.
    pub output: String,
}

#[derive(Serialize)]
pub(crate) struct LoadTodosResponse {
    pub tasks: Vec<astra_tools::task_mgmt::SessionTask>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserTodosQuery {
    /// Status filter; `active` returns all open work:
    /// pending+in_progress+paused. Default `active` so the
    /// cross-session view is "what do I still need to account for"
    /// rather than the noisy full history.
    pub status: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct UserTodoEntry {
    pub session_id: String,
    pub todo_id: String,
    pub title: String,
    pub status: String,
    pub updated_at: String,
    /// When the session containing this task was started. `None` when
    /// the session row no longer exists (deleted) — the task row stays
    /// so the user can still see it. Clients should render this as e.g.
    /// "session from 2 days ago" so the user knows which context the
    /// task belongs to without having to remember session IDs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_started_at: Option<String>,
    /// Short human title of the session if one was set, or `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_title: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct UserTodosResponse {
    pub tasks: Vec<UserTodoEntry>,
    pub total: usize,
}

fn normalize_user_todos_status_filter(
    status: Option<&str>,
) -> Result<&str, (StatusCode, Json<ErrorResponse>)> {
    let status = status.unwrap_or("active");
    if astra_tools::task_mgmt::VALID_LIST_STATUS_FILTERS.contains(&status) {
        Ok(status)
    } else {
        Err(error_response(
            StatusCode::BAD_REQUEST,
            format!(
                "invalid status '{}' (valid: {})",
                status,
                astra_tools::task_mgmt::VALID_LIST_STATUS_FILTERS.join("|")
            ),
        ))
    }
}

fn is_known_persisted_todo_status(status: &str) -> bool {
    matches!(
        status,
        "pending"
            | "in_progress"
            | "paused"
            | "completed"
            | "failed"
            | "cancelled"
            | "archived"
            | "deleted"
            | "migrated"
    )
}

fn adopt_duplicate_refusal(
    source_session: &str,
    source_task_id: &str,
    duplicate_id: &str,
    duplicate_title: &str,
) -> String {
    format!(
        "Refused: target session already has open task #{duplicate_id} with the same title\n{}",
        serde_json::json!({
            "success": false,
            "duplicate_of": duplicate_id,
            "duplicate_title": duplicate_title,
            "source_session_id": source_session,
            "source_task_id": source_task_id,
            "message": format!(
                "Target session already has active task '{}' with the same normalized title. Use the existing target task, or archive/rename it before adopting '{}:{}'.",
                duplicate_id, source_session, source_task_id
            ),
        })
    )
}

fn adoptable_source_status(status: &str) -> bool {
    matches!(status, "pending" | "in_progress" | "paused")
}

fn required_adopt_string(args: &serde_json::Value, field: &str) -> Result<String, String> {
    let Some(raw) = args.get(field) else {
        return Err(format!("'{field}' is required for adopt"));
    };
    let Some(value) = raw.as_str() else {
        return Err(format!("'{field}' must be a string for adopt"));
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("'{field}' must be a non-empty string for adopt"));
    }
    Ok(trimmed.to_string())
}

fn validate_adopt_args(args: &serde_json::Value) -> Result<(), String> {
    let Some(obj) = args.as_object() else {
        return Err("task_board.adopt arguments must be an object".to_string());
    };
    for key in obj.keys() {
        if !["action", "source_session_id", "task_id"].contains(&key.as_str()) {
            return Err(format!(
                "unknown field '{key}' for task_board.adopt (valid: action, source_session_id, task_id)"
            ));
        }
    }
    if let Some(action) = obj.get("action") {
        if !action.is_string() {
            return Err("field 'action' must be a string".to_string());
        }
    }
    Ok(())
}

fn clean_adopted_subtasks(subtasks_json: &str) -> Result<Option<serde_json::Value>, String> {
    let parsed = serde_json::from_str::<serde_json::Value>(subtasks_json)
        .map_err(|e| format!("source subtasks contains invalid JSON: {e}"))?;
    let arr = parsed
        .as_array()
        .ok_or_else(|| "source subtasks must be an array".to_string())?;
    if arr.is_empty() {
        return Ok(None);
    }

    let mut valid_ids = HashSet::new();
    let mut cleaned_with_deps = Vec::new();
    for (index, st) in arr.iter().enumerate() {
        let obj = st
            .as_object()
            .ok_or_else(|| format!("source subtasks[{index}] must be an object"))?;
        for key in obj.keys() {
            if ![
                "id",
                "title",
                "description",
                "status",
                "depends_on",
                "owner",
            ]
            .contains(&key.as_str())
            {
                return Err(format!("unknown source subtasks[{index}].{key}"));
            }
        }

        let id = obj
            .get("id")
            .ok_or_else(|| format!("source subtasks[{index}].id is required"))?
            .as_str()
            .ok_or_else(|| format!("source subtasks[{index}].id must be a string"))?
            .trim();
        if id.is_empty() {
            return Err(format!("source subtasks[{index}].id must be non-empty"));
        }
        if !valid_ids.insert(id.to_string()) {
            return Err(format!("duplicate source subtask id '{id}'"));
        }

        let title = obj
            .get("title")
            .ok_or_else(|| format!("source subtasks[{index}].title is required"))?
            .as_str()
            .ok_or_else(|| format!("source subtasks[{index}].title must be a string"))?
            .trim();
        if title.is_empty() {
            return Err(format!("source subtasks[{index}].title must be non-empty"));
        }

        if let Some(status) = obj.get("status")
            && !status.is_string()
        {
            return Err(format!("source subtasks[{index}].status must be a string"));
        }

        let mut clean = serde_json::json!({
            "id": id,
            "title": title,
            "status": "pending",
        });
        if let Some(description) = obj.get("description") {
            if !description.is_null() {
                let text = description.as_str().ok_or_else(|| {
                    format!("source subtasks[{index}].description must be a string")
                })?;
                clean["description"] = serde_json::json!(text);
            }
        }
        if let Some(owner) = obj.get("owner") {
            if !owner.is_null() {
                let text = owner
                    .as_str()
                    .ok_or_else(|| format!("source subtasks[{index}].owner must be a string"))?
                    .trim();
                if text.is_empty() {
                    return Err(format!("source subtasks[{index}].owner must be non-empty"));
                }
                clean["owner"] = serde_json::json!(text);
            }
        }
        let raw_deps = match obj.get("depends_on") {
            Some(value) if value.is_null() => Vec::new(),
            Some(value) => {
                let deps = value.as_array().ok_or_else(|| {
                    format!("source subtasks[{index}].depends_on must be an array")
                })?;
                deps.iter()
                    .enumerate()
                    .map(|(dep_index, dep)| {
                        let dep_id = dep.as_str().ok_or_else(|| {
                            format!(
                                "source subtasks[{index}].depends_on[{dep_index}] must be a string"
                            )
                        })?;
                        let dep_id = dep_id.trim();
                        if dep_id.is_empty() {
                            return Err(format!(
                                "source subtasks[{index}].depends_on[{dep_index}] must be non-empty"
                            ));
                        }
                        Ok(dep_id.to_string())
                    })
                    .collect::<Result<Vec<_>, String>>()?
            }
            None => Vec::new(),
        };
        cleaned_with_deps.push((clean, raw_deps));
    }
    if cleaned_with_deps.is_empty() {
        return Ok(None);
    }

    for (index, (clean, raw_deps)) in cleaned_with_deps.iter_mut().enumerate() {
        let id = clean
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let mut seen_deps = HashSet::new();
        let mut deps = Vec::with_capacity(raw_deps.len());
        for dep_id in raw_deps {
            if dep_id == &id {
                return Err(format!(
                    "source subtasks[{index}] cannot depend on itself ('{dep_id}')"
                ));
            }
            if !seen_deps.insert(dep_id.as_str()) {
                return Err(format!(
                    "source subtasks[{index}] has duplicate dependency '{dep_id}'"
                ));
            }
            if !valid_ids.contains(dep_id) {
                return Err(format!(
                    "source subtasks[{index}] has unknown dependency '{dep_id}'"
                ));
            }
            deps.push(serde_json::json!(dep_id));
        }
        if !deps.is_empty() {
            clean["depends_on"] = serde_json::Value::Array(deps);
        }
    }

    Ok(Some(serde_json::Value::Array(
        cleaned_with_deps
            .into_iter()
            .map(|(clean, _)| clean)
            .collect(),
    )))
}

fn todo_to_mo_datetime(rfc3339: &str, column: &'static str) -> Result<String, String> {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|dt| {
            dt.with_timezone(&chrono::Utc)
                .format("%Y-%m-%d %H:%M:%S%.6f")
                .to_string()
        })
        .map_err(|e| format!("invalid adopted task {column} timestamp '{rfc3339}': {e}"))
}

fn task_created_output(
    task_id: &str,
    title: &str,
    source_session: &str,
    source_task_id: &str,
) -> String {
    format!(
        "Task #{task_id} created: {title}\n{}",
        serde_json::json!({
            "success": true,
            "task_id": task_id,
            "source_session_id": source_session,
            "source_task_id": source_task_id,
            "message": format!("Task '{title}' adopted successfully"),
        })
    )
}

fn parse_next_task_counter(raw: i64, session_id: &str) -> Result<(u32, i64), String> {
    if raw <= 0 {
        return Err(format!(
            "session_todo_counters.next_id out of range for {session_id}: {raw}"
        ));
    }
    let current = u32::try_from(raw as u64)
        .map_err(|_| format!("session_todo_counters.next_id overflow for {session_id}"))?;
    let next_stored = u64::from(current) + 1;
    if next_stored > u64::from(u32::MAX) + 1 {
        return Err(format!("session_todo_counters exhausted for {session_id}"));
    }
    Ok((current, next_stored as i64))
}

fn session_todo_owner_mismatch_error(session_id: &str, user_id: &str, reason: &str) -> String {
    format!(
        "session_todo_counters owner mismatch for session_id={session_id} user_id={user_id}: {reason}"
    )
}

async fn ensure_session_todo_session_owner(
    executor: &mut sqlx::MySqlConnection,
    session_id: &str,
    user_id: &str,
) -> Result<(), String> {
    let session_exists: Option<(i32,)> = sqlx::query_as(SESSION_TODO_OWNER_LOCK_SQL)
        .bind(session_id)
        .bind(user_id)
        .fetch_optional(&mut *executor)
        .await
        .map_err(|e| e.to_string())?;
    if session_exists.is_some() {
        Ok(())
    } else {
        Err(session_todo_owner_mismatch_error(
            session_id,
            user_id,
            "agent_sessions owner root missing or belongs to another user",
        ))
    }
}

async fn ensure_session_todo_counter_owner_available(
    executor: &mut sqlx::MySqlConnection,
    session_id: &str,
    user_id: &str,
) -> Result<(), String> {
    ensure_session_todo_session_owner(executor, session_id, user_id).await
}

async fn adopt_task_into_session_atomic(
    shared: &astra_core::SharedPool,
    user_id: &str,
    source_session: &str,
    source_task_id: &str,
    target_session: &str,
) -> Result<String, String> {
    let mut tx = shared.get().begin().await.map_err(|e| e.to_string())?;

    ensure_session_todo_session_owner(&mut tx, source_session, user_id)
        .await
        .map_err(|e| format!("source task owner check failed: {e}"))?;

    let source_row: Option<(String, Option<String>, Option<String>, String)> = sqlx::query_as(
        "SELECT title, description, subtasks, status FROM session_todos \
             WHERE session_id = ? AND todo_id = ? AND user_id = ? \
               AND status NOT IN ('migrated', 'deleted') \
             LIMIT 1 FOR UPDATE",
    )
    .bind(source_session)
    .bind(source_task_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| format!("source lookup failed: {e}"))?;
    let Some((title, description, subtasks_json, source_status)) = source_row else {
        return Err(format!(
            "source task {source_session}:{source_task_id} not found, not owned by you, or already migrated"
        ));
    };
    if !adoptable_source_status(&source_status) {
        return Err(format!(
            "source task {source_session}:{source_task_id} is '{source_status}' — only pending, in_progress, or paused tasks can be adopted"
        ));
    }
    let cleaned_subtasks = match subtasks_json.as_deref().map(clean_adopted_subtasks) {
        Some(Ok(cleaned)) => cleaned,
        Some(Err(error)) => {
            return Err(format!(
                "source task {source_session}:{source_task_id} has invalid subtasks: {error}"
            ));
        }
        None => None,
    };

    ensure_session_todo_counter_owner_available(&mut tx, target_session, user_id)
        .await
        .map_err(|e| format!("initialize target task counter failed: {e}"))?;

    sqlx::query(
        "INSERT INTO session_todo_counters (session_id, user_id, next_id, version) VALUES (?, ?, 1, 0) \
         ON DUPLICATE KEY UPDATE next_id = next_id",
    )
    .bind(target_session)
    .bind(user_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("initialize target task counter failed: {e}"))?;

    let raw_next: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT next_id FROM session_todo_counters WHERE session_id = ? AND user_id = ? FOR UPDATE",
    )
    .bind(target_session)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await
    .map(|(next,)| next)
    .map_err(|e| format!("lock target task counter failed: {e}"))?;
    let (allocated, next_stored) = parse_next_task_counter(raw_next, target_session)?;
    let target_task_id = format!("task-{allocated}");

    let target_rows = sqlx::query(
        "SELECT todo_id, title, status, ordinal FROM session_todos \
         WHERE session_id = ? AND user_id = ? \
         ORDER BY ordinal ASC FOR UPDATE",
    )
    .bind(target_session)
    .bind(user_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| format!("target duplicate preflight failed: {e}"))?;
    let normalized_source_title = normalize_title(&title);
    for row in &target_rows {
        let todo_id: String = row.try_get("todo_id").map_err(|e| e.to_string())?;
        let target_title: String = row.try_get("title").map_err(|e| e.to_string())?;
        let status: String = row.try_get("status").map_err(|e| e.to_string())?;
        if matches!(status.as_str(), "pending" | "in_progress" | "paused")
            && normalize_title(&target_title) == normalized_source_title
        {
            return Err(adopt_duplicate_refusal(
                source_session,
                source_task_id,
                &todo_id,
                &target_title,
            ));
        }
        if todo_id == target_task_id {
            return Err(format!(
                "task counter desync — id '{target_task_id}' already exists in target session {target_session}"
            ));
        }
    }

    let migrate = sqlx::query(
        "UPDATE session_todos SET status = 'migrated', updated_at = NOW(6) \
         WHERE session_id = ? AND todo_id = ? AND user_id = ? AND status = ?",
    )
    .bind(source_session)
    .bind(source_task_id)
    .bind(user_id)
    .bind(&source_status)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("migrate source task failed: {e}"))?;
    if migrate.rows_affected() != 1 {
        return Err(format!(
            "source task {source_session}:{source_task_id} was already migrated by a concurrent adopt"
        ));
    }

    let metadata = serde_json::to_string(&serde_json::json!({
        "forked_from": format!("{source_session}:{source_task_id}"),
    }))
    .map_err(|e| format!("encode adopted metadata failed: {e}"))?;
    let subtasks = cleaned_subtasks
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| format!("encode adopted subtasks failed: {e}"))?;
    let now = chrono::Utc::now().to_rfc3339();
    let ordinal = i32::try_from(target_rows.len())
        .map_err(|_| format!("target task board {target_session} has too many rows"))?;

    sqlx::query(
        "INSERT INTO session_todos (\
            session_id, todo_id, user_id, ordinal, title, description, active_form, \
            status, owner, metadata, blocks, blocked_by, subtasks, archived_at, \
            created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, NULL, 'pending', NULL, ?, NULL, NULL, ?, NULL, ?, ?)",
    )
    .bind(target_session)
    .bind(&target_task_id)
    .bind(user_id)
    .bind(ordinal)
    .bind(&title)
    .bind(&description)
    .bind(metadata)
    .bind(subtasks)
    .bind(todo_to_mo_datetime(&now, "created_at")?)
    .bind(todo_to_mo_datetime(&now, "updated_at")?)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("insert adopted target task failed: {e}"))?;

    sqlx::query(
        "UPDATE session_todo_counters SET next_id = ?, version = version + 1 \
         WHERE session_id = ? AND user_id = ?",
    )
    .bind(next_stored)
    .bind(target_session)
    .bind(user_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("advance target task counter failed: {e}"))?;

    tx.commit()
        .await
        .map_err(|e| format!("commit adopt transaction failed: {e}"))?;

    Ok(task_created_output(
        &target_task_id,
        &title,
        source_session,
        source_task_id,
    ))
}

/// Build a session-scoped TaskManager backed by the configured pool.
/// `user_id` is bound on the store so every INSERT writes the
/// owning user — required for the cross-session
/// `idx_session_todos_user_status_updated` index. Returns `None`
/// when no SQL pool is wired (server is in in-memory-only mode for
/// tests).
fn build_task_manager(
    state: &AppState,
    session_id: &str,
    user_id: &str,
) -> Result<Option<TaskManager>, String> {
    let Some(pool) = state.shared_pool.as_ref() else {
        return Ok(None);
    };
    let store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::from_shared_for_user(pool, user_id)?);
    Ok(Some(TaskManager::new(session_id.to_string(), store)))
}

fn validate_execute_todo_args_action(action: &str, args: &serde_json::Value) -> Result<(), String> {
    let Some(raw_action) = args.get("action") else {
        return Ok(());
    };
    let Some(arg_action) = raw_action.as_str() else {
        return Err("field 'args.action' must be a string when provided".to_string());
    };
    if arg_action != action {
        return Err(format!(
            "field 'args.action' ('{arg_action}') must match request action ('{action}')"
        ));
    }
    Ok(())
}

fn todo_idempotency_error_response(error: String) -> (StatusCode, Json<ErrorResponse>) {
    let status = if error.contains("different todo create arguments")
        || error.contains("already in progress")
    {
        StatusCode::CONFLICT
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    error_response(status, error)
}

fn validate_create_idempotency_key(key: Option<&str>) -> Result<&str, String> {
    let Some(key) = key.map(str::trim).filter(|key| !key.is_empty()) else {
        return Err("idempotency_key is required for todo action 'create'".to_string());
    };
    if key.len() > 128 {
        return Err("idempotency_key must be at most 128 bytes".to_string());
    }
    if !key
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'))
    {
        return Err(
            "idempotency_key may contain only ASCII letters, digits, '-', '_', ':' or '.'"
                .to_string(),
        );
    }
    Ok(key)
}

enum TodoCreateIdempotency {
    Reserved,
    Replay(String),
}

async fn lookup_todo_idempotency_output(
    pool: &astra_core::SharedPool,
    session_id: &str,
    user_id: &str,
    action: &str,
    idempotency_key: &str,
) -> Result<Option<(String, Option<String>)>, String> {
    sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT args_json, output FROM session_todo_idempotency \
         WHERE session_id = ? AND user_id = ? AND action = ? AND idempotency_key = ?",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(action)
    .bind(idempotency_key)
    .fetch_optional(pool.get())
    .await
    .map_err(|e| format!("lookup todo idempotency key failed: {e}"))
}

async fn claim_todo_create_idempotency(
    pool: &astra_core::SharedPool,
    session_id: &str,
    user_id: &str,
    idempotency_key: &str,
    args: &serde_json::Value,
) -> Result<TodoCreateIdempotency, String> {
    let args_json =
        serde_json::to_string(args).map_err(|e| format!("encode todo create args: {e}"))?;
    let insert_result = sqlx::query(
        "INSERT INTO session_todo_idempotency \
            (session_id, user_id, action, idempotency_key, args_json, output, created_at, updated_at) \
         VALUES (?, ?, 'create', ?, ?, NULL, NOW(6), NOW(6))",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(idempotency_key)
    .bind(&args_json)
    .execute(pool.get())
    .await;

    let insert_error = match insert_result {
        Ok(_) => return Ok(TodoCreateIdempotency::Reserved),
        Err(error) => error,
    };

    let existing =
        lookup_todo_idempotency_output(pool, session_id, user_id, "create", idempotency_key)
            .await?;
    let Some((existing_args, mut output)) = existing else {
        return Err(format!(
            "reserve todo create idempotency key failed: {insert_error}"
        ));
    };
    if existing_args != args_json {
        return Err("idempotency_key already used for different todo create arguments".to_string());
    }

    for _ in 0..20 {
        if let Some(output) = output {
            return Ok(TodoCreateIdempotency::Replay(output));
        }
        sleep(Duration::from_millis(50)).await;
        output =
            lookup_todo_idempotency_output(pool, session_id, user_id, "create", idempotency_key)
                .await?
                .and_then(|(_, output)| output);
    }

    Err("idempotency_key is already in progress for todo create".to_string())
}

async fn complete_todo_create_idempotency(
    pool: &astra_core::SharedPool,
    session_id: &str,
    user_id: &str,
    idempotency_key: &str,
    output: &str,
) -> Result<(), String> {
    let result = sqlx::query(
        "UPDATE session_todo_idempotency \
         SET output = ?, updated_at = NOW(6) \
         WHERE session_id = ? AND user_id = ? AND action = 'create' AND idempotency_key = ?",
    )
    .bind(output)
    .bind(session_id)
    .bind(user_id)
    .bind(idempotency_key)
    .execute(pool.get())
    .await
    .map_err(|e| format!("record todo create idempotency output failed: {e}"))?;
    if result.rows_affected() != 1 {
        return Err(format!(
            "record todo create idempotency output affected {} rows",
            result.rows_affected()
        ));
    }
    Ok(())
}

/// `POST /sessions/{session_id}/todos:execute` — run a TaskManager action.
pub(crate) async fn execute_todo_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Json(req): Json<ExecuteTodoRequest>,
) -> Result<Json<ExecuteTodoResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    verify_session_owner(&state, &user.user_id, &session_id).await?;

    let action = req.action.trim();
    if action.is_empty() {
        return Ok(Json(ExecuteTodoResponse {
            output: "Error: field 'action' must be non-empty".to_string(),
        }));
    }

    if let Err(error) = validate_execute_todo_args_action(action, &req.args) {
        return Ok(Json(ExecuteTodoResponse {
            output: format!("Error: {error}"),
        }));
    }
    let create_idempotency_key = if action == "create" {
        match validate_create_idempotency_key(req.idempotency_key.as_deref()) {
            Ok(key) => Some(key.to_string()),
            Err(error) => {
                return Ok(Json(ExecuteTodoResponse {
                    output: format!("Error: {error}"),
                }));
            }
        }
    } else {
        None
    };
    if action != "create"
        && req
            .idempotency_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty())
    {
        return Ok(Json(ExecuteTodoResponse {
            output: "Error: idempotency_key is only valid for todo action 'create'".to_string(),
        }));
    }

    let manager = build_task_manager(&state, &session_id, &user.user_id)
        .map_err(|error| error_response(StatusCode::INTERNAL_SERVER_ERROR, error))?
        .ok_or_else(|| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "session_todos store not configured on this server",
            )
        })?;

    let output = match action {
        "create" => {
            let key = create_idempotency_key.expect("create idempotency key validated");
            let pool = state.shared_pool.as_ref().ok_or_else(|| {
                error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "session_todos store not configured on this server",
                )
            })?;
            match claim_todo_create_idempotency(pool, &session_id, &user.user_id, &key, &req.args)
                .await
                .map_err(todo_idempotency_error_response)?
            {
                TodoCreateIdempotency::Replay(output) => output,
                TodoCreateIdempotency::Reserved => {
                    let output = manager.create(&req.args).await;
                    // Best-effort: if this fails the sweeper will clean up the
                    // stale idempotency row. The client already received the
                    // task output — don't let idempotency tracking block that.
                    if let Err(e) = complete_todo_create_idempotency(
                        pool,
                        &session_id,
                        &user.user_id,
                        &key,
                        &output,
                    )
                    .await
                    {
                        tracing::error!(
                            target: "astra_runtime::session_todo",
                            error = %e,
                            session_id = %session_id,
                            idempotency_key = %key,
                            "complete_todo_create_idempotency failed — stale row will be \
                             cleaned by sweeper; task creation succeeded"
                        );
                    }
                    output
                }
            }
        }
        "update" => manager.update(&req.args).await,
        "list" => manager.list(&req.args).await,
        "get" => manager.get(&req.args).await,
        "stop" => manager.stop(&req.args).await,
        "adopt" => adopt_task_into_session(&state, &user.user_id, &session_id, &req.args).await,
        "archive" => manager.archive(&req.args).await,
        "fork_copy" => {
            copy_task_board_into_fork(&state, &user.user_id, &session_id, &req.args).await
        }
        other => format!(
            "Error: unknown todo action '{other}'. Valid: create, update, list, get, stop, adopt, archive"
        ),
    };

    Ok(Json(ExecuteTodoResponse { output }))
}

fn required_fork_copy_source_session(args: &serde_json::Value) -> Result<String, String> {
    let Some(obj) = args.as_object() else {
        return Err("task fork_copy arguments must be an object".to_string());
    };
    for key in obj.keys() {
        if !["action", "source_session_id"].contains(&key.as_str()) {
            return Err(format!(
                "unknown field '{key}' for task fork_copy (valid: action, source_session_id)"
            ));
        }
    }
    let Some(raw) = obj.get("source_session_id") else {
        return Err("'source_session_id' is required for task fork_copy".to_string());
    };
    let Some(value) = raw.as_str() else {
        return Err("'source_session_id' must be a string for task fork_copy".to_string());
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(
            "'source_session_id' must be a non-empty string for task fork_copy".to_string(),
        );
    }
    Ok(trimmed.to_string())
}

fn prepare_fork_copy_snapshot_for_target(
    mut source_snapshot: astra_tools::task_mgmt::TaskManagerSnapshot,
    target_version: u64,
) -> astra_tools::task_mgmt::TaskManagerSnapshot {
    source_snapshot = prepare_task_snapshot_for_fork(source_snapshot);
    source_snapshot.version = target_version;
    source_snapshot.restore_version = Some(target_version);
    source_snapshot
}

/// Internal fork support: copy the source session task board into an
/// empty forked child without migrating the source. This is intentionally
/// distinct from `adopt`, which is a user-facing cross-session move.
async fn copy_task_board_into_fork(
    state: &AppState,
    user_id: &str,
    target_session: &str,
    args: &serde_json::Value,
) -> String {
    let source_session = match required_fork_copy_source_session(args) {
        Ok(source_session) => source_session,
        Err(error) => return format!("Error: {error}"),
    };
    if source_session == target_session {
        return "Error: source_session_id matches the fork target session".to_string();
    }
    if let Err((status, body)) = verify_session_owner(state, user_id, &source_session).await {
        return format!(
            "Error: source session ownership check failed for task fork_copy: {} {}",
            status, body.detail
        );
    }

    let source_manager = match build_task_manager(state, &source_session, user_id) {
        Ok(Some(manager)) => manager,
        Ok(None) => return "Error: session_todos store not configured on this server".to_string(),
        Err(error) => return format!("Error: {error}"),
    };
    let target_manager = match build_task_manager(state, target_session, user_id) {
        Ok(Some(manager)) => manager,
        Ok(None) => return "Error: session_todos store not configured on this server".to_string(),
        Err(error) => return format!("Error: {error}"),
    };

    let target_snapshot = match target_manager.try_snapshot_state().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return format!("Error: load fork target task board {target_session}: {error}");
        }
    };
    if !target_snapshot.tasks.is_empty() {
        return format!(
            "Fork task board preserved: target already has {} task(s)\n{}",
            target_snapshot.tasks.len(),
            serde_json::json!({
                "success": true,
                "status": "preserved_existing_child",
                "source_session_id": source_session,
                "target_session_id": target_session,
                "count": target_snapshot.tasks.len(),
            })
        );
    }

    let snapshot = match source_manager.try_snapshot_state().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return format!("Error: load fork source task board {source_session}: {error}");
        }
    };
    let copied = snapshot.tasks.len();
    let snapshot = prepare_fork_copy_snapshot_for_target(snapshot, target_snapshot.version);
    if let Err(error) = target_manager.restore_snapshot(&snapshot).await {
        return format!("Error: copy fork task board into {target_session}: {error}");
    }

    format!(
        "Fork task board copied: {copied} task(s)\n{}",
        serde_json::json!({
            "success": true,
            "status": "copied",
            "source_session_id": source_session,
            "target_session_id": target_session,
            "count": copied,
        })
    )
}

/// `adopt`: copy a task from another of the user's sessions into the
/// current session, mark the source `migrated`. Server-side
/// implementation since it spans two sessions and requires user-id
/// ownership cross-check.
async fn adopt_task_into_session(
    state: &AppState,
    user_id: &str,
    target_session: &str,
    args: &serde_json::Value,
) -> String {
    if let Err(error) = validate_adopt_args(args) {
        return format!("Error: {error}");
    }
    let source_session = match required_adopt_string(args, "source_session_id") {
        Ok(value) => value,
        Err(error) => return format!("Error: {error}"),
    };
    let source_task_id = match required_adopt_string(args, "task_id") {
        Ok(value) => value,
        Err(error) => return format!("Error: {error}"),
    };
    if source_session == target_session {
        return format!(
            "No-op: task '{source_task_id}' is already in the current session\n{}",
            serde_json::json!({
                "success": true,
                "noop": true,
                "reason": "source_session_is_current_session",
                "source_session_id": source_session,
                "target_session_id": target_session,
                "task_id": source_task_id,
                "message": "Task is already in the current session; continue with task_board.update/task_board.get."
            })
        );
    }
    let Some(pool) = state.shared_pool.as_ref() else {
        return "Error: session_todos store not configured on this server".to_string();
    };

    match adopt_task_into_session_atomic(
        pool,
        user_id,
        &source_session,
        &source_task_id,
        target_session,
    )
    .await
    {
        Ok(output) => output,
        Err(error) if error.starts_with("Refused:") => error,
        Err(error) => format!("Error: {error}"),
    }
}

/// `GET /sessions/{session_id}/todos` — load the full task list.
/// Used by the CLI's task board observer to render the dashboard
/// without a per-action round-trip.
pub(crate) async fn load_todos_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Result<Json<LoadTodosResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    verify_session_owner(&state, &user.user_id, &session_id).await?;

    let pool = state.shared_pool.as_ref().ok_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "session_todos store not configured on this server",
        )
    })?;
    let store = MatrixOneTaskStore::from_shared_for_user(pool, &user.user_id)
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let tasks = store
        .load(&session_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(LoadTodosResponse { tasks }))
}

/// `GET /users/me/todos` — cross-session task view for the current
/// user. Index `idx_session_todos_user_status_updated` covers the
/// (user_id, status, updated_at) lookup. Returns the lightweight
/// `UserTodoEntry` shape so the LLM can quickly see "what's open
/// across all my sessions" without loading every task's full body
/// (subtasks, blocks, metadata stay session-local).
pub(crate) async fn list_user_todos_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<UserTodosQuery>,
) -> Result<Json<UserTodosResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let pool = state.shared_pool.as_ref().ok_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "session_todos store not configured on this server",
        )
    })?;

    let status_filter = normalize_user_todos_status_filter(query.status.as_deref())?;

    let invalid_status: Option<String> = sqlx::query_scalar(
        "SELECT st.status \
         FROM session_todos st FORCE INDEX (idx_session_todos_user_status_updated) \
         WHERE st.user_id = ? \
           AND st.status NOT IN ('pending', 'in_progress', 'paused', 'completed', 'failed', 'cancelled', 'archived', 'deleted', 'migrated') \
         ORDER BY st.updated_at DESC LIMIT 1",
    )
    .bind(&user.user_id)
    .fetch_optional(pool.get())
    .await
    .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some(status) = invalid_status {
        return Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("session_todos.status contains invalid status '{status}'"),
        ));
    }

    // U-12: LEFT JOIN agent_sessions so each row carries session
    // recency context. The `created_at` / `title` from agent_sessions
    // is NULL when the session was deleted — we surface it as
    // `session_started_at: null` so the client can still render the
    // task (better than silently dropping it).
    //
    // Result tuple: (session_id, todo_id, title, status,
    //                todo_updated_at, session_started_at?, session_title?)
    type Row = (
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    );
    let row_to_entry = |(
        session_id,
        todo_id,
        title,
        status,
        updated_at,
        session_started_at,
        session_title,
    ): Row|
     -> Result<UserTodoEntry, (StatusCode, Json<ErrorResponse>)> {
        if !is_known_persisted_todo_status(&status) {
            return Err(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("session_todos.status contains invalid status '{status}'"),
            ));
        }
        Ok(UserTodoEntry {
            session_id,
            todo_id,
            title,
            status,
            updated_at,
            session_started_at,
            session_title,
        })
    };

    let rows: Vec<UserTodoEntry> = match status_filter {
        "active" => sqlx::query_as::<_, Row>(
            "SELECT st.session_id, st.todo_id, st.title, st.status, \
                    CAST(st.updated_at AS CHAR) AS updated_at, \
                    CAST(s.created_at AS CHAR) AS session_started_at, \
                    s.title AS session_title \
             FROM session_todos st FORCE INDEX (idx_session_todos_user_status_updated) \
             LEFT JOIN agent_sessions s ON s.session_id = st.session_id \
             WHERE st.user_id = ? \
               AND st.status IN ('pending', 'in_progress', 'paused') \
             ORDER BY st.updated_at DESC LIMIT 200",
        )
        .bind(&user.user_id)
        .fetch_all(pool.get())
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .into_iter()
        .map(row_to_entry)
        .collect::<Result<Vec<_>, _>>()?,
        "all" => sqlx::query_as::<_, Row>(
            "SELECT st.session_id, st.todo_id, st.title, st.status, \
                    CAST(st.updated_at AS CHAR) AS updated_at, \
                    CAST(s.created_at AS CHAR) AS session_started_at, \
                    s.title AS session_title \
             FROM session_todos st FORCE INDEX (idx_session_todos_user_status_updated) \
             LEFT JOIN agent_sessions s ON s.session_id = st.session_id \
             WHERE st.user_id = ? \
             ORDER BY st.updated_at DESC LIMIT 200",
        )
        .bind(&user.user_id)
        .fetch_all(pool.get())
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .into_iter()
        .map(row_to_entry)
        .collect::<Result<Vec<_>, _>>()?,
        other => sqlx::query_as::<_, Row>(
            "SELECT st.session_id, st.todo_id, st.title, st.status, \
                    CAST(st.updated_at AS CHAR) AS updated_at, \
                    CAST(s.created_at AS CHAR) AS session_started_at, \
                    s.title AS session_title \
             FROM session_todos st FORCE INDEX (idx_session_todos_user_status_updated) \
             LEFT JOIN agent_sessions s ON s.session_id = st.session_id \
             WHERE st.user_id = ? AND st.status = ? \
             ORDER BY st.updated_at DESC LIMIT 200",
        )
        .bind(&user.user_id)
        .bind(other)
        .fetch_all(pool.get())
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .into_iter()
        .map(row_to_entry)
        .collect::<Result<Vec<_>, _>>()?,
    };

    let total = rows.len();
    Ok(Json(UserTodosResponse { tasks: rows, total }))
}

/// Confirm `session_id` is owned by `user_id`. Without this check a
/// caller could pass any session_id and read/write its todos.
/// `SessionService::get_session` already enforces the ownership
/// check (returns 404 for non-owned sessions to avoid leaking
/// existence) — we just propagate the error.
async fn verify_session_owner(
    state: &AppState,
    user_id: &str,
    session_id: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    state
        .session_service
        .get_session(session_id.to_string(), user_id.to_string())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_core::{MatrixOneSettings, SharedPool};
    use astra_services::auth::{
        AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
        AuthTokenRecord, AuthUserRecord, SessionActivityRecord, SessionCreateRequestData,
        SessionListFilter, SessionListRecord, SessionRecord, SessionService,
        SessionUpdateRequestData,
    };
    use astra_services::storage::ensure_core_schema;

    #[test]
    fn user_todos_status_filter_validation_rejects_typos() {
        assert_eq!(
            normalize_user_todos_status_filter(None).expect("default"),
            "active"
        );
        assert_eq!(
            normalize_user_todos_status_filter(Some("completed")).expect("completed"),
            "completed"
        );

        let typo = normalize_user_todos_status_filter(Some("cancelledd")).unwrap_err();
        assert_eq!(typo.0, StatusCode::BAD_REQUEST);
        assert!(
            typo.1.0.detail.contains("invalid status") && typo.1.0.detail.contains("cancelled"),
            "typo should return an actionable valid-status message: {:?}",
            typo.1.0.detail
        );
    }

    #[test]
    fn user_todos_query_rejects_unknown_fields() {
        let valid = serde_json::from_value::<UserTodosQuery>(serde_json::json!({
            "status": "completed"
        }))
        .expect("status is the only supported user todos query field");
        assert_eq!(valid.status.as_deref(), Some("completed"));

        let typo = serde_json::from_value::<UserTodosQuery>(serde_json::json!({
            "stats": "completed"
        }));
        assert!(
            typo.is_err(),
            "unknown /users/me/todos query parameters should not be ignored"
        );
    }

    #[test]
    fn fork_copy_required_source_session_rejects_bad_shapes() {
        for (args, expected) in [
            (serde_json::json!(null), "arguments must be an object"),
            (serde_json::json!({}), "source_session_id"),
            (
                serde_json::json!({"source_session_id": true}),
                "must be a string",
            ),
            (serde_json::json!({"source_session_id": "   "}), "non-empty"),
            (
                serde_json::json!({"source_session_id": "s1", "task_id": "task-1"}),
                "unknown field 'task_id'",
            ),
        ] {
            let err = required_fork_copy_source_session(&args)
                .expect_err("bad fork_copy args should be rejected");
            assert!(
                err.contains(expected),
                "expected {expected:?} in error for {args}: {err}"
            );
        }
        assert_eq!(
            required_fork_copy_source_session(&serde_json::json!({
                "source_session_id": "  parent-session  ",
            }))
            .expect("valid fork_copy source"),
            "parent-session"
        );
    }

    #[test]
    fn adopt_duplicate_refusal_is_structured_and_non_successful() {
        let output = adopt_duplicate_refusal("source-s", "task-7", "task-2", "Ship feature");
        assert!(output.starts_with("Refused:"), "{output}");
        assert!(output.contains("open task"), "{output}");
        let body = output
            .find('{')
            .and_then(|pos| serde_json::from_str::<serde_json::Value>(&output[pos..]).ok())
            .expect("structured body");
        assert_eq!(body["success"], false);
        assert_eq!(body["duplicate_of"], "task-2");
        assert!(
            body["message"].as_str().unwrap().contains("archive/rename"),
            "{output}"
        );
    }

    #[tokio::test]
    async fn adopt_same_session_is_idempotent_noop_before_store_access() {
        let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy));
        let output = adopt_task_into_session(
            &state,
            "user-1",
            "session-1",
            &serde_json::json!({
                "source_session_id": "session-1",
                "task_id": "task-7",
            }),
        )
        .await;

        assert!(!output.starts_with("Error:"), "{output}");
        let body = output
            .find('{')
            .and_then(|pos| serde_json::from_str::<serde_json::Value>(&output[pos..]).ok())
            .expect("same-session adopt should return structured no-op JSON");
        assert_eq!(body["success"], true);
        assert_eq!(body["noop"], true);
        assert_eq!(body["reason"], "source_session_is_current_session");
        assert_eq!(body["task_id"], "task-7");
    }

    #[test]
    fn adoptable_source_status_is_open_work_only() {
        for status in ["pending", "in_progress", "paused"] {
            assert!(
                adoptable_source_status(status),
                "{status} should be adoptable"
            );
        }
        for status in [
            "completed",
            "failed",
            "cancelled",
            "archived",
            "deleted",
            "migrated",
        ] {
            assert!(
                !adoptable_source_status(status),
                "{status} should not be adoptable"
            );
        }
    }

    #[test]
    fn adopt_required_strings_reject_missing_wrong_type_and_blank_values() {
        for (label, args) in [
            ("missing", serde_json::json!({})),
            ("wrong type", serde_json::json!({"source_session_id": true})),
            ("blank", serde_json::json!({"source_session_id": "   "})),
        ] {
            let err = required_adopt_string(&args, "source_session_id")
                .expect_err("bad source_session_id should be rejected");
            assert!(err.contains("source_session_id"), "{label}: {err}");
        }

        let trimmed =
            required_adopt_string(&serde_json::json!({"task_id": "  task-7  "}), "task_id")
                .expect("valid task_id");
        assert_eq!(trimmed, "task-7");
    }

    #[test]
    fn validate_adopt_args_rejects_unknown_fields_before_db_work() {
        let unknown = validate_adopt_args(&serde_json::json!({
            "source_session_id": "source",
            "task_id": "task-1",
            "copy_edges": true
        }))
        .expect_err("unknown adopt fields should be rejected");
        assert!(
            unknown.contains("copy_edges") && unknown.contains("unknown field"),
            "{unknown}"
        );

        let wrong_action_type = validate_adopt_args(&serde_json::json!({
            "action": true,
            "source_session_id": "source",
            "task_id": "task-1"
        }))
        .expect_err("wrong-type action should be rejected");
        assert!(
            wrong_action_type.contains("field 'action'") && wrong_action_type.contains("string"),
            "{wrong_action_type}"
        );
    }

    #[test]
    fn validate_execute_todo_args_action_rejects_mismatched_nested_action() {
        validate_execute_todo_args_action(
            "update",
            &serde_json::json!({"action": "update", "task_id": "task-1"}),
        )
        .expect("matching nested action should be accepted");
        validate_execute_todo_args_action("update", &serde_json::json!({"task_id": "task-1"}))
            .expect("omitted nested action should be accepted");

        let wrong_type =
            validate_execute_todo_args_action("update", &serde_json::json!({"action": true}))
                .expect_err("wrong-type nested action should be rejected");
        assert!(
            wrong_type.contains("args.action") && wrong_type.contains("string"),
            "{wrong_type}"
        );

        let mismatch = validate_execute_todo_args_action(
            "update",
            &serde_json::json!({"action": "create", "task_id": "task-1"}),
        )
        .expect_err("mismatched nested action should be rejected");
        assert!(
            mismatch.contains("args.action")
                && mismatch.contains("create")
                && mismatch.contains("update"),
            "{mismatch}"
        );
    }

    #[test]
    fn execute_todo_request_rejects_unknown_top_level_fields() {
        let parsed = serde_json::from_value::<ExecuteTodoRequest>(serde_json::json!({
            "action": "list",
            "args": {},
            "status": "active"
        }));
        assert!(
            parsed.is_err(),
            "todos:execute should reject unknown top-level request fields instead of ignoring them"
        );
    }

    #[test]
    fn prepare_fork_copy_snapshot_for_target_rebases_version_guard_to_child() {
        let source_snapshot = astra_tools::task_mgmt::TaskManagerSnapshot {
            tasks: vec![astra_tools::task_mgmt::SessionTask {
                id: "task-1".to_string(),
                title: "Carry forked work".to_string(),
                description: None,
                status: astra_tools::task_mgmt::SessionTaskStatusKind::InProgress,
                subtasks: vec![],
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                active_form: None,
                owner: None,
                metadata: None,
                blocks: vec![],
                blocked_by: vec![],
                archived_at: None,
            }],
            next_task_id: 2,
            version: 7,
            restore_version: Some(9),
        };

        let prepared = prepare_fork_copy_snapshot_for_target(source_snapshot, 3);

        assert_eq!(prepared.version, 3);
        assert_eq!(prepared.restore_version, Some(3));
        assert_eq!(
            prepared.tasks[0].status,
            astra_tools::task_mgmt::SessionTaskStatusKind::Paused
        );
        assert_eq!(
            prepared.tasks[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("fork_copied_from_status"))
                .and_then(|value| value.as_str()),
            Some("in_progress")
        );
    }

    #[test]
    fn clean_adopted_subtasks_resets_progress_and_preserves_valid_dependencies() {
        let cleaned = clean_adopted_subtasks(
            r#"[
                {
                    "id": "s1",
                    "title": "First",
                    "status": "completed",
                    "owner": "agent-a",
                    "depends_on": ["s2"]
                },
                {
                    "id": "s2",
                    "title": "Second",
                    "description": "keep this",
                    "status": "in_progress",
                    "depends_on": ["s1"]
                }
            ]"#,
        )
        .expect("valid source subtasks")
        .expect("non-empty subtasks");

        assert_eq!(
            cleaned,
            serde_json::json!([
                {
                    "id": "s1",
                    "title": "First",
                    "status": "pending",
                    "owner": "agent-a",
                    "depends_on": ["s2"]
                },
                {
                    "id": "s2",
                    "title": "Second",
                    "description": "keep this",
                    "status": "pending",
                    "depends_on": ["s1"]
                }
            ])
        );
    }

    #[test]
    fn clean_adopted_subtasks_rejects_corrupt_payloads_instead_of_dropping_work() {
        let cases = [
            ("not-json", "invalid JSON"),
            (r#"{"id":"s1"}"#, "must be an array"),
            (r#"[true]"#, "subtasks[0] must be an object"),
            (r#"[{"id":"","title":"nope"}]"#, "subtasks[0].id"),
            (r#"[{"title":"missing id"}]"#, "subtasks[0].id"),
            (r#"[{"id":"s1"}]"#, "subtasks[0].title"),
            (
                r#"[{"id":"s1","title":"one"},{"id":"s1","title":"dup"}]"#,
                "duplicate source subtask id",
            ),
            (
                r#"[{"id":"s1","title":"one","depends_on":[true]}]"#,
                "depends_on[0]",
            ),
            (
                r#"[{"id":"s1","title":"one","depends_on":["missing"]}]"#,
                "unknown dependency",
            ),
            (
                r#"[{"id":"s1","title":"one","depends_on":["s1"]}]"#,
                "cannot depend on itself",
            ),
            (
                r#"[{"id":"s1","title":"one","depends_on":["s2","s2"]},{"id":"s2","title":"two"}]"#,
                "duplicate dependency",
            ),
            (
                r#"[{"id":"s1","title":"one","notes":"typo"}]"#,
                "unknown source subtasks[0].notes",
            ),
        ];

        for (payload, expected) in cases {
            let err = clean_adopted_subtasks(payload).expect_err("bad source subtasks");
            assert!(
                err.contains(expected),
                "expected {expected:?} in error for {payload}: {err}"
            );
        }

        let empty = clean_adopted_subtasks("[]").expect("empty array is valid");
        assert!(empty.is_none());
    }

    #[tokio::test]
    async fn execute_todo_handler_rejects_blank_action_before_store_access() {
        let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy))
            .with_auth_service(Arc::new(TestAuth {
                user_id: "blank-action-user".to_string(),
            }))
            .with_session_service(Arc::new(TestSessionService));

        let Json(response) = execute_todo_handler(
            State(state),
            HeaderMap::new(),
            Path("blank-action-session".to_string()),
            Json(ExecuteTodoRequest {
                action: "   ".to_string(),
                args: serde_json::json!({}),
                idempotency_key: None,
            }),
        )
        .await
        .expect("blank action returns tool error output, not HTTP/store error");
        assert!(
            response.output.starts_with("Error:")
                && response.output.contains("action")
                && response.output.contains("non-empty"),
            "blank action should be rejected before store access: {}",
            response.output
        );
    }

    #[test]
    fn create_idempotency_key_validation_is_strict() {
        assert!(validate_create_idempotency_key(Some("todo-create:abc_123.ok")).is_ok());
        assert!(validate_create_idempotency_key(None).is_err());
        assert!(validate_create_idempotency_key(Some("   ")).is_err());
        assert!(validate_create_idempotency_key(Some("bad key")).is_err());
        assert!(validate_create_idempotency_key(Some(&"x".repeat(129))).is_err());
    }

    #[test]
    fn session_todo_owner_lock_sql_is_owner_bound() {
        let normalized = SESSION_TODO_OWNER_LOCK_SQL
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_uppercase();
        assert!(normalized.contains("WHERE SESSION_ID = ? AND USER_ID = ?"));
        assert!(
            !normalized.contains("WHERE SESSION_ID = ? FOR UPDATE"),
            "session todo owner lock must not lock by session_id before checking owner"
        );
    }

    #[tokio::test]
    async fn execute_todo_handler_rejects_idempotency_key_for_non_create_before_store_access() {
        let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy))
            .with_auth_service(Arc::new(TestAuth {
                user_id: "bad-idempotency-user".to_string(),
            }))
            .with_session_service(Arc::new(TestSessionService));

        let Json(response) = execute_todo_handler(
            State(state),
            HeaderMap::new(),
            Path("bad-idempotency-session".to_string()),
            Json(ExecuteTodoRequest {
                action: "list".to_string(),
                args: serde_json::json!({}),
                idempotency_key: Some("todo-create:wrong-action".to_string()),
            }),
        )
        .await
        .expect("bad idempotency action returns tool error output");
        assert!(
            response.output.starts_with("Error:")
                && response.output.contains("only valid")
                && response.output.contains("create"),
            "non-create idempotency key should be rejected before store access: {}",
            response.output
        );
    }

    #[tokio::test]
    async fn execute_todo_handler_requires_create_idempotency_key_before_store_access() {
        let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy))
            .with_auth_service(Arc::new(TestAuth {
                user_id: "missing-idempotency-user".to_string(),
            }))
            .with_session_service(Arc::new(TestSessionService));

        let Json(response) = execute_todo_handler(
            State(state),
            HeaderMap::new(),
            Path("missing-idempotency-session".to_string()),
            Json(ExecuteTodoRequest {
                action: "create".to_string(),
                args: serde_json::json!({"title": "must not touch store"}),
                idempotency_key: None,
            }),
        )
        .await
        .expect("missing create idempotency key returns tool error output");
        assert!(
            response.output.starts_with("Error:")
                && response.output.contains("idempotency_key")
                && response.output.contains("required"),
            "missing create idempotency key should be rejected before store access: {}",
            response.output
        );
    }

    #[derive(Clone)]
    struct Healthy;

    #[async_trait]
    impl crate::HealthChecker for Healthy {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    #[derive(Clone)]
    struct TestAuth {
        user_id: String,
    }

    #[async_trait]
    impl AuthService for TestAuth {
        async fn register(
            &self,
            _request: AuthRegisterRequestData,
        ) -> Result<AuthUserRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            unreachable!()
        }

        async fn login(
            &self,
            _request: AuthLoginRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            unreachable!()
        }

        async fn refresh(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            unreachable!()
        }

        async fn logout(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<(), (StatusCode, Json<crate::ErrorResponse>)> {
            unreachable!()
        }

        async fn current_user(
            &self,
            _headers: &HeaderMap,
        ) -> Result<AuthUserRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            Ok(AuthUserRecord {
                user_id: self.user_id.clone(),
                username: self.user_id.clone(),
                email: format!("{}@example.test", self.user_id),
                display_name: None,
            })
        }
    }

    #[derive(Clone)]
    struct TestSessionService;

    #[async_trait]
    impl SessionService for TestSessionService {
        async fn create_session(
            &self,
            _user_id: String,
            _request: SessionCreateRequestData,
        ) -> Result<SessionRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            unreachable!()
        }

        async fn list_sessions(
            &self,
            _filter: SessionListFilter,
        ) -> Result<SessionListRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            unreachable!()
        }

        async fn get_session(
            &self,
            session_id: String,
            user_id: String,
        ) -> Result<SessionRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            Ok(SessionRecord {
                session_id,
                user_id,
                agent_id: None,
                title: None,
                metadata: serde_json::Map::new(),
                status: "active".to_string(),
                event_count: 0,
                created_at: "now".to_string(),
                updated_at: None,
                ended_at: None,
            })
        }

        async fn update_session(
            &self,
            _session_id: String,
            _user_id: String,
            _request: SessionUpdateRequestData,
        ) -> Result<SessionRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            unreachable!()
        }

        async fn delete_session(
            &self,
            _session_id: String,
            _user_id: String,
        ) -> Result<(), (StatusCode, Json<crate::ErrorResponse>)> {
            unreachable!()
        }

        async fn get_session_activity(
            &self,
            _session_id: String,
            _user_id: String,
            _limit: u32,
            _cursor: Option<astra_services::auth::SessionActivityCursor>,
        ) -> Result<SessionActivityRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            unreachable!()
        }
    }

    async fn bootstrap_shared_pool() -> SharedPool {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 to run this ignored test"
        );
        let mut settings = MatrixOneSettings::from_env();
        settings.db_pool_max_connections = settings.db_pool_max_connections.min(4);
        settings.db_pool_min_connections = settings.db_pool_min_connections.min(1);
        let catalog =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
        ensure_core_schema(&settings, &catalog)
            .await
            .expect("ensure_core_schema");
        SharedPool::new(&settings).await.expect("connect matrixone")
    }

    async fn cleanup_session_rows(pool: &sqlx::Pool<sqlx::MySql>, session_id: &str, user_id: &str) {
        sqlx::query("DELETE FROM session_todo_idempotency WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(pool)
            .await
            .expect("cleanup session todo handler fixture session_todo_idempotency");
        sqlx::query("DELETE FROM session_todos WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(pool)
            .await
            .expect("cleanup session todo handler fixture session_todos");
        sqlx::query("DELETE FROM session_todo_counters WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(pool)
            .await
            .expect("cleanup session todo handler fixture session_todo_counters");
        sqlx::query("DELETE FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(pool)
            .await
            .expect("cleanup session todo handler fixture agent_sessions");
    }

    async fn prepare_session_todo_owner(
        pool: &sqlx::Pool<sqlx::MySql>,
        session_id: &str,
        user_id: &str,
    ) {
        cleanup_session_rows(pool, session_id, user_id).await;
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, agent_id, title, status, metadata)
             VALUES (?, ?, 'session-todo-handler-test', 'session todo handler test', 'active', '{}')",
        )
        .bind(session_id)
        .bind(user_id)
        .execute(pool)
        .await
        .expect("insert agent_sessions owner root");
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn adopt_duplicate_target_does_not_migrate_source_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-adopt-{}", uuid::Uuid::new_v4());
        let source_session = format!("s-adopt-source-{}", uuid::Uuid::new_v4());
        let target_session = format!("s-adopt-target-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &source_session, &user_id).await;
        prepare_session_todo_owner(&pool, &target_session, &user_id).await;

        let source_store: Arc<dyn TaskStore> =
            Arc::new(MatrixOneTaskStore::from_shared_for_user(&shared, &user_id).unwrap());
        let target_store: Arc<dyn TaskStore> =
            Arc::new(MatrixOneTaskStore::from_shared_for_user(&shared, &user_id).unwrap());
        let source = TaskManager::new(source_session.clone(), source_store);
        let target = TaskManager::new(target_session.clone(), target_store);
        let source_create = source
            .create(&serde_json::json!({"title": "Carry this work"}))
            .await;
        assert!(
            source_create.contains("\"success\":true"),
            "{source_create}"
        );
        let target_create = target
            .create(&serde_json::json!({"title": "Carry this work"}))
            .await;
        assert!(
            target_create.contains("\"success\":true"),
            "{target_create}"
        );
        let target_start = target
            .update(&serde_json::json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert!(target_start.contains("\"success\":true"), "{target_start}");
        let target_pause = target
            .update(&serde_json::json!({"task_id": "task-1", "new_status": "paused"}))
            .await;
        assert!(target_pause.contains("\"success\":true"), "{target_pause}");

        let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy))
            .with_shared_pool(shared.clone());
        let out = adopt_task_into_session(
            &state,
            &user_id,
            &target_session,
            &serde_json::json!({
                "source_session_id": source_session,
                "task_id": "task-1",
            }),
        )
        .await;
        assert!(
            out.starts_with("Refused:") && out.contains("duplicate_of"),
            "paused duplicate target should be refused before migrating source: {out}"
        );

        let status: String = sqlx::query_scalar(
            "SELECT status FROM session_todos WHERE session_id = ? AND todo_id = ? AND user_id = ?",
        )
        .bind(&source_session)
        .bind("task-1")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("source status");
        assert_eq!(status, "pending", "source must remain adoptable");

        cleanup_session_rows(&pool, &source_session, &user_id).await;
        cleanup_session_rows(&pool, &target_session, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn fork_copy_clones_parent_task_board_without_migrating_source_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-fork-copy-{}", uuid::Uuid::new_v4());
        let source_session = format!("s-fork-copy-source-{}", uuid::Uuid::new_v4());
        let target_session = format!("s-fork-copy-target-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &source_session, &user_id).await;
        prepare_session_todo_owner(&pool, &target_session, &user_id).await;

        let source_store: Arc<dyn TaskStore> =
            Arc::new(MatrixOneTaskStore::from_shared_for_user(&shared, &user_id).unwrap());
        let source = TaskManager::new(source_session.clone(), source_store);
        let source_create = source
            .create(&serde_json::json!({
                "title": "Carry forked work",
                "subtasks": [{ "id": "step-1", "title": "Do first step" }]
            }))
            .await;
        assert!(
            source_create.contains("\"success\":true"),
            "{source_create}"
        );
        let source_start = source
            .update(&serde_json::json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert!(source_start.contains("\"success\":true"), "{source_start}");

        let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy))
            .with_shared_pool(shared.clone())
            .with_session_service(Arc::new(TestSessionService));
        let out = copy_task_board_into_fork(
            &state,
            &user_id,
            &target_session,
            &serde_json::json!({
                "source_session_id": source_session,
            }),
        )
        .await;
        assert!(
            out.contains("\"success\":true") && out.contains("\"status\":\"copied\""),
            "fork_copy should report copied: {out}"
        );

        let source_status: String = sqlx::query_scalar(
            "SELECT status FROM session_todos WHERE session_id = ? AND todo_id = ? AND user_id = ?",
        )
        .bind(&source_session)
        .bind("task-1")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("source status");
        assert_eq!(
            source_status, "in_progress",
            "fork_copy must not migrate or otherwise alter the parent task"
        );

        let (target_status, target_metadata): (String, Option<String>) = sqlx::query_as(
            "SELECT status, metadata FROM session_todos WHERE session_id = ? AND todo_id = ? AND user_id = ?",
        )
        .bind(&target_session)
        .bind("task-1")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("target status");
        assert_eq!(
            target_status, "paused",
            "forked child should inherit work, not parent live execution state"
        );
        let target_metadata: serde_json::Value =
            serde_json::from_str(target_metadata.as_deref().unwrap_or("{}"))
                .expect("target metadata json");
        assert_eq!(
            target_metadata["fork_copied_from_status"], "in_progress",
            "fork copy should retain why the child task was paused"
        );

        let preserve = copy_task_board_into_fork(
            &state,
            &user_id,
            &target_session,
            &serde_json::json!({
                "source_session_id": source_session,
            }),
        )
        .await;
        assert!(
            preserve.contains("\"status\":\"preserved_existing_child\""),
            "second fork_copy must preserve existing child board: {preserve}"
        );

        cleanup_session_rows(&pool, &source_session, &user_id).await;
        cleanup_session_rows(&pool, &target_session, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn list_user_active_includes_paused_open_work_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-user-active-{}", uuid::Uuid::new_v4());
        let pending_session = format!("s-user-active-pending-{}", uuid::Uuid::new_v4());
        let running_session = format!("s-user-active-running-{}", uuid::Uuid::new_v4());
        let paused_session = format!("s-user-active-paused-{}", uuid::Uuid::new_v4());
        let completed_session = format!("s-user-active-completed-{}", uuid::Uuid::new_v4());
        for session_id in [
            &pending_session,
            &running_session,
            &paused_session,
            &completed_session,
        ] {
            prepare_session_todo_owner(&pool, session_id, &user_id).await;
        }

        let make_manager = |session_id: &str| {
            let store: Arc<dyn TaskStore> =
                Arc::new(MatrixOneTaskStore::from_shared_for_user(&shared, &user_id).unwrap());
            TaskManager::new(session_id.to_string(), store)
        };
        let pending = make_manager(&pending_session);
        let running = make_manager(&running_session);
        let paused = make_manager(&paused_session);
        let completed = make_manager(&completed_session);

        let created = pending
            .create(&serde_json::json!({"title": "pending cross-session work"}))
            .await;
        assert!(created.contains("\"success\":true"), "{created}");
        let created = running
            .create(&serde_json::json!({"title": "running cross-session work"}))
            .await;
        assert!(created.contains("\"success\":true"), "{created}");
        let updated = running
            .update(&serde_json::json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert!(updated.contains("\"success\":true"), "{updated}");
        let created = paused
            .create(&serde_json::json!({"title": "paused cross-session work"}))
            .await;
        assert!(created.contains("\"success\":true"), "{created}");
        let updated = paused
            .update(&serde_json::json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert!(updated.contains("\"success\":true"), "{updated}");
        let updated = paused
            .update(&serde_json::json!({"task_id": "task-1", "new_status": "paused"}))
            .await;
        assert!(updated.contains("\"success\":true"), "{updated}");
        let created = completed
            .create(&serde_json::json!({"title": "completed cross-session work"}))
            .await;
        assert!(created.contains("\"success\":true"), "{created}");
        let updated = completed
            .update(&serde_json::json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert!(updated.contains("\"success\":true"), "{updated}");
        let updated = completed
            .update(&serde_json::json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        assert!(updated.contains("\"success\":true"), "{updated}");

        let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy))
            .with_shared_pool(shared.clone())
            .with_auth_service(Arc::new(TestAuth {
                user_id: user_id.clone(),
            }));
        let Json(response) = list_user_todos_handler(
            State(state),
            HeaderMap::new(),
            Query(UserTodosQuery { status: None }),
        )
        .await
        .expect("list user todos");

        let titles: Vec<&str> = response
            .tasks
            .iter()
            .map(|task| task.title.as_str())
            .collect();
        assert!(
            titles.contains(&"pending cross-session work"),
            "default active view should include pending work: {titles:?}"
        );
        assert!(
            titles.contains(&"running cross-session work"),
            "default active view should include in-progress work: {titles:?}"
        );
        assert!(
            titles.contains(&"paused cross-session work"),
            "default active view should include paused open work for resume/fork: {titles:?}"
        );
        assert!(
            !titles.contains(&"completed cross-session work"),
            "default active view should not include completed history: {titles:?}"
        );

        for session_id in [
            &pending_session,
            &running_session,
            &paused_session,
            &completed_session,
        ] {
            cleanup_session_rows(&pool, session_id, &user_id).await;
        }
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn list_user_todos_fails_closed_on_unknown_persisted_status_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-user-todos-corrupt-status-{}", uuid::Uuid::new_v4());
        let session_id = format!("s-user-todos-corrupt-status-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &session_id, &user_id).await;

        let store: Arc<dyn TaskStore> =
            Arc::new(MatrixOneTaskStore::from_shared_for_user(&shared, &user_id).unwrap());
        let manager = TaskManager::new(session_id.clone(), store);
        let created = manager
            .create(&serde_json::json!({"title": "corrupt cross-session status"}))
            .await;
        assert!(created.contains("\"success\":true"), "{created}");
        sqlx::query(
            "UPDATE session_todos SET status = ? \
             WHERE session_id = ? AND todo_id = ? AND user_id = ?",
        )
        .bind("mystery")
        .bind(&session_id)
        .bind("task-1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("seed corrupt status");

        let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy))
            .with_shared_pool(shared.clone())
            .with_auth_service(Arc::new(TestAuth {
                user_id: user_id.clone(),
            }));
        let err = match list_user_todos_handler(
            State(state),
            HeaderMap::new(),
            Query(UserTodosQuery { status: None }),
        )
        .await
        {
            Ok(_) => panic!("corrupt task status must not be hidden by default active query"),
            Err(err) => err,
        };
        assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            err.1.0.detail.contains("session_todos.status")
                && err.1.0.detail.contains("invalid status")
                && err.1.0.detail.contains("mystery"),
            "bad status should be surfaced explicitly: {:?}",
            err.1.0
        );

        cleanup_session_rows(&pool, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn adopt_source_without_description_clones_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-adopt-empty-desc-{}", uuid::Uuid::new_v4());
        let source_session = format!("s-adopt-empty-desc-source-{}", uuid::Uuid::new_v4());
        let target_session = format!("s-adopt-empty-desc-target-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &source_session, &user_id).await;
        prepare_session_todo_owner(&pool, &target_session, &user_id).await;

        let source_store: Arc<dyn TaskStore> =
            Arc::new(MatrixOneTaskStore::from_shared_for_user(&shared, &user_id).unwrap());
        let source = TaskManager::new(source_session.clone(), source_store);
        let created = source
            .create(&serde_json::json!({"title": "Adopt me without description"}))
            .await;
        assert!(created.contains("\"success\":true"), "{created}");

        let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy))
            .with_shared_pool(shared.clone());
        let out = adopt_task_into_session(
            &state,
            &user_id,
            &target_session,
            &serde_json::json!({
                "source_session_id": source_session,
                "task_id": "task-1",
            }),
        )
        .await;
        assert!(
            out.contains("\"success\":true") && out.contains("task-1"),
            "adopt should clone sources with no description instead of passing description=null: {out}"
        );

        let source_status: String = sqlx::query_scalar(
            "SELECT status FROM session_todos WHERE session_id = ? AND todo_id = ? AND user_id = ?",
        )
        .bind(&source_session)
        .bind("task-1")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("source status");
        assert_eq!(source_status, "migrated");
        let source_tasks = source
            .load_tasks()
            .await
            .expect("source task board should decode migrated tombstone");
        assert_eq!(source_tasks[0].status.as_str(), "migrated");

        let target_row: (String, Option<String>, String, Option<String>) = sqlx::query_as(
            "SELECT title, description, status, metadata \
             FROM session_todos WHERE session_id = ? AND todo_id = ? AND user_id = ?",
        )
        .bind(&target_session)
        .bind("task-1")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("target clone row");
        assert_eq!(target_row.0, "Adopt me without description");
        assert_eq!(target_row.1, None);
        assert_eq!(target_row.2, "pending");
        let metadata: serde_json::Value =
            serde_json::from_str(target_row.3.as_deref().unwrap_or("{}")).unwrap();
        assert_eq!(metadata["forked_from"], format!("{source_session}:task-1"));

        cleanup_session_rows(&pool, &source_session, &user_id).await;
        cleanup_session_rows(&pool, &target_session, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn adopt_rejects_corrupt_source_subtasks_before_migrating_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-adopt-bad-subtasks-{}", uuid::Uuid::new_v4());
        let source_session = format!("s-adopt-bad-src-{}", uuid::Uuid::new_v4());
        let target_session = format!("s-adopt-bad-dst-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &source_session, &user_id).await;
        prepare_session_todo_owner(&pool, &target_session, &user_id).await;

        let source_store: Arc<dyn TaskStore> =
            Arc::new(MatrixOneTaskStore::from_shared_for_user(&shared, &user_id).unwrap());
        let source = TaskManager::new(source_session.clone(), source_store);
        let created = source
            .create(&serde_json::json!({"title": "Adopt me with corrupt subtasks"}))
            .await;
        assert!(created.contains("\"success\":true"), "{created}");

        let corrupt_subtasks = serde_json::json!([
            {
                "id": "s1",
                "title": "First",
                "status": "completed",
                "depends_on": ["missing"]
            }
        ])
        .to_string();
        sqlx::query(
            "UPDATE session_todos SET subtasks = ? \
             WHERE session_id = ? AND todo_id = ? AND user_id = ?",
        )
        .bind(&corrupt_subtasks)
        .bind(&source_session)
        .bind("task-1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("seed corrupt subtasks");

        let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy))
            .with_shared_pool(shared.clone());
        let out = adopt_task_into_session(
            &state,
            &user_id,
            &target_session,
            &serde_json::json!({
                "source_session_id": source_session,
                "task_id": "task-1",
            }),
        )
        .await;
        assert!(
            out.starts_with("Error:")
                && out.contains("invalid subtasks")
                && out.contains("unknown dependency"),
            "adopt should reject corrupt source subtasks before migration: {out}"
        );

        let source_status: String = sqlx::query_scalar(
            "SELECT status FROM session_todos \
             WHERE session_id = ? AND todo_id = ? AND user_id = ?",
        )
        .bind(&source_session)
        .bind("task-1")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("source status");
        assert_eq!(
            source_status, "pending",
            "source must remain adoptable when source subtasks are corrupt"
        );
        let target_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_todos \
             WHERE session_id = ? AND user_id = ?",
        )
        .bind(&target_session)
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("target row count");
        assert_eq!(target_rows, 0, "rejected adopt must not create a clone");

        cleanup_session_rows(&pool, &source_session, &user_id).await;
        cleanup_session_rows(&pool, &target_session, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn adopt_does_not_clone_cross_task_dependency_edges_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-adopt-clean-edges-{}", uuid::Uuid::new_v4());
        let source_session = format!("s-adopt-clean-edges-src-{}", uuid::Uuid::new_v4());
        let target_session = format!("s-adopt-clean-edges-dst-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &source_session, &user_id).await;
        prepare_session_todo_owner(&pool, &target_session, &user_id).await;

        let source_store: Arc<dyn TaskStore> =
            Arc::new(MatrixOneTaskStore::from_shared_for_user(&shared, &user_id).unwrap());
        let source = TaskManager::new(source_session.clone(), source_store);
        let producer = source
            .create(&serde_json::json!({"title": "Adopt producer"}))
            .await;
        assert!(producer.contains("\"success\":true"), "{producer}");
        let consumer = source
            .create(&serde_json::json!({"title": "Adopt consumer"}))
            .await;
        assert!(consumer.contains("\"success\":true"), "{consumer}");
        let linked = source
            .update(&serde_json::json!({"task_id": "task-1", "add_blocks": ["task-2"]}))
            .await;
        assert!(!linked.starts_with("Error:"), "{linked}");

        let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy))
            .with_shared_pool(shared.clone());
        let out = adopt_task_into_session(
            &state,
            &user_id,
            &target_session,
            &serde_json::json!({
                "source_session_id": source_session,
                "task_id": "task-1",
            }),
        )
        .await;
        assert!(
            out.contains("\"success\":true") && out.contains("task-1"),
            "adopt should clone source task without source-session dependency edges: {out}"
        );

        let target_row: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT blocks, blocked_by FROM session_todos \
             WHERE session_id = ? AND todo_id = ? AND user_id = ?",
        )
        .bind(&target_session)
        .bind("task-1")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("target dependency columns");
        assert_eq!(
            target_row,
            (None, None),
            "single-task adopt must not bring source-session task edges into target"
        );

        cleanup_session_rows(&pool, &source_session, &user_id).await;
        cleanup_session_rows(&pool, &target_session, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn execute_todo_handler_rejects_old_status_argument_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-canonical-{}", uuid::Uuid::new_v4());
        let session_id = format!("s-canonical-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &session_id, &user_id).await;

        let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy))
            .with_shared_pool(shared.clone())
            .with_auth_service(Arc::new(TestAuth {
                user_id: user_id.clone(),
            }))
            .with_session_service(Arc::new(TestSessionService));

        let Json(create_response) = execute_todo_handler(
            State(state.clone()),
            HeaderMap::new(),
            Path(session_id.clone()),
            Json(ExecuteTodoRequest {
                action: "create".to_string(),
                args: serde_json::json!({"title": "canonical cloud task"}),
                idempotency_key: Some(format!("test-create:{}", uuid::Uuid::new_v4())),
            }),
        )
        .await
        .expect("create todo");
        assert!(
            create_response.output.contains("\"success\":true"),
            "{}",
            create_response.output
        );

        let Json(update_response) = execute_todo_handler(
            State(state),
            HeaderMap::new(),
            Path(session_id.clone()),
            Json(ExecuteTodoRequest {
                action: "update".to_string(),
                args: serde_json::json!({
                    "task_id": "task-1",
                    "status": "completed"
                }),
                idempotency_key: None,
            }),
        )
        .await
        .expect("status alias update request should return a tool error");
        assert!(
            update_response.output.starts_with("Error:")
                && update_response.output.contains("unknown field")
                && update_response.output.contains("status")
                && update_response.output.contains("new_status"),
            "cloud task_board.update should reject the old status argument: {}",
            update_response.output
        );

        let status: String = sqlx::query_scalar(
            "SELECT status FROM session_todos \
             WHERE session_id = ? AND todo_id = ? AND user_id = ?",
        )
        .bind(&session_id)
        .bind("task-1")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("task status");
        assert_eq!(
            status, "pending",
            "rejected status alias must not mutate MatrixOne task state"
        );

        cleanup_session_rows(&pool, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn execute_todo_handler_rejects_terminal_reopen_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-terminal-{}", uuid::Uuid::new_v4());
        let session_id = format!("s-terminal-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &session_id, &user_id).await;

        let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy))
            .with_shared_pool(shared.clone())
            .with_auth_service(Arc::new(TestAuth {
                user_id: user_id.clone(),
            }))
            .with_session_service(Arc::new(TestSessionService));

        let Json(create_response) = execute_todo_handler(
            State(state.clone()),
            HeaderMap::new(),
            Path(session_id.clone()),
            Json(ExecuteTodoRequest {
                action: "create".to_string(),
                args: serde_json::json!({"title": "terminal cloud task"}),
                idempotency_key: Some(format!("test-create:{}", uuid::Uuid::new_v4())),
            }),
        )
        .await
        .expect("create todo");
        assert!(
            create_response.output.contains("\"success\":true"),
            "{}",
            create_response.output
        );

        let Json(start_response) = execute_todo_handler(
            State(state.clone()),
            HeaderMap::new(),
            Path(session_id.clone()),
            Json(ExecuteTodoRequest {
                action: "update".to_string(),
                args: serde_json::json!({
                    "task_id": "task-1",
                    "new_status": "in_progress",
                }),
                idempotency_key: None,
            }),
        )
        .await
        .expect("start todo");
        assert!(
            start_response.output.contains("\"success\":true"),
            "{}",
            start_response.output
        );

        let Json(start_response) = execute_todo_handler(
            State(state.clone()),
            HeaderMap::new(),
            Path(session_id.clone()),
            Json(ExecuteTodoRequest {
                action: "update".to_string(),
                args: serde_json::json!({
                    "task_id": "task-1",
                    "new_status": "in_progress",
                }),
                idempotency_key: None,
            }),
        )
        .await
        .expect("start parent");
        assert!(
            start_response.output.contains("\"success\":true"),
            "{}",
            start_response.output
        );

        let Json(complete_response) = execute_todo_handler(
            State(state.clone()),
            HeaderMap::new(),
            Path(session_id.clone()),
            Json(ExecuteTodoRequest {
                action: "update".to_string(),
                args: serde_json::json!({
                    "task_id": "task-1",
                    "new_status": "completed",
                }),
                idempotency_key: None,
            }),
        )
        .await
        .expect("complete todo");
        assert!(
            complete_response.output.contains("\"success\":true"),
            "{}",
            complete_response.output
        );

        let Json(reopen_response) = execute_todo_handler(
            State(state),
            HeaderMap::new(),
            Path(session_id.clone()),
            Json(ExecuteTodoRequest {
                action: "update".to_string(),
                args: serde_json::json!({
                    "task_id": "task-1",
                    "new_status": "in_progress",
                }),
                idempotency_key: None,
            }),
        )
        .await
        .expect("terminal reopen returns tool error output, not HTTP error");
        assert!(
            reopen_response.output.starts_with("Error:")
                && reopen_response.output.contains("already terminal")
                && reopen_response.output.contains("create a new task"),
            "terminal reopen should be rejected at cloud handler boundary: {}",
            reopen_response.output
        );

        let status: String = sqlx::query_scalar(
            "SELECT status FROM session_todos WHERE session_id = ? AND todo_id = ? AND user_id = ?",
        )
        .bind(&session_id)
        .bind("task-1")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("task status");
        assert_eq!(
            status, "completed",
            "refused terminal reopen must not mutate MatrixOne task state"
        );

        cleanup_session_rows(&pool, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn execute_todo_handler_subtask_update_rejects_explicit_parent_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-sub-reopen-{}", uuid::Uuid::new_v4());
        let session_id = format!("s-sub-reopen-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &session_id, &user_id).await;

        let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy))
            .with_shared_pool(shared.clone())
            .with_auth_service(Arc::new(TestAuth {
                user_id: user_id.clone(),
            }))
            .with_session_service(Arc::new(TestSessionService));

        let Json(create_response) = execute_todo_handler(
            State(state.clone()),
            HeaderMap::new(),
            Path(session_id.clone()),
            Json(ExecuteTodoRequest {
                action: "create".to_string(),
                args: serde_json::json!({
                    "title": "explicit parent with subtasks",
                    "subtasks": [
                        {"id": "s1", "title": "First step"},
                        {"id": "s2", "title": "Second step"}
                    ]
                }),
                idempotency_key: Some(format!("test-create:{}", uuid::Uuid::new_v4())),
            }),
        )
        .await
        .expect("create todo");
        assert!(
            create_response.output.contains("\"success\":true"),
            "{}",
            create_response.output
        );

        let Json(start_response) = execute_todo_handler(
            State(state.clone()),
            HeaderMap::new(),
            Path(session_id.clone()),
            Json(ExecuteTodoRequest {
                action: "update".to_string(),
                args: serde_json::json!({
                    "task_id": "task-1",
                    "new_status": "in_progress",
                }),
                idempotency_key: None,
            }),
        )
        .await
        .expect("start parent");
        assert!(
            start_response.output.contains("\"success\":true"),
            "{}",
            start_response.output
        );

        let Json(complete_response) = execute_todo_handler(
            State(state.clone()),
            HeaderMap::new(),
            Path(session_id.clone()),
            Json(ExecuteTodoRequest {
                action: "update".to_string(),
                args: serde_json::json!({
                    "task_id": "task-1",
                    "new_status": "completed",
                }),
                idempotency_key: None,
            }),
        )
        .await
        .expect("complete parent");
        assert!(
            complete_response.output.contains("\"success\":true"),
            "{}",
            complete_response.output
        );

        let Json(subtask_response) = execute_todo_handler(
            State(state),
            HeaderMap::new(),
            Path(session_id.clone()),
            Json(ExecuteTodoRequest {
                action: "update".to_string(),
                args: serde_json::json!({
                    "task_id": "task-1",
                    "subtask_id": "s1",
                    "new_status": "pending",
                }),
                idempotency_key: None,
            }),
        )
        .await
        .expect("subtask update");
        assert!(
            subtask_response.output.starts_with("Error:")
                && subtask_response.output.contains("already terminal")
                && subtask_response
                    .output
                    .contains("instead of editing its subtasks"),
            "subtask update should be rejected under explicit terminal parent: {}",
            subtask_response.output
        );

        let row: (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT status, metadata, subtasks FROM session_todos \
             WHERE session_id = ? AND todo_id = ? AND user_id = ?",
        )
        .bind(&session_id)
        .bind("task-1")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("task row");
        assert_eq!(
            row.0, "completed",
            "subtask update must not reopen explicitly completed parent"
        );
        let metadata = row
            .1
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
        assert!(
            metadata
                .as_ref()
                .and_then(|m| m.get("auto_completed_by_subtasks"))
                .is_none(),
            "explicit parent completion must not carry reversible auto-completion metadata: {metadata:?}"
        );
        let subtasks: serde_json::Value =
            serde_json::from_str(row.2.as_deref().unwrap_or("[]")).expect("subtasks json");
        assert_eq!(
            subtasks[0]["status"], "completed",
            "rejected subtask update must not mutate terminal history: {subtasks}"
        );

        cleanup_session_rows(&pool, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn execute_todo_handler_subtask_update_reopens_auto_completed_parent_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-auto-sub-reopen-{}", uuid::Uuid::new_v4());
        let session_id = format!("s-auto-sub-reopen-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &session_id, &user_id).await;

        let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy))
            .with_shared_pool(shared.clone())
            .with_auth_service(Arc::new(TestAuth {
                user_id: user_id.clone(),
            }))
            .with_session_service(Arc::new(TestSessionService));

        let Json(create_response) = execute_todo_handler(
            State(state.clone()),
            HeaderMap::new(),
            Path(session_id.clone()),
            Json(ExecuteTodoRequest {
                action: "create".to_string(),
                args: serde_json::json!({
                    "title": "auto-completed parent with subtasks",
                    "subtasks": [
                        {"id": "s1", "title": "First step"},
                        {"id": "s2", "title": "Second step"}
                    ]
                }),
                idempotency_key: Some(format!("test-create:{}", uuid::Uuid::new_v4())),
            }),
        )
        .await
        .expect("create todo");
        assert!(
            create_response.output.contains("\"success\":true"),
            "{}",
            create_response.output
        );

        for subtask_id in ["s1", "s2"] {
            let Json(update_response) = execute_todo_handler(
                State(state.clone()),
                HeaderMap::new(),
                Path(session_id.clone()),
                Json(ExecuteTodoRequest {
                    action: "update".to_string(),
                    args: serde_json::json!({
                        "task_id": "task-1",
                        "subtask_id": subtask_id,
                        "new_status": "completed",
                    }),
                    idempotency_key: None,
                }),
            )
            .await
            .expect("complete subtask");
            assert!(
                update_response.output.contains("\"success\":true"),
                "{}",
                update_response.output
            );
        }

        let completed_row: (String, Option<String>) = sqlx::query_as(
            "SELECT status, metadata FROM session_todos \
             WHERE session_id = ? AND todo_id = ? AND user_id = ?",
        )
        .bind(&session_id)
        .bind("task-1")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("completed task row");
        assert_eq!(completed_row.0, "completed");
        let completed_metadata: serde_json::Value =
            serde_json::from_str(completed_row.1.as_deref().unwrap_or("{}"))
                .expect("completed metadata");
        assert_eq!(
            completed_metadata["auto_completed_by_subtasks"], true,
            "auto-completed parent should persist its reversible marker"
        );

        let Json(reopen_response) = execute_todo_handler(
            State(state),
            HeaderMap::new(),
            Path(session_id.clone()),
            Json(ExecuteTodoRequest {
                action: "update".to_string(),
                args: serde_json::json!({
                    "task_id": "task-1",
                    "subtask_id": "s1",
                    "new_status": "pending",
                }),
                idempotency_key: None,
            }),
        )
        .await
        .expect("reopen subtask");
        assert!(
            reopen_response.output.contains("\"success\":true"),
            "{}",
            reopen_response.output
        );

        let reopened_row: (String, Option<String>) = sqlx::query_as(
            "SELECT status, metadata FROM session_todos \
             WHERE session_id = ? AND todo_id = ? AND user_id = ?",
        )
        .bind(&session_id)
        .bind("task-1")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("reopened task row");
        assert_eq!(
            reopened_row.0, "in_progress",
            "auto-completed parent should reopen when a subtask is reopened"
        );
        let reopened_metadata = reopened_row
            .1
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
        assert!(
            reopened_metadata
                .as_ref()
                .and_then(|m| m.get("auto_completed_by_subtasks"))
                .is_none(),
            "reopening should consume the reversible auto-completion marker: {reopened_metadata:?}"
        );

        cleanup_session_rows(&pool, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn execute_todo_handler_rejects_error_message_without_failed_status_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-error-message-{}", uuid::Uuid::new_v4());
        let session_id = format!("s-error-message-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &session_id, &user_id).await;

        let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy))
            .with_shared_pool(shared.clone())
            .with_auth_service(Arc::new(TestAuth {
                user_id: user_id.clone(),
            }))
            .with_session_service(Arc::new(TestSessionService));

        let Json(create_response) = execute_todo_handler(
            State(state.clone()),
            HeaderMap::new(),
            Path(session_id.clone()),
            Json(ExecuteTodoRequest {
                action: "create".to_string(),
                args: serde_json::json!({"title": "canonical error message task"}),
                idempotency_key: Some(format!("test-create:{}", uuid::Uuid::new_v4())),
            }),
        )
        .await
        .expect("create todo");
        assert!(
            create_response.output.contains("\"success\":true"),
            "{}",
            create_response.output
        );

        let Json(update_response) = execute_todo_handler(
            State(state.clone()),
            HeaderMap::new(),
            Path(session_id.clone()),
            Json(ExecuteTodoRequest {
                action: "update".to_string(),
                args: serde_json::json!({
                    "task_id": "task-1",
                    "new_status": "completed",
                    "error_message": "should not be stored"
                }),
                idempotency_key: None,
            }),
        )
        .await
        .expect("bad error_message update returns tool error output, not HTTP error");
        assert!(
            update_response.output.starts_with("Error:")
                && update_response
                    .output
                    .contains("requires new_status='failed'"),
            "error_message should be rejected at the cloud handler boundary: {}",
            update_response.output
        );

        let row: (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT status, description, metadata FROM session_todos \
             WHERE session_id = ? AND todo_id = ? AND user_id = ?",
        )
        .bind(&session_id)
        .bind("task-1")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("task row");
        assert_eq!(
            row.0, "pending",
            "bad error_message update must not mutate MatrixOne status"
        );
        assert!(
            row.1.as_deref().unwrap_or("").is_empty() && row.2.as_deref().unwrap_or("").is_empty(),
            "bad error_message update must not write description/metadata: {row:?}"
        );

        let Json(start_response) = execute_todo_handler(
            State(state.clone()),
            HeaderMap::new(),
            Path(session_id.clone()),
            Json(ExecuteTodoRequest {
                action: "update".to_string(),
                args: serde_json::json!({
                    "task_id": "task-1",
                    "new_status": "in_progress",
                }),
                idempotency_key: None,
            }),
        )
        .await
        .expect("start task before failure");
        assert!(
            start_response.output.contains("\"success\":true"),
            "{}",
            start_response.output
        );

        let Json(failed_response) = execute_todo_handler(
            State(state),
            HeaderMap::new(),
            Path(session_id.clone()),
            Json(ExecuteTodoRequest {
                action: "update".to_string(),
                args: serde_json::json!({
                    "task_id": "task-1",
                    "new_status": "failed",
                    "description": "Cloud verification failed",
                    "error_message": "missing Foo"
                }),
                idempotency_key: None,
            }),
        )
        .await
        .expect("valid failed update");
        assert!(
            failed_response.output.contains("\"success\":true"),
            "{}",
            failed_response.output
        );

        let failed_row: (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT status, description, metadata FROM session_todos \
             WHERE session_id = ? AND todo_id = ? AND user_id = ?",
        )
        .bind(&session_id)
        .bind("task-1")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("failed task row");
        assert_eq!(failed_row.0, "failed");
        let description = failed_row.1.as_deref().unwrap_or_default();
        assert!(
            description.contains("Cloud verification failed")
                && description.contains("Error: missing Foo"),
            "valid failed update should persist description and failure reason: {description}"
        );
        let metadata: serde_json::Value =
            serde_json::from_str(failed_row.2.as_deref().unwrap_or("{}")).unwrap();
        assert_eq!(metadata["error_message"], "missing Foo");

        cleanup_session_rows(&pool, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn adopt_terminal_source_does_not_migrate_or_clone_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-adopt-terminal-{}", uuid::Uuid::new_v4());
        let source_session = format!("s-adopt-terminal-source-{}", uuid::Uuid::new_v4());
        let target_session = format!("s-adopt-terminal-target-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &source_session, &user_id).await;
        prepare_session_todo_owner(&pool, &target_session, &user_id).await;

        let source_store: Arc<dyn TaskStore> =
            Arc::new(MatrixOneTaskStore::from_shared_for_user(&shared, &user_id).unwrap());
        let target_store: Arc<dyn TaskStore> =
            Arc::new(MatrixOneTaskStore::from_shared_for_user(&shared, &user_id).unwrap());
        let source = TaskManager::new(source_session.clone(), source_store);
        let target = TaskManager::new(target_session.clone(), target_store);
        let created = source
            .create(&serde_json::json!({"title": "Already shipped"}))
            .await;
        assert!(created.contains("\"success\":true"), "{created}");
        let started = source
            .update(&serde_json::json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert!(started.contains("\"success\":true"), "{started}");
        let completed = source
            .update(&serde_json::json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        assert!(completed.contains("\"success\":true"), "{completed}");

        let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy))
            .with_shared_pool(shared.clone());
        let out = adopt_task_into_session(
            &state,
            &user_id,
            &target_session,
            &serde_json::json!({
                "source_session_id": source_session,
                "task_id": "task-1",
            }),
        )
        .await;
        assert!(
            out.starts_with("Error:")
                && out.contains("completed")
                && out.contains("only pending, in_progress, or paused"),
            "terminal source should be refused before migrate/clone: {out}"
        );

        let source_status: String = sqlx::query_scalar(
            "SELECT status FROM session_todos WHERE session_id = ? AND todo_id = ? AND user_id = ?",
        )
        .bind(&source_session)
        .bind("task-1")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("source status");
        assert_eq!(
            source_status, "completed",
            "terminal source must stay completed, not migrated"
        );

        assert!(
            target.snapshot().await.unwrap().is_empty(),
            "refused terminal adopt must not create a target clone"
        );

        cleanup_session_rows(&pool, &source_session, &user_id).await;
        cleanup_session_rows(&pool, &target_session, &user_id).await;
    }
}
