//! Session workspace metadata — describes a session's runtime context.
//!
//! Written once on session start and updated per-turn with cumulative stats.
//! Stored at `~/.mo-agent/sessions/<session_id>/workspace.yaml`.
//!
//! This provides:
//! - Quick session identification without parsing the JSONL journal
//! - Context for session resumption and debugging
//! - Foundation for checkpoint-based rewind

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
        }
    }

    /// Create metadata with explicit values (for testing).
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
        .join(".mo-agent")
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
    fn write_read_workspace_file() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "test-ws-1";
        let dir = tmp
            .path()
            .join(".mo-agent")
            .join("sessions")
            .join(session_id);
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
    }
}
