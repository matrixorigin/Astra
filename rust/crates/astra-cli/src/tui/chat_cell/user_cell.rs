use ratatui::style::{Style, Stylize};
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
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let bg = user_message_style();
        let mut lines = Vec::new();
        for (i, text_line) in self.message.lines().enumerate() {
            let prefix = if i == 0 {
                Span::styled("› ", Style::default().cyan())
            } else {
                Span::raw("  ")
            };
            lines.push(Line::from(vec![
                prefix,
                Span::styled(text_line.to_string(), bg),
            ]));
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled("› ", Style::default().cyan())));
        }
        lines
    }
}
