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
    fn as_any_ref(&self) -> &dyn std::any::Any {
        self
    }

    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        // Cursor-style: bold accent `› ` prefix + soft tinted background
        // so the user turn reads as a distinct card against the dim
        // scrollback, without the box-drawing weight of a full border.
        let bg = user_message_style();
        let theme = crate::tui::theme::current();
        let prefix_style = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);

        let mut lines: Vec<Line<'static>> = Vec::new();

        for (i, text_line) in self.message.lines().enumerate() {
            let prefix = if i == 0 {
                Span::styled("› ", prefix_style)
            } else {
                Span::raw("  ")
            };
            lines.push(Line::from(vec![prefix, Span::raw(text_line.to_string())]).style(bg));
        }
        if self.message.is_empty() {
            lines.push(Line::from(Span::styled("› ", prefix_style)).style(bg));
        }

        // One trailing blank inside the tinted background so the card
        // has a little breathing room at the bottom. A second,
        // non-tinted blank separates it from the next cell.
        lines.push(Line::styled("", bg));
        lines.push(Line::default());
        lines
    }
}
