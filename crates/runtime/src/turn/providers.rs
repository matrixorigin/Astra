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
//! | `LiveRuntimeProvider` | Real-time token pressure, prompt-cache read share, error rate, budget |
//! | `SessionStateProvider` | Phase, circuit breaker, round budget |
//!
//! # Unhappy-path guarantees
//!
//! Every method must be panic-free. Missing observations retain their
//! declared zero/default values. Ratios without a measured
//! denominator return `None`, not a fabricated zero.

// ─── LiveRuntimeProvider ─────────────────────────────────────────────────────

/// Real-time metrics from the running turn loop.
pub trait LiveRuntimeProvider: Send + Sync {
    /// Token pressure in the current context window, 0.0–1.0.
    /// Returns 0.0 when the budget is unlimited (max_turns == 0).
    fn token_pressure(&self) -> f64;

    /// Ratio of prompt-cache reads to total input tokens for the current live
    /// runtime snapshot, 0.0–1.0. This is not a durable session aggregate.
    /// Returns `None` when no input denominator has been observed.
    fn cache_hit_ratio(&self) -> Option<f64>;

    /// Error rate across recent tool calls, 0.0–1.0.
    /// Returns 0.0 when no tool records exist.
    fn current_error_rate(&self) -> f64;

    /// Rounds remaining before the circuit breaker trips.
    fn budget_remaining(&self) -> u32;

    /// Maximum round budget allocated for this turn.
    fn budget_max(&self) -> u32;
}

// ─── SessionStateProvider ────────────────────────────────────────────────────

/// Session-level state: phase, circuit breaker, and turn budget.
pub trait SessionStateProvider: Send + Sync {
    /// Human-readable label for the current turn phase.
    fn current_phase_label(&self) -> &'static str;

    /// Circuit breaker state as a lowercase string: "monitoring", "tripped", or "recovering".
    fn circuit_breaker_state(&self) -> &'static str;

    /// Rounds remaining in the turn budget.
    fn remaining_turns(&self) -> u32;

    /// Maximum rounds for this turn.
    fn max_turns(&self) -> u32;
}
