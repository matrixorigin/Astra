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

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

fn is_duplicate_key_error(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::Database(db_err) => db_err.code().as_deref() == Some("1062"),
        _ => false,
    }
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
}

/// A restored checkpoint entry (lightweight, for listing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoredCheckpoint {
    pub number: u32,
    pub turn: u32,
    pub title: String,
    pub summary: String,
    pub total_tokens: u64,
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
            "SELECT session_id, user_id, title, status, event_count, metadata, \
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
                let status: String = row.try_get("status").unwrap_or_default();
                let title: Option<String> = row.try_get("title").ok().flatten();
                let event_count: i64 = row.try_get("event_count").unwrap_or(0);

                // Extract plan state from metadata JSON
                let metadata_str: Option<String> = row.try_get("metadata").ok().flatten();
                let (plan_json, plan_goal, plan_config, plan_rounds) = match metadata_str.as_deref()
                {
                    Some(m) if !m.is_empty() => extract_plan_from_metadata(m),
                    _ => (None, None, None, 0),
                };

                // Load active contract from task_contracts table
                let contract_json = Self::load_cloud_contract(pool, session_id)
                    .await
                    .ok()
                    .flatten();

                Ok(Some(RestoredSession {
                    session_id: session_id.to_string(),
                    turn_count: event_count as u32,
                    last_status: status,
                    title,
                    restored_from_cloud: true,
                    executing_plan_json: plan_json,
                    plan_goal,
                    plan_config_json: plan_config,
                    plan_execution_rounds: plan_rounds,
                    contract_json,
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
            "SELECT contract_id, task_id, goal, scope_json, subtasks_json, \
             criteria_json, version, status, \
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

    /// Restore recent tools from the last N events in MatrixOne.
    async fn restore_recent_tools(&self, session_id: &str) -> Result<Vec<String>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        let rows = sqlx::query(
            "SELECT metadata FROM agent_events \
             WHERE session_id = ? AND event_type = 'turn_complete' \
             ORDER BY created_at DESC LIMIT 5",
        )
        .bind(session_id)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("restore_recent_tools: {e}"))?;

        use sqlx::Row;
        let mut tools = Vec::new();
        for row in &rows {
            if let Ok(Some(meta_str)) = row.try_get::<Option<String>, _>("metadata")
                && let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta_str)
                && let Some(used) = meta.get("tools_used").and_then(|v| v.as_array())
            {
                for t in used {
                    if let Some(name) = t.as_str()
                        && !tools.contains(&name.to_string())
                    {
                        tools.push(name.to_string());
                    }
                }
            }
        }
        Ok(tools)
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

    /// List checkpoints from MatrixOne.
    async fn cloud_checkpoints(&self, session_id: &str) -> Result<Vec<RestoredCheckpoint>, String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        let rows = sqlx::query(
            "SELECT number, turn, title, summary, total_tokens \
             FROM session_checkpoints WHERE session_id = ? ORDER BY number",
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
                })
            })
            .collect();
        Ok(ckpts)
    }
}

#[async_trait]
impl SessionRestoreService for HybridRestoreService {
    async fn restore_session(&self, session_id: &str) -> Result<Option<RestoredSession>, String> {
        // Step 1: Try local workspace metadata first
        if let Some(ws) = self.restore_local_workspace(session_id) {
            let recent_tools = if self.pool.is_some() {
                self.restore_recent_tools(session_id)
                    .await
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

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
                turn_count: ws.turn_count,
                total_tokens_in: ws.total_tokens_in,
                total_tokens_out: ws.total_tokens_out,
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
                ..Default::default()
            }));
        }

        // Step 2: Fall back to MatrixOne
        self.restore_cloud_session(session_id).await
    }

    async fn list_checkpoints(&self, session_id: &str) -> Result<Vec<RestoredCheckpoint>, String> {
        // Try local first
        let local_entries =
            super::session_checkpoint::read_checkpoint_index(session_id).unwrap_or_default();

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
                // Return session state rewound to checkpoint turn
                Ok(Some(RestoredSession {
                    turn_count: ckpt.turn,
                    total_tokens_in: ckpt.total_tokens,
                    total_tokens_out: 0,
                    checkpoint_count: checkpoint_number,
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
            "SELECT session_id, title, status, event_count, metadata, updated_at \
             FROM agent_sessions \
             WHERE user_id = ? AND status IN ('active', 'paused') \
             ORDER BY updated_at DESC LIMIT 20",
        )
        .bind(user_id)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("list_resumable: {e}"))?;

        use sqlx::Row;
        let sessions = rows
            .iter()
            .filter_map(|row| {
                let session_id: String = row.try_get("session_id").ok()?;
                let title: Option<String> = row.try_get("title").ok().flatten();
                let status: String = row.try_get("status").ok()?;
                let event_count: i64 = row.try_get("event_count").unwrap_or(0);

                Some(RestoredSession {
                    session_id,
                    turn_count: event_count as u32,
                    last_status: status,
                    title,
                    restored_from_cloud: true,
                    ..Default::default()
                })
            })
            .collect();
        Ok(sessions)
    }
}

/// Push a checkpoint to MatrixOne for cross-device availability.
pub async fn push_checkpoint_to_cloud(
    pool: &sqlx::Pool<sqlx::MySql>,
    session_id: &str,
    user_id: &str,
    checkpoint: &super::session_checkpoint::Checkpoint,
) -> Result<(), String> {
    let checkpoint_id = uuid::Uuid::new_v4().to_string();
    let tools_json =
        serde_json::to_string(&checkpoint.tools_used).unwrap_or_else(|_| "[]".to_string());

    let updated = sqlx::query(
        "UPDATE session_checkpoints SET \
            turn = ?, title = ?, summary = ?, tools_json = ?, total_tokens = ?, \
            had_stalls = ?, error_count = ? \
         WHERE session_id = ? AND number = ?",
    )
    .bind(checkpoint.turn as i32)
    .bind(&checkpoint.title)
    .bind(&checkpoint.summary)
    .bind(&tools_json)
    .bind(checkpoint.total_tokens as i64)
    .bind(if checkpoint.had_stalls { 1i32 } else { 0 })
    .bind(checkpoint.error_count as i32)
    .bind(session_id)
    .bind(checkpoint.number as i32)
    .execute(pool)
    .await
    .map_err(|e| format!("push_checkpoint update: {e}"))?;

    if updated.rows_affected() == 0 {
        let inserted = sqlx::query(
            "INSERT INTO session_checkpoints \
             (checkpoint_id, session_id, user_id, number, turn, title, summary, \
              tools_json, total_tokens, had_stalls, error_count, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
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
        .execute(pool)
        .await;

        if let Err(e) = inserted {
            if is_duplicate_key_error(&e) {
                sqlx::query(
                    "UPDATE session_checkpoints SET \
                        turn = ?, title = ?, summary = ?, tools_json = ?, total_tokens = ?, \
                        had_stalls = ?, error_count = ? \
                     WHERE session_id = ? AND number = ?",
                )
                .bind(checkpoint.turn as i32)
                .bind(&checkpoint.title)
                .bind(&checkpoint.summary)
                .bind(&tools_json)
                .bind(checkpoint.total_tokens as i64)
                .bind(if checkpoint.had_stalls { 1i32 } else { 0 })
                .bind(checkpoint.error_count as i32)
                .bind(session_id)
                .bind(checkpoint.number as i32)
                .execute(pool)
                .await
                .map_err(|err| format!("push_checkpoint retry update: {err}"))?;
            } else {
                return Err(format!("push_checkpoint insert: {e}"));
            }
        }
    }

    Ok(())
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

    let updated = sqlx::query(
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
    .bind(checkpoint_number as i32)
    .execute(pool)
    .await
    .map_err(|e| format!("push_step_checkpoint update: {e}"))?;

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
        .bind(checkpoint_number as i32)
        .bind(turn as i32)
        .bind(title)
        .bind(tier)
        .bind(tools_json)
        .bind(state_json)
        .execute(pool)
        .await;

        if let Err(e) = inserted {
            if is_duplicate_key_error(&e) {
                sqlx::query(
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
                .bind(checkpoint_number as i32)
                .execute(pool)
                .await
                .map_err(|err| format!("push_step_checkpoint retry update: {err}"))?;
            } else {
                return Err(format!("push_step_checkpoint insert: {e}"));
            }
        }
    }

    Ok(())
}

/// Pull the latest Heavy step checkpoint JSON from MatrixOne for session recovery.
/// Returns the raw state_json string — caller deserializes to StepCheckpoint.
pub async fn pull_step_checkpoint_from_cloud(
    pool: &sqlx::Pool<sqlx::MySql>,
    session_id: &str,
) -> Result<Option<String>, String> {
    use sqlx::Row;

    let row = sqlx::query(
        "SELECT state_json FROM session_checkpoints \
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
                .try_get("state_json")
                .map_err(|e| format!("read state_json: {e}"))?;
            Ok(Some(json))
        }
        None => Ok(None),
    }
}

// ─── Plan State Cloud Sync ──────────────────────────────────────────────────

/// Push plan execution state to cloud via the agent_sessions.metadata JSON column.
/// Called at checkpoint boundaries and session end to enable cross-device plan restore.
pub async fn push_plan_state_to_cloud(
    pool: &sqlx::Pool<sqlx::MySql>,
    session_id: &str,
    executing_plan_json: Option<&str>,
    plan_goal: Option<&str>,
    plan_config_json: Option<&str>,
    plan_execution_rounds: usize,
) -> Result<(), String> {
    let mut metadata = serde_json::Map::new();
    if let Some(plan) = executing_plan_json {
        metadata.insert(
            "executing_plan".to_string(),
            serde_json::Value::String(plan.to_string()),
        );
    }
    if let Some(goal) = plan_goal {
        metadata.insert(
            "plan_goal".to_string(),
            serde_json::Value::String(goal.to_string()),
        );
    }
    if let Some(config) = plan_config_json {
        metadata.insert(
            "plan_config".to_string(),
            serde_json::Value::String(config.to_string()),
        );
    }
    if plan_execution_rounds > 0 {
        metadata.insert(
            "plan_execution_rounds".to_string(),
            serde_json::Value::Number(serde_json::Number::from(plan_execution_rounds)),
        );
    }

    let metadata_json = serde_json::Value::Object(metadata).to_string();

    sqlx::query("UPDATE agent_sessions SET metadata = ?, updated_at = NOW() WHERE session_id = ?")
        .bind(&metadata_json)
        .bind(session_id)
        .execute(pool)
        .await
        .map_err(|e| format!("push_plan_state: {e}"))?;

    Ok(())
}

/// Extract plan state from the metadata JSON returned by agent_sessions.
/// Returns (executing_plan_json, plan_goal, plan_config_json, plan_execution_rounds).
pub fn extract_plan_from_metadata(
    metadata_json: &str,
) -> (Option<String>, Option<String>, Option<String>, usize) {
    let parsed: serde_json::Value = match serde_json::from_str(metadata_json) {
        Ok(v) => v,
        Err(_) => return (None, None, None, 0),
    };
    let obj = match parsed.as_object() {
        Some(o) => o,
        None => return (None, None, None, 0),
    };

    let plan = obj
        .get("executing_plan")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let goal = obj
        .get("plan_goal")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let config = obj
        .get("plan_config")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let rounds = obj
        .get("plan_execution_rounds")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;

    (plan, goal, config, rounds)
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── RestoredSession ──

    #[test]
    fn restored_session_defaults() {
        let s = RestoredSession::default();
        assert_eq!(s.turn_count, 0);
        assert!(s.recent_tools.is_empty());
        assert!(s.learning_snapshot_json.is_none());
        assert!(!s.restored_from_cloud);
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
            },
            RestoredCheckpoint {
                number: 2,
                turn: 10,
                title: "Second".into(),
                summary: String::new(),
                total_tokens: 3000,
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
}
