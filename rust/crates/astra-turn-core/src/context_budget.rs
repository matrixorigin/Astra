//! Token budget allocation and compaction tier selection for the context pipeline.
//!
//! The planner uses these functions to decide how aggressively to compact
//! based on current pressure and recovery state.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::compaction_types::CompactionTier;
use crate::section_types::SectionKind;
/// Pressure threshold above which schema trimming begins.
/// Shared with `microcompact::AdaptiveCompactConfig::from_pressure()`.
pub const PRESSURE_TRIM_SCHEMAS: f64 = 0.60;
/// Pressure threshold above which message history compaction activates.
pub const PRESSURE_COMPACT_HISTORY: f64 = 0.75;
/// Pressure threshold above which aggressive pruning engages.
pub const PRESSURE_AGGRESSIVE_PRUNE: f64 = 0.90;

/// Select the compaction tier from a raw or predictive pressure value.
///
/// Thresholds are defined as constants (`PRESSURE_TRIM_SCHEMAS`,
/// `PRESSURE_COMPACT_HISTORY`, `PRESSURE_AGGRESSIVE_PRUNE`) shared with
/// the microcompact layer to eliminate comment-only alignment.
#[must_use]
pub fn select_compaction_tier(pressure: f64) -> CompactionTier {
    if pressure >= PRESSURE_AGGRESSIVE_PRUNE {
        CompactionTier::AggressivePrune
    } else if pressure >= PRESSURE_COMPACT_HISTORY {
        CompactionTier::CompactHistory
    } else if pressure >= PRESSURE_TRIM_SCHEMAS {
        CompactionTier::TrimSchemas
    } else {
        CompactionTier::Normal
    }
}

/// Gated tier selection: predictive can only INCREASE the tier, never
/// decrease it below what raw pressure alone would select. This prevents
/// a temporarily inflated reserve estimate from under-compacting.
#[must_use]
pub fn select_tier_gated(raw_pressure: f64, predictive_pressure: f64) -> CompactionTier {
    let raw_tier = select_compaction_tier(raw_pressure);
    let predictive_tier = select_compaction_tier(predictive_pressure);
    raw_tier.max(predictive_tier)
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
        allocations.insert(SectionKind::SelfModel, remaining / 4);
        allocations.insert(SectionKind::Skills, remaining / 4);
        allocations.insert(SectionKind::ProjectContext, remaining / 4);
        allocations.insert(SectionKind::RuntimeIdentity, remaining / 4);

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
        assert_eq!(select_compaction_tier(0.55), CompactionTier::Normal);
        assert_eq!(select_compaction_tier(0.60), CompactionTier::TrimSchemas);
        assert_eq!(select_compaction_tier(0.65), CompactionTier::TrimSchemas);
        assert_eq!(select_compaction_tier(0.75), CompactionTier::CompactHistory);
        assert_eq!(select_compaction_tier(0.80), CompactionTier::CompactHistory);
        assert_eq!(
            select_compaction_tier(0.90),
            CompactionTier::AggressivePrune
        );
        assert_eq!(
            select_compaction_tier(0.92),
            CompactionTier::AggressivePrune
        );
        assert_eq!(
            select_compaction_tier(1.05),
            CompactionTier::AggressivePrune
        );
    }

    #[test]
    fn tier_escalation_for_recovery() {
        let mut r = RecoveryState::default();
        r.record_ptl_error();
        let base = CompactionTier::Normal;
        let escalated = base.escalate_for_recovery(&r);
        assert!(escalated > base, "1 PTL should escalate Normal");

        r.record_ptl_error();
        let escalated2 = CompactionTier::Normal.escalate_for_recovery(&r);
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
        let tier = select_tier_gated(0.80, 0.55);
        assert_eq!(tier, CompactionTier::CompactHistory);

        // raw = 0.55 (Normal), predictive = 0.80 (CompactHistory)
        // Gated should escalate to CompactHistory
        let tier2 = select_tier_gated(0.55, 0.80);
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
            let gated = select_tier_gated(raw, predictive);
            prop_assert!(gated >= select_compaction_tier(raw));
            prop_assert!(gated >= select_compaction_tier(predictive));
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
