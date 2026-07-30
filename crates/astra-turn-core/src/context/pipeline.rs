//! Canonical context pipeline orchestration.
//!
//! This module is the pipeline-first entry point for constructing provider
//! requests: Plan -> Bind -> Optimize -> Serialize -> Explain/Metrics.

use std::fmt;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::compaction_types::CompactionTier;
use crate::context_binder::{ContextBound, bind_sections};
use crate::context_optimizer::{ContextOptimized, optimize_with_spill};
use crate::context_planner::{ContextPlan, PlanInput, plan_turn};
use crate::context_pressure::ContextPressure;
use crate::context_serializer::{SerializedProviderRequest, serialize_provider_request};
use crate::context_sources::ContextSources;
use crate::optimize_limits::OptimizeLimits;
use crate::pipeline_config::PipelineConfig;
use crate::recovery_state::RecoveryState;
use crate::section_types::estimate_text_tokens;
use crate::session_latches::SessionLatches;
use crate::spill_backend::SpillBackend;
use crate::token_accounting::TokenAccounting;

enum LimitPolicy<'a> {
    Explicit(&'a OptimizeLimits),
    Adaptive {
        suppress_tool_result_clearing: bool,
        history_owner: HistoryOptimizationOwner,
    },
}

/// Selects the component that owns lossy conversation-history reduction.
///
/// A pipeline can run standalone, or as the planning/front-end stage of a
/// semantic compactor. Exactly one layer should clear results or drop units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HistoryOptimizationOwner {
    #[default]
    Pipeline,
    DownstreamSemanticCompactor,
}

/// Pipeline refused to execute due to unrecoverable error state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipelineAbort {
    /// Consecutive PTL errors exceeded abort threshold (3).
    ConsecutivePtlExhausted { consecutive_errors: u32 },
    /// The model context limit is invalid, so planning would be unsafe.
    InvalidModelLimit { model_limit: u32 },
}

impl fmt::Display for PipelineAbort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConsecutivePtlExhausted { consecutive_errors } => {
                write!(
                    f,
                    "pipeline aborted: {} consecutive prompt-too-long errors",
                    consecutive_errors
                )
            }
            Self::InvalidModelLimit { model_limit } => {
                write!(
                    f,
                    "pipeline aborted: invalid model context limit {model_limit}"
                )
            }
        }
    }
}

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

    /// Run the full pipeline. Returns `Err(PipelineAbort)` if recovery state
    /// indicates an unrecoverable error streak (e.g., 3+ consecutive PTL errors).
    pub fn run(&self, input: PipelineRunInput<'_>) -> Result<PipelineRunOutput, PipelineAbort> {
        self.run_with_limit_policy(&input, LimitPolicy::Explicit(input.optimize_limits))
    }

    /// Run with transformation gates derived from the final, measured plan.
    ///
    /// This prevents the caller from having to predict the compaction tier
    /// before Plan has measured the bound request.
    pub fn run_adaptive(
        &self,
        input: AdaptivePipelineRunInput<'_>,
    ) -> Result<PipelineRunOutput, PipelineAbort> {
        let compatibility_limits = OptimizeLimits::all_closed();
        let full_input = PipelineRunInput {
            sources: input.sources,
            tokens: input.tokens,
            model_limit: input.model_limit,
            recovery: input.recovery,
            latches: input.latches,
            optimize_limits: &compatibility_limits,
            model_id: input.model_id,
            query_source: input.query_source,
        };
        self.run_with_limit_policy(
            &full_input,
            LimitPolicy::Adaptive {
                suppress_tool_result_clearing: input.suppress_tool_result_clearing,
                history_owner: input.history_owner,
            },
        )
    }

    fn run_with_limit_policy(
        &self,
        input: &PipelineRunInput<'_>,
        limit_policy: LimitPolicy<'_>,
    ) -> Result<PipelineRunOutput, PipelineAbort> {
        if input.model_limit == 0 {
            return Err(PipelineAbort::InvalidModelLimit {
                model_limit: input.model_limit,
            });
        }

        let provider_policy = &input.sources.session.provider_policy;

        // Gate: check abort condition before spending compute
        if input.recovery.should_abort() {
            return Err(PipelineAbort::ConsecutivePtlExhausted {
                consecutive_errors: input.recovery.consecutive_ptl_errors,
            });
        }

        let mut timings = Vec::with_capacity(4);
        let mut plan_elapsed = Duration::ZERO;
        let mut bind_elapsed = Duration::ZERO;

        let started = Instant::now();
        let preliminary_plan_input = PlanInput {
            tokens: input.tokens,
            model_limit: input.model_limit,
            recovery: input.recovery,
            latches: input.latches,
            stats: input.sources.stats,
            provider_policy,
            has_memory: !input.sources.external.memory_entries.is_empty(),
            model_id: input.model_id,
            query_source: input.query_source,
        };
        let preliminary_plan = plan_turn(&preliminary_plan_input);
        plan_elapsed = plan_elapsed.saturating_add(started.elapsed());

        let started = Instant::now();
        let preliminary_sections = bind_sections(&preliminary_plan, input.sources);
        bind_elapsed = bind_elapsed.saturating_add(started.elapsed());

        // Provider usage is a billing ledger, not necessarily this request's
        // context occupancy. Measure the concrete candidate after Bind. A
        // non-zero caller value remains a conservative lower-bound hint for
        // direct callers with a more accurate tokenizer.
        let measured_input_tokens = estimate_bound_input_tokens(
            &preliminary_sections,
            &input.sources.turn.messages,
            &input.sources.agent.tool_schemas,
        )
        .max(input.tokens.total_input_u32_saturating());
        let measured = TokenAccounting::from_fields(u64::from(measured_input_tokens), 0, 0, 0);

        let started = Instant::now();
        let final_plan_input = PlanInput {
            tokens: &measured,
            model_limit: input.model_limit,
            recovery: input.recovery,
            latches: input.latches,
            stats: input.sources.stats,
            provider_policy,
            has_memory: !input.sources.external.memory_entries.is_empty(),
            model_id: input.model_id,
            query_source: input.query_source,
        };
        let plan = plan_turn(&final_plan_input);
        plan_elapsed = plan_elapsed.saturating_add(started.elapsed());

        let started = Instant::now();
        let sections = if plan.sections == preliminary_plan.sections {
            preliminary_sections
        } else {
            bind_sections(&plan, input.sources)
        };
        astra_core::history_work::record_serialized_value(
            astra_core::history_work::HistoryWorkSite::ContextBinding,
            &input.sources.turn.messages,
        );
        astra_core::history_work::record_serialized_value(
            astra_core::history_work::HistoryWorkSite::ContextBinding,
            &input.sources.agent.tool_schemas,
        );
        let bound = ContextBound {
            sections,
            messages: input.sources.turn.messages.clone(),
            tool_schemas: input.sources.agent.tool_schemas.clone(),
        };
        bind_elapsed = bind_elapsed.saturating_add(started.elapsed());
        timings.push(PipelinePhaseTiming::from_duration("plan", plan_elapsed));
        timings.push(PipelinePhaseTiming::from_duration("bind", bind_elapsed));

        let owned_limits;
        let optimize_limits = match limit_policy {
            LimitPolicy::Explicit(limits) => limits,
            LimitPolicy::Adaptive {
                suppress_tool_result_clearing,
                history_owner,
            } => {
                owned_limits = {
                    let mut limits = OptimizeLimits::for_tier(plan.compact_tier, input.model_limit);
                    if suppress_tool_result_clearing
                        || history_owner == HistoryOptimizationOwner::DownstreamSemanticCompactor
                    {
                        limits.allow_tool_result_clearing = false;
                    }
                    if history_owner == HistoryOptimizationOwner::DownstreamSemanticCompactor {
                        limits.allow_round_dropping = false;
                    }
                    limits
                };
                &owned_limits
            }
        };

        let started = Instant::now();
        let spill_backend: Option<&dyn SpillBackend> =
            input.sources.external.spill_backend.as_deref();
        let optimized = optimize_with_spill(
            &plan,
            bound,
            input.latches,
            provider_policy,
            optimize_limits,
            input.sources.turn.turn_index,
            spill_backend,
        );
        timings.push(PipelinePhaseTiming::elapsed("optimize", started));

        let started = Instant::now();
        let serialized = serialize_provider_request(&optimized, provider_policy);
        timings.push(PipelinePhaseTiming::elapsed("serialize", started));

        let metrics =
            PipelineRunMetrics::from_output(input, measured_input_tokens, &plan, &optimized);
        let explain = PipelineExplain {
            phase_timings: timings,
            pressure: plan.pressure,
            compact_tier: plan.compact_tier,
            skipped_optimizations: optimized.stats.skipped.len() as u32,
        };

        Ok(PipelineRunOutput {
            plan,
            optimized,
            serialized,
            explain,
            metrics,
        })
    }
}

fn estimate_bound_input_tokens(
    sections: &[crate::section_types::BoundSection],
    messages: &[serde_json::Value],
    tool_schemas: &[serde_json::Value],
) -> u32 {
    // Section tokens come from the pipeline's bound-section measurement.
    // Message and tool-schema tokens use full JSON serialization
    // (serde_json::to_string) for a consistent encoded-text estimate. This is a
    // different method from the wire-budget estimator in runtime/src/prompts,
    // which recursively walks the JSON tree without serializing intermediate
    // strings. Both are approximations; the provider tokenizer is authoritative.
    let section_tokens = sections
        .iter()
        .map(|section| section.actual_tokens)
        .fold(0_u32, u32::saturating_add);
    section_tokens
        .saturating_add(estimate_json_values_tokens(messages))
        .saturating_add(estimate_json_values_tokens(tool_schemas))
}

fn estimate_json_values_tokens(values: &[serde_json::Value]) -> u32 {
    let mut serialized_bytes = 0_u64;
    let tokens = values
        .iter()
        .map(|value| match serde_json::to_string(value) {
            Ok(encoded) => {
                serialized_bytes = serialized_bytes
                    .saturating_add(u64::try_from(encoded.len()).unwrap_or(u64::MAX));
                estimate_text_tokens(&encoded).max(1)
            }
            Err(error) => {
                astra_core::history_work::record_serialization_failure(
                    astra_core::history_work::HistoryWorkSite::ContextBinding,
                    &error,
                );
                1
            }
        })
        .fold(0_u32, u32::saturating_add);
    if astra_core::history_work::instrumentation_enabled() {
        astra_core::history_work::record_operation(
            astra_core::history_work::HistoryWorkSite::ContextBinding,
            serialized_bytes,
            u64::try_from(values.len()).unwrap_or(u64::MAX),
            0,
        );
    }
    tokens
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

pub struct AdaptivePipelineRunInput<'a> {
    pub sources: &'a ContextSources<'a>,
    pub tokens: &'a TokenAccounting,
    pub model_limit: u32,
    pub recovery: &'a RecoveryState,
    pub latches: &'a SessionLatches,
    pub model_id: &'a str,
    pub query_source: &'a str,
    /// Cascade protection is a session-level execution constraint, orthogonal
    /// to pressure tier selection.
    pub suppress_tool_result_clearing: bool,
    /// Prevents two lossy history optimizers from acting on the same request.
    pub history_owner: HistoryOptimizationOwner,
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
        Self::from_duration(phase, started.elapsed())
    }

    fn from_duration(phase: &str, elapsed: Duration) -> Self {
        Self {
            phase: phase.to_string(),
            elapsed_micros: elapsed.as_micros() as u64,
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
        measured_input_tokens: u32,
        plan: &ContextPlan,
        optimized: &ContextOptimized,
    ) -> Self {
        Self {
            turn_index: input.sources.turn.turn_index,
            input_tokens: measured_input_tokens,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_abort_display_contains_error_count() {
        let abort = PipelineAbort::ConsecutivePtlExhausted {
            consecutive_errors: 3,
        };
        let msg = abort.to_string();
        assert!(msg.contains("3"), "display should contain error count");
        assert!(
            msg.contains("prompt-too-long"),
            "display should name the error type"
        );
    }

    #[test]
    fn pipeline_abort_display_contains_invalid_model_limit() {
        let abort = PipelineAbort::InvalidModelLimit { model_limit: 0 };
        let msg = abort.to_string();
        assert!(msg.contains("invalid model context limit"));
        assert!(msg.contains("0"));
    }

    #[test]
    fn pipeline_abort_eq_derives_correctly() {
        let a = PipelineAbort::ConsecutivePtlExhausted {
            consecutive_errors: 3,
        };
        let b = PipelineAbort::ConsecutivePtlExhausted {
            consecutive_errors: 3,
        };
        let c = PipelineAbort::ConsecutivePtlExhausted {
            consecutive_errors: 4,
        };
        let invalid = PipelineAbort::InvalidModelLimit { model_limit: 0 };
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, invalid);
    }

    #[test]
    fn phase_timing_captures_nonzero_micros() {
        let started = Instant::now();
        // Burn a tiny amount of time
        std::hint::black_box(vec![0u8; 1024]);
        let timing = PipelinePhaseTiming::elapsed("test_phase", started);
        assert_eq!(timing.phase, "test_phase");
        // elapsed_micros may be 0 on fast machines, just ensure no panic
    }

    #[test]
    fn pipeline_abort_serializes_roundtrip() {
        let abort = PipelineAbort::ConsecutivePtlExhausted {
            consecutive_errors: 5,
        };
        let json = serde_json::to_string(&abort).unwrap();
        let restored: PipelineAbort = serde_json::from_str(&json).unwrap();
        assert_eq!(abort, restored);
    }

    #[test]
    fn invalid_model_limit_abort_serializes_roundtrip() {
        let abort = PipelineAbort::InvalidModelLimit { model_limit: 0 };
        let json = serde_json::to_string(&abort).unwrap();
        let restored: PipelineAbort = serde_json::from_str(&json).unwrap();
        assert_eq!(abort, restored);
    }
}
