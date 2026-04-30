//! Executor contract: running an ActionPlan produces an ExecutionResult whose
//! observations and postcondition diff are verifiable without re-consulting
//! the LLM.
//!
//! These tests use a deterministic `RecordingHandler` so we can assert the
//! full shape of the result — not just "it didn't crash".

use astra_plan::action_plan::{
    Action, ActionHandler, ActionPlan, ExecutionPolicy, ExecutionResult, Executor, ObservedOutcome,
    PostCondition,
};
use serde_json::json;
use std::cell::RefCell;

// ─── Test handler: scripted success/failure per action index ─────────────────

struct ScriptedHandler {
    // maps action_index → success flag to report
    script: Vec<bool>,
    // records every call, in order, so the test can assert the handler was
    // driven in exactly the order the plan declares.
    calls: RefCell<Vec<u32>>,
}

impl ScriptedHandler {
    fn new(script: Vec<bool>) -> Self {
        Self {
            script,
            calls: RefCell::new(Vec::new()),
        }
    }
    fn calls(&self) -> Vec<u32> {
        self.calls.borrow().clone()
    }
}

impl ActionHandler for ScriptedHandler {
    fn handle(&self, action: &Action) -> ObservedOutcome {
        self.calls.borrow_mut().push(action.index());
        let success = self
            .script
            .get(action.index() as usize)
            .copied()
            .unwrap_or(true);
        ObservedOutcome::ToolCall {
            action_index: action.index(),
            tool: action.tool().to_string(),
            success,
            result: json!({"scripted": success}),
        }
    }
}

fn plan_with(n: u32) -> ActionPlan {
    let actions: Vec<Action> = (0..n)
        .map(|i| Action::new(i, "bash", json!({"cmd": format!("cmd-{i}")})))
        .collect();
    let postconditions: Vec<PostCondition> = (0..n)
        .map(|i| PostCondition::ToolCallSucceeded { action_index: i })
        .collect();
    ActionPlan::new(actions, postconditions).expect("valid plan")
}

// ─── Invariant 1: observations are produced in action order, one per action ─
//
// If the executor reorders, skips, or silently duplicates actions, the
// observation diff correlates to the wrong intent. This is the single most
// important structural invariant.
#[test]
fn run_all_produces_one_observation_per_action_in_order() {
    let plan = plan_with(3);
    let handler = ScriptedHandler::new(vec![true, true, true]);
    let result = Executor::new(ExecutionPolicy::RunAll).run(&plan, &handler);

    assert_eq!(handler.calls(), vec![0, 1, 2]);
    assert_eq!(result.observations.len(), 3);
    for (i, obs) in result.observations.iter().enumerate() {
        assert_eq!(obs.action_index(), i as u32);
    }
}

// ─── Invariant 2: all-success ⇒ unmet is empty, met covers everything ───────
#[test]
fn all_success_leaves_no_unmet_postconditions() {
    let plan = plan_with(2);
    let handler = ScriptedHandler::new(vec![true, true]);
    let result = Executor::new(ExecutionPolicy::RunAll).run(&plan, &handler);

    assert!(result.unmet.is_empty(), "unmet={:?}", result.unmet);
    assert_eq!(result.met.len(), plan.expected_postconditions().len());
    assert!(result.is_fully_satisfied());
}

// ─── Invariant 3: any failure surfaces as an unmet postcondition ────────────
//
// The whole point of typed postconditions is that a single failure propagates
// to a concrete "this expectation wasn't met" record — not a vague error.
#[test]
fn single_failure_appears_in_unmet_with_its_action_index() {
    let plan = plan_with(3);
    let handler = ScriptedHandler::new(vec![true, false, true]);
    let result = Executor::new(ExecutionPolicy::RunAll).run(&plan, &handler);

    assert_eq!(result.unmet.len(), 1);
    assert_eq!(
        result.unmet[0],
        PostCondition::ToolCallSucceeded { action_index: 1 }
    );
    // The other two are satisfied.
    assert_eq!(result.met.len(), 2);
    assert!(!result.is_fully_satisfied());
}

// ─── Invariant 4: met ∪ unmet = all postconditions, disjoint ────────────────
//
// Mathematical closure: every postcondition in the plan ends up in exactly
// one bucket. No silently dropped postconditions; no double-counted ones.
#[test]
fn met_and_unmet_partition_all_postconditions() {
    let plan = plan_with(4);
    let handler = ScriptedHandler::new(vec![true, false, true, false]);
    let result = Executor::new(ExecutionPolicy::RunAll).run(&plan, &handler);

    let total = plan.expected_postconditions().len();
    assert_eq!(result.met.len() + result.unmet.len(), total);

    // Disjointness: no postcondition appears in both buckets.
    for m in &result.met {
        assert!(!result.unmet.contains(m), "{m:?} in both buckets");
    }
}

// ─── Invariant 5: StopOnFailure halts execution but still reports unmet ─────
//
// When configured to stop on first failure, the executor must:
//  (a) stop calling the handler after the failing action, and
//  (b) still classify all postconditions — the unexecuted ones are UNMET
//      (because we have no observation for them). This is the key safety
//      property: partial execution doesn't create partial verdicts.
#[test]
fn stop_on_failure_halts_handler_and_marks_remaining_unmet() {
    let plan = plan_with(4);
    let handler = ScriptedHandler::new(vec![true, false, true, true]);
    let result = Executor::new(ExecutionPolicy::StopOnFailure).run(&plan, &handler);

    // Handler was called exactly through the failure — action 0 and 1, no more.
    assert_eq!(handler.calls(), vec![0, 1]);

    // Only 2 observations recorded.
    assert_eq!(result.observations.len(), 2);

    // Postcondition 0 met; 1 failed ⇒ unmet; 2 and 3 never observed ⇒ unmet.
    assert_eq!(result.met.len(), 1);
    assert_eq!(
        result.met[0],
        PostCondition::ToolCallSucceeded { action_index: 0 }
    );
    let unmet_indices: Vec<u32> = result
        .unmet
        .iter()
        .filter_map(|pc| pc.action_index())
        .collect();
    assert_eq!(unmet_indices, vec![1, 2, 3]);
}

// ─── Invariant 6: Executor has no hidden state across runs ──────────────────
//
// Running the same plan twice with fresh handlers yields identical results.
// Guards against accidental caching / memoisation that would hide regressions.
#[test]
fn executor_is_stateless_across_runs() {
    let plan = plan_with(3);

    let h1 = ScriptedHandler::new(vec![true, false, true]);
    let r1 = Executor::new(ExecutionPolicy::RunAll).run(&plan, &h1);

    let h2 = ScriptedHandler::new(vec![true, false, true]);
    let r2 = Executor::new(ExecutionPolicy::RunAll).run(&plan, &h2);

    assert_eq!(r1.observations.len(), r2.observations.len());
    assert_eq!(r1.met, r2.met);
    assert_eq!(r1.unmet, r2.unmet);
}

// ─── Invariant 7: handler receives the exact Action it must execute ─────────
//
// If the executor drops or reconstructs Action fields (e.g. args), downstream
// audit and idempotency classification break. Verify the handler sees the
// original tool name and args verbatim.
#[test]
fn handler_receives_original_tool_and_args() {
    // Capture the action via a handler that echoes (tool, args) back as a result.
    struct EchoHandler;
    impl ActionHandler for EchoHandler {
        fn handle(&self, action: &Action) -> ObservedOutcome {
            ObservedOutcome::ToolCall {
                action_index: action.index(),
                tool: action.tool().to_string(),
                success: true,
                result: json!({"echo_args": action.args().clone()}),
            }
        }
    }

    let plan = ActionPlan::new(
        vec![Action::new(
            0,
            "write_file",
            json!({"path": "/tmp/x", "body": "y"}),
        )],
        vec![PostCondition::ToolCallSucceeded { action_index: 0 }],
    )
    .unwrap();

    let result = Executor::new(ExecutionPolicy::RunAll).run(&plan, &EchoHandler);
    let ObservedOutcome::ToolCall {
        tool, result: r, ..
    } = &result.observations[0];
    assert_eq!(tool, "write_file");
    assert_eq!(
        r["echo_args"],
        json!({"path": "/tmp/x", "body": "y"}),
        "handler must receive verbatim args"
    );
}

// ─── Invariant 8: ExecutionResult round-trips through serde ─────────────────
//
// Downstream consumers (SelfModel, audit, journal) serialize these results.
#[test]
fn execution_result_serde_round_trip() {
    let plan = plan_with(2);
    let handler = ScriptedHandler::new(vec![true, false]);
    let result = Executor::new(ExecutionPolicy::RunAll).run(&plan, &handler);

    let encoded = serde_json::to_string(&result).unwrap();
    let decoded: ExecutionResult = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded.observations.len(), result.observations.len());
    assert_eq!(decoded.met, result.met);
    assert_eq!(decoded.unmet, result.unmet);
}
