//! Team management HTTP handlers — stub.

use axum::extract::{Path, State};
use axum::Json;

use super::AppState;

pub(super) async fn list_teams_handler(State(_state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "teams": [] }))
}

pub(super) async fn upsert_team_handler(
    State(_state): State<AppState>,
    Json(_body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "error": "not implemented" }))
}

pub(super) async fn get_team_handler(
    State(_state): State<AppState>,
    Path(_name): Path<String>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "error": "not implemented" }))
}

pub(super) async fn delete_team_handler(
    State(_state): State<AppState>,
    Path(_name): Path<String>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "deleted": false }))
}

pub(super) async fn list_executions_handler(
    State(_state): State<AppState>,
    Path(_name): Path<String>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "executions": [] }))
}
