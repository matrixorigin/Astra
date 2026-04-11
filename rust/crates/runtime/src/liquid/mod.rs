//! Liquid engine — step-level tactical adaptation within a single turn.
//!
//! The liquid module adds within-turn adaptation on top of the cross-turn
//! adaptive engine. It observes each tool call outcome in real time and can
//! make bounded adjustments (e.g., increasing verification strictness after
//! errors, adjusting tool selection hints after failures).

pub mod step_signals;
pub mod tactical;
