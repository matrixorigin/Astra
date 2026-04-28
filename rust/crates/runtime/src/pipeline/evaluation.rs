//! Turn evaluation engine.
//!
//! Moved to `astra-turn-core::evaluation`. This stub re-exports so that
//! existing `crate::pipeline::evaluation::*` paths continue to resolve.

pub use astra_turn_core::evaluation::*;

pub fn current_evaluation_thresholds() -> EvaluationThresholds {
    let cfg = astra_config::runtime_config::RuntimeConfig::load();
    EvaluationThresholds {
        sequential_read_churn: cfg
            .tool_selection
            .effective_sequential_read_churn_eval_threshold()
            as usize,
        redundant_overlapping_reads: cfg
            .tool_selection
            .effective_redundant_reads_eval_threshold()
            as usize,
        search_fanout: cfg.tool_selection.effective_search_fanout_eval_threshold() as usize,
        redundant_validation_retries: cfg
            .tool_selection
            .effective_redundant_validation_retries_eval_threshold()
            as usize,
        llm_round_churn: astra_turn_core::evaluation::LLM_ROUND_CHURN_THRESHOLD,
        exploration_family_churn: astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD,
    }
}
