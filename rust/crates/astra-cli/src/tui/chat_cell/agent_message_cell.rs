use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::ChatCell;

/// A small streaming cell emitted per commit tick.
/// Matches Codex's AgentMessageCell: a few lines with `• ` or `  ` prefix.
#[derive(Debug)]
pub(crate) struct AgentMessageCell {
    lines: Vec<Line<'static>>,
    is_first_line: bool,
}

impl AgentMessageCell {
    pub fn new(lines: Vec<Line<'static>>, is_first_line: bool) -> Self {
        Self { lines, is_first_line }
    }
}

impl ChatCell for AgentMessageCell {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }
    fn as_any_ref(&self) -> &dyn std::any::Any { self }

    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        self.lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let prefix = if i == 0 && self.is_first_line {
                    Span::styled("• ", Style::default().add_modifier(ratatui::style::Modifier::DIM))
                } else {
                    Span::raw("  ")
                };
                let mut spans = vec![prefix];
                spans.extend(line.spans.iter().cloned());
                Line::from(spans)
            })
            .collect()
    }

    fn is_stream_continuation(&self) -> bool {
        !self.is_first_line
    }
}
