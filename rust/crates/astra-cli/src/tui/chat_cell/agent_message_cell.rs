use ratatui::style::Stylize;
use ratatui::text::Line;

use super::ChatCell;
use crate::tui::wrapping::{RtOptions, adaptive_wrap_lines};

/// A small streaming cell emitted per commit tick.
/// Matches Codex's AgentMessageCell: a few lines with `• ` or `  ` prefix,
/// wrapped to terminal width via adaptive_wrap_lines (URL-aware, span-preserving).
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
        adaptive_wrap_lines(
            &self.lines,
            RtOptions::new(width as usize)
                .initial_indent(if self.is_first_line {
                    "• ".dim().into()
                } else {
                    "  ".into()
                })
                .subsequent_indent("  ".into()),
        )
    }

    fn is_stream_continuation(&self) -> bool {
        !self.is_first_line
    }
}
