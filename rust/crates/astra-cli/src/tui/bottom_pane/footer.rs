use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Widget,
};

pub(crate) struct Footer {
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub token_usage: Option<String>,
    pub is_turn_active: bool,
}

impl Footer {
    pub fn new() -> Self {
        Self {
            model: None,
            session_id: None,
            token_usage: None,
            is_turn_active: false,
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let dim = Style::default().fg(Color::DarkGray);

        let mut left_items: Vec<Span> = Vec::new();
        left_items.push(Span::raw("  ")); // 2-space indent like Codex FOOTER_INDENT_COLS
        if self.is_turn_active {
            left_items.push(Span::styled("⏹ interrupt", dim));
        } else {
            left_items.push(Span::styled("? for shortcuts", dim));
        }

        let mut right_items: Vec<Span> = Vec::new();
        if let Some(ref usage) = self.token_usage {
            right_items.push(Span::styled(usage.clone(), dim));
            right_items.push(Span::styled(" · ", dim));
        }
        if let Some(ref model) = self.model {
            right_items.push(Span::styled(model.clone(), dim));
        }
        right_items.push(Span::raw("  ")); // trailing indent

        // Compose: left items ... padding ... right items
        let left = Line::from(left_items);
        let right = Line::from(right_items);

        let left_w: usize = left.spans.iter().map(|s| s.content.len()).sum();
        let right_w: usize = right.spans.iter().map(|s| s.content.len()).sum();
        let padding = (area.width as usize).saturating_sub(left_w + right_w);

        let mut all_spans = left.spans;
        all_spans.push(Span::raw(" ".repeat(padding)));
        all_spans.extend(right.spans);

        Widget::render(Line::from(all_spans), area, buf);
    }
}
