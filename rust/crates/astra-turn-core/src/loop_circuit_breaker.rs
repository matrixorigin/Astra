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
///
/// Marked `#[non_exhaustive]` so future soft-intervention variants can be
/// added without breaking downstream exhaustive matches.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BreakerAction {
    /// Continue normally — no intervention needed.
    Continue,
    /// Periodic self-reflection prompt. Injected every N consecutive read-only
    /// rounds. Tools remain enabled — the model decides whether to continue.
    Introspect { consecutive_read_only: usize },
    /// Inject a correction message (stall detected, tools disabled next round).
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
    ///
    /// Note: this detector is progress-aware — if every round in the window
    /// introduces at least one novel tool signature not seen in prior rounds,
    /// the agent is considered to be making progress and the threshold is not
    /// applied. This prevents false positives on inherently read-only tasks
    /// like code review.
    pub read_only_stall_threshold: usize,
    /// Max number of Introspect soft-signals emitted per read-only streak
    /// (since the last mutation) before falling back to Continue. Prevents
    /// unbounded self-check prompts on genuinely long read-only sessions
    /// (e.g. large code review reading 100+ files). After this cap,
    /// `absolute_max_rounds` is the only remaining backstop.
    ///
    /// Semantics:
    /// - `0` → **unbounded** (no cap; introspect signals are emitted at every
    ///   `read_only_stall_threshold` multiple for the entire session).
    /// - `n > 0` → emit at most `n` Introspect signals per read-only streak;
    ///   once exhausted the breaker falls back to `Continue` until the next
    ///   mutation resets the counter.
    ///
    /// **Sentinel**: `0` here means **unbounded** (no cap on introspect emissions).
    ///
    /// **Note**: if this config is populated from `ToolSelectionConfig`
    /// (via the CLI adapter), `circuit_breaker_max_introspect_emissions = 0`
    /// in the user config means "use default (3)", NOT unbounded — the
    /// `0 = unbounded` sentinel only applies directly to `BreakerConfig`.
    /// Use a very large explicit value (e.g. `1000`) in user config to
    /// approximate unbounded behavior.
    ///
    /// **Warning**: do NOT construct `BreakerConfig` directly from a raw
    /// `circuit_breaker_max_introspect_emissions` user-config value without
    /// going through `effective_circuit_breaker_max_introspect_emissions()` first,
    /// as that would turn a user's "use default" (0) into "unbounded".
    pub max_introspect_emissions: usize,
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
            read_only_stall_threshold: 12,
            max_introspect_emissions: 3,
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
    /// Consecutive read-only rounds since last mutation (for periodic introspection).
    consecutive_read_only: usize,
    /// Number of Introspect soft-signals emitted since the last write (mutation).
    /// Resets to 0 whenever `produced_mutation` is true, giving each new
    /// read-only streak a fresh budget of self-check prompts.
    introspect_emissions_since_last_write: usize,
}

impl LoopCircuitBreaker {
    pub fn new(config: BreakerConfig) -> Self {
        Self {
            config,
            state: BreakerState::Closed,
            rounds: Vec::new(),
            half_open_rounds: 0,
            consecutive_read_only: 0,
            introspect_emissions_since_last_write: 0,
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

    /// Consecutive read-only rounds since the last reset point.
    pub fn consecutive_read_only(&self) -> usize {
        self.consecutive_read_only
    }

    /// Number of Introspect soft-signals emitted since the last write (mutation).
    pub fn introspect_emissions(&self) -> usize {
        self.introspect_emissions_since_last_write
    }

    /// Record a round and get the action to take.
    pub fn observe(&mut self, signal: RoundSignal) -> BreakerAction {
        // Track consecutive read-only streak for periodic introspection.
        if signal.produced_mutation || signal.tool_count == 0 {
            self.consecutive_read_only = 0;
            // A mutation (or pure answer round) means the agent is making
            // progress — reset the introspect emission counter so a later
            // read-only streak gets a fresh budget of self-check prompts.
            if signal.produced_mutation {
                self.introspect_emissions_since_last_write = 0;
            }
        } else {
            self.consecutive_read_only += 1;
        }

        self.rounds.push(signal);

        // Infrastructure hard ceiling — always abort regardless of state.
        if self.rounds.len() >= self.config.absolute_max_rounds {
            self.state = BreakerState::Open;
            return BreakerAction::Abort;
        }

        match self.state {
            BreakerState::Closed => {
                let action = self.evaluate_closed();
                if action != BreakerAction::Continue {
                    return action;
                }
                // Periodic introspection: every read_only_stall_threshold rounds
                // of consecutive read-only work, prompt self-reflection.
                // Capped by `max_introspect_emissions` to avoid unbounded
                // self-check prompts on genuinely long read-only sessions.
                let n = self.config.read_only_stall_threshold;
                let cap = self.config.max_introspect_emissions;
                let under_cap = cap == 0 || self.introspect_emissions_since_last_write < cap;
                if n > 0
                    && self.consecutive_read_only > 0
                    && self.consecutive_read_only.is_multiple_of(n)
                    && under_cap
                {
                    self.introspect_emissions_since_last_write += 1;
                    BreakerAction::Introspect {
                        consecutive_read_only: self.consecutive_read_only,
                    }
                } else {
                    BreakerAction::Continue
                }
            }
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
            self.consecutive_read_only = 0;
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
    /// tool_count > 0 but no mutation.
    ///
    /// Progress-aware: if every round in the tail introduces at least one
    /// novel tool signature not seen in any prior round of the tail, the
    /// agent is making genuine progress (e.g., reading different files for
    /// code review) and this detector does NOT fire.
    fn detect_prolonged_read_only_stall(&self) -> bool {
        let n = self.config.read_only_stall_threshold;
        if n == 0 || self.rounds.len() < n {
            return false;
        }
        let tail = &self.rounds[self.rounds.len() - n..];
        // All rounds must be read-only with tool calls.
        if !tail
            .iter()
            .all(|r| !r.produced_mutation && r.tool_count > 0)
        {
            return false;
        }
        // Progress check: does each round introduce at least one novel signature?
        // If yes, the agent is making progress — don't trip.
        let mut seen: BTreeSet<&String> = BTreeSet::new();
        let mut all_novel = true;
        for round in tail {
            let has_novel = round.tool_signatures.iter().any(|s| !seen.contains(s));
            if !has_novel {
                all_novel = false;
                break;
            }
            seen.extend(round.tool_signatures.iter());
        }
        // If every round had novel signatures, agent is progressing — no stall.
        !all_novel
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
        let mut cb = LoopCircuitBreaker::new(BreakerConfig {
            read_only_stall_threshold: 6,
            stall_threshold: 100, // disable no-progress stall for this test
            ..Default::default()
        });
        // Use a small set of signatures that repeat — simulates churn (not progress).
        let patterns = ["grep:a", "grep:b", "grep:c"];
        // Rounds 1-5: continue (below threshold of 6)
        for i in 0..5 {
            assert_eq!(
                cb.observe(signal(&[patterns[i % 3]], false)),
                BreakerAction::Continue,
                "round {i} should continue"
            );
        }
        // Round 6 hits the threshold — all read-only and no novel signatures.
        assert_eq!(
            cb.observe(signal(&[patterns[5 % 3]], false)),
            BreakerAction::InjectCorrection
        );
        assert_eq!(cb.state(), BreakerState::Open);
    }

    #[test]
    fn varied_exploration_with_novel_sigs_does_not_trip() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig {
            read_only_stall_threshold: 12,
            stall_threshold: 100,
            ..Default::default()
        });
        // 15 rounds of unique read-only exploration — each round has a novel signature.
        // Progress-aware detection should NOT hard-trip. Introspect at round 12.
        for i in 0..15 {
            let action = cb.observe(signal(&[&format!("read_file:file{i}.rs")], false));
            match action {
                BreakerAction::Introspect { .. } => {} // expected at multiples of 12
                BreakerAction::Continue => {}
                other => panic!("round {i} should not hard-trip, got {other:?}"),
            }
        }
        assert_eq!(cb.state(), BreakerState::Closed);
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
        // 7 rounds of unique read-only exploration (below threshold, and novel sigs = progress).
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
        let mut cb = LoopCircuitBreaker::new(BreakerConfig {
            read_only_stall_threshold: 6,
            stall_threshold: 100,
            ..Default::default()
        });
        // 4 read-only rounds with repeating patterns (churn).
        let patterns = ["grep:a", "grep:b"];
        for i in 0..4 {
            cb.observe(signal(&[patterns[i % 2]], false));
        }
        // Mutation resets the streak.
        cb.observe(signal(&["write_file:fix.rs"], true));
        // 5 more read-only rounds with repeating patterns (below threshold of 6).
        for i in 0..5 {
            assert_eq!(
                cb.observe(signal(&[patterns[i % 2]], false)),
                BreakerAction::Continue,
                "round {i} after mutation should continue"
            );
        }
        // Round 6 after mutation trips (6 consecutive read-only with no novel sigs).
        assert_eq!(
            cb.observe(signal(&[patterns[5 % 2]], false)),
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

    // ─── Progress-aware read_only_stall ───────────────────────────────

    /// BreakerAction has exactly 4 variants. Introspect is a soft self-check;
    /// InjectCorrection is the hard stall correction.
    #[test]
    fn breaker_action_variants_are_exhaustively_handled() {
        let actions = [
            BreakerAction::Continue,
            BreakerAction::Introspect {
                consecutive_read_only: 12,
            },
            BreakerAction::InjectCorrection,
            BreakerAction::Abort,
        ];
        for a in &actions {
            match a {
                BreakerAction::Continue => {}
                BreakerAction::InjectCorrection => {}
                BreakerAction::Introspect { .. } => {}
                BreakerAction::Abort => {}
            }
        }
    }

    /// Every 12 consecutive read-only rounds (with novel sigs = progress),
    /// emit Introspect to let the model self-reflect. Tools stay enabled.
    #[test]
    fn introspect_fires_every_12_read_only_rounds_with_progress() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig {
            stall_threshold: 100,
            ..Default::default()
        });
        let mut introspect_rounds = vec![];
        for i in 0..36 {
            let action = cb.observe(signal(&[&format!("read_file:f{i}")], false));
            if matches!(action, BreakerAction::Introspect { .. }) {
                introspect_rounds.push(i);
            }
        }
        // Fires at round 12, 24, 36 (0-indexed: 11, 23, 35)
        assert_eq!(introspect_rounds, vec![11, 23, 35]);
        // State stays Closed — Introspect is not a trip.
        assert_eq!(cb.state(), BreakerState::Closed);
    }

    /// Introspect does NOT fire if a mutation resets the streak.
    #[test]
    fn introspect_resets_on_mutation() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig {
            stall_threshold: 100,
            ..Default::default()
        });
        // 10 read-only rounds
        for i in 0..10 {
            assert_eq!(
                cb.observe(signal(&[&format!("read_file:f{i}")], false)),
                BreakerAction::Continue
            );
        }
        // Mutation resets
        cb.observe(signal(&["write_file:x"], true));
        // 11 more read-only rounds — no introspect yet (only 11 since reset)
        for i in 0..11 {
            assert_eq!(
                cb.observe(signal(&[&format!("read_file:g{i}")], false)),
                BreakerAction::Continue
            );
        }
        // 12th after reset → introspect
        assert_eq!(
            cb.observe(signal(&["read_file:g11"], false)),
            BreakerAction::Introspect {
                consecutive_read_only: 12
            }
        );
    }

    #[test]
    fn introspect_reports_actual_consecutive_read_only_count() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig {
            stall_threshold: 100,
            ..Default::default()
        });

        for i in 0..11 {
            assert_eq!(
                cb.observe(signal(&[&format!("read_file:f{i}")], false)),
                BreakerAction::Continue
            );
        }

        assert_eq!(
            cb.observe(signal(&["read_file:f11"], false)),
            BreakerAction::Introspect {
                consecutive_read_only: 12
            }
        );
    }

    #[test]
    fn half_open_recovery_resets_read_only_streak() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig {
            stall_threshold: 100,
            repetition_threshold: 3,
            read_only_stall_threshold: 5,
            max_introspect_emissions: 3,
            half_open_patience: 2,
            absolute_max_rounds: 100,
        });

        for _ in 0..2 {
            assert_eq!(
                cb.observe(signal(&["grep:repeat"], false)),
                BreakerAction::Continue
            );
        }
        assert_eq!(
            cb.observe(signal(&["grep:repeat"], false)),
            BreakerAction::InjectCorrection
        );
        cb.correction_injected();

        assert_eq!(
            cb.observe(signal(&["read_file:recovered.rs"], false)),
            BreakerAction::Continue
        );
        assert_eq!(cb.state(), BreakerState::Closed);
        assert_eq!(
            cb.consecutive_read_only(),
            0,
            "recovery must reset the old read-only streak"
        );
    }

    /// Code review scenario: 12 rounds reading different files = progress.
    /// Must NEVER trip the circuit breaker.
    #[test]
    fn code_review_12_unique_reads_never_trips() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig {
            stall_threshold: 100, // isolate read_only_stall
            ..Default::default()
        });
        // Simulate: skill, git_diff(stat), git_diff(full), bash(diff file1),
        // read_file(a), read_file(b), read_file(c), bash(diff file2), ...
        let tools: Vec<String> = (0..15).map(|i| format!("read_file:file{i}.rs")).collect();
        for (i, tool) in tools.iter().enumerate() {
            let action = cb.observe(signal(&[tool.as_str()], false));
            // Round 12 (index 11) gets Introspect — a self-reflection prompt, not a trip.
            let expected = if i == 11 {
                BreakerAction::Introspect {
                    consecutive_read_only: 12,
                }
            } else {
                BreakerAction::Continue
            };
            assert_eq!(
                action, expected,
                "round {i}: novel signature = progress, must not hard-trip"
            );
        }
        assert_eq!(cb.state(), BreakerState::Closed);
    }

    /// Weak model churning: repeating same grep patterns without progress.
    /// Must trip at read_only_stall_threshold (12).
    #[test]
    fn weak_model_churn_trips_at_threshold_12() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig {
            stall_threshold: 100, // isolate read_only_stall
            ..Default::default()
        });
        assert_eq!(cb.config.read_only_stall_threshold, 12);
        // Cycle through 3 patterns — after round 3, no novel signatures.
        let patterns = ["grep:foo", "grep:bar", "grep:baz"];
        for i in 0..11 {
            assert_eq!(
                cb.observe(signal(&[patterns[i % 3]], false)),
                BreakerAction::Continue,
                "round {i} should continue (below threshold)"
            );
        }
        // Round 12: trips
        assert_eq!(
            cb.observe(signal(&[patterns[11 % 3]], false)),
            BreakerAction::InjectCorrection
        );
    }

    /// Introspect emissions are capped per turn to prevent unbounded self-check
    /// spam on genuinely long read-only sessions.
    #[test]
    fn introspect_emissions_are_capped() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig {
            stall_threshold: 100,
            read_only_stall_threshold: 4,
            max_introspect_emissions: 2,
            ..Default::default()
        });
        let mut introspect_count = 0;
        // 20 read-only rounds with novel sigs — would fire at 4, 8, 12, 16, 20
        // but cap=2 limits to first two emissions only.
        for i in 0..20 {
            let action = cb.observe(signal(&[&format!("read_file:f{i}")], false));
            if matches!(action, BreakerAction::Introspect { .. }) {
                introspect_count += 1;
            }
        }
        assert_eq!(introspect_count, 2, "cap must limit introspect emissions");
        assert_eq!(cb.introspect_emissions(), 2);
        assert_eq!(cb.state(), BreakerState::Closed);
    }

    /// After a mutation, the introspect emission counter resets so a subsequent
    /// read-only streak gets a fresh budget of self-check prompts.
    #[test]
    fn introspect_cap_resets_on_mutation() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig {
            stall_threshold: 100,
            read_only_stall_threshold: 3,
            max_introspect_emissions: 1,
            ..Default::default()
        });
        // 3 read-only → 1 introspect (hits cap)
        for i in 0..3 {
            cb.observe(signal(&[&format!("read_file:a{i}")], false));
        }
        assert_eq!(cb.introspect_emissions(), 1);
        // Another 3 read-only — capped, no more introspect.
        for i in 0..3 {
            let action = cb.observe(signal(&[&format!("read_file:b{i}")], false));
            assert_eq!(action, BreakerAction::Continue);
        }
        // Mutation resets the emission budget.
        cb.observe(signal(&["write_file:x"], true));
        assert_eq!(cb.introspect_emissions(), 0);
        // Next 3 read-only rounds trigger a fresh introspect.
        for i in 0..2 {
            assert_eq!(
                cb.observe(signal(&[&format!("read_file:c{i}")], false)),
                BreakerAction::Continue
            );
        }
        assert!(matches!(
            cb.observe(signal(&["read_file:c2"], false)),
            BreakerAction::Introspect { .. }
        ));
    }

    /// max_introspect_emissions=0 disables the cap (unbounded introspection).
    #[test]
    fn introspect_cap_zero_is_unbounded() {
        let mut cb = LoopCircuitBreaker::new(BreakerConfig {
            stall_threshold: 100,
            read_only_stall_threshold: 3,
            max_introspect_emissions: 0,
            ..Default::default()
        });
        let mut count = 0;
        for i in 0..30 {
            let action = cb.observe(signal(&[&format!("read_file:f{i}")], false));
            if matches!(action, BreakerAction::Introspect { .. }) {
                count += 1;
            }
        }
        // 30 / 3 = 10 emissions
        assert_eq!(count, 10);
    }

    /// Default threshold is 12.
    #[test]
    fn default_read_only_stall_threshold_is_12() {
        let cfg = BreakerConfig::default();
        assert_eq!(cfg.read_only_stall_threshold, 12);
    }

    /// Default introspect cap is 3.
    #[test]
    fn default_max_introspect_emissions_is_3() {
        let cfg = BreakerConfig::default();
        assert_eq!(cfg.max_introspect_emissions, 3);
    }
}
