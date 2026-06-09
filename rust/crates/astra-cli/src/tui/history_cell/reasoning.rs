//! Reasoning / thinking history cell — the model's internal
//! monologue, when the provider exposes it.
//!
//! While streaming and after completion: render only a compact header
//! (`Thought · Xs · N lines · N tokens`). Provider reasoning can contain
//! internal tool transcript fragments, so raw reasoning text is never
//! printed in the chat surface.
//!
//! Duration is captured at `finalize()` and rendered in the
//! header (`· 3s`). Persists as [`TurnEvent::Thinking`].

use std::any::Any;
use std::time::{Duration, Instant};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::HistoryCell;
use crate::tui::turn_event::TurnEvent;

#[derive(Debug, Clone)]
pub(crate) struct ReasoningCell {
    text: String,
    live: bool,
    /// When the cell was created. Used to compute duration on
    /// finalize. Not persisted — we only keep the final
    /// `duration_ms`.
    started_at: Option<Instant>,
    duration: Option<Duration>,
    ts: Option<String>,
    /// Stamped at finalize. Lets the active-slot gradient gutter
    /// pin its phase at the freeze moment.
    frozen_at: super::FreezeStamp,
}

impl ReasoningCell {
    pub fn new_streaming() -> Self {
        Self {
            text: String::new(),
            live: true,
            started_at: Some(Instant::now()),
            duration: None,
            ts: None,
            frozen_at: super::FreezeStamp::default(),
        }
    }

    /// Build from a complete reasoning blob (replay path).
    #[allow(dead_code)]
    pub fn from_text(text: impl Into<String>, duration_ms: Option<u64>) -> Self {
        Self {
            text: text.into(),
            live: false,
            started_at: None,
            duration: duration_ms.map(Duration::from_millis),
            ts: None,
            frozen_at: super::FreezeStamp::default(),
        }
    }

    #[allow(dead_code)]
    pub fn with_ts(mut self, ts: impl Into<String>) -> Self {
        self.ts = Some(ts.into());
        self
    }

    #[allow(dead_code)]
    pub fn from_persist(ev: TurnEvent) -> Option<Self> {
        match ev {
            TurnEvent::Thinking {
                ts,
                text,
                duration_ms,
            } => Some(Self {
                text,
                live: false,
                started_at: None,
                duration: duration_ms.map(Duration::from_millis),
                ts,
                // Resumed from persistence — already settled. See
                // `FreezeStamp::revived` for the launch-independent
                // phase rationale.
                frozen_at: super::FreezeStamp::revived(),
            }),
            _ => None,
        }
    }

    pub fn push_delta(&mut self, delta: &str) {
        debug_assert!(self.live, "push_delta on finalised ReasoningCell");
        self.text.push_str(delta);
    }

    #[allow(dead_code)]
    pub fn text(&self) -> &str {
        &self.text
    }

    fn duration_label(&self) -> Option<String> {
        let d = self
            .duration
            .or_else(|| self.started_at.map(|t| t.elapsed()))?;
        let secs = d.as_secs();
        if secs == 0 {
            Some("<1s".into())
        } else {
            Some(format!("{secs}s"))
        }
    }
}

impl HistoryCell for ReasoningCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        // Empty + still live → nothing to show yet. The widget's
        // StatusIndicator handles the "thinking but no content"
        // case; we don't invent a placeholder here.
        if self.text.is_empty() {
            return Vec::new();
        }

        let theme = crate::tui::theme::current();
        let stat = Style::default().fg(theme.dim);
        let dim = Style::default().fg(theme.dim).add_modifier(Modifier::DIM);
        // Raw reasoning is intentionally not rendered, even while live.
        // Some providers include internal tool transcript fragments in
        // reasoning deltas; surfacing that text looks like stray debug output.
        // Keep counting it so the header still communicates activity.
        let line_count = self.text.lines().count();
        let token_count = approx_tokens(self.text.chars().count() as u64);
        let line_label = if line_count == 1 {
            "1 line".to_string()
        } else {
            format!("{line_count} lines")
        };
        let tok_label = if token_count == 1 {
            "1 token".to_string()
        } else {
            format!("{token_count} tokens")
        };
        let stat_text = self
            .duration_label()
            .map(|d| format!(" · {d} · {line_label} · {tok_label}"))
            .unwrap_or_else(|| format!(" · {line_label} · {tok_label}"));
        let header_line = Line::from(vec![
            Span::styled("• ", dim),
            super::assistant::thought_gradient("Thought", theme),
            Span::styled(stat_text, stat),
        ]);
        vec![header_line]
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn is_live(&self) -> bool {
        self.live
    }

    fn finalize(&mut self) {
        if self.live {
            self.live = false;
            if self.duration.is_none()
                && let Some(t) = self.started_at
            {
                self.duration = Some(t.elapsed());
            }
        }
        self.frozen_at.stamp_now();
    }

    fn frozen_phase(&self) -> Option<f32> {
        self.frozen_at.phase()
    }

    fn to_persist(&self) -> Option<TurnEvent> {
        Some(TurnEvent::Thinking {
            ts: self.ts.clone(),
            text: self.text.clone(),
            duration_ms: self.duration.map(|d| d.as_millis() as u64),
        })
    }
}

/// Break a logical line into visual rows at `width` display cells.
/// Splits on whitespace when possible, hard-breaks very long words.
fn soft_wrap(input: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    if width == 0 {
        return vec![input.to_string()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    for ch in input.chars() {
        let cw = ch.width().unwrap_or(0);
        if current_w + cw > width && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            current_w = 0;
        }
        current.push(ch);
        current_w += cw;
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Approximate token count from characters: chars / 4, ceiling.
/// Mirrors [`crate::tui::status_indicator::approx_tokens`].
fn approx_tokens(chars: u64) -> u64 {
    chars.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    fn render(cell: &ReasoningCell, width: u16, height: u16) -> String {
        let lines = cell.display_lines(width);
        let p =
            ratatui::widgets::Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false });
        buffer_to_string(&draw_widget(p, width, height))
    }

    // ── Lifecycle ────────────────────────────────────────────────

    #[test]
    fn new_streaming_is_live_and_empty() {
        let c = ReasoningCell::new_streaming();
        assert!(c.is_live());
        assert_eq!(c.text(), "");
    }

    #[test]
    fn push_delta_appends_text() {
        let mut c = ReasoningCell::new_streaming();
        c.push_delta("user wants X, ");
        c.push_delta("so do Y");
        assert_eq!(c.text(), "user wants X, so do Y");
    }

    #[test]
    fn finalize_flips_live_and_snapshots_duration() {
        let mut c = ReasoningCell::new_streaming();
        c.push_delta("some thought");
        // Back-date the start so `elapsed()` has something to report.
        c.started_at = Some(Instant::now() - Duration::from_secs(2));
        c.finalize();
        assert!(!c.is_live());
        assert!(c.duration.is_some(), "duration captured");
    }

    // ── Render ───────────────────────────────────────────────────

    #[test]
    fn live_header_shows_thought_with_time_lines_tokens() {
        let mut c = ReasoningCell::new_streaming();
        c.push_delta("draft");
        let out = render(&c, 60, 3);
        assert!(out.contains("Thought"), "missing Thought header: {out}");
        assert!(out.contains("1 line"), "missing line count: {out}");
        assert!(out.contains("token"), "missing token count: {out}");
    }

    #[test]
    fn finalised_header_shows_thought_with_time_lines_tokens() {
        let mut c = ReasoningCell::new_streaming();
        c.push_delta("analysis");
        c.started_at = Some(Instant::now() - Duration::from_secs(3));
        c.finalize();
        let out = render(&c, 60, 3);
        assert!(out.contains("Thought"), "missing header: {out}");
        assert!(out.contains("3s"), "missing duration: {out}");
        assert!(out.contains("1 line"), "missing line count: {out}");
        assert!(out.contains("token"), "missing token count: {out}");
    }

    #[test]
    fn empty_cell_renders_nothing() {
        let c = ReasoningCell::new_streaming();
        let out = render(&c, 60, 2).trim().to_string();
        assert!(
            out.is_empty() || out.chars().all(char::is_whitespace),
            "empty cell should yield nothing: {out:?}"
        );
    }

    #[test]
    fn live_cell_hides_raw_reasoning_body() {
        let mut c = ReasoningCell::new_streaming();
        c.push_delta("I need to inspect tools\n<tool_result>secret</tool_result>");
        let out = render(&c, 60, 4);
        assert!(out.contains("Thought"), "header missing: {out}");
        assert!(
            !out.contains("I need to inspect tools"),
            "raw reasoning must not render live: {out}"
        );
        assert!(
            !out.contains("<tool_result>"),
            "tool transcript fragments must not render live: {out}"
        );
    }

    #[test]
    fn live_cell_stays_single_header_line_even_with_many_rows() {
        let mut c = ReasoningCell::new_streaming();
        let total_rows = 20;
        let padding: Vec<String> = (1..=total_rows).map(|i| format!("row {i}")).collect();
        c.push_delta(&padding.join("\n"));

        let lines = c.display_lines(60);
        assert_eq!(
            lines.len(),
            1,
            "live reasoning should render only the compact header"
        );

        let rendered: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(rendered.contains("Thought"), "header missing: {rendered}");
        assert!(
            rendered.contains("20 lines"),
            "line count missing: {rendered}"
        );
        assert!(!rendered.contains("row 1"), "raw body leaked: {rendered}");
    }

    #[test]
    fn live_cell_has_no_overflow_counter() {
        let mut c = ReasoningCell::new_streaming();
        c.push_delta("alpha\nbeta\ngamma");
        let rendered: String = c
            .display_lines(60)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !rendered.contains("⋯"),
            "no raw reasoning overflow counter should render: {rendered}"
        );
    }

    #[test]
    fn live_cell_under_window_still_hides_every_row() {
        let mut c = ReasoningCell::new_streaming();
        c.push_delta("alpha\nbeta\ngamma");
        let out = render(&c, 60, 5);
        assert!(
            !out.contains("alpha"),
            "raw reasoning row must stay hidden: {out}"
        );
        assert!(!out.contains("beta"), "raw reasoning row leaked: {out}");
        assert!(!out.contains("gamma"), "raw reasoning row leaked: {out}");
    }

    #[test]
    fn finalised_cell_hides_body_and_shows_line_and_token_count() {
        // Once thinking is done, scrollback shows only the compact
        // header `Thought · Xs · N lines · N tokens` — not the 20-40 row
        // dim wall. The count cues the user that there's
        // substance behind the header without dumping it on-screen.
        // (The full text stays in the JSONL transcript for later.)
        let mut c = ReasoningCell::new_streaming();
        c.push_delta("line one\nline two\nline three");
        c.started_at = Some(Instant::now() - Duration::from_secs(3));
        c.finalize();
        let out = render(&c, 60, 4);
        assert!(out.contains("Thought"), "header missing: {out}");
        assert!(out.contains("3 lines"), "line count missing: {out}");
        assert!(out.contains("token"), "token count missing: {out}");
        assert!(
            !out.contains("line one"),
            "body must be hidden once finalised: {out}"
        );
        assert!(
            !out.contains("line two"),
            "body must be hidden once finalised: {out}"
        );
    }

    #[test]
    fn single_line_thought_uses_singular_label() {
        let mut c = ReasoningCell::new_streaming();
        c.push_delta("just one");
        c.started_at = Some(Instant::now() - Duration::from_secs(1));
        c.finalize();
        let out = render(&c, 60, 2);
        assert!(
            out.contains("1 line"),
            "singular form for single-line thought: {out}"
        );
    }

    // ── Persistence ──────────────────────────────────────────────

    #[test]
    fn persist_roundtrip_keeps_text_and_duration() {
        let c = ReasoningCell::from_text("some reasoning", Some(3120));
        let ev = c.to_persist().unwrap();
        let back = ReasoningCell::from_persist(ev).unwrap();
        assert_eq!(back.text(), "some reasoning");
        assert_eq!(back.duration, Some(Duration::from_millis(3120)));
        assert!(!back.is_live());
    }

    #[test]
    fn persist_without_duration_survives() {
        // Providers that send one big blob without a "start" event
        // won't have a duration; cell must persist with `None`.
        let c = ReasoningCell::from_text("single blob", None);
        let ev = c.to_persist().unwrap();
        match ev {
            TurnEvent::Thinking { duration_ms, .. } => {
                assert!(duration_ms.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn from_persist_rejects_wrong_variant() {
        let wrong = TurnEvent::User {
            ts: None,
            text: "x".into(),
        };
        assert!(ReasoningCell::from_persist(wrong).is_none());
    }

    // ── soft_wrap ────────────────────────────────────────────────

    #[test]
    fn soft_wrap_splits_at_width() {
        let rows = soft_wrap("a".repeat(25).as_str(), 10);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].chars().count(), 10);
        assert_eq!(rows[1].chars().count(), 10);
        assert_eq!(rows[2].chars().count(), 5);
    }

    #[test]
    fn soft_wrap_empty_input_produces_one_empty_row() {
        let rows = soft_wrap("", 10);
        assert_eq!(rows, vec![String::new()]);
    }

    // ── Snapshots ────────────────────────────────────────────────

    #[test]
    fn snapshot_finalised_reasoning_60() {
        let c = ReasoningCell::from_text("The user wants X.\nPlan: do Y, then Z.", Some(3000));
        crate::tui::testing::assert_tui_snapshot!("reasoning_finalised_60", render(&c, 60, 4));
    }
}
