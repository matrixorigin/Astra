//! Inline approval prompt rendered in the chat stream.
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
//! The focused button uses a cyan-reversed background (bold foreground on
//! the cell's theme colour), matching the look-and-feel users already
//! know from their IDEs. Other buttons render plain/dim. The footer
//! advertises the four key bindings so no training is required.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::ChatCell;
use crate::tui::approval::ButtonRow;

#[derive(Debug, Clone)]
pub(crate) struct ApprovalChatCell {
    pub id: u64,
    pub tool: String,
    pub header: String,
    pub detail: Option<String>,
    pub reason: String,
    pub focused: bool,
    pub buttons: ButtonRow,
}

impl ApprovalChatCell {
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
    pub fn move_button_left(&mut self) {
        if self.focused {
            self.buttons.move_left();
        }
    }

    pub fn move_button_right(&mut self) {
        if self.focused {
            self.buttons.move_right();
        }
    }
}

impl ChatCell for ApprovalChatCell {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn as_any_ref(&self) -> &dyn std::any::Any {
        self
    }

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
}

impl ApprovalChatCell {
    fn button_line(&self) -> Line<'static> {
        // Leading gutter indent aligned with detail/reason rows.
        let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];

        for (i, btn) in self.buttons.buttons().iter().enumerate() {
            // Separator between buttons (spaces).
            if i > 0 {
                spans.push(Span::raw("  "));
            }

            let is_focused = self.focused && i == self.buttons.focus();
            if is_focused {
                // Cursor-style reversed-cyan pill: "▸ Accept" with
                // cyan bg + black text + bold.
                let sel_style = Style::default()
                    .bg(Color::Cyan)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD);
                spans.push(Span::styled(format!("▸ {} ", btn.label), sel_style));
            } else {
                // Plain-rendered label.
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
