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
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

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
        let surface = crate::tui::style::composer_surface_style();
        let dim = Style::default().fg(Color::DarkGray);
        let title_style = Style::default()
            .fg(crate::tui::theme::current().accent)
            .add_modifier(Modifier::BOLD);
        for y in area.y..area.y + area.height {
            buf.set_string(area.x, y, " ".repeat(area.width as usize), surface);
        }

        if area.height >= 1 {
            let title = Line::from(vec![
                Span::raw("  "),
                Span::styled(self.title.trim().to_string(), title_style),
            ]);
            buf.set_line(area.x, area.y, &title, area.width);
        }
        if area.height >= 2 {
            let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
            spans.extend(shimmer_spans(&self.message));
            let body = Line::from(spans);
            buf.set_line(area.x, area.y + 1, &body, area.width);
        }
        if area.height >= 4 {
            let hint = Line::from(vec![
                Span::raw("  "),
                Span::styled("Esc to cancel".to_string(), dim),
            ]);
            buf.set_line(area.x, area.y + 3, &hint, area.width);
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        4
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
    use super::super::view::BottomPaneView;
    use super::BusyView;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    struct W<'a>(&'a BusyView);
    impl ratatui::widgets::Widget for W<'_> {
        fn render(self, area: Rect, buf: &mut Buffer) {
            self.0.render(area, buf);
        }
    }

    #[test]
    fn renders_title_and_message() {
        let v = BusyView::new("Running SQL…");
        let buf = draw_widget(W(&v), 40, 4);
        let s = buffer_to_string(&buf);
        assert!(s.contains("Working"));
        assert!(s.contains("Running"));
        assert!(s.contains("Esc"));
        assert!(!s.contains("⏳"));
        assert!(!s.contains("┌"));
    }

    #[test]
    fn custom_title_sticks() {
        let v = BusyView::new("hi").with_title(" Resume ");
        let buf = draw_widget(W(&v), 40, 4);
        let s = buffer_to_string(&buf);
        assert!(s.contains("Resume"));
    }

    #[test]
    fn esc_marks_cancel() {
        let mut v = BusyView::new("hi");
        v.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(v.is_complete());
        assert!(v.completion().unwrap().result.is_none());
    }
}
