//! Compaction Replay Lane
//!
//! When a context window overflow (413 / prompt-too-long) is detected during
//! the agentic loop, this module runs the compression pipeline and allows the
//! turn to retry with a compacted message list instead of hard-failing.
//!
//! Design: the lane runs at most once per turn (gated by
//! `consecutive_context_window_errors`). If the first compaction attempt frees
//! enough tokens, the turn retries immediately. If the second attempt still
//! fails, the error propagates as a structured `InterruptionRecord` with
//! `ResumeAction::CompactAndRetry` (for cross-session resume).

use super::context_compression::{CompressionPipeline, PipelineOutcome, TokenBudget};
use serde_json::Value;

/// Maximum number of automatic compact-and-retry cycles per turn.
///
/// After this many consecutive context-window errors *within the same turn*,
/// we stop retrying and let the error propagate with a structured
/// `InterruptionRecord`.
pub(crate) const MAX_COMPACT_RETRIES: u32 = 2;

/// Outcome of a compaction-replay attempt.
#[derive(Debug)]
#[allow(dead_code)] // Fields consumed by callers and future telemetry.
pub(crate) struct CompactionReplayResult {
    /// Whether the compaction freed any tokens at all.
    pub freed_any: bool,
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
}

/// Run the compression pipeline on the message list after a context-window error.
///
/// Returns `None` if there are too few messages to compact or if no tokens were
/// freed (compaction is futile). Returns `Some(result)` with details on success.
pub(crate) fn try_compact_for_retry(
    messages: &mut Vec<Value>,
    last_measured_tokens: Option<u64>,
    model_context_limit: u64,
) -> Option<CompactionReplayResult> {
    if messages.len() <= 4 {
        return None; // Too few messages to compact meaningfully
    }

    // Build a budget that reflects the overflow.
    // If we have measured tokens, use them; otherwise estimate from message content.
    let measured = last_measured_tokens.unwrap_or_else(|| {
        let total_chars: usize = messages.iter().map(|m| m.to_string().len()).sum();
        (total_chars / 4) as u64 // rough ~4 chars/token
    });

    let max_tokens = if model_context_limit > 0 {
        model_context_limit
    } else {
        // Conservative default: assume 128K context, 90% usable for prompt
        (128_000.0 * 0.9) as u64
    };

    let budget = TokenBudget {
        max_prompt_tokens: max_tokens,
        last_measured_tokens: measured,
        chars_per_token: 4.0,
    };

    if !budget.is_over_budget() && budget.pressure() < 0.85 {
        // Not under pressure — compaction won't help.
        return None;
    }

    let pipeline = CompressionPipeline::default_pipeline();
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
        freed_any: true,
        tokens_freed: outcome.total_tokens_freed,
        messages_removed,
        layer_descriptions,
        budget_likely_satisfied: outcome.budget_satisfied,
        pipeline_outcome: outcome,
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
        assert!(r.freed_any);
        assert!(r.tokens_freed > 0);
        assert!(
            msgs.len() < original_len || r.messages_removed > 0,
            "expected fewer messages after compaction"
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
            freed_any: true,
            tokens_freed: 5000,
            messages_removed: 12,
            layer_descriptions: vec!["ToolResultTruncation: ~2000 tokens".into()],
            budget_likely_satisfied: true,
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
}
