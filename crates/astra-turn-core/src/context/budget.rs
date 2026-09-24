//! Token budget allocation and compaction tier selection for the context pipeline.
//!
//! The planner uses these functions to decide how aggressively to compact
//! based on current pressure and recovery state.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::compaction_types::CompactionTier;
use crate::section_types::SectionKind;
pub use astra_turn_types::context_execution::{
    CompactionThresholds, PRESSURE_AGGRESSIVE_PRUNE, PRESSURE_COMPACT_HISTORY,
    PRESSURE_TRIM_SCHEMAS,
};

/// Select a tier from the admitted policy, never process-wide thresholds.
#[must_use]
pub fn select_compaction_tier(pressure: f64, thresholds: CompactionThresholds) -> CompactionTier {
    thresholds.select_tier(pressure)
}

/// Predictive pressure can escalate, never lower the raw-pressure tier.
#[must_use]
pub fn select_tier_gated(
    raw_pressure: f64,
    predictive_pressure: f64,
    thresholds: CompactionThresholds,
) -> CompactionTier {
    select_compaction_tier(raw_pressure, thresholds)
        .max(select_compaction_tier(predictive_pressure, thresholds))
}

/// Token budget allocated per section kind.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenBudget {
    pub effective_limit: u32,
    pub allocations: HashMap<SectionKind, u32>,
}

impl TokenBudget {
    /// Allocate token budget across sections based on tier and history.
    ///
    /// Higher tiers get tighter history budgets to free headroom.
    /// When `section_history` has EMA data for a section, the allocator
    /// shrinks toward observed usage + 50% headroom (capped at the
    /// tier-based maximum).
    #[must_use]
    pub fn allocate(
        effective_limit: u32,
        tier: CompactionTier,
        section_history: &HashMap<SectionKind, u32>,
    ) -> Self {
        let limit = effective_limit as f64;

        let fixed_budget = (limit * 0.10).min(2000.0) as u32;
        let memory_ratio = match tier {
            CompactionTier::Normal => 0.15,
            CompactionTier::TrimSchemas => 0.12,
            CompactionTier::CompactHistory => 0.08,
            CompactionTier::AggressivePrune => 0.05,
        };

        let mut allocations = HashMap::new();
        allocations.insert(SectionKind::Identity, fixed_budget);
        allocations.insert(SectionKind::Constraints, fixed_budget);

        // Conversation history is carried as provider messages, not a planned
        // text section, so it must not reserve section budget here.
        let memory_max = (limit * memory_ratio) as u32;
        let memory_budget = if let Some(&observed) = section_history.get(&SectionKind::Memory) {
            observed_budget_with_floor(observed, memory_max)
        } else {
            memory_max
        };
        allocations.insert(SectionKind::Memory, memory_budget);

        let allocated: u32 = allocations.values().sum();
        let remaining = effective_limit.saturating_sub(allocated);
        // Enumerate remainder kinds via `SectionKind::all_planned()` so adding
        // a new variant is a compile error at the enum site (via the
        // exhaustive match in `SectionKind::is_preallocated`) rather than a
        // silent budget-zero drop at runtime.
        let remainder_kinds: Vec<SectionKind> = SectionKind::all_planned()
            .iter()
            .copied()
            .filter(|k| !k.is_preallocated())
            .collect();
        if remainder_kinds.is_empty() {
            return Self {
                effective_limit,
                allocations,
            };
        }
        let base = remaining / remainder_kinds.len() as u32;
        let mut extra = remaining % remainder_kinds.len() as u32;
        for kind in remainder_kinds {
            let budget = base + u32::from(extra > 0);
            extra = extra.saturating_sub(1);
            allocations.insert(kind, budget);
        }

        Self {
            effective_limit,
            allocations,
        }
    }

    /// Get the budget for a specific section, returning 0 if not allocated.
    #[must_use]
    pub fn budget_for(&self, kind: SectionKind) -> u32 {
        self.allocations.get(&kind).copied().unwrap_or(0)
    }

    /// Total allocated across all sections.
    #[must_use]
    pub fn total_allocated(&self) -> u32 {
        self.allocations.values().sum()
    }
}

fn observed_budget_with_floor(observed: u32, max_budget: u32) -> u32 {
    const MIN_ADAPTIVE_SECTION_BUDGET: u32 = 256;
    if max_budget == 0 {
        return 0;
    }
    observed
        .saturating_add(observed / 2)
        .max(MIN_ADAPTIVE_SECTION_BUDGET)
        .min(max_budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery_state::RecoveryState;
    use proptest::prelude::*;

    #[test]
    fn tier_from_pressure_boundaries() {
        assert_eq!(
            select_compaction_tier(0.55, CompactionThresholds::default()),
            CompactionTier::Normal
        );
        assert_eq!(
            select_compaction_tier(0.60, CompactionThresholds::default()),
            CompactionTier::Normal
        );
        assert_eq!(
            select_compaction_tier(0.65, CompactionThresholds::default()),
            CompactionTier::TrimSchemas
        );
        assert_eq!(
            select_compaction_tier(0.75, CompactionThresholds::default()),
            CompactionTier::TrimSchemas
        );
        assert_eq!(
            select_compaction_tier(0.80, CompactionThresholds::default()),
            CompactionTier::CompactHistory
        );
        assert_eq!(
            select_compaction_tier(0.90, CompactionThresholds::default()),
            CompactionTier::AggressivePrune
        );
        assert_eq!(
            select_compaction_tier(0.92, CompactionThresholds::default()),
            CompactionTier::AggressivePrune
        );
        assert_eq!(
            select_compaction_tier(1.05, CompactionThresholds::default()),
            CompactionTier::AggressivePrune
        );
    }

    #[test]
    fn custom_thresholds_preserve_predictive_escalation_and_raw_floor() {
        use astra_turn_types::context_execution::{CompactConfig, ContextBudget};
        let policy = ContextBudget::resolve(
            Some(200_000),
            Some(16_384),
            0.6,
            6,
            8_000,
            CompactConfig::default(),
        );
        let thresholds = policy.compaction_thresholds();
        assert_eq!(
            select_tier_gated(0.30, 0.65, thresholds),
            CompactionTier::CompactHistory
        );
        assert_eq!(
            select_tier_gated(0.65, 0.30, thresholds),
            CompactionTier::CompactHistory
        );
        assert_eq!(
            select_tier_gated(0.65, 0.70, thresholds),
            CompactionTier::AggressivePrune
        );
        assert_eq!(
            select_compaction_tier(0.60, thresholds),
            CompactionTier::TrimSchemas
        );
        assert_eq!(
            select_compaction_tier(0.60001, thresholds),
            CompactionTier::CompactHistory
        );
    }

    #[test]
    fn tier_escalation_for_recovery() {
        let mut r = RecoveryState::default();
        r.record_ptl_error();
        let base = CompactionTier::Normal;
        let escalated = base.escalate_for_recovery(r.consecutive_ptl_errors);
        assert!(escalated > base, "1 PTL should escalate Normal");

        r.record_ptl_error();
        let escalated2 = CompactionTier::Normal.escalate_for_recovery(r.consecutive_ptl_errors);
        assert!(
            escalated2 >= CompactionTier::CompactHistory,
            "2 PTL should reach CompactHistory+"
        );
    }

    #[test]
    fn tier_ordering() {
        assert!(CompactionTier::Normal < CompactionTier::TrimSchemas);
        assert!(CompactionTier::TrimSchemas < CompactionTier::CompactHistory);
        assert!(CompactionTier::CompactHistory < CompactionTier::AggressivePrune);
    }

    #[test]
    fn gated_tier_never_deescalates() {
        // raw = 0.80 (CompactHistory), predictive = 0.55 (Normal)
        // Gated should stay at CompactHistory, not drop to Normal
        let tier = select_tier_gated(0.80, 0.55, CompactionThresholds::default());
        assert_eq!(tier, CompactionTier::CompactHistory);

        // raw = 0.55 (Normal), predictive = 0.80 (CompactHistory)
        // Gated should escalate to CompactHistory
        let tier2 = select_tier_gated(0.55, 0.80, CompactionThresholds::default());
        assert_eq!(tier2, CompactionTier::CompactHistory);
    }

    #[test]
    fn budget_total_never_exceeds_limit() {
        let history = HashMap::new();
        for &tier in &[
            CompactionTier::Normal,
            CompactionTier::TrimSchemas,
            CompactionTier::CompactHistory,
            CompactionTier::AggressivePrune,
        ] {
            let budget = TokenBudget::allocate(100_000, tier, &history);
            assert!(
                budget.total_allocated() <= budget.effective_limit,
                "tier={tier:?}: allocated={} > limit={}",
                budget.total_allocated(),
                budget.effective_limit,
            );
        }
    }

    #[test]
    fn budget_distributes_integer_remainder() {
        let budget = TokenBudget::allocate(100_003, CompactionTier::Normal, &HashMap::new());

        assert_eq!(
            budget.total_allocated(),
            budget.effective_limit,
            "section allocation should not discard integer-division remainders"
        );
    }

    proptest! {
        #[test]
        fn budget_total_never_exceeds_limit_for_any_tier(
            limit in 0u32..=1_000_000,
            tier_idx in 0usize..4,
        ) {
            let tiers = [
                CompactionTier::Normal,
                CompactionTier::TrimSchemas,
                CompactionTier::CompactHistory,
                CompactionTier::AggressivePrune,
            ];
            let budget = TokenBudget::allocate(limit, tiers[tier_idx], &HashMap::new());
            prop_assert!(
                budget.total_allocated() <= budget.effective_limit,
                "allocated={} > limit={} tier={:?}",
                budget.total_allocated(),
                budget.effective_limit,
                tiers[tier_idx],
            );
        }

        #[test]
        fn gated_tier_never_deescalates_for_any_pressure(
            raw in 0.0f64..1.5,
            predictive in 0.0f64..1.5,
        ) {
            let gated = select_tier_gated(raw, predictive, CompactionThresholds::default());
            prop_assert!(gated >= select_compaction_tier(raw, CompactionThresholds::default()));
            prop_assert!(gated >= select_compaction_tier(predictive, CompactionThresholds::default()));
        }
    }

    #[test]
    fn budget_does_not_reserve_ghost_history_section() {
        let budget = TokenBudget::allocate(100_000, CompactionTier::Normal, &HashMap::new());
        assert_eq!(
            budget.budget_for(SectionKind::History),
            0,
            "history travels in provider messages and must not reserve section budget"
        );
        assert!(budget.total_allocated() <= budget.effective_limit);
    }

    #[test]
    fn budget_uses_memory_history_to_shrink_overallocated_with_floor() {
        let mut history = HashMap::new();
        history.insert(SectionKind::Memory, 2u32);

        let budget = TokenBudget::allocate(100_000, CompactionTier::Normal, &history);
        let memory_budget = budget.budget_for(SectionKind::Memory);
        assert!(
            memory_budget < 15_000,
            "Memory budget should shrink from feedback, got {memory_budget}"
        );
        assert!(
            memory_budget >= 256,
            "Memory budget should retain a usable floor, got {memory_budget}"
        );
        assert!(budget.total_allocated() <= budget.effective_limit);
    }

    #[test]
    fn budget_without_history_does_not_allocate_history() {
        let budget = TokenBudget::allocate(100_000, CompactionTier::Normal, &HashMap::new());
        assert_eq!(budget.budget_for(SectionKind::History), 0);
    }

    #[test]
    fn budget_does_not_allocate_emergent_zero_budget_sections() {
        let budget = TokenBudget::allocate(100_000, CompactionTier::Normal, &HashMap::new());

        for kind in [
            SectionKind::EmergentSkills,
            SectionKind::EmergentMemory,
            SectionKind::EmergentSummary,
        ] {
            assert_eq!(
                budget.budget_for(kind),
                0,
                "{kind:?} is emitted by the planner with estimated_tokens=0, so it must not consume remainder budget"
            );
        }
        assert_eq!(
            budget.total_allocated(),
            budget.effective_limit,
            "usable planned sections should receive the full effective limit"
        );
    }

    #[test]
    fn higher_tier_gets_tighter_memory_budget() {
        let history = HashMap::new();
        let normal = TokenBudget::allocate(100_000, CompactionTier::Normal, &history);
        let aggressive = TokenBudget::allocate(100_000, CompactionTier::AggressivePrune, &history);
        assert!(
            normal.budget_for(SectionKind::Memory) > aggressive.budget_for(SectionKind::Memory),
            "Normal should have more memory budget than AggressivePrune"
        );
    }
}
