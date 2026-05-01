use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};

pub(crate) struct SelectionItem {
    pub name: String,
    pub description: Option<String>,
    pub is_current: bool,
}

pub(crate) struct ListSelectionView {
    items: Vec<SelectionItem>,
    header: Option<String>,
    footer_hint: Option<String>,
    selected: usize,
    filter: String,
    completed: bool,
    accepted_name: Option<String>,
}

impl ListSelectionView {
    pub fn new(items: Vec<SelectionItem>, header: Option<String>) -> Self {
        let initial_sel = items.iter().position(|i| i.is_current).unwrap_or(0);
        Self {
            items,
            header,
            footer_hint: Some("Press enter to confirm or esc to go back".into()),
            selected: initial_sel,
            filter: String::new(),
            completed: false,
            accepted_name: None,
        }
    }

    pub fn accepted_name(&self) -> Option<&str> {
        self.accepted_name.as_deref()
    }

    fn filtered_items(&self) -> Vec<(usize, &SelectionItem)> {
        if self.filter.is_empty() {
            self.items.iter().enumerate().collect()
        } else {
            let f = self.filter.to_lowercase();
            self.items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.name.to_lowercase().contains(&f))
                .collect()
        }
    }

    fn accept(&mut self) {
        let filtered = self.filtered_items();
        if let Some((_, item)) = filtered.get(self.selected) {
            self.accepted_name = Some(item.name.clone());
            self.completed = true;
        }
    }
}

const MAX_VISIBLE: usize = 12;

impl BottomPaneView for ListSelectionView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let dim = Style::default().fg(Color::DarkGray);
        let mut y = area.y;

        // Header
        if let Some(ref hdr) = self.header {
            if y < area.bottom() {
                let line = Line::from(Span::styled(format!("  {hdr}"), dim));
                Widget::render(line, Rect::new(area.x, y, area.width, 1), buf);
                y += 1;
            }
        }

        // Items
        let filtered = self.filtered_items();
        let visible_start = if self.selected >= MAX_VISIBLE {
            self.selected - MAX_VISIBLE + 1
        } else {
            0
        };
        let visible_end = (visible_start + MAX_VISIBLE).min(filtered.len());

        for (vi, &(_, item)) in filtered[visible_start..visible_end].iter().enumerate() {
            if y >= area.bottom() { break; }
            let idx = visible_start + vi;
            let is_sel = idx == self.selected;

            let marker = if is_sel { "› " } else { "  " };
            let num = format!("{}. ", idx + 1);

            let name_style = if is_sel {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else if item.is_current {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };

            let current_tag = if item.is_current { " (current)" } else { "" };

            let mut spans = vec![
                Span::raw("  "),
                Span::styled(marker, name_style),
                Span::styled(num, dim),
                Span::styled(format!("{}{}", &item.name, current_tag), name_style),
            ];

            if let Some(ref desc) = item.description {
                let budget = (area.width as usize).saturating_sub(8 + item.name.len() + current_tag.len());
                if budget > 5 {
                    let d: String = desc.chars().take(budget).collect();
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(d, dim));
                }
            }

            Widget::render(Line::from(spans), Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }

        if filtered.is_empty() && y < area.bottom() {
            let line = Line::from(Span::styled("  no matches", dim));
            Widget::render(line, Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }

        // Blank line + footer hint
        if y < area.bottom() { y += 1; }
        if let Some(ref hint) = self.footer_hint {
            if y < area.bottom() {
                let line = Line::from(Span::styled(format!("  {hint}"), dim));
                Widget::render(line, Rect::new(area.x, y, area.width, 1), buf);
            }
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let header_h = if self.header.is_some() { 1 } else { 0 };
        let filtered_count = self.filtered_items().len().max(1); // at least 1 for "no matches"
        let items_h = filtered_count.min(MAX_VISIBLE) as u16;
        let footer_h = if self.footer_hint.is_some() { 2 } else { 0 }; // blank + hint
        header_h + items_h + footer_h
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let filtered_len = self.filtered_items().len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if filtered_len > 0 {
                    self.selected = if self.selected == 0 {
                        filtered_len - 1
                    } else {
                        self.selected - 1
                    };
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if filtered_len > 0 {
                    self.selected = (self.selected + 1) % filtered_len;
                }
            }
            KeyCode::Enter => self.accept(),
            KeyCode::Esc => {
                self.completed = true;
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.selected = 0;
            }
            _ => {}
        }
    }

    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        None
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.completed = true;
        CancellationEvent::Consumed
    }

    fn is_complete(&self) -> bool {
        self.completed
    }

    fn completion(&self) -> Option<ViewCompletion> {
        if self.completed {
            Some(ViewCompletion {
                result: self.accepted_name.clone(),
            })
        } else {
            None
        }
    }
}
