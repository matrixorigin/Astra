//! REST API handlers for plan lifecycle management.
//!
//! - `POST /plans` — create a plan (server-side decomposition)
//! - `GET /plans` — list plans
//! - `GET /plans/{plan_id}` — get plan details
//! - `PUT /plans/{plan_id}` — edit a plan
//! - `POST /plans/{plan_id}/execute` — start plan execution
//! - `GET /plans/{plan_id}/status` — get plan status + metrics
//! - `DELETE /plans/{plan_id}` — delete a plan

use super::*;
use crate::plan::{
    ApprovalPolicy, PlanCapabilities, PlanLoadError, PlanModeState,
    ProjectContext, metrics::PlanMetrics,
};
use astra_services::task_orchestrator::{TaskPlan, TaskStatus};

const MAX_GOAL_LENGTH: usize = 10_000;
const MAX_INSTRUCTION_LENGTH: usize = 10_000;

// ─── Request / Response types ────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct CreatePlanRequest {
    pub goal: String,
    #[serde(default)]
    pub context: Option<ProjectContext>,
}

#[derive(Deserialize)]
pub(super) struct EditPlanRequest {
    pub instruction: String,
    /// Expected version for optimistic concurrency. If provided and doesn't match
    /// the current version on disk, the update is rejected with 409 Conflict.
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

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn load_plan_or_error(
    plan_id: &str,
) -> Result<PlanModeState, (StatusCode, Json<ErrorResponse>)> {
    PlanModeState::load_from_plans_dir(plan_id).map_err(|e| {
        let status = match &e {
            PlanLoadError::InvalidId(_) => StatusCode::BAD_REQUEST,
            PlanLoadError::NotFound(_) => StatusCode::NOT_FOUND,
            PlanLoadError::Corrupt(_) => StatusCode::UNPROCESSABLE_ENTITY,
            PlanLoadError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        error_response(status, e.to_string())
    })
}

/// Load a plan and verify the caller owns it. Returns 404 for non-owned plans
/// (don't leak existence via 403).
fn load_plan_owned(
    plan_id: &str,
    user_id: &str,
) -> Result<PlanModeState, (StatusCode, Json<ErrorResponse>)> {
    let state = load_plan_or_error(plan_id)?;
    if let Some(owner) = &state.created_by {
        if owner != user_id {
            return Err(error_response(StatusCode::NOT_FOUND, "plan not found"));
        }
    }
    Ok(state)
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
/// Returns a string that matches `PlanPhase` variant naming conventions.
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
        PlanModeState::new_with_owner(goal.clone(), context.clone(), user.user_id);

    let plan_id = plan_state.save_to_plans_dir().map_err(|e| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to save plan: {e}"),
        )
    })?;

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
) -> Result<Json<PlanListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    let saved = PlanModeState::list_saved_plans_for_user(&user.user_id);
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

    let plan_state = load_plan_owned(&plan_id, &user.user_id)?;
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

    let mut plan_state = load_plan_owned(&plan_id, &user.user_id)?;

    check_version(&plan_state, req.expected_version)?;

    if infer_phase_name(&plan_state) == "completed" {
        return Err(error_response(
            StatusCode::CONFLICT,
            "Cannot edit a completed plan",
        ));
    }

    plan_state.add_turn(&instruction, "(pending LLM response)");

    plan_state.save_to_plans_dir_with_id(&plan_id).map_err(|e| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to save plan: {e}"),
        )
    })?;

    let capabilities = PlanCapabilities::planning();

    Ok(Json(PlanResponse {
        plan_id,
        phase: "refining".to_string(),
        goal: plan_state.goal,
        version: plan_state.version,
        plan: Some(plan_state.plan),
        capabilities,
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

    let mut plan_state = load_plan_owned(&plan_id, &user.user_id)?;

    check_version(&plan_state, req.expected_version)?;

    if plan_state.plan.subtasks.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Cannot execute an empty plan",
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

    for subtask in &mut plan_state.plan.subtasks {
        if subtask.status == TaskStatus::Pending {
            subtask.status = TaskStatus::InProgress;
            break;
        }
    }

    plan_state.save_to_plans_dir_with_id(&plan_id).map_err(|e| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to save plan: {e}"),
        )
    })?;

    let completed = plan_state
        .plan
        .subtasks
        .iter()
        .filter(|s| s.status == TaskStatus::Completed)
        .count();
    let failed = plan_state
        .plan
        .subtasks
        .iter()
        .filter(|s| s.status == TaskStatus::Failed)
        .count();

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

/// `GET /plans/{plan_id}/status` — get plan execution status.
pub(super) async fn plan_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
) -> Result<Json<PlanStatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    let plan_state = load_plan_owned(&plan_id, &user.user_id)?;
    let phase_name = infer_phase_name(&plan_state);
    let capabilities = capabilities_for_phase_name(phase_name);

    let completed = plan_state
        .plan
        .subtasks
        .iter()
        .filter(|s| s.status == TaskStatus::Completed)
        .count();
    let failed = plan_state
        .plan
        .subtasks
        .iter()
        .filter(|s| s.status == TaskStatus::Failed)
        .count();

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

    // Verify ownership before deleting
    let _ = load_plan_owned(&plan_id, &user.user_id)?;

    PlanModeState::delete_saved_plan(&plan_id).map_err(|e| {
        let status = match &e {
            PlanLoadError::InvalidId(_) => StatusCode::BAD_REQUEST,
            PlanLoadError::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        error_response(status, e.to_string())
    })?;

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
        state.plan.subtasks.push(astra_services::task_orchestrator::SubtaskPlan {
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
        state.plan.subtasks.push(astra_services::task_orchestrator::SubtaskPlan {
            id: "s1".into(),
            title: "Step 1".into(),
            status: TaskStatus::Completed,
            ..Default::default()
        });
        state.plan.subtasks.push(astra_services::task_orchestrator::SubtaskPlan {
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
        state.plan.subtasks.push(astra_services::task_orchestrator::SubtaskPlan {
            id: "s1".into(),
            title: "Step 1".into(),
            status: TaskStatus::Completed,
            ..Default::default()
        });
        assert_eq!(infer_phase_name(&state), "completed");
    }

    // ── load_plan_or_error typed error tests ─────────────────────────────

    #[test]
    fn load_plan_or_error_rejects_path_traversal_with_400() {
        let (status, _) = load_plan_or_error("../etc/passwd").unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn load_plan_or_error_returns_404_for_missing() {
        let (status, _) = load_plan_or_error("nonexistent-plan-id-xyz").unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
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

    // ── input validation tests ───────────────────────────────────────────

    #[test]
    fn max_goal_and_instruction_length_constants() {
        const { assert!(MAX_GOAL_LENGTH >= 1_000) };
        const { assert!(MAX_INSTRUCTION_LENGTH >= 1_000) };
    }

    // ── save_to_plans_dir increments version ─────────────────────────────

    #[test]
    fn save_increments_version() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("v-test.json");

        let mut state = PlanModeState::new("version test".into(), ProjectContext::default());
        let v0 = state.version;
        state.save_to_file(&path).unwrap();

        state.save_to_file(&path).unwrap();
        assert_eq!(state.version, v0, "save_to_file should NOT bump version");

        // save_to_plans_dir_with_id bumps version
        let v_before = state.version;
        state.save_to_plans_dir_with_id("version-test").unwrap();
        assert_eq!(state.version, v_before + 1);
    }

    // ── execute_plan_handler subtask mutation test ────────────────────────

    #[test]
    fn execute_plan_marks_first_pending_as_in_progress() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("exec-test.json");

        let mut state = PlanModeState::new("exec test".into(), ProjectContext::default());
        state.plan.subtasks.push(astra_services::task_orchestrator::SubtaskPlan {
            id: "s1".into(),
            title: "First".into(),
            status: TaskStatus::Pending,
            ..Default::default()
        });
        state.plan.subtasks.push(astra_services::task_orchestrator::SubtaskPlan {
            id: "s2".into(),
            title: "Second".into(),
            status: TaskStatus::Pending,
            ..Default::default()
        });

        for subtask in &mut state.plan.subtasks {
            if subtask.status == TaskStatus::Pending {
                subtask.status = TaskStatus::InProgress;
                break;
            }
        }
        state.save_to_file(&path).unwrap();

        let loaded = PlanModeState::load_from_file(&path).unwrap();
        assert_eq!(loaded.plan.subtasks[0].status, TaskStatus::InProgress);
        assert_eq!(loaded.plan.subtasks[1].status, TaskStatus::Pending);
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
