//! Session Audit Query Layer — cloud-side structured queries over `agent_events`.
//!
//! Provides turn-level, tool-level, and session-level audit views.
//! All queries run against MatrixOne `agent_events` + `agent_sessions` tables.

use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};

use crate::{MutationScoreboard, PersistedMutationDecision, StagedMutationState};
use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

fn normalize_tool_name(name: String) -> String {
    let trimmed = name.trim_matches('"').trim();
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `SUBSTRING(..., 1, N)` caps for `agent_events.content` to avoid full LONGTEXT reads.
/// JSON columns (`metadata`, `token_usage`) are left intact so parsing stays valid.
mod agent_events_content_cap {
    pub const TURN_LIST_PREVIEW: u32 = 200;
    pub const TURN_DETAIL_CHILD: u32 = 65_536;
    pub const TOOL_LAST_ERROR: u32 = 2048;
    pub const ERROR_LIST_ENTRY: u32 = 8192;
}

// ── Response types ───────────────────────────────────────────────────────────

/// High-level session audit summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAuditSummary {
    pub session_id: String,
    pub status: String,
    pub turn_count: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tool_calls_total: u32,
    pub tool_calls_failed: u32,
    pub error_count: u32,
    pub stall_count: u32,
    pub checkpoint_count: u32,
    pub compact_count: u32,
    pub models_used: Vec<String>,
    pub duration_secs: f64,
    pub created_at: String,
    pub ended_at: Option<String>,
}

/// Brief tool-call info within a turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallBrief {
    pub name: String,
    pub ok: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One turn in the session timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnSummary {
    pub turn: u32,
    pub user_input_preview: String,
    pub tool_calls: Vec<ToolCallBrief>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub duration_ms: u64,
    pub has_error: bool,
    pub has_stall: bool,
    pub model: Option<String>,
    pub created_at: String,
}

/// Paginated turn list.
#[derive(Debug, Clone, Serialize)]
pub struct TurnListResponse {
    pub turns: Vec<TurnSummary>,
    pub total: u32,
    pub page: u32,
    pub per_page: u32,
}

/// Full detail for a single turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnDetail {
    pub turn: u32,
    pub user_input: String,
    pub assistant_output: String,
    pub tool_calls: Vec<ToolCallBrief>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub duration_ms: u64,
    pub ttft_ms: Option<u64>,
    pub context_ms: Option<u64>,
    pub selector_ms: Option<u64>,
    pub selector_strategy: Option<String>,
    pub budget_pressure: Option<f64>,
    pub tools_selected: Vec<String>,
    pub tools_used: Vec<String>,
    pub model: Option<String>,
    pub has_error: bool,
    pub error_message: Option<String>,
    pub stall_type: Option<String>,
    pub plan_subtask_id: Option<String>,
    pub created_at: String,
    /// Child events (tool_call, tool_error) from event expansion.
    pub child_events: Vec<ChildEvent>,
}

/// A child event (tool call or error) linked to a turn via parent_event_id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildEvent {
    pub event_id: String,
    pub event_type: String,
    pub content: String,
    pub metadata: serde_json::Value,
    pub created_at: String,
}

/// Per-tool analytics aggregated across a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolAnalytics {
    pub name: String,
    pub call_count: u32,
    pub success_count: u32,
    pub fail_count: u32,
    pub success_rate: f64,
    pub avg_duration_ms: f64,
    pub max_duration_ms: u64,
    pub total_duration_ms: u64,
    pub last_error: Option<String>,
}

/// An error/anomaly event in the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditErrorEntry {
    pub event_id: String,
    pub event_type: String,
    pub turn: Option<u32>,
    pub content: String,
    pub metadata: serde_json::Value,
    pub created_at: String,
}

/// Paginated error list.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorListResponse {
    pub errors: Vec<AuditErrorEntry>,
    pub total: u32,
}

// ── Cross-session types ──────────────────────────────────────────────────────

/// A session list item with key metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditSessionListItem {
    pub session_id: String,
    pub status: String,
    pub turn_count: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tool_calls_total: u32,
    pub error_count: u32,
    pub model: Option<String>,
    pub duration_secs: f64,
    pub created_at: String,
    pub ended_at: Option<String>,
}

/// Paginated session list response.
#[derive(Debug, Clone, Serialize)]
pub struct AuditSessionListResponse {
    pub sessions: Vec<AuditSessionListItem>,
    pub total: u32,
    pub page: u32,
    pub per_page: u32,
}

/// Aggregate statistics across multiple sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossSessionStats {
    pub session_count: u32,
    pub total_turns: u32,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub total_tool_calls: u32,
    pub total_tool_failures: u32,
    pub total_errors: u32,
    pub total_stalls: u32,
    pub avg_turns_per_session: f64,
    pub avg_tokens_per_session: f64,
    pub tool_error_rate: f64,
    pub total_mutations: u32,
    pub ready_mutations: u32,
    pub approval_required_mutations: u32,
    pub applied_mutations: u32,
    pub reverted_mutations: u32,
    pub blocked_mutations: u32,
    pub top_tools: Vec<ToolUsageBrief>,
    pub top_models: Vec<ModelUsageBrief>,
}

/// Brief tool usage info for cross-session stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUsageBrief {
    pub name: String,
    pub call_count: u32,
    pub success_rate: f64,
}

/// Brief model usage info for cross-session stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUsageBrief {
    pub model: String,
    pub session_count: u32,
    pub total_tokens: u64,
}

/// Cross-session tool analytics (global view).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossSessionToolAnalytics {
    pub name: String,
    pub total_calls: u32,
    pub total_success: u32,
    pub total_failures: u32,
    pub success_rate: f64,
    pub avg_duration_ms: f64,
    pub max_duration_ms: u64,
    pub sessions_used_in: u32,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossSessionMutationListResponse {
    pub mutations: Vec<crate::StagedMutation>,
    pub total: u32,
    pub page: u32,
    pub per_page: u32,
}

const MAX_AUDIT_SESSIONS_PER_PAGE: u32 = 100;
const MAX_CROSS_SESSION_TOOLS: i64 = 100;
const MAX_CROSS_SESSION_MUTATIONS_PER_PAGE: u32 = 100;

// ── Request params ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct TurnListParams {
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_per_page")]
    pub per_page: u32,
}

/// Query parameters for session list endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct AuditSessionListParams {
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_per_page")]
    pub per_page: u32,
    /// Filter: "active", "ended", or omit for all.
    pub status: Option<String>,
    /// Filter: only sessions using this model.
    pub model: Option<String>,
    /// Filter: sessions created after this ISO 8601 timestamp.
    pub since: Option<String>,
    /// Filter: sessions created before this ISO 8601 timestamp.
    pub until: Option<String>,
    /// Filter: sessions with at least this many turns.
    pub min_turns: Option<u32>,
    /// Sort field: "created" (default), "turns", "tokens", "duration".
    #[serde(default = "default_sort")]
    pub sort: String,
    /// Sort direction: "desc" (default) or "asc".
    #[serde(default = "default_order")]
    pub order: String,
}

/// Query parameters for cross-session stats endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct CrossSessionStatsParams {
    /// Stats since this ISO 8601 timestamp.
    pub since: Option<String>,
    /// Stats until this ISO 8601 timestamp.
    pub until: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CrossSessionMutationListParams {
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_per_page")]
    pub per_page: u32,
    pub since: Option<String>,
    pub until: Option<String>,
    pub state: Option<StagedMutationState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationStateUpdateRequest {
    pub state: StagedMutationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone)]
struct MutationStateOverride {
    mutation_id: String,
    state: StagedMutationState,
    note: Option<String>,
    created_at: String,
}

fn default_page() -> u32 {
    1
}
fn default_per_page() -> u32 {
    20
}
fn default_sort() -> String {
    "created".into()
}
fn default_order() -> String {
    "desc".into()
}

// ── Trait ─────────────────────────────────────────────────────────────────────

pub type AuditResult<T> = Result<T, (StatusCode, Json<ErrorResponse>)>;

#[async_trait]
pub trait SessionAuditService: Send + Sync {
    /// Get high-level session audit summary.
    async fn get_summary(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> AuditResult<SessionAuditSummary>;

    /// List turns in paginated timeline order.
    async fn list_turns(
        &self,
        user_id: &str,
        session_id: &str,
        params: &TurnListParams,
    ) -> AuditResult<TurnListResponse>;

    /// Get full detail for a single turn.
    async fn get_turn_detail(
        &self,
        user_id: &str,
        session_id: &str,
        turn: u32,
    ) -> AuditResult<TurnDetail>;

    /// Get per-tool analytics for a session.
    async fn get_tool_analytics(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> AuditResult<Vec<ToolAnalytics>>;

    /// List error/anomaly events in a session.
    async fn list_errors(&self, user_id: &str, session_id: &str) -> AuditResult<ErrorListResponse>;

    /// Get the per-session mutation scoreboard reconstructed from decision audits.
    async fn get_mutation_scoreboard(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> AuditResult<MutationScoreboard>;

    // ── Cross-session methods ────────────────────────────────────────────────

    /// List user's sessions with filtering and pagination.
    async fn list_sessions(
        &self,
        user_id: &str,
        params: &AuditSessionListParams,
    ) -> AuditResult<AuditSessionListResponse>;

    /// Get aggregate statistics across the user's sessions.
    async fn get_cross_session_stats(
        &self,
        user_id: &str,
        params: &CrossSessionStatsParams,
    ) -> AuditResult<CrossSessionStats>;

    /// Get tool analytics aggregated across all of the user's sessions.
    async fn get_cross_session_tools(
        &self,
        user_id: &str,
        params: &CrossSessionStatsParams,
    ) -> AuditResult<Vec<CrossSessionToolAnalytics>>;

    /// List staged mutations across the user's sessions.
    async fn list_cross_session_mutations(
        &self,
        user_id: &str,
        params: &CrossSessionMutationListParams,
    ) -> AuditResult<CrossSessionMutationListResponse>;
}

// ── Database implementation ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DatabaseSessionAuditService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseSessionAuditService {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }

    async fn verify_session_owner(
        &self,
        pool: &sqlx::Pool<sqlx::MySql>,
        session_id: &str,
        user_id: &str,
    ) -> AuditResult<()> {
        let row = query("SELECT user_id FROM agent_sessions WHERE session_id = ?")
            .bind(session_id)
            .fetch_optional(pool)
            .await
            .map_err(internal_error)?;

        match row {
            Some(r) => {
                let owner: String = r.try_get("user_id").map_err(internal_error)?;
                if owner != user_id {
                    Err(error_response(StatusCode::NOT_FOUND, "Session not found"))
                } else {
                    Ok(())
                }
            }
            None => Err(error_response(StatusCode::NOT_FOUND, "Session not found")),
        }
    }

    async fn load_cross_session_mutation_inputs(
        &self,
        pool: &sqlx::Pool<sqlx::MySql>,
        user_id: &str,
        since: Option<&str>,
        until: Option<&str>,
    ) -> AuditResult<(
        Vec<PersistedMutationDecision>,
        Vec<(String, MutationStateOverride)>,
    )> {
        let mut mutation_where_parts: Vec<String> = vec!["s.user_id = ?".into()];
        let mut mutation_bind_values: Vec<String> = vec![user_id.into()];
        if let Some(since) = since {
            mutation_where_parts.push("d.created_at >= ?".into());
            mutation_bind_values.push(since.to_string());
        }
        if let Some(until) = until {
            mutation_where_parts.push("d.created_at <= ?".into());
            mutation_bind_values.push(until.to_string());
        }
        let mutation_where_clause = mutation_where_parts.join(" AND ");
        let decisions_sql = format!(
            "SELECT d.decision_id, d.session_id, CAST(d.decision_output AS CHAR) AS decision_output \
             FROM ctx_decision_audits d \
             JOIN agent_sessions s ON s.session_id = d.session_id \
             WHERE {mutation_where_clause} AND d.decision_type = 'tool_selection' \
             ORDER BY d.created_at ASC"
        );
        let mut dq = sqlx::query(&decisions_sql);
        for v in &mutation_bind_values {
            dq = dq.bind(v);
        }
        let decision_rows = dq.fetch_all(pool).await.map_err(internal_error)?;
        let mutation_decisions = decision_rows
            .into_iter()
            .map(|row| {
                let decision_output = row
                    .try_get::<String, _>("decision_output")
                    .ok()
                    .and_then(|value| serde_json::from_str(&value).ok())
                    .unwrap_or(serde_json::Value::Null);
                PersistedMutationDecision {
                    decision_id: row.try_get("decision_id").unwrap_or_default(),
                    session_id: row.try_get("session_id").unwrap_or_default(),
                    decision_output,
                }
            })
            .collect::<Vec<_>>();

        let mut override_where_parts: Vec<String> = vec!["e.user_id = ?".into()];
        let mut override_bind_values: Vec<String> = vec![user_id.into()];
        if let Some(since) = since {
            override_where_parts.push("e.created_at >= ?".into());
            override_bind_values.push(since.to_string());
        }
        if let Some(until) = until {
            override_where_parts.push("e.created_at <= ?".into());
            override_bind_values.push(until.to_string());
        }
        let override_where_clause = override_where_parts.join(" AND ");
        let overrides_sql = format!(
            "SELECT e.session_id, CAST(e.metadata AS CHAR) AS metadata, e.created_at \
             FROM agent_events e \
             WHERE {override_where_clause} AND e.event_type = 'mutation_state' \
             ORDER BY e.created_at ASC"
        );
        let mut oq = sqlx::query(&overrides_sql);
        for v in &override_bind_values {
            oq = oq.bind(v);
        }
        let override_rows = oq.fetch_all(pool).await.map_err(internal_error)?;
        let mutation_overrides = override_rows
            .into_iter()
            .filter_map(|row| {
                let metadata = row
                    .try_get::<String, _>("metadata")
                    .ok()
                    .and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok())
                    .unwrap_or(serde_json::Value::Null);
                parse_mutation_state_override(
                    &metadata,
                    row.try_get("created_at").unwrap_or_default(),
                )
                .map(|override_entry| {
                    (
                        row.try_get("session_id").unwrap_or_default(),
                        override_entry,
                    )
                })
            })
            .collect::<Vec<_>>();

        Ok((mutation_decisions, mutation_overrides))
    }
}

#[async_trait]
impl SessionAuditService for DatabaseSessionAuditService {
    async fn get_summary(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> AuditResult<SessionAuditSummary> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        // Single round-trip: session row + owner check (replaces verify_session_owner + session SELECT).
        let sess_row = query(
            "SELECT user_id, status, created_at, ended_at FROM agent_sessions WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let sess_row =
            sess_row.ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Session not found"))?;
        let owner: String = sess_row.try_get("user_id").map_err(internal_error)?;
        if owner != user_id {
            return Err(error_response(StatusCode::NOT_FOUND, "Session not found"));
        }

        let status: String = sess_row.try_get("status").unwrap_or_default();
        let created_at: String = sess_row
            .try_get::<String, _>("created_at")
            .unwrap_or_default();
        let ended_at: Option<String> = sess_row.try_get("ended_at").ok();

        // One pass over agent_events: counts, tokens, duration bounds, distinct models.
        // MatrixOne rejects `SEPARATOR CHAR(31)`; embed the unit-separator as a literal (same as MySQL).
        const MODEL_SEP: char = '\u{001f}';
        let metrics_row = query(&format!(
            "SELECT \
               COUNT(CASE WHEN event_type = 'turn' THEN 1 END) AS turn_count, \
               COUNT(CASE WHEN event_type = 'turn_error' THEN 1 END) AS error_count, \
               COUNT(CASE WHEN event_type = 'stall_detected' THEN 1 END) AS stall_count, \
               COUNT(CASE WHEN event_type = 'checkpoint' THEN 1 END) AS checkpoint_count, \
               COUNT(CASE WHEN event_type = 'compact' THEN 1 END) AS compact_count, \
               COUNT(CASE WHEN event_type = 'tool_call' THEN 1 END) \
                 + COUNT(CASE WHEN event_type = 'tool_error' THEN 1 END) AS tool_calls_total, \
               COUNT(CASE WHEN event_type = 'tool_error' THEN 1 END) AS tool_calls_failed, \
               COALESCE(SUM(CASE WHEN event_type = 'turn' AND token_usage IS NOT NULL \
                 THEN COALESCE(token_input, 0) ELSE 0 END), 0) AS tokens_in, \
               COALESCE(SUM(CASE WHEN event_type = 'turn' AND token_usage IS NOT NULL \
                 THEN COALESCE(token_output, 0) ELSE 0 END), 0) AS tokens_out, \
               MIN(created_at) AS first_at, \
               MAX(created_at) AS last_at, \
               (SELECT GROUP_CONCAT(m ORDER BY m SEPARATOR '{sep}') \
                  FROM (SELECT DISTINCT llm_model_used AS m FROM agent_events e3 \
                        WHERE e3.session_id = ? AND e3.user_id = ? \
                          AND e3.llm_model_used IS NOT NULL) t) AS models_concat \
             FROM agent_events e \
             WHERE e.session_id = ? AND e.user_id = ?",
            sep = MODEL_SEP,
        ))
        .bind(session_id)
        .bind(user_id)
        .bind(session_id)
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;

        let turn_count: u32 = metrics_row.try_get::<i64, _>("turn_count").unwrap_or(0) as u32;
        let error_count: u32 = metrics_row.try_get::<i64, _>("error_count").unwrap_or(0) as u32;
        let stall_count: u32 = metrics_row.try_get::<i64, _>("stall_count").unwrap_or(0) as u32;
        let checkpoint_count: u32 = metrics_row
            .try_get::<i64, _>("checkpoint_count")
            .unwrap_or(0) as u32;
        let compact_count: u32 = metrics_row.try_get::<i64, _>("compact_count").unwrap_or(0) as u32;
        let tool_calls_total: u32 = metrics_row
            .try_get::<i64, _>("tool_calls_total")
            .unwrap_or(0) as u32;
        let tool_calls_failed: u32 = metrics_row
            .try_get::<i64, _>("tool_calls_failed")
            .unwrap_or(0) as u32;

        let tokens_in: i64 = metrics_row.try_get("tokens_in").unwrap_or(0);
        let tokens_out: i64 = metrics_row.try_get("tokens_out").unwrap_or(0);

        let first_at: Option<String> = metrics_row.try_get("first_at").ok();
        let last_at: Option<String> = metrics_row.try_get("last_at").ok();
        let duration_secs = compute_duration_secs(first_at.as_deref(), last_at.as_deref());

        let models_used: Vec<String> = metrics_row
            .try_get::<String, _>("models_concat")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.split(MODEL_SEP)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        Ok(SessionAuditSummary {
            session_id: session_id.to_string(),
            status,
            turn_count,
            tokens_in: tokens_in as u64,
            tokens_out: tokens_out as u64,
            tool_calls_total,
            tool_calls_failed,
            error_count,
            stall_count,
            checkpoint_count,
            compact_count,
            models_used,
            duration_secs,
            created_at,
            ended_at,
        })
    }

    async fn list_turns(
        &self,
        user_id: &str,
        session_id: &str,
        params: &TurnListParams,
    ) -> AuditResult<TurnListResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;

        let page = params.page.max(1);
        let per_page = params.per_page.clamp(1, 100);
        let offset = (page - 1) * per_page;

        // Count total turn events
        let count_row = query(
            "SELECT COUNT(*) AS cnt FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND event_type = 'turn'",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;
        let total: i64 = count_row.try_get("cnt").unwrap_or(0);

        // Fetch turn events with pagination (cap content in SQL — matches preview length)
        let turn_sql = format!(
            "SELECT event_id, \
             SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, {}) AS content, \
             token_usage, llm_model_used, metadata, created_at \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND event_type = 'turn' \
             ORDER BY created_at ASC \
             LIMIT ? OFFSET ?",
            agent_events_content_cap::TURN_LIST_PREVIEW
        );
        let rows = query(&turn_sql)
            .bind(session_id)
            .bind(user_id)
            .bind(per_page)
            .bind(offset)
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let turns: Vec<TurnSummary> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let content: String = row.try_get("content").unwrap_or_default();
                let meta: String = row.try_get("metadata").unwrap_or_default();
                let meta_json: serde_json::Value =
                    serde_json::from_str(&meta).unwrap_or(serde_json::Value::Null);
                let token_str: String = row.try_get("token_usage").unwrap_or_default();
                let token_json: serde_json::Value =
                    serde_json::from_str(&token_str).unwrap_or(serde_json::Value::Null);
                let model: Option<String> = row.try_get("llm_model_used").ok();
                let created_at: String = row.try_get("created_at").unwrap_or_default();

                let turn_num = meta_json
                    .get("turn")
                    .and_then(|v| v.as_u64())
                    .unwrap_or((offset + i as u32 + 1) as u64)
                    as u32;

                let tool_calls = extract_tool_calls_from_metadata(&meta_json);
                let has_error = meta_json
                    .get("error")
                    .map(|v| !v.is_null())
                    .unwrap_or(false);
                let has_stall = meta_json
                    .get("stall_type")
                    .map(|v| !v.is_null())
                    .unwrap_or(false);

                TurnSummary {
                    turn: turn_num,
                    user_input_preview: truncate_str(&content, 200),
                    tool_calls,
                    tokens_in: token_json
                        .get("input")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    tokens_out: token_json
                        .get("output")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    duration_ms: meta_json
                        .get("duration_ms")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    has_error,
                    has_stall,
                    model,
                    created_at,
                }
            })
            .collect();

        Ok(TurnListResponse {
            turns,
            total: total as u32,
            page,
            per_page,
        })
    }

    async fn get_turn_detail(
        &self,
        user_id: &str,
        session_id: &str,
        turn: u32,
    ) -> AuditResult<TurnDetail> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;

        // Find the turn event by position (LIMIT/OFFSET) — turns are ordered by created_at.
        // This avoids fetching the full event content for every turn in the session.
        let offset = turn.saturating_sub(1);
        let row = query(
            "SELECT event_id, content, token_usage, llm_model_used, metadata, created_at \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND event_type = 'turn' \
             ORDER BY created_at ASC \
             LIMIT 1 OFFSET ?",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(offset)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let row = row.ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Turn not found"))?;

        let event_id: String = row.try_get("event_id").unwrap_or_default();
        let content: String = row.try_get("content").unwrap_or_default();
        let meta_str: String = row.try_get("metadata").unwrap_or_default();
        let meta: serde_json::Value =
            serde_json::from_str(&meta_str).unwrap_or(serde_json::Value::Null);
        let token_str: String = row.try_get("token_usage").unwrap_or_default();
        let token_json: serde_json::Value =
            serde_json::from_str(&token_str).unwrap_or(serde_json::Value::Null);
        let model: Option<String> = row.try_get("llm_model_used").ok();
        let created_at: String = row.try_get("created_at").unwrap_or_default();

        let tool_calls = extract_tool_calls_from_metadata(&meta);

        let tools_selected: Vec<String> = meta
            .get("tools_selected")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let tools_used: Vec<String> = meta
            .get("tools_used")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        // Child events may carry huge tool I/O; cap content at the SQL layer.
        let child_sql = format!(
            "SELECT event_id, event_type, \
             SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, {}) AS content, \
             metadata, created_at \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND parent_event_id = ? \
             ORDER BY created_at ASC",
            agent_events_content_cap::TURN_DETAIL_CHILD
        );
        let child_rows = query(&child_sql)
            .bind(session_id)
            .bind(user_id)
            .bind(&event_id)
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let child_events: Vec<ChildEvent> = child_rows
            .iter()
            .map(|r| {
                let meta_raw: String = r.try_get("metadata").unwrap_or_default();
                ChildEvent {
                    event_id: r.try_get("event_id").unwrap_or_default(),
                    event_type: r.try_get("event_type").unwrap_or_default(),
                    content: r.try_get("content").unwrap_or_default(),
                    metadata: serde_json::from_str(&meta_raw).unwrap_or(serde_json::Value::Null),
                    created_at: r.try_get("created_at").unwrap_or_default(),
                }
            })
            .collect();

        let assistant_output = meta
            .get("assistant_output")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        Ok(TurnDetail {
            turn,
            user_input: content,
            assistant_output,
            tool_calls,
            tokens_in: token_json
                .get("input")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            tokens_out: token_json
                .get("output")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            duration_ms: meta
                .get("duration_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            ttft_ms: meta.get("ttft_ms").and_then(|v| v.as_u64()),
            context_ms: meta.get("context_ms").and_then(|v| v.as_u64()),
            selector_ms: meta.get("selector_ms").and_then(|v| v.as_u64()),
            selector_strategy: meta
                .get("selector_strategy")
                .and_then(|v| v.as_str())
                .map(String::from),
            budget_pressure: meta.get("budget_pressure").and_then(|v| v.as_f64()),
            tools_selected,
            tools_used,
            model,
            has_error: meta.get("error").map(|v| !v.is_null()).unwrap_or(false),
            error_message: meta.get("error").and_then(|v| v.as_str()).map(String::from),
            stall_type: meta
                .get("stall_type")
                .and_then(|v| v.as_str())
                .map(String::from),
            plan_subtask_id: meta
                .get("plan_subtask_id")
                .and_then(|v| v.as_str())
                .map(String::from),
            created_at,
            child_events,
        })
    }

    async fn get_tool_analytics(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> AuditResult<Vec<ToolAnalytics>> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;

        let rows = query(
            "SELECT \
               agg.tool_name, agg.total_calls, agg.total_success, agg.total_failures, \
               agg.avg_ms, agg.max_ms, agg.total_duration_ms \
              FROM (\
                SELECT \
                  meta_tool_name AS tool_name, \
                 COUNT(*) AS total_calls, \
                 COUNT(CASE WHEN event_type = 'tool_call' THEN 1 END) AS total_success, \
                 COUNT(CASE WHEN event_type = 'tool_error' THEN 1 END) AS total_failures, \
                 COALESCE(AVG(meta_duration_ms), 0) AS avg_ms, \
                 COALESCE(MAX(meta_duration_ms), 0) AS max_ms, \
                 COALESCE(SUM(meta_duration_ms), 0) AS total_duration_ms \
                FROM agent_events \
                WHERE session_id = ? AND user_id = ? \
                  AND event_type IN ('tool_call', 'tool_error') \
                GROUP BY tool_name \
              ) agg \
              ORDER BY agg.total_duration_ms DESC",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let err_sql = format!(
            "SELECT meta_tool_name AS tool_name, \
             SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, {}) AS content \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND event_type = 'tool_error' \
             ORDER BY created_at DESC LIMIT 200",
            agent_events_content_cap::TOOL_LAST_ERROR
        );
        let error_rows = query(&err_sql)
            .bind(session_id)
            .bind(user_id)
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;
        let mut latest_errors = std::collections::HashMap::<String, String>::new();
        for row in error_rows {
            let tool_name = normalize_tool_name(row.try_get("tool_name").unwrap_or_default());
            if latest_errors.contains_key(&tool_name) {
                continue;
            }
            let content: String = row.try_get("content").unwrap_or_default();
            if !content.is_empty() {
                latest_errors.insert(tool_name, content);
            }
        }

        let result: Vec<ToolAnalytics> = rows
            .iter()
            .filter_map(|row| {
                let name = normalize_tool_name(row.try_get("tool_name").unwrap_or_default());
                let total_calls = row.try_get::<i64, _>("total_calls").unwrap_or(0) as u32;
                if total_calls == 0 {
                    return None;
                }
                let total_success = row.try_get::<i64, _>("total_success").unwrap_or(0) as u32;
                let total_failures = row.try_get::<i64, _>("total_failures").unwrap_or(0) as u32;
                let total_duration_ms =
                    row.try_get::<i64, _>("total_duration_ms").unwrap_or(0) as u64;
                let last_error = latest_errors.get(&name).cloned();

                Some(ToolAnalytics {
                    name,
                    call_count: total_calls,
                    success_count: total_success,
                    fail_count: total_failures,
                    success_rate: total_success as f64 / total_calls as f64,
                    avg_duration_ms: row
                        .try_get::<f64, _>("avg_ms")
                        .or_else(|_| row.try_get::<i64, _>("avg_ms").map(|v| v as f64))
                        .unwrap_or(0.0),
                    max_duration_ms: row
                        .try_get::<i64, _>("max_ms")
                        .or_else(|_| row.try_get::<f64, _>("max_ms").map(|v| v as i64))
                        .unwrap_or(0) as u64,
                    total_duration_ms,
                    last_error,
                })
            })
            .collect();

        Ok(result)
    }

    async fn list_errors(&self, user_id: &str, session_id: &str) -> AuditResult<ErrorListResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;

        let list_err_sql = format!(
            "SELECT event_id, event_type, \
             SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, {}) AS content, \
             metadata, created_at \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? \
               AND event_type IN ('turn_error', 'stall_detected', 'error', 'turn_guard_verdict', 'tool_error') \
             ORDER BY created_at ASC \
             LIMIT 200",
            agent_events_content_cap::ERROR_LIST_ENTRY
        );
        let rows = query(&list_err_sql)
            .bind(session_id)
            .bind(user_id)
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let errors: Vec<AuditErrorEntry> = rows
            .iter()
            .map(|row| {
                let meta_str: String = row.try_get("metadata").unwrap_or_default();
                let meta: serde_json::Value =
                    serde_json::from_str(&meta_str).unwrap_or(serde_json::Value::Null);
                let turn = meta.get("turn").and_then(|v| v.as_u64()).map(|v| v as u32);

                AuditErrorEntry {
                    event_id: row.try_get("event_id").unwrap_or_default(),
                    event_type: row.try_get("event_type").unwrap_or_default(),
                    turn,
                    content: row.try_get("content").unwrap_or_default(),
                    metadata: meta,
                    created_at: row.try_get("created_at").unwrap_or_default(),
                }
            })
            .collect();

        let total = errors.len() as u32;
        Ok(ErrorListResponse { errors, total })
    }

    async fn get_mutation_scoreboard(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> AuditResult<MutationScoreboard> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;

        let rows = query(
            "SELECT decision_id, session_id, CAST(decision_output AS CHAR) AS decision_output \
             FROM ctx_decision_audits \
             WHERE session_id = ? AND decision_type = 'tool_selection' \
             ORDER BY created_at ASC",
        )
        .bind(session_id)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let decisions = rows
            .into_iter()
            .map(|row| {
                let decision_output = row
                    .try_get::<String, _>("decision_output")
                    .ok()
                    .and_then(|value| serde_json::from_str(&value).ok())
                    .unwrap_or(serde_json::Value::Null);
                PersistedMutationDecision {
                    decision_id: row.try_get("decision_id").unwrap_or_default(),
                    session_id: row.try_get("session_id").unwrap_or_default(),
                    decision_output,
                }
            })
            .collect::<Vec<_>>();

        let override_rows = query(
            "SELECT CAST(metadata AS CHAR) AS metadata, created_at \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND event_type = 'mutation_state' \
             ORDER BY created_at ASC",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let overrides = override_rows
            .into_iter()
            .filter_map(|row| {
                let metadata = row
                    .try_get::<String, _>("metadata")
                    .ok()
                    .and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok())
                    .unwrap_or(serde_json::Value::Null);
                parse_mutation_state_override(
                    &metadata,
                    row.try_get("created_at").unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>();

        Ok(build_mutation_scoreboard(session_id, decisions, overrides))
    }

    // ── Cross-session implementations ────────────────────────────────────────

    async fn list_sessions(
        &self,
        user_id: &str,
        params: &AuditSessionListParams,
    ) -> AuditResult<AuditSessionListResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let per_page = params.per_page.clamp(1, MAX_AUDIT_SESSIONS_PER_PAGE);
        let page = params.page.max(1);

        // Build WHERE with all filters pushed into SQL (including min_turns and model)
        // via a subquery that pre-aggregates event stats per session.
        let mut having_parts: Vec<String> = Vec::new();
        let mut where_parts: Vec<String> = vec!["s.user_id = ?".into()];
        let mut bind_values: Vec<String> = vec![user_id.into()];
        // Extra binds for the HAVING clause (appended after main binds)
        let mut having_bind_values: Vec<String> = Vec::new();

        if let Some(ref status) = params.status {
            where_parts.push("s.status = ?".into());
            bind_values.push(status.clone());
        }
        if let Some(ref since) = params.since {
            where_parts.push("s.created_at >= ?".into());
            bind_values.push(since.clone());
        }
        if let Some(ref until) = params.until {
            where_parts.push("s.created_at <= ?".into());
            bind_values.push(until.clone());
        }
        if let Some(min) = params.min_turns {
            having_parts.push("turn_count >= ?".into());
            having_bind_values.push(min.to_string());
        }
        if let Some(ref model) = params.model {
            // "session ever used this model" — not MAX which picks lexicographic max
            having_parts.push("SUM(CASE WHEN e.llm_model_used = ? THEN 1 ELSE 0 END) > 0".into());
            having_bind_values.push(model.clone());
        }

        let where_clause = where_parts.join(" AND ");
        let having_clause = if having_parts.is_empty() {
            String::new()
        } else {
            format!("HAVING {}", having_parts.join(" AND "))
        };

        // Sort column (references aliases from the SELECT)
        let sort_col = match params.sort.as_str() {
            "turns" => "turn_count",
            "tokens" => "tokens_in + tokens_out",
            "duration" => "duration_secs",
            _ => "s.created_at",
        };
        let order_dir = if params.order == "asc" { "ASC" } else { "DESC" };
        let offset = (page.saturating_sub(1)) * per_page;

        // Single query: JOIN + GROUP BY to get stats, model, and counts in one pass.
        // The CTE computes per-session aggregates; outer query handles pagination.
        let data_sql = format!(
            "SELECT \
               s.session_id, s.status, s.created_at, s.ended_at, \
               COUNT(CASE WHEN e.event_type = 'turn' THEN 1 END) AS turn_count, \
               COALESCE(SUM(e.token_input), 0) AS tokens_in, \
               COALESCE(SUM(e.token_output), 0) AS tokens_out, \
               COUNT(CASE WHEN e.event_type IN ('tool_call', 'tool_error') THEN 1 END) AS tool_calls, \
               COUNT(CASE WHEN e.event_type IN ('turn_error', 'error', 'tool_error') THEN 1 END) AS error_count, \
               MIN(e.created_at) AS first_ts, \
               MAX(e.created_at) AS last_ts, \
               MAX(CASE WHEN e.llm_model_used IS NOT NULL THEN e.llm_model_used END) AS model, \
               TIMESTAMPDIFF(SECOND, s.created_at, COALESCE(s.ended_at, NOW())) AS duration_secs \
             FROM agent_sessions s \
             LEFT JOIN agent_events e ON e.session_id = s.session_id AND e.user_id = s.user_id \
             WHERE {where_clause} \
             GROUP BY s.session_id, s.status, s.created_at, s.ended_at \
             {having_clause} \
             ORDER BY {sort_col} {order_dir} \
             LIMIT ? OFFSET ?"
        );

        let mut q = sqlx::query(&data_sql);
        for v in &bind_values {
            q = q.bind(v);
        }
        for v in &having_bind_values {
            q = q.bind(v);
        }
        q = q.bind(per_page as i64).bind(offset as i64);
        let rows = q.fetch_all(&pool).await.map_err(internal_error)?;

        let sessions: Vec<AuditSessionListItem> = rows
            .iter()
            .map(|row| {
                let first_ts: Option<String> = row.try_get("first_ts").ok();
                let last_ts: Option<String> = row.try_get("last_ts").ok();
                let duration = compute_duration_secs(first_ts.as_deref(), last_ts.as_deref());
                AuditSessionListItem {
                    session_id: row.try_get("session_id").unwrap_or_default(),
                    status: row.try_get("status").unwrap_or_default(),
                    turn_count: row.try_get::<i64, _>("turn_count").unwrap_or(0) as u32,
                    tokens_in: row.try_get::<i64, _>("tokens_in").unwrap_or(0) as u64,
                    tokens_out: row.try_get::<i64, _>("tokens_out").unwrap_or(0) as u64,
                    tool_calls_total: row.try_get::<i64, _>("tool_calls").unwrap_or(0) as u32,
                    error_count: row.try_get::<i64, _>("error_count").unwrap_or(0) as u32,
                    model: row.try_get::<String, _>("model").ok(),
                    duration_secs: duration,
                    created_at: row.try_get("created_at").unwrap_or_default(),
                    ended_at: row.try_get("ended_at").ok(),
                }
            })
            .collect();

        // Count total matching (same WHERE + HAVING, no LIMIT)
        let count_sql = format!(
            "SELECT COUNT(*) AS cnt FROM (\
               SELECT s.session_id, \
                 COUNT(CASE WHEN e.event_type = 'turn' THEN 1 END) AS turn_count \
               FROM agent_sessions s \
               LEFT JOIN agent_events e ON e.session_id = s.session_id AND e.user_id = s.user_id \
               WHERE {where_clause} \
               GROUP BY s.session_id \
               {having_clause}\
             ) sub"
        );
        let mut cq = sqlx::query(&count_sql);
        for v in &bind_values {
            cq = cq.bind(v);
        }
        for v in &having_bind_values {
            cq = cq.bind(v);
        }
        let total = cq
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?
            .try_get::<i64, _>("cnt")
            .unwrap_or(0) as u32;

        Ok(AuditSessionListResponse {
            sessions,
            total,
            page,
            per_page,
        })
    }

    async fn get_cross_session_stats(
        &self,
        user_id: &str,
        params: &CrossSessionStatsParams,
    ) -> AuditResult<CrossSessionStats> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        // Build time-range filter — all aggregates use agent_events.created_at
        // so numerator and denominator share the same time window.
        let mut where_parts: Vec<String> = vec!["e.user_id = ?".into()];
        let mut bind_values: Vec<String> = vec![user_id.into()];
        if let Some(ref since) = params.since {
            where_parts.push("e.created_at >= ?".into());
            bind_values.push(since.clone());
        }
        if let Some(ref until) = params.until {
            where_parts.push("e.created_at <= ?".into());
            bind_values.push(until.clone());
        }
        let where_clause = where_parts.join(" AND ");

        // Aggregate event stats — session_count derived from same event rows
        let agg_sql = format!(
            "SELECT \
               COUNT(DISTINCT e.session_id) as session_count, \
               COUNT(CASE WHEN event_type = 'turn' THEN 1 END) as total_turns, \
               COALESCE(SUM(token_input), 0) as tokens_in, \
               COALESCE(SUM(token_output), 0) as tokens_out, \
               COUNT(CASE WHEN event_type IN ('tool_call', 'tool_error') THEN 1 END) as total_tool_calls, \
               COUNT(CASE WHEN event_type = 'tool_error' THEN 1 END) as total_tool_failures, \
               COUNT(CASE WHEN event_type IN ('turn_error', 'error') THEN 1 END) as total_errors, \
               COUNT(CASE WHEN event_type = 'stall_detected' THEN 1 END) as total_stalls \
             FROM agent_events e \
             WHERE {where_clause}"
        );
        let mut aq = sqlx::query(&agg_sql);
        for v in &bind_values {
            aq = aq.bind(v);
        }
        let agg = aq.fetch_one(&pool).await.map_err(internal_error)?;

        let session_count = agg.try_get::<i64, _>("session_count").unwrap_or(0) as u32;

        let total_turns = agg.try_get::<i64, _>("total_turns").unwrap_or(0) as u32;
        let tokens_in = agg.try_get::<i64, _>("tokens_in").unwrap_or(0) as u64;
        let tokens_out = agg.try_get::<i64, _>("tokens_out").unwrap_or(0) as u64;
        let total_tool_calls = agg.try_get::<i64, _>("total_tool_calls").unwrap_or(0) as u32;
        let total_tool_failures = agg.try_get::<i64, _>("total_tool_failures").unwrap_or(0) as u32;
        let total_errors = agg.try_get::<i64, _>("total_errors").unwrap_or(0) as u32;
        let total_stalls = agg.try_get::<i64, _>("total_stalls").unwrap_or(0) as u32;

        // Top tools (by usage count)
        let tools_sql = format!(
            "SELECT \
               meta_tool_name as tool_name, \
               COUNT(*) as cnt, \
               COUNT(CASE WHEN event_type = 'tool_call' THEN 1 END) as ok_cnt \
             FROM agent_events e \
             WHERE {where_clause} AND event_type IN ('tool_call', 'tool_error') \
             GROUP BY tool_name \
             ORDER BY cnt DESC \
             LIMIT 10"
        );
        let mut tq = sqlx::query(&tools_sql);
        for v in &bind_values {
            tq = tq.bind(v);
        }
        let tool_rows = tq.fetch_all(&pool).await.map_err(internal_error)?;
        let top_tools: Vec<ToolUsageBrief> = tool_rows
            .iter()
            .map(|r| {
                let name = normalize_tool_name(r.try_get("tool_name").unwrap_or_default());
                let cnt = r.try_get::<i64, _>("cnt").unwrap_or(0) as u32;
                let ok = r.try_get::<i64, _>("ok_cnt").unwrap_or(0) as u32;
                ToolUsageBrief {
                    name,
                    call_count: cnt,
                    success_rate: if cnt > 0 { ok as f64 / cnt as f64 } else { 0.0 },
                }
            })
            .collect();

        // Top models (by session count + tokens)
        let models_sql = format!(
            "SELECT \
               llm_model_used as model, \
               COUNT(DISTINCT session_id) as sess_cnt, \
               COALESCE(SUM(token_total), 0) as total_tokens \
             FROM agent_events e \
             WHERE {where_clause} AND llm_model_used IS NOT NULL \
             GROUP BY model \
             ORDER BY sess_cnt DESC \
             LIMIT 5"
        );
        let mut mq = sqlx::query(&models_sql);
        for v in &bind_values {
            mq = mq.bind(v);
        }
        let model_rows = mq.fetch_all(&pool).await.map_err(internal_error)?;
        let top_models: Vec<ModelUsageBrief> = model_rows
            .iter()
            .map(|r| ModelUsageBrief {
                model: r.try_get("model").unwrap_or_default(),
                session_count: r.try_get::<i64, _>("sess_cnt").unwrap_or(0) as u32,
                total_tokens: r.try_get::<i64, _>("total_tokens").unwrap_or(0) as u64,
            })
            .collect();

        let (mutation_decisions, mutation_overrides) = self
            .load_cross_session_mutation_inputs(
                &pool,
                user_id,
                params.since.as_deref(),
                params.until.as_deref(),
            )
            .await?;
        let mutation_stats =
            aggregate_cross_session_mutation_stats(mutation_decisions, mutation_overrides);

        let sc = session_count.max(1) as f64;
        Ok(CrossSessionStats {
            session_count,
            total_turns,
            total_tokens_in: tokens_in,
            total_tokens_out: tokens_out,
            total_tool_calls,
            total_tool_failures,
            total_errors,
            total_stalls,
            avg_turns_per_session: total_turns as f64 / sc,
            avg_tokens_per_session: (tokens_in + tokens_out) as f64 / sc,
            tool_error_rate: if total_tool_calls > 0 {
                total_tool_failures as f64 / total_tool_calls as f64
            } else {
                0.0
            },
            total_mutations: mutation_stats.total_mutations,
            ready_mutations: mutation_stats.ready_mutations,
            approval_required_mutations: mutation_stats.approval_required_mutations,
            applied_mutations: mutation_stats.applied_mutations,
            reverted_mutations: mutation_stats.reverted_mutations,
            blocked_mutations: mutation_stats.blocked_mutations,
            top_tools,
            top_models,
        })
    }

    async fn get_cross_session_tools(
        &self,
        user_id: &str,
        params: &CrossSessionStatsParams,
    ) -> AuditResult<Vec<CrossSessionToolAnalytics>> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let mut where_parts: Vec<String> = vec!["e.user_id = ?".into()];
        let mut bind_values: Vec<String> = vec![user_id.into()];
        if let Some(ref since) = params.since {
            where_parts.push("e.created_at >= ?".into());
            bind_values.push(since.clone());
        }
        if let Some(ref until) = params.until {
            where_parts.push("e.created_at <= ?".into());
            bind_values.push(until.clone());
        }
        let where_clause = where_parts.join(" AND ");

        let sql = format!(
            "SELECT \
               agg.tool_name, agg.total_calls, agg.total_success, agg.total_failures, \
               agg.avg_ms, agg.max_ms, agg.sessions_used \
             FROM (\
               SELECT \
                  meta_tool_name AS tool_name, \
                  COUNT(*) AS total_calls, \
                  COUNT(CASE WHEN event_type = 'tool_call' THEN 1 END) AS total_success, \
                  COUNT(CASE WHEN event_type = 'tool_error' THEN 1 END) AS total_failures, \
                 COALESCE(AVG(meta_duration_ms), 0) AS avg_ms, \
                 COALESCE(MAX(meta_duration_ms), 0) AS max_ms, \
                 COUNT(DISTINCT session_id) AS sessions_used \
                FROM agent_events e \
                WHERE {where_clause} AND event_type IN ('tool_call', 'tool_error') \
                GROUP BY tool_name \
              ) agg \
              ORDER BY agg.total_calls DESC \
              LIMIT ?"
        );
        let mut q = sqlx::query(&sql);
        for v in &bind_values {
            q = q.bind(v);
        }
        q = q.bind(MAX_CROSS_SESSION_TOOLS);
        let rows = q.fetch_all(&pool).await.map_err(internal_error)?;

        let mut error_sql = format!(
            "SELECT meta_tool_name AS tool_name, \
             SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, {cap}) AS content \
             FROM agent_events e \
             WHERE {where_clause} AND event_type = 'tool_error' \
             ORDER BY created_at DESC",
            cap = agent_events_content_cap::TOOL_LAST_ERROR
        );
        if !rows.is_empty() {
            error_sql.push_str(" LIMIT 500");
        }
        let mut eq = sqlx::query(&error_sql);
        for v in &bind_values {
            eq = eq.bind(v);
        }
        let error_rows = eq.fetch_all(&pool).await.map_err(internal_error)?;
        let mut latest_errors = std::collections::HashMap::<String, String>::new();
        for row in error_rows {
            let tool_name = normalize_tool_name(row.try_get("tool_name").unwrap_or_default());
            if latest_errors.contains_key(&tool_name) {
                continue;
            }
            let content: String = row.try_get("content").unwrap_or_default();
            if !content.is_empty() {
                latest_errors.insert(tool_name, content);
            }
        }

        let result: Vec<CrossSessionToolAnalytics> = rows
            .iter()
            .map(|row| {
                let name = normalize_tool_name(row.try_get("tool_name").unwrap_or_default());
                let total_calls = row.try_get::<i64, _>("total_calls").unwrap_or(0) as u32;
                let total_success = row.try_get::<i64, _>("total_success").unwrap_or(0) as u32;
                let total_failures = row.try_get::<i64, _>("total_failures").unwrap_or(0) as u32;
                let last_error = latest_errors.get(&name).cloned();

                CrossSessionToolAnalytics {
                    name,
                    total_calls,
                    total_success,
                    total_failures,
                    success_rate: if total_calls > 0 {
                        total_success as f64 / total_calls as f64
                    } else {
                        0.0
                    },
                    avg_duration_ms: row
                        .try_get::<f64, _>("avg_ms")
                        .or_else(|_| row.try_get::<i64, _>("avg_ms").map(|v| v as f64))
                        .unwrap_or(0.0),
                    max_duration_ms: row
                        .try_get::<i64, _>("max_ms")
                        .or_else(|_| row.try_get::<f64, _>("max_ms").map(|v| v as i64))
                        .unwrap_or(0) as u64,
                    sessions_used_in: row.try_get::<i64, _>("sessions_used").unwrap_or(0) as u32,
                    last_error,
                }
            })
            .collect();

        Ok(result)
    }

    async fn list_cross_session_mutations(
        &self,
        user_id: &str,
        params: &CrossSessionMutationListParams,
    ) -> AuditResult<CrossSessionMutationListResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let (mutation_decisions, mutation_overrides) = self
            .load_cross_session_mutation_inputs(
                &pool,
                user_id,
                params.since.as_deref(),
                params.until.as_deref(),
            )
            .await?;
        let mut mutations =
            build_cross_session_mutation_scoreboards(mutation_decisions, mutation_overrides)
                .into_iter()
                .flat_map(|scoreboard| scoreboard.mutations.into_iter())
                .collect::<Vec<_>>();

        if let Some(state) = params.state {
            mutations.retain(|mutation| mutation.state == state);
        }

        mutations.sort_by(|left, right| {
            right
                .state_updated_at
                .cmp(&left.state_updated_at)
                .then_with(|| right.session_id.cmp(&left.session_id))
                .then_with(|| right.turn_index.cmp(&left.turn_index))
                .then_with(|| right.mutation_id.cmp(&left.mutation_id))
        });

        let total = mutations.len() as u32;
        let page = params.page.max(1);
        let per_page = params
            .per_page
            .clamp(1, MAX_CROSS_SESSION_MUTATIONS_PER_PAGE);
        let offset = (page.saturating_sub(1) * per_page) as usize;
        let mutations = mutations
            .into_iter()
            .skip(offset)
            .take(per_page as usize)
            .collect();

        Ok(CrossSessionMutationListResponse {
            mutations,
            total,
            page,
            per_page,
        })
    }
}

// ── Unconfigured fallback ────────────────────────────────────────────────────

pub struct UnconfiguredSessionAuditService;

#[async_trait]
impl SessionAuditService for UnconfiguredSessionAuditService {
    async fn get_summary(&self, _: &str, _: &str) -> AuditResult<SessionAuditSummary> {
        Err(internal_error("audit service not configured"))
    }
    async fn list_turns(
        &self,
        _: &str,
        _: &str,
        _: &TurnListParams,
    ) -> AuditResult<TurnListResponse> {
        Err(internal_error("audit service not configured"))
    }
    async fn get_turn_detail(&self, _: &str, _: &str, _: u32) -> AuditResult<TurnDetail> {
        Err(internal_error("audit service not configured"))
    }
    async fn get_tool_analytics(&self, _: &str, _: &str) -> AuditResult<Vec<ToolAnalytics>> {
        Err(internal_error("audit service not configured"))
    }
    async fn list_errors(&self, _: &str, _: &str) -> AuditResult<ErrorListResponse> {
        Err(internal_error("audit service not configured"))
    }
    async fn get_mutation_scoreboard(&self, _: &str, _: &str) -> AuditResult<MutationScoreboard> {
        Err(internal_error("audit service not configured"))
    }
    async fn list_sessions(
        &self,
        _: &str,
        _: &AuditSessionListParams,
    ) -> AuditResult<AuditSessionListResponse> {
        Err(internal_error("audit service not configured"))
    }
    async fn get_cross_session_stats(
        &self,
        _: &str,
        _: &CrossSessionStatsParams,
    ) -> AuditResult<CrossSessionStats> {
        Err(internal_error("audit service not configured"))
    }
    async fn get_cross_session_tools(
        &self,
        _: &str,
        _: &CrossSessionStatsParams,
    ) -> AuditResult<Vec<CrossSessionToolAnalytics>> {
        Err(internal_error("audit service not configured"))
    }
    async fn list_cross_session_mutations(
        &self,
        _: &str,
        _: &CrossSessionMutationListParams,
    ) -> AuditResult<CrossSessionMutationListResponse> {
        Err(internal_error("audit service not configured"))
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        let mut end = max_len;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

fn extract_tool_calls_from_metadata(meta: &serde_json::Value) -> Vec<ToolCallBrief> {
    meta.get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|tc| ToolCallBrief {
                    name: tc
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    ok: tc.get("ok").and_then(|v| v.as_bool()).unwrap_or(true),
                    duration_ms: tc.get("ms").and_then(|v| v.as_u64()).unwrap_or(0),
                    error: tc.get("error").and_then(|v| v.as_str()).map(String::from),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn compute_duration_secs(first: Option<&str>, last: Option<&str>) -> f64 {
    match (first, last) {
        (Some(f), Some(l)) => {
            // Try parsing as ISO 8601 / chrono-compatible timestamps
            if let (Ok(ft), Ok(lt)) = (
                chrono::NaiveDateTime::parse_from_str(f, "%Y-%m-%d %H:%M:%S%.f"),
                chrono::NaiveDateTime::parse_from_str(l, "%Y-%m-%d %H:%M:%S%.f"),
            ) {
                (lt - ft).num_milliseconds() as f64 / 1000.0
            } else if let (Ok(ft), Ok(lt)) = (
                chrono::DateTime::parse_from_rfc3339(f),
                chrono::DateTime::parse_from_rfc3339(l),
            ) {
                (lt - ft).num_milliseconds() as f64 / 1000.0
            } else {
                0.0
            }
        }
        _ => 0.0,
    }
}

fn build_mutation_scoreboard(
    session_id: &str,
    decisions: Vec<PersistedMutationDecision>,
    overrides: Vec<MutationStateOverride>,
) -> MutationScoreboard {
    let scoreboard = MutationScoreboard::from_persisted_decisions(
        format!("audit:mutation-scoreboard:{session_id}"),
        session_id.to_string(),
        decisions,
    );
    if overrides.is_empty() {
        return scoreboard;
    }

    let mut latest_overrides = std::collections::HashMap::<String, MutationStateOverride>::new();
    for override_entry in overrides {
        latest_overrides.insert(override_entry.mutation_id.clone(), override_entry);
    }

    let mutations = scoreboard
        .mutations
        .into_iter()
        .map(|mut mutation| {
            if let Some(override_entry) = latest_overrides.get(&mutation.mutation_id) {
                mutation.state = override_entry.state;
                mutation.state_note = override_entry.note.clone();
                mutation.state_updated_at = Some(override_entry.created_at.clone());
            }
            mutation
        })
        .collect::<Vec<_>>();

    MutationScoreboard::new(scoreboard.scoreboard_id, scoreboard.session_id, mutations)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct MutationStatsAggregate {
    total_mutations: u32,
    ready_mutations: u32,
    approval_required_mutations: u32,
    applied_mutations: u32,
    reverted_mutations: u32,
    blocked_mutations: u32,
}

fn parse_mutation_state_override(
    metadata: &serde_json::Value,
    created_at: String,
) -> Option<MutationStateOverride> {
    let mutation_id = metadata
        .get("mutation_id")
        .and_then(serde_json::Value::as_str)?;
    let state = metadata
        .get("state")
        .cloned()
        .and_then(|value| serde_json::from_value::<StagedMutationState>(value).ok())?;
    if !matches!(
        state,
        StagedMutationState::Applied | StagedMutationState::Reverted | StagedMutationState::Blocked
    ) {
        return None;
    }
    Some(MutationStateOverride {
        mutation_id: mutation_id.to_string(),
        state,
        note: metadata
            .get("note")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|note| !note.is_empty())
            .map(ToString::to_string),
        created_at,
    })
}

fn aggregate_cross_session_mutation_stats(
    decisions: Vec<PersistedMutationDecision>,
    overrides: Vec<(String, MutationStateOverride)>,
) -> MutationStatsAggregate {
    let mut aggregate = MutationStatsAggregate::default();
    for scoreboard in build_cross_session_mutation_scoreboards(decisions, overrides) {
        aggregate.total_mutations += scoreboard.total_mutations;
        aggregate.ready_mutations += scoreboard.ready_mutations;
        aggregate.approval_required_mutations += scoreboard.approval_required_mutations;
        aggregate.applied_mutations += scoreboard.applied_mutations;
        aggregate.reverted_mutations += scoreboard.reverted_mutations;
        aggregate.blocked_mutations += scoreboard.blocked_mutations;
    }

    aggregate
}

fn build_cross_session_mutation_scoreboards(
    decisions: Vec<PersistedMutationDecision>,
    overrides: Vec<(String, MutationStateOverride)>,
) -> Vec<MutationScoreboard> {
    let mut decisions_by_session =
        std::collections::HashMap::<String, Vec<PersistedMutationDecision>>::new();
    for decision in decisions {
        decisions_by_session
            .entry(decision.session_id.clone())
            .or_default()
            .push(decision);
    }

    let mut overrides_by_session =
        std::collections::HashMap::<String, Vec<MutationStateOverride>>::new();
    for (session_id, override_entry) in overrides {
        overrides_by_session
            .entry(session_id)
            .or_default()
            .push(override_entry);
    }

    let mut session_ids = decisions_by_session
        .keys()
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    session_ids.extend(overrides_by_session.keys().cloned());

    let mut scoreboards = session_ids
        .into_iter()
        .map(|session_id| {
            build_mutation_scoreboard(
                &session_id,
                decisions_by_session.remove(&session_id).unwrap_or_default(),
                overrides_by_session.remove(&session_id).unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    scoreboards.sort_by(|left, right| left.session_id.cmp(&right.session_id));
    scoreboards
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis() {
        let result = truncate_str("hello world", 5);
        assert_eq!(result, "hello…");
    }

    #[test]
    fn truncate_handles_unicode_boundary() {
        // CJK characters are 3 bytes each in UTF-8
        let s = "你好世界";
        let result = truncate_str(s, 7);
        // Should not panic, should truncate at char boundary
        assert!(result.ends_with('…'));
        assert!(result.len() <= 10); // 6 bytes for 2 CJK chars + 3 bytes for …
    }

    #[test]
    fn extract_tool_calls_empty_metadata() {
        let meta = serde_json::json!({});
        let calls = extract_tool_calls_from_metadata(&meta);
        assert!(calls.is_empty());
    }

    #[test]
    fn extract_tool_calls_from_json() {
        let meta = serde_json::json!({
            "tool_calls": [
                {"name": "bash", "ok": true, "ms": 150},
                {"name": "write_file", "ok": false, "ms": 200, "error": "permission denied"},
            ]
        });
        let calls = extract_tool_calls_from_metadata(&meta);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "bash");
        assert!(calls[0].ok);
        assert_eq!(calls[0].duration_ms, 150);
        assert!(calls[0].error.is_none());
        assert_eq!(calls[1].name, "write_file");
        assert!(!calls[1].ok);
        assert_eq!(calls[1].error.as_deref(), Some("permission denied"));
    }

    #[test]
    fn compute_duration_rfc3339() {
        let d = compute_duration_secs(
            Some("2026-04-01T10:00:00+08:00"),
            Some("2026-04-01T10:05:30+08:00"),
        );
        assert!((d - 330.0).abs() < 0.01);
    }

    #[test]
    fn compute_duration_mysql_format() {
        let d = compute_duration_secs(
            Some("2026-04-01 10:00:00.000000"),
            Some("2026-04-01 10:05:30.000000"),
        );
        assert!((d - 330.0).abs() < 0.01);
    }

    #[test]
    fn compute_duration_none_returns_zero() {
        assert_eq!(compute_duration_secs(None, None), 0.0);
        assert_eq!(compute_duration_secs(Some("x"), None), 0.0);
    }

    #[test]
    fn tool_analytics_success_rate_calculation() {
        let mut ta = ToolAnalytics {
            name: "test".into(),
            call_count: 10,
            success_count: 8,
            fail_count: 2,
            success_rate: 0.0,
            avg_duration_ms: 0.0,
            max_duration_ms: 0,
            total_duration_ms: 1000,
            last_error: None,
        };
        if ta.call_count > 0 {
            ta.success_rate = ta.success_count as f64 / ta.call_count as f64;
            ta.avg_duration_ms = ta.total_duration_ms as f64 / ta.call_count as f64;
        }
        assert!((ta.success_rate - 0.8).abs() < 0.001);
        assert!((ta.avg_duration_ms - 100.0).abs() < 0.001);
    }

    #[test]
    fn turn_list_params_defaults() {
        let p: TurnListParams = serde_json::from_str("{}").unwrap();
        assert_eq!(p.page, 1);
        assert_eq!(p.per_page, 20);
    }

    #[test]
    fn session_audit_summary_serialization() {
        let summary = SessionAuditSummary {
            session_id: "s1".into(),
            status: "active".into(),
            turn_count: 10,
            tokens_in: 5000,
            tokens_out: 3000,
            tool_calls_total: 25,
            tool_calls_failed: 2,
            error_count: 1,
            stall_count: 0,
            checkpoint_count: 2,
            compact_count: 0,
            models_used: vec!["gpt-4".into()],
            duration_secs: 120.5,
            created_at: "2026-04-01T10:00:00Z".into(),
            ended_at: None,
        };
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"turn_count\":10"));
        assert!(json.contains("\"tokens_in\":5000"));
    }

    // ── Cross-session type tests ─────────────────────────────────────────────

    #[test]
    fn session_list_params_defaults() {
        let p: AuditSessionListParams = serde_json::from_str("{}").unwrap();
        assert_eq!(p.page, 1);
        assert_eq!(p.per_page, 20);
        assert!(p.status.is_none());
        assert!(p.model.is_none());
        assert!(p.since.is_none());
        assert!(p.until.is_none());
        assert!(p.min_turns.is_none());
        assert_eq!(p.sort, "created");
        assert_eq!(p.order, "desc");
    }

    #[test]
    fn session_list_params_with_filters() {
        let p: AuditSessionListParams = serde_json::from_str(
            r#"{"status":"ended","model":"gpt-4","since":"2026-01-01","min_turns":5,"sort":"turns","order":"asc"}"#,
        )
        .unwrap();
        assert_eq!(p.status.as_deref(), Some("ended"));
        assert_eq!(p.model.as_deref(), Some("gpt-4"));
        assert_eq!(p.since.as_deref(), Some("2026-01-01"));
        assert_eq!(p.min_turns, Some(5));
        assert_eq!(p.sort, "turns");
        assert_eq!(p.order, "asc");
    }

    #[test]
    fn cross_session_stats_serialization() {
        let stats = CrossSessionStats {
            session_count: 10,
            total_turns: 150,
            total_tokens_in: 500_000,
            total_tokens_out: 300_000,
            total_tool_calls: 200,
            total_tool_failures: 15,
            total_errors: 5,
            total_stalls: 2,
            avg_turns_per_session: 15.0,
            avg_tokens_per_session: 80_000.0,
            tool_error_rate: 0.075,
            total_mutations: 12,
            ready_mutations: 4,
            approval_required_mutations: 3,
            applied_mutations: 2,
            reverted_mutations: 1,
            blocked_mutations: 2,
            top_tools: vec![ToolUsageBrief {
                name: "bash".into(),
                call_count: 100,
                success_rate: 0.95,
            }],
            top_models: vec![ModelUsageBrief {
                model: "gpt-4".into(),
                session_count: 8,
                total_tokens: 600_000,
            }],
        };
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("\"session_count\":10"));
        assert!(json.contains("\"total_turns\":150"));
        assert!(json.contains("\"total_mutations\":12"));
        assert!(json.contains("\"top_tools\":["));
        assert!(json.contains("\"top_models\":["));
    }

    #[test]
    fn cross_session_stats_zero_sessions() {
        let stats = CrossSessionStats {
            session_count: 0,
            total_turns: 0,
            total_tokens_in: 0,
            total_tokens_out: 0,
            total_tool_calls: 0,
            total_tool_failures: 0,
            total_errors: 0,
            total_stalls: 0,
            avg_turns_per_session: 0.0,
            avg_tokens_per_session: 0.0,
            tool_error_rate: 0.0,
            total_mutations: 0,
            ready_mutations: 0,
            approval_required_mutations: 0,
            applied_mutations: 0,
            reverted_mutations: 0,
            blocked_mutations: 0,
            top_tools: vec![],
            top_models: vec![],
        };
        assert_eq!(stats.session_count, 0);
        assert!(stats.top_tools.is_empty());
        assert!(stats.top_models.is_empty());
    }

    #[test]
    fn cross_session_tool_analytics_serialization() {
        let t = CrossSessionToolAnalytics {
            name: "write_file".into(),
            total_calls: 50,
            total_success: 48,
            total_failures: 2,
            success_rate: 0.96,
            avg_duration_ms: 120.5,
            max_duration_ms: 500,
            sessions_used_in: 7,
            last_error: Some("permission denied".into()),
        };
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("\"write_file\""));
        assert!(json.contains("\"sessions_used_in\":7"));
        assert!(json.contains("\"last_error\":\"permission denied\""));
    }

    #[test]
    fn session_list_item_serialization() {
        let item = AuditSessionListItem {
            session_id: "sess-123".into(),
            status: "ended".into(),
            turn_count: 25,
            tokens_in: 10_000,
            tokens_out: 8_000,
            tool_calls_total: 40,
            error_count: 1,
            model: Some("gpt-4".into()),
            duration_secs: 300.5,
            created_at: "2026-04-01T10:00:00Z".into(),
            ended_at: Some("2026-04-01T10:05:00Z".into()),
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"session_id\":\"sess-123\""));
        assert!(json.contains("\"turn_count\":25"));
        assert!(json.contains("\"ended_at\":\"2026-04-01T10:05:00Z\""));
    }

    #[test]
    fn cross_session_stats_params_defaults() {
        let p: CrossSessionStatsParams = serde_json::from_str("{}").unwrap();
        assert!(p.since.is_none());
        assert!(p.until.is_none());
    }

    #[test]
    fn tool_usage_brief_success_rate() {
        let t = ToolUsageBrief {
            name: "bash".into(),
            call_count: 100,
            success_rate: 0.95,
        };
        assert!((t.success_rate - 0.95).abs() < 0.001);
    }

    // ── Unhappy path / edge-case tests ──

    #[test]
    fn normalize_tool_name_empty() {
        assert_eq!(normalize_tool_name("".into()), "unknown");
    }

    #[test]
    fn normalize_tool_name_whitespace() {
        assert_eq!(normalize_tool_name("   ".into()), "unknown");
    }

    #[test]
    fn normalize_tool_name_quoted() {
        assert_eq!(normalize_tool_name("\"bash\"".into()), "bash");
    }

    #[test]
    fn normalize_tool_name_double_quoted_empty() {
        assert_eq!(normalize_tool_name("\"\"".into()), "unknown");
    }

    #[test]
    fn normalize_tool_name_normal() {
        assert_eq!(normalize_tool_name("write_file".into()), "write_file");
    }

    #[test]
    fn truncate_str_empty() {
        assert_eq!(truncate_str("", 10), "");
    }

    #[test]
    fn truncate_str_zero_max_len() {
        let result = truncate_str("hello", 0);
        assert_eq!(result, "…");
    }

    #[test]
    fn truncate_str_exact_boundary() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn truncate_str_multibyte_no_panic() {
        // 4 CJK chars = 12 bytes, truncate at 7 bytes → must find char boundary
        let result = truncate_str("你好世界", 7);
        assert!(result.ends_with('…'));
        // Should be "你好…" (6 bytes for 2 CJK + 3 bytes for …)
    }

    #[test]
    fn extract_tool_calls_null_metadata() {
        let meta = serde_json::json!(null);
        let calls = extract_tool_calls_from_metadata(&meta);
        assert!(calls.is_empty());
    }

    #[test]
    fn extract_tool_calls_non_array_tool_calls() {
        let meta = serde_json::json!({"tool_calls": "not_an_array"});
        let calls = extract_tool_calls_from_metadata(&meta);
        assert!(calls.is_empty());
    }

    #[test]
    fn extract_tool_calls_empty_array() {
        let meta = serde_json::json!({"tool_calls": []});
        let calls = extract_tool_calls_from_metadata(&meta);
        assert!(calls.is_empty());
    }

    #[test]
    fn extract_tool_calls_missing_fields_default() {
        let meta = serde_json::json!({"tool_calls": [{}]});
        let calls = extract_tool_calls_from_metadata(&meta);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "unknown");
        assert!(calls[0].ok);
        assert_eq!(calls[0].duration_ms, 0);
        assert!(calls[0].error.is_none());
    }

    #[test]
    fn compute_duration_invalid_formats() {
        assert_eq!(
            compute_duration_secs(Some("not-a-date"), Some("also-not")),
            0.0
        );
    }

    #[test]
    fn compute_duration_mixed_formats() {
        // One RFC3339, one MySQL → neither parser matches both
        assert_eq!(
            compute_duration_secs(
                Some("2026-04-01T10:00:00+08:00"),
                Some("2026-04-01 10:05:00.000000")
            ),
            0.0
        );
    }

    #[test]
    fn compute_duration_negative_result() {
        // End before start → negative duration
        let d = compute_duration_secs(
            Some("2026-04-01 10:05:00.000000"),
            Some("2026-04-01 10:00:00.000000"),
        );
        assert!(d < 0.0);
    }

    #[test]
    fn audit_session_list_params_defaults() {
        let json = r#"{}"#;
        let p: AuditSessionListParams = serde_json::from_str(json).unwrap();
        assert_eq!(p.page, 1);
        assert_eq!(p.per_page, 20);
        assert_eq!(p.sort, "created");
        assert_eq!(p.order, "desc");
        assert!(p.status.is_none());
        assert!(p.model.is_none());
        assert!(p.min_turns.is_none());
    }

    #[test]
    fn audit_session_list_params_custom() {
        let json = r#"{"page":2,"per_page":50,"status":"active","sort":"turns","order":"asc","min_turns":5}"#;
        let p: AuditSessionListParams = serde_json::from_str(json).unwrap();
        assert_eq!(p.page, 2);
        assert_eq!(p.per_page, 50);
        assert_eq!(p.status.as_deref(), Some("active"));
        assert_eq!(p.sort, "turns");
        assert_eq!(p.order, "asc");
        assert_eq!(p.min_turns, Some(5));
    }

    #[test]
    fn tool_call_brief_skip_serializing_none_error() {
        let tc = ToolCallBrief {
            name: "bash".into(),
            ok: true,
            duration_ms: 100,
            error: None,
        };
        let json = serde_json::to_string(&tc).unwrap();
        assert!(!json.contains("error"));
    }

    #[test]
    fn tool_call_brief_with_error() {
        let tc = ToolCallBrief {
            name: "bash".into(),
            ok: false,
            duration_ms: 200,
            error: Some("exit code 1".into()),
        };
        let json = serde_json::to_string(&tc).unwrap();
        assert!(json.contains("exit code 1"));
    }

    #[test]
    fn session_audit_summary_roundtrip() {
        let s = SessionAuditSummary {
            session_id: "s1".into(),
            status: "active".into(),
            turn_count: 0,
            tokens_in: 0,
            tokens_out: 0,
            tool_calls_total: 0,
            tool_calls_failed: 0,
            error_count: 0,
            stall_count: 0,
            checkpoint_count: 0,
            compact_count: 0,
            models_used: vec![],
            duration_secs: 0.0,
            created_at: "2024-01-01".into(),
            ended_at: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        let restored: SessionAuditSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.session_id, "s1");
        assert!(restored.models_used.is_empty());
    }

    #[test]
    fn cross_session_stats_serde() {
        let stats = CrossSessionStats {
            session_count: 10,
            total_turns: 100,
            total_tokens_in: 50000,
            total_tokens_out: 30000,
            total_tool_calls: 200,
            total_tool_failures: 5,
            total_errors: 2,
            total_stalls: 1,
            avg_turns_per_session: 10.0,
            avg_tokens_per_session: 8000.0,
            tool_error_rate: 0.025,
            total_mutations: 9,
            ready_mutations: 3,
            approval_required_mutations: 2,
            applied_mutations: 2,
            reverted_mutations: 1,
            blocked_mutations: 1,
            top_tools: vec![],
            top_models: vec![],
        };
        let json = serde_json::to_string(&stats).unwrap();
        let restored: CrossSessionStats = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.session_count, 10);
        assert_eq!(restored.total_mutations, 9);
        assert!((restored.tool_error_rate - 0.025).abs() < 0.001);
    }

    #[test]
    fn model_usage_brief_serde() {
        let m = ModelUsageBrief {
            model: "claude-3.5".into(),
            session_count: 5,
            total_tokens: 100000,
        };
        let json = serde_json::to_string(&m).unwrap();
        let restored: ModelUsageBrief = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.model, "claude-3.5");
    }

    #[test]
    fn cross_session_tool_analytics_serde() {
        let t = CrossSessionToolAnalytics {
            name: "bash".into(),
            total_calls: 100,
            total_success: 95,
            total_failures: 5,
            success_rate: 0.95,
            avg_duration_ms: 150.0,
            max_duration_ms: 5000,
            sessions_used_in: 8,
            last_error: Some("timeout".into()),
        };
        let json = serde_json::to_string(&t).unwrap();
        let restored: CrossSessionToolAnalytics = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.last_error.as_deref(), Some("timeout"));
    }

    #[test]
    fn build_mutation_scoreboard_skips_decisions_without_objective_score() {
        let scoreboard = build_mutation_scoreboard(
            "session-1",
            vec![
                PersistedMutationDecision {
                    decision_id: "decision-1".into(),
                    session_id: "session-1".into(),
                    decision_output: serde_json::json!({
                        "turn": 4,
                        "mutation_objective_score": {
                            "quality": {"point": 0.84, "lower": 0.84, "upper": 0.84},
                            "reward_hacking_risk": {"point": 0.10, "lower": 0.10, "upper": 0.10},
                            "causal_support": {"point": 0.75, "lower": 0.75, "upper": 0.75},
                            "was_corrected": false
                        },
                        "action_profiles": [
                            {
                                "tool_call_id": "call-1",
                                "tool_name": "edit_file",
                                "arguments": {"path": "src/lib.rs"},
                                "profile": {
                                    "bounded": true,
                                    "reversible": true,
                                    "requires_pre_state": false,
                                    "action_category": "write",
                                    "compensation_kind": "restore_file",
                                    "compensation_summary": "restore prior contents"
                                }
                            }
                        ]
                    }),
                },
                PersistedMutationDecision {
                    decision_id: "decision-2".into(),
                    session_id: "session-1".into(),
                    decision_output: serde_json::json!({
                        "turn": 5,
                        "action_profiles": [
                            {
                                "tool_call_id": "call-2",
                                "tool_name": "bash",
                                "arguments": {"command": "ls"},
                                "profile": {
                                    "bounded": true,
                                    "reversible": true,
                                    "requires_pre_state": false,
                                    "action_category": "read"
                                }
                            }
                        ]
                    }),
                },
            ],
            vec![],
        );

        assert_eq!(scoreboard.total_mutations, 1);
        assert_eq!(scoreboard.ready_mutations, 1);
        assert_eq!(scoreboard.mutations[0].tool_name, "edit_file");
        assert_eq!(scoreboard.mutations[0].turn_index, 4);
    }

    #[test]
    fn build_mutation_scoreboard_applies_latest_state_override() {
        let scoreboard = build_mutation_scoreboard(
            "session-1",
            vec![PersistedMutationDecision {
                decision_id: "decision-1".into(),
                session_id: "session-1".into(),
                decision_output: serde_json::json!({
                    "turn": 4,
                    "mutation_objective_score": {
                        "quality": {"point": 0.84, "lower": 0.84, "upper": 0.84},
                        "reward_hacking_risk": {"point": 0.10, "lower": 0.10, "upper": 0.10},
                        "causal_support": {"point": 0.75, "lower": 0.75, "upper": 0.75},
                        "was_corrected": false
                    },
                    "action_profiles": [
                        {
                            "tool_call_id": "call-1",
                            "tool_name": "edit_file",
                            "arguments": {"path": "src/lib.rs"},
                            "profile": {
                                "bounded": true,
                                "reversible": true,
                                "requires_pre_state": false,
                                "action_category": "write",
                                "compensation_kind": "restore_file",
                                "compensation_summary": "restore prior contents"
                            }
                        }
                    ]
                }),
            }],
            vec![
                MutationStateOverride {
                    mutation_id: "decision-1:call-1".into(),
                    state: StagedMutationState::Applied,
                    note: Some("promoted after review".into()),
                    created_at: "2026-04-12T12:00:00Z".into(),
                },
                MutationStateOverride {
                    mutation_id: "decision-1:call-1".into(),
                    state: StagedMutationState::Reverted,
                    note: Some("rolled back after regression".into()),
                    created_at: "2026-04-12T12:05:00Z".into(),
                },
            ],
        );

        assert_eq!(scoreboard.applied_mutations, 0);
        assert_eq!(scoreboard.reverted_mutations, 1);
        assert_eq!(scoreboard.mutations[0].state, StagedMutationState::Reverted);
        assert_eq!(
            scoreboard.mutations[0].state_note.as_deref(),
            Some("rolled back after regression")
        );
        assert_eq!(
            scoreboard.mutations[0].state_updated_at.as_deref(),
            Some("2026-04-12T12:05:00Z")
        );
    }

    #[test]
    fn aggregate_cross_session_mutation_stats_sums_per_session_scoreboards() {
        let stats = aggregate_cross_session_mutation_stats(
            vec![
                PersistedMutationDecision {
                    decision_id: "decision-1".into(),
                    session_id: "session-1".into(),
                    decision_output: serde_json::json!({
                        "turn": 1,
                        "mutation_objective_score": {
                            "quality": {"point": 0.90, "lower": 0.90, "upper": 0.90},
                            "reward_hacking_risk": {"point": 0.05, "lower": 0.05, "upper": 0.05},
                            "causal_support": {"point": 0.90, "lower": 0.90, "upper": 0.90},
                            "was_corrected": false
                        },
                        "action_profiles": [
                            {
                                "tool_call_id": "call-1",
                                "tool_name": "edit_file",
                                "arguments": {"path": "src/lib.rs"},
                                "profile": {
                                    "bounded": true,
                                    "reversible": true,
                                    "requires_pre_state": false,
                                    "action_category": "write",
                                    "compensation_kind": "restore_file",
                                    "compensation_summary": "restore prior contents"
                                }
                            }
                        ]
                    }),
                },
                PersistedMutationDecision {
                    decision_id: "decision-2".into(),
                    session_id: "session-2".into(),
                    decision_output: serde_json::json!({
                        "turn": 2,
                        "mutation_objective_score": {
                            "quality": {"point": 0.75, "lower": 0.75, "upper": 0.75},
                            "reward_hacking_risk": {"point": 0.15, "lower": 0.15, "upper": 0.15},
                            "causal_support": {"point": 0.70, "lower": 0.70, "upper": 0.70},
                            "was_corrected": false
                        },
                        "action_profiles": [
                            {
                                "tool_call_id": "call-2",
                                "tool_name": "bash",
                                "arguments": {"command": "git commit -m x"},
                                "profile": {
                                    "bounded": false,
                                    "reversible": true,
                                    "requires_pre_state": false,
                                    "action_category": "execute",
                                    "compensation_kind": "git_revert_commit",
                                    "compensation_summary": "revert the commit"
                                }
                            }
                        ]
                    }),
                },
            ],
            vec![(
                "session-2".into(),
                MutationStateOverride {
                    mutation_id: "decision-2:call-2".into(),
                    state: StagedMutationState::Applied,
                    note: Some("applied globally".into()),
                    created_at: "2026-04-12T12:00:00Z".into(),
                },
            )],
        );

        assert_eq!(stats.total_mutations, 2);
        assert_eq!(stats.ready_mutations, 1);
        assert_eq!(stats.applied_mutations, 1);
        assert_eq!(stats.approval_required_mutations, 0);
        assert_eq!(stats.blocked_mutations, 0);
    }

    #[test]
    fn build_cross_session_mutation_scoreboards_returns_session_scoped_mutations() {
        let scoreboards = build_cross_session_mutation_scoreboards(
            vec![
                PersistedMutationDecision {
                    decision_id: "decision-1".into(),
                    session_id: "session-a".into(),
                    decision_output: serde_json::json!({
                        "turn": 1,
                        "mutation_objective_score": {
                            "quality": {"point": 0.90, "lower": 0.90, "upper": 0.90},
                            "reward_hacking_risk": {"point": 0.05, "lower": 0.05, "upper": 0.05},
                            "causal_support": {"point": 0.90, "lower": 0.90, "upper": 0.90},
                            "was_corrected": false
                        },
                        "action_profiles": [
                            {
                                "tool_call_id": "call-a",
                                "tool_name": "edit_file",
                                "arguments": {"path": "src/a.rs"},
                                "profile": {
                                    "bounded": true,
                                    "reversible": true,
                                    "requires_pre_state": false,
                                    "action_category": "write",
                                    "compensation_kind": "restore_file",
                                    "compensation_summary": "restore prior contents"
                                }
                            }
                        ]
                    }),
                },
                PersistedMutationDecision {
                    decision_id: "decision-2".into(),
                    session_id: "session-b".into(),
                    decision_output: serde_json::json!({
                        "turn": 2,
                        "mutation_objective_score": {
                            "quality": {"point": 0.80, "lower": 0.80, "upper": 0.80},
                            "reward_hacking_risk": {"point": 0.10, "lower": 0.10, "upper": 0.10},
                            "causal_support": {"point": 0.80, "lower": 0.80, "upper": 0.80},
                            "was_corrected": false
                        },
                        "action_profiles": [
                            {
                                "tool_call_id": "call-b",
                                "tool_name": "edit_file",
                                "arguments": {"path": "src/b.rs"},
                                "profile": {
                                    "bounded": true,
                                    "reversible": true,
                                    "requires_pre_state": false,
                                    "action_category": "write",
                                    "compensation_kind": "restore_file",
                                    "compensation_summary": "restore prior contents"
                                }
                            }
                        ]
                    }),
                },
            ],
            vec![],
        );

        assert_eq!(scoreboards.len(), 2);
        assert_eq!(scoreboards[0].session_id, "session-a");
        assert_eq!(scoreboards[1].session_id, "session-b");
        assert_eq!(scoreboards[0].mutations[0].tool_args["path"], "src/a.rs");
        assert_eq!(scoreboards[1].mutations[0].tool_args["path"], "src/b.rs");
    }

    #[test]
    fn cross_session_mutation_list_params_defaults() {
        let params: CrossSessionMutationListParams = serde_json::from_str("{}").unwrap();
        assert_eq!(params.page, 1);
        assert_eq!(params.per_page, 20);
        assert!(params.since.is_none());
        assert!(params.until.is_none());
        assert!(params.state.is_none());
    }
}
