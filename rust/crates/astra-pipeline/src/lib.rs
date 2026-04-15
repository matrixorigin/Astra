//! Astra Agent Learning Pipeline — Core Types
//!
//! This crate provides the foundational types for the agent's learning subsystem:
//! - `TaskType` — 8-way task classification
//! - `DomainHint` — 7 domain categories
//! - `ToolFilter` — tool selection strategy
//! - `CalibrationAxis` — calibration targeting
//!
//! The full calibration implementation lives in `astra-runtime::pipeline::calibration`.

#![allow(clippy::too_many_arguments)]

pub mod routing;

pub use routing::{domain_hint_to_label, CalibrationAxis, DomainHint, TaskType, ToolFilter};
