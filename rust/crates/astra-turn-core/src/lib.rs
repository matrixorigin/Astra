//! Core turn types, contracts, and pure helpers extracted from the runtime crate.
//!
//! This crate contains modules that have no dependency on the runtime's
//! infrastructure (AppState, database connections, Axum, etc.) and can be
//! tested and compiled independently.

// Allow pre-existing patterns from runtime extraction; fix incrementally.
#![allow(
    clippy::collapsible_if,
    clippy::type_complexity,
    clippy::unnecessary_map_or
)]

pub mod agentic_recursion_guard;
pub mod agentic_verdict_audit;
pub mod boost_domain_hints;
pub mod cache;
pub mod cache_diagnostics;
pub mod chat_history_openai;
pub mod chat_turn_api_error;
pub mod chat_turn_explain_wire;
pub mod chat_turn_heuristics;
pub mod chat_turn_payload;
pub mod compaction_types;
pub mod compression_types;
pub mod concurrency_safety;
pub mod confidence_contract;
pub mod context_assembly_trace;
pub mod conversation_log;
pub mod edge_executor_id;
pub mod error_recovery;
pub mod evaluation;
pub mod execution_state;
pub mod explain;
pub mod explain_report_lines;
pub mod file_edit_journal;
pub mod followup_suggestion;
pub mod fork_cache_event;
pub mod fork_capture;
pub mod fork_prefix;
pub mod fork_prefix_store;
pub mod fork_reconstruct;
pub mod fork_resolve;
pub mod headless_tool_assembly;
pub mod headless_tool_status_display;
pub mod headless_tool_stderr_lines;
pub mod history;
pub mod hook_plans;
pub mod interaction_types;
pub mod interruption;
pub mod learning_quality_gate;
pub mod microcompact;
pub mod observer;
pub mod parallel_tool_exec;
pub mod pipeline_learning;
pub mod prepare_turn_explain_text;
pub mod recent_arg_hints;
pub mod response_guard;
pub mod routing;
pub mod safety_middleware;
pub mod selector_observability;
pub mod skill_selector_metrics;
pub mod sse_blocks;
pub mod sse_edge_stderr_lines;
pub mod state;
pub mod stop_hooks;
pub mod stop_hooks_yaml;
pub mod streaming_tool_exec;
pub mod tail_persist;
pub mod task;
pub mod thinking_config;
pub mod tool_args_repair;
pub mod tool_call_shape;
pub mod tool_health;
pub mod tool_hooks;
pub mod tool_result_compression;
pub mod tool_result_dedup;
pub mod tool_result_sanitize;
pub mod tool_result_semantics;
pub mod tool_result_storage;
pub mod tool_schema_prune;
pub mod tool_selection;
pub mod view;
pub mod xml_tool_call_fallback;

// Phase 15: turn leaf modules + cloud session modules
pub mod action_compensation;
pub mod approval_fingerprint;
pub mod chat_turn_sse_dispatch;
pub mod cloud_approval_policy;
pub mod cloud_attachments;
pub mod cloud_cache_diagnostics;
pub mod cloud_grouping;
pub mod cloud_session_facts;
pub mod cloud_session_memory_extract;
pub mod counter;
pub mod delegation_tree;
pub mod goal_tracker;
pub mod headless_tool_journal;
pub mod orchestration_types;
pub mod permission_sync;
pub mod permission_types;
pub mod persist;
pub mod quality;
pub mod routing_metrics;
pub mod tool_registry_chain;
pub mod tool_registry_meta;
pub mod tool_registry_report;

// Orchestration + liquid modules
pub mod liquid_step_signals;
pub mod liquid_tactical;
pub mod orchestration_builtin_agents;
pub mod orchestration_context_cache;
pub mod orchestration_progress;
pub mod orchestration_spawn_tool;
pub mod orchestration_team_config;
pub mod stream_events;
pub mod tool_argument_hints;

// Phase 16: bridge, edge, stall
pub mod bridge_circuit_breaker;
pub mod bridge_rate_limit_cooldown;
pub mod bridge_sse_events;
pub mod complete;
pub mod edge_prompt_context;
pub mod hydrate_reflect;
pub mod loop_circuit_breaker;
pub mod stall;
pub mod tool_registry_state;

// Phase 17: telemetry, replay, edge profile
pub mod activity;
pub mod agentic_turn_telemetry;
pub mod chat_turn_edge_profile;
pub mod cloud_compact_prompt;
pub mod cloud_summary;
pub mod sse_data_lines;

// Phase 18: contracts, session cache, trace collector
pub mod contracts;
pub mod turn_trace_collector;

// Phase 19: e2e hooks, llm dump, history apply, edge ledger
pub mod bridge_e2e_hooks;
pub mod edge_ledger;
pub mod llm_request_dump;

// Phase 20: decision explainer, cloud tool delivery, sse stream host
pub mod cloud_tool_delivery;
pub mod decision_explainer;
pub mod sse_stream_host;

// Phase 21: retrieval
pub mod retrieval;

// Phase 24: interaction types, chat_turn_heuristics, result_quality, turn_guard, stall preflight, turn flow, stop_hooks_yaml
// + agentic_turn_ingest, agentic_post_tool_policy, headless_tool_postprocess, headless_types
pub mod agentic_post_tool_policy;
pub mod agentic_stall_preflight;
pub mod agentic_turn_flow;
pub mod agentic_turn_ingest;
pub mod chat_turn_step_plan;
pub mod headless_tool_body_preview;
pub mod headless_tool_postprocess;
pub mod headless_types;
pub mod result_quality {
    pub use astra_turn_types::{ResultQuality, classify_result, quality_feedback};
}
pub mod agentic_prepare_payload;
pub mod routing_engine;
pub mod tool_categories;
pub mod tool_registry_plugin;
pub mod tool_registry_selection_edge_hints;
pub mod turn_guard;
pub mod ws_approval_gate;
pub mod ws_user_prompt_gate;
