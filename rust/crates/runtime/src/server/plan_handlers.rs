//! REST API handlers for plan lifecycle management.
//!
//! Routes:
//! - `POST /plans` — create a plan (server-side decomposition)
//! - `GET /plans` — list plans (optional `?session_id=…` and `?phase=…` filters)
//! - `GET /plans/{plan_id}` — get plan details
//! - `PUT /plans/{plan_id}` — edit a plan
//! - `POST /plans/{plan_id}/execute` — start plan execution
//! - `GET /plans/{plan_id}/status` — get plan status + metrics
//! - `POST /plans/{plan_id}/exit-plan-mode` — web-agent equivalent of the
//!   ExitPlanMode tool; flips the phase to `refining`/`planning` based on
//!   approval and optionally writes `plan_md` alongside the state.
//! - `POST /plans/{plan_id}/rewind` — reset one anchor + everything after
//!   (mirrors the CLI `rewind N` path); distinct from redo-step.
//! - `POST /plans/{plan_id}/redo-step` — reset one subtask for re-execution.
//! - `GET /plans/{plan_id}/step-runs` — list `plan_step_runs` rows (paginated).
//! - `DELETE /plans/{plan_id}` — delete a plan.
//!
//! All handlers go through [`AppState::plan_repo`] — the filesystem-backed
//! helpers on `PlanModeState` are the offline fallback, not the source of
//! truth.

use super::*;
use crate::plan::{
    ApprovalPolicy, PlanCapabilities, PlanLoadError, PlanModeState, PlanPhase, ProjectContext,
    metrics::PlanMetrics,
};
use astra_plan::{PlanListFilter, PlanStepRun};
use astra_services::task_orchestrator::{TaskPlan, TaskStatus};

const MAX_GOAL_LENGTH: usize = 10_000;
const MAX_INSTRUCTION_LENGTH: usize = 10_000;
const MAX_PLAN_MD_LENGTH: usize = 200_000;
const DEFAULT_RUNS_LIMIT: i32 = 100;

// ─── Request / Response types ────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct CreatePlanRequest {
    pub goal: String,
    #[serde(default)]
    pub context: Option<ProjectContext>,
    /// Optional session this plan is being authored in. When present the plan
    /// becomes the session's active plan.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct EditPlanRequest {
    pub instruction: String,
    /// Expected version for optimistic concurrency. Pass the version returned
    /// by the last GET; rejected with 409 Conflict on mismatch.
    #[serde(default)]
    pub expected_version: Option<u64>,
}

#[derive(Deserialize)]
pub(super) struct ExecutePlanRequest {
    #[serde(default)]
    pub step_by_step: bool,
    #[serde(default)]
    pub approval_policy: Option<ApprovalPolicy>,
    #[serde(default)]
    pub expected_version: Option<u64>,
    /// Session driving execution (required — rows in `plan_step_runs` need it).
    pub session_id: String,
}

#[derive(Deserialize)]
pub(super) struct ExitPlanModeRequest {
    pub approved: bool,
    /// Rendered markdown plan — stored alongside `plan_json` so web agents and
    /// CLI can render a human-readable view without re-deriving.
    #[serde(default)]
    pub plan_md: Option<String>,
    #[serde(default)]
    pub expected_version: Option<u64>,
}

#[derive(Deserialize)]
pub(super) struct RewindRequest {
    /// 1-based subtask index (as shown during execution) or subtask id prefix.
    pub anchor: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub expected_version: Option<u64>,
}

#[derive(Deserialize)]
pub(super) struct RedoStepRequest {
    pub subtask_id: String,
    #[serde(default)]
    pub expected_version: Option<u64>,
}

#[derive(Deserialize, Default)]
pub(super) struct ListPlansQuery {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub limit: Option<i32>,
}

#[derive(Deserialize, Default)]
pub(super) struct ListStepRunsQuery {
    #[serde(default)]
    pub subtask_id: Option<String>,
    #[serde(default)]
    pub limit: Option<i32>,
}

#[derive(Deserialize)]
pub(super) struct StartStepRunRequest {
    pub subtask_id: String,
    /// Session driving this attempt — every row in `plan_step_runs` carries the
    /// executor session so the audit chain is traceable cross-session (CLI
    /// authored, web user rewinds, CLI retries).
    pub session_id: String,
    /// `request_id` of the chat/run that produced this attempt. Lets an
    /// investigator jump from the step-run row → `agent_events` → tool calls.
    pub request_id: String,
    #[serde(default = "default_attempt_one")]
    pub attempt: i32,
}

fn default_attempt_one() -> i32 {
    1
}

#[derive(Deserialize)]
pub(super) struct FinishStepRunRequest {
    pub status: TaskStatus,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub artifact_ref: Option<String>,
}

/// Body for the one-shot `POST /plans/{id}/step-runs/completed` endpoint —
/// combines StartStepRunRequest fields with a terminal status + optional
/// error/artifact_ref. The CLI uses this on the happy path so each subtask
/// finalize is a single HTTP call instead of start + finish.
#[derive(Deserialize)]
pub(super) struct CompletedStepRunRequest {
    pub subtask_id: String,
    pub session_id: String,
    pub request_id: String,
    #[serde(default = "default_attempt_one")]
    pub attempt: i32,
    pub status: TaskStatus,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub artifact_ref: Option<String>,
}

#[derive(Serialize)]
pub(super) struct StartStepRunResponse {
    pub run_id: String,
    pub subtask_id: String,
    pub attempt: i32,
}

#[derive(Serialize)]
pub(super) struct PlanResponse {
    pub plan_id: String,
    pub phase: String,
    pub goal: String,
    pub version: u64,
    pub plan: Option<TaskPlan>,
    pub capabilities: PlanCapabilities,
    pub metrics: PlanMetrics,
}

#[derive(Serialize)]
pub(super) struct PlanStatusResponse {
    pub plan_id: String,
    pub phase: String,
    pub goal: String,
    pub version: u64,
    pub progress_pct: u32,
    pub subtask_count: usize,
    pub completed_count: usize,
    pub failed_count: usize,
    pub metrics: PlanMetrics,
    pub capabilities: PlanCapabilities,
}

#[derive(Serialize)]
pub(super) struct PlanListResponse {
    pub plans: Vec<PlanSummary>,
}

#[derive(Serialize)]
pub(super) struct PlanSummary {
    pub plan_id: String,
    pub goal: String,
    pub progress_pct: u32,
    pub subtask_count: usize,
    pub status: String,
}

#[derive(Serialize)]
pub(super) struct RewindResponse {
    pub plan_id: String,
    pub reset_count: usize,
    pub version: u64,
    pub plan: TaskPlan,
}

#[derive(Serialize)]
pub(super) struct RedoStepResponse {
    pub plan_id: String,
    pub subtask_id: String,
    pub attempt: i32,
    pub version: u64,
}

#[derive(Serialize)]
pub(super) struct StepRunsResponse {
    pub runs: Vec<PlanStepRun>,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn map_plan_load_err(e: PlanLoadError) -> (StatusCode, Json<ErrorResponse>) {
    let status = match &e {
        PlanLoadError::InvalidId(_) => StatusCode::BAD_REQUEST,
        PlanLoadError::NotFound(_) => StatusCode::NOT_FOUND,
        PlanLoadError::Corrupt(_) => StatusCode::UNPROCESSABLE_ENTITY,
        PlanLoadError::Conflict { .. } => StatusCode::CONFLICT,
        PlanLoadError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_response(status, e.to_string())
}

/// Resolve the `expected_version` to pass to `PlanRepository::save` from an
/// HTTP request. If the client supplied one we use it (so cross-client races
/// still 409); if not, we fall back to the version we just loaded so the
/// repo-level `save(..., None)` rule doesn't trigger on same-handler
/// load→save flows. The handler already holds `check_version` for
/// client-supplied values, so there's no concurrency gap.
fn resolve_expected_version(loaded: &PlanModeState, requested: Option<u64>) -> Option<u64> {
    requested.or(Some(loaded.version))
}

fn check_version(
    plan_state: &PlanModeState,
    expected: Option<u64>,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if let Some(expected) = expected {
        if expected != plan_state.version {
            return Err(error_response(
                StatusCode::CONFLICT,
                format!(
                    "Version conflict: expected {expected}, current {}. \
                     Another client may have modified this plan.",
                    plan_state.version
                ),
            ));
        }
    }
    Ok(())
}

/// Infer the phase name from a persisted `PlanModeState`.
fn infer_phase_name(plan_state: &PlanModeState) -> &'static str {
    if plan_state.plan.progress_pct() == 100 {
        "completed"
    } else if plan_state.plan.subtasks.is_empty() {
        "planning"
    } else if plan_state.plan.items_done() > 0 {
        "executing"
    } else {
        "refining"
    }
}

fn capabilities_for_phase_name(name: &str) -> PlanCapabilities {
    match name {
        "executing" => PlanCapabilities::auto_execute(),
        "completed" => PlanCapabilities::default(),
        _ => PlanCapabilities::planning(),
    }
}

/// Fire-and-forget journal emit. Journal append failures must not fail the
/// originating plan operation (they'd mask the real outcome) — we log and move
/// on, so plan state still gets returned to the caller. Callers pass `None`
/// for `session_id` when the plan has no associated session yet (web-author
/// before any session exists).
fn emit_plan_journal(
    session_id: Option<&str>,
    event: astra_services::session_journal::JournalEvent,
) {
    let Some(sid) = session_id else {
        return;
    };
    if sid.is_empty() {
        return;
    }
    match astra_services::session_journal::JournalWriter::new(sid) {
        Ok(writer) => {
            if let Err(e) = writer.append(&event) {
                tracing::warn!(
                    target: "astra_runtime::plan_handlers",
                    session_id = sid,
                    error = %e,
                    "plan journal append failed",
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "astra_runtime::plan_handlers",
                session_id = sid,
                error = %e,
                "plan journal open failed",
            );
        }
    }
}

fn status_counts(plan: &TaskPlan) -> (usize, usize) {
    let completed = plan
        .subtasks
        .iter()
        .filter(|s| s.status == TaskStatus::Completed)
        .count();
    let failed = plan
        .subtasks
        .iter()
        .filter(|s| s.status == TaskStatus::Failed)
        .count();
    (completed, failed)
}

// ─── Handlers ────────────────────────────────────────────────────────────────

/// `POST /plans` — create a new plan.
pub(super) async fn create_plan_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreatePlanRequest>,
) -> Result<(StatusCode, Json<PlanResponse>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    let goal = req.goal.trim().to_string();
    if goal.is_empty() || goal.len() > MAX_GOAL_LENGTH {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("Goal must be 1-{MAX_GOAL_LENGTH} characters"),
        ));
    }

    let context = req.context.unwrap_or_default();
    let mut plan_state =
        PlanModeState::new_with_owner(goal.clone(), context.clone(), user.user_id.clone());
    // session_hint seeds `plans.session_id` on first insert so the UPSERT below
    // doesn't need a second statement to record the routing link.
    plan_state.session_hint = req.session_id.clone();

    let plan_id = PlanModeState::generate_plan_id(&goal);
    state
        .plan_repo
        .save(&plan_id, &mut plan_state, None)
        .await
        .map_err(map_plan_load_err)?;

    // Link to the session if one was supplied — the plan becomes this session's
    // active plan atomically.
    if let Some(sid) = req.session_id.as_deref() {
        state
            .plan_repo
            .set_active_plan(sid, Some(&plan_id))
            .await
            .map_err(map_plan_load_err)?;
    }

    emit_plan_journal(
        req.session_id.as_deref(),
        astra_services::session_journal::JournalEvent::plan_lifecycle(
            req.session_id.as_deref(),
            "plan_created",
            Some(serde_json::json!({
                "plan_id": plan_id,
                "goal": goal,
                "user_id": user.user_id,
            })),
        ),
    );

    let phase = PlanPhase::Planning {
        goal: goal.clone(),
        context,
    };
    let capabilities = PlanCapabilities::for_phase(&phase);

    Ok((
        StatusCode::CREATED,
        Json(PlanResponse {
            plan_id,
            phase: phase.phase_name().to_string(),
            goal,
            version: plan_state.version,
            plan: None,
            capabilities,
            metrics: PlanMetrics::default(),
        }),
    ))
}

/// `GET /plans` — list plans owned by the current user.
pub(super) async fn list_plans_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<ListPlansQuery>,
) -> Result<Json<PlanListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    let filter = PlanListFilter {
        session_id: q.session_id.as_deref(),
        phase: q.phase.as_deref(),
        limit: q.limit,
    };
    let saved = state
        .plan_repo
        .list_for_user(&user.user_id, filter)
        .await
        .map_err(map_plan_load_err)?;

    let plans = saved
        .into_iter()
        .map(|p| PlanSummary {
            plan_id: p.name,
            goal: p.goal,
            progress_pct: p.progress_pct,
            subtask_count: p.subtask_count,
            status: p.status,
        })
        .collect();

    Ok(Json(PlanListResponse { plans }))
}

/// `GET /plans/{plan_id}` — get plan details.
pub(super) async fn get_plan_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
) -> Result<Json<PlanResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    let plan_state = state
        .plan_repo
        .load_owned(&plan_id, &user.user_id)
        .await
        .map_err(map_plan_load_err)?;
    let phase = infer_phase_name(&plan_state);
    let capabilities = capabilities_for_phase_name(phase);

    Ok(Json(PlanResponse {
        plan_id,
        phase: phase.to_string(),
        goal: plan_state.goal,
        version: plan_state.version,
        plan: Some(plan_state.plan),
        capabilities,
        metrics: PlanMetrics::default(),
    }))
}

/// `PUT /plans/{plan_id}` — edit a plan with a natural-language instruction.
pub(super) async fn update_plan_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
    Json(req): Json<EditPlanRequest>,
) -> Result<Json<PlanResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    let instruction = req.instruction.trim().to_string();
    if instruction.is_empty() || instruction.len() > MAX_INSTRUCTION_LENGTH {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("Instruction must be 1-{MAX_INSTRUCTION_LENGTH} characters"),
        ));
    }

    let mut plan_state = state
        .plan_repo
        .load_owned(&plan_id, &user.user_id)
        .await
        .map_err(map_plan_load_err)?;

    check_version(&plan_state, req.expected_version)?;

    if infer_phase_name(&plan_state) == "completed" {
        return Err(error_response(
            StatusCode::CONFLICT,
            "Cannot edit a completed plan",
        ));
    }

    plan_state.add_turn(&instruction, "(pending LLM response)");

    let session_hint = plan_state.session_hint.clone();
    let expected = resolve_expected_version(&plan_state, req.expected_version);
    state
        .plan_repo
        .save(&plan_id, &mut plan_state, expected)
        .await
        .map_err(map_plan_load_err)?;

    emit_plan_journal(
        session_hint.as_deref(),
        astra_services::session_journal::JournalEvent::plan_edit(
            session_hint.as_deref(),
            "edit",
            Some(serde_json::json!({
                "plan_id": plan_id,
                "instruction": instruction,
                "version": plan_state.version,
            })),
        ),
    );

    Ok(Json(PlanResponse {
        plan_id,
        phase: "refining".to_string(),
        goal: plan_state.goal,
        version: plan_state.version,
        plan: Some(plan_state.plan),
        capabilities: PlanCapabilities::planning(),
        metrics: PlanMetrics::default(),
    }))
}

/// `POST /plans/{plan_id}/execute` — start plan execution.
pub(super) async fn execute_plan_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
    Json(req): Json<ExecutePlanRequest>,
) -> Result<Json<PlanStatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    let mut plan_state = state
        .plan_repo
        .load_owned(&plan_id, &user.user_id)
        .await
        .map_err(map_plan_load_err)?;

    check_version(&plan_state, req.expected_version)?;

    if plan_state.plan.subtasks.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Cannot execute an empty plan",
        ));
    }

    if req.session_id.trim().is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "session_id is required for plan execution",
        ));
    }

    let mut capabilities = if req.step_by_step {
        PlanCapabilities::step_by_step()
    } else {
        PlanCapabilities::auto_execute()
    };
    if let Some(policy) = req.approval_policy {
        capabilities.requires_approval = policy;
    }

    // Flip the first pending subtask to in-progress so the CLI/executor has a
    // clear "current subtask" anchor on pickup.
    for subtask in &mut plan_state.plan.subtasks {
        if subtask.status == TaskStatus::Pending {
            subtask.status = TaskStatus::InProgress;
            break;
        }
    }

    // Pin the routing hint to the executing session, so future loads know who
    // to journal against without a second lookup.
    plan_state.session_hint = Some(req.session_id.clone());

    let goal = plan_state.goal.clone();
    let expected = resolve_expected_version(&plan_state, req.expected_version);
    state
        .plan_repo
        .save(&plan_id, &mut plan_state, expected)
        .await
        .map_err(map_plan_load_err)?;

    state
        .plan_repo
        .set_active_plan(&req.session_id, Some(&plan_id))
        .await
        .map_err(map_plan_load_err)?;

    emit_plan_journal(
        Some(&req.session_id),
        astra_services::session_journal::JournalEvent::plan_lifecycle(
            Some(&req.session_id),
            "execution_started",
            Some(serde_json::json!({
                "plan_id": plan_id,
                "step_by_step": req.step_by_step,
                "version": plan_state.version,
                "subtask_count": plan_state.plan.subtasks.len(),
            })),
        ),
    );
    emit_plan_journal(
        Some(&req.session_id),
        astra_services::session_journal::JournalEvent::goal_steered(
            Some(&req.session_id),
            0,
            "plan",
            None,
            &goal,
            Some(serde_json::json!({ "plan_id": plan_id })),
        ),
    );

    let (completed, failed) = status_counts(&plan_state.plan);

    Ok(Json(PlanStatusResponse {
        plan_id,
        phase: "executing".to_string(),
        goal: plan_state.goal,
        version: plan_state.version,
        progress_pct: plan_state.plan.progress_pct(),
        subtask_count: plan_state.plan.subtasks.len(),
        completed_count: completed,
        failed_count: failed,
        metrics: PlanMetrics::default(),
        capabilities,
    }))
}

/// `POST /plans/{plan_id}/exit-plan-mode` — web-agent counterpart of the
/// `ExitPlanMode` tool. Approving flips the phase hint to `refining` so the
/// next turn's tool-gate re-enables writes; rejecting keeps planning active.
pub(super) async fn exit_plan_mode_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
    Json(req): Json<ExitPlanModeRequest>,
) -> Result<Json<PlanResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    if let Some(md) = &req.plan_md {
        if md.len() > MAX_PLAN_MD_LENGTH {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                format!("plan_md exceeds {MAX_PLAN_MD_LENGTH} characters"),
            ));
        }
    }

    let mut plan_state = state
        .plan_repo
        .load_owned(&plan_id, &user.user_id)
        .await
        .map_err(map_plan_load_err)?;

    check_version(&plan_state, req.expected_version)?;

    // Stash the rendered markdown inside the state blob so web and CLI see
    // the same artifact. Stored under a well-known history key so it round-
    // trips through the existing (user, assistant) log — no schema change.
    if let Some(md) = req.plan_md {
        plan_state.add_turn("__plan_md__", &md);
    }

    let session_hint = plan_state.session_hint.clone();
    let expected = resolve_expected_version(&plan_state, req.expected_version);
    state
        .plan_repo
        .save(&plan_id, &mut plan_state, expected)
        .await
        .map_err(map_plan_load_err)?;

    // On approval, lift the write-tool guard by clearing the session's
    // active_plan_id. The `plans` row stays intact so the approved plan can
    // still drive execution via `/plans/{id}/execute`; we're only releasing
    // the guard, not the plan. Mirrors `tool_exit_plan_mode` so web-agent
    // and server-tool exit paths behave identically. Rejection leaves the
    // plan pinned so the next authoring pass still sees the guard.
    if req.approved {
        if let Some(sid) = session_hint.as_deref() {
            state
                .plan_repo
                .set_active_plan(sid, None)
                .await
                .map_err(map_plan_load_err)?;
        }
    }

    emit_plan_journal(
        session_hint.as_deref(),
        astra_services::session_journal::JournalEvent::plan_lifecycle(
            session_hint.as_deref(),
            if req.approved {
                "plan_approved"
            } else {
                "plan_rejected"
            },
            Some(serde_json::json!({
                "plan_id": plan_id,
                "version": plan_state.version,
                "subtask_count": plan_state.plan.subtasks.len(),
            })),
        ),
    );

    let phase_name = if req.approved { "refining" } else { "planning" };
    let capabilities = capabilities_for_phase_name(phase_name);

    Ok(Json(PlanResponse {
        plan_id,
        phase: phase_name.to_string(),
        goal: plan_state.goal,
        version: plan_state.version,
        plan: Some(plan_state.plan),
        capabilities,
        metrics: PlanMetrics::default(),
    }))
}

/// `POST /plans/{plan_id}/rewind` — reset one anchor + every subtask after it.
pub(super) async fn rewind_plan_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
    Json(req): Json<RewindRequest>,
) -> Result<Json<RewindResponse>, (StatusCode, Json<ErrorResponse>)> {
    use astra_plan::{PlanRewindAnchor, resolve_rewind_start_index, rewind_plan_from_subtask};

    let user = state.auth_service.current_user(&headers).await?;

    let anchor = req.anchor.trim();
    if anchor.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "anchor is required",
        ));
    }
    let anchor_parsed = if let Ok(n) = anchor.parse::<usize>() {
        PlanRewindAnchor::OneBased(n)
    } else {
        PlanRewindAnchor::IdPrefix(anchor.to_string())
    };

    let mut plan_state = state
        .plan_repo
        .load_owned(&plan_id, &user.user_id)
        .await
        .map_err(map_plan_load_err)?;
    check_version(&plan_state, req.expected_version)?;

    let idx = resolve_rewind_start_index(&plan_state.plan, &anchor_parsed)
        .map_err(|e| error_response(StatusCode::BAD_REQUEST, e))?;
    let reset_count = rewind_plan_from_subtask(&mut plan_state.plan, idx);

    plan_state
        .timeline
        .record(crate::plan::TimelineEventKind::SubtaskRewound {
            anchor: anchor.to_string(),
            from_idx: idx,
            reset_count,
            reason: req.reason.clone(),
        });

    let session_hint = plan_state.session_hint.clone();
    let expected = resolve_expected_version(&plan_state, req.expected_version);
    state
        .plan_repo
        .save(&plan_id, &mut plan_state, expected)
        .await
        .map_err(map_plan_load_err)?;

    emit_plan_journal(
        session_hint.as_deref(),
        astra_services::session_journal::JournalEvent::plan_edit(
            session_hint.as_deref(),
            "rewind",
            Some(serde_json::json!({
                "plan_id": plan_id,
                "anchor": anchor,
                "from_idx": idx,
                "reset_count": reset_count,
                "reason": req.reason,
                "version": plan_state.version,
            })),
        ),
    );

    Ok(Json(RewindResponse {
        plan_id,
        reset_count,
        version: plan_state.version,
        plan: plan_state.plan,
    }))
}

/// `POST /plans/{plan_id}/redo-step` — reset a single subtask for re-execution.
pub(super) async fn redo_step_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
    Json(req): Json<RedoStepRequest>,
) -> Result<Json<RedoStepResponse>, (StatusCode, Json<ErrorResponse>)> {
    use astra_plan::{PlanRewindAnchor, resolve_rewind_start_index};

    let user = state.auth_service.current_user(&headers).await?;

    let sid = req.subtask_id.trim();
    if sid.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "subtask_id is required",
        ));
    }

    let mut plan_state = state
        .plan_repo
        .load_owned(&plan_id, &user.user_id)
        .await
        .map_err(map_plan_load_err)?;
    check_version(&plan_state, req.expected_version)?;

    let idx = resolve_rewind_start_index(
        &plan_state.plan,
        &PlanRewindAnchor::IdPrefix(sid.to_string()),
    )
    .map_err(|e| error_response(StatusCode::BAD_REQUEST, e))?;

    plan_state.plan.subtasks[idx].reset_for_redo();
    let title = plan_state.plan.subtasks[idx].title.clone();
    let resolved_subtask_id = plan_state.plan.subtasks[idx].id.clone();

    // Compute the next attempt number by counting prior runs for this subtask.
    // The LocalCachePlanRepository returns an empty vec so attempt starts at 1
    // there — that's fine for offline/test paths.
    let prior_runs = state
        .plan_repo
        .list_step_runs(&plan_id, Some(&resolved_subtask_id), DEFAULT_RUNS_LIMIT)
        .await
        .map_err(map_plan_load_err)?;
    let next_attempt: i32 = prior_runs.iter().map(|r| r.attempt).max().unwrap_or(0) + 1;

    plan_state
        .timeline
        .record(crate::plan::TimelineEventKind::SubtaskRedone {
            subtask_id: resolved_subtask_id.clone(),
            title: title.clone(),
            attempt: next_attempt as u32,
        });

    let session_hint = plan_state.session_hint.clone();
    let expected = resolve_expected_version(&plan_state, req.expected_version);
    state
        .plan_repo
        .save(&plan_id, &mut plan_state, expected)
        .await
        .map_err(map_plan_load_err)?;

    emit_plan_journal(
        session_hint.as_deref(),
        astra_services::session_journal::JournalEvent::plan_edit(
            session_hint.as_deref(),
            "redo_step",
            Some(serde_json::json!({
                "plan_id": plan_id,
                "subtask_id": resolved_subtask_id,
                "title": title,
                "attempt": next_attempt,
                "version": plan_state.version,
            })),
        ),
    );

    Ok(Json(RedoStepResponse {
        plan_id,
        subtask_id: resolved_subtask_id,
        attempt: next_attempt,
        version: plan_state.version,
    }))
}

/// `POST /plans/{plan_id}/step-runs` — record the start of a subtask attempt.
/// Returns `run_id` so the executor can pair the subsequent finish call.
pub(super) async fn start_step_run_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
    Json(req): Json<StartStepRunRequest>,
) -> Result<(StatusCode, Json<StartStepRunResponse>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    let plan_state = state
        .plan_repo
        .load_owned(&plan_id, &user.user_id)
        .await
        .map_err(map_plan_load_err)?;

    if req.session_id.trim().is_empty() || req.request_id.trim().is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "session_id and request_id are required",
        ));
    }

    let subtask = plan_state
        .plan
        .subtasks
        .iter()
        .find(|s| s.id == req.subtask_id)
        .ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                format!("subtask {} not found in plan {}", req.subtask_id, plan_id),
            )
        })?;

    let run_id = state
        .plan_repo
        .record_step_run(astra_plan::NewStepRun {
            plan_id: &plan_id,
            subtask_id: &req.subtask_id,
            attempt: req.attempt,
            status: TaskStatus::InProgress,
            session_id: &req.session_id,
            request_id: &req.request_id,
        })
        .await
        .map_err(map_plan_load_err)?;

    let (completed, _) = status_counts(&plan_state.plan);
    emit_plan_journal(
        Some(&req.session_id),
        astra_services::session_journal::JournalEvent::plan_progress(
            Some(&req.session_id),
            0,
            &req.subtask_id,
            &subtask.title,
            "started",
            plan_state.plan.progress_pct(),
            plan_state.plan.subtasks.len(),
            completed,
        ),
    );

    Ok((
        StatusCode::CREATED,
        Json(StartStepRunResponse {
            run_id,
            subtask_id: req.subtask_id,
            attempt: req.attempt,
        }),
    ))
}

/// `POST /plans/{plan_id}/step-runs/completed` — record an attempt that
/// already reached a terminal state in a single HTTP call. The CLI executor's
/// happy path takes this route: a subtask completes, one POST creates the
/// finalized row. Saves one round-trip vs. start + finish.
///
/// Rejects non-terminal statuses (`pending`/`in_progress`) — those must use
/// `POST /step-runs` + `/finish` pair so the intermediate state is observable.
pub(super) async fn post_completed_step_run_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
    Json(req): Json<CompletedStepRunRequest>,
) -> Result<(StatusCode, Json<StartStepRunResponse>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    if req.session_id.trim().is_empty() || req.request_id.trim().is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "session_id and request_id are required",
        ));
    }
    if !req.status.is_terminal() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!(
                "status {:?} is not terminal; use POST /step-runs + /finish \
                 for attempts that start as pending/in_progress",
                req.status
            ),
        ));
    }

    let plan_state = state
        .plan_repo
        .load_owned(&plan_id, &user.user_id)
        .await
        .map_err(map_plan_load_err)?;

    let subtask = plan_state
        .plan
        .subtasks
        .iter()
        .find(|s| s.id == req.subtask_id)
        .ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                format!("subtask {} not found in plan {}", req.subtask_id, plan_id),
            )
        })?;

    let run_id = state
        .plan_repo
        .record_completed_step_run(
            astra_plan::NewStepRun {
                plan_id: &plan_id,
                subtask_id: &req.subtask_id,
                attempt: req.attempt,
                status: req.status,
                session_id: &req.session_id,
                request_id: &req.request_id,
            },
            req.error.as_deref(),
            req.artifact_ref.as_deref(),
        )
        .await
        .map_err(map_plan_load_err)?;

    let (completed, _) = status_counts(&plan_state.plan);
    let action = match req.status {
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
        _ => "finished",
    };
    emit_plan_journal(
        Some(&req.session_id),
        astra_services::session_journal::JournalEvent::plan_progress(
            Some(&req.session_id),
            0,
            &req.subtask_id,
            &subtask.title,
            action,
            plan_state.plan.progress_pct(),
            plan_state.plan.subtasks.len(),
            completed,
        ),
    );

    Ok((
        StatusCode::CREATED,
        Json(StartStepRunResponse {
            run_id,
            subtask_id: req.subtask_id,
            attempt: req.attempt,
        }),
    ))
}

/// `POST /plans/{plan_id}/step-runs/{run_id}/finish` — finalize an attempt.
/// Must happen exactly once per `run_id`; the repository rejects duplicate
/// finalizations with 404 (row already has finished_at set).
pub(super) async fn finish_step_run_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((plan_id, run_id)): Path<(String, String)>,
    Json(req): Json<FinishStepRunRequest>,
) -> Result<Json<PlanStepRun>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    // Ownership check so unrelated users can't finalize someone else's runs.
    let plan_state = state
        .plan_repo
        .load_owned(&plan_id, &user.user_id)
        .await
        .map_err(map_plan_load_err)?;

    state
        .plan_repo
        .finalize_step_run(
            &plan_id,
            &run_id,
            req.status,
            req.error.as_deref(),
            req.artifact_ref.as_deref(),
        )
        .await
        .map_err(map_plan_load_err)?;

    // Look up the finalized row to return it and journal progress with the
    // right subtask context. Bounded at DEFAULT_RUNS_LIMIT was a bug — a
    // plan with more attempts than the limit would 404 on success. Scan
    // ordered-by-started_at-DESC which is the natural "most recent first"
    // sort, and accept up to 1000 rows; finalize_step_run filters by plan_id
    // so this list is still plan-scoped.
    let runs = state
        .plan_repo
        .list_step_runs(&plan_id, None, 1000)
        .await
        .map_err(map_plan_load_err)?;
    let finalized = runs
        .into_iter()
        .find(|r| r.run_id == run_id)
        .ok_or_else(|| {
            error_response(
                StatusCode::NOT_FOUND,
                format!("step run {run_id} not found after finalize"),
            )
        })?;

    let subtask_title = plan_state
        .plan
        .subtasks
        .iter()
        .find(|s| s.id == finalized.subtask_id)
        .map(|s| s.title.clone())
        .unwrap_or_default();
    let (completed, _) = status_counts(&plan_state.plan);
    let action = match req.status {
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
        _ => "finished",
    };
    emit_plan_journal(
        Some(&finalized.session_id),
        astra_services::session_journal::JournalEvent::plan_progress(
            Some(&finalized.session_id),
            0,
            &finalized.subtask_id,
            &subtask_title,
            action,
            plan_state.plan.progress_pct(),
            plan_state.plan.subtasks.len(),
            completed,
        ),
    );

    Ok(Json(finalized))
}

/// `GET /plans/{plan_id}/step-runs` — list subtask attempt history.
pub(super) async fn list_step_runs_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<ListStepRunsQuery>,
) -> Result<Json<StepRunsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    // Ownership check before listing so we don't leak existence of unowned plans.
    let _ = state
        .plan_repo
        .load_owned(&plan_id, &user.user_id)
        .await
        .map_err(map_plan_load_err)?;

    let runs = state
        .plan_repo
        .list_step_runs(
            &plan_id,
            q.subtask_id.as_deref(),
            q.limit.unwrap_or(DEFAULT_RUNS_LIMIT),
        )
        .await
        .map_err(map_plan_load_err)?;

    Ok(Json(StepRunsResponse { runs }))
}

/// `GET /plans/{plan_id}/status` — get plan execution status.
pub(super) async fn plan_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
) -> Result<Json<PlanStatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    let plan_state = state
        .plan_repo
        .load_owned(&plan_id, &user.user_id)
        .await
        .map_err(map_plan_load_err)?;
    let phase_name = infer_phase_name(&plan_state);
    let capabilities = capabilities_for_phase_name(phase_name);

    let (completed, failed) = status_counts(&plan_state.plan);

    Ok(Json(PlanStatusResponse {
        plan_id,
        phase: phase_name.to_string(),
        goal: plan_state.goal,
        version: plan_state.version,
        progress_pct: plan_state.plan.progress_pct(),
        subtask_count: plan_state.plan.subtasks.len(),
        completed_count: completed,
        failed_count: failed,
        metrics: PlanMetrics::default(),
        capabilities,
    }))
}

/// `DELETE /plans/{plan_id}` — delete a saved plan.
pub(super) async fn delete_plan_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    // Verify ownership before deleting (404, not 403, for non-owners).
    let loaded = state
        .plan_repo
        .load_owned(&plan_id, &user.user_id)
        .await
        .map_err(map_plan_load_err)?;
    let session_hint = loaded.session_hint.clone();

    state
        .plan_repo
        .delete(&plan_id)
        .await
        .map_err(map_plan_load_err)?;

    emit_plan_journal(
        session_hint.as_deref(),
        astra_services::session_journal::JournalEvent::plan_lifecycle(
            session_hint.as_deref(),
            "plan_deleted",
            Some(serde_json::json!({ "plan_id": plan_id })),
        ),
    );

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── infer_phase_name tests ─────────────────────────────────────────────

    #[test]
    fn infer_phase_name_empty_plan_is_planning() {
        let state = PlanModeState::new("build auth".into(), ProjectContext::default());
        assert_eq!(infer_phase_name(&state), "planning");
    }

    #[test]
    fn infer_phase_name_with_pending_subtasks_is_refining() {
        let mut state = PlanModeState::new("add tests".into(), ProjectContext::default());
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "s1".into(),
                title: "Step 1".into(),
                status: TaskStatus::Pending,
                ..Default::default()
            });
        assert_eq!(infer_phase_name(&state), "refining");
    }

    #[test]
    fn infer_phase_name_with_in_progress_subtasks_is_executing() {
        let mut state = PlanModeState::new("add tests".into(), ProjectContext::default());
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "s1".into(),
                title: "Step 1".into(),
                status: TaskStatus::Completed,
                ..Default::default()
            });
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "s2".into(),
                title: "Step 2".into(),
                status: TaskStatus::Pending,
                ..Default::default()
            });
        assert_eq!(infer_phase_name(&state), "executing");
    }

    #[test]
    fn infer_phase_name_all_completed() {
        let mut state = PlanModeState::new("deploy service".into(), ProjectContext::default());
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "s1".into(),
                title: "Step 1".into(),
                status: TaskStatus::Completed,
                ..Default::default()
            });
        assert_eq!(infer_phase_name(&state), "completed");
    }

    // ── error mapping tests ──────────────────────────────────────────────

    #[test]
    fn map_plan_load_err_invalid_id_is_400() {
        let (status, _) = map_plan_load_err(PlanLoadError::InvalidId("../x".into()));
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn map_plan_load_err_not_found_is_404() {
        let (status, _) = map_plan_load_err(PlanLoadError::NotFound("x".into()));
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn map_plan_load_err_version_conflict_is_409() {
        let err = PlanLoadError::conflict(3, 4);
        let (status, _) = map_plan_load_err(err);
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[test]
    fn map_plan_load_err_matches_conflict_variant_not_internal_substring() {
        // Regression for the Internal-starts-with match pattern: an Internal
        // error whose message happens to begin with "version conflict" in its
        // own audit text must NOT be mapped to 409. The handler's match arm
        // must be variant-based, not string-based. Using a misleading prefix
        // here proves that — an Internal("version conflict...") returned 409
        // before the fix.
        let misleading = PlanLoadError::Internal(
            "version conflict detected by downstream audit service, not by us".into(),
        );
        let (status, _) = map_plan_load_err(misleading);
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal errors must stay 500 even when their message text begins with \
             'version conflict' — only the typed Conflict variant should map to 409"
        );
    }

    // ── version check tests ──────────────────────────────────────────────

    #[test]
    fn check_version_passes_when_none() {
        let state = PlanModeState::new("x".into(), ProjectContext::default());
        assert!(check_version(&state, None).is_ok());
    }

    #[test]
    fn check_version_passes_when_matching() {
        let state = PlanModeState::new("x".into(), ProjectContext::default());
        assert!(check_version(&state, Some(state.version)).is_ok());
    }

    #[test]
    fn check_version_fails_when_mismatched() {
        let state = PlanModeState::new("x".into(), ProjectContext::default());
        let (status, _) = check_version(&state, Some(state.version + 99)).unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
    }

    // ── input validation constants ───────────────────────────────────────

    #[test]
    fn max_length_constants() {
        const { assert!(MAX_GOAL_LENGTH >= 1_000) };
        const { assert!(MAX_INSTRUCTION_LENGTH >= 1_000) };
        const { assert!(MAX_PLAN_MD_LENGTH >= 10_000) };
    }

    // ── ownership test ───────────────────────────────────────────────────

    #[test]
    fn new_with_owner_sets_created_by() {
        let state = PlanModeState::new_with_owner(
            "goal".into(),
            ProjectContext::default(),
            "user-123".into(),
        );
        assert_eq!(state.created_by.as_deref(), Some("user-123"));
    }
}
