use super::spawn_mock;
use crate::cli::session::session_state::SessionState;
use crate::cli::slash::slash_router::handle_slash_command;
use crate::tests::isolate_credentials;
use axum::{Router, routing::post};

// ── slash commands with mock server ───────────────────────────────────

#[serial_test::serial]
#[tokio::test]
async fn slash_clear_creates_new_session() {
    let _creds_dir = isolate_credentials();
    let app = Router::new().route(
        "/sessions",
        post(|| async { axum::Json(serde_json::json!({"session_id": "new-sess-42"})) }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let mut state = SessionState {
        session_id: Some("old-sess".to_string()),
        turn: 5,
        history: vec![("q".to_string(), "a".to_string())],
        ..Default::default()
    };
    let exit = handle_slash_command("/clear", &api, None, &mut state, Some("fake-token"))
        .await
        .unwrap();
    assert!(!exit);
    assert_eq!(state.session_id.as_deref(), Some("new-sess-42"));
    assert_eq!(state.turn, 0);
    assert!(state.history.is_empty());
}

#[serial_test::serial]
#[tokio::test]
async fn slash_model_with_arg_sets_model() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let mut state = SessionState::default();
    let exit = handle_slash_command("/model gpt-4o", &api, None, &mut state, None)
        .await
        .unwrap();
    assert!(!exit);
    assert_eq!(state.model.as_deref(), Some("gpt-4o"));
}

#[serial_test::serial]
#[tokio::test]
async fn slash_model_catalog_selection_is_exact_and_preserves_state_on_rejection() {
    use crate::cli::slash::slash_config;
    let _creds_dir = isolate_credentials();
    let app = Router::new().route(
        "/models",
        axum::routing::get(|| async {
            let item = |offering_id: &str, name: &str, active: bool| {
                serde_json::json!({
                    "offering_id": offering_id, "access_id": "personal", "access_kind": "this_device",
                    "access_label": "Personal", "execution_placement": "edge",
                    "name": name, "provider": "openai", "is_active": active,
                    "context_window": 8192, "max_completion_tokens": 1024
                })
            };
            axum::Json(serde_json::json!({
                "items": [item("runner-one", "Work", true), item("runner-two", "Work", false),
                    item("runner-unique", "Unique", true), item("runner-three", "Work", true)],
                "next_cursor": null, "limit": 50, "total": 4, "catalog_revision": "sha256:test"
            }))
        }),
    );
    let base = super::spawn_mock_app(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let mut state = SessionState::default();
    for (selector, expected_name, expected_offering) in [
        ("runner-one", "Work", "runner-one"),
        ("work", "Work", "runner-one"), // ambiguous, including offline identities
        ("runner-two", "Work", "runner-one"), // unavailable exact identity
        ("unknown", "Work", "runner-one"),
        (
            "UNIQUE(thinking:high)",
            "Unique(thinking:high)",
            "runner-unique",
        ),
    ] {
        assert!(
            !handle_slash_command(
                &format!("/model {selector}"),
                &api,
                None,
                &mut state,
                Some("fake-token")
            )
            .await
            .unwrap()
        );
        assert_eq!(state.model.as_deref(), Some(expected_name));
        assert_eq!(state.offering_id.as_deref(), Some(expected_offering));
    }
    handle_slash_command(
        "/model runner-three",
        &api,
        None,
        &mut state,
        Some("fake-token"),
    )
    .await
    .unwrap();
    assert_eq!(state.offering_id.as_deref(), Some("runner-three"));
    crate::cli::session::session_runtime::ensure_state_default_model(
        &api,
        "fake-token",
        &mut state,
    )
    .await;
    assert_eq!(
        state.offering_id.as_deref(),
        Some("runner-three"),
        "the next-turn refresh must preserve the explicitly selected Offering"
    );
    let mut other_session = SessionState::default();
    handle_slash_command(
        "/model runner-one",
        &api,
        None,
        &mut other_session,
        Some("fake-token"),
    )
    .await
    .unwrap();
    assert_eq!(
        state.offering_id.as_deref(),
        Some("runner-three"),
        "a second session cannot overwrite the first selection"
    );
    slash_config::set_active_model_for_display(None);
}

#[tokio::test]
async fn slash_exit_returns_true() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let mut state = SessionState::default();
    let exit = handle_slash_command("/exit", &api, None, &mut state, None)
        .await
        .unwrap();
    assert!(exit);
}

// `slash_exit_writes_session_end_to_journal` and
// `slash_quit_writes_session_end_to_journal` exercised the
// line-mode REPL exit path through `finalize_repl_exit`. Both the
// path and the function are gone with the rest of the line-mode
// REPL; session_end is written by the TUI shutdown handler instead.

#[tokio::test]
async fn slash_unknown_command_does_not_crash() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let mut state = SessionState::default();
    let exit = handle_slash_command("/nonexistent_command_xyz", &api, None, &mut state, None)
        .await
        .unwrap();
    assert!(!exit);
}

#[tokio::test]
async fn slash_health_does_not_crash_empty() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let mut state = SessionState::default();
    // No health entries — should print "no data" gracefully
    let exit = handle_slash_command("/health", &api, None, &mut state, None)
        .await
        .unwrap();
    assert!(!exit);
}

#[tokio::test]
async fn slash_lsp_status_does_not_crash() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let mut state = SessionState::default();

    let exit = handle_slash_command("/lsp", &api, None, &mut state, None)
        .await
        .unwrap();
    assert!(!exit);

    let exit = handle_slash_command("/lsp status", &api, None, &mut state, None)
        .await
        .unwrap();
    assert!(!exit);
}

#[tokio::test]
async fn slash_health_with_entries_does_not_crash() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let mut state = SessionState {
        tool_health_entries: vec![
            astra_turn_core::tool_health_persistence::ToolHealthEntry {
                name: "bash".into(),
                total_calls: 15,
                total_failures: 3,
                input_validation_failures: 0,
                failure_rate: 0.2,
                last_updated_epoch: 0,
                recent_outcomes: vec![],
            },
            astra_turn_core::tool_health_persistence::ToolHealthEntry {
                name: "grep".into(),
                total_calls: 8,
                total_failures: 0,
                input_validation_failures: 0,
                failure_rate: 0.0,
                last_updated_epoch: 0,
                recent_outcomes: vec![],
            },
        ],
        ..Default::default()
    };
    let exit = handle_slash_command("/health", &api, None, &mut state, None)
        .await
        .unwrap();
    assert!(!exit);
}

#[tokio::test]
async fn slash_health_detail_mode() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let mut state = SessionState {
        tool_health_entries: vec![astra_turn_core::tool_health_persistence::ToolHealthEntry {
            name: "bash".into(),
            total_calls: 10,
            total_failures: 5,
            input_validation_failures: 0,
            failure_rate: 0.5,
            last_updated_epoch: 0,
            recent_outcomes: vec![],
        }],
        ..Default::default()
    };
    let exit = handle_slash_command("/health detail", &api, None, &mut state, None)
        .await
        .unwrap();
    assert!(!exit);
}

#[tokio::test]
async fn slash_cache_does_not_crash_without_active_session() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let mut state = SessionState::default();
    let exit = handle_slash_command("/cache", &api, None, &mut state, None)
        .await
        .unwrap();
    assert!(!exit);
}

#[tokio::test]
async fn slash_inspect_cache_does_not_crash_without_active_session() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let mut state = SessionState::default();
    let exit = handle_slash_command("/inspect cache", &api, None, &mut state, None)
        .await
        .unwrap();
    assert!(!exit);
}
