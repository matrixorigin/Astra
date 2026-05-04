use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};

const MAX_VISIBLE: usize = 14;

/// A read-only scrollable text view for displaying command output inline.
pub(crate) struct InfoView {
    title: String,
    lines: Vec<Line<'static>>,
    scroll: usize,
    completed: bool,
    reopen: Option<String>,
}

impl InfoView {
    pub fn new(title: String, lines: Vec<Line<'static>>) -> Self {
        Self {
            title,
            lines,
            scroll: 0,
            completed: false,
            reopen: None,
        }
    }

    pub fn with_reopen(mut self, parent: &str) -> Self {
        self.reopen = Some(parent.to_string());
        self
    }

    pub fn from_plain(title: &str, text: Vec<String>) -> Self {
        let dim = Style::default().fg(Color::DarkGray);
        let lines: Vec<Line<'static>> = text
            .into_iter()
            .map(|s| Line::from(Span::styled(s, dim)))
            .collect();
        Self::new(title.to_string(), lines)
    }

    pub fn from_key_value(title: &str, pairs: Vec<(&str, String)>) -> Self {
        let dim = Style::default().fg(Color::DarkGray);
        let val_style = Style::default();
        let lines: Vec<Line<'static>> = pairs
            .into_iter()
            .map(|(key, val)| {
                Line::from(vec![
                    Span::styled(format!("  {:<16}", format!("{key}:")), dim),
                    Span::styled(val, val_style),
                ])
            })
            .collect();
        Self::new(title.to_string(), lines)
    }

    fn visible_count(&self) -> usize {
        self.lines.len().min(MAX_VISIBLE)
    }
}

impl BottomPaneView for InfoView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 10 || area.height < 3 {
            return;
        }

        let dim = Style::default().fg(Color::DarkGray);
        let title_style = Style::default().add_modifier(Modifier::BOLD);
        let mut y = area.y;

        // Title
        if y < area.bottom() {
            let line = Line::from(Span::styled(format!("  {}", &self.title), title_style));
            Widget::render(line, Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }

        // Blank
        if y < area.bottom() {
            y += 1;
        }

        // Content
        let visible_end = (self.scroll + self.visible_count()).min(self.lines.len());
        for i in self.scroll..visible_end {
            if y >= area.bottom() {
                break;
            }
            Widget::render(
                self.lines[i].clone(),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y += 1;
        }

        // Scroll indicator if needed
        if self.lines.len() > MAX_VISIBLE && y < area.bottom() {
            let pos = self.scroll + 1;
            let total = self.lines.len();
            let indicator = Line::from(Span::styled(
                format!("  ({pos}–{visible_end} of {total})"),
                dim,
            ));
            Widget::render(indicator, Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }

        // Hint
        if y < area.bottom() {
            y += 1;
        }
        if y < area.bottom() {
            let hint = if self.lines.len() > MAX_VISIBLE {
                "  ↑/↓ scroll  Esc close"
            } else {
                "  Esc close"
            };
            Widget::render(
                Line::from(Span::styled(hint, dim)),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let title_h = 2; // title + blank
        let content_h = self.visible_count() as u16;
        let scroll_h = if self.lines.len() > MAX_VISIBLE { 1 } else { 0 };
        let hint_h = 2; // blank + hint
        title_h + content_h + scroll_h + hint_h
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if self.scroll + MAX_VISIBLE < self.lines.len() => {
                self.scroll += 1;
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(MAX_VISIBLE);
            }
            KeyCode::PageDown => {
                self.scroll =
                    (self.scroll + MAX_VISIBLE).min(self.lines.len().saturating_sub(MAX_VISIBLE));
            }
            KeyCode::Esc | KeyCode::Enter => {
                self.completed = true;
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
                result: None,
                reopen: self.reopen.clone(),
            })
        } else {
            None
        }
    }
}
