use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::ChatCell;
use crate::tui::style::user_message_style;

#[derive(Debug)]
pub(crate) struct UserChatCell {
    pub message: String,
}

impl UserChatCell {
    pub fn new(message: String) -> Self {
        Self { message }
    }
}

impl ChatCell for UserChatCell {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let bg = user_message_style();

        let mut lines = Vec::new();
        // No top blank — the FlexRenderable inset handles spacing above

        for (i, text_line) in self.message.lines().enumerate() {
            let prefix = if i == 0 {
                Span::styled(
                    "› ",
                    Style::default().add_modifier(Modifier::BOLD | Modifier::DIM),
                )
            } else {
                Span::raw("  ")
            };
            lines.push(Line::from(vec![prefix, Span::raw(text_line.to_string())]).style(bg));
        }
        if self.message.is_empty() {
            lines.push(
                Line::from(Span::styled(
                    "› ",
                    Style::default().add_modifier(Modifier::BOLD | Modifier::DIM),
                ))
                .style(bg),
            );
        }

        lines.push(Line::styled("", bg)); // blank line 1 below
        lines.push(Line::default());     // blank line 2 below (Codex has 2 blank lines after user)
        lines
    }
}
