//! Rendering layer for the `/context` panel.
//!
//! ```text
//! ┌ Context window (65% · warn) ──────────────────────────────────┐
//! │ ████████████▓▓▓▓▓▓▓▓▒▒▒░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░ │
//! │ ■ system    2.0k  (2.0%)                                      │
//! │ ■ tools     4.0k  (4.0%)                                      │
//! │ ■ memory    1.0k  (1.0%)                                      │
//! │ ■ history  50.0k (50.0%)                                      │
//! │ ■ current   0.5k  (0.5%)                                      │
//! │ total 57.5k / 100k                                            │
//! └───────────────────────────────────────────────────────────────┘
//! ```

#![allow(dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use super::model::{ContextBreakdown, PressureBand};

pub(crate) fn desired_height(b: &ContextBreakdown) -> u16 {
    // border (2) + bar (1) + categories + total (1)
    if b.categories.is_empty() {
        return 3;
    }
    (2 + 1 + b.categories.len() + 1) as u16
}

pub(crate) fn render(b: &ContextBreakdown, area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let band = b.band();
    let title = title_line(b, band);

    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(title);
    let inner = outer.inner(area);
    outer.render(area, buf);

    if b.categories.is_empty() {
        let msg = Line::from(Span::styled(
            "  no context trace yet — run a turn first",
            Style::default().add_modifier(Modifier::DIM),
        ));
        Paragraph::new(msg).render(inner, buf);
        return;
    }

    let bar_width = inner.width.saturating_sub(2) as usize; // leave 1-col padding each side
    let bar_line = stacked_bar_line(b, bar_width);
    let mut lines: Vec<Line<'static>> = vec![bar_line];
    lines.extend(category_rows(b));
    lines.push(total_line(b));

    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(inner, buf);
}

fn title_line(b: &ContextBreakdown, band: PressureBand) -> Line<'static> {
    let pct = b.usage_percent();
    let headline = format!(" Context window ({pct:.0}% · {}) ", band.label());
    Line::from(vec![Span::styled(
        headline,
        Style::default()
            .fg(band.color())
            .add_modifier(Modifier::BOLD),
    )])
}

/// One-row stacked bar; each category contributes proportional
/// characters sized to its share of `limit`.
fn stacked_bar_line(b: &ContextBreakdown, width: usize) -> Line<'static> {
    if width == 0 || b.limit == 0 {
        return Line::default();
    }
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(b.categories.len() + 2);
    spans.push(Span::raw(" "));
    let mut emitted = 0usize;
    for cat in &b.categories {
        let share = (cat.tokens as f64 / b.limit as f64 * width as f64).round() as usize;
        if share == 0 {
            continue;
        }
        let block = "█".repeat(share);
        spans.push(Span::styled(block, Style::default().fg(cat.kind.color())));
        emitted += share;
    }
    // Remaining free space → dim ░.
    if emitted < width {
        let remain = width - emitted;
        spans.push(Span::styled(
            "░".repeat(remain),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

fn category_rows(b: &ContextBreakdown) -> Vec<Line<'static>> {
    let label_width = b
        .categories
        .iter()
        .map(|c| c.kind.label().len())
        .max()
        .unwrap_or(7);
    b.categories
        .iter()
        .map(|c| {
            Line::from(vec![
                Span::raw(" "),
                Span::styled("■ ", Style::default().fg(c.kind.color())),
                Span::styled(
                    format!("{:<width$}", c.kind.label(), width = label_width),
                    Style::default().fg(c.kind.color()),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("{:>7}", fmt_tokens(c.tokens)),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  ({:>4.1}%)", c.pct_of_limit),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        })
        .collect()
}

fn total_line(b: &ContextBreakdown) -> Line<'static> {
    Line::from(vec![
        Span::raw(" "),
        Span::styled("total ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(
            fmt_tokens(b.total_used),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" / {}", fmt_tokens(b.limit)),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ])
}

fn fmt_tokens(n: u32) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::ContextBreakdown;
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};
    use astra_turn_core::context_assembly_trace::TokenBudgetTrace;

    fn trace(max: u32, sys: u32, hist: u32, mem: u32, tools: u32, user: u32) -> TokenBudgetTrace {
        let total = sys + hist + mem + tools + user;
        let pressure = if max == 0 {
            0.0
        } else {
            total as f64 / max as f64
        };
        TokenBudgetTrace {
            max_tokens: max,
            system_prompt_tokens: sys,
            history_tokens: hist,
            memory_tokens: mem,
            tool_schema_tokens: tools,
            user_message_tokens: user,
            total_used: total,
            budget_pressure: pressure,
            compression_triggered: false,
        }
    }

    struct PanelWidget<'a>(&'a ContextBreakdown);
    impl Widget for PanelWidget<'_> {
        fn render(self, area: Rect, buf: &mut Buffer) {
            render(self.0, area, buf);
        }
    }

    fn render_panel(b: &ContextBreakdown, w: u16, h: u16) -> String {
        let buf = draw_widget(PanelWidget(b), w, h);
        buffer_to_string(&buf)
    }

    #[test]
    fn snapshot_low_pressure_80x9() {
        let b = ContextBreakdown::from_trace(&trace(100_000, 2_000, 15_000, 500, 4_000, 200));
        insta::assert_snapshot!("context_panel_low_80x9", render_panel(&b, 80, 9));
    }

    #[test]
    fn snapshot_warning_pressure_80x9() {
        let b = ContextBreakdown::from_trace(&trace(100_000, 8_000, 50_000, 1_000, 10_000, 1_500));
        insta::assert_snapshot!("context_panel_warn_80x9", render_panel(&b, 80, 9));
    }

    #[test]
    fn snapshot_critical_pressure_80x9() {
        let b = ContextBreakdown::from_trace(&trace(100_000, 12_000, 70_000, 2_000, 10_000, 1_500));
        insta::assert_snapshot!("context_panel_critical_80x9", render_panel(&b, 80, 9));
    }

    #[test]
    fn snapshot_empty_no_trace_80x3() {
        let b = ContextBreakdown::empty();
        insta::assert_snapshot!("context_panel_empty_80x3", render_panel(&b, 80, 3));
    }

    #[test]
    fn snapshot_narrow_60x9() {
        let b = ContextBreakdown::from_trace(&trace(100_000, 2_000, 30_000, 500, 4_000, 200));
        insta::assert_snapshot!("context_panel_narrow_60x9", render_panel(&b, 60, 9));
    }

    #[test]
    fn snapshot_skips_zero_categories() {
        // memory and tools are zero — only three rows render.
        let b = ContextBreakdown::from_trace(&trace(100_000, 2_000, 10_000, 0, 0, 500));
        insta::assert_snapshot!("context_panel_sparse_80x7", render_panel(&b, 80, 7));
    }

    // ─── Pure helpers ─────────────────────────────────────────────

    #[test]
    fn fmt_tokens_handles_all_magnitudes() {
        assert_eq!(fmt_tokens(42), "42");
        assert_eq!(fmt_tokens(1_200), "1.2k");
        assert_eq!(fmt_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn desired_height_scales_with_categories() {
        // 5 categories → 2 (border) + 1 (bar) + 5 + 1 (total) = 9.
        let b = ContextBreakdown::from_trace(&trace(100_000, 1, 2, 3, 4, 5));
        assert_eq!(desired_height(&b), 9);
    }

    #[test]
    fn desired_height_empty_is_three_rows() {
        assert_eq!(desired_height(&ContextBreakdown::empty()), 3);
    }
}
