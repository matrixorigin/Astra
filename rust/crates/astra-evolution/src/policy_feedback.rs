//! Policy feedback controller.
//!
//! Given aggregated guard-hit statistics for a model, propose tuning deltas
//! to that model's `EffectiveToolPolicy`. Pure functions — no I/O, no global
//! state. The scheduler, journal aggregator, and proposal-persistence layers
//! wrap this with side effects.
//!
//! ## Control logic
//!
//! The only knob tuned here is `max_identical_tool_calls`. The other
//! workflow-guard fields follow it with fixed ratios once we have confidence
//! the tuning direction is stable. This keeps the control surface small
//! and auditable.
//!
//! Rules (tightening means a smaller number; loosening means larger):
//!
//! - **Sample size gate.** Below [`MIN_GUARD_HITS_FOR_PROPOSAL`] guard hits,
//!   return `None` — the signal is too noisy to act on.
//! - **Tighten zone.** When self-heal rate ≥ 90%, the guard is frequently
//!   saving the model from its own loops. Decrement by 1 (floor 2).
//! - **Loosen zone.** When self-heal rate ≤ 70%, the guard is firing but
//!   the model isn't recovering — too aggressive. Increment by 1 (cap 6).
//! - **Hysteresis band (70%, 90%).** No change. Prevents oscillation.

use serde::{Deserialize, Serialize};

/// Minimum guard hits in a window before a proposal is considered actionable.
/// Below this, data is too noisy — the controller returns `None`.
pub const MIN_GUARD_HITS_FOR_PROPOSAL: u64 = 50;

/// Self-heal rate at which to *tighten* (inclusive).
pub const TIGHTEN_AT_OR_ABOVE: f64 = 0.90;

/// Self-heal rate at which to *loosen* (inclusive).
pub const LOOSEN_AT_OR_BELOW: f64 = 0.70;

/// Floor — never tighten below this `max_identical_tool_calls`.
/// Matches the Haiku built-in profile value.
pub const MAX_IDENTICAL_CALLS_FLOOR: u32 = 2;

/// Ceiling — never loosen above this.
pub const MAX_IDENTICAL_CALLS_CEIL: u32 = 6;

/// Aggregated guard-hit stats for a single model over an observation window.
///
/// Intended to be built by the journal aggregator (a separate concern).
/// Input to [`propose_policy_tuning`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelGuardStats {
    /// The model id these stats describe (used to target the right profile).
    pub model_id: String,
    /// Total turns observed in the window. Informational only.
    pub total_turns: u64,
    /// Total guard hits (cached-repeat, duplicate-within-turn, etc.) observed
    /// in the window. Gate on [`MIN_GUARD_HITS_FOR_PROPOSAL`].
    pub guard_hits: u64,
    /// Subset of `guard_hits` where the guard fired AND the turn still
    /// succeeded downstream (heuristic: model adapted to the hint).
    pub self_heal_success: u64,
    /// Subset of `guard_hits` where the guard fired AND the turn failed or
    /// aborted. Guarantee: `self_heal_success + self_heal_failure == guard_hits`.
    pub self_heal_failure: u64,
    /// Current resolved `max_identical_tool_calls` for this model. Used as
    /// the starting point for deltas (so proposals are relative, not absolute).
    pub current_max_identical_calls: u32,
}

impl ModelGuardStats {
    /// Self-heal rate in `[0.0, 1.0]`, or `None` when no guard hits observed.
    pub fn self_heal_rate(&self) -> Option<f64> {
        if self.guard_hits == 0 {
            return None;
        }
        Some(self.self_heal_success as f64 / self.guard_hits as f64)
    }
}

/// A proposed tuning change for a model's workflow-guard profile.
///
/// Produced by [`propose_policy_tuning`]. The caller is responsible for
/// persistence, audit logging, and applying hysteresis across multiple
/// windows before committing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyTuningProposal {
    /// Matches the `model_match` field of `ModelPolicyProfile`.
    pub model_match: String,
    /// Proposed new `max_identical_tool_calls`. Only set when different from
    /// `ModelGuardStats::current_max_identical_calls`.
    pub new_max_identical_tool_calls: Option<u32>,
    /// Human-readable reason. Goes into the audit log.
    pub reason: String,
}

/// Decide whether to tighten / loosen / hold a model's policy given one
/// window of observations.
///
/// Returns `None` when the sample is too small to act on, or when no change
/// is warranted (stats land inside the hysteresis band).
#[must_use]
pub fn propose_policy_tuning(stats: &ModelGuardStats) -> Option<PolicyTuningProposal> {
    propose_policy_tuning_inner(stats)
}

/// Apply hysteresis: only return a proposal when the current window plus
/// the last `N-1` windows in `history` all propose the same change.
///
/// `history` should contain the most recent N-1 proposals (oldest first or
/// newest first — order doesn't matter; equality is by value). Consumers
/// typically maintain a bounded ring buffer of past proposals.
///
/// This keeps the per-window function pure while giving callers a simple
/// way to resist noise. Setting `n = 1` is equivalent to calling
/// [`propose_policy_tuning`] directly.
#[must_use]
pub fn propose_with_hysteresis(
    stats: &ModelGuardStats,
    history: &[PolicyTuningProposal],
    n: usize,
) -> Option<PolicyTuningProposal> {
    let current = propose_policy_tuning_inner(stats)?;

    if n <= 1 {
        return Some(current);
    }

    // Need at least `n - 1` prior proposals that all match `current`.
    if history.len() < n - 1 {
        return None;
    }
    if history.iter().take(n - 1).any(|p| p != &current) {
        return None;
    }
    Some(current)
}

fn propose_policy_tuning_inner(stats: &ModelGuardStats) -> Option<PolicyTuningProposal> {
    if stats.guard_hits < MIN_GUARD_HITS_FOR_PROPOSAL {
        return None;
    }
    let rate = stats.self_heal_rate()?;

    if rate >= TIGHTEN_AT_OR_ABOVE {
        // High self-heal rate → guard is doing useful work; tighten further
        // if possible, but respect the floor.
        if stats.current_max_identical_calls <= MAX_IDENTICAL_CALLS_FLOOR {
            return None;
        }
        let new_value = stats.current_max_identical_calls - 1;
        Some(PolicyTuningProposal {
            model_match: stats.model_id.clone(),
            new_max_identical_tool_calls: Some(new_value),
            reason: format!(
                "self-heal rate {:.0}% ≥ {:.0}% over {} guard hits: tightening \
                 max_identical_tool_calls {} → {}",
                rate * 100.0,
                TIGHTEN_AT_OR_ABOVE * 100.0,
                stats.guard_hits,
                stats.current_max_identical_calls,
                new_value
            ),
        })
    } else if rate <= LOOSEN_AT_OR_BELOW {
        // Low self-heal rate → guard is firing but not helping. Loosen,
        // respecting the ceiling.
        if stats.current_max_identical_calls >= MAX_IDENTICAL_CALLS_CEIL {
            return None;
        }
        let new_value = stats.current_max_identical_calls + 1;
        Some(PolicyTuningProposal {
            model_match: stats.model_id.clone(),
            new_max_identical_tool_calls: Some(new_value),
            reason: format!(
                "self-heal rate {:.0}% ≤ {:.0}% over {} guard hits: loosening \
                 max_identical_tool_calls {} → {}",
                rate * 100.0,
                LOOSEN_AT_OR_BELOW * 100.0,
                stats.guard_hits,
                stats.current_max_identical_calls,
                new_value
            ),
        })
    } else {
        // Hysteresis band — no change.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(model: &str, hits: u64, success: u64, current: u32) -> ModelGuardStats {
        assert!(success <= hits, "test bug: success > hits");
        ModelGuardStats {
            model_id: model.to_string(),
            total_turns: hits * 2, // arbitrary; not used in logic
            guard_hits: hits,
            self_heal_success: success,
            self_heal_failure: hits - success,
            current_max_identical_calls: current,
        }
    }

    #[test]
    fn below_sample_size_threshold_returns_none() {
        // Even a perfect 100% self-heal rate should be ignored if the
        // sample is too small — don't tune on noise.
        let s = stats(
            "opus",
            MIN_GUARD_HITS_FOR_PROPOSAL - 1,
            MIN_GUARD_HITS_FOR_PROPOSAL - 1,
            4,
        );
        assert!(propose_policy_tuning(&s).is_none());
    }

    #[test]
    fn zero_guard_hits_returns_none() {
        let s = stats("opus", 0, 0, 4);
        assert!(propose_policy_tuning(&s).is_none());
    }

    #[test]
    fn hysteresis_band_returns_none() {
        // 80% self-heal rate — squarely in the 70-90% hold zone.
        let s = stats("opus", 100, 80, 4);
        assert!(propose_policy_tuning(&s).is_none());
    }

    #[test]
    fn high_self_heal_rate_tightens_by_one() {
        // 95% success over 100 guard hits → tighten from 4 → 3.
        let s = stats("opus", 100, 95, 4);
        let proposal = propose_policy_tuning(&s).expect("should propose");
        assert_eq!(proposal.model_match, "opus");
        assert_eq!(proposal.new_max_identical_tool_calls, Some(3));
        assert!(
            proposal.reason.contains("tightening"),
            "reason missing 'tightening': {}",
            proposal.reason
        );
        assert!(proposal.reason.contains("95%"), "{}", proposal.reason);
    }

    #[test]
    fn low_self_heal_rate_loosens_by_one() {
        // 60% success over 100 hits → loosen 3 → 4.
        let s = stats("haiku", 100, 60, 3);
        let proposal = propose_policy_tuning(&s).expect("should propose");
        assert_eq!(proposal.model_match, "haiku");
        assert_eq!(proposal.new_max_identical_tool_calls, Some(4));
        assert!(
            proposal.reason.contains("loosening"),
            "reason missing 'loosening': {}",
            proposal.reason
        );
    }

    #[test]
    fn tighten_never_below_floor() {
        // Already at floor (2) — even with perfect self-heal rate, don't go lower.
        let s = stats("haiku", 100, 100, MAX_IDENTICAL_CALLS_FLOOR);
        assert!(propose_policy_tuning(&s).is_none());
    }

    #[test]
    fn loosen_never_above_ceiling() {
        let s = stats("experiment", 100, 10, MAX_IDENTICAL_CALLS_CEIL);
        assert!(propose_policy_tuning(&s).is_none());
    }

    #[test]
    fn rate_exactly_at_tighten_threshold_triggers_tightening() {
        // Boundary: 90% should tighten (threshold is inclusive).
        let s = stats("opus", 100, 90, 4);
        let p = propose_policy_tuning(&s).expect("boundary must act");
        assert_eq!(p.new_max_identical_tool_calls, Some(3));
    }

    #[test]
    fn rate_exactly_at_loosen_threshold_triggers_loosening() {
        // Boundary: 70% should loosen (threshold is inclusive).
        let s = stats("gpt-5", 100, 70, 3);
        let p = propose_policy_tuning(&s).expect("boundary must act");
        assert_eq!(p.new_max_identical_tool_calls, Some(4));
    }

    #[test]
    fn rate_just_above_loosen_threshold_holds() {
        // 71% — inside hysteresis, no action.
        let s = stats("gpt-5", 100, 71, 3);
        assert!(propose_policy_tuning(&s).is_none());
    }

    #[test]
    fn rate_just_below_tighten_threshold_holds() {
        // 89% — inside hysteresis, no action.
        let s = stats("opus", 100, 89, 4);
        assert!(propose_policy_tuning(&s).is_none());
    }

    #[test]
    fn proposal_carries_model_id_unchanged() {
        let s = stats("us.anthropic.claude-opus-4-7", 100, 95, 4);
        let p = propose_policy_tuning(&s).unwrap();
        assert_eq!(p.model_match, "us.anthropic.claude-opus-4-7");
    }

    #[test]
    fn self_heal_rate_matches_declared_invariant() {
        // `self_heal_success + self_heal_failure == guard_hits` must hold
        // for the rate calculation to be meaningful.
        let s = stats("x", 100, 60, 3);
        assert_eq!(s.self_heal_success + s.self_heal_failure, s.guard_hits);
        assert_eq!(s.self_heal_rate(), Some(0.60));
    }

    #[test]
    fn stats_round_trip_through_json() {
        let s = stats("opus", 100, 95, 4);
        let json = serde_json::to_string(&s).unwrap();
        let round_trip: ModelGuardStats = serde_json::from_str(&json).unwrap();
        assert_eq!(round_trip, s);
    }

    #[test]
    fn proposal_round_trip_through_json() {
        let p = propose_policy_tuning(&stats("opus", 100, 95, 4)).unwrap();
        let json = serde_json::to_string(&p).unwrap();
        let round_trip: PolicyTuningProposal = serde_json::from_str(&json).unwrap();
        assert_eq!(round_trip, p);
    }

    /// Two observation windows for the same model, both agreeing.
    ///
    /// The caller is responsible for hysteresis across windows (see
    /// `propose_with_hysteresis` below). This test documents that the
    /// per-window function is stateless: given identical stats, it always
    /// returns the same proposal.
    #[test]
    fn stateless_same_input_same_output() {
        let s1 = stats("opus", 100, 95, 4);
        let s2 = s1.clone();
        assert_eq!(propose_policy_tuning(&s1), propose_policy_tuning(&s2));
    }

    /// Only commit a tuning change when **N consecutive windows** agree —
    /// `current` counts as the N-th, so callers supply the `N-1` most
    /// recent prior proposals.
    #[test]
    fn hysteresis_requires_three_windows_of_agreement() {
        let window = stats("opus", 100, 95, 4);
        let single = propose_policy_tuning(&window);
        assert!(single.is_some(), "single-window decision works");

        // history.len() == 1: only 2 windows total → not enough for N=3.
        let history1 = vec![single.clone().unwrap()];
        assert_eq!(
            propose_with_hysteresis(&window, &history1, 3),
            None,
            "current + 1 prior = 2 windows, need 3"
        );

        // history.len() == 2 matches current: 3 windows total → commit.
        let history2 = vec![single.clone().unwrap(), single.clone().unwrap()];
        assert_eq!(
            propose_with_hysteresis(&window, &history2, 3),
            single,
            "current + 2 matching priors = 3/3 windows, commits"
        );

        // A disagreeing prior breaks the streak.
        let different = {
            let s = stats("opus", 100, 60, 4); // loosen direction
            propose_policy_tuning(&s).unwrap()
        };
        let mixed = vec![single.clone().unwrap(), different];
        assert_eq!(
            propose_with_hysteresis(&window, &mixed, 3),
            None,
            "one non-matching prior breaks the streak"
        );

        // Current disagrees with priors — also no commit.
        let disagreeing = stats("opus", 100, 50, 4);
        assert_eq!(
            propose_with_hysteresis(&disagreeing, &history2, 3),
            None,
            "current window must also agree"
        );

        // n = 1 is the degenerate case: no history needed.
        assert_eq!(
            propose_with_hysteresis(&window, &[], 1),
            single,
            "n=1 collapses to the single-window decision"
        );
    }
}
