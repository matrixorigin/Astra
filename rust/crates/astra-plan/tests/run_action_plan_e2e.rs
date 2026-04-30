//! End-to-end contract for `run_action_plan`: drive an `ActionPlan` against
//! a real `astra_tools::ToolExecutor` and verify the resulting
//! `ExecutionResult` correctly classifies met/unmet postconditions.
//!
//! These tests use `DefaultToolExecutor` + a real filesystem `TempDir`, not
//! mocks. The point is to prove that the bridge (ExecutionDriver +
//! observation_from_tool_result + async loop) actually talks to a live
//! executor and the observation → postcondition correlation survives the
//! crossing. If any test here regresses, the async wiring is broken even
//! if the unit tests still pass.

use astra_plan::action_plan::{
    Action, ActionPlan, ExecutionPolicy, PostCondition, run_action_plan,
};
use astra_tools::executor::DefaultToolExecutor;
use astra_tools::{ToolContext, ToolExecutor};
use serde_json::json;
use tempfile::TempDir;

fn make_executor() -> (TempDir, DefaultToolExecutor) {
    let tmp = TempDir::new().unwrap();
    let ctx = ToolContext::test(tmp.path());
    let exec = DefaultToolExecutor::new(ctx);
    (tmp, exec)
}

// ─── Invariant 1: a real read_file action succeeds end-to-end ───────────────
//
// If this breaks, the async adapter isn't actually invoking the executor,
// or the ObservedOutcome isn't being read back correctly.
#[tokio::test]
async fn read_existing_file_produces_met_postcondition_with_output() {
    let (tmp, exec) = make_executor();
    std::fs::write(tmp.path().join("hello.txt"), "world").unwrap();

    let plan = ActionPlan::new(
        vec![Action::new(0, "read_file", json!({"path": "hello.txt"}))],
        vec![PostCondition::ToolCallSucceeded { action_index: 0 }],
    )
    .unwrap();

    let result = run_action_plan(&plan, ExecutionPolicy::RunAll, &exec).await;

    assert!(
        result.is_fully_satisfied(),
        "unmet: {:?}; output: {:?}",
        result.unmet,
        result.observations.first().map(|o| format!("{o:?}")),
    );
    assert_eq!(result.observations.len(), 1);
    assert_eq!(result.audit.len(), 1);

    // The real tool output surfaces in the observation payload.
    let astra_plan::action_plan::ObservedOutcome::ToolCall { result: payload, .. } =
        &result.observations[0];
    let output = payload["output"].as_str().unwrap_or("");
    assert!(
        output.contains("world"),
        "expected file contents in output, got: {output}",
    );
}

// ─── Invariant 2: a failing action produces an unmet postcondition ──────────
//
// The tool flags is_error=true (nonexistent file); the bridge must turn
// that into success=false, which propagates to unmet. This is the whole
// reason we didn't trust string-sniffing in slice I.
#[tokio::test]
async fn reading_nonexistent_file_produces_unmet_postcondition() {
    let (_tmp, exec) = make_executor();

    let plan = ActionPlan::new(
        vec![Action::new(
            0,
            "read_file",
            json!({"path": "nope-does-not-exist.txt"}),
        )],
        vec![PostCondition::ToolCallSucceeded { action_index: 0 }],
    )
    .unwrap();

    let result = run_action_plan(&plan, ExecutionPolicy::RunAll, &exec).await;

    assert_eq!(result.unmet.len(), 1, "unmet should have 1 entry");
    assert_eq!(
        result.unmet[0],
        PostCondition::ToolCallSucceeded { action_index: 0 }
    );
    assert!(result.met.is_empty());

    // Audit still records the failed attempt.
    assert_eq!(result.audit.len(), 1);
    assert!(!result.audit[0].success, "audit.success must be false");
}

// ─── Invariant 3: StopOnFailure halts mid-plan against a real executor ──────
//
// Proves the policy really reaches the async adapter. The second action
// must NOT execute, so only 1 audit entry exists.
#[tokio::test]
async fn stop_on_failure_halts_async_loop_after_real_tool_failure() {
    let (tmp, exec) = make_executor();
    std::fs::write(tmp.path().join("ok.txt"), "ok").unwrap();

    let plan = ActionPlan::new(
        vec![
            // Action 0 FAILS: missing file.
            Action::new(0, "read_file", json!({"path": "missing.txt"})),
            // Action 1 would succeed if reached.
            Action::new(1, "read_file", json!({"path": "ok.txt"})),
        ],
        vec![
            PostCondition::ToolCallSucceeded { action_index: 0 },
            PostCondition::ToolCallSucceeded { action_index: 1 },
        ],
    )
    .unwrap();

    let result = run_action_plan(&plan, ExecutionPolicy::StopOnFailure, &exec).await;

    assert_eq!(
        result.audit.len(),
        1,
        "StopOnFailure must halt after the first failure; audit={:?}",
        result.audit,
    );
    assert_eq!(result.observations.len(), 1);

    // Both postconditions are unmet: #0 because the tool failed, #1 because
    // it was never reached.
    assert_eq!(result.unmet.len(), 2);
    let unmet_indices: Vec<u32> = result
        .unmet
        .iter()
        .filter_map(|pc| pc.action_index())
        .collect();
    assert_eq!(unmet_indices, vec![0, 1]);
}

// ─── Invariant 4: RunAll keeps going after a real tool failure ──────────────
#[tokio::test]
async fn run_all_continues_past_real_tool_failure() {
    let (tmp, exec) = make_executor();
    std::fs::write(tmp.path().join("ok.txt"), "ok").unwrap();

    let plan = ActionPlan::new(
        vec![
            Action::new(0, "read_file", json!({"path": "missing.txt"})), // fails
            Action::new(1, "read_file", json!({"path": "ok.txt"})),      // succeeds
        ],
        vec![
            PostCondition::ToolCallSucceeded { action_index: 0 },
            PostCondition::ToolCallSucceeded { action_index: 1 },
        ],
    )
    .unwrap();

    let result = run_action_plan(&plan, ExecutionPolicy::RunAll, &exec).await;

    assert_eq!(result.audit.len(), 2, "RunAll must execute both actions");
    assert_eq!(result.observations.len(), 2);

    // Action 0 failed, action 1 succeeded.
    assert!(!result.audit[0].success);
    assert!(result.audit[1].success);

    assert_eq!(result.met.len(), 1);
    assert_eq!(
        result.met[0],
        PostCondition::ToolCallSucceeded { action_index: 1 }
    );
    assert_eq!(result.unmet.len(), 1);
    assert_eq!(
        result.unmet[0],
        PostCondition::ToolCallSucceeded { action_index: 0 }
    );
}

// ─── Invariant 5: adapter accepts `&dyn ToolExecutor` (trait object) ────────
//
// Production callers frequently hold a `Box<dyn ToolExecutor>` or a trait
// reference. If the adapter signature accidentally forced a concrete type
// with `Sized`, this test would fail to compile.
#[tokio::test]
async fn adapter_works_with_trait_object_reference() {
    let (tmp, exec) = make_executor();
    std::fs::write(tmp.path().join("x.txt"), "y").unwrap();

    let as_trait_obj: &dyn ToolExecutor = &exec;

    let plan = ActionPlan::new(
        vec![Action::new(0, "read_file", json!({"path": "x.txt"}))],
        vec![PostCondition::ToolCallSucceeded { action_index: 0 }],
    )
    .unwrap();

    let result = run_action_plan(&plan, ExecutionPolicy::RunAll, as_trait_obj).await;
    assert!(result.is_fully_satisfied());
}

// ─── Invariant 6: observations / audit / plan.actions lengths are coherent ──
//
// A fuzz-style sanity check: under RunAll, all three lengths must match
// after any pattern of successes/failures, because RunAll runs every action
// once. This catches off-by-one errors in the async loop that unit tests
// on the sync ExecutionDriver might miss.
#[tokio::test]
async fn run_all_produces_one_audit_and_one_observation_per_action() {
    let (tmp, exec) = make_executor();
    std::fs::write(tmp.path().join("a.txt"), "a").unwrap();
    std::fs::write(tmp.path().join("c.txt"), "c").unwrap();

    let plan = ActionPlan::new(
        vec![
            Action::new(0, "read_file", json!({"path": "a.txt"})),       // ok
            Action::new(1, "read_file", json!({"path": "missing.txt"})), // fail
            Action::new(2, "read_file", json!({"path": "c.txt"})),       // ok
        ],
        vec![
            PostCondition::ToolCallSucceeded { action_index: 0 },
            PostCondition::ToolCallSucceeded { action_index: 1 },
            PostCondition::ToolCallSucceeded { action_index: 2 },
        ],
    )
    .unwrap();

    let result = run_action_plan(&plan, ExecutionPolicy::RunAll, &exec).await;

    assert_eq!(result.observations.len(), plan.actions().len());
    assert_eq!(result.audit.len(), plan.actions().len());
    // Classification is correct for the exact pattern [ok, fail, ok].
    let success_flags: Vec<bool> = result.audit.iter().map(|e| e.success).collect();
    assert_eq!(success_flags, vec![true, false, true]);
}
