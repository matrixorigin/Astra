use std::io::{self, Write};

use crossterm::{cursor, queue, style::Print};
use ratatui::backend::Backend;
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

use super::custom_terminal;

/// Insert history lines above the viewport into terminal scrollback.
/// Follows Codex's insert_history pattern exactly.
pub(crate) fn insert_history_lines_with_terminal<B: Backend + Write>(
    terminal: &mut custom_terminal::Terminal<B>,
    lines: &[Line<'_>],
    is_zellij: bool,
) -> io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }

    let area = terminal.viewport_area;
    let screen_size = terminal.size()?;
    let wrap_width = area.width.max(1) as usize;

    // Pre-wrap lines
    let mut wrapped: Vec<&Line<'_>> = Vec::new();
    let mut wrapped_rows: u16 = 0;
    for line in lines {
        let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
        let rows = if w == 0 { 1 } else { ((w + wrap_width - 1) / wrap_width) as u16 };
        wrapped_rows += rows;
        wrapped.push(line);
    }

    if wrapped_rows == 0 {
        return Ok(());
    }

    let writer = terminal.backend_mut();
    let mut should_update_area = false;
    let mut new_area = area;

    if is_zellij {
        // Zellij mode: emit newlines at bottom, write at absolute positions
        let space_below = screen_size.height.saturating_sub(area.bottom());
        let shift_down = wrapped_rows.min(space_below);
        let scroll_up = wrapped_rows.saturating_sub(shift_down);

        if scroll_up > 0 {
            queue!(writer, cursor::MoveTo(0, screen_size.height.saturating_sub(1)))?;
            for _ in 0..scroll_up {
                queue!(writer, Print("\n"))?;
            }
        }

        if shift_down > 0 {
            new_area.y += shift_down;
            should_update_area = true;
        }

        let cursor_top = area.top().saturating_sub(scroll_up + shift_down);
        queue!(writer, cursor::MoveTo(0, cursor_top))?;

        for (i, line) in wrapped.iter().enumerate() {
            if i > 0 {
                queue!(writer, Print("\r\n"))?;
            }
            write_history_line(writer, line, wrap_width)?;
        }
    } else {
        // Standard mode: use scroll regions (matching Codex insert_history.rs)
        //
        // Step 1: If viewport is near bottom, push it down by scrolling region above it
        if wrapped_rows > 0 && area.top() > 0 {
            let scroll_amount = wrapped_rows;

            // Clear viewport content BEFORE scroll, so stale composer/footer
            // text doesn't leak into scrollback when RI pushes it down
            for row in area.top()..area.bottom() {
                queue!(writer, cursor::MoveTo(0, row), Print("\x1b[2K"))?;
            }

            // Set scroll region to cover viewport top to screen bottom
            let top_1based = area.top() + 1;
            queue!(writer, Print(format!("\x1b[{};{}r", top_1based, screen_size.height)))?;
            queue!(writer, cursor::MoveTo(0, area.top()))?;
            for _ in 0..scroll_amount {
                queue!(writer, Print("\x1bM"))?; // Reverse Index: push content down
            }
            queue!(writer, Print("\x1b[r"))?; // Reset scroll region

            new_area.y = new_area.y.saturating_add(scroll_amount).min(screen_size.height);
            should_update_area = true;
        }

        // Step 2: Write new lines into the gap above the new viewport position
        if new_area.top() > 0 {
            let cursor_top = new_area.top().saturating_sub(wrapped_rows).max(0);
            queue!(writer, cursor::MoveTo(0, cursor_top))?;

            for (i, line) in wrapped.iter().enumerate() {
                if i > 0 {
                    queue!(writer, Print("\r\n"))?;
                }
                write_history_line(writer, line, wrap_width)?;
            }
        }
    }

    Write::flush(writer)?;

    if should_update_area {
        terminal.set_viewport_area(new_area);
    }
    if wrapped_rows > 0 {
        terminal.note_history_rows_inserted(wrapped_rows);
    }

    Ok(())
}

fn write_history_line(writer: &mut impl Write, line: &Line<'_>, _wrap_width: usize) -> io::Result<()> {
    for span in &line.spans {
        let ansi = build_ansi_style(&span.style);
        if !ansi.is_empty() {
            queue!(writer, Print(&ansi))?;
        }
        queue!(writer, Print(&span.content))?;
        if !ansi.is_empty() {
            queue!(writer, Print("\x1b[0m"))?;
        }
    }
    Ok(())
}

fn build_ansi_style(style: &ratatui::style::Style) -> String {
    let mut codes = Vec::new();
    if let Some(fg) = style.fg {
        if let Some(c) = color_to_ansi_fg(fg) { codes.push(c); }
    }
    if let Some(bg) = style.bg {
        if let Some(c) = color_to_ansi_bg(bg) { codes.push(c); }
    }
    if style.add_modifier.contains(ratatui::style::Modifier::BOLD) { codes.push("1".into()); }
    if style.add_modifier.contains(ratatui::style::Modifier::DIM) { codes.push("2".into()); }
    if style.add_modifier.contains(ratatui::style::Modifier::ITALIC) { codes.push("3".into()); }
    if style.add_modifier.contains(ratatui::style::Modifier::UNDERLINED) { codes.push("4".into()); }
    if codes.is_empty() { String::new() } else { format!("\x1b[{}m", codes.join(";")) }
}

fn color_to_ansi_fg(color: ratatui::style::Color) -> Option<String> {
    use ratatui::style::Color;
    match color {
        Color::Black => Some("30".into()), Color::Red => Some("31".into()),
        Color::Green => Some("32".into()), Color::Yellow => Some("33".into()),
        Color::Blue => Some("34".into()), Color::Magenta => Some("35".into()),
        Color::Cyan => Some("36".into()), Color::Gray => Some("37".into()),
        Color::DarkGray => Some("90".into()), Color::LightRed => Some("91".into()),
        Color::LightGreen => Some("92".into()), Color::LightYellow => Some("93".into()),
        Color::LightBlue => Some("94".into()), Color::LightMagenta => Some("95".into()),
        Color::LightCyan => Some("96".into()), Color::White => Some("97".into()),
        Color::Rgb(r, g, b) => Some(format!("38;2;{r};{g};{b}")),
        Color::Indexed(i) => Some(format!("38;5;{i}")),
        _ => None,
    }
}

fn color_to_ansi_bg(color: ratatui::style::Color) -> Option<String> {
    use ratatui::style::Color;
    match color {
        Color::Black => Some("40".into()), Color::Red => Some("41".into()),
        Color::Green => Some("42".into()), Color::Yellow => Some("43".into()),
        Color::Blue => Some("44".into()), Color::Magenta => Some("45".into()),
        Color::Cyan => Some("46".into()), Color::Gray => Some("47".into()),
        Color::DarkGray => Some("100".into()), Color::LightRed => Some("101".into()),
        Color::LightGreen => Some("102".into()), Color::LightYellow => Some("103".into()),
        Color::LightBlue => Some("104".into()), Color::LightMagenta => Some("105".into()),
        Color::LightCyan => Some("106".into()), Color::White => Some("107".into()),
        Color::Rgb(r, g, b) => Some(format!("48;2;{r};{g};{b}")),
        Color::Indexed(i) => Some(format!("48;5;{i}")),
        _ => None,
    }
}
