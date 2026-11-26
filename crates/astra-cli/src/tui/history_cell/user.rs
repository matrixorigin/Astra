//! User-turn history cell — the semantic conversational input sent to the turn.
//!
//! Rendered as a quoted input block:
//!
//! ```text
//! › first line of the user's message
//!   subsequent and wrapped lines align beneath it
//! ```
//!
//! A quiet slate background spans the content and one breathing row above and
//! below it. Only the first row carries the `›` marker; every content row uses
//! the same two-column text start as assistant messages. This keeps long user
//! input legible without reviving the old opaque card.
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
    pub(crate) const CONTENT_ROW_OFFSET: usize = 1;

    fn prefix_width(width: u16) -> u16 {
        if width > 3 { 2 } else { 0 }
    }

    pub(crate) fn content_width(width: u16) -> usize {
        usize::from(width.saturating_sub(Self::prefix_width(width)).max(1))
    }

    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ts: None,
        }
    }

    /// Reconstruct from a persisted event for session resume.
    pub fn from_persist(ev: TurnEvent) -> Option<Self> {
        match ev {
            TurnEvent::User { text, ts } => Some(Self { text, ts }),
            _ => None,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    fn format_rows(rows: impl IntoIterator<Item = String>, width: u16) -> Vec<Line<'static>> {
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
        // Background ownership is semantic metadata on the Line. Encoding a
        // viewport-sized blank row as spaces would leave a real terminal in
        // auto-wrap pending state at the right edge.
        let blank_row = || Line::default().style(bg);
        let mut lines: Vec<Line<'static>> = vec![blank_row()];
        let prefix_width = Self::prefix_width(width);
        let mut first_row = true;
        for row in rows {
            let prefix = if prefix_width == 0 {
                ""
            } else if first_row {
                "› "
            } else {
                "  "
            };
            lines.push(
                Line::from(vec![Span::styled(prefix, prefix_style), Span::raw(row)]).style(bg),
            );
            first_row = false;
        }
        lines.push(blank_row());
        lines
    }

    /// Keep source line boundaries for terminal soft wrapping and copying.
    pub(crate) fn terminal_lines(&self, width: u16) -> Vec<Line<'static>> {
        let clean = crate::tui::render::line_utils::sanitize_terminal_text(&self.text);
        Self::format_rows(clean.split('\n').map(str::to_owned), width)
    }
}

impl HistoryCell for UserCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let clean = crate::tui::render::line_utils::sanitize_terminal_text(&self.text);
        let rows = clean.split('\n').flat_map(|logical_row| {
            crate::tui::wrapping::wrap_data_line(
                logical_row,
                Self::content_width(width),
                usize::MAX,
            )
            .0
        });
        Self::format_rows(rows, width)
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
        let lines = cell.display_lines(40);
        assert_eq!(lines[1].spans[0].content.as_ref(), "› ");
        assert_eq!(lines[2].spans[0].content.as_ref(), "  ");
        assert_eq!(lines[3].spans[0].content.as_ref(), "  ");
        assert_eq!(lines[2].spans[1].content.as_ref(), "line two");
        assert_eq!(lines[3].spans[1].content.as_ref(), "line three");
    }

    #[test]
    fn long_unbroken_input_wraps_with_aligned_continuation_rows() {
        let input = "x".repeat(31);
        let cell = UserCell::new(input.clone());
        let lines = cell.display_lines(12);
        let content = &lines[1..lines.len() - 1];
        assert!(content.len() > 1, "input was not wrapped: {content:?}");
        assert_eq!(content[0].spans[0].content.as_ref(), "› ");
        for row in content {
            assert!(row.width() <= 12, "row exceeds terminal width: {row:?}");
        }
        for row in &content[1..] {
            assert_eq!(row.spans[0].content.as_ref(), "  ");
        }
        let reconstructed = content
            .iter()
            .map(|line| line.spans[1].content.as_ref())
            .collect::<String>();
        assert_eq!(reconstructed, input);
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
        let persisted = TurnEvent::User {
            text: "hello world".to_string(),
            ts: Some("2026-05-09T12:00:00Z".to_string()),
        };
        let back = UserCell::from_persist(persisted.clone()).expect("from_persist");
        assert_eq!(back.text, "hello world");
        assert_eq!(back.ts.as_deref(), Some("2026-05-09T12:00:00Z"));
        assert_eq!(back.to_persist(), Some(persisted));
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
            link: None,
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
        assert_eq!(
            lines[0].width(),
            0,
            "breathing rows must not encode the viewport width as spaces"
        );
        assert_eq!(lines[3].width(), 0);
    }

    #[test]
    fn user_input_surface_spans_the_full_rendered_width_with_breathing_rows() {
        const CASE: &str = "ASTRA_TEST_USER_SURFACE_PROFILE";
        if std::env::var_os(CASE).is_none() {
            // Isolate the process-wide theme cache and terminal hints. The
            // default/unstyled surface must be tested alongside coloured ones.
            let test = format!(
                "{}::user_input_surface_spans_the_full_rendered_width_with_breathing_rows",
                module_path!().split_once("::").unwrap().1
            );
            for profile in ["auto", "light", "dark", "plain", "no-color"] {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap());
                child
                    .args(["--exact", &test, "--nocapture"])
                    .env(CASE, profile)
                    .env(
                        "ASTRA_TUI_THEME",
                        if profile == "no-color" {
                            "light"
                        } else {
                            profile
                        },
                    )
                    .env("COLORTERM", "truecolor")
                    .env_remove("NO_COLOR")
                    .env_remove("COLORFGBG")
                    .env_remove("ASTRA_TERMINAL_FG")
                    .env_remove("ASTRA_TERMINAL_BG");
                if profile == "no-color" {
                    child.env("NO_COLOR", "1");
                }
                let output = child.output().expect("run isolated user surface test");
                assert!(output.status.success(), "{profile}: {output:?}");
                assert!(
                    String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                    "{output:?}"
                );
            }
            return;
        }

        let width = 48;
        let cell = UserCell::new("review these changes");
        let paragraph =
            crate::tui::render::line_utils::FullRowParagraph::new(cell.display_lines(width))
                .wrap(ratatui::widgets::Wrap { trim: false });
        let buffer = draw_widget(paragraph, width, 3);
        let style = crate::tui::style::user_message_style();
        let profile = std::env::var(CASE).unwrap();
        if matches!(profile.as_str(), "light" | "dark") {
            assert!(style.bg.is_some_and(|bg| bg != Color::Reset));
            assert!(style.fg.is_some_and(|fg| fg != Color::Reset));
        } else {
            assert_eq!(style, Style::default());
        }
        let expected = style.bg.unwrap_or(Color::Reset);
        let text_cell = &buffer[(2, 1)];
        assert_eq!(text_cell.symbol(), "r");
        assert_eq!(text_cell.fg, style.fg.unwrap_or(Color::Reset));

        for y in 0..3 {
            assert!(
                (0..width).all(|x| buffer[(x, y)].bg == expected),
                "user surface row {y} must reach the terminal edge"
            );
        }
    }
}
