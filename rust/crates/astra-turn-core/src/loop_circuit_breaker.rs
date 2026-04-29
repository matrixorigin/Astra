//! Anomaly-based circuit breaker for the agentic loop.
//!
//! Design philosophy: agent runs **unlimited by default**. The circuit breaker
//! fires only when it detects stall (no progress) or regression (getting worse).
//! Well-performing agents never see any intervention.
//!
//! State machine:
//!   Closed (normal) → Open (stall detected, inject correction) → HalfOpen
//!   (one more chance after correction) → terminal abort if still stalled.

use std::collections::BTreeSet;

/// Anomaly signals the circuit breaker observes each round.
#[derive(Debug, Clone, PartialEq)]
pub struct RoundSignal {
    /// Tool call signatures this round (tool_name:canonical_args).
    pub tool_signatures: BTreeSet<String>,
    /// Whether this round produced a mutation (file write, shell with side effects).
    pub produced_mutation: bool,
    /// Number of tool calls this round.
    pub tool_count: usize,
}

/// Circuit breaker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Normal operation — no intervention.
    Closed,
    /// Stall/regression detected — inject correction message.
    Open,
    /// Correction was injected, observing whether agent recovers.
    HalfOpen,
}

/// Action the loop should take after consulting the circuit breaker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakerAction {
    /// Continue normally — no intervention needed.
    Continue,
    /// Inject a correction message (stall detected).
    InjectCorrection,
    /// Hard abort — agent did not recover after correction.
    Abort,
}

/// Configuration for the loop circuit breaker.
#[derive(Debug, Clone)]
pub struct BreakerConfig {
    /// Consecutive stall rounds before tripping (Closed → Open).
    pub stall_threshold: usize,
    /// Consecutive identical-signature rounds before tripping.
    pub repetition_threshold: usize,
    /// Consecutive read-only rounds (tool_count > 0, no mutation) before tripping,
    /// regardless of whether tool signatures are novel. Catches "creative but
    /// unproductive exploration" where the agent uses different grep patterns
    /// each round without ever writing.
    pub read_only_stall_threshold: usize,
    /// Max rounds in HalfOpen before aborting.
    pub half_open_patience: usize,
    /// Hard ceiling (pure infrastructure guard, prevents infinite loops from bugs).
    pub absolute_max_rounds: usize,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            stall_threshold: 3,
            repetition_threshold: 3,
            read_only_stall_threshold: 8,
            half_open_patience: 2,
            absolute_max_rounds: 200,
        }
    }
}

/// The loop circuit breaker. Created per-turn, observes round signals,
/// and decides when to intervene.
#[derive(Debug)]
pub struct LoopCircuitBreaker {
    config: BreakerConfig,
    state: BreakerState,
    /// All round signals observed so far.
    rounds: Vec<RoundSignal>,
    /// How many rounds spent in HalfOpen since last trip.
    half_open_rounds: usize,
}

impl LoopCircuitBreaker {
    pub fn new(config: BreakerConfig) -> Self {
        Self {
            config,
            state: BreakerState::Closed,
            rounds: Vec::new(),
            half_open_rounds: 0,
        }
    }

    /// Create with default config.
    pub fn with_defaults() -> Self {
        Self::new(BreakerConfig::default())
    }

    /// Current state.
    pub fn state(&self) -> BreakerState {
        self.state
    }

    /// Total rounds observed.
    pub fn rounds_completed(&self) -> usize {
        self.rounds.len()
    }

    /// Record a round and get the action to take.
    pub fn observe(&mut self, signal: RoundSignal) -> BreakerAction {
        self.rounds.push(signal);

        // Infrastructure hard ceiling — always abort regardless of state.
        if self.rounds.len() >= self.config.absolute_max_rounds {
            self.state = BreakerState::Open;
            return BreakerAction::Abort;
        }

        match self.state {
            BreakerState::Closed => self.evaluate_closed(),
            BreakerState::HalfOpen => self.evaluate_half_open(),
            BreakerState::Open => {
                // Idempotent: if observe() is called while still Open (caller
                // hasn't called correction_injected() yet), repeat the
                // InjectCorrection signal rather than escalating to Abort.
                BreakerAction::InjectCorrection
            }
        }
    }

    /// Acknowledge that a correction was injected. Transitions Open → HalfOpen.
    pub fn correction_injected(&mut self) {
        if self.state == BreakerState::Open {
            self.state = BreakerState::HalfOpen;
            self.half_open_rounds = 0;
        }
    }

    fn evaluate_closed(&mut self) -> BreakerAction {
        if self.detect_repetition_stall()
            || self.detect_no_progress_stall()
            || self.detect_prolonged_read_only_stall()
        {
            self.state = BreakerState::Open;
            BreakerAction::InjectCorrection
        } else {
            BreakerAction::Continue
        }
    }

    fn evaluate_half_open(&mut self) -> BreakerAction {
        self.half_open_rounds += 1;
        let latest = &self.rounds[self.rounds.len() - 1];

        // Recovery: agent produced a mutation or broke ALL stall patterns.
        let still_stalling = self.detect_repetition_stall()
            || self.detect_no_progress_stall()
            || self.detect_prolonged_read_only_stall();
        if latest.produced_mutation || !still_stalling {
            self.state = BreakerState::Closed;
            self.half_open_rounds = 0;
            BreakerAction::Continue
        } else if self.half_open_rounds >= self.config.half_open_patience {
            BreakerAction::Abort
        } else {
            BreakerAction::Continue
        }
    }

    /// Detect N consecutive rounds with identical tool signatures.
    fn detect_repetition_stall(&self) -> bool {
        let n = self.config.repetition_threshold;
        if self.rounds.len() < n {
            return false;
        }
        let tail = &self.rounds[self.rounds.len() - n..];
        let reference = &tail[0].tool_signatures;
        // All N rounds have the same signature set and it's non-empty.
        !reference.is_empty() && tail.iter().all(|r| &r.tool_signatures == reference)
    }

    /// Detect N consecutive rounds with no mutations and no new tool patterns.
    fn detect_no_progress_stall(&self) -> bool {
        let n = self.config.stall_threshold;
        if self.rounds.len() < n {
            return false;
        }
        let tail = &self.rounds[self.rounds.len() - n..];
        // All recent rounds: no mutations and zero tool calls (empty rounds)
        // OR all exploration-only (no mutations).
        tail.iter()
            .all(|r| !r.produced_mutation && r.tool_count > 0)
            && self.no_new_patterns_in_tail(n)
    }

    /// Detect prolonged read-only exploration: N consecutive rounds with
    /// tool_count > 0 but no mutation, regardless of signature novelty.
    /// This catches "creative but unproductive" loops where the agent uses
    /// different grep/git_show patterns each round without ever writing.
    fn detect_prolonged_read_only_stall(&self) -> bool {
        let n = self.config.read_only_stall_threshold;
        if n == 0 || self.rounds.len() < n {
            return false;
        }
        let tail = &self.rounds[self.rounds.len() - n..];
        tail.iter()
            .all(|r| !r.produced_mutation && r.tool_count > 0)
    }

    /// Check if the last N rounds introduced any new tool signature not seen
    /// in the preceding window. Uses a sliding window of 2*N rounds before
    /// the tail (not the entire history) to prevent the prior set from growing
    /// unbounded after broad exploration.
    fn no_new_patterns_in_tail(&self, n: usize) -> bool {
        let split = self.rounds.len() - n;
        // Look back at most 2*N rounds before the tail as the comparison window.
        let window_start = split.saturating_sub(2 * n);
        let prior_sigs: BTreeSet<&String> = self.rounds[window_start..split]
            .iter()
            .flat_map(|r| r.tool_signatures.iter())
            .collect();
        // If prior window is empty (very early in the session), can't determine staleness.
        if prior_sigs.is_empty() {
            return false;
        }
        let tail_sigs: BTreeSet<&String> = self.rounds[split..]
            .iter()
            .flat_map(|r| r.tool_signatures.iter())
            .collect();
        // If tail introduced zero new signatures vs the recent window, it's stale.
        tail_sigs.is_subset(&prior_sigs)
    }
}

impl Default for LoopCircuitBreaker {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(tools: &[&str]) -> BTreeSet<String> {
        tools.iter().map(|s| s.to_string()).collect()
    }

    fn signal(tools: &[&str], mutation: bool) -> RoundSignal {
        RoundSignal {
            tool_signatures: sig(tools),
            produced_mutation: mutation,
            tool_count: tools.len(),
        }
    }

    // ─── Normal operation: no intervention ───────────────────────────────

    #[test]
    fn normal_progress_never_trips() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig::default());
        // 50 rounds of diverse productive work.
        for i in 0..50 {
            let action = cb.observe(signal(
                &[
                    &format!("read_file:file{i}.rs"),
                    &format!("write_file:file{i}.rs"),
                ],
                true,
            ));
            assert_eq!(action, BreakerAction::Continue);
            assert_eq!(cb.state(), BreakerState::Closed);
        }
    }

    #[test]
    fn exploration_with_eventual_mutation_does_not_trip() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig::default());
        // 2 exploration rounds (below threshold of 3).
        assert_eq!(
            cb.observe(signal(&["read_file:a.rs"], false)),
            BreakerAction::Continue
        );
        assert_eq!(
            cb.observe(signal(&["grep:pattern"], false)),
            BreakerAction::Continue
        );
        // Then a productive round.
        assert_eq!(
            cb.observe(signal(&["write_file:a.rs"], true)),
            BreakerAction::Continue
        );
        assert_eq!(cb.state(), BreakerState::Closed);
    }

    #[test]
    fn varied_exploration_trips_at_read_only_threshold() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig::default());
        // 7 exploration rounds (below read_only_stall_threshold of 8).
        for i in 0..7 {
            assert_eq!(
                cb.observe(signal(&[&format!("read_file:file{i}.rs")], false)),
                BreakerAction::Continue
            );
        }
        // Round 8 hits the read-only stall threshold.
        assert_eq!(
            cb.observe(signal(&["read_file:file7.rs"], false)),
            BreakerAction::InjectCorrection
        );
        assert_eq!(cb.state(), BreakerState::Open);
    }

    // ─── Repetition stall detection ─────────────────────────────────────

    #[test]
    fn repetition_stall_trips_at_threshold() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig::default());
        // 3 identical rounds → trip.
        assert_eq!(
            cb.observe(signal(&["read_file:same.rs"], false)),
            BreakerAction::Continue
        );
        assert_eq!(
            cb.observe(signal(&["read_file:same.rs"], false)),
            BreakerAction::Continue
        );
        assert_eq!(
            cb.observe(signal(&["read_file:same.rs"], false)),
            BreakerAction::InjectCorrection
        );
        assert_eq!(cb.state(), BreakerState::Open);
    }

    #[test]
    fn repetition_stall_does_not_trip_below_threshold() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig::default());
        assert_eq!(
            cb.observe(signal(&["read_file:same.rs"], false)),
            BreakerAction::Continue
        );
        assert_eq!(
            cb.observe(signal(&["read_file:same.rs"], false)),
            BreakerAction::Continue
        );
        // Different tool breaks the streak.
        assert_eq!(
            cb.observe(signal(&["grep:something"], false)),
            BreakerAction::Continue
        );
        assert_eq!(cb.state(), BreakerState::Closed);
    }

    // ─── No-progress stall detection ────────────────────────────────────

    #[test]
    fn no_progress_stall_trips_when_stale_patterns() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig::default());
        // First round establishes known patterns.
        assert_eq!(
            cb.observe(signal(&["read_file:a.rs"], false)),
            BreakerAction::Continue
        );
        // Rounds 2-3 repeat same pattern (below repetition_threshold of 3).
        assert_eq!(
            cb.observe(signal(&["read_file:a.rs"], false)),
            BreakerAction::Continue
        );
        // Round 3 is the 3rd consecutive identical → repetition stall trips.
        assert_eq!(
            cb.observe(signal(&["read_file:a.rs"], false)),
            BreakerAction::InjectCorrection
        );
    }

    #[test]
    fn no_progress_stall_varied_but_stale_patterns() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig::default());
        // Establish known patterns in early rounds.
        assert_eq!(
            cb.observe(signal(&["read_file:a.rs"], false)),
            BreakerAction::Continue
        );
        assert_eq!(
            cb.observe(signal(&["grep:pattern"], false)),
            BreakerAction::Continue
        );
        assert_eq!(
            cb.observe(signal(&["read_file:b.rs"], false)),
            BreakerAction::Continue
        );
        // Now cycle through already-seen patterns with no mutation.
        // These are not repetition (different each round) but no new patterns.
        assert_eq!(
            cb.observe(signal(&["read_file:a.rs"], false)),
            BreakerAction::Continue
        );
        assert_eq!(
            cb.observe(signal(&["grep:pattern"], false)),
            BreakerAction::Continue
        );
        assert_eq!(
            cb.observe(signal(&["read_file:b.rs"], false)),
            BreakerAction::InjectCorrection
        );
    }

    // ─── Recovery after correction ──────────────────────────────────────

    #[test]
    fn recovery_after_correction_returns_to_closed() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig::default());
        // Trip the breaker.
        cb.observe(signal(&["read_file:x"], false));
        cb.observe(signal(&["read_file:x"], false));
        cb.observe(signal(&["read_file:x"], false));
        assert_eq!(cb.state(), BreakerState::Open);

        // Correction injected.
        cb.correction_injected();
        assert_eq!(cb.state(), BreakerState::HalfOpen);

        // Agent recovers with a mutation.
        let action = cb.observe(signal(&["write_file:x"], true));
        assert_eq!(action, BreakerAction::Continue);
        assert_eq!(cb.state(), BreakerState::Closed);
    }

    #[test]
    fn abort_if_no_recovery_after_correction() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig {
            half_open_patience: 2,
            ..Default::default()
        });
        // Trip.
        cb.observe(signal(&["read_file:x"], false));
        cb.observe(signal(&["read_file:x"], false));
        cb.observe(signal(&["read_file:x"], false));
        cb.correction_injected();

        // Still stalling in HalfOpen.
        assert_eq!(
            cb.observe(signal(&["read_file:x"], false)),
            BreakerAction::Continue
        );
        assert_eq!(
            cb.observe(signal(&["read_file:x"], false)),
            BreakerAction::Abort
        );
    }

    // ─── Absolute max rounds (infrastructure guard) ─────────────────────

    #[test]
    fn absolute_max_rounds_aborts_even_if_healthy() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig {
            absolute_max_rounds: 5,
            ..Default::default()
        });
        for i in 0..4 {
            assert_eq!(
                cb.observe(signal(&[&format!("write_file:f{i}")], true)),
                BreakerAction::Continue
            );
        }
        // Round 5 hits the ceiling.
        assert_eq!(
            cb.observe(signal(&["write_file:f5"], true)),
            BreakerAction::Abort
        );
    }

    // ─── Edge cases ─────────────────────────────────────────────────────

    #[test]
    fn empty_tool_calls_do_not_count_as_repetition() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig::default());
        let empty = RoundSignal {
            tool_signatures: BTreeSet::new(),
            produced_mutation: false,
            tool_count: 0,
        };
        // Empty rounds should not trigger repetition (they're text-only responses).
        for _ in 0..5 {
            assert_eq!(cb.observe(empty.clone()), BreakerAction::Continue);
        }
    }

    #[test]
    fn mutation_resets_stall_window() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig::default());
        // 2 stale rounds.
        cb.observe(signal(&["read_file:a"], false));
        cb.observe(signal(&["read_file:a"], false));
        // Mutation breaks the pattern.
        cb.observe(signal(&["write_file:a"], true));
        // 2 more stale rounds — should not trip because window restarted.
        assert_eq!(
            cb.observe(signal(&["read_file:a"], false)),
            BreakerAction::Continue
        );
        assert_eq!(
            cb.observe(signal(&["read_file:a"], false)),
            BreakerAction::Continue
        );
        // Only on the 3rd consecutive does it trip again.
        assert_eq!(
            cb.observe(signal(&["read_file:a"], false)),
            BreakerAction::InjectCorrection
        );
    }

    #[test]
    fn half_open_recovery_with_new_pattern_not_mutation() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig::default());
        // Trip.
        cb.observe(signal(&["read_file:x"], false));
        cb.observe(signal(&["read_file:x"], false));
        cb.observe(signal(&["read_file:x"], false));
        cb.correction_injected();

        // Agent changes pattern (different tool) but no mutation — still recovery.
        let action = cb.observe(signal(&["grep:new_pattern"], false));
        assert_eq!(action, BreakerAction::Continue);
        assert_eq!(cb.state(), BreakerState::Closed);
    }

    // ─── Prolonged read-only stall detection ────────────────────────────

    #[test]
    fn read_only_stall_does_not_trip_below_threshold() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig::default());
        // 7 rounds of unique read-only exploration (threshold is 8).
        for i in 0..7 {
            assert_eq!(
                cb.observe(signal(&[&format!("grep:pattern{i}")], false)),
                BreakerAction::Continue
            );
        }
        assert_eq!(cb.state(), BreakerState::Closed);
    }

    #[test]
    fn read_only_stall_mutation_resets_streak() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig::default());
        // 6 read-only rounds.
        for i in 0..6 {
            cb.observe(signal(&[&format!("grep:p{i}")], false));
        }
        // Mutation resets the streak.
        cb.observe(signal(&["write_file:fix.rs"], true));
        // 7 more read-only rounds (below threshold again).
        for i in 0..7 {
            assert_eq!(
                cb.observe(signal(&[&format!("grep:q{i}")], false)),
                BreakerAction::Continue
            );
        }
        // Round 8 after mutation trips.
        assert_eq!(
            cb.observe(signal(&["grep:q7"], false)),
            BreakerAction::InjectCorrection
        );
    }

    #[test]
    fn read_only_stall_empty_rounds_do_not_count() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig::default());
        // Interleave read-only rounds with empty (text-only) rounds.
        for i in 0..20 {
            if i % 2 == 0 {
                cb.observe(signal(&[&format!("grep:p{i}")], false));
            } else {
                // Empty round (tool_count=0) breaks the read-only streak.
                cb.observe(RoundSignal {
                    tool_signatures: BTreeSet::new(),
                    produced_mutation: false,
                    tool_count: 0,
                });
            }
        }
        // Never trips because empty rounds break the consecutive streak.
        assert_eq!(cb.state(), BreakerState::Closed);
    }

    #[test]
    fn read_only_stall_disabled_when_threshold_zero() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig {
            read_only_stall_threshold: 0,
            ..Default::default()
        });
        // 50 read-only rounds — never trips because threshold is disabled.
        for i in 0..50 {
            let action = cb.observe(signal(&[&format!("grep:p{i}")], false));
            assert_eq!(action, BreakerAction::Continue);
        }
    }
}
