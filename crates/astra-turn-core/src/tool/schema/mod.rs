//! Tool schema pruning and surface helpers.
//!
//! The canonical `tool_schema_name` family lives in [`astra_core::tool_schema`]
//! so that every crate (astra-tools, astra-runtime-env, astra-turn-core)
//! admits OpenAI function-tool schemas by exactly the same rule: explicit
//! non-function types fail closed, while `{function:{name:...}}` shorthand is
//! accepted when `type` is omitted.
//! This module re-exports it for callers already rooted at `astra_turn_core`.

pub mod prune;
pub mod surface_subset;

/// Re-export of the canonical [`astra_core::tool_schema`] helpers.
///
/// The implementation lives in `astra-core` so that every crate admits
/// OpenAI function-tool schemas by exactly the same admission rule.
pub use astra_core::tool_schema::{
    prompt_schema_conflicting_tool_names, retain_tool_schemas_by_names, sort_tool_schemas_by_name,
    tool_names_from_schemas, tool_schema_name,
};
