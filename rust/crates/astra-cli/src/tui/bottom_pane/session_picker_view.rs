//! BottomPaneView wrapper around [`crate::tui::session_picker::SessionDiscovery`].
//!
//! Keystroke semantics:
//! - printable chars / backspace: update the filter (live fuzzy match)
//! - ↑ / ↓: move selection
//! - PgUp / PgDn: jump 5 rows
//! - Enter: accept and emit the selected session id via `ViewCompletion`
//! - Esc: cancel without resuming

#![allow(dead_code)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};
use crate::tui::session_picker::{SessionDiscovery, view as picker_view};

pub(crate) struct SessionPickerView {
    disco: SessionDiscovery,
    /// Accumulated filter text (drives [`SessionDiscovery::set_filter`]).
    filter: String,
    result: Option<String>,
    cancelled: bool,
}

impl SessionPickerView {
    pub fn new(disco: SessionDiscovery) -> Self {
        Self {
            disco,
            filter: String::new(),
            result: None,
            cancelled: false,
        }
    }

    fn update_filter(&mut self) {
        self.disco.set_filter(&self.filter);
    }

    #[cfg(test)]
    pub(crate) fn filter_text(&self) -> &str {
        &self.filter
    }

    #[cfg(test)]
    pub(crate) fn selected_id(&self) -> Option<String> {
        self.disco.accept()
    }

    #[cfg(test)]
    pub(crate) fn accepted(&self) -> Option<&str> {
        self.result.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn cancelled(&self) -> bool {
        self.cancelled
    }
}

impl BottomPaneView for SessionPickerView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        picker_view::render(&self.disco, area, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        picker_view::desired_height(&self.disco)
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.cancelled = true;
            }
            KeyCode::Enter => {
                if let Some(id) = self.disco.accept() {
                    self.result = Some(id);
                } else {
                    // No match — treat as cancel rather than dangling.
                    self.cancelled = true;
                }
            }
            KeyCode::Up => self.disco.move_up(),
            KeyCode::Down => self.disco.move_down(),
            KeyCode::PageUp => {
                for _ in 0..5 {
                    self.disco.move_up();
                }
            }
            KeyCode::PageDown => {
                for _ in 0..5 {
                    self.disco.move_down();
                }
            }
            KeyCode::Backspace if !self.filter.is_empty() => {
                self.filter.pop();
                self.update_filter();
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.filter.push(c);
                self.update_filter();
            }
            _ => {}
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
        self.result.is_some() || self.cancelled
    }

    fn completion(&self) -> Option<ViewCompletion> {
        if self.result.is_some() {
            Some(ViewCompletion {
                result: self.result.clone(),
                reopen: None,
            })
        } else if self.cancelled {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::session_picker::discovery::{SessionEntry, StaticSessionSource};

    fn entry(id: &str, summary: &str) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            cwd: "~/astra".into(),
            git_branch: Some("main".into()),
            git_head: None,
            turn_count: 1,
            tokens_in: 100,
            tokens_out: 50,
            cost_usd: None,
            summary: Some(summary.into()),
            status: "completed".into(),
            model: "s".into(),
            updated_at: "2024-01-01T00:00:00Z".into(),
            checkpoints: 0,
            plan_goal: None,
        }
    }

    fn fixture_view() -> SessionPickerView {
        let src = StaticSessionSource::new(vec![
            entry("sess_alpha", "alpha session"),
            entry("sess_beta", "beta session"),
            entry("sess_gamma", "gamma session"),
        ]);
        SessionPickerView::new(SessionDiscovery::new(src, 10))
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn press_char(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn typing_updates_filter_live() {
        let mut v = fixture_view();
        v.handle_key(press_char('b'));
        v.handle_key(press_char('e'));
        assert_eq!(v.filter_text(), "be");
        assert_eq!(v.selected_id().as_deref(), Some("sess_beta"));
    }

    #[test]
    fn backspace_shortens_filter() {
        let mut v = fixture_view();
        v.handle_key(press_char('x'));
        v.handle_key(press_char('y'));
        v.handle_key(press(KeyCode::Backspace));
        assert_eq!(v.filter_text(), "x");
    }

    #[test]
    fn arrows_move_selection() {
        let mut v = fixture_view();
        assert_eq!(v.selected_id().as_deref(), Some("sess_alpha"));
        v.handle_key(press(KeyCode::Down));
        assert_eq!(v.selected_id().as_deref(), Some("sess_beta"));
        v.handle_key(press(KeyCode::Up));
        assert_eq!(v.selected_id().as_deref(), Some("sess_alpha"));
    }

    #[test]
    fn enter_produces_completion_with_result() {
        let mut v = fixture_view();
        v.handle_key(press(KeyCode::Down));
        v.handle_key(press(KeyCode::Enter));
        assert!(v.is_complete());
        let c = v.completion().expect("completion");
        assert_eq!(c.result.as_deref(), Some("sess_beta"));
    }

    #[test]
    fn esc_cancels_without_result() {
        let mut v = fixture_view();
        v.handle_key(press(KeyCode::Esc));
        assert!(v.is_complete());
        let c = v.completion().expect("completion");
        assert_eq!(c.result, None);
    }

    #[test]
    fn enter_with_no_matches_cancels() {
        let mut v = fixture_view();
        v.handle_key(press_char('z'));
        v.handle_key(press_char('z'));
        v.handle_key(press_char('z'));
        v.handle_key(press(KeyCode::Enter));
        assert!(v.is_complete());
        assert_eq!(v.accepted(), None);
        assert!(v.cancelled());
    }
}
