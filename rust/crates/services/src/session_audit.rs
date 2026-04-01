//! Session Audit Query Layer — cloud-side structured queries over `agent_events`.
//!
//! Provides turn-level, tool-level, and session-level audit views.
//! All queries run against MatrixOne `agent_events` + `agent_sessions` tables.

use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};

use mo_agent_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

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

// ── Request params ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct TurnListParams {
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_per_page")]
    pub per_page: u32,
}

fn default_page() -> u32 {
    1
}
fn default_per_page() -> u32 {
    20
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
}

#[async_trait]
impl SessionAuditService for DatabaseSessionAuditService {
    async fn get_summary(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> AuditResult<SessionAuditSummary> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;

        // Get session metadata from agent_sessions
        let sess_row =
            query("SELECT status, created_at, ended_at FROM agent_sessions WHERE session_id = ?")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .map_err(internal_error)?;

        let status: String = sess_row.try_get("status").unwrap_or_default();
        let created_at: String = sess_row
            .try_get::<String, _>("created_at")
            .unwrap_or_default();
        let ended_at: Option<String> = sess_row.try_get("ended_at").ok();

        // Aggregate counts by event_type from agent_events
        let rows = query(
            "SELECT event_type, COUNT(*) as cnt \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? \
             GROUP BY event_type",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let mut turn_count: u32 = 0;
        let mut error_count: u32 = 0;
        let mut stall_count: u32 = 0;
        let mut checkpoint_count: u32 = 0;
        let mut compact_count: u32 = 0;
        let mut tool_calls_total: u32 = 0;
        let mut tool_calls_failed: u32 = 0;

        for row in &rows {
            let et: String = row.try_get("event_type").unwrap_or_default();
            let cnt: i64 = row.try_get("cnt").unwrap_or(0);
            match et.as_str() {
                "turn" => turn_count = cnt as u32,
                "turn_error" => error_count = cnt as u32,
                "stall_detected" => stall_count = cnt as u32,
                "checkpoint" => checkpoint_count = cnt as u32,
                "compact" => compact_count = cnt as u32,
                "tool_call" => tool_calls_total += cnt as u32,
                "tool_error" => {
                    tool_calls_failed = cnt as u32;
                    tool_calls_total += cnt as u32;
                }
                _ => {}
            }
        }

        // Token usage aggregation for turn events
        let token_row = query(
            "SELECT \
               COALESCE(SUM(JSON_EXTRACT(token_usage, '$.input')), 0) AS tokens_in, \
               COALESCE(SUM(JSON_EXTRACT(token_usage, '$.output')), 0) AS tokens_out \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND event_type = 'turn' AND token_usage IS NOT NULL",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;

        let tokens_in: i64 = token_row.try_get("tokens_in").unwrap_or(0);
        let tokens_out: i64 = token_row.try_get("tokens_out").unwrap_or(0);

        // Distinct models used
        let model_rows = query(
            "SELECT DISTINCT llm_model_used FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND llm_model_used IS NOT NULL",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let models_used: Vec<String> = model_rows
            .iter()
            .filter_map(|r| r.try_get::<String, _>("llm_model_used").ok())
            .collect();

        // Duration: difference between first and last event
        let dur_row = query(
            "SELECT \
               MIN(created_at) AS first_at, \
               MAX(created_at) AS last_at \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ?",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;

        let first_at: Option<String> = dur_row.try_get("first_at").ok();
        let last_at: Option<String> = dur_row.try_get("last_at").ok();
        let duration_secs = compute_duration_secs(first_at.as_deref(), last_at.as_deref());

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

        // Fetch turn events with pagination
        let rows = query(
            "SELECT event_id, content, token_usage, llm_model_used, metadata, created_at \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND event_type = 'turn' \
             ORDER BY created_at ASC \
             LIMIT ? OFFSET ?",
        )
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

        // Find the turn event. metadata JSON contains the turn number.
        let rows = query(
            "SELECT event_id, content, token_usage, llm_model_used, metadata, created_at \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND event_type = 'turn' \
             ORDER BY created_at ASC",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        // Find the row matching the requested turn number
        let mut matched_row = None;
        for (i, row) in rows.iter().enumerate() {
            let meta: String = row.try_get("metadata").unwrap_or_default();
            let meta_json: serde_json::Value =
                serde_json::from_str(&meta).unwrap_or(serde_json::Value::Null);
            let turn_num = meta_json
                .get("turn")
                .and_then(|v| v.as_u64())
                .unwrap_or((i + 1) as u64) as u32;
            if turn_num == turn {
                matched_row = Some(row);
                break;
            }
        }

        let row =
            matched_row.ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Turn not found"))?;

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

        // Fetch child events (tool_call, tool_error) linked via parent_event_id
        let child_rows = query(
            "SELECT event_id, event_type, content, metadata, created_at \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND parent_event_id = ? \
             ORDER BY created_at ASC",
        )
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

        // Query tool_call and tool_error events, extract tool_name and duration from metadata
        let rows = query(
            "SELECT event_type, metadata \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? \
               AND event_type IN ('tool_call', 'tool_error') \
             ORDER BY created_at ASC",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let mut tools: std::collections::HashMap<String, ToolAnalytics> =
            std::collections::HashMap::new();

        for row in &rows {
            let et: String = row.try_get("event_type").unwrap_or_default();
            let meta_str: String = row.try_get("metadata").unwrap_or_default();
            let meta: serde_json::Value =
                serde_json::from_str(&meta_str).unwrap_or(serde_json::Value::Null);

            let tool_name = meta
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let duration_ms = meta
                .get("duration_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let ok = et == "tool_call";
            let error_msg = meta.get("error").and_then(|v| v.as_str()).map(String::from);

            let entry = tools
                .entry(tool_name.clone())
                .or_insert_with(|| ToolAnalytics {
                    name: tool_name,
                    call_count: 0,
                    success_count: 0,
                    fail_count: 0,
                    success_rate: 0.0,
                    avg_duration_ms: 0.0,
                    max_duration_ms: 0,
                    total_duration_ms: 0,
                    last_error: None,
                });

            entry.call_count += 1;
            if ok {
                entry.success_count += 1;
            } else {
                entry.fail_count += 1;
                if error_msg.is_some() {
                    entry.last_error = error_msg;
                }
            }
            entry.total_duration_ms += duration_ms;
            entry.max_duration_ms = entry.max_duration_ms.max(duration_ms);
        }

        let mut result: Vec<ToolAnalytics> = tools
            .into_values()
            .map(|mut t| {
                if t.call_count > 0 {
                    t.success_rate = t.success_count as f64 / t.call_count as f64;
                    t.avg_duration_ms = t.total_duration_ms as f64 / t.call_count as f64;
                }
                t
            })
            .collect();

        // Sort by total duration descending (heaviest tools first)
        result.sort_by(|a, b| b.total_duration_ms.cmp(&a.total_duration_ms));
        Ok(result)
    }

    async fn list_errors(&self, user_id: &str, session_id: &str) -> AuditResult<ErrorListResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;

        let rows = query(
            "SELECT event_id, event_type, content, metadata, created_at \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? \
               AND event_type IN ('turn_error', 'stall_detected', 'error', 'turn_guard_verdict', 'tool_error') \
             ORDER BY created_at ASC \
             LIMIT 200",
        )
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
}
