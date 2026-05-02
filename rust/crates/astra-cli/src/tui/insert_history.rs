use std::io::{self, Write};

use crossterm::{
    cursor::MoveTo,
    queue,
    style::{
        Attribute, Color as CColor, Colors, Print, SetAttribute, SetBackgroundColor, SetColors,
        SetForegroundColor,
    },
    terminal::{Clear, ClearType},
};
use ratatui::backend::Backend;
use ratatui::layout::Size;
use ratatui::style::Modifier;
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

use super::custom_terminal;

/// Insert history lines above the viewport into terminal scrollback.
/// Matches Codex insert_history.rs Standard/Zellij paths.
pub(crate) fn insert_history_lines_with_terminal<B: Backend + Write>(
    terminal: &mut custom_terminal::Terminal<B>,
    lines: &[Line<'_>],
    is_zellij: bool,
) -> io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }

    let screen_size = terminal.backend().size().unwrap_or(Size::new(0, 0));
    let area = terminal.viewport_area;
    let last_cursor_pos = terminal.last_known_cursor_pos;
    let wrap_width = area.width.max(1) as usize;
    let mut should_update_area = false;

    // Pre-wrap lines (simple width-based wrapping)
    let mut wrapped: Vec<&Line<'_>> = Vec::new();
    let mut wrapped_rows: u16 = 0;
    for line in lines {
        let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
        let rows = if w == 0 {
            1
        } else {
            ((w + wrap_width - 1) / wrap_width) as u16
        };
        wrapped_rows += rows;
        wrapped.push(line);
    }

    if wrapped_rows == 0 {
        return Ok(());
    }

    let writer = terminal.backend_mut();

    if is_zellij {
        // Zellij mode: emit newlines at bottom, write at absolute positions
        let space_below = screen_size.height.saturating_sub(area.bottom());
        let shift_down = wrapped_rows.min(space_below);
        let scroll_up = wrapped_rows.saturating_sub(shift_down);

        if scroll_up > 0 {
            queue!(
                writer,
                MoveTo(0, screen_size.height.saturating_sub(1))
            )?;
            for _ in 0..scroll_up {
                queue!(writer, Print("\n"))?;
            }
        }

        let mut new_area = area;
        if shift_down > 0 {
            new_area.y += shift_down;
            should_update_area = true;
        }

        let cursor_top = area.top().saturating_sub(scroll_up + shift_down);
        queue!(writer, MoveTo(0, cursor_top))?;

        for (i, line) in wrapped.iter().enumerate() {
            if i > 0 {
                queue!(writer, Print("\r\n"))?;
            }
            queue!(writer, Clear(ClearType::UntilNewLine))?;
            write_history_line(writer, line)?;
        }

        // Restore cursor position
        queue!(writer, MoveTo(last_cursor_pos.x, last_cursor_pos.y))?;
        Write::flush(writer)?;

        let _ = writer;
        if should_update_area {
            terminal.set_viewport_area(new_area);
        }
        if wrapped_rows > 0 {
            terminal.note_history_rows_inserted(wrapped_rows);
        }
    } else {
        // Standard mode — matches Codex insert_history.rs Standard path:
        //
        // 1. If there's room below the viewport, set scroll region from
        //    viewport_top to screen_bottom and RI to push viewport down.
        // 2. Set scroll region to [1..new_viewport_top] (above viewport only).
        // 3. Write history lines within that protected region.
        // 4. Reset scroll region and restore cursor.

        let mut new_area = area;
        let cursor_top = if area.bottom() < screen_size.height {
            let scroll_amount = wrapped_rows.min(screen_size.height - area.bottom());

            let top_1based = area.top() + 1;
            queue!(
                writer,
                Print(format!("\x1b[{};{}r", top_1based, screen_size.height))
            )?;
            queue!(writer, MoveTo(0, area.top()))?;
            for _ in 0..scroll_amount {
                queue!(writer, Print("\x1bM"))?;
            }
            queue!(writer, Print("\x1b[r"))?;

            let ct = area.top().saturating_sub(1);
            new_area.y += scroll_amount;
            should_update_area = true;
            ct
        } else {
            area.top().saturating_sub(1)
        };

        if new_area.top() > 0 {
            queue!(
                writer,
                Print(format!("\x1b[1;{}r", new_area.top()))
            )?;

            queue!(writer, MoveTo(0, cursor_top))?;

            for line in &wrapped {
                queue!(writer, Print("\r\n"))?;
                queue!(writer, Clear(ClearType::UntilNewLine))?;
                write_history_line(writer, line)?;
            }

            queue!(writer, Print("\x1b[r"))?;
        }

        // Restore cursor position
        queue!(writer, MoveTo(last_cursor_pos.x, last_cursor_pos.y))?;
        Write::flush(writer)?;

        let _ = writer;
        if should_update_area {
            terminal.set_viewport_area(new_area);
        }
        if wrapped_rows > 0 {
            terminal.note_history_rows_inserted(wrapped_rows);
        }
    }

    Ok(())
}

fn write_history_line(writer: &mut impl Write, line: &Line<'_>) -> io::Result<()> {
    // Set line-level colors
    queue!(
        writer,
        SetColors(Colors::new(
            line.style
                .fg
                .map(Into::into)
                .unwrap_or(CColor::Reset),
            line.style
                .bg
                .map(Into::into)
                .unwrap_or(CColor::Reset),
        ))
    )?;

    // Write spans with style
    for span in &line.spans {
        let merged = span.style.patch(line.style);
        write_styled_span(writer, &span.content, &merged)?;
    }

    // Reset attributes after line
    queue!(
        writer,
        SetForegroundColor(CColor::Reset),
        SetBackgroundColor(CColor::Reset),
        SetAttribute(Attribute::Reset),
    )?;

    Ok(())
}

fn write_styled_span(
    writer: &mut impl Write,
    content: &str,
    style: &ratatui::style::Style,
) -> io::Result<()> {
    // Apply modifiers
    if style.add_modifier.contains(Modifier::BOLD) {
        queue!(writer, SetAttribute(Attribute::Bold))?;
    }
    if style.add_modifier.contains(Modifier::DIM) {
        queue!(writer, SetAttribute(Attribute::Dim))?;
    }
    if style.add_modifier.contains(Modifier::ITALIC) {
        queue!(writer, SetAttribute(Attribute::Italic))?;
    }
    if style.add_modifier.contains(Modifier::UNDERLINED) {
        queue!(writer, SetAttribute(Attribute::Underlined))?;
    }

    // Apply colors
    if let Some(fg) = style.fg {
        queue!(writer, SetForegroundColor(fg.into()))?;
    }
    if let Some(bg) = style.bg {
        queue!(writer, SetBackgroundColor(bg.into()))?;
    }

    queue!(writer, Print(content))?;

    // Reset modifiers (but keep line-level colors)
    if !style.add_modifier.is_empty() {
        queue!(writer, SetAttribute(Attribute::Reset))?;
    }

    Ok(())
}
