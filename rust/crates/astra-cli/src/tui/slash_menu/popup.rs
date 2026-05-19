//! Inline slash-menu popup widget.
//!
//! Renders a lightweight dropdown of filtered commands directly above
//! the composer, matching the style of claude-code and cursor: no
//! decorative border, no title, no footer — just the list of commands.
//!
//! Visual anatomy (80-col example, 5 matches):
//! ```text
//!   ▌ /help        show this help screen
//!     /history     browse session history
//!     /model       pick a model
//!     ↓ 2 more
//! ```
//!
//! Key features:
//! * **Group headers**: items are grouped by category (Core, Session & Plan, Observability, …)
//!   with dim `── Group Name ──` dividers when the filtered set spans multiple groups.
//! * **Matched characters** (from the filter token) render in the
//!   accent colour with UNDERLINE so the user can see why the row ranked.
//! * **Scroll hints**: `↑ N more` / `↓ N more` above/below the window
//!   when the list overflows.
//! * **Aliases**: shown inline as a dim `(h)` badge right of the name.
//! * **Responsive**: at <18 cols descriptions drop and only names render.
//!
//! Group-aware anatomy:
//! ```text
//!     ── Core ──
//!   ▌ /help        show this help screen
//!     /clear       clear screen
//!     ── Session & Plan ──
//!     /resume      resume a session
//!     ↓ 2 more
//! ```

#![allow(dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::SlashMenu;
use crate::command_registry::CommandGroup;

/// Maximum number of **data rows** shown at once. Does not include the
/// scroll-indicator rows (↑ N more / ↓ N more).
pub(crate) const MAX_VISIBLE_ROWS: u16 = 10;

/// Minimum width at which descriptions are shown. Below this we only
/// render names.
const MIN_DESC_WIDTH: u16 = 18;

/// Height the popup would like to occupy for the given menu.
///
/// Just the visible rows — no border, no header, no footer. Always at
/// least 1 row so the "no matches" line fits.
pub(crate) fn desired_height(menu: &SlashMenu) -> u16 {
    if menu.is_empty() {
        1 // "no matching commands"
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

    let theme = crate::tui::theme::current();

    if menu.is_empty() {
        render_empty(area, buf, theme);
        return;
    }

    // Decide how many rows the body gets.
    let body_height = area.height.min(MAX_VISIBLE_ROWS);

    let matches = menu.matches();
    let selected = menu.selected().unwrap_or(0);

    // Compute which window of items is visible, keeping the selection
    // roughly centred.
    let (window_start, window_end) = window_around(selected, matches.len(), body_height as usize);

    // Reserve one body-row for each scroll indicator if needed.
    let need_top_hint = window_start > 0;
    let need_bot_hint = window_end < matches.len();
    let hint_rows = need_top_hint as usize + need_bot_hint as usize;
    let visible_slots = (body_height as usize).saturating_sub(hint_rows);

    // Re-compute window so that, after reserving hint rows, the selected
    // item is inside `[window_start + top_hint, window_start + top_hint + visible_slots)`.
    let (window_start, window_end) = window_around(selected, matches.len(), visible_slots.max(1));
    let need_top_hint = window_start > 0;
    let need_bot_hint = window_end < matches.len();

    // Width of the name column — pad to the widest visible name so
    // descriptions align cleanly.
    let name_col_width = matches[window_start..window_end]
        .iter()
        .map(|i| i.name.chars().count())
        .max()
        .unwrap_or(0);

    // Layout columns: gutter (2) + name_col + 2 + alias_col + 2 + desc.
    let alias_col_width = matches[window_start..window_end]
        .iter()
        .map(|i| alias_label(i).map(|s| s.chars().count()).unwrap_or(0))
        .max()
        .unwrap_or(0);

    let content_width = area.width as usize;
    let show_desc = area.width >= MIN_DESC_WIDTH;
    let gutter_w = 2;
    let sep_w = 2;
    let alias_budget = if alias_col_width > 0 {
        alias_col_width + sep_w
    } else {
        0
    };
    let desc_budget = if show_desc {
        content_width
            .saturating_sub(gutter_w)
            .saturating_sub(name_col_width)
            .saturating_sub(sep_w)
            .saturating_sub(alias_budget)
    } else {
        0
    };

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(area.height as usize);

    // ── Scroll-up hint ───────────────────────────────────────────
    if need_top_hint {
        lines.push(scroll_hint(format!("↑ {} more", window_start), theme));
    }

    // ── Visible data rows (with group headers) ─────────────────
    let highlights = menu.match_indices();
    let prev_group_idx = matches
        .get(window_start.saturating_sub(1))
        .and_then(|it| it.group);
    let mut cur_group: Option<CommandGroup> = prev_group_idx;

    for (idx, item) in matches[window_start..window_end].iter().enumerate() {
        let absolute = window_start + idx;
        let is_selected = absolute == selected;
        let hi = highlights.get(absolute).cloned().unwrap_or_default();

        // Insert group header when group changes.
        let item_group = item.group;
        if item_group != cur_group {
            cur_group = item_group;
            if let Some(g) = item_group {
                let label = group_display_name(g);
                // Ensure the gutter column is included.
                let mut group_spans: Vec<Span<'static>> = Vec::with_capacity(3);
                group_spans.push(Span::raw("  "));
                group_spans.push(Span::styled(
                    format!("── {label} ──"),
                    Style::default()
                        .fg(theme.accent_dim())
                        .add_modifier(Modifier::ITALIC)
                        .add_modifier(Modifier::DIM),
                ));
                // Add the rest as separator.
                group_spans.push(Span::styled(
                    "─".repeat(
                        content_width
                            .saturating_sub(label.chars().count() + 8)
                            .min(80),
                    ),
                    Style::default().fg(theme.dim).add_modifier(Modifier::DIM),
                ));
                lines.push(Line::from(group_spans));
            }
        }

        let mut spans: Vec<Span<'static>> = Vec::with_capacity(8);

        // Gutter / cursor.
        spans.push(if is_selected {
            Span::styled("▌ ", Style::default().fg(theme.gutter))
        } else {
            Span::raw("  ")
        });

        // Name — with per-char highlights.
        spans.extend(render_name_spans(
            item.name,
            &hi,
            name_col_width,
            is_selected,
            theme,
        ));

        // Alias column.
        if alias_col_width > 0 {
            spans.push(Span::raw("  "));
            let alias = alias_label(item).unwrap_or_default();
            spans.push(Span::styled(
                pad_right_chars(&alias, alias_col_width),
                Style::default()
                    .fg(theme.dim)
                    .add_modifier(Modifier::ITALIC),
            ));
        }

        // Description column.
        if show_desc && desc_budget > 0 {
            spans.push(Span::raw("  "));
            let desc = truncate_ellipsis(item.description, desc_budget);
            let desc_style = if is_selected {
                Style::default().fg(theme.selected_fg)
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };
            spans.push(Span::styled(desc, desc_style));
        }

        let mut line = Line::from(spans);
        if is_selected {
            line = line.style(Style::default().bg(theme.selected_bg));
        }
        lines.push(line);
        // Show subcommands below the selected item (single-line, compact).
        if is_selected && !item.subcommands.is_empty() {
            let max_subs = 3.min(item.subcommands.len());
            for (name, desc) in item.subcommands.iter().take(max_subs) {
                let mut sub_spans: Vec<Span<'static>> = Vec::new();
                sub_spans.push(Span::raw("   · "));
                sub_spans.push(Span::styled(
                    format!("{name}"),
                    Style::default()
                        .fg(theme.accent_dim())
                        .add_modifier(Modifier::ITALIC),
                ));
                sub_spans.push(Span::styled(
                    format!(" — {desc}"),
                    Style::default().fg(theme.dim).add_modifier(Modifier::DIM),
                ));
                lines.push(Line::from(sub_spans));
            }
            if item.subcommands.len() > max_subs {
                lines.push(Line::from(Span::styled(
                    format!("   … {} more", item.subcommands.len() - max_subs),
                    Style::default().fg(theme.dim).add_modifier(Modifier::DIM),
                )));
            }
        }
    }

    // ── Scroll-down hint ────────────────────────────────────────
    if need_bot_hint {
        let remaining = matches.len() - window_end;
        lines.push(scroll_hint(format!("↓ {} more", remaining), theme));
    }

    Paragraph::new(lines).render(area, buf);
}

// ─── Helpers ────────────────────────────────────────────────────────

fn render_empty(area: Rect, buf: &mut Buffer, theme: &crate::tui::theme::Theme) {
    let lines = vec![Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "no matching commands — try a shorter prefix",
            Style::default().fg(theme.dim).add_modifier(Modifier::DIM),
        ),
    ])];
    Paragraph::new(lines).render(area, buf);
}

fn scroll_hint(label: String, theme: &crate::tui::theme::Theme) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            label,
            Style::default()
                .fg(theme.accent_dim())
                .add_modifier(Modifier::ITALIC),
        ),
    ])
}

/// Split `name` into styled spans so that bytes whose offsets appear in
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
        let set: std::collections::HashSet<u32> = match_indices.iter().copied().collect();
        for (i, ch) in name.char_indices() {
            let hit = set.contains(&(i as u32));
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
    let name_chars = name.chars().count();
    if col_width > name_chars {
        spans.push(Span::raw(" ".repeat(col_width - name_chars)));
    }

    spans
}

fn alias_label(item: &super::SlashItem) -> Option<String> {
    if item.aliases.is_empty() {
        None
    } else {
        Some(format!("({})", item.aliases.join(", ")))
    }
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

fn pad_right_chars(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + (width - n));
    out.push_str(s);
    for _ in n..width {
        out.push(' ');
    }
    out
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

/// Human-readable display name for a command group.
fn group_display_name(group: CommandGroup) -> &'static str {
    match group {
        CommandGroup::Core => "Core",
        CommandGroup::Workspace => "Workspace",
        CommandGroup::SessionPlan => "Session & Plan",
        CommandGroup::MemoryTasks => "Memory & Tasks",
        CommandGroup::Observability => "Observability",
        CommandGroup::Skills => "Skills",
        CommandGroup::Mcp => "MCP",
        CommandGroup::TeamAccount => "Team & Account",
        CommandGroup::System => "System",
    }
}

#[cfg(test)]
mod tests {
    use super::super::{SlashItem, SlashMenu};
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    fn menu_fixture() -> SlashMenu {
        SlashMenu::new(vec![
            SlashItem::simple("/help", "show help"),
            SlashItem::simple("/history", "browse session history"),
            SlashItem::simple("/model", "pick a model"),
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
        insta::assert_snapshot!("slash_popup_three_default_80", render_menu(&menu, 80, 5));
    }

    #[test]
    fn snapshot_filtered_to_one() {
        let mut menu = menu_fixture();
        menu.set_filter("/he");
        insta::assert_snapshot!("slash_popup_filtered_he_80", render_menu(&menu, 80, 5));
    }

    #[test]
    fn snapshot_second_item_selected() {
        let mut menu = menu_fixture();
        menu.move_down();
        insta::assert_snapshot!("slash_popup_second_selected_80", render_menu(&menu, 80, 5));
    }

    #[test]
    fn snapshot_no_matches_shows_message() {
        let mut menu = menu_fixture();
        menu.set_filter("/zzz_no_match_here");
        insta::assert_snapshot!("slash_popup_no_matches_80", render_menu(&menu, 80, 3));
    }

    #[test]
    fn snapshot_narrow_truncates_description() {
        let menu = menu_fixture();
        insta::assert_snapshot!("slash_popup_narrow_28", render_menu(&menu, 28, 5));
    }

    #[test]
    fn snapshot_long_list_windows_around_selection() {
        // 12 items, body fits 10 — selecting #11 must scroll AND show
        // the "↑ N more" hint at the top.
        let items: Vec<SlashItem> = (0..12)
            .map(|i| {
                let name: &'static str = Box::leak(format!("/cmd{i:02}").into_boxed_str());
                let desc: &'static str = Box::leak(format!("description {i}").into_boxed_str());
                SlashItem::simple(name, desc)
            })
            .collect();
        let mut menu = SlashMenu::new(items);
        for _ in 0..11 {
            menu.move_down();
        }
        insta::assert_snapshot!("slash_popup_long_list_sel11_80", render_menu(&menu, 80, 12));
    }

    #[test]
    fn snapshot_groups_with_headers() {
        // 6 items across 3 groups — should render group headers.
        use crate::command_registry::CommandGroup;
        let items: Vec<SlashItem> = vec![
            SlashItem {
                name: "/help",
                description: "show help",
                group: Some(CommandGroup::Core),
                ..Default::default()
            },
            SlashItem {
                name: "/clear",
                description: "clear screen",
                group: Some(CommandGroup::Core),
                ..Default::default()
            },
            SlashItem {
                name: "/resume",
                description: "resume a session",
                group: Some(CommandGroup::SessionPlan),
                ..Default::default()
            },
            SlashItem {
                name: "/plan",
                description: "manage plan",
                group: Some(CommandGroup::SessionPlan),
                ..Default::default()
            },
            SlashItem {
                name: "/model",
                description: "pick a model",
                group: Some(CommandGroup::Core),
                ..Default::default()
            },
            SlashItem {
                name: "/config",
                description: "runtime config",
                group: Some(CommandGroup::Observability),
                ..Default::default()
            },
        ];
        let menu = SlashMenu::new(items);
        insta::assert_snapshot!(
            "slash_popup_groups_with_headers_80",
            render_menu(&menu, 80, 10)
        );
    }

    #[test]
    fn desired_height_clamps_at_max_plus_chrome() {
        let items: Vec<SlashItem> = (0..20)
            .map(|i| {
                let name: &'static str = Box::leak(format!("/x{i}").into_boxed_str());
                SlashItem::simple(name, "")
            })
            .collect();
        let menu = SlashMenu::new(items);
        assert_eq!(desired_height(&menu), MAX_VISIBLE_ROWS);
    }

    #[test]
    fn desired_height_minimum_for_empty_menu() {
        let mut menu = menu_fixture();
        menu.set_filter("/zzz");
        assert_eq!(desired_height(&menu), 1);
    }

    #[test]
    fn pad_and_truncate_behaviour() {
        assert_eq!(pad_right_chars("abc", 5), "abc  ");
        assert_eq!(pad_right_chars("abcdef", 3), "abcdef");
        assert_eq!(truncate_ellipsis("abcdef", 4), "abc…");
        assert_eq!(truncate_ellipsis("abc", 4), "abc");
        assert_eq!(truncate_ellipsis("abc", 1), "…");
        assert_eq!(truncate_ellipsis("abc", 0), "");
    }

    #[test]
    fn window_around_keeps_selection_visible_and_centred() {
        // Small list → full window.
        assert_eq!(window_around(0, 3, 10), (0, 3));
        // Big list, selection at end → window pinned to bottom.
        assert_eq!(window_around(11, 12, 10), (2, 12));
        // Selection in middle centres roughly.
        let (s, e) = window_around(5, 20, 10);
        assert_eq!(e - s, 10);
        assert!(s <= 5 && 5 < e);
        // Edge: empty list or zero rows.
        assert_eq!(window_around(0, 0, 10), (0, 0));
        assert_eq!(window_around(0, 10, 0), (0, 0));
    }

    #[test]
    fn narrow_terminal_falls_back_to_chromeless() {
        // Width below MIN_CHROME_WIDTH should still render something.
        let menu = menu_fixture();
        let out = render_menu(&menu, 14, 5);
        // No border glyphs.
        assert!(
            !out.contains('╭'),
            "narrow popup should not draw box: {out}"
        );
        assert!(out.contains("/help"), "name must still render: {out}");
    }

    #[test]
    fn selected_item_shows_subcommands() {
        use crate::command_registry::TuiHandler;

        const SUBS: &[(&str, &str)] = &[
            ("list", "List all memories"),
            ("search", "Search memories by query"),
            ("inspect", "Inspect a single memory by ID"),
        ];
        let item = SlashItem {
            name: "/memory",
            description: "Memory operations",
            subcommands: SUBS,
            aliases: &[],
            usage_boost: 0,
            group: Some(CommandGroup::MemoryTasks),
            tui_handler: TuiHandler::Panel,
            usage_examples: &[],
        };
        let menu = SlashMenu::new(vec![item]);
        let output = render_menu(&menu, 80, 10);
        assert!(
            output.contains("list"),
            "should show 'list' subcommand: {output}"
        );
        assert!(
            output.contains("Search memories"),
            "should show 'search' subcommand desc: {output}"
        );
    }
}
