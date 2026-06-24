//! Session workspace metadata — describes a session's runtime context.
//!
//! Written once on session start and updated per-turn with cumulative stats.
//! Stored at
//! `~/.astra/sessions/v1/users/<owner>/sessions/<session_id>/workspace.yaml`.
//!
//! This provides:
//! - Quick session identification without parsing the JSONL journal
//! - Context for session resumption and debugging
//! - Foundation for checkpoint-based rewind

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::{
    SessionArtifactJsonRecord, SessionArtifactJsonStore, SessionArtifactStore,
    StoredSessionArtifact,
};

pub const WORKSPACE_METADATA_ARTIFACT_KIND: &str = "workspace_metadata";

fn is_zero(v: &usize) -> bool {
    *v == 0
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextTraceToolSurface {
    #[serde(default)]
    pub tools_available: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub visible_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub surface_scope: String,
    #[serde(default)]
    pub latency_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextTraceMemorySignal {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub query: String,
    #[serde(default)]
    pub candidates_considered: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_memory_ids: Vec<String>,
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(default)]
    pub latency_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextTraceHistorySignal {
    #[serde(default)]
    pub total_turns_available: u32,
    #[serde(default)]
    pub retained_turns: usize,
    #[serde(default)]
    pub compressed_turns: usize,
    #[serde(default)]
    pub dropped_turns: usize,
    #[serde(default)]
    pub compression_ratio: f64,
    #[serde(default)]
    pub tokens_before: u32,
    #[serde(default)]
    pub tokens_after: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextTraceBudgetSignal {
    #[serde(default)]
    pub max_tokens: u32,
    #[serde(default)]
    pub total_used: u32,
    #[serde(default)]
    pub budget_pressure: f64,
    #[serde(default)]
    pub compression_triggered: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextTraceTimingSignal {
    #[serde(default)]
    pub turn: u32,
    #[serde(default)]
    pub context_assembly_ms: u64,
    #[serde(default)]
    pub ttft_ms: u64,
    #[serde(default)]
    pub llm_total_ms: u64,
    #[serde(default)]
    pub tool_execution_ms: u64,
    #[serde(default)]
    pub total_ms: u64,
}

/// Canonical per-turn cloud/local trace signal for resume and auto-tuning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextTraceSignal {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_surface: Option<ContextTraceToolSurface>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<ContextTraceMemorySignal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<ContextTraceHistorySignal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<ContextTraceBudgetSignal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<ContextTraceTimingSignal>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub explanations: Vec<String>,
}

impl ContextTraceSignal {
    pub fn preview(&self) -> String {
        let mut parts = Vec::new();
        if !self.turn_id.is_empty() {
            parts.push(self.turn_id.clone());
        }
        if let Some(selection) = self.tool_surface.as_ref() {
            if !selection.visible_tools.is_empty() {
                let label = if selection.surface_scope.is_empty() {
                    "tools".to_string()
                } else {
                    format!("tools[{}]", selection.surface_scope)
                };
                parts.push(format!("{label}: {}", selection.visible_tools.join(", ")));
            }
        }
        if let Some(memory) = self.memory.as_ref()
            && !memory.selected_memory_ids.is_empty()
        {
            let detail = if !memory.query.is_empty() {
                {
                    let preview: String = memory.query.chars().take(64).collect();
                    if memory.query.chars().count() > 64 {
                        format!(" for \"{preview}...\"")
                    } else {
                        format!(" for \"{preview}\"")
                    }
                }
            } else {
                Default::default()
            };
            parts.push(format!(
                "memory: {} selected{}",
                memory.selected_memory_ids.len(),
                detail
            ));
        }
        if let Some(history) = self.history.as_ref()
            && history.compressed_turns > 0
        {
            if history.compression_ratio > 0.0 {
                parts.push(format!(
                    "history: {} compressed (ratio {ratio:.2})",
                    history.compressed_turns,
                    ratio = history.compression_ratio
                ));
            } else {
                parts.push(format!("history: {} compressed", history.compressed_turns));
            }
        }
        if let Some(budget) = self.budget.as_ref()
            && (budget.max_tokens > 0 || budget.total_used > 0)
        {
            parts.push(format!("budget: {:.2}", budget.budget_pressure));
            parts.push(format!("tokens: {}", budget.total_used));
        }
        if let Some(timing) = self.timing.as_ref()
            && timing.total_ms > 0
        {
            parts.push(format!("time: {}ms", timing.total_ms));
        }
        if let Some(explanation) = self.explanations.first() {
            let preview: String = explanation.chars().take(96).collect();
            if explanation.chars().count() > 96 {
                parts.push(format!("note: {preview}..."));
            } else {
                parts.push(format!("note: {preview}"));
            }
        }
        parts.join(" | ")
    }
}

/// Durable projection for a background shell owned by a session.
///
/// This is intentionally not a process handle. On resume, non-terminal rows
/// are restored as visible stale handles so the user can inspect captured
/// output without the UI pretending it can still control an old process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundShellTaskProjection {
    pub id: String,
    pub status: String,
    pub title: String,
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    pub stdout_path: String,
    pub stderr_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
}

/// Durable projection for a background local agent owned by a session.
///
/// This stores the user/model-visible lifecycle state, not a runtime handle.
/// On resume, non-terminal agents are restored as stale/unavailable tasks:
/// the previous local executor is gone, but the task remains visible with the
/// latest known tail/result/error for inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundLocalAgentFanoutProjection {
    pub group_id: String,
    pub group_title: String,
    pub target_count: usize,
    pub slot_index: usize,
    pub slot_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundLocalAgentTaskProjection {
    pub id: String,
    pub status: String,
    pub title: String,
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fanout: Option<BackgroundLocalAgentFanoutProjection>,
}

/// Session workspace metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceMetadata {
    /// Session ID (UUID).
    pub session_id: String,
    /// Working directory at session start.
    pub cwd: String,
    /// Git repository root (if in a git repo).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_root: Option<String>,
    /// Git branch at session start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// Git HEAD commit at session start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_head: Option<String>,
    /// Concrete LLM model override used to start the session.
    ///
    /// `None` means the runtime used the server/API default. Symbolic values
    /// such as `default` are normalized away at this persistence boundary.
    #[serde(
        default,
        skip_serializing_if = "is_model_override_none",
        serialize_with = "serialize_model_override",
        deserialize_with = "deserialize_model_override"
    )]
    pub model: Option<String>,
    /// Permission mode active for this session.
    ///
    /// Stored as the canonical CLI string (`auto`, `plan`,
    /// `accept_edits`, `prompt`, or `deny`). Missing means the session
    /// predates mode persistence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// ISO 8601 creation timestamp.
    pub created_at: String,
    /// ISO 8601 last-updated timestamp.
    pub updated_at: String,
    /// Total turn count.
    pub turn_count: u32,
    /// Cumulative prompt tokens.
    pub total_tokens_in: u64,
    /// Cumulative completion tokens.
    pub total_tokens_out: u64,
    /// Cumulative prompt-cache read tokens.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub total_cache_read_tokens: u64,
    /// Cumulative prompt-cache creation tokens.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub total_cache_creation_tokens: u64,
    /// Session status: "active", "completed", "error".
    pub status: String,
    /// Brief summary (updated on checkpoints or session end).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Checkpoint turns (turn numbers where checkpoints were created).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub checkpoints: Vec<u32>,
    /// Active plan being executed (JSON-serialized TaskPlan).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub executing_plan_json: Option<String>,
    /// Goal text for the executing plan.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub plan_goal: Option<String>,
    /// Plan execution config (JSON-serialized PlanExecutionConfig).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub plan_config_json: Option<String>,
    /// Number of parallel execution rounds completed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub plan_execution_rounds: usize,
    /// Active durable task contract (JSON-serialized TaskContract).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub contract_json: Option<String>,
    /// Operator corrections injected during plan pause (persisted for crash recovery).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan_corrections: Vec<String>,
    /// Set when this session was forked from another local session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Turn count on the parent at fork time (audit boundary).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_at_turn: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_note: Option<String>,
    /// Correlates this session with multi-agent / cloud-orchestrated work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_role: Option<String>,
    /// Compact summary of the most recent context-assembly trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_context_trace: Option<ContextTraceSignal>,
    /// Durable projection of local background shell tasks for resume.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background_shell_tasks: Vec<BackgroundShellTaskProjection>,
    /// Durable projection of local background agent tasks for resume.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background_local_agent_tasks: Vec<BackgroundLocalAgentTaskProjection>,
    /// Last turn-commit persistence error observed by the live session.
    /// When present, local resume should assume workspace/journal state may be stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_persistence_error: Option<String>,

    // ─── Session state persistence (for resume) ───
    /// Skills discovered during this session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discovered_skills: Vec<String>,

    // ─── Adaptive engine state (for resume without oscillation) ───
    /// Last turn where a scenario change occurred (anti-flap cooldown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scenario_change_turn: Option<u32>,
    /// Direction of the last token-budget change: +1 (increase), -1 (decrease), 0 (none).
    #[serde(default)]
    pub last_token_budget_direction: i8,
    /// Turn where the last token-budget direction change occurred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_token_budget_change_turn: Option<u32>,
    /// Active A/B experiment ID (if enrolled in an experiment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_experiment_id: Option<String>,
    /// Active A/B experiment variant (if enrolled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_variant: Option<String>,
    /// Tuned RuntimeConfig (JSON). Persisted so auto-tuning adjustments survive restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tuned_config_json: Option<String>,
}

fn deserialize_model_override<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(astra_core::model_override::normalize_model_override_owned(
        raw,
    ))
}

fn serialize_model_override<S>(model: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    astra_core::model_override::normalize_model_override(model.as_deref()).serialize(serializer)
}

fn is_model_override_none(model: &Option<String>) -> bool {
    astra_core::model_override::normalize_model_override(model.as_deref()).is_none()
}

impl WorkspaceMetadata {
    /// Create initial metadata for a new session.
    pub fn new(session_id: &str, model: &str) -> Self {
        let now = chrono::Utc::now().to_rfc3339();

        // Detect git context
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string());

        let git_root = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    String::from_utf8(o.stdout)
                        .ok()
                        .map(|s| s.trim().to_string())
                } else {
                    None
                }
            });

        let git_branch = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    String::from_utf8(o.stdout)
                        .ok()
                        .map(|s| s.trim().to_string())
                } else {
                    None
                }
            });

        let git_head = std::process::Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    String::from_utf8(o.stdout)
                        .ok()
                        .map(|s| s.trim().to_string())
                } else {
                    None
                }
            });

        Self {
            session_id: session_id.to_string(),
            cwd,
            git_root,
            git_branch,
            git_head,
            model: astra_core::model_override::normalize_model_override(Some(model))
                .map(str::to_string),
            permission_mode: None,
            created_at: now.clone(),
            updated_at: now,
            turn_count: 0,
            total_tokens_in: 0,
            total_tokens_out: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            status: "active".to_string(),
            summary: None,
            checkpoints: Vec::new(),
            executing_plan_json: None,
            plan_goal: None,
            plan_config_json: None,
            plan_execution_rounds: 0,
            contract_json: None,
            plan_corrections: Vec::new(),
            parent_session_id: None,
            forked_at_turn: None,
            fork_note: None,
            correlation_id: None,
            agent_role: None,
            last_context_trace: None,
            background_shell_tasks: Vec::new(),
            background_local_agent_tasks: Vec::new(),
            last_persistence_error: None,
            discovered_skills: Vec::new(),
            last_scenario_change_turn: None,
            last_token_budget_direction: 0,
            last_token_budget_change_turn: None,
            active_experiment_id: None,
            active_variant: None,
            tuned_config_json: None,
        }
    }
    pub fn with_context(
        session_id: &str,
        model: &str,
        cwd: &str,
        git_branch: Option<&str>,
    ) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            session_id: session_id.to_string(),
            cwd: cwd.to_string(),
            git_root: None,
            git_branch: git_branch.map(|s| s.to_string()),
            git_head: None,
            model: astra_core::model_override::normalize_model_override(Some(model))
                .map(str::to_string),
            permission_mode: None,
            created_at: now.clone(),
            updated_at: now,
            turn_count: 0,
            total_tokens_in: 0,
            total_tokens_out: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            status: "active".to_string(),
            summary: None,
            checkpoints: Vec::new(),
            executing_plan_json: None,
            plan_goal: None,
            plan_config_json: None,
            plan_execution_rounds: 0,
            contract_json: None,
            plan_corrections: Vec::new(),
            parent_session_id: None,
            forked_at_turn: None,
            fork_note: None,
            correlation_id: None,
            agent_role: None,
            last_context_trace: None,
            background_shell_tasks: Vec::new(),
            background_local_agent_tasks: Vec::new(),
            last_persistence_error: None,
            discovered_skills: Vec::new(),
            last_scenario_change_turn: None,
            last_token_budget_direction: 0,
            last_token_budget_change_turn: None,
            active_experiment_id: None,
            active_variant: None,
            tuned_config_json: None,
        }
    }

    /// Update after a turn completes.
    pub fn record_turn(
        &mut self,
        tokens_in: u64,
        tokens_out: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
    ) {
        self.turn_count += 1;
        self.total_tokens_in += tokens_in;
        self.total_tokens_out += tokens_out;
        self.total_cache_read_tokens += cache_read_tokens;
        self.total_cache_creation_tokens += cache_creation_tokens;
        self.updated_at = chrono::Utc::now().to_rfc3339();
    }

    /// Record a checkpoint at the current turn.
    pub fn record_checkpoint(&mut self) {
        self.checkpoints.push(self.turn_count);
        self.updated_at = chrono::Utc::now().to_rfc3339();
    }

    /// Mark session as completed.
    pub fn mark_completed(&mut self, summary: Option<&str>) {
        self.status = "completed".to_string();
        if let Some(s) = summary {
            self.summary = Some(s.to_string());
        }
        self.updated_at = chrono::Utc::now().to_rfc3339();
    }

    /// Mark session as errored.
    pub fn mark_error(&mut self, error: &str) {
        self.status = "error".to_string();
        self.summary = Some(format!("Error: {}", &error[..error.len().min(200)]));
        self.updated_at = chrono::Utc::now().to_rfc3339();
    }
}

pub fn to_remote_artifact_record(
    metadata: &WorkspaceMetadata,
    user_id: &str,
) -> Result<SessionArtifactJsonRecord, serde_json::Error> {
    Ok(SessionArtifactJsonRecord {
        artifact_id: String::new(),
        session_id: metadata.session_id.clone(),
        user_id: user_id.to_string(),
        artifact_kind: WORKSPACE_METADATA_ARTIFACT_KIND.to_string(),
        source: Some("workspace_metadata".to_string()),
        turn: Some(metadata.turn_count),
        round: None,
        content: serde_json::to_value(metadata)?,
        metadata: Some(json!({
            "model": astra_core::model_override::normalize_model_override(metadata.model.as_deref()),
            "status": metadata.status,
            "git_branch": metadata.git_branch,
        })),
    })
}

pub async fn persist_remote_workspace(
    metadata: &WorkspaceMetadata,
    user_id: &str,
    store: &impl SessionArtifactJsonStore,
) -> Result<StoredSessionArtifact, String> {
    let record = to_remote_artifact_record(metadata, user_id).map_err(|error| error.to_string())?;
    store
        .persist_json_artifact(record)
        .await
        .map_err(|error| error.to_string())
}

pub fn read_workspace(session_id: &str) -> std::io::Result<WorkspaceMetadata> {
    let path = workspace_file_path(session_id)?;
    let metadata = std::fs::metadata(&path)?;
    const MAX_WORKSPACE_YAML_SIZE: u64 = 1024 * 1024; // 1 MB
    if metadata.len() > MAX_WORKSPACE_YAML_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "workspace.yaml too large ({} bytes, max {})",
                metadata.len(),
                MAX_WORKSPACE_YAML_SIZE
            ),
        ));
    }
    let content = std::fs::read_to_string(&path)?;
    serde_yaml_ng::from_str(&content)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Write workspace metadata to disk.
pub fn write_workspace(metadata: &WorkspaceMetadata) -> std::io::Result<()> {
    let dir = validated_workspace_dir(&metadata.session_id)?;
    std::fs::create_dir_all(&dir)?;
    sync_parent_dir(&dir)?;
    let path = dir.join("workspace.yaml");
    let yaml = serde_yaml_ng::to_string(metadata)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    // Atomic write: tmp → fsync → rename → fsync parent
    let tmp_path = path.with_extension("yaml.tmp");
    {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        // Set restrictive permissions (0o600) before writing sensitive session data
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        let mut writer = std::io::BufWriter::new(file);
        writer.write_all(yaml.as_bytes())?;
        writer.flush()?;
        writer.get_ref().sync_all()?; // fsync data to disk
    }
    // Atomic rename
    std::fs::rename(&tmp_path, &path)?;
    sync_parent_dir(&path)?;
    Ok(())
}

/// Read workspace metadata when present, while preserving corruption as an error.
pub fn read_workspace_optional(session_id: &str) -> std::io::Result<Option<WorkspaceMetadata>> {
    match read_workspace(session_id) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Get the `workspace.yaml` path for a validated session id.
pub fn workspace_file_path(session_id: &str) -> std::io::Result<PathBuf> {
    Ok(validated_workspace_dir(session_id)?.join("workspace.yaml"))
}

/// Move an unreadable or corrupt `workspace.yaml` aside before rebuilding it.
///
/// Returns the backup path when a workspace file existed and was renamed.
pub fn backup_invalid_workspace_file(session_id: &str) -> std::io::Result<Option<PathBuf>> {
    let path = workspace_file_path(session_id)?;
    if !path.exists() {
        return Ok(None);
    }

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let backup_path = path.with_file_name(format!("workspace.yaml.corrupt-{stamp}"));
    std::fs::rename(&path, &backup_path)?;
    sync_parent_dir(&backup_path)?;
    Ok(Some(backup_path))
}

fn validated_workspace_dir(session_id: &str) -> std::io::Result<PathBuf> {
    crate::session_journal::validate_session_id(session_id)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    Ok(workspace_dir(session_id))
}

fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::File::open(parent)?.sync_all()
}

/// Get the workspace directory for a session.
fn workspace_dir(session_id: &str) -> PathBuf {
    assert!(
        crate::session_journal::validate_session_id(session_id).is_ok(),
        "unsafe session ID passed to workspace_dir: {session_id}"
    );
    crate::local_session_artifact_store()
        .session_dir(session_id)
        .expect("validated session_id must resolve workspace dir")
}

/// Get the workspace directory path (public, for use by checkpoint module).
pub fn workspace_dir_for(session_id: &str) -> PathBuf {
    workspace_dir(session_id)
}

/// Summary of a historical session for the same project.
#[derive(Debug, Clone)]
pub struct ProjectSessionSummary {
    pub session_id: String,
    pub summary: Option<String>,
    pub model: Option<String>,
    pub turn_count: u32,
    pub status: String,
    pub updated_at: String,
    pub git_branch: Option<String>,
}

/// Maximum number of sessions to scan when searching by git_root.
/// We over-scan since many sessions may belong to other repos.
const GIT_ROOT_SCAN_MULTIPLIER: usize = 5;
/// Maximum characters for summary preview in project context formatting.
const SUMMARY_PREVIEW_CHARS: usize = 200;

/// List recent sessions that share the same `git_root`, excluding the current session.
/// Returns up to `limit` sessions, most-recent first.
pub fn list_sessions_by_git_root(
    git_root: &str,
    exclude_session: Option<&str>,
    limit: usize,
) -> Vec<ProjectSessionSummary> {
    // Get recent session IDs (scan a wider set to filter down)
    let scan_limit = limit * GIT_ROOT_SCAN_MULTIPLIER;
    let session_ids = match crate::session_journal::list_sessions_by_time(scan_limit) {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("[knowledge-backflow] Failed to scan sessions: {e}");
            return Vec::new();
        }
    };

    let mut results = Vec::new();
    for sid in &session_ids {
        if let Some(exclude) = exclude_session
            && sid == exclude
        {
            continue;
        }
        // Try reading workspace metadata; skip if unavailable, but surface corruption.
        let workspace = match read_workspace_optional(sid) {
            Ok(workspace) => workspace,
            Err(error) => {
                eprintln!(
                    "[knowledge-backflow] Failed to read workspace for {}: {}",
                    sid, error
                );
                continue;
            }
        };
        if let Some(ws) = workspace
            && ws.git_root.as_deref() == Some(git_root)
        {
            results.push(ProjectSessionSummary {
                session_id: sid.clone(),
                summary: ws.summary,
                model: ws.model,
                turn_count: ws.turn_count,
                status: ws.status,
                updated_at: ws.updated_at,
                git_branch: ws.git_branch,
            });
            if results.len() >= limit {
                break;
            }
        }
    }
    results
}

/// Format project session summaries into a context string for injection.
pub fn format_project_context(summaries: &[ProjectSessionSummary]) -> String {
    if summaries.is_empty() {
        return String::new();
    }
    let mut out = String::from("[Project context — previous sessions on this repo]\n");
    for (i, s) in summaries.iter().enumerate() {
        let branch = s.git_branch.as_deref().unwrap_or("?");
        let summary = s.summary.as_deref().unwrap_or("(no summary)");
        // Truncate summary preview
        let summary: String = summary.chars().take(SUMMARY_PREVIEW_CHARS).collect();
        out.push_str(&format!(
            "{}. [{}] ({}, {} turns, branch: {}) {}\n",
            i + 1,
            s.status,
            &s.updated_at[..10.min(s.updated_at.len())],
            s.turn_count,
            branch,
            summary,
        ));
    }
    out
}

/// Finalize workspace at session end: extract summary from journal and mark completed.
/// Returns the summary string if one was found.
pub fn finalize_workspace_on_end(session_id: &str) -> Option<String> {
    // Read current workspace; bail if it doesn't exist
    let mut ws = match read_workspace_optional(session_id) {
        Ok(Some(ws)) => ws,
        Ok(None) => return None,
        Err(error) => {
            eprintln!("[knowledge-backflow] Failed to read workspace on end: {error}");
            return None;
        }
    };

    // Extract summary from the last compact event's metadata
    let summary = match extract_last_compact_summary(session_id) {
        Ok(summary) => summary,
        Err(error) => {
            eprintln!(
                "[knowledge-backflow] Failed to read compact summary from journal on end: {error}"
            );
            None
        }
    };

    ws.mark_completed(summary.as_deref());

    if let Err(e) = write_workspace(&ws) {
        eprintln!("[knowledge-backflow] Failed to update workspace on end: {e}");
    }

    summary
}

/// Extract the summary from the last Compact journal event that has compact_summary metadata.
fn extract_last_compact_summary(session_id: &str) -> std::io::Result<Option<String>> {
    let events = crate::session_journal::read_journal(session_id)?;
    Ok(events
        .iter()
        .rev()
        .filter(|e| e.event_type == crate::session_journal::JournalEventType::Compact)
        .find_map(|e| {
            e.metadata
                .as_ref()
                .and_then(|m| m.get("compact_summary"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        }))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_journal::JournalDirGuard;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct RecordingArtifactStore {
        seen: Arc<Mutex<Option<SessionArtifactJsonRecord>>>,
    }

    #[async_trait]
    impl SessionArtifactJsonStore for RecordingArtifactStore {
        async fn persist_json_artifact(
            &self,
            record: SessionArtifactJsonRecord,
        ) -> Result<StoredSessionArtifact, crate::SessionArtifactStoreError> {
            *astra_core::sync_poison::recover_mutex_lock(&self.seen) = Some(record.clone());
            Ok(StoredSessionArtifact {
                artifact_id: "artifact-1".into(),
                session_id: record.session_id,
                user_id: record.user_id,
                artifact_kind: record.artifact_kind,
                source: record.source,
                turn: record.turn,
                round: record.round,
                content: record.content,
                metadata: record.metadata,
                retention_policy: Some("default".into()),
                retention_until: None,
                status: Some("active".into()),
                referenced_by_manifest_count: 0,
                referenced_by_state_items_count: 0,
                referenced_by_citation_count: 0,
                created_at: Some("2026-04-25T14:00:00Z".into()),
            })
        }

        async fn load_json_artifact(
            &self,
            _user_id: &str,
            _session_id: &str,
            _artifact_id: &str,
        ) -> Result<Option<StoredSessionArtifact>, crate::SessionArtifactStoreError> {
            Ok(None)
        }

        async fn load_latest_json_artifact(
            &self,
            _user_id: &str,
            _session_id: &str,
            _artifact_kind: &str,
        ) -> Result<Option<StoredSessionArtifact>, crate::SessionArtifactStoreError> {
            Ok(None)
        }

        async fn list_json_artifacts(
            &self,
            _user_id: &str,
            _session_id: &str,
            _artifact_kind: Option<&str>,
            _limit: usize,
        ) -> Result<Vec<StoredSessionArtifact>, crate::SessionArtifactStoreError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn new_workspace_has_correct_defaults() {
        let ws =
            WorkspaceMetadata::with_context("sess-1", "gpt-4", "/home/user/project", Some("main"));
        assert_eq!(ws.session_id, "sess-1");
        assert_eq!(ws.model.as_deref(), Some("gpt-4"));
        assert_eq!(ws.cwd, "/home/user/project");
        assert_eq!(ws.git_branch, Some("main".to_string()));
        assert_eq!(ws.turn_count, 0);
        assert_eq!(ws.total_tokens_in, 0);
        assert_eq!(ws.status, "active");
        assert!(ws.checkpoints.is_empty());
    }

    #[test]
    fn record_turn_increments_counters() {
        let mut ws = WorkspaceMetadata::with_context("s", "m", "/tmp", None);
        ws.record_turn(100, 50, 20, 5);
        assert_eq!(ws.turn_count, 1);
        assert_eq!(ws.total_tokens_in, 100);
        assert_eq!(ws.total_tokens_out, 50);
        assert_eq!(ws.total_cache_read_tokens, 20);
        assert_eq!(ws.total_cache_creation_tokens, 5);

        ws.record_turn(200, 100, 30, 7);
        assert_eq!(ws.turn_count, 2);
        assert_eq!(ws.total_tokens_in, 300);
        assert_eq!(ws.total_tokens_out, 150);
        assert_eq!(ws.total_cache_read_tokens, 50);
        assert_eq!(ws.total_cache_creation_tokens, 12);
    }

    #[test]
    fn record_checkpoint_appends_turn_number() {
        let mut ws = WorkspaceMetadata::with_context("s", "m", "/tmp", None);
        ws.record_turn(10, 5, 0, 0);
        ws.record_turn(10, 5, 0, 0);
        ws.record_turn(10, 5, 0, 0);
        ws.record_checkpoint();
        assert_eq!(ws.checkpoints, vec![3]);

        ws.record_turn(10, 5, 0, 0);
        ws.record_turn(10, 5, 0, 0);
        ws.record_checkpoint();
        assert_eq!(ws.checkpoints, vec![3, 5]);
    }

    #[test]
    fn mark_completed_updates_status() {
        let mut ws = WorkspaceMetadata::with_context("s", "m", "/tmp", None);
        ws.mark_completed(Some("Task done"));
        assert_eq!(ws.status, "completed");
        assert_eq!(ws.summary, Some("Task done".to_string()));
    }

    #[test]
    fn mark_error_updates_status() {
        let mut ws = WorkspaceMetadata::with_context("s", "m", "/tmp", None);
        ws.mark_error("Connection refused");
        assert_eq!(ws.status, "error");
        assert!(ws.summary.as_ref().unwrap().contains("Connection refused"));
    }

    #[test]
    fn workspace_serializes_to_yaml() {
        let ws = WorkspaceMetadata::with_context("sess-1", "gpt-4", "/home/user", Some("main"));
        let yaml = serde_yaml_ng::to_string(&ws).unwrap();
        assert!(yaml.contains("session_id: sess-1"));
        assert!(yaml.contains("model: gpt-4"));
        assert!(yaml.contains("status: active"));
        // Optional empty fields should be omitted
        assert!(!yaml.contains("summary"));
    }

    #[test]
    fn workspace_does_not_persist_symbolic_default_model() {
        let ws = WorkspaceMetadata::with_context("sess-1", " default ", "/home/user", Some("main"));
        let yaml = serde_yaml_ng::to_string(&ws).unwrap();
        assert!(!yaml.contains("model:"));
    }

    #[test]
    fn workspace_legacy_default_model_deserializes_as_no_override() {
        let yaml = "session_id: s\ncwd: /tmp\nmodel: default\ncreated_at: '2025-01-01T00:00:00Z'\nupdated_at: '2025-01-01T00:00:00Z'\nturn_count: 0\ntotal_tokens_in: 0\ntotal_tokens_out: 0\nstatus: active\n";
        let ws: WorkspaceMetadata = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(ws.model.is_none());
    }

    #[test]
    fn workspace_yaml_round_trip() {
        let mut ws = WorkspaceMetadata::with_context("sess-1", "gpt-4", "/home/user", Some("main"));
        ws.permission_mode = Some("plan".into());
        ws.record_turn(100, 50, 25, 4);
        ws.record_checkpoint();
        ws.mark_completed(Some("Done"));

        let yaml = serde_yaml_ng::to_string(&ws).unwrap();
        let parsed: WorkspaceMetadata = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(parsed.session_id, "sess-1");
        assert_eq!(parsed.permission_mode.as_deref(), Some("plan"));
        assert_eq!(parsed.turn_count, 1);
        assert_eq!(parsed.checkpoints, vec![1]);
        assert_eq!(parsed.status, "completed");
        assert_eq!(parsed.summary, Some("Done".to_string()));
        assert_eq!(parsed.total_cache_read_tokens, 25);
        assert_eq!(parsed.total_cache_creation_tokens, 4);
        assert!(
            !yaml.contains("health_avoidance_tools"),
            "tool health belongs in turn health surfaces, not workspace metadata"
        );
    }

    #[test]
    fn workspace_remote_artifact_record_uses_workspace_kind() {
        let mut ws = WorkspaceMetadata::with_context("sess-1", "gpt-4", "/home/user", Some("main"));
        ws.record_turn(100, 50, 0, 0);
        let record = to_remote_artifact_record(&ws, "user-1").unwrap();
        assert_eq!(record.session_id, "sess-1");
        assert_eq!(record.user_id, "user-1");
        assert_eq!(record.artifact_kind, WORKSPACE_METADATA_ARTIFACT_KIND);
        assert_eq!(record.source.as_deref(), Some("workspace_metadata"));
        assert_eq!(record.turn, Some(1));
        assert_eq!(record.content["cwd"], "/home/user");
        assert_eq!(record.metadata.as_ref().unwrap()["status"], "active");
    }

    #[tokio::test]
    async fn persist_remote_workspace_uses_workspace_record() {
        let mut ws = WorkspaceMetadata::with_context("sess-1", "gpt-4", "/home/user", Some("main"));
        ws.record_turn(100, 50, 0, 0);
        let store = RecordingArtifactStore::default();

        let stored = persist_remote_workspace(&ws, "user-1", &store)
            .await
            .unwrap();
        let seen = store
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("captured record");

        assert_eq!(seen.artifact_kind, WORKSPACE_METADATA_ARTIFACT_KIND);
        assert_eq!(seen.turn, Some(1));
        assert_eq!(stored.artifact_id, "artifact-1");
        assert_eq!(stored.content["session_id"], "sess-1");
    }

    #[test]
    fn workspace_context_trace_signal_round_trip() {
        let mut ws = WorkspaceMetadata::with_context("sess-trace", "gpt-4", "/tmp", Some("main"));
        ws.last_context_trace = Some(ContextTraceSignal {
            turn_id: "turn-7".into(),
            captured_at: Some("2026-04-10T12:00:00Z".into()),
            tool_surface: Some(ContextTraceToolSurface {
                tools_available: 12,
                visible_tools: vec!["lsp".into(), "view".into()],
                surface_scope: "latest_round".into(),
                latency_ms: 18,
            }),
            memory: Some(ContextTraceMemorySignal {
                query: "resume trace persistence".into(),
                candidates_considered: 7,
                selected_memory_ids: vec!["m1".into(), "m2".into()],
                total_tokens: 240,
                latency_ms: 9,
            }),
            history: Some(ContextTraceHistorySignal {
                total_turns_available: 10,
                retained_turns: 5,
                compressed_turns: 3,
                dropped_turns: 2,
                compression_ratio: 0.58,
                tokens_before: 900,
                tokens_after: 522,
            }),
            budget: Some(ContextTraceBudgetSignal {
                max_tokens: 20_000,
                total_used: 14_200,
                budget_pressure: 0.84,
                compression_triggered: true,
            }),
            timing: Some(ContextTraceTimingSignal {
                turn: 7,
                context_assembly_ms: 14,
                ttft_ms: 220,
                llm_total_ms: 1100,
                tool_execution_ms: 330,
                total_ms: 1600,
            }),
            explanations: vec!["Kept LSP because symbol-aware navigation was required.".into()],
        });

        let yaml = serde_yaml_ng::to_string(&ws).unwrap();
        let parsed: WorkspaceMetadata = serde_yaml_ng::from_str(&yaml).unwrap();

        assert_eq!(parsed.last_context_trace, ws.last_context_trace);
    }

    #[test]
    fn background_shell_tasks_round_trip_through_workspace_yaml() {
        let mut ws = WorkspaceMetadata::with_context("sess-bg", "gpt-4", "/tmp", Some("main"));
        ws.background_shell_tasks = vec![BackgroundShellTaskProjection {
            id: "bg-shell-1".into(),
            status: "running".into(),
            title: "cargo test -p astra-cli".into(),
            started_at_ms: 1_766_000_000_123,
            ended_at_ms: Some(1_766_000_005_678),
            stdout_path: "/tmp/astra/bg-shell-1.stdout".into(),
            stderr_path: "/tmp/astra/bg-shell-1.stderr".into(),
            exit_code: None,
            terminal_reason: None,
        }];

        let yaml = serde_yaml_ng::to_string(&ws).unwrap();
        let parsed: WorkspaceMetadata = serde_yaml_ng::from_str(&yaml).unwrap();

        assert_eq!(parsed.background_shell_tasks, ws.background_shell_tasks);
    }

    #[test]
    fn background_local_agent_tasks_round_trip_through_workspace_yaml() {
        let mut ws =
            WorkspaceMetadata::with_context("sess-bg-agent", "gpt-4", "/tmp", Some("main"));
        ws.background_local_agent_tasks = vec![BackgroundLocalAgentTaskProjection {
            id: "agent-1".into(),
            status: "running".into(),
            title: "review auth flow".into(),
            started_at_ms: 1_766_000_000_123,
            ended_at_ms: None,
            output_tail: Some("reviewing auth middleware".into()),
            terminal_reason: None,
            fanout: Some(BackgroundLocalAgentFanoutProjection {
                group_id: "review-1".into(),
                group_title: "review fanout".into(),
                target_count: 3,
                slot_index: 1,
                slot_label: "review auth flow".into(),
            }),
        }];

        let yaml = serde_yaml_ng::to_string(&ws).unwrap();
        let parsed: WorkspaceMetadata = serde_yaml_ng::from_str(&yaml).unwrap();

        assert_eq!(
            parsed.background_local_agent_tasks,
            ws.background_local_agent_tasks
        );
    }

    #[test]
    fn context_trace_preview_labels_tool_surface_scope() {
        let trace = ContextTraceSignal {
            turn_id: "turn-7".into(),
            captured_at: None,
            tool_surface: Some(ContextTraceToolSurface {
                tools_available: 4,
                visible_tools: vec!["lsp".into()],
                surface_scope: "latest_round".into(),
                latency_ms: 9,
            }),
            memory: None,
            history: None,
            budget: None,
            timing: None,
            explanations: Vec::new(),
        };

        assert!(trace.preview().contains("tools[latest_round]: lsp"));
    }

    #[test]
    fn workspace_fork_and_coordination_round_trip() {
        let mut ws = WorkspaceMetadata::with_context("child", "gpt-4", "/proj", Some("main"));
        ws.parent_session_id = Some("parent-uuid".into());
        ws.forked_at_turn = Some(7);
        ws.fork_note = Some("experiment".into());
        ws.correlation_id = Some("corr-abc".into());
        ws.agent_role = Some("planner".into());
        let yaml = serde_yaml_ng::to_string(&ws).unwrap();
        let parsed: WorkspaceMetadata = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(parsed.parent_session_id.as_deref(), Some("parent-uuid"));
        assert_eq!(parsed.forked_at_turn, Some(7));
        assert_eq!(parsed.fork_note.as_deref(), Some("experiment"));
        assert_eq!(parsed.correlation_id.as_deref(), Some("corr-abc"));
        assert_eq!(parsed.agent_role.as_deref(), Some("planner"));
    }

    #[test]
    fn write_read_workspace_file() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "test-ws-1";
        let dir = tmp.path().join(".astra").join("sessions").join(session_id);
        std::fs::create_dir_all(&dir).unwrap();

        let mut ws = WorkspaceMetadata::with_context(session_id, "claude", "/tmp", None);
        ws.record_turn(200, 100, 40, 8);

        // Write to the temp dir
        let path = dir.join("workspace.yaml");
        let yaml = serde_yaml_ng::to_string(&ws).unwrap();
        std::fs::write(&path, &yaml).unwrap();

        // Read back
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: WorkspaceMetadata = serde_yaml_ng::from_str(&content).unwrap();
        assert_eq!(parsed.session_id, session_id);
        assert_eq!(parsed.turn_count, 1);
        assert_eq!(parsed.total_tokens_in, 200);
        assert_eq!(parsed.total_cache_read_tokens, 40);
        assert_eq!(parsed.total_cache_creation_tokens, 8);
    }

    #[test]
    fn workspace_plan_state_round_trip() {
        let mut ws = WorkspaceMetadata::with_context("plan-sess", "gpt-4", "/tmp", Some("main"));
        ws.executing_plan_json = Some(
            r#"{"subtasks":[{"id":"s1","title":"task 1","status":"InProgress","depends_on":[]}]}"#
                .to_string(),
        );
        ws.plan_goal = Some("Implement feature X".to_string());
        ws.plan_config_json = Some(r#"{"step_by_step":true}"#.to_string());
        ws.plan_execution_rounds = 3;

        let yaml = serde_yaml_ng::to_string(&ws).unwrap();
        let parsed: WorkspaceMetadata = serde_yaml_ng::from_str(&yaml).unwrap();

        assert_eq!(parsed.executing_plan_json, ws.executing_plan_json);
        assert_eq!(parsed.plan_goal, Some("Implement feature X".to_string()));
        assert_eq!(parsed.plan_config_json, ws.plan_config_json);
        assert_eq!(parsed.plan_execution_rounds, 3);
    }

    #[test]
    fn workspace_no_plan_omits_fields() {
        let ws = WorkspaceMetadata::with_context("s", "m", "/tmp", None);
        let yaml = serde_yaml_ng::to_string(&ws).unwrap();
        // Plan fields should be omitted when None/0
        assert!(!yaml.contains("executing_plan_json"));
        assert!(!yaml.contains("plan_goal"));
        assert!(!yaml.contains("plan_config_json"));
        assert!(!yaml.contains("plan_execution_rounds"));
    }

    #[test]
    fn format_project_context_empty() {
        assert!(format_project_context(&[]).is_empty());
    }

    #[test]
    fn format_project_context_renders_summaries() {
        let summaries = vec![
            ProjectSessionSummary {
                session_id: "s1".into(),
                summary: Some("Fixed auth bug".into()),
                model: Some("gpt-4".into()),
                turn_count: 15,
                status: "completed".into(),
                updated_at: "2025-01-15T10:00:00Z".into(),
                git_branch: Some("main".into()),
            },
            ProjectSessionSummary {
                session_id: "s2".into(),
                summary: None,
                model: Some("claude".into()),
                turn_count: 3,
                status: "active".into(),
                updated_at: "2025-01-14T08:00:00Z".into(),
                git_branch: None,
            },
        ];
        let ctx = format_project_context(&summaries);
        assert!(ctx.contains("[Project context"));
        assert!(ctx.contains("Fixed auth bug"));
        assert!(ctx.contains("(no summary)"));
        assert!(ctx.contains("branch: main"));
        assert!(ctx.contains("15 turns"));
    }

    #[test]
    fn format_project_context_truncates_long_summary() {
        let long_summary = "x".repeat(300);
        let summaries = vec![ProjectSessionSummary {
            session_id: "s1".into(),
            summary: Some(long_summary),
            model: Some("gpt-4".into()),
            turn_count: 5,
            status: "completed".into(),
            updated_at: "2025-01-15T10:00:00Z".into(),
            git_branch: Some("main".into()),
        }];
        let ctx = format_project_context(&summaries);
        // Summary should be truncated to 200 chars
        assert!(
            ctx.len() < 400,
            "context should be bounded, got {} chars",
            ctx.len()
        );
        // But should still contain the core structure
        assert!(ctx.contains("[Project context"));
        assert!(ctx.contains("[completed]"));
    }

    #[test]
    fn format_project_context_handles_short_updated_at() {
        // Edge case: updated_at shorter than 10 chars
        let summaries = vec![ProjectSessionSummary {
            session_id: "s1".into(),
            summary: Some("Short summary".into()),
            model: Some("m".into()),
            turn_count: 1,
            status: "active".into(),
            updated_at: "2025".into(), // only 4 chars
            git_branch: None,
        }];
        let ctx = format_project_context(&summaries);
        assert!(ctx.contains("2025")); // should not panic on short string
        assert!(ctx.contains("branch: ?"));
    }

    #[test]
    #[serial_test::serial]
    fn list_sessions_by_git_root_skips_corrupt_workspace_without_hiding_valid_matches() {
        let temp = tempfile::tempdir().unwrap();
        let sessions_dir = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let _guard = JournalDirGuard::new(&sessions_dir);

        let valid_sid = "git-root-valid";
        let corrupt_sid = "git-root-corrupt";
        std::fs::create_dir_all(
            crate::session_journal::journal_file_path(valid_sid)
                .parent()
                .unwrap(),
        )
        .unwrap();
        std::fs::write(crate::session_journal::journal_file_path(valid_sid), "").unwrap();
        std::fs::write(crate::session_journal::journal_file_path(corrupt_sid), "").unwrap();

        let mut valid = WorkspaceMetadata::new(valid_sid, "gpt-5");
        valid.git_root = Some("/repo".to_string());
        valid.summary = Some("keep me".to_string());
        write_workspace(&valid).unwrap();

        let mut corrupt = WorkspaceMetadata::new(corrupt_sid, "gpt-5");
        corrupt.git_root = Some("/repo".to_string());
        write_workspace(&corrupt).unwrap();
        let corrupt_path = workspace_file_path(corrupt_sid).unwrap();
        std::fs::write(&corrupt_path, ":\nnot-valid-yaml").unwrap();

        let summaries = list_sessions_by_git_root("/repo", None, 10);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].session_id, valid_sid);
        assert_eq!(summaries[0].summary.as_deref(), Some("keep me"));
    }

    #[test]
    fn finalize_workspace_on_end_with_compact_summary() {
        use crate::session_journal;

        let sid = format!("test-finalize-{}", std::process::id());
        // Create workspace
        let ws = WorkspaceMetadata::new(&sid, "test-model");
        write_workspace(&ws).unwrap();

        // Write a compact event with summary
        let journal = session_journal::JournalWriter::new(&sid).unwrap();
        let evt = session_journal::JournalEvent::compact_with_summary(
            Some(&sid),
            5,
            3,
            1,
            Some("User implemented auth system"),
        );
        journal.append(&evt).unwrap();

        // Finalize
        let summary = finalize_workspace_on_end(&sid);
        assert_eq!(summary.as_deref(), Some("User implemented auth system"));

        // Verify workspace was updated
        let ws2 = read_workspace(&sid).unwrap();
        assert_eq!(ws2.status, "completed");
        assert_eq!(ws2.summary.as_deref(), Some("User implemented auth system"));

        // Cleanup
        let _ = std::fs::remove_dir_all(workspace_dir_for(&sid));
        let _ = std::fs::remove_dir_all(workspace_dir_for(&sid));
    }

    #[test]
    fn finalize_workspace_on_end_no_compact_no_summary() {
        let sid = format!("test-finalize-empty-{}", std::process::id());
        // Create workspace with no journal events
        let ws = WorkspaceMetadata::new(&sid, "test-model");
        write_workspace(&ws).unwrap();

        // Finalize: no compact events → no summary
        let summary = finalize_workspace_on_end(&sid);
        assert!(summary.is_none());

        // Verify workspace was marked completed but has no summary
        let ws2 = read_workspace(&sid).unwrap();
        assert_eq!(ws2.status, "completed");
        assert!(ws2.summary.is_none());

        // Cleanup
        let _ = std::fs::remove_dir_all(workspace_dir_for(&sid));
    }

    #[test]
    fn finalize_workspace_on_end_ignores_invalid_workspace_without_overwriting() {
        let sid = format!("test-finalize-invalid-{}", std::process::id());
        let ws = WorkspaceMetadata::new(&sid, "test-model");
        write_workspace(&ws).unwrap();

        let workspace_path = workspace_file_path(&sid).unwrap();
        std::fs::write(&workspace_path, ":\nnot-valid-yaml").unwrap();

        let summary = finalize_workspace_on_end(&sid);
        assert!(summary.is_none());
        assert_eq!(
            std::fs::read_to_string(&workspace_path).unwrap(),
            ":\nnot-valid-yaml"
        );

        let _ = std::fs::remove_dir_all(workspace_dir_for(&sid));
    }

    #[test]
    #[serial_test::serial]
    fn extract_last_compact_summary_surfaces_unreadable_journal() {
        let temp = tempfile::tempdir().unwrap();
        let sessions_dir = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let _guard = JournalDirGuard::new(&sessions_dir);
        let sid = "workspace-summary-unreadable";
        std::fs::create_dir_all(crate::session_journal::journal_file_path(sid)).unwrap();

        let error = extract_last_compact_summary(sid)
            .expect_err("directory journal path should surface an error");

        assert_eq!(error.kind(), std::io::ErrorKind::IsADirectory);
    }

    #[test]
    fn workspace_adaptive_state_round_trip() {
        let mut ws =
            WorkspaceMetadata::with_context("adapt-sess", "gpt-4", "/tmp", Some("feature-x"));
        ws.last_scenario_change_turn = Some(12);
        ws.last_token_budget_direction = -1;
        ws.last_token_budget_change_turn = Some(10);
        ws.active_experiment_id = Some("exp-001".to_string());
        ws.active_variant = Some("treatment-a".to_string());
        ws.tuned_config_json = Some(r#"{"max_tokens":4096}"#.to_string());

        let yaml = serde_yaml_ng::to_string(&ws).unwrap();
        let parsed: WorkspaceMetadata = serde_yaml_ng::from_str(&yaml).unwrap();

        assert_eq!(parsed.last_scenario_change_turn, Some(12));
        assert_eq!(parsed.last_token_budget_direction, -1);
        assert_eq!(parsed.last_token_budget_change_turn, Some(10));
        assert_eq!(parsed.active_experiment_id.as_deref(), Some("exp-001"));
        assert_eq!(parsed.active_variant.as_deref(), Some("treatment-a"));
        assert_eq!(
            parsed.tuned_config_json.as_deref(),
            Some(r#"{"max_tokens":4096}"#)
        );
    }

    #[test]
    fn workspace_adaptive_state_defaults_on_missing_fields() {
        // YAML from older versions without adaptive fields should deserialize cleanly
        let yaml = "session_id: s\ncwd: /tmp\nmodel: m\ncreated_at: '2025-01-01T00:00:00Z'\nupdated_at: '2025-01-01T00:00:00Z'\nturn_count: 5\ntotal_tokens_in: 100\ntotal_tokens_out: 50\nstatus: active\n";
        let ws: WorkspaceMetadata = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(ws.last_scenario_change_turn, None);
        assert_eq!(ws.last_token_budget_direction, 0);
        assert_eq!(ws.last_token_budget_change_turn, None);
        assert_eq!(ws.active_experiment_id, None);
        assert_eq!(ws.active_variant, None);
        assert_eq!(ws.tuned_config_json, None);
    }

    #[test]
    fn workspace_adaptive_state_omitted_when_default() {
        let ws = WorkspaceMetadata::with_context("s", "m", "/tmp", None);
        let yaml = serde_yaml_ng::to_string(&ws).unwrap();
        assert!(
            !yaml.contains("last_scenario_change_turn"),
            "should omit None fields"
        );
        assert!(
            !yaml.contains("last_token_budget_change_turn"),
            "should omit None fields"
        );
        assert!(
            !yaml.contains("active_experiment_id"),
            "should omit None fields"
        );
        assert!(!yaml.contains("active_variant"), "should omit None fields");
        assert!(
            !yaml.contains("tuned_config_json"),
            "should omit None fields"
        );
        assert!(
            !yaml.contains("health_avoidance_tools"),
            "tool health belongs in turn health surfaces, not workspace metadata"
        );
    }

    #[test]
    #[serial_test::serial]
    fn read_workspace_optional_distinguishes_missing_invalid_and_existing() {
        let temp = tempfile::tempdir().unwrap();
        let sessions_dir = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let _guard = JournalDirGuard::new(&sessions_dir);
        let session_id = "workspace-optional-session";

        assert!(read_workspace_optional(session_id).unwrap().is_none());

        let mut ws = WorkspaceMetadata::new(session_id, "gpt-5");
        ws.cwd = "/repo".to_string();
        write_workspace(&ws).unwrap();
        assert_eq!(
            read_workspace_optional(session_id).unwrap().unwrap().cwd,
            "/repo"
        );

        std::fs::write(
            workspace_file_path(session_id).unwrap(),
            ":\nnot-valid-yaml",
        )
        .unwrap();
        let error = read_workspace_optional(session_id).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    #[serial_test::serial]
    fn backup_invalid_workspace_file_preserves_corrupt_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let sessions_dir = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let _guard = JournalDirGuard::new(&sessions_dir);
        let session_id = "workspace-backup-session";

        let path = workspace_file_path(session_id).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let corrupt_bytes = b":\nnot-valid-yaml".to_vec();
        std::fs::write(&path, &corrupt_bytes).unwrap();

        let backup = backup_invalid_workspace_file(session_id)
            .unwrap()
            .expect("existing corrupt workspace should be moved aside");

        assert!(!path.exists(), "original workspace path should be vacated");
        assert_eq!(std::fs::read(&backup).unwrap(), corrupt_bytes);
        assert!(
            backup
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("workspace.yaml.corrupt-"))
        );
    }

    #[test]
    fn write_workspace_rejects_invalid_session_id() {
        let ws = WorkspaceMetadata::with_context("../bad", "gpt-5", "/repo", Some("main"));
        let error = write_workspace(&ws).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
}
