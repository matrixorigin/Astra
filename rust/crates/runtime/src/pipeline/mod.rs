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

pub mod calibration;
pub mod defaults;
pub mod engine;
pub mod entity;
pub mod evaluation;
pub mod event;
pub mod learning;
pub mod mo_persistence;
pub mod pattern;
pub mod persistence;
pub mod routing;
pub mod scheduling;
pub mod stages;
pub mod state;
pub mod step_checkpoint;
pub mod step_protocol;
pub mod step_recorder;
pub mod step_restore;

pub use engine::*;
pub use event::*;
pub use mo_persistence::*;
pub use scheduling::*;
pub use state::*;
pub use step_checkpoint::*;
pub use step_protocol::*;
pub use step_recorder::*;
pub use step_restore::*;
