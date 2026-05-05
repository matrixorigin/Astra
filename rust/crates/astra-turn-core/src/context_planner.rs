//! Context pipeline Plan phase — pure function, zero I/O.
//!
//! The planner reads immutable state (token counts, model config, latches,
//! recovery, statistics) and produces a `ContextPlan` describing what the
//! turn needs: which sections to include, their budgets, the compaction
//! tier, and the cache strategy.

use serde::{Deserialize, Serialize};

use crate::compaction_types::CompactionTier;
use crate::context_budget::{TokenBudget, select_tier_gated};
use crate::context_pressure::{ContextPressure, ContextReserves};
use crate::microcompact::PromptCacheProtocol;
use crate::pipeline_config::ProviderCachePolicy;
use crate::pipeline_stats::PipelineStats;
use crate::recovery_state::RecoveryState;
use crate::section_types::{
    CacheScope, CompressionPriority, PlannedSection, SectionKind, SectionSource,
};
use crate::session_latches::SessionLatches;
use crate::token_accounting::TokenAccounting;

/// Cache strategy selected by the planner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStrategy {
    pub protocol: PromptCacheProtocol,
    pub use_global_scope: bool,
    pub max_markers: u32,
}

/// The output of the Plan phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPlan {
    pub sections: Vec<PlannedSection>,
    pub budget: TokenBudget,
    pub compact_tier: CompactionTier,
    pub cache_strategy: CacheStrategy,
    pub pressure: ContextPressure,
    pub reserves: ContextReserves,
}

/// Inputs to the planner — everything it needs to make a decision.
/// All references are immutable (Plan is a pure function).
pub struct PlanInput<'a> {
    pub tokens: &'a TokenAccounting,
    pub model_limit: u32,
    pub recovery: &'a RecoveryState,
    pub latches: &'a SessionLatches,
    pub stats: &'a PipelineStats,
    pub provider_policy: &'a ProviderCachePolicy,
    /// Whether memory text has already been retrieved by the runtime.
    pub has_memory: bool,
    /// Model ID for reserve estimation bucketing.
    pub model_id: &'a str,
    /// Query source for reserve estimation bucketing.
    pub query_source: &'a str,
}

/// Plan a turn: compute pressure, select tier, allocate budgets, choose cache strategy.
///
/// This is a pure function with no I/O. It produces a `ContextPlan` that
/// the Bind and Optimize phases will execute.
#[must_use]
pub fn plan_turn(input: &PlanInput<'_>) -> ContextPlan {
    // 1. Compute reserves from historical response data
    let reserves = input.stats.response_token_estimates.reserve_for(
        input.model_id,
        input.query_source,
        input.recovery,
    );

    // 2. Compute raw and predictive pressure
    let pressure = ContextPressure::compute(
        input.tokens.total_input_u32_saturating(),
        input.model_limit,
        reserves,
    );

    // 3. Select compaction tier (gated: predictive can escalate, not de-escalate)
    let tier =
        select_tier_gated(pressure.raw, pressure.value).escalate_for_recovery(input.recovery);

    // 4. Allocate token budgets per section
    let section_history = input.stats.section_token_history();
    let budget = TokenBudget::allocate(input.model_limit, tier, &section_history);

    // 5. Choose cache strategy based on provider policy + latches
    let cache_strategy = plan_cache_strategy(input.provider_policy, input.latches);

    // 6. Build section manifest
    let sections = plan_section_manifest(&budget, input.has_memory);

    ContextPlan {
        sections,
        budget,
        compact_tier: tier,
        cache_strategy,
        pressure,
        reserves,
    }
}

/// Determine the cache strategy from provider policy and session latches.
fn plan_cache_strategy(policy: &ProviderCachePolicy, latches: &SessionLatches) -> CacheStrategy {
    let use_global_scope = policy.supports_global_scope
        && latches
            .cache_scope
            .map_or(true, |s| s == CacheScope::Global);

    CacheStrategy {
        protocol: policy.protocol,
        use_global_scope,
        max_markers: policy.max_markers,
    }
}

/// Build the section manifest: which sections to include and with what properties.
///
/// Identity and Constraints are always present. Memory is included only if
/// retrieval has already produced concrete snippets. Conversation history
/// travels in the provider messages array, not as a hollow system section.
/// Emergent sections are always included (the Bind phase will produce empty
/// BoundSections if there's nothing to inject).
fn plan_section_manifest(budget: &TokenBudget, has_memory: bool) -> Vec<PlannedSection> {
    let mut sections = vec![
        PlannedSection {
            kind: SectionKind::Identity,
            scope: CacheScope::Global,
            estimated_tokens: budget.budget_for(SectionKind::Identity),
            priority: CompressionPriority::Never,
            source: SectionSource::Static,
        },
        PlannedSection {
            kind: SectionKind::Constraints,
            scope: CacheScope::Global,
            estimated_tokens: budget.budget_for(SectionKind::Constraints),
            priority: CompressionPriority::Never,
            source: SectionSource::Static,
        },
        PlannedSection {
            kind: SectionKind::SelfModel,
            scope: CacheScope::Session,
            estimated_tokens: budget.budget_for(SectionKind::SelfModel),
            priority: CompressionPriority::LastResort,
            source: SectionSource::Environment,
        },
        PlannedSection {
            kind: SectionKind::ProjectContext,
            scope: CacheScope::Session,
            estimated_tokens: budget.budget_for(SectionKind::ProjectContext),
            priority: CompressionPriority::LastResort,
            source: SectionSource::Environment,
        },
        PlannedSection {
            kind: SectionKind::Skills,
            scope: CacheScope::Session,
            estimated_tokens: budget.budget_for(SectionKind::Skills),
            priority: CompressionPriority::Normal,
            source: SectionSource::Skill,
        },
        PlannedSection {
            kind: SectionKind::RuntimeIdentity,
            scope: CacheScope::None,
            estimated_tokens: budget.budget_for(SectionKind::RuntimeIdentity),
            priority: CompressionPriority::Normal,
            source: SectionSource::Environment,
        },
    ];

    if has_memory {
        sections.push(PlannedSection {
            kind: SectionKind::Memory,
            scope: CacheScope::None,
            estimated_tokens: budget.budget_for(SectionKind::Memory),
            priority: CompressionPriority::Normal,
            source: SectionSource::Memory,
        });
    }

    // Emergent sections always present (Bind produces empty if nothing to inject)
    for kind in [
        SectionKind::EmergentSkills,
        SectionKind::EmergentMemory,
        SectionKind::EmergentSummary,
    ] {
        sections.push(PlannedSection {
            kind,
            scope: CacheScope::None,
            estimated_tokens: 0,
            priority: CompressionPriority::First,
            source: SectionSource::Emergent,
        });
    }

    sections
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline_config::ProviderCachePolicy;

    fn default_input() -> (
        TokenAccounting,
        RecoveryState,
        SessionLatches,
        PipelineStats,
        ProviderCachePolicy,
    ) {
        (
            TokenAccounting::default(),
            RecoveryState::default(),
            SessionLatches::default(),
            PipelineStats::default(),
            ProviderCachePolicy::default(),
        )
    }

    fn make_plan_input<'a>(
        tokens: &'a TokenAccounting,
        recovery: &'a RecoveryState,
        latches: &'a SessionLatches,
        stats: &'a PipelineStats,
        policy: &'a ProviderCachePolicy,
    ) -> PlanInput<'a> {
        PlanInput {
            tokens,
            model_limit: 100_000,
            recovery,
            latches,
            stats,
            provider_policy: policy,
            has_memory: true,
            model_id: "test-model",
            query_source: "repl",
        }
    }

    #[test]
    fn plan_normal_pressure_selects_normal_tier() {
        let (tokens, recovery, latches, stats, policy) = default_input();
        let input = make_plan_input(&tokens, &recovery, &latches, &stats, &policy);
        let plan = plan_turn(&input);
        assert_eq!(plan.compact_tier, CompactionTier::Normal);
        assert!(plan.pressure.raw < 0.60);
    }

    #[test]
    fn plan_high_pressure_selects_compact_history() {
        let (mut tokens, recovery, latches, stats, policy) = default_input();
        tokens.prompt = 80_000; // 80% of 100K
        let input = make_plan_input(&tokens, &recovery, &latches, &stats, &policy);
        let plan = plan_turn(&input);
        assert!(plan.compact_tier >= CompactionTier::CompactHistory);
    }

    #[test]
    fn plan_huge_token_accounting_saturates_instead_of_truncating() {
        let (mut tokens, recovery, latches, stats, policy) = default_input();
        tokens.prompt = u64::from(u32::MAX) + 10;

        let input = make_plan_input(&tokens, &recovery, &latches, &stats, &policy);
        let plan = plan_turn(&input);

        assert!(
            plan.pressure.raw > 1.0,
            "huge token accounting must not wrap to low pressure"
        );
        assert_eq!(plan.compact_tier, CompactionTier::AggressivePrune);
    }

    #[test]
    fn plan_recovery_escalates_tier() {
        let (tokens, mut recovery, latches, stats, policy) = default_input();
        recovery.record_ptl_error();
        let input = make_plan_input(&tokens, &recovery, &latches, &stats, &policy);
        let plan = plan_turn(&input);
        assert!(
            plan.compact_tier >= CompactionTier::TrimSchemas,
            "1 PTL error should escalate from Normal"
        );
    }

    #[test]
    fn plan_section_manifest_always_includes_identity_and_constraints() {
        let (tokens, recovery, latches, stats, policy) = default_input();
        let input = make_plan_input(&tokens, &recovery, &latches, &stats, &policy);
        let plan = plan_turn(&input);
        assert!(
            plan.sections
                .iter()
                .any(|s| s.kind == SectionKind::Identity)
        );
        assert!(
            plan.sections
                .iter()
                .any(|s| s.kind == SectionKind::Constraints)
        );
    }

    #[test]
    fn plan_section_manifest_excludes_memory_when_unavailable() {
        let (tokens, recovery, latches, stats, policy) = default_input();
        let mut input = make_plan_input(&tokens, &recovery, &latches, &stats, &policy);
        input.has_memory = false;
        let plan = plan_turn(&input);
        assert!(!plan.sections.iter().any(|s| s.kind == SectionKind::Memory));
    }

    #[test]
    fn plan_section_manifest_includes_memory_when_available() {
        let (tokens, recovery, latches, stats, policy) = default_input();
        let input = make_plan_input(&tokens, &recovery, &latches, &stats, &policy);
        let plan = plan_turn(&input);
        assert!(plan.sections.iter().any(|s| s.kind == SectionKind::Memory));
    }

    #[test]
    fn plan_section_manifest_does_not_emit_hollow_history_section() {
        let (tokens, recovery, latches, stats, policy) = default_input();
        let input = make_plan_input(&tokens, &recovery, &latches, &stats, &policy);
        let plan = plan_turn(&input);
        assert!(!plan.sections.iter().any(|s| s.kind == SectionKind::History));
    }

    #[test]
    fn plan_budget_total_never_exceeds_limit() {
        let (tokens, recovery, latches, stats, policy) = default_input();
        let input = make_plan_input(&tokens, &recovery, &latches, &stats, &policy);
        let plan = plan_turn(&input);
        assert!(
            plan.budget.total_allocated() <= input.model_limit,
            "allocated={} > limit={}",
            plan.budget.total_allocated(),
            input.model_limit,
        );
    }

    #[test]
    fn plan_cache_strategy_varies_by_provider() {
        let (tokens, recovery, latches, stats, _) = default_input();

        let anthropic = ProviderCachePolicy::anthropic();
        let input = make_plan_input(&tokens, &recovery, &latches, &stats, &anthropic);
        let plan_a = plan_turn(&input);
        assert_eq!(
            plan_a.cache_strategy.protocol,
            PromptCacheProtocol::AnthropicCacheControl
        );
        assert!(plan_a.cache_strategy.use_global_scope);

        let openai = ProviderCachePolicy::openai_compatible();
        let input = make_plan_input(&tokens, &recovery, &latches, &stats, &openai);
        let plan_o = plan_turn(&input);
        assert_eq!(plan_o.cache_strategy.protocol, PromptCacheProtocol::Prefix);
        assert!(!plan_o.cache_strategy.use_global_scope);
    }

    #[test]
    fn plan_predictive_escalates_but_never_below_raw() {
        // Raw pressure is high (0.80 → CompactHistory),
        // but predictive with reserves would be even higher
        let (mut tokens, recovery, latches, mut stats, policy) = default_input();
        tokens.prompt = 80_000;
        // Feed the estimator so reserves are non-zero
        let feedback = crate::context_feedback::ContextFeedback::from_usage(0, 0, 0, 5000, false);
        stats
            .response_token_estimates
            .record("test-model", "repl", &feedback);

        let input = make_plan_input(&tokens, &recovery, &latches, &stats, &policy);
        let plan = plan_turn(&input);
        // Predictive pressure should be >= raw pressure
        assert!(plan.pressure.value >= plan.pressure.raw);
        // Tier should be at least CompactHistory (from raw)
        assert!(plan.compact_tier >= CompactionTier::CompactHistory);
    }

    #[test]
    fn plan_includes_emergent_sections() {
        let (tokens, recovery, latches, stats, policy) = default_input();
        let input = make_plan_input(&tokens, &recovery, &latches, &stats, &policy);
        let plan = plan_turn(&input);
        assert!(
            plan.sections
                .iter()
                .any(|s| s.kind == SectionKind::EmergentSkills)
        );
        assert!(
            plan.sections
                .iter()
                .any(|s| s.kind == SectionKind::EmergentMemory)
        );
        assert!(
            plan.sections
                .iter()
                .any(|s| s.kind == SectionKind::EmergentSummary)
        );
    }
}
