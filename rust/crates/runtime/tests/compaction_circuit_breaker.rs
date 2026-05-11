//! Circuit breaker contract for compaction-replay.
//!
//! Motivation: without a circuit breaker, a session whose context is
//! irrecoverably over the limit keeps hammering the pipeline on every turn.
//! Each attempt runs the full compression pass, fails to free meaningful
//! tokens, the next LLM call 413s again, and we repeat. A single tripped
//! session can burn hundreds of doomed API calls.
//!
//! Contract:
//!   * `CompactionEffectivenessTracker` counts consecutive futile attempts
//!     (attempt returned no tokens freed, OR pipeline refused to run).
//!   * After `MAX_CONSECUTIVE_FUTILE_ATTEMPTS` the tracker is "open" —
//!     subsequent calls to `try_compact_for_retry_*` return `Futile` without
//!     executing the pipeline, and the caller propagates a structured
//!     `ContextOverflow` interruption instead of looping.
//!   * A successful attempt (tokens_freed > 0) resets the counter.
//!
//! These tests lock in the public surface. They compile against the public
//! API only so a red/green cycle is possible without poking at private
//! fields.

use astra_runtime::turn::compaction_replay::{
    CompactionEffectivenessTracker, CompactionReplayOutcome, MAX_CONSECUTIVE_FUTILE_ATTEMPTS,
    try_compact_for_retry_checked,
};
use serde_json::{Value, json};

fn make_messages(n: usize) -> Vec<Value> {
    let mut msgs = vec![json!({"role": "system", "content": "sys"})];
    for i in 0..n {
        msgs.push(json!({"role": "user", "content": format!("q{i}")}));
        msgs.push(json!({
            "role": "assistant",
            "content": format!("a{i}: {}", "x".repeat(2000)),
            "tool_calls": []
        }));
    }
    msgs
}

#[test]
fn tracker_counts_futile_attempts() {
    let mut tracker = CompactionEffectivenessTracker::default();
    assert_eq!(tracker.consecutive_futile_attempts, 0);
    assert!(!tracker.is_circuit_open());

    tracker.record_futile();
    assert_eq!(tracker.consecutive_futile_attempts, 1);
    assert!(!tracker.is_circuit_open());

    tracker.record_futile();
    tracker.record_futile();
    assert_eq!(tracker.consecutive_futile_attempts, 3);
    // Default MAX is 3 (matches reference implementation).
    assert_eq!(MAX_CONSECUTIVE_FUTILE_ATTEMPTS, 3);
    assert!(
        tracker.is_circuit_open(),
        "circuit must open once the counter hits MAX"
    );
}

#[test]
fn tracker_resets_futile_counter_on_successful_compaction() {
    let mut tracker = CompactionEffectivenessTracker::default();
    tracker.record_futile();
    tracker.record_futile();
    assert_eq!(tracker.consecutive_futile_attempts, 2);

    tracker.record_compaction(5_000);
    assert_eq!(
        tracker.consecutive_futile_attempts, 0,
        "a successful compaction must clear the futile streak"
    );
    assert_eq!(tracker.cumulative_tokens_freed, 5_000);
    assert!(!tracker.is_circuit_open());
}

#[test]
fn tracker_futile_count_persists_across_mark_insufficient() {
    // mark_insufficient should NOT zero the futile counter — it only annotates
    // the last record_compaction. Futile counting is an independent axis.
    let mut tracker = CompactionEffectivenessTracker::default();
    tracker.record_futile();
    tracker.record_futile();
    tracker.mark_insufficient();
    assert_eq!(tracker.consecutive_futile_attempts, 2);
}

#[test]
fn tracker_to_json_exposes_circuit_state() {
    let mut tracker = CompactionEffectivenessTracker::default();
    tracker.record_futile();
    tracker.record_futile();
    tracker.record_futile();
    let v = tracker.to_json();
    assert_eq!(v["consecutive_futile_attempts"], 3);
    assert_eq!(v["circuit_open"], true);
}

#[test]
fn checked_helper_short_circuits_when_open() {
    // When the circuit is open, try_compact_for_retry_checked must return
    // `Futile` WITHOUT mutating messages — not re-run the pipeline.
    let mut tracker = CompactionEffectivenessTracker::default();
    tracker.record_futile();
    tracker.record_futile();
    tracker.record_futile();
    assert!(tracker.is_circuit_open());

    let mut msgs = make_messages(20);
    let original_len = msgs.len();
    let original_first = msgs[0].clone();

    let outcome = try_compact_for_retry_checked(
        &mut msgs,
        &mut tracker,
        Some(200_000), // way over budget
        100_000,
        1,
    );
    assert_eq!(outcome, CompactionReplayOutcome::CircuitOpen);
    assert_eq!(msgs.len(), original_len, "messages must be untouched");
    assert_eq!(msgs[0], original_first);
    // Counter stays at MAX (no further increments from short-circuit path).
    assert_eq!(
        tracker.consecutive_futile_attempts,
        MAX_CONSECUTIVE_FUTILE_ATTEMPTS
    );
}

#[test]
fn checked_helper_increments_futile_on_no_progress() {
    // A too-small message list → pipeline returns None → record_futile fires.
    let mut tracker = CompactionEffectivenessTracker::default();
    let mut msgs = vec![
        json!({"role": "system", "content": "sys"}),
        json!({"role": "user", "content": "hi"}),
    ];
    let outcome =
        try_compact_for_retry_checked(&mut msgs, &mut tracker, Some(10_000), 1_000, 1);
    assert_eq!(outcome, CompactionReplayOutcome::Futile);
    assert_eq!(tracker.consecutive_futile_attempts, 1);
}

#[test]
fn checked_helper_resets_on_progress() {
    let mut tracker = CompactionEffectivenessTracker::default();
    tracker.record_futile();
    tracker.record_futile();
    assert_eq!(tracker.consecutive_futile_attempts, 2);

    let mut msgs = make_messages(20);
    let outcome = try_compact_for_retry_checked(
        &mut msgs,
        &mut tracker,
        Some(200_000),
        100_000,
        1,
    );
    match outcome {
        CompactionReplayOutcome::Compacted(result) => {
            assert!(result.tokens_freed > 0);
        }
        other => panic!("expected Compacted, got {other:?}"),
    }
    assert_eq!(
        tracker.consecutive_futile_attempts, 0,
        "successful compaction must reset the streak"
    );
}
