//! Session restore: reconstruct session state from MatrixOne + local files.
//!
//! # Architecture
//!
//! ```text
//! restore_session(user_id, session_id)
//!   ├─ 1. Prove the MatrixOne session belongs to user_id
//!   ├─ 2. Pull owner-scoped local journal/checkpoints when present
//!   ├─ 3. Pull owner-bound cloud artifacts/events/checkpoints
//!   └─ 4. Return RestoredSession for the REPL to continue
//! ```
//!
//! Local-only restore is a separate API path and intentionally never reads MatrixOne.
//! Cloud restore is always owner-bound.

use astra_core::is_duplicate_key_error;
use async_trait::async_trait;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use astra_core::canonical_names::{append_unique_names, normalize_name_list};

use crate::{SessionArtifactJsonRecord, SessionArtifactJsonStore, StoredSessionArtifact};

const STEP_CHECKPOINT_NUMBER_OFFSET: u32 = 1_000_000_000;
const MAX_CLOUD_RESTORE_CHECKPOINTS: u32 = 200;
pub const COMPOSITE_SNAPSHOT_INDEX_ARTIFACT_KIND: &str = "composite_snapshot_index";
pub const COMPOSITE_SNAPSHOT_INDEX_PROJECTION_ID: &str = "projection:composite-snapshot-index";
const SESSION_STATE_SYNC_METADATA_MARKER: &str = "_session_state_sync";
const CLOUD_CHECKPOINTS_SELECT_SQL: &str = "\
    SELECT number, turn, title, summary, total_tokens, contract_state_json \
    FROM ( \
        SELECT number, turn, title, summary, total_tokens, contract_state_json \
        FROM session_checkpoints \
        WHERE user_id = ? AND session_id = ? AND state_json IS NULL \
        ORDER BY number DESC \
        LIMIT ? \
    ) AS recent_checkpoints \
    ORDER BY number";
const CLOUD_CHECKPOINT_COUNT_SQL: &str = "\
    SELECT COUNT(*) AS checkpoint_count \
    FROM session_checkpoints \
    WHERE user_id = ? AND session_id = ? AND state_json IS NULL";
pub const MAX_PROMPT_HISTORY_TRANSCRIPT_ROWS: i64 = 80;
pub const PROMPT_HISTORY_TRANSCRIPT_SELECT_SQL: &str = "\
    SELECT role, content \
    FROM ( \
        SELECT sti.item_seq, sti.role, sti.content \
        FROM session_transcript_items sti \
        LEFT JOIN agent_runs r \
          ON r.user_id = sti.user_id \
         AND r.session_id = sti.session_id \
         AND r.run_id = sti.run_id \
        WHERE sti.session_id = ? \
          AND sti.user_id = ? \
          AND sti.role IN ('user', 'assistant', 'system') \
          AND ( \
              sti.run_id IS NULL \
              OR (r.run_id IS NOT NULL AND r.parent_run_id IS NULL) \
          ) \
        ORDER BY sti.item_seq DESC \
        LIMIT ? \
    ) recent_prompt_history \
    ORDER BY item_seq";
pub const PROMPT_HISTORY_TRANSCRIPT_EXISTS_SQL: &str = "\
    SELECT 1 AS present \
    FROM session_transcript_items sti \
    LEFT JOIN agent_runs r \
      ON r.user_id = sti.user_id \
     AND r.session_id = sti.session_id \
     AND r.run_id = sti.run_id \
    WHERE sti.session_id = ? \
      AND sti.user_id = ? \
      AND sti.role IN ('user', 'assistant', 'system') \
      AND ( \
          sti.run_id IS NULL \
          OR (r.run_id IS NOT NULL AND r.parent_run_id IS NULL) \
      ) \
    LIMIT 1";
const PUSH_SESSION_STATE_UPSERT_SQL: &str = "INSERT INTO agent_sessions \
             (session_id, user_id, status, metadata, created_at, updated_at, last_active_at) \
             VALUES (?, ?, 'active', ?, NOW(6), NOW(6), NOW(6)) \
             ON DUPLICATE KEY UPDATE \
             metadata = IF(user_id = VALUES(user_id), VALUES(metadata), metadata), \
             updated_at = IF(user_id = VALUES(user_id), NOW(6), updated_at), \
             last_active_at = IF(user_id = VALUES(user_id), NOW(6), last_active_at)";

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

trait SessionRestoreRow {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error>;
    fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error>;
    fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error>;
    fn i32_column(&self, column: &str) -> Result<i32, sqlx::Error>;
}

impl SessionRestoreRow for sqlx::mysql::MySqlRow {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
        use sqlx::Row;
        self.try_get::<String, _>(column)
    }

    fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
        use sqlx::Row;
        self.try_get::<Option<String>, _>(column)
    }

    fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
        use sqlx::Row;
        self.try_get::<i64, _>(column)
    }

    fn i32_column(&self, column: &str) -> Result<i32, sqlx::Error> {
        use sqlx::Row;
        self.try_get::<i32, _>(column)
    }
}

fn mysql_string(
    row: &impl SessionRestoreRow,
    context: &str,
    column: &str,
) -> Result<String, String> {
    row.string_column(column)
        .map_err(|e| format!("{context}: decode column `{column}`: {e}"))
}

fn mysql_optional_string(
    row: &impl SessionRestoreRow,
    context: &str,
    column: &str,
) -> Result<Option<String>, String> {
    row.optional_string_column(column)
        .map_err(|e| format!("{context}: decode column `{column}`: {e}"))
}

fn mysql_i64(row: &impl SessionRestoreRow, context: &str, column: &str) -> Result<i64, String> {
    row.i64_column(column)
        .map_err(|e| format!("{context}: decode column `{column}`: {e}"))
}

fn mysql_i32(row: &impl SessionRestoreRow, context: &str, column: &str) -> Result<i32, String> {
    row.i32_column(column)
        .map_err(|e| format!("{context}: decode column `{column}`: {e}"))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CloudRestoreTimings {
    session_query_ms: u64,
    heavy_checkpoint_ms: u64,
    transcript_ms: u64,
    context_trace_ms: u64,
    recent_tools_ms: u64,
    contract_ms: u64,
    checkpoint_fallback_ms: u64,
    total_ms: u64,
}

impl CloudRestoreTimings {
    fn emit(self, session_id: &str, restored: bool) {
        tracing::info!(
            target: "astra_services::session_restore",
            %session_id,
            restored,
            session_query_ms = self.session_query_ms,
            heavy_checkpoint_ms = self.heavy_checkpoint_ms,
            transcript_ms = self.transcript_ms,
            context_trace_ms = self.context_trace_ms,
            recent_tools_ms = self.recent_tools_ms,
            contract_ms = self.contract_ms,
            checkpoint_fallback_ms = self.checkpoint_fallback_ms,
            total_ms = self.total_ms,
            "cloud session restore timings"
        );
    }
}

fn elapsed_ms(started_at: std::time::Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn metadata_json_state(metadata_str: Option<&str>) -> Result<SessionMetadataState, String> {
    match metadata_str.map(str::trim).filter(|m| !m.is_empty()) {
        Some(metadata) => extract_session_state_from_metadata(metadata),
        None => Ok(SessionMetadataState::default()),
    }
}

fn non_negative_i64_to_u64(value: i64, context: &str, column: &str) -> Result<u64, String> {
    u64::try_from(value)
        .map_err(|_| format!("{context}: column `{column}` expected u64 range, got {value}"))
}

fn non_negative_i64_to_u32(value: i64, context: &str, column: &str) -> Result<u32, String> {
    u32::try_from(value)
        .map_err(|_| format!("{context}: column `{column}` expected u32 range, got {value}"))
}

fn non_negative_i32_to_u32(value: i32, context: &str, column: &str) -> Result<u32, String> {
    u32::try_from(value)
        .map_err(|_| format!("{context}: column `{column}` expected u32 range, got {value}"))
}

fn token_usage_i64_or_zero(value: &Value, field: &str, context: &str) -> Result<i64, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(0),
        Some(raw) => {
            let count = raw.as_i64().ok_or_else(|| {
                format!("{context}: token_usage field `{field}` must be an integer, got {raw}")
            })?;
            if count < 0 {
                return Err(format!(
                    "{context}: token_usage field `{field}` must be non-negative, got {count}"
                ));
            }
            Ok(count)
        }
    }
}

fn cache_token_counts_from_token_usage_json(
    raw: &str,
    context: &str,
) -> Result<(i64, i64), String> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| format!("{context}: token_usage JSON decode failed: {error}"))?;
    if !value.is_object() {
        return Err(format!(
            "{context}: token_usage must be an object, got {value}"
        ));
    }
    Ok((
        token_usage_i64_or_zero(&value, "cached_input_tokens", context)?,
        token_usage_i64_or_zero(&value, "cache_creation_tokens", context)?,
    ))
}

fn apply_restore_cache_token_usage(
    raw: &str,
    event_id: &str,
    cache_read_total: &mut i64,
    cache_creation_total: &mut i64,
) -> bool {
    let (cache_read, cache_creation) = match cache_token_counts_from_token_usage_json(
        raw,
        "restore_cloud_session.cache_tokens",
    ) {
        Ok(counts) => counts,
        Err(error) => {
            tracing::warn!(
                target: "astra_services::session_restore",
                event_id = event_id,
                error = %error,
                "invalid token_usage while restoring cache totals; skipping event token counters"
            );
            return false;
        }
    };
    let Some(next_cache_read_total) = cache_read_total.checked_add(cache_read) else {
        tracing::warn!(
            target: "astra_services::session_restore",
            event_id = event_id,
            current_total = *cache_read_total,
            delta = cache_read,
            "cache read token total overflow while restoring session; skipping event token counters"
        );
        return false;
    };
    let Some(next_cache_creation_total) = cache_creation_total.checked_add(cache_creation) else {
        tracing::warn!(
            target: "astra_services::session_restore",
            event_id = event_id,
            current_total = *cache_creation_total,
            delta = cache_creation,
            "cache creation token total overflow while restoring session; skipping event token counters"
        );
        return false;
    };

    *cache_read_total = next_cache_read_total;
    *cache_creation_total = next_cache_creation_total;
    true
}

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
    /// Total prompt-cache read tokens consumed so far.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub total_cache_read_tokens: u64,
    /// Total prompt-cache creation tokens consumed so far.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub total_cache_creation_tokens: u64,
    /// Recently used tools (for context carry-forward).
    pub recent_tools: Vec<String>,
    /// Number of checkpoints available.
    pub checkpoint_count: u32,
    /// Status of the session when last active.
    pub last_status: String,
    /// Git branch from workspace metadata.
    pub git_branch: Option<String>,
    /// Model used in the session.
    pub model: Option<String>,
    /// Permission mode used when the session was last active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Session title (if set).
    pub title: Option<String>,
    /// Whether restoration was from cloud (true) or local only (false).
    pub restored_from_cloud: bool,
    /// Conversation messages from Step Protocol heavy checkpoint (for LLM resume).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conversation_messages: Vec<serde_json::Value>,
    /// Blocked/health-avoidance tools from checkpoint.
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
    /// Serialized context pipeline state (stats + latches + recovery).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline_state: Option<serde_json::Value>,
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
    /// Authoritative workspace metadata snapshot when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<super::session_workspace::WorkspaceMetadata>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResumableSessionsResponse {
    pub sessions: Vec<RestoredSession>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionMetadataState {
    pub executing_plan_json: Option<String>,
    pub plan_goal: Option<String>,
    pub plan_config_json: Option<String>,
    pub plan_execution_rounds: usize,
    pub git_branch: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CloudHeavyCheckpointState {
    pub messages: Vec<serde_json::Value>,
    pub blocked_tools: Vec<String>,
    pub recent_tools: Vec<String>,
    pub approval_overrides: Option<serde_json::Value>,
    pub interruption: Option<serde_json::Value>,
    pub compaction_state: Option<serde_json::Value>,
    pub pipeline_state: Option<serde_json::Value>,
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
    /// Restore a session owned by a user. Returns None if the session is missing or not owned.
    async fn restore_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<RestoredSession>, String>;

    /// List available checkpoints for a session.
    async fn list_checkpoints(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Vec<RestoredCheckpoint>, String>;

    /// Restore session state to a specific checkpoint.
    async fn restore_to_checkpoint(
        &self,
        user_id: &str,
        session_id: &str,
        checkpoint_number: u32,
    ) -> Result<Option<RestoredSession>, String>;

    /// List resumable sessions for a user (active or paused).
    async fn list_resumable_sessions(&self, user_id: &str) -> Result<Vec<RestoredSession>, String>;

    /// Restore session state to a specific composite snapshot.
    /// Uses the `RestoreSelector` to determine which dimensions to restore.
    async fn restore_to_composite_snapshot(
        &self,
        user_id: &str,
        session_id: &str,
        snapshot_id: &str,
        selector: &astra_core::composite_snapshot::RestoreSelector,
    ) -> Result<Option<RestoredCompositeState>, String> {
        let _ = (user_id, session_id, snapshot_id, selector);
        Ok(None)
    }

    /// List composite snapshots for a session.
    async fn list_composite_snapshots(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<astra_core::composite_snapshot::CompositeSnapshotIndex, String> {
        let _ = (user_id, session_id);
        Ok(astra_core::composite_snapshot::CompositeSnapshotIndex::default())
    }
}

async fn restore_cloud_cache_token_totals(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
) -> Result<(i64, i64), String> {
    let rows = sqlx::query(
        "SELECT event_id, IFNULL(CAST(token_usage AS CHAR), '{}') AS token_usage \
         FROM agent_events \
         WHERE session_id = ? AND user_id = ? \
           AND event_type IN ('user_query', 'llm_response') AND token_usage IS NOT NULL",
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("restore_cloud_session.cache_tokens: {e}"))?;

    let mut cache_read_total = 0i64;
    let mut cache_creation_total = 0i64;
    for row in rows {
        let event_id = mysql_string(&row, "restore_cloud_session.cache_tokens", "event_id")
            .unwrap_or_else(|error| {
                tracing::warn!(
                    target: "astra_services::session_restore",
                    error = %error,
                    "could not decode event_id while restoring cache totals"
                );
                "<unknown>".to_string()
            });
        let raw = mysql_string(&row, "restore_cloud_session.cache_tokens", "token_usage")?;
        apply_restore_cache_token_usage(
            &raw,
            &event_id,
            &mut cache_read_total,
            &mut cache_creation_total,
        );
    }
    Ok((cache_read_total, cache_creation_total))
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

    /// Restore only local filesystem state. This intentionally never reads MatrixOne.
    pub async fn restore_local_session(
        &self,
        session_id: &str,
    ) -> Result<Option<RestoredSession>, String> {
        self.restore_session_inner(None, session_id).await
    }

    /// List only local filesystem checkpoints. This intentionally never reads MatrixOne.
    pub async fn list_local_checkpoints(
        &self,
        session_id: &str,
    ) -> Result<Vec<RestoredCheckpoint>, String> {
        self.list_checkpoints_inner(None, session_id).await
    }

    /// Restore only local filesystem state to a checkpoint.
    pub async fn restore_local_to_checkpoint(
        &self,
        session_id: &str,
        checkpoint_number: u32,
    ) -> Result<Option<RestoredSession>, String> {
        let session = match self.restore_local_session(session_id).await? {
            Some(session) => session,
            None => return Ok(None),
        };
        let checkpoints = self.list_local_checkpoints(session_id).await?;
        Self::apply_checkpoint(session_id, session, &checkpoints, checkpoint_number)
    }

    async fn require_owned_cloud_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<bool, String> {
        let pool = self.pool.as_ref().ok_or_else(|| {
            "owner-bound session restore requires a MatrixOne pool; use local-only restore for local files"
                .to_string()
        })?;
        crate::storage::agent_session_exists_for_user(pool, session_id, user_id)
            .await
            .map_err(|error| format!("session owner check: {error}"))
    }

    fn apply_checkpoint(
        session_id: &str,
        session: RestoredSession,
        checkpoints: &[RestoredCheckpoint],
        checkpoint_number: u32,
    ) -> Result<Option<RestoredSession>, String> {
        let Some(ckpt) = checkpoints
            .iter()
            .find(|checkpoint| checkpoint.number == checkpoint_number)
        else {
            return Err(format!(
                "checkpoint {} not found for session {}",
                checkpoint_number, session_id
            ));
        };
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

    async fn restore_session_inner(
        &self,
        user_id: Option<&str>,
        session_id: &str,
    ) -> Result<Option<RestoredSession>, String> {
        if let Some(user_id) = user_id
            && !self
                .require_owned_cloud_session(user_id, session_id)
                .await?
        {
            return Ok(None);
        }

        let local_journal = summarize_local_journal(user_id, session_id)?;
        let mut local_workspace_error = None;
        let local_workspace = match self.restore_local_workspace(user_id, session_id) {
            Ok(workspace) => workspace,
            Err(error) => {
                local_workspace_error = Some(error);
                None
            }
        };

        if let Some(ws) = local_workspace {
            let mut recent_tools = if let Some(user_id) = user_id {
                self.restore_recent_tools(user_id, session_id).await?
            } else {
                Vec::new()
            };
            if recent_tools.is_empty() {
                recent_tools = recent_tools_from_context_trace(ws.last_context_trace.as_ref());
            }
            if recent_tools.is_empty()
                && let Some(summary) = local_journal.as_ref()
            {
                recent_tools = normalize_name_list(summary.recent_tools.iter().map(String::as_str));
            }

            let ckpt_count = local_checkpoint_count(user_id, session_id, "restore_session_inner")?;

            return Ok(Some(restored_session_from_workspace(
                ws,
                local_journal.as_ref(),
                recent_tools,
                ckpt_count,
                false,
            )));
        }

        if let Some(user_id) = user_id
            && let Some(ws) = self.restore_cloud_workspace(user_id, session_id).await?
        {
            let mut recent_tools = self.restore_recent_tools(user_id, session_id).await?;
            if recent_tools.is_empty() {
                recent_tools =
                    recent_tools_from_context_trace(ws.metadata.last_context_trace.as_ref());
            }
            if recent_tools.is_empty()
                && let Some(summary) = local_journal.as_ref()
            {
                recent_tools = normalize_name_list(summary.recent_tools.iter().map(String::as_str));
            }

            let local_ckpt_count =
                local_checkpoint_count(Some(user_id), session_id, "restore_session_inner")?;
            let cloud_ckpt_count = self
                .cloud_checkpoint_count(user_id, session_id)
                .await
                .map_err(|e| format!("restore_session_inner cloud checkpoint count: {e}"))?;

            return Ok(Some(restored_session_from_workspace(
                ws.metadata,
                local_journal.as_ref(),
                recent_tools,
                cloud_ckpt_count.max(local_ckpt_count),
                true,
            )));
        }

        if let Some(summary) = local_journal {
            let ckpt_count = local_checkpoint_count(user_id, session_id, "restore_session_inner")?;

            return Ok(Some(RestoredSession {
                session_id: session_id.to_string(),
                turn_count: summary.turn_count,
                total_tokens_in: summary.total_tokens_in,
                total_tokens_out: summary.total_tokens_out,
                total_cache_read_tokens: summary.total_cache_read_tokens,
                total_cache_creation_tokens: summary.total_cache_creation_tokens,
                recent_tools: normalize_name_list(summary.recent_tools),
                checkpoint_count: ckpt_count,
                last_status: summary.last_status,
                model: summary.model,
                permission_mode: summary.permission_mode,
                restored_from_cloud: false,
                ..Default::default()
            }));
        }

        if let Some(user_id) = user_id {
            let cloud_result = self.restore_cloud_session(user_id, session_id).await?;
            if cloud_result.is_some() {
                return Ok(cloud_result);
            }
        }

        if let Some(error) = local_workspace_error {
            return Err(error);
        }

        Ok(None)
    }

    async fn list_checkpoints_inner(
        &self,
        user_id: Option<&str>,
        session_id: &str,
    ) -> Result<Vec<RestoredCheckpoint>, String> {
        if let Some(user_id) = user_id
            && !self
                .require_owned_cloud_session(user_id, session_id)
                .await?
        {
            return Ok(Vec::new());
        }

        let local_entries = if user_id.is_some() {
            Vec::new()
        } else {
            super::session_checkpoint::read_checkpoint_index(session_id).map_err(|error| {
                format!(
                    "list_checkpoints_inner: failed to read local checkpoint index for {session_id}: {error}"
                )
            })?
        };
        let local = parse_local_checkpoint_entries(&local_entries);
        let cloud = if let Some(user_id) = user_id {
            self.cloud_checkpoints(user_id, session_id)
                .await
                .map_err(|error| {
                    format!(
                        "list_checkpoints_inner: failed to read cloud checkpoints for {session_id}: {error}"
                    )
                })?
        } else {
            Vec::new()
        };
        Ok(merge_checkpoints(local, cloud))
    }

    /// Try restoring workspace metadata from local YAML file.
    fn restore_local_workspace(
        &self,
        user_id: Option<&str>,
        session_id: &str,
    ) -> Result<Option<super::session_workspace::WorkspaceMetadata>, String> {
        // The legacy YAML workspace has no physical owner in its path. It is
        // valid only for local/CLI restore and must never be consulted by an
        // authenticated cloud restore.
        if user_id.is_some() {
            return Ok(None);
        }
        match super::session_workspace::read_workspace(session_id) {
            Ok(ws) => Ok(Some(ws)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("read local workspace for {session_id}: {e}")),
        }
    }

    /// Try restoring workspace metadata from remote session artifacts.
    async fn restore_cloud_workspace(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<CloudWorkspaceArtifact>, String> {
        let pool = match self.pool.as_ref() {
            Some(pool) => pool,
            None => return Ok(None),
        };
        let artifact = crate::session_artifact_store::load_latest_json_artifact_from_pool(
            pool,
            user_id,
            session_id,
            super::session_workspace::WORKSPACE_METADATA_ARTIFACT_KIND,
        )
        .await
        .map_err(|e| format!("restore_cloud_workspace: {e}"))?;

        let Some(artifact) = artifact else {
            return Ok(None);
        };

        let metadata =
            serde_json::from_value::<super::session_workspace::WorkspaceMetadata>(artifact.content)
                .map_err(|e| format!("restore_cloud_workspace: {e}"))?;

        Ok(Some(CloudWorkspaceArtifact { metadata }))
    }

    async fn restore_cloud_composite_snapshot_index(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<astra_core::composite_snapshot::CompositeSnapshotIndex>, String> {
        let pool = match self.pool.as_ref() {
            Some(pool) => pool,
            None => return Ok(None),
        };
        let artifact = crate::session_artifact_store::load_json_artifact_from_pool(
            pool,
            user_id,
            session_id,
            COMPOSITE_SNAPSHOT_INDEX_PROJECTION_ID,
        )
        .await
        .map_err(|error| format!("restore_cloud_composite_snapshot_index: {error}"))?;

        let Some(artifact) = artifact else {
            return Ok(None);
        };

        let mut index = serde_json::from_value::<
            astra_core::composite_snapshot::CompositeSnapshotIndex,
        >(artifact.content)
        .map_err(|error| format!("restore_cloud_composite_snapshot_index: {error}"))?;
        index.normalize_versions();
        Ok(Some(index))
    }

    /// Restore from MatrixOne agent_sessions table.
    async fn restore_cloud_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<RestoredSession>, String> {
        let started_at = std::time::Instant::now();
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(None),
        };

        let session_query_started_at = std::time::Instant::now();
        let row = sqlx::query(
            "SELECT s.session_id, s.user_id, s.title, s.status, s.event_count, CAST(s.metadata AS CHAR) AS metadata_json, \
             COALESCE(event_summary.turn_count, 0) AS turn_count, \
             COALESCE(event_summary.total_tokens_in, 0) AS total_tokens_in, \
             COALESCE(event_summary.total_tokens_out, 0) AS total_tokens_out, \
             COALESCE(checkpoint_summary.checkpoint_count, 0) AS checkpoint_count, \
             latest_model.llm_model_used AS latest_model, \
             s.created_at, s.updated_at \
              FROM agent_sessions s \
              LEFT JOIN ( \
                SELECT user_id, session_id, \
                       COALESCE(MAX(turn_seq), 0) AS turn_count, \
                       CAST(COALESCE(SUM(CASE WHEN event_type IN ('user_query', 'llm_response') AND token_usage IS NOT NULL \
                         THEN COALESCE(token_input, 0) ELSE 0 END), 0) AS SIGNED) AS total_tokens_in, \
                       CAST(COALESCE(SUM(CASE WHEN event_type IN ('user_query', 'llm_response') AND token_usage IS NOT NULL \
                         THEN COALESCE(token_output, 0) ELSE 0 END), 0) AS SIGNED) AS total_tokens_out \
                FROM agent_events \
                GROUP BY user_id, session_id \
              ) event_summary \
                ON event_summary.user_id = s.user_id AND event_summary.session_id = s.session_id \
              LEFT JOIN ( \
                SELECT user_id, session_id, COUNT(*) AS checkpoint_count \
                FROM session_checkpoints \
                WHERE state_json IS NULL \
                GROUP BY user_id, session_id \
              ) checkpoint_summary \
                ON checkpoint_summary.user_id = s.user_id AND checkpoint_summary.session_id = s.session_id \
              LEFT JOIN ( \
                SELECT user_id, session_id, llm_model_used \
                FROM ( \
                  SELECT user_id, session_id, llm_model_used, \
                         ROW_NUMBER() OVER (PARTITION BY user_id, session_id ORDER BY created_at DESC, event_id DESC) AS rn \
                  FROM agent_events \
                  WHERE llm_model_used IS NOT NULL AND llm_model_used != '' \
                ) ranked_models \
                WHERE rn = 1 \
              ) latest_model \
                ON latest_model.user_id = s.user_id AND latest_model.session_id = s.session_id \
              WHERE s.session_id = ? AND s.user_id = ?",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("restore_cloud_session: {e}"))?;
        let session_query_ms = elapsed_ms(session_query_started_at);

        match row {
            Some(row) => {
                let status = mysql_string(&row, "restore_cloud_session", "status")?;
                let title = mysql_optional_string(&row, "restore_cloud_session", "title")?;
                let turn_count = mysql_i64(&row, "restore_cloud_session", "turn_count")?;
                let total_tokens_in = mysql_i64(&row, "restore_cloud_session", "total_tokens_in")?;
                let total_tokens_out =
                    mysql_i64(&row, "restore_cloud_session", "total_tokens_out")?;
                let checkpoint_count =
                    mysql_i64(&row, "restore_cloud_session", "checkpoint_count")?;
                let (total_cache_read_tokens, total_cache_creation_tokens) =
                    restore_cloud_cache_token_totals(pool, user_id, session_id).await?;

                // Extract plan state from metadata JSON
                let metadata_str =
                    mysql_optional_string(&row, "restore_cloud_session", "metadata_json")?;
                let metadata_state = metadata_json_state(metadata_str.as_deref())?;

                let heavy_started_at = std::time::Instant::now();
                let heavy_state = self
                    .restore_latest_heavy_checkpoint_state(user_id, session_id)
                    .await?;
                let heavy_checkpoint_ms = elapsed_ms(heavy_started_at);

                let transcript_started_at = std::time::Instant::now();
                let transcript_messages = match heavy_state.as_ref() {
                    Some(heavy) if !heavy.messages.is_empty() => Vec::new(),
                    _ => {
                        self.restore_cloud_transcript_messages(user_id, session_id)
                            .await?
                    }
                };
                let transcript_ms = elapsed_ms(transcript_started_at);

                let context_trace_started_at = std::time::Instant::now();
                let last_context_trace = self
                    .restore_latest_context_trace_signal(user_id, session_id)
                    .await?;
                let context_trace_ms = elapsed_ms(context_trace_started_at);

                let recent_tools_started_at = std::time::Instant::now();
                let mut recent_tools = self.restore_recent_tools(user_id, session_id).await?;
                if recent_tools.is_empty()
                    && let Some(heavy) = heavy_state.as_ref()
                {
                    append_unique_names(
                        &mut recent_tools,
                        heavy.recent_tools.iter().map(String::as_str),
                    );
                }
                if recent_tools.is_empty() {
                    recent_tools = recent_tools_from_context_trace(last_context_trace.as_ref());
                }
                let recent_tools_ms = elapsed_ms(recent_tools_started_at);

                let latest_model = astra_core::model_override::normalize_model_override_owned(
                    mysql_optional_string(&row, "restore_cloud_session", "latest_model")?,
                );
                let model = metadata_state.model.clone().or(latest_model);

                // Load active contract from task_contracts table
                let contract_started_at = std::time::Instant::now();
                let mut contract_json =
                    Self::load_cloud_contract(pool, user_id, session_id).await?;
                let contract_ms = elapsed_ms(contract_started_at);

                // Fallback: try latest checkpoint's contract state
                let checkpoint_fallback_started_at = std::time::Instant::now();
                if contract_json.is_none() {
                    let ckpts = self.cloud_checkpoints(user_id, session_id).await?;
                    contract_json = ckpts
                        .iter()
                        .rev()
                        .find_map(|c| c.contract_state_json.clone());
                }
                let checkpoint_fallback_ms = elapsed_ms(checkpoint_fallback_started_at);

                CloudRestoreTimings {
                    session_query_ms,
                    heavy_checkpoint_ms,
                    transcript_ms,
                    context_trace_ms,
                    recent_tools_ms,
                    contract_ms,
                    checkpoint_fallback_ms,
                    total_ms: elapsed_ms(started_at),
                }
                .emit(session_id, true);

                Ok(Some(RestoredSession {
                    session_id: session_id.to_string(),
                    turn_count: non_negative_i64_to_u32(
                        turn_count,
                        "restore_cloud_session",
                        "turn_count",
                    )?,
                    total_tokens_in: non_negative_i64_to_u64(
                        total_tokens_in,
                        "restore_cloud_session",
                        "total_tokens_in",
                    )?,
                    total_tokens_out: non_negative_i64_to_u64(
                        total_tokens_out,
                        "restore_cloud_session",
                        "total_tokens_out",
                    )?,
                    total_cache_read_tokens: non_negative_i64_to_u64(
                        total_cache_read_tokens,
                        "restore_cloud_session",
                        "total_cache_read_tokens",
                    )?,
                    total_cache_creation_tokens: non_negative_i64_to_u64(
                        total_cache_creation_tokens,
                        "restore_cloud_session",
                        "total_cache_creation_tokens",
                    )?,
                    last_status: status,
                    title,
                    restored_from_cloud: true,
                    recent_tools,
                    checkpoint_count: non_negative_i64_to_u32(
                        checkpoint_count,
                        "restore_cloud_session",
                        "checkpoint_count",
                    )?,
                    git_branch: metadata_state.git_branch.clone(),
                    model,
                    permission_mode: metadata_state.permission_mode.clone(),
                    conversation_messages: heavy_state
                        .as_ref()
                        .map(|heavy| heavy.messages.clone())
                        .filter(|messages| !messages.is_empty())
                        .unwrap_or(transcript_messages),
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
                    pipeline_state: heavy_state
                        .as_ref()
                        .and_then(|heavy| heavy.pipeline_state.clone()),
                    executing_plan_json: metadata_state.executing_plan_json,
                    plan_goal: metadata_state.plan_goal,
                    plan_config_json: metadata_state.plan_config_json,
                    plan_execution_rounds: metadata_state.plan_execution_rounds,
                    contract_json,
                    last_context_trace,
                    ..Default::default()
                }))
            }
            None => {
                CloudRestoreTimings {
                    session_query_ms,
                    total_ms: elapsed_ms(started_at),
                    ..Default::default()
                }
                .emit(session_id, false);
                Ok(None)
            }
        }
    }

    /// Load the active contract for this session from cloud task_contracts table.
    /// Returns the contract as serialized JSON (matching local workspace format).
    async fn load_cloud_contract(
        pool: &sqlx::Pool<sqlx::MySql>,
        user_id: &str,
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
             WHERE session_id = ? AND user_id = ? AND status = 'active' \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("load_cloud_contract: {e}"))?;

        match row {
            Some(row) => {
                // Reconstruct contract as JSON matching TaskContract serde format
                let contract_id = mysql_string(&row, "load_cloud_contract", "contract_id")?;
                let task_id = mysql_string(&row, "load_cloud_contract", "task_id")?;
                let goal = mysql_string(&row, "load_cloud_contract", "goal")?;
                let version = mysql_i32(&row, "load_cloud_contract", "version")?;
                let status = mysql_string(&row, "load_cloud_contract", "status")?;
                let created_at = mysql_string(&row, "load_cloud_contract", "created_at")?;
                let updated_at = mysql_string(&row, "load_cloud_contract", "updated_at")?;
                let scope_json = mysql_optional_string(&row, "load_cloud_contract", "scope_json")?;
                let subtasks_json = mysql_string(&row, "load_cloud_contract", "subtasks_json")?;
                let criteria_json = mysql_string(&row, "load_cloud_contract", "criteria_json")?;

                // Parse sub-objects so serde round-trips correctly
                let scope: serde_json::Value = match scope_json.as_deref() {
                    Some(json) if !json.trim().is_empty() => {
                        serde_json::from_str(json).map_err(|e| format!("parse scope: {e}"))?
                    }
                    _ => serde_json::json!({}),
                };
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

    /// Restore recent tools from recent cloud checkpoints.
    async fn restore_recent_tools(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Vec<String>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        let checkpoint_rows = sqlx::query(
            "SELECT CAST(tools_json AS CHAR) AS tools_json FROM session_checkpoints \
             WHERE user_id = ? AND session_id = ? AND state_json IS NULL \
             ORDER BY number DESC LIMIT 5",
        )
        .bind(user_id)
        .bind(session_id)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("restore_recent_tools: {e}"))?;

        let mut tools = Vec::new();
        for row in &checkpoint_rows {
            let Some(tools_json) =
                mysql_optional_string(row, "restore_recent_tools", "tools_json")?
            else {
                continue;
            };
            if tools_json.trim().is_empty() {
                continue;
            }
            let used = serde_json::from_str::<Vec<String>>(&tools_json)
                .map_err(|e| format!("restore_recent_tools: parse tools_json: {e}"))?;
            append_unique_names(&mut tools, used.iter().map(String::as_str));
        }

        Ok(tools)
    }

    /// Restore the latest structured context-trace signal from cloud events.
    async fn restore_latest_context_trace_signal(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<super::session_workspace::ContextTraceSignal>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(None),
        };

        let row = sqlx::query(
            "SELECT CAST(metadata AS CHAR) AS metadata_json FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND event_type = 'context_trace_signal' \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("restore_latest_context_trace_signal: {e}"))?;

        let Some(row) = row else {
            return Ok(None);
        };
        let Some(metadata_json) =
            mysql_optional_string(&row, "restore_latest_context_trace_signal", "metadata_json")?
        else {
            return Ok(None);
        };
        if metadata_json.trim().is_empty() {
            return Ok(None);
        }
        serde_json::from_str(&metadata_json)
            .map(Some)
            .map_err(|e| format!("restore_latest_context_trace_signal: parse metadata_json: {e}"))
    }

    async fn restore_latest_heavy_checkpoint_state(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<CloudHeavyCheckpointState>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(None),
        };

        let Some(state_json) = pull_step_checkpoint_from_cloud(pool, user_id, session_id).await?
        else {
            return Ok(None);
        };
        parse_cloud_heavy_checkpoint_state(&state_json)
    }

    async fn restore_cloud_transcript_messages(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Vec<serde_json::Value>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        let rows = sqlx::query(PROMPT_HISTORY_TRANSCRIPT_SELECT_SQL)
            .bind(session_id)
            .bind(user_id)
            .bind(MAX_PROMPT_HISTORY_TRANSCRIPT_ROWS)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("restore_cloud_transcript_messages: {e}"))?;

        let mut messages = Vec::new();
        for row in &rows {
            let role = mysql_string(row, "restore_cloud_transcript_messages", "role")?;
            let content = mysql_string(row, "restore_cloud_transcript_messages", "content")?;
            if content.trim().is_empty() {
                continue;
            }
            match role.as_str() {
                "user" | "assistant" | "system" | "tool" => messages.push(serde_json::json!({
                    "role": role,
                    "content": content,
                })),
                _ => {}
            }
        }
        Ok(messages)
    }

    /// List checkpoints from MatrixOne.
    async fn cloud_checkpoints(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Vec<RestoredCheckpoint>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        let rows = sqlx::query(CLOUD_CHECKPOINTS_SELECT_SQL)
            .bind(user_id)
            .bind(session_id)
            .bind(MAX_CLOUD_RESTORE_CHECKPOINTS)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("cloud_checkpoints: {e}"))?;

        let ckpts = rows
            .iter()
            .map(|row| {
                Ok(RestoredCheckpoint {
                    number: non_negative_i32_to_u32(
                        mysql_i32(row, "cloud_checkpoints", "number")?,
                        "cloud_checkpoints",
                        "number",
                    )?,
                    turn: non_negative_i32_to_u32(
                        mysql_i32(row, "cloud_checkpoints", "turn")?,
                        "cloud_checkpoints",
                        "turn",
                    )?,
                    title: mysql_string(row, "cloud_checkpoints", "title")?,
                    summary: mysql_optional_string(row, "cloud_checkpoints", "summary")?
                        .unwrap_or_default(),
                    total_tokens: non_negative_i64_to_u64(
                        mysql_i64(row, "cloud_checkpoints", "total_tokens")?,
                        "cloud_checkpoints",
                        "total_tokens",
                    )?,
                    contract_state_json: mysql_optional_string(
                        row,
                        "cloud_checkpoints",
                        "contract_state_json",
                    )?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(ckpts)
    }

    async fn cloud_checkpoint_count(&self, user_id: &str, session_id: &str) -> Result<u32, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(0),
        };
        let row = sqlx::query(CLOUD_CHECKPOINT_COUNT_SQL)
            .bind(user_id)
            .bind(session_id)
            .fetch_one(pool)
            .await
            .map_err(|e| format!("cloud_checkpoint_count: {e}"))?;
        let count = mysql_i64(&row, "cloud_checkpoint_count", "checkpoint_count")?;
        non_negative_i64_to_u32(count, "cloud_checkpoint_count", "checkpoint_count")
    }
}

fn local_checkpoint_count(
    user_id: Option<&str>,
    session_id: &str,
    context: &str,
) -> Result<u32, String> {
    let count = match user_id {
        // Authenticated counts come from cloud checkpoint rows. The runtime
        // local step format is owned by `astra-pipeline`; the services crate
        // must not duplicate that codec or reverse-depend on runtime layers.
        Some(_) => 0,
        None => super::session_checkpoint::read_checkpoint_index(session_id)
            .map_err(|error| {
                format!(
                    "{context}: failed to read local checkpoint index for {session_id}: {error}"
                )
            })?
            .len(),
    };
    u32::try_from(count).map_err(|_| {
        format!(
            "{context}: local checkpoint index for {session_id} has too many entries: {}",
            count
        )
    })
}

fn composite_snapshot_index_to_remote_artifact_record(
    session_id: &str,
    user_id: &str,
    index: &astra_core::composite_snapshot::CompositeSnapshotIndex,
) -> Result<SessionArtifactJsonRecord, serde_json::Error> {
    Ok(SessionArtifactJsonRecord {
        artifact_id: COMPOSITE_SNAPSHOT_INDEX_PROJECTION_ID.to_string(),
        session_id: session_id.to_string(),
        user_id: user_id.to_string(),
        artifact_kind: COMPOSITE_SNAPSHOT_INDEX_ARTIFACT_KIND.to_string(),
        source: Some("composite_snapshot_index".to_string()),
        turn: index.snapshots.last().map(|snapshot| snapshot.turn),
        round: None,
        content: serde_json::to_value(index)?,
        metadata: Some(json!({
            "snapshot_count": index.snapshots.len(),
            "latest_version": index.current_version(),
        })),
        references: Vec::new(),
    })
}

pub async fn persist_remote_composite_snapshot_index(
    session_id: &str,
    user_id: &str,
    index: &astra_core::composite_snapshot::CompositeSnapshotIndex,
    store: &impl SessionArtifactJsonStore,
) -> Result<StoredSessionArtifact, String> {
    let record = composite_snapshot_index_to_remote_artifact_record(session_id, user_id, index)
        .map_err(|error| error.to_string())?;
    store
        .upsert_json_artifact_projection(record)
        .await
        .map_err(|error| error.to_string())
}

fn merge_composite_snapshot_indexes(
    local: astra_core::composite_snapshot::CompositeSnapshotIndex,
    remote: astra_core::composite_snapshot::CompositeSnapshotIndex,
) -> astra_core::composite_snapshot::CompositeSnapshotIndex {
    let mut merged = BTreeMap::new();
    for snapshot in local.snapshots {
        merged.insert(snapshot.snapshot_id.clone(), snapshot);
    }
    for snapshot in remote.snapshots {
        merged.insert(snapshot.snapshot_id.clone(), snapshot);
    }
    let mut snapshots: Vec<_> = merged.into_values().collect();
    snapshots.sort_by(|left, right| {
        (
            left.version == 0,
            left.version,
            left.created_at.as_str(),
            left.snapshot_id.as_str(),
        )
            .cmp(&(
                right.version == 0,
                right.version,
                right.created_at.as_str(),
                right.snapshot_id.as_str(),
            ))
    });
    let mut index = astra_core::composite_snapshot::CompositeSnapshotIndex { snapshots };
    index.normalize_versions();
    index
}

fn merge_composite_snapshot_sources(
    session_id: &str,
    local: astra_core::composite_snapshot::CompositeSnapshotIndex,
    remote: Result<Option<astra_core::composite_snapshot::CompositeSnapshotIndex>, String>,
) -> Result<astra_core::composite_snapshot::CompositeSnapshotIndex, String> {
    let remote = remote
        .map_err(|error| {
            format!(
                "list_composite_snapshots: failed to read remote composite snapshot index for {session_id}: {error}"
            )
        })?
        .unwrap_or_default();
    Ok(merge_composite_snapshot_indexes(local, remote))
}

/// Parse checkpoint number from a heavy checkpoint filename ref (e.g. `000005-heavy.json`).
fn parse_heavy_checkpoint_number(session_state_ref: &str) -> Option<u32> {
    session_state_ref
        .strip_suffix("-heavy.json")
        .and_then(|prefix| prefix.parse().ok())
}

fn recent_tools_from_context_trace(
    trace: Option<&super::session_workspace::ContextTraceSignal>,
) -> Vec<String> {
    let mut tools = Vec::new();
    if let Some(visible_tools) = trace
        .and_then(|signal| signal.tool_surface.as_ref())
        .map(|selection| selection.visible_tools.iter().map(String::as_str))
    {
        append_unique_names(&mut tools, visible_tools);
    }
    tools
}

fn parse_local_checkpoint_entries(local_entries: &[String]) -> Vec<RestoredCheckpoint> {
    local_entries
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
        .collect()
}

fn merge_checkpoints(
    local: Vec<RestoredCheckpoint>,
    cloud: Vec<RestoredCheckpoint>,
) -> Vec<RestoredCheckpoint> {
    let mut merged = BTreeMap::new();
    for checkpoint in local {
        merged.insert(checkpoint.number, checkpoint);
    }
    for checkpoint in cloud {
        merged.insert(checkpoint.number, checkpoint);
    }
    merged.into_values().collect()
}

#[derive(Debug, Clone, Default)]
struct LocalJournalSummary {
    turn_count: u32,
    total_tokens_in: u64,
    total_tokens_out: u64,
    total_cache_read_tokens: u64,
    total_cache_creation_tokens: u64,
    recent_tools: Vec<String>,
    model: Option<String>,
    permission_mode: Option<String>,
    last_status: String,
}

#[derive(Debug, Clone)]
struct CloudWorkspaceArtifact {
    metadata: super::session_workspace::WorkspaceMetadata,
}

fn summarize_local_journal(
    user_id: Option<&str>,
    session_id: &str,
) -> Result<Option<LocalJournalSummary>, String> {
    let digest = match user_id {
        Some(user_id) => {
            crate::session_journal::read_journal_for_digest_for_user(user_id, session_id)
        }
        None => crate::session_journal::read_journal_for_digest(session_id),
    };
    let (events, _, _) = match digest {
        Ok(digest) => digest,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "failed to read session journal for {session_id}: {error}"
            ));
        }
    };
    if events.is_empty() {
        return Ok(None);
    }

    let mut summary = LocalJournalSummary {
        last_status: "local".to_string(),
        ..Default::default()
    };
    let mut latest_turn_index: Option<usize> = None;

    for (idx, event) in events.iter().enumerate() {
        if let Some(mode) = event
            .edge_policy
            .as_ref()
            .and_then(|policy| policy.permission_mode.clone())
        {
            summary.permission_mode = Some(mode);
        } else if event.event_type == crate::session_journal::JournalEventType::PermissionAudit
            && event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("kind"))
                .and_then(|kind| kind.as_str())
                == Some("permission_mode_changed")
            && let Some(mode) = event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("to_mode"))
                .and_then(|mode| mode.as_str())
        {
            summary.permission_mode = Some(mode.to_string());
        }

        match event.event_type {
            crate::session_journal::JournalEventType::Turn => {
                summary.turn_count += 1;
                summary.total_tokens_in += event.tokens_in.unwrap_or(0);
                summary.total_tokens_out += event.tokens_out.unwrap_or(0);
                summary.total_cache_read_tokens += event.cache_read_tokens.unwrap_or(0);
                summary.total_cache_creation_tokens += event.cache_creation_tokens.unwrap_or(0);
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
            append_unique_names(
                &mut summary.recent_tools,
                tools_used.iter().map(String::as_str),
            );
        }
        if summary.recent_tools.is_empty()
            && let Some(tool_calls) = event.tool_calls.as_ref()
        {
            append_unique_names(
                &mut summary.recent_tools,
                tool_calls.iter().map(|call| call.name.as_str()),
            );
        }
    }

    Ok(Some(summary))
}

fn restored_session_from_workspace(
    ws: super::session_workspace::WorkspaceMetadata,
    local_journal: Option<&LocalJournalSummary>,
    recent_tools: Vec<String>,
    checkpoint_count: u32,
    restored_from_cloud: bool,
) -> RestoredSession {
    let workspace = ws.clone();
    let turn_count = local_journal
        .map(|summary| ws.turn_count.max(summary.turn_count))
        .unwrap_or(ws.turn_count);
    let total_tokens_in = local_journal
        .map(|summary| ws.total_tokens_in.max(summary.total_tokens_in))
        .unwrap_or(ws.total_tokens_in);
    let total_tokens_out = local_journal
        .map(|summary| ws.total_tokens_out.max(summary.total_tokens_out))
        .unwrap_or(ws.total_tokens_out);
    let total_cache_read_tokens = local_journal
        .map(|summary| {
            ws.total_cache_read_tokens
                .max(summary.total_cache_read_tokens)
        })
        .unwrap_or(ws.total_cache_read_tokens);
    let total_cache_creation_tokens = local_journal
        .map(|summary| {
            ws.total_cache_creation_tokens
                .max(summary.total_cache_creation_tokens)
        })
        .unwrap_or(ws.total_cache_creation_tokens);
    let permission_mode = ws
        .permission_mode
        .clone()
        .or_else(|| local_journal.and_then(|summary| summary.permission_mode.clone()));
    let model = ws
        .model
        .as_deref()
        .and_then(|model| astra_core::model_override::normalize_model_override(Some(model)))
        .map(str::to_string)
        .or_else(|| local_journal.and_then(|summary| summary.model.clone()));

    RestoredSession {
        session_id: ws.session_id.clone(),
        turn_count,
        total_tokens_in,
        total_tokens_out,
        total_cache_read_tokens,
        total_cache_creation_tokens,
        recent_tools,
        checkpoint_count,
        last_status: ws.status.clone(),
        git_branch: ws.git_branch.clone(),
        model,
        permission_mode,
        title: None,
        restored_from_cloud,
        executing_plan_json: ws.executing_plan_json.clone(),
        plan_goal: ws.plan_goal.clone(),
        plan_config_json: ws.plan_config_json.clone(),
        plan_execution_rounds: ws.plan_execution_rounds,
        contract_json: ws.contract_json.clone(),
        plan_corrections: ws.plan_corrections.clone(),
        last_context_trace: ws.last_context_trace.clone(),
        workspace: Some(workspace),
        ..Default::default()
    }
}

fn cloud_heavy_payload(
    root: &serde_json::Value,
) -> Result<Option<&serde_json::Map<String, serde_json::Value>>, String> {
    let Some(heavy) = root.get("Heavy") else {
        return Ok(None);
    };
    heavy.as_object().map(Some).ok_or_else(|| {
        "invalid cloud heavy checkpoint JSON field: field=Heavy, expected=object".to_string()
    })
}

fn required_cloud_heavy_field<T>(
    heavy: &serde_json::Map<String, serde_json::Value>,
    field: &'static str,
) -> Result<T, String>
where
    T: DeserializeOwned,
{
    let value = heavy
        .get(field)
        .ok_or_else(|| format!("missing cloud heavy checkpoint JSON field: field={field}"))?;
    if value.is_null() {
        return Err(format!(
            "invalid cloud heavy checkpoint JSON field: field={field}, expected=non-null"
        ));
    }
    serde_json::from_value(value.clone()).map_err(|source| {
        format!("invalid cloud heavy checkpoint JSON field: field={field}, source={source}")
    })
}

/// Parse cloud step-checkpoint JSON into the heavy-state fields needed for restore.
/// Accepts only the current externally tagged `{"Heavy": ...}` shape.
pub fn parse_cloud_heavy_checkpoint_state(
    state_json: &str,
) -> Result<Option<CloudHeavyCheckpointState>, String> {
    let root = serde_json::from_str::<serde_json::Value>(state_json)
        .map_err(|source| format!("invalid cloud heavy checkpoint JSON: source={source}"))?;
    let Some(heavy) = cloud_heavy_payload(&root)? else {
        return Ok(None);
    };
    let recent_tools: Vec<String> = required_cloud_heavy_field(heavy, "recent_tools")?;
    Ok(Some(CloudHeavyCheckpointState {
        messages: required_cloud_heavy_field(heavy, "messages")?,
        blocked_tools: required_cloud_heavy_field(heavy, "blocked_tools")?,
        recent_tools: normalize_name_list(recent_tools),
        approval_overrides: heavy
            .get("approval_overrides")
            .cloned()
            .filter(|v| !v.is_null()),
        interruption: heavy.get("interruption").cloned().filter(|v| !v.is_null()),
        compaction_state: heavy
            .get("compaction_state")
            .cloned()
            .filter(|v| !v.is_null()),
        pipeline_state: heavy
            .get("pipeline_state")
            .cloned()
            .filter(|v| !v.is_null()),
    }))
}

#[async_trait]
impl SessionRestoreService for HybridRestoreService {
    async fn restore_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<RestoredSession>, String> {
        self.restore_session_inner(Some(user_id), session_id).await
    }

    async fn list_checkpoints(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Vec<RestoredCheckpoint>, String> {
        self.list_checkpoints_inner(Some(user_id), session_id).await
    }

    async fn restore_to_checkpoint(
        &self,
        user_id: &str,
        session_id: &str,
        checkpoint_number: u32,
    ) -> Result<Option<RestoredSession>, String> {
        let session = match self.restore_session(user_id, session_id).await? {
            Some(s) => s,
            None => return Ok(None),
        };
        let checkpoints = self.list_checkpoints(user_id, session_id).await?;
        Self::apply_checkpoint(session_id, session, &checkpoints, checkpoint_number)
    }

    async fn list_resumable_sessions(&self, user_id: &str) -> Result<Vec<RestoredSession>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        let rows = sqlx::query(
            "SELECT s.session_id, s.title, s.status, CAST(s.metadata AS CHAR) AS metadata_json, \
         COALESCE(event_summary.turn_count, 0) AS turn_count, \
         latest_model.llm_model_used AS latest_model \
         FROM agent_sessions s \
         LEFT JOIN ( \
           SELECT user_id, session_id, COALESCE(MAX(turn_seq), 0) AS turn_count \
           FROM agent_events \
           GROUP BY user_id, session_id \
         ) event_summary \
           ON event_summary.user_id = s.user_id AND event_summary.session_id = s.session_id \
         LEFT JOIN ( \
           SELECT user_id, session_id, llm_model_used \
           FROM ( \
             SELECT user_id, session_id, llm_model_used, \
                    ROW_NUMBER() OVER (PARTITION BY user_id, session_id ORDER BY created_at DESC, event_id DESC) AS rn \
             FROM agent_events \
             WHERE llm_model_used IS NOT NULL AND llm_model_used != '' \
           ) ranked_models \
           WHERE rn = 1 \
         ) latest_model \
           ON latest_model.user_id = s.user_id AND latest_model.session_id = s.session_id \
         WHERE s.user_id = ? AND s.status IN ('active', 'paused') \
         ORDER BY s.updated_at DESC LIMIT 20",
        )
        .bind(user_id)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("list_resumable: {e}"))?;

        let mut sessions = Vec::new();
        for row in &rows {
            let session_id = mysql_string(row, "list_resumable_sessions", "session_id")?;
            let title = mysql_optional_string(row, "list_resumable_sessions", "title")?;
            let status = mysql_string(row, "list_resumable_sessions", "status")?;
            let turn_count = mysql_i64(row, "list_resumable_sessions", "turn_count")?;
            let metadata_json =
                mysql_optional_string(row, "list_resumable_sessions", "metadata_json")?;
            let metadata_state = metadata_json_state(metadata_json.as_deref())?;
            let latest_model = astra_core::model_override::normalize_model_override_owned(
                mysql_optional_string(row, "list_resumable_sessions", "latest_model")?,
            );
            let model = metadata_state.model.clone().or(latest_model);
            let mut restored = if let Some(workspace) =
                self.restore_cloud_workspace(user_id, &session_id).await?
            {
                let mut recent_tools = self.restore_recent_tools(user_id, &session_id).await?;
                if recent_tools.is_empty() {
                    recent_tools = recent_tools_from_context_trace(
                        workspace.metadata.last_context_trace.as_ref(),
                    );
                }
                let checkpoint_count = self
                    .cloud_checkpoint_count(user_id, &session_id)
                    .await
                    .map_err(|e| format!("list_resumable_sessions checkpoint count: {e}"))?;
                restored_session_from_workspace(
                    workspace.metadata,
                    None,
                    recent_tools,
                    checkpoint_count,
                    true,
                )
            } else {
                RestoredSession {
                    session_id: session_id.clone(),
                    restored_from_cloud: true,
                    ..Default::default()
                }
            };
            restored.turn_count = restored.turn_count.max(non_negative_i64_to_u32(
                turn_count,
                "list_resumable_sessions",
                "turn_count",
            )?);
            restored.last_status = status;
            restored.title = restored.title.or(title);
            if restored.git_branch.is_none() {
                restored.git_branch = metadata_state.git_branch.clone();
            }
            if restored.model.is_none() {
                restored.model = model;
            }
            if restored.permission_mode.is_none() {
                restored.permission_mode = metadata_state.permission_mode.clone();
            }
            restored.restored_from_cloud = true;
            sessions.push(restored);
        }
        Ok(sessions)
    }

    async fn restore_to_composite_snapshot(
        &self,
        user_id: &str,
        session_id: &str,
        snapshot_id: &str,
        selector: &astra_core::composite_snapshot::RestoreSelector,
    ) -> Result<Option<RestoredCompositeState>, String> {
        let index = self.list_composite_snapshots(user_id, session_id).await?;
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
            match self
                .restore_to_checkpoint(user_id, session_id, ckpt_num)
                .await
            {
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
        user_id: &str,
        session_id: &str,
    ) -> Result<astra_core::composite_snapshot::CompositeSnapshotIndex, String> {
        if !self
            .require_owned_cloud_session(user_id, session_id)
            .await?
        {
            return Ok(astra_core::composite_snapshot::CompositeSnapshotIndex::default());
        }
        // Runtime pushes this mutable projection after every successful local
        // index update. The authenticated services view therefore reads the
        // owner-scoped remote projection only.
        let local = astra_core::composite_snapshot::CompositeSnapshotIndex::default();
        let remote = self
            .restore_cloud_composite_snapshot_index(user_id, session_id)
            .await;
        merge_composite_snapshot_sources(session_id, local, remote)
    }
}

// ─── MatrixOneSyncService push methods ─────────────────────────────────────

impl crate::state_sync::MatrixOneSyncService {
    async fn reject_foreign_real_session_for_session_state(
        &self,
        session_id: &str,
        user_id: &str,
    ) -> Result<(), String> {
        let rows = sqlx::query(
            "SELECT CAST(metadata AS CHAR) AS metadata_json \
             FROM agent_sessions WHERE session_id = ? AND user_id <> ?",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("load foreign session metadata: {e}"))?;

        for row in rows {
            let metadata_json =
                mysql_optional_string(&row, "push_session_state_foreign_owner", "metadata_json")?;
            if !session_state_sync_metadata_marker_present(metadata_json.as_deref())? {
                return Err("push_session_state: session_id belongs to another owner".to_string());
            }
        }
        Ok(())
    }

    /// Push a checkpoint to MatrixOne for cross-device availability.
    pub async fn push_checkpoint(
        &self,
        session_id: &str,
        user_id: &str,
        checkpoint: &super::session_checkpoint::Checkpoint,
    ) -> Result<(), String> {
        let started_at = std::time::Instant::now();
        let checkpoint_id = uuid::Uuid::new_v4().to_string();
        let tools_json = checkpoint_tools_json(checkpoint);

        let payload_size = checkpoint.title.len()
            + checkpoint.summary.len()
            + tools_json.len()
            + checkpoint
                .contract_state_json
                .as_ref()
                .map_or(0, |s| s.len());

        let log_result = |status: &str, error_msg: Option<&str>| {
            log_checkpoint_sync(
                &self.audit,
                checkpoint.number,
                SessionSyncLogEntry {
                    user_id,
                    session_id,
                    sync_type: "checkpoint",
                    payload_size,
                    duration_ms: Some(elapsed_ms(started_at)),
                    status,
                    error_msg,
                },
            );
        };

        match crate::storage::agent_session_exists_for_user(&self.pool, session_id, user_id).await {
            Ok(true) => {}
            Ok(false) => {
                let err = "push_checkpoint owner mismatch".to_string();
                log_result("error", Some(&err));
                return Err(err);
            }
            Err(e) => {
                let err = format!("push_checkpoint owner check: {e}");
                log_result("error", Some(&err));
                return Err(err);
            }
        }

        let updated = match sqlx::query(
            "UPDATE session_checkpoints SET \
                turn = ?, title = ?, summary = ?, tools_json = ?, total_tokens = ?, \
                had_stalls = ?, error_count = ?, contract_state_json = ? \
             WHERE user_id = ? AND session_id = ? AND number = ?",
        )
        .bind(checkpoint.turn as i32)
        .bind(&checkpoint.title)
        .bind(&checkpoint.summary)
        .bind(&tools_json)
        .bind(checkpoint.total_tokens as i64)
        .bind(if checkpoint.had_stalls { 1i32 } else { 0 })
        .bind(checkpoint.error_count as i32)
        .bind(&checkpoint.contract_state_json)
        .bind(user_id)
        .bind(session_id)
        .bind(checkpoint.number as i32)
        .execute(&self.pool)
        .await
        {
            Ok(u) => u,
            Err(e) => {
                let err = format!("push_checkpoint update: {e}");
                log_result("error", Some(&err));
                return Err(err);
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
            .execute(&self.pool)
            .await;

            if let Err(e) = inserted {
                if is_duplicate_key_error(&e) {
                    let retry = sqlx::query(
                        "UPDATE session_checkpoints SET \
                            turn = ?, title = ?, summary = ?, tools_json = ?, total_tokens = ?, \
                            had_stalls = ?, error_count = ?, contract_state_json = ? \
             WHERE user_id = ? AND session_id = ? AND number = ?",
                    )
                    .bind(checkpoint.turn as i32)
                    .bind(&checkpoint.title)
                    .bind(&checkpoint.summary)
                    .bind(&tools_json)
                    .bind(checkpoint.total_tokens as i64)
                    .bind(if checkpoint.had_stalls { 1i32 } else { 0 })
                    .bind(checkpoint.error_count as i32)
                    .bind(&checkpoint.contract_state_json)
                    .bind(user_id)
                    .bind(session_id)
                    .bind(checkpoint.number as i32)
                    .execute(&self.pool)
                    .await;
                    match retry {
                        Ok(updated) if updated.rows_affected() > 0 => {}
                        Ok(_) => {
                            let err = "push_checkpoint owner mismatch".to_string();
                            log_result("error", Some(&err));
                            return Err(err);
                        }
                        Err(e) => {
                            let err = format!("push_checkpoint retry update: {e}");
                            log_result("error", Some(&err));
                            return Err(err);
                        }
                    }
                } else {
                    let err = format!("push_checkpoint insert: {e}");
                    log_result("error", Some(&err));
                    return Err(err);
                }
            }
        }

        log_result("success", None);
        Ok(())
    }

    /// Push a Step Protocol checkpoint to MatrixOne with full state_json.
    #[allow(clippy::too_many_arguments)]
    pub async fn push_step_checkpoint(
        &self,
        session_id: &str,
        user_id: &str,
        checkpoint_number: u32,
        turn: u32,
        tier: &str,
        title: &str,
        tools_json: &str,
        state_json: &str,
    ) -> Result<(), String> {
        let started_at = std::time::Instant::now();
        let checkpoint_id = uuid::Uuid::new_v4().to_string();
        let cloud_number = cloud_step_checkpoint_number(checkpoint_number)?;
        let payload_size = title.len() + tier.len() + tools_json.len() + state_json.len();

        let log_result = |status: &str, error_msg: Option<&str>| {
            log_checkpoint_sync(
                &self.audit,
                checkpoint_number,
                SessionSyncLogEntry {
                    user_id,
                    session_id,
                    sync_type: "step_checkpoint",
                    payload_size,
                    duration_ms: Some(elapsed_ms(started_at)),
                    status,
                    error_msg,
                },
            );
        };

        match crate::storage::agent_session_exists_for_user(&self.pool, session_id, user_id).await {
            Ok(true) => {}
            Ok(false) => {
                let err = "push_step_checkpoint owner mismatch".to_string();
                log_result("error", Some(&err));
                return Err(err);
            }
            Err(e) => {
                let err = format!("push_step_checkpoint owner check: {e}");
                log_result("error", Some(&err));
                return Err(err);
            }
        }

        let updated = match sqlx::query(
            "UPDATE session_checkpoints SET \
                turn = ?, title = ?, summary = ?, tools_json = ?, state_json = ? \
	             WHERE user_id = ? AND session_id = ? AND number = ?",
        )
        .bind(turn as i32)
        .bind(title)
        .bind(tier)
        .bind(tools_json)
        .bind(state_json)
        .bind(user_id)
        .bind(session_id)
        .bind(cloud_number)
        .execute(&self.pool)
        .await
        {
            Ok(updated) => updated,
            Err(e) => {
                let err = format!("push_step_checkpoint update: {e}");
                log_result("error", Some(&err));
                return Err(err);
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
            .execute(&self.pool)
            .await;

            if let Err(e) = inserted {
                if is_duplicate_key_error(&e) {
                    let retry = sqlx::query(
                        "UPDATE session_checkpoints SET \
                            turn = ?, title = ?, summary = ?, tools_json = ?, state_json = ? \
                         WHERE user_id = ? AND session_id = ? AND number = ?",
                    )
                    .bind(turn as i32)
                    .bind(title)
                    .bind(tier)
                    .bind(tools_json)
                    .bind(state_json)
                    .bind(user_id)
                    .bind(session_id)
                    .bind(cloud_number)
                    .execute(&self.pool)
                    .await;
                    match retry {
                        Ok(updated) if updated.rows_affected() > 0 => {}
                        Ok(_) => {
                            let err = "push_step_checkpoint owner mismatch".to_string();
                            log_result("error", Some(&err));
                            return Err(err);
                        }
                        Err(error) => {
                            let err = format!("push_step_checkpoint retry update: {error}");
                            log_result("error", Some(&err));
                            return Err(err);
                        }
                    }
                } else {
                    let err = format!("push_step_checkpoint insert: {e}");
                    log_result("error", Some(&err));
                    return Err(err);
                }
            }
        }

        log_result("success", None);
        Ok(())
    }

    /// Push resumable session state to cloud via the agent_sessions.metadata JSON column.
    #[allow(clippy::too_many_arguments)]
    pub async fn push_session_state(
        &self,
        session_id: &str,
        user_id: &str,
        executing_plan_json: Option<&str>,
        plan_goal: Option<&str>,
        plan_config_json: Option<&str>,
        plan_execution_rounds: usize,
        git_branch: Option<&str>,
        model: Option<&str>,
    ) -> Result<(), String> {
        let started_at = std::time::Instant::now();

        let existing_metadata_row = sqlx::query(
            "SELECT CAST(metadata AS CHAR) AS metadata_json \
             FROM agent_sessions WHERE session_id = ? AND user_id = ? LIMIT 1",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("load session metadata: {e}"))?;
        let existing_metadata_json = existing_metadata_row
            .as_ref()
            .map(|row| mysql_optional_string(row, "push_session_state", "metadata_json"))
            .transpose()?
            .flatten();

        if existing_metadata_row.is_none() {
            self.reject_foreign_real_session_for_session_state(session_id, user_id)
                .await?;
        }

        let metadata_json = merge_session_state_metadata(
            existing_metadata_json.as_deref(),
            executing_plan_json,
            plan_goal,
            plan_config_json,
            plan_execution_rounds,
            git_branch,
            model,
        )?;
        let payload_size = metadata_json.len();

        let result = sqlx::query(PUSH_SESSION_STATE_UPSERT_SQL)
            .bind(session_id)
            .bind(user_id)
            .bind(&metadata_json)
            .execute(&self.pool)
            .await
            .and_then(|result| {
                if result.rows_affected() == 0 {
                    Err(sqlx::Error::RowNotFound)
                } else {
                    Ok(result)
                }
            })
            .map(|_| ())
            .map_err(|e| format!("push_session_state: {e}"));

        let (status, error_msg) = match &result {
            Ok(()) => ("success", None),
            Err(e) => ("error", Some(e.as_str())),
        };
        log_session_sync(
            &self.audit,
            SessionSyncLogEntry {
                user_id,
                session_id,
                sync_type: "session_state",
                payload_size,
                duration_ms: Some(elapsed_ms(started_at)),
                status,
                error_msg,
            },
        );

        result
    }

    /// Push a structured context-trace signal as a first-class cloud event.
    pub async fn push_context_trace_signal(
        &self,
        session_id: &str,
        user_id: &str,
        signal: &super::session_workspace::ContextTraceSignal,
    ) -> Result<(), String> {
        let started_at = std::time::Instant::now();
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

        let log_result = |status: &str, error_msg: Option<&str>| {
            log_session_sync(
                &self.audit,
                SessionSyncLogEntry {
                    user_id,
                    session_id,
                    sync_type: "context_trace",
                    payload_size,
                    duration_ms: Some(elapsed_ms(started_at)),
                    status,
                    error_msg,
                },
            );
        };

        let event_id = uuid::Uuid::now_v7().to_string();
        let mut tx = match self.pool.begin().await {
            Ok(tx) => tx,
            Err(e) => {
                let err = format!("push_context_trace_signal begin transaction: {e}");
                log_result("error", Some(&err));
                return Err(err);
            }
        };

        let insert_result = match sqlx::query(
            "INSERT INTO agent_events \
             (event_id, session_id, user_id, agent_id, agent_version, event_type, content, \
              parent_event_id, causal_chain_id, metadata, reasoning_content, meta_tool_name, \
              meta_duration_ms, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
        )
        .bind(&event_id)
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
                .tool_surface
                .as_ref()
                .and_then(|selection| selection.visible_tools.first().cloned()),
        )
        .bind(duration_ms)
        .execute(&mut *tx)
        .await
        {
            Ok(result) => result,
            Err(e) => {
                let err = format!("push_context_trace_signal insert event: {e}");
                log_result("error", Some(&err));
                return Err(err);
            }
        };
        let inserted_events = match i64::try_from(insert_result.rows_affected()) {
            Ok(count) if count > 0 => count,
            Ok(_) => {
                let err = "push_context_trace_signal inserted no event rows".to_string();
                log_result("error", Some(&err));
                return Err(err);
            }
            Err(_) => {
                let err = "push_context_trace_signal inserted row count overflow".to_string();
                log_result("error", Some(&err));
                return Err(err);
            }
        };

        if let Err(e) = crate::storage::add_agent_session_event_count_or_create(
            &mut *tx,
            session_id,
            user_id,
            inserted_events,
            Some(&event_id),
        )
        .await
        {
            let err = format!("push_context_trace_signal event_count delta: {e}");
            log_result("error", Some(&err));
            return Err(err);
        }

        if let Err(e) = tx.commit().await {
            let err = format!("push_context_trace_signal commit: {e}");
            log_result("error", Some(&err));
            return Err(err);
        }

        log_result("success", None);
        Ok(())
    }
}

pub type ExtractedPlanMetadata = (Option<String>, Option<String>, Option<String>, usize);

fn checkpoint_tools_json(checkpoint: &super::session_checkpoint::Checkpoint) -> String {
    let tools_used = normalize_name_list(checkpoint.tools_used.iter().map(String::as_str));
    serde_json::to_string(&tools_used).expect("canonical checkpoint tools must serialize")
}

struct SessionSyncLogEntry<'a> {
    user_id: &'a str,
    session_id: &'a str,
    sync_type: &'a str,
    payload_size: usize,
    duration_ms: Option<u64>,
    status: &'a str,
    error_msg: Option<&'a str>,
}

fn log_session_sync(audit: &crate::state_sync::SyncAuditWriter, entry: SessionSyncLogEntry<'_>) {
    audit.log(crate::state_sync::SyncAuditEntry {
        user_id: entry.user_id.to_string(),
        session_id: entry.session_id.to_string(),
        sync_type: entry.sync_type.to_string(),
        direction: crate::state_sync::SyncDirection::Push,
        payload_size: entry.payload_size,
        duration_ms: entry.duration_ms,
        status: entry.status.to_string(),
        error_message: entry.error_msg.map(|s| s.to_string()),
    });
}

fn log_checkpoint_sync(
    audit: &crate::state_sync::SyncAuditWriter,
    checkpoint_number: u32,
    entry: SessionSyncLogEntry<'_>,
) {
    let error_with_number = entry
        .error_msg
        .map(|e| format!("[checkpoint #{}] {}", checkpoint_number, e));
    log_session_sync(
        audit,
        SessionSyncLogEntry {
            error_msg: error_with_number.as_deref().or(entry.error_msg),
            ..entry
        },
    );
}

/// Pull the latest Heavy step checkpoint JSON from MatrixOne for session recovery.
/// Returns the raw state_json string — caller deserializes to StepCheckpoint.
pub async fn pull_step_checkpoint_from_cloud(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
) -> Result<Option<String>, String> {
    use sqlx::Row;

    let row = sqlx::query(
        "SELECT CAST(state_json AS CHAR) AS state_json_json FROM session_checkpoints \
         WHERE user_id = ? AND session_id = ? AND summary = 'heavy' AND state_json IS NOT NULL \
         ORDER BY number DESC LIMIT 1",
    )
    .bind(user_id)
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
) -> Result<String, String> {
    let mut metadata = session_metadata_object_for_merge(existing_metadata_json)?;
    metadata.insert(
        SESSION_STATE_SYNC_METADATA_MARKER.to_string(),
        serde_json::Value::Bool(true),
    );

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

    if let Some(model) = astra_core::model_override::normalize_model_override(model) {
        metadata.insert(
            "model".to_string(),
            serde_json::Value::String(model.to_string()),
        );
    } else {
        metadata.remove("model");
    }

    Ok(serde_json::Value::Object(metadata).to_string())
}

fn session_state_sync_metadata_marker_present(
    existing_metadata_json: Option<&str>,
) -> Result<bool, String> {
    let metadata = session_metadata_object_for_merge(existing_metadata_json)?;
    Ok(metadata
        .get(SESSION_STATE_SYNC_METADATA_MARKER)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false))
}

fn session_metadata_object_for_merge(
    existing_metadata_json: Option<&str>,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let Some(metadata) = existing_metadata_json
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
    else {
        return Ok(serde_json::Map::new());
    };
    let parsed: serde_json::Value = serde_json::from_str(metadata).map_err(|e| {
        let prefix = &metadata[..metadata.len().min(200)];
        format!("session metadata JSON parse failed before merge: {e}; payload_prefix={prefix:?}")
    })?;
    parsed.as_object().cloned().ok_or_else(|| {
        format!(
            "session metadata JSON must be an object before merge, got {}",
            value_type_name(&parsed)
        )
    })
}

fn value_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Extract plan state from the metadata JSON returned by agent_sessions.
/// Returns (executing_plan_json, plan_goal, plan_config_json, plan_execution_rounds).
pub fn extract_session_state_from_metadata(
    metadata_json: &str,
) -> Result<SessionMetadataState, String> {
    // Defense: reject excessively large metadata to prevent DoS
    const MAX_METADATA_SIZE: usize = 512 * 1024; // 512 KB
    if metadata_json.len() > MAX_METADATA_SIZE {
        return Err(format!(
            "session metadata JSON exceeds maximum size: {} > {MAX_METADATA_SIZE}",
            metadata_json.len()
        ));
    }
    let parsed: serde_json::Value = match serde_json::from_str(metadata_json) {
        Ok(v) => v,
        Err(e) => {
            let prefix = &metadata_json[..metadata_json.len().min(200)];
            return Err(format!(
                "session metadata JSON parse failed: {e}; payload_prefix={prefix:?}"
            ));
        }
    };
    let obj = match parsed.as_object() {
        Some(o) => o,
        None => {
            return Err("session metadata JSON must be an object".to_string());
        }
    };
    let plan_execution_rounds = match obj.get("plan_execution_rounds") {
        Some(value) => {
            let rounds = value.as_u64().ok_or_else(|| {
                "session metadata `plan_execution_rounds` must be a u64".to_string()
            })?;
            usize::try_from(rounds).map_err(|_| {
                format!("session metadata `plan_execution_rounds` exceeds usize::MAX: {rounds}")
            })?
        }
        None => 0,
    };

    Ok(SessionMetadataState {
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
        plan_execution_rounds,
        git_branch: obj
            .get("git_branch")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        model: obj
            .get("model")
            .and_then(|v| v.as_str())
            .and_then(|s| astra_core::model_override::normalize_model_override(Some(s)))
            .map(str::to_string),
        permission_mode: obj
            .get("permission_mode")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

pub fn extract_plan_from_metadata(metadata_json: &str) -> Result<ExtractedPlanMetadata, String> {
    let state = extract_session_state_from_metadata(metadata_json)?;
    Ok((
        state.executing_plan_json,
        state.plan_goal,
        state.plan_config_json,
        state.plan_execution_rounds,
    ))
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_journal::JournalDirGuard;
    use crate::session_workspace;

    const REAL_SESSION_0AC769_FIXTURE: &str =
        include_str!("../fixtures/real_session_0ac769_min.jsonl");

    struct FakeSessionRestoreRow {
        failed_column: Option<&'static str>,
    }

    impl FakeSessionRestoreRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
            }
        }

        fn fail_if_needed(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl SessionRestoreRow for FakeSessionRestoreRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "status" => "active",
                "session_id" => "sess-1",
                "title" => "Checkpoint",
                _ => unreachable!("unexpected string column: {column}"),
            }
            .to_string())
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "title" => Some("Resume work".to_string()),
                "latest_model" => None,
                _ => unreachable!("unexpected optional string column: {column}"),
            })
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "turn_count" => 42,
                "total_tokens_in" => 1000,
                _ => unreachable!("unexpected i64 column: {column}"),
            })
        }

        fn i32_column(&self, column: &str) -> Result<i32, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "number" => 7,
                _ => unreachable!("unexpected i32 column: {column}"),
            })
        }
    }

    #[test]
    fn session_restore_row_decode_helpers_preserve_database_values() {
        let row = FakeSessionRestoreRow::complete();

        assert_eq!(
            mysql_string(&row, "restore_cloud_session", "status").unwrap(),
            "active"
        );
        assert_eq!(
            mysql_optional_string(&row, "restore_cloud_session", "title").unwrap(),
            Some("Resume work".to_string())
        );
        assert_eq!(
            mysql_optional_string(&row, "restore_cloud_session", "latest_model").unwrap(),
            None
        );
        assert_eq!(
            mysql_i64(&row, "restore_cloud_session", "turn_count").unwrap(),
            42
        );
        assert_eq!(mysql_i32(&row, "cloud_checkpoints", "number").unwrap(), 7);
    }

    #[test]
    fn session_restore_row_decode_helpers_fail_loudly_with_context_and_column() {
        for (column, decode) in [
            (
                "status",
                mysql_string(
                    &FakeSessionRestoreRow::fail_on("status"),
                    "restore_cloud_session",
                    "status",
                ),
            ),
            (
                "title",
                mysql_optional_string(
                    &FakeSessionRestoreRow::fail_on("title"),
                    "restore_cloud_session",
                    "title",
                )
                .map(|_| String::new()),
            ),
            (
                "turn_count",
                mysql_i64(
                    &FakeSessionRestoreRow::fail_on("turn_count"),
                    "restore_cloud_session",
                    "turn_count",
                )
                .map(|_| String::new()),
            ),
            (
                "number",
                mysql_i32(
                    &FakeSessionRestoreRow::fail_on("number"),
                    "cloud_checkpoints",
                    "number",
                )
                .map(|_| String::new()),
            ),
        ] {
            let err = decode.unwrap_err();
            assert!(
                err.contains(&format!("decode column `{column}`")),
                "error should identify failed column: {err}"
            );
            assert!(
                err.contains("restore_cloud_session") || err.contains("cloud_checkpoints"),
                "error should identify decode context: {err}"
            );
        }
    }

    #[test]
    fn push_session_state_upsert_is_atomically_owner_guarded() {
        let sql = PUSH_SESSION_STATE_UPSERT_SQL;
        assert!(
            sql.contains("VALUES (?, ?, 'active', ?, NOW(6), NOW(6), NOW(6))"),
            "insert path must target the owner-bound (user_id, session_id) primary key directly"
        );
        assert!(
            !sql.contains("user_id <>"),
            "owner-bound sessions must allow different users to persist the same logical session_id independently"
        );
        assert!(
            !sql.contains("WHERE NOT EXISTS"),
            "insert path must not retain the old global-session-id guard"
        );
        assert!(
            !sql.contains(concat!("ELSE ", "NULL")),
            "owner mismatch must not rely on NOT NULL constraint failures"
        );
        assert!(
            !sql.contains("status ="),
            "duplicate push must preserve existing session status instead of assigning a no-op"
        );
        for assignment in [
            "metadata = IF(user_id = VALUES(user_id), VALUES(metadata), metadata)",
            "updated_at = IF(user_id = VALUES(user_id), NOW(6), updated_at)",
            "last_active_at = IF(user_id = VALUES(user_id), NOW(6), last_active_at)",
        ] {
            assert!(
                sql.contains(assignment),
                "session-state upsert assignment must be owner-guarded: {assignment}"
            );
        }
    }

    #[test]
    fn cloud_restore_timings_are_structured_segments() {
        let timings = CloudRestoreTimings {
            session_query_ms: 1,
            heavy_checkpoint_ms: 2,
            transcript_ms: 3,
            context_trace_ms: 4,
            recent_tools_ms: 5,
            contract_ms: 6,
            checkpoint_fallback_ms: 7,
            total_ms: 8,
        };

        assert_eq!(timings.session_query_ms, 1);
        assert_eq!(timings.heavy_checkpoint_ms, 2);
        assert_eq!(timings.transcript_ms, 3);
        assert_eq!(timings.context_trace_ms, 4);
        assert_eq!(timings.recent_tools_ms, 5);
        assert_eq!(timings.contract_ms, 6);
        assert_eq!(timings.checkpoint_fallback_ms, 7);
        assert_eq!(timings.total_ms, 8);
    }

    #[test]
    fn cache_token_counts_from_token_usage_json_preserves_cache_split() {
        let raw = json!({
            "input_tokens": 10,
            "cached_input_tokens": 4,
            "cache_creation_tokens": 3,
            "output_tokens": 2,
            "total_tokens": 19,
        })
        .to_string();
        assert_eq!(
            cache_token_counts_from_token_usage_json(&raw, "test").expect("cache split"),
            (4, 3)
        );
        assert_eq!(
            cache_token_counts_from_token_usage_json("{}", "test").expect("missing fields default"),
            (0, 0)
        );
        let error = cache_token_counts_from_token_usage_json(
            r#"{"cached_input_tokens":-1,"cache_creation_tokens":0}"#,
            "test",
        )
        .expect_err("negative cache counts must fail");
        assert!(error.contains("non-negative"));
    }

    #[test]
    fn restore_cache_token_totals_skip_invalid_rows() {
        let mut cache_read_total = 0;
        let mut cache_creation_total = 0;
        let valid = json!({
            "cached_input_tokens": 4,
            "cache_creation_tokens": 3,
        })
        .to_string();

        assert!(apply_restore_cache_token_usage(
            &valid,
            "event-ok",
            &mut cache_read_total,
            &mut cache_creation_total,
        ));
        assert!(!apply_restore_cache_token_usage(
            "{not-json",
            "event-bad-json",
            &mut cache_read_total,
            &mut cache_creation_total,
        ));
        assert!(!apply_restore_cache_token_usage(
            r#"{"cached_input_tokens":-1,"cache_creation_tokens":0}"#,
            "event-negative",
            &mut cache_read_total,
            &mut cache_creation_total,
        ));

        assert_eq!((cache_read_total, cache_creation_total), (4, 3));
    }

    #[test]
    fn restore_cache_token_totals_skip_overflowing_rows_atomically() {
        let mut cache_read_total = i64::MAX - 1;
        let mut cache_creation_total = 10;
        let overflowing = json!({
            "cached_input_tokens": 2,
            "cache_creation_tokens": 5,
        })
        .to_string();

        assert!(!apply_restore_cache_token_usage(
            &overflowing,
            "event-overflow",
            &mut cache_read_total,
            &mut cache_creation_total,
        ));
        assert_eq!((cache_read_total, cache_creation_total), (i64::MAX - 1, 10));
    }

    #[test]
    fn metadata_json_state_defaults_only_for_absent_or_empty_metadata() {
        assert_eq!(
            metadata_json_state(None).unwrap(),
            SessionMetadataState::default()
        );
        assert_eq!(
            metadata_json_state(Some("")).unwrap(),
            SessionMetadataState::default()
        );

        let state = metadata_json_state(Some(
            r#"{"model":"gpt-5","permission_mode":"accept_edits"}"#,
        ))
        .unwrap();
        assert_eq!(state.model.as_deref(), Some("gpt-5"));
        assert_eq!(state.permission_mode.as_deref(), Some("accept_edits"));
    }

    #[test]
    fn metadata_json_state_fails_loudly_on_corrupt_present_metadata() {
        for (metadata, expected) in [
            ("not json", "parse failed"),
            ("[]", "must be an object"),
            (
                r#"{"plan_execution_rounds":"three"}"#,
                "plan_execution_rounds",
            ),
        ] {
            let error = metadata_json_state(Some(metadata)).expect_err("metadata should fail");
            assert!(
                error.contains(expected),
                "metadata error should identify `{expected}`: {error}"
            );
        }
    }

    #[test]
    fn non_negative_numeric_conversions_fail_loudly_on_invalid_values() {
        assert_eq!(
            non_negative_i64_to_u64(42, "restore_cloud_session", "total_tokens_in").unwrap(),
            42
        );
        assert_eq!(
            non_negative_i64_to_u32(42, "restore_cloud_session", "turn_count").unwrap(),
            42
        );
        assert_eq!(
            non_negative_i32_to_u32(42, "cloud_checkpoints", "turn").unwrap(),
            42
        );

        for err in [
            non_negative_i64_to_u64(-1, "restore_cloud_session", "total_tokens_in").unwrap_err(),
            non_negative_i64_to_u32(-1, "restore_cloud_session", "turn_count").unwrap_err(),
            non_negative_i32_to_u32(-1, "cloud_checkpoints", "turn").unwrap_err(),
            non_negative_i64_to_u32(i64::MAX, "restore_cloud_session", "turn_count").unwrap_err(),
        ] {
            assert!(
                err.contains("expected") && err.contains("column"),
                "error should identify invalid numeric restore value: {err}"
            );
        }
    }

    // ── RestoredSession ──

    #[test]
    fn restored_session_defaults() {
        let s = RestoredSession::default();
        assert_eq!(s.turn_count, 0);
        assert!(s.recent_tools.is_empty());
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
            recent_tools: vec!["git".into(), "grep".into()],
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
        let result = svc
            .restore_local_session("nonexistent-session")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn local_only_list_checkpoints_empty() {
        let svc = HybridRestoreService::local_only();
        let ckpts = svc
            .list_local_checkpoints("nonexistent-session")
            .await
            .unwrap();
        assert!(ckpts.is_empty());
    }

    #[tokio::test]
    async fn local_only_list_checkpoints_unreadable_index_fails_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = uuid::Uuid::new_v4().to_string();
        let checkpoint_index = session_workspace::workspace_dir_for(&sid)
            .join("checkpoints")
            .join("index.md");
        std::fs::create_dir_all(&checkpoint_index).unwrap();

        let svc = HybridRestoreService::local_only();
        let error = svc
            .list_local_checkpoints(&sid)
            .await
            .expect_err("unreadable checkpoint index must fail checkpoint listing");

        assert!(
            error.contains("failed to read local checkpoint index"),
            "checkpoint listing error should identify local index failure: {error}"
        );
        assert!(
            error.contains("list_checkpoints_inner"),
            "checkpoint listing error should include context: {error}"
        );
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
        let result = svc.restore_local_to_checkpoint("nonexistent", 5).await;
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
            .restore_local_session("00000000-0000-0000-0000-000000000000")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn local_only_restore_falls_back_to_real_session_journal_without_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "0ac7696c-8a67-4e9f-b7bb-88b3bf7b59a0";
        let path = crate::session_journal::journal_file_path(sid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, REAL_SESSION_0AC769_FIXTURE).unwrap();

        let svc = HybridRestoreService::local_only();
        let restored = svc
            .restore_local_session(sid)
            .await
            .unwrap()
            .expect("journal-only session should restore");

        assert_eq!(restored.session_id, sid);
        assert_eq!(restored.turn_count, 1);
        assert_eq!(restored.total_tokens_in, 33_659);
        assert_eq!(restored.total_tokens_out, 2_855);
        assert_eq!(restored.recent_tools, vec!["git", "read_file", "grep"]);
        assert_eq!(restored.model.as_deref(), Some("glm-5.1"));
        assert_eq!(restored.last_status, "completed");
        assert!(!restored.restored_from_cloud);
    }

    #[tokio::test]
    async fn local_restore_uses_real_session_journal_tools_when_workspace_has_no_trace() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "0ac7696c-8a67-4e9f-b7bb-88b3bf7b59a0";
        let path = crate::session_journal::journal_file_path(sid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, REAL_SESSION_0AC769_FIXTURE).unwrap();

        let ws = session_workspace::WorkspaceMetadata::with_context(sid, "glm-5.1", "/repo", None);
        session_workspace::write_workspace(&ws).unwrap();

        let svc = HybridRestoreService::local_only();
        let restored = svc
            .restore_local_session(sid)
            .await
            .unwrap()
            .expect("workspace-backed session should restore");

        assert_eq!(restored.recent_tools, vec!["git", "read_file", "grep"]);
        assert_eq!(restored.turn_count, 1);
        assert_eq!(restored.total_tokens_in, 33_659);
        assert_eq!(restored.total_tokens_out, 2_855);
    }

    #[tokio::test]
    async fn local_restore_corrupt_workspace_falls_back_to_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = uuid::Uuid::new_v4().to_string();

        let writer = crate::session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&crate::session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("test-model"),
            ))
            .unwrap();
        writer
            .append(&crate::session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                Some("test-model"),
                "continue",
                "restored",
                0,
                10,
                5,
                5,
            ))
            .unwrap();

        let workspace_dir = session_workspace::workspace_dir_for(&sid);
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::write(workspace_dir.join("workspace.yaml"), ":\nnot-valid-yaml").unwrap();

        let svc = HybridRestoreService::local_only();
        let restored = svc
            .restore_local_session(&sid)
            .await
            .unwrap()
            .expect("corrupt workspace should still restore from journal");
        assert_eq!(restored.session_id, sid);
        assert_eq!(restored.turn_count, 1);
        assert_eq!(restored.model.as_deref(), Some("test-model"));
        assert_eq!(restored.last_status, "local");
        assert!(!restored.restored_from_cloud);
    }

    #[tokio::test]
    async fn local_restore_unreadable_checkpoint_index_fails_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = uuid::Uuid::new_v4().to_string();

        let ws = session_workspace::WorkspaceMetadata::with_context(&sid, "gpt-5", "/repo", None);
        session_workspace::write_workspace(&ws).unwrap();
        let checkpoint_index = session_workspace::workspace_dir_for(&sid)
            .join("checkpoints")
            .join("index.md");
        std::fs::create_dir_all(&checkpoint_index).unwrap();

        let svc = HybridRestoreService::local_only();
        let error = svc
            .restore_local_session(&sid)
            .await
            .expect_err("unreadable checkpoint index must fail restore");

        assert!(
            error.contains("failed to read local checkpoint index"),
            "restore error should identify checkpoint index failure: {error}"
        );
        assert!(
            error.contains("restore_session_inner"),
            "restore error should include context: {error}"
        );
    }

    #[tokio::test]
    async fn local_restore_recovers_permission_mode_from_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = uuid::Uuid::new_v4().to_string();

        let writer = crate::session_journal::JournalWriter::new(&sid).unwrap();
        let mut start =
            crate::session_journal::JournalEvent::session_start(Some(&sid), Some("test-model"));
        start.edge_policy = Some(crate::session_journal::EdgePolicySnapshot {
            permission_mode: Some("accept_edits".into()),
            cloud_policy_version: None,
            rules_fingerprint: None,
        });
        writer.append(&start).unwrap();
        writer
            .append(&crate::session_journal::JournalEvent::permission_audit(
                Some(&sid),
                Some(1),
                serde_json::json!({
                    "kind": "permission_mode_changed",
                    "from_mode": "accept_edits",
                    "to_mode": "plan",
                    "source": "test",
                    "changed": true
                }),
            ))
            .unwrap();
        writer
            .append(&crate::session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                Some("test-model"),
                "continue",
                "restored",
                0,
                10,
                5,
                5,
            ))
            .unwrap();

        let svc = HybridRestoreService::local_only();
        let restored = svc
            .restore_local_session(&sid)
            .await
            .unwrap()
            .expect("journal-only session should restore");

        assert_eq!(restored.permission_mode.as_deref(), Some("plan"));
    }

    #[tokio::test]
    async fn local_restore_uses_journal_model_when_workspace_model_is_symbolic_default() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = uuid::Uuid::new_v4().to_string();

        let writer = crate::session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&crate::session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&crate::session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                Some("gpt-5"),
                "continue",
                "restored",
                0,
                10,
                5,
                5,
            ))
            .unwrap();
        let mut ws = session_workspace::WorkspaceMetadata::new(&sid, "default");
        ws.turn_count = 1;
        session_workspace::write_workspace(&ws).unwrap();

        let svc = HybridRestoreService::local_only();
        let restored = svc
            .restore_local_session(&sid)
            .await
            .unwrap()
            .expect("workspace-backed session should restore");

        assert_eq!(restored.model.as_deref(), Some("gpt-5"));
    }

    #[tokio::test]
    async fn local_restore_unreadable_journal_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = uuid::Uuid::new_v4().to_string();

        let path = crate::session_journal::journal_file_path(&sid);
        std::fs::create_dir_all(&path).unwrap();

        let svc = HybridRestoreService::local_only();
        let error = svc
            .restore_local_session(&sid)
            .await
            .expect_err("unreadable journal should fail local restore");

        assert!(error.contains("failed to read session journal"), "{error}");
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

    #[test]
    fn parse_local_checkpoint_entries_reads_index_format() {
        let parsed = parse_local_checkpoint_entries(&[
            "001 - Turn 5 - First checkpoint".to_string(),
            "002 - Turn 9 - Second checkpoint".to_string(),
        ]);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].number, 1);
        assert_eq!(parsed[0].turn, 5);
        assert_eq!(parsed[0].title, "First checkpoint");
    }

    #[test]
    fn merge_checkpoints_prefers_cloud_metadata_for_same_number() {
        let local = vec![RestoredCheckpoint {
            number: 3,
            turn: 10,
            title: "Local title".into(),
            summary: String::new(),
            total_tokens: 0,
            contract_state_json: None,
        }];
        let cloud = vec![RestoredCheckpoint {
            number: 3,
            turn: 10,
            title: "Cloud title".into(),
            summary: "Rich summary".into(),
            total_tokens: 1234,
            contract_state_json: Some("{\"contract\":true}".into()),
        }];

        let merged = merge_checkpoints(local, cloud);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].title, "Cloud title");
        assert_eq!(merged[0].summary, "Rich summary");
        assert_eq!(merged[0].total_tokens, 1234);
        assert_eq!(
            merged[0].contract_state_json.as_deref(),
            Some("{\"contract\":true}")
        );
    }

    #[test]
    fn merge_composite_snapshot_indexes_prefers_remote_snapshot_for_same_id() {
        let local = astra_core::composite_snapshot::CompositeSnapshotIndex {
            snapshots: vec![astra_core::composite_snapshot::CompositeSnapshot {
                snapshot_id: "snap-1".into(),
                session_id: "s1".into(),
                turn: 2,
                created_at: "2025-01-01T00:00:00Z".into(),
                version: 1,
                label: Some("local".into()),
                refs: vec![],
            }],
        };
        let remote = astra_core::composite_snapshot::CompositeSnapshotIndex {
            snapshots: vec![astra_core::composite_snapshot::CompositeSnapshot {
                snapshot_id: "snap-1".into(),
                session_id: "s1".into(),
                turn: 2,
                created_at: "2025-01-01T00:00:01Z".into(),
                version: 1,
                label: Some("remote".into()),
                refs: vec![],
            }],
        };

        let merged = merge_composite_snapshot_indexes(local, remote);
        assert_eq!(merged.snapshots.len(), 1);
        assert_eq!(merged.snapshots[0].label.as_deref(), Some("remote"));
    }

    #[test]
    fn merge_composite_snapshot_sources_keeps_local_when_remote_is_absent() {
        let local = astra_core::composite_snapshot::CompositeSnapshotIndex {
            snapshots: vec![astra_core::composite_snapshot::CompositeSnapshot {
                snapshot_id: "snap-local".into(),
                session_id: "s1".into(),
                turn: 2,
                created_at: "2025-01-01T00:00:00Z".into(),
                version: 1,
                label: Some("local".into()),
                refs: vec![],
            }],
        };

        let merged = merge_composite_snapshot_sources("s1", local, Ok(None)).unwrap();

        assert_eq!(merged.snapshots.len(), 1);
        assert_eq!(merged.snapshots[0].snapshot_id, "snap-local");
    }

    #[test]
    fn merge_composite_snapshot_sources_fails_loudly_on_remote_error() {
        let local = astra_core::composite_snapshot::CompositeSnapshotIndex::default();

        let error = merge_composite_snapshot_sources(
            "s1",
            local,
            Err("restore_cloud_composite_snapshot_index: corrupt artifact JSON".into()),
        )
        .unwrap_err();

        assert!(
            error.contains("list_composite_snapshots"),
            "remote composite snapshot error should keep caller context: {error}"
        );
        assert!(
            error.contains("failed to read remote composite snapshot index"),
            "remote composite snapshot error should identify the failed source: {error}"
        );
        assert!(
            error.contains("corrupt artifact JSON"),
            "remote composite snapshot error should preserve the source error: {error}"
        );
    }

    #[tokio::test]
    async fn local_only_restore_to_checkpoint_session_not_found() {
        let svc = HybridRestoreService::local_only();
        // Session doesn't exist → returns Ok(None)
        let result = svc
            .restore_local_to_checkpoint("nonexistent-session-id", 1)
            .await;
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
        assert!(s.git_branch.is_none());
        assert!(s.model.is_none());
        assert!(s.title.is_none());
        assert_eq!(s.total_tokens_in, 0);
        assert_eq!(s.total_tokens_out, 0);
        assert_eq!(s.checkpoint_count, 0);
    }

    // ── HybridRestoreService local_only behavior ──

    #[tokio::test]
    async fn local_only_recent_tools_returns_empty() {
        let svc = HybridRestoreService::local_only();
        let tools = svc
            .restore_recent_tools("user1", "nonexistent")
            .await
            .unwrap();
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn local_only_cloud_checkpoints_returns_empty() {
        let svc = HybridRestoreService::local_only();
        let ckpts = svc.cloud_checkpoints("user1", "nonexistent").await.unwrap();
        assert!(ckpts.is_empty());
    }

    #[tokio::test]
    async fn local_only_cloud_checkpoint_count_returns_zero() {
        let svc = HybridRestoreService::local_only();
        let count = svc
            .cloud_checkpoint_count("user1", "nonexistent")
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn cloud_checkpoint_queries_are_bounded_and_projection_aware() {
        assert_eq!(MAX_CLOUD_RESTORE_CHECKPOINTS, 200);
        assert!(
            CLOUD_CHECKPOINTS_SELECT_SQL
                .to_ascii_uppercase()
                .contains("LIMIT ?"),
            "cloud checkpoint restore must bound LONGTEXT reads"
        );
        assert!(
            CLOUD_CHECKPOINTS_SELECT_SQL.contains("ORDER BY number DESC"),
            "inner query must choose the latest checkpoints before applying LIMIT"
        );
        assert!(
            CLOUD_CHECKPOINTS_SELECT_SQL.ends_with("ORDER BY number"),
            "outer query must restore ascending checkpoint order for callers"
        );
        assert!(
            !CLOUD_CHECKPOINT_COUNT_SQL.contains("contract_state_json"),
            "checkpoint count must not read LONGTEXT checkpoint bodies"
        );
        assert!(
            CLOUD_CHECKPOINT_COUNT_SQL
                .to_ascii_uppercase()
                .contains("COUNT(*)"),
            "checkpoint count must stay a lightweight aggregate"
        );
    }

    #[test]
    fn prompt_history_transcript_query_restores_only_root_conversation_rows() {
        for sql in [
            PROMPT_HISTORY_TRANSCRIPT_SELECT_SQL,
            PROMPT_HISTORY_TRANSCRIPT_EXISTS_SQL,
        ] {
            let upper = sql.to_ascii_uppercase();

            assert!(
                upper.contains("LEFT JOIN AGENT_RUNS"),
                "prompt-history transcript restore must classify transcript rows by durable run lineage: {sql}"
            );
            assert!(
                upper.contains("R.SESSION_ID = STI.SESSION_ID"),
                "run lineage lookup must stay scoped to the same session: {sql}"
            );
            assert!(
                upper.contains("STI.RUN_ID IS NULL"),
                "system/session transcript rows without a run owner remain restorable: {sql}"
            );
            assert!(
                upper.contains("R.RUN_ID IS NOT NULL") && upper.contains("R.PARENT_RUN_ID IS NULL"),
                "child/subrun transcript rows must not be restored as main prompt history: {sql}"
            );
        }
        assert!(
            PROMPT_HISTORY_TRANSCRIPT_SELECT_SQL
                .to_ascii_uppercase()
                .contains("ORDER BY STI.ITEM_SEQ DESC"),
            "inner prompt-history transcript query must choose the newest bounded rows first: {PROMPT_HISTORY_TRANSCRIPT_SELECT_SQL}"
        );
        assert!(
            PROMPT_HISTORY_TRANSCRIPT_SELECT_SQL
                .to_ascii_uppercase()
                .contains("LIMIT ?"),
            "prompt-history transcript restore must be bounded: {PROMPT_HISTORY_TRANSCRIPT_SELECT_SQL}"
        );
        assert!(
            PROMPT_HISTORY_TRANSCRIPT_SELECT_SQL
                .to_ascii_uppercase()
                .ends_with("ORDER BY ITEM_SEQ"),
            "prompt-history transcript restore must preserve transcript order: {PROMPT_HISTORY_TRANSCRIPT_SELECT_SQL}"
        );
        assert_eq!(
            MAX_PROMPT_HISTORY_TRANSCRIPT_ROWS, 80,
            "transcript fallback is not the canonical log; keep it bounded"
        );
    }

    static SESSION_RESTORE_DB: tokio::sync::OnceCell<astra_core::MatrixOneSettings> =
        tokio::sync::OnceCell::const_new();

    async fn setup_session_restore_db_it() -> astra_core::SharedPool {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored MatrixOne restore tests"
        );
        let settings = SESSION_RESTORE_DB
            .get_or_init(|| async {
                let settings = astra_core::MatrixOneSettings::from_env();
                let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                    .unwrap_or_else(|_| "mysql".to_string());
                crate::storage::ensure_core_schema(&settings, &catalog)
                    .await
                    .expect("ensure_core_schema");
                settings
            })
            .await
            .clone();
        astra_core::SharedPool::new(&settings)
            .await
            .expect("SharedPool::new")
    }

    async fn cleanup_prompt_history_restore_fixture(
        pool: &astra_core::SharedPool,
        user_id: &str,
        session_id: &str,
    ) {
        for sql in [
            "DELETE FROM session_transcript_items WHERE user_id = ? AND session_id = ?",
            "DELETE FROM agent_runs WHERE user_id = ? AND session_id = ?",
            "DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?",
        ] {
            let _ = sqlx::query(sql)
                .bind(user_id)
                .bind(session_id)
                .execute(pool.get())
                .await;
        }
    }

    async fn insert_prompt_history_restore_session(
        pool: &astra_core::SharedPool,
        user_id: &str,
        session_id: &str,
    ) {
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, status, created_at, updated_at, last_active_at)
             VALUES (?, ?, 'active', NOW(6), NOW(6), NOW(6))",
        )
        .bind(session_id)
        .bind(user_id)
        .execute(pool.get())
        .await
        .expect("insert restore fixture session");
    }

    async fn insert_prompt_history_restore_run(
        pool: &astra_core::SharedPool,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        parent_run_id: Option<&str>,
        depth: i32,
    ) {
        let root_run_id = parent_run_id.unwrap_or(run_id);
        let ancestor_path = match parent_run_id {
            Some(parent) => format!("{parent}/{run_id}"),
            None => run_id.to_string(),
        };
        sqlx::query(
            "INSERT INTO agent_runs
             (run_id, user_id, session_id, parent_run_id, root_run_id, ancestor_path, depth, status)
             VALUES (?, ?, ?, ?, ?, ?, ?, 'completed')",
        )
        .bind(run_id)
        .bind(user_id)
        .bind(session_id)
        .bind(parent_run_id)
        .bind(root_run_id)
        .bind(ancestor_path)
        .bind(depth)
        .execute(pool.get())
        .await
        .expect("insert restore fixture run");
    }

    async fn insert_prompt_history_restore_transcript(
        pool: &astra_core::SharedPool,
        user_id: &str,
        session_id: &str,
        item_seq: i64,
        run_id: Option<&str>,
        role: &str,
        content: &str,
    ) {
        sqlx::query(
            "INSERT INTO session_transcript_items
             (session_id, item_seq, user_id, run_id, role, content, content_hash, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, NOW(6))",
        )
        .bind(session_id)
        .bind(item_seq)
        .bind(user_id)
        .bind(run_id)
        .bind(role)
        .bind(content)
        .bind(format!("fixture:{item_seq}"))
        .execute(pool.get())
        .await
        .expect("insert restore fixture transcript");
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn prompt_history_transcript_restore_matches_real_run_lineage_and_limit_on_matrixone() {
        let pool = setup_session_restore_db_it().await;
        let user_id = format!("user-{}", uuid::Uuid::new_v4());
        let session_id = format!("sess-{}", uuid::Uuid::new_v4());
        let root_run_id = format!("root-{}", uuid::Uuid::new_v4());
        let child_run_id = format!("child-{}", uuid::Uuid::new_v4());
        cleanup_prompt_history_restore_fixture(&pool, &user_id, &session_id).await;

        insert_prompt_history_restore_session(&pool, &user_id, &session_id).await;
        insert_prompt_history_restore_run(&pool, &user_id, &session_id, &root_run_id, None, 0)
            .await;
        insert_prompt_history_restore_run(
            &pool,
            &user_id,
            &session_id,
            &child_run_id,
            Some(&root_run_id),
            1,
        )
        .await;
        for seq in 1..=90 {
            insert_prompt_history_restore_transcript(
                &pool,
                &user_id,
                &session_id,
                seq,
                Some(&root_run_id),
                "user",
                &format!("root-{seq:02}"),
            )
            .await;
        }
        insert_prompt_history_restore_transcript(
            &pool,
            &user_id,
            &session_id,
            905,
            Some(&child_run_id),
            "assistant",
            "child-output-must-not-be-prompt-history",
        )
        .await;
        insert_prompt_history_restore_transcript(
            &pool,
            &user_id,
            &session_id,
            91,
            None,
            "system",
            "session-note",
        )
        .await;

        let service = HybridRestoreService::new(pool.get().clone());
        let messages = service
            .restore_cloud_transcript_messages(&user_id, &session_id)
            .await
            .expect("restore transcript prompt history");

        cleanup_prompt_history_restore_fixture(&pool, &user_id, &session_id).await;

        assert_eq!(messages.len(), MAX_PROMPT_HISTORY_TRANSCRIPT_ROWS as usize);
        assert_eq!(messages.first().unwrap()["content"], "root-12");
        assert_eq!(messages.last().unwrap()["content"], "session-note");
        assert!(
            messages.iter().all(|message| {
                message["content"].as_str() != Some("child-output-must-not-be-prompt-history")
            }),
            "child run transcript rows are work-unit output, not main prompt history"
        );
    }

    // ── Checkpoint convergence ──

    #[test]
    fn checkpoint_tools_json_for_cloud_push_is_canonical() {
        let ckpt = crate::session_checkpoint::Checkpoint {
            number: 3,
            turn: 15,
            title: "Phase A done".into(),
            summary: "Token efficiency implemented".into(),
            tools_used: vec![" bash ".into(), "bash".into(), "".into(), " grep".into()],
            total_tokens: 50_000,
            had_stalls: true,
            error_count: 1,
            contract_state_json: Some(r#"{"contract_id":"c1"}"#.to_string()),
        };
        assert_eq!(ckpt.number, 3);
        assert_eq!(ckpt.turn, 15);
        assert!(ckpt.had_stalls);
        assert_eq!(ckpt.error_count, 1);
        assert_eq!(checkpoint_tools_json(&ckpt), r#"["bash","grep"]"#);
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
            "plan_config": "{\"step_by_step\":true}",
            "plan_execution_rounds": 3
        }"#;
        let (plan, goal, config, rounds) = extract_plan_from_metadata(metadata).unwrap();
        assert!(plan.is_some());
        assert!(plan.unwrap().contains("subtasks"));
        assert_eq!(goal, Some("Build feature X".to_string()));
        assert!(config.is_some());
        assert_eq!(rounds, 3);
    }

    #[test]
    fn extract_plan_from_metadata_empty() {
        let (plan, goal, config, rounds) = extract_plan_from_metadata("{}").unwrap();
        assert!(plan.is_none());
        assert!(goal.is_none());
        assert!(config.is_none());
        assert_eq!(rounds, 0);
    }

    #[test]
    fn extract_plan_from_metadata_invalid_json_fails_loudly() {
        let error = extract_plan_from_metadata("not json").expect_err("invalid JSON must fail");
        assert!(
            error.contains("parse failed"),
            "invalid metadata should fail loudly: {error}"
        );
    }

    #[test]
    fn extract_plan_from_metadata_partial() {
        let metadata = r#"{"plan_goal": "Fix bug", "plan_execution_rounds": 1}"#;
        let (plan, goal, config, rounds) = extract_plan_from_metadata(metadata).unwrap();
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
        )
        .unwrap();
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
        assert_eq!(
            parsed
                .get(SESSION_STATE_SYNC_METADATA_MARKER)
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(
            session_state_sync_metadata_marker_present(Some(&merged)).unwrap(),
            "merged session-state metadata must be recognized as sync-created"
        );
        assert!(
            !session_state_sync_metadata_marker_present(Some(r#"{"owner":true}"#)).unwrap(),
            "foreign real session metadata must not pass the sync-created marker check"
        );
    }

    #[test]
    fn merge_session_state_metadata_does_not_persist_symbolic_default_model() {
        let merged = merge_session_state_metadata(
            Some(r#"{"agent_id":"astra-server"}"#),
            None,
            None,
            None,
            0,
            None,
            Some(" default "),
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert!(parsed.get("model").is_none());
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
        )
        .unwrap();
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
    fn merge_session_state_metadata_fails_loudly_on_corrupt_existing_metadata() {
        let error =
            merge_session_state_metadata(Some("{not-json"), None, None, None, 0, None, None)
                .unwrap_err();

        assert!(
            error.contains("session metadata JSON parse failed before merge"),
            "merge must report corrupt existing metadata: {error}"
        );
        assert!(
            error.contains("payload_prefix"),
            "merge error should include bounded payload context: {error}"
        );
    }

    #[test]
    fn merge_session_state_metadata_fails_loudly_on_non_object_existing_metadata() {
        let error =
            merge_session_state_metadata(Some("[]"), None, None, None, 0, None, None).unwrap_err();

        assert!(
            error.contains("session metadata JSON must be an object before merge"),
            "merge must reject non-object metadata: {error}"
        );
        assert!(
            error.contains("array"),
            "merge error should identify the JSON type: {error}"
        );
    }

    #[test]
    fn extract_session_state_from_metadata_ignores_non_plan_trace_fields() {
        let metadata = r#"{
            "executing_plan": "{\"subtasks\":[]}",
            "git_branch": "feature/cloud-sync",
            "model": "gpt-5.4",
            "last_context_trace": {
                "turn_id": "turn-9",
                "visible_tools": ["lsp", "view"],
                "memory_query": "resume trace persistence",
                "memories_selected": 2,
                "compressed_turns": 1,
                "compression_ratio": 0.72,
                "budget_pressure": 0.88,
                "total_tokens_used": 12345
            }
        }"#;
        let state = extract_session_state_from_metadata(metadata).unwrap();
        assert!(state.executing_plan_json.is_some());
        assert_eq!(state.plan_execution_rounds, 0);
        assert_eq!(state.git_branch.as_deref(), Some("feature/cloud-sync"));
        assert_eq!(state.model.as_deref(), Some("gpt-5.4"));
    }

    #[test]
    fn extract_session_state_from_metadata_drops_symbolic_default_model() {
        let state = extract_session_state_from_metadata(r#"{"model":"default"}"#).unwrap();
        assert_eq!(state.model, None);
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
        let json = minimal_session_json(r#""git_branch":null,"model":null,"title":null"#);
        let s: RestoredSession = serde_json::from_str(&json).unwrap();
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
    fn cloud_step_checkpoint_number_uses_disjoint_namespace() {
        assert_eq!(
            cloud_step_checkpoint_number(1).unwrap(),
            1_000_000_001,
            "step checkpoints should avoid the session-checkpoint number range"
        );
    }

    #[test]
    fn recent_tools_from_context_trace_uses_visible_tools() {
        let trace = session_workspace::ContextTraceSignal {
            turn_id: "turn-7".into(),
            captured_at: None,
            tool_surface: Some(session_workspace::ContextTraceToolSurface {
                tools_available: 8,
                visible_tools: vec!["bash".into(), "grep".into(), "bash".into()],
                surface_scope: "latest_round".into(),
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
    fn recent_tools_from_context_trace_drops_blank_tool_names() {
        let trace = session_workspace::ContextTraceSignal {
            turn_id: "turn-blank-tools".into(),
            captured_at: None,
            tool_surface: Some(session_workspace::ContextTraceToolSurface {
                tools_available: 5,
                visible_tools: vec![
                    "".into(),
                    "  ".into(),
                    " rg ".into(),
                    "rg".into(),
                    " bash".into(),
                ],
                surface_scope: "latest_round".into(),
                latency_ms: 3,
            }),
            memory: None,
            history: None,
            budget: None,
            timing: None,
            explanations: Vec::new(),
        };

        assert_eq!(
            recent_tools_from_context_trace(Some(&trace)),
            vec!["rg".to_string(), "bash".to_string()],
            "resume recent_tools must contain canonical non-empty tool names only"
        );
    }

    #[test]
    fn composite_snapshot_index_uses_one_stable_remote_projection_id() {
        let index = astra_core::composite_snapshot::CompositeSnapshotIndex::default();
        let first =
            composite_snapshot_index_to_remote_artifact_record("session-a", "user-a", &index)
                .expect("serialize first index");
        let second =
            composite_snapshot_index_to_remote_artifact_record("session-a", "user-a", &index)
                .expect("serialize replayed index");

        assert_eq!(first.artifact_id, COMPOSITE_SNAPSHOT_INDEX_PROJECTION_ID);
        assert_eq!(second.artifact_id, first.artifact_id);
    }

    #[test]
    fn authenticated_local_summary_reads_only_the_requested_owner() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = uuid::Uuid::new_v4().to_string();
        let owner_writer = crate::session_journal::JournalWriter::for_user("user-a", &session_id)
            .expect("owner journal");
        owner_writer
            .append(&crate::session_journal::JournalEvent::session_start(
                Some(&session_id),
                Some("owner-model"),
            ))
            .unwrap();

        assert!(
            summarize_local_journal(Some("user-b"), &session_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            summarize_local_journal(Some("user-a"), &session_id)
                .unwrap()
                .and_then(|summary| summary.model),
            Some("owner-model".to_string())
        );
    }

    #[test]
    fn parse_cloud_heavy_checkpoint_state_accepts_only_tagged_shape() {
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
            pipeline_state: None,
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
        let untagged = serde_json::json!({
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
            Ok(Some(expected.clone()))
        );
        assert_eq!(
            parse_cloud_heavy_checkpoint_state(&untagged),
            Ok(None),
            "unversioned/unwrapped cloud heavy payloads must not be restored"
        );
    }

    #[test]
    fn parse_cloud_heavy_checkpoint_state_normalizes_recent_tools() {
        let checkpoint = serde_json::json!({
            "Heavy": {
                "messages": [],
                "blocked_tools": [],
                "recent_tools": ["", "  ", " rg ", "rg", " bash"]
            }
        })
        .to_string();

        let state = parse_cloud_heavy_checkpoint_state(&checkpoint)
            .unwrap()
            .expect("tagged heavy checkpoint should restore");

        assert_eq!(
            state.recent_tools,
            vec!["rg".to_string(), "bash".to_string()]
        );
    }

    #[test]
    fn parse_cloud_heavy_checkpoint_state_rejects_corrupt_json() {
        let error = parse_cloud_heavy_checkpoint_state("{not-json").unwrap_err();
        assert!(error.contains("invalid cloud heavy checkpoint JSON"));
    }

    #[test]
    fn parse_cloud_heavy_checkpoint_state_rejects_missing_required_heavy_fields() {
        for field in ["messages", "blocked_tools", "recent_tools"] {
            let mut heavy = serde_json::Map::new();
            heavy.insert("messages".into(), serde_json::json!([]));
            heavy.insert("blocked_tools".into(), serde_json::json!([]));
            heavy.insert("recent_tools".into(), serde_json::json!([]));
            heavy.remove(field);
            let checkpoint = serde_json::json!({ "Heavy": heavy }).to_string();

            let error = parse_cloud_heavy_checkpoint_state(&checkpoint).unwrap_err();
            assert!(
                error.contains(&format!("field={field}")),
                "error should identify missing field {field}: {error}"
            );
        }
    }

    #[test]
    fn parse_cloud_heavy_checkpoint_state_rejects_null_or_corrupt_required_fields() {
        for (field, value) in [
            ("messages", serde_json::Value::Null),
            ("blocked_tools", serde_json::Value::Null),
            ("recent_tools", serde_json::Value::Null),
            ("messages", serde_json::json!("not-array")),
            ("blocked_tools", serde_json::json!([1])),
            ("recent_tools", serde_json::json!({"tool": "rg"})),
        ] {
            let mut heavy = serde_json::Map::new();
            heavy.insert("messages".into(), serde_json::json!([]));
            heavy.insert("blocked_tools".into(), serde_json::json!([]));
            heavy.insert("recent_tools".into(), serde_json::json!([]));
            heavy.insert(field.into(), value);
            let checkpoint = serde_json::json!({ "Heavy": heavy }).to_string();

            let error = parse_cloud_heavy_checkpoint_state(&checkpoint).unwrap_err();
            assert!(
                error.contains(&format!("field={field}")),
                "error should identify corrupt field {field}: {error}"
            );
        }
    }

    #[test]
    fn parse_cloud_heavy_checkpoint_state_rejects_non_object_heavy_payload() {
        let checkpoint = serde_json::json!({ "Heavy": "not-object" }).to_string();
        let error = parse_cloud_heavy_checkpoint_state(&checkpoint).unwrap_err();
        assert!(error.contains("field=Heavy"));
    }
}
