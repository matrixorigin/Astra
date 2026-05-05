//! Integration test: PipelineSession lifecycle across multiple turns.
//!
//! Validates the Phase 11 contract:
//! - Session accumulates stats across turns
//! - Feedback loop updates cache hit ratio and response token estimates
//! - Recovery state escalates and resets correctly
//! - Shadow mode produces clean diffs when pipeline is deterministic
//! - Warm start preserves cross-session continuity

use std::collections::HashMap;

use astra_turn_core::context_feedback::ContextFeedback;
use astra_turn_core::context_sources::{
    AgentContext, EdgeProfile, ExternalSources, SessionContext, StaticSections, TurnState,
};
use astra_turn_core::microcompact::ProviderCacheStrategy;
use astra_turn_core::optimize_limits::OptimizeLimits;
use astra_turn_core::pipeline_config::{PipelineConfig, ProviderCachePolicy};
use astra_turn_core::pipeline_session::{PipelineSession, TurnInput};
use astra_turn_core::pipeline_session_serde::{deserialize_stats, serialize_stats};
use astra_turn_core::recovery_state::RecoveryState;
use astra_turn_core::token_accounting::TokenAccounting;

fn make_session_context() -> SessionContext {
    SessionContext {
        session_id: "integration-test-session".into(),
        run_id: "run-1".into(),
        model_id: "claude-sonnet-4-6".into(),
        model_limit: 200_000,
        provider_policy: ProviderCachePolicy::anthropic(),
        provider_strategy: ProviderCacheStrategy::default(),
        project_context: "Integration test project context.".into(),
        edge_profile: EdgeProfile {
            cwd: Some("/home/user/project".into()),
            git_branch: Some("main".into()),
            os: Some("linux".into()),
            shell: Some("zsh".into()),
            agent_id: Some("test-agent".into()),
            edge_executor_id: Some("exec-1".into()),
        },
        self_model: Some("Good at coding, struggles with design.".into()),
    }
}

fn make_turn_state(turn_index: u32, message_count: usize) -> TurnState {
    let messages: Vec<serde_json::Value> = (0..message_count)
        .map(|i| {
            if i % 2 == 0 {
                serde_json::json!({"role": "user", "content": format!("Message {}", i)})
            } else {
                serde_json::json!({"role": "assistant", "content": format!("Response {}", i)})
            }
        })
        .collect();

    TurnState {
        messages,
        tool_results: vec![],
        tokens: TokenAccounting::default(),
        active_skills: vec!["code-review".into()],
        recent_file_reads: HashMap::new(),
        remaining_turns: 20 - turn_index,
        turn_index,
        recovery: RecoveryState::default(),
        last_user_message: format!("Turn {} message", turn_index),
    }
}

#[test]
fn ten_turn_session_lifecycle() {
    let config = PipelineConfig {
        provider_policy: ProviderCachePolicy::anthropic(),
    };
    let mut sess = PipelineSession::new(config);
    let statics = StaticSections::test_default();
    let agent = AgentContext {
        agent_id: "test-agent".into(),
        persona: "You are a helpful coding assistant.".into(),
        tool_schemas: vec![serde_json::json!({"name": "read_file", "description": "Read a file"})],
        skill_names: vec!["code-review".into()],
        delegation_targets: vec![],
    };
    let session = make_session_context();
    let external = ExternalSources {
        memory_snippets: vec!["User prefers concise answers.".into()],
        spill_dir: None,
        ..Default::default()
    };
    let limits = OptimizeLimits::default();

    for turn_idx in 1..=10 {
        let turn = make_turn_state(turn_idx, (turn_idx * 2) as usize);
        let input = TurnInput {
            statics: &statics,
            agent: &agent,
            session: &session,
            turn: &turn,
            external: &external,
            optimize_limits: &limits,
            model_id: "claude-sonnet-4-6",
            query_source: "repl",
        };

        let output = sess.run_turn(input).expect("should not abort in 10 turns");

        assert_eq!(output.metrics.turn_index, turn_idx);
        assert!(output.explain.phase_timings.len() == 4);

        let cache_read = 800 + (turn_idx as u64 * 10);
        let cache_creation = 200u64.saturating_sub(turn_idx as u64 * 15);
        let completion = 300 + (turn_idx as u64 * 50);
        let feedback =
            ContextFeedback::from_usage(0, cache_read, cache_creation, completion, false);
        sess.record_feedback("claude-sonnet-4-6", "repl", feedback, None);
    }

    assert_eq!(sess.turns_completed(), 10);
    assert_eq!(sess.stats.turns_executed, 10);
    assert!(
        sess.stats.avg_cache_hit_ratio > 0.5,
        "after 10 turns with mostly cache hits, ratio should be > 0.5, got {}",
        sess.stats.avg_cache_hit_ratio
    );
}

#[test]
fn warm_start_from_serialized_stats() {
    let config = PipelineConfig {
        provider_policy: ProviderCachePolicy::anthropic(),
    };

    let mut first_session = PipelineSession::new(config.clone());
    for _ in 1..=5 {
        let feedback = ContextFeedback::from_usage(0, 900, 100, 400, false);
        first_session.record_feedback("claude-sonnet-4-6", "repl", feedback, None);
    }

    let bytes = serialize_stats(&first_session.stats).expect("serialization should succeed");
    let restored_stats = deserialize_stats(&bytes).expect("deserialization should succeed");

    let second_session = PipelineSession::with_warm_stats(config, restored_stats);
    assert_eq!(second_session.stats.turns_executed, 5);
    assert!(second_session.stats.avg_cache_hit_ratio > 0.8);
}

#[test]
fn ptl_error_recovery_and_abort() {
    let config = PipelineConfig::default();
    let mut sess = PipelineSession::new(config);

    sess.record_ptl_error();
    assert!(!sess.should_abort());

    sess.record_ptl_error();
    assert!(!sess.should_abort());

    sess.record_ptl_error();
    assert!(sess.should_abort());

    let statics = StaticSections::test_default();
    let agent = AgentContext::default();
    let session = make_session_context();
    let turn = make_turn_state(1, 2);
    let external = ExternalSources {
        memory_snippets: vec![],
        spill_dir: None,
        ..Default::default()
    };
    let limits = OptimizeLimits::default();

    let input = TurnInput {
        statics: &statics,
        agent: &agent,
        session: &session,
        turn: &turn,
        external: &external,
        optimize_limits: &limits,
        model_id: "model",
        query_source: "repl",
    };

    let result = sess.run_turn(input);
    assert!(result.is_err(), "aborted session should refuse run_turn");
}

#[test]
fn shadow_mode_produces_deterministic_output() {
    let config = PipelineConfig {
        provider_policy: ProviderCachePolicy::anthropic(),
    };
    let sess = PipelineSession::new(config);
    let statics = StaticSections::test_default();
    let agent = AgentContext::default();
    let session = make_session_context();
    let turn = make_turn_state(1, 4);
    let external = ExternalSources {
        memory_snippets: vec!["test memory".into()],
        spill_dir: None,
        ..Default::default()
    };
    let limits = OptimizeLimits::default();

    let input1 = TurnInput {
        statics: &statics,
        agent: &agent,
        session: &session,
        turn: &turn,
        external: &external,
        optimize_limits: &limits,
        model_id: "claude-sonnet-4-6",
        query_source: "repl",
    };
    let output1 = sess.run_turn(input1).unwrap();

    let input2 = TurnInput {
        statics: &statics,
        agent: &agent,
        session: &session,
        turn: &turn,
        external: &external,
        optimize_limits: &limits,
        model_id: "claude-sonnet-4-6",
        query_source: "repl",
    };
    let shadow = sess.run_turn_shadow(input2, &output1.optimized).unwrap();
    assert!(
        shadow.diff.is_clean(),
        "deterministic pipeline should produce clean shadow diff: {:?}",
        shadow.diff.alerts
    );
}

#[test]
fn response_token_estimator_improves_over_turns() {
    let config = PipelineConfig::default();
    let mut sess = PipelineSession::new(config);

    let initial_reserve =
        sess.stats
            .response_token_estimates
            .reserve_for("model", "repl", &RecoveryState::default());
    assert_eq!(initial_reserve.output_tokens, 500);

    for i in 1..=20 {
        let feedback = ContextFeedback::from_usage(0, 0, 0, 1000 + i * 100, false);
        sess.record_feedback("model", "repl", feedback, None);
    }

    let learned_reserve =
        sess.stats
            .response_token_estimates
            .reserve_for("model", "repl", &RecoveryState::default());
    assert!(
        learned_reserve.output_tokens > 500,
        "after 20 samples with completion > 1000, reserve should exceed default floor 500, got {}",
        learned_reserve.output_tokens
    );
}

#[test]
fn full_lifecycle_with_emergent_and_latches() {
    use astra_turn_core::pipeline_session::AdaptiveTurnInput;
    use astra_turn_core::section_types::CacheScope;

    let config = PipelineConfig {
        provider_policy: ProviderCachePolicy::anthropic(),
    };
    let mut sess = PipelineSession::new(config);

    let statics = StaticSections::test_default();
    let agent = AgentContext {
        agent_id: "test-agent".into(),
        persona: "Expert coder".into(),
        tool_schemas: vec![
            serde_json::json!({"name": "read_file"}),
            serde_json::json!({"name": "write_file"}),
        ],
        skill_names: vec!["review".into()],
        delegation_targets: vec![],
    };
    let session = make_session_context();
    let external = ExternalSources {
        memory_snippets: vec!["User works on astra-engine.".into()],
        spill_dir: None,
        ..Default::default()
    };

    // Turn 1: initial turn — latch cache scope, run pipeline
    sess.latch_cache_scope(CacheScope::Global, 1);
    assert_eq!(sess.latches.cache_scope, Some(CacheScope::Global));

    let turn1 = make_turn_state(1, 2);
    let input1 = AdaptiveTurnInput {
        statics: &statics,
        agent: &agent,
        session: &session,
        turn: &turn1,
        external: &external,
        model_id: "claude-sonnet-4-6",
        query_source: "repl",
    };
    let output1 = sess
        .run_turn_adaptive(input1)
        .expect("turn 1 should succeed");
    assert_eq!(output1.metrics.turn_index, 1);

    // Simulate: during turn 1 execution, we discover a skill and prefetch memory
    sess.push_emergent_skill("debug", "error detected in tool output", 1);
    sess.push_emergent_memory("Related: user debugged similar issue last week", 0.85, 1);

    let feedback1 = ContextFeedback::from_usage(0, 900, 100, 400, false);
    sess.record_feedback("claude-sonnet-4-6", "repl", feedback1, Some(&output1));

    // Turn 2: emergent context should be available (not empty)
    assert!(
        !sess.emergent.is_empty(),
        "emergent should have items from turn 1"
    );

    let turn2 = make_turn_state(2, 4);
    let input2 = AdaptiveTurnInput {
        statics: &statics,
        agent: &agent,
        session: &session,
        turn: &turn2,
        external: &external,
        model_id: "claude-sonnet-4-6",
        query_source: "repl",
    };
    let output2 = sess
        .run_turn_adaptive(input2)
        .expect("turn 2 should succeed");
    assert_eq!(output2.metrics.turn_index, 2);

    let feedback2 = ContextFeedback::from_usage(0, 950, 50, 350, false);
    sess.record_feedback("claude-sonnet-4-6", "repl", feedback2, Some(&output2));

    // Turn 3: latch a beta header
    sess.latch_header("anthropic-beta", "prompt-caching-2024-07-31", 3);
    assert!(sess.latches.has_header("anthropic-beta"));

    let turn3 = make_turn_state(3, 6);
    let input3 = AdaptiveTurnInput {
        statics: &statics,
        agent: &agent,
        session: &session,
        turn: &turn3,
        external: &external,
        model_id: "claude-sonnet-4-6",
        query_source: "repl",
    };
    let output3 = sess
        .run_turn_adaptive(input3)
        .expect("turn 3 should succeed");

    let feedback3 = ContextFeedback::from_usage(0, 980, 20, 300, false);
    sess.record_feedback("claude-sonnet-4-6", "repl", feedback3, Some(&output3));

    // Verify accumulated state
    assert_eq!(sess.turns_completed(), 3);
    assert!(sess.stats.avg_cache_hit_ratio > 0.8);
    assert!(!sess.stats.section_token_history().is_empty());

    // Verify latches are frozen
    assert!(
        !sess.latch_cache_scope(CacheScope::Session, 4),
        "scope should not re-latch"
    );
    assert!(
        !sess.latch_header("anthropic-beta", "different-value", 4),
        "header should not re-latch"
    );

    // Verify warm start preservation
    let bytes = serialize_stats(&sess.stats).unwrap();
    let restored = deserialize_stats(&bytes).unwrap();
    assert_eq!(restored.turns_executed, 3);
    assert!((restored.avg_cache_hit_ratio - sess.stats.avg_cache_hit_ratio).abs() < 1e-9);
}
