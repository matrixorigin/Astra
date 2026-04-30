//! Audit contract: every executed action must leave a stable, hashable trace
//! so later layers (rollback, learning, replay) can correlate observed
//! behavior with the exact intent that produced it.

use astra_plan::action_plan::{
    Action, ActionHandler, ActionPlan, ExecutionPolicy, Executor, ObservedOutcome, PostCondition,
};
use astra_turn_types::ToolIdempotency;
use serde_json::json;

struct OkHandler;
impl ActionHandler for OkHandler {
    fn handle(&self, action: &Action) -> ObservedOutcome {
        ObservedOutcome::ToolCall {
            action_index: action.index(),
            tool: action.tool().to_string(),
            success: true,
            result: json!({"ok": true}),
        }
    }
}

struct FailHandler;
impl ActionHandler for FailHandler {
    fn handle(&self, action: &Action) -> ObservedOutcome {
        ObservedOutcome::ToolCall {
            action_index: action.index(),
            tool: action.tool().to_string(),
            success: false,
            result: json!({"err": "nope"}),
        }
    }
}

fn plan_of(actions: Vec<Action>) -> ActionPlan {
    let pcs = actions
        .iter()
        .map(|a| PostCondition::ToolCallSucceeded {
            action_index: a.index(),
        })
        .collect();
    ActionPlan::new(actions, pcs).unwrap()
}

// ─── Invariant 1: every executed action produces exactly one entry ──────────
#[test]
fn one_audit_entry_per_executed_action_in_order() {
    let plan = plan_of(vec![
        Action::new(0, "read_file", json!({"path": "/tmp/a"})),
        Action::new(1, "bash", json!({"cmd": "ls"})),
        Action::new(2, "write_file", json!({"path": "/tmp/b", "body": "x"})),
    ]);
    let result = Executor::new(ExecutionPolicy::RunAll).run(&plan, &OkHandler);

    assert_eq!(result.audit.len(), 3);
    assert_eq!(result.audit[0].action_index, 0);
    assert_eq!(result.audit[0].tool, "read_file");
    assert_eq!(result.audit[1].tool, "bash");
    assert_eq!(result.audit[2].tool, "write_file");
}

// ─── Invariant 2: idempotency is drawn from the single registry ─────────────
//
// The audit MUST NOT re-classify tools locally; otherwise two places disagree
// on whether `bash` is idempotent and retry decisions go wrong. Bind to the
// central classifier.
#[test]
fn audit_idempotency_matches_central_registry() {
    let plan = plan_of(vec![
        Action::new(0, "read_file", json!({})),
        Action::new(1, "write_file", json!({"path": "/tmp/x"})),
        Action::new(2, "bash", json!({"cmd": "ls"})),
        Action::new(3, "ask_user", json!({"prompt": "?"})),
    ]);
    let result = Executor::new(ExecutionPolicy::RunAll).run(&plan, &OkHandler);

    assert_eq!(result.audit[0].idempotency, ToolIdempotency::PureRead);
    assert_eq!(
        result.audit[1].idempotency,
        ToolIdempotency::IdempotentWrite
    );
    assert_eq!(result.audit[2].idempotency, ToolIdempotency::NonIdempotent);
    assert_eq!(result.audit[3].idempotency, ToolIdempotency::NonIdempotent);
}

// ─── Invariant 3: args_hash is stable for equal args ────────────────────────
#[test]
fn equal_args_produce_equal_hash_across_runs() {
    let mk = || {
        plan_of(vec![Action::new(
            0,
            "bash",
            json!({"cmd": "ls", "dir": "/tmp"}),
        )])
    };
    let r1 = Executor::new(ExecutionPolicy::RunAll).run(&mk(), &OkHandler);
    let r2 = Executor::new(ExecutionPolicy::RunAll).run(&mk(), &OkHandler);

    assert_eq!(r1.audit[0].args_hash, r2.audit[0].args_hash);
    assert!(
        !r1.audit[0].args_hash.is_empty(),
        "args_hash must be a non-empty stable digest"
    );
}

// ─── Invariant 4: args_hash is canonical (field order invariant) ────────────
//
// Without canonicalization, `{"a":1,"b":2}` and `{"b":2,"a":1}` would hash to
// different values — breaking dedup and replay correlation. The audit layer
// MUST serialize args canonically before hashing.
#[test]
fn args_hash_is_invariant_under_json_field_order() {
    let plan_a = plan_of(vec![Action::new(0, "bash", json!({"a": 1, "b": 2}))]);
    let plan_b = plan_of(vec![Action::new(0, "bash", json!({"b": 2, "a": 1}))]);
    let ra = Executor::new(ExecutionPolicy::RunAll).run(&plan_a, &OkHandler);
    let rb = Executor::new(ExecutionPolicy::RunAll).run(&plan_b, &OkHandler);
    assert_eq!(
        ra.audit[0].args_hash, rb.audit[0].args_hash,
        "hash must be invariant under JSON object key order",
    );
}

// ─── Invariant 5: different args → different hash (no collisions on trivial)─
#[test]
fn different_args_produce_different_hash() {
    let p1 = plan_of(vec![Action::new(0, "bash", json!({"cmd": "ls"}))]);
    let p2 = plan_of(vec![Action::new(0, "bash", json!({"cmd": "rm -rf /"}))]);
    let r1 = Executor::new(ExecutionPolicy::RunAll).run(&p1, &OkHandler);
    let r2 = Executor::new(ExecutionPolicy::RunAll).run(&p2, &OkHandler);
    assert_ne!(r1.audit[0].args_hash, r2.audit[0].args_hash);
}

// ─── Invariant 6: result_hash recorded for both success and failure ─────────
//
// Audit must be symmetric: a failed action is still an action that happened
// and must be hashable for replay / dedup.
#[test]
fn result_hash_exists_for_success_and_failure() {
    let plan = plan_of(vec![Action::new(0, "bash", json!({"cmd": "ls"}))]);

    let ok = Executor::new(ExecutionPolicy::RunAll).run(&plan, &OkHandler);
    assert!(
        !ok.audit[0].result_hash.is_empty(),
        "success must still hash result payload"
    );
    assert!(ok.audit[0].success);

    let fail = Executor::new(ExecutionPolicy::RunAll).run(&plan, &FailHandler);
    assert!(
        !fail.audit[0].result_hash.is_empty(),
        "failure must still hash result payload"
    );
    assert!(!fail.audit[0].success);

    // And the two payloads differ, so hashes differ.
    assert_ne!(ok.audit[0].result_hash, fail.audit[0].result_hash);
}

// ─── Invariant 7: audit length == observations length (strict parity) ───────
#[test]
fn audit_and_observations_have_identical_length_after_stop_on_failure() {
    let plan = plan_of(vec![
        Action::new(0, "bash", json!({"cmd": "true"})),
        Action::new(1, "bash", json!({"cmd": "false"})),
        Action::new(2, "bash", json!({"cmd": "never"})),
    ]);
    // Handler fails on action 1; policy stops after.
    struct FailAtOne;
    impl ActionHandler for FailAtOne {
        fn handle(&self, action: &Action) -> ObservedOutcome {
            ObservedOutcome::ToolCall {
                action_index: action.index(),
                tool: action.tool().to_string(),
                success: action.index() != 1,
                result: json!({}),
            }
        }
    }
    let result = Executor::new(ExecutionPolicy::StopOnFailure).run(&plan, &FailAtOne);

    // StopOnFailure ⇒ executor halts after action 1. Audit length MUST equal
    // observations length — both record exactly what actually ran.
    assert_eq!(result.audit.len(), result.observations.len());
    assert_eq!(result.audit.len(), 2);
    assert_eq!(result.audit.last().unwrap().action_index, 1);
}

// ─── Invariant 8: timestamps are monotonically non-decreasing ───────────────
#[test]
fn audit_timestamps_are_monotonically_non_decreasing() {
    let plan = plan_of(vec![
        Action::new(0, "bash", json!({"cmd": "a"})),
        Action::new(1, "bash", json!({"cmd": "b"})),
        Action::new(2, "bash", json!({"cmd": "c"})),
    ]);
    let result = Executor::new(ExecutionPolicy::RunAll).run(&plan, &OkHandler);

    for w in result.audit.windows(2) {
        assert!(
            w[1].recorded_at_unix_ms >= w[0].recorded_at_unix_ms,
            "timestamps must be non-decreasing: {} then {}",
            w[0].recorded_at_unix_ms,
            w[1].recorded_at_unix_ms,
        );
    }
}

// ─── Invariant 9: full serde round-trip preserves audit ─────────────────────
#[test]
fn execution_result_serde_preserves_audit() {
    let plan = plan_of(vec![
        Action::new(0, "write_file", json!({"path": "/tmp/x"})),
        Action::new(1, "bash", json!({"cmd": "ls"})),
    ]);
    let result = Executor::new(ExecutionPolicy::RunAll).run(&plan, &OkHandler);

    let encoded = serde_json::to_string(&result).unwrap();
    let decoded: astra_plan::action_plan::ExecutionResult = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded.audit.len(), result.audit.len());
    for (a, b) in decoded.audit.iter().zip(result.audit.iter()) {
        assert_eq!(a.action_index, b.action_index);
        assert_eq!(a.tool, b.tool);
        assert_eq!(a.idempotency, b.idempotency);
        assert_eq!(a.args_hash, b.args_hash);
        assert_eq!(a.result_hash, b.result_hash);
        assert_eq!(a.success, b.success);
        assert_eq!(a.recorded_at_unix_ms, b.recorded_at_unix_ms);
    }
}
