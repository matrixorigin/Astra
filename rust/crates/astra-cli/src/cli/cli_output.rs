//! Structured CLI output helpers.
//!
//! Provides consistent output patterns for success, warning, error, and progress messages.
//! All output goes to stderr (since stdout is reserved for piping/scripting).
//!
//! ## Usage
//!
//! ```ignore
//! cli_ok!("Session saved");
//! cli_ok!("Saved {} skills", count);
//! cli_warn!("No files found");
//! cli_err!("Failed to connect: {}", err);
//! cli_info!("Loading configuration...");
//! cli_section!("Settings");
//! cli_list!([("Name", "John"), ("Age", "30")]);
//! ```

use crate::theme;
use crossterm::style::Stylize;

/// Print a success message with ✓ icon.
///
/// Example: `  ✓ Session saved`
#[macro_export]
macro_rules! cli_ok {
    ($($arg:tt)*) => {{
        use $crate::theme::icon_ok;
        eprintln!("  {} {}", icon_ok(), format!($($arg)*));
    }};
}

/// Print a warning message with ⚠ icon (yellow).
///
/// Example: `  ⚠ No files found`
#[macro_export]
macro_rules! cli_warn {
    ($($arg:tt)*) => {{
        use $crate::theme::icon_warn;
        use crossterm::style::Stylize;
        eprintln!("  {} {}", icon_warn(), format!($($arg)*).yellow());
    }};
}

/// Print an error message with ✗ icon (red).
///
/// Example: `  ✗ Failed to connect: timeout`
#[macro_export]
macro_rules! cli_err {
    ($($arg:tt)*) => {{
        use $crate::theme::icon_err;
        use crossterm::style::Stylize;
        eprintln!("  {} {}", icon_err(), format!($($arg)*).red());
    }};
}

/// Print an info message with ℹ icon.
///
/// Example: `  ℹ Loading configuration...`
#[macro_export]
macro_rules! cli_info {
    ($($arg:tt)*) => {{
        use $crate::theme::icon_info;
        eprintln!("  {} {}", icon_info(), format!($($arg)*));
    }};
}

/// Print a section header (cyan, bold).
///
/// Example: `  ═══ Settings ═══`
#[macro_export]
macro_rules! cli_section {
    ($title:expr) => {{
        use crossterm::style::Stylize;
        use $crate::theme::section;
        eprintln!();
        eprintln!("  {}", section($title).bold());
    }};
}

/// Print a dimmed secondary line (indented).
///
/// Example: `     This is additional context`
#[macro_export]
macro_rules! cli_dim {
    ($($arg:tt)*) => {{
        use crossterm::style::Stylize;
        eprintln!("     {}", format!($($arg)*).dim());
    }};
}

/// Print a labeled value on a single line.
///
/// Example: `  Profile: default`
pub fn cli_kv(label: &str, value: &str) {
    eprintln!("  {}: {}", label.dim(), value);
}

/// Print a table with headers and rows.
///
/// Headers are bold, values are styled based on theme.
pub fn cli_table(headers: &[&str], rows: &[Vec<String>]) {
    if headers.is_empty() || rows.is_empty() {
        return;
    }

    // Calculate column widths
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }

    // Print header
    let header_line: String = headers
        .iter()
        .zip(&widths)
        .map(|(h, w)| format!("{:width$}", h, width = w))
        .collect::<Vec<_>>()
        .join("  ");
    eprintln!("  {}", header_line.bold());

    // Separator
    let sep: String = widths
        .iter()
        .map(|w| "─".repeat(*w))
        .collect::<Vec<_>>()
        .join("──");
    eprintln!("  {}", sep.dim());

    // Rows
    for row in rows {
        let row_line: String = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{:width$}", cell, width = w))
            .collect::<Vec<_>>()
            .join("  ");
        eprintln!("  {}", row_line);
    }
}

/// Print a simple key-value list (two columns).
///
/// Example:
/// ```text
///   Name     John
///   Age      30
/// ```
pub fn cli_kvlist(items: &[(&str, &str)]) {
    if items.is_empty() {
        return;
    }
    let max_key_len = items.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (key, value) in items {
        eprintln!("  {:width$}  {}", key.dim(), value, width = max_key_len);
    }
}

/// Print an indented bullet item.
///
/// Example: `  • skill-name`
pub fn cli_bullet(text: &str) {
    eprintln!("  {} {}", "•".dim(), text);
}

/// Print a numbered item (1-indexed).
///
/// Example: `  1. First item`
pub fn cli_numbered(index: usize, text: &str) {
    eprintln!("  {}. {}", format!("{:>2}", index).dim(), text);
}

/// Print a progress indicator line that can be updated.
///
/// Returns the text for use with `cli_progress_done`.
pub fn cli_progress(message: &str) {
    use std::io::Write;
    eprint!("  {} {}...", theme::icon_info(), message.dim());
    let _ = std::io::stderr().flush();
}

/// Complete a progress line with success.
pub fn cli_progress_done() {
    eprintln!(" {}", theme::icon_ok());
}

/// Complete a progress line with failure.
pub fn cli_progress_fail() {
    eprintln!(" {}", theme::icon_err());
}

/// Print an empty line for visual separation.
pub fn cli_blank() {
    eprintln!();
}

// Re-export macros at module level
pub use crate::{cli_dim, cli_err, cli_info, cli_ok, cli_section, cli_warn};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_kv_prints_formatted() {
        // Just verify no panic
        cli_kv("Label", "Value");
    }

    #[test]
    fn cli_table_basic() {
        cli_table(
            &["Name", "Value"],
            &[
                vec!["foo".to_string(), "bar".to_string()],
                vec!["baz".to_string(), "qux".to_string()],
            ],
        );
    }

    #[test]
    fn cli_table_empty_no_panic() {
        cli_table(&[], &[]);
        cli_table(&["A"], &[]);
    }

    #[test]
    fn cli_kvlist_formats_columns() {
        cli_kvlist(&[("short", "value"), ("longerkey", "val2")]);
    }

    #[test]
    fn cli_bullet_numbered() {
        cli_bullet("item one");
        cli_numbered(1, "first");
        cli_numbered(10, "tenth");
    }
}
