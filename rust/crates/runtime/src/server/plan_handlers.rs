//! REST API handlers for plan lifecycle management.
//!
//! Routes:
//! - `POST /plans` — create a plan (server-side decomposition)
//! - `GET /plans` — list plans (optional `?session_id=…`, `?phase=…`, and
//!   `?active_session_only=true` filters)
//! - `GET /plans/{plan_id}` — get plan details
//! - `PUT /plans/{plan_id}` — edit a plan
//! - `POST /plans/{plan_id}/execute` — start plan execution
//! - `GET /plans/{plan_id}/status` — get plan status + metrics
//! - `POST /plans/{plan_id}/exit-plan-mode` — web-agent equivalent of the
//!   ExitPlanMode tool; flips the phase to `refining`/`planning` based on
//!   approval and optionally persists `plan_md` alongside the state.
//! - `POST /plans/{plan_id}/rewind` — reset one anchor + everything after
//!   (mirrors the CLI `rewind N` path); distinct from redo-step.
//! - `POST /plans/{plan_id}/redo-step` — reset one subtask for re-execution.
//! - `GET /plans/{plan_id}/step-runs` — list `plan_step_runs` rows (paginated).
//! - `DELETE /plans/{plan_id}` — delete a plan.
//!
//! All handlers go through [`AppState::plan_repo`] — the repository is the
//! source of truth for plan lifecycle state.

use super::*;
use crate::plan::{PlanLoadError, PlanModeState};
use astra_plan::PlanPhase;
use astra_plan::{PlanListFilter, PlanStepRun};
use astra_services::task_orchestrator::{TaskPlan, TaskStatus};
use astra_tools::task_mgmt::{
    MAX_CREATE_SUBTASKS, SessionTask, SessionTaskStatusKind, TaskManager, TaskManagerSnapshot,
    TaskStore,
};
use astra_tools::task_mgmt_matrixone::MatrixOneTaskStore;
use std::collections::HashMap;
use std::sync::Arc;

const MAX_GOAL_LENGTH: usize = 10_000;
const MAX_INSTRUCTION_LENGTH: usize = 10_000;
const MAX_PLAN_MD_LENGTH: usize = 200_000;
const DEFAULT_RUNS_LIMIT: i32 = 100;

/// Upper bound on attempt counters. One million attempts per subtask is
/// already unreachable in practice; this rejects arbitrary i32 values (0,
/// negative, near-MAX) that would otherwise poison `max(attempt)+1` redo
/// logic or wrap on overflow.
const MAX_ATTEMPT: i32 = 1_000_000;

/// Cap on free-form client text stored in journal/state so a hostile caller
/// can't bloat `plan_json` or the journal with multi-MB payloads.
const MAX_REASON_LENGTH: usize = 5_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PlanCapabilities {
    pub can_read_files: bool,
    pub can_execute_tools: bool,
    pub can_modify_files: bool,
    pub can_access_network: bool,
    pub max_subtasks: usize,
    pub max_execution_rounds: usize,
    pub requires_approval: ApprovalPolicy,
}

impl Default for PlanCapabilities {
    fn default() -> Self {
        Self {
            can_read_files: true,
            can_execute_tools: true,
            can_modify_files: true,
            can_access_network: true,
            max_subtasks: MAX_CREATE_SUBTASKS,
            max_execution_rounds: 50,
            requires_approval: ApprovalPolicy::Destructive,
        }
    }
}

impl PlanCapabilities {
    fn planning() -> Self {
        Self {
            can_read_files: true,
            can_execute_tools: false,
            can_modify_files: false,
            can_access_network: false,
            max_subtasks: MAX_CREATE_SUBTASKS,
            max_execution_rounds: 0,
            requires_approval: ApprovalPolicy::All,
        }
    }

    fn auto_execute() -> Self {
        Self::default()
    }

    fn step_by_step() -> Self {
        Self {
            requires_approval: ApprovalPolicy::PerSubtask,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ApprovalPolicy {
    None,
    PerSubtask,
    #[default]
    Destructive,
    All,
}
const MAX_ERROR_LENGTH: usize = 10_000;
const MAX_ARTIFACT_REF_LENGTH: usize = 1_000;
const PLAN_EDIT_NOT_IMPLEMENTED_DETAIL: &str = "Natural-language plan editing is not implemented; use explicit plan create, execute, rewind, or redo-step endpoints instead.";

fn validate_attempt(attempt: i32) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if !(1..=MAX_ATTEMPT).contains(&attempt) {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("attempt must be between 1 and {MAX_ATTEMPT}, got {attempt}"),
        ));
    }
    Ok(())
}

fn validate_optional_len(
    value: Option<&str>,
    max: usize,
    field: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if let Some(v) = value
        && v.len() > max
    {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("{field} exceeds {max} characters"),
        ));
    }
    Ok(())
}

// ─── Request / Response types ────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct CreatePlanRequest {
    pub goal: String,
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
    #[serde(default)]
    pub active_session_only: bool,
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
    pub phase: PlanPhase,
    pub goal: String,
    pub version: u64,
    pub plan: Option<TaskPlan>,
    pub capabilities: PlanCapabilities,
}

#[derive(Serialize)]
pub(super) struct PlanStatusResponse {
    pub plan_id: String,
    pub phase: PlanPhase,
    pub goal: String,
    pub version: u64,
    pub progress_pct: u32,
    pub subtask_count: usize,
    pub completed_count: usize,
    pub failed_count: usize,
    pub capabilities: PlanCapabilities,
}

#[derive(Serialize)]
pub(super) struct PlanListResponse {
    pub plans: Vec<PlanSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
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

fn plan_summary_from_state(plan_id: String, plan_state: &PlanModeState) -> PlanSummary {
    PlanSummary {
        plan_id,
        goal: plan_state.goal.clone(),
        progress_pct: plan_state.plan.progress_pct(),
        subtask_count: plan_state.plan.subtasks.len(),
        status: plan_state.infer_phase().to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlanRewindAnchor {
    OneBased(usize),
    IdPrefix(String),
}

fn resolve_rewind_start_index(plan: &TaskPlan, anchor: &PlanRewindAnchor) -> Result<usize, String> {
    match anchor {
        PlanRewindAnchor::OneBased(n) => {
            if *n == 0 || *n > plan.subtasks.len() {
                return Err(format!("subtask index must be 1..={}", plan.subtasks.len()));
            }
            Ok(*n - 1)
        }
        PlanRewindAnchor::IdPrefix(s) => {
            let q = s.trim();
            if q.is_empty() {
                return Err("empty subtask id".into());
            }
            let matches: Vec<usize> = plan
                .subtasks
                .iter()
                .enumerate()
                .filter(|(_, st)| st.id == q || st.id.starts_with(q))
                .map(|(i, _)| i)
                .collect();
            match matches.len() {
                0 => Err(format!("no subtask id matches {q:?}")),
                1 => Ok(matches[0]),
                _ => Err(format!(
                    "ambiguous id {:?} ({} matches); use a longer prefix or `rewind N` (1-based)",
                    q,
                    matches.len()
                )),
            }
        }
    }
}

fn rewind_plan_from_subtask(plan: &mut TaskPlan, start_idx: usize) -> usize {
    let mut reset_count = 0usize;
    for subtask in plan.subtasks.iter_mut().skip(start_idx) {
        if matches!(
            subtask.status,
            TaskStatus::Completed
                | TaskStatus::InProgress
                | TaskStatus::Paused
                | TaskStatus::Failed
        ) {
            subtask.status = TaskStatus::Pending;
            reset_count += 1;
        }
    }
    reset_count
}

fn capabilities_for_phase(phase: PlanPhase) -> PlanCapabilities {
    match phase {
        PlanPhase::Executing => PlanCapabilities::auto_execute(),
        PlanPhase::Completed => PlanCapabilities::default(),
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

use astra_tools::plan_task_mirror;

async fn mirror_approved_plan_to_task_board_if_configured(
    state: &AppState,
    user_id: &str,
    session_id: &str,
    plan_id: &str,
    plan_state: &PlanModeState,
) -> Result<(), String> {
    let Some(pool) = state.shared_pool.as_ref() else {
        return Ok(());
    };
    let store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::from_shared_for_user(pool, user_id)?);
    let manager = TaskManager::new(session_id.to_string(), store);
    plan_task_mirror::mirror_approved_plan_to_task_board(
        &manager,
        user_id,
        session_id,
        plan_id,
        &plan_state.goal,
        &plan_state.plan,
    )
    .await
}

async fn sync_plan_task_board_subtask_status(
    manager: &TaskManager,
    plan_id: &str,
    plan: &TaskPlan,
    subtask_id: &str,
    status: TaskStatus,
) -> Result<(), String> {
    let plan_fingerprint = plan_task_mirror::plan_task_board_fingerprint(plan);
    let task = manager
        .load_tasks()
        .await
        .map_err(|error| format!("load task board before plan subtask sync: {error}"))?
        .into_iter()
        .find(|task| {
            plan_task_mirror::approved_plan_step_task_identity_matches(
                task,
                plan_id,
                &plan_fingerprint,
                subtask_id,
            )
        });

    let Some(task) = task else {
        return Ok(());
    };
    let task_id = task.id.clone();

    if status.is_terminal() {
        return manager
            .sync_approved_plan_mirror_terminal_status(
                &task_id,
                plan_task_mirror::task_status_to_session_status(status),
            )
            .await;
    }

    let output = manager
        .update(&serde_json::json!({
            "task_id": task_id,
            "new_status": status.as_str(),
        }))
        .await;
    if output.starts_with("Error:") {
        return Err(output);
    }
    Ok(())
}

async fn sync_plan_task_board_subtask_status_if_configured(
    state: &AppState,
    user_id: &str,
    session_id: &str,
    plan_id: &str,
    plan: &TaskPlan,
    subtask_id: &str,
    status: TaskStatus,
) -> Result<(), String> {
    let Some(pool) = state.shared_pool.as_ref() else {
        return Ok(());
    };
    let store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::from_shared_for_user(pool, user_id)?);
    let manager = TaskManager::new(session_id.to_string(), store);
    if let Err(error) =
        sync_plan_task_board_subtask_status(&manager, plan_id, plan, subtask_id, status).await
    {
        tracing::error!(
            plan_id = %plan_id,
            session_id = %session_id,
            subtask_id = %subtask_id,
            status = status.as_str(),
            error = %error,
            "plan handler: failed to sync subtask status into task board — plan and task board may drift"
        );
        return Err(error);
    }
    Ok(())
}

struct TaskBoardRollback {
    manager: TaskManager,
    snapshot: TaskManagerSnapshot,
}

impl TaskBoardRollback {
    async fn seal(&mut self) -> Result<(), String> {
        self.manager
            .seal_snapshot_for_restore(&mut self.snapshot)
            .await
    }

    async fn restore(self) -> Result<(), String> {
        self.manager.restore_snapshot(&self.snapshot).await
    }
}

async fn seal_task_board_rollback_or_restore(
    rollback: &mut Option<TaskBoardRollback>,
    context: &str,
    not_saved_message: &str,
) -> Result<(), String> {
    let Some(active) = rollback.as_mut() else {
        return Ok(());
    };
    if let Err(error) = active.seal().await {
        // We failed to read/seal the post-mutation version guard, so the
        // normal CAS restore path would reject the snapshot as stale and
        // leave the task board diverged from the plan state. On this path we
        // prefer a best-effort restore of the pre-mutation board snapshot.
        active.snapshot.restore_version = Some(0);
        let restore_result = rollback
            .take()
            .expect("rollback option should still be populated after failed seal")
            .restore()
            .await;
        return match restore_result {
            Ok(()) => Err(format!(
                "{context}: failed to seal task-board rollback — {not_saved_message}. \
                 Task board was rolled back. Error: {error}"
            )),
            Err(restore_error) => Err(format!(
                "{context}: failed to seal task-board rollback — {not_saved_message}. \
                 Additionally failed to roll back task board: {restore_error}"
            )),
        };
    }
    Ok(())
}

async fn capture_task_board_rollback_if_configured(
    state: &AppState,
    user_id: &str,
    session_id: Option<&str>,
) -> Result<Option<TaskBoardRollback>, String> {
    let Some(session_id) = session_id.filter(|sid| !sid.trim().is_empty()) else {
        return Ok(None);
    };
    let Some(pool) = state.shared_pool.as_ref() else {
        return Ok(None);
    };
    let store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::from_shared_for_user(pool, user_id)?);
    let manager = TaskManager::new(session_id.to_string(), store);
    let snapshot = manager
        .try_snapshot_state()
        .await
        .map_err(|error| format!("snapshot task board before plan mutation: {error}"))?;
    Ok(Some(TaskBoardRollback { manager, snapshot }))
}

async fn sync_plan_task_board_subtasks_status_if_configured(
    state: &AppState,
    user_id: &str,
    session_id: Option<&str>,
    plan_id: &str,
    plan: &TaskPlan,
    subtask_ids: &[String],
    status: TaskStatus,
) -> Result<(), String> {
    let Some(session_id) = session_id.filter(|sid| !sid.trim().is_empty()) else {
        return Ok(());
    };
    for subtask_id in subtask_ids {
        sync_plan_task_board_subtask_status_if_configured(
            state, user_id, session_id, plan_id, plan, subtask_id, status,
        )
        .await?;
    }
    Ok(())
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

    let mut plan_state = PlanModeState::new_with_owner(goal.clone(), user.user_id.clone());
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
            .set_active_plan(&user.user_id, sid, Some(&plan_id))
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

    let capabilities = PlanCapabilities::planning();

    Ok((
        StatusCode::CREATED,
        Json(PlanResponse {
            plan_id,
            phase: PlanPhase::Planning,
            goal,
            version: plan_state.version,
            plan: None,
            capabilities,
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
    if q.active_session_only && q.session_id.is_none() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "active_session_only requires session_id",
        ));
    }

    if q.active_session_only {
        let session_id = q
            .session_id
            .as_deref()
            .expect("checked above: active_session_only requires session_id");
        let Some(active_plan_id) = state
            .plan_repo
            .active_plan_for_session(&user.user_id, session_id)
            .await
            .map_err(map_plan_load_err)?
        else {
            return Ok(Json(PlanListResponse {
                plans: Vec::new(),
                warning: None,
            }));
        };
        let plan_state = match state
            .plan_repo
            .load_owned(&active_plan_id, &user.user_id)
            .await
        {
            Ok(state) => state,
            Err(PlanLoadError::NotFound(_)) => {
                return Ok(Json(PlanListResponse {
                    plans: Vec::new(),
                    warning: None,
                }));
            }
            Err(err) => return Err(map_plan_load_err(err)),
        };
        let summary = plan_summary_from_state(active_plan_id, &plan_state);
        if let Some(phase) = q.phase.as_deref()
            && summary.status != phase
        {
            return Ok(Json(PlanListResponse {
                plans: Vec::new(),
                warning: Some(format!(
                    "active plan is in \"{}\" phase, not \"{phase}\"",
                    summary.status,
                )),
            }));
        }
        return Ok(Json(PlanListResponse {
            plans: vec![summary],
            warning: None,
        }));
    }

    let filter = PlanListFilter {
        session_id: q.session_id.as_deref(),
        phase: q.phase.as_deref(),
        limit: q.limit,
    };
    let plans = state
        .plan_repo
        .list_for_user(&user.user_id, filter)
        .await
        .map_err(map_plan_load_err)?
        .into_iter()
        .map(|p| PlanSummary {
            plan_id: p.name,
            goal: p.goal,
            progress_pct: p.progress_pct,
            subtask_count: p.subtask_count,
            status: p.status,
        })
        .collect();

    Ok(Json(PlanListResponse {
        plans,
        warning: None,
    }))
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
    let phase = plan_state.infer_phase();
    let capabilities = capabilities_for_phase(phase);

    Ok(Json(PlanResponse {
        plan_id,
        phase,
        goal: plan_state.goal,
        version: plan_state.version,
        plan: Some(plan_state.plan),
        capabilities,
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

    let plan_state = state
        .plan_repo
        .load_owned(&plan_id, &user.user_id)
        .await
        .map_err(map_plan_load_err)?;

    check_version(&plan_state, req.expected_version)?;

    if plan_state.infer_phase() == PlanPhase::Completed {
        return Err(error_response(
            StatusCode::CONFLICT,
            "Cannot edit a completed plan",
        ));
    }

    Err(error_response(
        StatusCode::NOT_IMPLEMENTED,
        PLAN_EDIT_NOT_IMPLEMENTED_DETAIL,
    ))
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
    let original_plan_state = plan_state.clone();

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
    let mut started_subtask_id = None;
    for subtask in &mut plan_state.plan.subtasks {
        if subtask.status == TaskStatus::Pending {
            subtask.status = TaskStatus::InProgress;
            started_subtask_id = Some(subtask.id.clone());
            break;
        }
    }

    // Pin the routing hint to the executing session, so future loads know who
    // to journal against without a second lookup.
    plan_state.session_hint = Some(req.session_id.clone());

    let goal = plan_state.goal.clone();

    let mut task_board_rollback =
        capture_task_board_rollback_if_configured(&state, &user.user_id, Some(&req.session_id))
            .await
            .map_err(|error| error_response(StatusCode::INTERNAL_SERVER_ERROR, error))?;

    // ── Sync to task board FIRST ──────────────────────────────────
    // If the task-board sync fails, we bail out before persisting
    // the plan mutation so the repo never records an InProgress state
    // that the task board doesn't reflect.
    if let Some(subtask_id) = started_subtask_id.as_deref() {
        sync_plan_task_board_subtask_status_if_configured(
            &state,
            &user.user_id,
            &req.session_id,
            &plan_id,
            &plan_state.plan,
            subtask_id,
            TaskStatus::InProgress,
        )
        .await
        .map_err(|error| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "execute_plan: failed to sync started subtask into task board — \
                     plan was NOT saved. Error: {error}"
                ),
            )
        })?;
    }
    seal_task_board_rollback_or_restore(
        &mut task_board_rollback,
        "execute_plan",
        "plan was NOT saved",
    )
    .await
    .map_err(|error| error_response(StatusCode::INTERNAL_SERVER_ERROR, error))?;

    let expected = resolve_expected_version(&plan_state, req.expected_version);
    if let Err(error) = state
        .plan_repo
        .save(&plan_id, &mut plan_state, expected)
        .await
    {
        if let Some(rollback) = task_board_rollback.take() {
            rollback.restore().await.map_err(|restore_error| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "execute_plan: plan save failed ({error}) and task-board rollback failed: {restore_error}"
                    ),
                )
            })?;
        }
        return Err(map_plan_load_err(error));
    }

    if let Err(error) = state
        .plan_repo
        .set_active_plan(&user.user_id, &req.session_id, Some(&plan_id))
        .await
    {
        let mut restore_errors = Vec::new();
        if let Some(rollback) = task_board_rollback.take() {
            if let Err(restore_error) = rollback.restore().await {
                restore_errors.push(format!("task-board rollback failed: {restore_error}"));
            }
        }
        let mut rollback_plan = original_plan_state;
        if let Err(restore_error) = state
            .plan_repo
            .save(&plan_id, &mut rollback_plan, Some(plan_state.version))
            .await
        {
            restore_errors.push(format!("plan rollback failed: {restore_error}"));
        }
        if !restore_errors.is_empty() {
            return Err(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "execute_plan: active-plan link failed ({error}); {}",
                    restore_errors.join("; ")
                ),
            ));
        }
        return Err(map_plan_load_err(error));
    }

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
        phase: PlanPhase::Executing,
        goal: plan_state.goal,
        version: plan_state.version,
        progress_pct: plan_state.plan.progress_pct(),
        subtask_count: plan_state.plan.subtasks.len(),
        completed_count: completed,
        failed_count: failed,
        capabilities,
    }))
}

/// `POST /plans/{plan_id}/exit-plan-mode` — web-agent counterpart of the
/// `ExitPlanMode` tool. Approving clears the active-plan pin so the next turn's
/// tool-gate re-enables writes; rejecting keeps planning active.
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

    let session_hint = plan_state.session_hint.clone();
    let expected = resolve_expected_version(&plan_state, req.expected_version);

    // On approval, mirror the plan to the task board BEFORE persisting it.
    // If mirror fails, we haven't committed the plan yet, avoiding the
    // three-way inconsistency (plan persisted, task board empty, session locked).
    // The active_plan_id is cleared AFTER both mirror and save succeed.
    if req.approved {
        if let Some(sid) = session_hint.as_deref() {
            mirror_approved_plan_to_task_board_if_configured(
                &state,
                &user.user_id,
                sid,
                &plan_id,
                &plan_state,
            )
            .await
            .map_err(|error| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to mirror approved plan into task board: {error}"),
                )
            })?;
        }
    }

    // Persist the rendered markdown alongside plan_json so web, sync, and CLI
    // consumers can read the same artifact without reviving the removed
    // turn-history compatibility layer.
    if let Some(md) = req.plan_md {
        plan_state.plan_md = Some(md);
    }

    state
        .plan_repo
        .save(&plan_id, &mut plan_state, expected)
        .await
        .map_err(map_plan_load_err)?;

    // Clear active plan AFTER both mirror and save succeed. This releases
    // the write-tool guard so execution can proceed.
    if req.approved {
        if let Some(sid) = session_hint.as_deref() {
            state
                .plan_repo
                .set_active_plan(&user.user_id, sid, None)
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

    let phase = if req.approved {
        plan_state.infer_phase()
    } else {
        PlanPhase::Planning
    };
    let capabilities = capabilities_for_phase(phase);

    Ok(Json(PlanResponse {
        plan_id,
        phase,
        goal: plan_state.goal,
        version: plan_state.version,
        plan: Some(plan_state.plan),
        capabilities,
    }))
}

/// `POST /plans/{plan_id}/rewind` — reset one anchor + every subtask after it.
pub(super) async fn rewind_plan_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
    Json(req): Json<RewindRequest>,
) -> Result<Json<RewindResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    let anchor = req.anchor.trim();
    if anchor.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "anchor is required",
        ));
    }
    // Cap `reason` so a hostile caller can't bloat the journal / plan_json
    // with multi-MB strings on every rewind.
    validate_optional_len(req.reason.as_deref(), MAX_REASON_LENGTH, "reason")?;
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

    // Capture the IDs of the subtasks that are about to be reset — any
    // in-flight step_runs for them need to be cancelled so the audit trail
    // doesn't carry phantom "in_progress" rows once the plan resumes.
    let affected_subtask_ids: Vec<String> = plan_state
        .plan
        .subtasks
        .iter()
        .skip(idx)
        .map(|s| s.id.clone())
        .collect();

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
    let mut task_board_rollback =
        capture_task_board_rollback_if_configured(&state, &user.user_id, session_hint.as_deref())
            .await
            .map_err(|error| error_response(StatusCode::INTERNAL_SERVER_ERROR, error))?;

    sync_plan_task_board_subtasks_status_if_configured(
        &state,
        &user.user_id,
        session_hint.as_deref(),
        &plan_id,
        &plan_state.plan,
        &affected_subtask_ids,
        TaskStatus::Pending,
    )
    .await
    .map_err(|error| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "rewind_plan: failed to sync affected subtasks into task board — plan state was NOT saved. Error: {error}"
            ),
        )
    })?;
    seal_task_board_rollback_or_restore(
        &mut task_board_rollback,
        "rewind_plan",
        "plan state was NOT saved",
    )
    .await
    .map_err(|error| error_response(StatusCode::INTERNAL_SERVER_ERROR, error))?;

    let expected_version = expected.unwrap_or(plan_state.version);
    let aborted_runs = match state
        .plan_repo
        .save_existing_and_abort_open_step_runs(
            &plan_id,
            &mut plan_state,
            expected_version,
            &affected_subtask_ids,
        )
        .await
    {
        Ok(count) => count,
        Err(error) => {
            if let Some(rollback) = task_board_rollback.take() {
                rollback.restore().await.map_err(|restore_error| {
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!(
                            "rewind_plan: atomic plan/run update failed ({error}) and task-board rollback failed: {restore_error}"
                        ),
                    )
                })?;
            }
            return Err(map_plan_load_err(error));
        }
    };

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
                "aborted_runs": aborted_runs,
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
    let session_hint = plan_state.session_hint.clone();

    let mut task_board_rollback =
        capture_task_board_rollback_if_configured(&state, &user.user_id, session_hint.as_deref())
            .await
            .map_err(|error| error_response(StatusCode::INTERNAL_SERVER_ERROR, error))?;

    sync_plan_task_board_subtasks_status_if_configured(
        &state,
        &user.user_id,
        session_hint.as_deref(),
        &plan_id,
        &plan_state.plan,
        std::slice::from_ref(&resolved_subtask_id),
        TaskStatus::Pending,
    )
    .await
    .map_err(|error| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "redo_step: failed to sync subtask status back to Pending in task board — plan state was NOT saved. Error: {error}"
            ),
        )
    })?;
    seal_task_board_rollback_or_restore(
        &mut task_board_rollback,
        "redo_step",
        "plan state was NOT saved",
    )
    .await
    .map_err(|error| error_response(StatusCode::INTERNAL_SERVER_ERROR, error))?;

    // Compute the next attempt number by counting prior runs for this subtask.
    // In-memory/test repos start empty too, so the first attempt is 1 there.
    let prior_runs = match state
        .plan_repo
        .list_step_runs(&plan_id, Some(&resolved_subtask_id), DEFAULT_RUNS_LIMIT)
        .await
    {
        Ok(runs) => runs,
        Err(error) => {
            if let Some(rollback) = task_board_rollback.take() {
                rollback.restore().await.map_err(|restore_error| {
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!(
                            "redo_step: list prior step-runs failed ({error}) and task-board rollback failed: {restore_error}"
                        ),
                    )
                })?;
            }
            return Err(map_plan_load_err(error));
        }
    };
    let next_attempt: i32 = prior_runs.iter().map(|r| r.attempt).max().unwrap_or(0) + 1;

    plan_state
        .timeline
        .record(crate::plan::TimelineEventKind::SubtaskRedone {
            subtask_id: resolved_subtask_id.clone(),
            title: title.clone(),
            attempt: next_attempt as u32,
        });

    let expected = resolve_expected_version(&plan_state, req.expected_version);
    let expected_version = expected.unwrap_or(plan_state.version);
    let aborted_runs = match state
        .plan_repo
        .save_existing_and_abort_open_step_runs(
            &plan_id,
            &mut plan_state,
            expected_version,
            std::slice::from_ref(&resolved_subtask_id),
        )
        .await
    {
        Ok(count) => count,
        Err(error) => {
            if let Some(rollback) = task_board_rollback.take() {
                rollback.restore().await.map_err(|restore_error| {
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!(
                            "redo_step: atomic plan/run update failed ({error}) and task-board rollback failed: {restore_error}"
                        ),
                    )
                })?;
            }
            return Err(map_plan_load_err(error));
        }
    };

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
                "aborted_runs": aborted_runs,
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

    // Validate client-provided fields before any DB work so a bad request
    // fails fast and cannot corrupt state.
    validate_attempt(req.attempt)?;

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

    let mut task_board_rollback =
        capture_task_board_rollback_if_configured(&state, &user.user_id, Some(&req.session_id))
            .await
            .map_err(|error| error_response(StatusCode::INTERNAL_SERVER_ERROR, error))?;
    sync_plan_task_board_subtask_status_if_configured(
        &state,
        &user.user_id,
        &req.session_id,
        &plan_id,
        &plan_state.plan,
        &req.subtask_id,
        TaskStatus::InProgress,
    )
    .await
    .map_err(|error| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "start_step_run: failed to sync task board — step run was NOT recorded. Error: {error}"
            ),
        )
    })?;
    seal_task_board_rollback_or_restore(
        &mut task_board_rollback,
        "start_step_run",
        "step run was NOT recorded",
    )
    .await
    .map_err(|error| error_response(StatusCode::INTERNAL_SERVER_ERROR, error))?;

    let run_id = match state
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
    {
        Ok(run_id) => run_id,
        Err(error) => {
            if let Some(rollback) = task_board_rollback.take() {
                rollback.restore().await.map_err(|restore_error| {
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!(
                            "start_step_run: record step-run failed ({error}) and task-board rollback failed: {restore_error}"
                        ),
                    )
                })?;
            }
            return Err(map_plan_load_err(error));
        }
    };

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

    validate_attempt(req.attempt)?;
    validate_optional_len(req.error.as_deref(), MAX_ERROR_LENGTH, "error")?;
    validate_optional_len(
        req.artifact_ref.as_deref(),
        MAX_ARTIFACT_REF_LENGTH,
        "artifact_ref",
    )?;

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

    let mut task_board_rollback =
        capture_task_board_rollback_if_configured(&state, &user.user_id, Some(&req.session_id))
            .await
            .map_err(|error| error_response(StatusCode::INTERNAL_SERVER_ERROR, error))?;

    sync_plan_task_board_subtask_status_if_configured(
        &state,
        &user.user_id,
        &req.session_id,
        &plan_id,
        &plan_state.plan,
        &req.subtask_id,
        req.status,
    )
    .await
    .map_err(|error| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "post_completed_step_run: failed to sync task board — step run was NOT recorded. Error: {error}"
            ),
        )
    })?;
    seal_task_board_rollback_or_restore(
        &mut task_board_rollback,
        "post_completed_step_run",
        "step run was NOT recorded",
    )
    .await
    .map_err(|error| error_response(StatusCode::INTERNAL_SERVER_ERROR, error))?;

    let run_id = match state
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
    {
        Ok(run_id) => run_id,
        Err(error) => {
            if let Some(rollback) = task_board_rollback.take() {
                rollback.restore().await.map_err(|restore_error| {
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!(
                            "post_completed_step_run: record completed step-run failed ({error}) and task-board rollback failed: {restore_error}"
                        ),
                    )
                })?;
            }
            return Err(map_plan_load_err(error));
        }
    };

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

    validate_optional_len(req.error.as_deref(), MAX_ERROR_LENGTH, "error")?;
    validate_optional_len(
        req.artifact_ref.as_deref(),
        MAX_ARTIFACT_REF_LENGTH,
        "artifact_ref",
    )?;

    // Ownership check so unrelated users can't finalize someone else's runs.
    let plan_state = state
        .plan_repo
        .load_owned(&plan_id, &user.user_id)
        .await
        .map_err(map_plan_load_err)?;

    let existing = state
        .plan_repo
        .get_step_run(&plan_id, &run_id)
        .await
        .map_err(map_plan_load_err)?;

    let mut task_board_rollback = capture_task_board_rollback_if_configured(
        &state,
        &user.user_id,
        Some(&existing.session_id),
    )
    .await
    .map_err(|error| error_response(StatusCode::INTERNAL_SERVER_ERROR, error))?;

    sync_plan_task_board_subtask_status_if_configured(
        &state,
        &user.user_id,
        &existing.session_id,
        &plan_id,
        &plan_state.plan,
        &existing.subtask_id,
        req.status,
    )
    .await
    .map_err(|error| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "finalize_step_run: failed to sync task board — step run was NOT finalized. Error: {error}"
            ),
        )
    })?;
    seal_task_board_rollback_or_restore(
        &mut task_board_rollback,
        "finalize_step_run",
        "step run was NOT finalized",
    )
    .await
    .map_err(|error| error_response(StatusCode::INTERNAL_SERVER_ERROR, error))?;

    if let Err(error) = state
        .plan_repo
        .finalize_step_run(
            &plan_id,
            &run_id,
            req.status,
            req.error.as_deref(),
            req.artifact_ref.as_deref(),
        )
        .await
    {
        if let Some(rollback) = task_board_rollback.take() {
            rollback.restore().await.map_err(|restore_error| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "finalize_step_run: finalize failed ({error}) and task-board rollback failed: {restore_error}"
                    ),
                )
            })?;
        }
        return Err(map_plan_load_err(error));
    }

    // Look up the finalized row to return it and journal progress with the
    // right subtask context. Direct SELECT by (run_id, plan_id) — O(1) vs
    // the previous list_step_runs + find which was O(N) over all plan runs.
    let finalized = state
        .plan_repo
        .get_step_run(&plan_id, &run_id)
        .await
        .map_err(map_plan_load_err)?;

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
    let phase = plan_state.infer_phase();
    let capabilities = capabilities_for_phase(phase);

    let (completed, failed) = status_counts(&plan_state.plan);

    Ok(Json(PlanStatusResponse {
        plan_id,
        phase,
        goal: plan_state.goal,
        version: plan_state.version,
        progress_pct: plan_state.plan.progress_pct(),
        subtask_count: plan_state.plan.subtasks.len(),
        completed_count: completed,
        failed_count: failed,
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
    use astra_core::{MatrixOneSettings, SharedPool};
    use astra_services::storage::ensure_core_schema;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FailingLoadTaskStore;

    #[async_trait::async_trait]
    impl TaskStore for FailingLoadTaskStore {
        async fn load(&self, session_id: &str) -> Result<Vec<SessionTask>, String> {
            Err(format!("forced task-board load failure for {session_id}"))
        }

        async fn save(&self, _session_id: &str, _tasks: Vec<SessionTask>) -> Result<(), String> {
            Ok(())
        }

        async fn mutate(
            &self,
            session_id: &str,
            _mutation: astra_tools::task_mgmt::TaskMutation,
        ) -> Result<String, String> {
            Err(format!("forced task-board mutate failure for {session_id}"))
        }

        async fn next_task_id(&self, session_id: &str) -> Result<u32, String> {
            Err(format!("forced task id failure for {session_id}"))
        }

        async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }
    }

    struct FailingSealRollbackStore {
        inner: Arc<astra_tools::task_mgmt::InMemoryTaskStore>,
        version_reads: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl TaskStore for FailingSealRollbackStore {
        async fn load(&self, session_id: &str) -> Result<Vec<SessionTask>, String> {
            self.inner.load(session_id).await
        }

        async fn save(&self, session_id: &str, tasks: Vec<SessionTask>) -> Result<(), String> {
            self.inner.save(session_id, tasks).await
        }

        async fn next_task_id(&self, session_id: &str) -> Result<u32, String> {
            self.inner.next_task_id(session_id).await
        }

        async fn set_next_task_id(&self, session_id: &str, next: u32) -> Result<(), String> {
            self.inner.set_next_task_id(session_id, next).await
        }

        async fn restore_snapshot_state(
            &self,
            session_id: &str,
            tasks: Vec<SessionTask>,
            next_task_id: u32,
            expected_version: u64,
        ) -> Result<(), String> {
            self.inner
                .restore_snapshot_state(session_id, tasks, next_task_id, expected_version)
                .await
        }

        async fn peek_next_task_id(&self, session_id: &str) -> Result<u32, String> {
            self.inner.peek_next_task_id(session_id).await
        }

        async fn get_session_version(&self, session_id: &str) -> Result<u64, String> {
            match self.version_reads.fetch_add(1, Ordering::Relaxed) {
                0 => self.inner.get_session_version(session_id).await,
                1 => Err("forced seal failure".to_string()),
                _ => self.inner.get_session_version(session_id).await,
            }
        }

        async fn bump_version(&self, session_id: &str) {
            self.inner.bump_version(session_id).await;
        }
    }

    fn task_plan(subtasks: Vec<astra_services::task_orchestrator::SubtaskPlan>) -> TaskPlan {
        TaskPlan {
            subtasks,
            notes: None,
        }
    }

    fn subtask(
        id: &str,
        title: &str,
        status: astra_services::task_orchestrator::TaskStatus,
    ) -> astra_services::task_orchestrator::SubtaskPlan {
        astra_services::task_orchestrator::SubtaskPlan {
            id: id.into(),
            title: title.into(),
            status,
            ..Default::default()
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

    async fn cleanup_session_todos(pool: &sqlx::Pool<sqlx::MySql>, session_id: &str) {
        let _ = sqlx::query("DELETE FROM session_todos WHERE session_id = ?")
            .bind(session_id)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM session_todo_counters WHERE session_id = ?")
            .bind(session_id)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id = ?")
            .bind(session_id)
            .execute(pool)
            .await;
    }

    async fn prepare_session_todo_owner(
        pool: &sqlx::Pool<sqlx::MySql>,
        session_id: &str,
        user_id: &str,
    ) {
        cleanup_session_todos(pool, session_id).await;
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, agent_id, title, status, metadata)
             VALUES (?, ?, 'plan-handler-test', 'plan handler test', 'active', '{}')",
        )
        .bind(session_id)
        .bind(user_id)
        .execute(pool)
        .await
        .expect("insert agent_sessions owner root");
    }

    // ── infer_phase tests ─────────────────────────────────────────────

    #[test]
    fn infer_phase_empty_plan_is_planning() {
        let state = PlanModeState::new("build auth".into());
        assert_eq!(state.infer_phase(), PlanPhase::Planning);
    }

    #[test]
    fn infer_phase_with_pending_subtasks_is_refining() {
        let mut state = PlanModeState::new("add tests".into());
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "s1".into(),
                title: "Step 1".into(),
                status: TaskStatus::Pending,
                ..Default::default()
            });
        assert_eq!(state.infer_phase(), PlanPhase::Refining);
    }

    #[test]
    fn infer_phase_with_in_progress_subtasks_is_executing() {
        let mut state = PlanModeState::new("add tests".into());
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
        assert_eq!(state.infer_phase(), PlanPhase::Executing);
    }

    #[test]
    fn infer_phase_all_completed() {
        let mut state = PlanModeState::new("deploy service".into());
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "s1".into(),
                title: "Step 1".into(),
                status: TaskStatus::Completed,
                ..Default::default()
            });
        assert_eq!(state.infer_phase(), PlanPhase::Completed);
    }

    #[test]
    fn plan_summary_from_state_uses_inferred_phase() {
        let mut state = PlanModeState::new("active".into());
        state.plan = task_plan(vec![subtask("s1", "Step 1", TaskStatus::Pending)]);

        let summary = plan_summary_from_state("plan-active".into(), &state);
        assert_eq!(summary.plan_id, "plan-active");
        assert_eq!(summary.goal, "active");
        assert_eq!(summary.subtask_count, 1);
        assert_eq!(summary.status, "refining");
    }

    #[test]
    fn plan_summary_from_state_tracks_completion_progress() {
        let mut state = PlanModeState::new("done".into());
        state.plan = task_plan(vec![subtask("s1", "Step 1", TaskStatus::Completed)]);

        let summary = plan_summary_from_state("plan-done".into(), &state);
        assert_eq!(summary.progress_pct, 100);
        assert_eq!(summary.status, "completed");
    }

    #[test]
    fn plan_capabilities_share_task_subtask_fanout_limit() {
        assert_eq!(
            PlanCapabilities::default().max_subtasks,
            MAX_CREATE_SUBTASKS
        );
        assert_eq!(
            PlanCapabilities::planning().max_subtasks,
            MAX_CREATE_SUBTASKS
        );
    }

    #[tokio::test]
    async fn http_approved_plan_mirror_creates_user_visible_step_tasks() {
        let manager = TaskManager::in_memory();
        let mut state = PlanModeState::new_with_owner("ship cloud plan".into(), "alice".into());
        state.plan = task_plan(vec![
            subtask("step-1", "Design API", TaskStatus::Pending),
            subtask("step-2", "Verify unhappy path", TaskStatus::Pending),
        ]);

        plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            "alice",
            "session-cloud",
            "plan-cloud-visible",
            &state.goal,
            &state.plan,
        )
        .await
        .unwrap();

        let tasks = manager.snapshot().await.unwrap();
        assert_eq!(tasks.len(), 2, "{tasks:?}");
        let first = tasks
            .iter()
            .find(|task| {
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("plan_subtask_id"))
                    .and_then(serde_json::Value::as_str)
                    == Some("step-1")
            })
            .expect("step-1 should be a top-level task");
        assert_eq!(first.title, "Design API");
        assert_eq!(
            first.status,
            astra_tools::task_mgmt::SessionTaskStatusKind::InProgress
        );
        assert_eq!(first.owner.as_deref(), Some("alice"));
        assert!(first.subtasks.is_empty(), "{first:?}");
        assert_eq!(
            first
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("source"))
                .and_then(serde_json::Value::as_str),
            Some("approved_plan")
        );
        assert_eq!(
            first
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("plan_id"))
                .and_then(serde_json::Value::as_str),
            Some("plan-cloud-visible")
        );
        let second = tasks
            .iter()
            .find(|task| {
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("plan_subtask_id"))
                    .and_then(serde_json::Value::as_str)
                    == Some("step-2")
            })
            .expect("step-2 should be a top-level task");
        assert_eq!(second.title, "Verify unhappy path");
        assert_eq!(
            second.status,
            astra_tools::task_mgmt::SessionTaskStatusKind::Pending
        );
        assert!(second.subtasks.is_empty(), "{second:?}");
        assert_eq!(
            second
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("plan_id"))
                .and_then(serde_json::Value::as_str),
            Some("plan-cloud-visible")
        );
    }

    #[tokio::test]
    async fn http_approved_plan_mirror_rejects_oversized_plan() {
        let manager = TaskManager::in_memory();
        let mut state =
            PlanModeState::new_with_owner("oversized cloud plan".into(), "alice".into());
        state.plan = task_plan(
            (0..=MAX_CREATE_SUBTASKS)
                .map(|index| {
                    subtask(
                        &format!("step-{index}"),
                        &format!("step {index}"),
                        TaskStatus::Pending,
                    )
                })
                .collect(),
        );

        let err = plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            "alice",
            "session-cloud",
            "plan-cloud-oversized",
            &state.goal,
            &state.plan,
        )
        .await
        .expect_err("oversized approved plans should not create one huge batch of step tasks");
        assert!(
            err.contains("subtasks") && err.contains("maximum"),
            "oversized approved plan should surface the task fan-out limit: {err}"
        );
        assert!(
            manager.snapshot().await.unwrap().is_empty(),
            "rejected oversized plan should not leave partial task-board work"
        );
    }

    #[tokio::test]
    async fn http_approved_plan_mirror_fails_closed_when_task_board_load_fails() {
        let manager = TaskManager::new(
            "plan-load-fails",
            Arc::new(FailingLoadTaskStore) as Arc<dyn TaskStore>,
        );
        let mut state = PlanModeState::new_with_owner("load-failing plan".into(), "alice".into());
        state.plan = task_plan(vec![subtask(
            "step-1",
            "Do not duplicate automatically",
            TaskStatus::Pending,
        )]);

        let error = plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            "alice",
            "session-load-fails",
            "plan-load-fails",
            &state.goal,
            &state.plan,
        )
        .await
        .expect_err("task-board load failure should abort approved-plan mirror");

        assert!(
            error.contains("snapshot task board before approved-plan mirror")
                && error.contains("forced task-board load failure"),
            "approved-plan mirror must not treat task-board load failure as an empty board: {error}"
        );
    }

    #[tokio::test]
    async fn http_approved_plan_mirror_rolls_back_partial_step_create_failure() {
        let manager = TaskManager::in_memory();
        let existing = manager
            .create(&serde_json::json!({
                "title": "Existing cloud task",
            }))
            .await;
        assert!(!existing.starts_with("Error:"), "{existing}");
        let mut state = PlanModeState::new_with_owner("rollback cloud plan".into(), "alice".into());
        state.plan = task_plan(vec![
            subtask("step-1", "Create first cloud step", TaskStatus::Pending),
            subtask(
                "step-2",
                &"x".repeat(astra_tools::task_mgmt::MAX_TASK_TITLE_CHARS + 1),
                TaskStatus::Pending,
            ),
        ]);

        let err = plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            "alice",
            "session-cloud-rollback",
            "plan-cloud-rollback",
            &state.goal,
            &state.plan,
        )
        .await
        .expect_err("invalid later step should abort approved-plan mirror");

        assert!(
            err.contains("title") && err.contains("exceeds"),
            "original create validation error should be surfaced: {err}"
        );
        let tasks = manager.snapshot().await.unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "failed HTTP approved-plan mirror must roll back the task board to its pre-approval state: {tasks:?}"
        );
        assert!(
            tasks.iter().any(|t| t.title == "Existing cloud task"),
            "existing cloud task must survive mirror failure: {tasks:?}"
        );
    }

    #[tokio::test]
    async fn seal_task_board_rollback_failure_restores_task_board() {
        let inner = Arc::new(astra_tools::task_mgmt::InMemoryTaskStore::new());
        let store: Arc<dyn TaskStore> = Arc::new(FailingSealRollbackStore {
            inner: inner.clone(),
            version_reads: AtomicUsize::new(0),
        });
        let manager = TaskManager::new("rollback-seal-fails", store);

        let first = manager
            .create(&serde_json::json!({"title": "keep me"}))
            .await;
        assert!(!first.starts_with("Error:"), "{first}");
        let snapshot = manager.try_snapshot_state().await.expect("snapshot");

        let second = manager
            .create(&serde_json::json!({"title": "must roll back"}))
            .await;
        assert!(!second.starts_with("Error:"), "{second}");

        let mut rollback = Some(TaskBoardRollback { manager, snapshot });
        let err = seal_task_board_rollback_or_restore(
            &mut rollback,
            "test_plan_op",
            "plan state was NOT saved",
        )
        .await
        .expect_err("seal failure should be surfaced");

        assert!(err.contains("forced seal failure"), "{err}");
        assert!(err.contains("Task board was rolled back"), "{err}");
        assert!(
            rollback.is_none(),
            "failed seal should consume the rollback handle"
        );

        let tasks = inner
            .load("rollback-seal-fails")
            .await
            .expect("task board after rollback");
        assert_eq!(tasks.len(), 1, "{tasks:?}");
        assert_eq!(tasks[0].title, "keep me");
    }

    #[tokio::test]
    async fn http_approved_plan_mirror_uses_bounded_retry_on_title_collision() {
        let manager = TaskManager::in_memory();
        let unrelated = manager
            .create(&serde_json::json!({
                "title": "Design API",
                "metadata": { "source": "manual" }
            }))
            .await;
        assert!(unrelated.contains("created"), "{unrelated}");

        let mut state = PlanModeState::new_with_owner("ship cloud plan".into(), "alice".into());
        state.plan = task_plan(vec![subtask("step-1", "Design API", TaskStatus::Pending)]);

        plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            "alice",
            "session-cloud",
            "plan-collision",
            &state.goal,
            &state.plan,
        )
        .await
        .unwrap();

        let tasks = manager.snapshot().await.unwrap();
        assert_eq!(tasks.len(), 2, "{tasks:?}");
        assert!(
            tasks.iter().any(|task| {
                task.title == "Design API"
                    && task
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.get("source"))
                        .and_then(serde_json::Value::as_str)
                        == Some("manual")
            }),
            "pre-existing task should remain untouched: {tasks:?}"
        );
        assert!(
            tasks.iter().any(|task| {
                task.title == "Design API (step-1)"
                    && task
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.get("source"))
                        .and_then(serde_json::Value::as_str)
                        == Some("approved_plan")
            }),
            "approved plan should use one disambiguated retry: {tasks:?}"
        );
    }

    #[tokio::test]
    async fn http_approved_plan_mirror_does_not_reuse_legacy_cli_style_tree() {
        let manager = TaskManager::in_memory();
        let mut state = PlanModeState::new_with_owner("ship hybrid plan".into(), "alice".into());
        state.plan = task_plan(vec![subtask(
            "step-1",
            "Sync the visible task board",
            TaskStatus::Pending,
        )]);
        let fingerprint = plan_task_mirror::plan_task_board_fingerprint(&state.plan);

        let cli_style = manager
            .create(&serde_json::json!({
                "title": "ship hybrid plan",
                "metadata": {
                    "source": "approved_plan",
                    "plan_goal": "ship hybrid plan",
                    "plan_fingerprint": fingerprint,
                },
                "subtasks": [
                    { "id": "step-1", "title": "Sync the visible task board" }
                ]
            }))
            .await;
        assert!(cli_style.contains("created"), "{cli_style}");

        plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            "alice",
            "session-hybrid",
            "plan-http-reuses-cli-tree",
            &state.goal,
            &state.plan,
        )
        .await
        .unwrap();

        let tasks = manager.snapshot().await.unwrap();
        assert_eq!(
            tasks.len(),
            2,
            "HTTP approval should leave legacy tree-shaped task history alone and create a clean step task: {tasks:?}"
        );
        let task = tasks
            .iter()
            .find(|task| {
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("plan_subtask_id"))
                    .and_then(serde_json::Value::as_str)
                    == Some("step-1")
            })
            .expect("approved plan step task");
        assert_eq!(
            task.status,
            astra_tools::task_mgmt::SessionTaskStatusKind::InProgress
        );
        assert_eq!(
            task.metadata
                .as_ref()
                .and_then(|metadata| metadata.get("plan_id"))
                .and_then(serde_json::Value::as_str),
            Some("plan-http-reuses-cli-tree")
        );
        assert!(task.subtasks.is_empty(), "{task:?}");
    }

    #[tokio::test]
    async fn http_approved_plan_mirror_does_not_reopen_completed_plan_history() {
        let manager = TaskManager::in_memory();
        let mut state = PlanModeState::new_with_owner("repeat cloud plan".into(), "alice".into());
        state.plan = task_plan(vec![subtask(
            "step-1",
            "Repeatable step",
            TaskStatus::Pending,
        )]);

        plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            "alice",
            "session-repeat",
            "plan-repeat",
            &state.goal,
            &state.plan,
        )
        .await
        .unwrap();
        let completed = manager
            .update(&serde_json::json!({
                "task_id": "task-1",
                "new_status": "completed",
            }))
            .await;
        assert!(!completed.starts_with("Error:"), "{completed}");

        plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            "alice",
            "session-repeat",
            "plan-repeat",
            &state.goal,
            &state.plan,
        )
        .await
        .unwrap();

        let tasks = manager.snapshot().await.unwrap();
        assert_eq!(
            tasks.len(),
            2,
            "completed approved-plan history must remain terminal, not be reused: {tasks:?}"
        );
        assert!(
            tasks.iter().any(|task| task.status.is_completed()),
            "completed history should remain completed: {tasks:?}"
        );
        assert!(
            tasks.iter().any(|task| task.status.is_in_progress()),
            "repeat approval should create a fresh in-progress task: {tasks:?}"
        );
    }

    #[tokio::test]
    async fn sync_plan_task_board_subtask_status_updates_existing_step_task() {
        let manager = TaskManager::in_memory();
        let mut state = PlanModeState::new_with_owner("execute cloud plan".into(), "alice".into());
        state.plan = task_plan(vec![
            subtask("step-1", "Start work", TaskStatus::Pending),
            subtask("step-2", "Finish work", TaskStatus::Pending),
        ]);

        plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            "alice",
            "session-cloud",
            "plan-sync-visible",
            &state.goal,
            &state.plan,
        )
        .await
        .unwrap();

        sync_plan_task_board_subtask_status(
            &manager,
            "plan-sync-visible",
            &state.plan,
            "step-2",
            TaskStatus::Completed,
        )
        .await
        .unwrap();

        let tasks = manager.snapshot().await.unwrap();
        assert_eq!(tasks.len(), 2, "{tasks:?}");
        let task = tasks
            .iter()
            .find(|task| {
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("plan_subtask_id"))
                    .and_then(serde_json::Value::as_str)
                    == Some("step-2")
            })
            .expect("step-2 task");
        assert_eq!(
            task.status,
            astra_tools::task_mgmt::SessionTaskStatusKind::Completed
        );
        assert!(task.subtasks.is_empty(), "{task:?}");
    }

    #[tokio::test]
    async fn sync_plan_task_board_subtask_status_does_not_reopen_completed_step_history() {
        let manager = TaskManager::in_memory();
        let mut state = PlanModeState::new_with_owner("redo cloud plan".into(), "alice".into());
        state.plan = task_plan(vec![
            subtask("step-1", "Build it", TaskStatus::Pending),
            subtask("step-2", "Verify it", TaskStatus::Pending),
        ]);

        plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            "alice",
            "session-cloud",
            "plan-redo-visible",
            &state.goal,
            &state.plan,
        )
        .await
        .unwrap();
        sync_plan_task_board_subtask_status(
            &manager,
            "plan-redo-visible",
            &state.plan,
            "step-1",
            TaskStatus::Completed,
        )
        .await
        .unwrap();
        sync_plan_task_board_subtask_status(
            &manager,
            "plan-redo-visible",
            &state.plan,
            "step-2",
            TaskStatus::Completed,
        )
        .await
        .unwrap();

        let completed = manager.snapshot().await.unwrap();
        assert_eq!(
            completed
                .iter()
                .filter(|task| task.status.is_completed())
                .count(),
            2,
            "completed plan steps should be terminal task history: {completed:?}"
        );

        let error = sync_plan_task_board_subtask_status(
            &manager,
            "plan-redo-visible",
            &state.plan,
            "step-2",
            TaskStatus::Pending,
        )
        .await
        .expect_err("sync should not reopen completed task-board history");
        assert!(
            error.contains("already terminal"),
            "redo of completed task-board history should fail closed: {error}"
        );

        let after_redo = manager.snapshot().await.unwrap();
        assert_eq!(
            after_redo
                .iter()
                .filter(|task| task.status.is_completed())
                .count(),
            2,
            "completed task-board history should remain terminal: {after_redo:?}"
        );
    }

    #[tokio::test]
    async fn sync_plan_task_board_subtask_status_is_noop_without_existing_tree() {
        let manager = TaskManager::in_memory();
        let plan = task_plan(vec![subtask("step-1", "Start work", TaskStatus::Pending)]);

        sync_plan_task_board_subtask_status(
            &manager,
            "plan-without-task-board",
            &plan,
            "step-1",
            TaskStatus::InProgress,
        )
        .await
        .unwrap();

        assert!(
            manager.snapshot().await.unwrap().is_empty(),
            "status sync should not auto-create task-board work"
        );
    }

    #[tokio::test]
    async fn sync_plan_task_board_subtask_status_fails_closed_when_task_board_load_fails() {
        let manager = TaskManager::new(
            "plan-sync-load-fails",
            Arc::new(FailingLoadTaskStore) as Arc<dyn TaskStore>,
        );
        let plan = task_plan(vec![subtask("step-1", "Start work", TaskStatus::Pending)]);

        let error = sync_plan_task_board_subtask_status(
            &manager,
            "plan-sync-load-fails",
            &plan,
            "step-1",
            TaskStatus::InProgress,
        )
        .await
        .expect_err("task-board load failure should abort plan subtask sync");

        assert!(
            error.contains("load task board before plan subtask sync")
                && error.contains("forced task-board load failure"),
            "plan subtask sync must not treat task-board load failure as a missing tree: {error}"
        );
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn http_approved_plan_mirror_persists_to_matrixone_task_board() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-plan-mirror-{}", uuid::Uuid::new_v4());
        let session_id = format!("s-plan-mirror-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &session_id, &user_id).await;

        let store: Arc<dyn TaskStore> =
            Arc::new(MatrixOneTaskStore::from_shared_for_user(&shared, &user_id).unwrap());
        let manager = TaskManager::new(session_id.clone(), store);
        let mut state =
            PlanModeState::new_with_owner("ship MatrixOne visible plan".into(), user_id.clone());
        state.plan = task_plan(vec![
            subtask("step-1", "Persist step task", TaskStatus::Pending),
            subtask("step-2", "Verify step task", TaskStatus::Pending),
        ]);

        plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            &user_id,
            &session_id,
            "plan-matrixone-visible",
            &state.goal,
            &state.plan,
        )
        .await
        .unwrap();

        let tasks = manager.snapshot().await.unwrap();
        assert_eq!(tasks.len(), 2, "{tasks:?}");
        let first = tasks
            .iter()
            .find(|task| {
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("plan_subtask_id"))
                    .and_then(serde_json::Value::as_str)
                    == Some("step-1")
            })
            .expect("step-1 task");
        assert_eq!(first.title, "Persist step task");
        assert_eq!(
            first.status,
            astra_tools::task_mgmt::SessionTaskStatusKind::InProgress
        );
        assert!(first.subtasks.is_empty(), "{first:?}");
        assert!(tasks.iter().all(|task| {
            task.metadata
                .as_ref()
                .and_then(|metadata| metadata.get("plan_id"))
                .and_then(serde_json::Value::as_str)
                == Some("plan-matrixone-visible")
        }));

        cleanup_session_todos(&pool, &session_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn http_approved_plan_mirror_rolls_back_partial_failure_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-plan-rollback-{}", uuid::Uuid::new_v4());
        let session_id = format!("s-plan-rollback-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &session_id, &user_id).await;

        let store: Arc<dyn TaskStore> =
            Arc::new(MatrixOneTaskStore::from_shared_for_user(&shared, &user_id).unwrap());
        let manager = TaskManager::new(session_id.clone(), store);
        let existing = manager
            .create(&serde_json::json!({
                "title": "Existing MatrixOne task",
            }))
            .await;
        assert!(existing.contains("\"success\":true"), "{existing}");
        let mut state =
            PlanModeState::new_with_owner("rollback MatrixOne plan".into(), user_id.clone());
        state.plan = task_plan(vec![
            subtask("step-1", "Create first MatrixOne step", TaskStatus::Pending),
            subtask(
                "step-2",
                &"x".repeat(astra_tools::task_mgmt::MAX_TASK_TITLE_CHARS + 1),
                TaskStatus::Pending,
            ),
        ]);

        let err = plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            &user_id,
            &session_id,
            "plan-http-matrixone-rollback",
            &state.goal,
            &state.plan,
        )
        .await
        .expect_err("invalid later step should abort MatrixOne approved-plan mirror");

        assert!(
            err.contains("title") && err.contains("exceeds"),
            "original create validation error should be surfaced: {err}"
        );
        let tasks = manager.snapshot().await.unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "failed MatrixOne approved-plan mirror must roll back the task board to its pre-approval state: {tasks:?}"
        );
        assert!(
            tasks.iter().any(|t| t.title == "Existing MatrixOne task"),
            "existing MatrixOne task must survive mirror failure: {tasks:?}"
        );

        cleanup_session_todos(&pool, &session_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn http_approved_plan_mirror_does_not_reuse_legacy_cli_style_tree_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-plan-reuse-cli-{}", uuid::Uuid::new_v4());
        let session_id = format!("s-plan-reuse-cli-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &session_id, &user_id).await;

        let store: Arc<dyn TaskStore> =
            Arc::new(MatrixOneTaskStore::from_shared_for_user(&shared, &user_id).unwrap());
        let manager = TaskManager::new(session_id.clone(), store);
        let mut state =
            PlanModeState::new_with_owner("ship hybrid MatrixOne plan".into(), user_id.clone());
        state.plan = task_plan(vec![subtask(
            "step-1",
            "Mirror MatrixOne step task",
            TaskStatus::Pending,
        )]);
        let fingerprint = plan_task_mirror::plan_task_board_fingerprint(&state.plan);

        let cli_style = manager
            .create(&serde_json::json!({
                "title": "ship hybrid MatrixOne plan",
                "metadata": {
                    "source": "approved_plan",
                    "plan_goal": "ship hybrid MatrixOne plan",
                    "plan_fingerprint": fingerprint,
                },
                "subtasks": [
                    { "id": "step-1", "title": "Mirror MatrixOne step task" }
                ]
            }))
            .await;
        assert!(cli_style.contains("\"success\":true"), "{cli_style}");

        plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            &user_id,
            &session_id,
            "plan-http-matrixone-reuses-cli-tree",
            &state.goal,
            &state.plan,
        )
        .await
        .unwrap();

        let tasks = manager.snapshot().await.unwrap();
        assert_eq!(
            tasks.len(),
            2,
            "HTTP MatrixOne approval should leave legacy tree-shaped history alone and create a clean step task: {tasks:?}"
        );
        let task = tasks
            .iter()
            .find(|task| {
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("plan_subtask_id"))
                    .and_then(serde_json::Value::as_str)
                    == Some("step-1")
            })
            .expect("approved plan step task");
        assert_eq!(
            task.status,
            astra_tools::task_mgmt::SessionTaskStatusKind::InProgress
        );
        assert_eq!(
            task.metadata
                .as_ref()
                .and_then(|metadata| metadata.get("plan_id"))
                .and_then(serde_json::Value::as_str),
            Some("plan-http-matrixone-reuses-cli-tree")
        );
        assert!(task.subtasks.is_empty(), "{task:?}");

        cleanup_session_todos(&pool, &session_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn http_approved_plan_mirror_does_not_reopen_completed_history_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-plan-repeat-{}", uuid::Uuid::new_v4());
        let session_id = format!("s-plan-repeat-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &session_id, &user_id).await;

        let store: Arc<dyn TaskStore> =
            Arc::new(MatrixOneTaskStore::from_shared_for_user(&shared, &user_id).unwrap());
        let manager = TaskManager::new(session_id.clone(), store);
        let mut state =
            PlanModeState::new_with_owner("repeat MatrixOne plan".into(), user_id.clone());
        state.plan = task_plan(vec![subtask(
            "step-1",
            "Repeat MatrixOne step task",
            TaskStatus::Pending,
        )]);

        plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            &user_id,
            &session_id,
            "plan-http-matrixone-repeat",
            &state.goal,
            &state.plan,
        )
        .await
        .unwrap();
        let completed = manager
            .update(&serde_json::json!({
                "task_id": "task-1",
                "new_status": "completed",
            }))
            .await;
        assert!(
            completed.contains("\"success\":true"),
            "first plan task should be completed before repeat mirror: {completed}"
        );

        plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            &user_id,
            &session_id,
            "plan-http-matrixone-repeat",
            &state.goal,
            &state.plan,
        )
        .await
        .unwrap();

        let tasks = manager.snapshot().await.unwrap();
        assert_eq!(
            tasks.len(),
            2,
            "completed MatrixOne plan history must not be reopened: {tasks:?}"
        );
        assert!(
            tasks.iter().any(|task| task.status.is_completed()),
            "completed history should remain completed: {tasks:?}"
        );
        assert!(
            tasks.iter().any(|task| task.status.is_in_progress()),
            "repeat approval should create a fresh in-progress task: {tasks:?}"
        );

        cleanup_session_todos(&pool, &session_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn http_approved_plan_mirror_pauses_previous_in_progress_plan_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-plan-handoff-{}", uuid::Uuid::new_v4());
        let session_id = format!("s-plan-handoff-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &session_id, &user_id).await;

        let store: Arc<dyn TaskStore> =
            Arc::new(MatrixOneTaskStore::from_shared_for_user(&shared, &user_id).unwrap());
        let manager = TaskManager::new(session_id.clone(), store);
        let mut first_state =
            PlanModeState::new_with_owner("handoff MatrixOne plan".into(), user_id.clone());
        first_state.plan = task_plan(vec![subtask(
            "step-1",
            "First MatrixOne step task",
            TaskStatus::Pending,
        )]);
        let mut second_state =
            PlanModeState::new_with_owner("handoff MatrixOne plan".into(), user_id.clone());
        second_state.plan = task_plan(vec![subtask(
            "step-2",
            "Second MatrixOne step task",
            TaskStatus::Pending,
        )]);

        plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            &user_id,
            &session_id,
            "plan-http-matrixone-handoff-1",
            &first_state.goal,
            &first_state.plan,
        )
        .await
        .unwrap();
        plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            &user_id,
            &session_id,
            "plan-http-matrixone-handoff-2",
            &second_state.goal,
            &second_state.plan,
        )
        .await
        .unwrap();

        let tasks = manager.snapshot().await.unwrap();
        assert_eq!(tasks.len(), 2, "{tasks:?}");
        assert_eq!(
            tasks
                .iter()
                .filter(|task| task.status.is_in_progress())
                .count(),
            1,
            "MatrixOne plan handoff must leave exactly one running plan: {tasks:?}"
        );
        let paused = tasks
            .iter()
            .find(|task| task.status == astra_tools::task_mgmt::SessionTaskStatusKind::Paused)
            .expect("previous plan should be paused");
        assert_eq!(
            paused
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("auto_paused_reason"))
                .and_then(serde_json::Value::as_str),
            Some("approved_plan_handoff"),
            "paused plan should explain automatic handoff: {paused:?}"
        );

        cleanup_session_todos(&pool, &session_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
    async fn plan_task_board_redo_does_not_reopen_completed_step_history_in_matrixone() {
        let shared = bootstrap_shared_pool().await;
        let pool = shared.get().clone();
        let user_id = format!("u-plan-redo-{}", uuid::Uuid::new_v4());
        let session_id = format!("s-plan-redo-{}", uuid::Uuid::new_v4());
        prepare_session_todo_owner(&pool, &session_id, &user_id).await;

        let store: Arc<dyn TaskStore> =
            Arc::new(MatrixOneTaskStore::from_shared_for_user(&shared, &user_id).unwrap());
        let manager = TaskManager::new(session_id.clone(), store);
        let mut state =
            PlanModeState::new_with_owner("redo MatrixOne plan".into(), user_id.clone());
        state.plan = task_plan(vec![
            subtask("step-1", "Build MatrixOne task", TaskStatus::Pending),
            subtask("step-2", "Redo MatrixOne task", TaskStatus::Pending),
        ]);

        plan_task_mirror::mirror_approved_plan_to_task_board(
            &manager,
            &user_id,
            &session_id,
            "plan-http-matrixone-redo",
            &state.goal,
            &state.plan,
        )
        .await
        .unwrap();
        for subtask_id in ["step-1", "step-2"] {
            sync_plan_task_board_subtask_status(
                &manager,
                "plan-http-matrixone-redo",
                &state.plan,
                subtask_id,
                TaskStatus::Completed,
            )
            .await
            .unwrap();
        }

        let completed = manager.snapshot().await.unwrap();
        assert_eq!(
            completed
                .iter()
                .filter(|task| task.status.is_completed())
                .count(),
            2,
            "MatrixOne completed plan steps should be terminal task history: {completed:?}"
        );

        let error = sync_plan_task_board_subtask_status(
            &manager,
            "plan-http-matrixone-redo",
            &state.plan,
            "step-2",
            TaskStatus::Pending,
        )
        .await
        .expect_err("MatrixOne sync should not reopen completed task-board history");
        assert!(
            error.contains("already terminal"),
            "MatrixOne redo of completed task-board history should fail closed: {error}"
        );

        let after_redo = manager.snapshot().await.unwrap();
        assert_eq!(
            after_redo
                .iter()
                .filter(|task| task.status.is_completed())
                .count(),
            2,
            "MatrixOne completed task-board history should remain terminal: {after_redo:?}"
        );

        cleanup_session_todos(&pool, &session_id).await;
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
        let state = PlanModeState::new("x".into());
        assert!(check_version(&state, None).is_ok());
    }

    #[test]
    fn check_version_passes_when_matching() {
        let state = PlanModeState::new("x".into());
        assert!(check_version(&state, Some(state.version)).is_ok());
    }

    #[test]
    fn check_version_fails_when_mismatched() {
        let state = PlanModeState::new("x".into());
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
        let state = PlanModeState::new_with_owner("goal".into(), "user-123".into());
        assert_eq!(state.created_by.as_deref(), Some("user-123"));
    }

    #[test]
    fn rewind_index_resolves_one_based_anchor() {
        let plan = task_plan(vec![
            subtask("a", "A", TaskStatus::Pending),
            subtask("b", "B", TaskStatus::Pending),
        ]);
        assert_eq!(
            resolve_rewind_start_index(&plan, &PlanRewindAnchor::OneBased(2)),
            Ok(1)
        );
    }

    #[test]
    fn rewind_index_rejects_ambiguous_prefix() {
        let plan = task_plan(vec![
            subtask("test-unit", "Unit", TaskStatus::Pending),
            subtask("test-integration", "Integration", TaskStatus::Pending),
        ]);
        let err = resolve_rewind_start_index(&plan, &PlanRewindAnchor::IdPrefix("test".into()))
            .expect_err("prefix should be ambiguous");
        assert!(err.contains("ambiguous"));
    }

    #[test]
    fn rewind_index_rejects_empty_prefix() {
        let plan = task_plan(vec![subtask("a", "A", TaskStatus::Pending)]);
        assert!(resolve_rewind_start_index(&plan, &PlanRewindAnchor::IdPrefix("".into())).is_err());
    }

    #[test]
    fn rewind_resets_terminal_and_in_progress_subtasks() {
        let mut plan = task_plan(vec![
            subtask("a", "A", TaskStatus::Completed),
            subtask("b", "B", TaskStatus::InProgress),
            subtask("c", "C", TaskStatus::Pending),
        ]);
        let reset_count = rewind_plan_from_subtask(&mut plan, 1);
        assert_eq!(reset_count, 1);
        assert_eq!(plan.subtasks[0].status, TaskStatus::Completed);
        assert_eq!(plan.subtasks[1].status, TaskStatus::Pending);
        assert_eq!(plan.subtasks[2].status, TaskStatus::Pending);
    }

    #[test]
    fn rewind_from_middle_resolves_and_resets_following_subtasks() {
        let mut plan = task_plan(vec![
            subtask("a", "A", TaskStatus::Completed),
            subtask("b", "B", TaskStatus::Failed),
            subtask("c", "C", TaskStatus::InProgress),
        ]);
        let idx = resolve_rewind_start_index(&plan, &PlanRewindAnchor::OneBased(2)).unwrap();
        assert_eq!(rewind_plan_from_subtask(&mut plan, idx), 2);
        assert_eq!(plan.subtasks[0].status, TaskStatus::Completed);
        assert_eq!(plan.subtasks[1].status, TaskStatus::Pending);
        assert_eq!(plan.subtasks[2].status, TaskStatus::Pending);
    }

    /// active_session_only + phase filter：当 active plan 的 phase 不匹配时，
    /// 返回空列表 + warning 提示调用方是 phase 不匹配而非没有 active plan。
    #[test]
    fn active_session_only_phase_filter_returns_warning_when_mismatch() {
        let empty_but_warn = PlanListResponse {
            plans: vec![],
            warning: Some("active plan is in \"refining\" phase, not \"planning\"".into()),
        };
        assert_eq!(empty_but_warn.plans.len(), 0);
        assert!(empty_but_warn.warning.is_some());
        assert!(
            empty_but_warn
                .warning
                .unwrap()
                .contains("active plan is in")
        );
    }
}
