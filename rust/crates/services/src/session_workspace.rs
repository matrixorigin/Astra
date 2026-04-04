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
        self.summary = summary.map(|s| s.to_string());
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
    std::fs::write(&path, yaml)?;
    Ok(())
}

/// Read workspace metadata from disk.
pub fn read_workspace(session_id: &str) -> std::io::Result<WorkspaceMetadata> {
    let path = workspace_dir(session_id).join("workspace.yaml");
    let content = std::fs::read_to_string(&path)?;
    serde_yaml::from_str(&content)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Get the workspace directory for a session.
fn workspace_dir(session_id: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".astra")
        .join("sessions")
        .join(session_id)
}

/// Get the workspace directory path (public, for use by checkpoint module).
pub fn workspace_dir_for(session_id: &str) -> PathBuf {
    workspace_dir(session_id)
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

        let yaml = serde_yaml::to_string(&ws).unwrap();
        let parsed: WorkspaceMetadata = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.session_id, "sess-1");
        assert_eq!(parsed.turn_count, 1);
        assert_eq!(parsed.checkpoints, vec![1]);
        assert_eq!(parsed.status, "completed");
        assert_eq!(parsed.summary, Some("Done".to_string()));
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
}
