//! Message-Level Compaction (Overflow Recovery)
//!
//! When a context window overflow (413 / prompt-too-long) is detected during
//! the agentic loop, this module runs the message compression pipeline and
//! allows the turn to retry with a compacted message list.
//!
//! This is complementary to, not replaced by, the Context Pipeline's
//! `CompactionTier` system which handles *prompt-level* compaction (schema
//! pruning, section reduction). This module handles *message-level* compaction
//! (removing old turns, summarizing tool results, truncating large outputs).
//!
//! The pipeline's `RecoveryState` is informed of PTL errors and compaction
//! outcomes via `record_ptl_error()` / `record_reactive_compact()` in
//! `agentic_loop_execution_phase.rs`, ensuring the planner can escalate its
//! own tier on the next turn.
//!
//! Design: tiered escalation (default → aggressive → emergency) gated by
//! `consecutive_context_window_errors`. After MAX_COMPACT_RETRIES the error
//! propagates as a structured `InterruptionRecord` with
//! `ResumeAction::CompactAndRetry` (for cross-session resume).

use super::context_compression::{CompressionPipeline, PipelineOutcome, TokenBudget};
use serde_json::Value;

/// Maximum number of automatic compact-and-retry cycles per turn.
///
/// After this many consecutive context-window errors *within the same turn*,
/// we stop retrying and let the error propagate with a structured
/// `InterruptionRecord`.
pub(crate) const MAX_COMPACT_RETRIES: u32 = 3;

/// Fallback context limit when model_context_limit is 0: 128K * 90%.
const DEFAULT_CONTEXT_LIMIT: u64 = 115_200;

/// Outcome of a compaction-replay attempt.
#[derive(Debug)]
#[allow(dead_code)] // Fields consumed by callers and future telemetry.
pub(crate) struct CompactionReplayResult {
    /// Estimated tokens freed by the compression pipeline.
    pub tokens_freed: u64,
    /// Number of messages removed.
    pub messages_removed: usize,
    /// Per-layer descriptions for telemetry.
    pub layer_descriptions: Vec<String>,
    /// Whether the pipeline believes the budget is now satisfied.
    pub budget_likely_satisfied: bool,
    /// Full pipeline outcome (for trace/journal).
    pub pipeline_outcome: PipelineOutcome,
    /// Which compaction tier was used.
    pub tier: CompactionTier,
}

/// Compaction tier label for telemetry and escalation tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionTier {
    Default,
    Aggressive,
    Emergency,
}

impl CompactionTier {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Aggressive => "aggressive",
            Self::Emergency => "emergency",
        }
    }
}

/// Tracks compaction effectiveness across retries within a turn.
///
/// When compaction frees tokens but the next LLM call still fails with a
/// context-window error, this indicates "insufficient compaction" — the freed
/// amount wasn't enough. Tracking this enables escalation to more aggressive
/// tiers and telemetry for diagnosing chronic context pressure.
#[derive(Debug, Default)]
pub struct CompactionEffectivenessTracker {
    /// Tokens freed by the last compaction attempt.
    pub last_tokens_freed: u64,
    /// Whether the last compaction was followed by another context-window error.
    pub last_was_insufficient: bool,
    /// Cumulative tokens freed across all compaction attempts in this turn.
    pub cumulative_tokens_freed: u64,
    /// Number of compaction attempts in this turn.
    pub attempt_count: u32,
}

impl CompactionEffectivenessTracker {
    /// Record a compaction result.
    pub fn record_compaction(&mut self, tokens_freed: u64) {
        self.last_tokens_freed = tokens_freed;
        self.last_was_insufficient = false;
        self.cumulative_tokens_freed += tokens_freed;
        self.attempt_count += 1;
    }

    /// Mark that the last compaction was insufficient (still got a 413).
    pub fn mark_insufficient(&mut self) {
        self.last_was_insufficient = true;
    }

    /// Build a summary for telemetry.
    #[allow(dead_code)] // Used by future telemetry emission.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "cumulative_tokens_freed": self.cumulative_tokens_freed,
            "attempt_count": self.attempt_count,
            "last_tokens_freed": self.last_tokens_freed,
            "last_was_insufficient": self.last_was_insufficient,
        })
    }
}

/// Test-only helper: calls `try_compact_for_retry_tiered` with retry_count=1.
#[cfg(test)]
pub(crate) fn try_compact_for_retry(
    messages: &mut Vec<Value>,
    last_measured_tokens: Option<u64>,
    model_context_limit: u64,
) -> Option<CompactionReplayResult> {
    try_compact_for_retry_tiered(messages, last_measured_tokens, model_context_limit, 1)
}

/// Run the compression pipeline on the message list after a context-window error.
///
/// Returns `None` if there are too few messages to compact or if no tokens were
/// freed (compaction is futile). Returns `Some(result)` with details on success.
///
/// The `retry_count` parameter enables tiered escalation:
/// - retry 1: default pipeline (balanced thresholds)
/// - retry 2: aggressive pipeline (lower thresholds, fewer preserved turns)
/// - retry 3+: emergency pipeline (absolute last resort)
pub(crate) fn try_compact_for_retry_tiered(
    messages: &mut Vec<Value>,
    last_measured_tokens: Option<u64>,
    model_context_limit: u64,
    retry_count: u32,
) -> Option<CompactionReplayResult> {
    if messages.len() <= 4 {
        return None; // Too few messages to compact meaningfully
    }

    // Build a budget that reflects the overflow.
    //
    // Fallback estimation when `last_measured_tokens` is unknown: we must
    // account for *all* token sources the LLM will count, not just plain
    // string `content`. In particular:
    //   * tool_call arguments (often very large),
    //   * tool_result messages (array or string content),
    //   * multimodal / array-form content.
    //
    // Using `prompts::estimate_tokens` (which walks content + tool_calls and
    // adds per-message + fixed overhead) keeps this in sync with the
    // pre-request pressure estimate in `agentic_loop_lifecycle` so a
    // CompactAndRetry triggered precisely by large tool_calls is not
    // silently skipped.
    let measured =
        last_measured_tokens.unwrap_or_else(|| crate::prompts::estimate_tokens(messages) as u64);

    let max_tokens = if model_context_limit > 0 {
        model_context_limit
    } else {
        DEFAULT_CONTEXT_LIMIT
    };

    let budget = TokenBudget {
        max_prompt_tokens: max_tokens,
        last_measured_tokens: measured,
        current_round_index: None,
    };

    if !budget.is_over_budget() && budget.pressure() < 0.85 {
        return None;
    }

    // Tiered escalation: default → aggressive → emergency.
    let (pipeline, tier) = if retry_count <= 1 {
        (
            CompressionPipeline::default_pipeline(),
            CompactionTier::Default,
        )
    } else if retry_count == 2 {
        (
            CompressionPipeline::aggressive_pipeline(),
            CompactionTier::Aggressive,
        )
    } else {
        (
            CompressionPipeline::emergency_pipeline(),
            CompactionTier::Emergency,
        )
    };
    let outcome = pipeline.compress_if_needed(messages, &budget);

    if outcome.total_tokens_freed == 0 {
        return None; // Pipeline did nothing
    }

    let messages_removed: usize = outcome
        .layer_results
        .iter()
        .map(|(_, r)| r.messages_removed)
        .sum();
    let layer_descriptions: Vec<String> = outcome
        .layer_results
        .iter()
        .map(|(name, r)| format!("{}: ~{} tokens", name, r.estimated_tokens_freed))
        .collect();

    Some(CompactionReplayResult {
        tokens_freed: outcome.total_tokens_freed,
        messages_removed,
        layer_descriptions,
        budget_likely_satisfied: outcome.budget_satisfied,
        pipeline_outcome: outcome,
        tier,
    })
}

/// Build a concise summary string for the compaction event (for logs/journal).
pub(crate) fn compaction_summary(result: &CompactionReplayResult) -> String {
    let layers = result.layer_descriptions.join(", ");
    format!(
        "compacted ~{} tokens ({} messages removed) via [{}]; budget {}",
        result.tokens_freed,
        result.messages_removed,
        layers,
        if result.budget_likely_satisfied {
            "likely satisfied"
        } else {
            "still pressured"
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_messages(n: usize) -> Vec<Value> {
        let mut msgs = vec![json!({"role": "system", "content": "You are helpful."})];
        for i in 0..n {
            let long_content = "x".repeat(2000); // ~500 tokens each
            msgs.push(json!({"role": "user", "content": format!("Question {i}")}));
            msgs.push(json!({
                "role": "assistant",
                "content": format!("Answer {i}: {long_content}"),
                "tool_calls": []
            }));
        }
        msgs
    }

    #[test]
    fn compaction_returns_none_for_few_messages() {
        let mut msgs = vec![
            json!({"role": "system", "content": "Hi"}),
            json!({"role": "user", "content": "Hello"}),
        ];
        let result = try_compact_for_retry(&mut msgs, Some(1000), 128_000);
        assert!(result.is_none());
    }

    #[test]
    fn compaction_returns_none_when_not_over_budget() {
        let mut msgs = make_messages(3);
        // Well under budget
        let result = try_compact_for_retry(&mut msgs, Some(10_000), 200_000);
        assert!(result.is_none());
    }

    #[test]
    fn compaction_frees_tokens_when_over_budget() {
        let mut msgs = make_messages(20); // ~40+ messages, lots of content
        let original_len = msgs.len();
        // Way over budget — measured tokens exceed limit
        let result = try_compact_for_retry(&mut msgs, Some(200_000), 100_000);
        assert!(result.is_some(), "expected compaction to run");
        let r = result.unwrap();
        assert!(r.tokens_freed > 0);
        assert!(
            msgs.len() < original_len || r.messages_removed > 0,
            "expected fewer messages after compaction"
        );
    }

    #[test]
    fn compaction_replay_sanitizes_empty_assistant_tool_calls() {
        let mut msgs = make_messages(8);
        let result = try_compact_for_retry(&mut msgs, Some(200_000), 100_000);
        assert!(result.is_some(), "expected compaction to run");
        let first_assistant = msgs
            .iter()
            .find(|msg| msg.get("role").and_then(Value::as_str) == Some("assistant"))
            .expect("assistant message");
        assert!(
            first_assistant.get("tool_calls").is_none(),
            "{first_assistant:?}"
        );
    }

    #[test]
    fn compaction_frees_tokens_without_measured() {
        let mut msgs = make_messages(20);
        // No measured tokens — should estimate from content
        let result = try_compact_for_retry(&mut msgs, None, 10_000);
        // With 20 turns of 2000-char answers, content is ~40K chars = ~10K tokens,
        // which exceeds the 10K limit
        assert!(
            result.is_some(),
            "expected compaction to run with estimated tokens"
        );
    }

    #[test]
    fn compaction_summary_format() {
        let result = CompactionReplayResult {
            tokens_freed: 5000,
            messages_removed: 12,
            layer_descriptions: vec!["ToolResultTruncation: ~2000 tokens".into()],
            budget_likely_satisfied: true,
            tier: CompactionTier::Default,
            pipeline_outcome: PipelineOutcome {
                layer_results: Vec::new(),
                total_tokens_freed: 5000,
                budget_satisfied: true,
            },
        };
        let s = compaction_summary(&result);
        assert!(s.contains("5000"));
        assert!(s.contains("12 messages"));
        assert!(s.contains("likely satisfied"));
    }

    #[test]
    fn max_compact_retries_is_reasonable() {
        const { assert!(MAX_COMPACT_RETRIES >= 1) };
        const { assert!(MAX_COMPACT_RETRIES <= 5) };
    }

    #[test]
    fn tiered_escalation_uses_aggressive_on_retry_2() {
        let mut msgs1 = make_messages(20);
        let mut msgs2 = msgs1.clone();

        let r1 = try_compact_for_retry_tiered(&mut msgs1, Some(200_000), 100_000, 1);
        let r2 = try_compact_for_retry_tiered(&mut msgs2, Some(200_000), 100_000, 2);

        assert!(r1.is_some(), "tier-1 should compact");
        assert!(r2.is_some(), "tier-2 should compact");

        let r1 = r1.unwrap();
        let r2 = r2.unwrap();
        assert_eq!(r1.tier, CompactionTier::Default);
        assert_eq!(r2.tier, CompactionTier::Aggressive);

        // Aggressive pipeline should free at least as many tokens
        assert!(
            r2.tokens_freed >= r1.tokens_freed,
            "aggressive tier ({}) should free >= default ({})",
            r2.tokens_freed,
            r1.tokens_freed,
        );
    }

    #[test]
    fn emergency_tier_on_retry_3() {
        let mut msgs = make_messages(20);
        let r = try_compact_for_retry_tiered(&mut msgs, Some(200_000), 100_000, 3);
        assert!(r.is_some(), "emergency tier should compact");
        let r = r.unwrap();
        assert_eq!(r.tier, CompactionTier::Emergency);
    }

    #[test]
    fn effectiveness_tracker_records_compaction() {
        let mut tracker = CompactionEffectivenessTracker::default();
        tracker.record_compaction(5000);
        assert_eq!(tracker.cumulative_tokens_freed, 5000);
        assert_eq!(tracker.attempt_count, 1);
        assert!(!tracker.last_was_insufficient);

        tracker.mark_insufficient();
        assert!(tracker.last_was_insufficient);
        assert_eq!(tracker.last_tokens_freed, 5000);

        tracker.record_compaction(3000);
        assert_eq!(tracker.cumulative_tokens_freed, 8000);
        assert_eq!(tracker.attempt_count, 2);
        assert!(!tracker.last_was_insufficient);
    }

    #[test]
    fn effectiveness_tracker_telemetry() {
        let mut tracker = CompactionEffectivenessTracker::default();
        tracker.record_compaction(5000);
        tracker.mark_insufficient();
        let json = tracker.to_json();
        assert_eq!(json["cumulative_tokens_freed"], 5000);
        assert_eq!(json["attempt_count"], 1);
        assert_eq!(json["last_was_insufficient"], true);
    }

    #[test]
    fn fallback_estimate_counts_tool_calls_arguments() {
        // Regression: assistant messages carrying only tool_calls (no string content)
        // must contribute to the fallback token estimate. Otherwise a conversation
        // dominated by large tool-call arguments silently stays under budget and
        // compaction is skipped exactly when it is most needed.
        let big_args = "x".repeat(80_000); // ~20k tokens of arguments
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "go"}),
        ];
        // Add many assistant messages whose *only* payload is tool_calls arguments.
        for i in 0..10 {
            msgs.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call_{i}"),
                    "type": "function",
                    "function": {"name": "write_file", "arguments": &big_args},
                }],
            }));
            msgs.push(json!({
                "role": "tool",
                "tool_call_id": format!("call_{i}"),
                "content": "ok",
            }));
        }

        // Model limit 100k, real cost is ~200k+ tokens from tool_calls alone.
        // Fallback (no measured_tokens) MUST detect overflow.
        let result = try_compact_for_retry(&mut msgs, None, 100_000);
        assert!(
            result.is_some(),
            "fallback estimate ignored tool_calls arguments \
             and wrongly reported under-budget"
        );
    }

    #[test]
    fn cjk_content_triggers_compaction_without_measured_tokens() {
        // CJK chars are ~1.5 tokens each, not 0.75 (3 bytes / 4).
        // Without measured tokens, the fallback estimation should use
        // CJK-aware logic so it correctly detects being over budget.
        let cjk_content = "你好世界".repeat(2000); // 8000 CJK chars ≈ 12000 tokens
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "任务"}),
            json!({"role": "assistant", "content": &cjk_content, "tool_calls": []}),
            json!({"role": "user", "content": "继续"}),
            json!({"role": "assistant", "content": &cjk_content, "tool_calls": []}),
            json!({"role": "user", "content": "完成"}),
            json!({"role": "assistant", "content": &cjk_content, "tool_calls": []}),
        ];
        // limit = 15K tokens. With naive bytes/4: 3*24K bytes / 4 = 18K → over.
        // But CJK chars are 3 bytes, so tokens = 3*8000*1.5 = 36K → way over.
        // Both should trigger, but CJK-aware estimate should be higher.
        let result = try_compact_for_retry(&mut msgs, None, 15_000);
        assert!(
            result.is_some(),
            "CJK content without measured tokens should trigger compaction"
        );
    }
}
