//! Inline slash-menu popup widget.
//!
//! Renders a compact dropdown list of filtered commands with their
//! descriptions. Selected row is highlighted. Long item lists scroll
//! around the selection so the active row stays visible.
//!
//! Layout per row (80-col example):
//! ```text
//!   /help         show help
//! ▌ /history     browse history
//!   /model        pick a model
//! ```
//! - 2-space gutter (or ▌ for the selected row)
//! - padded command name column (width = longest name)
//! - single space, then description (truncated with ellipsis if needed)

#![allow(dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::SlashMenu;

/// Maximum visible rows. If more items match, the window scrolls.
pub(crate) const MAX_VISIBLE_ROWS: u16 = 10;

/// Minimum height the popup should request from its parent (always at
/// least 1 row, even when empty, so "no matches" can be shown).
pub(crate) fn desired_height(menu: &SlashMenu) -> u16 {
    if menu.is_empty() {
        1
    } else {
        (menu.len() as u16).min(MAX_VISIBLE_ROWS)
    }
}

/// Render `menu` into `area` of `buf`. Safe to call with any area size;
/// rows beyond `area.height` are clipped.
pub(crate) fn render(menu: &SlashMenu, area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    if menu.is_empty() {
        let msg = Line::from(Span::styled(
            "  no matching commands",
            Style::default().add_modifier(Modifier::DIM),
        ));
        Paragraph::new(msg).render(area, buf);
        return;
    }

    // Fixed 2-space gutter ("  " or "▌ " for the selected row).
    let matches = menu.matches();
    let selected = menu.selected().unwrap_or(0);

    // Compute visible window around the selection so the active row
    // stays on-screen when the list is long.
    let max_rows = area.height.min(MAX_VISIBLE_ROWS) as usize;
    let (window_start, window_end) = window_around(selected, matches.len(), max_rows);

    // Width of the name column — pad to the widest visible command name
    // so descriptions align.
    let name_col_width = matches[window_start..window_end]
        .iter()
        .map(|i| i.name.len())
        .max()
        .unwrap_or(0);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(window_end - window_start);

    let theme = crate::tui::theme::current();

    for (idx, item) in matches[window_start..window_end].iter().enumerate() {
        let absolute = window_start + idx;
        let is_selected = absolute == selected;

        let gutter = if is_selected {
            Span::styled("▌ ", Style::default().fg(theme.gutter))
        } else {
            Span::raw("  ")
        };

        let padded_name = pad_right(item.name, name_col_width);
        let name_style = if is_selected {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.accent)
        };

        // Compute remaining width for the description column:
        //   total width − gutter (2) − name column − 2 separating spaces
        let desc_budget = (area.width as usize)
            .saturating_sub(2)
            .saturating_sub(name_col_width)
            .saturating_sub(2);
        let truncated_desc = truncate_ellipsis(item.description, desc_budget);

        let mut line = Line::from(vec![
            gutter,
            Span::styled(padded_name, name_style),
            Span::raw("  "),
            Span::styled(truncated_desc, Style::default().add_modifier(Modifier::DIM)),
        ]);
        // Cursor-style: the whole row gets a subtle tinted background
        // when selected so it stays visible even on wide terminals
        // where the left gutter is off-screen in peripheral vision.
        if is_selected {
            line = line.style(Style::default().bg(theme.selected_bg));
        }
        lines.push(line);
    }

    Paragraph::new(lines).render(area, buf);
}

/// Compute the half-open range `[start, end)` of items to show so that
/// `selected` remains visible within a window of at most `max_rows` rows.
fn window_around(selected: usize, total: usize, max_rows: usize) -> (usize, usize) {
    if total <= max_rows {
        return (0, total);
    }
    // Try to centre-ish: keep at most 2 rows above the selection.
    let above = 2.min(selected);
    let start = selected.saturating_sub(above);
    let end = (start + max_rows).min(total);
    // If we hit the bottom, pull the start up.
    let start = end.saturating_sub(max_rows);
    (start, end)
}

fn pad_right(s: &str, width: usize) -> String {
    if s.len() >= width {
        s.to_string()
    } else {
        let mut out = String::with_capacity(width);
        out.push_str(s);
        for _ in s.len()..width {
            out.push(' ');
        }
        out
    }
}

fn truncate_ellipsis(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if s.chars().count() <= width {
        return s.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut out: String = s.chars().take(width - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::super::{SlashItem, SlashMenu};
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    fn menu_fixture() -> SlashMenu {
        SlashMenu::new(vec![
            SlashItem {
                name: "/help",
                description: "show help",
            },
            SlashItem {
                name: "/history",
                description: "browse session history",
            },
            SlashItem {
                name: "/model",
                description: "pick a model",
            },
        ])
    }

    /// Tiny adapter widget so we can reuse the harness.
    struct PopupWidget<'a>(&'a SlashMenu);
    impl Widget for PopupWidget<'_> {
        fn render(self, area: Rect, buf: &mut Buffer) {
            render(self.0, area, buf);
        }
    }

    fn render_menu(menu: &SlashMenu, w: u16, h: u16) -> String {
        let buf = draw_widget(PopupWidget(menu), w, h);
        buffer_to_string(&buf)
    }

    #[test]
    fn snapshot_three_items_default_selection() {
        let menu = menu_fixture();
        insta::assert_snapshot!("slash_popup_three_default_80", render_menu(&menu, 80, 3));
    }

    #[test]
    fn snapshot_filtered_to_one() {
        let mut menu = menu_fixture();
        menu.set_filter("/he");
        insta::assert_snapshot!("slash_popup_filtered_he_80", render_menu(&menu, 80, 3));
    }

    #[test]
    fn snapshot_second_item_selected() {
        let mut menu = menu_fixture();
        menu.move_down();
        insta::assert_snapshot!("slash_popup_second_selected_80", render_menu(&menu, 80, 3));
    }

    #[test]
    fn snapshot_no_matches_shows_message() {
        let mut menu = menu_fixture();
        menu.set_filter("/zzz_no_match_here");
        insta::assert_snapshot!("slash_popup_no_matches_80", render_menu(&menu, 80, 2));
    }

    #[test]
    fn snapshot_narrow_truncates_description() {
        let menu = menu_fixture();
        insta::assert_snapshot!("slash_popup_narrow_28", render_menu(&menu, 28, 3));
    }

    #[test]
    fn snapshot_long_list_windows_around_selection() {
        // 12 items, max 10 visible — selecting #11 must scroll.
        let items: Vec<SlashItem> = (0..12)
            .map(|i| {
                // Leak a static-lifetime string via Box::leak — cheap in test.
                let name: &'static str = Box::leak(format!("/cmd{i:02}").into_boxed_str());
                let desc: &'static str = Box::leak(format!("description {i}").into_boxed_str());
                SlashItem {
                    name,
                    description: desc,
                }
            })
            .collect();
        let mut menu = SlashMenu::new(items);
        for _ in 0..11 {
            menu.move_down();
        }
        insta::assert_snapshot!("slash_popup_long_list_sel11_80", render_menu(&menu, 80, 10));
    }

    // ─── Non-snapshot unit tests ──────────────────────────────────

    #[test]
    fn desired_height_clamps_at_max() {
        let items: Vec<SlashItem> = (0..20)
            .map(|i| {
                let name: &'static str = Box::leak(format!("/x{i}").into_boxed_str());
                SlashItem {
                    name,
                    description: "",
                }
            })
            .collect();
        let menu = SlashMenu::new(items);
        assert_eq!(desired_height(&menu), MAX_VISIBLE_ROWS);
    }

    #[test]
    fn desired_height_minimum_one_for_empty_menu() {
        let mut menu = menu_fixture();
        menu.set_filter("/zzz");
        assert_eq!(desired_height(&menu), 1);
    }

    #[test]
    fn pad_right_and_truncate_behaviour() {
        assert_eq!(pad_right("abc", 5), "abc  ");
        assert_eq!(pad_right("abcdef", 3), "abcdef");
        assert_eq!(truncate_ellipsis("abcdef", 4), "abc…");
        assert_eq!(truncate_ellipsis("abc", 4), "abc");
        assert_eq!(truncate_ellipsis("abc", 1), "…");
        assert_eq!(truncate_ellipsis("abc", 0), "");
    }

    #[test]
    fn window_around_keeps_selection_visible() {
        assert_eq!(window_around(0, 3, 10), (0, 3));
        assert_eq!(window_around(5, 10, 10), (0, 10));
        assert_eq!(window_around(11, 12, 10), (2, 12));
        // Selection near start with big list: window pinned to top.
        assert_eq!(window_around(1, 20, 10), (0, 10));
    }
}
