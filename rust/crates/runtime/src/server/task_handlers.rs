use super::*;
use astra_services::session_journal;
use astra_services::task_orchestrator::{TaskCheckpoint, TaskOutcome, TaskPlan};
use astra_thin_client::ASTRA_EDGE_ID_HEADER;

// ─── Query parameters ───────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct TaskListQuery {
    /// Optional status filter: pending, in_progress, paused, completed, failed, cancelled
    pub status: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct TaskProgressQuery {
    /// Session ID to read plan-progress journal events from.
    pub session_id: Option<String>,
}

// ─── Response types ─────────────────────────────────────────────────────────

#[derive(Serialize)]
pub(super) struct TaskListResponse {
    pub tasks: Vec<astra_services::TaskListItem>,
    pub total: usize,
}

#[derive(Serialize)]
pub(super) struct PlanProgressEventResponse {
    pub subtask_id: String,
    pub subtask_title: String,
    pub action: String,
    pub progress_pct: u32,
    pub total_subtasks: usize,
    pub completed_subtasks: usize,
    pub timestamp: String,
}

#[derive(Serialize)]
pub(super) struct TaskProgressResponse {
    pub task: astra_services::TaskRecord,
    pub progress_events: Vec<PlanProgressEventResponse>,
}

// ─── Handlers ───────────────────────────────────────────────────────────────

/// `GET /tasks` — list tasks for the authenticated user.
pub(super) async fn list_tasks_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TaskListQuery>,
) -> Result<Json<TaskListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    let status_filter = query.status.and_then(|s| parse_task_status(&s));

    let tasks = state
        .task_service
        .list_tasks(&user.user_id, status_filter)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let total = tasks.len();
    Ok(Json(TaskListResponse { tasks, total }))
}

/// `GET /tasks/{task_id}` — get a single task with its plan.
pub(super) async fn get_task_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
) -> Result<Json<astra_services::TaskRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    let task = state
        .task_service
        .get_task(&task_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "task not found"))?;

    if task.user_id != user.user_id {
        return Err(error_response(StatusCode::NOT_FOUND, "task not found"));
    }

    Ok(Json(task))
}

/// `GET /tasks/{task_id}/progress` — get task + plan progress events from
/// the session journal.
pub(super) async fn task_progress_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Query(query): Query<TaskProgressQuery>,
) -> Result<Json<TaskProgressResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    let task = state
        .task_service
        .get_task(&task_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "task not found"))?;

    if task.user_id != user.user_id {
        return Err(error_response(StatusCode::NOT_FOUND, "task not found"));
    }

    // Read plan-progress events from the session journal.
    let session_id = query
        .session_id
        .or_else(|| task.session_id.clone())
        .unwrap_or_default();

    let progress_events = if session_id.is_empty() {
        Vec::new()
    } else {
        extract_plan_progress_events(&session_id)
    };

    Ok(Json(TaskProgressResponse {
        task,
        progress_events,
    }))
}

/// `POST /tasks` — create a new task.
pub(super) async fn create_task_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateTaskRequest>,
) -> Result<(StatusCode, Json<astra_services::TaskRecord>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    let req = astra_services::TaskCreateRequest {
        title: payload.title,
        description: payload.description,
        plan: None,
        parent_task_id: payload.parent_task_id,
        project_type: payload.project_type,
        goal_pattern: payload.goal_pattern,
    };

    let session_id = payload.session_id.unwrap_or_default();
    let task_id = state
        .task_service
        .create_task(&user.user_id, &session_id, req)
        .await
        .map_err(|e| error_response(StatusCode::BAD_REQUEST, e))?;

    let task = state
        .task_service
        .get_task(&task_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "task created but not found",
            )
        })?;

    Ok((StatusCode::CREATED, Json(task)))
}

/// `PUT /tasks/{task_id}/status` — update a task's status.
pub(super) async fn update_task_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(payload): Json<UpdateStatusRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    let task = state
        .task_service
        .get_task(&task_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "task not found"))?;
    if task.user_id != user.user_id {
        return Err(error_response(StatusCode::NOT_FOUND, "task not found"));
    }

    let status = parse_task_status(&payload.status).ok_or_else(|| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("invalid status: {}", payload.status),
        )
    })?;

    state
        .task_service
        .update_status(&task_id, status)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ─── Phase 3 task leases ─────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct LeaseClaimRequest {
    pub edge_agent_id: String,
    #[serde(default)]
    pub ttl_sec: Option<i64>,
}

fn edge_id_header(headers: &HeaderMap) -> String {
    headers
        .get(ASTRA_EDGE_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// `GET /tasks/{task_id}/lease`
pub(super) async fn get_task_lease_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let task = state
        .task_service
        .get_task(&task_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "task not found"))?;
    if task.user_id != user.user_id {
        return Err(error_response(StatusCode::NOT_FOUND, "task not found"));
    }

    let view = state
        .task_lease_service
        .get_lease(&user.user_id, &task_id)
        .await
        .map_err(|e| error_response(StatusCode::SERVICE_UNAVAILABLE, e))?;
    Ok(Json(match view {
        Some(v) => serde_json::to_value(v).unwrap_or_else(|_| serde_json::json!({})),
        None => serde_json::json!({ "lease": null }),
    }))
}

/// `POST /tasks/{task_id}/lease/claim`
pub(super) async fn post_task_lease_claim_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(body): Json<LeaseClaimRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    if body.edge_agent_id.trim().is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "edge_agent_id required",
        ));
    }
    let task = state
        .task_service
        .get_task(&task_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "task not found"))?;
    if task.user_id != user.user_id {
        return Err(error_response(StatusCode::NOT_FOUND, "task not found"));
    }

    let edge_id = edge_id_header(&headers);
    let ttl = body.ttl_sec.unwrap_or(300);
    let result = state
        .task_lease_service
        .try_claim_lease(&user.user_id, &task_id, &body.edge_agent_id, &edge_id, ttl)
        .await
        .map_err(|e| error_response(StatusCode::SERVICE_UNAVAILABLE, e))?;
    Ok(Json(
        serde_json::to_value(result).unwrap_or_else(|_| serde_json::json!({})),
    ))
}

/// `POST /tasks/{task_id}/lease/release`
pub(super) async fn post_task_lease_release_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(body): Json<LeaseClaimRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    if body.edge_agent_id.trim().is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "edge_agent_id required",
        ));
    }
    let task = state
        .task_service
        .get_task(&task_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "task not found"))?;
    if task.user_id != user.user_id {
        return Err(error_response(StatusCode::NOT_FOUND, "task not found"));
    }

    let released = state
        .task_lease_service
        .release_lease(&user.user_id, &task_id, &body.edge_agent_id)
        .await
        .map_err(|e| error_response(StatusCode::SERVICE_UNAVAILABLE, e))?;
    Ok(Json(serde_json::json!({ "released": released })))
}

/// `POST /tasks/{task_id}/lease/renew`
pub(super) async fn post_task_lease_renew_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(body): Json<LeaseClaimRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    if body.edge_agent_id.trim().is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "edge_agent_id required",
        ));
    }
    let task = state
        .task_service
        .get_task(&task_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "task not found"))?;
    if task.user_id != user.user_id {
        return Err(error_response(StatusCode::NOT_FOUND, "task not found"));
    }

    let edge_id = edge_id_header(&headers);
    let ttl = body.ttl_sec.unwrap_or(300);
    let view = state
        .task_lease_service
        .renew_lease(&user.user_id, &task_id, &body.edge_agent_id, &edge_id, ttl)
        .await
        .map_err(|e| error_response(StatusCode::SERVICE_UNAVAILABLE, e))?;
    Ok(Json(match view {
        Some(v) => serde_json::to_value(v).unwrap_or_else(|_| serde_json::json!({})),
        None => serde_json::json!({ "renewed": false }),
    }))
}

// ─── Request bodies ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct CreateTaskRequest {
    pub title: String,
    pub description: Option<String>,
    pub session_id: Option<String>,
    pub parent_task_id: Option<String>,
    pub project_type: Option<String>,
    pub goal_pattern: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct UpdateStatusRequest {
    pub status: String,
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn parse_task_status(s: &str) -> Option<astra_services::TaskStatus> {
    match s {
        "pending" => Some(astra_services::TaskStatus::Pending),
        "in_progress" => Some(astra_services::TaskStatus::InProgress),
        "paused" => Some(astra_services::TaskStatus::Paused),
        "completed" => Some(astra_services::TaskStatus::Completed),
        "failed" => Some(astra_services::TaskStatus::Failed),
        "cancelled" => Some(astra_services::TaskStatus::Cancelled),
        _ => None,
    }
}

fn extract_plan_progress_events(session_id: &str) -> Vec<PlanProgressEventResponse> {
    let events = match session_journal::read_journal(session_id) {
        Ok(events) => events,
        Err(err) => {
            tracing::warn!(
                target: "astra_runtime::task_progress",
                session_id,
                err = %err,
                "failed to read plan progress journal"
            );
            return Vec::new();
        }
    };

    events
        .into_iter()
        .filter(|evt| evt.event_type == session_journal::JournalEventType::PlanProgress)
        .filter_map(|evt| {
            let meta = evt.metadata.as_ref()?;
            Some(PlanProgressEventResponse {
                subtask_id: meta.get("subtask_id")?.as_str()?.to_string(),
                subtask_title: meta.get("subtask_title")?.as_str()?.to_string(),
                action: meta.get("action")?.as_str()?.to_string(),
                progress_pct: meta.get("progress_pct")?.as_u64()? as u32,
                total_subtasks: meta.get("total_subtasks")?.as_u64()? as usize,
                completed_subtasks: meta.get("completed_subtasks")?.as_u64()? as usize,
                timestamp: evt.ts,
            })
        })
        .collect()
}

// ─── Task RPC dispatcher ────────────────────────────────────────────────────
//
// Edge-cloud architecture: the CLI proxies its TaskService trait calls
// through this single endpoint instead of connecting to MatrixOne
// directly. Every method on `astra_services::TaskService` is exposed
// here so `HttpTaskService` (CLI side) can be a uniform `{method, args}`
// HTTP request. Per-method REST endpoints are still preferred for web
// agents — this RPC is the CLI-internal escape hatch.

#[derive(Deserialize)]
pub(super) struct TaskRpcRequest {
    pub method: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

#[derive(Serialize)]
pub(super) struct TaskRpcResponse {
    pub result: serde_json::Value,
}

/// `POST /tasks:rpc` — proxy entry point for `TaskService` trait
/// methods. CLI's `HttpTaskService` impl posts here; server-side
/// `state.task_service` (MatrixOneTaskService in production) does
/// the work. Every method scopes to the authenticated user; methods
/// that take a `task_id` verify ownership before mutating.
pub(super) async fn task_rpc_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TaskRpcRequest>,
) -> Result<Json<TaskRpcResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    // Helper: parse + ownership-check a task_id from args.
    async fn require_owned_task(
        state: &AppState,
        user_id: &str,
        args: &serde_json::Value,
    ) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
        let task_id = args
            .get("task_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| error_response(StatusCode::BAD_REQUEST, "missing 'task_id' in args"))?
            .to_string();
        let task = state
            .task_service
            .get_task(&task_id)
            .await
            .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "task not found"))?;
        if task.user_id != user_id {
            return Err(error_response(StatusCode::NOT_FOUND, "task not found"));
        }
        Ok(task_id)
    }

    let result = match req.method.as_str() {
        "create_task" => {
            let session_id = req
                .args
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let create_req: astra_services::TaskCreateRequest = serde_json::from_value(
                req.args.get("req").cloned().unwrap_or_default(),
            )
            .map_err(|e| error_response(StatusCode::BAD_REQUEST, format!("decode req: {e}")))?;
            let id = state
                .task_service
                .create_task(&user.user_id, session_id, create_req)
                .await
                .map_err(|e| error_response(StatusCode::BAD_REQUEST, e))?;
            serde_json::json!({ "task_id": id })
        }
        "get_task" => {
            let task_id = req
                .args
                .get("task_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| error_response(StatusCode::BAD_REQUEST, "missing 'task_id'"))?;
            let task = state
                .task_service
                .get_task(task_id)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            // Return None for non-owned tasks the same way as missing
            // (avoid leaking existence). Wrap as JSON value so client
            // can deserialize into Option<TaskRecord>.
            match task {
                Some(t) if t.user_id == user.user_id => {
                    serde_json::to_value(&t).unwrap_or(serde_json::Value::Null)
                }
                _ => serde_json::Value::Null,
            }
        }
        "list_tasks" => {
            let status_filter = req
                .args
                .get("status_filter")
                .and_then(|v| v.as_str())
                .and_then(parse_task_status);
            let tasks = state
                .task_service
                .list_tasks(&user.user_id, status_filter)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::to_value(&tasks).unwrap_or(serde_json::Value::Null)
        }
        "update_status" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            let status_str = req
                .args
                .get("status")
                .and_then(|v| v.as_str())
                .ok_or_else(|| error_response(StatusCode::BAD_REQUEST, "missing 'status'"))?;
            let status = parse_task_status(status_str).ok_or_else(|| {
                error_response(
                    StatusCode::BAD_REQUEST,
                    format!("invalid status: {status_str}"),
                )
            })?;
            state
                .task_service
                .update_status(&task_id, status)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "update_progress" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            let pct = req
                .args
                .get("progress_pct")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let done = req
                .args
                .get("items_done")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let total = req
                .args
                .get("items_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            state
                .task_service
                .update_progress(&task_id, pct, done, total)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "save_checkpoint" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            let checkpoint: TaskCheckpoint =
                serde_json::from_value(req.args.get("checkpoint").cloned().unwrap_or_default())
                    .map_err(|e| {
                        error_response(StatusCode::BAD_REQUEST, format!("decode checkpoint: {e}"))
                    })?;
            state
                .task_service
                .save_checkpoint(&task_id, &checkpoint)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "update_plan" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            let plan: TaskPlan = serde_json::from_value(
                req.args.get("plan").cloned().unwrap_or_default(),
            )
            .map_err(|e| error_response(StatusCode::BAD_REQUEST, format!("decode plan: {e}")))?;
            state
                .task_service
                .update_plan(&task_id, &plan)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "fail_task" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            let error_msg = req
                .args
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("(no error message)");
            state
                .task_service
                .fail_task(&task_id, error_msg)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "complete_task" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            state
                .task_service
                .complete_task(&task_id)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "complete_plan_run" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            let pct = req
                .args
                .get("progress_pct")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let done = req
                .args
                .get("items_done")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let total = req
                .args
                .get("items_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let outcome: TaskOutcome =
                serde_json::from_value(req.args.get("outcome").cloned().unwrap_or_default())
                    .map_err(|e| {
                        error_response(StatusCode::BAD_REQUEST, format!("decode outcome: {e}"))
                    })?;
            state
                .task_service
                .complete_plan_run(&task_id, pct, done, total, outcome)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "record_feedback" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            let rating = req.args.get("rating").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
            let outcome: TaskOutcome =
                serde_json::from_value(req.args.get("outcome").cloned().unwrap_or_default())
                    .map_err(|e| {
                        error_response(StatusCode::BAD_REQUEST, format!("decode outcome: {e}"))
                    })?;
            let completion_time_sec = req
                .args
                .get("completion_time_sec")
                .and_then(|v| v.as_i64())
                .map(|i| i as i32);
            state
                .task_service
                .record_feedback(&task_id, rating, outcome, completion_time_sec)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "increment_replan_count" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            state
                .task_service
                .increment_replan_count(&task_id)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "extract_template" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            let goal_pattern = req
                .args
                .get("goal_pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let template_id = state
                .task_service
                .extract_template(&task_id, goal_pattern)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            match template_id {
                Some(id) => serde_json::json!({ "template_id": id }),
                None => serde_json::json!({}),
            }
        }
        "recommend_templates" => {
            let goal = req
                .args
                .get("goal")
                .and_then(|v| v.as_str())
                .ok_or_else(|| error_response(StatusCode::BAD_REQUEST, "missing 'goal'"))?;
            let project_type = req.args.get("project_type").and_then(|v| v.as_str());
            let limit = req.args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
            let recs = state
                .task_service
                .recommend_templates(&user.user_id, goal, project_type, limit)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::to_value(&recs).unwrap_or(serde_json::Value::Null)
        }
        "get_learning_stats" => {
            let goal_pattern = req
                .args
                .get("goal_pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| error_response(StatusCode::BAD_REQUEST, "missing 'goal_pattern'"))?;
            let stats = state
                .task_service
                .get_learning_stats(&user.user_id, goal_pattern)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::to_value(&stats).unwrap_or(serde_json::Value::Null)
        }
        "record_template_usage" => {
            // Template ownership is implicit via the user check at
            // template creation; we don't re-verify here because
            // templates are user-scoped at the table level.
            let template_id = req
                .args
                .get("template_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| error_response(StatusCode::BAD_REQUEST, "missing 'template_id'"))?;
            state
                .task_service
                .record_template_usage(template_id)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        other => {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                format!("unknown task RPC method: {other}"),
            ));
        }
    };

    Ok(Json(TaskRpcResponse { result }))
}
