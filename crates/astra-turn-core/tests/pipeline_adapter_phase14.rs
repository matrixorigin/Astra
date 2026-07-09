//! Phase 14 TDD: ContextSources adapter — builds pipeline input from AgenticLoopState-like data.
//!
//! The adapter doesn't depend on AgenticLoopState directly (it's in runtime crate).
//! Instead, it takes the data products that AgenticLoopState holds and packages them
//! into ContextSources. The runtime wires this at the call site.

use std::collections::HashMap;

use astra_turn_core::context_sources::*;
use astra_turn_core::microcompact::ProviderCacheStrategy;
use astra_turn_core::optimize_limits::OptimizeLimits;
use astra_turn_core::pipeline_config::{PipelineConfig, ProviderCachePolicy};
use astra_turn_core::pipeline_session::{PipelineSession, TurnInput};
use astra_turn_core::recovery_state::RecoveryState;
use astra_turn_core::token_accounting::TokenAccounting;

/// Simulates the data available on AgenticLoopState at the point where
/// execute_turn is called. The adapter builds ContextSources from this.
struct MockLoopState {
    messages: Vec<serde_json::Value>,
    prompt_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    active_skills: Vec<String>,
    model_id: String,
    model_limit: u32,
    session_id: String,
    project_context: String,
    cwd: Option<String>,
    git_branch: Option<String>,
    memory_entries: Vec<MemoryEntry>,
    turn_index: u32,
}

impl MockLoopState {
    fn default_test() -> Self {
        Self {
            messages: vec![
                serde_json::json!({"role": "user", "content": "Fix the bug"}),
                serde_json::json!({"role": "assistant", "content": "Looking at it."}),
            ],
            prompt_tokens: 5000,
            cache_read_tokens: 3000,
            cache_creation_tokens: 500,
            active_skills: vec!["code_review".into()],
            model_id: "claude-sonnet-4-6".into(),
            model_limit: 200_000,
            session_id: "test-sess-1".into(),
            project_context: "Rust project with cargo".into(),
            cwd: Some("/home/user/project".into()),
            git_branch: Some("main".into()),
            memory_entries: vec![MemoryEntry::new("User prefers concise answers.")],
            turn_index: 3,
        }
    }

    fn build_turn_state(&self) -> TurnState {
        TurnState {
            messages: self.messages.clone(),
            tool_results: vec![],
            tokens: TokenAccounting::from_fields(
                self.prompt_tokens,
                self.cache_read_tokens,
                self.cache_creation_tokens,
                0,
            ),
            active_skills: self.active_skills.clone(),
            recent_file_reads: HashMap::new(),
            remaining_turns: 20,
            turn_index: self.turn_index,
            recovery: RecoveryState::default(),
            last_user_message: self
                .messages
                .iter()
                .rev()
                .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
                .and_then(|m| m.get("content").and_then(|c| c.as_str()))
                .unwrap_or("")
                .to_string(),
        }
    }

    fn build_session_context(&self) -> SessionContext {
        SessionContext {
            session_id: self.session_id.clone(),
            run_id: "run-1".into(),
            model_id: self.model_id.clone(),
            provider_name: "anthropic".into(),
            model_limit: self.model_limit,
            provider_policy: ProviderCachePolicy::anthropic(),
            provider_strategy: ProviderCacheStrategy::default(),
            project_context: self.project_context.clone(),
            edge_profile: EdgeProfile {
                cwd: self.cwd.clone(),
                git_branch: self.git_branch.clone(),
                ..Default::default()
            },
            self_model: None,
            deferred_tools_block: String::new(),
            skill_listing_block: String::new(),
            current_date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
            user_id: None,
        }
    }

    fn build_external(&self) -> ExternalSources {
        ExternalSources {
            memory_entries: self.memory_entries.clone(),
            spill_dir: None,
            ..Default::default()
        }
    }
}

#[test]
fn adapter_produces_valid_turn_input() {
    let mock = MockLoopState::default_test();
    let turn = mock.build_turn_state();
    let session = mock.build_session_context();
    let external = mock.build_external();

    assert_eq!(turn.messages.len(), 2);
    assert_eq!(turn.turn_index, 3);
    assert_eq!(session.model_limit, 200_000);
    assert_eq!(external.memory_entries.len(), 1);
}

// NOTE: `adapter_feeds_pipeline_session_run_turn`, `adapter_feeds_pipeline_session_adaptive`,
// `pipeline_output_system_blocks_contain_identity`, and `pipeline_output_has_anthropic_cache_markers`
// were removed — they are fully subsumed by the composite integration tests in
// `runtime/src/turn/context_pipeline_adapter.rs` which exercise the real adapter path
// with stronger assertions (section ordering, scope boundary checks, per-marker validation).

#[test]
fn pipeline_output_flattened_matches_concatenation_of_blocks() {
    let mock = MockLoopState::default_test();
    let statics = StaticSections::test_default();
    let agent = AgentContext::default();
    let session = mock.build_session_context();
    let turn = mock.build_turn_state();
    let external = mock.build_external();
    let limits = OptimizeLimits::default();

    let mut sess = PipelineSession::new(PipelineConfig {
        provider_policy: ProviderCachePolicy::anthropic(),
        ..Default::default()
    });

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

    let output = sess.run_turn(input).unwrap();
    let flattened =
        astra_turn_core::context_serializer::flatten_serialized_system_blocks(&output.serialized);

    // Flattened should be non-empty and contain all block texts
    assert!(!flattened.is_empty());
    for block in &output.serialized.system_blocks {
        assert!(
            flattened.contains(&block.text),
            "flattened should contain each block's text"
        );
    }
}

#[test]
fn pipeline_to_anthropic_message_format() {
    use astra_turn_core::context_serializer::system_blocks_to_anthropic_message;

    let mock = MockLoopState::default_test();
    let statics = StaticSections::test_default();
    let agent = AgentContext::default();
    let session = mock.build_session_context();
    let turn = mock.build_turn_state();
    let external = mock.build_external();
    let limits = OptimizeLimits::default();

    let mut sess = PipelineSession::new(PipelineConfig {
        provider_policy: ProviderCachePolicy::anthropic(),
        ..Default::default()
    });

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

    let output = sess.run_turn(input).unwrap();
    let (msg, plain) = system_blocks_to_anthropic_message(&output.serialized);

    // Message shape: {"role": "system", "content": [...]}
    assert_eq!(msg["role"], "system");
    let content = msg["content"].as_array().unwrap();
    assert!(!content.is_empty());

    // Each block is {"type": "text", "text": "..."}
    for block in content {
        assert_eq!(block["type"], "text");
        assert!(block["text"].as_str().is_some());
    }

    // Plain text is non-empty
    assert!(!plain.is_empty());
    assert!(plain.contains("expert")); // from identity section

    // At least one block has cache_control (Anthropic policy)
    assert!(
        content.iter().any(|b| b.get("cache_control").is_some()),
        "Anthropic format should have cache_control on at least one block"
    );
}
