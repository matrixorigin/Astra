//! `/timeline` panel — read-only session turn timeline.
//!
//! Uses the MatrixOne-backed session journal to reconstruct each turn
//! of the current session and surface cumulative metrics (tokens,
//! tools, duration) per turn. It is an observational surface and does not own
//! session restore or lifecycle state.

pub(crate) mod model;
pub(crate) mod view;

pub(crate) use model::{JournalTurnSource, Timeline};

#[cfg(test)]
mod tests;
