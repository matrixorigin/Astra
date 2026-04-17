//! Astra Agent Learning Pipeline — Core Types
//!
//! This crate provides the foundational types for the agent's learning subsystem:
//! - `TaskType` — 8-way task classification
//! - `DomainHint` — 7 domain categories
//! - `ToolFilter` — tool selection strategy
//! - `CalibrationAxis` — calibration targeting

#![allow(clippy::too_many_arguments)]

pub mod calibration;
pub mod engine;
pub mod entity;
pub mod event;
pub mod feedback_extraction;
pub mod feedback_store;
pub mod routing;
pub mod state;
pub mod step_checkpoint;
pub mod step_protocol;
pub mod step_recorder;
pub mod tool_health_types;

pub use routing::{CalibrationAxis, DomainHint, TaskType, ToolFilter, domain_hint_to_label};
pub use tool_health_types::ToolHealthEntry;
