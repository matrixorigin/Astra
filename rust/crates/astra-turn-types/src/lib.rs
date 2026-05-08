//! Core turn types for astra runtime.
//!
//! This crate provides foundational types used during turn execution,
//! extracted from the monolithic runtime crate for better modularity.

pub mod continuity;
mod implicit_feedback;
mod memory_ranking;
mod memory_structure;
mod memory_writability;
mod result_quality;
mod runtime_scaffolding;
pub mod session_facts;
mod tool_idempotency;

pub use implicit_feedback::{
    ImplicitSignal, StructuredFeedback, detect_implicit_feedback_signal,
    implicit_feedback_context_injection, implicit_feedback_rating,
};
pub use memory_ranking::{
    PERSISTENT_TYPES, RankableMemory, SESSION_SCOPED_TYPE, composite_score,
    is_persistent_type, partition_by_scope, sort_memories, tier_weight,
};
pub use memory_structure::{
    PERSISTENT_MEMORY_TYPES, PersistentStoreRejection, is_persistent_memory_type,
    should_store_persistent_memory, validate_persistent_memory_content,
};
pub use memory_writability::should_store_in_memory;
pub use result_quality::{ResultQuality, classify_result, quality_feedback};
pub use runtime_scaffolding::{SCAFFOLDING_BODY_PREFIXES, is_runtime_scaffolding_message};
pub use tool_idempotency::{ToolIdempotency, classify_tool_idempotency};
