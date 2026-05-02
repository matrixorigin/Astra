//! Renders diff output with line numbers, gutter signs, and colors.
//!
//! Inspired by Codex's diff_render.rs (MIT) but simplified for astra's
//! use case: tool output strings with +/- prefix lines rather than
//! structured FileChange objects.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::color::is_light;
use super::terminal_palette::default_bg;

/// Dark terminal diff colors (muted tints).
const DARK_ADD_FG: Color = Color::Green;
const DARK_DEL_FG: Color = Color::Red;
const DARK_ADD_BG: Color = Color::Rgb(33, 58, 43);
const DARK_DEL_BG: Color = Color::Rgb(74, 34, 29);

/// Light terminal diff colors (GitHub-style pastels).
const LIGHT_ADD_FG: Color = Color::Rgb(31, 35, 40);
const LIGHT_DEL_FG: Color = Color::Rgb(31, 35, 40);
const LIGHT_ADD_BG: Color = Color::Rgb(218, 251, 225);
const LIGHT_DEL_BG: Color = Color::Rgb(255, 235, 233);

fn is_light_bg() -> bool {
    default_bg().map_or(false, is_light)
}

/// Style for an added line.
fn add_style() -> Style {
    if is_light_bg() {
        Style::default().fg(LIGHT_ADD_FG).bg(LIGHT_ADD_BG)
    } else {
        Style::default().fg(DARK_ADD_FG)
    }
}

/// Style for a deleted line.
fn del_style() -> Style {
    if is_light_bg() {
        Style::default().fg(LIGHT_DEL_FG).bg(LIGHT_DEL_BG)
    } else {
        Style::default().fg(DARK_DEL_FG)
    }
}

/// Style for context/unchanged lines.
fn ctx_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Style for the gutter (line numbers + sign).
fn gutter_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Style for the diff header (file name).
fn header_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Render a diff string (with +/- prefix lines) into styled ratatui Lines.
///
/// Handles formats:
/// - `+line` — added line (green)
/// - `-line` — deleted line (red)
/// - ` line` — context/unchanged line (dim)
/// - `@@ ... @@` — hunk header (cyan)
/// - Lines like `3+ 0-` — summary (dim)
/// - File headers like `--- a/file` / `+++ b/file`
pub fn render_diff_lines(diff_text: &str, max_lines: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut line_num_add: u32 = 0;
    let mut line_num_del: u32 = 0;

    for raw in diff_text.lines().take(max_lines) {
        if raw.starts_with("@@") {
            // Hunk header
            let hunk_style = Style::default().fg(Color::Cyan);
            lines.push(Line::from(Span::styled(format!("    {raw}"), hunk_style)));
            // Try to parse line numbers from @@ -N,M +N,M @@
            if let Some(nums) = parse_hunk_header(raw) {
                line_num_del = nums.0;
                line_num_add = nums.1;
            }
            continue;
        }

        if raw.starts_with("--- ") || raw.starts_with("+++ ") {
            lines.push(Line::from(Span::styled(format!("    {raw}"), header_style())));
            continue;
        }

        if raw.starts_with('+') {
            line_num_add += 1;
            let num = format!("{:>4} ", line_num_add);
            let content = &raw[1..];
            lines.push(Line::from(vec![
                Span::styled(num, gutter_style()),
                Span::styled("+ ", add_style()),
                Span::styled(content.to_string(), add_style()),
            ]));
        } else if raw.starts_with('-') {
            line_num_del += 1;
            let num = format!("{:>4} ", line_num_del);
            let content = &raw[1..];
            lines.push(Line::from(vec![
                Span::styled(num, gutter_style()),
                Span::styled("- ", del_style()),
                Span::styled(content.to_string(), del_style()),
            ]));
        } else if raw.starts_with(' ') {
            line_num_add += 1;
            line_num_del += 1;
            let num = format!("{:>4} ", line_num_add);
            let content = &raw[1..];
            lines.push(Line::from(vec![
                Span::styled(num, gutter_style()),
                Span::styled("  ", ctx_style()),
                Span::styled(content.to_string(), ctx_style()),
            ]));
        } else {
            // Summary lines like "3+ 0-" or other non-diff content
            lines.push(Line::from(Span::styled(
                format!("    {raw}"),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    if diff_text.lines().count() > max_lines {
        let remaining = diff_text.lines().count() - max_lines;
        lines.push(Line::from(Span::styled(
            format!("    … +{remaining} more lines"),
            Style::default().fg(Color::DarkGray),
        )));
    }

    lines
}

/// Parse hunk header `@@ -old_start,old_count +new_start,new_count @@`
fn parse_hunk_header(header: &str) -> Option<(u32, u32)> {
    let parts: Vec<&str> = header.split_whitespace().collect();
    let mut old_start = 0u32;
    let mut new_start = 0u32;
    for part in &parts {
        if let Some(s) = part.strip_prefix('-') {
            old_start = s.split(',').next()?.parse().ok()?;
        } else if let Some(s) = part.strip_prefix('+') {
            new_start = s.split(',').next()?.parse().ok()?;
        }
    }
    Some((old_start.saturating_sub(1), new_start.saturating_sub(1)))
}
