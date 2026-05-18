//! Pure types extracted from runtime orchestration for cross-crate use.
//!
//! These types have zero runtime dependencies and can be used by turn-core
//! modules that need to reference agent status and metrics.

use std::time::SystemTime;

/// Current status of a spawned agent.
///
/// `Completed` is reused for every terminal state where progress
/// was preserved, including budget-exhaustion early-exit paths that
/// the loop reports as resumable interruptions. Callers that need to
/// distinguish task completion from interruption should read
/// `finish_reason`.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentStatus {
    Initializing,
    Running {
        activity: String,
    },
    Idle,
    Completed {
        result: String,
        #[allow(dead_code)]
        finish_reason: Option<String>,
    },
    Failed {
        error: String,
        #[allow(dead_code)]
        finish_reason: Option<String>,
    },
    Cancelled,
}

impl AgentStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled
        )
    }
}

/// Metrics tracked for a spawned agent.
#[derive(Debug, Clone, Default)]
pub struct SpawnedAgentMetrics {
    pub turns_completed: u32,
    pub tool_calls: u32,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub permission_requests: u32,
    pub permission_requests_approved: u32,
    pub tools_blocked: u32,
}

/// Summary info for a spawned agent (no runtime dependencies).
#[derive(Debug, Clone)]
pub struct SpawnedAgentInfo {
    pub agent_id: String,
    pub run_id: String,
    pub parent_run_id: String,
    pub agent_type: String,
    pub description: String,
    pub status: AgentStatus,
    pub started_at: SystemTime,
    pub metrics: SpawnedAgentMetrics,
    pub has_permission_issues: bool,
}
