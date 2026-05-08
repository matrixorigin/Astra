//! Shared TUI test harness — cfg(test) only.
//!
//! Provides three layers of helpers used across tui tests:
//! 1. [`keys`] — terse `KeyEvent` constructors (replaces per-module copies).
//! 2. [`render`] — ratatui `TestBackend` → `Buffer` → human-readable `String`.
//! 3. [`snapshot`] — thin wrappers around `insta` for rendered buffers.
//!
//! Conventions:
//! - Snapshots trim trailing whitespace per row so diffs stay small and stable.
//! - Width/height conventions: narrow (40x12), default (80x24), wide (120x40).
//! - Use `snapshot_buffer!("name", &buffer)` to emit a named snapshot.

#![cfg(test)]
#![allow(dead_code)]

pub(crate) mod keys {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    pub(crate) fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub(crate) fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    pub(crate) fn ctrl_char(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    pub(crate) fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    pub(crate) fn alt_char(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    pub(crate) fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    pub(crate) fn typed(s: &str) -> Vec<KeyEvent> {
        s.chars().map(|c| key(KeyCode::Char(c))).collect()
    }
}

pub(crate) mod render {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::widgets::Widget;

    /// Render a `Widget` into a `TestBackend` buffer of the given size.
    pub(crate) fn draw_widget<W: Widget>(widget: W, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("construct TestBackend terminal");
        terminal
            .draw(|f| {
                f.render_widget(widget, f.area());
            })
            .expect("TestBackend draw");
        terminal.backend().buffer().clone()
    }

    /// Convert a `Buffer` to a multi-line string. Each row is trimmed on the
    /// right so trailing spaces do not inflate snapshot diffs.
    pub(crate) fn buffer_to_string(buf: &Buffer) -> String {
        let area = buf.area();
        let mut rows: Vec<String> = Vec::with_capacity(area.height as usize);
        for y in 0..area.height {
            let mut row = String::with_capacity(area.width as usize);
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    row.push_str(cell.symbol());
                }
            }
            // Trim only trailing whitespace — preserve leading indentation.
            let trimmed_end = row.trim_end_matches(' ');
            rows.push(trimmed_end.to_string());
        }
        rows.join("\n")
    }
}

/// Assert a `Buffer` matches a named insta snapshot.
///
/// Usage: `snapshot_buffer!("user_cell_basic", &buffer);`
#[allow(unused_macros)]
macro_rules! snapshot_buffer {
    ($name:expr, $buffer:expr) => {{
        let rendered = $crate::tui::testing::render::buffer_to_string($buffer);
        insta::assert_snapshot!($name, rendered);
    }};
}
#[allow(unused_imports)]
pub(crate) use snapshot_buffer;

#[cfg(test)]
mod harness_self_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::style::Style;
    use ratatui::widgets::Paragraph;

    #[test]
    fn keys_builders_match_expected_modifiers() {
        assert_eq!(keys::key(KeyCode::Enter).modifiers, KeyModifiers::NONE);
        assert_eq!(
            keys::ctrl(KeyCode::Char('c')).modifiers,
            KeyModifiers::CONTROL
        );
        assert_eq!(keys::ctrl_char('c').code, KeyCode::Char('c'));
        assert_eq!(keys::alt(KeyCode::Left).modifiers, KeyModifiers::ALT);
        assert_eq!(keys::alt_char('b').code, KeyCode::Char('b'));
        assert_eq!(keys::shift(KeyCode::Tab).modifiers, KeyModifiers::SHIFT);
    }

    #[test]
    fn typed_emits_one_event_per_char() {
        let events = keys::typed("hi");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].code, KeyCode::Char('h'));
        assert_eq!(events[1].code, KeyCode::Char('i'));
    }

    #[test]
    fn buffer_to_string_trims_trailing_whitespace() {
        let p = Paragraph::new("hi").style(Style::default());
        let buf = render::draw_widget(p, 10, 2);
        let s = render::buffer_to_string(&buf);
        let lines: Vec<&str> = s.split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "hi", "row 0 should be trimmed, got {:?}", lines[0]);
        assert_eq!(lines[1], "", "blank row should trim to empty string");
    }

    #[test]
    fn buffer_to_string_preserves_leading_spaces() {
        let p = Paragraph::new("  indented");
        let buf = render::draw_widget(p, 20, 1);
        let s = render::buffer_to_string(&buf);
        assert_eq!(s, "  indented");
    }
}
