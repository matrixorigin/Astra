//! A lightweight "busy" overlay used while a slash handler runs
//! blocking work (SQL query, session restore, git invocation). The
//! BottomPaneView surface renders a shimmer-animated line so users
//! can see the turn hasn't frozen.
//!
//! Usage shape:
//! ```ignore
//! ctx.bottom_pane.push_view(Box::new(BusyView::new("Running SQL…")));
//! // …await real work…
//! ctx.bottom_pane.pop_view();
//! ```
//!
//! The handler is responsible for popping before pushing the real
//! result view so the user sees a clean transition.

#![allow(dead_code)]

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};
use crate::tui::shimmer::shimmer_spans;

pub(crate) struct BusyView {
    title: String,
    message: String,
    cancelled: bool,
}

impl BusyView {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            title: " Working ".to_string(),
            message: message.into(),
            cancelled: false,
        }
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }
}

impl BottomPaneView for BusyView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Line::from(Span::styled(
                self.title.clone(),
                Style::default().fg(Color::Cyan),
            )));
        let inner = outer.inner(area);
        outer.render(area, buf);

        // A shimmer-animated "⏳ message…" line.
        let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
        spans.push(Span::styled("⏳ ", Style::default().fg(Color::Yellow)));
        spans.extend(shimmer_spans(&self.message));
        let hint = Line::from(Span::styled(
            "  (Esc to cancel)".to_string(),
            Style::default().fg(Color::DarkGray),
        ));
        Paragraph::new(vec![Line::from(spans), Line::default(), hint]).render(inner, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        // border(2) + spinner line + blank + hint = 5
        5
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if matches!(key.code, KeyCode::Esc) {
            self.cancelled = true;
        }
    }

    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        None
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.cancelled = true;
        CancellationEvent::Consumed
    }

    fn is_complete(&self) -> bool {
        self.cancelled
    }

    fn completion(&self) -> Option<ViewCompletion> {
        if self.cancelled {
            Some(ViewCompletion {
                result: None,
                reopen: None,
            })
        } else {
            None
        }
    }

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        true
    }

    fn hint_keys(&self) -> Option<String> {
        None // hint already baked into the render
    }

    fn reserve_status_footer(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    struct W<'a>(&'a BusyView);
    impl ratatui::widgets::Widget for W<'_> {
        fn render(self, area: Rect, buf: &mut Buffer) {
            self.0.render(area, buf);
        }
    }

    #[test]
    fn renders_title_and_message() {
        let v = BusyView::new("Running SQL…");
        let buf = draw_widget(W(&v), 40, 5);
        let s = buffer_to_string(&buf);
        assert!(s.contains("Working"));
        assert!(s.contains("Running"));
        assert!(s.contains("Esc"));
    }

    #[test]
    fn custom_title_sticks() {
        let v = BusyView::new("hi").with_title(" Resume ");
        let buf = draw_widget(W(&v), 40, 5);
        let s = buffer_to_string(&buf);
        assert!(s.contains("Resume"));
    }

    #[test]
    fn esc_marks_cancel() {
        let mut v = BusyView::new("hi");
        v.handle_key(KeyEvent::new(
            KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(v.is_complete());
        assert!(v.completion().unwrap().result.is_none());
    }
}
