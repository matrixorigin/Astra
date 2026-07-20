//! Skill-level checkpointing for crash recovery.
//!
//! # Design
//!
//! When a skill (isolated sub-run) crashes mid-execution, all progress is lost.
//! This module provides skill-level checkpoints that capture:
//!
//! 1. **Skill metadata**: name, instructions, allowed tools, model override
//! 2. **Execution state**: turns completed, tokens consumed, partial output
//! 3. **Tool history**: which tools executed within the skill (for exactly-once dedup)
//!
//! On recovery, the `SkillCheckpointManager` checks for interrupted skills and
//! resumes them from the last checkpoint instead of starting from scratch.
//!
//! # Unhappy Paths
//!
//! - Skill panics: checkpoint saved before each turn, can resume from last good state
//! - Tool execution error: checkpoint records error, skill can retry or skip
//! - Concurrent skill execution: checkpoints are per-skill-name, no cross-skill interference
//! - Checkpoint corruption: validation on load, falls back to fresh execution if invalid

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;
use thiserror::Error;

/// Checkpoint for a skill execution (sub-run).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillCheckpoint {
    /// Skill name (e.g., "review-code", "github-pr-review")
    pub skill_name: String,
    /// Skill instructions (for resuming with same context)
    pub instructions: String,
    /// Task context passed to the skill
    pub task_context: String,
    /// Model override (if any)
    /// Max tokens budget
    pub max_tokens: Option<u32>,
    /// Allowed tools for the skill
    pub allowed_tools: Vec<String>,
    /// Effort level (if specified)
    pub effort: Option<String>,
    /// Agent type (if specified)
    pub agent_type: Option<String>,
    /// Parent recursion depth
    pub parent_recursion_depth: u8,
    /// Execution state
    pub state: SkillExecutionState,
    /// Timestamp when checkpoint was created
    pub checkpointed_at: u64,
}

/// Execution state of a skill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SkillExecutionState {
    /// Skill is running (not yet completed)
    Running {
        /// Turns completed so far
        turns_completed: u32,
        /// Tokens consumed so far
        tokens_consumed: u32,
        /// Partial output accumulated
        partial_output: String,
        /// Tool calls executed within the skill (for dedup)
        tool_history: Vec<SkillToolCallRecord>,
    },
    /// Skill completed successfully
    Completed {
        /// Final output
        output: String,
        /// Total turns
        total_turns: u32,
        /// Total tokens
        total_tokens: u32,
    },
    /// Skill failed with error
    Failed {
        /// Error message
        error: String,
        /// Turns completed before failure
        turns_completed: u32,
        /// Tokens consumed before failure
        tokens_consumed: u32,
    },
}

/// Record of a tool call within a skill (for exactly-once dedup).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillToolCallRecord {
    /// Tool name
    pub tool_name: String,
    /// Tool arguments (JSON)
    pub args: serde_json::Value,
    /// Tool output
    pub output: String,
    /// Whether the tool returned an error
    pub is_error: bool,
    /// Timestamp
    pub executed_at: u64,
}

/// Errors during skill checkpoint operations.
#[derive(Debug, Error)]
pub enum SkillCheckpointError {
    #[error("Checkpoint file not found: {0}")]
    NotFound(PathBuf),

    #[error("Checkpoint file corrupted: {0}")]
    Corrupted(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Manages skill-level checkpoints for crash recovery.
pub struct SkillCheckpointManager {
    /// Directory where checkpoint files are stored
    checkpoint_dir: PathBuf,
    /// In-memory cache of active checkpoints (skill_name -> checkpoint)
    active_checkpoints: HashMap<String, SkillCheckpoint>,
}

impl SkillCheckpointManager {
    /// Create a new checkpoint manager.
    pub fn new(checkpoint_dir: PathBuf) -> Self {
        Self {
            checkpoint_dir,
            active_checkpoints: HashMap::new(),
        }
    }

    /// Initialize the checkpoint directory if it doesn't exist.
    pub fn init(&self) -> Result<(), SkillCheckpointError> {
        if !self.checkpoint_dir.exists() {
            std::fs::create_dir_all(&self.checkpoint_dir)?;
        }
        Ok(())
    }

    /// Save a checkpoint to disk.
    pub fn save_checkpoint(
        &mut self,
        checkpoint: SkillCheckpoint,
    ) -> Result<(), SkillCheckpointError> {
        let path = self.checkpoint_path(&checkpoint.skill_name);
        let json = serde_json::to_string_pretty(&checkpoint)?;
        std::fs::write(&path, json)?;
        self.active_checkpoints
            .insert(checkpoint.skill_name.clone(), checkpoint.clone());
        tracing::debug!(
            skill_name = %checkpoint.skill_name,
            path = %path.display(),
            "Skill checkpoint saved"
        );
        Ok(())
    }

    /// Load a checkpoint for a skill (if it exists).
    pub fn load_checkpoint(
        &self,
        skill_name: &str,
    ) -> Result<Option<SkillCheckpoint>, SkillCheckpointError> {
        // Check in-memory cache first
        if let Some(checkpoint) = self.active_checkpoints.get(skill_name) {
            return Ok(Some(checkpoint.clone()));
        }

        let path = self.checkpoint_path(skill_name);
        if !path.exists() {
            return Ok(None);
        }

        let json = std::fs::read_to_string(&path)?;
        let checkpoint: SkillCheckpoint = serde_json::from_str(&json)
            .map_err(|e| SkillCheckpointError::Corrupted(e.to_string()))?;

        tracing::debug!(
            skill_name = %skill_name,
            path = %path.display(),
            "Skill checkpoint loaded"
        );
        Ok(Some(checkpoint))
    }

    /// Check if a skill has an interrupted checkpoint that can be resumed.
    pub fn has_resumable_checkpoint(&self, skill_name: &str) -> bool {
        if let Ok(Some(checkpoint)) = self.load_checkpoint(skill_name) {
            matches!(checkpoint.state, SkillExecutionState::Running { .. })
        } else {
            false
        }
    }

    /// Mark a skill as completed and save the final checkpoint.
    pub fn mark_completed(
        &mut self,
        skill_name: &str,
        output: String,
        total_turns: u32,
        total_tokens: u32,
    ) -> Result<(), SkillCheckpointError> {
        if let Some(checkpoint) = self.active_checkpoints.get_mut(skill_name) {
            checkpoint.state = SkillExecutionState::Completed {
                output: output.clone(),
                total_turns,
                total_tokens,
            };
            checkpoint.checkpointed_at = current_timestamp();
            let checkpoint_clone = checkpoint.clone();
            return self.save_checkpoint(checkpoint_clone);
        }

        // If no active checkpoint, create a new completed one
        let checkpoint = SkillCheckpoint {
            skill_name: skill_name.to_string(),
            instructions: String::new(),
            task_context: String::new(),
            max_tokens: None,
            allowed_tools: Vec::new(),
            effort: None,
            agent_type: None,
            parent_recursion_depth: 0,
            state: SkillExecutionState::Completed {
                output,
                total_turns,
                total_tokens,
            },
            checkpointed_at: current_timestamp(),
        };
        self.save_checkpoint(checkpoint)
    }

    /// Mark a skill as failed and save the checkpoint.
    pub fn mark_failed(
        &mut self,
        skill_name: &str,
        error: String,
        turns_completed: u32,
        tokens_consumed: u32,
    ) -> Result<(), SkillCheckpointError> {
        if let Some(checkpoint) = self.active_checkpoints.get_mut(skill_name) {
            checkpoint.state = SkillExecutionState::Failed {
                error,
                turns_completed,
                tokens_consumed,
            };
            checkpoint.checkpointed_at = current_timestamp();
            let checkpoint_clone = checkpoint.clone();
            return self.save_checkpoint(checkpoint_clone);
        }

        let checkpoint = SkillCheckpoint {
            skill_name: skill_name.to_string(),
            instructions: String::new(),
            task_context: String::new(),
            max_tokens: None,
            allowed_tools: Vec::new(),
            effort: None,
            agent_type: None,
            parent_recursion_depth: 0,
            state: SkillExecutionState::Failed {
                error,
                turns_completed,
                tokens_consumed,
            },
            checkpointed_at: current_timestamp(),
        };
        self.save_checkpoint(checkpoint)
    }

    /// Delete a checkpoint (after skill completes or is abandoned).
    pub fn delete_checkpoint(&mut self, skill_name: &str) -> Result<(), SkillCheckpointError> {
        let path = self.checkpoint_path(skill_name);
        if path.exists() {
            std::fs::remove_file(&path)?;
            self.active_checkpoints.remove(skill_name);
            tracing::debug!(
                skill_name = %skill_name,
                path = %path.display(),
                "Skill checkpoint deleted"
            );
        }
        Ok(())
    }

    /// List all active (running) checkpoints.
    pub fn list_active_checkpoints(&self) -> Vec<&SkillCheckpoint> {
        self.active_checkpoints
            .values()
            .filter(|cp| matches!(cp.state, SkillExecutionState::Running { .. }))
            .collect()
    }

    /// Update a running checkpoint with progress (called after each turn).
    pub fn update_progress(
        &mut self,
        skill_name: &str,
        turns_completed: u32,
        tokens_consumed: u32,
        partial_output: &str,
        tool_history: Vec<SkillToolCallRecord>,
    ) -> Result<(), SkillCheckpointError> {
        let path = self.checkpoint_path(skill_name);
        let checkpoint = {
            let cp = self
                .active_checkpoints
                .get_mut(skill_name)
                .ok_or_else(|| SkillCheckpointError::NotFound(path.clone()))?;

            cp.state = SkillExecutionState::Running {
                turns_completed,
                tokens_consumed,
                partial_output: partial_output.to_string(),
                tool_history,
            };
            cp.checkpointed_at = current_timestamp();
            cp.clone()
        };
        self.save_checkpoint(checkpoint)
    }

    fn checkpoint_path(&self, skill_name: &str) -> PathBuf {
        self.checkpoint_dir
            .join(format!("{skill_name}.skill_checkpoint.json"))
    }
}

fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn checkpoint_manager_init_creates_directory() {
        let dir = tempdir().unwrap();
        let checkpoint_dir = dir.path().join("skill_checkpoints");
        let manager = SkillCheckpointManager::new(checkpoint_dir.clone());
        manager.init().unwrap();
        assert!(checkpoint_dir.exists());
    }

    #[test]
    fn save_and_load_checkpoint_roundtrip() {
        let dir = tempdir().unwrap();
        let mut manager = SkillCheckpointManager::new(dir.path().to_path_buf());
        manager.init().unwrap();

        let checkpoint = SkillCheckpoint {
            skill_name: "test-skill".to_string(),
            instructions: "Test instructions".to_string(),
            task_context: "Test task".to_string(),
            max_tokens: Some(4096),
            allowed_tools: vec!["bash".to_string(), "read_file".to_string()],
            effort: Some("high".to_string()),
            agent_type: Some("explore".to_string()),
            parent_recursion_depth: 0,
            state: SkillExecutionState::Running {
                turns_completed: 2,
                tokens_consumed: 1500,
                partial_output: "Partial result".to_string(),
                tool_history: vec![SkillToolCallRecord {
                    tool_name: "bash".to_string(),
                    args: serde_json::json!({"command": "ls"}),
                    output: "file1.txt\nfile2.txt".to_string(),
                    is_error: false,
                    executed_at: 1234567890,
                }],
            },
            checkpointed_at: current_timestamp(),
        };

        manager.save_checkpoint(checkpoint.clone()).unwrap();
        let loaded = manager.load_checkpoint("test-skill").unwrap().unwrap();

        assert_eq!(loaded.skill_name, checkpoint.skill_name);
        assert_eq!(loaded.instructions, checkpoint.instructions);
        assert_eq!(loaded.state, checkpoint.state);
    }

    #[test]
    fn has_resumable_checkpoint_returns_true_for_running() {
        let dir = tempdir().unwrap();
        let mut manager = SkillCheckpointManager::new(dir.path().to_path_buf());
        manager.init().unwrap();

        let checkpoint = SkillCheckpoint {
            skill_name: "resumable-skill".to_string(),
            instructions: String::new(),
            task_context: String::new(),
            max_tokens: None,
            allowed_tools: Vec::new(),
            effort: None,
            agent_type: None,
            parent_recursion_depth: 0,
            state: SkillExecutionState::Running {
                turns_completed: 1,
                tokens_consumed: 500,
                partial_output: String::new(),
                tool_history: Vec::new(),
            },
            checkpointed_at: current_timestamp(),
        };

        manager.save_checkpoint(checkpoint).unwrap();
        assert!(manager.has_resumable_checkpoint("resumable-skill"));
    }

    #[test]
    fn has_resumable_checkpoint_returns_false_for_completed() {
        let dir = tempdir().unwrap();
        let mut manager = SkillCheckpointManager::new(dir.path().to_path_buf());
        manager.init().unwrap();

        manager
            .mark_completed("completed-skill", "Final output".to_string(), 5, 2000)
            .unwrap();

        assert!(!manager.has_resumable_checkpoint("completed-skill"));
    }

    #[test]
    fn update_progress_saves_running_state() {
        let dir = tempdir().unwrap();
        let mut manager = SkillCheckpointManager::new(dir.path().to_path_buf());
        manager.init().unwrap();

        let checkpoint = SkillCheckpoint {
            skill_name: "progress-skill".to_string(),
            instructions: String::new(),
            task_context: String::new(),
            max_tokens: None,
            allowed_tools: Vec::new(),
            effort: None,
            agent_type: None,
            parent_recursion_depth: 0,
            state: SkillExecutionState::Running {
                turns_completed: 0,
                tokens_consumed: 0,
                partial_output: String::new(),
                tool_history: Vec::new(),
            },
            checkpointed_at: current_timestamp(),
        };

        manager.save_checkpoint(checkpoint).unwrap();
        manager
            .update_progress(
                "progress-skill",
                3,
                1800,
                "Turn 3 output",
                vec![SkillToolCallRecord {
                    tool_name: "bash".to_string(),
                    args: serde_json::json!({}),
                    output: "output".to_string(),
                    is_error: false,
                    executed_at: current_timestamp(),
                }],
            )
            .unwrap();

        let loaded = manager.load_checkpoint("progress-skill").unwrap().unwrap();
        match loaded.state {
            SkillExecutionState::Running {
                turns_completed,
                tokens_consumed,
                partial_output,
                tool_history,
            } => {
                assert_eq!(turns_completed, 3);
                assert_eq!(tokens_consumed, 1800);
                assert_eq!(partial_output, "Turn 3 output");
                assert_eq!(tool_history.len(), 1);
            }
            _ => panic!("Expected Running state"),
        }
    }

    #[test]
    fn mark_failed_saves_error_state() {
        let dir = tempdir().unwrap();
        let mut manager = SkillCheckpointManager::new(dir.path().to_path_buf());
        manager.init().unwrap();

        manager
            .mark_failed("failed-skill", "Skill crashed".to_string(), 2, 1200)
            .unwrap();

        let loaded = manager.load_checkpoint("failed-skill").unwrap().unwrap();
        match loaded.state {
            SkillExecutionState::Failed {
                error,
                turns_completed,
                tokens_consumed,
            } => {
                assert_eq!(error, "Skill crashed");
                assert_eq!(turns_completed, 2);
                assert_eq!(tokens_consumed, 1200);
            }
            _ => panic!("Expected Failed state"),
        }
    }

    #[test]
    fn delete_checkpoint_removes_file() {
        let dir = tempdir().unwrap();
        let mut manager = SkillCheckpointManager::new(dir.path().to_path_buf());
        manager.init().unwrap();

        manager
            .mark_completed("delete-skill", "Output".to_string(), 1, 500)
            .unwrap();
        manager.delete_checkpoint("delete-skill").unwrap();

        assert!(manager.load_checkpoint("delete-skill").unwrap().is_none());
    }

    #[test]
    fn list_active_checkpoints_filters_running() {
        let dir = tempdir().unwrap();
        let mut manager = SkillCheckpointManager::new(dir.path().to_path_buf());
        manager.init().unwrap();

        // Add a running skill
        let running = SkillCheckpoint {
            skill_name: "running".to_string(),
            instructions: String::new(),
            task_context: String::new(),
            max_tokens: None,
            allowed_tools: Vec::new(),
            effort: None,
            agent_type: None,
            parent_recursion_depth: 0,
            state: SkillExecutionState::Running {
                turns_completed: 1,
                tokens_consumed: 500,
                partial_output: String::new(),
                tool_history: Vec::new(),
            },
            checkpointed_at: current_timestamp(),
        };
        manager.save_checkpoint(running).unwrap();

        // Add a completed skill
        manager
            .mark_completed("completed", "Done".to_string(), 2, 1000)
            .unwrap();

        let active = manager.list_active_checkpoints();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].skill_name, "running");
    }
}
