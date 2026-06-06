//! Inline `@`-mention menu — file completion from the composer.
//!
//! The menu opens when the user types `@` in a position that reasonably
//! precedes a path reference:
//! - at the very start of the buffer, or
//! - right after a whitespace character.
//!
//! Once open, the text from that `@` up to (a) the cursor or (b) the next
//! whitespace is used as a partial path. If it contains a `/` the menu
//! scans the named subdirectory (via a [`FileProvider`]); otherwise it
//! lists cwd-level entries.
//!
//! This module is pure logic; I/O lives behind [`FileProvider`].

#![allow(dead_code)]

pub(crate) mod menu;
pub(crate) mod popup;
pub(crate) mod provider;

pub(crate) use menu::{MentionMenu, extract_mention_at};
pub(crate) use provider::{FileEntry, FileProvider};

#[cfg(test)]
mod tests;
