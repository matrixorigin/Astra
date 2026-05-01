use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};
use tokio::sync::oneshot;

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalChoice {
    AllowOnce,
    AlwaysAllow,
    AutoRunSession,
    Deny,
}

pub(crate) struct ApprovalOverlay {
    tool: String,
    header: String,
    detail: Option<String>,
    reason: String,
    selected: usize,
    response_tx: Option<oneshot::Sender<bool>>,
    completed: bool,
}

impl ApprovalOverlay {
    pub fn new(
        tool: String,
        header: String,
        detail: Option<String>,
        reason: String,
        response_tx: oneshot::Sender<bool>,
    ) -> Self {
        Self {
            tool,
            header,
            detail,
            reason,
            selected: 0,
            response_tx: Some(response_tx),
            completed: false,
        }
    }

    fn options() -> &'static [(&'static str, &'static str, bool)] {
        &[
            ("Y", "Yes, allow once", true),
            ("N", "No, deny", false),
        ]
    }

    fn respond(&mut self, allow: bool) {
        if let Some(tx) = self.response_tx.take() {
            let _ = tx.send(allow);
        }
        self.completed = true;
    }
}

impl BottomPaneView for ApprovalOverlay {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let dim = Style::default().fg(Color::DarkGray);
        let yellow = Style::default().fg(Color::Yellow);
        let bold = Style::default().add_modifier(Modifier::BOLD);

        let mut y = area.y;

        // Header: ⚠ Tool approval needed
        if y < area.bottom() {
            let line = Line::from(vec![
                Span::raw("  "),
                Span::styled("⚠ ", yellow),
                Span::styled(&self.header, yellow),
            ]);
            Widget::render(line, Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }

        // Detail (e.g. command preview)
        if let Some(ref detail) = self.detail {
            for dl in detail.lines().take(3) {
                if y >= area.bottom() { break; }
                let line = Line::from(vec![
                    Span::raw("    "),
                    Span::styled(dl.to_string(), dim),
                ]);
                Widget::render(line, Rect::new(area.x, y, area.width, 1), buf);
                y += 1;
            }
        }

        // Reason
        if !self.reason.is_empty() && y < area.bottom() {
            let line = Line::from(vec![
                Span::raw("    "),
                Span::styled(format!("Reason: {}", &self.reason), dim),
            ]);
            Widget::render(line, Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }

        // Blank separator
        if y < area.bottom() {
            y += 1;
        }

        // Options
        let options = Self::options();
        for (i, (key, label, _)) in options.iter().enumerate() {
            if y >= area.bottom() { break; }
            let is_selected = i == self.selected;
            let key_style = if is_selected {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                bold
            };
            let label_style = if is_selected {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };
            let indicator = if is_selected { "▸ " } else { "  " };
            let line = Line::from(vec![
                Span::raw("  "),
                Span::styled(indicator, label_style),
                Span::styled(format!("[{key}] "), key_style),
                Span::styled(*label, label_style),
            ]);
            Widget::render(line, Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let mut h: u16 = 1; // header
        if let Some(ref detail) = self.detail {
            h += detail.lines().take(3).count() as u16;
        }
        if !self.reason.is_empty() {
            h += 1;
        }
        h += 1; // blank separator
        h += Self::options().len() as u16;
        h
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => self.respond(true),
            KeyCode::Char('n') | KeyCode::Char('N') => self.respond(false),
            KeyCode::Enter => {
                let allow = Self::options()[self.selected].2;
                self.respond(allow);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < Self::options().len() {
                    self.selected += 1;
                }
            }
            KeyCode::Esc => self.respond(false),
            _ => {}
        }
    }

    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        None
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.respond(false);
        CancellationEvent::Consumed
    }

    fn is_complete(&self) -> bool {
        self.completed
    }

    fn completion(&self) -> Option<ViewCompletion> {
        if self.completed {
            Some(ViewCompletion { result: None })
        } else {
            None
        }
    }
}
