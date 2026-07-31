//! Renders diff output with aligned old/new line numbers, gutter signs, and
//! full-row semantic surfaces.
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
use super::theme;

fn is_light_bg() -> bool {
    default_bg().is_some_and(is_light)
}

/// Style for an added line — reads from the current theme.
fn add_style() -> Style {
    theme::current().diff_add_style()
}

/// Style for a deleted line — reads from the current theme.
fn del_style() -> Style {
    theme::current().diff_del_style()
}

/// Style for context/unchanged lines.
fn ctx_style() -> Style {
    theme::current().diff_context_style()
}

/// Style for the gutter (line numbers + sign).
fn gutter_style() -> Style {
    let theme = theme::current();
    Style::default().fg(theme.dim)
}

/// Style for the diff header (file name).
fn header_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

const HEADER_INDENT: &str = "            ";

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
            let hunk_style = theme::current().diff_hunk_style();
            lines.push(Line::from(Span::styled(
                format!("{HEADER_INDENT}{raw}"),
                hunk_style,
            )));
            // Try to parse line numbers from @@ -N,M +N,M @@
            if let Some(nums) = parse_hunk_header(raw) {
                line_num_del = nums.0;
                line_num_add = nums.1;
            }
            continue;
        }

        if raw.starts_with("--- ") || raw.starts_with("+++ ") {
            current_lang = diff_header_language(raw);
            // Style file headers with path: directory dim, filename bright.
            let (prefix, path) = if let Some(path) = raw.strip_prefix("--- a/") {
                ("            --- a/", path)
            } else if let Some(path) = raw.strip_prefix("+++ b/") {
                ("            +++ b/", path)
            } else {
                // No recognised prefix — render as plain bold.
                lines.push(Line::from(Span::styled(
                    format!("{HEADER_INDENT}{raw}"),
                    header_style(),
                )));
                continue;
            };
            let mut spans = vec![Span::styled(
                prefix.to_string(),
                theme::current()
                    .diff_context_style()
                    .add_modifier(Modifier::BOLD),
            )];
            spans.extend(crate::tui::path_style::style_file_path(path));
            lines.push(Line::from(spans));
            continue;
        }

        if raw.starts_with('+') {
            line_num_add += 1;
            let content = &raw[1..];
            lines.push(render_content_line(
                None,
                Some(line_num_add),
                "+ ",
                content,
                add_style(),
                current_lang.as_deref(),
            ));
        } else if raw.starts_with('-') {
            line_num_del += 1;
            let content = &raw[1..];
            lines.push(render_content_line(
                Some(line_num_del),
                None,
                "- ",
                content,
                del_style(),
                current_lang.as_deref(),
            ));
        } else if raw.starts_with(' ') {
            line_num_add += 1;
            line_num_del += 1;
            let content = &raw[1..];
            lines.push(render_content_line(
                Some(line_num_del),
                Some(line_num_add),
                "  ",
                content,
                ctx_style(),
                current_lang.as_deref(),
            ));
        } else {
            // Summary lines like "3+ 0-" or other non-diff content
            if raw.starts_with("… +") {
                let theme = crate::tui::theme::current();
                let dim_style = Style::default().fg(theme.dim);
                lines.push(Line::from(vec![
                    Span::styled(format!("    {raw}"), dim_style),
                    Span::styled(
                        " (Ctrl+O to view transcript)".to_string(),
                        dim_style.add_modifier(ratatui::style::Modifier::ITALIC),
                    ),
                ]));
            } else {
                lines.push(Line::from(Span::styled(
                    format!("    {raw}"),
                    Style::default().fg(theme::current().dim),
                )));
            }
        }
    }

    let total_lines = diff_text.lines().count();
    if total_lines > max_lines {
        let remaining = total_lines - max_lines;
        let theme = crate::tui::theme::current();
        let dim_style = Style::default().fg(theme.dim);
        lines.push(Line::from(vec![
            Span::styled(format!("    … +{remaining} more lines"), dim_style),
            Span::styled(
                " (Ctrl+O to view transcript)".to_string(),
                dim_style.add_modifier(ratatui::style::Modifier::ITALIC),
            ),
        ]));
    }

    lines
}

fn render_content_line(
    old_number: Option<u32>,
    new_number: Option<u32>,
    prefix: &str,
    content: &str,
    style: Style,
    lang: Option<&str>,
) -> Line<'static> {
    let gutter = gutter_style().bg(style.bg.unwrap_or(Color::Reset));
    let old_number = old_number.map_or_else(String::new, |number| number.to_string());
    let new_number = new_number.map_or_else(String::new, |number| number.to_string());
    let mut spans = vec![
        Span::styled(format!("{old_number:>4} {new_number:>4} │ "), gutter),
        Span::styled(prefix.to_string(), style),
    ];
    spans.extend(highlighted_diff_content(content, lang, style).spans);
    let mut line = Line::from(spans);
    // Spans colour the occupied cells; the line owns the physical row. Buffer
    // and scrollback renderers use this metadata to extend the semantic add/
    // delete surface through the terminal edge without padding with spaces.
    if let Some(background) = style.bg {
        line.style = Style::default().bg(background);
    }
    line
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
    use crate::tui::render::line_utils::FullRowParagraph;
    use crate::tui::testing::render::draw_widget;
    use ratatui::widgets::Wrap;

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
            added.spans.iter().all(|span| span.style.bg == expected_bg),
            "every span in an added line should carry the diff background: {added:?}"
        );
        assert_eq!(added.style.bg, expected_bg);
    }

    #[test]
    fn old_and_new_line_numbers_form_a_stable_two_column_grid() {
        let lines = render_diff_lines("@@ -41,2 +99,2 @@\n-old\n+new\n same\n", 20);
        let text = |index: usize| {
            lines[index]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        assert!(text(1).starts_with("  41      │ - "), "{:?}", text(1));
        assert!(text(2).starts_with("       99 │ + "), "{:?}", text(2));
        assert!(text(3).starts_with("  42  100 │   "), "{:?}", text(3));
    }

    #[test]
    fn added_and_deleted_surfaces_fill_the_complete_physical_row() {
        let lines = render_diff_lines("@@ -1 +1 @@\n-old\n+new\n", 20);
        let area_width = 52;
        let buffer = draw_widget(
            FullRowParagraph::new(lines).wrap(Wrap { trim: false }),
            area_width,
            3,
        );
        let theme = crate::tui::theme::current();
        assert!((0..area_width).all(|x| buffer[(x, 1)].bg == theme.diff_del_bg));
        assert!((0..area_width).all(|x| buffer[(x, 2)].bg == theme.diff_add_bg));
    }
}
