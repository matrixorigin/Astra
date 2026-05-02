use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};
use tokio::sync::oneshot;

use crate::chat_stream::ApprovalResponse;
use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};

struct ApprovalOption {
    key: char,
    label: &'static str,
    response: ApprovalResponse,
}

const OPTIONS: &[ApprovalOption] = &[
    ApprovalOption { key: 'y', label: "Yes, allow once", response: ApprovalResponse::AllowOnce },
    ApprovalOption { key: 'n', label: "No, deny", response: ApprovalResponse::Deny },
    ApprovalOption { key: 'a', label: "Always allow this tool", response: ApprovalResponse::AlwaysAllow },
    ApprovalOption { key: '!', label: "Auto-run session (allow all)", response: ApprovalResponse::AutoRunSession },
    ApprovalOption { key: 's', label: "Skip tool", response: ApprovalResponse::Skip },
];

pub(crate) struct ApprovalOverlay {
    tool: String,
    header: String,
    detail: Option<String>,
    reason: String,
    selected: usize,
    response_tx: Option<oneshot::Sender<ApprovalResponse>>,
    completed: bool,
}

impl ApprovalOverlay {
    pub fn new(
        tool: String,
        header: String,
        detail: Option<String>,
        reason: String,
        response_tx: oneshot::Sender<ApprovalResponse>,
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

    fn respond(&mut self, response: ApprovalResponse) {
        if let Some(tx) = self.response_tx.take() {
            let _ = tx.send(response);
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
        let sel_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);

        let mut y = area.y;

        // Header
        if y < area.bottom() {
            Widget::render(
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled("⚠ ", yellow),
                    Span::styled(&self.header, yellow),
                ]),
                Rect::new(area.x, y, area.width, 1), buf,
            );
            y += 1;
        }

        // Detail
        if let Some(ref detail) = self.detail {
            for dl in detail.lines().take(3) {
                if y >= area.bottom() { break; }
                Widget::render(
                    Line::from(vec![Span::raw("    "), Span::styled(dl.to_string(), dim)]),
                    Rect::new(area.x, y, area.width, 1), buf,
                );
                y += 1;
            }
        }

        // Reason
        if !self.reason.is_empty() && y < area.bottom() {
            Widget::render(
                Line::from(vec![Span::raw("    "), Span::styled(format!("Reason: {}", &self.reason), dim)]),
                Rect::new(area.x, y, area.width, 1), buf,
            );
            y += 1;
        }

        // Blank
        if y < area.bottom() { y += 1; }

        // Options
        for (i, opt) in OPTIONS.iter().enumerate() {
            if y >= area.bottom() { break; }
            let is_sel = i == self.selected;
            let row_style = if is_sel { sel_style } else { Style::default() };
            let key_style = if is_sel { sel_style } else { bold };

            Widget::render(
                Line::from(vec![
                    Span::styled(if is_sel { "  › " } else { "    " }, row_style),
                    Span::styled(format!("[{}] ", opt.key), key_style),
                    Span::styled(opt.label, row_style),
                ]),
                Rect::new(area.x, y, area.width, 1), buf,
            );
            y += 1;
        }

        // Hint
        if y < area.bottom() { y += 1; }
        if y < area.bottom() {
            Widget::render(
                Line::from(Span::styled(
                    "  Press key or Enter to confirm, Esc to deny", dim,
                )),
                Rect::new(area.x, y, area.width, 1), buf,
            );
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let mut h: u16 = 1; // header
        if let Some(ref detail) = self.detail {
            h += detail.lines().take(3).count() as u16;
        }
        if !self.reason.is_empty() { h += 1; }
        h += 1; // blank
        h += OPTIONS.len() as u16;
        h += 2; // blank + hint
        h
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            // Direct key shortcuts
            KeyCode::Char('y') | KeyCode::Char('Y') => self.respond(ApprovalResponse::AllowOnce),
            KeyCode::Char('n') | KeyCode::Char('N') => self.respond(ApprovalResponse::Deny),
            KeyCode::Char('a') | KeyCode::Char('A') => self.respond(ApprovalResponse::AlwaysAllow),
            KeyCode::Char('!') => self.respond(ApprovalResponse::AutoRunSession),
            KeyCode::Char('s') | KeyCode::Char('S') => self.respond(ApprovalResponse::Skip),
            // Arrow navigation
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected > 0 { self.selected -= 1; }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < OPTIONS.len() { self.selected += 1; }
            }
            KeyCode::Enter => {
                self.respond(OPTIONS[self.selected].response);
            }
            KeyCode::Esc => self.respond(ApprovalResponse::Deny),
            _ => {}
        }
    }

    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        None
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.respond(ApprovalResponse::Deny);
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
