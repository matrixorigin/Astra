//! Turn-summary history cell — the single-line metrics band
//! after every completed turn.
//!
//! Shape:
//!
//! ```text
//! ⏱ 16.6s (ttft 1.7s) │ ⚡ 23.7k ↑23.2k ↓408 │ 🛠 2 │ Σ 145k · $0.014
//! ```
//!
//! Icons encode the section type so the reader can locate
//! elapsed / tokens / tools / session totals at a glance. Sections
//! that don't apply to this turn are elided.
//!
//! Persists as [`TurnEvent::TurnSummary`]. Never live.

use std::any::Any;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::HistoryCell;
use crate::tui::turn_event::TurnEvent;

#[derive(Debug, Clone, Default)]
pub(crate) struct TurnSummaryCell {
    pub elapsed_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub tools: u32,
    pub cumulative_tokens: Option<u64>,
    pub cumulative_cost_usd: Option<f64>,
    pub ts: Option<String>,
}

impl TurnSummaryCell {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(dead_code)]
    pub fn from_persist(ev: TurnEvent) -> Option<Self> {
        match ev {
            TurnEvent::TurnSummary {
                ts,
                elapsed_ms,
                ttft_ms,
                tokens_in,
                tokens_out,
                tools,
                cumulative_tokens,
                cumulative_cost_usd,
            } => Some(Self {
                elapsed_ms,
                ttft_ms,
                tokens_in,
                tokens_out,
                tools,
                cumulative_tokens,
                cumulative_cost_usd,
                ts,
            }),
            _ => None,
        }
    }
}

impl HistoryCell for TurnSummaryCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let mut segments: Vec<String> = Vec::new();

        // ⏱ elapsed + optional ttft.
        if let Some(elapsed) = self.elapsed_ms {
            match self.ttft_ms {
                Some(ttft) if ttft > 0 => segments.push(format!(
                    "⏱ {} (ttft {})",
                    fmt_duration_ms(elapsed),
                    fmt_ms(ttft)
                )),
                _ => segments.push(format!("⏱ {}", fmt_duration_ms(elapsed))),
            }
        }

        // ⚡ tokens: total + in/out breakdown.
        if let (Some(tin), Some(tout)) = (self.tokens_in, self.tokens_out) {
            segments.push(format!(
                "⚡ {} ↑{} ↓{}",
                fmt_tokens(tin + tout),
                fmt_tokens(tin),
                fmt_tokens(tout)
            ));
        }

        if self.tools > 0 {
            segments.push(format!("🛠 {}", self.tools));
        }

        // Σ session totals — either the running cumulative token
        // count, the session cost, or both (joined by ` · `).
        let mut sigma_parts: Vec<String> = Vec::new();
        if let Some(c) = self.cumulative_tokens
            && c > 0
        {
            sigma_parts.push(fmt_tokens(c));
        }
        if let Some(cost) = self.cumulative_cost_usd
            && cost > 0.0
        {
            sigma_parts.push(fmt_cost(cost));
        }
        if !sigma_parts.is_empty() {
            segments.push(format!("Σ {}", sigma_parts.join(" · ")));
        }

        if segments.is_empty() {
            return Vec::new();
        }

        let dim = Style::default().fg(Color::DarkGray);
        vec![Line::from(Span::styled(
            format!("  ─ {} ─", segments.join(" │ ")),
            dim,
        ))]
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn to_persist(&self) -> Option<TurnEvent> {
        Some(TurnEvent::TurnSummary {
            ts: self.ts.clone(),
            elapsed_ms: self.elapsed_ms,
            ttft_ms: self.ttft_ms,
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            tools: self.tools,
            cumulative_tokens: self.cumulative_tokens,
            cumulative_cost_usd: self.cumulative_cost_usd,
        })
    }
}

// ── Formatting helpers ──────────────────────────────────────────
//
// Kept local so the TurnSummaryCell renders deterministically
// without pulling in the broader `mod.rs` helpers, which will
// themselves be rewritten in Phase 3.

/// Elapsed duration in ms. Whole seconds below a minute, `Nm Ss`
/// above — matches the coarse format we use in the orbiter so the
/// band doesn't jitter sub-second.
fn fmt_duration_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let sub = ms % 1000;
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs >= 10 || sub == 0 {
        // ≥ 10 s or an exact-second tick — drop the decimal so the
        // band doesn't jitter on long turns or exact multiples.
        format!("{secs}s")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// Sub-turn ttft: ms below 1s, decimal seconds above.
fn fmt_ms(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn fmt_cost(usd: f64) -> String {
    if usd >= 1.0 {
        format!("${usd:.2}")
    } else if usd >= 0.01 {
        format!("${usd:.3}")
    } else {
        format!("${usd:.4}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    fn render(cell: &TurnSummaryCell, width: u16) -> String {
        let lines = cell.display_lines(width);
        let p = ratatui::widgets::Paragraph::new(lines)
            .wrap(ratatui::widgets::Wrap { trim: false });
        buffer_to_string(&draw_widget(p, width, 1))
    }

    fn mk_full() -> TurnSummaryCell {
        TurnSummaryCell {
            elapsed_ms: Some(16_600),
            ttft_ms: Some(1_757),
            tokens_in: Some(23_200),
            tokens_out: Some(408),
            tools: 2,
            cumulative_tokens: Some(145_000),
            cumulative_cost_usd: Some(0.014),
            ts: None,
        }
    }

    // ── Render ───────────────────────────────────────────────────

    #[test]
    fn full_summary_contains_all_sections() {
        let out = render(&mk_full(), 120);
        for seg in ["⏱", "⚡", "🛠", "Σ"] {
            assert!(out.contains(seg), "missing section {seg:?} in {out}");
        }
        assert!(out.contains("ttft"), "ttft nested missing: {out}");
        assert!(out.contains("145.0k"));
    }

    #[test]
    fn tools_zero_is_elided() {
        let mut c = mk_full();
        c.tools = 0;
        let out = render(&c, 120);
        assert!(!out.contains("🛠"), "tools=0 should not render: {out}");
    }

    #[test]
    fn ttft_zero_is_elided_from_time_segment() {
        let mut c = mk_full();
        c.ttft_ms = Some(0);
        let out = render(&c, 120);
        assert!(!out.contains("ttft"), "ttft=0 must not render: {out}");
        assert!(out.contains("⏱"));
    }

    #[test]
    fn sigma_section_elided_when_cumulative_zero() {
        let mut c = mk_full();
        c.cumulative_tokens = Some(0);
        c.cumulative_cost_usd = Some(0.0);
        let out = render(&c, 120);
        assert!(!out.contains('Σ'));
    }

    #[test]
    fn empty_cell_renders_nothing() {
        let c = TurnSummaryCell::default();
        let lines = c.display_lines(80);
        assert!(lines.is_empty(), "default cell must not produce output");
    }

    // ── Formatting helpers ───────────────────────────────────────

    #[test]
    fn fmt_duration_boundaries() {
        assert_eq!(fmt_duration_ms(400), "0.4s");
        assert_eq!(fmt_duration_ms(1_500), "1.5s");
        assert_eq!(fmt_duration_ms(16_600), "16s"); // >= 10s drops decimal
        assert_eq!(fmt_duration_ms(60_000), "1m 0s");
        assert_eq!(fmt_duration_ms(125_000), "2m 5s");
    }

    #[test]
    fn fmt_tokens_scales() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_234), "1.2k");
        assert_eq!(fmt_tokens(23_200), "23.2k");
        assert_eq!(fmt_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn fmt_cost_precision_scales_with_magnitude() {
        assert_eq!(fmt_cost(0.0042), "$0.0042");
        assert_eq!(fmt_cost(0.014), "$0.014");
        assert_eq!(fmt_cost(2.5), "$2.50");
    }

    // ── Persistence ──────────────────────────────────────────────

    #[test]
    fn persist_roundtrip_keeps_every_field() {
        let orig = mk_full();
        let ev = orig.to_persist().unwrap();
        let back = TurnSummaryCell::from_persist(ev).unwrap();
        assert_eq!(back.elapsed_ms, orig.elapsed_ms);
        assert_eq!(back.ttft_ms, orig.ttft_ms);
        assert_eq!(back.tokens_in, orig.tokens_in);
        assert_eq!(back.tokens_out, orig.tokens_out);
        assert_eq!(back.tools, orig.tools);
        assert_eq!(back.cumulative_tokens, orig.cumulative_tokens);
        assert_eq!(back.cumulative_cost_usd, orig.cumulative_cost_usd);
    }

    #[test]
    fn from_persist_rejects_wrong_variant() {
        let wrong = TurnEvent::User {
            ts: None,
            text: "x".into(),
        };
        assert!(TurnSummaryCell::from_persist(wrong).is_none());
    }

    // ── Snapshot ─────────────────────────────────────────────────

    #[test]
    fn snapshot_full_band_120() {
        insta::assert_snapshot!("turn_summary_full_120", render(&mk_full(), 120));
    }
}
