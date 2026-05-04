//! Token budget allocation and compaction tier selection for the context pipeline.
//!
//! The planner uses these functions to decide how aggressively to compact
//! based on current pressure and recovery state.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::compaction_types::CompactionTier;
use crate::section_types::SectionKind;

/// Select the compaction tier from a raw or predictive pressure value.
///
/// Thresholds align with `AdaptiveCompactConfig::from_pressure()` in
/// microcompact.rs, but this function returns the tier (policy) rather
/// than the adaptive parameters (mechanism).
#[must_use]
pub fn select_compaction_tier(pressure: f64) -> CompactionTier {
    if pressure >= 0.90 {
        CompactionTier::AggressivePrune
    } else if pressure >= 0.75 {
        CompactionTier::CompactHistory
    } else if pressure >= 0.60 {
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
    #[must_use]
    pub fn allocate(
        effective_limit: u32,
        tier: CompactionTier,
        _section_history: &HashMap<SectionKind, u32>,
    ) -> Self {
        let limit = effective_limit as f64;

        let fixed_budget = (limit * 0.10).min(2000.0) as u32;
        let history_ratio = match tier {
            CompactionTier::Normal => 0.50,
            CompactionTier::TrimSchemas => 0.40,
            CompactionTier::CompactHistory => 0.30,
            CompactionTier::AggressivePrune => 0.15,
        };
        let memory_ratio = match tier {
            CompactionTier::Normal => 0.15,
            CompactionTier::TrimSchemas => 0.12,
            CompactionTier::CompactHistory => 0.08,
            CompactionTier::AggressivePrune => 0.05,
        };

        let mut allocations = HashMap::new();
        allocations.insert(SectionKind::Identity, fixed_budget);
        allocations.insert(SectionKind::Constraints, fixed_budget);
        allocations.insert(SectionKind::History, (limit * history_ratio) as u32);
        allocations.insert(SectionKind::Memory, (limit * memory_ratio) as u32);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery_state::RecoveryState;

    #[test]
    fn tier_from_pressure_boundaries() {
        assert_eq!(select_compaction_tier(0.55), CompactionTier::Normal);
        assert_eq!(select_compaction_tier(0.60), CompactionTier::TrimSchemas);
        assert_eq!(select_compaction_tier(0.65), CompactionTier::TrimSchemas);
        assert_eq!(select_compaction_tier(0.75), CompactionTier::CompactHistory);
        assert_eq!(select_compaction_tier(0.80), CompactionTier::CompactHistory);
        assert_eq!(select_compaction_tier(0.90), CompactionTier::AggressivePrune);
        assert_eq!(select_compaction_tier(0.92), CompactionTier::AggressivePrune);
        assert_eq!(select_compaction_tier(1.05), CompactionTier::AggressivePrune);
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
        assert!(escalated2 >= CompactionTier::CompactHistory, "2 PTL should reach CompactHistory+");
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

    #[test]
    fn higher_tier_gets_tighter_history_budget() {
        let history = HashMap::new();
        let normal = TokenBudget::allocate(100_000, CompactionTier::Normal, &history);
        let aggressive = TokenBudget::allocate(100_000, CompactionTier::AggressivePrune, &history);
        assert!(
            normal.budget_for(SectionKind::History) > aggressive.budget_for(SectionKind::History),
            "Normal should have more history budget than AggressivePrune"
        );
    }
}
