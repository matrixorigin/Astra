//! Pure types extracted from runtime orchestration for cross-crate use.
//!
//! These types have zero runtime dependencies and can be used by turn-core
//! modules that need to reference agent status and metrics.

use std::time::SystemTime;

use super::fanout_group::AgentFanoutSlotIdentity;

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
    Cancelled {
        /// Whether the cancellation was triggered by the user (Ctrl+C
        /// / Ctrl+G x) as opposed to a system condition (parent
        /// cancel, depth violation, etc.). When `true`, the wire
        /// payload tells the LLM explicitly NOT to respawn the work —
        /// otherwise the LLM observes "cancelled" and treats it like a
        /// transient failure, immediately spawning a replacement and
        /// negating the user's intent.
        by_user: bool,
        /// Free-form rationale forwarded to the LLM; empty string =
        /// "cancelled" (legacy default).
        reason: String,
    },
}

impl AgentStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }

    /// Construct a Cancelled status without a reason — preserves the
    /// pre-rich-cancel call sites where no rationale was ever passed.
    pub fn cancelled_anonymous() -> Self {
        Self::Cancelled {
            by_user: false,
            reason: String::new(),
        }
    }

    /// Construct a Cancelled status flagging the cancel as user-driven.
    pub fn cancelled_by_user(reason: impl Into<String>) -> Self {
        Self::Cancelled {
            by_user: true,
            reason: reason.into(),
        }
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
    pub run_in_background: bool,
    pub fanout_slot: Option<AgentFanoutSlotIdentity>,
}

/// Random `edge_executor_id` values for §5.5 when `ASTRA_EDGE_EXECUTOR_ID` is unset.
#[must_use]
pub fn random_edge_executor_instance_id() -> String {
    format!("edge-{}", uuid::Uuid::new_v4())
}

#[cfg(test)]
mod edge_executor_id_tests {
    use super::*;

    #[test]
    fn has_edge_prefix() {
        let id = random_edge_executor_instance_id();
        assert!(id.starts_with("edge-"));
    }

    #[test]
    fn unique_each_call() {
        let a = random_edge_executor_instance_id();
        let b = random_edge_executor_instance_id();
        assert_ne!(a, b);
    }

    #[test]
    fn uuid_portion_valid() {
        let id = random_edge_executor_instance_id();
        let uuid_part = &id["edge-".len()..];
        assert!(uuid::Uuid::parse_str(uuid_part).is_ok());
    }
}
