use super::*;

#[test]
fn dispatch_turn_event_collects_explain_events() {
    let mut result = TurnResult::new();
    let block = "data: {\"type\":\"explain\",\"total_ms\":7,\"tools_selected\":1,\"tools_available\":2,\"tool_selection\":null,\"tool_selection_fallback\":null,\"steps\":[]}\n\n";
    let mut render = StreamRenderState::new();
    dispatch_turn_event_block(block, &mut result, &mut render, false, &mut vec![]);
    assert_eq!(result.explain_turns.len(), 1);
    assert_eq!(
        result.explain_turns[0].get("type").and_then(|v| v.as_str()),
        Some("explain")
    );
}

#[test]
fn dispatch_thinking_delta_captures_reasoning_content() {
    let mut result = TurnResult::new();
    let mut render = StreamRenderState::new();
    // thinking_delta (Kimi-k2.5 / Moonshot style)
    let block = "data: {\"type\":\"thinking_delta\",\"content\":\"Let me think...\"}\n\n";
    dispatch_turn_event_block(block, &mut result, &mut render, false, &mut vec![]);
    assert_eq!(result.reasoning_content, "Let me think...");
}

#[test]
fn dispatch_reasoning_delta_captures_reasoning_content() {
    let mut result = TurnResult::new();
    let mut render = StreamRenderState::new();
    // reasoning_delta (DeepSeek-R1 style)
    let block = "data: {\"type\":\"reasoning_delta\",\"content\":\"Step 1: search PRs\"}\n\n";
    dispatch_turn_event_block(block, &mut result, &mut render, false, &mut vec![]);
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
    dispatch_turn_event_block(block, &mut result, &mut render, false, &mut vec![]);
    assert_eq!(result.reasoning_content, "part1 part2");
}

/// Verifies that an assistant tool-call message includes reasoning_content when the
/// LLM produced thinking output.  Without this field, thinking models return HTTP 400:
/// "thinking is enabled but reasoning_content is missing in assistant tool call message"
#[test]
fn assistant_tc_msg_includes_reasoning_content_when_present() {
    let reasoning = "I should call github_list_prs.".to_string();
    let tool_call = serde_json::json!({
        "id": "tc-1",
        "name": "github_list_prs",
        "arguments": {"owner": "matrixorigin", "repo": "matrixone"}
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

#[test]
fn truncate_skill_desc_for_completion_empty() {
    assert_eq!(truncate_skill_desc_for_completion(""), "");
}

#[test]
fn truncate_skill_desc_for_completion_short_unchanged() {
    let s = "Short skill blurb";
    assert_eq!(truncate_skill_desc_for_completion(s), s);
}

#[test]
fn truncate_skill_desc_for_completion_exact_limit_no_ellipsis() {
    let s: String = (0..39).map(|_| 'a').collect();
    assert_eq!(truncate_skill_desc_for_completion(&s), s);
}

#[test]
fn truncate_skill_desc_for_completion_ascii_long_gets_ellipsis() {
    let s: String = (0..45).map(|_| 'b').collect();
    let out = truncate_skill_desc_for_completion(&s);
    assert!(out.ends_with('…'), "out={out:?}");
    assert_eq!(out.chars().count(), 40);
    let head: String = s.chars().take(39).collect();
    assert!(out.starts_with(&head));
}

/// Regression: byte index 39 was inside U+2014 EM DASH (3 bytes) — must not slice by bytes.
#[test]
fn truncate_skill_desc_for_completion_em_dash_no_panic() {
    let s = "Review and manage persistent memories — promote, clean up, and organize knowledge across sessions";
    let expect: String = s.chars().take(39).collect();
    let out = truncate_skill_desc_for_completion(s);
    assert_eq!(out, format!("{expect}…"));
}

#[test]
fn truncate_skill_desc_for_completion_cjk_no_panic() {
    let s = "数据".repeat(30);
    let out = truncate_skill_desc_for_completion(&s);
    assert!(out.ends_with('…'));
    assert_eq!(out.chars().count(), 40);
}

#[test]
fn resolve_unique_prefix_command() {
    let resolved = resolve_slash_command("/mo").expect("/mo should resolve to /model");
    assert_eq!(resolved, "/model");
}

#[test]
fn resolve_review_command() {
    let resolved = resolve_slash_command("/review").expect("/review should resolve");
    assert_eq!(resolved, "/review");
}

#[test]
fn resolve_journal_target_session_uses_active_session_without_argument() {
    let state = ReplState {
        session_id: Some("sess-123".to_string()),
        ..Default::default()
    };
    let (resolved, from_prefix) =
        resolve_journal_target_session("", &state, "missing").expect("should resolve");
    assert_eq!(resolved, "sess-123");
    assert!(!from_prefix);
}

#[tokio::test]
async fn slash_explain_toggles_state() {
    let api =
        astra_thin_client::ThinClient::new("http://127.0.0.1:8000", None).expect("test API URL");
    let mut state = ReplState::default();
    let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
        edge_tools::all_tool_schemas(),
    ));
    assert_eq!(state.explain, ExplainMode::Off);

    let should_exit = handle_slash_command("/explain", &api, None, &mut state, None, &selector)
        .await
        .expect("slash command should succeed");
    assert!(!should_exit);
    assert_eq!(state.explain, ExplainMode::On);

    let should_exit = handle_slash_command("/explain", &api, None, &mut state, None, &selector)
        .await
        .expect("slash command should succeed");
    assert!(!should_exit);
    assert_eq!(state.explain, ExplainMode::Verbose);

    let should_exit = handle_slash_command("/explain", &api, None, &mut state, None, &selector)
        .await
        .expect("slash command should succeed");
    assert!(!should_exit);
    assert_eq!(state.explain, ExplainMode::Off);
}

#[test]
fn quiet_dispatch_captures_text_without_output() {
    // In quiet mode, dispatch_turn_event_block should capture text but not print.
    // We can't easily test print suppression, but we verify text capture.
    let block = "data: {\"type\":\"text_delta\",\"content\":\"hello world\"}\n\n";
    let mut result = TurnResult::new();
    let mut render = StreamRenderState::new();
    dispatch_turn_event_block(block, &mut result, &mut render, true, &mut vec![]);
    assert_eq!(result.full_text, "hello world");
}

#[tokio::test]
async fn initialize_multi_agent_runtime_wires_spawner_and_engine() {
    let api =
        astra_thin_client::ThinClient::new("http://127.0.0.1:8000", None).expect("test API URL");
    let mut state = ReplState::default();

    initialize_multi_agent_runtime(&mut state, &api, "fake-token".to_string()).await;

    assert!(state.delegation_engine.is_some());
    let spawner = state.agent_spawner.expect("agent spawner should be wired");
    assert!(spawner.has_executor());
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
fn compact_assistant_message_optional_session_memory_anchor() {
    let with_anchor =
        repl_turn::compact_assistant_message(3, "Summary body", Some("- [fact] one\n- [fact] two"));
    assert!(with_anchor.contains("[Session memory anchor]"));
    assert!(with_anchor.contains("[fact] one"));
    assert!(with_anchor.contains("[Prior context — 3 turns compacted]"));
    assert!(with_anchor.contains("Summary body"));

    let no_anchor = repl_turn::compact_assistant_message(2, "Only summary", None);
    assert!(!no_anchor.contains("[Session memory anchor]"));
    assert!(no_anchor.contains("[Prior context — 2 turns compacted]"));
    assert!(no_anchor.contains("Only summary"));
}

#[test]
fn system_skill_toggle_lifecycle() {
    let available = prompts::builtin_system_skills();
    let mut active: Vec<prompts::SystemSkill> = Vec::new();

    // Activate markdown
    let md = available.iter().find(|s| s.name == "markdown").unwrap();
    active.push(md.clone());
    assert_eq!(active.len(), 1);

    // Deactivate markdown
    active.retain(|s| s.name != "markdown");
    assert!(active.is_empty());

    // Activate both
    for s in &available {
        active.push(s.clone());
    }
    assert!(active.len() >= 2);

    // Build instructions
    let block = prompts::build_skill_instructions(&active);
    assert!(block.contains("Markdown"));
    assert!(block.contains("Concise"));
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
