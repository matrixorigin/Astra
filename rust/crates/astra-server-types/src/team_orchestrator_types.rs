//! Pure types and helper functions extracted from team_orchestrator.
//!
//! These types have no dependency on runtime internals and can be shared
//! across crates via `astra-server-types`.

use std::collections::HashMap;
use std::sync::Arc;

use astra_services::coordination::{AgentResult, DelegationResult};
use astra_services::learning_merge::{AgentLearning, MergedLearning, VersionVector, merge_agent_learnings};

// ─── Types ──────────────────────────────────────────────────────────────────

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
    Partial,
    CompletedWithConflicts,
    CompletedOverBudget,
    Failed,
}

impl std::fmt::Display for TeamExecutionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Completed => write!(f, "completed"),
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

/// Build a human-readable summary of failed agents.
pub fn summarize_failed_agents(result: &DelegationResult) -> String {
    if result.agent_results.is_empty() {
        return "delegation produced no agent results".to_string();
    }

    let failed: Vec<&AgentResult> = result
        .agent_results
        .iter()
        .filter(|agent| !agent.is_success())
        .collect();
    if failed.is_empty() {
        return "delegation did not produce a successful result".to_string();
    }

    let details: Vec<String> = failed
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
    let remainder = failed.len().saturating_sub(details.len());
    let suffix = if remainder > 0 {
        format!(" (+{} more)", remainder)
    } else {
        String::new()
    };

    format!(
        "{} of {} agents failed ({}){suffix}",
        failed.len(),
        result.agent_results.len(),
        details.join("; "),
    )
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
    match result.status.as_str() {
        "completed" => {
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
        "partial" => (
            TeamExecutionStatus::Partial,
            Some(append_merge_conflict_summary(
                summarize_failed_agents(result),
                conflict_count,
            )),
        ),
        "failed" => (
            TeamExecutionStatus::Failed,
            Some(append_merge_conflict_summary(
                summarize_failed_agents(result),
                conflict_count,
            )),
        ),
        other => (
            TeamExecutionStatus::Failed,
            Some(append_merge_conflict_summary(
                format!("delegation ended in unexpected status \'{other}\'"),
                conflict_count,
            )),
        ),
    }
}

/// Extract a synthetic AgentLearning from an agent result.
pub fn extract_learning_from_result(result: &AgentResult) -> AgentLearning {
    let mut version = VersionVector::new();
    version.increment(&result.agent_id);

    AgentLearning {
        agent_id: result.agent_id.clone(),
        session_id: result.run_id.clone(),
        version,
        successful_patterns: vec![],
        failed_patterns: vec![],
        discovered_facts: vec![],
        quality_score: if result.is_success() { 0.8 } else { 0.2 },
    }
}

/// Aggregate learnings from all successful agents in a delegation result.
pub fn merge_team_learnings(agent_results: &[AgentResult]) -> Option<MergedLearning> {
    let learnings: Vec<AgentLearning> = agent_results
        .iter()
        .filter(|r| r.is_success())
        .map(|r| extract_learning_from_result(r))
        .collect();
    if learnings.is_empty() {
        None
    } else {
        Some(merge_agent_learnings(&learnings))
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
