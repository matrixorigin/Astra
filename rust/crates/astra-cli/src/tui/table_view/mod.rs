//! `/table` — re-render the most recent `mo_query` output as a
//! navigable ratatui table.
//!
//! Layering:
//! - [`parser`]: pure — splits mysql-client ASCII output (`+---+` /
//!   `| col |`) into a structured [`MysqlTable`].
//! - `nav`: pure — tracks selected row + horizontal scroll.
//! - `view`: ratatui widget.
//! - `bottom_pane::table_view`: BottomPaneView wrapper.

#![allow(dead_code)]

pub(crate) mod nav;
pub(crate) mod parser;
pub(crate) mod view;

#[allow(unused_imports)]
pub(crate) use nav::TableNav;
#[allow(unused_imports)]
pub(crate) use parser::{MysqlTable, parse};

#[cfg(test)]
mod tests;
