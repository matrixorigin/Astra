use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};

const MAX_VISIBLE_LINES: usize = 16;

/// Full conversation transcript including thinking content and tool output.
pub(crate) struct TranscriptView {
    lines: Vec<Line<'static>>,
    scroll: usize,
    completed: bool,
}

impl TranscriptView {
    pub fn new(lines: Vec<Line<'static>>) -> Self {
        let scroll = lines.len().saturating_sub(MAX_VISIBLE_LINES);
        Self {
            lines,
            scroll,
            completed: false,
        }
    }
}

impl BottomPaneView for TranscriptView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 10 || area.height < 3 {
            return;
        }

        let dim = Style::default().fg(Color::DarkGray);
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let mut y = area.y;

        // Title
        if y < area.bottom() {
            Widget::render(
                Line::from(vec![
                    Span::styled("  Transcript", bold),
                    Span::styled(format!("  ({} lines)", self.lines.len()), dim),
                ]),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y += 1;
        }

        // Content
        let visible_end = (self.scroll + MAX_VISIBLE_LINES).min(self.lines.len());
        for i in self.scroll..visible_end {
            if y >= area.bottom() { break; }
            Widget::render(
                self.lines[i].clone(),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y += 1;
        }

        // Scroll indicator
        if self.lines.len() > MAX_VISIBLE_LINES && y < area.bottom() {
            Widget::render(
                Line::from(Span::styled(
                    format!("  ({}-{} of {})", self.scroll + 1, visible_end, self.lines.len()),
                    dim,
                )),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y += 1;
        }

        // Hint
        if y < area.bottom() { y += 1; }
        if y < area.bottom() {
            Widget::render(
                Line::from(Span::styled(
                    "  ↑/↓ scroll  PgUp/PgDn page  Home/End  Esc close",
                    dim,
                )),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let title_h = 1;
        let content_h = self.lines.len().min(MAX_VISIBLE_LINES) as u16;
        let scroll_h = if self.lines.len() > MAX_VISIBLE_LINES { 1 } else { 0 };
        let hint_h = 2;
        title_h + content_h + scroll_h + hint_h
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let max_scroll = self.lines.len().saturating_sub(MAX_VISIBLE_LINES);
        match key.code {
            KeyCode::Esc => self.completed = true,
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = (self.scroll + 1).min(max_scroll);
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(MAX_VISIBLE_LINES);
            }
            KeyCode::PageDown => {
                self.scroll = (self.scroll + MAX_VISIBLE_LINES).min(max_scroll);
            }
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = max_scroll,
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
            Some(ViewCompletion { result: None, reopen: None })
        } else {
            None
        }
    }
}
