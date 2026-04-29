//! Structured plan metrics for observability.
//!
//! Tracks timing, token usage, cost, and per-subtask execution details.

use serde::{Deserialize, Serialize};

/// Aggregated metrics for a plan's lifecycle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanMetrics {
    /// When the plan was created (Unix timestamp seconds).
    #[serde(default)]
    pub created_at: u64,
    /// Duration of the planning/decomposition phase (ms).
    #[serde(default)]
    pub planning_duration_ms: u64,
    /// Number of edits made to the plan.
    #[serde(default)]
    pub edit_count: u32,
    /// Number of plan versions created.
    #[serde(default)]
    pub version_count: u32,
    /// When execution started (Unix timestamp seconds).
    #[serde(default)]
    pub execution_started_at: Option<u64>,
    /// Total execution duration (ms).
    #[serde(default)]
    pub execution_duration_ms: Option<u64>,
    /// Per-subtask metrics.
    #[serde(default)]
    pub subtask_metrics: Vec<SubtaskMetrics>,
    /// Total prompt tokens consumed across all plan operations.
    #[serde(default)]
    pub total_prompt_tokens: u64,
    /// Total completion tokens consumed.
    #[serde(default)]
    pub total_completion_tokens: u64,
    /// Total estimated cost (USD).
    #[serde(default)]
    pub total_cost: f64,
    /// Number of approval gates hit.
    #[serde(default)]
    pub approval_count: u32,
    /// Total number of retries across all subtasks.
    #[serde(default)]
    pub retry_count: u32,
}

impl PlanMetrics {
    /// Record token usage from a turn.
    pub fn record_tokens(&mut self, prompt: u64, completion: u64) {
        self.total_prompt_tokens += prompt;
        self.total_completion_tokens += completion;
    }

    /// Record a retry attempt.
    pub fn record_retry(&mut self) {
        self.retry_count += 1;
    }

    /// Record an approval gate.
    pub fn record_approval(&mut self) {
        self.approval_count += 1;
    }

    /// Add or update metrics for a specific subtask.
    pub fn record_subtask_completion(
        &mut self,
        subtask_id: &str,
        status: &str,
        duration_ms: u64,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) {
        if let Some(existing) = self
            .subtask_metrics
            .iter_mut()
            .find(|m| m.subtask_id == subtask_id)
        {
            existing.status = status.to_string();
            existing.duration_ms = duration_ms;
            existing.prompt_tokens += prompt_tokens;
            existing.completion_tokens += completion_tokens;
            existing.tool_calls += tool_calls;
        } else {
            self.subtask_metrics.push(SubtaskMetrics {
                subtask_id: subtask_id.to_string(),
                status: status.to_string(),
                duration_ms,
                prompt_tokens,
                completion_tokens,
                tool_calls,
                retries: 0,
            });
        }
    }

    /// Record a subtask retry. Creates a placeholder entry if the subtask
    /// hasn't been recorded yet (retries typically happen before completion).
    pub fn record_subtask_retry(&mut self, subtask_id: &str) {
        self.record_retry();
        if let Some(existing) = self
            .subtask_metrics
            .iter_mut()
            .find(|m| m.subtask_id == subtask_id)
        {
            existing.retries += 1;
        } else {
            self.subtask_metrics.push(SubtaskMetrics {
                subtask_id: subtask_id.to_string(),
                status: "retrying".to_string(),
                retries: 1,
                ..Default::default()
            });
        }
    }

    /// Format metrics for CLI display.
    pub fn format_display(&self) -> String {
        let mut out = String::new();
        out.push_str("┌── Plan Metrics ──────────────────────────────────\n");

        if self.planning_duration_ms > 0 {
            out.push_str(&format!("│ Planning:    {}ms\n", self.planning_duration_ms));
        }
        if let Some(exec_ms) = self.execution_duration_ms {
            out.push_str(&format!("│ Execution:   {}ms\n", exec_ms));
        }

        out.push_str(&format!("│ Edits:       {}\n", self.edit_count));
        out.push_str(&format!("│ Versions:    {}\n", self.version_count));

        let total_tokens = self.total_prompt_tokens + self.total_completion_tokens;
        if total_tokens > 0 {
            out.push_str("│\n");
            out.push_str(&format!(
                "│ Tokens:      {} ({} prompt + {} completion)\n",
                total_tokens, self.total_prompt_tokens, self.total_completion_tokens
            ));
        }
        if self.total_cost > 0.0 {
            out.push_str(&format!("│ Cost:        ${:.4}\n", self.total_cost));
        }

        if self.retry_count > 0 {
            out.push_str(&format!("│ Retries:     {}\n", self.retry_count));
        }
        if self.approval_count > 0 {
            out.push_str(&format!("│ Approvals:   {}\n", self.approval_count));
        }

        if !self.subtask_metrics.is_empty() {
            out.push_str("│\n");
            out.push_str("│ Per-subtask:\n");
            for sm in &self.subtask_metrics {
                let status_icon = match sm.status.as_str() {
                    "completed" => "✓",
                    "failed" => "✗",
                    "in_progress" => "▶",
                    _ => "·",
                };
                out.push_str(&format!(
                    "│   {} {} — {}ms, {} tokens, {} tools",
                    status_icon,
                    sm.subtask_id,
                    sm.duration_ms,
                    sm.prompt_tokens + sm.completion_tokens,
                    sm.tool_calls,
                ));
                if sm.retries > 0 {
                    out.push_str(&format!(" ({} retries)", sm.retries));
                }
                out.push('\n');
            }
        }

        out.push_str("└──────────────────────────────────────────────────\n");
        out
    }
}

/// Metrics for a single subtask's execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubtaskMetrics {
    /// The subtask ID.
    pub subtask_id: String,
    /// Final status ("completed", "failed", "pending", etc.).
    pub status: String,
    /// Total execution time (ms).
    pub duration_ms: u64,
    /// Prompt tokens consumed.
    pub prompt_tokens: u64,
    /// Completion tokens consumed.
    pub completion_tokens: u64,
    /// Number of tool calls made.
    pub tool_calls: u32,
    /// Number of retries.
    pub retries: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_track_tokens() {
        let mut m = PlanMetrics::default();
        m.record_tokens(100, 50);
        m.record_tokens(200, 75);
        assert_eq!(m.total_prompt_tokens, 300);
        assert_eq!(m.total_completion_tokens, 125);
    }

    #[test]
    fn metrics_track_subtask() {
        let mut m = PlanMetrics::default();
        m.record_subtask_completion("setup", "completed", 500, 100, 50, 3);
        m.record_subtask_retry("setup");
        assert_eq!(m.subtask_metrics.len(), 1);
        assert_eq!(m.subtask_metrics[0].retries, 1);
        assert_eq!(m.retry_count, 1);
    }

    #[test]
    fn record_subtask_retry_creates_placeholder_for_unknown_id() {
        let mut m = PlanMetrics::default();
        assert!(m.subtask_metrics.is_empty());

        m.record_subtask_retry("unknown-subtask-42");

        assert_eq!(m.subtask_metrics.len(), 1);
        let entry = &m.subtask_metrics[0];
        assert_eq!(entry.subtask_id, "unknown-subtask-42");
        assert_eq!(entry.status, "retrying");
        assert_eq!(entry.retries, 1);
        assert_eq!(entry.duration_ms, 0);
        assert_eq!(m.retry_count, 1);
    }

    #[test]
    fn record_subtask_retry_increments_existing() {
        let mut m = PlanMetrics::default();
        m.record_subtask_completion("task-1", "completed", 500, 100, 50, 3);

        m.record_subtask_retry("task-1");
        m.record_subtask_retry("task-1");

        assert_eq!(m.subtask_metrics.len(), 1);
        assert_eq!(m.subtask_metrics[0].retries, 2);
        assert_eq!(m.retry_count, 2);
    }

    #[test]
    fn metrics_format_display_not_empty() {
        let mut m = PlanMetrics::default();
        m.record_tokens(100, 50);
        let display = m.format_display();
        assert!(display.contains("Tokens"));
        assert!(display.contains("150"));
    }
}
