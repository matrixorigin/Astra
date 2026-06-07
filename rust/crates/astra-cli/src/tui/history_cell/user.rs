//! User-turn history cell — what the user typed, as they typed it.
//!
//! Rendered as a quoted input block:
//!
//! ```text
//! › first line of the user's message
//! › second line (if any)
//! › ...
//! ```
//!
//! A soft tinted background spans the whole block and every content
//! row gets the same `› ` quote prefix so the message reads as one
//! visual unit rather than a prompt/continuation pair.
//!
//! Persists as [`TurnEvent::User`]. Never enters a live state —
//! the text is fully known at construction time.

use std::any::Any;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::HistoryCell;
use crate::tui::style::user_message_style;
use crate::tui::turn_event::TurnEvent;

#[derive(Debug, Clone)]
pub(crate) struct UserCell {
    text: String,
    /// Optional RFC3339 timestamp. `None` is fine; renderers don't
    /// display it, but it's persisted for audit/sort use cases.
    ts: Option<String>,
}

impl UserCell {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ts: None,
        }
    }

    /// Associate a timestamp at construction time. Callers that
    /// already have an `Instant` converted to RFC3339 can pass it;
    /// otherwise leave unset.
    #[allow(dead_code)]
    pub fn with_ts(mut self, ts: impl Into<String>) -> Self {
        self.ts = Some(ts.into());
        self
    }

    /// Reconstruct from a persisted event. Used by the resume path
    /// in Phase 4.
    #[allow(dead_code)]
    pub fn from_persist(ev: TurnEvent) -> Option<Self> {
        match ev {
            TurnEvent::User { text, ts } => Some(Self { text, ts }),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl HistoryCell for UserCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let bg = user_message_style();
        let theme = crate::tui::theme::current();
        let prefix_style = Style::default()
            .fg(theme.accent_dim())
            .add_modifier(Modifier::DIM);
        let pad = Line::from(Span::raw("")).style(bg);

        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(pad.clone());

        if self.text.is_empty() {
            lines.push(Line::from(Span::styled("› ", prefix_style)).style(bg));
        } else {
            for row in self.text.lines() {
                let prefix = Span::styled("› ", prefix_style);
                lines.push(Line::from(vec![prefix, Span::raw(row.to_string())]).style(bg));
            }
        }
        lines.push(pad);

        lines
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn to_persist(&self) -> Option<TurnEvent> {
        Some(TurnEvent::User {
            ts: self.ts.clone(),
            text: self.text.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    fn render_cell(cell: &UserCell, width: u16, height: u16) -> String {
        let lines = cell.display_lines(width);
        let p =
            ratatui::widgets::Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false });
        buffer_to_string(&draw_widget(p, width, height))
    }

    #[test]
    fn single_line_renders_prefix_and_text() {
        let cell = UserCell::new("rebuild the index");
        let out = render_cell(&cell, 40, 5);
        let rows: Vec<&str> = out.lines().collect();
        let first = rows.get(1).copied().unwrap_or_default();
        assert!(
            first.trim_start().starts_with('›'),
            "missing › prefix: {first:?}"
        );
        assert!(
            first.contains("rebuild the index"),
            "text missing: {first:?}"
        );
    }

    #[test]
    fn multiline_renders_prefix_only_on_first_row() {
        let cell = UserCell::new("line one\nline two\nline three");
        let out = render_cell(&cell, 40, 7);
        let rows: Vec<&str> = out.lines().collect();
        assert!(
            rows[1].trim_start().starts_with('›'),
            "row 1 missing prefix"
        );
        assert!(
            rows[2].trim_start().starts_with('›'),
            "row 2 missing prefix"
        );
        assert!(
            rows[3].trim_start().starts_with('›'),
            "row 3 missing prefix"
        );
        assert!(rows[2].contains("line two"));
        assert!(rows[3].contains("line three"));
    }

    #[test]
    fn empty_text_still_renders_prefix_band() {
        // Shouldn't normally happen — BottomPane filters empty
        // submits — but the cell must degrade gracefully.
        let cell = UserCell::new("");
        let out = render_cell(&cell, 20, 5);
        assert!(
            out.lines()
                .nth(1)
                .unwrap_or_default()
                .trim_start()
                .starts_with('›')
        );
    }

    #[test]
    fn is_live_and_finalize_defaults() {
        // User cells are never live — the text is fully known at
        // construction. finalize() must be a no-op.
        let mut cell = UserCell::new("x");
        assert!(!cell.is_live());
        cell.finalize(); // must not panic, must not mutate.
        assert_eq!(cell.text(), "x");
    }

    #[test]
    fn persist_roundtrip_preserves_text_and_ts() {
        let original = UserCell::new("hello world").with_ts("2026-05-09T12:00:00Z");
        let persisted = original.to_persist().expect("must persist");
        let back = UserCell::from_persist(persisted.clone()).expect("from_persist");
        assert_eq!(back.text, "hello world");
        assert_eq!(back.ts.as_deref(), Some("2026-05-09T12:00:00Z"));
    }

    #[test]
    fn slash_command_user_cell_is_tight_one_content_line() {
        let cell = UserCell::new("/model glm-5.1");
        let lines = cell.display_lines(60);
        assert_eq!(
            lines.len(),
            3,
            "slash UserCell should render top pad, content, bottom pad: {:?}",
            lines
        );
    }

    #[test]
    fn prose_user_cell_is_tight_one_content_line() {
        let cell = UserCell::new("just a prose question");
        let lines = cell.display_lines(60);
        assert_eq!(
            lines.len(),
            3,
            "UserCell should emit pad + content + pad; got {:?}",
            lines
        );
    }

    #[test]
    fn from_persist_rejects_wrong_variant() {
        // Defensive: resume replay iterates a mixed `TurnEvent`
        // stream and dispatches by kind. Passing the wrong event
        // to the wrong constructor must return None rather than
        // silently building a blank cell.
        let wrong = TurnEvent::System {
            ts: None,
            level: crate::tui::turn_event::SystemLevel::Info,
            text: "not a user".into(),
        };
        assert!(UserCell::from_persist(wrong).is_none());
    }

    #[test]
    fn snapshot_single_line_40col() {
        let cell = UserCell::new("rebuild the index");
        crate::tui::testing::assert_tui_snapshot!("user_single_line_40", render_cell(&cell, 40, 3));
    }

    #[test]
    fn snapshot_multiline_60col() {
        let cell = UserCell::new("first line\nsecond line with more words\nthird line");
        crate::tui::testing::assert_tui_snapshot!("user_multiline_60", render_cell(&cell, 60, 5));
    }
}
