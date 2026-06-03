//! Cloud turn implementation — typed context compression for Astra ↔ LLM API.
//!
//! This module implements **multi-layer progressive context compression**:
//!
//! | Position | Layer                     | Trigger (pressure) | Behaviour |
//! |----------|---------------------------|---------------------|-----------|
//! | 1        | DuplicateReadElimination  | Tier × 0.625       | Merge consecutive read_file results with same path |
//! | 2        | ToolResultTruncation      | Tier × 0.75        | Truncate old tool results to max length |
//! | 3        | TieredCompaction          | Tier × 0.9375      | Drop middle turns; insert boundary marker |
//! | 4        | ReactiveCompact           | 0.95                | Emergency: keep only last 4 messages |
//!
//! Triggers derive from `CompactionTier::pre_turn_trigger(max_window_tokens)`
//! — a window-adaptive baseline.
//!
//! ## Design
//!
//! - **Typed internal representation**: `Message` (from `astra_turn_core`)
//!   replaces `serde_json::Value` inside all layers. The engine converts
//!   `Vec<Value>` → `Vec<Message>` on entry and `Vec<Message>` → `Vec<Value>`
//!   on exit.
//! - **Pipeline engine** (`compaction_engine.rs`): orchestrates layers,
//!   adjusts the effective budget between layers, stops when satisfied.
//! - **Layers** (`layers/*.rs`): each implements `CompressionLayer`.

pub mod analytics;
pub mod compaction;
pub mod compaction_engine;
pub(crate) mod helpers;
pub mod layers;
pub mod memoria_compact;
pub mod memory_orchestrator;
pub mod session_end_governance;

/// Re-export the pipeline entry point for callers throughout the runtime.
pub use compaction_engine::CompactionEngine;
/// Re-export layers for tests and direct consumers.
pub use layers::{
    DuplicateReadElimination, ReactiveCompact, TieredCompaction, ToolResultTruncation,
};
