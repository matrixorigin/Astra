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
    pub cwd: Option<String>,
    pub is_turn_active: bool,
}

impl Footer {
    pub fn new() -> Self {
        let cwd = std::env::current_dir().ok().map(|p| {
            let home = dirs::home_dir();
            match home {
                Some(h) if p.starts_with(&h) => {
                    format!("~/{}", p.strip_prefix(&h).unwrap_or(&p).display())
                }
                _ => p.display().to_string(),
            }
        });
        Self {
            model: None,
            session_id: None,
            token_usage: None,
            cwd,
            is_turn_active: false,
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let dim = Style::default().fg(Color::DarkGray);
        let sep = Span::styled(" · ", dim);

        // Build right side: model · dir · tokens
        let mut right_parts: Vec<Span> = Vec::new();

        if let Some(ref model) = self.model {
            right_parts.push(Span::styled(model.clone(), dim));
        }

        if let Some(ref cwd) = self.cwd {
            if !right_parts.is_empty() {
                right_parts.push(sep.clone());
            }
            let display = if cwd.len() > 25 {
                format!("…{}", &cwd[cwd.len() - 24..])
            } else {
                cwd.clone()
            };
            right_parts.push(Span::styled(display, dim));
        }

        if let Some(ref usage) = self.token_usage {
            if !right_parts.is_empty() {
                right_parts.push(sep.clone());
            }
            right_parts.push(Span::styled(usage.clone(), dim));
        }

        // Build left side
        let mut left_parts: Vec<Span> = Vec::new();
        left_parts.push(Span::raw("  ")); // 2-space indent
        if self.is_turn_active {
            left_parts.push(Span::styled("⏹ interrupt", dim));
        } else {
            left_parts.push(Span::styled("? for shortcuts", dim));
        }

        // Compose: left ... padding ... right
        let left_w: usize = left_parts.iter().map(|s| s.content.len()).sum();
        let right_w: usize = right_parts.iter().map(|s| s.content.len()).sum();
        let padding = (area.width as usize).saturating_sub(left_w + right_w + 2); // +2 for trailing margin

        let mut all_spans = left_parts;
        all_spans.push(Span::raw(" ".repeat(padding)));
        all_spans.extend(right_parts);

        Widget::render(Line::from(all_spans), area, buf);
    }
}
