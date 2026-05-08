//! Two-pane timeline widget — list on the left, turn snapshot on the right.
//!
//! ```text
//! ┌ Timeline · 3 turns · 2.5k in 1.2k out ──────────────────────────────┐
//! │▌  #1   1.5s  0t   500/200  hi                    turn 1             │
//! │   #2   1.7s  2t   800/400  read the file         started 10:01 · 1.7s│
//! │   #3   1.8s  1t  1200/600  fix the bug           tokens 500 / 200    │
//! │                                                  tools 0             │
//! │                                                  user:               │
//! │                                                   hi                 │
//! │                                                  assistant:          │
//! │                                                   reply to turn 1    │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```

#![allow(dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use super::Timeline;

const MIN_SPLIT_WIDTH: u16 = 80;
pub(crate) const MAX_VISIBLE_ROWS: u16 = 18;

pub(crate) fn desired_height(tl: &Timeline) -> u16 {
    if tl.is_empty() {
        return 3;
    }
    // Detail pane needs ~8 rows; list grows up to MAX_VISIBLE_ROWS.
    let rows = (tl.total() as u16).clamp(8, MAX_VISIBLE_ROWS);
    rows + 2 // border
}

pub(crate) fn render(tl: &Timeline, area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(title_line(tl));
    let inner = outer.inner(area);
    outer.render(area, buf);

    if tl.is_empty() {
        Paragraph::new(Line::from(Span::styled(
            "  no turns recorded yet for this session",
            Style::default().add_modifier(Modifier::DIM),
        )))
        .render(inner, buf);
        return;
    }

    if inner.width < MIN_SPLIT_WIDTH {
        render_list(tl, inner, buf);
        return;
    }

    let chunks = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(inner);
    render_list(tl, chunks[0], buf);
    render_detail(tl, chunks[1], buf);
}

fn title_line(tl: &Timeline) -> Line<'static> {
    let n = tl.total();
    let tin = tl.grand_total_tokens_in();
    let tout = tl.grand_total_tokens_out();
    let title = format!(" Timeline · {n} turns · {} in {} out ", fmt_tokens(tin), fmt_tokens(tout));
    Line::from(Span::styled(
        title,
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    ))
}

fn render_list(tl: &Timeline, area: Rect, buf: &mut Buffer) {
    let dim = Style::default().fg(Color::DarkGray);
    let rows = area.height.min(MAX_VISIBLE_ROWS) as usize;
    let selected = tl.selected().unwrap_or(0);
    let turns = tl.turns();
    let (start, end) = window_around(selected, turns.len(), rows);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(end - start);
    for (idx, t) in turns[start..end].iter().enumerate() {
        let absolute = start + idx;
        let is_sel = absolute == selected;

        let theme = crate::tui::theme::current();
        let gutter = if is_sel {
            Span::styled("▌ ", Style::default().fg(theme.gutter))
        } else {
            Span::raw("  ")
        };
        // Previously used `Color::White` for non-selected rows, which
        // made them invisible on light terminals. `theme.fg` resolves
        // to `Color::Reset` under both presets, letting the terminal
        // supply whichever is legible for its background.
        let row_color = if t.is_error() {
            theme.error
        } else if is_sel {
            theme.accent
        } else {
            theme.fg
        };
        let name_style = if is_sel {
            Style::default().fg(row_color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(row_color)
        };

        let dur = t
            .duration_ms
            .map(fmt_duration)
            .unwrap_or_else(|| "-".into());
        let tools = t.tool_count.map(|c| format!("{c}t")).unwrap_or_else(|| "-".into());
        let toks = match (t.tokens_in, t.tokens_out) {
            (Some(i), Some(o)) => format!("{}/{}", fmt_tokens_u64(i), fmt_tokens_u64(o)),
            _ => "-".into(),
        };
        let user = t
            .user_preview
            .clone()
            .unwrap_or_else(|| "(no input)".into());

        let mut line = Line::from(vec![
            gutter,
            Span::styled(format!("#{:<3}", t.turn), name_style),
            Span::raw(" "),
            Span::styled(format!("{dur:>5}"), dim),
            Span::raw(" "),
            Span::styled(format!("{tools:>3}"), dim),
            Span::raw(" "),
            Span::styled(format!("{toks:>11}"), dim),
            Span::raw("  "),
            Span::styled(user, Style::default().fg(row_color)),
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

fn render_detail(tl: &Timeline, area: Rect, buf: &mut Buffer) {
    let Some(t) = tl.selected_turn() else {
        return;
    };
    let dim = Style::default().fg(Color::DarkGray);
    let label = Style::default().fg(Color::Cyan);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("turn {}", t.turn),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(vec![
        Span::styled("started ", label),
        Span::styled(short_time(&t.started_at), dim),
    ]));
    if let Some(ms) = t.duration_ms {
        lines.push(Line::from(vec![
            Span::styled("took    ", label),
            Span::styled(fmt_duration(ms), dim),
        ]));
    }
    if let (Some(tin), Some(tout)) = (t.tokens_in, t.tokens_out) {
        lines.push(Line::from(vec![
            Span::styled("tokens  ", label),
            Span::styled(format!("{tin} / {tout}"), dim),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("cum in  ", label),
        Span::styled(fmt_tokens_u64(t.cumulative_tokens_in), dim),
    ]));
    lines.push(Line::from(vec![
        Span::styled("cum out ", label),
        Span::styled(fmt_tokens_u64(t.cumulative_tokens_out), dim),
    ]));
    if let Some(tc) = t.tool_count {
        lines.push(Line::from(vec![
            Span::styled("tools   ", label),
            Span::styled(tc.to_string(), dim),
        ]));
    }
    if let Some(ref err) = t.error {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "error",
            Style::default().fg(Color::Red),
        )));
        lines.push(Line::from(Span::styled(
            err.clone(),
            Style::default().fg(Color::Red),
        )));
    }
    if let Some(ref p) = t.user_preview {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("user", label)));
        lines.push(Line::from(Span::styled(p.clone(), dim)));
    }
    if let Some(ref p) = t.assistant_preview {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("assistant", label)));
        lines.push(Line::from(Span::styled(p.clone(), dim)));
    }

    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(area, buf);
}

fn window_around(selected: usize, total: usize, visible: usize) -> (usize, usize) {
    if total <= visible {
        return (0, total);
    }
    let above = 1.min(selected);
    let start = selected.saturating_sub(above);
    let end = (start + visible).min(total);
    let start = end.saturating_sub(visible);
    (start, end)
}

fn fmt_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{:.1}m", ms as f64 / 60_000.0)
    }
}

fn fmt_tokens(n: u64) -> String {
    fmt_tokens_u64(n)
}

fn fmt_tokens_u64(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// Truncate an RFC3339 ISO timestamp to `HH:MM:SS`.
fn short_time(iso: &str) -> String {
    // Find the 'T' separator; take 8 chars after.
    if let Some(idx) = iso.find('T') {
        let rest = &iso[idx + 1..];
        let cut: String = rest.chars().take(8).collect();
        return cut;
    }
    iso.to_string()
}

#[cfg(test)]
mod tests {
    use super::super::model::{StaticTurnSource, TimelineTurn};
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    fn mk(t: u32, tin: u64, tout: u64, tools: u32, user: &str, err: Option<&str>) -> TimelineTurn {
        TimelineTurn {
            turn: t,
            started_at: format!("2024-01-15T10:{:02}:00Z", t),
            duration_ms: Some(1500 + (t as u64) * 100),
            model: Some("sonnet-4.6".into()),
            tokens_in: Some(tin),
            tokens_out: Some(tout),
            tool_count: Some(tools),
            user_preview: Some(user.into()),
            assistant_preview: Some(format!("reply to turn {t}")),
            error: err.map(String::from),
            cumulative_tokens_in: 0,
            cumulative_tokens_out: 0,
        }
    }

    fn fixture() -> Timeline {
        let src = StaticTurnSource::new(vec![
            mk(1, 500, 200, 0, "hi", None),
            mk(2, 800, 400, 2, "read the file", None),
            mk(3, 1200, 600, 1, "fix the bug", None),
        ]);
        Timeline::new(src, "sess_test")
    }

    struct Widget<'a>(&'a Timeline);
    impl ratatui::widgets::Widget for Widget<'_> {
        fn render(self, area: Rect, buf: &mut Buffer) {
            render(self.0, area, buf);
        }
    }

    fn render_tl(tl: &Timeline, w: u16, h: u16) -> String {
        let buf = draw_widget(Widget(tl), w, h);
        buffer_to_string(&buf)
    }

    #[test]
    fn snapshot_three_turns_default_selection() {
        let tl = fixture();
        insta::assert_snapshot!("timeline_default_100x10", render_tl(&tl, 100, 10));
    }

    #[test]
    fn snapshot_second_turn_selected() {
        let mut tl = fixture();
        tl.move_down();
        insta::assert_snapshot!("timeline_second_selected_100x10", render_tl(&tl, 100, 10));
    }

    #[test]
    fn snapshot_narrow_collapses_to_single_pane() {
        let tl = fixture();
        insta::assert_snapshot!("timeline_narrow_70x10", render_tl(&tl, 70, 10));
    }

    #[test]
    fn snapshot_empty_state() {
        let src = StaticTurnSource::new(vec![]);
        let tl = Timeline::new(src, "sess_empty");
        insta::assert_snapshot!("timeline_empty_80x3", render_tl(&tl, 80, 3));
    }

    #[test]
    fn snapshot_error_turn_highlighted() {
        let src = StaticTurnSource::new(vec![
            mk(1, 500, 200, 0, "hi", None),
            mk(2, 0, 0, 0, "boom", Some("rate limited")),
        ]);
        let mut tl = Timeline::new(src, "sess_err");
        tl.move_down();
        insta::assert_snapshot!("timeline_error_turn_100x10", render_tl(&tl, 100, 10));
    }

    // ─── Helpers ──────────────────────────────────────────────────

    #[test]
    fn fmt_duration_handles_scales() {
        assert_eq!(fmt_duration(250), "250ms");
        assert_eq!(fmt_duration(2_500), "2.5s");
        assert_eq!(fmt_duration(120_000), "2.0m");
    }

    #[test]
    fn short_time_extracts_hhmmss() {
        assert_eq!(short_time("2024-01-15T10:23:45Z"), "10:23:45");
    }
}
