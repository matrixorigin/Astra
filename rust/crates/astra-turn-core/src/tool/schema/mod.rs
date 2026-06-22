//! Tool schema pruning and selection.
//!
//! The canonical `tool_schema_name` family lives in [`astra_core::tool_schema`]
//! so that every crate (astra-tools, astra-runtime-env, astra-turn-core)
//! admits OpenAI function-tool schemas by exactly the same fail-closed rule.
//! This module re-exports it for callers already rooted at `astra_turn_core`.

pub mod prune;
pub mod selection;

/// Re-export of the canonical [`astra_core::tool_schema`] helpers.
///
/// The implementation lives in `astra-core` so that every crate admits
/// OpenAI function-tool schemas by exactly the same fail-closed rule.
pub use astra_core::tool_schema::{
    retain_tool_schemas_by_names, tool_names_from_schemas, tool_schema_name,
};
