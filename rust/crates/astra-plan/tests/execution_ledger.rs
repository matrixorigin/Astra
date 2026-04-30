//! Contract for `ExecutionLedger`: a bounded, append-only history of
//! `ExecutionResult`s that the self-model reads to learn about the last
//! few runs. The ledger is the bridge between the executor (slice C/E)
//! and the self-awareness layer (slice D).
//!
//! Every invariant here exists to prevent a specific failure mode that would
//! make the feedback loop lie: stale `latest`, silent overflow errors,
//! unstable iteration order, or unbounded growth.

use astra_plan::action_plan::{
    Action, ActionHandler, ActionPlan, ExecutionLedger, ExecutionLedgerError, ExecutionPolicy,
    ExecutionResult, Executor, ObservedOutcome, PostCondition,
};
use serde_json::json;

// ─── helpers ────────────────────────────────────────────────────────────────

struct OkWithTag(String);
impl ActionHandler for OkWithTag {
    fn handle(&self, action: &Action) -> ObservedOutcome {
        ObservedOutcome::ToolCall {
            action_index: action.index(),
            tool: action.tool().to_string(),
            success: true,
            result: json!({"tag": self.0}),
        }
    }
}

fn single_action_plan(cmd: &str) -> ActionPlan {
    ActionPlan::new(
        vec![Action::new(0, "bash", json!({"cmd": cmd}))],
        vec![PostCondition::ToolCallSucceeded { action_index: 0 }],
    )
    .unwrap()
}

fn run(cmd: &str, tag: &str) -> ExecutionResult {
    Executor::new(ExecutionPolicy::RunAll)
        .run(&single_action_plan(cmd), &OkWithTag(tag.to_string()))
}

fn extract_tag(r: &ExecutionResult) -> String {
    match &r.observations[0] {
        ObservedOutcome::ToolCall { result, .. } => result["tag"].as_str().unwrap().to_string(),
    }
}

// ─── Invariant 1: empty ledger has no latest ────────────────────────────────
//
// "Latest" means "most recently recorded". An empty ledger has NO latest —
// returning `None` forces callers (including `SelfModel`) to handle the
// "never executed anything" case explicitly, instead of silently
// fabricating an all-met verdict.
#[test]
fn empty_ledger_latest_is_none_and_len_is_zero() {
    let ledger = ExecutionLedger::new(4).unwrap();
    assert!(ledger.latest().is_none());
    assert_eq!(ledger.len(), 0);
    assert!(ledger.is_empty());
}

// ─── Invariant 2: record → latest reflects the new entry ────────────────────
#[test]
fn record_makes_entry_visible_as_latest() {
    let mut ledger = ExecutionLedger::new(4).unwrap();
    let r = run("echo hi", "alpha");
    ledger.record(r);

    let latest = ledger.latest().expect("latest must be Some after record");
    // The tag we embedded in the result propagates all the way through.
    assert_eq!(
        latest.observations[0],
        ObservedOutcome::ToolCall {
            action_index: 0,
            tool: "bash".to_string(),
            success: true,
            result: json!({"tag": "alpha"}),
        }
    );
    assert_eq!(ledger.len(), 1);
}

// ─── Invariant 3: capacity bound — overflow drops OLDEST, keeps newest ──────
//
// This is the key "not a memory leak" invariant. When capacity is exhausted,
// the ledger MUST drop the oldest entry, not reject the new one. Rejection
// would couple the ledger to a backpressure mechanism that doesn't exist in
// the executor and silently lose the *newest* observation — the opposite of
// what self-awareness needs.
#[test]
fn overflow_drops_oldest_entry_not_newest() {
    let mut ledger = ExecutionLedger::new(2).unwrap();
    ledger.record(run("c1", "first"));
    ledger.record(run("c2", "second"));
    ledger.record(run("c3", "third")); // overflow — "first" must be dropped

    assert_eq!(ledger.len(), 2);

    let tags: Vec<String> = ledger.iter().map(extract_tag).collect();

    // Oldest-to-newest iteration: "first" dropped; order stable.
    assert_eq!(tags, vec!["second".to_string(), "third".to_string()]);

    // latest() must be "third".
    assert_eq!(extract_tag(ledger.latest().unwrap()), "third");
}

// ─── Invariant 4: iter order is oldest → newest, stable ─────────────────────
#[test]
fn iter_preserves_insertion_order_oldest_first() {
    let mut ledger = ExecutionLedger::new(5).unwrap();
    for t in ["a", "b", "c", "d"] {
        ledger.record(run(&format!("cmd-{t}"), t));
    }

    let tags: Vec<String> = ledger.iter().map(extract_tag).collect();
    assert_eq!(tags, vec!["a", "b", "c", "d"]);
}

// ─── Invariant 5: len ≤ capacity always ─────────────────────────────────────
#[test]
fn len_never_exceeds_capacity_under_heavy_load() {
    let cap = 3;
    let mut ledger = ExecutionLedger::new(cap).unwrap();
    for _ in 0..50 {
        ledger.record(run("spam", "x"));
        assert!(ledger.len() <= cap, "len {} > cap {}", ledger.len(), cap);
    }
    assert_eq!(ledger.len(), cap);
}

// ─── Invariant 6: zero capacity is a construction error ─────────────────────
//
// A ledger with capacity 0 is a silent memory hole: every `record` call is
// dropped and `latest()` always returns None, which WILL mislead the
// self-model into thinking nothing has run. Reject at construction so a
// mis-wired config fails loudly instead of quietly.
#[test]
fn zero_capacity_is_rejected_at_construction() {
    let err = ExecutionLedger::new(0).unwrap_err();
    assert!(
        matches!(err, ExecutionLedgerError::ZeroCapacity),
        "expected ZeroCapacity, got {err:?}",
    );
}

// ─── Invariant 7: serde round-trip — ledger persists across process ────────
//
// The self-awareness system assumes ledgers can be snapshotted and rehydrated
// (journal replay, session resume). Serialize → Deserialize must preserve
// capacity, length, ordering, and contents.
#[test]
fn serde_round_trip_preserves_capacity_len_order_and_contents() {
    let mut ledger = ExecutionLedger::new(3).unwrap();
    ledger.record(run("one", "one"));
    ledger.record(run("two", "two"));

    let encoded = serde_json::to_string(&ledger).unwrap();
    let decoded: ExecutionLedger = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded.capacity(), ledger.capacity());
    assert_eq!(decoded.len(), ledger.len());
    let orig_tags: Vec<String> = ledger.iter().map(extract_tag).collect();
    let decoded_tags: Vec<String> = decoded.iter().map(extract_tag).collect();
    assert_eq!(decoded_tags, orig_tags);
}

// ─── Invariant 8: decoded ledger still honors overflow semantics ────────────
//
// After a round-trip, the ledger must STILL drop oldest on overflow. Guards
// against a deserializer that rebuilds the struct with broken internal
// invariants (e.g. forgets the ring-buffer head, making all future records
// append forever).
#[test]
fn decoded_ledger_still_drops_oldest_on_further_overflow() {
    let mut ledger = ExecutionLedger::new(2).unwrap();
    ledger.record(run("one", "one"));
    ledger.record(run("two", "two"));

    let encoded = serde_json::to_string(&ledger).unwrap();
    let mut decoded: ExecutionLedger = serde_json::from_str(&encoded).unwrap();

    decoded.record(run("three", "three"));

    let tags: Vec<String> = decoded.iter().map(extract_tag).collect();
    assert_eq!(tags, vec!["two", "three"]);
}

// ─── Invariant 9: `latest_unmet` convenience — the load-bearing accessor ────
//
// The self-model's one consumer of the ledger is "give me the unmet list from
// the most recent run". Exposing this as a typed accessor prevents every
// caller from re-implementing the `.latest().map(|r| r.unmet.clone())` dance
// and silently diverging. Empty ledger → None (not Some(empty)), because
// "never ran" and "ran and passed everything" are semantically different and
// the prompt-renderer already distinguishes them.
#[test]
fn latest_unmet_returns_none_for_empty_and_reflects_last_run_when_present() {
    let mut ledger = ExecutionLedger::new(2).unwrap();
    assert!(ledger.latest_unmet().is_none());

    // Record a run where everything succeeded ⇒ empty unmet ⇒ Some(vec![]).
    ledger.record(run("ok", "ok"));
    let got = ledger
        .latest_unmet()
        .expect("latest_unmet must be Some after at least one run");
    assert!(got.is_empty(), "successful run must have empty unmet");

    // Now force an unmet postcondition by running a plan that fails.
    struct AlwaysFail;
    impl ActionHandler for AlwaysFail {
        fn handle(&self, a: &Action) -> ObservedOutcome {
            ObservedOutcome::ToolCall {
                action_index: a.index(),
                tool: a.tool().to_string(),
                success: false,
                result: json!({}),
            }
        }
    }
    let plan = single_action_plan("will fail");
    let failed = Executor::new(ExecutionPolicy::RunAll).run(&plan, &AlwaysFail);
    ledger.record(failed);

    let got = ledger.latest_unmet().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0], PostCondition::ToolCallSucceeded { action_index: 0 });
}
