//! Session workspace metadata — describes a session's runtime context.
//!
//! Written once on session start and updated per-turn with cumulative stats.
//! Stored at `~/.astra/sessions/<session_id>/workspace.yaml`.
//!
//! This provides:
//! - Quick session identification without parsing the JSONL journal
//! - Context for session resumption and debugging
//! - Foundation for checkpoint-based rewind

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn is_zero(v: &usize) -> bool {
    *v == 0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextTraceToolSelection {
    #[serde(default)]
    pub tools_available: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_tools: Vec<String>,
    #[serde(default)]
    pub rejected_tools: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub strategy: String,
    #[serde(default)]
    pub confidence: f64,
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
    pub tool_selection: Option<ContextTraceToolSelection>,
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
        if let Some(selection) = self.tool_selection.as_ref() {
            if !selection.selected_tools.is_empty() {
                parts.push(format!("tools: {}", selection.selected_tools.join(", ")));
            }
            if !selection.strategy.is_empty() {
                parts.push(format!(
                    "strategy: {} ({:.2})",
                    selection.strategy, selection.confidence
                ));
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GoalMilestoneSignalSnapshot {
    ToolSuccess { tool: String, detail: String },
    TestPass { count: u32 },
    TestFail { count: u32 },
    FileChanged { path: String },
    CommitMade { message: String },
    UserApproval,
    UserDisapproval,
    BuildSuccess,
    BuildFail,
}

impl GoalMilestoneSignalSnapshot {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ToolSuccess { .. } => "tool_success",
            Self::TestPass { .. } => "test_pass",
            Self::TestFail { .. } => "test_fail",
            Self::FileChanged { .. } => "file_changed",
            Self::CommitMade { .. } => "commit_made",
            Self::UserApproval => "user_approval",
            Self::UserDisapproval => "user_disapproval",
            Self::BuildSuccess => "build_success",
            Self::BuildFail => "build_fail",
        }
    }

    pub fn detail(&self) -> Option<String> {
        match self {
            Self::ToolSuccess { tool, detail } => Some(format!("{tool}: {detail}")),
            Self::TestPass { count } => Some(format!("{count} tests passed")),
            Self::TestFail { count } => Some(format!("{count} tests failed")),
            Self::FileChanged { path } => Some(path.clone()),
            Self::CommitMade { message } => Some(message.clone()),
            Self::UserApproval => Some("user approved".to_string()),
            Self::UserDisapproval => Some("user rejected".to_string()),
            Self::BuildSuccess => Some("build succeeded".to_string()),
            Self::BuildFail => Some("build failed".to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalMilestoneSnapshot {
    pub turn: u32,
    pub signal: GoalMilestoneSignalSnapshot,
    #[serde(default)]
    pub relevance: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalProgressSnapshot {
    pub goal: String,
    #[serde(default)]
    pub completion_score: f64,
    #[serde(default)]
    pub momentum: f64,
    #[serde(default)]
    pub milestone_count: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(default)]
    pub weighted_progress: f64,
    #[serde(default)]
    pub negative_signals: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub milestones: Vec<GoalMilestoneSnapshot>,
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
    /// LLM model used.
    pub model: String,
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

    // ─── Session state persistence (for resume) ───
    /// User-stated session goal (e.g. "implement auth module").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_goal: Option<String>,
    /// Persisted live goal-tracker state for resume and self-surface reporting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_progress: Option<GoalProgressSnapshot>,
    /// Skills explicitly pinned by the user.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_skills: Vec<String>,
    /// Skills discovered during this session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discovered_skills: Vec<String>,
    /// Tools manually pinned by self-modification actions for this session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_tools: Vec<String>,
    /// Tools manually deprioritized by self-modification actions for this session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deprioritized_tools: Vec<String>,

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
            model: model.to_string(),
            created_at: now.clone(),
            updated_at: now,
            turn_count: 0,
            total_tokens_in: 0,
            total_tokens_out: 0,
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
            session_goal: None,
            goal_progress: None,
            pinned_skills: Vec::new(),
            discovered_skills: Vec::new(),
            pinned_tools: Vec::new(),
            deprioritized_tools: Vec::new(),
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
            model: model.to_string(),
            created_at: now.clone(),
            updated_at: now,
            turn_count: 0,
            total_tokens_in: 0,
            total_tokens_out: 0,
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
            session_goal: None,
            goal_progress: None,
            pinned_skills: Vec::new(),
            discovered_skills: Vec::new(),
            pinned_tools: Vec::new(),
            deprioritized_tools: Vec::new(),
            last_scenario_change_turn: None,
            last_token_budget_direction: 0,
            last_token_budget_change_turn: None,
            active_experiment_id: None,
            active_variant: None,
            tuned_config_json: None,
        }
    }

    /// Update after a turn completes.
    pub fn record_turn(&mut self, tokens_in: u64, tokens_out: u64) {
        self.turn_count += 1;
        self.total_tokens_in += tokens_in;
        self.total_tokens_out += tokens_out;
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

/// Write workspace metadata to disk.
pub fn write_workspace(metadata: &WorkspaceMetadata) -> std::io::Result<()> {
    let dir = workspace_dir(&metadata.session_id);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("workspace.yaml");
    let yaml = serde_yaml::to_string(metadata)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = dir.join(".workspace.yaml.tmp");
    // Write to tmp, set perms, fsync, then atomically rename.
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(yaml.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        file.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Read workspace metadata from disk.
pub fn read_workspace(session_id: &str) -> std::io::Result<WorkspaceMetadata> {
    let path = workspace_dir(session_id).join("workspace.yaml");
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
    serde_yaml::from_str(&content)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Get the workspace directory for a session.
fn workspace_dir(session_id: &str) -> PathBuf {
    assert!(
        crate::session_journal::validate_session_id(session_id).is_ok(),
        "unsafe session ID passed to workspace_dir: {session_id}"
    );
    crate::session_journal::local_sessions_dir().join(session_id)
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
    pub model: String,
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
        // Try reading workspace metadata; skip if unavailable
        if let Ok(ws) = read_workspace(sid)
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
    let mut ws = match read_workspace(session_id) {
        Ok(ws) => ws,
        Err(_) => return None,
    };

    // Extract summary from the last compact event's metadata
    let summary = extract_last_compact_summary(session_id);

    ws.mark_completed(summary.as_deref());

    if let Err(e) = write_workspace(&ws) {
        eprintln!("[knowledge-backflow] Failed to update workspace on end: {e}");
    }

    summary
}

/// Extract the summary from the last Compact journal event that has compact_summary metadata.
fn extract_last_compact_summary(session_id: &str) -> Option<String> {
    let events = crate::session_journal::read_journal(session_id).ok()?;
    events
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
        })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_workspace_has_correct_defaults() {
        let ws =
            WorkspaceMetadata::with_context("sess-1", "gpt-4", "/home/user/project", Some("main"));
        assert_eq!(ws.session_id, "sess-1");
        assert_eq!(ws.model, "gpt-4");
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
        ws.record_turn(100, 50);
        assert_eq!(ws.turn_count, 1);
        assert_eq!(ws.total_tokens_in, 100);
        assert_eq!(ws.total_tokens_out, 50);

        ws.record_turn(200, 100);
        assert_eq!(ws.turn_count, 2);
        assert_eq!(ws.total_tokens_in, 300);
        assert_eq!(ws.total_tokens_out, 150);
    }

    #[test]
    fn record_checkpoint_appends_turn_number() {
        let mut ws = WorkspaceMetadata::with_context("s", "m", "/tmp", None);
        ws.record_turn(10, 5);
        ws.record_turn(10, 5);
        ws.record_turn(10, 5);
        ws.record_checkpoint();
        assert_eq!(ws.checkpoints, vec![3]);

        ws.record_turn(10, 5);
        ws.record_turn(10, 5);
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
        let yaml = serde_yaml::to_string(&ws).unwrap();
        assert!(yaml.contains("session_id: sess-1"));
        assert!(yaml.contains("model: gpt-4"));
        assert!(yaml.contains("status: active"));
        // Optional empty fields should be omitted
        assert!(!yaml.contains("summary"));
    }

    #[test]
    fn workspace_yaml_round_trip() {
        let mut ws = WorkspaceMetadata::with_context("sess-1", "gpt-4", "/home/user", Some("main"));
        ws.record_turn(100, 50);
        ws.record_checkpoint();
        ws.mark_completed(Some("Done"));
        ws.pinned_tools = vec!["bash".into()];
        ws.deprioritized_tools = vec!["web_fetch".into()];

        let yaml = serde_yaml::to_string(&ws).unwrap();
        let parsed: WorkspaceMetadata = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.session_id, "sess-1");
        assert_eq!(parsed.turn_count, 1);
        assert_eq!(parsed.checkpoints, vec![1]);
        assert_eq!(parsed.status, "completed");
        assert_eq!(parsed.summary, Some("Done".to_string()));
        assert_eq!(parsed.pinned_tools, vec!["bash".to_string()]);
        assert_eq!(parsed.deprioritized_tools, vec!["web_fetch".to_string()]);
    }

    #[test]
    fn workspace_context_trace_signal_round_trip() {
        let mut ws = WorkspaceMetadata::with_context("sess-trace", "gpt-4", "/tmp", Some("main"));
        ws.last_context_trace = Some(ContextTraceSignal {
            turn_id: "turn-7".into(),
            captured_at: Some("2026-04-10T12:00:00Z".into()),
            tool_selection: Some(ContextTraceToolSelection {
                tools_available: 12,
                selected_tools: vec!["lsp".into(), "view".into()],
                rejected_tools: 4,
                strategy: "code-intel".into(),
                confidence: 0.91,
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

        let yaml = serde_yaml::to_string(&ws).unwrap();
        let parsed: WorkspaceMetadata = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(parsed.last_context_trace, ws.last_context_trace);
    }

    #[test]
    fn workspace_fork_and_coordination_round_trip() {
        let mut ws = WorkspaceMetadata::with_context("child", "gpt-4", "/proj", Some("main"));
        ws.parent_session_id = Some("parent-uuid".into());
        ws.forked_at_turn = Some(7);
        ws.fork_note = Some("experiment".into());
        ws.correlation_id = Some("corr-abc".into());
        ws.agent_role = Some("planner".into());
        let yaml = serde_yaml::to_string(&ws).unwrap();
        let parsed: WorkspaceMetadata = serde_yaml::from_str(&yaml).unwrap();
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
        ws.record_turn(200, 100);

        // Write to the temp dir
        let path = dir.join("workspace.yaml");
        let yaml = serde_yaml::to_string(&ws).unwrap();
        std::fs::write(&path, &yaml).unwrap();

        // Read back
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: WorkspaceMetadata = serde_yaml::from_str(&content).unwrap();
        assert_eq!(parsed.session_id, session_id);
        assert_eq!(parsed.turn_count, 1);
        assert_eq!(parsed.total_tokens_in, 200);
    }

    #[test]
    fn workspace_backward_compat_no_checkpoints() {
        // YAML without checkpoints field should deserialize with empty vec
        let yaml = "session_id: s\ncwd: /tmp\nmodel: m\ncreated_at: '2025-01-01T00:00:00Z'\nupdated_at: '2025-01-01T00:00:00Z'\nturn_count: 0\ntotal_tokens_in: 0\ntotal_tokens_out: 0\nstatus: active\n";
        let ws: WorkspaceMetadata = serde_yaml::from_str(yaml).unwrap();
        assert!(ws.checkpoints.is_empty());
        // Plan fields default to None/0
        assert!(ws.executing_plan_json.is_none());
        assert!(ws.plan_goal.is_none());
        assert!(ws.plan_config_json.is_none());
        assert_eq!(ws.plan_execution_rounds, 0);
        assert!(ws.last_context_trace.is_none());
        assert!(ws.goal_progress.is_none());
    }

    #[test]
    fn workspace_plan_state_round_trip() {
        let mut ws = WorkspaceMetadata::with_context("plan-sess", "gpt-4", "/tmp", Some("main"));
        ws.executing_plan_json = Some(
            r#"{"subtasks":[{"id":"s1","title":"task 1","status":"InProgress","depends_on":[]}]}"#
                .to_string(),
        );
        ws.plan_goal = Some("Implement feature X".to_string());
        ws.plan_config_json = Some(r#"{"step_by_step":true,"auto_execute":false}"#.to_string());
        ws.plan_execution_rounds = 3;

        let yaml = serde_yaml::to_string(&ws).unwrap();
        let parsed: WorkspaceMetadata = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(parsed.executing_plan_json, ws.executing_plan_json);
        assert_eq!(parsed.plan_goal, Some("Implement feature X".to_string()));
        assert_eq!(parsed.plan_config_json, ws.plan_config_json);
        assert_eq!(parsed.plan_execution_rounds, 3);
    }

    #[test]
    fn workspace_no_plan_omits_fields() {
        let ws = WorkspaceMetadata::with_context("s", "m", "/tmp", None);
        let yaml = serde_yaml::to_string(&ws).unwrap();
        // Plan fields should be omitted when None/0
        assert!(!yaml.contains("executing_plan_json"));
        assert!(!yaml.contains("plan_goal"));
        assert!(!yaml.contains("plan_config_json"));
        assert!(!yaml.contains("plan_execution_rounds"));
    }

    #[test]
    fn workspace_goal_progress_round_trip() {
        let mut ws = WorkspaceMetadata::with_context("goal-sess", "gpt-4", "/tmp", Some("main"));
        ws.session_goal = Some("ship auth".to_string());
        ws.goal_progress = Some(GoalProgressSnapshot {
            goal: "ship auth".to_string(),
            completion_score: 0.65,
            momentum: 0.4,
            milestone_count: 3,
            summary: "Well underway — 65% estimated".to_string(),
            weighted_progress: 1.2,
            negative_signals: 0.1,
            milestones: vec![
                GoalMilestoneSnapshot {
                    turn: 1,
                    signal: GoalMilestoneSignalSnapshot::FileChanged {
                        path: "src/auth.rs".to_string(),
                    },
                    relevance: 0.8,
                },
                GoalMilestoneSnapshot {
                    turn: 2,
                    signal: GoalMilestoneSignalSnapshot::TestPass { count: 12 },
                    relevance: 0.9,
                },
            ],
        });

        let yaml = serde_yaml::to_string(&ws).unwrap();
        let parsed: WorkspaceMetadata = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(parsed.goal_progress, ws.goal_progress);
        assert!(yaml.contains("goal_progress"));
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
                model: "gpt-4".into(),
                turn_count: 15,
                status: "completed".into(),
                updated_at: "2025-01-15T10:00:00Z".into(),
                git_branch: Some("main".into()),
            },
            ProjectSessionSummary {
                session_id: "s2".into(),
                summary: None,
                model: "claude".into(),
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
            model: "gpt-4".into(),
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
            model: "m".into(),
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
        let _ = std::fs::remove_dir_all(crate::session_journal::local_sessions_dir().join(&sid));
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
    fn workspace_adaptive_state_round_trip() {
        let mut ws =
            WorkspaceMetadata::with_context("adapt-sess", "gpt-4", "/tmp", Some("feature-x"));
        ws.last_scenario_change_turn = Some(12);
        ws.last_token_budget_direction = -1;
        ws.last_token_budget_change_turn = Some(10);
        ws.active_experiment_id = Some("exp-001".to_string());
        ws.active_variant = Some("treatment-a".to_string());
        ws.tuned_config_json = Some(r#"{"max_tokens":4096}"#.to_string());

        let yaml = serde_yaml::to_string(&ws).unwrap();
        let parsed: WorkspaceMetadata = serde_yaml::from_str(&yaml).unwrap();

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
        let ws: WorkspaceMetadata = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(ws.last_scenario_change_turn, None);
        assert_eq!(ws.last_token_budget_direction, 0);
        assert_eq!(ws.last_token_budget_change_turn, None);
        assert_eq!(ws.active_experiment_id, None);
        assert_eq!(ws.active_variant, None);
        assert_eq!(ws.tuned_config_json, None);
        assert!(ws.pinned_tools.is_empty());
        assert!(ws.deprioritized_tools.is_empty());
    }

    #[test]
    fn workspace_adaptive_state_omitted_when_default() {
        let ws = WorkspaceMetadata::with_context("s", "m", "/tmp", None);
        let yaml = serde_yaml::to_string(&ws).unwrap();
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
        assert!(!yaml.contains("pinned_tools"), "should omit empty vectors");
        assert!(
            !yaml.contains("deprioritized_tools"),
            "should omit empty vectors"
        );
    }
}
