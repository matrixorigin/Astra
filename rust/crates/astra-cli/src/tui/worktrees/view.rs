//! Two-pane worktree widget.
//!
//! ```text
//! ┌ Worktrees (3) ──────────────────────────────────────────────────────┐
//! │▌ ⎇ main @ 616d4cf        12 sessions · last 2h ago                   │
//! │  ⎇ enhance_tui @ c823abc  3 sessions · last now                      │
//! │  (detached @ deadbee)     0 sessions                                 │
//! │                                                                      │
//! │  path: /home/xp/astra                                                │
//! │  branch: main @ 616d4cf                                              │
//! │  sessions: 12 (most recent 2h ago)                                   │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```

#![allow(dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use super::WorktreeList;

const MIN_SPLIT_WIDTH: u16 = 80;
pub(crate) const MAX_VISIBLE_ROWS: u16 = 12;

pub(crate) fn desired_height(list: &WorktreeList) -> u16 {
    if list.is_empty() {
        return 3;
    }
    let rows = (list.len() as u16).clamp(2, MAX_VISIBLE_ROWS);
    rows + 2
}

pub(crate) fn render(list: &WorktreeList, area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Line::from(Span::styled(
            format!(" Worktrees ({}) ", list.len()),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
    let inner = outer.inner(area);
    outer.render(area, buf);

    if list.is_empty() {
        Paragraph::new(Line::from(Span::styled(
            "  no worktrees for this repo",
            Style::default().add_modifier(Modifier::DIM),
        )))
        .render(inner, buf);
        return;
    }

    if inner.width < MIN_SPLIT_WIDTH {
        render_list(list, inner, buf);
        return;
    }

    let chunks = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(inner);
    render_list(list, chunks[0], buf);
    render_detail(list, chunks[1], buf);
}

fn render_list(list: &WorktreeList, area: Rect, buf: &mut Buffer) {
    let dim = Style::default().fg(Color::DarkGray);
    let theme = crate::tui::theme::current();
    let sel_idx = list.selected().unwrap_or(0);
    let mut lines = Vec::with_capacity(list.len());
    for (i, e) in list.entries().iter().enumerate() {
        let is_sel = i == sel_idx;
        let gutter = if is_sel {
            Span::styled("▌ ", Style::default().fg(theme.gutter))
        } else {
            Span::raw("  ")
        };
        let label_style = if is_sel {
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
        } else if e.is_bare {
            Style::default().add_modifier(Modifier::DIM)
        } else if e.is_detached {
            Style::default().fg(theme.warn)
        } else {
            Style::default()
        };
        let label = e.label();
        let sessions = if e.session_count == 0 {
            "no sessions".to_string()
        } else {
            format!("{} sessions", e.session_count)
        };
        let mut line = Line::from(vec![
            gutter,
            Span::styled(format!("{label:<28}"), label_style),
            Span::raw(" "),
            Span::styled(sessions, dim),
        ]);
        if is_sel {
            line = line.style(Style::default().bg(theme.selected_bg));
        }
        lines.push(line);
    }
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(area, buf);
}

fn render_detail(list: &WorktreeList, area: Rect, buf: &mut Buffer) {
    let Some(e) = list.selected_entry() else {
        return;
    };
    let label = Style::default().fg(Color::Cyan);
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        e.label(),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("path    ", label),
        Span::styled(e.path.clone(), dim),
    ]));
    if let Some(ref b) = e.branch {
        lines.push(Line::from(vec![
            Span::styled("branch  ", label),
            Span::styled(b.clone(), dim),
        ]));
    }
    if let Some(ref h) = e.head {
        lines.push(Line::from(vec![
            Span::styled("head    ", label),
            Span::styled(h.clone(), dim),
        ]));
    }
    if e.is_bare {
        lines.push(Line::from(Span::styled("bare repo", Color::Yellow)));
    }
    if e.is_detached {
        lines.push(Line::from(Span::styled("detached head", Color::Yellow)));
    }
    lines.push(Line::from(vec![
        Span::styled("sessions", label),
        Span::raw(" "),
        Span::styled(e.session_count.to_string(), dim),
    ]));
    if let Some(ref when) = e.last_session_at {
        lines.push(Line::from(vec![
            Span::styled("most    ", label),
            Span::styled(when.chars().take(19).collect::<String>(), dim),
        ]));
    }
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(area, buf);
}

#[cfg(test)]
mod tests {
    use super::super::WorktreeList;
    use super::super::model::{WorktreeEntry, parse};
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    fn enrich(mut v: Vec<WorktreeEntry>, counts: &[usize]) -> Vec<WorktreeEntry> {
        for (i, e) in v.iter_mut().enumerate() {
            if let Some(&c) = counts.get(i) {
                e.session_count = c;
            }
        }
        v
    }

    fn fixture() -> WorktreeList {
        let raw = "\
worktree /home/xp/astra
HEAD 616d4cf81abc
branch refs/heads/main

worktree /home/xp/astra-wt-a
HEAD c823abc456de
branch refs/heads/enhance_tui

worktree /home/xp/astra-wt-detached
HEAD deadbeef12345
detached
";
        let v = enrich(parse(raw), &[12, 3, 0]);
        WorktreeList::new(v)
    }

    struct W<'a>(&'a WorktreeList);
    impl Widget for W<'_> {
        fn render(self, area: Rect, buf: &mut Buffer) {
            render(self.0, area, buf);
        }
    }

    fn draw(l: &WorktreeList, w: u16, h: u16) -> String {
        let buf = draw_widget(W(l), w, h);
        buffer_to_string(&buf)
    }

    #[test]
    fn snapshot_three_worktrees_default() {
        let l = fixture();
        insta::assert_snapshot!("worktrees_three_100x7", draw(&l, 100, 7));
    }

    #[test]
    fn snapshot_second_selected() {
        let mut l = fixture();
        l.move_down();
        insta::assert_snapshot!("worktrees_second_selected_100x7", draw(&l, 100, 7));
    }

    #[test]
    fn snapshot_detached_selected() {
        let mut l = fixture();
        l.move_down();
        l.move_down();
        insta::assert_snapshot!("worktrees_detached_selected_100x7", draw(&l, 100, 7));
    }

    #[test]
    fn snapshot_narrow_60() {
        let l = fixture();
        insta::assert_snapshot!("worktrees_narrow_60x7", draw(&l, 60, 7));
    }

    #[test]
    fn snapshot_empty() {
        let l = WorktreeList::new(Vec::new());
        insta::assert_snapshot!("worktrees_empty_80x3", draw(&l, 80, 3));
    }

    #[test]
    fn desired_height_clamps_by_size() {
        let l = fixture();
        // 3 entries → clamp(2,12) = 3, +2 border = 5.
        assert_eq!(desired_height(&l), 5);
    }

    #[test]
    fn desired_height_empty_minimum_three() {
        assert_eq!(desired_height(&WorktreeList::new(Vec::new())), 3);
    }
}
