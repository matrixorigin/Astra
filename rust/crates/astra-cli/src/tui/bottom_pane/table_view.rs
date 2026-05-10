//! BottomPaneView wrapper for the table panel.

#![allow(dead_code)]

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};
use crate::tui::table_view::{MysqlTable, TableNav, view as table_view};

pub(crate) struct TablePanelView {
    table: MysqlTable,
    nav: TableNav,
    completed: bool,
}

impl TablePanelView {
    pub fn new(table: MysqlTable) -> Self {
        let nav = TableNav::new(table.num_rows(), table.num_cols());
        Self {
            table,
            nav,
            completed: false,
        }
    }
}

impl BottomPaneView for TablePanelView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        table_view::render(&self.table, &self.nav, area, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        table_view::desired_height(&self.table)
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.completed = true;
            }
            KeyCode::Up => self.nav.move_up(),
            KeyCode::Down => self.nav.move_down(),
            KeyCode::Left => self.nav.scroll_left(),
            KeyCode::Right => self.nav.scroll_right(),
            KeyCode::Home => self.nav.jump_start(),
            KeyCode::End => self.nav.jump_end(),
            KeyCode::PageUp => {
                for _ in 0..5 {
                    self.nav.move_up();
                }
            }
            KeyCode::PageDown => {
                for _ in 0..5 {
                    self.nav.move_down();
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
        Some("↑↓ rows · ←→ cols · Home/End jump · q / Esc close".into())
    }

    fn reserve_status_footer(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::table_view::parse;
    use crossterm::event::KeyModifiers;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn fixture() -> TablePanelView {
        let t = parse(
            "\
+----+-------+
| id | name  |
+----+-------+
|  1 | alice |
|  2 | bob   |
+----+-------+
",
        )
        .unwrap();
        TablePanelView::new(t)
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
    fn arrows_navigate_rows_and_cols() {
        let mut v = fixture();
        assert_eq!(v.nav.row, 0);
        v.handle_key(press(KeyCode::Down));
        assert_eq!(v.nav.row, 1);
        v.handle_key(press(KeyCode::Right));
        assert_eq!(v.nav.col_offset, 1);
    }
}
