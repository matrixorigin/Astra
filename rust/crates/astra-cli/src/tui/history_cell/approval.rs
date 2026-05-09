//! Inline approval prompt rendered above the composer.
//!
//! Visual language mirrors Cursor/Copilot:
//!
//! ```text
//! ⏸ bash wants to run
//!   rm -rf /tmp/scratch
//!   destructive path outside cwd
//!
//! ▸ Accept      Reject    Always   Skip
//!   ← → navigate · Enter confirm · Esc reject
//! ```
//!
//! The focused button uses a reversed pill (accent bg, contrasting fg);
//! other buttons render plain/dim. Footer advertises the four key
//! bindings so no training is required.
//!
//! Lives in `history_cell` but is NOT committed to scrollback — the
//! bottom pane owns its lifetime (one live approval cell at a time,
//! destroyed when the user resolves). The trait membership just keeps
//! the cell API uniform; `to_persist` returns `None` so the cell is
//! never written to the transcript.

use std::any::Any;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::HistoryCell;
use crate::tui::approval::ButtonRow;
use crate::tui::turn_event::TurnEvent;

#[derive(Debug, Clone)]
pub(crate) struct ApprovalCell {
    pub id: u64,
    pub tool: String,
    pub header: String,
    pub detail: Option<String>,
    pub reason: String,
    pub focused: bool,
    pub buttons: ButtonRow,
}

impl ApprovalCell {
    pub fn new(
        id: u64,
        tool: String,
        header: String,
        detail: Option<String>,
        reason: String,
        focused: bool,
    ) -> Self {
        Self {
            id,
            tool,
            header,
            detail,
            reason,
            focused,
            buttons: ButtonRow::primary(),
        }
    }

    /// Construct with the extended Accept-all / Reject-all buttons
    /// appended. Call when more than one approval is pending.
    #[allow(dead_code)]
    pub fn with_batch(
        id: u64,
        tool: String,
        header: String,
        detail: Option<String>,
        reason: String,
        focused: bool,
    ) -> Self {
        Self {
            id,
            tool,
            header,
            detail,
            reason,
            focused,
            buttons: ButtonRow::primary_with_batch(),
        }
    }

    /// Move button focus — only honoured when this cell itself is focused.
    #[allow(dead_code)]
    pub fn move_button_left(&mut self) {
        if self.focused {
            self.buttons.move_left();
        }
    }

    #[allow(dead_code)]
    pub fn move_button_right(&mut self) {
        if self.focused {
            self.buttons.move_right();
        }
    }

    fn button_line(&self) -> Line<'static> {
        // Leading gutter indent aligned with detail/reason rows.
        let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];

        for (i, btn) in self.buttons.buttons().iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }

            let is_focused = self.focused && i == self.buttons.focus();
            if is_focused {
                // Cursor-style reversed pill. Colours come from the
                // active theme so the button remains readable on both
                // dark and light terminals (the old `bg(Cyan).fg(Black)`
                // pair collapsed to invisible on light backgrounds).
                let theme = crate::tui::theme::current();
                let sel_style = Style::default()
                    .bg(theme.accent)
                    .fg(theme.selected_fg)
                    .add_modifier(Modifier::BOLD);
                spans.push(Span::styled(format!("▸ {} ", btn.label), sel_style));
            } else {
                let label_style = if self.focused {
                    Style::default().fg(Color::Gray)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                spans.push(Span::styled(format!("  {} ", btn.label), label_style));
            }
        }
        Line::from(spans)
    }
}

impl HistoryCell for ApprovalCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let dim = Style::default().fg(Color::DarkGray);
        let yellow = Style::default().fg(Color::Yellow);

        let mut lines = Vec::new();

        // Header row: ⏸ <header>
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("⏸ ", yellow),
            Span::styled(self.header.clone(), yellow),
        ]));

        // Optional detail (first 2 lines, dim).
        if let Some(ref detail) = self.detail {
            for dl in detail.lines().take(2) {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(dl.to_string(), dim),
                ]));
            }
        }

        // Reason, prefixed.
        if !self.reason.is_empty() {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(self.reason.clone(), dim),
            ]));
        }

        // Blank row before buttons for breathing space.
        lines.push(Line::default());

        // Button row.
        lines.push(self.button_line());

        // Key-binding hint (only for focused cell — unfocused ones are
        // reminders, not action surfaces, so don't clutter).
        if self.focused {
            lines.push(Line::from(Span::styled(
                "    ← → navigate · Enter confirm · Esc reject".to_string(),
                dim,
            )));
        }

        lines
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    /// Approval cells are ephemeral — they disappear when the user
    /// resolves. Never persisted.
    fn to_persist(&self) -> Option<TurnEvent> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focused_cell_renders_hint_and_arrow() {
        let cell = ApprovalCell::new(
            1,
            "bash".into(),
            "bash wants to run".into(),
            Some("rm -rf /tmp/x".into()),
            "destructive path".into(),
            true,
        );
        let lines = cell.display_lines(80);
        let rendered: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("⏸"), "header glyph missing");
        assert!(rendered.contains("rm -rf /tmp/x"), "detail missing");
        assert!(rendered.contains("destructive path"), "reason missing");
        assert!(rendered.contains("▸"), "focus arrow missing on focused cell");
        assert!(
            rendered.contains("← → navigate"),
            "key-binding hint missing on focused cell"
        );
    }

    #[test]
    fn unfocused_cell_omits_hint_and_arrow() {
        let cell = ApprovalCell::new(
            1,
            "bash".into(),
            "bash wants to run".into(),
            None,
            "reason".into(),
            false,
        );
        let lines = cell.display_lines(80);
        let rendered: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !rendered.contains("▸"),
            "unfocused cell should not show focus arrow"
        );
        assert!(
            !rendered.contains("← → navigate"),
            "unfocused cell should not show key hint"
        );
    }

    #[test]
    fn never_persists() {
        let cell = ApprovalCell::new(1, "t".into(), "h".into(), None, "r".into(), true);
        assert!(cell.to_persist().is_none());
    }
}
