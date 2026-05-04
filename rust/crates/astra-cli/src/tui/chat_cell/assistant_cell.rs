use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use std::time::Instant;

use super::ChatCell;
use crate::tui::markdown_render::render_markdown_text_with_width;
use crate::tui::render::line_utils::line_to_static;

#[derive(Debug)]
pub(crate) struct AssistantChatCell {
    rendered_lines: Vec<Line<'static>>,
    source: Option<String>,
    thinking_chunks: Vec<String>,
    thinking_started_at: Option<Instant>,
    thinking_finished: bool,
    is_first_chunk: bool,
}

impl AssistantChatCell {
    pub fn from_rendered(lines: Vec<Line<'static>>) -> Self {
        Self {
            rendered_lines: lines,
            source: None,
            thinking_chunks: Vec::new(),
            thinking_started_at: None,
            thinking_finished: false,
            is_first_chunk: true,
        }
    }

    pub fn from_source(markdown: String, width: u16) -> Self {
        let text = render_markdown_text_with_width(&markdown, Some(width as usize));
        let lines: Vec<Line<'static>> = text.lines.iter().map(line_to_static).collect();
        Self {
            rendered_lines: lines,
            source: Some(markdown),
            thinking_chunks: Vec::new(),
            thinking_started_at: None,
            thinking_finished: false,
            is_first_chunk: false,
        }
    }

    pub fn update_rendered_lines(&mut self, lines: Vec<Line<'static>>) {
        self.rendered_lines = lines;
        self.is_first_chunk = false;
    }

    pub fn set_source(&mut self, source: String, width: u16) {
        let text = render_markdown_text_with_width(&source, Some(width as usize));
        self.rendered_lines = text.lines.iter().map(line_to_static).collect();
        self.source = Some(source);
    }

    pub fn start_thinking(&mut self) {
        if self.thinking_started_at.is_none() {
            self.thinking_started_at = Some(Instant::now());
        }
    }

    pub fn push_thinking_chunk(&mut self, text: &str) {
        if self.thinking_started_at.is_none() {
            self.thinking_started_at = Some(Instant::now());
        }
        self.thinking_chunks.push(text.to_string());
    }

    pub fn finish_thinking(&mut self) {
        self.thinking_finished = true;
    }

    pub fn is_thinking(&self) -> bool {
        self.thinking_started_at.is_some() && !self.thinking_finished
    }

    #[allow(dead_code)]
    pub fn reflow(&mut self, width: u16) {
        if let Some(ref source) = self.source {
            let text = render_markdown_text_with_width(source, Some(width as usize));
            self.rendered_lines = text.lines.iter().map(line_to_static).collect();
        }
    }

    fn thinking_elapsed_str(&self) -> String {
        match self.thinking_started_at {
            Some(t) => {
                let secs = t.elapsed().as_secs_f32();
                if secs < 1.0 {
                    format!("{:.0}ms", secs * 1000.0)
                } else {
                    format!("{secs:.1}s")
                }
            }
            None => String::new(),
        }
    }
}

impl ChatCell for AssistantChatCell {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn as_any_ref(&self) -> &dyn std::any::Any {
        self
    }

    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();

        // Main content with • prefix on first line (Codex style)
        if !self.rendered_lines.is_empty() {
            for (i, line) in self.rendered_lines.iter().enumerate() {
                let prefix = if i == 0 {
                    Span::styled("• ", Style::default().dim())
                } else {
                    Span::raw("  ")
                };
                let mut spans = vec![prefix];
                spans.extend(line.spans.iter().cloned());
                lines.push(Line::from(spans));
            }
        } else if self.is_thinking() {
            // Thinking in progress, no content yet → shimmer "Working" like Codex
            let working_text = format!(
                "Working ({} • esc to interrupt)",
                self.thinking_elapsed_str()
            );
            let mut spans = vec![Span::styled("• ", Style::default().dim())];
            spans.extend(crate::tui::shimmer::shimmer_spans(&working_text));
            lines.push(Line::from(spans));
        }
        // When thinking is done but no content: show nothing (will be filled by tokens)
        // Thinking text is NOT shown in main viewport (Codex: transcript_only)

        lines
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();

        // Include thinking content (hidden in display_lines)
        if !self.thinking_chunks.is_empty() {
            let dim_italic = Style::default()
                .dim()
                .add_modifier(ratatui::style::Modifier::ITALIC);
            let elapsed = self.thinking_elapsed_str();
            lines.push(Line::from(Span::styled(
                format!("  │ Thinking ({elapsed})"),
                dim_italic,
            )));
            let full = self.thinking_chunks.join("");
            for text_line in full.lines().take(20) {
                let preview: String = text_line.chars().take(width as usize - 6).collect();
                lines.push(Line::from(Span::styled(
                    format!("  │ {preview}"),
                    dim_italic,
                )));
            }
            if full.lines().count() > 20 {
                lines.push(Line::from(Span::styled(
                    format!("  │ … +{} more lines", full.lines().count() - 20),
                    dim_italic,
                )));
            }
            lines.push(Line::default());
        }

        // Then the normal display content
        lines.extend(self.display_lines(width));
        lines
    }
}
