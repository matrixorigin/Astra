//! User-turn history cell — the semantic conversational input sent to the turn.
//!
//! Rendered as a quoted input block:
//!
//! ```text
//! › first line of the user's message
//! › second line (if any)
//! › ...
//! ```
//!
//! A quiet slate background spans the content and one breathing row above and
//! below it; every content row gets the same `› ` prefix. This keeps a short
//! user turn legible as an input block without reviving the old opaque card.
//!
//! Local UI/control actions use `SystemCell::action` instead, so this type is
//! always prompt-facing. Persists as [`TurnEvent::User`]. Never enters a live state —
//! the text is fully known at construction time.

use std::any::Any;

use ratatui::style::{Color, Style};
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
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let bg = user_message_style();
        let theme = crate::tui::theme::current();
        let prefix_style = Style::default().fg(if theme.is_light {
            Color::DarkGray
        } else {
            theme.accent_dim()
        });

        // The transcript separator intentionally gives UserCell no extra
        // blank line. Keep its compact, symmetric breathing room here so the
        // visual boundary survives both live rendering and persisted replay.
        let blank_row = || Line::from(Span::raw(" ".repeat(usize::from(width.max(1))))).style(bg);
        let mut lines: Vec<Line<'static>> = vec![blank_row()];

        if self.text.is_empty() {
            lines.push(
                Line::from(vec![
                    Span::raw(" "),
                    Span::styled("›", prefix_style),
                    Span::raw(" "),
                ])
                .style(bg),
            );
        } else {
            for row in self.text.lines() {
                lines.push(
                    Line::from(vec![
                        Span::raw(" "),
                        Span::styled("›", prefix_style),
                        Span::raw(" "),
                        Span::raw(row.to_string()),
                    ])
                    .style(bg),
                );
            }
        }

        lines.push(blank_row());
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
    fn prose_user_cell_keeps_symmetric_breathing_room() {
        let cell = UserCell::new("just a prose question");
        let lines = cell.display_lines(60);
        assert_eq!(
            lines.len(),
            3,
            "UserCell should keep one content row plus two breathing rows; got {:?}",
            lines
        );
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|span| span.content.trim().is_empty())
        );
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|span| span.content.contains("just a prose question"))
        );
        assert!(
            lines[2]
                .spans
                .iter()
                .all(|span| span.content.trim().is_empty())
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
    fn every_user_content_row_carries_the_same_quiet_surface() {
        let cell = UserCell::new("first\nsecond");
        let lines = cell.display_lines(60);
        assert_eq!(lines.len(), 4);
        let expected = crate::tui::style::user_message_style().bg;
        assert!(lines.iter().all(|line| line.style.bg == expected));
    }

    #[test]
    fn user_input_surface_spans_the_full_rendered_width_with_breathing_rows() {
        let width = 48;
        let cell = UserCell::new("review these changes");
        let paragraph = ratatui::widgets::Paragraph::new(cell.display_lines(width))
            .wrap(ratatui::widgets::Wrap { trim: false });
        let buffer = draw_widget(paragraph, width, 3);
        let expected = crate::tui::style::user_message_style()
            .bg
            .expect("user surface always has a background");

        for y in 0..3 {
            assert!(
                (0..width).all(|x| buffer[(x, y)].bg == expected),
                "user surface row {y} must reach the terminal edge"
            );
        }
    }
}
