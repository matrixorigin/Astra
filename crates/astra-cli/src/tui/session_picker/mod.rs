//! Interactive session-resume picker.
//!
//! Layered like the slash / mention menus:
//! - [`discovery`] — pure `SessionDiscovery` (fuzzy filtering, selection)
//!   plus a `SessionSource` trait so tests can inject fixtures.
//! - [`view`] (next task) — ratatui widget rendering the two-pane UI.
//!
//! The picker lives in `BottomPaneView`, not `view_stack`, because
//! existing slash/skill popups already use BottomPaneView and we want
//! uniform Esc / completion semantics.

#![allow(dead_code)]

pub(crate) mod discovery;
pub(crate) mod view;

pub(crate) use discovery::{FsSessionSource, SessionDiscovery};

#[cfg(test)]
mod tests;
