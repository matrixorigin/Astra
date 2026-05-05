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
        sess.record_feedback("claude-sonnet-4-6", "repl", feedback);
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
        first_session.record_feedback("claude-sonnet-4-6", "repl", feedback);
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

    let initial_reserve = sess
        .stats
        .response_token_estimates
        .reserve_for("model", "repl", &RecoveryState::default());
    assert_eq!(initial_reserve.output_tokens, 500);

    for i in 1..=20 {
        let feedback = ContextFeedback::from_usage(0, 0, 0, 1000 + i * 100, false);
        sess.record_feedback("model", "repl", feedback);
    }

    let learned_reserve = sess
        .stats
        .response_token_estimates
        .reserve_for("model", "repl", &RecoveryState::default());
    assert!(
        learned_reserve.output_tokens > 500,
        "after 20 samples with completion > 1000, reserve should exceed default floor 500, got {}",
        learned_reserve.output_tokens
    );
}
