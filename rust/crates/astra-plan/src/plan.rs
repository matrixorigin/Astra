//! Shared plan-mode exports plus execution capability policy.

pub use super::decompose::*;

use serde::{Deserialize, Serialize};

/// Capability-based permission model for plan phases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanCapabilities {
    /// Whether the agent can read files (context gathering).
    pub can_read_files: bool,
    /// Whether tools can be executed (shell, write, etc.).
    pub can_execute_tools: bool,
    /// Whether files can be modified.
    pub can_modify_files: bool,
    /// Whether network access is allowed.
    pub can_access_network: bool,
    /// Maximum number of subtasks allowed.
    pub max_subtasks: usize,
    /// Maximum number of execution rounds.
    pub max_execution_rounds: usize,
    /// Approval policy for tool execution.
    pub requires_approval: ApprovalPolicy,
}

impl Default for PlanCapabilities {
    fn default() -> Self {
        Self {
            can_read_files: true,
            can_execute_tools: true,
            can_modify_files: true,
            can_access_network: true,
            max_subtasks: 20,
            max_execution_rounds: 50,
            requires_approval: ApprovalPolicy::Destructive,
        }
    }
}

impl PlanCapabilities {
    /// Capabilities for plan-only chat and refining phases (no tools).
    pub fn planning() -> Self {
        Self {
            can_read_files: true,
            can_execute_tools: false,
            can_modify_files: false,
            can_access_network: false,
            max_subtasks: 20,
            max_execution_rounds: 0,
            requires_approval: ApprovalPolicy::All,
        }
    }

    /// Capabilities for auto-execution (full tools, approve destructive only).
    pub fn auto_execute() -> Self {
        Self::default()
    }

    /// Capabilities for step-by-step execution (full tools, approve each subtask).
    pub fn step_by_step() -> Self {
        Self {
            requires_approval: ApprovalPolicy::PerSubtask,
            ..Self::default()
        }
    }
}

/// Approval policy for tool execution during plan execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// No approval required — auto-execute everything.
    None,
    /// Approve each subtask before starting it.
    PerSubtask,
    /// Only approve destructive operations (file writes, shell commands).
    #[default]
    Destructive,
    /// Approve every single tool call.
    All,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planning_capabilities_disable_execution() {
        let caps = PlanCapabilities::planning();
        assert!(caps.can_read_files);
        assert!(!caps.can_execute_tools);
        assert!(!caps.can_modify_files);
        assert!(!caps.can_access_network);
        assert_eq!(caps.max_execution_rounds, 0);
        assert_eq!(caps.requires_approval, ApprovalPolicy::All);
    }

    #[test]
    fn step_by_step_capabilities_require_per_subtask_approval() {
        let caps = PlanCapabilities::step_by_step();
        assert!(caps.can_execute_tools);
        assert_eq!(caps.requires_approval, ApprovalPolicy::PerSubtask);
    }
}
