//! Inline slash-command menu (pure logic).
//!
//! Holds the list of commands, the current filter, fuzzy match scoring, and
//! selection state. No rendering, no I/O — a reducer-friendly value type.
//!
//! The `is_open_for` rule is: the menu should be visible iff the composer
//! buffer (first line) starts with `/`. Opening/closing is a decision
//! for the caller based on that predicate; the menu itself is constructed
//! when opened and dropped when closed.

pub(crate) mod menu;
pub(crate) mod popup;

pub(crate) use menu::{SlashItem, SlashMenu, is_open_for, score_token};

#[cfg(test)]
mod tests;
