//! Contract: `SelfModel::with_execution_ledger(&ledger)` binds the
//! self-awareness section to the **latest** `ExecutionResult` in the
//! ledger. This is the single high-level wiring point between the
//! execution layer (slices A–F) and the prompt surface (slice D).
//!
//! Three-state semantics encode "latest run wins":
//!
//! | ledger state            | unmet_postconditions becomes |
//! |-------------------------|------------------------------|
//! | empty (never ran)       | unchanged (no-op)            |
//! | latest ran, all met     | cleared (empty)              |
//! | latest ran, some unmet  | replaced with latest.unmet   |
//!
//! Any drift from this table leaks stale verdicts into the prompt and
//! breaks the "self-model sees the current truth" invariant.

use astra_plan::action_plan::{
    Action, ActionHandler, ActionPlan, ExecutionLedger, ExecutionPolicy, Executor, ObservedOutcome,
    PostCondition,
};
use astra_runtime::self_model::{SelfModel, UnmetPostCondition};
use serde_json::json;

// ─── Fixtures ───────────────────────────────────────────────────────────────

fn minimal_self_model() -> SelfModel {
    let empty = serde_json::json!({
        "capabilities": {
            "total_tools": 0, "tool_names": [], "tool_health": [],
            "deprioritized_tools": [], "pinned_tools": [], "skills": [],
            "boosted_tools": [], "widen_selection_pending": false, "outcome_memory": []
        },
        "state": {
            "turn_number": 0, "token_budget": null, "scenario": null,
            "active_experiment": null, "session_elapsed_secs": 0,
            "correction_count": 0, "compression_count": 0
        },
        "goals": {
            "goal": null, "session_goal": null, "plan_goal": null,
            "tracked_goal": null, "goal_source": "none", "tracking_status": "idle",
            "progress": null, "recent_milestones": [], "milestone_count": 0
        },
        "recent_signals": [],
        "constraints": {
            "max_mutations_per_turn": 2, "config_drift_ceiling": 0.3,
            "min_tool_pool_size": 5, "token_reserve_fraction": 0.2
        }
    });
    serde_json::from_value(empty).unwrap()
}

fn plan_one() -> ActionPlan {
    ActionPlan::new(
        vec![Action::new(0, "bash", json!({"cmd": "true"}))],
        vec![PostCondition::ToolCallSucceeded { action_index: 0 }],
    )
    .unwrap()
}

struct Always(bool);
impl ActionHandler for Always {
    fn handle(&self, a: &Action) -> ObservedOutcome {
        ObservedOutcome::ToolCall {
            action_index: a.index(),
            tool: a.tool().to_string(),
            success: self.0,
            result: json!({}),
        }
    }
}

// ─── Invariant 1: empty ledger is a no-op — does NOT clobber existing unmet ─
//
// An empty ledger means "never ran". In that state, the caller might still
// have attached unmet from another source (e.g. an out-of-band verifier).
// We must not silently erase it.
#[test]
fn empty_ledger_does_not_touch_existing_unmet_postconditions() {
    let preexisting = vec![UnmetPostCondition {
        action_index: 42,
        kind: "tool_call_succeeded".to_string(),
    }];
    let ledger = ExecutionLedger::new(2).unwrap(); // empty
    let sm = minimal_self_model()
        .with_unmet_postconditions(preexisting.clone())
        .with_execution_ledger(&ledger);

    assert_eq!(sm.unmet_postconditions, preexisting);
}

// ─── Invariant 2: latest all-met ⇒ clears unmet_postconditions ──────────────
//
// If the latest run succeeded entirely, the self-model MUST reflect that.
// Keeping old unmet around after a successful run is a lie.
#[test]
fn latest_all_met_clears_stale_unmet() {
    let stale = vec![
        UnmetPostCondition {
            action_index: 0,
            kind: "tool_call_succeeded".to_string(),
        },
        UnmetPostCondition {
            action_index: 1,
            kind: "tool_call_succeeded".to_string(),
        },
    ];
    let mut ledger = ExecutionLedger::new(2).unwrap();
    ledger.record(Executor::new(ExecutionPolicy::RunAll).run(&plan_one(), &Always(true)));

    let sm = minimal_self_model()
        .with_unmet_postconditions(stale)
        .with_execution_ledger(&ledger);

    assert!(
        sm.unmet_postconditions.is_empty(),
        "successful latest run must clear stale unmet, got {:?}",
        sm.unmet_postconditions,
    );
}

// ─── Invariant 3: latest has unmet ⇒ replaces unmet_postconditions ──────────
#[test]
fn latest_with_unmet_replaces_previous_unmet_list() {
    let preexisting = vec![UnmetPostCondition {
        action_index: 99,
        kind: "tool_call_succeeded".to_string(),
    }];
    let mut ledger = ExecutionLedger::new(2).unwrap();
    ledger.record(Executor::new(ExecutionPolicy::RunAll).run(&plan_one(), &Always(false)));

    let sm = minimal_self_model()
        .with_unmet_postconditions(preexisting)
        .with_execution_ledger(&ledger);

    // Preexisting (index 99) must be gone; latest unmet is action 0.
    assert_eq!(sm.unmet_postconditions.len(), 1);
    assert_eq!(sm.unmet_postconditions[0].action_index, 0);
    assert_eq!(sm.unmet_postconditions[0].kind, "tool_call_succeeded");
}

// ─── Invariant 4: only the MOST RECENT run matters (history is not merged) ──
//
// If the ledger has [fail, fail, succeed], the self-model sees NO unmet.
// If it has [succeed, fail], the self-model sees the second run's unmet.
// We must not accumulate unmet across runs; that would turn the self-model
// into a pessimistic historical log instead of a current-state snapshot.
#[test]
fn only_latest_entry_shapes_unmet_regardless_of_history() {
    let mut ledger = ExecutionLedger::new(4).unwrap();
    ledger.record(Executor::new(ExecutionPolicy::RunAll).run(&plan_one(), &Always(false)));
    ledger.record(Executor::new(ExecutionPolicy::RunAll).run(&plan_one(), &Always(false)));
    ledger.record(Executor::new(ExecutionPolicy::RunAll).run(&plan_one(), &Always(true)));

    let sm = minimal_self_model().with_execution_ledger(&ledger);
    assert!(
        sm.unmet_postconditions.is_empty(),
        "final run succeeded, self-model must be clean; got {:?}",
        sm.unmet_postconditions,
    );

    // Inverse direction: succeed then fail ⇒ latest fail dominates.
    let mut ledger2 = ExecutionLedger::new(4).unwrap();
    ledger2.record(Executor::new(ExecutionPolicy::RunAll).run(&plan_one(), &Always(true)));
    ledger2.record(Executor::new(ExecutionPolicy::RunAll).run(&plan_one(), &Always(false)));

    let sm2 = minimal_self_model().with_execution_ledger(&ledger2);
    assert_eq!(sm2.unmet_postconditions.len(), 1);
    assert_eq!(sm2.unmet_postconditions[0].action_index, 0);
}

// ─── Invariant 5: binding is visible in the prompt (end-to-end) ─────────────
//
// This is the "does it actually affect the LLM?" test. After binding a
// ledger whose latest run failed, `to_system_prompt_section` must surface
// the failure under the dedicated header with the correct action index.
#[test]
fn prompt_section_reflects_ledger_latest_after_binding() {
    let mut ledger = ExecutionLedger::new(2).unwrap();
    ledger.record(Executor::new(ExecutionPolicy::RunAll).run(&plan_one(), &Always(false)));

    let sm = minimal_self_model().with_execution_ledger(&ledger);
    let prompt = sm.to_system_prompt_section();

    assert!(prompt.contains("Unmet postconditions"));
    assert!(
        prompt.contains("action 0") || prompt.contains("#0"),
        "prompt must name action 0; got:\n{prompt}"
    );
}

// ─── Invariant 6: binding is idempotent — second call does not drift ────────
//
// Calling `with_execution_ledger(&same_ledger)` twice must produce the same
// SelfModel state as calling it once. Guards against an implementation that
// accidentally appends instead of replaces.
#[test]
fn double_binding_same_ledger_is_idempotent() {
    let mut ledger = ExecutionLedger::new(2).unwrap();
    ledger.record(Executor::new(ExecutionPolicy::RunAll).run(&plan_one(), &Always(false)));

    let sm_once = minimal_self_model().with_execution_ledger(&ledger);
    let sm_twice = minimal_self_model()
        .with_execution_ledger(&ledger)
        .with_execution_ledger(&ledger);

    assert_eq!(sm_once.unmet_postconditions, sm_twice.unmet_postconditions);
}

// ─── Invariant 7: ordering of builder calls does not matter ─────────────────
//
// `.with_unmet_postconditions(X).with_execution_ledger(&L)` must equal
// `.with_execution_ledger(&L)` when the ledger is non-empty. The ledger
// binding is the authoritative write when present.
#[test]
fn ledger_binding_wins_over_previously_set_unmet_regardless_of_order() {
    let manual = vec![UnmetPostCondition {
        action_index: 100,
        kind: "tool_call_succeeded".to_string(),
    }];
    let mut ledger = ExecutionLedger::new(2).unwrap();
    ledger.record(Executor::new(ExecutionPolicy::RunAll).run(&plan_one(), &Always(false)));

    let sm_a = minimal_self_model()
        .with_unmet_postconditions(manual.clone())
        .with_execution_ledger(&ledger);
    let sm_b = minimal_self_model().with_execution_ledger(&ledger);

    assert_eq!(sm_a.unmet_postconditions, sm_b.unmet_postconditions);
    // And neither retains the "manual" index 100.
    assert!(
        !sm_a
            .unmet_postconditions
            .iter()
            .any(|u| u.action_index == 100)
    );
}
