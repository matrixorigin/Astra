//! Layer 2: Tool Result Truncation.
//!
//! Truncates old tool-result content bodies to `max_keep_chars`.
//! Protects results within the protected head and the current round.

use super::super::helpers::protected_head_end;
use super::compression_layer_boilerplate;
use astra_turn_core::compression_types::{
    CompressionLayer, CompressionResult, Message, TokenBudget,
};
use std::time::Duration;

/// Truncates old tool-result content to `max_keep_chars`.
///
/// Uses `TokenBudget::now_secs` as the wall-clock time for deterministic
/// age-based cutoff, so tests can control time without mocking the system clock.
pub struct ToolResultTruncation {
    age_threshold: Duration,
    max_keep_chars: usize,
    trigger: f64,
    treat_missing_timestamp_as_old: bool,
}

impl ToolResultTruncation {
    pub fn new(age_threshold: Duration, max_keep_chars: usize, trigger_pressure: f64) -> Self {
        Self {
            age_threshold,
            max_keep_chars,
            trigger: trigger_pressure,
            treat_missing_timestamp_as_old: false,
        }
    }

    pub fn with_missing_timestamps_treated_as_old(mut self) -> Self {
        self.treat_missing_timestamp_as_old = true;
        self
    }
}

impl CompressionLayer for ToolResultTruncation {
    compression_layer_boilerplate!("tool_result_truncation", ToolResultTruncation);

    fn compress(&self, messages: &mut Vec<Message>, budget: &TokenBudget) -> CompressionResult {
        let cutoff = budget.now_secs.saturating_sub(self.age_threshold.as_secs());
        let head_end = protected_head_end(messages);
        let mut freed_tokens: usize = 0;
        let mut count: usize = 0;
        let mut affected_turns: Vec<u32> = Vec::new();

        for (idx, msg) in messages.iter_mut().enumerate() {
            if msg.role != "tool" {
                continue;
            }
            if idx < head_end {
                continue;
            }
            if let Some(current_round) = budget.current_round_index {
                if let Some(round_idx) = msg.round_index {
                    if round_idx >= current_round {
                        continue;
                    }
                }
            }
            match msg.timestamp {
                Some(ts) if ts > cutoff => continue,
                Some(_) => {}
                None if !self.treat_missing_timestamp_as_old => continue,
                None => {}
            }
            if let Some(ref content) = msg.content {
                let len = content.len();
                if len > self.max_keep_chars {
                    let safe_end = content.floor_char_boundary(self.max_keep_chars);
                    let truncated =
                        format!("{}… [truncated, was {} chars]", &content[..safe_end], len);
                    let original_tokens = crate::prompts::estimate_str_tokens(content);
                    let remaining_tokens = crate::prompts::estimate_str_tokens(&truncated);
                    msg.set_content(truncated);
                    msg.is_synthetic = true;
                    count += 1;
                    freed_tokens += original_tokens.saturating_sub(remaining_tokens);
                    let turn = msg.round_index.unwrap_or((idx / 2) as u32);
                    if affected_turns.last() != Some(&turn) {
                        affected_turns.push(turn);
                    }
                }
            }
        }

        CompressionResult {
            messages_removed: 0,
            estimated_tokens_freed: freed_tokens as u64,
            description: format!(
                "Truncated {} old tool results, freed ~{} tokens",
                count, freed_tokens
            ),
            affected_turns,
        }
    }
}
