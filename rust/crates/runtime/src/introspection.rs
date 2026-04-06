//! Introspection API — cloud-side data for `get_agent_info` tool.
//!
//! Types, trait, scoring, and database implementation live in `astra_services::introspection`.
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

fn default_turns() -> i32 {
    10
}
fn default_context_window() -> i64 {
    128000
}
fn default_retrieval_turns() -> i32 {
    5
}
fn default_recall_limit() -> i32 {
    10
}
fn default_raw_token_budget() -> i32 {
    2000
}
fn default_task_hint() -> String {
    "default".into()
}

#[derive(Deserialize)]
pub struct MemoryIntrospectionQuery {
    pub session_id: String,
}

#[derive(Deserialize)]
pub struct ContextTrendQuery {
    pub session_id: String,
    #[serde(default = "default_turns")]
    pub turns: i32,
    #[serde(default = "default_context_window")]
    pub context_window: i64,
}

#[derive(Deserialize)]
pub struct ContextSnapshotQuery {
    pub session_id: String,
    pub turn_index: Option<i32>,
    #[serde(default)]
    pub detail: bool,
    #[serde(default)]
    pub raw: bool,
    #[serde(default = "default_raw_token_budget")]
    pub raw_token_budget: i32,
}

#[derive(Deserialize)]
pub struct RetrievalQualityQuery {
    pub session_id: String,
    #[serde(default = "default_retrieval_turns")]
    pub turns: i32,
}

#[derive(Deserialize)]
pub struct MemoryRecallQuery {
    pub session_id: String,
    pub query: String,
    #[serde(default = "default_task_hint")]
    pub task_hint: String,
    #[serde(default = "default_recall_limit")]
    pub limit: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_functions() {
        assert_eq!(default_turns(), 10);
        assert_eq!(default_context_window(), 128000);
        assert_eq!(default_retrieval_turns(), 5);
        assert_eq!(default_recall_limit(), 10);
        assert_eq!(default_raw_token_budget(), 2000);
        assert_eq!(default_task_hint(), "default");
    }

    #[test]
    fn context_trend_query_defaults() {
        let json = r#"{"session_id": "s1"}"#;
        let q: ContextTrendQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.session_id, "s1");
        assert_eq!(q.turns, 10);
        assert_eq!(q.context_window, 128000);
    }

    #[test]
    fn context_snapshot_query_defaults() {
        let json = r#"{"session_id": "s1"}"#;
        let q: ContextSnapshotQuery = serde_json::from_str(json).unwrap();
        assert!(!q.detail);
        assert!(!q.raw);
        assert_eq!(q.raw_token_budget, 2000);
        assert!(q.turn_index.is_none());
    }

    #[test]
    fn retrieval_quality_query_defaults() {
        let json = r#"{"session_id": "s1"}"#;
        let q: RetrievalQualityQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.turns, 5);
    }

    #[test]
    fn memory_recall_query_defaults() {
        let json = r#"{"session_id": "s1", "query": "test"}"#;
        let q: MemoryRecallQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.task_hint, "default");
        assert_eq!(q.limit, 10);
    }
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub async fn get_memory_introspection_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<MemoryIntrospectionQuery>,
) -> Result<Json<MemoryIntrospectionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let resp = state
        .introspection_service
        .get_memory_introspection(&user.user_id, &params.session_id)
        .await?;
    Ok(Json(resp))
}

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

pub async fn get_context_trend_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ContextTrendQuery>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let resp = state
        .introspection_service
        .get_context_trend(
            &user.user_id,
            &params.session_id,
            params.turns,
            params.context_window,
        )
        .await?;
    Ok(Json(resp))
}

pub async fn get_context_snapshot_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ContextSnapshotQuery>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let resp = state
        .introspection_service
        .get_context_snapshot(
            &user.user_id,
            &params.session_id,
            params.turn_index,
            params.detail,
            params.raw,
            params.raw_token_budget,
        )
        .await?;
    Ok(Json(resp))
}

pub async fn get_retrieval_quality_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<RetrievalQualityQuery>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let resp = state
        .introspection_service
        .get_retrieval_quality(&user.user_id, &params.session_id, params.turns)
        .await?;
    Ok(Json(resp))
}

pub async fn get_memory_recall_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<MemoryRecallQuery>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let resp = state
        .introspection_service
        .get_memory_recall(
            &user.user_id,
            &params.session_id,
            &params.query,
            &params.task_hint,
            params.limit,
        )
        .await?;
    Ok(Json(resp))
}
