pub(crate) mod assistant_cell;
pub(crate) mod system_cell;
pub(crate) mod user_cell;

use std::any::Any;
use std::fmt::Debug;

use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

pub(crate) trait ChatCell: Debug + Send + Sync + Any {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>>;

    fn desired_height(&self, width: u16) -> u16 {
        let lines = self.display_lines(width);
        let text = ratatui::text::Text::from(lines);
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .line_count(width) as u16
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.display_lines(width)
    }

    fn is_stream_continuation(&self) -> bool {
        false
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        None
    }
}
