use super::*;

// ── command_router ────────────────────────────────────────────────────

#[tokio::test]
async fn execute_cli_health_command() {
    let _creds_dir = isolate_credentials();
    let app = Router::new().route(
        "/health",
        get(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let result = execute_cli_command(
        Some(Command::Health),
        Some("nonexistent-profile".to_string()),
        None,
        false,
        None,
        &api,
        false,
        0.0,
        false,
    )
    .await;
    // Health command should succeed regardless of auth
    assert!(result.is_ok());
}

// ── repl_turn pure functions ──────────────────────────────────────────

#[test]
fn build_effective_line_plain() {
    let state = ReplState::default();
    let result =
        repl_turn::build_effective_line("hello", &state, &mut crate::ui_adapter::LineUiAdapter);
    assert_eq!(result, "hello");
}

#[test]
fn build_effective_line_with_system_skills() {
    let mut state = ReplState::default();
    let skills = prompts::builtin_system_skills();
    if let Some(md) = skills.iter().find(|s| s.name == "markdown") {
        state.active_system_skills.push(md.clone());
    }
    let result =
        repl_turn::build_effective_line("hello", &state, &mut crate::ui_adapter::LineUiAdapter);
    assert!(result.contains("hello"));
    assert!(result.contains("Markdown"));
}

#[test]
fn history_as_messages_normal_turns() {
    let history = vec![
        ("q1".to_string(), "a1".to_string()),
        ("q2".to_string(), "a2".to_string()),
    ];
    let msgs = repl_turn::history_as_messages(&history);
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[1]["role"], "assistant");
}

#[test]
fn history_as_messages_compacted_turn() {
    let history = vec![("".to_string(), "summary".to_string())];
    let msgs = repl_turn::history_as_messages(&history);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["role"], "assistant");
}

// ── slash_memory mock ─────────────────────────────────────────────────

#[tokio::test]
async fn slash_memory_search_with_mock() {
    let app = Router::new().route(
        "/memory/search",
        post(|| async {
            axum::Json(serde_json::json!({
                "results": [
                    {"content": "user prefers Rust", "memory_type": "profile", "score": 0.9}
                ]
            }))
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let mut state = ReplState {
        session_id: Some("sess-1".to_string()),
        ..Default::default()
    };
    // This should not panic or error
    let result = handle_memory_domain_command(
        "/memory",
        "search rust preferences",
        &api,
        &mut state,
        Some("fake-token"),
    )
    .await;
    assert!(result.is_ok());
}
