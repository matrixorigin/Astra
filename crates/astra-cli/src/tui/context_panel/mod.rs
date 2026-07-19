//! `/context` panel — visualize token budget breakdown.
//!
//! Reads from the most recent [`ContextAssemblyTrace`] captured by
//! the observability session and renders a glyph grid plus a
//! category legend and nested sub-sections so the user can see at
//! a glance where their context window is going.

pub(crate) mod model;
pub(crate) mod view;

pub(crate) use model::{ContextBreakdown, ContextSnapshot, Section};

#[cfg(test)]
mod tests;
