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
        // Button row lives INSIDE the card, so the caller pads the
        // leading bar. We only emit spans for the buttons + their
        // inter-button spacing here.
        let mut spans: Vec<Span<'static>> = Vec::new();
        let theme = crate::tui::theme::current();
        let dim = Style::default().fg(Color::DarkGray);
        let body = if self.focused {
            Style::default().fg(Color::Gray)
        } else {
            dim
        };

        for (i, btn) in self.buttons.buttons().iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" ".to_string(), dim));
            }

            let is_focused = self.focused && i == self.buttons.focus();
            if is_focused {
                // Cursor-style reversed pill with bracket markers so
                // the focused button reads as a target even when the
                // terminal strips background color (screen readers,
                // monochrome, copy-paste).
                let sel_style = Style::default()
                    .bg(theme.accent)
                    .fg(theme.selected_fg)
                    .add_modifier(Modifier::BOLD);
                spans.push(Span::styled(format!(" {} ", btn.label), sel_style));
            } else {
                // Subtle bracket outline so all buttons have the same
                // shape as the focused one — just without the fill.
                spans.push(Span::styled(format!(" {} ", btn.label), body));
            }
        }
        Line::from(spans)
    }
}

impl HistoryCell for ApprovalCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        // Colors come from the active theme so the card remains
        // legible on both dark and light terminals. `accent` is the
        // card's identity color (border + focused pill); `muted` is
        // for detail/reason rows and the unfocused hint.
        let theme = crate::tui::theme::current();
        let accent_style = Style::default().fg(theme.accent);
        let muted = Style::default().fg(Color::DarkGray);
        let body_style = if self.focused {
            Style::default().fg(Color::Gray)
        } else {
            muted
        };

        let mut lines = Vec::new();

        // ── Top border with embedded title ────────────────────────
        //     ╭─ ⏸ bash wants to run ─────────────────────────
        let top_border = Line::from(vec![
            Span::styled("╭─ ".to_string(), accent_style),
            Span::styled("⏸ ".to_string(), accent_style),
            Span::styled(
                self.header.clone(),
                accent_style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ".to_string(), accent_style),
        ]);
        lines.push(top_border);

        // Body rows use a vertical accent bar on the left so the
        // card reads as one visual block, not a heap of bullet
        // points. Mirrors Cursor's inline tool cards.
        let bar = Span::styled("│ ".to_string(), accent_style);

        // Optional detail (first 3 lines — bumped from 2 so a
        // multi-line bash command isn't mystery-truncated).
        if let Some(ref detail) = self.detail {
            for dl in detail.lines().take(3) {
                lines.push(Line::from(vec![
                    bar.clone(),
                    Span::styled(dl.to_string(), body_style),
                ]));
            }
        }

        // Reason — prefixed with a subtle marker so it's clearly
        // not user-command text.
        if !self.reason.is_empty() {
            lines.push(Line::from(vec![
                bar.clone(),
                Span::styled("⚠ ".to_string(), muted),
                Span::styled(self.reason.clone(), muted),
            ]));
        }

        // Breathing space inside the card.
        lines.push(Line::from(bar.clone()));

        // Button row (prefixed with bar so alignment is preserved).
        let mut button_row = vec![bar.clone()];
        button_row.extend(self.button_line().spans);
        lines.push(Line::from(button_row));

        // ── Bottom border: hint on the border itself ──────────────
        // Focused: advertise every binding on the border, Cursor-
        // style. Unfocused: plain border close — the user isn't
        // looking at this card for actions yet.
        let bottom = if self.focused {
            Line::from(vec![
                Span::styled("╰─ ".to_string(), accent_style),
                Span::styled(
                    "↑↓←→ select · Enter confirm · Esc reject · Ctrl+Enter quick accept"
                        .to_string(),
                    muted,
                ),
                Span::styled(" ".to_string(), accent_style),
            ])
        } else {
            Line::from(vec![Span::styled("╰─".to_string(), accent_style)])
        };
        lines.push(bottom);

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

    fn render(cell: &ApprovalCell) -> String {
        cell.display_lines(80)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn focused_cell_renders_rounded_box_and_hint() {
        let cell = ApprovalCell::new(
            1,
            "bash".into(),
            "bash wants to run".into(),
            Some("rm -rf /tmp/x".into()),
            "destructive path".into(),
            true,
        );
        let rendered = render(&cell);
        assert!(rendered.contains("⏸"), "header glyph missing");
        assert!(rendered.contains("rm -rf /tmp/x"), "detail missing");
        assert!(rendered.contains("destructive path"), "reason missing");
        // Cursor-style rounded box edges.
        assert!(rendered.contains("╭─"), "top border missing");
        assert!(rendered.contains("╰─"), "bottom border missing");
        assert!(
            rendered.contains("│"),
            "left-edge accent bar missing inside the card"
        );
        // Hint is advertised on the bottom border for focused cells,
        // including every key binding the user can reach.
        assert!(
            rendered.contains("↑↓←→ select"),
            "arrow-key hint missing on focused cell"
        );
        assert!(
            rendered.contains("Ctrl+Enter"),
            "Ctrl+Enter shortcut hint missing on focused cell"
        );
    }

    #[test]
    fn unfocused_cell_keeps_border_but_omits_hint() {
        let cell = ApprovalCell::new(
            1,
            "bash".into(),
            "bash wants to run".into(),
            None,
            "reason".into(),
            false,
        );
        let rendered = render(&cell);
        // Box edges still render so the pending queue reads as a
        // stack of cards.
        assert!(rendered.contains("╭─"));
        assert!(rendered.contains("╰─"));
        // But the action hint is reserved for the focused cell.
        assert!(
            !rendered.contains("↑↓←→ select"),
            "unfocused cell should not advertise actions"
        );
    }

    #[test]
    fn never_persists() {
        let cell = ApprovalCell::new(1, "t".into(), "h".into(), None, "r".into(), true);
        assert!(cell.to_persist().is_none());
    }
}
