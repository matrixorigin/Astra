//! Schema-level invariants for `ActionPlan`.
//!
//! These tests encode the contract that distinguishes a typed ActionPlan from
//! the free-text plan narrative produced by decomposition. They MUST fail
//! before the schema exists, and each test targets a single invariant.

use astra_plan::action_plan::{Action, ActionPlan, ActionPlanError, PostCondition};
use serde_json::json;

// ─── Invariant 1: an empty action list is not a plan ─────────────────────────
//
// An ActionPlan with zero actions has no executable intent. If we accept it,
// downstream executor loops become silent no-ops and the observation layer has
// nothing to diff against. Reject at construction time.
#[test]
fn rejects_empty_action_list() {
    let err = ActionPlan::new(vec![], vec![]).unwrap_err();
    assert!(
        matches!(err, ActionPlanError::EmptyActions),
        "expected EmptyActions, got {err:?}",
    );
}

// ─── Invariant 2: every action is verifiable ─────────────────────────────────
//
// The reason we typed ActionPlan in the first place: the executor must be able
// to diff observations against expected postconditions WITHOUT asking the LLM
// to re-read intent. If an action has no postcondition that references it,
// either directly or via a global "AllSucceeded" clause, construction fails.
//
// This is the hard wall that stops the system from drifting back to
// "LLM decides what counts as done".
#[test]
fn rejects_action_without_any_postcondition_coverage() {
    let err = ActionPlan::new(
        vec![
            Action::new(0, "bash", json!({"cmd": "true"})),
            Action::new(1, "write_file", json!({"path": "/tmp/a"})),
        ],
        // Only action 0 is covered; action 1 has no matching postcondition.
        vec![PostCondition::ToolCallSucceeded { action_index: 0 }],
    )
    .unwrap_err();

    assert!(
        matches!(err, ActionPlanError::UncoveredAction { action_index: 1 }),
        "expected UncoveredAction {{ action_index: 1 }}, got {err:?}",
    );
}

#[test]
fn accepts_when_every_action_is_covered() {
    let plan = ActionPlan::new(
        vec![
            Action::new(0, "bash", json!({"cmd": "true"})),
            Action::new(1, "write_file", json!({"path": "/tmp/a"})),
        ],
        vec![
            PostCondition::ToolCallSucceeded { action_index: 0 },
            PostCondition::ToolCallSucceeded { action_index: 1 },
        ],
    )
    .expect("plan with full postcondition coverage must be accepted");

    assert_eq!(plan.actions().len(), 2);
    assert_eq!(plan.expected_postconditions().len(), 2);
}

// ─── Invariant 3: postconditions cannot dangle ───────────────────────────────
//
// Symmetric to invariant 2: a postcondition that references a non-existent
// action is either a typo or a stale plan. Reject so observation diff never
// silently ignores it.
#[test]
fn rejects_postcondition_referencing_unknown_action() {
    let err = ActionPlan::new(
        vec![Action::new(0, "bash", json!({"cmd": "true"}))],
        vec![
            PostCondition::ToolCallSucceeded { action_index: 0 },
            PostCondition::ToolCallSucceeded { action_index: 7 }, // out of range
        ],
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            ActionPlanError::DanglingPostCondition { action_index: 7 }
        ),
        "expected DanglingPostCondition {{ action_index: 7 }}, got {err:?}",
    );
}

// ─── Invariant 4: action indices are dense and stable ────────────────────────
//
// Observations later key into actions by index. If indices are sparse or
// duplicated, diff correlation breaks. Enforce at construction: actions must
// be 0..N with no gaps or duplicates.
#[test]
fn rejects_non_dense_action_indices() {
    let err = ActionPlan::new(
        vec![
            Action::new(0, "bash", json!({})),
            Action::new(2, "bash", json!({})), // skipped 1
        ],
        vec![
            PostCondition::ToolCallSucceeded { action_index: 0 },
            PostCondition::ToolCallSucceeded { action_index: 2 },
        ],
    )
    .unwrap_err();

    assert!(
        matches!(err, ActionPlanError::NonDenseIndices { .. }),
        "expected NonDenseIndices, got {err:?}",
    );
}

#[test]
fn rejects_duplicate_action_indices() {
    let err = ActionPlan::new(
        vec![
            Action::new(0, "bash", json!({})),
            Action::new(0, "bash", json!({})),
        ],
        vec![PostCondition::ToolCallSucceeded { action_index: 0 }],
    )
    .unwrap_err();

    assert!(
        matches!(err, ActionPlanError::DuplicateActionIndex { index: 0 }),
        "expected DuplicateActionIndex {{ 0 }}, got {err:?}",
    );
}

#[test]
fn rejects_out_of_order_action_indices_even_when_dense() {
    let err = ActionPlan::new(
        vec![
            Action::new(1, "bash", json!({"cmd": "second"})),
            Action::new(0, "bash", json!({"cmd": "first"})),
        ],
        vec![
            PostCondition::ToolCallSucceeded { action_index: 0 },
            PostCondition::ToolCallSucceeded { action_index: 1 },
        ],
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            ActionPlanError::ActionIndexPositionMismatch {
                position: 0,
                index: 1
            }
        ),
        "expected ActionIndexPositionMismatch {{ position: 0, index: 1 }}, got {err:?}",
    );
}

// ─── Invariant 5: actions must name a non-empty tool ─────────────────────────
//
// The tool name is the anchor for idempotency classification and audit. An
// empty tool name means "do nothing identifiable" — reject.
#[test]
fn rejects_action_with_empty_tool_name() {
    let err = ActionPlan::new(
        vec![Action::new(0, "", json!({}))],
        vec![PostCondition::ToolCallSucceeded { action_index: 0 }],
    )
    .unwrap_err();

    assert!(
        matches!(err, ActionPlanError::EmptyToolName { action_index: 0 }),
        "expected EmptyToolName {{ 0 }}, got {err:?}",
    );
}

#[test]
fn rejects_action_with_whitespace_only_tool_name() {
    let err = ActionPlan::new(
        vec![Action::new(0, " \t\n ", json!({}))],
        vec![PostCondition::ToolCallSucceeded { action_index: 0 }],
    )
    .unwrap_err();

    assert!(
        matches!(err, ActionPlanError::EmptyToolName { action_index: 0 }),
        "expected EmptyToolName {{ 0 }}, got {err:?}",
    );
}

// ─── Invariant 6: JSON round-trip preserves identity ─────────────────────────
//
// ActionPlans cross process boundaries (persistence, replay, audit). Every
// field must survive serde round-trip, otherwise "observed plan" and
// "replayed plan" drift.
#[test]
fn serde_round_trip_preserves_actions_and_postconditions() {
    let original = ActionPlan::new(
        vec![
            Action::new(0, "bash", json!({"cmd": "echo hi"})),
            Action::new(1, "write_file", json!({"path": "/tmp/x", "body": "y"})),
        ],
        vec![
            PostCondition::ToolCallSucceeded { action_index: 0 },
            PostCondition::ToolCallSucceeded { action_index: 1 },
        ],
    )
    .unwrap();

    let encoded = serde_json::to_string(&original).unwrap();
    let decoded: ActionPlan = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded.actions().len(), original.actions().len());
    for (a, b) in decoded.actions().iter().zip(original.actions().iter()) {
        assert_eq!(a.index(), b.index());
        assert_eq!(a.tool(), b.tool());
        assert_eq!(a.args(), b.args());
    }
    assert_eq!(
        decoded.expected_postconditions(),
        original.expected_postconditions()
    );
}

#[test]
fn serde_rejects_invalid_plan_instead_of_bypassing_constructor() {
    let invalid = json!({
        "actions": [
            {"index": 0, "tool": "bash", "args": {}}
        ],
        "expected_postconditions": [
            {"kind": "tool_call_succeeded", "action_index": 99}
        ]
    });

    let result = serde_json::from_value::<ActionPlan>(invalid);

    assert!(
        result.is_err(),
        "deserialization must enforce ActionPlan::new invariants; dangling postconditions cannot enter via JSON",
    );
}

#[test]
fn serde_rejects_empty_plan_instead_of_fabricating_noop_execution() {
    let invalid = json!({
        "actions": [],
        "expected_postconditions": []
    });

    let result = serde_json::from_value::<ActionPlan>(invalid);

    assert!(
        result.is_err(),
        "deserialization must reject empty plans just like ActionPlan::new",
    );
}

#[test]
fn serde_rejects_action_with_extra_free_text_field() {
    let invalid = json!({
        "actions": [
            {
                "index": 0,
                "tool": "bash",
                "args": {},
                "description": "free-text intent must not be accepted here"
            }
        ],
        "expected_postconditions": [
            {"kind": "tool_call_succeeded", "action_index": 0}
        ]
    });

    let result = serde_json::from_value::<ActionPlan>(invalid);

    assert!(
        result.is_err(),
        "typed ActionPlan JSON must reject extra free-text action fields instead of ignoring them",
    );
}

#[test]
fn serde_rejects_action_with_whitespace_only_tool_name() {
    let invalid = json!({
        "actions": [
            {"index": 0, "tool": "   ", "args": {}}
        ],
        "expected_postconditions": [
            {"kind": "tool_call_succeeded", "action_index": 0}
        ]
    });

    let result = serde_json::from_value::<ActionPlan>(invalid);

    assert!(
        result.is_err(),
        "deserialization must reject whitespace-only tool names just like ActionPlan::new",
    );
}

// ─── Invariant 7: ActionPlan is distinct from TaskPlan narrative ─────────────
//
// Guard test: typed ActionPlan must not accidentally re-introduce free-text
// intent. An `Action` carries only (tool, args); no `description`, no
// `narrative`, no `reasoning`. This is what makes it executable without LLM
// re-interpretation.
//
// We encode this as a compile-time-adjacent assertion: the struct's public
// surface does not include a string description field. If someone adds one,
// this test breaks on purpose to force a design discussion.
#[test]
fn action_public_surface_is_tool_and_args_only() {
    // Reflect via serde_json: serialize a single action and assert key set.
    let a = Action::new(0, "bash", json!({"cmd": "true"}));
    let v = serde_json::to_value(&a).unwrap();
    let obj = v.as_object().expect("Action must serialize to object");

    let keys: std::collections::BTreeSet<&str> = obj.keys().map(|k| k.as_str()).collect();
    let expected: std::collections::BTreeSet<&str> =
        ["index", "tool", "args"].into_iter().collect();

    assert_eq!(
        keys, expected,
        "Action serialized shape drifted; typed action must stay (index, tool, args) only. \
         Adding fields like `description` re-introduces free-text intent.",
    );
}
