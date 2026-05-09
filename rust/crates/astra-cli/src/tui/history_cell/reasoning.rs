//! Reasoning / thinking history cell — the model's internal
//! monologue, when the provider exposes it.
//!
//! Rendered as plain dim-italic rows with a bullet prefix,
//! matching Codex's `ReasoningSummaryCell`. Deliberately NOT a
//! framed window or a collapsing pill — the previous refactor
//! invented both and both were wrong. A reasoning cell is just
//! another cell in the scrollback; if the user doesn't want to
//! see it they can hide reasoning via a toggle (not implemented
//! here; that's a ChatWidget concern).
//!
//! Duration is captured at `finalize()` and rendered in the
//! header (`(3s)`). Persists as [`TurnEvent::Thinking`].

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
}

impl ReasoningCell {
    pub fn new_streaming() -> Self {
        Self {
            text: String::new(),
            live: true,
            started_at: Some(Instant::now()),
            duration: None,
            ts: None,
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
        let d = self.duration.or_else(|| self.started_at.map(|t| t.elapsed()))?;
        let secs = d.as_secs();
        if secs == 0 {
            Some("<1s".into())
        } else {
            Some(format!("{secs}s"))
        }
    }
}

impl HistoryCell for ReasoningCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        // Empty + still live → nothing to show yet. The widget's
        // StatusIndicator handles the "thinking but no content"
        // case; we don't invent a placeholder here.
        if self.text.is_empty() {
            return Vec::new();
        }

        let dim_italic = Style::default()
            .add_modifier(Modifier::DIM)
            .add_modifier(Modifier::ITALIC);

        // Header: `💭 Thinking (Xs)` live, `💭 Thought for Xs` done.
        let header_text = if self.live {
            match self.duration_label() {
                Some(d) => format!("💭 Thinking ({d})"),
                None => "💭 Thinking…".to_string(),
            }
        } else {
            match self.duration_label() {
                Some(d) => format!("💭 Thought for {d}"),
                None => "💭 Thought".to_string(),
            }
        };

        let mut lines: Vec<Line<'static>> =
            vec![Line::from(Span::styled(header_text, dim_italic))];

        // Body: one line per logical line of thinking, each dim
        // italic, prefixed with `  ` for alignment under the bullet.
        // Hard-wrap long lines by display width to avoid terminal
        // reflow chaos on tables in reasoning.
        let inner_w = (width as usize).saturating_sub(2).max(20);
        for logical in self.text.lines() {
            for row in soft_wrap(logical, inner_w) {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(row, dim_italic),
                ]));
            }
        }

        lines
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    fn render(cell: &ReasoningCell, width: u16, height: u16) -> String {
        let lines = cell.display_lines(width);
        let p = ratatui::widgets::Paragraph::new(lines)
            .wrap(ratatui::widgets::Wrap { trim: false });
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
    fn live_header_says_thinking() {
        // Note: the `💭` emoji is a 2-display-cell grapheme; the
        // test backend shows it with a trailing space cell, so the
        // visible text is `💭  Thinking` (two spaces). Match the
        // informative substring rather than the exact glyph layout.
        let mut c = ReasoningCell::new_streaming();
        c.push_delta("draft");
        let out = render(&c, 60, 3);
        assert!(out.contains("Thinking"), "missing Thinking header: {out}");
        assert!(out.contains("💭"), "missing emoji: {out}");
    }

    #[test]
    fn finalised_header_says_thought_for() {
        let mut c = ReasoningCell::new_streaming();
        c.push_delta("analysis");
        c.started_at = Some(Instant::now() - Duration::from_secs(3));
        c.finalize();
        let out = render(&c, 60, 3);
        assert!(out.contains("Thought for"), "missing header: {out}");
        assert!(out.contains("3s"), "missing duration: {out}");
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
    fn body_rows_are_indented_under_bullet() {
        let mut c = ReasoningCell::new_streaming();
        c.push_delta("first thought\nsecond thought");
        c.finalize();
        let out = render(&c, 60, 4);
        let body_rows: Vec<&str> = out
            .lines()
            .filter(|l| !l.contains("Thought") && !l.trim().is_empty())
            .collect();
        for row in &body_rows {
            assert!(
                row.starts_with("  "),
                "body row must indent: {row:?}"
            );
        }
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
        let c = ReasoningCell::from_text(
            "The user wants X.\nPlan: do Y, then Z.",
            Some(3000),
        );
        insta::assert_snapshot!("reasoning_finalised_60", render(&c, 60, 4));
    }
}
