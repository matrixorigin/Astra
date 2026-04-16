//! Core turn types, contracts, and pure helpers extracted from the runtime crate.
//!
//! This crate contains modules that have no dependency on the runtime's
//! infrastructure (AppState, database connections, Axum, etc.) and can be
//! tested and compiled independently.

pub mod agentic_verdict_audit;
pub mod cache;
pub mod chat_turn_api_error;
pub mod chat_turn_explain_wire;
pub mod confidence_contract;
pub mod edge_executor_id;
pub mod error_recovery;
pub mod execution_state;
pub mod explain;
pub mod file_edit_journal;
pub mod firewall;
pub mod followup_suggestion;
pub mod observer;
pub mod routing;
pub mod skill_instructions_merge;
pub mod snapshot;
pub mod state;
pub mod stop_hooks;
pub mod task;
pub mod tool_args_repair;
pub mod tool_call_shape;
pub mod tool_result_semantics;
pub mod tool_selection;
pub mod unconsumed;
