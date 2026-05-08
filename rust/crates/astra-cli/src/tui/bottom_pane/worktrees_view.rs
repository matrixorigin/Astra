//! BottomPaneView wrapper for the worktrees panel.

#![allow(dead_code)]

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};
use crate::tui::worktrees::{WorktreeList, view as worktrees_view};

pub(crate) struct WorktreesView {
    list: WorktreeList,
    completed: bool,
}

impl WorktreesView {
    pub fn new(list: WorktreeList) -> Self {
        Self {
            list,
            completed: false,
        }
    }
}

impl BottomPaneView for WorktreesView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        worktrees_view::render(&self.list, area, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        worktrees_view::desired_height(&self.list)
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.completed = true;
            }
            KeyCode::Up => self.list.move_up(),
            KeyCode::Down => self.list.move_down(),
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
        Some("↑↓ navigate · q / Esc close".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::worktrees::model::parse;
    use crossterm::event::KeyModifiers;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn fixture() -> WorktreesView {
        let v = parse("worktree /a\nHEAD abc\nbranch refs/heads/main\n\nworktree /b\nHEAD def\nbranch refs/heads/feat\n");
        WorktreesView::new(WorktreeList::new(v))
    }

    #[test]
    fn esc_closes() {
        let mut v = fixture();
        v.handle_key(press(KeyCode::Esc));
        assert!(v.is_complete());
    }

    #[test]
    fn arrows_navigate() {
        let mut v = fixture();
        assert_eq!(v.list.selected(), Some(0));
        v.handle_key(press(KeyCode::Down));
        assert_eq!(v.list.selected(), Some(1));
    }

    #[test]
    fn q_closes() {
        let mut v = fixture();
        v.handle_key(press(KeyCode::Char('q')));
        assert!(v.is_complete());
    }
}
