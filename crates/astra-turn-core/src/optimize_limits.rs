//! Gated optimizer limits for the context pipeline.
//!
//! Each optimization transformation has an independent boolean gate.
//! If a gate is closed, the step is skipped and the trace records
//! `skipped(reason)`. This makes the optimizer auditable.

use serde::{Deserialize, Serialize};

/// Per-turn gate struct controlling which optimizations are allowed.
///
/// The optimizer checks each gate before acting. Closed gates produce
/// trace entries showing what *could have* happened but didn't.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizeLimits {
    /// Reorder sections within explicitly marked reorderable groups.
    pub allow_reorder: bool,
    /// Clear old tool results (microcompact).
    pub allow_tool_result_clearing: bool,
    /// Prune tool schemas under pressure.
    pub allow_schema_pruning: bool,
    /// Spill oversized content to disk.
    pub allow_spill: bool,
    /// LLM-based history summarization (expensive, lossy).
    pub allow_llm_summary: bool,
    /// Drop entire API rounds (emergency only).
    pub allow_round_dropping: bool,
    /// Maximum number of sections that can be reordered per turn.
    pub max_reorder_moves: u32,
    /// Maximum tokens that can be cleared in a single optimize call.
    /// Circuit breaker: if clearing would exceed this, stop and emit a trace warning.
    pub max_clear_tokens: u32,
}

impl Default for OptimizeLimits {
    fn default() -> Self {
        Self {
            allow_reorder: false,
            allow_tool_result_clearing: true,
            allow_schema_pruning: true,
            allow_spill: true,
            allow_llm_summary: true,
            allow_round_dropping: true,
            max_reorder_moves: 2,
            max_clear_tokens: 100_000,
        }
    }
}

impl OptimizeLimits {
    /// All gates closed — no transformations allowed.
    #[must_use]
    pub fn all_closed() -> Self {
        Self {
            allow_reorder: false,
            allow_tool_result_clearing: false,
            allow_schema_pruning: false,
            allow_spill: false,
            allow_llm_summary: false,
            allow_round_dropping: false,
            max_reorder_moves: 0,
            max_clear_tokens: 0,
        }
    }

    /// Build limits appropriate for a given compaction tier.
    ///
    /// Gates are progressively opened as pressure increases:
    /// - Normal: tool_result_clearing only (defensive)
    /// - TrimSchemas: + schema_pruning
    /// - CompactHistory: + spill
    /// - AggressivePrune: + round_dropping
    #[must_use]
    pub fn for_tier(tier: crate::compaction_types::CompactionTier, model_limit: u32) -> Self {
        use crate::compaction_types::CompactionTier;

        match tier {
            CompactionTier::Normal => Self {
                allow_reorder: false,
                allow_tool_result_clearing: true,
                allow_schema_pruning: false,
                allow_spill: false,
                allow_llm_summary: false,
                allow_round_dropping: false,
                max_reorder_moves: 0,
                max_clear_tokens: model_limit / 2,
            },
            CompactionTier::TrimSchemas => Self {
                allow_reorder: false,
                allow_tool_result_clearing: true,
                allow_schema_pruning: true,
                allow_spill: false,
                allow_llm_summary: false,
                allow_round_dropping: false,
                max_reorder_moves: 0,
                max_clear_tokens: model_limit / 2,
            },
            CompactionTier::CompactHistory => Self {
                allow_reorder: false,
                allow_tool_result_clearing: true,
                allow_schema_pruning: true,
                allow_spill: true,
                allow_llm_summary: false,
                allow_round_dropping: false,
                max_reorder_moves: 2,
                max_clear_tokens: model_limit / 2,
            },
            CompactionTier::AggressivePrune => Self {
                allow_reorder: true,
                allow_tool_result_clearing: true,
                allow_schema_pruning: true,
                allow_spill: true,
                allow_llm_summary: true,
                allow_round_dropping: true,
                max_reorder_moves: 4,
                max_clear_tokens: model_limit / 2,
            },
        }
    }

    /// Whether any transformation gate is open.
    #[must_use]
    pub fn any_open(&self) -> bool {
        self.allow_reorder
            || self.allow_tool_result_clearing
            || self.allow_schema_pruning
            || self.allow_spill
            || self.allow_llm_summary
            || self.allow_round_dropping
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction_types::CompactionTier;

    #[test]
    fn default_gates_match_spec() {
        let d = OptimizeLimits::default();
        assert!(!d.allow_reorder, "reorder should be off by default");
        assert!(d.allow_tool_result_clearing);
        assert!(d.allow_schema_pruning);
        assert!(d.allow_spill);
        assert!(d.allow_llm_summary);
        assert!(d.allow_round_dropping);
        assert_eq!(d.max_reorder_moves, 2);
        assert_eq!(d.max_clear_tokens, 100_000);
    }

    #[test]
    fn all_closed_blocks_everything() {
        let c = OptimizeLimits::all_closed();
        assert!(!c.allow_reorder);
        assert!(!c.allow_tool_result_clearing);
        assert!(!c.allow_schema_pruning);
        assert!(!c.allow_spill);
        assert!(!c.allow_llm_summary);
        assert!(!c.allow_round_dropping);
        assert_eq!(c.max_reorder_moves, 0);
        assert_eq!(c.max_clear_tokens, 0);
        assert!(!c.any_open());
    }

    #[test]
    fn any_open_detects_single_gate() {
        let mut c = OptimizeLimits::all_closed();
        assert!(!c.any_open());
        c.allow_spill = true;
        assert!(c.any_open());
    }

    #[test]
    fn for_tier_normal_is_conservative() {
        let l = OptimizeLimits::for_tier(CompactionTier::Normal, 200_000);
        assert!(l.allow_tool_result_clearing);
        assert!(!l.allow_schema_pruning);
        assert!(!l.allow_spill);
        assert!(!l.allow_round_dropping);
        assert!(!l.allow_reorder);
        assert_eq!(l.max_clear_tokens, 100_000);
    }

    #[test]
    fn for_tier_trim_schemas_opens_pruning() {
        let l = OptimizeLimits::for_tier(CompactionTier::TrimSchemas, 200_000);
        assert!(l.allow_schema_pruning);
        assert!(!l.allow_spill);
        assert!(!l.allow_round_dropping);
    }

    #[test]
    fn for_tier_compact_history_opens_spill() {
        let l = OptimizeLimits::for_tier(CompactionTier::CompactHistory, 200_000);
        assert!(l.allow_schema_pruning);
        assert!(l.allow_spill);
        assert!(!l.allow_round_dropping);
        assert_eq!(l.max_reorder_moves, 2);
    }

    #[test]
    fn for_tier_aggressive_opens_all() {
        let l = OptimizeLimits::for_tier(CompactionTier::AggressivePrune, 200_000);
        assert!(l.allow_reorder);
        assert!(l.allow_schema_pruning);
        assert!(l.allow_spill);
        assert!(l.allow_round_dropping);
        assert!(l.allow_llm_summary);
        assert_eq!(l.max_reorder_moves, 4);
    }

    #[test]
    fn for_tier_monotonically_opens_gates() {
        let tiers = [
            CompactionTier::Normal,
            CompactionTier::TrimSchemas,
            CompactionTier::CompactHistory,
            CompactionTier::AggressivePrune,
        ];
        let mut prev_open_count = 0u32;
        for tier in tiers {
            let l = OptimizeLimits::for_tier(tier, 128_000);
            let open_count = [
                l.allow_reorder,
                l.allow_tool_result_clearing,
                l.allow_schema_pruning,
                l.allow_spill,
                l.allow_llm_summary,
                l.allow_round_dropping,
            ]
            .iter()
            .filter(|&&b| b)
            .count() as u32;
            assert!(
                open_count >= prev_open_count,
                "{tier:?}: open_count {open_count} < prev {prev_open_count}"
            );
            prev_open_count = open_count;
        }
    }
}
