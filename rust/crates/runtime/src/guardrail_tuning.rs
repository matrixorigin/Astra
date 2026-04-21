//! Guardrail auto-tuning.
//!
//! Rolling-stats-based, bounded-Δ adjustment for the auto-reflection
//! signal threshold (how many accumulated evolution signals are needed
//! before a reflection turn is triggered).
//!
//! Design principles:
//! 1. Rolling window of recent per-turn outcomes (failing tool calls).
//! 2. Bounded Δ: adjust threshold by at most ±1 per tuning tick.
//! 3. Never fully disable: threshold is clamped to `[MIN, MAX]`.
//! 4. Hysteresis: only tune after observing at least MIN_SAMPLES turns.
//!
//! The tuner is owned by `StallTrackingState::guardrail_tuner`; the auto-
//! reflection path reads `reflection_threshold()` each turn instead of
//! the const, and `record_turn_outcome()` is called once per turn with
//! a boolean indicating whether the turn had any tool failure.

/// Default starting threshold (= historical `AUTO_REFLECTION_SIGNAL_THRESHOLD`).
pub const DEFAULT_REFLECTION_THRESHOLD: u32 = 3;
/// Lowest threshold the tuner will ever set (never below 2 — prevents
/// reflection on every turn).
pub const MIN_REFLECTION_THRESHOLD: u32 = 2;
/// Highest threshold the tuner will ever set (never above 6 — prevents
/// effectively disabling reflection).
pub const MAX_REFLECTION_THRESHOLD: u32 = 6;

/// Rolling window size: how many recent turn outcomes to consider.
const WINDOW_SIZE: usize = 10;
/// Minimum samples required before the first tuning decision.
const MIN_SAMPLES: usize = 5;
/// Tune after every N turn outcomes recorded.
const TUNE_INTERVAL: u32 = 5;
/// If recent failure rate exceeds this, react faster (decrease threshold).
const HIGH_FAIL_RATE: f32 = 0.40;
/// If recent failure rate is below this, back off (increase threshold).
const LOW_FAIL_RATE: f32 = 0.10;

/// Per-session guardrail tuner.
///
/// Cheap (a few integers + a bounded VecDeque); safe to keep on every
/// `AgenticLoopState` via `StallTrackingState`.
#[derive(Debug, Clone)]
pub struct GuardrailTuner {
    threshold: u32,
    window: std::collections::VecDeque<bool>,
    turns_seen: u32,
    last_tune_at: u32,
    last_delta: i32,
}

impl Default for GuardrailTuner {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_REFLECTION_THRESHOLD,
            window: std::collections::VecDeque::with_capacity(WINDOW_SIZE),
            turns_seen: 0,
            last_tune_at: 0,
            last_delta: 0,
        }
    }
}

impl GuardrailTuner {
    /// Current (possibly tuned) reflection signal threshold.
    #[inline]
    pub fn reflection_threshold(&self) -> u32 {
        self.threshold
    }

    /// Number of turns observed since session start (for diagnostics).
    #[inline]
    pub fn turns_seen(&self) -> u32 {
        self.turns_seen
    }

    /// Last Δ applied (+1, 0, -1) — useful for self-awareness rendering.
    #[inline]
    pub fn last_delta(&self) -> i32 {
        self.last_delta
    }

    /// Recent failure rate in `[0.0, 1.0]`; `None` while under MIN_SAMPLES.
    pub fn recent_fail_rate(&self) -> Option<f32> {
        if self.window.len() < MIN_SAMPLES {
            return None;
        }
        let fails = self.window.iter().filter(|&&f| f).count();
        Some(fails as f32 / self.window.len() as f32)
    }

    /// Record one turn's outcome. `had_failure = true` means the turn
    /// ended with at least one failing tool call (or other negative
    /// signal) — the caller decides the heuristic.
    ///
    /// Returns `Some(delta)` if the threshold was adjusted this call,
    /// `None` otherwise.
    pub fn record_turn_outcome(&mut self, had_failure: bool) -> Option<i32> {
        if self.window.len() == WINDOW_SIZE {
            self.window.pop_front();
        }
        self.window.push_back(had_failure);
        self.turns_seen = self.turns_seen.saturating_add(1);

        if self.turns_seen.saturating_sub(self.last_tune_at) < TUNE_INTERVAL {
            return None;
        }
        let rate = self.recent_fail_rate()?;
        self.last_tune_at = self.turns_seen;

        let delta: i32 = if rate >= HIGH_FAIL_RATE && self.threshold > MIN_REFLECTION_THRESHOLD {
            -1
        } else if rate <= LOW_FAIL_RATE && self.threshold < MAX_REFLECTION_THRESHOLD {
            1
        } else {
            0
        };
        if delta != 0 {
            let next = self.threshold as i32 + delta;
            self.threshold = next.clamp(
                MIN_REFLECTION_THRESHOLD as i32,
                MAX_REFLECTION_THRESHOLD as i32,
            ) as u32;
            self.last_delta = delta;
            Some(delta)
        } else {
            self.last_delta = 0;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_historical_constant() {
        let t = GuardrailTuner::default();
        assert_eq!(t.reflection_threshold(), DEFAULT_REFLECTION_THRESHOLD);
        assert_eq!(t.turns_seen(), 0);
        assert!(t.recent_fail_rate().is_none());
    }

    #[test]
    fn no_tuning_before_min_samples() {
        let mut t = GuardrailTuner::default();
        for _ in 0..(MIN_SAMPLES - 1) {
            assert!(t.record_turn_outcome(true).is_none());
        }
        assert_eq!(t.reflection_threshold(), DEFAULT_REFLECTION_THRESHOLD);
    }

    #[test]
    fn high_fail_rate_lowers_threshold_bounded() {
        let mut t = GuardrailTuner::default();
        // Drive failure rate to 100% and tune many times.
        for _ in 0..100 {
            t.record_turn_outcome(true);
        }
        assert_eq!(t.reflection_threshold(), MIN_REFLECTION_THRESHOLD);
        assert!(t.reflection_threshold() >= MIN_REFLECTION_THRESHOLD);
    }

    #[test]
    fn low_fail_rate_raises_threshold_bounded() {
        let mut t = GuardrailTuner::default();
        for _ in 0..100 {
            t.record_turn_outcome(false);
        }
        assert_eq!(t.reflection_threshold(), MAX_REFLECTION_THRESHOLD);
    }

    #[test]
    fn mid_fail_rate_stays_stable() {
        let mut t = GuardrailTuner::default();
        // ~20% fail rate (1 in 5) → between LOW (10%) and HIGH (40%) → no change.
        for i in 0..50 {
            t.record_turn_outcome(i % 5 == 0);
        }
        assert_eq!(t.reflection_threshold(), DEFAULT_REFLECTION_THRESHOLD);
    }

    #[test]
    fn bounded_delta_per_tick() {
        // Transition from all-fail → all-success should take multiple
        // ticks because each tick is bounded to Δ=±1.
        let mut t = GuardrailTuner::default();
        for _ in 0..TUNE_INTERVAL {
            t.record_turn_outcome(true);
        }
        let after_first_tune = t.reflection_threshold();
        assert_eq!(after_first_tune, DEFAULT_REFLECTION_THRESHOLD - 1);
        assert_eq!(t.last_delta(), -1);
    }

    #[test]
    fn never_disables_even_under_pressure() {
        let mut t = GuardrailTuner::default();
        for _ in 0..10_000 {
            t.record_turn_outcome(true);
        }
        assert!(t.reflection_threshold() >= MIN_REFLECTION_THRESHOLD);
        assert!(t.reflection_threshold() <= MAX_REFLECTION_THRESHOLD);
    }
}
