//! Provider traits for the Observation Plane.
//!
//! These traits abstract the runtime state behind read-only interfaces so
//! that `execution_phase`, `introspect`, and `reflect` can access facts
//! through a unified surface without reaching into `AgenticLoopState`
//! fields directly.
//!
//! # Design
//!
//! | Trait | Responsibility |
//! |-------|----------------|
//! | `LiveRuntimeProvider` | Real-time token pressure, cache hit ratio, error rate, budget |
//! | `ObservationProvider` | Journal facts, trends, entry counts |
//! | `SessionStateProvider` | Task board completion, phase, circuit breaker |
//!
//! # Unhappy-path guarantees
//!
//! Every method must be panic-free. When underlying data is absent (empty
//! journal, missing task board, unlimited budget), methods return sensible
//! zero/default values rather than panicking or returning `Option`.

use astra_core::observation_journal::{JournalFacts, MetricTrend};

// ─── LiveRuntimeProvider ─────────────────────────────────────────────────────

/// Real-time metrics from the running turn loop.
pub trait LiveRuntimeProvider {
    /// Token pressure in the current context window, 0.0–1.0.
    /// Returns 0.0 when the budget is unlimited (max_turns == 0).
    fn token_pressure(&self) -> f64;

    /// Ratio of cache reads to total input tokens, 0.0–1.0.
    /// Returns 0.0 when no input tokens have been consumed.
    fn cache_hit_ratio(&self) -> f64;

    /// Error rate across recent tool calls, 0.0–1.0.
    /// Returns 0.0 when no tool records exist.
    fn current_error_rate(&self) -> f64;

    /// Rounds remaining before the circuit breaker trips.
    fn budget_remaining(&self) -> u32;

    /// Maximum round budget allocated for this turn.
    fn budget_max(&self) -> u32;
}

// ─── ObservationProvider ─────────────────────────────────────────────────────

/// Historical observation data from the turn journal.
pub trait ObservationProvider {
    /// Extract a factual snapshot from the journal.
    ///
    /// The returned `JournalFacts` includes outcome streaks, budget data,
    /// and read-only streaks. **Does not** include live metrics (cache
    /// pressure, error rate, task completion) — those come from the other
    /// provider traits.
    fn extract_facts(&self) -> JournalFacts;

    /// Compute metric trends across the journal's ring buffer.
    fn compute_trends(&self) -> Vec<MetricTrend>;

    /// Number of entries in the journal ring buffer.
    fn journal_len(&self) -> usize;

    /// Whether the journal has any recorded turns.
    fn journal_is_empty(&self) -> bool;
}

// ─── SessionStateProvider ────────────────────────────────────────────────────

/// Session-level state: task progress, phase, and circuit breaker.
pub trait SessionStateProvider {
    /// Ratio of completed tasks, 0.0–1.0.
    ///
    /// Returns 1.0 when no unfinished tasks remain on the board.
    /// Returns 0.0 when the task board snapshot is unavailable or empty.
    fn task_completion_ratio(&self) -> f64;

    /// Human-readable label for the current turn phase.
    fn current_phase_label(&self) -> &'static str;

    /// Circuit breaker state as a lowercase string: "armed", "tripped", or "disabled".
    fn circuit_breaker_state(&self) -> &'static str;

    /// Rounds remaining in the turn budget.
    fn remaining_turns(&self) -> u32;

    /// Maximum rounds for this turn.
    fn max_turns(&self) -> u32;
}
