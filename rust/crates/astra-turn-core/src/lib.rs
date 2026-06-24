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

pub mod action_compensation;
pub mod activity;
pub mod agent_live_event;

pub mod agentic;
pub mod alert_dispatcher;
pub mod approval;
pub mod boost_domain_hints;
pub mod cache;
pub mod cache_diagnostics;
pub mod cache_placement;
mod canonical_json;
pub mod capability;
pub mod chat_history_openai;
pub mod chat_turn_api_error;
pub mod chat_turn_edge_profile;
pub mod chat_turn_explain_wire;
pub mod chat_turn_heuristics;
pub mod chat_turn_payload;
pub mod chat_turn_sse_dispatch;
pub mod chat_turn_step_plan;
pub mod cloud;
pub mod compaction_types;
pub mod compression_types;
pub mod concurrency_safety;
pub mod confidence_contract;
pub mod context;
pub mod conversation_log;
pub mod edge_ledger;
pub mod edge_prompt_context;
pub mod emergent_context;
pub mod error_recovery;
pub mod evaluation;
pub mod execution_state;
pub mod explain;
pub mod explain_report_lines;
pub mod file_edit_journal;
pub mod followup_suggestion;
pub mod fork;
pub mod guardrails;
pub mod headless;
pub mod history;
pub mod hook_plans;
pub mod injection_tracking;
pub mod input_classifier;
pub mod interaction_types;
pub mod interruption;
pub mod introspect;
pub mod lru_map;
pub mod microcompact;
pub mod observer;
pub mod optimize_limits;
pub mod orchestration;
pub mod parallel_tool_exec;
pub mod permission;
pub mod pipeline;
pub mod prepare_turn_explain_text;
pub mod prompt_facing;
pub mod reasoning_capabilities;
pub mod recent_arg_hints;
pub mod recovery_state;
pub mod response_guard;
pub mod routing;
pub mod routing_engine;
pub mod routing_metrics;
pub mod runtime_scaffolding;
pub mod safety_middleware;
pub mod section_types;
pub mod session_latches;
pub mod shadow_diff;
pub mod spill_backend;
pub mod sse;
pub mod stall;
pub mod state;
pub mod stop_hooks;
pub mod streaming_tool_exec;
pub mod sync_utils;
pub use sync_utils::{
    rwlock_check_contains_or_default, rwlock_read_clone_or_default, rwlock_write_reset_on_poison,
};
pub mod tail_persist;
pub mod task;
pub mod task_context;
pub mod thinking_config;
pub mod token_accounting;
pub mod tool;
pub mod tool_allowlist;
pub mod trace_alert;
pub mod turn_event_sink;
pub mod turn_metrics;
pub mod working_memory;
pub mod xml_tool_call_fallback;

pub mod bridge;
pub mod contracts;
pub mod stream_events;
pub mod turn_guard;
pub mod ws_approval_gate;
pub mod ws_user_prompt_gate;

// Re-export result_quality (types from astra_turn_types)
pub mod result_quality {
    pub use astra_turn_types::{ResultQuality, classify_result, quality_feedback};
}

// Existing modules restored (files still on disk)
pub mod complete;
pub mod hydrate_reflect;
pub mod loop_circuit_breaker;
pub mod persist;
pub mod retrieval;
pub mod trace_event;
pub mod turn_trace_collector;
pub mod unified_timeline;
pub mod view;

// Re-exports: old flat module names → new directory paths
pub mod decision_explainer;
pub mod delegation_tree;
pub mod hallucination_tripwire;
pub mod liquid_step_signals;
pub mod liquid_tactical;
pub mod llm_request_dump;

pub use cloud::approval_policy as cloud_approval_policy;
pub use cloud::attachments as cloud_attachments;
pub use cloud::cache_diagnostics as cloud_cache_diagnostics;
pub use cloud::session_facts as cloud_session_facts;
pub use cloud::session_memory_extract as cloud_session_memory_extract;
pub use cloud::summary as cloud_summary;
pub use cloud::tool_delivery as cloud_tool_delivery;
pub use tool::args::hints as tool_argument_hints;
pub use tool::args::repair as tool_args_repair;
pub use tool::args::shape as tool_call_shape;
pub use tool::categories as tool_categories;
pub use tool::categories::surface as tool_surface;
pub use tool::categories::workaround as tool_workaround;
pub use tool::health as tool_health;
pub use tool::health::persistence as tool_health_persistence;
pub use tool::policy as tool_policy;
pub use tool::policy::hooks as tool_hooks;
pub use tool::policy::preview as tool_preview;
pub use tool::registry::chain as tool_registry_chain;
pub use tool::registry::meta as tool_registry_meta;
pub use tool::registry::plugin as tool_registry_plugin;
pub use tool::registry::report as tool_registry_report;
pub use tool::registry::state as tool_registry_state;
pub use tool::result::compression as tool_result_compression;
pub use tool::result::dedup as tool_result_dedup;
pub use tool::result::sanitize as tool_result_sanitize;
pub use tool::result::semantics as tool_result_semantics;
pub use tool::result::storage as tool_result_storage;
pub use tool::schema::prune as tool_schema_prune;
pub use tool::schema::surface_subset as tool_surface_subset;

// Re-exports: context_* → context::*
pub use context::assembly_trace as context_assembly_trace;
pub use context::binder as context_binder;
pub use context::budget as context_budget;
pub use context::feedback as context_feedback;
pub use context::optimizer as context_optimizer;
pub use context::pipeline as context_pipeline;
pub use context::planner as context_planner;
pub use context::pressure as context_pressure;
pub use context::serializer as context_serializer;
pub use context::sources as context_sources;

// Re-exports: headless_* → headless::*
pub use headless::assembly as headless_tool_assembly;
pub use headless::body_preview as headless_tool_body_preview;
pub use headless::journal as headless_tool_journal;
pub use headless::postprocess as headless_tool_postprocess;
pub use headless::status_display as headless_tool_status_display;
pub use headless::stderr_lines as headless_tool_stderr_lines;

// Re-exports: agentic_* → agentic::*
pub use agentic::post_tool_policy as agentic_post_tool_policy;
pub use agentic::prepare_payload as agentic_prepare_payload;
pub use agentic::recursion_guard as agentic_recursion_guard;
pub use agentic::stall_preflight as agentic_stall_preflight;
pub use agentic::turn_flow as agentic_turn_flow;
pub use agentic::turn_ingest as agentic_turn_ingest;
pub use agentic::turn_telemetry as agentic_turn_telemetry;
pub use agentic::verdict_audit as agentic_verdict_audit;

// Re-exports: approval_* → approval::*
pub use approval::base_digest as approval_base_digest;
pub use approval::batch_group as approval_batch_group;
pub use approval::fingerprint as approval_fingerprint;
pub use approval::request_key as approval_request_key;
pub use approval::sink as approval_sink;
pub use approval::ux_layer as approval_ux_layer;

// Re-exports: permission_* → permission::*
pub use permission::types as permission_types;

// Re-exports: orchestration_* → orchestration::*
pub use orchestration::builtin_agents as orchestration_builtin_agents;
pub use orchestration::context_cache as orchestration_context_cache;
pub use orchestration::fanout_group as orchestration_fanout_group;
pub use orchestration::progress as orchestration_progress;
pub use orchestration::spawn_tool as orchestration_spawn_tool;
pub use orchestration::team_config as orchestration_team_config;
pub use orchestration::types as orchestration_types;

// Re-exports: pipeline_* → pipeline::*
pub use pipeline::config as pipeline_config;
pub use pipeline::journal as pipeline_journal;
pub use pipeline::metrics as pipeline_metrics;
pub use pipeline::session as pipeline_session;
pub use pipeline::session_serde as pipeline_session_serde;
pub use pipeline::stats as pipeline_stats;

// Re-exports: fork_* → fork::*
pub use fork::cache_event as fork_cache_event;
pub use fork::capture as fork_capture;
pub use fork::prefix as fork_prefix;
pub use fork::prefix_store as fork_prefix_store;
pub use fork::reconstruct as fork_reconstruct;
pub use fork::resolve as fork_resolve;

// Re-exports: sse_* → sse::*
pub use sse::blocks as sse_blocks;
pub use sse::data_lines as sse_data_lines;
pub use sse::edge_stderr_lines as sse_edge_stderr_lines;
pub use sse::stream_host as sse_stream_host;

// Re-exports: bridge_* → bridge::*
pub use bridge::circuit_breaker as bridge_circuit_breaker;
pub use bridge::e2e_hooks as bridge_e2e_hooks;
pub use bridge::rate_limit_cooldown as bridge_rate_limit_cooldown;
pub use bridge::sse_events as bridge_sse_events;

// Re-exports: stop_hooks_* → stop_hooks::*
pub use stop_hooks::stop_hooks_yaml;
