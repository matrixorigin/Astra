//! Inline `@`-mention popup widget.
//!
//! Visual language mirrors `slash_menu::popup`:
//! - cyan `▌` gutter on the selected row,
//! - kind indicator (`▸` for directories, ` ` for files),
//! - path column sized to the widest visible entry,
//! - width-aware `…` truncation when the terminal is narrow,
//! - empty-menu message in dim style.

#![allow(dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::provider::FileKind;
use super::{FileEntry, MentionMenu};

pub(crate) const MAX_VISIBLE_ROWS: u16 = 10;

pub(crate) fn desired_height(menu: &MentionMenu) -> u16 {
    if menu.is_empty() {
        1
    } else {
        (menu.len() as u16).min(MAX_VISIBLE_ROWS)
    }
}

pub(crate) fn render(menu: &MentionMenu, area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    if menu.is_empty() {
        let msg = Line::from(Span::styled(
            "  no matching files",
            Style::default().add_modifier(Modifier::DIM),
        ));
        Paragraph::new(msg).render(area, buf);
        return;
    }

    let matches = menu.matches();
    let selected = menu.selected().unwrap_or(0);
    let max_rows = area.height.min(MAX_VISIBLE_ROWS) as usize;
    let (window_start, window_end) = window_around(selected, matches.len(), max_rows);

    let path_col_width = matches[window_start..window_end]
        .iter()
        .map(|e| e.path.len())
        .max()
        .unwrap_or(0);

    let theme = crate::tui::theme::current();
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(window_end - window_start);
    for (idx, entry) in matches[window_start..window_end].iter().enumerate() {
        let absolute = window_start + idx;
        let is_selected = absolute == selected;

        let gutter = if is_selected {
            Span::styled("▌ ", Style::default().fg(theme.gutter))
        } else {
            Span::raw("  ")
        };

        let (kind_glyph, kind_style) = match entry.kind {
            FileKind::Dir => ("▸ ", Style::default().fg(Color::Blue)),
            FileKind::File => ("  ", Style::default()),
        };

        let padded_path = pad_right(&entry.path, path_col_width);
        let name_style = if is_selected {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        // Width budget: gutter(2) + kind(2) + path + trailing '  ' = 6
        let _budget = (area.width as usize)
            .saturating_sub(2)
            .saturating_sub(2)
            .saturating_sub(path_col_width);

        let path_visible = truncate_ellipsis(&padded_path, (area.width as usize).saturating_sub(4));

        let mut line = Line::from(vec![
            gutter,
            Span::styled(kind_glyph, kind_style),
            Span::styled(path_visible, name_style),
        ]);
        if is_selected {
            line = line.style(Style::default().bg(theme.selected_bg));
        }
        lines.push(line);
    }

    Paragraph::new(lines).render(area, buf);
}

fn window_around(selected: usize, total: usize, max_rows: usize) -> (usize, usize) {
    if total <= max_rows {
        return (0, total);
    }
    let above = 2.min(selected);
    let start = selected.saturating_sub(above);
    let end = (start + max_rows).min(total);
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
    let out: String = s.chars().take(width - 1).collect();
    format!("{out}…")
}

/// Produce the text to splice into the composer for a selected entry.
/// - Files:      `@path `  (trailing space so the user can keep typing)
/// - Directories: `@path/` (no space — encourages continued path entry)
pub(crate) fn format_replacement(entry: &FileEntry) -> String {
    match entry.kind {
        FileKind::Dir => format!("@{}/", entry.path.trim_end_matches('/')),
        FileKind::File => format!("@{} ", entry.path),
    }
}

#[cfg(test)]
mod tests {
    use super::super::MentionToken;
    use super::super::provider::{FileKind, StaticFileProvider};
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    fn fixture_menu() -> MentionMenu {
        let mut menu = MentionMenu::new(StaticFileProvider::with_root_listing(&[
            ("src", FileKind::Dir),
            ("tests", FileKind::Dir),
            ("Cargo.toml", FileKind::File),
            ("README.md", FileKind::File),
        ]));
        menu.set_token(&MentionToken {
            at_byte: 0,
            end_byte: 1,
            partial: String::new(),
        });
        menu
    }

    struct PopupWidget<'a>(&'a MentionMenu);
    impl Widget for PopupWidget<'_> {
        fn render(self, area: Rect, buf: &mut Buffer) {
            render(self.0, area, buf);
        }
    }

    fn render_menu(menu: &MentionMenu, w: u16, h: u16) -> String {
        let buf = draw_widget(PopupWidget(menu), w, h);
        buffer_to_string(&buf)
    }

    #[test]
    fn snapshot_default_four_entries() {
        let menu = fixture_menu();
        crate::tui::testing::assert_tui_snapshot!(
            "mention_popup_default_80",
            render_menu(&menu, 80, 4)
        );
    }

    #[test]
    fn snapshot_file_selected() {
        let mut menu = fixture_menu();
        // Skip past the two dirs to land on a file.
        menu.move_down();
        menu.move_down();
        crate::tui::testing::assert_tui_snapshot!(
            "mention_popup_file_selected_80",
            render_menu(&menu, 80, 4)
        );
    }

    #[test]
    fn snapshot_filtered_single() {
        let mut menu = fixture_menu();
        menu.set_token(&MentionToken {
            at_byte: 0,
            end_byte: 5,
            partial: "rea".into(),
        });
        crate::tui::testing::assert_tui_snapshot!(
            "mention_popup_filtered_rea_80",
            render_menu(&menu, 80, 2)
        );
    }

    #[test]
    fn snapshot_no_matches() {
        let mut menu = fixture_menu();
        menu.set_token(&MentionToken {
            at_byte: 0,
            end_byte: 10,
            partial: "zzz_no_match".into(),
        });
        crate::tui::testing::assert_tui_snapshot!(
            "mention_popup_no_matches_80",
            render_menu(&menu, 80, 2)
        );
    }

    #[test]
    fn snapshot_narrow_truncates() {
        let long_name = "a_very_long_filename_that_wont_fit.rs";
        let mut menu = MentionMenu::new(StaticFileProvider::with_root_listing(&[(
            long_name,
            FileKind::File,
        )]));
        menu.set_token(&MentionToken {
            at_byte: 0,
            end_byte: 1,
            partial: String::new(),
        });
        crate::tui::testing::assert_tui_snapshot!(
            "mention_popup_narrow_20",
            render_menu(&menu, 20, 2)
        );
    }

    // ─── Pure unit tests ──────────────────────────────────────────

    #[test]
    fn format_replacement_uses_trailing_slash_for_dirs() {
        let dir = FileEntry {
            path: "src".into(),
            kind: FileKind::Dir,
        };
        let file = FileEntry {
            path: "Cargo.toml".into(),
            kind: FileKind::File,
        };
        assert_eq!(format_replacement(&dir), "@src/");
        assert_eq!(format_replacement(&file), "@Cargo.toml ");
    }

    #[test]
    fn desired_height_rules() {
        let menu = fixture_menu();
        assert_eq!(desired_height(&menu), 4);

        let mut empty = fixture_menu();
        empty.set_token(&MentionToken {
            at_byte: 0,
            end_byte: 10,
            partial: "xyz".into(),
        });
        assert_eq!(desired_height(&empty), 1);
    }
}
