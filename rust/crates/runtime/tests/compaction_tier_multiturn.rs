//! Multiturn compaction tier progression (Phase 3).
//!
//! Simulates a 20-turn conversation whose running context tokens grow
//! monotonically and verifies:
//!
//!   * `ContextBudget::compaction_tier` walks through the documented tiers
//!     in the expected order (Normal → TrimSchemas → CompactHistory →
//!     AggressivePrune) as the context fills
//!   * The tier boundaries scale with `compact_threshold` (tight budgets
//!     escalate sooner)
//!   * `compaction_tier_calibrated` never *downgrades* from either the
//!     estimated or measured tier
//!   * Consecutive context-window errors bump the tier toward
//!     AggressivePrune (capped)
//!   * `budget_pressure` matches the documented scalar per tier
//!
//! This exercises the pure-function compaction-decision layer that the
//! runtime loop consults each turn; a real 20-turn mock-LLM run would
//! layer on little more than additional scaffolding without changing
//! the classification contract.

use astra_runtime::prompts::{
    CompactionTier, ContextBudget, budget_for_model, compaction_tier_calibrated,
};

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
fn calibrated_never_downgrades_from_estimated_or_measured() {
    let budget = default_budget();
    let limit = budget.effective_input_limit();

    // Estimated says Normal, but measured (from provider) says CompactHistory.
    let estimated = (limit as f64 * 0.40) as usize; // Normal
    let measured = (limit as f64 * 0.80) as u64; // CompactHistory

    let calibrated = compaction_tier_calibrated(&budget, estimated, Some(measured), 0);
    assert_eq!(
        calibrated,
        CompactionTier::CompactHistory,
        "calibrated must surface the stricter of estimated vs measured"
    );
}

#[test]
fn context_window_errors_bump_tier_toward_aggressive_prune() {
    let budget = default_budget();
    let limit = budget.effective_input_limit();
    let estimated = (limit as f64 * 0.40) as usize; // Normal

    // Zero errors → stays Normal.
    assert_eq!(
        compaction_tier_calibrated(&budget, estimated, None, 0),
        CompactionTier::Normal
    );

    // One error bumps by 1 rank → TrimSchemas.
    assert_eq!(
        compaction_tier_calibrated(&budget, estimated, None, 1),
        CompactionTier::TrimSchemas
    );

    // Three errors saturate at AggressivePrune.
    assert_eq!(
        compaction_tier_calibrated(&budget, estimated, None, 3),
        CompactionTier::AggressivePrune
    );

    // Capped: 10 errors still can't exceed AggressivePrune.
    assert_eq!(
        compaction_tier_calibrated(&budget, estimated, None, 10),
        CompactionTier::AggressivePrune
    );
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
