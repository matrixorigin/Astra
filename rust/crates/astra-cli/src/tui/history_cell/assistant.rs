//! Assistant-reply history cell — the final model answer.
//!
//! Owns the **raw markdown source** for the whole reply. Growing
//! the reply during a stream is `push_delta`; finalising freezes
//! the source. `display_lines` re-renders the source on each draw,
//! so width changes (terminal resize) and theme changes between
//! runs both produce correct output without the cell having to
//! cache pre-wrapped `Line`s.
//!
//! This supersedes the four-way split that the old TUI used
//! (`AssistantChatCell` + `StreamController` + `AgentMessageCell` +
//! thinking window). One source of truth, one render path. See
//! `docs/design/tui-refactor.md` §3.3.
//!
//! Reply body is rendered with a `┃ ` accent-gutter Cursor-style,
//! matching existing visual grammar. A trailing blinking cursor
//! block (`▎`) is appended to the final rendered line while the
//! cell is still live, as a "more is coming" cue.

use std::any::Any;

use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};

use super::HistoryCell;
use crate::tui::markdown_render::render_markdown_text_with_width;
use crate::tui::render::line_utils::line_to_static;
use crate::tui::turn_event::TurnEvent;

#[derive(Debug, Clone)]
pub(crate) struct AssistantCell {
    /// Raw markdown. Growing during stream via `push_delta`.
    source: String,
    /// `true` while tokens can still arrive. Flipped to false by
    /// `finalize()`.
    live: bool,
    ts: Option<String>,
}

impl AssistantCell {
    pub fn new_streaming() -> Self {
        Self {
            source: String::new(),
            live: true,
            ts: None,
        }
    }

    /// Construct from a complete markdown blob — e.g. replay on
    /// resume, or a non-streaming model reply. Not live.
    #[allow(dead_code)]
    pub fn from_markdown(markdown: impl Into<String>) -> Self {
        Self {
            source: markdown.into(),
            live: false,
            ts: None,
        }
    }

    #[allow(dead_code)]
    pub fn with_ts(mut self, ts: impl Into<String>) -> Self {
        self.ts = Some(ts.into());
        self
    }

    /// Resume constructor.
    #[allow(dead_code)]
    pub fn from_persist(ev: TurnEvent) -> Option<Self> {
        match ev {
            TurnEvent::Assistant { ts, markdown } => Some(Self {
                source: markdown,
                live: false,
                ts,
            }),
            _ => None,
        }
    }

    /// Append streamed content. Deliberately takes `&str` (not
    /// char-by-char) so callers can buffer per-token or per-SSE
    /// event; the render pipeline wraps internally.
    pub fn push_delta(&mut self, delta: &str) {
        debug_assert!(self.live, "push_delta on finalised AssistantCell");
        self.source.push_str(delta);
    }

    #[allow(dead_code)]
    pub fn source(&self) -> &str {
        &self.source
    }
}

impl HistoryCell for AssistantCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        // Reserve two columns for the `┃ ` gutter so tables,
        // horizontal rules, and code blocks don't overflow the
        // terminal and wrap mid-border. The width floor (20)
        // keeps the renderer sane on very small terminals.
        let inner_w = (width as usize).saturating_sub(2).max(20);
        let text = render_markdown_text_with_width(&self.source, Some(inner_w));
        let rendered: Vec<Line<'static>> = text.lines.iter().map(line_to_static).collect();

        if rendered.is_empty() {
            return Vec::new();
        }

        let theme = crate::tui::theme::current();
        let gutter_style = Style::default().fg(theme.accent).bold();
        let last_idx = rendered.len() - 1;

        rendered
            .into_iter()
            .enumerate()
            .map(|(i, line)| {
                let mut spans = vec![Span::styled("┃ ", gutter_style)];
                spans.extend(line.spans.iter().cloned());
                // Trailing blink cursor on the final rendered line
                // while the cell is still receiving tokens — users
                // need to see "more is coming" without the entire
                // line animating.
                if self.live && i == last_idx {
                    spans.push(Span::styled(
                        "▎",
                        Style::default()
                            .add_modifier(Modifier::SLOW_BLINK)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                Line::from(spans)
            })
            .collect()
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
        self.live = false;
    }

    fn to_persist(&self) -> Option<TurnEvent> {
        // Persist even live cells so that a crash mid-stream still
        // leaves a partial transcript record. The live/finalised
        // distinction is an in-memory-render concern; the disk
        // format just stores the current markdown.
        Some(TurnEvent::Assistant {
            ts: self.ts.clone(),
            markdown: self.source.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    fn render(cell: &AssistantCell, width: u16, height: u16) -> String {
        let lines = cell.display_lines(width);
        let p =
            ratatui::widgets::Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false });
        buffer_to_string(&draw_widget(p, width, height))
    }

    // ── Streaming lifecycle ──────────────────────────────────────

    #[test]
    fn new_streaming_is_live() {
        let c = AssistantCell::new_streaming();
        assert!(c.is_live());
        assert_eq!(c.source(), "");
    }

    #[test]
    fn push_delta_appends_to_source() {
        let mut c = AssistantCell::new_streaming();
        c.push_delta("Hello ");
        c.push_delta("world");
        assert_eq!(c.source(), "Hello world");
    }

    #[test]
    fn finalize_flips_is_live() {
        let mut c = AssistantCell::new_streaming();
        c.push_delta("done");
        assert!(c.is_live());
        c.finalize();
        assert!(!c.is_live());
    }

    #[test]
    fn from_markdown_is_not_live() {
        let c = AssistantCell::from_markdown("# Complete");
        assert!(!c.is_live());
    }

    // ── Render ───────────────────────────────────────────────────

    #[test]
    fn renders_gutter_on_every_line() {
        let c = AssistantCell::from_markdown("first\n\nsecond");
        let out = render(&c, 60, 4);
        for row in out.lines().filter(|l| !l.is_empty()) {
            assert!(
                row.starts_with('┃'),
                "every rendered row needs the ┃ gutter: {row:?}"
            );
        }
    }

    #[test]
    fn live_cell_trailing_cursor_appears_on_last_line_only() {
        // Use a paragraph break (`\n\n`) so markdown produces two
        // separate rendered lines. A soft break in markdown is
        // treated as whitespace, which would yield one rendered
        // line — correct behaviour, just not what this test needs
        // to observe.
        let mut c = AssistantCell::new_streaming();
        c.push_delta("line one\n\nline two");
        let out = render(&c, 60, 4);
        let rows: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
        assert!(
            rows.len() >= 2,
            "expected at least two non-blank rows: {out}"
        );
        // Cursor shouldn't appear on earlier lines.
        assert!(
            !rows[0].contains('▎'),
            "cursor leaked on first line: {:?}",
            rows[0]
        );
        // And must appear on the last one.
        assert!(
            rows.last().unwrap().contains('▎'),
            "cursor missing on last line: {:?}",
            rows.last()
        );
    }

    #[test]
    fn finalised_cell_has_no_cursor() {
        let mut c = AssistantCell::new_streaming();
        c.push_delta("answer");
        c.finalize();
        let out = render(&c, 60, 2);
        assert!(
            !out.contains('▎'),
            "cursor must vanish after finalize: {out}"
        );
    }

    #[test]
    fn empty_cell_renders_nothing() {
        let c = AssistantCell::new_streaming();
        let out = render(&c, 60, 2).trim().to_string();
        assert!(
            out.is_empty() || out.chars().all(char::is_whitespace),
            "empty source should produce nothing: {out:?}"
        );
    }

    #[test]
    fn inline_code_backticks_are_stripped() {
        // Guards the fix ported from d1cfb0f3a — `Event::Code`
        // renders the inner text, not backticks-around-it.
        let c = AssistantCell::from_markdown("use `foo()` here");
        let out = render(&c, 60, 1);
        assert!(out.contains("foo()"));
        assert!(
            !out.contains("`foo()`"),
            "inline code should not keep backticks: {out}"
        );
    }

    // ── Persistence ──────────────────────────────────────────────

    #[test]
    fn persist_roundtrip_preserves_markdown() {
        let orig = AssistantCell::from_markdown("# Plan\n\n- a\n- b");
        let ev = orig.to_persist().unwrap();
        let back = AssistantCell::from_persist(ev).unwrap();
        assert_eq!(back.source(), orig.source());
        assert!(!back.is_live(), "reloaded cell must not be live");
    }

    #[test]
    fn live_cell_still_persists_in_flight_markdown() {
        // A crash mid-stream should leave SOMETHING in the journal
        // — half a reply is more useful than nothing. Persistence
        // is therefore not gated on `live == false`.
        let mut c = AssistantCell::new_streaming();
        c.push_delta("partial");
        match c.to_persist() {
            Some(TurnEvent::Assistant { markdown, .. }) => {
                assert_eq!(markdown, "partial");
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn from_persist_rejects_wrong_variant() {
        let wrong = TurnEvent::User {
            ts: None,
            text: "x".into(),
        };
        assert!(AssistantCell::from_persist(wrong).is_none());
    }

    // ── Snapshots ────────────────────────────────────────────────

    #[test]
    fn snapshot_simple_paragraph_60() {
        let c = AssistantCell::from_markdown("Here is the plan:\n\n- step one\n- step two");
        insta::assert_snapshot!("assistant_paragraph_60", render(&c, 60, 5));
    }

    #[test]
    fn snapshot_with_inline_code_60() {
        let c = AssistantCell::from_markdown("Use `cargo build` to compile the project.");
        insta::assert_snapshot!("assistant_inline_code_60", render(&c, 60, 2));
    }
}
