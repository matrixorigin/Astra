//! Turn-summary history cell — the compact post-turn metrics band.
//!
//! Shape:
//!
//! ```text
//!   16s total · ttft 1.8s · 23.6k tokens · 2 tools (145.0k overall · $0.014 spent)
//! ```
//!
//! The summary intentionally stays in product language rather than
//! exposing raw telemetry grammar. Lower-value details such as
//! per-direction token arrows are omitted so the band reads like a
//! calm recap instead of an engineering dashboard. Sections that
//! don't apply to this turn are elided.
//!
//! Persists as [`TurnEvent::TurnSummary`]. Never live.

use std::any::Any;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::HistoryCell;
use crate::cli::turn::turn_reporting::{format_cache_usage_summary, format_usage_count};
use crate::tui::turn_event::TurnEvent;

#[derive(Debug, Clone, Default)]
pub(crate) struct TurnSummaryCell {
    pub elapsed_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    /// Structured primary-provider metrics retained for diagnostics/replay and
    /// rendered in a compact form in the default completion marker.
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    /// Cached input alongside the fresh `tokens_in`.
    pub cache_read_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub model_name: Option<String>,
    pub auxiliary_summary: Option<String>,
    pub input_usage_complete: bool,
    pub usage_partial: bool,
    pub tools: u32,
    pub cumulative_tokens: Option<u64>,
    pub cumulative_cost_usd: Option<f64>,
    pub ts: Option<String>,
}

impl TurnSummaryCell {
    pub fn from_persist(ev: TurnEvent) -> Option<Self> {
        match ev {
            TurnEvent::TurnSummary {
                ts,
                elapsed_ms,
                ttft_ms,
                tokens_in,
                tokens_out,
                cache_read_tokens,
                cache_creation_tokens,
                model_name,
                auxiliary_summary,
                input_usage_complete,
                usage_partial,
                tools,
                cumulative_tokens,
                cumulative_cost_usd,
            } => Some(Self {
                elapsed_ms,
                ttft_ms,
                tokens_in,
                tokens_out,
                cache_read_tokens,
                cache_creation_tokens,
                model_name,
                auxiliary_summary,
                input_usage_complete,
                usage_partial,
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
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let theme = crate::tui::theme::current();
        let label = Style::default().fg(theme.dim);
        let value = Style::default().fg(theme.fg);
        let sections = self.sections(label, value);
        if sections.is_empty() {
            return Vec::new();
        }

        pack_sections_into_lines(sections, width)
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
            cache_read_tokens: self.cache_read_tokens,
            cache_creation_tokens: self.cache_creation_tokens,
            model_name: self.model_name.clone(),
            auxiliary_summary: self.auxiliary_summary.clone(),
            input_usage_complete: self.input_usage_complete,
            usage_partial: self.usage_partial,
            tools: self.tools,
            cumulative_tokens: self.cumulative_tokens,
            cumulative_cost_usd: self.cumulative_cost_usd,
        })
    }
}

impl TurnSummaryCell {
    fn sections(&self, label: Style, value: Style) -> Vec<Section> {
        let mut sections: Vec<Section> = Vec::new();
        let mut secondary_parts: Vec<Vec<Span<'static>>> = Vec::new();

        if let Some(elapsed) = self.elapsed_ms {
            sections.push(Section::primary(vec![
                Span::styled(fmt_duration_ms(elapsed), value),
                Span::styled(" total", label),
            ]));
        }

        if let Some(ttft) = self.ttft_ms
            && ttft > 0
        {
            sections.push(Section::primary(vec![
                Span::styled("ttft ", label),
                Span::styled(fmt_ms(ttft), value),
            ]));
        }

        let usage_observed = self.tokens_in.is_some()
            || self.tokens_out.is_some()
            || self.cache_read_tokens.is_some()
            || self.cache_creation_tokens.is_some();
        let provider_tokens = self
            .tokens_in
            .unwrap_or(0)
            .saturating_add(self.cache_read_tokens.unwrap_or(0))
            .saturating_add(self.cache_creation_tokens.unwrap_or(0))
            .saturating_add(self.tokens_out.unwrap_or(0));
        if usage_observed {
            sections.push(Section::primary(vec![
                Span::styled(
                    format_usage_count(provider_tokens, !self.usage_partial),
                    value,
                ),
                Span::styled(" tokens", label),
            ]));
        }

        if let Some(cache) = format_cache_usage_summary(
            self.tokens_in,
            self.cache_read_tokens,
            self.cache_creation_tokens,
            self.input_usage_complete,
        ) {
            sections.push(Section::primary(vec![Span::styled(cache, value)]));
        }

        if self.tools > 0 {
            sections.push(Section::primary(vec![
                Span::styled(self.tools.to_string(), value),
                Span::styled(if self.tools == 1 { " tool" } else { " tools" }, label),
            ]));
        }

        let cumulative_tokens = self
            .cumulative_tokens
            .filter(|c| *c > provider_tokens)
            .map(|c| Span::styled(format_usage_count(c, true), value));

        if let Some(cost) = self.cumulative_cost_usd
            && cost > 0.0
        {
            secondary_parts.push(vec![
                Span::styled(fmt_cost(cost), value),
                Span::styled(" spent", label),
            ]);
        }

        if cumulative_tokens.is_some() || !secondary_parts.is_empty() {
            let mut spans: Vec<Span<'static>> = Vec::new();
            spans.push(Span::styled("(", label));
            if let Some(tokens) = cumulative_tokens {
                spans.push(tokens);
                spans.push(Span::styled(" overall", label));
            } else {
                spans.push(Span::styled("Overall", label));
            }
            for part in secondary_parts {
                spans.push(Span::styled(" · ", label));
                spans.extend(part);
            }
            spans.push(Span::styled(")", label));
            sections.push(Section::secondary(spans));
        }

        sections
    }
}

// ── Formatting helpers ──────────────────────────────────────────
//
// Kept local so the TurnSummaryCell renders deterministically without coupling
// its persisted projection to unrelated status-line formatting.

/// Elapsed duration in ms. Whole seconds below a minute, `Nm Ss`
/// above — matches the coarse format we use elsewhere so the
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

fn fmt_cost(usd: f64) -> String {
    if usd >= 1.0 {
        format!("${usd:.2}")
    } else if usd >= 0.01 {
        format!("${usd:.3}")
    } else {
        format!("${usd:.4}")
    }
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

#[derive(Debug, Clone)]
struct Section {
    spans: Vec<Span<'static>>,
    secondary: bool,
}

impl Section {
    fn primary(spans: Vec<Span<'static>>) -> Self {
        Self {
            spans,
            secondary: false,
        }
    }

    fn secondary(spans: Vec<Span<'static>>) -> Self {
        Self {
            spans,
            secondary: true,
        }
    }
}

fn pack_sections_into_lines(sections: Vec<Section>, width: u16) -> Vec<Line<'static>> {
    let available = usize::from(width.max(24));
    let indent = Span::styled("  ", Style::default().fg(crate::tui::theme::current().dim));
    let indent_width = unicode_width::UnicodeWidthStr::width(indent.content.as_ref());
    let sep = Span::styled(" · ", Style::default().fg(crate::tui::theme::current().dim));
    let sep_width = unicode_width::UnicodeWidthStr::width(sep.content.as_ref());
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = vec![indent.clone()];
    let mut current_width = indent_width;

    for section in sections {
        let section_width = spans_width(&section.spans);
        let extra = if current_width == indent_width {
            0
        } else {
            sep_width
        };
        if current_width > indent_width && current_width + extra + section_width > available {
            lines.push(Line::from(current));
            current = vec![indent.clone()];
            current_width = indent_width;
        }
        if current_width > indent_width {
            current.push(sep.clone());
            current_width += sep_width;
        }
        current_width += section_width;
        current.extend(section.spans);
    }

    if current_width > indent_width {
        lines.push(Line::from(current));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    fn render(cell: &TurnSummaryCell, width: u16) -> String {
        let lines = cell.display_lines(width);
        let p =
            ratatui::widgets::Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false });
        buffer_to_string(&draw_widget(p, width, 3))
    }

    #[test]
    fn complete_input_cache_rate_survives_missing_output_usage() {
        use crate::cli::stream::streaming_types::{AttributedTokenUsage, UsageAttribution};
        use crate::cli::turn::turn_reporting::project_primary_usage;
        use crate::tui::app_event::TuiAppEvent;
        use crate::tui::chat_widget::{ChatWidget, TurnContext, translate};
        for (fresh, read, write, complete, expected) in [
            (Some(100), Some(900), Some(0), true, Some("90% cached")),
            (Some(100), Some(900), Some(0), false, None),
            (Some(100), Some(900), Some(100), true, Some("81% cached")),
            (Some(0), Some(0), Some(0), true, None),
            (Some(100), None, Some(0), false, None),
        ] {
            let usage = UsageAttribution {
                primary: Some(AttributedTokenUsage {
                    fresh_input_tokens: fresh,
                    cache_read_tokens: read,
                    cache_creation_tokens: write,
                    output_tokens: None,
                }),
                primary_input_complete: complete,
                primary_complete: false,
                auxiliary: Some(AttributedTokenUsage {
                    cache_read_tokens: Some(50_000),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let projected = project_primary_usage(&usage, true);
            let ctx = TurnContext {
                tokens_in: projected.fresh_input_tokens,
                tokens_out: projected.output_tokens,
                cache_read_tokens: projected.cache_read_tokens,
                cache_creation_tokens: projected.cache_creation_tokens,
                input_usage_complete: projected.input_complete,
                usage_partial: !projected.complete,
                ..Default::default()
            };
            let mut widget = ChatWidget::new("cache-input");
            widget.handle_event(translate(TuiAppEvent::TurnComplete, ctx).unwrap());
            let cell = widget
                .history()
                .iter()
                .find_map(|cell| cell.as_any_ref().downcast_ref::<TurnSummaryCell>())
                .unwrap();
            let out = render(cell, 120);
            if let Some(expected) = expected {
                assert!(out.contains(expected), "{out}");
            } else {
                assert!(!out.contains('%'), "{out}");
            }
            if fresh == Some(100) && read == Some(900) && write == Some(0) {
                assert!(out.contains("≥1.0k tokens"), "{out}");
            }
            let serialized = serde_json::to_string(&cell.to_persist().unwrap()).unwrap();
            let restored =
                TurnSummaryCell::from_persist(serde_json::from_str(&serialized).unwrap()).unwrap();
            assert_eq!(render(&restored, 120), out);
        }
    }

    #[test]
    fn accounted_usage_fallback_still_renders_turn_tokens_and_cache_rate() {
        use crate::cli::stream::streaming_types::UsageAttribution;
        use crate::cli::turn::turn_reporting::project_turn_usage;
        use crate::tui::app_event::TuiAppEvent;
        use crate::tui::chat_widget::{ChatWidget, TurnContext, translate};
        // No provider-attempt Explain facts reached this topology, but the
        // runtime still accounted the turn. The footer must report this turn's
        // tokens and cache rate instead of only the session total.
        let attribution = UsageAttribution::default();
        let accounted = astra_turn_types::CanonicalTokenUsage::new(
            Some(10_000),
            Some(90_000),
            Some(0),
            Some(20),
        )
        .unwrap();
        let projected = project_turn_usage(&attribution, true, Some(accounted));
        let ctx = TurnContext {
            tokens_in: projected.fresh_input_tokens,
            tokens_out: projected.output_tokens,
            cache_read_tokens: projected.cache_read_tokens,
            cache_creation_tokens: projected.cache_creation_tokens,
            input_usage_complete: projected.input_complete,
            usage_partial: !projected.complete,
            cumulative_tokens: Some(145_000),
            ..Default::default()
        };
        let mut widget = ChatWidget::new("accounted-usage");
        widget.handle_event(translate(TuiAppEvent::TurnComplete, ctx).unwrap());
        let cell = widget
            .history()
            .iter()
            .find_map(|cell| cell.as_any_ref().downcast_ref::<TurnSummaryCell>())
            .unwrap();
        let out = render(cell, 120);
        assert!(out.contains("100.0k tokens"), "{out}");
        assert!(out.contains("90% cached"), "{out}");
        assert!(out.contains("145.0k overall"), "{out}");
    }

    #[test]
    fn turn_footer_e2e_survives_missing_explain_attribution() {
        use crate::cli::stream::streaming_types::UsageAttribution;
        use crate::cli::turn::turn_entry::TurnUsage;
        use crate::cli::turn::turn_reporting::project_turn_usage;
        use crate::tui::app_event::TuiAppEvent;
        use crate::tui::chat_widget::{ChatWidget, TurnContext, translate};
        // Production wiring, end to end: the server accounted this turn but
        // delivered no provider-attempt Explain facts — the exact shape that
        // dropped both the tokens and cache segments from the footer.
        let mut result = crate::tests::stub_stream_result("done");
        result.usage_attribution = UsageAttribution::default();
        result.qualified_usage = astra_turn_types::CanonicalTokenUsage::new(
            Some(10_000),
            Some(90_000),
            Some(0),
            Some(20),
        )
        .ok();
        // Raw counters exist, but the footer must not need them.
        result.prompt_tokens = 100_020;
        result.completion_tokens = 20;
        result.cache_read_tokens = 90_000;
        let usage = TurnUsage::from_stream_result(&result).expect("turn usage");
        let projected = project_turn_usage(
            &usage.usage_attribution,
            usage.usage_observed,
            usage.qualified_usage,
        );
        let ctx = TurnContext {
            elapsed_ms: Some(20_000),
            ttft_ms: Some(2_200),
            tools: 3,
            tokens_in: projected.fresh_input_tokens,
            tokens_out: projected.output_tokens,
            cache_read_tokens: projected.cache_read_tokens,
            cache_creation_tokens: projected.cache_creation_tokens,
            input_usage_complete: projected.input_complete,
            usage_partial: !projected.complete,
            cumulative_tokens: Some(145_000),
            ..Default::default()
        };
        let mut widget = ChatWidget::new("footer-e2e");
        widget.handle_event(translate(TuiAppEvent::TurnComplete, ctx).unwrap());
        let cell = widget
            .history()
            .iter()
            .find_map(|cell| cell.as_any_ref().downcast_ref::<TurnSummaryCell>())
            .unwrap();
        let out = render(cell, 120);
        assert!(out.contains("100.0k tokens"), "{out}");
        assert!(out.contains("90% cached"), "{out}");
        assert!(out.contains("3 tools"), "{out}");
        assert!(out.contains("145.0k overall"), "{out}");
    }

    #[test]
    fn turn_footer_e2e_unknown_write_lane_stays_a_lower_bound() {
        use crate::cli::stream::streaming_types::UsageAttribution;
        use crate::cli::turn::turn_entry::TurnUsage;
        use crate::cli::turn::turn_reporting::project_turn_usage;
        use crate::tui::app_event::TuiAppEvent;
        use crate::tui::chat_widget::{ChatWidget, TurnContext, translate};
        let mut result = crate::tests::stub_stream_result("done");
        result.usage_attribution = UsageAttribution::default();
        result.qualified_usage =
            astra_turn_types::CanonicalTokenUsage::new(Some(10_000), Some(90_000), None, Some(20))
                .ok();
        result.prompt_tokens = 100_000;
        result.completion_tokens = 20;
        result.cache_read_tokens = 90_000;
        let usage = TurnUsage::from_stream_result(&result).expect("turn usage");
        let projected = project_turn_usage(
            &usage.usage_attribution,
            usage.usage_observed,
            usage.qualified_usage,
        );
        let ctx = TurnContext {
            tokens_in: projected.fresh_input_tokens,
            tokens_out: projected.output_tokens,
            cache_read_tokens: projected.cache_read_tokens,
            cache_creation_tokens: projected.cache_creation_tokens,
            input_usage_complete: projected.input_complete,
            usage_partial: projected.observed && !projected.complete,
            ..Default::default()
        };
        let mut widget = ChatWidget::new("footer-e2e-partial");
        widget.handle_event(translate(TuiAppEvent::TurnComplete, ctx).unwrap());
        let cell = widget
            .history()
            .iter()
            .find_map(|cell| cell.as_any_ref().downcast_ref::<TurnSummaryCell>())
            .unwrap();
        let out = render(cell, 120);
        assert!(out.contains("≥100.0k tokens"), "{out}");
        assert!(out.contains("≥90.0k cached"), "{out}");
        assert!(!out.contains('%'), "{out}");
    }

    fn mk_full() -> TurnSummaryCell {
        TurnSummaryCell {
            elapsed_ms: Some(16_600),
            ttft_ms: Some(1_757),
            tokens_in: Some(23_200),
            tokens_out: Some(408),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            model_name: None,
            auxiliary_summary: None,
            input_usage_complete: false,
            usage_partial: false,
            tools: 2,
            cumulative_tokens: Some(145_000),
            cumulative_cost_usd: Some(0.014),
            ts: None,
        }
    }

    // ── Render ───────────────────────────────────────────────────

    #[test]
    fn full_summary_contains_only_user_facing_sections() {
        let out = render(&mk_full(), 120);
        for seg in [
            "16s total",
            "ttft",
            "1.8s",
            "23.6k tokens",
            "tools",
            "overall",
            "spent",
        ] {
            assert!(out.contains(seg), "missing section {seg:?} in {out}");
        }
        for diagnostic in ["first response", "with ", "request_judgment"] {
            assert!(
                !out.contains(diagnostic),
                "diagnostic section {diagnostic:?} leaked into {out}"
            );
        }
    }

    #[test]
    fn tools_zero_is_elided() {
        let mut c = mk_full();
        c.tools = 0;
        let out = render(&c, 120);
        assert!(!out.contains(" tools"), "tools=0 should not render: {out}");
    }

    #[test]
    fn ttft_zero_is_elided_from_time_segment() {
        let mut c = mk_full();
        c.ttft_ms = Some(0);
        let out = render(&c, 120);
        assert!(!out.contains("ttft"), "ttft=0 must not render: {out}");
        assert!(out.contains("16s"));
    }

    #[test]
    fn model_name_is_compact_and_user_facing() {
        let mut c = mk_full();
        c.model_name = Some("deepseek-flash".into());
        let out = render(&c, 120);
        assert!(
            !out.contains("with deepseek-flash"),
            "unexpected model decoration: {out}"
        );
    }

    #[test]
    fn usage_details_are_not_dumped_into_default_summary() {
        let c = TurnSummaryCell {
            tokens_in: Some(1_200),
            tokens_out: Some(300),
            cache_read_tokens: Some(98_800),
            cache_creation_tokens: Some(0),
            auxiliary_summary: Some("Jev (jev-1.13.0) · request_judgment".into()),
            usage_partial: true,
            cumulative_tokens: Some(145_000),
            ..Default::default()
        };
        let out = render(&c, 120);
        assert!(
            out.contains("tokens"),
            "primary usage should remain visible: {out}"
        );
        assert!(
            out.contains("≥100.3k tokens"),
            "known primary usage should remain visible: {out}"
        );
        assert!(
            out.contains("≥98.8k cached"),
            "known cache reads missing: {out}"
        );
        let restored = TurnSummaryCell::from_persist(c.to_persist().unwrap()).unwrap();
        assert_eq!(
            render(&restored, 120),
            out,
            "cache must survive history replay"
        );
        for diagnostic in ["Jev", "request_judgment", "usage not fully attributed"] {
            assert!(
                !out.contains(diagnostic),
                "diagnostic {diagnostic:?} leaked into {out}"
            );
        }
    }

    #[test]
    fn partial_input_without_output_remains_visible() {
        let c = TurnSummaryCell {
            tokens_in: Some(1_200),
            usage_partial: true,
            ..Default::default()
        };
        let out = render(&c, 120);
        assert!(out.contains("≥1.2k tokens"), "known input missing: {out}");
    }

    #[test]
    fn overall_is_elided_when_it_only_repeats_the_current_turn() {
        let c = TurnSummaryCell {
            tokens_in: Some(1_200),
            tokens_out: Some(300),
            cache_read_tokens: Some(98_800),
            cumulative_tokens: Some(100_300),
            ..Default::default()
        };
        let out = render(&c, 120);
        assert!(
            !out.contains("overall"),
            "duplicate overall is noise: {out}"
        );
    }

    #[test]
    fn usage_summary_is_empty_when_only_diagnostics_are_present() {
        let mut c = mk_full();
        c.ttft_ms = None;
        c.tokens_in = None;
        c.tokens_out = None;
        c.cache_read_tokens = None;
        c.cache_creation_tokens = None;
        c.elapsed_ms = None;
        c.tools = 0;
        c.cumulative_tokens = None;
        c.cumulative_cost_usd = None;
        let out = render(&c, 120);
        assert!(out.trim().is_empty(), "diagnostic-only cell leaked: {out}");
    }

    #[test]
    fn narrow_width_wraps_summary_into_multiple_lines() {
        let mut cell = mk_full();
        cell.model_name = Some("deepseek-flash".into());
        let out = render(&cell, 32);
        let non_empty: Vec<&str> = out.lines().filter(|line| !line.trim().is_empty()).collect();
        assert!(
            non_empty.len() >= 2,
            "narrow summaries should wrap cleanly; got {out:?}"
        );
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

    // ── Persistence ──────────────────────────────────────────────

    #[test]
    fn persist_roundtrip_keeps_every_field() {
        let mut orig = mk_full();
        orig.cache_read_tokens = Some(18_000);
        orig.cache_creation_tokens = Some(7);
        orig.model_name = Some("deepseek-flash".into());
        orig.auxiliary_summary = Some("Jev (deepseek-judgement) · request_judgment".into());
        orig.usage_partial = true;
        orig.ts = Some("2026-09-20T00:00:00Z".into());
        let ev = orig.to_persist().unwrap();
        let back = TurnSummaryCell::from_persist(ev).unwrap();
        assert_eq!(back.elapsed_ms, orig.elapsed_ms);
        assert_eq!(back.ttft_ms, orig.ttft_ms);
        assert_eq!(back.tokens_in, orig.tokens_in);
        assert_eq!(back.tokens_out, orig.tokens_out);
        assert_eq!(back.cache_read_tokens, orig.cache_read_tokens);
        assert_eq!(back.cache_creation_tokens, orig.cache_creation_tokens);
        assert_eq!(back.model_name, orig.model_name);
        assert_eq!(back.auxiliary_summary, orig.auxiliary_summary);
        assert_eq!(back.usage_partial, orig.usage_partial);
        assert_eq!(back.tools, orig.tools);
        assert_eq!(back.cumulative_tokens, orig.cumulative_tokens);
        assert_eq!(back.cumulative_cost_usd, orig.cumulative_cost_usd);
        assert_eq!(back.ts, orig.ts);
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
        crate::tui::testing::assert_tui_snapshot!("turn_summary_full_120", render(&mk_full(), 120));
    }
}
