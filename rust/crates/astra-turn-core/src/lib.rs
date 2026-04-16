//! Core turn types, contracts, and pure helpers extracted from the runtime crate.
//!
//! This crate contains modules that have no dependency on the runtime's
//! infrastructure (AppState, database connections, Axum, etc.) and can be
//! tested and compiled independently.

// Allow pre-existing patterns from runtime extraction; fix incrementally.
#![allow(clippy::collapsible_if, clippy::type_complexity, clippy::unnecessary_map_or)]

pub mod agent_progress_ui;
pub mod agentic_recursion_guard;
pub mod agentic_verdict_audit;
pub mod boost_domain_hints;
pub mod cache;
pub mod cache_diagnostics;
pub mod chat_history_openai;
pub mod chat_turn_api_error;
pub mod chat_turn_explain_wire;
pub mod confidence_contract;
pub mod context_assembly_trace;
pub mod edge_executor_id;
pub mod error_recovery;
pub mod execution_state;
pub mod explain;
pub mod explain_report_lines;
pub mod file_edit_journal;
pub mod firewall;
pub mod followup_suggestion;
pub mod headless_tool_assembly;
pub mod headless_tool_status_display;
pub mod headless_tool_stderr_lines;
pub mod history;
pub mod hook_plans;
pub mod interruption;
pub mod microcompact;
pub mod observer;
pub mod persist_inputs;
pub mod prepare_turn_explain_text;
pub mod refresh;
pub mod response_guard;
pub mod routing;
pub mod safety_middleware;
pub mod skill_instructions_merge;
pub mod snapshot;
pub mod sse_blocks;
pub mod sse_edge_stderr_lines;
pub mod state;
pub mod stop_hooks;
pub mod tail_persist;
pub mod task;
pub mod tool_args_repair;
pub mod tool_call_shape;
pub mod tool_result_sanitize;
pub mod tool_result_semantics;
pub mod tool_result_storage;
pub mod tool_selection;
pub mod unconsumed;
pub mod view;
pub mod xml_tool_call_fallback;
pub mod parallel_tool_exec;
pub mod streaming_tool_exec;
