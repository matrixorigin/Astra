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

// ── Re-export all pub(crate) items so sibling modules see them via `use super::*` ──
pub(crate) use self::agent_runtime::*;
pub(crate) use self::auth_flow::*;
pub(crate) use self::chat_stream::*;
pub(crate) use self::cli_config::cli_args::*;
pub(crate) use self::cli_config::cli_utils::*;
pub(crate) use self::cloud_sync::*;
pub(crate) use self::edge_lifecycle::*;
pub(crate) use self::permission_manager::*;
pub(crate) use self::project_instructions::*;
pub(crate) use self::session::session_runtime::*;
pub(crate) use self::session::session_state::*;
pub(crate) use self::slash::slash_account::*;
pub(crate) use self::slash::slash_bug::*;
pub(crate) use self::slash::slash_debug::*;
pub(crate) use self::slash::slash_info::*;
pub(crate) use self::slash::slash_memory::*;
pub(crate) use self::slash::slash_messaging::*;
pub(crate) use self::slash::slash_session::*;
pub(crate) use self::slash::slash_skill::*;
pub(crate) use self::slash::slash_state::*;
pub(crate) use self::startup_trace::*;
pub(crate) use self::stream::streaming_types::*;
