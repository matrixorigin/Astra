use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Widget,
};

use crate::command_registry::{self, CommandMeta};

const MAX_VISIBLE: usize = 10;

fn is_tui_native(name: &str) -> bool {
    matches!(name,
        "/help" | "/commands" | "/model" | "/stats" | "/skill" | "/skills"
        | "/copy" | "/version" | "/whoami" | "/history"
        | "/instructions" | "/exit" | "/quit"
    )
}

pub(crate) struct SlashPopup {
    filter: String,
    matches: Vec<&'static CommandMeta>,
    selected: usize,
}

impl SlashPopup {
    pub fn new() -> Self {
        let mut popup = Self {
            filter: String::new(),
            matches: Vec::new(),
            selected: 0,
        };
        popup.update_matches();
        popup
    }

    pub fn set_filter(&mut self, text: &str) {
        let first_line = text.lines().next().unwrap_or("");
        self.filter = first_line
            .strip_prefix('/')
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_lowercase();
        self.update_matches();
        if self.selected >= self.matches.len() {
            self.selected = 0;
        }
    }

    fn update_matches(&mut self) {
        self.matches = command_registry::COMMANDS
            .iter()
            .filter(|m| !m.is_alias && !m.name.contains(' '))
            .filter(|m| {
                if self.filter.is_empty() {
                    true
                } else {
                    let name = m.name.trim_start_matches('/');
                    name.starts_with(&self.filter)
                }
            })
            .collect();
    }

    pub fn move_up(&mut self) {
        if !self.matches.is_empty() {
            self.selected = if self.selected == 0 {
                self.matches.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    pub fn move_down(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    pub fn selected_command(&self) -> Option<&'static str> {
        self.matches.get(self.selected).map(|m| m.name)
    }

    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }

    pub fn height(&self) -> u16 {
        self.matches.len().min(MAX_VISIBLE) as u16
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 || self.matches.is_empty() {
            return;
        }

        let dim = Style::default().fg(Color::DarkGray);

        let visible_start = if self.selected >= MAX_VISIBLE {
            self.selected - MAX_VISIBLE + 1
        } else {
            0
        };
        let visible_end = (visible_start + MAX_VISIBLE).min(self.matches.len());

        for (vi, i) in (visible_start..visible_end).enumerate() {
            let row = area.y + vi as u16;
            if row >= area.bottom() { break; }

            let meta = self.matches[i];
            let is_sel = i == self.selected;
            let native = is_tui_native(meta.name);

            let marker = if native { "● " } else { "  " };
            let name_w = 16;
            let padded_name = format!("{:<width$}", meta.name, width = name_w);
            let desc_budget = (area.width as usize).saturating_sub(4 + name_w);
            let desc: String = meta.description.chars().take(desc_budget).collect();

            let line = if is_sel {
                let sel = Style::default().fg(Color::Cyan).add_modifier(ratatui::style::Modifier::BOLD);
                Line::from(vec![
                    Span::styled(marker, sel),
                    Span::styled(padded_name, sel),
                    Span::styled(desc, sel),
                ])
            } else {
                let marker_style = if native {
                    Style::default().fg(Color::Green)
                } else {
                    dim
                };
                Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::raw(padded_name),
                    Span::styled(desc, dim),
                ])
            };
            Widget::render(line, Rect::new(area.x, row, area.width, 1), buf);
        }
    }
}
