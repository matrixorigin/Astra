//! Canonical context pipeline orchestration.
//!
//! This module is the pipeline-first entry point for constructing provider
//! requests: Plan -> Bind -> Optimize -> Serialize -> Explain/Metrics.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::compaction_types::CompactionTier;
use crate::context_binder::bind_all;
use crate::context_optimizer::{ContextOptimized, optimize};
use crate::context_planner::{ContextPlan, PlanInput, plan_turn};
use crate::context_pressure::ContextPressure;
use crate::context_serializer::{SerializedProviderRequest, serialize_provider_request};
use crate::context_sources::ContextSources;
use crate::optimize_limits::OptimizeLimits;
use crate::pipeline_config::PipelineConfig;
use crate::recovery_state::RecoveryState;
use crate::session_latches::SessionLatches;
use crate::token_accounting::TokenAccounting;

#[derive(Debug, Clone)]
pub struct ContextPipeline {
    config: PipelineConfig,
}

impl ContextPipeline {
    #[must_use]
    pub fn new(config: PipelineConfig) -> Self {
        Self { config }
    }

    #[must_use]
    pub fn config(&self) -> &PipelineConfig {
        &self.config
    }

    #[must_use]
    pub fn run(&self, input: PipelineRunInput<'_>) -> PipelineRunOutput {
        let mut timings = Vec::with_capacity(4);

        let started = Instant::now();
        let plan_input = PlanInput {
            tokens: input.tokens,
            model_limit: input.model_limit,
            recovery: input.recovery,
            latches: input.latches,
            stats: input.sources.stats,
            provider_policy: &self.config.provider_policy,
            has_memory: !input.sources.external.memory_snippets.is_empty(),
            model_id: input.model_id,
            query_source: input.query_source,
        };
        let plan = plan_turn(&plan_input);
        timings.push(PipelinePhaseTiming::elapsed("plan", started));

        let started = Instant::now();
        let bound = bind_all(&plan, input.sources);
        timings.push(PipelinePhaseTiming::elapsed("bind", started));

        let started = Instant::now();
        let optimized = optimize(
            &plan,
            bound,
            input.latches,
            &self.config.provider_policy,
            input.optimize_limits,
        );
        timings.push(PipelinePhaseTiming::elapsed("optimize", started));

        let started = Instant::now();
        let serialized = serialize_provider_request(&optimized, &self.config.provider_policy);
        timings.push(PipelinePhaseTiming::elapsed("serialize", started));

        let metrics = PipelineRunMetrics::from_output(&input, &plan, &optimized);
        let explain = PipelineExplain {
            phase_timings: timings,
            pressure: plan.pressure,
            compact_tier: plan.compact_tier,
            skipped_optimizations: optimized.stats.skipped.len() as u32,
        };

        PipelineRunOutput {
            plan,
            optimized,
            serialized,
            explain,
            metrics,
        }
    }
}

pub struct PipelineRunInput<'a> {
    pub sources: &'a ContextSources<'a>,
    pub tokens: &'a TokenAccounting,
    pub model_limit: u32,
    pub recovery: &'a RecoveryState,
    pub latches: &'a SessionLatches,
    pub optimize_limits: &'a OptimizeLimits,
    pub model_id: &'a str,
    pub query_source: &'a str,
}

#[derive(Debug)]
pub struct PipelineRunOutput {
    pub plan: ContextPlan,
    pub optimized: ContextOptimized,
    pub serialized: SerializedProviderRequest,
    pub explain: PipelineExplain,
    pub metrics: PipelineRunMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineExplain {
    pub phase_timings: Vec<PipelinePhaseTiming>,
    pub pressure: ContextPressure,
    pub compact_tier: CompactionTier,
    pub skipped_optimizations: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelinePhaseTiming {
    pub phase: String,
    pub elapsed_micros: u64,
}

impl PipelinePhaseTiming {
    fn elapsed(phase: &str, started: Instant) -> Self {
        Self {
            phase: phase.to_string(),
            elapsed_micros: started.elapsed().as_micros() as u64,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineRunMetrics {
    pub turn_index: u32,
    pub input_tokens: u32,
    pub output_reserve_tokens: u32,
    pub raw_pressure: f64,
    pub predictive_pressure: f64,
    pub compact_tier: CompactionTier,
    pub sections: u32,
    pub messages: u32,
    pub tool_schemas: u32,
    pub cache_markers: u32,
    pub tokens_cleared: u32,
    pub avg_cache_hit_ratio: f64,
}

impl PipelineRunMetrics {
    fn from_output(
        input: &PipelineRunInput<'_>,
        plan: &ContextPlan,
        optimized: &ContextOptimized,
    ) -> Self {
        Self {
            turn_index: input.sources.turn.turn_index,
            input_tokens: input.tokens.total_input_u32_saturating(),
            output_reserve_tokens: plan.reserves.output_tokens,
            raw_pressure: plan.pressure.raw,
            predictive_pressure: plan.pressure.value,
            compact_tier: plan.compact_tier,
            sections: optimized.sections.len() as u32,
            messages: optimized.messages.len() as u32,
            tool_schemas: optimized.tool_schemas.len() as u32,
            cache_markers: optimized.cache_markers.len() as u32,
            tokens_cleared: optimized.stats.tokens_cleared,
            avg_cache_hit_ratio: input.sources.stats.avg_cache_hit_ratio,
        }
    }
}
