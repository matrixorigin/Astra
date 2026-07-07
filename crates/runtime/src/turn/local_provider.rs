//! `LocalSessionProvider` — a single concrete struct that implements
//! [`LiveRuntimeProvider`], [`ObservationProvider`], and [`SessionStateProvider`]
//! by reading from an [`AgenticLoopState`] reference.
//!
//! # Safety
//!
//! All methods are panic-free. When underlying data is absent (empty journal,
//! missing task board, unlimited budget), they return sensible zero/defaults.
//!
//! # Usage
//!
//! ```ignore
//! let provider = LocalSessionProvider::new(&state);
//! let mut facts = provider.extract_facts();
//! facts.token_pressure = provider.token_pressure();
//! facts.task_completion_ratio = provider.task_completion_ratio();
//! let actions = policy.decide(&facts);
//! ```

use astra_core::observation_journal::{JournalFacts, MetricTrend};

use super::agentic_loop::host::{self, AgenticLoopState};
use super::providers::{LiveRuntimeProvider, ObservationProvider, SessionStateProvider};

// ─── LocalSessionProvider ────────────────────────────────────────────────────

/// Single-provider implementation that reads live and historical facts from an
/// in-memory [`AgenticLoopState`] reference.
pub struct LocalSessionProvider<'a> {
    state: &'a AgenticLoopState,
}

impl<'a> LocalSessionProvider<'a> {
    pub fn new(state: &'a AgenticLoopState) -> Self {
        Self { state }
    }
}

// ─── LiveRuntimeProvider impl ────────────────────────────────────────────────

impl LiveRuntimeProvider for LocalSessionProvider<'_> {
    fn token_pressure(&self) -> f64 {
        host::introspect_token_pressure(self.state)
    }

    fn cache_hit_ratio(&self) -> f64 {
        let total_in =
            self.state.total_prompt + self.state.total_cache_read + self.state.total_cache_creation;
        if total_in > 0 {
            self.state.total_cache_read as f64 / total_in as f64
        } else {
            0.0
        }
    }

    fn current_error_rate(&self) -> f64 {
        let errors = self.state.turn_guard.health.recent_errors(10);
        if errors.is_empty() {
            0.0
        } else {
            // Rate is relative to total tool calls for normalization;
            // fall back to a fixed window when total calls is zero.
            let total = self.state.stall.tool_call_records.len().max(1);
            (errors.len() as f64 / total as f64).min(1.0)
        }
    }

    fn budget_remaining(&self) -> u32 {
        self.state.remaining_turns as u32
    }

    fn budget_max(&self) -> u32 {
        self.state.max_turns as u32
    }
}

// ─── ObservationProvider impl ────────────────────────────────────────────────

impl ObservationProvider for LocalSessionProvider<'_> {
    fn extract_facts(&self) -> JournalFacts {
        self.state.observation_journal.extract_facts(
            self.state.remaining_turns as u32,
            self.state.max_turns as u32,
        )
    }

    fn compute_trends(&self) -> Vec<MetricTrend> {
        self.state.observation_journal.compute_trends()
    }

    fn journal_len(&self) -> usize {
        self.state.observation_journal.len()
    }

    fn journal_is_empty(&self) -> bool {
        self.state.observation_journal.is_empty()
    }
}

// ─── SessionStateProvider impl ───────────────────────────────────────────────

impl SessionStateProvider for LocalSessionProvider<'_> {
    fn task_completion_ratio(&self) -> f64 {
        let snapshot = &self.state.hooks.task_board_snapshot;
        if !snapshot.has_any_tracked_tasks() {
            return 0.0;
        }
        snapshot.completed_count as f64 / snapshot.tracked_count.max(1) as f64
    }

    fn current_phase_label(&self) -> &'static str {
        "execution"
    }

    fn circuit_breaker_state(&self) -> &'static str {
        self.state.stall.circuit_breaker.state().operator_label()
    }

    fn remaining_turns(&self) -> u32 {
        self.state.remaining_turns as u32
    }

    fn max_turns(&self) -> u32 {
        self.state.max_turns as u32
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::agentic_loop::host::AgenticLoopState;
    use crate::turn::runtime_policy::RuntimePolicy;
    use astra_core::observation_journal::ObservationJournal;

    fn make_state() -> AgenticLoopState {
        let mut state = host::make_test_loop_state();
        state.max_turn_input_tokens = 100_000;
        state
    }

    fn make_provider(state: &AgenticLoopState) -> LocalSessionProvider<'_> {
        LocalSessionProvider::new(state)
    }

    // ── LiveRuntimeProvider tests ───────────────────────────────────────

    #[test]
    fn live_provider_zero_defaults() {
        let mut state = host::make_test_loop_state();
        state.remaining_turns = 0;
        state.max_turns = 0;
        let p = make_provider(&state);
        assert_eq!(p.budget_remaining(), 0);
        assert_eq!(p.budget_max(), 0);
        assert!((p.cache_hit_ratio() - 0.0).abs() < f64::EPSILON);
        assert!((p.current_error_rate() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn live_provider_unlimited_budget_has_zero_pressure() {
        // max_turn_input_tokens == 0 → unlimited → token_pressure == 0.0
        let mut state = host::make_test_loop_state();
        state.max_turn_input_tokens = 0;
        state.max_turns = 0;
        state.remaining_turns = 0;
        let p = make_provider(&state);
        assert!((p.token_pressure() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn live_provider_cache_hit_ratio_with_data() {
        let mut state = make_state();
        state.total_cache_read = 750;
        state.total_prompt = 200;
        state.total_cache_creation = 50;
        // total_in = 1000, cache ratio = 750/1000 = 0.75
        let p = make_provider(&state);
        assert!((p.cache_hit_ratio() - 0.75).abs() < 0.001);
    }

    #[test]
    fn live_provider_cache_hit_ratio_zero_when_no_tokens() {
        let mut state = make_state();
        state.total_cache_read = 0;
        state.total_prompt = 0;
        state.total_cache_creation = 0;
        let p = make_provider(&state);
        assert!((p.cache_hit_ratio() - 0.0).abs() < f64::EPSILON);
    }

    // ── ObservationProvider tests ───────────────────────────────────────

    #[test]
    fn observation_provider_empty_journal() {
        let state = make_state();
        let p = make_provider(&state);
        assert!(p.journal_is_empty());
        assert_eq!(p.journal_len(), 0);
        let facts = p.extract_facts();
        // All streaks should be zero for empty journal
        assert_eq!(facts.streaks.consecutive_rounds_with_outcome, 0);
        assert_eq!(facts.streaks.consecutive_rounds_without_outcome, 0);
    }

    #[test]
    fn observation_provider_with_entries() {
        let mut state = make_state();
        // Record a successful turn to populate the journal
        let metrics = astra_core::observation::TurnMetrics {
            rounds_completed: 1,
            tool_calls_total: 5,
            mutation_count: 2,
            error_count: 0,
            cache_hits: 3,
            tokens_consumed: 1500,
            ..Default::default()
        };
        state.observation_journal.record_turn(&metrics);
        let p = make_provider(&state);
        assert!(!p.journal_is_empty());
        assert_eq!(p.journal_len(), 1);
    }

    // ── SessionStateProvider tests ──────────────────────────────────────

    #[test]
    fn session_provider_empty_board_is_unknown_not_complete() {
        let state = make_state();
        let p = make_provider(&state);
        assert!((p.task_completion_ratio() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn session_provider_completed_board_is_complete() {
        let mut state = make_state();
        state.hooks.task_board_snapshot.tracked_count = 2;
        state.hooks.task_board_snapshot.completed_count = 2;
        let p = make_provider(&state);
        assert!((p.task_completion_ratio() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn session_provider_phase_and_turns() {
        let state = make_state();
        let p = make_provider(&state);
        assert_eq!(p.current_phase_label(), "execution");
        assert_eq!(p.remaining_turns(), 10);
        assert_eq!(p.max_turns(), 10);
    }

    #[test]
    fn session_provider_circuit_breaker_monitoring() {
        let state = make_state();
        let p = make_provider(&state);
        // Default circuit breaker is passively monitoring normal operation.
        assert_eq!(p.circuit_breaker_state(), "monitoring");
    }

    // ── Composition test ────────────────────────────────────────────────

    #[test]
    fn provider_composition_smoke() {
        let state = make_state();
        let p = make_provider(&state);
        // All three traits work together without conflict
        let _pressure = p.token_pressure();
        let _facts = p.extract_facts();
        let _ratio = p.task_completion_ratio();
        let _trends = p.compute_trends();
        let _cache = p.cache_hit_ratio();
        let _errors = p.current_error_rate();
    }
}
