use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::Widget,
};

pub(crate) struct Footer {
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub token_usage: Option<String>,
    pub cwd: Option<String>,
}

impl Footer {
    pub fn new() -> Self {
        Self {
            model: None,
            session_id: None,
            token_usage: None,
            cwd: std::env::current_dir()
                .ok()
                .map(|p| {
                    let home = dirs::home_dir();
                    match home {
                        Some(h) if p.starts_with(&h) => {
                            format!("~/{}", p.strip_prefix(&h).unwrap_or(&p).display())
                        }
                        _ => p.display().to_string(),
                    }
                }),
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let mut items: Vec<Span> = Vec::new();

        if let Some(ref model) = self.model {
            items.push(Span::styled(model.clone(), Style::default().cyan()));
        }

        if let Some(ref usage) = self.token_usage {
            if !items.is_empty() {
                items.push(Span::raw(" │ "));
            }
            items.push(Span::raw(usage.clone()));
        }

        if let Some(ref cwd) = self.cwd {
            if !items.is_empty() {
                items.push(Span::raw(" │ "));
            }
            let display = if cwd.len() > 30 {
                format!("…{}", &cwd[cwd.len() - 29..])
            } else {
                cwd.clone()
            };
            items.push(Span::styled(display, Style::default().dim()));
        }

        items.push(Span::raw(" │ "));
        items.push(Span::styled("Ctrl+C", Style::default().yellow()));
        items.push(Span::raw(" quit"));

        let line = Line::from(items);
        Widget::render(line, area, buf);
    }
}
