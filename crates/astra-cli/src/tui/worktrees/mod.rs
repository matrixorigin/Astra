//! `/worktrees` — list git worktrees for the current repo alongside
//! the number of astra sessions that ran in each.
//!
//! Provides a per-worktree mental model: when you have main +
//! feature-A + feature-B worktrees each running their own astra
//! session, this panel makes that instantly legible.

pub(crate) mod model;
pub(crate) mod view;

pub(crate) use model::{WorktreeList, parse};

#[cfg(test)]
mod tests;
