//! Non-blocking approval queue.
//!
//! Holds [`PendingApproval`]s that the model has asked the user to resolve
//! (allow / deny / skip / always-allow / auto-run). The queue is wire-up
//! plumbing; the UI surface (inline tool cell badge + status-line counter)
//! reads from it and renders, then calls `respond_*` to resolve entries.

#![allow(dead_code)]

pub(crate) mod queue;

#[allow(unused_imports)]
pub(crate) use queue::{ApprovalQueue, ApprovalView, PendingApproval};

#[cfg(test)]
mod tests;
