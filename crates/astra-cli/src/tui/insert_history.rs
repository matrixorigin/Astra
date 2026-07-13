use std::io::{self, Write};

use crossterm::{
    cursor::MoveTo,
    queue,
    style::{Attribute, Color as CColor, Colors, Print, SetAttribute, SetColors},
    terminal::{Clear, ClearType},
};
use ratatui::backend::Backend;
use ratatui::layout::Size;
use ratatui::style::{Color as RColor, Modifier, Style};
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

    // Pre-wrap lines using adaptive wrapping (URL-aware, span-preserving)
    let mut wrapped: Vec<Line<'static>> = Vec::new();
    let mut wrapped_rows: u16 = 0;
    for line in lines {
        let line_w: usize = line.spans.iter().map(|s| s.content.width()).sum();
        if line_w == 0 {
            wrapped.push(super::render::line_utils::line_to_static(line));
            wrapped_rows += 1;
        } else if super::wrapping::line_contains_url_like(line)
            && !super::wrapping::line_has_mixed_url_and_non_url_tokens(line)
        {
            // Pure URL line — don't wrap, let terminal handle it (keeps URL clickable)
            let physical = line_w.max(1).div_ceil(wrap_width) as u16;
            wrapped_rows += physical;
            wrapped.push(super::render::line_utils::line_to_static(line));
        } else {
            let line_wrapped = super::wrapping::adaptive_wrap_line(
                line,
                super::wrapping::RtOptions::new(wrap_width),
            );
            for wl in &line_wrapped {
                wrapped_rows += wl.width().max(1).div_ceil(wrap_width) as u16;
            }
            wrapped.extend(
                line_wrapped
                    .into_iter()
                    .map(|l| crate::tui::render::line_utils::line_to_static(&l)),
            );
        }
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
            queue!(writer, MoveTo(0, screen_size.height.saturating_sub(1)))?;
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
            queue!(writer, Print(format!("\x1b[1;{}r", new_area.top())))?;

            queue!(writer, MoveTo(0, cursor_top))?;

            for line in &wrapped {
                queue!(writer, Print("\r\n"))?;
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
    write_history_line_content(writer, line)
}

/// Write one physical scrollback row. Diff surfaces are represented on spans
/// (so syntax highlighting can preserve foreground colours), while a terminal
/// scrollback writer has no buffer to paint after the final span. Erase the
/// remainder of a semantic diff row with its background active. Printing
/// spaces through the last physical column would arm terminal auto-wrap and
/// can make the following CRLF insert a phantom blank row.
fn write_history_line_content(writer: &mut impl Write, line: &Line<'_>) -> io::Result<()> {
    queue!(writer, SetAttribute(Attribute::Reset))?;
    queue!(writer, Clear(ClearType::UntilNewLine))?;

    for span in &line.spans {
        let merged = line.style.patch(span.style);
        write_styled_span(writer, &span.content, &merged)?;
    }

    if let Some(bg) = full_row_background(line) {
        apply_style_from_clean_state(writer, &Style::default().bg(bg))?;
        queue!(writer, Clear(ClearType::UntilNewLine))?;
    }
    queue!(writer, SetAttribute(Attribute::Reset))?;

    Ok(())
}

/// A full-width scrollback surface is reserved for a complete diff row.
/// Ordinary spans may carry a local background (selection, emphasis, a code
/// token) and must not accidentally turn into a horizontal stripe.
fn full_row_background(line: &Line<'_>) -> Option<RColor> {
    let candidate = line.style.bg.or_else(|| {
        line.spans
            .iter()
            .find(|span| !span.content.is_empty())
            .and_then(|span| line.style.patch(span.style).bg)
    })?;
    if line.style.bg.is_some() {
        return line.style.bg;
    }

    // A cell that deliberately paints a complete row supplies the same
    // background on every structural span (gutter, line number, marker and
    // content). A local highlight leaves another span on the default surface,
    // so it stays local.
    (line.spans.len() >= 2
        && line
            .spans
            .iter()
            .filter(|span| !span.content.is_empty())
            .all(|span| line.style.patch(span.style).bg == Some(candidate)))
    .then_some(candidate)
}

fn write_styled_span(
    writer: &mut impl Write,
    content: &str,
    style: &ratatui::style::Style,
) -> io::Result<()> {
    apply_style_from_clean_state(writer, style)?;
    queue!(writer, Print(content))?;
    Ok(())
}

fn apply_style_from_clean_state(
    writer: &mut impl Write,
    style: &ratatui::style::Style,
) -> io::Result<()> {
    queue!(
        writer,
        SetAttribute(Attribute::Reset),
        SetColors(Colors::new(
            style
                .fg
                .map(custom_terminal::to_crossterm_color)
                .unwrap_or(CColor::Reset),
            style
                .bg
                .map(custom_terminal::to_crossterm_color)
                .unwrap_or(CColor::Reset),
        ))
    )?;

    if style.add_modifier.contains(Modifier::REVERSED) {
        queue!(writer, SetAttribute(Attribute::Reverse))?;
    }
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
    if style.add_modifier.contains(Modifier::CROSSED_OUT) {
        queue!(writer, SetAttribute(Attribute::CrossedOut))?;
    }
    if style.add_modifier.contains(Modifier::SLOW_BLINK) {
        queue!(writer, SetAttribute(Attribute::SlowBlink))?;
    }
    if style.add_modifier.contains(Modifier::RAPID_BLINK) {
        queue!(writer, SetAttribute(Attribute::RapidBlink))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{full_row_background, write_history_line, write_history_line_content};
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn write_history_line_isolates_terminal_style_per_span() {
        let mut out = Vec::new();
        let line = Line::from(vec![
            Span::styled("link", Style::default().add_modifier(Modifier::UNDERLINED)),
            Span::raw(" normal"),
        ]);

        write_history_line(&mut out, &line).expect("history line should render");
        let rendered = String::from_utf8(out).expect("rendered bytes should be utf8");

        assert!(
            rendered.starts_with("\x1b[0m"),
            "history line must start from clean SGR state; rendered={rendered:?}"
        );

        let link_end = rendered.find("link").expect("link text present") + "link".len();
        let normal_start = rendered.find(" normal").expect("normal text present");
        assert!(
            rendered[link_end..normal_start].contains("\x1b[0m"),
            "non-underlined span must reset style after an underlined span; rendered={rendered:?}"
        );

        let reset_count = rendered.matches("\x1b[0m").count();
        assert!(
            reset_count >= 4,
            "line start, each span, and line end must each reset SGR state; rendered={rendered:?}"
        );
    }

    #[test]
    fn diff_span_surface_is_extended_to_the_physical_scrollback_row() {
        let theme = crate::tui::theme::Theme::dark();
        let line = Line::from(vec![
            Span::styled("  └ ", Style::default().bg(theme.diff_add_bg)),
            Span::styled("   1 + changed", Style::default().bg(theme.diff_add_bg)),
        ]);
        assert_eq!(full_row_background(&line), Some(theme.diff_add_bg));

        let mut out = Vec::new();
        write_history_line_content(&mut out, &line).expect("history row writes");
        let rendered = String::from_utf8(out).expect("history bytes are UTF-8");
        let plain = crate::cli::theme::strip_ansi(&rendered);
        assert_eq!(plain, "  └    1 + changed");
        assert!(
            rendered.matches("\x1b[K").count() >= 2,
            "the semantic row must clear its remaining terminal cells under the diff background: {rendered:?}"
        );
    }

    #[test]
    fn blank_diff_row_still_paints_exactly_one_physical_scrollback_row() {
        let theme = crate::tui::theme::Theme::dark();
        let line = Line::from(vec![
            Span::styled("    ", Style::default().bg(theme.diff_add_bg)),
            Span::styled("   2 + ", Style::default().bg(theme.diff_add_bg)),
        ]);

        let mut out = Vec::new();
        write_history_line_content(&mut out, &line).expect("blank diff row writes");
        let rendered = String::from_utf8(out).expect("history bytes are UTF-8");
        let plain = crate::cli::theme::strip_ansi(&rendered);

        assert_eq!(plain, "       2 + ");
        assert!(
            !plain.contains('\n'),
            "one diff item must not inject an extra row"
        );
        assert!(
            UnicodeWidthStr::width(plain.as_ref()) < 40,
            "semantic content must not be padded to the physical edge: {plain:?}"
        );
    }

    #[test]
    fn a_locally_highlighted_span_does_not_become_a_full_row_surface() {
        let theme = crate::tui::theme::Theme::dark();
        let line = Line::from(vec![
            Span::raw("ordinary "),
            Span::styled("highlight", Style::default().bg(theme.diff_add_bg)),
        ]);
        assert_eq!(full_row_background(&line), None);
    }
}
