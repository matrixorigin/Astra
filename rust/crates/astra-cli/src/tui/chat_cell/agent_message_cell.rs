use ratatui::style::Style;
use ratatui::text::{Line, Span};
use super::ChatCell;

/// A small streaming cell emitted per commit tick.
/// Matches Codex's AgentMessageCell: a few lines with `• ` or `  ` prefix,
/// wrapped to terminal width.
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

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let prefix_w: u16 = 2; // "• " or "  "
        let content_w = (width as usize).saturating_sub(prefix_w as usize);
        let mut result = Vec::new();
        let mut is_first_output_line = true;

        for line in &self.lines {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();

            // Wrap long lines by character width
            let wrapped = wrap_text(&text, content_w);
            for wl in &wrapped {
                let prefix = if is_first_output_line && self.is_first_line {
                    Span::styled("• ", Style::default().add_modifier(ratatui::style::Modifier::DIM))
                } else {
                    Span::raw("  ")
                };
                is_first_output_line = false;

                // Try to preserve original span styles for the first wrap line
                // For simplicity, use the line's merged style
                let style = line.style;
                result.push(Line::from(vec![
                    prefix,
                    Span::styled(wl.clone(), style),
                ]));
            }
            if wrapped.is_empty() {
                let prefix = if is_first_output_line && self.is_first_line {
                    Span::styled("• ", Style::default().add_modifier(ratatui::style::Modifier::DIM))
                } else {
                    Span::raw("  ")
                };
                is_first_output_line = false;
                result.push(Line::from(prefix));
            }
        }
        result
    }

    fn is_stream_continuation(&self) -> bool {
        !self.is_first_line
    }
}

fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_w = 0;

    for ch in text.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_w + cw > max_width && current_w > 0 {
            lines.push(std::mem::take(&mut current));
            current_w = 0;
        }
        current.push(ch);
        current_w += cw;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}
