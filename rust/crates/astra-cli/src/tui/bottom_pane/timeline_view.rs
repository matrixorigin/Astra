//! BottomPaneView wrapper for the session timeline panel.

#![allow(dead_code)]

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};
use crate::tui::timeline::{Timeline, view as timeline_view};

pub(crate) struct TimelineView {
    timeline: Timeline,
    completed: bool,
}

impl TimelineView {
    pub fn new(timeline: Timeline) -> Self {
        Self {
            timeline,
            completed: false,
        }
    }
}

impl BottomPaneView for TimelineView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        timeline_view::render(&self.timeline, area, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        timeline_view::desired_height(&self.timeline)
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.completed = true;
            }
            KeyCode::Up => self.timeline.move_up(),
            KeyCode::Down => self.timeline.move_down(),
            KeyCode::PageUp => {
                for _ in 0..5 {
                    self.timeline.move_up();
                }
            }
            KeyCode::PageDown => {
                for _ in 0..5 {
                    self.timeline.move_down();
                }
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
        Some("↑↓ navigate · PgUp/PgDn page · q / Esc close".into())
    }

    fn reserve_status_footer(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::timeline::model::{StaticTurnSource, TimelineTurn};
    use crossterm::event::KeyModifiers;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn fixture() -> TimelineView {
        let src = StaticTurnSource::new(vec![
            TimelineTurn {
                turn: 1,
                started_at: "2024-01-15T10:01:00Z".into(),
                duration_ms: Some(1500),
                model: None,
                tokens_in: Some(100),
                tokens_out: Some(50),
                tool_count: Some(0),
                user_preview: Some("hi".into()),
                assistant_preview: Some("hello".into()),
                error: None,
                cumulative_tokens_in: 0,
                cumulative_tokens_out: 0,
            },
            TimelineTurn {
                turn: 2,
                started_at: "2024-01-15T10:02:00Z".into(),
                duration_ms: Some(1700),
                model: None,
                tokens_in: Some(200),
                tokens_out: Some(100),
                tool_count: Some(1),
                user_preview: Some("next".into()),
                assistant_preview: Some("ok".into()),
                error: None,
                cumulative_tokens_in: 0,
                cumulative_tokens_out: 0,
            },
        ]);
        TimelineView::new(Timeline::new(src, "sess_test"))
    }

    #[test]
    fn esc_closes() {
        let mut v = fixture();
        v.handle_key(press(KeyCode::Esc));
        assert!(v.is_complete());
    }

    #[test]
    fn q_closes() {
        let mut v = fixture();
        v.handle_key(press(KeyCode::Char('q')));
        assert!(v.is_complete());
    }

    #[test]
    fn arrows_move_selection_inside_timeline() {
        let mut v = fixture();
        assert_eq!(v.timeline.selected(), Some(0));
        v.handle_key(press(KeyCode::Down));
        assert_eq!(v.timeline.selected(), Some(1));
        v.handle_key(press(KeyCode::Up));
        assert_eq!(v.timeline.selected(), Some(0));
    }
}
