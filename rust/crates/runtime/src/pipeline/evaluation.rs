//! Turn evaluation engine.
//!
//! Moved to `astra-turn-core::evaluation`. This stub re-exports so that
//! existing `crate::pipeline::evaluation::*` paths continue to resolve.

pub use astra_turn_core::evaluation::*;

pub fn current_evaluation_thresholds() -> EvaluationThresholds {
    let cfg = crate::runtime_config::RuntimeConfig::load();
    EvaluationThresholds {
        sequential_read_churn: cfg
            .tool_selection
            .effective_sequential_read_churn_eval_threshold()
            as usize,
        redundant_overlapping_reads: cfg
            .tool_selection
            .effective_redundant_reads_eval_threshold()
            as usize,
    }
}
