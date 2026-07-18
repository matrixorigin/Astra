#![allow(unstable_name_collisions)]
//! `astra-cli` library crate.
//!
//! Provides the CLI application logic shared between the main `astra` binary
//! and external consumers (tests, sub-binaries like `mock_mcp_server`).
//!
//! Module layout:
//! - `cli/` — CLI-specific modules (slash commands, TUI, session mgmt, etc.)
//! - `edge_tools` / `edge_tools/` — edge tool definitions and execution
//! - `tui` — Ratatui terminal UI
//! - Top-level utility modules (delta_log, diff_utils, mcp_client, etc.)

// Clippy 1.94 — allow backlog; refine incrementally.
#![allow(
    dead_code,
    deprecated,
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::field_reassign_with_default,
    clippy::items_after_test_module,
    clippy::let_unit_value,
    clippy::manual_strip,
    clippy::needless_borrow,
    clippy::redundant_closure,
    clippy::single_match,
    clippy::unnecessary_mut_passed
)]

// ═══════════════════════════ Top-level utility modules ═══════════════════
pub mod admin_cli;
pub(crate) mod background_task_error;
pub mod diff_utils;
pub mod edge_tools;
pub mod entrypoint;
pub mod explain_dag;
pub mod git_branch_cache;
pub mod lock_recovery;
pub mod manifest_loader;
pub mod mcp_client;
pub mod sandbox_retry;
pub(crate) mod skill_instructions;
#[cfg(test)]
pub(crate) mod test_utils;
pub mod tool_safety_guard;

// ═══════════════════════════ CLI modules ═════════════════════════════════
pub mod cli;

// ═══════════════════════════ TUI ════════════════════════════════════════
pub mod tui;

// SSE streaming types
pub(crate) use crate::cli::stream::streaming_types::{
    PartialTurnData, StreamResult, TurnFailure, VerdictEvent,
};

// Session state
pub(crate) use crate::cli::plan::plan_monitor::{format_duration_short, format_plan_progress};
pub(crate) use cli::session::session_state::{ExplainMode, SessionState, SkillDevState};

// Cloud sync
pub(crate) use cli::cloud_sync::post_auth_cloud_resync;

// Core command routing

#[cfg(test)]
pub(crate) mod tests {
    pub(crate) use super::test_utils::HomeGuard;
    pub(crate) use super::test_utils::TestUi;
    pub(crate) use super::test_utils::heavy_checkpoint_with_runtime_state;
    pub(crate) use super::test_utils::isolate_credentials;
    pub(crate) use super::test_utils::isolated_sessions_dir;
    pub(crate) use super::test_utils::stub_stream_result;
    pub(crate) use super::test_utils::stub_stream_result_with_records;
    pub(crate) use super::test_utils::test_temp_dir;
    pub(crate) use super::test_utils::wait_until;

    pub(crate) use crate::cli::slash::slash_session::resolve_journal_target_session;
    pub(crate) use crate::cli::slash::slash_task;
    pub(crate) use astra_services::session_journal;
    use axum::{Router, routing::get};

    async fn mock_models_response() -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!({
            "models": [
                {
                    "name": "test-model",
                    "is_active": true,
                    "context_window": 200_000
                },
                {
                    "name": "mock-model",
                    "is_active": true,
                    "context_window": 200_000
                }
            ]
        }))
    }

    async fn spawn_mock_app(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        tokio::task::yield_now().await;
        base
    }

    async fn spawn_mock(app: Router) -> String {
        spawn_mock_app(app.route("/models", get(mock_models_response))).await
    }

    mod auth_tests;
    mod chat_stream_tests;
    mod chat_turn_tests;
    mod cli_args_tests;
    mod cloud_sync_tests;
    mod cost_tracking_tests;
    mod preamble_tests;
    mod resume_tests;
    mod self_command_tests;
    mod slash_command_tests;
    mod stats_tools_tests;
}
