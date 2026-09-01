use super::spawn_mock;
use crate::cli::cli_config::cli_args::Command;
use crate::cli::command_router::execute_cli_command;
use crate::cli::session::session_state::SessionState;
use crate::cli::slash::slash_memory::handle_memory_domain_command;
use crate::tests::isolate_credentials;
use astra_runtime::prompts;
use axum::{Router, routing::get, routing::post};

// ── command_router ────────────────────────────────────────────────────

#[serial_test::serial]
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
        &crate::cli::cli_config::cli_context::CliContext::default(),
    )
    .await;
    // Health command should succeed regardless of auth
    assert!(result.is_ok());
}

// ── chat_turn pure functions ──────────────────────────────────────────

#[test]
fn prepare_input_keeps_plain_user_message() {
    let state = SessionState::default();
    let result = crate::cli::session::session_input::prepare_input(
        "hello",
        &state,
        &mut crate::cli::ui_adapter::LineUiAdapter,
    );
    assert_eq!(result.user_message, "hello");
    assert!(result.runtime_required_texts.is_empty());
}

#[test]
fn prepare_input_routes_system_skills_out_of_user_message() {
    let mut state = SessionState::default();
    let skills = prompts::builtin_system_skills();
    if let Some(md) = skills.iter().find(|s| s.name == "markdown") {
        state.active_system_skills.push(md.clone());
    }
    let result = crate::cli::session::session_input::prepare_input(
        "hello",
        &state,
        &mut crate::cli::ui_adapter::LineUiAdapter,
    );
    assert_eq!(result.user_message, "hello");
    assert_eq!(result.active_system_skill_names, vec!["markdown"]);
    assert!(result.runtime_required_texts[0].contains("Markdown"));
}

#[test]
fn history_as_messages_normal_turns() {
    let history = vec![
        ("q1".to_string(), "a1".to_string()),
        ("q2".to_string(), "a2".to_string()),
    ];
    let msgs = crate::cli::session::session_projection::history_as_messages(&history);
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[1]["role"], "assistant");
}

#[test]
fn history_as_messages_compacted_turn() {
    let history = vec![("".to_string(), "summary".to_string())];
    let msgs = crate::cli::session::session_projection::history_as_messages(&history);
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
    let mut state = SessionState {
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
