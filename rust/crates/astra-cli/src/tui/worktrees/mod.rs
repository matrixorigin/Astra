//! `/worktrees` — list git worktrees for the current repo alongside
//! the number of astra sessions that ran in each.
//!
//! Provides a per-worktree mental model: when you have main +
//! feature-A + feature-B worktrees each running their own astra
//! session, this panel makes that instantly legible.

#![allow(dead_code)]

pub(crate) mod model;
pub(crate) mod view;

#[allow(unused_imports)]
pub(crate) use model::{WorktreeEntry, WorktreeList, parse};

#[cfg(test)]
mod tests;
