//! User-turn history cell — what the user typed, as they typed it.
//!
//! Rendered as a Cursor-style block:
//!
//! ```text
//! › first line of the user's message
//!   second line (if any)
//!   ...
//! ```
//!
//! The `› ` prefix is painted in the theme accent (bold) so user
//! turns anchor the eye when scanning scrollback. A soft tinted
//! background spans the whole block as a secondary signal on
//! terminals that support it (see [`user_message_style`]).
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
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);

        let mut lines: Vec<Line<'static>> = Vec::new();

        if self.text.is_empty() {
            // Defensive: empty submits shouldn't reach us (BottomPane
            // filters them), but if one slips through we render the
            // prefix alone so the reader isn't greeted by a blank
            // band with no indicator of what it represents.
            lines.push(Line::from(Span::styled("› ", prefix_style)).style(bg));
        } else {
            for (i, row) in self.text.lines().enumerate() {
                let prefix = if i == 0 {
                    Span::styled("› ", prefix_style)
                } else {
                    // Continuation lines indent to the width of `› `
                    // so the text column stays aligned.
                    Span::raw("  ")
                };
                lines.push(Line::from(vec![prefix, Span::raw(row.to_string())]).style(bg));
            }
        }

        // No cell-local trailing blanks: `flush_chat_widget` adds
        // exactly one blank row between committed cells, which
        // matches Claude Code / Codex spacing (one visible gap
        // between `› prose` and the next cell, zero between
        // `› /cmd` and its `⎿ response` pair). The earlier
        // `tinted-blank + plain-blank` pair over-indented every
        // prose turn by ~2 rows of dead air before the model
        // response, which pushed tool output off the fold.
        //
        // Slash commands continue to suppress even the batch-level
        // separator — see `flush_chat_widget`.
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
        let out = render_cell(&cell, 40, 3);
        // First row should start with the `›` marker and contain the text.
        let first = out.lines().next().unwrap_or_default();
        assert!(first.starts_with('›'), "missing › prefix: {first:?}");
        assert!(
            first.contains("rebuild the index"),
            "text missing: {first:?}"
        );
    }

    #[test]
    fn multiline_renders_prefix_only_on_first_row() {
        let cell = UserCell::new("line one\nline two\nline three");
        let out = render_cell(&cell, 40, 6);
        let rows: Vec<&str> = out.lines().collect();
        assert!(rows[0].starts_with('›'), "row 0 missing prefix");
        assert!(
            !rows[1].starts_with('›'),
            "row 1 should indent, not repeat prefix: {:?}",
            rows[1]
        );
        assert!(rows[1].contains("line two"));
        assert!(rows[2].contains("line three"));
    }

    #[test]
    fn empty_text_still_renders_prefix_band() {
        // Shouldn't normally happen — BottomPane filters empty
        // submits — but the cell must degrade gracefully.
        let cell = UserCell::new("");
        let out = render_cell(&cell, 20, 3);
        assert!(out.lines().next().unwrap_or_default().starts_with('›'));
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
        // Same contract as prose now — no trailing blanks. The
        // distinction between prose and slash lives in
        // `flush_chat_widget`, which suppresses even the batch
        // separator for the `/cmd → ⎿ response` pair.
        let cell = UserCell::new("/model glm-5.1");
        let lines = cell.display_lines(60);
        assert_eq!(
            lines.len(),
            1,
            "slash UserCell is exactly one line: {:?}",
            lines
        );
    }

    #[test]
    fn prose_user_cell_is_tight_one_content_line() {
        // Prose used to emit 2 trailing blanks on top of the batch
        // separator added by `flush_chat_widget` (3 blanks total
        // between `› prose` and the next cell). That pushed the
        // model's reply / tool output off-screen. Both prose and
        // slash now render exactly the content rows; `flush_chat_widget`
        // owns the single blank separator between committed cells.
        let cell = UserCell::new("just a prose question");
        let lines = cell.display_lines(60);
        assert_eq!(
            lines.len(),
            1,
            "UserCell should emit exactly its content rows, no trailing blanks; got {:?}",
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
        insta::assert_snapshot!("user_single_line_40", render_cell(&cell, 40, 3));
    }

    #[test]
    fn snapshot_multiline_60col() {
        let cell = UserCell::new("first line\nsecond line with more words\nthird line");
        insta::assert_snapshot!("user_multiline_60", render_cell(&cell, 60, 5));
    }
}
