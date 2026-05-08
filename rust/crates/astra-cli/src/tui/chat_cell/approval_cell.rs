//! Inline approval prompt rendered in the chat stream.
//!
//! Shown between tool cells when a tool asks for user approval and the
//! session's permission mode is not auto-run. Replaces the old modal
//! overlay with a non-blocking visual so the user can keep reading other
//! background work while deciding.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::ChatCell;

/// How the cell is drawn depends on whether it's the focused (next-up)
/// approval. The focused cell highlights its key hints in cyan; others
/// render dim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApprovalChatCell {
    pub id: u64,
    pub tool: String,
    pub header: String,
    pub detail: Option<String>,
    pub reason: String,
    pub focused: bool,
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
        let accent = if self.focused {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        let key_style = if self.focused {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        };

        let mut lines = Vec::new();

        // Header row: ⏸ [focus marker] <header>
        let marker = if self.focused { "▌ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(marker, accent),
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
                Span::styled(format!("Reason: {}", &self.reason), dim),
            ]));
        }

        // Key hint row.
        let hint = if self.focused {
            "    [y] allow  [n] deny  [a] always  [s] skip  ·  Tab next"
        } else {
            "    [y] allow  [n] deny  [a] always  [s] skip"
        };
        lines.push(Line::from(Span::styled(hint.to_string(), key_style)));

        lines
    }
}
