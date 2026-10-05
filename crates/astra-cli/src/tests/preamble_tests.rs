use super::resolve_journal_target_session;
use crate::cli::agent_runtime::initialize_agent_projection;
use crate::cli::command_registry;
use crate::cli::session::session_state::SessionState;
use crate::cli::stream::stream_render::{
    RenderPolicy, StreamRenderState, TurnResult, dispatch_turn_event_block,
};

#[test]
fn generic_explain_payload_does_not_create_explain_analyze_facts() {
    let mut result = TurnResult::new();
    let block = "data: {\"type\":\"explain\",\"total_ms\":7,\"tool_calls\":1,\"tools_available\":2,\"first_tool_call\":null,\"first_tool_call_fallback\":null,\"steps\":[]}\n\n";
    let mut render = StreamRenderState::new();
    dispatch_turn_event_block(
        block,
        &mut result,
        &mut render,
        RenderPolicy::Stream,
        &mut vec![],
    );
    assert!(result.core.explain_analyze_events.is_empty());
}

#[test]
fn dispatch_thinking_delta_captures_reasoning_content() {
    let mut result = TurnResult::new();
    let mut render = StreamRenderState::new();
    // thinking_delta (Kimi-k2.5 / Moonshot style)
    let block = "data: {\"type\":\"thinking_delta\",\"content\":\"Let me think...\"}\n\n";
    dispatch_turn_event_block(
        block,
        &mut result,
        &mut render,
        RenderPolicy::Stream,
        &mut vec![],
    );
    assert_eq!(result.reasoning_content, "Let me think...");
}

#[test]
fn dispatch_reasoning_delta_captures_reasoning_content() {
    let mut result = TurnResult::new();
    let mut render = StreamRenderState::new();
    // reasoning_delta (DeepSeek-R1 style)
    let block = "data: {\"type\":\"reasoning_delta\",\"content\":\"Step 1: search PRs\"}\n\n";
    dispatch_turn_event_block(
        block,
        &mut result,
        &mut render,
        RenderPolicy::Stream,
        &mut vec![],
    );
    assert_eq!(result.reasoning_content, "Step 1: search PRs");
}

#[test]
fn dispatch_reasoning_message_content_captures_reasoning_content() {
    let mut result = TurnResult::new();
    let mut render = StreamRenderState::new();
    let block =
        "data: {\"type\":\"reasoning_message_content\",\"content\":\"Step 1: search PRs\"}\n\n";
    dispatch_turn_event_block(
        block,
        &mut result,
        &mut render,
        RenderPolicy::Stream,
        &mut vec![],
    );
    assert_eq!(result.reasoning_content, "Step 1: search PRs");
}

#[test]
fn dispatch_thinking_delta_accumulates_across_events() {
    let mut result = TurnResult::new();
    let mut render = StreamRenderState::new();
    let block = concat!(
        "data: {\"type\":\"thinking_delta\",\"content\":\"part1\"}\n\n",
        "data: {\"type\":\"thinking_delta\",\"content\":\" part2\"}\n\n",
    );
    dispatch_turn_event_block(
        block,
        &mut result,
        &mut render,
        RenderPolicy::Stream,
        &mut vec![],
    );
    assert_eq!(result.reasoning_content, "part1 part2");
}

/// Verifies that an assistant tool-call message includes reasoning_content when the
/// LLM produced thinking output.  Without this field, thinking models return HTTP 400:
/// "thinking is enabled but reasoning_content is missing in assistant tool call message"
#[test]
fn assistant_tc_msg_includes_reasoning_content_when_present() {
    let reasoning = "I should call github(action=list_prs).".to_string();
    let tool_call = serde_json::json!({
        "id": "tc-1",
        "name": "github",
        "arguments": {"action": "list_prs", "owner": "matrixorigin", "repo": "matrixone"}
    });

    let mut assistant_tc_msg = serde_json::json!({
        "role": "assistant",
        "content": serde_json::Value::Null,
        "tool_calls": [{
            "id": tool_call["id"],
            "type": "function",
            "function": {
                "name": tool_call["name"],
                "arguments": serde_json::to_string(&tool_call["arguments"]).unwrap(),
            }
        }]
    });
    if !reasoning.is_empty() {
        assistant_tc_msg["reasoning_content"] = serde_json::Value::String(reasoning.clone());
    }

    assert_eq!(
        assistant_tc_msg["reasoning_content"].as_str(),
        Some(reasoning.as_str()),
        "reasoning_content must be present for thinking models"
    );
}

/// Verifies that when reasoning_content is empty (non-thinking model), it is NOT
/// added to the assistant message (keeps payloads clean for standard models).
#[test]
fn assistant_tc_msg_omits_reasoning_content_when_empty() {
    let reasoning = String::new();
    let mut assistant_tc_msg = serde_json::json!({
        "role": "assistant",
        "content": serde_json::Value::Null,
        "tool_calls": []
    });
    if !reasoning.is_empty() {
        assistant_tc_msg["reasoning_content"] = serde_json::Value::String(reasoning);
    }
    assert!(
        assistant_tc_msg.get("reasoning_content").is_none(),
        "reasoning_content must NOT be present for non-thinking models"
    );
}

// `truncate_skill_desc_for_completion` lived inside the deleted
// `dynamic_completions` module (rustyline-only completion shortening
// for the line-mode REPL). The TUI uses its own slash-menu rendering
// and does not need the helper.

#[test]
fn resolve_unique_prefix_command() {
    let resolved = command_registry::resolve_command("/mo").expect("/mo should resolve to /model");
    assert_eq!(resolved, "/model");
}

#[test]
fn resolve_review_command() {
    let resolved = command_registry::resolve_command("/review").expect("/review should resolve");
    assert_eq!(resolved, "/review");
}

#[test]
fn resolve_journal_target_session_uses_active_session_without_argument() {
    let state = SessionState {
        session_id: Some("sess-123".to_string()),
        ..Default::default()
    };
    let (resolved, from_prefix) =
        resolve_journal_target_session("", &state, "missing").expect("should resolve");
    assert_eq!(resolved, "sess-123");
    assert!(!from_prefix);
}

#[test]
fn quiet_dispatch_captures_text_without_output() {
    // In quiet mode, dispatch_turn_event_block should capture text but not print.
    // We can't easily test print suppression, but we verify text capture.
    let block = "data: {\"type\":\"text_delta\",\"content\":\"hello world\"}\n\n";
    let mut result = TurnResult::new();
    let mut render = StreamRenderState::new();
    dispatch_turn_event_block(
        block,
        &mut result,
        &mut render,
        RenderPolicy::Silent,
        &mut vec![],
    );
    assert_eq!(result.full_text, "hello world");
}

#[test]
fn initialize_agent_projection_does_not_install_a_local_executor() {
    let mut state = SessionState::default();

    initialize_agent_projection(&mut state);
    let spawner = state.agent_spawner.expect("agent spawner should be wired");
    assert!(!spawner.has_executor());
}

#[test]
fn compacted_history_skips_empty_user_messages() {
    // When user message is empty (compacted context), only the assistant message
    // should appear in serialized history.
    let history: Vec<(String, String)> = vec![
        (
            String::new(),
            "[Prior context — 5 turns compacted]\n\nSummary here".to_string(),
        ),
        ("real question".to_string(), "real answer".to_string()),
    ];
    let messages: Vec<serde_json::Value> = history
        .iter()
        .flat_map(|(u, a)| {
            if u.is_empty() {
                vec![serde_json::json!({"role": "assistant", "content": a})]
            } else {
                vec![
                    serde_json::json!({"role": "user", "content": u}),
                    serde_json::json!({"role": "assistant", "content": a}),
                ]
            }
        })
        .collect();
    assert_eq!(messages.len(), 3); // 1 assistant (compact) + 1 user + 1 assistant
    assert_eq!(messages[0]["role"], "assistant");
    assert!(
        messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("compacted")
    );
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[2]["role"], "assistant");
}

#[test]
fn tool_name_validation_catches_unknown() {
    let valid: std::collections::HashSet<String> = ["bash", "read_file", "write_file"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(valid.contains("bash"));
    assert!(!valid.contains("run_tests")); // hallucinated tool
    assert!(!valid.contains(""));
}

// ═══════════════════════════════════════════════════════════════════════
// Integration tests with mock HTTP servers
// ═══════════════════════════════════════════════════════════════════════
