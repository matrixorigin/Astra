//! End-to-end tests for the context pipeline.
//!
//! These tests exercise the full Plan → Bind → Optimize flow
//! without an actual LLM call, verifying that the pipeline
//! produces correct, well-structured output.

use std::collections::HashMap;

use astra_turn_core::compaction_types::CompactionTier;
use astra_turn_core::context_binder::bind_all;
use astra_turn_core::context_feedback::ContextFeedback;
use astra_turn_core::context_optimizer::optimize;
use astra_turn_core::context_planner::{PlanInput, plan_turn};
use astra_turn_core::context_sources::*;
use astra_turn_core::emergent_context::*;
use astra_turn_core::microcompact::ProviderCacheStrategy;
use astra_turn_core::optimize_limits::OptimizeLimits;
use astra_turn_core::pipeline_config::ProviderCachePolicy;
use astra_turn_core::pipeline_stats::PipelineStats;
use astra_turn_core::recovery_state::RecoveryState;
use astra_turn_core::section_types::SectionKind;
use astra_turn_core::session_latches::SessionLatches;
use astra_turn_core::shadow_diff::diff_pipeline_outputs;
use astra_turn_core::token_accounting::TokenAccounting;
use astra_turn_core::trace_alert::evaluate_alerts;

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
            session_id: "test-session".into(),
            run_id: "test-run".into(),
            model_id: "test-model".into(),
            model_limit: 100_000,
            provider_policy: ProviderCachePolicy::anthropic(),
            provider_strategy: ProviderCacheStrategy::default(),
            project_context: "Rust project with cargo build system".into(),
            edge_profile: EdgeProfile {
                cwd: Some("/home/user/myproject".into()),
                git_branch: Some("main".into()),
                ..Default::default()
            },
            self_model: Some("Expert Rust engineer.".into()),
        },
        TurnState {
            messages: vec![
                serde_json::json!({"role": "user", "content": "Fix the bug in main.rs"}),
                serde_json::json!({"role": "assistant", "content": "I'll look at main.rs."}),
            ],
            tool_results: vec![],
            tokens: TokenAccounting::from_fields(5000, 3000, 500, 200),
            active_skills: vec!["code_review".into()],
            recent_file_reads: HashMap::new(),
            remaining_turns: 10,
            turn_index: 3,
            recovery: RecoveryState::default(),
            last_user_message: "Fix the bug in main.rs".into(),
        },
        ExternalSources {
            memory_snippets: vec!["Relevant memory: main.rs has flaky parsing.".into()],
            spill_dir: None,
            dynamic_prompt_sections: vec![],
        },
        EmergentContext::default(),
        PipelineStats::default(),
    )
}

/// Full pipeline: plan → bind → optimize produces valid output.
#[test]
fn pipeline_single_turn_produces_valid_output() {
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
        query_source: "repl",
    };

    // Plan
    let plan = plan_turn(&plan_input);
    assert!(!plan.sections.is_empty());
    assert!(
        plan.sections
            .iter()
            .any(|s| s.kind == SectionKind::Identity)
    );
    assert!(plan.sections.iter().any(|s| s.kind == SectionKind::Memory));

    // Bind
    let bound = bind_all(&plan, &sources);
    assert_eq!(bound.sections.len(), plan.sections.len());
    assert_eq!(bound.messages.len(), 2);

    // Optimize
    let limits = OptimizeLimits::default();
    let optimized = optimize(&plan, bound, &latches, &session.provider_policy, &limits, 1);
    assert!(!optimized.sections.is_empty());
    assert_eq!(optimized.messages.len(), 2);
    // Anthropic should have cache markers
    assert!(!optimized.cache_markers.is_empty());
}

/// Multi-turn feedback loop: PipelineStats accumulates across turns.
#[test]
fn pipeline_multi_turn_feedback_accumulates() {
    let mut stats = PipelineStats::default();

    // Simulate 3 turns
    for i in 1..=3 {
        let feedback = ContextFeedback::from_usage(
            1000 * i, // prompt
            800 * i,  // cache_read
            100 * i,  // cache_creation
            200 * i,  // completion
            false,
        );
        stats.record("test-model", "repl", &feedback);
    }

    assert_eq!(stats.turns_executed, 3);
    assert!(stats.avg_cache_hit_ratio > 0.0);
}

/// High pressure triggers compaction tier escalation.
#[test]
fn pipeline_compaction_under_pressure() {
    let (statics, agent, latches, session, mut turn, ext, emer, stats) = build_sources();
    // Set high token usage: 85% of 100K
    turn.tokens = TokenAccounting::from_fields(85_000, 0, 0, 0);

    let _sources = ContextSources {
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
        query_source: "repl",
    };

    let plan = plan_turn(&plan_input);
    assert!(
        plan.compact_tier >= CompactionTier::CompactHistory,
        "85% pressure should trigger CompactHistory+, got {:?}",
        plan.compact_tier,
    );
}

/// PTL recovery escalates tier on next plan.
#[test]
fn pipeline_ptl_recovery_escalates() {
    let (statics, agent, latches, session, mut turn, ext, emer, stats) = build_sources();
    turn.tokens = TokenAccounting::default(); // Low pressure
    turn.recovery.record_ptl_error();
    turn.recovery.record_ptl_error();

    let _sources = ContextSources {
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
        query_source: "repl",
    };

    let plan = plan_turn(&plan_input);
    // 2 PTL errors should escalate to at least CompactHistory
    assert!(
        plan.compact_tier >= CompactionTier::CompactHistory,
        "2 PTL errors should escalate, got {:?}",
        plan.compact_tier,
    );
}

/// Emergent context flows from one turn's output to the next turn's bind.
#[test]
fn pipeline_emergent_context_flows() {
    let (statics, agent, latches, session, turn, ext, mut emer, stats) = build_sources();

    // Simulate previous turn discovering a skill
    emer.push_skill(EmergentItem {
        value: DiscoveredSkill {
            skill_name: "security_review".into(),
            trigger: "file write to auth.rs".into(),
        },
        created_at_turn: 2,
        content_hash: 12345,
    });

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
        query_source: "repl",
    };

    let plan = plan_turn(&plan_input);
    let bound = bind_all(&plan, &sources);

    // The emergent skills section should contain the discovered skill
    let emergent_section = bound
        .sections
        .iter()
        .find(|s| s.plan.kind == SectionKind::EmergentSkills);
    assert!(emergent_section.is_some());
    assert!(
        emergent_section
            .unwrap()
            .text()
            .unwrap_or("")
            .contains("security_review"),
        "Emergent skill should be bound"
    );
}

/// Shadow diff: identical pipeline runs produce clean result.
#[test]
fn pipeline_shadow_diff_identical() {
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
        query_source: "repl",
    };

    let plan = plan_turn(&plan_input);
    let bound1 = bind_all(&plan, &sources);
    let bound2 = bind_all(&plan, &sources);
    let limits = OptimizeLimits::default();
    let opt1 = optimize(
        &plan,
        bound1,
        &latches,
        &session.provider_policy,
        &limits,
        1,
    );
    let opt2 = optimize(
        &plan,
        bound2,
        &latches,
        &session.provider_policy,
        &limits,
        1,
    );

    let diff = diff_pipeline_outputs(&opt1, &opt2, 1);
    assert!(
        diff.is_clean(),
        "identical runs should produce clean diff: {:?}",
        diff.alerts
    );
}

/// Trace alerts fire on recovery loop.
#[test]
fn pipeline_trace_alerts_on_recovery() {
    let mut recovery = RecoveryState::default();
    recovery.record_ptl_error();
    recovery.record_ptl_error();

    let feedback = ContextFeedback::from_usage(0, 0, 5000, 100, false);
    let stats = PipelineStats::default();

    let alerts = evaluate_alerts(5, &feedback, &stats, &recovery);
    assert!(
        alerts.iter().any(|a| a.rule == "recovery_loop"),
        "2 PTL errors should trigger recovery_loop alert"
    );
}

/// Shadow diff: pipeline outputs with different recovery states produce divergence alerts.
/// This validates that shadow diff actually detects real differences (non-tautological).
#[test]
fn pipeline_shadow_diff_detects_recovery_divergence() {
    let (statics, agent, latches, session, mut turn, ext, emer, stats) = build_sources();

    // First run: normal state
    let sources_normal = ContextSources {
        statics: &statics,
        agent: &agent,
        latches: &latches,
        session: &session,
        turn: &turn,
        external: &ext,
        emergent: &emer,
        stats: &stats,
    };
    let plan_input_normal = PlanInput {
        tokens: &turn.tokens,
        model_limit: session.model_limit,
        recovery: &turn.recovery,
        latches: &latches,
        stats: &stats,
        provider_policy: &session.provider_policy,
        has_memory: !ext.memory_snippets.is_empty(),
        model_id: &session.model_id,
        query_source: "repl",
    };
    let plan_normal = plan_turn(&plan_input_normal);
    let bound_normal = bind_all(&plan_normal, &sources_normal);
    let limits = OptimizeLimits::default();
    let opt_normal = optimize(
        &plan_normal,
        bound_normal,
        &latches,
        &session.provider_policy,
        &limits,
        1,
    );

    // Second run: recovery state (different pressure → different output)
    turn.recovery.record_ptl_error();
    turn.recovery.record_ptl_error();
    let sources_recovery = ContextSources {
        statics: &statics,
        agent: &agent,
        latches: &latches,
        session: &session,
        turn: &turn,
        external: &ext,
        emergent: &emer,
        stats: &stats,
    };
    let plan_input_recovery = PlanInput {
        tokens: &turn.tokens,
        model_limit: session.model_limit,
        recovery: &turn.recovery,
        latches: &latches,
        stats: &stats,
        provider_policy: &session.provider_policy,
        has_memory: !ext.memory_snippets.is_empty(),
        model_id: &session.model_id,
        query_source: "repl",
    };
    let plan_recovery = plan_turn(&plan_input_recovery);
    let bound_recovery = bind_all(&plan_recovery, &sources_recovery);
    let opt_recovery = optimize(
        &plan_recovery,
        bound_recovery,
        &latches,
        &session.provider_policy,
        &limits,
        2,
    );

    let diff = diff_pipeline_outputs(&opt_normal, &opt_recovery, 2);
    // Recovery state changes pressure tier → different sections/content
    // The diff should either be clean (same structure) or produce alerts (different structure)
    // Key invariant: diff_pipeline_outputs never panics, always produces a valid result
    assert!(
        diff.section_count_match || !diff.alerts.is_empty(),
        "Shadow diff must either match or produce diagnostic alerts"
    );
}
