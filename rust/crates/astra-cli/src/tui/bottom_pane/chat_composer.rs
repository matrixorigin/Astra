use crossterm::event::KeyEvent;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Widget},
};

use super::textarea::{TextArea, TextAreaAction};

#[derive(Debug)]
pub(crate) struct ChatComposer {
    textarea: TextArea,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: Option<String>,
    prompt_prefix: String,
}

impl ChatComposer {
    pub fn new() -> Self {
        Self {
            textarea: TextArea::new(),
            history: Vec::new(),
            history_index: None,
            draft: None,
            prompt_prefix: "› ".to_string(),
        }
    }

    pub fn text(&self) -> String {
        self.textarea.text().to_string()
    }

    pub fn is_empty(&self) -> bool {
        self.textarea.is_empty()
    }

    pub fn clear_and_submit(&mut self) -> String {
        let text = self.textarea.text().to_string();
        if !text.trim().is_empty() {
            self.history.push(text.clone());
        }
        self.textarea.clear();
        self.history_index = None;
        self.draft = None;
        text
    }

    pub fn clear_draft(&mut self) {
        self.textarea.clear();
        self.history_index = None;
        self.draft = None;
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        let border_overhead = 2; // top + bottom border
        let prefix_w = self.prompt_prefix.len() as u16;
        let inner_w = width.saturating_sub(border_overhead + prefix_w);
        self.textarea.desired_height(inner_w) + border_overhead
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ComposerAction {
        match self.textarea.handle_key(key) {
            TextAreaAction::Submit => {
                if self.textarea.is_empty() {
                    ComposerAction::Consumed
                } else {
                    ComposerAction::Submit
                }
            }
            TextAreaAction::Cancel => {
                if !self.is_empty() {
                    self.clear_draft();
                    ComposerAction::Consumed
                } else {
                    ComposerAction::Interrupt
                }
            }
            TextAreaAction::Quit => ComposerAction::Quit,
            TextAreaAction::HistoryPrev => {
                self.navigate_history_prev();
                ComposerAction::Consumed
            }
            TextAreaAction::HistoryNext => {
                self.navigate_history_next();
                ComposerAction::Consumed
            }
            TextAreaAction::Changed | TextAreaAction::Consumed => ComposerAction::Consumed,
            TextAreaAction::Unhandled => ComposerAction::Unhandled,
        }
    }

    fn navigate_history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.draft = Some(self.textarea.text().to_string());
                self.history_index = Some(self.history.len() - 1);
            }
            Some(0) => return,
            Some(i) => {
                self.history_index = Some(i - 1);
            }
        }
        if let Some(i) = self.history_index {
            self.textarea.set_text(&self.history[i]);
        }
    }

    fn navigate_history_next(&mut self) {
        match self.history_index {
            None => return,
            Some(i) if i + 1 >= self.history.len() => {
                self.history_index = None;
                if let Some(ref draft) = self.draft.take() {
                    self.textarea.set_text(draft);
                } else {
                    self.textarea.clear();
                }
            }
            Some(i) => {
                self.history_index = Some(i + 1);
                self.textarea.set_text(&self.history[i + 1]);
            }
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Input ")
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        block.render(area, buf);

        if inner.height == 0 || inner.width == 0 {
            return;
        }

        // Render prompt prefix on first line
        let prefix = Span::styled(&self.prompt_prefix, Style::default().cyan());
        let prefix_width = self.prompt_prefix.len() as u16;
        let prefix_area = Rect::new(inner.x, inner.y, prefix_width.min(inner.width), 1);
        Widget::render(Line::from(prefix), prefix_area, buf);

        // Textarea gets the remaining width
        let text_area = Rect::new(
            inner.x + prefix_width.min(inner.width),
            inner.y,
            inner.width.saturating_sub(prefix_width),
            inner.height,
        );

        if self.textarea.is_empty() {
            let placeholder = Span::styled("Type a message...", Style::default().dark_gray());
            Widget::render(Line::from(placeholder), text_area, buf);
        } else {
            self.textarea.render(text_area, buf);
        }
    }

    pub fn cursor_position(&self, area: Rect) -> Option<(u16, u16)> {
        let block = Block::default().borders(Borders::ALL);
        let inner = block.inner(area);
        let prefix_width = self.prompt_prefix.len() as u16;
        let text_area = Rect::new(
            inner.x + prefix_width.min(inner.width),
            inner.y,
            inner.width.saturating_sub(prefix_width),
            inner.height,
        );
        self.textarea.cursor_position(text_area)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposerAction {
    Submit,
    Interrupt,
    Quit,
    Consumed,
    Unhandled,
}
