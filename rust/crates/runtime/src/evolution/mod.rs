//! Multi-axis self-evolution engine.
//!
//! Detects runtime signals (tool failures, user corrections, pattern drift, stalls),
//! generates evolution proposals across four axes (skill, pattern, calibration, entity),
//! and applies approved changes back into the system.

pub mod service;
pub use astra_evolution::store;
pub use astra_evolution::types;
pub use astra_evolution::promotion_gate;
