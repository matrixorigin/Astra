//! HTTP handlers for team definitions and snapshots.
//!
//! Routes:
//!   GET    /teams                       — list teams for the authenticated user
//!   POST   /teams                       — create or update a team definition
//!   GET    /teams/{name}                — get a team by name
//!   DELETE /teams/{name}                — delete a team
//!   GET    /teams/snapshots/{id}        — get an owner-scoped snapshot

use std::sync::Arc;

use super::super::*;
use astra_services::team_persistence::{
    TeamDefinition, TeamPersistenceService, TeamSnapshotListCursor,
    team_snapshot_cursor_db_created_at, team_snapshot_cursor_snapshot_id,
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
        members: body.members,
        context: body.context.unwrap_or_default(),
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

    let def = store
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

// ─── Snapshot Pagination ────────────────────────────────────────────────────

fn default_limit() -> u32 {
    50
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

// ─── Request / Response Types ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateTeamRequest {
    pub name: String,
    pub description: String,
    pub members: Vec<astra_services::team_persistence::TeamMemberDef>,
    #[serde(default)]
    pub context: Option<std::collections::HashMap<String, String>>,
}

#[derive(Debug, Serialize)]
pub(crate) struct TeamListResponse {
    pub teams: Vec<TeamDefinition>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DeleteTeamResponse {
    pub deleted: bool,
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
    let record = store
        .save_snapshot(&record)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(SnapshotEntry::from(record)))
}

/// GET /teams/snapshots/{id}
pub(crate) async fn get_snapshot_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SnapshotEntry>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let store = require_owner_team_store(&state, &user.user_id).await?;
    let snapshot = store
        .find_snapshot(&id, &user.user_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| {
            error_response(StatusCode::NOT_FOUND, format!("snapshot '{id}' not found"))
        })?;
    Ok(Json(SnapshotEntry::from(snapshot)))
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
}
