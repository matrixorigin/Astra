//! Renders diff output with line numbers, gutter signs, and colors.
//!
//! Inspired by Codex's diff_render.rs (MIT) but simplified for astra's
//! use case: tool output strings with +/- prefix lines rather than
//! structured FileChange objects.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::diff_utils::parse_hunk_header;

use super::color::is_light;
use super::render::highlight::highlight_code_to_lines;
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
    default_bg().is_some_and(is_light)
}

/// Style for an added line.
fn add_style() -> Style {
    if is_light_bg() {
        Style::default().fg(LIGHT_ADD_FG).bg(LIGHT_ADD_BG)
    } else {
        Style::default().fg(DARK_ADD_FG).bg(DARK_ADD_BG)
    }
}

/// Style for a deleted line.
fn del_style() -> Style {
    if is_light_bg() {
        Style::default().fg(LIGHT_DEL_FG).bg(LIGHT_DEL_BG)
    } else {
        Style::default().fg(DARK_DEL_FG).bg(DARK_DEL_BG)
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
    let mut current_lang: Option<String> = None;

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
            current_lang = diff_header_language(raw);
            lines.push(Line::from(Span::styled(
                format!("    {raw}"),
                header_style(),
            )));
            continue;
        }

        if raw.starts_with('+') {
            line_num_add += 1;
            let num = format!("{:>4} ", line_num_add);
            let content = &raw[1..];
            lines.push(render_content_line(
                &num,
                "+ ",
                content,
                add_style(),
                current_lang.as_deref(),
            ));
        } else if raw.starts_with('-') {
            line_num_del += 1;
            let num = format!("{:>4} ", line_num_del);
            let content = &raw[1..];
            lines.push(render_content_line(
                &num,
                "- ",
                content,
                del_style(),
                current_lang.as_deref(),
            ));
        } else if raw.starts_with(' ') {
            line_num_add += 1;
            line_num_del += 1;
            let num = format!("{:>4} ", line_num_add);
            let content = &raw[1..];
            lines.push(render_content_line(
                &num,
                "  ",
                content,
                ctx_style(),
                current_lang.as_deref(),
            ));
        } else {
            // Summary lines like "3+ 0-" or other non-diff content
            let rendered = if raw.starts_with("… +") {
                format!("    {raw} · Ctrl+O transcript")
            } else {
                format!("    {raw}")
            };
            lines.push(Line::from(Span::styled(
                rendered,
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    let total_lines = diff_text.lines().count();
    if total_lines > max_lines {
        let remaining = total_lines - max_lines;
        lines.push(Line::from(Span::styled(
            format!("    … +{remaining} more lines · Ctrl+O transcript"),
            Style::default().fg(Color::DarkGray),
        )));
    }

    lines
}

fn render_content_line(
    number: &str,
    prefix: &str,
    content: &str,
    style: Style,
    lang: Option<&str>,
) -> Line<'static> {
    let gutter = gutter_style().bg(style.bg.unwrap_or(Color::Reset));
    let mut spans = vec![
        Span::styled(number.to_string(), gutter),
        Span::styled(prefix.to_string(), style),
    ];
    spans.extend(highlighted_diff_content(content, lang, style).spans);
    Line::from(spans)
}

fn highlighted_diff_content(content: &str, lang: Option<&str>, style: Style) -> Line<'static> {
    let line = lang
        .and_then(|lang| {
            let mut highlighted = highlight_code_to_lines(content, lang).into_iter();
            highlighted.next()
        })
        .unwrap_or_else(|| Line::raw(content.to_string()));
    let bg = style.bg.unwrap_or(Color::Reset);
    let fg = style.fg.unwrap_or(Color::Reset);
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|span| {
            let mut span_style = span.style.bg(bg);
            if span.style.fg.is_none() {
                span_style = span_style.fg(fg);
            }
            Span::styled(span.content.into_owned(), span_style)
        })
        .collect();
    Line::from(spans)
}

fn diff_header_language(header: &str) -> Option<String> {
    let path = header
        .strip_prefix("--- ")
        .or_else(|| header.strip_prefix("+++ "))?
        .split_whitespace()
        .next()?;
    if path == "/dev/null" {
        return None;
    }
    let path = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{add_style, diff_header_language, render_diff_lines};

    #[test]
    fn diff_headers_set_language_for_highlighted_content() {
        let lines = render_diff_lines(
            "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-fn old_name() {}\n+fn new_name() {}\n",
            20,
        );

        assert!(lines[4].spans.len() > 3, "expected highlighted code spans");
    }

    #[test]
    fn dev_null_headers_disable_language_detection() {
        assert_eq!(diff_header_language("--- /dev/null"), None);
    }

    #[test]
    fn added_line_background_applies_to_every_span() {
        let lines = render_diff_lines(
            "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n+fn new_name() {}\n",
            20,
        );
        let added = &lines[3];
        let expected_bg = add_style().bg;
        assert!(
            added
                .spans
                .iter()
                .all(|span| span.style.bg == expected_bg),
            "every span in an added line should carry the diff background: {added:?}"
        );
    }
}
