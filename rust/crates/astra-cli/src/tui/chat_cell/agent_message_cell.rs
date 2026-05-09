use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};

use super::ChatCell;
use crate::tui::wrapping::{RtOptions, adaptive_wrap_lines};

/// A small streaming cell emitted per commit tick.
///
/// Cursor-style assistant reply: every line gets a colored `┃ `
/// gutter in the theme accent so the reply reads as a distinct
/// pull-quote without needing a separate label row. `is_first_line`
/// still exists so the outer flow knows this cell continues a stream
/// (no extra leading blank), but the visual prefix is now uniform
/// across continuation cells.
#[derive(Debug)]
pub(crate) struct AgentMessageCell {
    lines: Vec<Line<'static>>,
    is_first_line: bool,
}

impl AgentMessageCell {
    pub fn new(lines: Vec<Line<'static>>, is_first_line: bool) -> Self {
        Self {
            lines,
            is_first_line,
        }
    }
}

impl ChatCell for AgentMessageCell {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn as_any_ref(&self) -> &dyn std::any::Any {
        self
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let theme = crate::tui::theme::current();
        let gutter: Line<'static> = Line::from(Span::styled(
            "┃ ",
            Style::default().fg(theme.accent).bold(),
        ));
        adaptive_wrap_lines(
            &self.lines,
            RtOptions::new(width as usize)
                .initial_indent(gutter.clone())
                .subsequent_indent(gutter),
        )
    }

    fn is_stream_continuation(&self) -> bool {
        !self.is_first_line
    }
}
