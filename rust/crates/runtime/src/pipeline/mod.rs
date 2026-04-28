//! Cognitive Agent Runtime Pipeline
//!
//! A state-machine–based execution engine that drives the agent loop through
//! typed phases: Perceive → Plan → Execute → Evaluate → Reflect.
//!
//! # Design principles
//!
//! 1. **TurnState is the single source of truth** — no implicit local variables.
//! 2. **Every state mutation emits a TurnEvent** — enables replay, audit, learning.
//! 3. **Typed phase transitions** — compiler enforces valid state machine edges.
//! 4. **Goal-gradient budget** — budget expands/contracts based on progress rate.
//! 5. **Structured reflection** — not just "nudge", but causal analysis + strategy change.

pub mod evaluation;
pub mod learning;
pub mod learning_quality_gate;
pub mod persistence;


// Re-export from astra-pipeline for public API compatibility
pub use astra_pipeline::{step_protocol, step_recorder};
pub use astra_pipeline::{calibration, engine, entity, event, feedback_extraction, feedback_store, pattern, state, step_checkpoint, step_restore};
pub use astra_turn_core::routing_engine as routing;
