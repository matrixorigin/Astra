//! Probe helper — compare a child's first-response cache_read_tokens
//! against the parent-side estimate carried on [`InheritedChildPrefix`]
//! and emit a [`ForkCacheEvent`].
//!
//! ## Role in the fork-prefix pipeline
//!
//! - PR 5.5 plumbed [`InheritedChildPrefix`] into [`SpawnRunConfig`].
//! - **PR 5.6 (this)** consumes it from the executor side: when the
//!   child completes its first API turn, we read the accumulated
//!   `total_cache_read` from the child's loop state and emit one
//!   [`ForkCacheEvent`] per spawn.
//!
//! ## Why a pure function + small state struct
//!
//! The `AgenticLoopHost::on_turn_completed` hook fires after every
//! ingested turn. We only want ONE probe per child spawn — after the
//! first turn. A tiny [`ForkCacheProbeState`] records whether the
//! probe has fired. The classifier + sink emission is a pure call
//! wrapping [`astra_turn_core::fork_cache_event::evaluate_fork_cache`].
//!
//! This module does NOT:
//! - Install the hook (that's the caller's impl of
//!   `AgenticLoopHost::on_turn_completed`).
//! - Construct the [`ForkCacheProbeState`] — the executor owns its
//!   lifecycle so `on_turn_completed` can see the same instance
//!   across turns.

use astra_turn_core::fork_cache_event::{
    ForkCacheEventSink, ForkCacheProbe, ForkCacheThresholds, evaluate_fork_cache,
};

use super::spawner::InheritedChildPrefix;

/// Tracks whether the first-response probe has fired for a given
/// child spawn. One instance per executor; reset by constructing a
/// fresh default.
///
/// Internally a single bool. Kept as a struct (not a bare bool) so
/// callers that want to observe "was this probed?" in tests can
/// query `fired()` rather than peek the bool directly — and so the
/// probe API can grow fields later (e.g. turn index, observed
/// delta) without breaking callers.
#[derive(Debug, Default, Clone, Copy)]
pub struct ForkCacheProbeState {
    fired: bool,
}

impl ForkCacheProbeState {
    /// New, un-fired state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the probe has already emitted.
    pub fn fired(&self) -> bool {
        self.fired
    }
}

/// Emit exactly one [`ForkCacheEvent`] for this child spawn on the
/// first successful ingested turn. Subsequent calls are no-ops.
///
/// Arguments:
/// - `probe_state` — per-spawn state recording "already probed".
/// - `inherited` — the prefix the executor was handed by the
///   spawner. `None` means the child wasn't requested to inherit —
///   nothing to probe, returns immediately.
/// - `child_run_id` — child's run id for the event payload.
/// - `observed_cache_read_tokens` — value from the child's first
///   API response. Callers read this from
///   `state.total_cache_read` in their `on_turn_completed` hook
///   (the ingest step has already added the latest turn's
///   `cache_read_input_tokens` into that accumulator).
/// - `thresholds` — classifier thresholds.
/// - `sink` — event bus.
pub fn maybe_emit_fork_cache_probe(
    probe_state: &mut ForkCacheProbeState,
    inherited: Option<&InheritedChildPrefix>,
    child_run_id: &str,
    observed_cache_read_tokens: u64,
    thresholds: ForkCacheThresholds,
    sink: &dyn ForkCacheEventSink,
) {
    let Some(inherited) = inherited else {
        return;
    };
    if probe_state.fired {
        return;
    }
    probe_state.fired = true;

    let probe = ForkCacheProbe {
        prefix_id: inherited.prefix_id.clone(),
        parent_run_id: inherited.parent_run_id.clone(),
        child_run_id: child_run_id.to_string(),
        expected_cache_read_tokens: inherited.expected_cache_read_tokens,
        observed_cache_read_tokens,
        provider: inherited.provider.clone(),
    };
    sink.emit(evaluate_fork_cache(probe, thresholds));
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_core::fork_cache_event::{ForkCacheEvent, ForkCacheOutcome};
    use astra_turn_core::fork_prefix::ProviderKind;
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct CollectSink(Mutex<Vec<ForkCacheEvent>>);
    impl ForkCacheEventSink for CollectSink {
        fn emit(&self, event: ForkCacheEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn sample_inherited() -> InheritedChildPrefix {
        InheritedChildPrefix {
            prefix_id: "pfx-p1".into(),
            parent_run_id: "run-parent".into(),
            provider: ProviderKind::Anthropic,
            prefix_messages: vec![],
            expected_cache_read_tokens: 10_000,
        }
    }

    #[test]
    fn none_inherited_is_no_op() {
        // Child that didn't request inheritance — helper must not
        // touch the sink and must not mark the probe as fired (so
        // future turns on the same child stay correctly untouched).
        let mut state = ForkCacheProbeState::new();
        let sink = CollectSink::default();
        maybe_emit_fork_cache_probe(
            &mut state,
            None,
            "run-child",
            0,
            ForkCacheThresholds::default(),
            &sink,
        );
        assert!(!state.fired(), "probe must not fire when inherited is None");
        assert!(sink.0.lock().unwrap().is_empty());
    }

    #[test]
    fn first_call_emits_one_event() {
        let mut state = ForkCacheProbeState::new();
        let sink = CollectSink::default();
        let inherited = sample_inherited();

        maybe_emit_fork_cache_probe(
            &mut state,
            Some(&inherited),
            "run-child",
            9_500, // 95% — Hit
            ForkCacheThresholds::default(),
            &sink,
        );

        let events = sink.0.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].outcome, ForkCacheOutcome::Hit);
        assert_eq!(events[0].prefix_id, "pfx-p1");
        assert_eq!(events[0].child_run_id, "run-child");
        assert_eq!(events[0].observed_cache_read_tokens, 9_500);
        assert!(state.fired(), "fired flag must flip after first emit");
    }

    #[test]
    fn subsequent_calls_are_no_ops() {
        // The hook fires on every ingested turn; we only want ONE
        // event per child spawn (first-response probe). Second and
        // later calls must be no-ops, even with different observed
        // values that would classify differently.
        let mut state = ForkCacheProbeState::new();
        let sink = CollectSink::default();
        let inherited = sample_inherited();

        maybe_emit_fork_cache_probe(
            &mut state,
            Some(&inherited),
            "run-child",
            9_500,
            ForkCacheThresholds::default(),
            &sink,
        );
        maybe_emit_fork_cache_probe(
            &mut state,
            Some(&inherited),
            "run-child",
            0, // would classify as Miss, but must not emit
            ForkCacheThresholds::default(),
            &sink,
        );
        maybe_emit_fork_cache_probe(
            &mut state,
            Some(&inherited),
            "run-child",
            1_000_000, // would classify as ExceededExpected
            ForkCacheThresholds::default(),
            &sink,
        );

        let events = sink.0.lock().unwrap();
        assert_eq!(
            events.len(),
            1,
            "exactly one event per child spawn, got {}",
            events.len()
        );
    }

    #[test]
    fn miss_observation_still_emits_one_event() {
        // Hit / Miss / Drift / ExceededExpected all route through
        // the same path; absence of an emission on Miss would break
        // the "exactly one event per spawn" contract. Pin it.
        let mut state = ForkCacheProbeState::new();
        let sink = CollectSink::default();
        maybe_emit_fork_cache_probe(
            &mut state,
            Some(&sample_inherited()),
            "run-child",
            0, // Miss
            ForkCacheThresholds::default(),
            &sink,
        );
        assert_eq!(sink.0.lock().unwrap().len(), 1);
        assert_eq!(
            sink.0.lock().unwrap()[0].outcome,
            ForkCacheOutcome::Miss
        );
    }

    #[test]
    fn fired_flag_persists_across_none_calls() {
        // If a hook wrapper first emits an event (fired=true) and a
        // later call erroneously passes None, the flag must still
        // read true — "already fired" is the terminal state.
        let mut state = ForkCacheProbeState::new();
        let sink = CollectSink::default();
        maybe_emit_fork_cache_probe(
            &mut state,
            Some(&sample_inherited()),
            "run-child",
            9_000,
            ForkCacheThresholds::default(),
            &sink,
        );
        maybe_emit_fork_cache_probe(
            &mut state,
            None,
            "run-child",
            0,
            ForkCacheThresholds::default(),
            &sink,
        );
        assert!(state.fired());
        assert_eq!(sink.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn event_carries_parent_run_id_from_inherited() {
        let mut state = ForkCacheProbeState::new();
        let sink = CollectSink::default();
        let inherited = InheritedChildPrefix {
            prefix_id: "pfx-77".into(),
            parent_run_id: "run-parent-XYZ".into(),
            provider: ProviderKind::Anthropic,
            prefix_messages: vec![],
            expected_cache_read_tokens: 500,
        };
        maybe_emit_fork_cache_probe(
            &mut state,
            Some(&inherited),
            "run-child-QQ",
            500,
            ForkCacheThresholds::default(),
            &sink,
        );
        let events = sink.0.lock().unwrap();
        assert_eq!(events[0].parent_run_id, "run-parent-XYZ");
        assert_eq!(events[0].child_run_id, "run-child-QQ");
    }

    #[test]
    fn zero_expected_with_zero_observed_still_fires_one_event() {
        // Degenerate case: capture side hasn't yet plumbed an
        // expected-token estimate (PR 5.5 uses 0 sentinel). The
        // probe must STILL emit — downstream analytics should see
        // the Miss so they can weigh child-count-with-inheritance
        // against child-count-that-missed.
        let mut state = ForkCacheProbeState::new();
        let sink = CollectSink::default();
        let mut inherited = sample_inherited();
        inherited.expected_cache_read_tokens = 0;
        maybe_emit_fork_cache_probe(
            &mut state,
            Some(&inherited),
            "run-child",
            0,
            ForkCacheThresholds::default(),
            &sink,
        );
        let events = sink.0.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].outcome, ForkCacheOutcome::Miss);
    }
}
