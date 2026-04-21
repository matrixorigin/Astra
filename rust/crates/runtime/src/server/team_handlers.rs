//! HTTP handlers for team CRUD, execution history, and lifecycle.
//!
//! Routes:
//!   GET    /teams                       — list teams for the authenticated user
//!   POST   /teams                       — create or update a team definition
//!   GET    /teams/{name}                — get a team by name
//!   DELETE /teams/{name}                — delete a team
//!   GET    /teams/{name}/executions     — list execution history
//!   POST   /teams/{name}/execute        — run team task via [`TeamExecutionOrchestrator`]

use std::sync::Arc;

use astra_server_types::team_orchestrator_traits::{
    DelegationExecutor, DelegationTracking, RunPersistence,
};
use astra_server_types::team_orchestrator_types::{OrchestratorConfig, sum_usage};

use super::*;
use crate::server::team_orchestrator::{TeamExecutionOrchestrator, TeamExecutionReport};
use astra_services::team_persistence::{
    TeamDefinition, TeamExecutionRecord, TeamPersistenceService,
};

fn require_team_store(
    state: &AppState,
) -> Result<&Arc<dyn TeamPersistenceService>, (StatusCode, Json<ErrorResponse>)> {
    state.team_store.as_ref().ok_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "team service not configured",
        )
    })
}

fn require_delegation_engine(
    state: &AppState,
) -> Result<
    &Arc<crate::server::delegation_engine::DelegationEngine>,
    (StatusCode, Json<ErrorResponse>),
> {
    state.delegation_engine().ok_or_else(|| {
        astra_core::error_response_coded(
            StatusCode::SERVICE_UNAVAILABLE,
            "delegation engine not configured (multi-agent execution unavailable)",
            "delegation_not_configured",
        )
    })
}

// ─── List Teams ─────────────────────────────────────────────────────────────

/// GET /teams
pub(super) async fn list_teams_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<TeamListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_team_store(&state)?;

    let teams = store
        .list_teams(&user.user_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(TeamListResponse {
        teams: teams.into_iter().map(TeamSummary::from).collect(),
    }))
}

// ─── Get Team ───────────────────────────────────────────────────────────────

/// GET /teams/{name}
pub(super) async fn get_team_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<TeamDetailResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_team_store(&state)?;

    let team = store
        .load_team(&user.user_id, &name)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, format!("team '{name}' not found")))?;

    Ok(Json(TeamDetailResponse::from(team)))
}

// ─── Create / Update Team ───────────────────────────────────────────────────

/// POST /teams
pub(super) async fn upsert_team_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateTeamRequest>,
) -> Result<Json<TeamDetailResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_team_store(&state)?;

    let now = chrono::Utc::now().to_rfc3339();
    let existing = store
        .load_team(&user.user_id, &body.name)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let team_id = existing
        .as_ref()
        .map(|t| t.team_id.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let created_at = existing
        .as_ref()
        .map(|t| t.created_at.clone())
        .unwrap_or_else(|| now.clone());

    let def = TeamDefinition {
        team_id,
        user_id: user.user_id.clone(),
        name: body.name,
        description: body.description,
        coordination: body.coordination,
        members: body.members,
        context: body.context.unwrap_or_default(),
        worktree_mode: body.worktree_mode.unwrap_or_default(),
        budget: body.budget,
        max_parallel: body.max_parallel.unwrap_or(0),
        created_at,
        updated_at: now,
    };

    // Validate before saving
    astra_services::team_persistence::validate_team(&def).map_err(|errs| {
        let msg = errs
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        error_response(StatusCode::BAD_REQUEST, msg)
    })?;

    store
        .save_team(&def)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(TeamDetailResponse::from(def)))
}

// ─── Delete Team ────────────────────────────────────────────────────────────

/// DELETE /teams/{name}
pub(super) async fn delete_team_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<DeleteTeamResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_team_store(&state)?;

    let deleted = store
        .delete_team(&user.user_id, &name)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    if !deleted {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            format!("team '{name}' not found"),
        ));
    }

    Ok(Json(DeleteTeamResponse { deleted: true }))
}

// ─── Execution History ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(super) struct ExecutionHistoryQuery {
    #[serde(default = "default_limit")]
    limit: u32,
}
fn default_limit() -> u32 {
    50
}

/// GET /teams/{name}/executions
pub(super) async fn list_executions_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<ExecutionHistoryQuery>,
    headers: HeaderMap,
) -> Result<Json<ExecutionListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_team_store(&state)?;

    // Resolve name → team_id
    let team = store
        .load_team(&user.user_id, &name)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, format!("team '{name}' not found")))?;

    let limit = if query.limit == 0 {
        50
    } else {
        query.limit.clamp(1, 500)
    };

    let executions = store
        .list_executions(&team.team_id, limit)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(ExecutionListResponse {
        team_id: team.team_id,
        team_name: name,
        executions: executions.into_iter().map(ExecutionEntry::from).collect(),
    }))
}

/// POST /teams/{name}/execute
pub(super) async fn execute_team_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ExecuteTeamRequest>,
) -> Result<Json<TeamExecuteResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let team_store: Arc<dyn TeamPersistenceService> = require_team_store(&state)?.clone();
    let engine = require_delegation_engine(&state)?;

    let session_id = body
        .session_id
        .clone()
        .unwrap_or_else(|| "team-http-session".to_string());
    let source_agent_id = body
        .source_agent_id
        .clone()
        .unwrap_or_else(|| "orchestrator".to_string());
    let delegation_engine: Arc<dyn DelegationExecutor> = engine.clone();
    let delegation_tracker: Arc<dyn DelegationTracking> = engine.tracker().clone();
    let run_engine: Arc<dyn RunPersistence> = engine.run_engine().clone();
    let profile_registry = engine.registry().clone();

    let orch = TeamExecutionOrchestrator::new(
        team_store,
        delegation_engine,
        delegation_tracker,
        run_engine,
        profile_registry,
        OrchestratorConfig {
            user_id: user.user_id.clone(),
            session_id,
            source_agent_id,
            progress: None,
        },
    );

    let report = orch.execute_team(&name, &body.task, None).await;
    map_team_execution_report_to_http(report)
}

fn map_team_execution_report_to_http(
    report: TeamExecutionReport,
) -> Result<Json<TeamExecuteResponse>, (StatusCode, Json<ErrorResponse>)> {
    if let Some(ref err) = report.error {
        if err.contains("not found") && err.contains("team") {
            return Err(astra_core::error_response(
                StatusCode::NOT_FOUND,
                err.clone(),
            ));
        }
        if err.contains("team validation failed") {
            return Err(astra_core::error_response(
                StatusCode::BAD_REQUEST,
                err.clone(),
            ));
        }
        if err.contains("failed to load team") {
            return Err(astra_core::error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                err.clone(),
            ));
        }
    }

    Ok(Json(TeamExecuteResponse::from(report)))
}

// ─── Request / Response Types ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(super) struct ExecuteTeamRequest {
    pub task: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub source_agent_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct TeamExecuteResponse {
    pub team_name: String,
    pub status: String,
    pub delegation_id: String,
    pub parent_run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub agent_count: usize,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_tool_calls: u32,
}

impl From<TeamExecutionReport> for TeamExecuteResponse {
    fn from(r: TeamExecutionReport) -> Self {
        let (tp, tc, tt) = r
            .delegation_result
            .as_ref()
            .map(sum_usage)
            .unwrap_or((0, 0, 0));
        Self {
            team_name: r.team_name,
            status: r.status.to_string(),
            delegation_id: r.delegation_id,
            parent_run_id: r.parent_run_id,
            error: r.error,
            agent_count: r
                .delegation_result
                .as_ref()
                .map(|d| d.agent_results.len())
                .unwrap_or(0),
            total_prompt_tokens: tp,
            total_completion_tokens: tc,
            total_tool_calls: tt,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct CreateTeamRequest {
    pub name: String,
    pub description: String,
    pub coordination: astra_services::team_persistence::TeamCoordination,
    pub members: Vec<astra_services::team_persistence::TeamMemberDef>,
    #[serde(default)]
    pub context: Option<std::collections::HashMap<String, String>>,
    #[serde(default)]
    pub worktree_mode: Option<astra_services::team_persistence::WorktreeMode>,
    #[serde(default)]
    pub budget: Option<astra_services::team_persistence::TeamBudget>,
    #[serde(default)]
    pub max_parallel: Option<u32>,
}

#[derive(Debug, Serialize)]
pub(super) struct TeamListResponse {
    pub teams: Vec<TeamSummary>,
}

#[derive(Debug, Serialize)]
pub(super) struct TeamSummary {
    pub team_id: String,
    pub name: String,
    pub description: String,
    pub member_count: usize,
    pub coordination: astra_services::team_persistence::TeamCoordination,
    pub worktree_mode: astra_services::team_persistence::WorktreeMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<astra_services::team_persistence::TeamBudget>,
    pub max_parallel: u32,
}

impl From<TeamDefinition> for TeamSummary {
    fn from(t: TeamDefinition) -> Self {
        Self {
            team_id: t.team_id,
            name: t.name,
            description: t.description,
            member_count: t.members.len(),
            coordination: t.coordination,
            worktree_mode: t.worktree_mode,
            budget: t.budget,
            max_parallel: t.max_parallel,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct TeamDetailResponse {
    pub team_id: String,
    pub user_id: String,
    pub name: String,
    pub description: String,
    pub coordination: astra_services::team_persistence::TeamCoordination,
    pub members: Vec<astra_services::team_persistence::TeamMemberDef>,
    pub context: std::collections::HashMap<String, String>,
    pub worktree_mode: astra_services::team_persistence::WorktreeMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<astra_services::team_persistence::TeamBudget>,
    pub max_parallel: u32,
    pub created_at: String,
    pub updated_at: String,
}

impl From<TeamDefinition> for TeamDetailResponse {
    fn from(t: TeamDefinition) -> Self {
        Self {
            team_id: t.team_id,
            user_id: t.user_id,
            name: t.name,
            description: t.description,
            coordination: t.coordination,
            members: t.members,
            context: t.context,
            worktree_mode: t.worktree_mode,
            budget: t.budget,
            max_parallel: t.max_parallel,
            created_at: t.created_at,
            updated_at: t.updated_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct DeleteTeamResponse {
    pub deleted: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct ExecutionListResponse {
    pub team_id: String,
    pub team_name: String,
    pub executions: Vec<ExecutionEntry>,
}

#[derive(Debug, Serialize)]
pub(super) struct ExecutionEntry {
    pub execution_id: String,
    pub team_id: String,
    pub task: String,
    pub status: String,
    pub result_json: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
}

impl From<TeamExecutionRecord> for ExecutionEntry {
    fn from(r: TeamExecutionRecord) -> Self {
        Self {
            execution_id: r.execution_id,
            team_id: r.team_id,
            task: r.task,
            status: r.status,
            result_json: r.result_json,
            started_at: r.started_at,
            completed_at: r.completed_at,
        }
    }
}

// ─── Snapshots ──────────────────────────────────────────────────────────────

/// GET /teams/{name}/snapshots
pub(super) async fn list_snapshots_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SnapshotListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_team_store(&state)?;
    store
        .load_team(&user.user_id, &name)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, format!("team '{name}' not found")))?;
    let snaps = store
        .list_snapshots(&name, &user.user_id, 50)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(SnapshotListResponse {
        snapshots: snaps.into_iter().map(SnapshotEntry::from).collect(),
    }))
}

/// POST /teams/{name}/snapshots
pub(super) async fn create_snapshot_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateSnapshotRequest>,
) -> Result<Json<SnapshotEntry>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_team_store(&state)?;
    let team = store
        .load_team(&user.user_id, &name)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, format!("team '{name}' not found")))?;

    let snapshot_id = format!("snap-{}", Uuid::new_v4());
    let now = chrono::Utc::now().to_rfc3339();
    let team_json = serde_json::to_string(&team).ok();

    let record = astra_services::team_persistence::TeamSnapshotRecord {
        snapshot_id: snapshot_id.clone(),
        team_name: name,
        user_id: user.user_id,
        label: body.label.unwrap_or_default(),
        git_commit: body.git_commit,
        session_id: body.session_id,
        team_definition_json: team_json,
        created_at: now,
    };
    store
        .save_snapshot(&record)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(SnapshotEntry::from(record)))
}

/// DELETE /teams/snapshots/{id}
pub(super) async fn delete_snapshot_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<DeleteTeamResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_team_store(&state)?;
    let deleted = store
        .delete_snapshot(&id, &user.user_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if !deleted {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            format!("snapshot '{id}' not found"),
        ));
    }
    Ok(Json(DeleteTeamResponse { deleted: true }))
}

#[derive(Debug, Deserialize)]
pub(super) struct CreateSnapshotRequest {
    pub label: Option<String>,
    pub git_commit: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct SnapshotListResponse {
    pub snapshots: Vec<SnapshotEntry>,
}

#[derive(Debug, Serialize)]
pub(super) struct SnapshotEntry {
    pub snapshot_id: String,
    pub team_name: String,
    pub label: String,
    pub git_commit: Option<String>,
    pub session_id: Option<String>,
    pub team_definition_json: Option<String>,
    pub created_at: String,
}

impl From<astra_services::team_persistence::TeamSnapshotRecord> for SnapshotEntry {
    fn from(r: astra_services::team_persistence::TeamSnapshotRecord) -> Self {
        Self {
            snapshot_id: r.snapshot_id,
            team_name: r.team_name,
            label: r.label,
            git_commit: r.git_commit,
            session_id: r.session_id,
            team_definition_json: r.team_definition_json,
            created_at: r.created_at,
        }
    }
}
