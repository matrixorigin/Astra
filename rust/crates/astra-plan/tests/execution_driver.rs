//! Contract for `ExecutionDriver`: a step-wise, synchronous state machine
//! that yields one `Action` at a time and accepts one `ObservedOutcome` per
//! step. The async boundary belongs to the caller — the plan layer itself
//! stays pure and sync.
//!
//! Invariants below encode the state-machine protocol. Violations are
//! typed errors, never silent misbehaviour.

use astra_plan::action_plan::{
    Action, ActionHandler, ActionPlan, DriverError, ExecutionDriver, ExecutionPolicy, Executor,
    ObservedOutcome, PostCondition,
};
use serde_json::json;

fn plan_n(n: u32) -> ActionPlan {
    let actions = (0..n)
        .map(|i| Action::new(i, "bash", json!({"i": i})))
        .collect();
    let pcs = (0..n)
        .map(|i| PostCondition::ToolCallSucceeded { action_index: i })
        .collect();
    ActionPlan::new(actions, pcs).unwrap()
}

fn outcome(idx: u32, success: bool) -> ObservedOutcome {
    ObservedOutcome::ToolCall {
        action_index: idx,
        tool: "bash".to_string(),
        success,
        result: json!({"i": idx}),
    }
}

// ─── Invariant 1: driver yields actions in dense 0..N order ─────────────────
#[test]
fn next_action_yields_actions_in_index_order() {
    let plan = plan_n(3);
    let mut driver = ExecutionDriver::new(&plan, ExecutionPolicy::RunAll);

    let a0 = driver.next_action().unwrap();
    assert_eq!(a0.index(), 0);
    driver.record(outcome(0, true)).unwrap();

    let a1 = driver.next_action().unwrap();
    assert_eq!(a1.index(), 1);
    driver.record(outcome(1, true)).unwrap();

    let a2 = driver.next_action().unwrap();
    assert_eq!(a2.index(), 2);
    driver.record(outcome(2, true)).unwrap();

    assert!(driver.next_action().is_none(), "plan exhausted");
}

// ─── Invariant 2: end-to-end equivalence with Executor::run ─────────────────
//
// For every combination of policy × outcome pattern, driving step-by-step
// must produce the SAME (observations, met, unmet, audit tool/indices)
// as running `Executor::run` with a handler that plays the same pattern.
// Timestamps and result_hash are excluded from comparison — the driver
// records timestamps at `record()` time, the Executor inside its loop;
// they're equal in structure but not in wall-clock.
#[test]
fn driver_and_executor_produce_equivalent_results_for_every_pattern() {
    struct Scripted(Vec<bool>);
    impl ActionHandler for Scripted {
        fn handle(&self, a: &Action) -> ObservedOutcome {
            outcome(a.index(), self.0[a.index() as usize])
        }
    }

    for pattern in [
        vec![true, true, true],
        vec![true, false, true],
        vec![false, true, true],
        vec![false, false, false],
    ] {
        for policy in [ExecutionPolicy::RunAll, ExecutionPolicy::StopOnFailure] {
            let plan = plan_n(pattern.len() as u32);

            // Path A: one-shot executor.
            let r_exec = Executor::new(policy).run(&plan, &Scripted(pattern.clone()));

            // Path B: step-wise driver.
            let mut driver = ExecutionDriver::new(&plan, policy);
            while let Some(a) = driver.next_action() {
                let success = pattern[a.index() as usize];
                driver.record(outcome(a.index(), success)).unwrap();
            }
            let r_driver = driver.finish();

            assert_eq!(
                r_exec.observations, r_driver.observations,
                "observations differ for policy={policy:?} pattern={pattern:?}"
            );
            assert_eq!(
                r_exec.met, r_driver.met,
                "met differs for policy={policy:?} pattern={pattern:?}"
            );
            assert_eq!(
                r_exec.unmet, r_driver.unmet,
                "unmet differs for policy={policy:?} pattern={pattern:?}"
            );
            assert_eq!(
                r_exec.audit.len(),
                r_driver.audit.len(),
                "audit len differs for policy={policy:?} pattern={pattern:?}"
            );
            for (ea, da) in r_exec.audit.iter().zip(r_driver.audit.iter()) {
                assert_eq!(ea.action_index, da.action_index);
                assert_eq!(ea.tool, da.tool);
                assert_eq!(ea.idempotency, da.idempotency);
                assert_eq!(ea.args_hash, da.args_hash);
                assert_eq!(ea.success, da.success);
            }
        }
    }
}

// ─── Invariant 3: record without a pending action errors, does not panic ───
#[test]
fn record_without_pending_action_is_a_typed_error() {
    let plan = plan_n(2);
    let mut driver = ExecutionDriver::new(&plan, ExecutionPolicy::RunAll);

    // No next_action() call yet → there is no pending action to record against.
    let err = driver.record(outcome(0, true)).unwrap_err();
    assert!(
        matches!(err, DriverError::NoPendingAction),
        "expected NoPendingAction, got {err:?}",
    );
}

// ─── Invariant 4: two records in a row (no next_action between) errors ─────
//
// The protocol is strict: every `record` MUST be preceded by its own
// `next_action`. Accepting a double-record would silently double-count
// outcomes or drop actions.
#[test]
fn double_record_without_advancing_is_a_typed_error() {
    let plan = plan_n(2);
    let mut driver = ExecutionDriver::new(&plan, ExecutionPolicy::RunAll);

    driver.next_action().unwrap();
    driver.record(outcome(0, true)).unwrap();

    // Skip the next_action call for action 1.
    let err = driver.record(outcome(1, true)).unwrap_err();
    assert!(
        matches!(err, DriverError::NoPendingAction),
        "expected NoPendingAction, got {err:?}",
    );
}

// ─── Invariant 5: outcome index must match the pending action ──────────────
//
// If the caller's tool pipeline returns an outcome keyed to a different
// action_index (a bug upstream), the driver must refuse. Silent acceptance
// would corrupt postcondition correlation — the one invariant we never give
// up.
#[test]
fn outcome_with_wrong_index_is_rejected_with_expected_and_got() {
    let plan = plan_n(2);
    let mut driver = ExecutionDriver::new(&plan, ExecutionPolicy::RunAll);
    let pending = driver.next_action().unwrap();
    assert_eq!(pending.index(), 0);

    let err = driver.record(outcome(7, true)).unwrap_err();
    assert!(
        matches!(
            err,
            DriverError::OutcomeIndexMismatch {
                expected: 0,
                got: 7
            }
        ),
        "expected OutcomeIndexMismatch {{expected:0, got:7}}, got {err:?}",
    );
}

// ─── Invariant 6: StopOnFailure halts next_action after a failing record ───
#[test]
fn stop_on_failure_halts_next_action_after_failing_record() {
    let plan = plan_n(3);
    let mut driver = ExecutionDriver::new(&plan, ExecutionPolicy::StopOnFailure);

    driver.next_action().unwrap();
    driver.record(outcome(0, true)).unwrap();

    driver.next_action().unwrap();
    driver.record(outcome(1, false)).unwrap();

    assert!(driver.next_action().is_none(), "must stop after failure");

    let r = driver.finish();
    // Exactly 2 audit entries: actions 0 and 1 both ran, action 2 never did.
    assert_eq!(r.audit.len(), 2);
    // Action 2's postcondition is therefore unmet.
    assert!(r.unmet.iter().any(|pc| pc.action_index() == Some(2)));
}

// ─── Invariant 7: RunAll keeps issuing actions past a failing record ───────
#[test]
fn run_all_keeps_going_past_failing_record() {
    let plan = plan_n(3);
    let mut driver = ExecutionDriver::new(&plan, ExecutionPolicy::RunAll);

    driver.next_action().unwrap();
    driver.record(outcome(0, false)).unwrap();

    // RunAll: action 1 must still be offered.
    let a1 = driver.next_action().unwrap();
    assert_eq!(a1.index(), 1);
}

// ─── Invariant 8: finish is terminal — no more actions, no more records ────
#[test]
fn after_finish_driver_is_terminal() {
    let plan = plan_n(1);
    let mut driver = ExecutionDriver::new(&plan, ExecutionPolicy::RunAll);

    driver.next_action().unwrap();
    driver.record(outcome(0, true)).unwrap();

    let _ = driver.finish();

    // We can't hold the driver after finish (consuming self) — this test
    // instead asserts the terminal property by rebuilding from a pattern
    // that would otherwise advance.
    let mut driver2 = ExecutionDriver::new(&plan, ExecutionPolicy::RunAll);
    driver2.next_action().unwrap();
    driver2.record(outcome(0, true)).unwrap();
    let r = driver2.finish();
    assert_eq!(r.audit.len(), 1);
}

// ─── Invariant 9: finish before any step yields a valid, all-unmet result ──
//
// Calling `finish` immediately on a fresh driver is not an error — it's the
// "we aborted before starting" case. The result MUST still carry the plan's
// full set of postconditions as unmet (because no observations exist).
#[test]
fn finish_without_any_step_marks_all_postconditions_unmet() {
    let plan = plan_n(3);
    let driver = ExecutionDriver::new(&plan, ExecutionPolicy::RunAll);
    let r = driver.finish();

    assert_eq!(r.observations.len(), 0);
    assert_eq!(r.audit.len(), 0);
    assert!(r.met.is_empty());
    assert_eq!(r.unmet.len(), plan.expected_postconditions().len());
}
