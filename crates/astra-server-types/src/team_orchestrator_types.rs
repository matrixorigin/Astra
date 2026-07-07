//! Pure types and helper functions extracted from team_orchestrator.
//!
//! These types have no dependency on runtime internals and can be shared
//! across crates via `astra-server-types`.

use std::collections::HashMap;
use std::sync::Arc;

use astra_services::coordination::{AgentResult, DelegationResult, delegation_result_status_kind};

// ─── Types ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatusKind {
    Created,
    Running,
    Completed,
    Unfinished,
    Partial,
    CompletedWithConflicts,
    CompletedOverBudget,
    Failed,
    Cancelled,
    Interrupted,
    Timeout,
    Other,
}

pub fn run_status_kind(status: &str) -> RunStatusKind {
    match status {
        "created" => RunStatusKind::Created,
        "running" => RunStatusKind::Running,
        "completed" => RunStatusKind::Completed,
        "unfinished" => RunStatusKind::Unfinished,
        "partial" | "partial_failure" => RunStatusKind::Partial,
        "completed_with_conflicts" => RunStatusKind::CompletedWithConflicts,
        "completed_over_budget" => RunStatusKind::CompletedOverBudget,
        "failed" => RunStatusKind::Failed,
        "cancelled" => RunStatusKind::Cancelled,
        "interrupted" => RunStatusKind::Interrupted,
        "timeout" => RunStatusKind::Timeout,
        _ => RunStatusKind::Other,
    }
}

pub fn run_status_is_done(status: &str) -> bool {
    matches!(
        run_status_kind(status),
        RunStatusKind::Completed
            | RunStatusKind::Unfinished
            | RunStatusKind::Partial
            | RunStatusKind::CompletedWithConflicts
            | RunStatusKind::CompletedOverBudget
    )
}

pub fn run_status_is_failed(status: &str) -> bool {
    matches!(
        run_status_kind(status),
        RunStatusKind::Failed
            | RunStatusKind::Cancelled
            | RunStatusKind::Interrupted
            | RunStatusKind::Timeout
    )
}

pub fn run_status_is_active(status: &str) -> bool {
    matches!(
        run_status_kind(status),
        RunStatusKind::Created | RunStatusKind::Running
    )
}

pub fn run_status_is_completed(status: &str) -> bool {
    run_status_kind(status) == RunStatusKind::Completed
}

/// Progress phases emitted during team execution.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionPhase {
    /// Team loaded and validated, profiles resolved.
    Preparing {
        team_name: String,
        member_count: usize,
    },
    /// Worktrees created (only for Isolated mode).
    WorktreesCreated { agent_ids: Vec<String> },
    /// Delegation started via DelegationEngine.
    Executing { delegation_id: String },
    /// Real-time agent state update within a delegation.
    AgentProgress {
        delegation_id: String,
        /// agent_id → current state
        agent_states: HashMap<String, String>,
        completed_count: usize,
        total_count: usize,
    },
    /// Delegation completed, merging worktrees.
    Merging { agent_count: usize },
    /// Merge complete, producing final report.
    Reporting { status: TeamExecutionStatus },
}

/// Callback for reporting execution progress to the UI layer.
pub type ProgressCallback = Arc<dyn Fn(ExecutionPhase) + Send + Sync>;

/// Configuration for creating a TeamExecutionOrchestrator.
pub struct OrchestratorConfig {
    pub user_id: String,
    pub session_id: String,
    /// Source agent ID requesting the team execution (for delegation validation).
    pub source_agent_id: String,
    /// Optional progress callback for UI integration.
    pub progress: Option<ProgressCallback>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TeamExecutionStatus {
    Completed,
    Unfinished,
    Partial,
    CompletedWithConflicts,
    CompletedOverBudget,
    Failed,
}

impl std::fmt::Display for TeamExecutionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Completed => write!(f, "completed"),
            Self::Unfinished => write!(f, "unfinished"),
            Self::Partial => write!(f, "partial"),
            Self::CompletedWithConflicts => write!(f, "completed_with_conflicts"),
            Self::CompletedOverBudget => write!(f, "completed_over_budget"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

// ─── Helper Functions ───────────────────────────────────────────────────────

/// Sum token usage across all agent results.
pub fn sum_usage(result: &DelegationResult) -> (u64, u64, u32) {
    let mut prompt = 0u64;
    let mut completion = 0u64;
    let mut tools = 0u32;
    for r in &result.agent_results {
        prompt += r.prompt_tokens;
        completion += r.completion_tokens;
        tools += r.tool_calls;
    }
    (prompt, completion, tools)
}

/// Build a human-readable summary of unfinished and failed agents.
pub fn summarize_unsuccessful_agents(result: &DelegationResult) -> String {
    if result.agent_results.is_empty() {
        return "delegation produced no agent results".to_string();
    }

    let unsuccessful: Vec<&AgentResult> = result
        .agent_results
        .iter()
        .filter(|agent| !agent.is_success())
        .collect();
    if unsuccessful.is_empty() {
        return "delegation produced no unsuccessful agent results".to_string();
    }

    let unfinished = unsuccessful
        .iter()
        .filter(|agent| agent.is_unfinished())
        .count();
    let failed = unsuccessful.len().saturating_sub(unfinished);

    let details: Vec<String> = unsuccessful
        .iter()
        .take(3)
        .map(|agent| {
            let reason = agent
                .error
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(agent.status.as_str());
            format!("{}: {}", agent.agent_id, reason)
        })
        .collect();
    let remainder = unsuccessful.len().saturating_sub(details.len());
    let suffix = if remainder > 0 {
        format!(" (+{} more)", remainder)
    } else {
        String::new()
    };

    let summary = match (failed, unfinished) {
        (0, unfinished) => format!(
            "{unfinished} of {} agents unfinished",
            result.agent_results.len()
        ),
        (failed, 0) => format!("{failed} of {} agents failed", result.agent_results.len()),
        (failed, unfinished) => format!(
            "{} of {} agents did not finish cleanly ({unfinished} unfinished, {failed} failed)",
            unsuccessful.len(),
            result.agent_results.len()
        ),
    };

    format!("{summary} ({}){suffix}", details.join("; "))
}

/// Append merge conflict info to a summary string.
pub fn append_merge_conflict_summary(summary: String, conflict_count: usize) -> String {
    if conflict_count == 0 {
        return summary;
    }
    format!("{summary}; merge produced {conflict_count} conflict(s)")
}

/// Derive team execution status from delegation result and merge conflicts.
pub fn derive_team_status(
    result: &DelegationResult,
    conflict_count: usize,
) -> (TeamExecutionStatus, Option<String>) {
    match delegation_result_status_kind(&result.status) {
        astra_services::coordination::DelegationResultStatusKind::Completed => {
            let status = if conflict_count > 0 {
                TeamExecutionStatus::CompletedWithConflicts
            } else {
                TeamExecutionStatus::Completed
            };
            let error = if conflict_count > 0 {
                Some(format!("merge produced {conflict_count} conflict(s)"))
            } else {
                None
            };
            (status, error)
        }
        astra_services::coordination::DelegationResultStatusKind::Unfinished => (
            TeamExecutionStatus::Unfinished,
            Some(append_merge_conflict_summary(
                summarize_unsuccessful_agents(result),
                conflict_count,
            )),
        ),
        astra_services::coordination::DelegationResultStatusKind::Partial => (
            TeamExecutionStatus::Partial,
            Some(append_merge_conflict_summary(
                summarize_unsuccessful_agents(result),
                conflict_count,
            )),
        ),
        astra_services::coordination::DelegationResultStatusKind::Failed => (
            TeamExecutionStatus::Failed,
            Some(append_merge_conflict_summary(
                summarize_unsuccessful_agents(result),
                conflict_count,
            )),
        ),
        astra_services::coordination::DelegationResultStatusKind::Other => (
            TeamExecutionStatus::Failed,
            Some(append_merge_conflict_summary(
                format!("delegation ended in unexpected status '{}'", result.status),
                conflict_count,
            )),
        ),
    }
}

// ─── warn_persist! macro ────────────────────────────────────────────────────

/// Log a warning when a best-effort persistence operation fails.
#[macro_export]
macro_rules! warn_persist {
    ($op:expr, $label:expr) => {
        if let Err(e) = $op {
            astra_core::agent_warn!("orchestrator", "{}: {e}", $label);
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed_agent(agent_id: &str) -> AgentResult {
        AgentResult {
            agent_id: agent_id.to_string(),
            run_id: format!("run-{agent_id}"),
            status: "completed".to_string(),
            output: Some("ok".to_string()),
            error: None,
            prompt_tokens: 10,
            completion_tokens: 5,
            tool_calls: 1,
        }
    }

    fn failed_agent(agent_id: &str, status: &str) -> AgentResult {
        AgentResult {
            agent_id: agent_id.to_string(),
            run_id: format!("run-{agent_id}"),
            status: status.to_string(),
            output: None,
            error: Some("boom".to_string()),
            prompt_tokens: 10,
            completion_tokens: 0,
            tool_calls: 0,
        }
    }

    fn unfinished_agent(agent_id: &str, status: &str) -> AgentResult {
        AgentResult {
            agent_id: agent_id.to_string(),
            run_id: format!("run-{agent_id}"),
            status: status.to_string(),
            output: Some("waiting for input".to_string()),
            error: None,
            prompt_tokens: 10,
            completion_tokens: 0,
            tool_calls: 0,
        }
    }

    #[test]
    fn run_status_helpers_cover_shared_coordination_taxonomy() {
        assert_eq!(run_status_kind("created"), RunStatusKind::Created);
        assert_eq!(run_status_kind("running"), RunStatusKind::Running);
        assert_eq!(run_status_kind("completed"), RunStatusKind::Completed);
        assert_eq!(run_status_kind("unfinished"), RunStatusKind::Unfinished);
        assert_eq!(run_status_kind("partial"), RunStatusKind::Partial);
        assert_eq!(run_status_kind("partial_failure"), RunStatusKind::Partial);
        assert_eq!(
            run_status_kind("completed_with_conflicts"),
            RunStatusKind::CompletedWithConflicts
        );
        assert_eq!(
            run_status_kind("completed_over_budget"),
            RunStatusKind::CompletedOverBudget
        );
        assert_eq!(run_status_kind("failed"), RunStatusKind::Failed);
        assert_eq!(run_status_kind("cancelled"), RunStatusKind::Cancelled);
        assert_eq!(run_status_kind("interrupted"), RunStatusKind::Interrupted);
        assert_eq!(run_status_kind("timeout"), RunStatusKind::Timeout);
        assert_eq!(run_status_kind("weird"), RunStatusKind::Other);
    }

    #[test]
    fn run_status_helpers_keep_done_failed_and_active_groups_distinct() {
        assert!(run_status_is_active("created"));
        assert!(run_status_is_active("running"));
        assert!(run_status_is_done("completed"));
        assert!(run_status_is_done("unfinished"));
        assert!(run_status_is_done("partial"));
        assert!(run_status_is_done("partial_failure"));
        assert!(run_status_is_done("completed_with_conflicts"));
        assert!(run_status_is_done("completed_over_budget"));
        assert!(run_status_is_failed("failed"));
        assert!(run_status_is_failed("cancelled"));
        assert!(run_status_is_failed("interrupted"));
        assert!(run_status_is_failed("timeout"));
        assert!(run_status_is_completed("completed"));
        assert!(!run_status_is_completed("completed_over_budget"));
    }

    #[test]
    fn derive_team_status_marks_conflict_and_budget_variants_as_done() {
        let completed = DelegationResult::from_results(
            "del-1",
            vec![completed_agent("a"), completed_agent("b")],
            Some("done".to_string()),
        );
        let (status, error) = derive_team_status(&completed, 2);
        assert_eq!(status, TeamExecutionStatus::CompletedWithConflicts);
        assert_eq!(error.as_deref(), Some("merge produced 2 conflict(s)"));
        assert!(run_status_is_done(&status.to_string()));
        assert!(run_status_is_done(
            &TeamExecutionStatus::CompletedOverBudget.to_string()
        ));
    }

    #[test]
    fn derive_team_status_surfaces_partial_and_unexpected_statuses() {
        let unfinished = DelegationResult::from_results(
            "del-1.5",
            vec![completed_agent("a"), unfinished_agent("b", "waiting")],
            None,
        );
        let (unfinished_status, unfinished_error) = derive_team_status(&unfinished, 0);
        assert_eq!(unfinished_status, TeamExecutionStatus::Unfinished);
        assert!(
            unfinished_error
                .as_deref()
                .is_some_and(|value| value.contains("unfinished")),
            "unfinished error should summarize unfinished agents"
        );

        let partial = DelegationResult::from_results(
            "del-2",
            vec![completed_agent("a"), failed_agent("b", "timeout")],
            None,
        );
        let (partial_status, partial_error) = derive_team_status(&partial, 0);
        assert_eq!(partial_status, TeamExecutionStatus::Partial);
        assert!(
            partial_error
                .as_deref()
                .is_some_and(|value| value.contains("1 of 2 agents failed")),
            "partial error should summarize failed agents"
        );

        let unexpected = DelegationResult {
            delegation_id: "del-3".to_string(),
            status: "mystery".to_string(),
            agent_results: vec![],
            aggregated_output: None,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
        };
        let (unexpected_status, unexpected_error) = derive_team_status(&unexpected, 0);
        assert_eq!(unexpected_status, TeamExecutionStatus::Failed);
        assert!(
            unexpected_error
                .as_deref()
                .is_some_and(|value| value.contains("unexpected status")),
            "unexpected status should be surfaced explicitly"
        );
    }
}
