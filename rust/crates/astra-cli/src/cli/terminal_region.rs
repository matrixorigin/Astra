//! Diff-based terminal region renderer.
//!
//! Maintains a list of lines currently displayed on the terminal.
//! On update, diffs old vs new lines and only rewrites changed lines.
//! This eliminates flicker and enables Ink-style atomic updates.
//!
//! **Note on terminal wrapping:** When a line exceeds terminal width,
//! the terminal wraps it to multiple physical rows. This module tracks
//! physical rows to ensure correct cursor positioning during updates.

use std::io::{self, Write};

use crossterm::{cursor, execute, terminal};

/// A managed region of terminal output.
///
/// Tracks what's currently on screen and performs minimal updates
/// when content changes — like a simplified React reconciler for
/// terminal lines.
pub(super) struct TerminalRegion {
    /// Lines currently displayed, each with its physical row count.
    lines: Vec<LineEntry>,
    /// Cached terminal width for wrap calculations.
    term_width: u16,
}

/// A line entry with its content and calculated physical row count.
struct LineEntry {
    content: String,
    /// Number of physical terminal rows this line occupies.
    physical_rows: usize,
}

impl TerminalRegion {
    pub(super) fn new() -> Self {
        let term_width = terminal::size().map(|(w, _)| w).unwrap_or(80);
        Self {
            lines: Vec::new(),
            term_width,
        }
    }

    /// Number of logical lines currently on screen.
    #[allow(dead_code)]
    pub(super) fn height(&self) -> usize {
        self.lines.len()
    }

    /// Total physical rows occupied by all lines.
    fn total_physical_rows(&self) -> usize {
        self.lines.iter().map(|e| e.physical_rows).sum()
    }

    /// Calculate physical rows for a line based on current terminal width.
    fn calc_physical_rows(&self, line: &str) -> usize {
        if self.term_width == 0 {
            return 1;
        }
        let visible_width = visible_char_width(line);
        if visible_width == 0 {
            return 1; // empty line still takes 1 row
        }
        // Ceiling division: how many rows needed
        visible_width.div_ceil(self.term_width as usize)
    }

    /// Refresh terminal width (call on resize or before update).
    fn refresh_term_width(&mut self) {
        if let Ok((w, _)) = terminal::size() {
            self.term_width = w;
        }
    }

    /// Replace the entire region with new content.
    ///
    /// Diffs against current lines and only redraws what changed.
    /// If new content is shorter, clears leftover lines.
    /// If new content is longer, appends new lines.
    pub(super) fn update(&mut self, new_lines: Vec<String>) {
        // Refresh terminal width in case of resize
        self.refresh_term_width();

        if self.lines.is_empty() && new_lines.is_empty() {
            return;
        }

        let old_len = self.lines.len();
        let new_len = new_lines.len();

        if old_len == 0 {
            // First render — just print everything.
            let mut entries = Vec::with_capacity(new_len);
            for line in new_lines {
                println!("{line}");
                let rows = self.calc_physical_rows(&line);
                entries.push(LineEntry {
                    content: line,
                    physical_rows: rows,
                });
            }
            let _ = io::stdout().flush();
            self.lines = entries;
            return;
        }

        // Find first differing line.
        let first_diff = self
            .lines
            .iter()
            .zip(new_lines.iter())
            .position(|(old, new)| old.content != *new)
            .unwrap_or(old_len.min(new_len));

        // Nothing changed and same length — no-op.
        if first_diff == old_len && first_diff == new_len {
            return;
        }

        // Calculate physical rows to move up (from end to first_diff)
        let rows_up: usize = self.lines[first_diff..]
            .iter()
            .map(|e| e.physical_rows)
            .sum();
        if rows_up > 0 {
            execute!(io::stdout(), cursor::MoveUp(rows_up as u16)).ok();
        }
        execute!(io::stdout(), cursor::MoveToColumn(0)).ok();

        // Clear from cursor down (removes all old content from first_diff onwards)
        execute!(
            io::stdout(),
            terminal::Clear(terminal::ClearType::FromCursorDown)
        )
        .ok();

        // Truncate old lines to first_diff
        self.lines.truncate(first_diff);

        // Print new lines from first_diff onwards
        for line in &new_lines[first_diff..] {
            println!("{line}");
            let rows = self.calc_physical_rows(line);
            self.lines.push(LineEntry {
                content: line.clone(),
                physical_rows: rows,
            });
        }

        let _ = io::stdout().flush();
    }

    /// Append lines without diffing (for content that only grows).
    #[allow(dead_code)]
    pub(super) fn append(&mut self, new_lines: &[String]) {
        for line in new_lines {
            println!("{line}");
            let rows = self.calc_physical_rows(line);
            self.lines.push(LineEntry {
                content: line.clone(),
                physical_rows: rows,
            });
        }
        let _ = io::stdout().flush();
    }

    /// Append a single line.
    #[allow(dead_code)]
    pub(super) fn append_line(&mut self, line: String) {
        println!("{line}");
        let _ = io::stdout().flush();
        let rows = self.calc_physical_rows(&line);
        self.lines.push(LineEntry {
            content: line,
            physical_rows: rows,
        });
    }

    /// Clear the entire region from the terminal.
    pub(super) fn clear(&mut self) {
        // Refresh width in case terminal was resized
        self.refresh_term_width();
        let rows = self.total_physical_rows();
        if rows > 0 {
            execute!(
                io::stdout(),
                cursor::MoveUp(rows as u16),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            )
            .ok();
            let _ = io::stdout().flush();
            self.lines.clear();
        }
    }

    /// Remove the last `n` logical lines from the region.
    #[allow(dead_code)]
    pub(super) fn pop_lines(&mut self, n: usize) {
        self.refresh_term_width();
        let n = n.min(self.lines.len());
        if n > 0 {
            let rows_to_remove: usize = self.lines[self.lines.len() - n..]
                .iter()
                .map(|e| e.physical_rows)
                .sum();
            execute!(
                io::stdout(),
                cursor::MoveUp(rows_to_remove as u16),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            )
            .ok();
            let _ = io::stdout().flush();
            self.lines.truncate(self.lines.len() - n);
        }
    }
}

/// Calculate visible character width of a string (stripping ANSI codes).
///
/// Uses a heuristic:
/// - ASCII printable chars (0x20-0x7E) = 1 width
/// - Control chars (0x00-0x1F, 0x7F) = 0 width
/// - Zero-width Unicode (combining marks, etc.) = 0 width
/// - CJK, emoji, etc. = 2 width
pub(super) fn visible_char_width(s: &str) -> usize {
    let stripped = strip_ansi_codes(s);
    stripped.chars().map(char_display_width).sum()
}

/// Estimate display width of a single character.
pub(super) fn char_display_width(c: char) -> usize {
    match c {
        // ASCII control characters (including tab, backspace)
        '\x00'..='\x1f' | '\x7f' => 0,
        // ASCII printable
        '\x20'..='\x7e' => 1,
        // Common zero-width Unicode ranges
        '\u{200B}'..='\u{200F}' => 0, // Zero-width space, joiners, marks
        '\u{2060}'..='\u{2064}' => 0, // Word joiner, invisible operators
        '\u{FEFF}' => 0,              // BOM / zero-width no-break space
        '\u{FE00}'..='\u{FE0F}' => 0, // Variation selectors
        // Combining diacritical marks
        '\u{0300}'..='\u{036F}' => 0,
        '\u{1AB0}'..='\u{1AFF}' => 0,
        '\u{1DC0}'..='\u{1DFF}' => 0,
        '\u{20D0}'..='\u{20FF}' => 0,
        '\u{FE20}'..='\u{FE2F}' => 0,
        // Everything else (CJK, emoji, etc.) = 2
        _ => 2,
    }
}

/// Strip ANSI escape sequences from a string.
///
/// Handles:
/// - SGR sequences: ESC [ ... m (colors, styles)
/// - CSI sequences: ESC [ ... @ through ~ (cursor movement, clear, etc.)
/// - OSC sequences: ESC ] ... BEL or ST (window title, etc.)
fn strip_ansi_codes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek() {
                // CSI sequence: ESC [ ... (final byte in @-~)
                Some('[') => {
                    let _ = chars.next(); // consume '['
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                    continue;
                }
                // OSC sequence: ESC ] ... (terminated by BEL or ESC \)
                Some(']') => {
                    let _ = chars.next(); // consume ']'
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            // BEL
                            break;
                        }
                        if c == '\u{1b}' && matches!(chars.peek(), Some('\\')) {
                            let _ = chars.next(); // consume '\'
                            break;
                        }
                    }
                    continue;
                }
                // Other single-char escapes (ESC c, ESC D, etc.)
                Some(c) if ('@'..='_').contains(c) => {
                    let _ = chars.next();
                    continue;
                }
                _ => {}
            }
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_region_is_empty() {
        let r = TerminalRegion::new();
        assert_eq!(r.height(), 0);
    }

    #[test]
    fn strip_ansi_codes_removes_colors() {
        let colored = "\x1b[31mred\x1b[0m text";
        assert_eq!(strip_ansi_codes(colored), "red text");
    }

    #[test]
    fn visible_width_handles_cjk() {
        assert_eq!(visible_char_width("hello"), 5);
        assert_eq!(visible_char_width("你好"), 4); // 2 chars * 2 width
        assert_eq!(visible_char_width("hi你好"), 6); // 2 + 4
    }

    #[test]
    fn visible_width_ignores_ansi() {
        let colored = "\x1b[32mgreen\x1b[0m";
        assert_eq!(visible_char_width(colored), 5);
    }

    #[test]
    fn calc_physical_rows_handles_wrap() {
        let mut r = TerminalRegion::new();
        r.term_width = 10;
        assert_eq!(r.calc_physical_rows("hello"), 1);
        assert_eq!(r.calc_physical_rows("hello world!!"), 2); // 13 chars, wraps
        assert_eq!(r.calc_physical_rows("12345678901234567890"), 2); // exactly 20
        assert_eq!(r.calc_physical_rows("123456789012345678901"), 3); // 21 chars
    }

    #[test]
    fn calc_physical_rows_with_cjk() {
        let mut r = TerminalRegion::new();
        r.term_width = 10;
        // "你好" = 4 visible width, fits in 10
        assert_eq!(r.calc_physical_rows("你好"), 1);
        // "你好你好你好" = 12 visible width, wraps to 2 rows
        assert_eq!(r.calc_physical_rows("你好你好你好"), 2);
    }

    #[test]
    fn diff_finds_first_change() {
        let old: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let new: Vec<String> = vec!["a".into(), "X".into(), "c".into()];
        let first_diff = old
            .iter()
            .zip(new.iter())
            .position(|(o, n)| o != n)
            .unwrap_or(old.len().min(new.len()));
        assert_eq!(first_diff, 1);
    }

    #[test]
    fn diff_detects_append() {
        let old: Vec<String> = vec!["a".into(), "b".into()];
        let new: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let first_diff = old
            .iter()
            .zip(new.iter())
            .position(|(o, n)| o != n)
            .unwrap_or(old.len().min(new.len()));
        assert_eq!(first_diff, 2);
    }

    #[test]
    fn diff_detects_shrink() {
        let old: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let new: Vec<String> = vec!["a".into(), "b".into()];
        let first_diff = old
            .iter()
            .zip(new.iter())
            .position(|(o, n)| o != n)
            .unwrap_or(old.len().min(new.len()));
        assert_eq!(first_diff, 2);
    }

    #[test]
    fn char_width_zero_width_chars() {
        // Zero-width space
        assert_eq!(char_display_width('\u{200B}'), 0);
        // Combining acute accent
        assert_eq!(char_display_width('\u{0301}'), 0);
        // Control char (backspace)
        assert_eq!(char_display_width('\x08'), 0);
        // Tab
        assert_eq!(char_display_width('\t'), 0);
    }

    #[test]
    fn strip_ansi_handles_osc_sequences() {
        // OSC sequence with BEL terminator
        let osc = "\x1b]0;Window Title\x07text";
        assert_eq!(strip_ansi_codes(osc), "text");
        // OSC sequence with ST terminator
        let osc_st = "\x1b]0;Title\x1b\\text";
        assert_eq!(strip_ansi_codes(osc_st), "text");
    }

    #[test]
    fn strip_ansi_handles_csi_cursor() {
        // Cursor movement sequences
        let cursor_up = "\x1b[5Atext";
        assert_eq!(strip_ansi_codes(cursor_up), "text");
        let clear_line = "\x1b[2Ktext";
        assert_eq!(strip_ansi_codes(clear_line), "text");
    }
}
