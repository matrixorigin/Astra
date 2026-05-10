//! Row/column navigation state for the table view — RED phase stub.

#![allow(dead_code)]

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct TableNav {
    pub row: usize,
    pub col_offset: usize,
    total_rows: usize,
    total_cols: usize,
}

impl TableNav {
    pub fn new(total_rows: usize, total_cols: usize) -> Self {
        Self {
            row: 0,
            col_offset: 0,
            total_rows,
            total_cols,
        }
    }

    pub fn move_up(&mut self) {
        if self.total_rows == 0 {
            return;
        }
        self.row = self.row.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.total_rows == 0 {
            return;
        }
        if self.row + 1 < self.total_rows {
            self.row += 1;
        }
    }

    pub fn scroll_left(&mut self) {
        self.col_offset = self.col_offset.saturating_sub(1);
    }

    pub fn scroll_right(&mut self) {
        if self.total_cols == 0 {
            return;
        }
        if self.col_offset + 1 < self.total_cols {
            self.col_offset += 1;
        }
    }

    pub fn jump_start(&mut self) {
        self.row = 0;
    }

    pub fn jump_end(&mut self) {
        if self.total_rows == 0 {
            self.row = 0;
        } else {
            self.row = self.total_rows - 1;
        }
    }

    pub fn row_valid(&self) -> bool {
        self.total_rows > 0 && self.row < self.total_rows
    }
}
