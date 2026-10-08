//! Fixed proactive/retry compaction policy over canonical history values.
//!
//! Mechanical rewrites live in `compaction`, shared with request-budget
//! compaction. This owner selects the existing ordered policy, prepares one
//! isolated candidate, and installs it only after useful progress.

use astra_config::runtime_config::CompressionConfig;
use astra_turn_core::compaction_types::CompactionTier;
pub use astra_turn_core::compression_types::{PipelineOutcome, TokenBudget};
use astra_turn_core::context_assembly_trace::CompressionMethod;
use serde_json::Value;

use super::compaction::{
    compact_duplicate_tool_outputs, compact_middle_messages, truncate_old_tool_results,
};

pub struct CompactionEngine {
    triggers: [f64; 4],
    keep_length: usize,
    age_secs: u64,
    keep_recent_turns: usize,
}

impl Default for CompactionEngine {
    fn default() -> Self {
        Self::default_pipeline_for(64_000)
    }
}

impl CompactionEngine {
    /// Run the fixed cheap-to-expensive schedule. The original canonical
    /// history remains available if candidate preparation fails unexpectedly;
    /// no externally supplied layer executes inside this rewrite boundary.
    pub fn compress_if_needed(
        &self,
        messages: &mut Vec<Value>,
        budget: &TokenBudget,
    ) -> PipelineOutcome {
        astra_turn_core::chat_history_openai::sanitize_empty_assistant_tool_calls_mut(messages);
        let mut outcome = PipelineOutcome {
            layer_results: Vec::new(),
            total_tokens_freed: 0,
            budget_satisfied: !budget.is_over_budget(),
        };
        if budget.pressure() <= self.triggers.into_iter().fold(f64::MAX, f64::min) {
            return outcome;
        }

        // One failure-isolation copy, without converting/reconstructing every
        // message, content block and provider-owned tool-call extension.
        astra_core::history_work::record_serialized_value(
            astra_core::history_work::HistoryWorkSite::CompactionHistoryClone,
            messages,
        );
        let mut candidate = messages.clone();
        let mut running_budget = budget.clone();
        let schedule = [
            CompressionMethod::DuplicateToolOutputElimination,
            CompressionMethod::ToolResultTruncation,
            CompressionMethod::TieredCompaction,
            CompressionMethod::ReactiveCompact,
        ];
        for (method, trigger) in schedule.into_iter().zip(self.triggers) {
            if running_budget.pressure() <= trigger {
                continue;
            }
            let (name, result) = match method {
                CompressionMethod::DuplicateToolOutputElimination => (
                    "duplicate_tool_output_elimination",
                    compact_duplicate_tool_outputs(&mut candidate),
                ),
                CompressionMethod::ToolResultTruncation => (
                    "tool_result_truncation",
                    truncate_old_tool_results(
                        &mut candidate,
                        &running_budget,
                        self.age_secs,
                        self.keep_length,
                    ),
                ),
                CompressionMethod::TieredCompaction => (
                    "tiered_compaction",
                    compact_middle_messages(
                        &mut candidate,
                        self.keep_recent_turns.saturating_mul(2),
                        false,
                    ),
                ),
                CompressionMethod::ReactiveCompact => (
                    "reactive_compact",
                    compact_middle_messages(&mut candidate, 4, true),
                ),
                CompressionMethod::LlmSummarization => {
                    unreachable!("model summaries are owned outside mechanical compaction")
                }
            };
            if result.estimated_tokens_freed == 0 {
                continue;
            }
            outcome.total_tokens_freed += result.estimated_tokens_freed;
            running_budget.last_measured_tokens = running_budget
                .last_measured_tokens
                .saturating_sub(result.estimated_tokens_freed);
            outcome.layer_results.push((name.to_string(), result));
            if !running_budget.is_over_budget() {
                break;
            }
        }
        outcome.budget_satisfied = !running_budget.is_over_budget();
        if outcome.total_tokens_freed > 0 {
            *messages = candidate;
        }
        outcome
    }

    pub fn from_config(config: &CompressionConfig, max_tokens: u64) -> Self {
        let base = CompactionTier::pre_turn_trigger(max_tokens);
        Self {
            triggers: [
                (base * 0.625).clamp(0.0, 1.0),
                (base * 0.75).clamp(0.0, 1.0),
                (base * 0.9375).clamp(0.0, 1.0),
                0.95,
            ],
            keep_length: config
                .max_tool_result_length
                .try_into()
                .expect("u32 fits usize"),
            age_secs: 3600,
            keep_recent_turns: config
                .preserve_recent_turns
                .try_into()
                .expect("u32 fits usize"),
        }
    }

    pub fn default_pipeline_for(max_tokens: u64) -> Self {
        Self::from_config(&CompressionConfig::default(), max_tokens)
    }

    pub fn aggressive_pipeline() -> Self {
        Self {
            triggers: [0.0; 4],
            keep_length: 512,
            age_secs: 300,
            keep_recent_turns: 2,
        }
    }

    pub fn emergency_pipeline() -> Self {
        Self {
            triggers: [0.0; 4],
            keep_length: 128,
            age_secs: 0,
            keep_recent_turns: 1,
        }
    }
}
