//! BottomPaneView wrapper for the context-window breakdown.

#![allow(dead_code)]

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};
use crate::tui::context_panel::{ContextBreakdown, view as panel_view};

pub(crate) struct ContextPanelView {
    breakdown: ContextBreakdown,
    completed: bool,
}

impl ContextPanelView {
    pub fn new(breakdown: ContextBreakdown) -> Self {
        Self {
            breakdown,
            completed: false,
        }
    }
}

impl BottomPaneView for ContextPanelView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        panel_view::render(&self.breakdown, area, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        panel_view::desired_height(&self.breakdown)
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
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
        Some("Enter / q / Esc close".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn esc_marks_complete() {
        let mut v = ContextPanelView::new(ContextBreakdown::empty());
        v.handle_key(press(KeyCode::Esc));
        assert!(v.is_complete());
        assert!(v.completion().unwrap().result.is_none());
    }

    #[test]
    fn enter_also_closes() {
        let mut v = ContextPanelView::new(ContextBreakdown::empty());
        v.handle_key(press(KeyCode::Enter));
        assert!(v.is_complete());
    }

    #[test]
    fn q_closes() {
        let mut v = ContextPanelView::new(ContextBreakdown::empty());
        v.handle_key(press(KeyCode::Char('q')));
        assert!(v.is_complete());
    }

    #[test]
    fn ignores_other_keys() {
        let mut v = ContextPanelView::new(ContextBreakdown::empty());
        v.handle_key(press(KeyCode::Char('x')));
        assert!(!v.is_complete());
    }
}
