//! Modal view stack for the TUI.
//!
//! A stack of [`View`] trait objects with:
//! - **Top-down event routing**: the topmost view sees the event first;
//!   if it returns [`EventResult::Unhandled`], the next one down is tried.
//! - **Bottom-up rendering**: base view draws first, overlays composite on top.
//! - **Lifecycle hooks**: `on_enter` when pushed, `on_exit` when popped.
//!
//! This is the foundation for dialog/overlay management
//! (settings, doctor, trust prompts, approval overlays, etc).

#![allow(dead_code)]

pub(crate) mod stack;

#[cfg(test)]
mod tests;
