use super::*;
use astra_services::session_journal;

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
    pub tasks: Vec<astra_services::TaskRecord>,
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
    let _user = state.auth_service.current_user(&headers).await?;

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
        .get("x-mo-edge-id")
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
        Err(_) => return Vec::new(),
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
