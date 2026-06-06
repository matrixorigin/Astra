//! Non-blocking approval queue.
//!
//! Holds [`PendingApproval`]s that the model has asked the user to resolve
//! (allow / deny / skip / always-allow / auto-run). The queue is wire-up
//! plumbing; the UI surface (inline tool cell badge + status-line counter)
//! reads from it and renders, then calls `respond_*` to resolve entries.

#![allow(dead_code)]

pub(crate) mod button_row;
pub(crate) mod queue;

pub(crate) use button_row::{Button, ButtonAction, ButtonRow};
pub(crate) use queue::{ApprovalQueue, ApprovalView};

#[cfg(test)]
mod button_row_tests;
#[cfg(test)]
mod tests;
