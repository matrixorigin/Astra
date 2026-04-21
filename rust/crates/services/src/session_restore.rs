//! Session restore: reconstruct session state from MatrixOne + local files.
//!
//! # Architecture
//!
//! ```text
//! restore_session(session_id)
//!   ├─ 1. Pull workspace metadata (local → fallback to MatrixOne)
//!   ├─ 2. Pull events (agent_events → reconstruct turn_count, recent_tools)
//!   ├─ 3. Pull learning state (learning_snapshots → merge into pipeline modules)
//!   ├─ 4. Pull checkpoints (session_checkpoints → optional rewind target)
//!   └─ 5. Return RestoredSession for the REPL to continue
//! ```
//!
//! The restore is local-first: tries local files first, falls back to MatrixOne
//! for data that may have been created on a different device.

use astra_core::is_duplicate_key_error;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const STEP_CHECKPOINT_NUMBER_OFFSET: u32 = 1_000_000_000;

// ─── Restored Session State ─────────────────────────────────────────────────

/// The reconstructed state needed to resume a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RestoredSession {
    /// Session ID being restored.
    pub session_id: String,
    /// Turn number to continue from.
    pub turn_count: u32,
    /// Total input tokens consumed so far.
    pub total_tokens_in: u64,
    /// Total output tokens consumed so far.
    pub total_tokens_out: u64,
    /// Recently used tools (for context carry-forward).
    pub recent_tools: Vec<String>,
    /// Learning snapshot JSON (for pipeline module merge).
    pub learning_snapshot_json: Option<String>,
    /// Number of checkpoints available.
    pub checkpoint_count: u32,
    /// Status of the session when last active.
    pub last_status: String,
    /// Git branch from workspace metadata.
    pub git_branch: Option<String>,
    /// Model used in the session.
    pub model: Option<String>,
    /// Session title (if set).
    pub title: Option<String>,
    /// Whether restoration was from cloud (true) or local only (false).
    pub restored_from_cloud: bool,
    /// Conversation messages from Step Protocol heavy checkpoint (for LLM resume).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conversation_messages: Vec<serde_json::Value>,
    /// Blocked/deprioritized tools from checkpoint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_tools: Vec<String>,
    /// Serialized approval overrides restored from a heavy checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_overrides: Option<serde_json::Value>,
    /// Structured interruption payload restored from a heavy checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interruption: Option<serde_json::Value>,
    /// Serialized compaction-state payload restored from a heavy checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_state: Option<serde_json::Value>,
    /// Active plan being executed (JSON-serialized TaskPlan).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executing_plan_json: Option<String>,
    /// Goal text for the executing plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_goal: Option<String>,
    /// Plan execution config (JSON-serialized PlanExecutionConfig).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_config_json: Option<String>,
    /// Number of parallel execution rounds completed.
    #[serde(default)]
    pub plan_execution_rounds: usize,
    /// Active durable task contract (JSON-serialized TaskContract).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_json: Option<String>,
    /// Operator corrections stacked during plan pause (restored for crash recovery).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan_corrections: Vec<String>,
    /// Latest structured context-trace signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_context_trace: Option<super::session_workspace::ContextTraceSignal>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionMetadataState {
    pub executing_plan_json: Option<String>,
    pub plan_goal: Option<String>,
    pub plan_config_json: Option<String>,
    pub plan_execution_rounds: usize,
    pub git_branch: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CloudHeavyCheckpointState {
    pub messages: Vec<serde_json::Value>,
    pub blocked_tools: Vec<String>,
    pub recent_tools: Vec<String>,
    pub approval_overrides: Option<serde_json::Value>,
    pub interruption: Option<serde_json::Value>,
    pub compaction_state: Option<serde_json::Value>,
}

/// A restored checkpoint entry (lightweight, for listing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoredCheckpoint {
    pub number: u32,
    pub turn: u32,
    pub title: String,
    pub summary: String,
    pub total_tokens: u64,
    /// Contract state at this checkpoint (for verification context restore).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_state_json: Option<String>,
}

/// Tool health entry mirrored from the runtime crate for breakpoint restore.
/// Avoids a circular dependency (runtime depends on services).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BreakpointToolHealthEntry {
    pub name: String,
    pub total_calls: usize,
    pub total_failures: usize,
    pub failure_rate: f64,
    #[serde(default)]
    pub last_updated_epoch: u64,
}

/// Result of restoring to a specific breakpoint.
#[derive(Debug, Clone)]
pub struct RestoredBreakpoint {
    /// The base restored session.
    pub session: RestoredSession,
    /// Breakpoint metadata.
    pub breakpoint_id: String,
    /// Tool health entries from the breakpoint.
    pub tool_health_entries: Vec<BreakpointToolHealthEntry>,
    /// Correction history JSON (to restore TurnGuard state).
    pub correction_history_json: Option<String>,
    /// Composite snapshot at the breakpoint, if one was recorded.
    /// Callers can use this to selectively restore data/memory/git dimensions.
    pub composite_snapshot: Option<astra_core::composite_snapshot::CompositeSnapshot>,
}

/// Result of restoring from a composite snapshot.
#[derive(Debug, Clone)]
pub struct RestoredCompositeState {
    /// The base restored session (from session state dimension).
    pub session: Option<RestoredSession>,
    /// The composite snapshot that was restored from.
    pub snapshot: astra_core::composite_snapshot::CompositeSnapshot,
    /// Which dimensions were successfully restored.
    pub restored_dimensions: Vec<String>,
    /// Data snapshot ref to restore (caller handles actual SQL).
    pub data_snapshot_to_restore: Option<astra_core::composite_snapshot::DataSnapshotRef>,
    /// Git commit to checkout (caller handles actual git).
    pub git_commit_to_checkout: Option<String>,
}

// ─── Session Restore Trait ──────────────────────────────────────────────────

/// Abstraction for restoring session state from various backends.
#[async_trait]
pub trait SessionRestoreService: Send + Sync {
    /// Restore a session by ID. Returns None if session not found.
    async fn restore_session(&self, session_id: &str) -> Result<Option<RestoredSession>, String>;

    /// List available checkpoints for a session.
    async fn list_checkpoints(&self, session_id: &str) -> Result<Vec<RestoredCheckpoint>, String>;

    /// Restore session state to a specific checkpoint.
    async fn restore_to_checkpoint(
        &self,
        session_id: &str,
        checkpoint_number: u32,
    ) -> Result<Option<RestoredSession>, String>;

    /// List resumable sessions for a user (active or paused).
    async fn list_resumable_sessions(&self, user_id: &str) -> Result<Vec<RestoredSession>, String>;

    /// Restore session state to a specific composite snapshot.
    /// Uses the `RestoreSelector` to determine which dimensions to restore.
    async fn restore_to_composite_snapshot(
        &self,
        session_id: &str,
        snapshot_id: &str,
        selector: &astra_core::composite_snapshot::RestoreSelector,
    ) -> Result<Option<RestoredCompositeState>, String> {
        let _ = (session_id, snapshot_id, selector);
        Ok(None)
    }

    /// List composite snapshots for a session.
    async fn list_composite_snapshots(
        &self,
        session_id: &str,
    ) -> Result<astra_core::composite_snapshot::CompositeSnapshotIndex, String> {
        let _ = session_id;
        Ok(astra_core::composite_snapshot::CompositeSnapshotIndex::default())
    }
}

// ─── Local-First Implementation ─────────────────────────────────────────────

/// Restores from local files first, falls back to MatrixOne.
pub struct HybridRestoreService {
    pool: Option<sqlx::Pool<sqlx::MySql>>,
}

impl HybridRestoreService {
    /// Create with MatrixOne pool for cloud fallback.
    pub fn new(pool: sqlx::Pool<sqlx::MySql>) -> Self {
        Self { pool: Some(pool) }
    }

    /// Create for local-only mode (no cloud fallback).
    pub fn local_only() -> Self {
        Self { pool: None }
    }

    /// Try restoring workspace metadata from local YAML file.
    fn restore_local_workspace(
        &self,
        session_id: &str,
    ) -> Option<super::session_workspace::WorkspaceMetadata> {
        super::session_workspace::read_workspace(session_id).ok()
    }

    /// Restore from MatrixOne agent_sessions table.
    async fn restore_cloud_session(
        &self,
        session_id: &str,
    ) -> Result<Option<RestoredSession>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(None),
        };

        let row = sqlx::query(
            "SELECT session_id, user_id, title, status, event_count, CAST(metadata AS CHAR) AS metadata_json, \
             (SELECT COUNT(*) FROM agent_events ae WHERE ae.session_id = agent_sessions.session_id AND event_type = 'user_query') AS turn_count, \
             (SELECT COALESCE(SUM(CASE WHEN event_type = 'user_query' AND token_usage IS NOT NULL \
                 THEN COALESCE(token_input, 0) ELSE 0 END), 0) \
               FROM agent_events ae WHERE ae.session_id = agent_sessions.session_id) AS total_tokens_in, \
             (SELECT COALESCE(SUM(CASE WHEN event_type = 'user_query' AND token_usage IS NOT NULL \
                THEN COALESCE(token_output, 0) ELSE 0 END), 0) \
               FROM agent_events ae WHERE ae.session_id = agent_sessions.session_id) AS total_tokens_out, \
             (SELECT COUNT(*) FROM session_checkpoints sc WHERE sc.session_id = agent_sessions.session_id AND state_json IS NULL) AS checkpoint_count, \
             (SELECT e.llm_model_used FROM agent_events e WHERE e.session_id = agent_sessions.session_id \
               AND e.llm_model_used IS NOT NULL AND e.llm_model_used != '' ORDER BY e.created_at DESC LIMIT 1) AS latest_model, \
             created_at, updated_at \
              FROM agent_sessions WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("restore_cloud_session: {e}"))?;

        match row {
            Some(row) => {
                use sqlx::Row;
                let user_id: String = row.try_get("user_id").unwrap_or_default();
                let status: String = row.try_get("status").unwrap_or_default();
                let title: Option<String> = row.try_get("title").ok().flatten();
                let turn_count: i64 = row.try_get("turn_count").unwrap_or(0);
                let total_tokens_in: i64 = row.try_get("total_tokens_in").unwrap_or(0);
                let total_tokens_out: i64 = row.try_get("total_tokens_out").unwrap_or(0);
                let checkpoint_count: i64 = row.try_get("checkpoint_count").unwrap_or(0);

                // Extract plan state from metadata JSON
                let metadata_str: Option<String> = row.try_get("metadata_json").ok().flatten();
                let metadata_state = metadata_str
                    .as_deref()
                    .filter(|m| !m.is_empty())
                    .map(extract_session_state_from_metadata)
                    .unwrap_or_default();

                let heavy_state = self
                    .restore_latest_heavy_checkpoint_state(session_id)
                    .await
                    .ok()
                    .flatten();

                let last_context_trace = self
                    .restore_latest_context_trace_signal(session_id)
                    .await
                    .ok()
                    .flatten();
                let mut recent_tools = self
                    .restore_recent_tools(session_id)
                    .await
                    .unwrap_or_default();
                if recent_tools.is_empty()
                    && let Some(heavy) = heavy_state.as_ref()
                {
                    append_unique_tools(
                        &mut recent_tools,
                        heavy.recent_tools.iter().map(String::as_str),
                    );
                }
                if recent_tools.is_empty() {
                    recent_tools = recent_tools_from_context_trace(last_context_trace.as_ref());
                }
                let latest_model: Option<String> = row.try_get("latest_model").ok().flatten();
                let model = metadata_state.model.clone().or(latest_model);
                let learning_snapshot_json = if user_id.is_empty() {
                    None
                } else {
                    self.restore_learning(&user_id, "default")
                        .await
                        .ok()
                        .flatten()
                };

                // Load active contract from task_contracts table
                let mut contract_json = Self::load_cloud_contract(pool, session_id)
                    .await
                    .ok()
                    .flatten();

                // Fallback: try latest checkpoint's contract state
                if contract_json.is_none()
                    && let Ok(ckpts) = self.cloud_checkpoints(session_id).await
                {
                    contract_json = ckpts
                        .iter()
                        .rev()
                        .find_map(|c| c.contract_state_json.clone());
                }

                Ok(Some(RestoredSession {
                    session_id: session_id.to_string(),
                    turn_count: turn_count as u32,
                    total_tokens_in: total_tokens_in.max(0) as u64,
                    total_tokens_out: total_tokens_out.max(0) as u64,
                    last_status: status,
                    title,
                    restored_from_cloud: true,
                    recent_tools,
                    learning_snapshot_json,
                    checkpoint_count: checkpoint_count.max(0) as u32,
                    git_branch: metadata_state.git_branch.clone(),
                    model,
                    conversation_messages: heavy_state
                        .as_ref()
                        .map(|heavy| heavy.messages.clone())
                        .unwrap_or_default(),
                    blocked_tools: heavy_state
                        .as_ref()
                        .map(|heavy| heavy.blocked_tools.clone())
                        .unwrap_or_default(),
                    approval_overrides: heavy_state
                        .as_ref()
                        .and_then(|heavy| heavy.approval_overrides.clone()),
                    interruption: heavy_state
                        .as_ref()
                        .and_then(|heavy| heavy.interruption.clone()),
                    compaction_state: heavy_state
                        .as_ref()
                        .and_then(|heavy| heavy.compaction_state.clone()),
                    executing_plan_json: metadata_state.executing_plan_json,
                    plan_goal: metadata_state.plan_goal,
                    plan_config_json: metadata_state.plan_config_json,
                    plan_execution_rounds: metadata_state.plan_execution_rounds,
                    contract_json,
                    last_context_trace,
                    ..Default::default()
                }))
            }
            None => Ok(None),
        }
    }

    /// Load the active contract for this session from cloud task_contracts table.
    /// Returns the contract as serialized JSON (matching local workspace format).
    async fn load_cloud_contract(
        pool: &sqlx::Pool<sqlx::MySql>,
        session_id: &str,
    ) -> Result<Option<String>, String> {
        let row = sqlx::query(
            "SELECT contract_id, task_id, goal, \
             CAST(scope_json AS CHAR) AS scope_json, \
             CAST(subtasks_json AS CHAR) AS subtasks_json, \
             CAST(criteria_json AS CHAR) AS criteria_json, \
             version, status, \
             CAST(created_at AS CHAR) AS created_at, \
             CAST(updated_at AS CHAR) AS updated_at \
             FROM task_contracts \
             WHERE session_id = ? AND status = 'active' \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("load_cloud_contract: {e}"))?;

        match row {
            Some(row) => {
                use sqlx::Row;
                // Reconstruct contract as JSON matching TaskContract serde format
                let contract_id: String = row.try_get("contract_id").map_err(|e| e.to_string())?;
                let task_id: String = row.try_get("task_id").map_err(|e| e.to_string())?;
                let goal: String = row.try_get("goal").map_err(|e| e.to_string())?;
                let version: i32 = row.try_get("version").unwrap_or(1);
                let status: String = row.try_get("status").unwrap_or_default();
                let created_at: String = row.try_get("created_at").unwrap_or_default();
                let updated_at: String = row.try_get("updated_at").unwrap_or_default();
                let scope_json: Option<String> = row.try_get("scope_json").ok().flatten();
                let subtasks_json: String =
                    row.try_get("subtasks_json").map_err(|e| e.to_string())?;
                let criteria_json: String =
                    row.try_get("criteria_json").map_err(|e| e.to_string())?;

                // Parse sub-objects so serde round-trips correctly
                let scope: serde_json::Value = scope_json
                    .as_deref()
                    .and_then(|j| serde_json::from_str(j).ok())
                    .unwrap_or(serde_json::json!({}));
                let subtasks: serde_json::Value = serde_json::from_str(&subtasks_json)
                    .map_err(|e| format!("parse subtasks: {e}"))?;
                let criteria: serde_json::Value = serde_json::from_str(&criteria_json)
                    .map_err(|e| format!("parse criteria: {e}"))?;

                let contract = serde_json::json!({
                    "contract_id": contract_id,
                    "task_id": task_id,
                    "goal": goal,
                    "scope": scope,
                    "subtasks": subtasks,
                    "global_verification": criteria,
                    "version": version,
                    "status": status,
                    "created_at": created_at,
                    "updated_at": updated_at,
                });

                Ok(Some(contract.to_string()))
            }
            None => Ok(None),
        }
    }

    /// Restore recent tools from recent cloud checkpoints, with a legacy turn-complete fallback.
    async fn restore_recent_tools(&self, session_id: &str) -> Result<Vec<String>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        use sqlx::Row;

        let checkpoint_rows = sqlx::query(
            "SELECT CAST(tools_json AS CHAR) AS tools_json FROM session_checkpoints \
             WHERE session_id = ? AND state_json IS NULL \
             ORDER BY number DESC LIMIT 5",
        )
        .bind(session_id)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("restore_recent_tools: {e}"))?;

        let mut tools = Vec::new();
        for row in &checkpoint_rows {
            if let Ok(Some(tools_json)) = row.try_get::<Option<String>, _>("tools_json")
                && let Ok(used) = serde_json::from_str::<Vec<String>>(&tools_json)
            {
                append_unique_tools(&mut tools, used.iter().map(String::as_str));
            }
        }

        if !tools.is_empty() {
            return Ok(tools);
        }

        let legacy_rows = sqlx::query(
            "SELECT CAST(metadata AS CHAR) AS metadata_json FROM agent_events \
             WHERE session_id = ? AND event_type = 'turn_complete' \
             ORDER BY created_at DESC LIMIT 5",
        )
        .bind(session_id)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("restore_recent_tools: {e}"))?;

        for row in &legacy_rows {
            if let Ok(Some(meta_str)) = row.try_get::<Option<String>, _>("metadata_json")
                && let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta_str)
                && let Some(used) = meta.get("tools_used").and_then(|v| v.as_array())
            {
                append_unique_tools(&mut tools, used.iter().filter_map(|tool| tool.as_str()));
            }
        }
        Ok(tools)
    }

    /// Restore the latest structured context-trace signal from cloud events.
    async fn restore_latest_context_trace_signal(
        &self,
        session_id: &str,
    ) -> Result<Option<super::session_workspace::ContextTraceSignal>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(None),
        };

        let row = sqlx::query(
            "SELECT CAST(metadata AS CHAR) AS metadata_json FROM agent_events \
             WHERE session_id = ? AND event_type = 'context_trace_signal' \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("restore_latest_context_trace_signal: {e}"))?;

        use sqlx::Row;
        Ok(row.and_then(|row| {
            row.try_get::<Option<String>, _>("metadata_json")
                .ok()
                .flatten()
                .and_then(|meta| serde_json::from_str(&meta).ok())
        }))
    }

    /// Pull learning snapshot from MatrixOne.
    async fn restore_learning(
        &self,
        user_id: &str,
        profile: &str,
    ) -> Result<Option<String>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(None),
        };

        let row = sqlx::query(
            "SELECT snapshot_json FROM learning_snapshots \
             WHERE user_id = ? AND profile_name = ? \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(profile)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("restore_learning: {e}"))?;

        match row {
            Some(row) => {
                use sqlx::Row;
                let json: String = row
                    .try_get("snapshot_json")
                    .map_err(|e| format!("decode learning: {e}"))?;
                Ok(Some(json))
            }
            None => Ok(None),
        }
    }

    async fn restore_latest_heavy_checkpoint_state(
        &self,
        session_id: &str,
    ) -> Result<Option<CloudHeavyCheckpointState>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(None),
        };

        let state_json = pull_step_checkpoint_from_cloud(pool, session_id).await?;
        Ok(state_json
            .as_deref()
            .and_then(parse_cloud_heavy_checkpoint_state))
    }

    /// List checkpoints from MatrixOne.
    async fn cloud_checkpoints(&self, session_id: &str) -> Result<Vec<RestoredCheckpoint>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        let rows = sqlx::query(
            "SELECT number, turn, title, summary, total_tokens, contract_state_json \
             FROM session_checkpoints \
             WHERE session_id = ? AND state_json IS NULL \
             ORDER BY number",
        )
        .bind(session_id)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("cloud_checkpoints: {e}"))?;

        use sqlx::Row;
        let ckpts = rows
            .iter()
            .filter_map(|row| {
                Some(RestoredCheckpoint {
                    number: row.try_get::<i32, _>("number").ok()? as u32,
                    turn: row.try_get::<i32, _>("turn").ok()? as u32,
                    title: row.try_get("title").ok()?,
                    summary: row.try_get("summary").unwrap_or_default(),
                    total_tokens: row.try_get::<i64, _>("total_tokens").unwrap_or(0) as u64,
                    contract_state_json: row
                        .try_get::<Option<String>, _>("contract_state_json")
                        .ok()
                        .flatten(),
                })
            })
            .collect();
        Ok(ckpts)
    }
}

/// Reads `composite_snapshots.json` from the session step-checkpoint directory.
///
/// Must stay aligned with `astra_runtime::pipeline::step_checkpoint::read_composite_snapshot_index`
/// (same path on disk). The services crate cannot depend on runtime due to a dependency cycle.
fn read_composite_snapshot_index_local(
    session_id: &str,
) -> Result<astra_core::composite_snapshot::CompositeSnapshotIndex, String> {
    let path = composite_snapshots_json_path(session_id);
    if !path.exists() {
        return Ok(astra_core::composite_snapshot::CompositeSnapshotIndex::default());
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("read composite_snapshots.json: {e}"))?;
    let mut index: astra_core::composite_snapshot::CompositeSnapshotIndex =
        serde_json::from_str(&content)
            .map_err(|e| format!("parse composite_snapshots.json: {e}"))?;
    index.normalize_versions();
    Ok(index)
}

fn composite_snapshots_json_path(session_id: &str) -> PathBuf {
    crate::session_journal::local_sessions_dir()
        .join(session_id)
        .join("step_checkpoints")
        .join("composite_snapshots.json")
}

/// Parse checkpoint number from a heavy checkpoint filename ref (e.g. `000005-heavy.json`).
fn parse_heavy_checkpoint_number(session_state_ref: &str) -> Option<u32> {
    session_state_ref
        .strip_suffix("-heavy.json")
        .and_then(|prefix| prefix.parse().ok())
}

fn append_unique_tools<'a>(tools: &mut Vec<String>, candidates: impl IntoIterator<Item = &'a str>) {
    for name in candidates {
        if !tools.iter().any(|existing| existing == name) {
            tools.push(name.to_string());
        }
    }
}

fn recent_tools_from_context_trace(
    trace: Option<&super::session_workspace::ContextTraceSignal>,
) -> Vec<String> {
    let mut tools = Vec::new();
    if let Some(selected_tools) = trace
        .and_then(|signal| signal.tool_selection.as_ref())
        .map(|selection| selection.selected_tools.iter().map(String::as_str))
    {
        append_unique_tools(&mut tools, selected_tools);
    }
    tools
}

#[derive(Debug, Clone, Default)]
struct LocalJournalSummary {
    turn_count: u32,
    total_tokens_in: u64,
    total_tokens_out: u64,
    recent_tools: Vec<String>,
    model: Option<String>,
    last_status: String,
}

fn summarize_local_journal(session_id: &str) -> Option<LocalJournalSummary> {
    let (events, _, _) = crate::session_journal::read_journal_for_digest(session_id).ok()?;
    if events.is_empty() {
        return None;
    }

    let mut summary = LocalJournalSummary {
        last_status: "local".to_string(),
        ..Default::default()
    };
    let mut latest_turn_index: Option<usize> = None;

    for (idx, event) in events.iter().enumerate() {
        match event.event_type {
            crate::session_journal::JournalEventType::Turn => {
                summary.turn_count += 1;
                summary.total_tokens_in += event.tokens_in.unwrap_or(0);
                summary.total_tokens_out += event.tokens_out.unwrap_or(0);
                if event.model.is_some() {
                    summary.model = event.model.clone();
                }
                latest_turn_index = Some(idx);
            }
            crate::session_journal::JournalEventType::SessionStart if summary.model.is_none() => {
                summary.model = event.model.clone();
            }
            crate::session_journal::JournalEventType::SessionEnd => {
                summary.last_status = "completed".to_string();
            }
            _ => {}
        }
    }

    if let Some(idx) = latest_turn_index
        && let Some(event) = events.get(idx)
    {
        if let Some(tools_used) = event.tools_used.as_ref() {
            append_unique_tools(
                &mut summary.recent_tools,
                tools_used.iter().map(String::as_str),
            );
        }
        if summary.recent_tools.is_empty()
            && let Some(tool_calls) = event.tool_calls.as_ref()
        {
            append_unique_tools(
                &mut summary.recent_tools,
                tool_calls.iter().map(|call| call.name.as_str()),
            );
        }
    }

    Some(summary)
}

fn cloud_heavy_payload(root: &serde_json::Value) -> Option<&serde_json::Value> {
    if let Some(heavy) = root.get("Heavy") {
        return Some(heavy);
    }
    if root.get("messages").is_some()
        || root.get("blocked_tools").is_some()
        || root.get("recent_tools").is_some()
        || root.get("approval_overrides").is_some()
        || root.get("interruption").is_some()
        || root.get("compaction_state").is_some()
    {
        return Some(root);
    }
    None
}

/// Parse cloud step-checkpoint JSON into the heavy-state fields needed for restore.
/// Accepts both the current externally tagged `{"Heavy": ...}` shape and legacy
/// rows that stored the heavy payload unwrapped.
pub fn parse_cloud_heavy_checkpoint_state(state_json: &str) -> Option<CloudHeavyCheckpointState> {
    let root = serde_json::from_str::<serde_json::Value>(state_json).ok()?;
    let heavy = cloud_heavy_payload(&root)?;
    Some(CloudHeavyCheckpointState {
        messages: heavy
            .get("messages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        blocked_tools: heavy
            .get("blocked_tools")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default(),
        recent_tools: heavy
            .get("recent_tools")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default(),
        approval_overrides: heavy
            .get("approval_overrides")
            .cloned()
            .filter(|v| !v.is_null()),
        interruption: heavy.get("interruption").cloned().filter(|v| !v.is_null()),
        compaction_state: heavy
            .get("compaction_state")
            .cloned()
            .filter(|v| !v.is_null()),
    })
}

#[async_trait]
impl SessionRestoreService for HybridRestoreService {
    async fn restore_session(&self, session_id: &str) -> Result<Option<RestoredSession>, String> {
        // Step 1: Try local workspace metadata first
        let local_journal = summarize_local_journal(session_id);
        if let Some(ws) = self.restore_local_workspace(session_id) {
            let mut recent_tools = if self.pool.is_some() {
                self.restore_recent_tools(session_id)
                    .await
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            if recent_tools.is_empty() {
                recent_tools = recent_tools_from_context_trace(ws.last_context_trace.as_ref());
            }
            if recent_tools.is_empty()
                && let Some(summary) = local_journal.as_ref()
            {
                recent_tools = summary.recent_tools.clone();
            }

            // Try local learning file
            let learning = self
                .restore_learning("local", "default")
                .await
                .unwrap_or(None);

            let ckpt_count = super::session_checkpoint::read_checkpoint_index(session_id)
                .map(|v| v.len() as u32)
                .unwrap_or(0);

            return Ok(Some(RestoredSession {
                session_id: session_id.to_string(),
                turn_count: local_journal.as_ref().map_or(ws.turn_count, |summary| {
                    ws.turn_count.max(summary.turn_count)
                }),
                total_tokens_in: local_journal
                    .as_ref()
                    .map_or(ws.total_tokens_in, |summary| {
                        ws.total_tokens_in.max(summary.total_tokens_in)
                    }),
                total_tokens_out: local_journal
                    .as_ref()
                    .map_or(ws.total_tokens_out, |summary| {
                        ws.total_tokens_out.max(summary.total_tokens_out)
                    }),
                recent_tools,
                learning_snapshot_json: learning,
                checkpoint_count: ckpt_count,
                last_status: ws.status,
                git_branch: ws.git_branch,
                model: Some(ws.model),
                title: None,
                restored_from_cloud: false,
                executing_plan_json: ws.executing_plan_json,
                plan_goal: ws.plan_goal,
                plan_config_json: ws.plan_config_json,
                plan_execution_rounds: ws.plan_execution_rounds,
                contract_json: ws.contract_json,
                plan_corrections: ws.plan_corrections,
                last_context_trace: ws.last_context_trace,
                ..Default::default()
            }));
        }

        if let Some(summary) = local_journal {
            let learning = self
                .restore_learning("local", "default")
                .await
                .unwrap_or(None);
            let ckpt_count = super::session_checkpoint::read_checkpoint_index(session_id)
                .map(|v| v.len() as u32)
                .unwrap_or(0);

            return Ok(Some(RestoredSession {
                session_id: session_id.to_string(),
                turn_count: summary.turn_count,
                total_tokens_in: summary.total_tokens_in,
                total_tokens_out: summary.total_tokens_out,
                recent_tools: summary.recent_tools,
                learning_snapshot_json: learning,
                checkpoint_count: ckpt_count,
                last_status: summary.last_status,
                model: summary.model,
                restored_from_cloud: false,
                ..Default::default()
            }));
        }

        // Step 2: Fall back to MatrixOne
        self.restore_cloud_session(session_id).await
    }

    async fn list_checkpoints(&self, session_id: &str) -> Result<Vec<RestoredCheckpoint>, String> {
        // Try local first
        let local_entries = super::session_checkpoint::read_checkpoint_index(session_id)
            .unwrap_or_else(|e| {
                astra_core::agent_warn!(
                    "restore",
                    "failed to read checkpoint index for {session_id}: {e}"
                );
                Vec::new()
            });

        if !local_entries.is_empty() {
            // Parse the index entries (format: "NNN - Turn NN - title")
            let ckpts = local_entries
                .iter()
                .filter_map(|entry| {
                    let parts: Vec<&str> = entry.splitn(3, " - ").collect();
                    if parts.len() >= 3 {
                        let number: u32 = parts[0].trim().parse().ok()?;
                        let turn_str = parts[1].trim().strip_prefix("Turn ")?.trim();
                        let turn: u32 = turn_str.parse().ok()?;
                        let title = parts[2].trim().to_string();
                        Some(RestoredCheckpoint {
                            number,
                            turn,
                            title,
                            summary: String::new(),
                            total_tokens: 0,
                            contract_state_json: None,
                        })
                    } else {
                        None
                    }
                })
                .collect();
            return Ok(ckpts);
        }

        // Fall back to cloud
        self.cloud_checkpoints(session_id).await
    }

    async fn restore_to_checkpoint(
        &self,
        session_id: &str,
        checkpoint_number: u32,
    ) -> Result<Option<RestoredSession>, String> {
        // First restore the full session
        let session = match self.restore_session(session_id).await? {
            Some(s) => s,
            None => return Ok(None),
        };

        // Find the checkpoint
        let checkpoints = self.list_checkpoints(session_id).await?;
        let target = checkpoints.iter().find(|c| c.number == checkpoint_number);

        match target {
            Some(ckpt) => {
                // Use contract state from checkpoint if the session doesn't have one
                let contract_json = session
                    .contract_json
                    .or_else(|| ckpt.contract_state_json.clone());

                Ok(Some(RestoredSession {
                    turn_count: ckpt.turn,
                    total_tokens_in: ckpt.total_tokens,
                    total_tokens_out: 0,
                    checkpoint_count: checkpoint_number,
                    contract_json,
                    ..session
                }))
            }
            None => Err(format!(
                "checkpoint {} not found for session {}",
                checkpoint_number, session_id
            )),
        }
    }

    async fn list_resumable_sessions(&self, user_id: &str) -> Result<Vec<RestoredSession>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        let rows = sqlx::query(
            "SELECT s.session_id, s.title, s.status, CAST(s.metadata AS CHAR) AS metadata_json, \
         (SELECT COUNT(*) FROM agent_events WHERE session_id = s.session_id AND event_type = 'user_query') AS turn_count, \
         (SELECT e.llm_model_used FROM agent_events e WHERE e.session_id = s.session_id AND e.llm_model_used IS NOT NULL AND e.llm_model_used != '' ORDER BY e.created_at DESC LIMIT 1) AS latest_model \
         FROM agent_sessions s \
         WHERE s.user_id = ? AND s.status IN ('active', 'paused') \
         ORDER BY s.updated_at DESC LIMIT 20",
        )
        .bind(user_id)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("list_resumable: {e}"))?;

        use sqlx::Row;
        let mut sessions = Vec::new();
        for row in &rows {
            let session_id: String = match row.try_get("session_id") {
                Ok(value) => value,
                Err(_) => continue,
            };
            let title: Option<String> = row.try_get("title").ok().flatten();
            let status: String = match row.try_get("status") {
                Ok(value) => value,
                Err(_) => continue,
            };
            let turn_count: i64 = row.try_get("turn_count").unwrap_or(0);
            let metadata_state = row
                .try_get::<Option<String>, _>("metadata_json")
                .ok()
                .flatten()
                .as_deref()
                .map(extract_session_state_from_metadata)
                .unwrap_or_default();
            let latest_model: Option<String> = row.try_get("latest_model").ok().flatten();
            let model = metadata_state.model.clone().or(latest_model);

            sessions.push(RestoredSession {
                session_id,
                turn_count: turn_count as u32,
                last_status: status,
                title,
                git_branch: metadata_state.git_branch.clone(),
                model,
                restored_from_cloud: true,
                ..Default::default()
            });
        }
        Ok(sessions)
    }

    async fn restore_to_composite_snapshot(
        &self,
        session_id: &str,
        snapshot_id: &str,
        selector: &astra_core::composite_snapshot::RestoreSelector,
    ) -> Result<Option<RestoredCompositeState>, String> {
        let index = read_composite_snapshot_index_local(session_id)?;
        let Some(snapshot) = index
            .snapshots
            .iter()
            .find(|s| s.snapshot_id == snapshot_id)
            .cloned()
        else {
            return Ok(None);
        };

        if snapshot.session_id != session_id {
            return Err(format!(
                "composite snapshot {} belongs to session {}, not {}",
                snapshot_id, snapshot.session_id, session_id
            ));
        }

        let mut restored_dimensions: Vec<String> = Vec::new();
        let mut session: Option<RestoredSession> = None;

        if selector.restore_session_state
            && let Some(ref_str) = snapshot.session_state()
            && let Some(ckpt_num) = parse_heavy_checkpoint_number(ref_str)
        {
            match self.restore_to_checkpoint(session_id, ckpt_num).await {
                Ok(Some(s)) => {
                    session = Some(s);
                    restored_dimensions.push("session".to_string());
                }
                Ok(None) => {}
                Err(e) => return Err(e),
            }
        }

        let data_snapshot_to_restore = if selector.restore_data {
            snapshot.data_snapshot().cloned()
        } else {
            None
        };

        let git_commit_to_checkout = if selector.restore_git {
            snapshot.git_commit().map(str::to_string)
        } else {
            None
        };

        Ok(Some(RestoredCompositeState {
            session,
            snapshot,
            restored_dimensions,
            data_snapshot_to_restore,
            git_commit_to_checkout,
        }))
    }

    async fn list_composite_snapshots(
        &self,
        session_id: &str,
    ) -> Result<astra_core::composite_snapshot::CompositeSnapshotIndex, String> {
        read_composite_snapshot_index_local(session_id)
    }
}

/// Push a checkpoint to MatrixOne for cross-device availability.
/// Also logs to session_sync_log for audit trail.
pub async fn push_checkpoint_to_cloud(
    pool: &sqlx::Pool<sqlx::MySql>,
    session_id: &str,
    user_id: &str,
    checkpoint: &super::session_checkpoint::Checkpoint,
) -> Result<(), String> {
    let checkpoint_id = uuid::Uuid::new_v4().to_string();
    let tools_json =
        serde_json::to_string(&checkpoint.tools_used).unwrap_or_else(|_| "[]".to_string());

    // Calculate payload_size for sync log (estimate based on serialized fields)
    let payload_size = checkpoint.title.len()
        + checkpoint.summary.len()
        + tools_json.len()
        + checkpoint
            .contract_state_json
            .as_ref()
            .map_or(0, |s| s.len());

    // Helper: log sync result (success or failure) and propagate result
    let log_and_return = |result: Result<(), String>, size: usize| async move {
        let (status, error_msg) = match &result {
            Ok(()) => ("success", None),
            Err(e) => ("error", Some(e.as_str())),
        };
        let _ = log_checkpoint_sync(
            pool,
            user_id,
            session_id,
            "checkpoint",
            checkpoint.number,
            size,
            status,
            error_msg,
        )
        .await;
        result
    };

    let updated = match sqlx::query(
        "UPDATE session_checkpoints SET \
            turn = ?, title = ?, summary = ?, tools_json = ?, total_tokens = ?, \
            had_stalls = ?, error_count = ?, contract_state_json = ? \
         WHERE session_id = ? AND number = ?",
    )
    .bind(checkpoint.turn as i32)
    .bind(&checkpoint.title)
    .bind(&checkpoint.summary)
    .bind(&tools_json)
    .bind(checkpoint.total_tokens as i64)
    .bind(if checkpoint.had_stalls { 1i32 } else { 0 })
    .bind(checkpoint.error_count as i32)
    .bind(&checkpoint.contract_state_json)
    .bind(session_id)
    .bind(checkpoint.number as i32)
    .execute(pool)
    .await
    {
        Ok(u) => u,
        Err(e) => {
            let err = format!("push_checkpoint update: {e}");
            return log_and_return(Err(err), payload_size).await;
        }
    };

    if updated.rows_affected() == 0 {
        let inserted = sqlx::query(
            "INSERT INTO session_checkpoints \
             (checkpoint_id, session_id, user_id, number, turn, title, summary, \
              tools_json, total_tokens, had_stalls, error_count, contract_state_json, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
        )
        .bind(&checkpoint_id)
        .bind(session_id)
        .bind(user_id)
        .bind(checkpoint.number as i32)
        .bind(checkpoint.turn as i32)
        .bind(&checkpoint.title)
        .bind(&checkpoint.summary)
        .bind(&tools_json)
        .bind(checkpoint.total_tokens as i64)
        .bind(if checkpoint.had_stalls { 1i32 } else { 0 })
        .bind(checkpoint.error_count as i32)
        .bind(&checkpoint.contract_state_json)
        .execute(pool)
        .await;

        if let Err(e) = inserted {
            if is_duplicate_key_error(&e) {
                if let Err(e) = sqlx::query(
                    "UPDATE session_checkpoints SET \
                        turn = ?, title = ?, summary = ?, tools_json = ?, total_tokens = ?, \
                        had_stalls = ?, error_count = ?, contract_state_json = ? \
                     WHERE session_id = ? AND number = ?",
                )
                .bind(checkpoint.turn as i32)
                .bind(&checkpoint.title)
                .bind(&checkpoint.summary)
                .bind(&tools_json)
                .bind(checkpoint.total_tokens as i64)
                .bind(if checkpoint.had_stalls { 1i32 } else { 0 })
                .bind(checkpoint.error_count as i32)
                .bind(&checkpoint.contract_state_json)
                .bind(session_id)
                .bind(checkpoint.number as i32)
                .execute(pool)
                .await
                {
                    let err = format!("push_checkpoint retry update: {e}");
                    return log_and_return(Err(err), payload_size).await;
                }
            } else {
                let err = format!("push_checkpoint insert: {e}");
                return log_and_return(Err(err), payload_size).await;
            }
        }
    }

    log_and_return(Ok(()), payload_size).await
}

/// Log session sync activity to session_sync_log for audit trail.
/// This helps diagnose sync issues like Session 7875e355 where cloud work reached
/// MatrixOne through some paths but left no sync-log breadcrumb.
async fn log_session_sync(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    sync_type: &str,
    payload_size: usize,
    status: &str,
    error_msg: Option<&str>,
) -> Result<(), String> {
    let inserted = sqlx::query(
        "INSERT INTO session_sync_log \
         (sync_id, user_id, session_id, sync_type, sync_direction, payload_size, status, error_message, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, NOW())",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(user_id)
    .bind(session_id)
    .bind(sync_type)
    .bind("push")
    .bind(payload_size as i64)
    .bind(status)
    .bind(error_msg)
    .execute(pool)
    .await
    .map_err(|e| format!("log_session_sync: {e}"))?;

    // Apply retention pruning using the same policy as state_sync.
    // This prevents unbounded sync_log growth for long-running sessions.
    if inserted.rows_affected() > 0
        && let Some(retain) = crate::state_sync::sync_log_retain_limit(status)
    {
        let _ = sqlx::query(crate::state_sync::build_sync_log_prune_query())
            .bind(user_id)
            .bind(status)
            .bind(sync_type)
            .bind(user_id)
            .bind(status)
            .bind(sync_type)
            .bind(retain as i64)
            .execute(pool)
            .await;
    }

    Ok(())
}

/// Log checkpoint sync with checkpoint-number context preserved in the error field.
#[allow(clippy::too_many_arguments)]
async fn log_checkpoint_sync(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    sync_type: &str,
    checkpoint_number: u32,
    payload_size: usize,
    status: &str,
    error_msg: Option<&str>,
) -> Result<(), String> {
    let error_with_number = error_msg.map(|e| format!("[checkpoint #{}] {}", checkpoint_number, e));
    log_session_sync(
        pool,
        user_id,
        session_id,
        sync_type,
        payload_size,
        status,
        error_with_number.as_deref().or(error_msg),
    )
    .await
}

/// Push a Step Protocol checkpoint to MatrixOne with full state_json.
/// Accepts pre-serialized JSON to avoid coupling services crate to runtime types.
/// The caller serializes the StepCheckpoint; this function stores it.
#[allow(clippy::too_many_arguments)]
pub async fn push_step_checkpoint_to_cloud(
    pool: &sqlx::Pool<sqlx::MySql>,
    session_id: &str,
    user_id: &str,
    checkpoint_number: u32,
    turn: u32,
    tier: &str,
    title: &str,
    tools_json: &str,
    state_json: &str,
) -> Result<(), String> {
    let checkpoint_id = uuid::Uuid::new_v4().to_string();
    let cloud_number = cloud_step_checkpoint_number(checkpoint_number)?;
    let payload_size = title.len() + tier.len() + tools_json.len() + state_json.len();
    let log_and_return = |result: Result<(), String>, size: usize| async move {
        let (status, error_msg) = match &result {
            Ok(()) => ("success", None),
            Err(e) => ("error", Some(e.as_str())),
        };
        let _ = log_checkpoint_sync(
            pool,
            user_id,
            session_id,
            "step_checkpoint",
            checkpoint_number,
            size,
            status,
            error_msg,
        )
        .await;
        result
    };

    let updated = match sqlx::query(
        "UPDATE session_checkpoints SET \
            turn = ?, title = ?, summary = ?, tools_json = ?, state_json = ? \
         WHERE session_id = ? AND number = ?",
    )
    .bind(turn as i32)
    .bind(title)
    .bind(tier)
    .bind(tools_json)
    .bind(state_json)
    .bind(session_id)
    .bind(cloud_number)
    .execute(pool)
    .await
    {
        Ok(updated) => updated,
        Err(e) => {
            let err = format!("push_step_checkpoint update: {e}");
            return log_and_return(Err(err), payload_size).await;
        }
    };

    if updated.rows_affected() == 0 {
        let inserted = sqlx::query(
            "INSERT INTO session_checkpoints \
             (checkpoint_id, session_id, user_id, number, turn, title, summary, \
              tools_json, state_json, total_tokens, had_stalls, error_count, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0, 0, NOW())",
        )
        .bind(&checkpoint_id)
        .bind(session_id)
        .bind(user_id)
        .bind(cloud_number)
        .bind(turn as i32)
        .bind(title)
        .bind(tier)
        .bind(tools_json)
        .bind(state_json)
        .execute(pool)
        .await;

        if let Err(e) = inserted {
            if is_duplicate_key_error(&e) {
                if let Err(err) = sqlx::query(
                    "UPDATE session_checkpoints SET \
                        turn = ?, title = ?, summary = ?, tools_json = ?, state_json = ? \
                     WHERE session_id = ? AND number = ?",
                )
                .bind(turn as i32)
                .bind(title)
                .bind(tier)
                .bind(tools_json)
                .bind(state_json)
                .bind(session_id)
                .bind(cloud_number)
                .execute(pool)
                .await
                {
                    let err = format!("push_step_checkpoint retry update: {err}");
                    return log_and_return(Err(err), payload_size).await;
                }
            } else {
                let err = format!("push_step_checkpoint insert: {e}");
                return log_and_return(Err(err), payload_size).await;
            }
        }
    }

    log_and_return(Ok(()), payload_size).await
}

/// Pull the latest Heavy step checkpoint JSON from MatrixOne for session recovery.
/// Returns the raw state_json string — caller deserializes to StepCheckpoint.
pub async fn pull_step_checkpoint_from_cloud(
    pool: &sqlx::Pool<sqlx::MySql>,
    session_id: &str,
) -> Result<Option<String>, String> {
    use sqlx::Row;

    let row = sqlx::query(
        "SELECT CAST(state_json AS CHAR) AS state_json_json FROM session_checkpoints \
         WHERE session_id = ? AND summary = 'heavy' AND state_json IS NOT NULL \
         ORDER BY number DESC LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("pull_step_checkpoint: {e}"))?;

    match row {
        Some(row) => {
            let json: String = row
                .try_get("state_json_json")
                .map_err(|e| format!("read state_json: {e}"))?;
            Ok(Some(json))
        }
        None => Ok(None),
    }
}

// ─── Plan State Cloud Sync ──────────────────────────────────────────────────

/// Push resumable session state to cloud via the agent_sessions.metadata JSON column.
/// Called at checkpoint boundaries and session end to enable cross-device restore.
#[allow(clippy::too_many_arguments)]
pub async fn push_session_state_to_cloud(
    pool: &sqlx::Pool<sqlx::MySql>,
    session_id: &str,
    user_id: &str,
    executing_plan_json: Option<&str>,
    plan_goal: Option<&str>,
    plan_config_json: Option<&str>,
    plan_execution_rounds: usize,
    git_branch: Option<&str>,
    model: Option<&str>,
) -> Result<(), String> {
    use sqlx::Row;

    let existing_metadata_json = sqlx::query(
        "SELECT CAST(metadata AS CHAR) AS metadata_json \
         FROM agent_sessions WHERE session_id = ? LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("load session metadata: {e}"))?
    .and_then(|row| {
        row.try_get::<Option<String>, _>("metadata_json")
            .ok()
            .flatten()
    });

    let metadata_json = merge_session_state_metadata(
        existing_metadata_json.as_deref(),
        executing_plan_json,
        plan_goal,
        plan_config_json,
        plan_execution_rounds,
        git_branch,
        model,
    );
    let payload_size = metadata_json.len();
    let log_and_return = |result: Result<(), String>, size: usize| async move {
        let (status, error_msg) = match &result {
            Ok(()) => ("success", None),
            Err(e) => ("error", Some(e.as_str())),
        };
        let _ = log_session_sync(
            pool,
            user_id,
            session_id,
            "session_state",
            size,
            status,
            error_msg,
        )
        .await;
        result
    };

    match sqlx::query(
        "INSERT INTO agent_sessions \
         (session_id, user_id, status, metadata, created_at, updated_at, last_active_at) \
         VALUES (?, ?, 'active', ?, NOW(), NOW(), NOW()) \
         ON DUPLICATE KEY UPDATE metadata = ?, updated_at = NOW(), last_active_at = NOW()",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(&metadata_json)
    .bind(&metadata_json)
    .execute(pool)
    .await
    {
        Ok(_) => log_and_return(Ok(()), payload_size).await,
        Err(e) => {
            let err = format!("push_session_state: {e}");
            log_and_return(Err(err), payload_size).await
        }
    }
}

/// Push a structured context-trace signal as a first-class cloud event.
pub async fn push_context_trace_signal_to_cloud(
    pool: &sqlx::Pool<sqlx::MySql>,
    session_id: &str,
    user_id: &str,
    signal: &super::session_workspace::ContextTraceSignal,
) -> Result<(), String> {
    let metadata_json = serde_json::to_string(signal)
        .map_err(|e| format!("serialize context_trace_signal: {e}"))?;
    let duration_ms = signal
        .timing
        .as_ref()
        .map(|timing| timing.total_ms.min(i32::MAX as u64) as i32);
    let content = {
        let preview = signal.preview();
        if preview.is_empty() {
            "context trace signal".to_string()
        } else {
            preview
        }
    };
    let payload_size = metadata_json.len() + content.len();
    let log_and_return = |result: Result<(), String>, size: usize| async move {
        let (status, error_msg) = match &result {
            Ok(()) => ("success", None),
            Err(e) => ("error", Some(e.as_str())),
        };
        let _ = log_session_sync(
            pool,
            user_id,
            session_id,
            "context_trace",
            size,
            status,
            error_msg,
        )
        .await;
        result
    };

    if let Err(e) = sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, agent_id, agent_version, event_type, content, \
          parent_event_id, causal_chain_id, metadata, reasoning_content, meta_tool_name, \
          meta_duration_ms, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(session_id)
    .bind(user_id)
    .bind("astra-cli")
    .bind(env!("CARGO_PKG_VERSION"))
    .bind("context_trace_signal")
    .bind(content)
    .bind(None::<String>)
    .bind(&signal.turn_id)
    .bind(metadata_json)
    .bind(None::<String>)
    .bind(
        signal
            .tool_selection
            .as_ref()
            .and_then(|selection| selection.selected_tools.first().cloned()),
    )
    .bind(duration_ms)
    .execute(pool)
    .await
    {
        let err = format!("push_context_trace_signal: {e}");
        return log_and_return(Err(err), payload_size).await;
    }

    if let Err(e) = reconcile_session_event_count(pool, session_id, user_id).await {
        return log_and_return(Err(e), payload_size).await;
    }

    log_and_return(Ok(()), payload_size).await
}

async fn reconcile_session_event_count(
    pool: &sqlx::Pool<sqlx::MySql>,
    session_id: &str,
    user_id: &str,
) -> Result<(), String> {
    let event_count = crate::storage::load_agent_event_count(pool, session_id)
        .await
        .map_err(|e| format!("reconcile_session_event_count load: {e}"))?;
    crate::storage::upsert_agent_session_event_count(pool, session_id, user_id, event_count)
        .await
        .map_err(|e| format!("reconcile_session_event_count: {e}"))?;
    Ok(())
}

fn cloud_step_checkpoint_number(checkpoint_number: u32) -> Result<i32, String> {
    let namespaced = STEP_CHECKPOINT_NUMBER_OFFSET
        .checked_add(checkpoint_number)
        .ok_or_else(|| format!("step checkpoint number overflow: {checkpoint_number}"))?;
    i32::try_from(namespaced)
        .map_err(|_| format!("step checkpoint number out of range: {namespaced}"))
}

fn merge_session_state_metadata(
    existing_metadata_json: Option<&str>,
    executing_plan_json: Option<&str>,
    plan_goal: Option<&str>,
    plan_config_json: Option<&str>,
    plan_execution_rounds: usize,
    git_branch: Option<&str>,
    model: Option<&str>,
) -> String {
    let mut metadata = existing_metadata_json
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();

    if let Some(plan) = executing_plan_json {
        metadata.insert(
            "executing_plan".to_string(),
            serde_json::Value::String(plan.to_string()),
        );
    } else {
        metadata.remove("executing_plan");
    }

    if let Some(goal) = plan_goal {
        metadata.insert(
            "plan_goal".to_string(),
            serde_json::Value::String(goal.to_string()),
        );
    } else {
        metadata.remove("plan_goal");
    }

    if let Some(config) = plan_config_json {
        metadata.insert(
            "plan_config".to_string(),
            serde_json::Value::String(config.to_string()),
        );
    } else {
        metadata.remove("plan_config");
    }

    if plan_execution_rounds > 0 {
        metadata.insert(
            "plan_execution_rounds".to_string(),
            serde_json::Value::Number(serde_json::Number::from(plan_execution_rounds)),
        );
    } else {
        metadata.remove("plan_execution_rounds");
    }

    if let Some(branch) = git_branch {
        metadata.insert(
            "git_branch".to_string(),
            serde_json::Value::String(branch.to_string()),
        );
    } else {
        metadata.remove("git_branch");
    }

    if let Some(model) = model {
        metadata.insert(
            "model".to_string(),
            serde_json::Value::String(model.to_string()),
        );
    } else {
        metadata.remove("model");
    }

    serde_json::Value::Object(metadata).to_string()
}

/// Extract plan state from the metadata JSON returned by agent_sessions.
/// Returns (executing_plan_json, plan_goal, plan_config_json, plan_execution_rounds).
pub fn extract_session_state_from_metadata(metadata_json: &str) -> SessionMetadataState {
    // Defense: reject excessively large metadata to prevent DoS
    const MAX_METADATA_SIZE: usize = 512 * 1024; // 512 KB
    if metadata_json.len() > MAX_METADATA_SIZE {
        eprintln!(
            "[WARN] session metadata too large ({} bytes), skipping plan extraction",
            metadata_json.len()
        );
        return SessionMetadataState::default();
    }
    let parsed: serde_json::Value = match serde_json::from_str(metadata_json) {
        Ok(v) => v,
        Err(_) => return SessionMetadataState::default(),
    };
    let obj = match parsed.as_object() {
        Some(o) => o,
        None => return SessionMetadataState::default(),
    };

    SessionMetadataState {
        executing_plan_json: obj
            .get("executing_plan")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        plan_goal: obj
            .get("plan_goal")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        plan_config_json: obj
            .get("plan_config")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        plan_execution_rounds: obj
            .get("plan_execution_rounds")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize,
        git_branch: obj
            .get("git_branch")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        model: obj
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    }
}

pub fn extract_plan_from_metadata(
    metadata_json: &str,
) -> (Option<String>, Option<String>, Option<String>, usize) {
    let state = extract_session_state_from_metadata(metadata_json);
    (
        state.executing_plan_json,
        state.plan_goal,
        state.plan_config_json,
        state.plan_execution_rounds,
    )
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_journal::JournalDirGuard;
    use crate::session_workspace;

    const REAL_SESSION_0AC769_FIXTURE: &str =
        include_str!("../fixtures/real_session_0ac769_min.jsonl");

    // ── RestoredSession ──

    #[test]
    fn restored_session_defaults() {
        let s = RestoredSession::default();
        assert_eq!(s.turn_count, 0);
        assert!(s.recent_tools.is_empty());
        assert!(s.learning_snapshot_json.is_none());
        assert!(!s.restored_from_cloud);
        assert!(s.last_context_trace.is_none());
    }

    #[test]
    fn restored_session_json_roundtrip() {
        let s = RestoredSession {
            session_id: "sess-1".into(),
            turn_count: 15,
            total_tokens_in: 5000,
            total_tokens_out: 3000,
            recent_tools: vec!["git_status".into(), "grep".into()],
            learning_snapshot_json: Some("{\"entities\":[]}".into()),
            checkpoint_count: 3,
            last_status: "active".into(),
            git_branch: Some("main".into()),
            model: Some("gpt-4".into()),
            title: Some("Refactor session".into()),
            restored_from_cloud: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let loaded: RestoredSession = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.session_id, "sess-1");
        assert_eq!(loaded.turn_count, 15);
        assert_eq!(loaded.recent_tools.len(), 2);
        assert!(loaded.restored_from_cloud);
    }

    // ── RestoredCheckpoint ──

    #[test]
    fn restored_checkpoint_json_roundtrip() {
        let c = RestoredCheckpoint {
            number: 3,
            turn: 15,
            title: "Phase A complete".into(),
            summary: "Finished token efficiency work".into(),
            total_tokens: 50000,
            contract_state_json: None,
        };
        let json = serde_json::to_string(&c).unwrap();
        let loaded: RestoredCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.number, 3);
        assert_eq!(loaded.total_tokens, 50000);
    }

    // ── HybridRestoreService (local-only mode) ──

    #[tokio::test]
    async fn local_only_restore_nonexistent_returns_none() {
        let svc = HybridRestoreService::local_only();
        let result = svc.restore_session("nonexistent-session").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn local_only_list_checkpoints_empty() {
        let svc = HybridRestoreService::local_only();
        let ckpts = svc.list_checkpoints("nonexistent-session").await.unwrap();
        assert!(ckpts.is_empty());
    }

    #[tokio::test]
    async fn local_only_list_resumable_empty() {
        let svc = HybridRestoreService::local_only();
        let sessions = svc.list_resumable_sessions("user1").await.unwrap();
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn restore_to_nonexistent_checkpoint_errors() {
        let svc = HybridRestoreService::local_only();
        let result = svc.restore_to_checkpoint("nonexistent", 5).await;
        // Either None (session not found) or Error (checkpoint not found)
        match result {
            Ok(None) => {} // session not found → ok
            Err(_) => {}   // checkpoint not found → ok
            Ok(Some(_)) => panic!("should not restore from nonexistent"),
        }
    }

    // ── Integration-style test with real workspace ──

    #[tokio::test]
    async fn restore_from_local_workspace() {
        // This test depends on whether workspace files exist
        // Just verify the function doesn't panic and returns a valid type
        let svc = HybridRestoreService::local_only();

        // Use a UUID that doesn't exist → should return None
        let result = svc
            .restore_session("00000000-0000-0000-0000-000000000000")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn local_only_restore_falls_back_to_real_session_journal_without_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "0ac7696c-8a67-4e9f-b7bb-88b3bf7b59a0";
        std::fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
            REAL_SESSION_0AC769_FIXTURE,
        )
        .unwrap();

        let svc = HybridRestoreService::local_only();
        let restored = svc
            .restore_session(sid)
            .await
            .unwrap()
            .expect("journal-only session should restore");

        assert_eq!(restored.session_id, sid);
        assert_eq!(restored.turn_count, 1);
        assert_eq!(restored.total_tokens_in, 33_659);
        assert_eq!(restored.total_tokens_out, 2_855);
        assert_eq!(restored.recent_tools, vec!["git_show", "read_file", "grep"]);
        assert_eq!(restored.model.as_deref(), Some("glm-5.1"));
        assert_eq!(restored.last_status, "completed");
        assert!(!restored.restored_from_cloud);
    }

    #[tokio::test]
    async fn local_restore_uses_real_session_journal_tools_when_workspace_has_no_trace() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "0ac7696c-8a67-4e9f-b7bb-88b3bf7b59a0";
        std::fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
            REAL_SESSION_0AC769_FIXTURE,
        )
        .unwrap();

        let ws = session_workspace::WorkspaceMetadata::with_context(sid, "glm-5.1", "/repo", None);
        session_workspace::write_workspace(&ws).unwrap();

        let svc = HybridRestoreService::local_only();
        let restored = svc
            .restore_session(sid)
            .await
            .unwrap()
            .expect("workspace-backed session should restore");

        assert_eq!(restored.recent_tools, vec!["git_show", "read_file", "grep"]);
        assert_eq!(restored.turn_count, 1);
        assert_eq!(restored.total_tokens_in, 33_659);
        assert_eq!(restored.total_tokens_out, 2_855);
    }

    // ── Restored session field coverage ──

    #[test]
    fn restored_session_cloud_flag_distinguishes_source() {
        let local = RestoredSession {
            restored_from_cloud: false,
            ..Default::default()
        };
        let cloud = RestoredSession {
            restored_from_cloud: true,
            ..Default::default()
        };
        assert!(!local.restored_from_cloud);
        assert!(cloud.restored_from_cloud);
    }

    #[test]
    fn restored_checkpoint_ordering() {
        let ckpts = [
            RestoredCheckpoint {
                number: 1,
                turn: 5,
                title: "First".into(),
                summary: String::new(),
                total_tokens: 1000,
                contract_state_json: None,
            },
            RestoredCheckpoint {
                number: 2,
                turn: 10,
                title: "Second".into(),
                summary: String::new(),
                total_tokens: 3000,
                contract_state_json: None,
            },
        ];
        assert!(ckpts[0].turn < ckpts[1].turn);
        assert!(ckpts[0].total_tokens < ckpts[1].total_tokens);
    }

    #[tokio::test]
    async fn local_only_restore_to_checkpoint_session_not_found() {
        let svc = HybridRestoreService::local_only();
        // Session doesn't exist → returns Ok(None)
        let result = svc.restore_to_checkpoint("nonexistent-session-id", 1).await;
        assert!(matches!(result, Ok(None)));
    }

    // ── Restore field completeness ──

    #[test]
    fn restored_session_all_fields_populated() {
        let s = RestoredSession {
            session_id: "s-full".into(),
            turn_count: 42,
            total_tokens_in: 100_000,
            total_tokens_out: 80_000,
            recent_tools: vec!["bash".into(), "grep".into(), "read_file".into()],
            learning_snapshot_json: Some(r#"{"entities":["Rust","MatrixOne"]}"#.into()),
            checkpoint_count: 5,
            last_status: "active".into(),
            git_branch: Some("feature/resume".into()),
            model: Some("claude-3".into()),
            title: Some("Implement session resume".into()),
            restored_from_cloud: false,
            ..Default::default()
        };
        // Verify every field survives serialization
        let json = serde_json::to_string(&s).unwrap();
        let loaded: RestoredSession = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.session_id, "s-full");
        assert_eq!(loaded.turn_count, 42);
        assert_eq!(loaded.total_tokens_in, 100_000);
        assert_eq!(loaded.total_tokens_out, 80_000);
        assert_eq!(loaded.recent_tools.len(), 3);
        assert!(loaded.learning_snapshot_json.is_some());
        assert_eq!(loaded.checkpoint_count, 5);
        assert_eq!(loaded.last_status, "active");
        assert_eq!(loaded.git_branch.as_deref(), Some("feature/resume"));
        assert_eq!(loaded.model.as_deref(), Some("claude-3"));
        assert_eq!(loaded.title.as_deref(), Some("Implement session resume"));
        assert!(!loaded.restored_from_cloud);
    }

    #[test]
    fn restored_session_partial_fields_default_safely() {
        // Simulate a cloud restore with minimal data
        let s = RestoredSession {
            session_id: "s-partial".into(),
            turn_count: 3,
            last_status: "active".into(),
            restored_from_cloud: true,
            ..Default::default()
        };
        assert!(s.recent_tools.is_empty());
        assert!(s.learning_snapshot_json.is_none());
        assert!(s.git_branch.is_none());
        assert!(s.model.is_none());
        assert!(s.title.is_none());
        assert_eq!(s.total_tokens_in, 0);
        assert_eq!(s.total_tokens_out, 0);
        assert_eq!(s.checkpoint_count, 0);
    }

    // ── HybridRestoreService local_only behavior ──

    #[tokio::test]
    async fn local_only_learning_restore_returns_none() {
        let svc = HybridRestoreService::local_only();
        let result = svc.restore_learning("user1", "default").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn local_only_recent_tools_returns_empty() {
        let svc = HybridRestoreService::local_only();
        let tools = svc.restore_recent_tools("nonexistent").await.unwrap();
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn local_only_cloud_checkpoints_returns_empty() {
        let svc = HybridRestoreService::local_only();
        let ckpts = svc.cloud_checkpoints("nonexistent").await.unwrap();
        assert!(ckpts.is_empty());
    }

    // ── Checkpoint convergence ──

    #[test]
    fn checkpoint_fields_for_cloud_push() {
        // Verify Checkpoint struct has all fields needed by push_checkpoint_to_cloud
        let ckpt = crate::session_checkpoint::Checkpoint {
            number: 3,
            turn: 15,
            title: "Phase A done".into(),
            summary: "Token efficiency implemented".into(),
            tools_used: vec!["bash".into(), "grep".into()],
            total_tokens: 50_000,
            had_stalls: true,
            error_count: 1,
            contract_state_json: Some(r#"{"contract_id":"c1"}"#.to_string()),
        };
        assert_eq!(ckpt.number, 3);
        assert_eq!(ckpt.turn, 15);
        assert!(ckpt.had_stalls);
        assert_eq!(ckpt.error_count, 1);
        let tools_json = serde_json::to_string(&ckpt.tools_used).unwrap();
        assert!(tools_json.contains("bash"));
    }

    #[test]
    fn restored_checkpoint_covers_rewind_fields() {
        let ckpt = RestoredCheckpoint {
            number: 7,
            turn: 35,
            title: "Checkpoint after refactor".into(),
            summary: "All tests passing after auth refactor".into(),
            total_tokens: 120_000,
            contract_state_json: None,
        };
        // Verify the fields needed for /rewind from cloud
        assert_eq!(ckpt.number, 7);
        assert_eq!(ckpt.turn, 35);
        assert!(!ckpt.title.is_empty());
        assert!(!ckpt.summary.is_empty());
        assert!(ckpt.total_tokens > 0);
    }

    // ── restore_to_checkpoint semantics ──

    #[test]
    fn restored_session_rewind_preserves_identity() {
        // Simulate what restore_to_checkpoint does: rewind turn but keep session_id
        let original = RestoredSession {
            session_id: "s-rewind".into(),
            turn_count: 20,
            total_tokens_in: 80_000,
            model: Some("gpt-4".into()),
            ..Default::default()
        };
        let rewound = RestoredSession {
            turn_count: 10,
            total_tokens_in: 40_000,
            checkpoint_count: 3,
            ..original.clone()
        };
        assert_eq!(rewound.session_id, "s-rewind");
        assert_eq!(rewound.model, Some("gpt-4".into()));
        assert_eq!(rewound.turn_count, 10);
        assert!(rewound.turn_count < original.turn_count);
    }

    // ── Plan state cloud sync ──

    #[test]
    fn extract_plan_from_metadata_full() {
        let metadata = r#"{
            "executing_plan": "{\"subtasks\":[{\"id\":\"s1\",\"title\":\"task\"}]}",
            "plan_goal": "Build feature X",
            "plan_config": "{\"step_by_step\":true,\"auto_execute\":false}",
            "plan_execution_rounds": 3
        }"#;
        let (plan, goal, config, rounds) = extract_plan_from_metadata(metadata);
        assert!(plan.is_some());
        assert!(plan.unwrap().contains("subtasks"));
        assert_eq!(goal, Some("Build feature X".to_string()));
        assert!(config.is_some());
        assert_eq!(rounds, 3);
    }

    #[test]
    fn extract_plan_from_metadata_empty() {
        let (plan, goal, config, rounds) = extract_plan_from_metadata("{}");
        assert!(plan.is_none());
        assert!(goal.is_none());
        assert!(config.is_none());
        assert_eq!(rounds, 0);
    }

    #[test]
    fn extract_plan_from_metadata_invalid_json() {
        let (plan, goal, config, rounds) = extract_plan_from_metadata("not json");
        assert!(plan.is_none());
        assert!(goal.is_none());
        assert!(config.is_none());
        assert_eq!(rounds, 0);
    }

    #[test]
    fn extract_plan_from_metadata_partial() {
        let metadata = r#"{"plan_goal": "Fix bug", "plan_execution_rounds": 1}"#;
        let (plan, goal, config, rounds) = extract_plan_from_metadata(metadata);
        assert!(plan.is_none());
        assert_eq!(goal, Some("Fix bug".to_string()));
        assert!(config.is_none());
        assert_eq!(rounds, 1);
    }

    #[test]
    fn merge_session_state_metadata_preserves_unrelated_fields() {
        let merged = merge_session_state_metadata(
            Some(r#"{"agent_id":"astra-server","note":"keep me"}"#),
            Some("{\"subtasks\":[]}"),
            Some("finish migration"),
            None,
            2,
            Some("main"),
            Some("gpt-5.4"),
        );
        let parsed: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(
            parsed.get("agent_id").and_then(|v| v.as_str()),
            Some("astra-server")
        );
        assert_eq!(parsed.get("note").and_then(|v| v.as_str()), Some("keep me"));
        assert_eq!(
            parsed.get("plan_goal").and_then(|v| v.as_str()),
            Some("finish migration")
        );
        assert_eq!(
            parsed.get("plan_execution_rounds").and_then(|v| v.as_u64()),
            Some(2)
        );
        assert_eq!(
            parsed.get("git_branch").and_then(|v| v.as_str()),
            Some("main")
        );
        assert_eq!(
            parsed.get("model").and_then(|v| v.as_str()),
            Some("gpt-5.4")
        );
    }

    #[test]
    fn merge_session_state_metadata_clears_absent_plan_fields() {
        let merged = merge_session_state_metadata(
            Some(
                r#"{"agent_id":"astra-server","executing_plan":"{}","plan_goal":"stale","plan_config":"{}","plan_execution_rounds":3,"git_branch":"stale-branch","model":"stale-model"}"#,
            ),
            None,
            None,
            None,
            0,
            None,
            None,
        );
        let parsed: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(
            parsed.get("agent_id").and_then(|v| v.as_str()),
            Some("astra-server")
        );
        assert!(parsed.get("executing_plan").is_none());
        assert!(parsed.get("plan_goal").is_none());
        assert!(parsed.get("plan_config").is_none());
        assert!(parsed.get("plan_execution_rounds").is_none());
        assert!(parsed.get("git_branch").is_none());
        assert!(parsed.get("model").is_none());
    }

    #[test]
    fn extract_session_state_from_metadata_ignores_non_plan_trace_fields() {
        let metadata = r#"{
            "executing_plan": "{\"subtasks\":[]}",
            "git_branch": "feature/cloud-sync",
            "model": "gpt-5.4",
            "last_context_trace": {
                "turn_id": "turn-9",
                "selected_tools": ["lsp", "view"],
                "selection_strategy": "code-intel",
                "selection_confidence": 0.93,
                "memory_query": "resume trace persistence",
                "memories_selected": 2,
                "compressed_turns": 1,
                "compression_ratio": 0.72,
                "budget_pressure": 0.88,
                "total_tokens_used": 12345
            }
        }"#;
        let state = extract_session_state_from_metadata(metadata);
        assert!(state.executing_plan_json.is_some());
        assert_eq!(state.plan_execution_rounds, 0);
        assert_eq!(state.git_branch.as_deref(), Some("feature/cloud-sync"));
        assert_eq!(state.model.as_deref(), Some("gpt-5.4"));
    }

    #[test]
    fn parse_heavy_checkpoint_number_from_ref() {
        assert_eq!(
            super::parse_heavy_checkpoint_number("000005-heavy.json"),
            Some(5)
        );
        assert_eq!(
            super::parse_heavy_checkpoint_number("000042-heavy.json"),
            Some(42)
        );
        assert!(super::parse_heavy_checkpoint_number("not-a-checkpoint").is_none());
        assert!(super::parse_heavy_checkpoint_number("000005-light.json").is_none());
    }

    #[test]
    fn restored_session_plan_fields_roundtrip() {
        let s = RestoredSession {
            session_id: "plan-sess".into(),
            executing_plan_json: Some(r#"{"subtasks":[]}"#.into()),
            plan_goal: Some("Cloud sync".into()),
            plan_config_json: Some(r#"{"step_by_step":false}"#.into()),
            plan_execution_rounds: 5,
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let loaded: RestoredSession = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.executing_plan_json, s.executing_plan_json);
        assert_eq!(loaded.plan_goal, s.plan_goal);
        assert_eq!(loaded.plan_config_json, s.plan_config_json);
        assert_eq!(loaded.plan_execution_rounds, 5);
    }

    // -----------------------------------------------------------------------
    // Unhappy-path / edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn restored_session_minimal_json() {
        // Required fields without serde(default)
        let json = r#"{
            "session_id":"s1","last_status":"active","turn_count":0,
            "total_tokens_in":0,"total_tokens_out":0,"recent_tools":[],
            "checkpoint_count":0,"restored_from_cloud":false
        }"#;
        let s: RestoredSession = serde_json::from_str(json).unwrap();
        assert_eq!(s.session_id, "s1");
        assert_eq!(s.turn_count, 0);
        assert!(s.conversation_messages.is_empty());
        assert!(s.blocked_tools.is_empty());
        assert!(s.plan_corrections.is_empty());
        assert_eq!(s.plan_execution_rounds, 0);
    }

    fn minimal_session_json(extra: &str) -> String {
        format!(
            r#"{{"session_id":"s1","last_status":"x","turn_count":0,
            "total_tokens_in":0,"total_tokens_out":0,"recent_tools":[],
            "checkpoint_count":0,"restored_from_cloud":false{}}}"#,
            if extra.is_empty() {
                "".to_string()
            } else {
                format!(",{extra}")
            }
        )
    }

    #[test]
    fn restored_session_with_extra_fields_ignored() {
        let json = minimal_session_json(r#""unknown_field":"value","another":42"#);
        let s: RestoredSession = serde_json::from_str(&json).unwrap();
        assert_eq!(s.session_id, "s1");
    }

    #[test]
    fn restored_session_null_optional_fields() {
        let json = minimal_session_json(
            r#""learning_snapshot_json":null,"git_branch":null,"model":null,"title":null"#,
        );
        let s: RestoredSession = serde_json::from_str(&json).unwrap();
        assert!(s.learning_snapshot_json.is_none());
        assert!(s.git_branch.is_none());
        assert!(s.model.is_none());
        assert!(s.title.is_none());
    }

    #[test]
    fn restored_checkpoint_serialization_roundtrip() {
        let ckpt = RestoredCheckpoint {
            number: 5,
            turn: 10,
            title: "Phase 1 complete".into(),
            summary: "Implemented auth module".into(),
            total_tokens: 50000,
            contract_state_json: Some(r#"{"id":"c1"}"#.into()),
        };
        let json = serde_json::to_string(&ckpt).unwrap();
        let loaded: RestoredCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.number, 5);
        assert_eq!(loaded.turn, 10);
        assert_eq!(loaded.title, "Phase 1 complete");
        assert_eq!(
            loaded.contract_state_json.as_deref(),
            Some(r#"{"id":"c1"}"#)
        );
    }

    #[test]
    fn restored_checkpoint_missing_contract_state() {
        let json = r#"{"number":1,"turn":2,"title":"t","summary":"s","total_tokens":100}"#;
        let ckpt: RestoredCheckpoint = serde_json::from_str(json).unwrap();
        assert!(ckpt.contract_state_json.is_none());
    }

    #[test]
    fn parse_heavy_checkpoint_number_zero_padded() {
        assert_eq!(
            super::parse_heavy_checkpoint_number("000001-heavy.json"),
            Some(1)
        );
        assert_eq!(
            super::parse_heavy_checkpoint_number("000999-heavy.json"),
            Some(999)
        );
    }

    #[test]
    fn parse_heavy_checkpoint_number_no_padding() {
        assert_eq!(
            super::parse_heavy_checkpoint_number("1-heavy.json"),
            Some(1)
        );
        assert_eq!(
            super::parse_heavy_checkpoint_number("42-heavy.json"),
            Some(42)
        );
    }

    #[test]
    fn parse_heavy_checkpoint_number_empty_string() {
        assert!(super::parse_heavy_checkpoint_number("").is_none());
    }

    #[test]
    fn parse_heavy_checkpoint_number_wrong_suffix() {
        assert!(super::parse_heavy_checkpoint_number("000005-light.json").is_none());
        assert!(super::parse_heavy_checkpoint_number("000005-heavy.txt").is_none());
        assert!(super::parse_heavy_checkpoint_number("000005.json").is_none());
    }

    #[test]
    fn parse_heavy_checkpoint_number_non_numeric_prefix() {
        assert!(super::parse_heavy_checkpoint_number("abc-heavy.json").is_none());
        assert!(super::parse_heavy_checkpoint_number("-heavy.json").is_none());
    }

    #[test]
    fn parse_heavy_checkpoint_number_negative() {
        // "-1-heavy.json" → strip suffix → "-1" → parse fails
        assert!(super::parse_heavy_checkpoint_number("-1-heavy.json").is_none());
    }

    #[test]
    fn is_duplicate_key_error_non_database_error() {
        let err = sqlx::Error::RowNotFound;
        assert!(!astra_core::is_duplicate_key_error(&err));
    }

    #[test]
    fn is_duplicate_key_error_detects_protocol_duplicate_wrapper() {
        let fake_err = sqlx::Error::Protocol("1062: Duplicate entry 'test' for key".into());
        assert!(astra_core::is_duplicate_key_error(&fake_err));
    }

    #[test]
    fn restored_session_conversation_messages_preserved() {
        let s = RestoredSession {
            session_id: "s1".into(),
            conversation_messages: vec![
                serde_json::json!({"role": "user", "content": "hi"}),
                serde_json::json!({"role": "assistant", "content": "hello"}),
            ],
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let loaded: RestoredSession = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.conversation_messages.len(), 2);
        assert_eq!(loaded.conversation_messages[0]["role"], "user");
    }

    #[test]
    fn restored_session_blocked_tools_and_corrections() {
        let s = RestoredSession {
            session_id: "s1".into(),
            blocked_tools: vec!["dangerous_tool".into()],
            plan_corrections: vec!["skip step 3".into(), "add validation".into()],
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let loaded: RestoredSession = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.blocked_tools, vec!["dangerous_tool"]);
        assert_eq!(loaded.plan_corrections.len(), 2);
    }

    #[test]
    fn restored_session_skip_serializing_empty_collections() {
        let s = RestoredSession::default();
        let json = serde_json::to_string(&s).unwrap();
        // Empty vecs should not appear in serialized JSON
        assert!(!json.contains("conversation_messages"));
        assert!(!json.contains("blocked_tools"));
        assert!(!json.contains("plan_corrections"));
    }

    #[test]
    fn push_step_checkpoint_sync_logs_audit_records() {
        let source = include_str!("session_restore.rs");
        let start = source
            .find("pub async fn push_step_checkpoint_to_cloud")
            .expect("step checkpoint push function");
        let end = source
            .find("/// Pull the latest Heavy step checkpoint JSON from MatrixOne for session recovery.")
            .expect("step checkpoint function end marker");
        let snippet = &source[start..end];
        assert!(
            snippet.contains("log_checkpoint_sync("),
            "push_step_checkpoint_to_cloud should log sync attempts for audit trails"
        );
        assert!(
            snippet.contains("\"step_checkpoint\""),
            "step checkpoint sync log should use a dedicated sync_type"
        );
    }

    #[test]
    fn push_context_trace_signal_reconciles_session_event_count() {
        let source = include_str!("session_restore.rs");
        let start = source
            .find("pub async fn push_context_trace_signal_to_cloud")
            .expect("context trace push function");
        let end = source
            .find("/// Extract plan state from the metadata JSON returned by agent_sessions.")
            .expect("context trace function end marker");
        let snippet = &source[start..end];
        assert!(
            snippet.contains("reconcile_session_event_count(pool, session_id, user_id).await"),
            "context trace cloud inserts must reconcile agent_sessions.event_count"
        );
    }

    #[test]
    fn cloud_step_checkpoint_number_uses_disjoint_namespace() {
        assert_eq!(
            cloud_step_checkpoint_number(1).unwrap(),
            1_000_000_001,
            "step checkpoints should avoid the session-checkpoint number range"
        );
    }

    #[test]
    fn restore_cloud_session_uses_user_query_count_for_turns() {
        let source = include_str!("session_restore.rs");
        let start = source
            .find("async fn restore_cloud_session")
            .expect("restore cloud session function");
        let end = source
            .find("/// List checkpoints from MatrixOne.")
            .expect("restore cloud session end marker");
        let snippet = &source[start..end];
        assert!(
            snippet.contains("event_type = 'user_query'"),
            "cloud restore should derive turn_count from user_query events"
        );
        assert!(
            snippet.contains("AS turn_count"),
            "cloud restore query should materialize a turn_count column"
        );
    }

    #[test]
    fn list_resumable_sessions_uses_user_query_count_for_turns() {
        let source = include_str!("session_restore.rs");
        assert!(
            source.contains(
                "(SELECT COUNT(*) FROM agent_events WHERE session_id = agent_sessions.session_id AND event_type = 'user_query') AS turn_count"
            ),
            "resumable session listing should derive turn_count from user_query events"
        );
        assert!(
            source.contains("turn_count: turn_count as u32"),
            "resumable session listing should populate RestoredSession.turn_count from the derived turn_count column"
        );
    }

    #[test]
    fn list_resumable_sessions_restores_resume_identity_fields() {
        let source = include_str!("session_restore.rs");
        assert!(
            source.contains("CAST(metadata AS CHAR) AS metadata_json"),
            "resumable session listing should read session metadata so git_branch/model can survive cloud-only resume lists"
        );
        assert!(
            source.contains("latest_model"),
            "resumable session listing should fetch latest_model via subquery to avoid N+1 queries"
        );
        assert!(
            source.contains("git_branch: metadata_state.git_branch.clone()"),
            "resumable session listing should populate RestoredSession.git_branch from session metadata"
        );
    }

    #[test]
    fn recent_tools_from_context_trace_uses_selected_tools() {
        let trace = session_workspace::ContextTraceSignal {
            turn_id: "turn-7".into(),
            captured_at: None,
            tool_selection: Some(session_workspace::ContextTraceToolSelection {
                tools_available: 8,
                selected_tools: vec!["bash".into(), "grep".into(), "bash".into()],
                rejected_tools: 0,
                strategy: "recent_tools".into(),
                confidence: 0.91,
                latency_ms: 12,
            }),
            memory: None,
            history: None,
            budget: None,
            timing: None,
            explanations: Vec::new(),
        };

        assert_eq!(
            recent_tools_from_context_trace(Some(&trace)),
            vec!["bash".to_string(), "grep".to_string()]
        );
        assert!(recent_tools_from_context_trace(None).is_empty());
    }

    #[test]
    fn parse_cloud_heavy_checkpoint_state_accepts_tagged_and_legacy_shapes() {
        let messages = vec![serde_json::json!({"role":"user","content":"hi"})];
        let approval_overrides = serde_json::json!({"rules": []});
        let interruption = serde_json::json!({"kind":"rate_limited"});
        let compaction_state = serde_json::json!({"attempt_count": 2});
        let expected = CloudHeavyCheckpointState {
            messages: messages.clone(),
            blocked_tools: vec!["bash".into()],
            recent_tools: vec!["rg".into()],
            approval_overrides: Some(approval_overrides.clone()),
            interruption: Some(interruption.clone()),
            compaction_state: Some(compaction_state.clone()),
        };
        let tagged = serde_json::json!({
            "Heavy": {
                "light": {},
                "messages": messages,
                "blocked_tools": ["bash"],
                "recent_tools": ["rg"],
                "approval_overrides": approval_overrides,
                "interruption": interruption,
                "compaction_state": compaction_state
            }
        })
        .to_string();
        let legacy = serde_json::json!({
            "messages": expected.messages.clone(),
            "blocked_tools": expected.blocked_tools.clone(),
            "recent_tools": expected.recent_tools.clone(),
            "approval_overrides": expected.approval_overrides.clone(),
            "interruption": expected.interruption.clone(),
            "compaction_state": expected.compaction_state.clone()
        })
        .to_string();

        assert_eq!(
            parse_cloud_heavy_checkpoint_state(&tagged),
            Some(expected.clone())
        );
        assert_eq!(parse_cloud_heavy_checkpoint_state(&legacy), Some(expected));
    }

    #[test]
    fn restore_recent_tools_prefers_checkpoint_tools_json() {
        let source = include_str!("session_restore.rs");
        let start = source
            .find("async fn restore_recent_tools")
            .expect("restore recent tools function");
        let end = source
            .find("/// Restore the latest structured context-trace signal from cloud events.")
            .expect("restore recent tools end marker");
        let snippet = &source[start..end];
        assert!(
            snippet.contains("CAST(tools_json AS CHAR) AS tools_json"),
            "recent tool restore should decode authoritative tools_json from cloud checkpoints using a MatrixOne-compatible string cast"
        );
        assert!(
            snippet.contains("state_json IS NULL"),
            "recent tool restore should ignore state-bearing step checkpoint rows"
        );
        assert!(
            snippet.contains("CAST(metadata AS CHAR) AS metadata_json"),
            "legacy turn_complete metadata fallback should decode JSON through a MatrixOne-compatible string cast"
        );
    }

    #[test]
    fn restore_cloud_session_populates_recent_tools() {
        let source = include_str!("session_restore.rs");
        let start = source
            .find("async fn restore_cloud_session")
            .expect("restore cloud session function");
        let end = source
            .find("/// List checkpoints from MatrixOne.")
            .expect("restore cloud session end marker");
        let snippet = &source[start..end];
        assert!(
            snippet.contains("restore_recent_tools(session_id)"),
            "cloud restore should recover recent tools instead of always returning an empty list"
        );
        assert!(
            snippet.contains("recent_tools,"),
            "cloud restore should populate RestoredSession.recent_tools"
        );
    }

    #[test]
    fn restore_cloud_session_restores_cloud_summary_fields() {
        let source = include_str!("session_restore.rs");
        let start = source
            .find("async fn restore_cloud_session")
            .expect("restore cloud session function");
        let end = source
            .find("/// List checkpoints from MatrixOne.")
            .expect("restore cloud session end marker");
        let snippet = &source[start..end];
        assert!(
            snippet.contains("token_input"),
            "cloud restore should recover prompt-token totals from agent_events"
        );
        assert!(
            snippet.contains("token_output"),
            "cloud restore should recover completion-token totals from agent_events"
        );
        assert!(
            snippet.contains(
                "session_checkpoints sc WHERE sc.session_id = agent_sessions.session_id AND state_json IS NULL"
            ),
            "cloud restore should recover ordinary checkpoint counts from session_checkpoints"
        );
        assert!(
            snippet.contains("restore_learning(&user_id, \"default\")"),
            "cloud restore should recover the latest cloud learning snapshot for the user"
        );
    }

    #[test]
    fn restore_cloud_session_restores_resume_identity_fields() {
        let source = include_str!("session_restore.rs");
        let start = source
            .find("async fn restore_cloud_session")
            .expect("restore cloud session function");
        let end = source
            .find("/// List checkpoints from MatrixOne.")
            .expect("restore cloud session end marker");
        let snippet = &source[start..end];
        assert!(
            snippet.contains("latest_model"),
            "cloud restore should fetch latest_model via subquery to avoid N+1 queries"
        );
        assert!(
            snippet.contains("CAST(metadata AS CHAR) AS metadata_json"),
            "cloud restore should decode session metadata through a MatrixOne-compatible string cast"
        );
        assert!(
            snippet.contains("git_branch: metadata_state.git_branch.clone()"),
            "cloud restore should restore git_branch from session metadata"
        );
    }

    #[test]
    fn restore_latest_context_trace_signal_uses_matrixone_json_cast() {
        let source = include_str!("session_restore.rs");
        let start = source
            .find("async fn restore_latest_context_trace_signal")
            .expect("restore latest context trace function");
        let end = source
            .find("/// Pull learning snapshot from MatrixOne.")
            .expect("restore latest context trace end marker");
        let snippet = &source[start..end];
        assert!(
            snippet.contains("CAST(metadata AS CHAR) AS metadata_json"),
            "context trace restore should decode JSON metadata through a MatrixOne-compatible string cast"
        );
    }

    #[test]
    fn pull_step_checkpoint_from_cloud_uses_matrixone_json_cast() {
        let source = include_str!("session_restore.rs");
        let start = source
            .find("pub async fn pull_step_checkpoint_from_cloud")
            .expect("pull_step_checkpoint function");
        let end = source
            .find("// ─── Plan State Cloud Sync")
            .expect("pull_step_checkpoint end marker");
        let snippet = &source[start..end];
        assert!(
            snippet.contains("CAST(state_json AS CHAR) AS state_json_json"),
            "heavy step checkpoint restore should decode state_json through a MatrixOne-compatible string cast"
        );
    }

    #[test]
    fn push_session_state_cloud_upserts_agent_session_row() {
        let source = include_str!("session_restore.rs");
        let start = source
            .find("pub async fn push_session_state_to_cloud")
            .expect("session state push function");
        let end = source
            .find("/// Push a structured context-trace signal as a first-class cloud event.")
            .expect("session state function end marker");
        let snippet = &source[start..end];
        assert!(
            snippet.contains("INSERT INTO agent_sessions"),
            "push_session_state_to_cloud should create missing agent_sessions rows"
        );
        assert!(
            snippet.contains("ON DUPLICATE KEY UPDATE metadata = ?"),
            "push_session_state_to_cloud should upsert metadata for existing sessions"
        );
    }

    #[test]
    fn push_session_state_cloud_merges_resume_identity_metadata() {
        let source = include_str!("session_restore.rs");
        let start = source
            .find("pub async fn push_session_state_to_cloud")
            .expect("session state push function");
        let end = source
            .find("/// Push a structured context-trace signal as a first-class cloud event.")
            .expect("session state function end marker");
        let snippet = &source[start..end];
        assert!(
            snippet.contains("git_branch"),
            "session state cloud sync should persist git_branch in metadata"
        );
        assert!(
            snippet.contains("model"),
            "session state cloud sync should persist model in metadata"
        );
    }

    #[test]
    fn push_session_state_sync_logs_audit_records() {
        let source = include_str!("session_restore.rs");
        let start = source
            .find("pub async fn push_session_state_to_cloud")
            .expect("session state push function");
        let end = source
            .find("/// Push a structured context-trace signal as a first-class cloud event.")
            .expect("session state function end marker");
        let snippet = &source[start..end];
        assert!(
            snippet.contains("log_session_sync("),
            "push_session_state_to_cloud should record sync attempts in session_sync_log"
        );
        assert!(
            snippet.contains("\"session_state\""),
            "push_session_state_to_cloud should log with sync_type=session_state"
        );
    }

    #[test]
    fn push_context_trace_sync_logs_audit_records() {
        let source = include_str!("session_restore.rs");
        let start = source
            .find("pub async fn push_context_trace_signal_to_cloud")
            .expect("context trace push function");
        let end = source
            .find("async fn reconcile_session_event_count")
            .expect("context trace function end marker");
        let snippet = &source[start..end];
        assert!(
            snippet.contains("log_session_sync("),
            "push_context_trace_signal_to_cloud should record sync attempts in session_sync_log"
        );
        assert!(
            snippet.contains("\"context_trace\""),
            "push_context_trace_signal_to_cloud should log with sync_type=context_trace"
        );
    }

    #[test]
    fn reconcile_session_event_count_uses_matrixone_compatible_bound_counts() {
        let source = include_str!("session_restore.rs");
        let start = source
            .find("async fn reconcile_session_event_count")
            .expect("reconcile helper");
        let end = source
            .find("fn cloud_step_checkpoint_number")
            .expect("reconcile helper end marker");
        let snippet = &source[start..end];
        assert!(
            snippet.contains("load_agent_event_count"),
            "reconcile_session_event_count should load event_count before upserting so MatrixOne does not need a subquery in ON DUPLICATE KEY UPDATE"
        );
        assert!(
            snippet.contains("upsert_agent_session_event_count"),
            "reconcile_session_event_count should use the shared bound-value upsert helper"
        );
    }

    #[test]
    fn cloud_checkpoints_excludes_step_rows() {
        let source = include_str!("session_restore.rs");
        let start = source
            .find("async fn cloud_checkpoints")
            .expect("cloud checkpoints function");
        let end = source
            .find("/// Restore composite state (data snapshot + git commit) from local files.")
            .expect("cloud checkpoints function end marker");
        let snippet = &source[start..end];
        assert!(
            snippet.contains("state_json IS NULL"),
            "cloud_checkpoints should only restore session checkpoints, not step checkpoint rows"
        );
    }

    #[test]
    fn push_step_checkpoint_uses_namespaced_cloud_number() {
        let source = include_str!("session_restore.rs");
        let start = source
            .find("pub async fn push_step_checkpoint_to_cloud")
            .expect("step checkpoint push function");
        let end = source
            .find("/// Pull the latest Heavy step checkpoint JSON from MatrixOne for session recovery.")
            .expect("step checkpoint function end marker");
        let snippet = &source[start..end];
        assert!(
            snippet.contains("cloud_step_checkpoint_number(checkpoint_number)?"),
            "step checkpoint cloud writes should use a disjoint number namespace"
        );
    }
}
