use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::ChatCell;
use crate::tui::markdown_render::render_markdown_text_with_width;
use crate::tui::render::line_utils::line_to_static;

#[derive(Debug)]
pub(crate) struct AssistantChatCell {
    rendered_lines: Vec<Line<'static>>,
    source: Option<String>,
}

impl AssistantChatCell {
    pub fn from_rendered(lines: Vec<Line<'static>>) -> Self {
        Self {
            rendered_lines: lines,
            source: None,
        }
    }

    pub fn from_source(markdown: String, width: u16) -> Self {
        let text = render_markdown_text_with_width(&markdown, Some(width as usize));
        let lines: Vec<Line<'static>> = text.lines.iter().map(line_to_static).collect();
        Self {
            rendered_lines: lines,
            source: Some(markdown),
        }
    }

    pub fn reflow(&mut self, width: u16) {
        if let Some(ref source) = self.source {
            let text = render_markdown_text_with_width(source, Some(width as usize));
            self.rendered_lines = text.lines.iter().map(line_to_static).collect();
        }
    }
}

impl ChatCell for AssistantChatCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::with_capacity(self.rendered_lines.len());
        for (i, line) in self.rendered_lines.iter().enumerate() {
            let prefix = if i == 0 {
                Span::styled("  ", Style::default())
            } else {
                Span::raw("  ")
            };
            let mut spans = vec![prefix];
            spans.extend(line.spans.iter().cloned());
            lines.push(Line::from(spans));
        }
        lines
    }
}
