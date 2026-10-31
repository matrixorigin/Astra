//! Introspection API — cloud-side data for `get_agent_info` tool.
//!
//! Types, trait, and database implementation live in `astra_services::introspection`.
//! This module re-exports them and adds HTTP handler functions.

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use serde_json::Value;

use crate::{AppState, ErrorResponse};

// Re-export everything from the services crate — single source of truth.
pub use astra_services::introspection::*;

// ── Query parameter types (runtime-only) ─────────────────────────────────────

fn default_tool_history_window_hours() -> i32 {
    24
}

#[derive(Deserialize)]
pub struct ToolHistoryQuery {
    pub tool: String,
    #[serde(default = "default_tool_history_window_hours")]
    pub window_hours: i32,
}

#[derive(Deserialize)]
pub struct DriftCheckQuery {
    pub session_id: String,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub async fn get_skills_introspection_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SkillsIntrospectionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let resp = state
        .introspection_service
        .get_skills_introspection(&user.user_id)
        .await?;
    Ok(Json(resp))
}

pub async fn get_tool_history_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ToolHistoryQuery>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let resp = state
        .introspection_service
        .get_tool_history(&user.user_id, &params.tool, params.window_hours)
        .await?;
    Ok(Json(resp))
}

pub async fn get_drift_check_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<DriftCheckQuery>,
) -> Result<Json<IntentDriftCheckResponseV2>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let resp = state
        .introspection_service
        .get_drift_check(&user.user_id, &params.session_id)
        .await?;
    Ok(Json(resp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_history_query_defaults() {
        let q: ToolHistoryQuery = serde_json::from_str(r#"{"tool":"grep"}"#).unwrap();
        assert_eq!(q.tool, "grep");
        assert_eq!(q.window_hours, 24);
    }

    #[test]
    fn drift_check_query_requires_session_id() {
        let bad = serde_json::from_str::<DriftCheckQuery>("{}");
        assert!(bad.is_err());
        let ok: DriftCheckQuery = serde_json::from_str(r#"{"session_id":"s1"}"#).unwrap();
        assert_eq!(ok.session_id, "s1");
    }
}
