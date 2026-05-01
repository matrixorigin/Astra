use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

use super::ChatCell;

#[derive(Debug)]
pub(crate) struct ThinkingChatCell {
    chunks: Vec<String>,
    started_at: Instant,
    finished: bool,
    collapsed: bool,
}

impl ThinkingChatCell {
    pub fn new() -> Self {
        Self {
            chunks: Vec::new(),
            started_at: Instant::now(),
            finished: false,
            collapsed: true,
        }
    }

    pub fn push_chunk(&mut self, text: &str) {
        self.chunks.push(text.to_string());
    }

    pub fn finish(&mut self) {
        self.finished = true;
    }

    #[allow(dead_code)]
    pub fn toggle_collapsed(&mut self) {
        self.collapsed = !self.collapsed;
    }

    fn elapsed_str(&self) -> String {
        let secs = self.started_at.elapsed().as_secs_f32();
        if secs < 1.0 {
            format!("{:.0}ms", secs * 1000.0)
        } else {
            format!("{secs:.1}s")
        }
    }

    fn full_text(&self) -> String {
        self.chunks.join("")
    }
}

impl ChatCell for ThinkingChatCell {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }
    fn as_any_ref(&self) -> &dyn std::any::Any { self }
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let border_style = Style::default().fg(Color::DarkGray);
        let text_style = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC);

        let status = if self.finished { "done" } else { "..." };
        let header = Line::from(vec![
            Span::styled("  │ ", border_style),
            Span::styled(
                format!("Thinking ({}) {status}", self.elapsed_str()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]);

        let mut lines = vec![header];

        if !self.collapsed && !self.chunks.is_empty() {
            let content = self.full_text();
            let max_width = (width as usize).saturating_sub(6);
            let max_lines = 6;
            let content_lines: Vec<&str> = content.lines().collect();
            let start = content_lines.len().saturating_sub(max_lines);
            for text_line in &content_lines[start..] {
                let truncated = truncate_by_width(text_line, max_width);
                lines.push(Line::from(vec![
                    Span::styled("  │ ", border_style),
                    Span::styled(truncated, text_style),
                ]));
            }
            if start > 0 {
                lines.insert(1, Line::from(vec![
                    Span::styled("  │ ", border_style),
                    Span::styled(
                        format!("… {start} earlier lines"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        }
        // collapsed: just the header line, no content preview

        lines
    }
}

fn truncate_by_width(s: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(s) <= max_width {
        return s.to_string();
    }
    let mut width = 0;
    let mut end = 0;
    for (i, c) in s.char_indices() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if width + cw + 1 > max_width {
            break;
        }
        width += cw;
        end = i + c.len_utf8();
    }
    format!("{}…", &s[..end])
}
