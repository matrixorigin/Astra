//! Cheap exact-duplicate elimination over canonical history.
//!
//! Unique evidence is replaced only by the semantic-summary or recoverable
//! history-artifact owners. Pressure alone does not authorize its deletion.
use astra_turn_core::compaction_types::CompactionTier;
pub use astra_turn_core::compression_types::{PipelineOutcome, TokenBudget};
use serde_json::Value;

use super::compaction::compact_duplicate_tool_outputs;

pub struct CompactionEngine {
    trigger: f64,
}

impl Default for CompactionEngine {
    fn default() -> Self {
        Self::default_pipeline_for(64_000)
    }
}

impl CompactionEngine {
    pub fn compress_if_needed(
        &self,
        messages: &mut Vec<Value>,
        budget: &TokenBudget,
    ) -> PipelineOutcome {
        let mut outcome = PipelineOutcome {
            layer_results: Vec::new(),
            total_tokens_freed: 0,
            budget_satisfied: !budget.is_over_budget(),
        };
        if budget.pressure() <= self.trigger {
            return outcome;
        }
        let result = compact_duplicate_tool_outputs(messages);
        if result.estimated_tokens_freed > 0 {
            outcome.total_tokens_freed = result.estimated_tokens_freed;
            outcome.budget_satisfied = budget
                .last_measured_tokens
                .saturating_sub(result.estimated_tokens_freed)
                <= budget.max_prompt_tokens;
            outcome
                .layer_results
                .push(("duplicate_tool_output_elimination".into(), result));
        }
        outcome
    }

    pub fn default_pipeline_for(max_tokens: u64) -> Self {
        Self {
            trigger: (CompactionTier::pre_turn_trigger(max_tokens) * 0.625).clamp(0.0, 1.0),
        }
    }

    pub fn aggressive_pipeline() -> Self {
        Self { trigger: 0.0 }
    }
}
