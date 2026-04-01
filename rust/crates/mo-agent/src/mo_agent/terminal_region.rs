//! Diff-based terminal region renderer.
//!
//! Maintains a list of lines currently displayed on the terminal.
//! On update, diffs old vs new lines and only rewrites changed lines.
//! This eliminates flicker and enables Ink-style atomic updates.

use std::io::{self, Write};

use crossterm::{cursor, execute, terminal};

/// A managed region of terminal output.
///
/// Tracks what's currently on screen and performs minimal updates
/// when content changes — like a simplified React reconciler for
/// terminal lines.
pub(super) struct TerminalRegion {
    /// Lines currently displayed on the terminal.
    lines: Vec<String>,
}

impl TerminalRegion {
    pub(super) fn new() -> Self {
        Self { lines: Vec::new() }
    }

    /// Number of lines currently on screen.
    #[allow(dead_code)]
    pub(super) fn height(&self) -> usize {
        self.lines.len()
    }

    /// Replace the entire region with new content.
    ///
    /// Diffs against current lines and only redraws what changed.
    /// If new content is shorter, clears leftover lines.
    /// If new content is longer, appends new lines.
    pub(super) fn update(&mut self, new_lines: Vec<String>) {
        if self.lines.is_empty() && new_lines.is_empty() {
            return;
        }

        let old_len = self.lines.len();
        let new_len = new_lines.len();

        if old_len == 0 {
            // First render — just print everything.
            for line in &new_lines {
                println!("{line}");
            }
            let _ = io::stdout().flush();
            self.lines = new_lines;
            return;
        }

        // Find first differing line.
        let first_diff = self
            .lines
            .iter()
            .zip(new_lines.iter())
            .position(|(old, new)| old != new)
            .unwrap_or(old_len.min(new_len));

        // Nothing changed and same length — no-op.
        if first_diff == old_len && first_diff == new_len {
            return;
        }

        // Move cursor up to the first differing line.
        let lines_up = old_len - first_diff;
        if lines_up > 0 {
            execute!(io::stdout(), cursor::MoveUp(lines_up as u16)).ok();
        }
        execute!(io::stdout(), cursor::MoveToColumn(0)).ok();

        // Rewrite from first_diff to end of new content.
        for line in &new_lines[first_diff..] {
            // Clear the line first (in case new line is shorter than old).
            execute!(io::stdout(), terminal::Clear(terminal::ClearType::CurrentLine)).ok();
            println!("{line}");
        }

        // If new content is shorter, clear remaining old lines.
        if new_len < old_len {
            execute!(
                io::stdout(),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            )
            .ok();
        }

        let _ = io::stdout().flush();
        self.lines = new_lines;
    }

    /// Append lines without diffing (for content that only grows).
    #[allow(dead_code)]
    pub(super) fn append(&mut self, new_lines: &[String]) {
        for line in new_lines {
            println!("{line}");
            self.lines.push(line.clone());
        }
        let _ = io::stdout().flush();
    }

    /// Append a single line.
    #[allow(dead_code)]
    pub(super) fn append_line(&mut self, line: String) {
        println!("{line}");
        let _ = io::stdout().flush();
        self.lines.push(line);
    }

    /// Clear the entire region from the terminal.
    pub(super) fn clear(&mut self) {
        let n = self.lines.len();
        if n > 0 {
            execute!(
                io::stdout(),
                cursor::MoveUp(n as u16),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            )
            .ok();
            let _ = io::stdout().flush();
            self.lines.clear();
        }
    }

    /// Remove the last `n` lines from the region.
    #[allow(dead_code)]
    pub(super) fn pop_lines(&mut self, n: usize) {
        let n = n.min(self.lines.len());
        if n > 0 {
            execute!(
                io::stdout(),
                cursor::MoveUp(n as u16),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            )
            .ok();
            let _ = io::stdout().flush();
            self.lines.truncate(self.lines.len() - n);
        }
    }
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
}
