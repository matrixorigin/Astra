//! BottomPaneView wrapper for the session timeline panel.

#![allow(dead_code)]

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};
use crate::tui::timeline::{Timeline, view as timeline_view};

pub(crate) struct TimelineView {
    timeline: Timeline,
    drill_scroll: u16,
    last_drill_max_scroll: Cell<u16>,
    completed: bool,
}

impl TimelineView {
    pub fn new(timeline: Timeline) -> Self {
        Self {
            timeline,
            drill_scroll: 0,
            last_drill_max_scroll: Cell::new(0),
            completed: false,
        }
    }

    fn enter_drill(&mut self) {
        self.timeline.enter_drill();
        self.drill_scroll = 0;
        self.last_drill_max_scroll.set(0);
    }

    fn exit_drill(&mut self) {
        self.timeline.exit_drill();
        self.drill_scroll = 0;
        self.last_drill_max_scroll.set(0);
    }

    fn scroll_drill_down(&mut self, amount: u16) {
        let max = self.last_drill_max_scroll.get();
        self.drill_scroll = self.drill_scroll.saturating_add(amount).min(max);
    }

    fn scroll_drill_up(&mut self, amount: u16) {
        self.drill_scroll = self.drill_scroll.saturating_sub(amount);
    }
}

impl BottomPaneView for TimelineView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let max_scroll =
            timeline_view::render_with_drill_scroll(&self.timeline, area, buf, self.drill_scroll);
        self.last_drill_max_scroll.set(max_scroll);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        timeline_view::desired_height(&self.timeline)
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.timeline.is_drilled() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.exit_drill(),
                KeyCode::Up => self.scroll_drill_up(1),
                KeyCode::Down => self.scroll_drill_down(1),
                KeyCode::PageUp => self.scroll_drill_up(10),
                KeyCode::PageDown => self.scroll_drill_down(10),
                KeyCode::Home => self.drill_scroll = 0,
                KeyCode::End => self.drill_scroll = self.last_drill_max_scroll.get(),
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.completed = true,
            KeyCode::Enter => self.enter_drill(),
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
        if self.timeline.is_drilled() {
            Some("↑↓ scroll · PgUp/PgDn page · Home/End · Esc back".into())
        } else {
            Some("↑↓ navigate · Enter trace · PgUp/PgDn page · Esc close".into())
        }
    }

    fn reserve_status_footer(&self) -> bool {
        true
    }

    fn render_as_overlay(&self) -> bool {
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
                ttft_ms: None,
                context_ms: None,
                memoria_ms: None,
                llm_rounds: None,
                selected_skills: None,
                total_tool_ms: None,
                total_llm_ms: None,
                tool_calls: Vec::new(),
                user_input: None,
                assistant_output: None,
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
                ttft_ms: None,
                context_ms: None,
                memoria_ms: None,
                llm_rounds: None,
                selected_skills: None,
                total_tool_ms: None,
                total_llm_ms: None,
                tool_calls: Vec::new(),
                user_input: None,
                assistant_output: None,
            },
        ]);
        TimelineView::new(Timeline::new(src, "sess_test"))
    }

    #[test]
    fn timeline_overlay_does_not_inflate_bottom_pane_height() {
        let mut pane = crate::tui::bottom_pane::BottomPane::new();
        let before = pane.desired_height(80);

        pane.push_view(Box::new(fixture()));

        assert!(pane.has_overlay_view());
        assert_eq!(
            pane.desired_height(80),
            before,
            "timeline must not resize the native scrollback viewport"
        );
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

    #[test]
    fn drilled_view_scrolls_detail() {
        let mut v = fixture();
        v.handle_key(press(KeyCode::Enter));
        assert!(v.timeline.is_drilled());
        assert_eq!(v.drill_scroll, 0);

        v.last_drill_max_scroll.set(12);
        v.handle_key(press(KeyCode::Down));
        assert_eq!(v.drill_scroll, 1);
        v.handle_key(press(KeyCode::PageDown));
        assert_eq!(v.drill_scroll, 11);
        v.handle_key(press(KeyCode::PageDown));
        assert_eq!(v.drill_scroll, 12);
        v.handle_key(press(KeyCode::Up));
        assert_eq!(v.drill_scroll, 11);
        v.handle_key(press(KeyCode::Home));
        assert_eq!(v.drill_scroll, 0);
        v.handle_key(press(KeyCode::End));
        assert_eq!(v.drill_scroll, 12);

        v.handle_key(press(KeyCode::Esc));
        assert!(!v.timeline.is_drilled());
        assert_eq!(v.drill_scroll, 0);
    }
}
