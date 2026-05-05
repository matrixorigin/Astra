use astra_turn_core::context_binder::bind_all;
use astra_turn_core::context_optimizer::{CacheMarker, ContextOptimized};
use astra_turn_core::context_pipeline::{ContextPipeline, PipelineRunInput};
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
    assert!(!identity.text().is_empty());
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
        explain_only: false,
        provider_policy: session.provider_policy.clone(),
    });
    let run = pipeline.run(PipelineRunInput {
        sources: &sources,
        tokens: &turn.tokens,
        model_limit: session.model_limit,
        recovery: &turn.recovery,
        latches: &latches,
        optimize_limits: &OptimizeLimits::default(),
        model_id: &session.model_id,
        query_source: "harness",
    });

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
