use std::{
    io::{self, Error, ErrorKind},
    time::Duration,
};

use crate::{
    event::query_cursor_position,
    terminal::{disable_raw_mode, enable_raw_mode, sys::is_raw_mode_enabled},
};

/// Returns the cursor position (column, row).
///
/// The top left cell is represented as `(0, 0)`.
///
/// On unix systems, this function will block and possibly time out while
/// [`crossterm::event::read`](crate::event::read) or [`crossterm::event::poll`](crate::event::poll) are being called.
pub fn position() -> io::Result<(u16, u16)> {
    if is_raw_mode_enabled() {
        read_position_raw()
    } else {
        read_position()
    }
}

fn read_position() -> io::Result<(u16, u16)> {
    enable_raw_mode()?;
    let pos = read_position_raw();
    disable_raw_mode()?;
    pos
}

fn read_position_raw() -> io::Result<(u16, u16)> {
    query_cursor_position(Duration::from_millis(2000))?
        .position
        .ok_or_else(|| {
            Error::new(
                ErrorKind::Other,
                "The cursor position could not be read within a normal duration",
            )
        })
}
