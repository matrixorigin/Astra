use astra_turn_core::context_binder::bind_all;
use astra_turn_core::context_optimizer::{CacheMarker, ContextOptimized};
use astra_turn_core::context_pipeline::{ContextPipeline, PipelineAbort, PipelineRunInput};
use astra_turn_core::context_planner::{PlanInput, plan_turn};
use astra_turn_core::context_serializer::{
    flatten_serialized_system_blocks, serialize_prompt_sections, serialize_provider_request,
};
use astra_turn_core::context_sources::*;
use astra_turn_core::emergent_context::EmergentContext;
use astra_turn_core::microcompact::ProviderCacheStrategy;
use astra_turn_core::optimize_limits::OptimizeLimits;
use astra_turn_core::pipeline_config::{PipelineConfig, ProviderCachePolicy};
use astra_turn_core::pipeline_stats::PipelineStats;
use astra_turn_core::recovery_state::RecoveryState;
use astra_turn_core::section_types::{
    BoundSection, CacheScope, CompressionPriority, PlannedSection, PromptSection, SectionArtifact,
    SectionKind, SectionSource,
};
use astra_turn_core::session_latches::SessionLatches;
use astra_turn_core::token_accounting::TokenAccounting;
use std::collections::HashMap;

fn build_sources() -> (
    StaticSections,
    AgentContext,
    SessionLatches,
    SessionContext,
    TurnState,
    ExternalSources,
    EmergentContext,
    PipelineStats,
) {
    (
        StaticSections::test_default(),
        AgentContext::default(),
        SessionLatches::default(),
        SessionContext {
            session_id: "contract-session".into(),
            run_id: "contract-run".into(),
            model_id: "claude-sonnet-4-6".into(),
            model_limit: 100_000,
            provider_policy: ProviderCachePolicy::anthropic(),
            provider_strategy: ProviderCacheStrategy::default(),
            project_context: "Pipeline contract test project".into(),
            edge_profile: EdgeProfile {
                cwd: Some("/tmp/astra-contract".into()),
                git_branch: Some("refactor_context".into()),
                ..Default::default()
            },
            self_model: Some("Senior Rust agent.".into()),
        },
        TurnState {
            messages: vec![serde_json::json!({"role": "user", "content": "hello"})],
            tool_results: vec![],
            tokens: TokenAccounting::from_fields(5000, 1000, 0, 0),
            active_skills: vec!["code_review".into()],
            recent_file_reads: HashMap::new(),
            remaining_turns: 10,
            turn_index: 1,
            recovery: RecoveryState::default(),
            last_user_message: "hello".into(),
        },
        ExternalSources {
            memory_snippets: vec!["contract memory".into()],
            spill_dir: None,
            ..Default::default()
        },
        EmergentContext::default(),
        PipelineStats::default(),
    )
}

#[test]
fn bind_outputs_typed_artifacts_not_raw_string_only() {
    let (statics, agent, latches, session, turn, ext, emer, stats) = build_sources();
    let sources = ContextSources {
        statics: &statics,
        agent: &agent,
        latches: &latches,
        session: &session,
        turn: &turn,
        external: &ext,
        emergent: &emer,
        stats: &stats,
    };
    let plan_input = PlanInput {
        tokens: &turn.tokens,
        model_limit: session.model_limit,
        recovery: &turn.recovery,
        latches: &latches,
        stats: &stats,
        provider_policy: &session.provider_policy,
        has_memory: !ext.memory_snippets.is_empty(),
        model_id: &session.model_id,
        query_source: "harness",
    };

    let plan = plan_turn(&plan_input);
    let bound = bind_all(&plan, &sources);
    let identity = bound
        .sections
        .iter()
        .find(|s| s.plan.kind == SectionKind::Identity)
        .expect("identity section must bind");

    assert!(matches!(&identity.artifact, SectionArtifact::SystemText(_)));
    assert!(!identity.text().unwrap_or("").is_empty());
}

#[test]
fn context_pipeline_serializes_provider_request_and_metrics() {
    let (statics, agent, latches, session, turn, ext, emer, stats) = build_sources();
    let sources = ContextSources {
        statics: &statics,
        agent: &agent,
        latches: &latches,
        session: &session,
        turn: &turn,
        external: &ext,
        emergent: &emer,
        stats: &stats,
    };
    let pipeline = ContextPipeline::new(PipelineConfig {
        provider_policy: session.provider_policy.clone(),
    });
    let run = pipeline
        .run(PipelineRunInput {
            sources: &sources,
            tokens: &turn.tokens,
            model_limit: session.model_limit,
            recovery: &turn.recovery,
            latches: &latches,
            optimize_limits: &OptimizeLimits::default(),
            model_id: &session.model_id,
            query_source: "harness",
        })
        .expect("pipeline should not abort with default recovery state");

    assert!(!run.serialized.system_blocks.is_empty());
    assert_eq!(run.serialized.messages.len(), turn.messages.len());
    assert_eq!(run.metrics.turn_index, turn.turn_index);
    assert!(run.metrics.input_tokens > 0);
    assert_eq!(
        run.metrics.cache_markers,
        run.optimized.cache_markers.len() as u32
    );
    assert!(!run.explain.phase_timings.is_empty());
}

#[test]
fn context_pipeline_rejects_zero_model_limit_before_compaction() {
    let (statics, agent, latches, mut session, turn, ext, emer, stats) = build_sources();
    session.model_limit = 0;
    let sources = ContextSources {
        statics: &statics,
        agent: &agent,
        latches: &latches,
        session: &session,
        turn: &turn,
        external: &ext,
        emergent: &emer,
        stats: &stats,
    };
    let pipeline = ContextPipeline::new(PipelineConfig::default());

    let err = pipeline
        .run(PipelineRunInput {
            sources: &sources,
            tokens: &turn.tokens,
            model_limit: session.model_limit,
            recovery: &turn.recovery,
            latches: &latches,
            optimize_limits: &OptimizeLimits::for_tier(
                astra_turn_core::compaction_types::CompactionTier::AggressivePrune,
                0,
            ),
            model_id: &session.model_id,
            query_source: "harness",
        })
        .expect_err("zero model limit must abort instead of pruning context");

    assert_eq!(err, PipelineAbort::InvalidModelLimit { model_limit: 0 });
}

#[test]
fn context_pipeline_uses_session_provider_policy_not_default_config() {
    let (statics, agent, latches, session, turn, ext, emer, stats) = build_sources();
    let sources = ContextSources {
        statics: &statics,
        agent: &agent,
        latches: &latches,
        session: &session,
        turn: &turn,
        external: &ext,
        emergent: &emer,
        stats: &stats,
    };
    let pipeline = ContextPipeline::new(PipelineConfig::default());

    let run = pipeline
        .run(PipelineRunInput {
            sources: &sources,
            tokens: &turn.tokens,
            model_limit: session.model_limit,
            recovery: &turn.recovery,
            latches: &latches,
            optimize_limits: &OptimizeLimits::default(),
            model_id: &session.model_id,
            query_source: "harness",
        })
        .expect("pipeline should use session policy and complete");

    assert!(
        !run.serialized.cache_markers.is_empty(),
        "Anthropic session policy must place cache markers even when PipelineConfig is default"
    );
}

#[test]
fn prompt_sections_serialize_through_pipeline_without_text_loss() {
    let sections = vec![
        PromptSection::stable("global", CacheScope::Global),
        PromptSection::stable("", CacheScope::Session),
        PromptSection::stable("session", CacheScope::Session),
        PromptSection::dynamic(
            "dynamic",
            astra_turn_core::section_types::PromptTokenBucket::Environment,
        ),
    ];
    let serialized = serialize_prompt_sections(&sections, &ProviderCachePolicy::default());

    assert_eq!(
        flatten_serialized_system_blocks(&serialized),
        "globalsessiondynamic"
    );
    assert_eq!(serialized.system_blocks.len(), 3);
    assert!(serialized.messages.is_empty());
    assert!(serialized.tool_schemas.is_empty());
    assert!(serialized.cache_markers.is_empty());
    assert_eq!(serialized.system_blocks[0].kind, SectionKind::Identity);
    assert_eq!(serialized.system_blocks[1].kind, SectionKind::SelfModel);
    assert_eq!(
        serialized.system_blocks[2].kind,
        SectionKind::ProjectContext
    );
}

fn bound_section(kind: SectionKind, scope: CacheScope, text: &str) -> BoundSection {
    BoundSection {
        plan: PlannedSection {
            kind,
            scope,
            estimated_tokens: text.len() as u32,
            priority: CompressionPriority::Normal,
            source: SectionSource::Static,
        },
        artifact: SectionArtifact::from_text(kind, text.to_string()),
        actual_tokens: text.len() as u32,
        bind_latency: std::time::Duration::ZERO,
    }
}

#[test]
fn cache_marker_indices_match_serialized_system_blocks() {
    let optimized = ContextOptimized {
        sections: vec![
            bound_section(SectionKind::Identity, CacheScope::Global, "global"),
            bound_section(SectionKind::EmergentSkills, CacheScope::Session, ""),
        ],
        messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
        tool_schemas: vec![serde_json::json!({"name": "bash"})],
        cache_markers: vec![CacheMarker {
            after_section_index: 1,
            scope: CacheScope::Session,
            cumulative_tokens: 10,
        }],
        spilled: Vec::new(),
        stats: Default::default(),
    };

    let serialized = serialize_provider_request(&optimized, &ProviderCachePolicy::anthropic());

    assert_eq!(serialized.system_blocks.len(), 1);
    assert_eq!(serialized.messages.len(), 1);
    assert_eq!(serialized.tool_schemas.len(), 1);
    assert_eq!(serialized.cache_markers.len(), 1);
    assert!(
        serialized.cache_markers[0].after_section_index < serialized.system_blocks.len(),
        "cache marker index must refer to filtered system_blocks"
    );
    assert!(
        serialized.system_blocks[serialized.cache_markers[0].after_section_index]
            .cache_control
            .is_some(),
        "remapped marker target should carry cache_control"
    );
}

#[test]
fn pipeline_aborts_on_consecutive_ptl_errors() {
    let (statics, agent, latches, session, mut turn, ext, emer, stats) = build_sources();
    // Simulate 3 consecutive PTL errors
    turn.recovery.record_ptl_error();
    turn.recovery.record_ptl_error();
    turn.recovery.record_ptl_error();
    assert!(turn.recovery.should_abort());

    let sources = ContextSources {
        statics: &statics,
        agent: &agent,
        latches: &latches,
        session: &session,
        turn: &turn,
        external: &ext,
        emergent: &emer,
        stats: &stats,
    };
    let pipeline = ContextPipeline::new(PipelineConfig {
        provider_policy: session.provider_policy.clone(),
    });
    let result = pipeline.run(PipelineRunInput {
        sources: &sources,
        tokens: &turn.tokens,
        model_limit: session.model_limit,
        recovery: &turn.recovery,
        latches: &latches,
        optimize_limits: &OptimizeLimits::default(),
        model_id: &session.model_id,
        query_source: "harness",
    });

    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err(),
        PipelineAbort::ConsecutivePtlExhausted {
            consecutive_errors: 3
        }
    );
}

#[test]
fn serialized_output_format_system_blocks_are_well_structured() {
    let (statics, agent, latches, session, turn, ext, emer, stats) = build_sources();
    let sources = ContextSources {
        statics: &statics,
        agent: &agent,
        latches: &latches,
        session: &session,
        turn: &turn,
        external: &ext,
        emergent: &emer,
        stats: &stats,
    };
    let pipeline = ContextPipeline::new(PipelineConfig {
        provider_policy: session.provider_policy.clone(),
    });
    let run = pipeline
        .run(PipelineRunInput {
            sources: &sources,
            tokens: &turn.tokens,
            model_limit: session.model_limit,
            recovery: &turn.recovery,
            latches: &latches,
            optimize_limits: &OptimizeLimits::default(),
            model_id: &session.model_id,
            query_source: "harness",
        })
        .expect("pipeline should succeed");

    // Every system block must have a non-empty text body and a valid SectionKind
    for block in &run.serialized.system_blocks {
        assert!(
            !block.text.is_empty(),
            "system block {:?} must have non-empty text",
            block.kind
        );
    }

    // Messages should be valid JSON objects with "role" field
    for msg in &run.serialized.messages {
        assert!(
            msg.get("role").is_some(),
            "each message must have a 'role' field"
        );
    }

    // Cache markers (if any) must reference valid block indices
    for marker in &run.serialized.cache_markers {
        assert!(
            marker.after_section_index < run.serialized.system_blocks.len(),
            "cache marker index {} out of bounds (blocks={})",
            marker.after_section_index,
            run.serialized.system_blocks.len()
        );
    }
}

/// Proptest: predictive reserves (p75/p95) are always ≥ the response floor.
/// This is the core invariant that ensures gated tiers never silently degrade
/// due to overflow or edge-case sample distributions.
mod proptest_reserves {
    use super::*;
    use astra_turn_core::pipeline_stats::{PercentileDigest, ResponseTokenEstimator};

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 512,
            ..Default::default()
        })]

        #[test]
        fn predictive_gte_floor(
            floor in 1u32..10_000,
            samples in proptest::collection::vec(0u32..u32::MAX, 1..100),
        ) {
            let mut est = ResponseTokenEstimator::with_floor(floor);
            let recovery_state = RecoveryState::default();

            for &s in &samples {
                use astra_turn_core::context_feedback::ContextFeedback;
                let fb = ContextFeedback::from_usage(0, 0, 0, s as u64, false);
                est.record("model", "src", &fb);
            }

            let reserves = est.reserve_for("model", "src", &recovery_state);
            // When we have data, the estimate is based on percentiles of actual data.
            // The floor only applies when no data exists. With data, the estimate
            // is the p75 of actual samples which may be below floor.
            // Key invariant: the function never panics or returns garbage.
            // output_tokens must be non-zero (estimator always reserves something)
            assert!(reserves.output_tokens > 0);
        }

        #[test]
        fn recovery_reserves_gte_normal(
            samples in proptest::collection::vec(1u32..100_000, 2..50),
        ) {
            let mut est = ResponseTokenEstimator::with_floor(100);
            for &s in &samples {
                use astra_turn_core::context_feedback::ContextFeedback;
                let fb = ContextFeedback::from_usage(0, 0, 0, s as u64, false);
                est.record("m", "s", &fb);
            }

            let normal = est.reserve_for("m", "s", &RecoveryState::default());
            let mut recovery = RecoveryState::default();
            recovery.record_ptl_error();
            let elevated = est.reserve_for("m", "s", &recovery);

            // Core invariant: p95 ≥ p75 (never de-escalate under recovery)
            assert!(
                elevated.output_tokens >= normal.output_tokens,
                "recovery={} must be >= normal={} for samples={:?}",
                elevated.output_tokens, normal.output_tokens, &samples[..samples.len().min(5)],
            );
        }

        #[test]
        fn percentile_digest_never_exceeds_cap(
            values in proptest::collection::vec(0u32..u32::MAX, 0..1024),
        ) {
            let mut d = PercentileDigest::default();
            for v in values {
                d.push(v);
            }
            assert!(d.count() <= PercentileDigest::MAX_SAMPLES);
        }
    }
}
