//! Session todos REST surface.
//!
//! Wraps `astra_tools::task_mgmt::TaskManager` over `MatrixOneTaskStore`
//! so edge clients (CLI, web agent) never connect to MO directly. The
//! TaskManager business logic (cycle detection, parent reconciliation,
//! id allocation) lives on the server; clients send the raw action +
//! args from the LLM `task` tool and receive the rendered string output.
//!
//! Endpoints:
//! - `POST /sessions/{session_id}/todos:execute` — run a TaskManager
//!   action (create/update/list/get/stop/archive) and return its string
//!   output.
//! - `GET /sessions/{session_id}/todos` — load the full task list.
//!
//! User isolation: every request resolves the user via the auth header
//! and verifies the session belongs to that user before touching
//! `session_todos`. We do NOT trust the client-supplied `session_id`
//! to skip ownership checks.

use super::*;
use astra_tools::task_mgmt::{TaskManager, TaskStore};
use astra_tools::task_mgmt_matrixone::MatrixOneTaskStore;
use std::sync::Arc;

#[derive(Deserialize)]
pub(crate) struct ExecuteTodoRequest {
    /// `task` tool action: `create | update | list | get | stop | archive`.
    pub action: String,
    /// Action arguments — same shape the LLM emits to the `task` tool.
    /// Unknown fields are ignored by TaskManager.
    #[serde(default)]
    pub args: serde_json::Value,
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
pub(crate) struct UserTodosQuery {
    /// Status filter; `active` returns pending+in_progress only.
    /// Default `active` so the cross-session view is "what am I
    /// still working on" rather than the noisy full history.
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

/// Build a session-scoped TaskManager backed by the configured pool.
/// `user_id` is bound on the store so every INSERT writes the
/// owning user — required for the cross-session
/// `idx_session_todos_user_status_updated` index. Returns `None`
/// when no SQL pool is wired (server is in in-memory-only mode for
/// tests).
fn build_task_manager(state: &AppState, session_id: &str, user_id: &str) -> Option<TaskManager> {
    let pool = state.shared_pool.as_ref()?;
    let store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::from_shared(pool).with_user_id(user_id));
    Some(TaskManager::new(session_id.to_string(), store))
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

    let manager = build_task_manager(&state, &session_id, &user.user_id).ok_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "session_todos store not configured on this server",
        )
    })?;

    let output = match req.action.as_str() {
        "create" => manager.create(&req.args).await,
        "update" => manager.update(&req.args).await,
        "list" => manager.list(&req.args).await,
        "get" => manager.get(&req.args).await,
        "stop" => manager.stop(&req.args).await,
        "adopt" => adopt_task_into_session(&state, &user.user_id, &session_id, &req.args).await,
        "archive" => manager.archive(&req.args).await,
        other => format!(
            "Error: unknown todo action '{other}'. Valid: create, update, list, get, stop, adopt, archive"
        ),
    };

    Ok(Json(ExecuteTodoResponse { output }))
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
    let Some(source_session) = args.get("source_session_id").and_then(|v| v.as_str()) else {
        return "Error: 'source_session_id' is required for adopt".to_string();
    };
    let Some(source_task_id) = args.get("task_id").and_then(|v| v.as_str()) else {
        return "Error: 'task_id' is required for adopt".to_string();
    };
    if source_session == target_session {
        return "Error: source_session_id matches the current session — nothing to adopt"
            .to_string();
    }
    let Some(pool) = state.shared_pool.as_ref() else {
        return "Error: session_todos store not configured on this server".to_string();
    };

    // U-1 race fix: CAS-first. The source-migration UPDATE doubles
    // as the concurrency gate — if `rows_affected == 0`, another
    // concurrent `adopt` got there first (or the source vanished /
    // was already migrated / wrong owner). Without the CAS, two
    // concurrent adopts could both SELECT successfully, both
    // create clones in different sessions, and we'd end up with
    // two duplicate tasks linked to one (now-migrated) source.
    //
    // The CAS query reads + updates in one statement; we capture
    // the row's identity bits via a returning-style RETURNING
    // (or follow-up SELECT inside the same tx) so the clone has
    // the right title/description/etc.
    let mut tx = match pool.get().begin().await {
        Ok(tx) => tx,
        Err(e) => return format!("Error: begin tx for adopt: {e}"),
    };

    // 1. Snapshot the source row inside the tx so the read and
    //    the migrate-CAS share a snapshot. Pinning by status NOT
    //    IN ('migrated','deleted') — if it's already in either
    //    state, we abort cleanly.
    let source_row: Option<(String, Option<String>, Option<String>)> = match sqlx::query_as(
        "SELECT title, description, subtasks FROM session_todos \
         WHERE session_id = ? AND todo_id = ? AND user_id = ? \
           AND status NOT IN ('migrated', 'deleted') \
         LIMIT 1",
    )
    .bind(source_session)
    .bind(source_task_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await
    {
        Ok(opt) => opt,
        Err(e) => {
            let _ = tx.rollback().await;
            return format!("Error: source lookup failed: {e}");
        }
    };
    let Some((title, description, subtasks_json)) = source_row else {
        let _ = tx.rollback().await;
        return format!(
            "Error: source task {source_session}:{source_task_id} not found, not owned by you, or already migrated"
        );
    };

    // 2. CAS-mark the source migrated. If rows_affected == 0,
    //    another concurrent adopt won the race between our SELECT
    //    and now — abort without creating a duplicate clone.
    let migrate_result = sqlx::query(
        "UPDATE session_todos SET status = 'migrated', updated_at = NOW(6) \
         WHERE session_id = ? AND todo_id = ? AND user_id = ? \
           AND status NOT IN ('migrated', 'deleted')",
    )
    .bind(source_session)
    .bind(source_task_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await;
    let migrate_rows = match migrate_result {
        Ok(r) => r.rows_affected(),
        Err(e) => {
            let _ = tx.rollback().await;
            return format!("Error: source migrate failed: {e}");
        }
    };
    if migrate_rows == 0 {
        let _ = tx.rollback().await;
        return format!(
            "Error: source task {source_session}:{source_task_id} was already migrated by a concurrent adopt; nothing to do"
        );
    }
    if let Err(e) = tx.commit().await {
        return format!("Error: commit adopt source migrate: {e}");
    }

    // 3. With the source CAS-claimed, create the clone in the
    //    target session. We do this OUTSIDE the source-tx because
    //    the manager opens its own tx and pool re-entrancy on the
    //    same connection is awkward. Trade-off: a crash between
    //    commit (source migrated) and create (target row) leaves
    //    a "ghost" source — recoverable manually but rare.
    let store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::from_shared(pool).with_user_id(user_id));
    let manager = TaskManager::new(target_session.to_string(), store);

    // U-14: carry subtasks across when present. Pre-fix the clone
    // dropped them — the user/model would adopt a parent task and
    // lose the structure that explained how to do it. We reset
    // each subtask's `status` to `pending` and clear `depends_on`
    // ids that reference removed siblings (left as-is here; the
    // schema allows arbitrary id strings, and TaskManager filters
    // dangling deps at runtime).
    let mut create_args = serde_json::json!({
        "title": title,
        "description": description,
        "metadata": {
            "forked_from": format!("{source_session}:{source_task_id}"),
        },
    });
    if let Some(json) = subtasks_json
        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&json)
        && let Some(arr) = parsed.as_array()
        && !arr.is_empty()
    {
        // Reset status fields so adopted subtasks start fresh in
        // the target session even if the source had partial
        // progress. Drop fields TaskManager doesn't accept on
        // create (e.g. `created_at`).
        let cleaned: Vec<serde_json::Value> = arr
            .iter()
            .filter_map(|st| {
                let id = st.get("id")?.as_str()?;
                let title = st.get("title")?.as_str()?;
                let mut clean = serde_json::json!({
                    "id": id,
                    "title": title,
                    "status": "pending",
                });
                if let Some(d) = st.get("description").and_then(|v| v.as_str()) {
                    clean["description"] = serde_json::json!(d);
                }
                if let Some(deps) = st.get("depends_on").and_then(|v| v.as_array()) {
                    clean["depends_on"] = serde_json::Value::Array(deps.clone());
                }
                Some(clean)
            })
            .collect();
        if !cleaned.is_empty() {
            create_args["subtasks"] = serde_json::Value::Array(cleaned);
        }
    }
    let create_output = manager.create(&create_args).await;
    if create_output.starts_with("Error") {
        // The source is migrated but the clone failed — log loudly
        // and surface the error. Manual recovery: the operator can
        // flip the source back to its prior status if needed.
        tracing::error!(
            source_session,
            source_task_id,
            user_id,
            "adopt: source migrated but clone create failed: {create_output}"
        );
    }

    create_output
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
    let store = MatrixOneTaskStore::from_shared(pool);
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

    let status_filter = query.status.as_deref().unwrap_or("active");

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
    ): Row| UserTodoEntry {
        session_id,
        todo_id,
        title,
        status,
        updated_at,
        session_started_at,
        session_title,
    };

    let rows: Vec<UserTodoEntry> = match status_filter {
        "active" => sqlx::query_as::<_, Row>(
            "SELECT st.session_id, st.todo_id, st.title, st.status, \
                    CAST(st.updated_at AS CHAR) AS updated_at, \
                    CAST(s.created_at AS CHAR) AS session_started_at, \
                    s.title AS session_title \
             FROM session_todos st FORCE INDEX (idx_session_todos_user_status_updated) \
             LEFT JOIN agent_sessions s ON s.session_id = st.session_id \
             WHERE st.user_id = ? AND st.status IN ('pending', 'in_progress') \
             ORDER BY st.updated_at DESC LIMIT 200",
        )
        .bind(&user.user_id)
        .fetch_all(pool.get())
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .into_iter()
        .map(row_to_entry)
        .collect(),
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
        .collect(),
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
        .collect(),
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
