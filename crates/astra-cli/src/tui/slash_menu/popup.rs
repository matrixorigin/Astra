//! Inline command discovery under the composer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use super::SlashMenu;
use crate::tui::history_cell::truncate_by_width;

const MAX_VISIBLE_ROWS: u16 = 10;

pub(crate) fn desired_composer_height(menu: &SlashMenu) -> u16 {
    (menu.len() as u16).clamp(1, MAX_VISIBLE_ROWS)
}

pub(crate) fn render_composer(menu: &SlashMenu, area: Rect, buf: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let theme = crate::tui::theme::current();
    let dim = Style::default().fg(theme.dim);
    let width = usize::from(area.width);
    let matches = menu.matches();
    if matches.is_empty() {
        let message = if width >= 22 {
            "  No matching commands"
        } else {
            "  No matches"
        };
        buf.set_line(area.x, area.y, &Line::styled(message, dim), area.width);
        return;
    }

    let selected = menu.selected().unwrap_or(0);
    let slots = area.height.min(MAX_VISIBLE_ROWS);
    let (start, end) = window_around(selected, matches.len(), usize::from(slots));
    let visible = &matches[start..end];
    let name_width = matches
        .iter()
        .map(|item| item.name.width())
        .max()
        .unwrap_or(0)
        .min(if width >= 44 {
            (width * 2 / 5).max(12)
        } else {
            width.saturating_sub(2)
        });
    let desc_width = width.saturating_sub(name_width + 4);
    let highlights = menu.match_indices();
    for (index, item) in visible.iter().enumerate() {
        let absolute = start + index;
        let focused = selected == absolute;
        let marker = if focused { "›" } else { " " };
        let marker_style = Style::default().fg(theme.accent).bold();
        let mut spans = vec![Span::styled(format!("{marker} "), marker_style)];
        let name = truncate_by_width(&item.name, name_width);
        spans.extend(render_name_spans(
            &name,
            highlights
                .get(absolute)
                .map_or(&[], |indices| indices.as_slice()),
            name_width,
            focused,
            theme,
        ));
        if desc_width >= 12 {
            let style = if focused {
                Style::default().fg(theme.selected_fg)
            } else {
                dim
            };
            spans.push(Span::styled(
                format!("  {}", truncate_by_width(&item.description, desc_width)),
                style,
            ));
        }
        let line = Line::from(spans);
        let rect = Rect::new(area.x, area.y + index as u16, area.width, 1);
        if focused {
            Widget::render(
                crate::tui::render::line_utils::FullRowParagraph::new(vec![
                    line.style(Style::default().bg(theme.selected_bg)),
                ]),
                rect,
                buf,
            );
        } else {
            line.render(rect, buf);
        }
    }
}

/// Split `name` into styled spans so that character indices in
/// `match_indices` render with the accent colour + UNDERLINE, and the
/// whole string is padded to `col_width` characters.
fn render_name_spans(
    name: &str,
    match_indices: &[u32],
    col_width: usize,
    is_selected: bool,
    theme: &crate::tui::theme::Theme,
) -> Vec<Span<'static>> {
    let base_name = if is_selected {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.accent)
    };
    let matched = base_name.add_modifier(Modifier::UNDERLINED);

    let mut spans: Vec<Span<'static>> = Vec::new();
    if match_indices.is_empty() {
        spans.push(Span::styled(name.to_string(), base_name));
    } else {
        // Walk char-by-char, grouping runs of (matched?).
        let mut cur = String::new();
        let mut cur_hit = false;
        for (i, ch) in name.chars().enumerate() {
            let hit = match_indices.contains(&(i as u32));
            if hit != cur_hit && !cur.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut cur),
                    if cur_hit { matched } else { base_name },
                ));
            }
            cur.push(ch);
            cur_hit = hit;
        }
        if !cur.is_empty() {
            spans.push(Span::styled(cur, if cur_hit { matched } else { base_name }));
        }
    }

    // Trailing padding to align into `col_width`.
    let name_width = name.width();
    if col_width > name_width {
        spans.push(Span::raw(" ".repeat(col_width - name_width)));
    }

    spans
}

/// Compute the half-open range `[start, end)` so the selected row stays
/// visible with at most `max_rows` slots. Centres the selection when
/// possible so the user sees context above AND below.
fn window_around(selected: usize, total: usize, max_rows: usize) -> (usize, usize) {
    if max_rows == 0 || total == 0 {
        return (0, 0);
    }
    if total <= max_rows {
        return (0, total);
    }
    // Keep ~half above when possible.
    let above = (max_rows / 2).min(selected);
    let start = selected.saturating_sub(above);
    let end = (start + max_rows).min(total);
    let start = end.saturating_sub(max_rows);
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::super::{SlashItem, SlashMenu};
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    fn menu_fixture() -> SlashMenu {
        SlashMenu::new(vec![
            SlashItem::simple("/help", "Show keyboard shortcuts"),
            SlashItem::simple("/history", "Open the session transcript"),
            SlashItem::simple("/model", "Choose a model and thinking depth"),
        ])
    }

    struct Picker<'a>(&'a SlashMenu);

    impl Widget for Picker<'_> {
        fn render(self, area: Rect, buf: &mut Buffer) {
            render_composer(self.0, area, buf);
        }
    }

    #[test]
    fn one_row_shows_the_selected_action() {
        let mut menu = menu_fixture();
        menu.go_last();
        let output = buffer_to_string(&draw_widget(Picker(&menu), 24, 1));
        assert!(output.contains("/model"), "{output}");
    }

    #[test]
    fn fuzzy_match_shows_only_the_matching_command() {
        let mut menu = menu_fixture();
        menu.set_filter("/hstry");
        assert_eq!(desired_composer_height(&menu), 1);
        let output = buffer_to_string(&draw_widget(Picker(&menu), 80, 1));
        assert!(output.contains("/history"), "{output}");
    }
}
