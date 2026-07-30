//! CLI modules — all sub-modules under `src/cli/`.
//!
//! This file replaces the 80+ `#[path = "cli/xxx.rs"] mod xxx;` declarations
//! that were previously in `main.rs`, following standard Rust module conventions.

// ── Module declarations ──
pub mod agent_loader;
pub mod agent_runtime;
pub mod app_server;
pub mod arg_render;
pub mod auth_flow;
pub mod chat_stream;
pub mod cli_config;
pub mod cloud_sync;
pub mod command_registry;
pub mod command_router;
pub mod command_usage;
pub mod config_manager;
pub mod context_dump;
pub mod context_references;
pub mod delegate_subrun;
pub mod diagnostic_log;
pub mod diff_presenter;
pub mod durable_bridge;
pub mod edge_lifecycle;
pub mod effects;
pub mod execution_state_summary;
pub(crate) mod exit_code;
pub mod followup_suggestion;
pub(crate) mod history_work;
pub mod http_task_service;
pub mod http_team_store;
pub mod interactive_chat;
pub mod journal_diff;
pub mod journal_digest;
pub mod journal_tree;
pub mod mcp_config;
pub mod mock_llm;
pub mod notifications;
pub mod one_shot_session_routing;
pub(crate) mod permission_command;
pub mod permission_manager;
pub mod plan;
pub mod preferences_client;
pub mod project_instructions;
pub mod self_command;
pub mod session;
pub mod skill_catalog;
pub mod skill_subrun;
pub mod slash;
pub mod spawn_subrun;
pub mod sse_utils;
pub mod startup_trace;
pub mod stream;
pub mod surface;
pub(crate) mod task;
pub mod terminal_hyperlinks;
pub mod terminal_region;
pub mod theme;
pub mod tool_call_groups;
pub mod tool_result_status;
pub(crate) mod tool_surface_injection;
pub mod turn;
pub mod ui_adapter;
pub mod workspace_trust;
