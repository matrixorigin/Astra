//! TUI status line — a composable line anchored to the bottom of the
//! chat pane. The rendered string is derived purely from a
//! [`StatusContext`] value, which means every visual variation has a
//! snapshot and nothing depends on the event loop state.

pub(crate) mod context_bar;
pub(crate) mod line;

pub(crate) use context_bar::ContextBar;
pub(crate) use line::{BackgroundTaskCounts, StatusContext, StatusLine};

#[cfg(test)]
mod snapshot_tests;
#[cfg(test)]
mod tests;
