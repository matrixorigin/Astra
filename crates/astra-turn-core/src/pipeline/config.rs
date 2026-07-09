//! Pipeline configuration and provider cache policy.

use serde::{Deserialize, Serialize};

use crate::context_budget::TierThresholds;
use crate::microcompact::{CompactStrategy, PromptCacheProtocol};
use crate::optimize_limits::OptimizeLimits;
use crate::pipeline_stats::ReservePercentiles;

/// Provider-level cache policy consumed by the optimizer.
///
/// Each provider must declare its capabilities before it can execute
/// through the pipeline. This drives marker placement, scope selection,
/// and fork/skip-cache-write behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCachePolicy {
    /// Prompt caching protocol (prefix-only vs Anthropic cache_control).
    pub protocol: PromptCacheProtocol,
    /// Compaction placeholder style (Normalized vs Minimal).
    pub compact_strategy: CompactStrategy,
    /// Maximum cache markers this provider supports per request.
    pub max_markers: u32,
    /// Whether `scope: global` or equivalent is supported.
    pub supports_global_scope: bool,
    /// Fork/side-query behavior that reuses a prefix without polluting the main cache.
    pub supports_skip_cache_write: bool,
}

impl Default for ProviderCachePolicy {
    fn default() -> Self {
        Self {
            protocol: PromptCacheProtocol::Prefix,
            compact_strategy: CompactStrategy::Normalized,
            max_markers: 0,
            supports_global_scope: false,
            supports_skip_cache_write: false,
        }
    }
}

impl ProviderCachePolicy {
    /// Anthropic-style provider with cache_control support.
    #[must_use]
    pub fn anthropic() -> Self {
        Self {
            protocol: PromptCacheProtocol::AnthropicCacheControl,
            compact_strategy: CompactStrategy::Minimal,
            max_markers: 4,
            supports_global_scope: true,
            supports_skip_cache_write: true,
        }
    }

    /// OpenAI-compatible provider with prefix caching.
    #[must_use]
    pub fn openai_compatible() -> Self {
        Self {
            protocol: PromptCacheProtocol::Prefix,
            compact_strategy: CompactStrategy::Normalized,
            max_markers: 0,
            supports_global_scope: false,
            supports_skip_cache_write: false,
        }
    }
}

/// How the planner turns pressure into a compaction tier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PressureMode {
    /// Tier from `max(raw, predictive)` pressure — prediction can only
    /// escalate (production default).
    #[default]
    Predictive,
    /// Tier from raw pressure only. Predictive pressure is still computed
    /// and traced, but does not influence tier selection. Ablation arm for
    /// the predictive-vs-reactive experiment.
    Reactive,
}

/// How the pipeline arranges and transforms sections.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssemblerMode {
    /// Full pipeline: tier-gated compaction, spill, cache-marker placement
    /// (production default).
    #[default]
    Structured,
    /// Naive stable-prefix-then-append baseline: every optimizer gate is
    /// forced closed and no cache markers are emitted. Sections keep their
    /// bind order. Baseline arm for controlled assembler comparisons.
    Flat,
}

/// Planner decision policy — the deterministic knobs the Plan phase reads.
///
/// Every field defaults to the production constants, so `PlanPolicy::default()`
/// reproduces current behavior exactly. Experiments override individual knobs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlanPolicy {
    /// Predictive (gated) vs reactive (raw-only) tier selection.
    pub pressure_mode: PressureMode,
    /// Pressure ladder for compaction-tier selection.
    pub tier_thresholds: TierThresholds,
    /// Percentiles the reserve estimator reads (steady / recovery).
    pub reserve_percentiles: ReservePercentiles,
}

/// Top-level pipeline configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PipelineConfig {
    /// Provider cache policy for the current session.
    pub provider_policy: ProviderCachePolicy,
    /// Planner decision policy (pressure mode, thresholds, percentiles).
    pub plan_policy: PlanPolicy,
    /// Structured pipeline vs flat baseline assembly.
    pub assembler_mode: AssemblerMode,
    /// When set, replaces tier-derived `OptimizeLimits` in
    /// `cascade_aware_limits` (cascade suppression still applies on top).
    /// Experiment hook for optimizer-knob sweeps; `None` in production.
    pub limits_override: Option<OptimizeLimits>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_default_policy() {
        let c = PipelineConfig::default();
        assert_eq!(c.provider_policy.max_markers, 0);
        assert!(!c.provider_policy.supports_global_scope);
    }

    #[test]
    fn default_plan_policy_reproduces_production_constants() {
        let c = PipelineConfig::default();
        assert_eq!(c.plan_policy.pressure_mode, PressureMode::Predictive);
        assert_eq!(c.assembler_mode, AssemblerMode::Structured);
        assert!(c.limits_override.is_none());
        assert_eq!(c.plan_policy.tier_thresholds.trim_schemas, 0.60);
        assert_eq!(c.plan_policy.tier_thresholds.compact_history, 0.75);
        assert_eq!(c.plan_policy.tier_thresholds.aggressive_prune, 0.90);
        assert_eq!(c.plan_policy.reserve_percentiles.steady, 0.75);
        assert_eq!(c.plan_policy.reserve_percentiles.recovery, 0.95);
    }

    #[test]
    fn legacy_config_json_without_new_fields_deserializes() {
        // Configs persisted before PlanPolicy/AssemblerMode existed must load.
        let policy_json = serde_json::to_string(&ProviderCachePolicy::default()).unwrap();
        let json = format!(r#"{{"provider_policy":{policy_json}}}"#);
        let c: PipelineConfig = serde_json::from_str(&json).expect("legacy config should load");
        assert_eq!(c.plan_policy.pressure_mode, PressureMode::Predictive);
        assert_eq!(c.assembler_mode, AssemblerMode::Structured);
    }

    #[test]
    fn anthropic_policy_has_cache_control() {
        let p = ProviderCachePolicy::anthropic();
        assert_eq!(p.protocol, PromptCacheProtocol::AnthropicCacheControl);
        assert!(p.supports_global_scope);
        assert!(p.max_markers > 0);
    }

    #[test]
    fn openai_policy_uses_prefix() {
        let p = ProviderCachePolicy::openai_compatible();
        assert_eq!(p.protocol, PromptCacheProtocol::Prefix);
        assert!(!p.supports_global_scope);
        assert_eq!(p.max_markers, 0);
    }
}
