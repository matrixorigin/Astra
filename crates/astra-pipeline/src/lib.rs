//! Astra Agent Learning Pipeline — Core Types
//!
//! This crate provides the foundational types for the agent's learning subsystem:
//! - `TaskType` — 8-way task classification
//! - `DomainHint` — 7 domain categories
//! - `CalibrationAxis` — calibration targeting

#![allow(clippy::too_many_arguments)]

pub mod crash_recovery;
pub mod event;
pub mod feedback_extraction;
pub mod feedback_store;
pub mod output_stream;
pub mod reflection_feedback;
pub mod routing;
pub mod scheduling;
pub mod skill_checkpoint;
pub mod step_checkpoint;
pub mod step_protocol;
pub mod step_recorder;
pub mod step_restore;

pub use routing::{CalibrationAxis, DomainHint, TaskType, domain_hint_to_label};
pub mod tool_health_types;
pub mod trace_query;
pub mod trace_retention;
pub mod trace_usage;
pub use tool_health_types::{
    TOOL_OUTCOME_RING_CAPACITY, ToolHealthEntry, ToolOutcome, ToolOutcomeCacheEntry,
};
