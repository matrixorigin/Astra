//! Legacy `ChatCell` trait — kept exclusively for
//! `ApprovalChatCell`, which renders the inline approval widget
//! inside `BottomPane`. The scrollback path moved to the
//! `history_cell::HistoryCell` trait in the refactor; new cells
//! go there. This module exists so the approval widget doesn't
//! have to be rewritten at the same time.

pub(crate) mod approval_cell;

use std::any::Any;
use std::fmt::Debug;

use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

pub(crate) trait ChatCell: Debug + Send + Sync + Any {
    fn as_any_mut(&mut self) -> &mut dyn Any;

    fn as_any_ref(&self) -> &dyn Any;

    fn display_lines(&self, width: u16) -> Vec<Line<'static>>;

    fn desired_height(&self, width: u16) -> u16 {
        let lines = self.display_lines(width);
        let text = ratatui::text::Text::from(lines);
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .line_count(width) as u16
    }
}
