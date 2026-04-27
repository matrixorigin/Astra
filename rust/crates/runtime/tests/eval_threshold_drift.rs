//! Guards against drift between `astra-turn-core` calibration constants and
//! the `astra-config` `effective_*` defaults that runtime callers hand to the
//! evaluation pipeline.
//!
//! Both crates compile independently: `astra-config` does not depend on
//! `astra-turn-core`, so they can silently disagree. Runtime is the only place
//! that depends on both — this file lives there so a drift fails CI.

use astra_config::runtime_config::ToolSelectionConfig;
use astra_turn_core::evaluation::{
    REDUNDANT_OVERLAPPING_READS_THRESHOLD, REDUNDANT_VALIDATION_RETRIES_THRESHOLD,
    SEARCH_FANOUT_THRESHOLD, SEQUENTIAL_READ_CHURN_THRESHOLD,
};

fn assert_default_matches(actual: u32, expected: usize, name: &str) {
    assert_eq!(
        actual as usize, expected,
        "{name}: astra-config default diverged from astra-turn-core calibration constant ({actual} != {expected})",
    );
}

#[test]
fn default_sequential_read_churn_matches_turn_core() {
    let cfg = ToolSelectionConfig::default();
    assert_default_matches(
        cfg.effective_sequential_read_churn_eval_threshold(),
        SEQUENTIAL_READ_CHURN_THRESHOLD,
        "sequential_read_churn",
    );
}

#[test]
fn default_redundant_reads_matches_turn_core() {
    let cfg = ToolSelectionConfig::default();
    assert_default_matches(
        cfg.effective_redundant_reads_eval_threshold(),
        REDUNDANT_OVERLAPPING_READS_THRESHOLD,
        "redundant_overlapping_reads",
    );
}

#[test]
fn default_search_fanout_matches_turn_core() {
    let cfg = ToolSelectionConfig::default();
    assert_default_matches(
        cfg.effective_search_fanout_eval_threshold(),
        SEARCH_FANOUT_THRESHOLD,
        "search_fanout",
    );
}

#[test]
fn default_redundant_validation_retries_matches_turn_core() {
    let cfg = ToolSelectionConfig::default();
    assert_default_matches(
        cfg.effective_redundant_validation_retries_eval_threshold(),
        REDUNDANT_VALIDATION_RETRIES_THRESHOLD,
        "redundant_validation_retries",
    );
}
