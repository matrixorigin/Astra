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
    /// True while the cell is still receiving stream tokens. Drives the
    /// trailing cursor block in `display_lines` so users can see that
    /// more output is coming. Default `true` because every chat cell
    /// starts as live; callers flip it to `false` via `finalize()` the
    /// moment the stream closes.
    streaming: bool,
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
            streaming: true,
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
            // `from_source` is used to reconstruct finalized cells
            // (e.g. when replaying history), so they're not streaming.
            streaming: false,
        }
    }

    /// Mark this cell's stream as complete — hides the trailing cursor.
    pub fn finalize(&mut self) {
        self.streaming = false;
    }

    /// Whether a mid-stream cursor should render.
    pub fn is_streaming(&self) -> bool {
        self.streaming && !self.rendered_lines.is_empty()
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

    #[cfg(test)]
    pub(crate) fn with_streaming(mut self, streaming: bool) -> Self {
        self.streaming = streaming;
        self
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
            let last_idx = self.rendered_lines.len().saturating_sub(1);
            let show_cursor = self.is_streaming();
            for (i, line) in self.rendered_lines.iter().enumerate() {
                let prefix = if i == 0 {
                    Span::styled("• ", Style::default().dim())
                } else {
                    Span::raw("  ")
                };
                let mut spans = vec![prefix];
                spans.extend(line.spans.iter().cloned());
                // Append a blinking block cursor to the final rendered
                // line while tokens are still streaming — gives users a
                // clear "more is coming" cue without animating the text
                // itself (which the terminal cursor blink handles).
                if show_cursor && i == last_idx {
                    spans.push(Span::styled(
                        "▎",
                        Style::default()
                            .add_modifier(ratatui::style::Modifier::SLOW_BLINK)
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    ));
                }
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

#[cfg(test)]
mod streaming_cursor_tests {
    use super::*;

    fn line_text(l: &Line<'_>) -> String {
        l.spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn streaming_cell_appends_cursor_to_last_line() {
        let lines = vec![
            Line::from(Span::raw("first")),
            Line::from(Span::raw("last")),
        ];
        let cell = AssistantChatCell::from_rendered(lines);
        assert!(cell.is_streaming(), "new from_rendered should be streaming");

        let out = cell.display_lines(80);
        assert_eq!(out.len(), 2);
        assert!(
            !line_text(&out[0]).contains('▎'),
            "cursor should only be on the last line; first={:?}",
            line_text(&out[0])
        );
        assert!(
            line_text(&out[1]).ends_with('▎'),
            "last line should end with the streaming cursor; got {:?}",
            line_text(&out[1])
        );
    }

    #[test]
    fn finalized_cell_has_no_cursor() {
        let lines = vec![Line::from(Span::raw("done"))];
        let mut cell = AssistantChatCell::from_rendered(lines);
        cell.finalize();
        assert!(!cell.is_streaming());

        let out = cell.display_lines(80);
        assert_eq!(out.len(), 1);
        assert!(
            !line_text(&out[0]).contains('▎'),
            "finalized cell must not render a cursor; got {:?}",
            line_text(&out[0])
        );
    }

    #[test]
    fn from_source_is_not_streaming() {
        // History replay — should render as a static cell, no cursor.
        let cell = AssistantChatCell::from_source("hello world".into(), 80);
        assert!(!cell.is_streaming());
        let out = cell.display_lines(80);
        assert!(
            out.iter().all(|l| !line_text(l).contains('▎')),
            "from_source must never render a cursor"
        );
    }

    #[test]
    fn empty_streaming_cell_shows_working_not_cursor() {
        // No rendered lines yet + thinking → "Working" shimmer, no cursor.
        let mut cell = AssistantChatCell::from_rendered(vec![]);
        cell.start_thinking();
        let out = cell.display_lines(80);
        assert_eq!(out.len(), 1);
        assert!(!line_text(&out[0]).contains('▎'));
        assert!(line_text(&out[0]).contains("Working"));
    }
}
