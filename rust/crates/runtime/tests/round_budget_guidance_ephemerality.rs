//! Round-budget guidance ephemerality tests (Phase 3).
//!
//! `round_budget_directive_with` / `tool_round_guidance_trace_with` are pure
//! functions of `(messages, round_index, warning, limit)`. The production
//! pipeline regenerates the guidance string on every turn and injects it into
//! the *system* prompt — not into persistent `messages` history — so guidance
//! naturally cannot accumulate.
//!
//! These tests pin that contract:
//!
//!   * Guidance text at a given round is identical regardless of prior state
//!     (pure function property)
//!   * Guidance is empty below the warning threshold, present between warning
//!     and limit, and stronger at-or-above the limit
//!   * Across a simulated 5-turn loop, concatenating all guidance strings
//!     never contains more than one "Round Budget" heading per turn
//!   * `synthesize_or_batch_directive` only fires when the visible tail of the
//!     history is tool results; it disappears once the model emits text

use astra_runtime::prompts::{
    ROUND_BUDGET_HARD_LIMIT, ROUND_BUDGET_THRESHOLD, round_budget_directive,
    round_budget_directive_with, synthesize_or_batch_directive, tool_round_guidance_trace_with,
    tool_round_guidance_with,
};
use serde_json::json;

fn tool_msg(name: &str) -> serde_json::Value {
    json!({"role": "tool", "tool_call_id": format!("id-{name}"), "content": "ok"})
}

fn assistant_text(text: &str) -> serde_json::Value {
    json!({"role": "assistant", "content": text})
}

#[test]
fn directive_is_empty_below_warning_threshold() {
    for r in 0..ROUND_BUDGET_THRESHOLD {
        assert_eq!(
            round_budget_directive(r),
            "",
            "round {r} should not trigger guidance"
        );
    }
}

#[test]
fn directive_is_warning_between_threshold_and_limit() {
    for r in ROUND_BUDGET_THRESHOLD..ROUND_BUDGET_HARD_LIMIT {
        let s = round_budget_directive(r);
        assert!(
            s.contains("Round Budget Warning"),
            "round {r} missing warning"
        );
        assert!(
            !s.contains("Round Budget Exceeded"),
            "round {r} wrongly exceeded"
        );
    }
}

#[test]
fn directive_is_exceeded_at_or_above_limit() {
    let s = round_budget_directive(ROUND_BUDGET_HARD_LIMIT);
    assert!(s.contains("Round Budget Exceeded"));
    assert!(s.contains("You MUST produce your final answer NOW"));
}

#[test]
fn directive_is_pure_function_of_round_index_and_bounds() {
    // Same inputs → identical output, independent of when or how often called.
    let a = round_budget_directive_with(5, 3, 6);
    let b = round_budget_directive_with(5, 3, 6);
    assert_eq!(a, b);

    // Different round → different content.
    let c = round_budget_directive_with(2, 3, 6);
    assert_ne!(a, c);
}

#[test]
fn simulated_5_turn_loop_never_accumulates_guidance_blocks() {
    // Build up a growing messages list that the loop would hand to the guidance
    // helper each turn. The guidance string returned per turn must contain
    // exactly one "Round Budget" heading (at most), regardless of history size.
    let mut messages = vec![json!({"role": "user", "content": "go"})];

    for round in 0..=ROUND_BUDGET_HARD_LIMIT {
        // Simulate the model responding with tool calls on each round.
        messages.push(json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": format!("r{round}"), "type": "function", "function": {"name": "bash", "arguments": "{}"}}]
        }));
        messages.push(tool_msg(&format!("r{round}")));

        let guidance = tool_round_guidance_with(
            &messages,
            round + 1,
            ROUND_BUDGET_THRESHOLD,
            ROUND_BUDGET_HARD_LIMIT,
        );

        let budget_headings = guidance.matches("## ⚡ Round Budget Warning").count()
            + guidance.matches("## ⚠ Round Budget Exceeded").count();
        assert!(
            budget_headings <= 1,
            "round {round}: guidance accumulated {budget_headings} Round Budget headings:\n{guidance}"
        );
    }
}

#[test]
fn synthesize_directive_fires_after_tool_tail_and_clears_after_assistant_text() {
    let warn_round = ROUND_BUDGET_THRESHOLD;
    let tool_tail = vec![
        json!({"role": "user", "content": "go"}),
        tool_msg("a"),
        tool_msg("b"),
    ];
    assert!(!synthesize_or_batch_directive(&tool_tail, warn_round).is_empty());

    // After assistant speaks, the trailing tool-result window is gone.
    let mut resolved = tool_tail.clone();
    resolved.push(assistant_text("here is what I found"));
    assert_eq!(synthesize_or_batch_directive(&resolved, warn_round), "");
}

#[test]
fn trace_signals_mirror_directive_state() {
    let messages = vec![
        json!({"role": "user", "content": "go"}),
        tool_msg("a"),
        tool_msg("b"),
    ];

    // Below threshold: no warning, no synthesize nudge, but parallel_feedback
    // may still fire (it's purely structural on trailing tool count).
    let (s_low, signals_low) = tool_round_guidance_trace_with(&messages, 0, 3, 6);
    assert!(!signals_low.round_budget_warning);
    assert!(!signals_low.synthesize_or_batch);
    assert!(!s_low.contains("Round Budget"));

    // At threshold with tool tail: warning + synthesize both fire.
    let (s_mid, signals_mid) = tool_round_guidance_trace_with(&messages, 3, 3, 6);
    assert!(signals_mid.round_budget_warning);
    assert!(signals_mid.synthesize_or_batch);
    assert!(s_mid.contains("Round Budget Warning"));

    // At limit: exceeded heading present.
    let (s_high, signals_high) = tool_round_guidance_trace_with(&messages, 6, 3, 6);
    assert!(signals_high.round_budget_warning);
    assert!(s_high.contains("Round Budget Exceeded"));
}
