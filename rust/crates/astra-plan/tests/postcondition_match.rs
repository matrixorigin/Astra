//! Invariants for `PostCondition::matches(&ObservedOutcome)`.
//!
//! `matches` is the single source of truth the executor uses to decide
//! "did what I expected happen?". It must be index-strict, outcome-strict,
//! and total (never panic).

use astra_plan::action_plan::{ObservedOutcome, PostCondition};
use serde_json::json;

// ─── Invariant 1: matching index + successful outcome → true ────────────────
#[test]
fn tool_call_succeeded_matches_successful_tool_call_at_same_index() {
    let pc = PostCondition::ToolCallSucceeded { action_index: 3 };
    let observed = ObservedOutcome::ToolCall {
        action_index: 3,
        tool: "bash".into(),
        success: true,
        result: json!({"exit_code": 0}),
    };
    assert!(pc.matches(&observed));
}

// ─── Invariant 2: index mismatch → false ─────────────────────────────────────
//
// The postcondition names action 3; an outcome from action 4 — even a
// successful tool call with the same tool name — does NOT satisfy it. This is
// why we carry `action_index` on both sides: observation correlation is by
// index, not by tool name.
#[test]
fn index_mismatch_does_not_match_even_with_same_tool_and_success() {
    let pc = PostCondition::ToolCallSucceeded { action_index: 3 };
    let observed = ObservedOutcome::ToolCall {
        action_index: 4,
        tool: "bash".into(),
        success: true,
        result: json!({}),
    };
    assert!(
        !pc.matches(&observed),
        "index mismatch must not satisfy postcondition",
    );
}

// ─── Invariant 3: success=false → false ──────────────────────────────────────
//
// Same action, but the tool reported failure. A postcondition that says
// "succeeded" is not satisfied by a failed outcome, period.
#[test]
fn failed_tool_call_does_not_satisfy_succeeded_postcondition() {
    let pc = PostCondition::ToolCallSucceeded { action_index: 3 };
    let observed = ObservedOutcome::ToolCall {
        action_index: 3,
        tool: "bash".into(),
        success: false,
        result: json!({"exit_code": 1}),
    };
    assert!(!pc.matches(&observed));
}

// ─── Invariant 4: result payload is irrelevant to the success decision ──────
//
// The `result` JSON is free-form audit payload. It must not influence whether
// the postcondition matches — only `success` and `action_index` do. This
// guards against future drift where a matcher sneaks in string heuristics
// against the payload.
#[test]
fn result_payload_does_not_change_match_decision() {
    let pc = PostCondition::ToolCallSucceeded { action_index: 0 };

    let ok_empty = ObservedOutcome::ToolCall {
        action_index: 0,
        tool: "bash".into(),
        success: true,
        result: json!({}),
    };
    let ok_rich = ObservedOutcome::ToolCall {
        action_index: 0,
        tool: "bash".into(),
        success: true,
        result: json!({"bytes": 123, "note": "looks bad but status ok"}),
    };

    assert!(pc.matches(&ok_empty));
    assert!(pc.matches(&ok_rich));

    let fail_empty = ObservedOutcome::ToolCall {
        action_index: 0,
        tool: "bash".into(),
        success: false,
        result: json!({}),
    };
    let fail_rich = ObservedOutcome::ToolCall {
        action_index: 0,
        tool: "bash".into(),
        success: false,
        result: json!({"note": "looks fine but status fail"}),
    };

    assert!(!pc.matches(&fail_empty));
    assert!(!pc.matches(&fail_rich));
}

// ─── Invariant 5: matches is total over many index/success combos ───────────
//
// A light exhaustive sweep confirms `matches` never panics and the decision
// reduces to `(index equal) && (success = true)`.
#[test]
fn matches_is_total_and_reduces_to_index_and_success() {
    for expected_idx in 0u32..4 {
        let pc = PostCondition::ToolCallSucceeded {
            action_index: expected_idx,
        };
        for observed_idx in 0u32..4 {
            for success in [false, true] {
                let outcome = ObservedOutcome::ToolCall {
                    action_index: observed_idx,
                    tool: "bash".into(),
                    success,
                    result: json!({}),
                };
                let got = pc.matches(&outcome);
                let want = expected_idx == observed_idx && success;
                assert_eq!(
                    got, want,
                    "pc({expected_idx}) × obs({observed_idx}, success={success}) \
                     expected {want}, got {got}",
                );
            }
        }
    }
}

// ─── Invariant 6: round-trip — outcomes and postconditions are persistable ──
//
// Observations go to the audit/journal pipeline. If serde drifts, replay lies.
#[test]
fn observed_outcome_serde_round_trip() {
    let outcome = ObservedOutcome::ToolCall {
        action_index: 7,
        tool: "write_file".into(),
        success: true,
        result: json!({"path": "/tmp/x", "bytes": 42}),
    };
    let encoded = serde_json::to_string(&outcome).unwrap();
    let decoded: ObservedOutcome = serde_json::from_str(&encoded).unwrap();

    match (outcome, decoded) {
        (
            ObservedOutcome::ToolCall {
                action_index: ai,
                tool: ta,
                success: sa,
                result: ra,
            },
            ObservedOutcome::ToolCall {
                action_index: bi,
                tool: tb,
                success: sb,
                result: rb,
            },
        ) => {
            assert_eq!(ai, bi);
            assert_eq!(ta, tb);
            assert_eq!(sa, sb);
            assert_eq!(ra, rb);
        }
    }
}
