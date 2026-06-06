//! `/timeline` panel — read-only session turn timeline.
//!
//! Uses the MatrixOne-backed session journal to reconstruct each turn
//! of the current session and surface cumulative metrics (tokens,
//! tools, duration) per turn. This is the foundational read side of
//! what will eventually become a time-travel view (restore to the
//! state at turn N). Phase 3.4 ships the read surface first.

#![allow(dead_code)]

pub(crate) mod model;
pub(crate) mod view;

pub(crate) use model::{JournalTurnSource, Timeline};

#[cfg(test)]
mod tests;
