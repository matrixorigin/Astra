//! Adaptive learning and auto-tuning engine.
//!
//! Provides feedback signal processing, parameter tuning rules,
//! and evolution triggers — decoupled from runtime infrastructure.

pub mod auto_tuning;
pub mod drift_source;

pub use drift_source::DriftSource;
