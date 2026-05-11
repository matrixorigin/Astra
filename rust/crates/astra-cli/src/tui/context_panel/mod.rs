//! `/context` panel — visualize token budget breakdown.
//!
//! Reads from the most recent [`ContextAssemblyTrace`] captured by
//! the observability session and renders a Claude-Code-style grid
//! plus a category legend and nested sub-sections so the user can
//! see at a glance where their context window is going.

#![allow(dead_code)]

pub(crate) mod model;
pub(crate) mod view;

#[allow(unused_imports)]
pub(crate) use model::{
    Category, CategoryKind, ContextBreakdown, ContextSnapshot, HistorySummary, MemoryItem,
    PressureBand, Section, SectionItem, SkillItem, ToolItem, TurnDetail,
};

#[cfg(test)]
mod tests;
