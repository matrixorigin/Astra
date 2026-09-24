//! Resolved context and compaction settings shared by admission and consumers.
//! Resolution takes explicit inputs; this module never reads process settings.
use serde::{Deserialize, Serialize};

pub const PRESSURE_TRIM_SCHEMAS: f64 = 0.60;
pub const PRESSURE_COMPACT_HISTORY: f64 = 0.75;
pub const PRESSURE_AGGRESSIVE_PRUNE: f64 = 0.90;

/// Token-budget compaction tier based on context window usage.
///
/// Variants are declared in ascending order of aggressiveness. `PartialOrd`/`Ord`
/// derive ordinal comparison from this order, so guards like
/// `tier < CompactionTier::CompactHistory` and escalation via `tier.max(other)`
/// rely on keeping new variants inserted at the correct position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTier {
    /// No compaction action is needed.
    Normal,
    /// Reduce dynamic tool schemas to free headroom.
    TrimSchemas,
    /// Compact older conversation turns, preserving recent turns.
    CompactHistory,
    /// Aggressively prune and summarize history.
    AggressivePrune,
}

impl Default for CompactionTier {
    /// `Normal` — no compaction has been applied yet.
    fn default() -> Self {
        Self::Normal
    }
}

impl CompactionTier {
    /// Scalar 0.0–0.9 for edge tool output scaling / selection.
    #[must_use]
    pub fn budget_pressure(self) -> f64 {
        match self {
            Self::Normal => 0.0,
            Self::TrimSchemas => 0.3,
            Self::CompactHistory => 0.6,
            Self::AggressivePrune => 0.9,
        }
    }

    /// Escalate the tier based on recovery state. After prompt-too-long
    /// errors, the planner forces a more aggressive tier than pressure
    /// alone would dictate.
    #[must_use]
    pub fn escalate_for_recovery(self, consecutive_ptl_errors: u32) -> Self {
        let min_tier = match consecutive_ptl_errors {
            0 => Self::Normal,
            1 => Self::TrimSchemas,
            2 => Self::CompactHistory,
            _ => Self::AggressivePrune,
        };
        self.max(min_tier)
    }

    // ── Resolved-policy thresholds ──────────────────────────────────

    /// Pre-turn trigger measured against the already-resolved usable-input
    /// limit. The limit, rather than this ratio, carries catalog and reserve
    /// differences; branching on raw window size here would apply a second,
    /// unmeasured heuristic.
    #[must_use]
    pub fn pre_turn_trigger(_usable_input_tokens: u64) -> f64 {
        PRESSURE_COMPACT_HISTORY
    }

    /// Warn ten percentage points before the resolved trigger.
    #[must_use]
    pub fn pre_turn_warning(_usable_input_tokens: u64) -> f64 {
        PRESSURE_COMPACT_HISTORY - 0.10
    }

    /// Aggressive trigger shared with the pipeline budget selector.
    #[must_use]
    pub fn aggressive_trigger(_usable_input_tokens: u64) -> f64 {
        PRESSURE_AGGRESSIVE_PRUNE
    }
}

/// Configuration for LLM-based compaction summary (Phase 2 feature).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactConfig {
    /// Enable LLM-generated summary instead of pure truncation.
    /// Defaults to `true`.
    pub enable_summary: bool,
    /// Maximum tokens to generate for the summary.
    pub summary_token_budget: usize,
    /// Maximum PTL retry attempts before falling back to truncation.
    pub max_ptl_retries: usize,
    /// Minimum compaction tier that triggers LLM summary.
    /// Defaults to CompactHistory (75%+ context usage).
    pub summary_min_tier: CompactionTier,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            enable_summary: true,
            summary_token_budget: 20_000,
            max_ptl_retries: 3,
            summary_min_tier: CompactionTier::CompactHistory,
        }
    }
}

impl CompactConfig {
    /// Returns true if LLM summary should be attempted for the given tier.
    pub fn should_summarize(&self, tier: CompactionTier) -> bool {
        if !self.enable_summary {
            return false;
        }
        let tier_level = |t: CompactionTier| match t {
            CompactionTier::Normal => 0,
            CompactionTier::TrimSchemas => 1,
            CompactionTier::CompactHistory => 2,
            CompactionTier::AggressivePrune => 3,
        };
        tier_level(tier) >= tier_level(self.summary_min_tier)
    }
}

pub const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 200_000;
pub const DEFAULT_OUTPUT_RESERVE_TOKENS: usize = 16_384;
pub const DEFAULT_PROTOCOL_RESERVE_TOKENS: usize = 300;

/// Provenance for a resolved context-window policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextWindowPolicySource {
    /// Both the raw window and completion limit came from the model catalog.
    ModelCatalog,
    /// The catalog supplied only part of the limit metadata.
    PartialModelCatalog,
    /// No catalog window was available, so the documented generic fallback
    /// was used. No model-name matching is performed.
    GenericFallback,
}

/// One exact token policy shared by assembly, wire preflight, trace, and
/// compaction-effectiveness checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextWindowPolicy {
    pub raw_context_window_tokens: usize,
    pub usable_input_limit_tokens: usize,
    pub reserved_output_tokens: usize,
    pub reserved_summary_tokens: usize,
    pub reserved_protocol_tokens: usize,
    pub auto_compact_trigger_tokens: usize,
    pub hard_input_limit_tokens: usize,
    pub source: ContextWindowPolicySource,
}

impl ContextWindowPolicy {
    #[must_use]
    fn resolve(
        context_window_tokens: Option<u32>,
        max_completion_tokens: Option<u32>,
        summary_reserve_tokens: usize,
        compact_threshold: f64,
    ) -> Self {
        let catalog_context_window = context_window_tokens.filter(|tokens| *tokens > 0);
        let catalog_max_completion = max_completion_tokens.filter(|tokens| *tokens > 0);
        let raw = catalog_context_window
            .map(|tokens| tokens as usize)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);
        // Catalog metadata is authoritative. The fallback is one fixed
        // documented reserve, clamped only to keep malformed/tiny windows
        // arithmetically valid; it never branches on provider/model text.
        let output = catalog_max_completion
            .map(|tokens| tokens as usize)
            .unwrap_or_else(|| DEFAULT_OUTPUT_RESERVE_TOKENS.min((raw / 4).max(1)))
            .min(raw.saturating_sub(1));
        let protocol = DEFAULT_PROTOCOL_RESERVE_TOKENS.min(raw.saturating_sub(output));
        let hard_input = raw.saturating_sub(output).saturating_sub(protocol);
        let summary = summary_reserve_tokens.min(hard_input / 4);
        let usable_input = hard_input.saturating_sub(summary);
        let threshold = if compact_threshold.is_finite() {
            compact_threshold.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let auto_compact_trigger = (usable_input as f64 * threshold)
            .floor()
            .min(usize::MAX as f64) as usize;
        let source = match (catalog_context_window, catalog_max_completion) {
            (Some(_), Some(_)) => ContextWindowPolicySource::ModelCatalog,
            (Some(_), None) | (None, Some(_)) => ContextWindowPolicySource::PartialModelCatalog,
            (None, None) => ContextWindowPolicySource::GenericFallback,
        };
        Self {
            raw_context_window_tokens: raw,
            usable_input_limit_tokens: usable_input,
            reserved_output_tokens: output,
            reserved_summary_tokens: summary,
            reserved_protocol_tokens: protocol,
            auto_compact_trigger_tokens: auto_compact_trigger,
            hard_input_limit_tokens: hard_input,
            source,
        }
    }

    /// Exit-gate target: a successful compaction must land at least ten
    /// percentage points below the trigger, measured against usable input.
    #[must_use]
    pub fn post_compaction_target_tokens(self) -> usize {
        self.auto_compact_trigger_tokens
            .saturating_sub(self.usable_input_limit_tokens / 10)
    }
}

/// Exact admitted context settings. There is no ratio or unresolved fallback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextBudget {
    pub window_policy: ContextWindowPolicy,
    pub compact_threshold: f64,
    pub keep_recent_turns: usize,
    pub memory_budget_chars: usize,
    pub compact_config: CompactConfig,
}

impl ContextBudget {
    /// Canonical resolution from captured model and runtime settings.
    /// Zero/missing model limits retain the documented generic fallback.
    pub fn resolve(
        context_window_tokens: Option<u32>,
        max_completion_tokens: Option<u32>,
        compact_threshold: f64,
        keep_recent_turns: usize,
        memory_budget_chars: usize,
        compact_config: CompactConfig,
    ) -> Self {
        let compact_threshold = if compact_threshold.is_finite() {
            compact_threshold.clamp(0.0, 1.0)
        } else {
            0.0
        };
        Self {
            window_policy: ContextWindowPolicy::resolve(
                context_window_tokens,
                max_completion_tokens,
                compact_config.summary_token_budget,
                compact_threshold,
            ),
            compact_threshold,
            keep_recent_turns,
            memory_budget_chars,
            compact_config,
        }
    }
    pub fn model_limit(&self) -> usize {
        self.window_policy.raw_context_window_tokens
    }
    pub fn effective_input_limit(&self) -> usize {
        self.window_policy.usable_input_limit_tokens
    }
    pub fn compact_trigger(&self) -> usize {
        self.window_policy.auto_compact_trigger_tokens
    }
    pub fn window_policy(&self) -> ContextWindowPolicy {
        self.window_policy
    }
    pub fn capped_output_tokens(&self) -> usize {
        self.window_policy.reserved_output_tokens
    }
    /// Whether the given token count exceeds the compact trigger.
    pub fn should_compact(&self, estimated_tokens: usize) -> bool {
        estimated_tokens > self.compact_trigger()
    }

    /// Determine the compaction tier for the current token usage.
    ///
    /// The tier boundaries are scaled relative to `compact_threshold`:
    /// - TrimSchemas: starts at 80% of compact_threshold
    /// - CompactHistory: starts at compact_threshold
    /// - AggressivePrune: starts at 113% of compact_threshold
    ///
    /// With default compact_threshold=0.75, this gives ~60%/75%/85% boundaries.
    /// With aggressive compact_threshold=0.60, boundaries become ~48%/60%/68%.
    pub fn compaction_tier(&self, estimated_tokens: usize) -> CompactionTier {
        self.compaction_thresholds()
            .select_tier(estimated_tokens as f64 / self.effective_input_limit() as f64)
    }

    pub fn compaction_thresholds(&self) -> CompactionThresholds {
        CompactionThresholds::from_compact_threshold(self.compact_threshold)
    }
}

/// Resolved tier boundaries shared by context pressure and the pipeline planner.
/// Comparisons deliberately retain ContextBudget's strict `>` boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionThresholds {
    pub trim_schemas: f64,
    pub compact_history: f64,
    pub aggressive_prune: f64,
}

impl CompactionThresholds {
    fn from_compact_threshold(threshold: f64) -> Self {
        Self {
            trim_schemas: threshold * 0.80,
            compact_history: threshold,
            aggressive_prune: threshold * 1.133,
        }
    }

    pub fn select_tier(self, pressure: f64) -> CompactionTier {
        if pressure > self.aggressive_prune {
            CompactionTier::AggressivePrune
        } else if pressure > self.compact_history {
            CompactionTier::CompactHistory
        } else if pressure > self.trim_schemas {
            CompactionTier::TrimSchemas
        } else {
            CompactionTier::Normal
        }
    }
}

impl Default for CompactionThresholds {
    fn default() -> Self {
        Self::from_compact_threshold(PRESSURE_COMPACT_HISTORY)
    }
}

/// Configuration for Memoria-based compaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoriaCompactConfig {
    /// Minimum tokens before attempting Memoria retrieval.
    pub min_tokens_for_retrieval: usize,
    /// Maximum memories to retrieve for context.
    pub max_memories: usize,
    /// Maximum prompt tokens reserved for non-snapshot working memories.
    pub max_memory_tokens: usize,
}

impl Default for MemoriaCompactConfig {
    fn default() -> Self {
        Self {
            min_tokens_for_retrieval: 5_000,
            max_memories: 10,
            max_memory_tokens: 4_000,
        }
    }
}

/// Configuration for turn-count-based microcompaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnCountCompactConfig {
    pub enabled: bool,
    pub trigger_threshold: usize,
    pub keep_recent: usize,
}

impl Default for TurnCountCompactConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger_threshold: 8,
            keep_recent: 3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_policy_preserves_explicit_limits_and_compaction_settings() {
        let compact = CompactConfig {
            enable_summary: false,
            summary_token_budget: 3_000,
            max_ptl_retries: 1,
            summary_min_tier: CompactionTier::AggressivePrune,
        };
        let budget =
            ContextBudget::resolve(Some(128_000), Some(32_000), 0.6, 9, 4_000, compact.clone());
        assert_eq!(budget.model_limit(), 128_000);
        assert_eq!(budget.capped_output_tokens(), 32_000);
        assert_eq!(budget.effective_input_limit(), 92_700);
        assert_eq!(budget.compact_trigger(), 55_620);
        assert!(!budget.should_compact(55_620));
        assert!(budget.should_compact(55_621));
        assert_eq!(budget.keep_recent_turns, 9);
        assert_eq!(budget.memory_budget_chars, 4_000);
        assert_eq!(budget.compact_config, compact);
        assert_eq!(
            budget.window_policy.source,
            ContextWindowPolicySource::ModelCatalog
        );
        let encoded = serde_json::to_value(&budget).unwrap();
        assert_eq!(
            budget,
            serde_json::from_value::<ContextBudget>(encoded.clone()).unwrap()
        );
        let mut missing = encoded;
        missing.as_object_mut().unwrap().remove("window_policy");
        assert!(serde_json::from_value::<ContextBudget>(missing).is_err());
        let mut legacy = serde_json::to_value(&budget).unwrap();
        legacy["output_reserve_ratio"] = serde_json::json!(0.1);
        assert!(serde_json::from_value::<ContextBudget>(legacy).is_err());
    }

    #[test]
    fn canonical_fallback_and_tiny_windows_have_exact_reserves() {
        let generic = ContextBudget::resolve(None, None, 0.75, 6, 8_000, CompactConfig::default());
        let zero =
            ContextBudget::resolve(Some(0), Some(0), 0.75, 6, 8_000, CompactConfig::default());
        assert_eq!(generic, zero);
        assert_eq!(generic.effective_input_limit(), 163_316);
        assert_eq!(generic.capped_output_tokens(), 16_384);
        for window in [1, 2, 10, 300, 4_096] {
            let budget = ContextBudget::resolve(
                Some(window),
                Some(64_000),
                0.75,
                6,
                8_000,
                CompactConfig::default(),
            );
            assert!(budget.capped_output_tokens() < window as usize);
            assert!(budget.effective_input_limit() <= window as usize);
            assert!(budget.compact_trigger() <= budget.effective_input_limit());
            assert_eq!(
                budget.compaction_tier(10_000),
                CompactionTier::AggressivePrune
            );
        }
    }

    #[test]
    fn tier_boundaries_follow_the_resolved_threshold() {
        for threshold in [0.0, 0.6, 0.75, 1.0, f64::NAN] {
            let budget = ContextBudget::resolve(
                Some(100_000),
                Some(4_000),
                threshold,
                6,
                8_000,
                CompactConfig::default(),
            );
            assert!(budget.compact_threshold.is_finite());
            assert_eq!(budget.compaction_tier(0), CompactionTier::Normal);
            assert_eq!(
                budget.compaction_tier(budget.effective_input_limit() * 2),
                CompactionTier::AggressivePrune
            );
            if budget.compact_threshold > 0.0 {
                assert_eq!(
                    budget.compaction_tier(budget.compact_trigger()),
                    CompactionTier::TrimSchemas
                );
                assert_eq!(
                    budget.compaction_tier(budget.compact_trigger() + 1),
                    CompactionTier::CompactHistory
                );
            }
        }
    }
}
