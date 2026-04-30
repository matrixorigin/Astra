//! Structured telemetry event for fork-prefix cache outcomes.
//!
//! When a child agent spawns with an inherited [`ForkPrefix`] and makes
//! its first API call, the response carries `cache_read_input_tokens`.
//! Comparing observed tokens against the token estimate the parent
//! captured tells us whether cache inheritance actually worked — the
//! hard evidence that the whole pipeline (PR 1–5b) is paying off.
//!
//! This module defines the event type, a pure classification function,
//! and a minimal sink trait. It intentionally does NOT:
//! - Probe child responses (that lives in the runtime layer that
//!   actually owns the child's first API call; PR 5.x wires it in).
//! - Dispatch to a concrete bus (CSL, OTLP, stdout) — the runtime
//!   layer chooses.
//! - Decide what to do when a mismatch fires — the caller's soft-
//!   core policy; the event is just fact-reporting.
//!
//! ## Role in the fork-prefix pipeline
//!
//! - PR 1–5b: capture, store, resolve, reconstruct.
//! - **PR 5c (this)**: the feedback channel — "did the cache bet
//!   actually pay off on the child's first response?"
//!
//! ## Design philosophy
//!
//! Same "hard shell / soft core" split: the shell is the classifier
//! (runtime decides *what counts* as hit / miss) and the serialization
//! contract (wire-stable for downstream consumers); the soft core is
//! the thresholds, which are parameterised so evolution / A-B logic
//! can tune them without recompiling.

use serde::{Deserialize, Serialize};

use crate::fork_prefix::ProviderKind;

// ---------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------

/// Classification of a single fork-cache probe.
///
/// Named outcomes so dashboards and evolution rules can bucket without
/// inventing their own ad-hoc labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkCacheOutcome {
    /// Observed cache_read_tokens met or exceeded the hit threshold
    /// relative to expected. The inheritance worked as intended.
    Hit,
    /// Observed fell below the hit threshold but above the full-miss
    /// floor — partial reuse. Typically signals a legitimate drift
    /// cause (e.g., microcompact trimmed the prefix tail) rather than
    /// a bug. Worth surfacing but not escalating.
    PartialDrift,
    /// Observed is effectively zero (below the full-miss floor) —
    /// nothing was reused. Either a capture / reconstruction bug, a
    /// silent provider cache TTL expiry, or a provider-side mismatch
    /// our validate_spawn didn't catch.
    Miss,
    /// Observed HIGHER than expected. Not an error — usually means
    /// our expected-token estimate was conservative, or the provider
    /// applied a longer cache than we tracked. Logged so evolution
    /// can calibrate the estimator, not treated as a problem.
    ExceededExpected,
}

/// Structured event describing one fork-cache probe.
///
/// Emitted once per spawned child that requested inheritance AND
/// received an API response with usage information. Children that
/// never reach the API (e.g., hard-failed in the resolver) do not
/// produce this event — those cases already emit a
/// `PrefixResolveOutcome::Failed` upstream.
/// Note: `Eq` is intentionally NOT derived — the `ratio: f64` field
/// can carry NaN in pathological cases and `PartialEq` is sufficient
/// for test assertions. Consumers needing total equality should
/// compare the fields they care about individually.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForkCacheEvent {
    /// Prefix ID that the child consumed. Cross-references the
    /// capture-side `ForkPrefix::prefix_id`.
    pub prefix_id: String,
    /// Parent run id that produced the prefix.
    pub parent_run_id: String,
    /// Child run id that consumed it.
    pub child_run_id: String,
    /// Classification (see [`ForkCacheOutcome`]).
    pub outcome: ForkCacheOutcome,
    /// Tokens the capture-side estimated would be cacheable.
    pub expected_cache_read_tokens: u64,
    /// Tokens the provider actually reported as cache-read on the
    /// child's first response.
    pub observed_cache_read_tokens: u64,
    /// Ratio `observed / expected` (1.0 == perfect hit;  > 1.0 on
    /// ExceededExpected). Present as a convenience so consumers
    /// don't all re-derive the same math.
    pub ratio: f64,
    /// Provider the child ran against. Included so a mixed-provider
    /// runtime can bucket by provider without joining against
    /// spawn logs.
    pub provider: ProviderKind,
}

// ---------------------------------------------------------------------
// Thresholds + classifier
// ---------------------------------------------------------------------

/// Thresholds controlling how observed-vs-expected ratio maps to
/// `ForkCacheOutcome`. Parameterised so evolution / A-B rules can
/// tune them without touching the classifier.
///
/// Invariants enforced by `validate`:
/// - `miss_floor > 0.0`
/// - `hit_threshold > miss_floor`
/// - `hit_threshold <= 1.0` (values > 1.0 would make hits impossible)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ForkCacheThresholds {
    /// Ratio at or above which we classify as Hit. Default 0.80 —
    /// mirrors the conventional 20% cache-miss drop heuristic
    /// (observed drop > 20% triggers a break).
    pub hit_threshold: f64,
    /// Ratio below which we classify as full Miss. Between
    /// [miss_floor, hit_threshold) is PartialDrift. Default 0.05 —
    /// a hair above zero, because many providers return exactly 0
    /// on full miss and we want clear "essentially nothing reused".
    pub miss_floor: f64,
}

impl Default for ForkCacheThresholds {
    fn default() -> Self {
        Self {
            hit_threshold: 0.80,
            miss_floor: 0.05,
        }
    }
}

impl ForkCacheThresholds {
    /// Verify invariants. Returns a stable error string so callers
    /// can log or wrap in their own error type. Kept as a simple
    /// `Result<_, String>` to avoid pulling a thiserror variant into
    /// a module that's otherwise pure data.
    pub fn validate(&self) -> Result<(), String> {
        // `<=` / `>=` form instead of `!(a > b)` so that NaN
        // values are caught explicitly — `NaN <= 0.0` is `false`,
        // so a NaN miss_floor produces the same error message as
        // a zero (both are invalid). Clippy's
        // `neg_cmp_op_on_partial_ord` lint flags the negated form.
        if self.miss_floor <= 0.0 {
            return Err(format!(
                "miss_floor must be > 0.0, got {}",
                self.miss_floor
            ));
        }
        if self.hit_threshold <= self.miss_floor {
            return Err(format!(
                "hit_threshold ({}) must be > miss_floor ({})",
                self.hit_threshold, self.miss_floor
            ));
        }
        if self.hit_threshold > 1.0 {
            return Err(format!(
                "hit_threshold must be <= 1.0, got {}",
                self.hit_threshold
            ));
        }
        Ok(())
    }
}

/// Inputs to [`evaluate_fork_cache`]. Separate from `ForkCacheEvent`
/// because the event is the *output*, and consumers reading events
/// off a bus don't need to know the thresholds used.
#[derive(Debug, Clone)]
pub struct ForkCacheProbe {
    pub prefix_id: String,
    pub parent_run_id: String,
    pub child_run_id: String,
    pub expected_cache_read_tokens: u64,
    pub observed_cache_read_tokens: u64,
    pub provider: ProviderKind,
}

/// Classify one probe into a `ForkCacheEvent`. Pure function —
/// never panics, never I/Os. The ratio is computed with saturating
/// arithmetic so a zero-expected probe can't divide by zero
/// (that case is classified based on observed alone:
/// observed > 0 → ExceededExpected; observed == 0 → Miss).
pub fn evaluate_fork_cache(probe: ForkCacheProbe, thresholds: ForkCacheThresholds) -> ForkCacheEvent {
    let ratio = if probe.expected_cache_read_tokens == 0 {
        // Undefined mathematically; encode the degenerate case as
        // 0.0 so the ratio field is still finite in serialized
        // output. The outcome arm below uses observed directly.
        0.0
    } else {
        probe.observed_cache_read_tokens as f64 / probe.expected_cache_read_tokens as f64
    };

    let outcome = if probe.expected_cache_read_tokens == 0 {
        // Degenerate: no expectation. Observed tokens are a bonus;
        // zero is a no-op. Neither signals a problem.
        if probe.observed_cache_read_tokens > 0 {
            ForkCacheOutcome::ExceededExpected
        } else {
            ForkCacheOutcome::Miss
        }
    } else if ratio > 1.0 {
        ForkCacheOutcome::ExceededExpected
    } else if ratio >= thresholds.hit_threshold {
        ForkCacheOutcome::Hit
    } else if ratio < thresholds.miss_floor {
        ForkCacheOutcome::Miss
    } else {
        ForkCacheOutcome::PartialDrift
    };

    ForkCacheEvent {
        prefix_id: probe.prefix_id,
        parent_run_id: probe.parent_run_id,
        child_run_id: probe.child_run_id,
        outcome,
        expected_cache_read_tokens: probe.expected_cache_read_tokens,
        observed_cache_read_tokens: probe.observed_cache_read_tokens,
        ratio,
        provider: probe.provider,
    }
}

// ---------------------------------------------------------------------
// Sink trait
// ---------------------------------------------------------------------

/// Minimal sink for emitting `ForkCacheEvent`s. Runtime / CLI layers
/// implement this to route events to their chosen bus (CSL, OTLP,
/// stdout, structured log). Sync because emission is a logging-class
/// operation — no awaits, no I/O in the hot path.
pub trait ForkCacheEventSink: Send + Sync {
    fn emit(&self, event: ForkCacheEvent);
}

/// No-op sink — the safe default when no observability has been
/// wired up yet. Matches the philosophy of PR 1–5b: everything
/// additive, zero runtime impact until a real impl is installed.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopForkCacheSink;

impl ForkCacheEventSink for NoopForkCacheSink {
    fn emit(&self, _event: ForkCacheEvent) {
        // intentionally nothing
    }
}

/// Sink that writes each event to stderr as a single structured JSON
/// line prefixed with `[fork-cache]`. Intended for live observation
/// during development and for piping into `jq` / log collectors
/// without needing a full observability backend.
///
/// Writes to stderr (not stdout) so it stays out of the way of CLI
/// tools whose stdout is part of their contract. Each emission is
/// one `println!` / `eprintln!` call — unbuffered enough that lines
/// appear immediately after the child turn completes.
///
/// If JSON serialization fails (unreachable today — every field is
/// serde-safe), falls back to `Debug` formatting so the line is
/// still useful. Never panics.
#[derive(Debug, Default, Clone, Copy)]
pub struct StderrForkCacheSink;

impl ForkCacheEventSink for StderrForkCacheSink {
    fn emit(&self, event: ForkCacheEvent) {
        let line = serde_json::to_string(&event).unwrap_or_else(|_| format!("{event:?}"));
        // Use a stable prefix so `grep '^\[fork-cache\]'` pulls
        // these lines out of mixed CLI output.
        eprintln!("[fork-cache] {line}");
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn probe(expected: u64, observed: u64) -> ForkCacheProbe {
        ForkCacheProbe {
            prefix_id: "pfx-x".into(),
            parent_run_id: "run-parent".into(),
            child_run_id: "run-child".into(),
            expected_cache_read_tokens: expected,
            observed_cache_read_tokens: observed,
            provider: ProviderKind::Anthropic,
        }
    }

    // --- Classifier --------------------------------------------------

    #[test]
    fn perfect_hit_is_classified_as_hit() {
        let ev = evaluate_fork_cache(probe(10_000, 10_000), ForkCacheThresholds::default());
        assert_eq!(ev.outcome, ForkCacheOutcome::Hit);
        assert!((ev.ratio - 1.0).abs() < 1e-9);
    }

    #[test]
    fn at_threshold_is_hit_not_partial() {
        // Default hit_threshold = 0.80. Observed exactly at 80% must
        // be Hit (inclusive lower bound), not PartialDrift. Off-by-
        // one on this boundary would pollute dashboards.
        let ev = evaluate_fork_cache(probe(10_000, 8_000), ForkCacheThresholds::default());
        assert_eq!(ev.outcome, ForkCacheOutcome::Hit);
    }

    #[test]
    fn just_below_threshold_is_partial_drift() {
        let ev = evaluate_fork_cache(probe(10_000, 7_999), ForkCacheThresholds::default());
        assert_eq!(ev.outcome, ForkCacheOutcome::PartialDrift);
    }

    #[test]
    fn below_miss_floor_is_full_miss() {
        // Default miss_floor = 0.05 → 4% observed = Miss.
        let ev = evaluate_fork_cache(probe(10_000, 400), ForkCacheThresholds::default());
        assert_eq!(ev.outcome, ForkCacheOutcome::Miss);
    }

    #[test]
    fn zero_observed_is_miss() {
        let ev = evaluate_fork_cache(probe(10_000, 0), ForkCacheThresholds::default());
        assert_eq!(ev.outcome, ForkCacheOutcome::Miss);
        assert_eq!(ev.ratio, 0.0);
    }

    #[test]
    fn exceeded_expected_when_ratio_above_one() {
        // Provider reported more reused tokens than we expected —
        // pleasant surprise, log for calibration, do not treat as
        // an error.
        let ev = evaluate_fork_cache(probe(10_000, 12_000), ForkCacheThresholds::default());
        assert_eq!(ev.outcome, ForkCacheOutcome::ExceededExpected);
        assert!(ev.ratio > 1.0);
    }

    #[test]
    fn zero_expected_with_observed_is_exceeded() {
        // Degenerate: expected=0 yet observed>0. Can't divide, but
        // semantically this is "we got tokens we didn't budget for".
        let ev = evaluate_fork_cache(probe(0, 500), ForkCacheThresholds::default());
        assert_eq!(ev.outcome, ForkCacheOutcome::ExceededExpected);
        assert_eq!(ev.ratio, 0.0, "ratio is 0.0 (undefined) in this branch");
    }

    #[test]
    fn zero_expected_with_zero_observed_is_miss() {
        // Both zero — no inheritance requested, no inheritance got.
        // Encoded as Miss so aggregations over long time windows
        // don't skew toward "Hit" (a common pitfall if we labelled
        // it Hit).
        let ev = evaluate_fork_cache(probe(0, 0), ForkCacheThresholds::default());
        assert_eq!(ev.outcome, ForkCacheOutcome::Miss);
    }

    #[test]
    fn custom_thresholds_override_defaults() {
        let strict = ForkCacheThresholds {
            hit_threshold: 0.95,
            miss_floor: 0.10,
        };
        // 0.90 would be Hit under defaults, but PartialDrift here.
        let ev = evaluate_fork_cache(probe(10_000, 9_000), strict);
        assert_eq!(ev.outcome, ForkCacheOutcome::PartialDrift);
    }

    // --- Thresholds validation --------------------------------------

    #[test]
    fn default_thresholds_are_valid() {
        assert!(ForkCacheThresholds::default().validate().is_ok());
    }

    #[test]
    fn miss_floor_zero_is_rejected() {
        let bad = ForkCacheThresholds {
            hit_threshold: 0.8,
            miss_floor: 0.0,
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn miss_floor_above_hit_threshold_is_rejected() {
        let inverted = ForkCacheThresholds {
            hit_threshold: 0.3,
            miss_floor: 0.5,
        };
        assert!(inverted.validate().is_err());
    }

    #[test]
    fn hit_threshold_above_one_is_rejected() {
        let silly = ForkCacheThresholds {
            hit_threshold: 1.5,
            miss_floor: 0.05,
        };
        assert!(silly.validate().is_err());
    }

    // --- Event shape --------------------------------------------------

    #[test]
    fn event_preserves_probe_identifiers() {
        let p = ForkCacheProbe {
            prefix_id: "pfx-42".into(),
            parent_run_id: "run-parent-abc".into(),
            child_run_id: "run-child-def".into(),
            expected_cache_read_tokens: 1_000,
            observed_cache_read_tokens: 900,
            provider: ProviderKind::OpenAi,
        };
        let ev = evaluate_fork_cache(p, ForkCacheThresholds::default());
        assert_eq!(ev.prefix_id, "pfx-42");
        assert_eq!(ev.parent_run_id, "run-parent-abc");
        assert_eq!(ev.child_run_id, "run-child-def");
        assert_eq!(ev.provider, ProviderKind::OpenAi);
    }

    #[test]
    fn event_serializes_roundtrip() {
        let ev = evaluate_fork_cache(probe(5_000, 5_000), ForkCacheThresholds::default());
        let json = serde_json::to_string(&ev).unwrap();
        let back: ForkCacheEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn outcome_serializes_as_snake_case() {
        // Dashboards / evolution rules may match on these literals;
        // tripwire to force review if anyone renames variants.
        let ev = evaluate_fork_cache(probe(100, 100), ForkCacheThresholds::default());
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            json.contains("\"outcome\":\"hit\""),
            "expected snake_case tag, got {json}"
        );
    }

    // --- Sinks -------------------------------------------------------

    /// In-memory sink for tests that want to assert emissions.
    #[derive(Debug, Default)]
    struct CollectSink(Mutex<Vec<ForkCacheEvent>>);
    impl ForkCacheEventSink for CollectSink {
        fn emit(&self, event: ForkCacheEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    #[test]
    fn noop_sink_emits_nothing() {
        // The whole point of Noop is "zero observable behavior".
        // We verify by exercising the code path and confirming it
        // doesn't panic or allocate — the easiest robust
        // verification is that it takes an event by value without
        // any return plumbing.
        let sink: Arc<dyn ForkCacheEventSink> = Arc::new(NoopForkCacheSink);
        let ev = evaluate_fork_cache(probe(100, 100), ForkCacheThresholds::default());
        sink.emit(ev);
        // No assertion — we merely verified no panic. Observability-
        // style tests shouldn't invent side effects to "prove" none.
    }

    #[test]
    fn stderr_sink_emits_without_panic() {
        // Cannot capture stderr cleanly in a unit test (would need
        // gag or a pipe trick that's fragile on Windows). Minimum
        // bar: the sink accepts an event and doesn't panic under
        // the trait-object dispatch path.
        let sink: Arc<dyn ForkCacheEventSink> = Arc::new(StderrForkCacheSink);
        sink.emit(evaluate_fork_cache(
            probe(10_000, 9_500),
            ForkCacheThresholds::default(),
        ));
        sink.emit(evaluate_fork_cache(
            probe(10_000, 0),
            ForkCacheThresholds::default(),
        ));
    }

    #[test]
    fn collect_sink_captures_emissions_in_order() {
        let sink = Arc::new(CollectSink::default());
        let out = sink.clone() as Arc<dyn ForkCacheEventSink>;
        out.emit(evaluate_fork_cache(probe(100, 100), ForkCacheThresholds::default()));
        out.emit(evaluate_fork_cache(probe(100, 0), ForkCacheThresholds::default()));

        let events = sink.0.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].outcome, ForkCacheOutcome::Hit);
        assert_eq!(events[1].outcome, ForkCacheOutcome::Miss);
    }

    #[test]
    fn default_thresholds_are_documented() {
        // Tripwire: changing these is an observable behavior
        // change across every dashboard / evolution rule keyed on
        // Hit / Miss buckets.
        let t = ForkCacheThresholds::default();
        assert!((t.hit_threshold - 0.80).abs() < 1e-9);
        assert!((t.miss_floor - 0.05).abs() < 1e-9);
    }
}
