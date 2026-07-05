//! Pure types extracted from runtime orchestration for cross-crate use.
//!
//! These types have zero runtime dependencies and can be used by turn-core
//! modules that need to reference agent status and metrics.

use std::time::SystemTime;

use super::fanout_group::{AgentFanoutSlotIdentity, AgentFanoutSlotStatus};

pub const AGENT_FINISH_REASON_NORMAL: &str = "normal";

/// Current status of a spawned agent.
///
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
    Interrupted {
        partial_result: String,
        finish_reason: String,
    },
    Failed {
        error: String,
        #[allow(dead_code)]
        finish_reason: Option<String>,
    },
    Waiting {
        reason: String,
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
            Self::Completed { .. }
                | Self::Interrupted { .. }
                | Self::Failed { .. }
                | Self::Cancelled { .. }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFanoutStatusProjection {
    pub status: AgentFanoutSlotStatus,
    pub terminal_reason: Option<String>,
}

pub fn agent_finish_reason_text(finish_reason: Option<&str>) -> &str {
    finish_reason
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .unwrap_or(AGENT_FINISH_REASON_NORMAL)
}

pub fn agent_finish_reason_is_normal(finish_reason: Option<&str>) -> bool {
    agent_finish_reason_text(finish_reason) == AGENT_FINISH_REASON_NORMAL
}

pub fn agent_completion_is_interrupted(finish_reason: Option<&str>) -> bool {
    !agent_finish_reason_is_normal(finish_reason)
}

pub fn agent_finish_reason_is_parent_budget(reason: &str) -> bool {
    matches!(
        reason.trim(),
        "budget_exhausted"
            | "turn_budget_exhausted"
            | "token_budget_exceeded"
            | "context_overflow"
            | "max_turns_exceeded"
            | "max_turns"
    )
}

pub fn project_agent_status_to_fanout_slot(status: &AgentStatus) -> AgentFanoutStatusProjection {
    let (status, terminal_reason) = match status {
        AgentStatus::Completed {
            result: _,
            finish_reason,
        } => {
            let reason = agent_finish_reason_text(finish_reason.as_deref());
            if reason == AGENT_FINISH_REASON_NORMAL {
                (AgentFanoutSlotStatus::Completed, None)
            } else {
                (AgentFanoutSlotStatus::Failed, Some(reason.to_string()))
            }
        }
        AgentStatus::Interrupted { finish_reason, .. } => {
            let reason = finish_reason.trim();
            if agent_finish_reason_is_parent_budget(reason) {
                (
                    AgentFanoutSlotStatus::CancelledByParentBudget,
                    Some(reason.to_string()),
                )
            } else {
                (AgentFanoutSlotStatus::Interrupted, Some(reason.to_string()))
            }
        }
        AgentStatus::Failed {
            error,
            finish_reason,
        } => (
            AgentFanoutSlotStatus::Failed,
            finish_reason.clone().or_else(|| Some(error.clone())),
        ),
        AgentStatus::Cancelled { by_user, reason } => {
            let reason = if reason.is_empty() {
                None
            } else {
                Some(reason.clone())
            };
            if *by_user {
                (AgentFanoutSlotStatus::CancelledByUser, reason)
            } else {
                (AgentFanoutSlotStatus::CancelledByParentBudget, reason)
            }
        }
        AgentStatus::Waiting { .. } => (AgentFanoutSlotStatus::Running, None),
        AgentStatus::Initializing | AgentStatus::Running { .. } | AgentStatus::Idle => {
            (AgentFanoutSlotStatus::Running, None)
        }
    };

    AgentFanoutStatusProjection {
        status,
        terminal_reason,
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
mod tests {
    use super::*;

    #[test]
    fn finish_reason_classification_is_shared() {
        assert!(!agent_completion_is_interrupted(None));
        assert!(!agent_completion_is_interrupted(Some("normal")));
        assert!(!agent_completion_is_interrupted(Some(" normal ")));
        assert!(!agent_completion_is_interrupted(Some("")));
        assert!(agent_completion_is_interrupted(Some("budget_exhausted")));
        assert!(agent_completion_is_interrupted(Some("empty_completion")));

        assert!(agent_finish_reason_is_parent_budget("budget_exhausted"));
        assert!(agent_finish_reason_is_parent_budget(" context_overflow "));
        assert!(!agent_finish_reason_is_parent_budget("empty_completion"));
    }

    #[test]
    fn fanout_projection_separates_budget_interruptions_from_child_interruptions() {
        let budget_interrupted = AgentStatus::Interrupted {
            partial_result: "partial review".to_string(),
            finish_reason: "budget_exhausted".to_string(),
        };
        let projection = project_agent_status_to_fanout_slot(&budget_interrupted);
        assert_eq!(
            projection.status,
            AgentFanoutSlotStatus::CancelledByParentBudget
        );
        assert_eq!(
            projection.terminal_reason.as_deref(),
            Some("budget_exhausted")
        );

        let empty_completion = AgentStatus::Interrupted {
            partial_result: String::new(),
            finish_reason: "empty_completion".to_string(),
        };
        let projection = project_agent_status_to_fanout_slot(&empty_completion);
        assert_eq!(projection.status, AgentFanoutSlotStatus::Interrupted);
        assert_eq!(
            projection.terminal_reason.as_deref(),
            Some("empty_completion")
        );
    }

    #[test]
    fn fanout_projection_treats_only_normal_completion_as_completed() {
        let completed = AgentStatus::Completed {
            result: "done".to_string(),
            finish_reason: Some("normal".to_string()),
        };
        let projection = project_agent_status_to_fanout_slot(&completed);
        assert_eq!(projection.status, AgentFanoutSlotStatus::Completed);
        assert!(projection.terminal_reason.is_none());

        let non_normal = AgentStatus::Completed {
            result: "partial".to_string(),
            finish_reason: Some("empty_completion".to_string()),
        };
        let projection = project_agent_status_to_fanout_slot(&non_normal);
        assert_eq!(projection.status, AgentFanoutSlotStatus::Failed);
        assert_eq!(
            projection.terminal_reason.as_deref(),
            Some("empty_completion")
        );
    }

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
