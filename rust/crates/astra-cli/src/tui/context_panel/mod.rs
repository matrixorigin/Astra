//! `/context` panel — visualize token budget breakdown.
//!
//! Reads from the cumulative [`TokenBudgetTrace`] captured on each turn
//! and renders a stacked category bar plus per-category table so the
//! user can see at a glance where their context window is going.

#![allow(dead_code)]

pub(crate) mod model;
pub(crate) mod view;

#[allow(unused_imports)]
pub(crate) use model::{Category, CategoryKind, ContextBreakdown, PressureBand};

#[cfg(test)]
mod tests;
