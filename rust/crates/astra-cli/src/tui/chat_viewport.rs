use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Text},
    widgets::{Clear, Paragraph, Widget},
};

use super::chat_cell::ChatCell;

#[derive(Debug, Default)]
pub(crate) struct ChatViewport {
    cells: Vec<Box<dyn ChatCell>>,
    scroll_offset: u16,
    auto_follow: bool,
    needs_scroll_to_bottom: bool,
    last_width: u16,
}

impl ChatViewport {
    pub fn new() -> Self {
        Self {
            cells: Vec::new(),
            scroll_offset: 0,
            auto_follow: true,
            needs_scroll_to_bottom: false,
            last_width: 80,
        }
    }

    pub fn push_cell(&mut self, cell: Box<dyn ChatCell>) {
        self.cells.push(cell);
        if self.auto_follow {
            self.needs_scroll_to_bottom = true;
        }
    }

    pub fn push_cell_get_idx(&mut self, cell: Box<dyn ChatCell>) -> usize {
        let idx = self.cells.len();
        self.push_cell(cell);
        idx
    }

    pub fn replace_cell(&mut self, idx: usize, cell: Box<dyn ChatCell>) {
        if idx < self.cells.len() {
            self.cells[idx] = cell;
            if self.auto_follow {
                self.needs_scroll_to_bottom = true;
            }
        }
    }

    pub fn mutate_cell<F>(&mut self, idx: usize, f: F)
    where
        F: FnOnce(&mut dyn std::any::Any),
    {
        if idx < self.cells.len() {
            let any = self.cells[idx].as_any_mut();
            f(any);
            // Don't auto-scroll on mutation — only push_cell triggers scroll-to-bottom.
            // In-place updates (thinking chunks, tool status) shouldn't jump the viewport.
        }
    }

    pub fn scroll_up(&mut self, amount: u16) {
        self.auto_follow = false;
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    pub fn scroll_down(&mut self, amount: u16, _width: u16, viewport_height: u16) {
        let total = self.total_content_height(self.last_width);
        self.scroll_offset = self
            .scroll_offset
            .saturating_add(amount)
            .min(total.saturating_sub(viewport_height));
        if self.is_at_bottom(viewport_height) {
            self.auto_follow = true;
        }
    }

    pub fn scroll_page_up(&mut self, viewport_height: u16) {
        self.scroll_up(viewport_height.saturating_sub(2));
    }

    pub fn scroll_page_down(&mut self, viewport_height: u16) {
        self.scroll_down(viewport_height.saturating_sub(2), self.last_width, viewport_height);
    }

    pub fn jump_to_top(&mut self) {
        self.auto_follow = false;
        self.scroll_offset = 0;
    }

    pub fn jump_to_bottom(&mut self, _width: u16, viewport_height: u16) {
        let total = self.total_content_height(self.last_width);
        self.scroll_offset = total.saturating_sub(viewport_height);
        self.auto_follow = true;
    }

    fn is_at_bottom(&self, viewport_height: u16) -> bool {
        let total = self.total_content_height(self.last_width);
        self.scroll_offset + viewport_height >= total
    }

    fn total_content_height(&self, width: u16) -> u16 {
        let mut h: u16 = 0;
        for (i, cell) in self.cells.iter().enumerate() {
            if i > 0 {
                h = h.saturating_add(1); // 1-line spacing between cells
            }
            h = h.saturating_add(cell.desired_height(width));
        }
        h
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.last_width = area.width;

        // Clear the entire area first — prevents stale glyphs from previous frames
        Clear.render(area, buf);

        if self.needs_scroll_to_bottom {
            let total = self.total_content_height(area.width);
            self.scroll_offset = total.saturating_sub(area.height);
            self.needs_scroll_to_bottom = false;
        }

        let width = area.width;
        let mut all_lines: Vec<Line<'static>> = Vec::new();
        for (i, cell) in self.cells.iter().enumerate() {
            if i > 0 {
                all_lines.push(Line::default()); // 1-line spacing between cells
            }
            let lines = cell.display_lines(width);
            all_lines.extend(lines);
        }

        let total_lines = all_lines.len() as u16;
        let scroll = self
            .scroll_offset
            .min(total_lines.saturating_sub(area.height));

        let text = Text::from(all_lines);
        let paragraph = Paragraph::new(text).scroll((scroll, 0));
        Widget::render(paragraph, area, buf);
    }
}
