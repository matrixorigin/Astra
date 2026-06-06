//! CLI modules — all sub-modules under `src/cli/`.
//!
//! This file replaces the 80+ `#[path = "cli/xxx.rs"] mod xxx;` declarations
//! that were previously in `main.rs`, following standard Rust module conventions.

// Re-export external crate aliases so cli/ files using `use super::*`
// can reference `prompts::`, `session_journal::`, `tool_registry::`, etc.
pub(crate) use astra_runtime::prompts;
pub(crate) use astra_runtime::tool_registry;
pub(crate) use astra_services::session_journal;

// Standard library re-exports — previously resolved via main.rs's top-level `use`
pub(crate) use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command as SysCommand, Stdio},
    sync::{Mutex, OnceLock},
};

// Third-party crate re-exports — these were in main.rs's top-level `use` block
pub(crate) use crossterm::{
    cursor,
    event::{self, KeyEvent},
    style::Stylize,
    terminal,
};
pub(crate) use serde::{Deserialize, Serialize};

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
pub mod file_history;
pub mod followup_suggestion;
pub mod http_task_service;
pub mod http_team_store;
pub mod idle_agent_messages;
pub mod interactive_chat;
pub mod journal_diff;
pub mod journal_digest;
pub mod journal_tree;
pub mod mcp_config;
pub mod mock_llm;
pub mod notifications;
pub mod one_shot_session_routing;
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
pub mod task;
pub mod terminal_hyperlinks;
pub mod terminal_region;
pub mod theme;
pub mod tool_call_groups;
pub mod tool_result_status;
pub mod turn;
pub mod ui_adapter;
pub mod workspace_trust;

pub(crate) use self::agent_runtime::initialize_multi_agent_runtime;
pub(crate) use self::auth_flow::{
    do_login, do_register, is_auth_error, is_llm_provider_auth_error,
};
pub(crate) use self::chat_stream::{
    ApprovalRequestTx, AskUserRequestTx, ChatTurnParams, PlanReviewRequestTx, StreamEvent,
    StreamEventTx, stream_chat_sse,
};
pub(crate) use self::cli_config::cli_args;
pub(crate) use self::cli_config::cli_args::JournalDigestArgs;
pub(crate) use self::cli_config::cli_context;
pub(crate) use self::cli_config::cli_output;
pub(crate) use self::cli_config::cli_utils;
pub(crate) use self::cli_config::cli_utils::{
    SessionResumePreflight, clear_profile_last_session_if_matches_or_warn, compact_or_raw,
    credential_store, get_profile_and_token, interactive_select, load_credentials, map_thin_err,
    normalize_model_override, persist_profile_last_session_or_warn,
    persist_profile_memoria_api_key, prefix_chars, preflight_remote_resume_session,
    print_json_or_raw, profile_name, prompt_or, prompt_password_masked, truncate_str, urlencoding,
};
pub(crate) use self::cloud_sync::{
    append_cloud_pull_sync_journal, try_cloud_pull, try_cloud_pull_preferences,
};
pub(crate) use self::edge_lifecycle::register_and_start_heartbeat;
pub(crate) use self::permission_manager::PermissionManager;
pub(crate) use self::plan::{
    plan_commands, plan_executor, plan_lifecycle, plan_runtime, plan_task_board,
};
pub(crate) use self::project_instructions::discover_project_instructions;
pub(crate) use self::session::session_runtime;
pub(crate) use self::session::session_runtime::print_session_banner;
pub(crate) use self::session::session_state;
pub(crate) use self::session::session_state::{ContinuationAnchor, ExplainMode, SessionState};
pub(crate) use self::session::{
    session_checkpointing, session_compaction, session_continuation, session_input,
    session_lessons, session_projection, session_recovery, session_restore_client,
    session_side_effects, session_startup, session_stats_scan, session_todo_client,
};
pub(crate) use self::slash::{
    slash_agent, slash_config, slash_info, slash_inspect, slash_mcp, slash_memory, slash_plan,
    slash_router, slash_session, slash_stats, slash_task, slash_team, slash_telemetry,
};
pub(crate) use self::startup_trace::StartupTracer;
pub(crate) use self::stream::stream_render;
pub(crate) use self::stream::streaming_types::StreamResult;
pub(crate) use self::stream::{stream_events_writer, streaming_md, streaming_types};
pub(crate) use self::surface::{run_status_surface, task_checkpoint_surface};
pub(crate) use self::task::{task_result_projection, task_summary};
pub(crate) use self::turn::{turn_entry, turn_facade};
