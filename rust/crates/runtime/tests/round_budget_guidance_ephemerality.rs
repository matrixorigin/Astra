//! Round-budget guidance ephemerality tests — POST-REFACTOR.
//!
//! The countdown-based round budget directive has been replaced by the
//! anomaly-based `LoopCircuitBreaker`. These tests verify that:
// These tests intentionally call deprecated functions to verify their no-op contract.
#![allow(deprecated)]
//!
//!   * `round_budget_directive` always returns empty (agent never sees budget pressure)
//!   * `synthesize_or_batch_directive` always returns empty
//!   * `tool_round_guidance_trace_with` never sets `round_budget_warning` or
//!     `synthesize_or_batch` signals
//!   * Parallel-batching nudge and positive feedback still work

use astra_runtime::prompts::{
    ROUND_BUDGET_HARD_LIMIT, ROUND_BUDGET_THRESHOLD, round_budget_directive,
    round_budget_directive_with, synthesize_or_batch_directive, tool_round_guidance_trace_with,
};
use serde_json::json;

fn tool_msg(name: &str) -> serde_json::Value {
    json!({"role": "tool", "tool_call_id": format!("id-{name}"), "content": "ok"})
}

#[test]
fn directive_is_always_empty_regardless_of_round() {
    for r in 0..=ROUND_BUDGET_HARD_LIMIT + 50 {
        assert_eq!(
            round_budget_directive(r),
            "",
            "round {r} should return empty (budget pressure removed)"
        );
    }
}

#[test]
fn directive_with_is_always_empty() {
    assert_eq!(round_budget_directive_with(0, 3, 6), "");
    assert_eq!(round_budget_directive_with(5, 3, 6), "");
    assert_eq!(round_budget_directive_with(100, 3, 6), "");
}

#[test]
fn synthesize_directive_is_always_empty() {
    let messages = vec![
        json!({"role": "user", "content": "go"}),
        tool_msg("a"),
        tool_msg("b"),
    ];
    assert_eq!(
        synthesize_or_batch_directive(&messages, ROUND_BUDGET_THRESHOLD, ROUND_BUDGET_THRESHOLD),
        ""
    );
    assert_eq!(
        synthesize_or_batch_directive(&messages, ROUND_BUDGET_HARD_LIMIT, ROUND_BUDGET_THRESHOLD),
        ""
    );
}

#[test]
fn trace_signals_never_set_budget_warning() {
    let messages = vec![
        json!({"role": "user", "content": "go"}),
        tool_msg("a"),
        tool_msg("b"),
    ];

    // At any round, budget signals are always false.
    let (_, signals) = tool_round_guidance_trace_with(&messages, 0, 3, 6);
    assert!(!signals.round_budget_warning);
    assert!(!signals.synthesize_or_batch);

    let (_, signals) = tool_round_guidance_trace_with(&messages, 3, 3, 6);
    assert!(!signals.round_budget_warning);
    assert!(!signals.synthesize_or_batch);

    let (_, signals) = tool_round_guidance_trace_with(&messages, 100, 3, 6);
    assert!(!signals.round_budget_warning);
    assert!(!signals.synthesize_or_batch);
}

#[test]
fn parallel_feedback_still_works() {
    // Parallel feedback fires when trailing tool count > 1.
    let messages = vec![
        json!({"role": "user", "content": "go"}),
        json!({"role": "assistant", "content": "", "tool_calls": [
            {"id": "a", "type": "function", "function": {"name": "read_file", "arguments": "{}"}},
            {"id": "b", "type": "function", "function": {"name": "grep", "arguments": "{}"}}
        ]}),
        tool_msg("a"),
        tool_msg("b"),
    ];

    let (guidance, signals) = tool_round_guidance_trace_with(&messages, 1, 8, 15);
    assert!(signals.parallel_feedback);
    assert!(guidance.contains("parallel"));
}

#[test]
fn constants_still_exported_for_backward_compat() {
    // These constants are retained for serde/test compat but no longer drive behavior.
    assert_eq!(ROUND_BUDGET_THRESHOLD, 8);
    assert_eq!(ROUND_BUDGET_HARD_LIMIT, 15);
}
