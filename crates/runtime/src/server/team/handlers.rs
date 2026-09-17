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

use super::super::*;
use crate::server::team::orchestrator::{
    TeamExecutionErrorKind, TeamExecutionOrchestrator, TeamExecutionReport,
};
use astra_services::team_persistence::{
    TeamDefinition, TeamExecutionListCursor, TeamExecutionRecord, TeamPersistenceService,
    TeamSnapshotListCursor, team_execution_cursor_db_started_at,
    team_execution_cursor_execution_id, team_snapshot_cursor_db_created_at,
    team_snapshot_cursor_snapshot_id,
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

async fn require_owner_team_store<'a>(
    state: &'a AppState,
    user_id: &str,
) -> Result<&'a Arc<dyn TeamPersistenceService>, (StatusCode, Json<ErrorResponse>)> {
    let store = require_team_store(state)?;
    store.ensure_builtins(user_id).await.map_err(|error| {
        tracing::error!(
            user_id,
            error = %error,
            "failed to initialize owner-scoped built-in teams"
        );
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "team templates are temporarily unavailable",
        )
    })?;
    Ok(store)
}

fn require_delegation_engine(
    state: &AppState,
) -> Result<
    &Arc<crate::server::delegation::engine::DelegationEngine>,
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

fn validate_team_execution_session(
    session: &astra_services::SessionRecord,
    expected_user_id: &str,
    expected_session_id: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if session.session_id != expected_session_id
        || session.user_id != expected_user_id
        || session.status != "active"
    {
        return Err(error_response(
            StatusCode::CONFLICT,
            "team execution requires an active session owned by the authenticated user",
        ));
    }
    Ok(())
}

async fn load_team_by_name_or_id(
    store: &Arc<dyn TeamPersistenceService>,
    user_id: &str,
    name_or_id: &str,
) -> Result<Option<TeamDefinition>, (StatusCode, Json<ErrorResponse>)> {
    let by_name = store
        .load_team(user_id, name_or_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if by_name.is_some() {
        return Ok(by_name);
    }
    store
        .load_team_by_id(user_id, name_or_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))
}

// ─── List Teams ─────────────────────────────────────────────────────────────

/// GET /teams
pub(crate) async fn list_teams_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<TeamListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_owner_team_store(&state, &user.user_id).await?;

    let teams = store
        .list_teams(&user.user_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(TeamListResponse { teams }))
}

// ─── Get Team ───────────────────────────────────────────────────────────────

/// GET /teams/{name}
pub(crate) async fn get_team_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<TeamDefinition>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_owner_team_store(&state, &user.user_id).await?;

    let team = load_team_by_name_or_id(store, &user.user_id, &name)
        .await?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, format!("team '{name}' not found")))?;

    Ok(Json(team))
}

// ─── Create / Update Team ───────────────────────────────────────────────────

/// POST /teams
pub(crate) async fn upsert_team_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateTeamRequest>,
) -> Result<Json<TeamDefinition>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_owner_team_store(&state, &user.user_id).await?;

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

    Ok(Json(def))
}

// ─── Delete Team ────────────────────────────────────────────────────────────

/// DELETE /teams/{name}
pub(crate) async fn delete_team_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<DeleteTeamResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_owner_team_store(&state, &user.user_id).await?;

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
pub(crate) struct ExecutionHistoryQuery {
    #[serde(default = "default_limit")]
    limit: u32,
    pub after_started_at: Option<String>,
    pub after_execution_id: Option<String>,
}
fn default_limit() -> u32 {
    50
}

impl ExecutionHistoryQuery {
    fn cursor(&self) -> Result<Option<TeamExecutionListCursor>, (StatusCode, Json<ErrorResponse>)> {
        match (&self.after_started_at, &self.after_execution_id) {
            (None, None) => Ok(None),
            (Some(started_at), Some(execution_id)) => {
                let cursor = TeamExecutionListCursor {
                    started_at: started_at.clone(),
                    execution_id: execution_id.clone(),
                };
                team_execution_cursor_db_started_at(&cursor)
                    .map_err(|error| error_response(StatusCode::BAD_REQUEST, error))?;
                team_execution_cursor_execution_id(&cursor)
                    .map_err(|error| error_response(StatusCode::BAD_REQUEST, error))?;
                Ok(Some(cursor))
            }
            _ => Err(error_response(
                StatusCode::BAD_REQUEST,
                "team execution list cursor requires both after_started_at and after_execution_id",
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct SnapshotHistoryQuery {
    #[serde(default = "default_limit")]
    limit: u32,
    pub after_created_at: Option<String>,
    pub after_snapshot_id: Option<String>,
}

impl SnapshotHistoryQuery {
    fn cursor(&self) -> Result<Option<TeamSnapshotListCursor>, (StatusCode, Json<ErrorResponse>)> {
        match (&self.after_created_at, &self.after_snapshot_id) {
            (None, None) => Ok(None),
            (Some(created_at), Some(snapshot_id)) => {
                let cursor = TeamSnapshotListCursor {
                    created_at: created_at.clone(),
                    snapshot_id: snapshot_id.clone(),
                };
                team_snapshot_cursor_db_created_at(&cursor)
                    .map_err(|error| error_response(StatusCode::BAD_REQUEST, error))?;
                team_snapshot_cursor_snapshot_id(&cursor)
                    .map_err(|error| error_response(StatusCode::BAD_REQUEST, error))?;
                Ok(Some(cursor))
            }
            _ => Err(error_response(
                StatusCode::BAD_REQUEST,
                "team snapshot list cursor requires both after_created_at and after_snapshot_id",
            )),
        }
    }
}

/// GET /teams/{name}/executions
pub(crate) async fn list_executions_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<ExecutionHistoryQuery>,
    headers: HeaderMap,
) -> Result<Json<ExecutionListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_owner_team_store(&state, &user.user_id).await?;

    let team = load_team_by_name_or_id(store, &user.user_id, &name)
        .await?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, format!("team '{name}' not found")))?;

    let limit = if query.limit == 0 {
        50
    } else {
        query.limit.clamp(1, 500)
    };

    let page = store
        .list_executions_page(&team.team_id, limit, query.cursor()?)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(ExecutionListResponse {
        team_id: team.team_id,
        team_name: team.name,
        executions: page
            .executions
            .into_iter()
            .map(ExecutionEntry::from)
            .collect(),
        limit: page.limit,
        next_cursor: page.next_cursor,
    }))
}

/// POST /teams/{name}/execute
pub(crate) async fn execute_team_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ExecuteTeamRequest>,
) -> Result<Json<TeamExecuteResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    if body.session_id.is_empty() || body.session_id.trim() != body.session_id {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "team execution requires an exact non-empty session_id",
        ));
    }
    let session = state
        .session_service
        .get_session(body.session_id.clone(), user.user_id.clone())
        .await?;
    validate_team_execution_session(&session, &user.user_id, &body.session_id)?;
    let team_store: Arc<dyn TeamPersistenceService> =
        require_owner_team_store(&state, &user.user_id)
            .await?
            .clone();
    let engine = require_delegation_engine(&state)?;

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
            session_id: body.session_id,
            // HTTP callers never choose execution identity. The selected
            // server profile is part of server composition.
            source_agent_id: "orchestrator".to_string(),
            progress: None,
        },
    );

    let report = orch.execute_team(&name, &body.task, None).await;
    map_team_execution_report_to_http(report)
}

fn map_team_execution_report_to_http(
    report: TeamExecutionReport,
) -> Result<Json<TeamExecuteResponse>, (StatusCode, Json<ErrorResponse>)> {
    let error_status = match report.error_kind {
        Some(TeamExecutionErrorKind::TeamNotFound) => Some(StatusCode::NOT_FOUND),
        Some(TeamExecutionErrorKind::InvalidTeam) => Some(StatusCode::BAD_REQUEST),
        Some(TeamExecutionErrorKind::Persistence) => Some(StatusCode::INTERNAL_SERVER_ERROR),
        Some(TeamExecutionErrorKind::Execution) | None => None,
    };
    if let Some(status) = error_status {
        return Err(astra_core::error_response_coded(
            status,
            report
                .error
                .clone()
                .unwrap_or_else(|| "team execution failed without error detail".to_string()),
            report
                .error_kind
                .map(TeamExecutionErrorKind::as_str)
                .unwrap_or("team_execution_failed"),
        ));
    }

    Ok(Json(TeamExecuteResponse::from(report)))
}

// ─── Request / Response Types ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecuteTeamRequest {
    pub task: String,
    pub session_id: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct TeamExecuteResponse {
    pub team_name: String,
    pub status: String,
    pub delegation_id: String,
    pub parent_run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
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
            error_code: r.error_kind.map(|kind| kind.as_str().to_string()),
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
pub(crate) struct CreateTeamRequest {
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
pub(crate) struct TeamListResponse {
    pub teams: Vec<TeamDefinition>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DeleteTeamResponse {
    pub deleted: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ExecutionListResponse {
    pub team_id: String,
    pub team_name: String,
    pub executions: Vec<ExecutionEntry>,
    pub limit: u32,
    pub next_cursor: Option<TeamExecutionListCursor>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ExecutionEntry {
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
pub(crate) async fn list_snapshots_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<SnapshotHistoryQuery>,
    headers: HeaderMap,
) -> Result<Json<SnapshotListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_owner_team_store(&state, &user.user_id).await?;
    store
        .load_team(&user.user_id, &name)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, format!("team '{name}' not found")))?;
    let limit = if query.limit == 0 {
        default_limit()
    } else {
        query.limit
    };
    let page = store
        .list_snapshots_page(&name, &user.user_id, limit, query.cursor()?)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(SnapshotListResponse {
        snapshots: page
            .snapshots
            .into_iter()
            .map(SnapshotEntry::from)
            .collect(),
        limit: page.limit,
        next_cursor: page.next_cursor,
    }))
}

/// POST /teams/{name}/snapshots
pub(crate) async fn create_snapshot_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateSnapshotRequest>,
) -> Result<Json<SnapshotEntry>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_owner_team_store(&state, &user.user_id).await?;
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
pub(crate) async fn delete_snapshot_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<DeleteTeamResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_owner_team_store(&state, &user.user_id).await?;
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
pub(crate) struct CreateSnapshotRequest {
    pub label: Option<String>,
    pub git_commit: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SnapshotListResponse {
    pub snapshots: Vec<SnapshotEntry>,
    pub limit: u32,
    pub next_cursor: Option<TeamSnapshotListCursor>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SnapshotEntry {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn failed_report(kind: TeamExecutionErrorKind, message: &str) -> TeamExecutionReport {
        TeamExecutionReport {
            team_name: "reviewers".to_string(),
            delegation_id: String::new(),
            parent_run_id: String::new(),
            delegation_result: None,
            merge_result: None,
            status: astra_server_types::team_orchestrator_types::TeamExecutionStatus::Failed,
            error_kind: Some(kind),
            error: Some(message.to_string()),
        }
    }

    #[test]
    fn team_execution_http_mapping_uses_typed_failure_kind() {
        let not_found = map_team_execution_report_to_http(failed_report(
            TeamExecutionErrorKind::TeamNotFound,
            "arbitrary detail",
        ))
        .unwrap_err();
        assert_eq!(not_found.0, StatusCode::NOT_FOUND);
        assert_eq!(not_found.1.0.error_code.as_deref(), Some("team_not_found"));

        let persistence = map_team_execution_report_to_http(failed_report(
            TeamExecutionErrorKind::Persistence,
            "database unavailable",
        ))
        .unwrap_err();
        assert_eq!(persistence.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            persistence.1.0.error_code.as_deref(),
            Some("team_persistence_error")
        );
    }

    #[test]
    fn execution_error_text_cannot_impersonate_http_protocol() {
        let Json(response) = map_team_execution_report_to_http(failed_report(
            TeamExecutionErrorKind::Execution,
            "team not found while rendering an agent quote",
        ))
        .unwrap();
        assert_eq!(
            response.error_code.as_deref(),
            Some("team_execution_failed")
        );
    }

    #[test]
    fn team_execution_query_cursor_requires_complete_seek_key() {
        let q = serde_json::from_value::<ExecutionHistoryQuery>(serde_json::json!({
            "limit": 10,
            "after_started_at": "2026-10-01T12:34:56.123456",
            "after_execution_id": "exec-5"
        }))
        .unwrap();
        let cursor = q.cursor().unwrap().unwrap();
        assert_eq!(cursor.started_at, "2026-10-01T12:34:56.123456");
        assert_eq!(cursor.execution_id, "exec-5");

        let missing_id = serde_json::from_value::<ExecutionHistoryQuery>(serde_json::json!({
            "after_started_at": "2026-10-01T12:34:56.123456"
        }))
        .unwrap();
        assert_eq!(missing_id.cursor().unwrap_err().0, StatusCode::BAD_REQUEST);

        let invalid_time = serde_json::from_value::<ExecutionHistoryQuery>(serde_json::json!({
            "after_started_at": "not-a-date",
            "after_execution_id": "exec-5"
        }))
        .unwrap();
        assert_eq!(
            invalid_time.cursor().unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn team_snapshot_query_cursor_requires_complete_seek_key() {
        let q = serde_json::from_value::<SnapshotHistoryQuery>(serde_json::json!({
            "limit": 10,
            "after_created_at": "2026-10-01T12:34:56.123456",
            "after_snapshot_id": "snap-5"
        }))
        .unwrap();
        let cursor = q.cursor().unwrap().unwrap();
        assert_eq!(cursor.created_at, "2026-10-01T12:34:56.123456");
        assert_eq!(cursor.snapshot_id, "snap-5");

        let missing_id = serde_json::from_value::<SnapshotHistoryQuery>(serde_json::json!({
            "after_created_at": "2026-10-01T12:34:56.123456"
        }))
        .unwrap();
        assert_eq!(missing_id.cursor().unwrap_err().0, StatusCode::BAD_REQUEST);

        let invalid_time = serde_json::from_value::<SnapshotHistoryQuery>(serde_json::json!({
            "after_created_at": "not-a-date",
            "after_snapshot_id": "snap-5"
        }))
        .unwrap();
        assert_eq!(
            invalid_time.cursor().unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    fn session_record(
        user_id: &str,
        session_id: &str,
        status: &str,
    ) -> astra_services::SessionRecord {
        astra_services::SessionRecord {
            session_id: session_id.to_string(),
            user_id: user_id.to_string(),
            agent_id: None,
            title: None,
            metadata: serde_json::Map::new(),
            status: status.to_string(),
            event_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: None,
            ended_at: None,
        }
    }

    #[test]
    fn team_execution_session_must_match_exact_owner_identity_and_be_active() {
        let owned = session_record("user-a", "session-a", "active");
        validate_team_execution_session(&owned, "user-a", "session-a").unwrap();
        assert_eq!(
            validate_team_execution_session(&owned, "user-b", "session-a")
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            validate_team_execution_session(&owned, "user-a", "session-b")
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        let deleting = session_record("user-a", "session-a", "deleting");
        assert_eq!(
            validate_team_execution_session(&deleting, "user-a", "session-a")
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
    }

    #[test]
    fn team_execute_request_requires_session_and_rejects_source_identity() {
        assert!(
            serde_json::from_value::<ExecuteTeamRequest>(serde_json::json!({"task": "do it"}))
                .is_err()
        );
        assert!(
            serde_json::from_value::<ExecuteTeamRequest>(serde_json::json!({
                "task": "do it",
                "session_id": "session-a",
                "source_agent_id": "attacker"
            }))
            .is_err()
        );
        let request = serde_json::from_value::<ExecuteTeamRequest>(serde_json::json!({
            "task": "do it",
            "session_id": "session-a"
        }))
        .unwrap();
        assert_eq!(request.session_id, "session-a");
    }
}
