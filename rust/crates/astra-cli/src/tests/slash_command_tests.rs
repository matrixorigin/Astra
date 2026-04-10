use super::*;

// ── slash commands with mock server ───────────────────────────────────

#[tokio::test]
async fn slash_clear_creates_new_session() {
    let _creds_dir = isolate_credentials();
    let app = Router::new().route(
        "/sessions",
        post(|| async { axum::Json(serde_json::json!({"session_id": "new-sess-42"})) }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
        edge_tools::all_tool_schemas(),
    ));
    let mut state = ReplState {
        session_id: Some("old-sess".to_string()),
        turn: 5,
        history: vec![("q".to_string(), "a".to_string())],
        ..Default::default()
    };
    let exit = handle_slash_command(
        "/clear",
        &api,
        None,
        &mut state,
        Some("fake-token"),
        &selector,
    )
    .await
    .unwrap();
    assert!(!exit);
    assert_eq!(state.session_id.as_deref(), Some("new-sess-42"));
    assert_eq!(state.turn, 0);
    assert!(state.history.is_empty());
}

#[tokio::test]
async fn slash_model_with_arg_sets_model() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
        edge_tools::all_tool_schemas(),
    ));
    let mut state = ReplState::default();
    let exit = handle_slash_command("/model gpt-4o", &api, None, &mut state, None, &selector)
        .await
        .unwrap();
    assert!(!exit);
    assert_eq!(state.model.as_deref(), Some("gpt-4o"));
}

#[tokio::test]
async fn slash_exit_returns_true() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
        edge_tools::all_tool_schemas(),
    ));
    let mut state = ReplState::default();
    let exit = handle_slash_command("/exit", &api, None, &mut state, None, &selector)
        .await
        .unwrap();
    assert!(exit);
}

#[tokio::test]
async fn slash_exit_writes_session_end_to_journal() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
        edge_tools::all_tool_schemas(),
    ));

    let sid = format!("test-exit-end-{}", uuid::Uuid::new_v4());
    let writer = session_journal::JournalWriter::new(&sid).unwrap();
    writer
        .append(&session_journal::JournalEvent::session_start(
            Some(&sid),
            None,
        ))
        .unwrap();

    let mut state = ReplState {
        session_id: Some(sid.clone()),
        turn: 3,
        journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
        ..ReplState::default()
    };

    let exit = handle_slash_command("/exit", &api, None, &mut state, None, &selector)
        .await
        .unwrap();
    assert!(exit);

    // Verify session_end was written to journal
    let events = session_journal::read_journal(&sid).unwrap();
    let has_session_end = events
        .iter()
        .any(|e| matches!(e.event_type, session_journal::JournalEventType::SessionEnd));
    assert!(
        has_session_end,
        "session_end event must be written to journal on /exit"
    );
}

#[tokio::test]
async fn slash_quit_writes_session_end_to_journal() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
        edge_tools::all_tool_schemas(),
    ));

    let sid = format!("test-quit-end-{}", uuid::Uuid::new_v4());
    let writer = session_journal::JournalWriter::new(&sid).unwrap();
    writer
        .append(&session_journal::JournalEvent::session_start(
            Some(&sid),
            None,
        ))
        .unwrap();

    let mut state = ReplState {
        session_id: Some(sid.clone()),
        turn: 1,
        journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
        ..ReplState::default()
    };

    let exit = handle_slash_command("/quit", &api, None, &mut state, None, &selector)
        .await
        .unwrap();
    assert!(exit);

    let events = session_journal::read_journal(&sid).unwrap();
    let has_session_end = events
        .iter()
        .any(|e| matches!(e.event_type, session_journal::JournalEventType::SessionEnd));
    assert!(
        has_session_end,
        "session_end event must be written to journal on /quit"
    );
}

#[tokio::test]
async fn slash_unknown_command_does_not_crash() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
        edge_tools::all_tool_schemas(),
    ));
    let mut state = ReplState::default();
    let exit = handle_slash_command(
        "/nonexistent_command_xyz",
        &api,
        None,
        &mut state,
        None,
        &selector,
    )
    .await
    .unwrap();
    assert!(!exit);
}

#[tokio::test]
async fn slash_health_does_not_crash_empty() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
        edge_tools::all_tool_schemas(),
    ));
    let mut state = ReplState::default();
    // No health entries — should print "no data" gracefully
    let exit = handle_slash_command("/health", &api, None, &mut state, None, &selector)
        .await
        .unwrap();
    assert!(!exit);
}

#[tokio::test]
async fn slash_lsp_status_does_not_crash() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
        edge_tools::all_tool_schemas(),
    ));
    let mut state = ReplState::default();

    let exit = handle_slash_command("/lsp", &api, None, &mut state, None, &selector)
        .await
        .unwrap();
    assert!(!exit);

    let exit = handle_slash_command("/lsp status", &api, None, &mut state, None, &selector)
        .await
        .unwrap();
    assert!(!exit);
}

#[tokio::test]
async fn slash_health_with_entries_does_not_crash() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
        edge_tools::all_tool_schemas(),
    ));
    let mut state = ReplState {
        tool_health_entries: vec![
            astra_runtime::pipeline::persistence::ToolHealthEntry {
                name: "bash".into(),
                total_calls: 15,
                total_failures: 3,
                failure_rate: 0.2,
                last_updated_epoch: 0,
            },
            astra_runtime::pipeline::persistence::ToolHealthEntry {
                name: "grep".into(),
                total_calls: 8,
                total_failures: 0,
                failure_rate: 0.0,
                last_updated_epoch: 0,
            },
        ],
        ..Default::default()
    };
    let exit = handle_slash_command("/health", &api, None, &mut state, None, &selector)
        .await
        .unwrap();
    assert!(!exit);
}

#[tokio::test]
async fn slash_health_detail_mode() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
        edge_tools::all_tool_schemas(),
    ));
    let mut state = ReplState {
        tool_health_entries: vec![astra_runtime::pipeline::persistence::ToolHealthEntry {
            name: "bash".into(),
            total_calls: 10,
            total_failures: 5,
            failure_rate: 0.5,
            last_updated_epoch: 0,
        }],
        ..Default::default()
    };
    let exit = handle_slash_command("/health detail", &api, None, &mut state, None, &selector)
        .await
        .unwrap();
    assert!(!exit);
}
