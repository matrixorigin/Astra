//! Multiturn compaction tier progression.
//!
//! Simulates a 20-turn conversation whose running context tokens grow
//! monotonically and verifies:
//!
//!   * `ContextBudget::compaction_tier` walks through the documented tiers
//!     in the expected order (Normal → TrimSchemas → CompactHistory →
//!     AggressivePrune) as the context fills
//!   * The tier boundaries scale with `compact_threshold` (tight budgets
//!     escalate sooner)
//!   * `budget_pressure` matches the documented scalar per tier
//!
//! After the Phase 1 refactor, the authoritative per-turn tier comes from
//! the context pipeline's planner (`ContextPlan::compact_tier`). The
//! legacy `compaction_tier_calibrated` helper was retired along with its
//! integration tests; what remains here exercises the pure-function
//! `ContextBudget::compaction_tier` that the planner itself consults.

use astra_runtime::prompts::{CompactionTier, ContextBudget, budget_for_model};

fn default_budget() -> ContextBudget {
    let mut b = budget_for_model(Some("claude-sonnet-4"));
    // Pin the threshold so the assertions below don't drift with config.
    b.compact_threshold = 0.75;
    b
}

#[test]
fn tier_progression_walks_all_four_stages_as_usage_grows() {
    let budget = default_budget();
    let limit = budget.effective_input_limit() as f64;

    // 20-turn simulation: each turn adds roughly 5% of effective limit.
    let mut observed: Vec<CompactionTier> = Vec::new();
    for turn in 0..20 {
        let usage = (limit * (turn as f64 + 1.0) / 20.0) as usize;
        observed.push(budget.compaction_tier(usage));
    }

    // Must monotonically non-decrease through the rank order.
    let ranks: Vec<u8> = observed
        .iter()
        .map(|t| match t {
            CompactionTier::Normal => 0,
            CompactionTier::TrimSchemas => 1,
            CompactionTier::CompactHistory => 2,
            CompactionTier::AggressivePrune => 3,
        })
        .collect();
    for pair in ranks.windows(2) {
        assert!(
            pair[1] >= pair[0],
            "tier must not decrease as usage grows: {ranks:?}"
        );
    }

    // Must hit each of the four tiers at least once across the 20 turns.
    assert!(
        observed.contains(&CompactionTier::Normal),
        "never saw Normal"
    );
    assert!(
        observed.contains(&CompactionTier::TrimSchemas),
        "never saw TrimSchemas: {observed:?}"
    );
    assert!(
        observed.contains(&CompactionTier::CompactHistory),
        "never saw CompactHistory: {observed:?}"
    );
    assert!(
        observed.contains(&CompactionTier::AggressivePrune),
        "never saw AggressivePrune: {observed:?}"
    );
}

#[test]
fn tight_threshold_escalates_sooner_than_default() {
    let mut default_b = default_budget();
    default_b.compact_threshold = 0.75;
    let mut tight_b = default_budget();
    tight_b.compact_threshold = 0.60;

    let limit = default_b.effective_input_limit();
    // Half the effective limit: default stays Normal, tight already in TrimSchemas.
    let half = limit / 2;
    assert_eq!(default_b.compaction_tier(half), CompactionTier::Normal);
    assert_eq!(tight_b.compaction_tier(half), CompactionTier::TrimSchemas);
}

#[test]
fn budget_pressure_scalar_matches_documented_contract() {
    assert!((CompactionTier::Normal.budget_pressure() - 0.0).abs() < f64::EPSILON);
    assert!((CompactionTier::TrimSchemas.budget_pressure() - 0.3).abs() < f64::EPSILON);
    assert!((CompactionTier::CompactHistory.budget_pressure() - 0.6).abs() < f64::EPSILON);
    assert!((CompactionTier::AggressivePrune.budget_pressure() - 0.9).abs() < f64::EPSILON);
}

#[test]
fn zero_tokens_classifies_as_normal_regardless_of_budget_shape() {
    for threshold in [0.50, 0.60, 0.75, 0.85] {
        let mut b = default_budget();
        b.compact_threshold = threshold;
        assert_eq!(
            b.compaction_tier(0),
            CompactionTier::Normal,
            "zero tokens at threshold {threshold} must be Normal"
        );
    }
}
