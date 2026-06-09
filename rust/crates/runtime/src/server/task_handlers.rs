use super::*;
use astra_services::session_journal;
use astra_services::task_orchestrator::{TaskCheckpoint, TaskOutcome, TaskPlan};
use astra_thin_client::ASTRA_EDGE_ID_HEADER;

// ─── Query parameters ───────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TaskListQuery {
    /// Optional status filter: pending, in_progress, paused, completed, failed, cancelled
    pub status: Option<String>,
    /// Optional session filter.
    pub session_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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

    let status_filter = parse_task_status_filter("status", query.status.as_deref())?;

    let tasks = if let Some(session_id) = query.session_id.as_deref() {
        state
            .execution
            .task_service
            .list_recent_tasks_for_session(&user.user_id, session_id, status_filter)
            .await
            .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
    } else {
        state
            .execution
            .task_service
            .list_recent_tasks(&user.user_id, status_filter)
            .await
            .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
    };

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
        .execution
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
        .execution
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
        .execution
        .task_service
        .create_task(&user.user_id, &session_id, req)
        .await
        .map_err(|e| error_response(StatusCode::BAD_REQUEST, e))?;

    let task = state
        .execution
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
        .execution
        .task_service
        .get_task(&task_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "task not found"))?;
    if task.user_id != user.user_id {
        return Err(error_response(StatusCode::NOT_FOUND, "task not found"));
    }

    let status =
        parse_task_status_filter("status", Some(payload.status.as_str()))?.ok_or_else(|| {
            error_response(StatusCode::BAD_REQUEST, "missing status after validation")
        })?;

    state
        .execution
        .task_service
        .update_status(&task_id, status)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ─── Phase 3 task leases ─────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
        .execution
        .task_service
        .get_task(&task_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "task not found"))?;
    if task.user_id != user.user_id {
        return Err(error_response(StatusCode::NOT_FOUND, "task not found"));
    }

    let view = state
        .execution
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
        .execution
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
        .execution
        .task_lease_service
        .try_claim_lease(&user.user_id, &task_id, &body.edge_agent_id, &edge_id, ttl)
        .await
        .map_err(|e| error_response(StatusCode::SERVICE_UNAVAILABLE, e))?;
    Ok(Json(
        serde_json::to_value(result).unwrap_or_else(|_| serde_json::json!({})),
    ))
}

/// `POST /tasks/lease/claim-next`
pub(super) async fn post_task_lease_claim_next_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LeaseClaimRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    if body.edge_agent_id.trim().is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "edge_agent_id required",
        ));
    }

    let edge_id = edge_id_header(&headers);
    let ttl = body.ttl_sec.unwrap_or(300);
    let result = state
        .execution
        .task_lease_service
        .claim_next_claimable_lease(&user.user_id, &body.edge_agent_id, &edge_id, ttl)
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
        .execution
        .task_service
        .get_task(&task_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "task not found"))?;
    if task.user_id != user.user_id {
        return Err(error_response(StatusCode::NOT_FOUND, "task not found"));
    }

    let released = state
        .execution
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
        .execution
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
        .execution
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
#[serde(deny_unknown_fields)]
pub(super) struct CreateTaskRequest {
    pub title: String,
    pub description: Option<String>,
    pub session_id: Option<String>,
    pub parent_task_id: Option<String>,
    pub project_type: Option<String>,
    pub goal_pattern: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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

const VALID_TASK_STATUS_VALUES: &[&str] = &[
    "pending",
    "in_progress",
    "paused",
    "completed",
    "failed",
    "cancelled",
];

fn parse_task_status_filter(
    field: &str,
    raw: Option<&str>,
) -> Result<Option<astra_services::TaskStatus>, (StatusCode, Json<ErrorResponse>)> {
    let Some(status) = raw else {
        return Ok(None);
    };
    parse_task_status(status)
        .map(Some)
        .ok_or_else(|| invalid_task_status_filter(field, status))
}

fn parse_task_status_arg(
    args: &serde_json::Value,
    field: &str,
) -> Result<Option<astra_services::TaskStatus>, (StatusCode, Json<ErrorResponse>)> {
    let Some(raw) = args.get(field) else {
        return Ok(None);
    };
    let Some(status) = raw.as_str() else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("field '{field}' must be a string"),
        ));
    };
    parse_task_status_filter(field, Some(status))
}

fn invalid_task_status_filter(field: &str, status: &str) -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        StatusCode::BAD_REQUEST,
        format!(
            "invalid {field} '{}' (valid: {})",
            status,
            VALID_TASK_STATUS_VALUES.join("|")
        ),
    )
}

fn validate_task_rpc_method(method: &str) -> Result<&str, (StatusCode, Json<ErrorResponse>)> {
    if method.trim().is_empty() {
        Err(error_response(
            StatusCode::BAD_REQUEST,
            "field 'method' must be non-empty",
        ))
    } else {
        Ok(method)
    }
}

fn optional_usize_arg(
    args: &serde_json::Value,
    field: &str,
    default: usize,
) -> Result<usize, (StatusCode, Json<ErrorResponse>)> {
    let Some(value) = args.get(field) else {
        return Ok(default);
    };
    let Some(raw) = value.as_u64() else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("field '{field}' must be a non-negative integer"),
        ));
    };
    usize::try_from(raw).map_err(|_| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("field '{field}' is too large"),
        )
    })
}

fn required_u32_arg(
    args: &serde_json::Value,
    field: &str,
) -> Result<u32, (StatusCode, Json<ErrorResponse>)> {
    let Some(value) = args.get(field) else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("missing '{field}'"),
        ));
    };
    let Some(raw) = value.as_u64() else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("field '{field}' must be a non-negative integer"),
        ));
    };
    u32::try_from(raw).map_err(|_| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("field '{field}' is too large"),
        )
    })
}

fn required_u8_arg(
    args: &serde_json::Value,
    field: &str,
) -> Result<u8, (StatusCode, Json<ErrorResponse>)> {
    let raw = required_u32_arg(args, field)?;
    u8::try_from(raw).map_err(|_| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("field '{field}' is too large"),
        )
    })
}

fn optional_i32_arg(
    args: &serde_json::Value,
    field: &str,
) -> Result<Option<i32>, (StatusCode, Json<ErrorResponse>)> {
    let Some(value) = args.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(raw) = value.as_i64() else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("field '{field}' must be an integer"),
        ));
    };
    i32::try_from(raw).map(Some).map_err(|_| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("field '{field}' is too large"),
        )
    })
}

fn required_non_empty_string_arg<'a>(
    args: &'a serde_json::Value,
    field: &str,
) -> Result<&'a str, (StatusCode, Json<ErrorResponse>)> {
    let Some(value) = args.get(field) else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("missing '{field}'"),
        ));
    };
    let Some(text) = value.as_str() else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("field '{field}' must be a string"),
        ));
    };
    if text.trim().is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("field '{field}' must be non-empty"),
        ));
    }
    Ok(text)
}

fn optional_string_arg<'a>(
    args: &'a serde_json::Value,
    field: &str,
) -> Result<Option<&'a str>, (StatusCode, Json<ErrorResponse>)> {
    let Some(value) = args.get(field) else {
        return Ok(None);
    };
    let Some(text) = value.as_str() else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("field '{field}' must be a string"),
        ));
    };
    Ok(Some(text))
}

fn required_struct_arg<T: serde::de::DeserializeOwned>(
    args: &serde_json::Value,
    field: &str,
) -> Result<T, (StatusCode, Json<ErrorResponse>)> {
    let Some(value) = args.get(field) else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("missing '{field}'"),
        ));
    };
    serde_json::from_value(value.clone())
        .map_err(|e| error_response(StatusCode::BAD_REQUEST, format!("decode {field}: {e}")))
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
#[serde(deny_unknown_fields)]
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
/// `state.execution.task_service` (MatrixOneTaskService in production) does
/// the work. Every method scopes to the authenticated user; methods
/// that take a `task_id` verify ownership before mutating.
pub(super) async fn task_rpc_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TaskRpcRequest>,
) -> Result<Json<TaskRpcResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let method = validate_task_rpc_method(req.method.as_str())?;

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
            .execution
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

    let result = match method {
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
                .execution
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
                .execution
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
        "list_recent_tasks" => {
            let status_filter = parse_task_status_arg(&req.args, "status_filter")?;
            let tasks = state
                .execution
                .task_service
                .list_recent_tasks(&user.user_id, status_filter)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::to_value(&tasks).unwrap_or(serde_json::Value::Null)
        }
        "list_recent_tasks_for_session" => {
            let session_id = req
                .args
                .get("session_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| error_response(StatusCode::BAD_REQUEST, "missing 'session_id'"))?;
            let status_filter = parse_task_status_arg(&req.args, "status_filter")?;
            let tasks = state
                .execution
                .task_service
                .list_recent_tasks_for_session(&user.user_id, session_id, status_filter)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::to_value(&tasks).unwrap_or(serde_json::Value::Null)
        }
        "search_tasks" => {
            let query = required_non_empty_string_arg(&req.args, "query")?;
            let limit = optional_usize_arg(&req.args, "limit", 8)?;
            let tasks = state
                .execution
                .task_service
                .search_tasks(&user.user_id, query, limit)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::to_value(&tasks).unwrap_or(serde_json::Value::Null)
        }
        "list_claimable_tasks_for_worker" => {
            let limit = optional_usize_arg(&req.args, "limit", 200)?;
            let tasks = state
                .execution
                .task_service
                .list_claimable_tasks_for_worker(&user.user_id, limit)
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
            let status =
                parse_task_status_filter("status", Some(status_str))?.ok_or_else(|| {
                    error_response(StatusCode::BAD_REQUEST, "missing status after validation")
                })?;
            state
                .execution
                .task_service
                .update_status(&task_id, status)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "update_progress" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            let pct = required_u32_arg(&req.args, "progress_pct")?;
            let done = required_u32_arg(&req.args, "items_done")?;
            let total = required_u32_arg(&req.args, "items_total")?;
            state
                .execution
                .task_service
                .update_progress(&task_id, pct, done, total)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "save_checkpoint" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            let checkpoint: TaskCheckpoint = required_struct_arg(&req.args, "checkpoint")?;
            state
                .execution
                .task_service
                .save_checkpoint(&task_id, &checkpoint)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "update_plan" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            let plan: TaskPlan = required_struct_arg(&req.args, "plan")?;
            state
                .execution
                .task_service
                .update_plan(&task_id, &plan)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "fail_task" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            let error_msg = required_non_empty_string_arg(&req.args, "error")?;
            state
                .execution
                .task_service
                .fail_task(&task_id, error_msg)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "complete_task" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            state
                .execution
                .task_service
                .complete_task(&task_id)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "complete_task_with_outcome" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            let outcome: TaskOutcome = required_struct_arg(&req.args, "outcome")?;
            state
                .execution
                .task_service
                .complete_task_with_outcome(&task_id, outcome)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "complete_plan_run" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            let pct = required_u32_arg(&req.args, "progress_pct")?;
            let done = required_u32_arg(&req.args, "items_done")?;
            let total = required_u32_arg(&req.args, "items_total")?;
            let outcome: TaskOutcome = required_struct_arg(&req.args, "outcome")?;
            state
                .execution
                .task_service
                .complete_plan_run(&task_id, pct, done, total, outcome)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "record_feedback" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            let rating = required_u8_arg(&req.args, "rating")?;
            let outcome: TaskOutcome = required_struct_arg(&req.args, "outcome")?;
            let completion_time_sec = optional_i32_arg(&req.args, "completion_time_sec")?;
            state
                .execution
                .task_service
                .record_feedback(&task_id, rating, outcome, completion_time_sec)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "increment_replan_count" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            state
                .execution
                .task_service
                .increment_replan_count(&task_id)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::json!({ "ok": true })
        }
        "extract_template" => {
            let task_id = require_owned_task(&state, &user.user_id, &req.args).await?;
            let goal_pattern = required_non_empty_string_arg(&req.args, "goal_pattern")?;
            let template_id = state
                .execution
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
            let goal = required_non_empty_string_arg(&req.args, "goal")?;
            let project_type = optional_string_arg(&req.args, "project_type")?;
            let limit = optional_usize_arg(&req.args, "limit", 5)?;
            let recs = state
                .execution
                .task_service
                .recommend_templates(&user.user_id, goal, project_type, limit)
                .await
                .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            serde_json::to_value(&recs).unwrap_or(serde_json::Value::Null)
        }
        "get_learning_stats" => {
            let goal_pattern = required_non_empty_string_arg(&req.args, "goal_pattern")?;
            let stats = state
                .execution
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
            let template_id = required_non_empty_string_arg(&req.args, "template_id")?;
            state
                .execution
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_status_filter_accepts_valid_values_and_absent_filter() {
        assert!(parse_task_status_filter("status", None).unwrap().is_none());

        let parsed = parse_task_status_filter("status", Some("paused")).unwrap();
        assert!(matches!(parsed, Some(astra_services::TaskStatus::Paused)));
    }

    #[test]
    fn task_status_filter_rejects_invalid_value_instead_of_listing_everything() {
        let Err((status, Json(err))) = parse_task_status_filter("status", Some("cancelledd"))
        else {
            panic!("invalid status must be rejected");
        };

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(err.detail.contains("invalid status 'cancelledd'"));
        assert!(err.detail.contains("cancelled"));
    }

    #[test]
    fn task_rest_requests_reject_unknown_fields() {
        let list_query = serde_json::from_value::<TaskListQuery>(serde_json::json!({
            "stats": "completed"
        }));
        assert!(
            list_query.is_err(),
            "GET /tasks should reject unknown query fields instead of defaulting the status filter"
        );

        let progress_query = serde_json::from_value::<TaskProgressQuery>(serde_json::json!({
            "session": "s1"
        }));
        assert!(
            progress_query.is_err(),
            "GET /tasks/:id/progress should reject unknown query fields"
        );

        let create = serde_json::from_value::<CreateTaskRequest>(serde_json::json!({
            "title": "new task",
            "titel": "typo"
        }));
        assert!(
            create.is_err(),
            "POST /tasks should reject typo fields instead of silently dropping them"
        );

        let status = serde_json::from_value::<UpdateStatusRequest>(serde_json::json!({
            "status": "completed",
            "state": "done"
        }));
        assert!(
            status.is_err(),
            "PUT /tasks/:id/status should reject unknown fields"
        );
    }

    #[test]
    fn lease_and_rpc_requests_reject_unknown_fields() {
        let lease = serde_json::from_value::<LeaseClaimRequest>(serde_json::json!({
            "edge_agent_id": "edge-1",
            "ttl_sec": 30,
            "ttl": 30
        }));
        assert!(
            lease.is_err(),
            "lease requests should reject typo fields instead of using defaults"
        );

        let rpc = serde_json::from_value::<TaskRpcRequest>(serde_json::json!({
            "method": "list_recent_tasks",
            "args": {},
            "status_filter": "completed"
        }));
        assert!(
            rpc.is_err(),
            "tasks:rpc should reject top-level fields outside method/args"
        );
    }

    #[test]
    fn task_rpc_method_must_be_non_empty() {
        let Err((status, Json(err))) = validate_task_rpc_method("   ") else {
            panic!("blank method should be rejected");
        };

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(err.detail.contains("method") && err.detail.contains("non-empty"));
        assert_eq!(
            validate_task_rpc_method("list_recent_tasks").expect("valid method"),
            "list_recent_tasks"
        );
    }

    #[test]
    fn optional_usize_arg_rejects_wrong_type_but_allows_absent_default() {
        assert_eq!(
            optional_usize_arg(&serde_json::json!({}), "limit", 8).expect("default limit"),
            8
        );
        assert_eq!(
            optional_usize_arg(&serde_json::json!({"limit": 3}), "limit", 8)
                .expect("explicit limit"),
            3
        );

        for args in [
            serde_json::json!({"limit": "3"}),
            serde_json::json!({"limit": -1}),
        ] {
            let Err((status, Json(err))) = optional_usize_arg(&args, "limit", 8) else {
                panic!("wrong-type limit should be rejected: {args}");
            };
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(
                err.detail.contains("limit") && err.detail.contains("non-negative integer"),
                "{err:?}"
            );
        }
    }

    #[test]
    fn required_u32_arg_rejects_missing_wrong_type_and_oversized_values() {
        assert_eq!(
            required_u32_arg(&serde_json::json!({"progress_pct": 42}), "progress_pct")
                .expect("valid progress"),
            42
        );

        let missing = required_u32_arg(&serde_json::json!({}), "progress_pct")
            .expect_err("missing progress should be rejected");
        assert_eq!(missing.0, StatusCode::BAD_REQUEST);
        assert!(missing.1.0.detail.contains("missing 'progress_pct'"));

        let wrong_type =
            required_u32_arg(&serde_json::json!({"progress_pct": true}), "progress_pct")
                .expect_err("wrong-type progress should be rejected");
        assert_eq!(wrong_type.0, StatusCode::BAD_REQUEST);
        assert!(
            wrong_type.1.0.detail.contains("progress_pct")
                && wrong_type.1.0.detail.contains("non-negative integer")
        );

        let oversized = required_u32_arg(
            &serde_json::json!({"progress_pct": u64::from(u32::MAX) + 1}),
            "progress_pct",
        )
        .expect_err("oversized progress should be rejected");
        assert_eq!(oversized.0, StatusCode::BAD_REQUEST);
        assert!(oversized.1.0.detail.contains("too large"));
    }

    #[test]
    fn required_u8_arg_rejects_oversized_rating() {
        assert_eq!(
            required_u8_arg(&serde_json::json!({"rating": 5}), "rating").expect("valid rating"),
            5
        );

        let oversized = required_u8_arg(&serde_json::json!({"rating": 256}), "rating")
            .expect_err("oversized rating should be rejected");
        assert_eq!(oversized.0, StatusCode::BAD_REQUEST);
        assert!(oversized.1.0.detail.contains("rating"));
        assert!(oversized.1.0.detail.contains("too large"));
    }

    #[test]
    fn optional_i32_arg_rejects_wrong_type_and_oversized_values() {
        assert_eq!(
            optional_i32_arg(&serde_json::json!({}), "completion_time_sec")
                .expect("absent optional int"),
            None
        );
        assert_eq!(
            optional_i32_arg(
                &serde_json::json!({"completion_time_sec": null}),
                "completion_time_sec",
            )
            .expect("null optional int"),
            None
        );
        assert_eq!(
            optional_i32_arg(
                &serde_json::json!({"completion_time_sec": 120}),
                "completion_time_sec",
            )
            .expect("valid optional int"),
            Some(120)
        );

        let wrong_type = optional_i32_arg(
            &serde_json::json!({"completion_time_sec": "120"}),
            "completion_time_sec",
        )
        .expect_err("wrong-type optional int should be rejected");
        assert_eq!(wrong_type.0, StatusCode::BAD_REQUEST);
        assert!(wrong_type.1.0.detail.contains("completion_time_sec"));

        let oversized = optional_i32_arg(
            &serde_json::json!({"completion_time_sec": i64::from(i32::MAX) + 1}),
            "completion_time_sec",
        )
        .expect_err("oversized optional int should be rejected");
        assert_eq!(oversized.0, StatusCode::BAD_REQUEST);
        assert!(oversized.1.0.detail.contains("too large"));
    }

    #[test]
    fn string_rpc_args_reject_missing_wrong_type_and_blank_values() {
        assert_eq!(
            required_non_empty_string_arg(&serde_json::json!({"goal": "ship"}), "goal")
                .expect("valid string"),
            "ship"
        );
        assert_eq!(
            optional_string_arg(&serde_json::json!({}), "project_type")
                .expect("absent optional string"),
            None
        );
        assert_eq!(
            optional_string_arg(&serde_json::json!({"project_type": "rust"}), "project_type")
                .expect("valid optional string"),
            Some("rust")
        );

        for (label, args, expected) in [
            ("missing", serde_json::json!({}), "missing 'goal'"),
            (
                "wrong type",
                serde_json::json!({"goal": true}),
                "must be a string",
            ),
            ("blank", serde_json::json!({"goal": "   "}), "non-empty"),
        ] {
            let Err((status, Json(err))) = required_non_empty_string_arg(&args, "goal") else {
                panic!("{label} goal should be rejected");
            };
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(err.detail.contains(expected), "{label}: {err:?}");
        }

        let wrong_optional =
            optional_string_arg(&serde_json::json!({"project_type": true}), "project_type")
                .expect_err("wrong-type optional string should be rejected");
        assert_eq!(wrong_optional.0, StatusCode::BAD_REQUEST);
        assert!(wrong_optional.1.0.detail.contains("project_type"));
    }

    #[test]
    fn required_struct_arg_rejects_missing_and_invalid_structures() {
        let outcome: TaskOutcome =
            required_struct_arg(&serde_json::json!({"outcome": "success"}), "outcome")
                .expect("valid outcome");
        assert_eq!(outcome, TaskOutcome::Success);

        let missing = required_struct_arg::<TaskOutcome>(&serde_json::json!({}), "outcome")
            .expect_err("missing outcome should be rejected");
        assert_eq!(missing.0, StatusCode::BAD_REQUEST);
        assert!(missing.1.0.detail.contains("missing 'outcome'"));

        let invalid =
            required_struct_arg::<TaskOutcome>(&serde_json::json!({"outcome": {}}), "outcome")
                .expect_err("invalid outcome should be rejected");
        assert_eq!(invalid.0, StatusCode::BAD_REQUEST);
        assert!(invalid.1.0.detail.contains("decode outcome"));

        let checkpoint: TaskCheckpoint = required_struct_arg(
            &serde_json::json!({
                "checkpoint": {
                    "active_subtask_id": "s1",
                    "turn": 3,
                    "session_id": "sess",
                    "state": {}
                }
            }),
            "checkpoint",
        )
        .expect("valid checkpoint");
        assert_eq!(checkpoint.turn, 3);

        let missing_checkpoint =
            required_struct_arg::<TaskCheckpoint>(&serde_json::json!({}), "checkpoint")
                .expect_err("missing checkpoint should not decode as default");
        assert_eq!(missing_checkpoint.0, StatusCode::BAD_REQUEST);
        assert!(
            missing_checkpoint
                .1
                .0
                .detail
                .contains("missing 'checkpoint'")
        );
    }

    #[test]
    fn task_rpc_status_filter_rejects_wrong_type() {
        let args = serde_json::json!({"status_filter": true});

        let Err((status, Json(err))) = parse_task_status_arg(&args, "status_filter") else {
            panic!("non-string status_filter must be rejected");
        };

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(err.detail, "field 'status_filter' must be a string");
    }

    #[test]
    fn task_rpc_status_filter_rejects_invalid_value() {
        let args = serde_json::json!({"status_filter": "done"});

        let Err((status, Json(err))) = parse_task_status_arg(&args, "status_filter") else {
            panic!("invalid status_filter must be rejected");
        };

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(err.detail.contains("invalid status_filter 'done'"));
        assert!(err.detail.contains("in_progress"));
    }

    #[test]
    fn status_update_errors_include_valid_values() {
        let Err((status, Json(err))) = parse_task_status_filter("status", Some("")) else {
            panic!("blank status should be rejected");
        };

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(err.detail.contains("invalid status ''"));
        assert!(err.detail.contains("pending|in_progress"));
    }
}
