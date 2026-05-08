//! Two-pane session picker widget: list on the left, detail on the right.
//!
//! ```text
//! ┌ Resume session ────────────────────────────── filter: tui ──┐
//! │  1h ago · 12 turns · $0.42 · ~/astra · enhance_tui          │
//! │    refactor tui approval                                     │
//! │  2h ago ·  3 turns · $0.05 · ~/astra · main                  │
//! │    initial setup                                             │
//! │                                                              │
//! │  ├ sess_abc123  (completed)                                  │
//! │  │ sonnet-4.6 · ⎇ enhance_tui @ 616d4cf                       │
//! │  │ 1.2k in + 0.8k out · 2 checkpoints                         │
//! │  │ refactor tui approval                                      │
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! Narrow terminals (< 70 cols) collapse to a single-column list.

#![allow(dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use super::SessionDiscovery;

const MIN_SPLIT_WIDTH: u16 = 70;
pub(crate) const MAX_VISIBLE_ROWS: u16 = 12;

/// Desired total height: 2 rows per entry (header + summary), plus
/// border and filter hint. Bounded by MAX_VISIBLE_ROWS.
pub(crate) fn desired_height(disco: &SessionDiscovery) -> u16 {
    if disco.is_empty() {
        return 3; // border + empty state
    }
    let rows = (disco.len() as u16 * 2).min(MAX_VISIBLE_ROWS);
    rows + 2
}

pub(crate) fn render(disco: &SessionDiscovery, area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let title = title_line(disco);
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = outer.inner(area);
    outer.render(area, buf);

    if disco.is_empty() {
        let msg = if disco.filter().is_empty() {
            "  no previous sessions yet — run one to resume it later"
        } else {
            "  no sessions match the filter"
        };
        Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default().add_modifier(Modifier::DIM),
        )))
        .render(inner, buf);
        return;
    }

    if inner.width < MIN_SPLIT_WIDTH {
        render_list(disco, inner, buf, false);
        return;
    }

    let chunks = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(inner);
    render_list(disco, chunks[0], buf, true);
    render_detail(disco, chunks[1], buf);
}

fn title_line(disco: &SessionDiscovery) -> Line<'static> {
    let total = format!(" Resume session ({}) ", disco.len());
    let mut spans = vec![Span::styled(
        total,
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )];
    if !disco.filter().is_empty() {
        spans.push(Span::styled(
            format!("filter: {} ", disco.filter()),
            Style::default().fg(Color::Yellow),
        ));
    }
    Line::from(spans)
}

fn render_list(disco: &SessionDiscovery, area: Rect, buf: &mut Buffer, _two_pane: bool) {
    let dim = Style::default().fg(Color::DarkGray);
    let matches = disco.matches();
    let selected = disco.selected().unwrap_or(0);

    // Window around selection so it stays visible.
    let rows_per_entry = 2; // header + summary
    let max_rows = area.height as usize;
    let entries_visible = (max_rows / rows_per_entry).max(1);
    let (start, end) = window_around(selected, matches.len(), entries_visible);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity((end - start) * 2);
    for (idx, entry) in matches[start..end].iter().enumerate() {
        let absolute = start + idx;
        let is_selected = absolute == selected;

        let gutter = if is_selected {
            Span::styled("▌ ", Style::default().fg(Color::Cyan))
        } else {
            Span::raw("  ")
        };

        let name_style = if is_selected {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let age = short_age(&entry.updated_at);
        let turns = format!("{}t", entry.turn_count);
        let cost = entry
            .cost_usd
            .map(|c| format!("${c:.2}"))
            .unwrap_or_default();
        let branch = entry.git_branch.as_deref().unwrap_or("");
        let cwd = shorten_path(&entry.cwd, 30);

        // Header row: gutter + age · turns · cost · cwd · branch
        let mut header_spans: Vec<Span<'static>> = vec![gutter];
        header_spans.push(Span::styled(
            format!("{age:>7}"),
            Style::default().fg(Color::Yellow),
        ));
        header_spans.push(Span::styled(" · ", dim));
        header_spans.push(Span::styled(turns.clone(), name_style));
        if !cost.is_empty() {
            header_spans.push(Span::styled(" · ", dim));
            header_spans.push(Span::styled(cost, name_style));
        }
        header_spans.push(Span::styled(" · ", dim));
        header_spans.push(Span::styled(cwd, name_style));
        if !branch.is_empty() {
            header_spans.push(Span::styled(" · ", dim));
            header_spans.push(Span::styled(
                format!("⎇ {branch}"),
                Style::default().fg(Color::Blue),
            ));
        }
        lines.push(Line::from(header_spans));

        // Summary row.
        let summary = entry
            .summary
            .as_deref()
            .unwrap_or("(no summary)");
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(summary.to_string(), dim),
        ]));
    }

    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(area, buf);
}

fn render_detail(disco: &SessionDiscovery, area: Rect, buf: &mut Buffer) {
    let Some(entry) = disco.selected_entry() else {
        return;
    };

    let dim = Style::default().fg(Color::DarkGray);
    let label = Style::default().fg(Color::Cyan);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("{} ", entry.id),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(vec![
        Span::styled("status ", label),
        Span::styled(entry.status.clone(), dim),
    ]));
    lines.push(Line::from(vec![
        Span::styled("model  ", label),
        Span::styled(entry.model.clone(), dim),
    ]));
    if let Some(b) = entry.git_branch.as_deref() {
        let head = entry
            .git_head
            .as_deref()
            .map(|h| format!(" @ {}", &h[..7.min(h.len())]))
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled("branch ", label),
            Span::styled(format!("{b}{head}"), dim),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("tokens ", label),
        Span::styled(
            format!(
                "{} in · {} out",
                fmt_tokens(entry.tokens_in),
                fmt_tokens(entry.tokens_out)
            ),
            dim,
        ),
    ]));
    if let Some(c) = entry.cost_usd {
        lines.push(Line::from(vec![
            Span::styled("cost   ", label),
            Span::styled(format!("${c:.2}"), dim),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("turns  ", label),
        Span::styled(entry.turn_count.to_string(), dim),
    ]));
    if entry.checkpoints > 0 {
        lines.push(Line::from(vec![
            Span::styled("ckpts  ", label),
            Span::styled(entry.checkpoints.to_string(), dim),
        ]));
    }
    if let Some(goal) = entry.plan_goal.as_deref() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("plan goal", label)));
        lines.push(Line::from(Span::styled(goal.to_string(), dim)));
    }
    if let Some(sum) = entry.summary.as_deref() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("summary", label)));
        lines.push(Line::from(Span::styled(sum.to_string(), dim)));
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

fn shorten_path(p: &str, max: usize) -> String {
    if p.chars().count() <= max {
        return p.to_string();
    }
    let tail: String = p.chars().skip(p.chars().count() - (max - 1)).collect();
    format!("…{tail}")
}

fn fmt_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// Short relative age label. We parse the RFC3339 timestamp here to
/// avoid depending on `SystemTime::now()` at render time (tests want
/// deterministic output; discovery is expected to pre-compute).
fn short_age(iso: &str) -> String {
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(iso) else {
        return iso.chars().take(10).collect();
    };
    let now = chrono::Utc::now();
    let secs = now
        .signed_duration_since(dt.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::super::discovery::{SessionDiscovery, SessionEntry, StaticSessionSource};
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    #[allow(clippy::too_many_arguments)]
    fn mk(id: &str, cwd: &str, branch: &str, turns: u32, tin: u64, tout: u64, cost: Option<f64>, sum: &str, iso: &str) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            cwd: cwd.into(),
            git_branch: Some(branch.into()),
            git_head: Some("616d4cf81".into()),
            turn_count: turns,
            tokens_in: tin,
            tokens_out: tout,
            cost_usd: cost,
            summary: Some(sum.into()),
            status: "completed".into(),
            model: "sonnet-4.6".into(),
            updated_at: iso.into(),
            checkpoints: 2,
            plan_goal: None,
        }
    }

    fn fixture() -> SessionDiscovery {
        // Use ISO timestamps far in the past so `short_age` yields
        // stable "days ago" values independent of when the test runs.
        let src = StaticSessionSource::new(vec![
            mk("sess_abc123", "~/astra", "enhance_tui", 12, 8100, 3400, Some(0.42), "refactor tui approval", "2024-01-15T10:00:00Z"),
            mk("sess_def456", "~/astra", "main", 3, 1200, 600, Some(0.05), "initial setup", "2024-01-15T08:00:00Z"),
            mk("sess_xyz789", "~/other", "feat/login", 20, 42000, 18000, Some(3.10), "add auth flow with OAuth", "2024-01-14T12:00:00Z"),
        ]);
        SessionDiscovery::new(src, 10)
    }

    struct PickerWidget<'a>(&'a SessionDiscovery);
    impl Widget for PickerWidget<'_> {
        fn render(self, area: Rect, buf: &mut Buffer) {
            render(self.0, area, buf);
        }
    }

    fn render_picker(d: &SessionDiscovery, w: u16, h: u16) -> String {
        let buf = draw_widget(PickerWidget(d), w, h);
        buffer_to_string(&buf)
    }

    #[test]
    fn snapshot_three_sessions_default_selection() {
        let d = fixture();
        insta::assert_snapshot!("session_picker_default_100x10", render_picker(&d, 100, 10));
    }

    #[test]
    fn snapshot_filtered_to_single_match() {
        let mut d = fixture();
        d.set_filter("auth");
        insta::assert_snapshot!("session_picker_filter_auth_100x10", render_picker(&d, 100, 10));
    }

    #[test]
    fn snapshot_narrow_collapses_to_single_pane() {
        let d = fixture();
        insta::assert_snapshot!("session_picker_narrow_60x10", render_picker(&d, 60, 10));
    }

    #[test]
    fn snapshot_empty_state_no_filter() {
        let src = StaticSessionSource::new(vec![]);
        let d = SessionDiscovery::new(src, 10);
        insta::assert_snapshot!("session_picker_empty_80x4", render_picker(&d, 80, 4));
    }

    #[test]
    fn snapshot_empty_after_filter() {
        let mut d = fixture();
        d.set_filter("zzz_nope");
        insta::assert_snapshot!("session_picker_empty_filtered_80x4", render_picker(&d, 80, 4));
    }

    #[test]
    fn snapshot_selection_moved_down() {
        let mut d = fixture();
        d.move_down();
        insta::assert_snapshot!("session_picker_second_selected_100x10", render_picker(&d, 100, 10));
    }

    // ─── Pure unit tests ──────────────────────────────────────────

    #[test]
    fn shorten_path_preserves_short() {
        assert_eq!(shorten_path("~/astra", 30), "~/astra");
    }

    #[test]
    fn shorten_path_tail_ellipsizes() {
        let s = shorten_path("/home/xupeng/astra/very/deep/path/inside", 20);
        assert!(s.starts_with('…'));
        assert!(s.chars().count() <= 20);
    }

    #[test]
    fn fmt_tokens_compact() {
        assert_eq!(fmt_tokens(42), "42");
        assert_eq!(fmt_tokens(1200), "1.2k");
        assert_eq!(fmt_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn desired_height_scales_with_entries() {
        let d = fixture();
        // 3 entries × 2 rows = 6, plus border (2) = 8.
        assert_eq!(desired_height(&d), 8);
    }

    #[test]
    fn desired_height_empty_uses_minimum() {
        let src = StaticSessionSource::new(vec![]);
        let d = SessionDiscovery::new(src, 10);
        assert_eq!(desired_height(&d), 3);
    }
}
