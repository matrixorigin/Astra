//! Multi-axis self-evolution engine.
//!
//! Detects runtime signals (tool failures, user corrections, pattern drift, stalls),
//! generates evolution proposals across four axes (skill, pattern, calibration, entity),
//! and applies approved changes back into the system.

pub mod evolver;
pub mod service;
pub mod signal_collector;
pub mod store;
pub mod types;
