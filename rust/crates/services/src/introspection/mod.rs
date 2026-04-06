//! Introspection API — cloud-side data for `get_agent_info` tool.
//!
//! Design principle: all numeric reasoning happens in Rust functions.
//! Callers (LLM) receive conclusions, not raw data.

pub mod database;
pub mod scoring;

pub use database::DatabaseIntrospectionService;
pub use scoring::MemoryScoreBreakdown;

use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use astra_core::{ErrorResponse, internal_error};

// ── Response types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct MemoryIntrospectionResponse {
    pub episodic: EpisodicStats,
    pub semantic: SemanticStats,
    pub procedural: ProceduralStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EpisodicStats {
    pub turns: i64,
    pub total_events: i64,
    pub tool_intensity: String,
    pub session_depth: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SemanticStats {
    pub ctx_snapshots: i64,
    pub peak_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_managed_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_assembly_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_prompt_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_completion_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_total_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProceduralStats {
    pub skill_selections: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accuracy_rate: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub category: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillsIntrospectionResponse {
    pub installed: Vec<SkillInfo>,
    pub cloud: Vec<SkillInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryRecallItem {
    pub rank: usize,
    pub memory_id: String,
    pub final_score: f64,
    pub scores: MemoryScoreBreakdown,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

pub type ServiceResult<T> = Result<T, (StatusCode, Json<ErrorResponse>)>;

#[async_trait]
pub trait IntrospectionService: Send + Sync {
    async fn get_memory_introspection(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> ServiceResult<MemoryIntrospectionResponse>;

    async fn get_skills_introspection(
        &self,
        user_id: &str,
    ) -> ServiceResult<SkillsIntrospectionResponse>;

    async fn get_context_trend(
        &self,
        user_id: &str,
        session_id: &str,
        turns: i32,
        context_window: i64,
    ) -> ServiceResult<Value>;

    async fn get_context_snapshot(
        &self,
        user_id: &str,
        session_id: &str,
        turn_index: Option<i32>,
        detail: bool,
        raw: bool,
        raw_token_budget: i32,
    ) -> ServiceResult<Value>;

    async fn get_retrieval_quality(
        &self,
        user_id: &str,
        session_id: &str,
        turns: i32,
    ) -> ServiceResult<Value>;

    async fn get_memory_recall(
        &self,
        user_id: &str,
        session_id: &str,
        query_str: &str,
        task_hint: &str,
        limit: i32,
    ) -> ServiceResult<Value>;
}

// ── Unconfigured implementation ──────────────────────────────────────────────

pub struct UnconfiguredIntrospectionService;

#[async_trait]
impl IntrospectionService for UnconfiguredIntrospectionService {
    async fn get_memory_introspection(
        &self,
        _: &str,
        _: &str,
    ) -> ServiceResult<MemoryIntrospectionResponse> {
        Err(internal_error("introspection service not configured"))
    }
    async fn get_skills_introspection(
        &self,
        _: &str,
    ) -> ServiceResult<SkillsIntrospectionResponse> {
        Err(internal_error("introspection service not configured"))
    }
    async fn get_context_trend(&self, _: &str, _: &str, _: i32, _: i64) -> ServiceResult<Value> {
        Err(internal_error("introspection service not configured"))
    }
    async fn get_context_snapshot(
        &self,
        _: &str,
        _: &str,
        _: Option<i32>,
        _: bool,
        _: bool,
        _: i32,
    ) -> ServiceResult<Value> {
        Err(internal_error("introspection service not configured"))
    }
    async fn get_retrieval_quality(&self, _: &str, _: &str, _: i32) -> ServiceResult<Value> {
        Err(internal_error("introspection service not configured"))
    }
    async fn get_memory_recall(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: i32,
    ) -> ServiceResult<Value> {
        Err(internal_error("introspection service not configured"))
    }
}

// ── Query parameter types ────────────────────────────────────────────────────

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
    fn context_trend_query_defaults() {
        let json = r#"{"session_id": "s1"}"#;
        let q: ContextTrendQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.turns, 10);
        assert_eq!(q.context_window, 128000);
    }

    #[test]
    fn context_snapshot_query_defaults() {
        let json = r#"{"session_id": "s1"}"#;
        let q: ContextSnapshotQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.raw_token_budget, 2000);
        assert!(!q.detail);
        assert!(!q.raw);
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
        let json = r#"{"session_id": "s1", "query": "hello"}"#;
        let q: MemoryRecallQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.task_hint, "default");
        assert_eq!(q.limit, 10);
    }
}
