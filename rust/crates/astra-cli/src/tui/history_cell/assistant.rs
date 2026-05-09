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
//!
//! ## Inline `<think>` handling
//!
//! Some providers bundle the model's chain-of-thought into the
//! main token stream as `<think>…</think>` rather than using the
//! separate thinking protocol (which would build a `ReasoningCell`
//! instead). This cell detects a leading `<think>` block in its
//! source and renders:
//!
//! - **Live, pre-`</think>`** — partial thinking content shown dim
//!   so the user knows the model isn't frozen. Session 881fc081
//!   demonstrated the failure mode without this: 60-second-long
//!   thinks would show as raw `<think>The user is asking…` text.
//! - **After `</think>`** — collapsed to a one-line
//!   `✻ Thought (N lines)` dim header; the body markdown starts
//!   immediately under it.
//!
//! Thinking content stays in `source` and is persisted unchanged
//! so resume + future `/think toggle` can surface it.

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

    /// Split the raw source into the optional leading `<think>`
    /// block and the reply body. Returns
    /// `(think_inner, think_closed, body)`:
    ///
    /// - `think_inner`: content *inside* the tags, not including
    ///   the opening/closing markers. `None` if the source didn't
    ///   start with `<think>` at all.
    /// - `think_closed`: `true` once `</think>` has arrived;
    ///   `false` means we're mid-think (still streaming).
    /// - `body`: everything after `</think>` (empty while still
    ///   thinking). Always the raw-markdown slice the existing
    ///   renderer expects.
    ///
    /// Only the LEADING `<think>` matters — the model sometimes
    /// re-uses `<think>` mid-reply as a code fence or prose
    /// reference, and treating those as meta tags would mangle
    /// the rendered output. The opening tag is matched only when
    /// it's the first non-whitespace content in `source`.
    fn split_think(&self) -> (Option<&str>, bool, &str) {
        let trimmed = self.source.trim_start();
        let leading_ws = self.source.len() - trimmed.len();
        let Some(after_open) = trimmed.strip_prefix("<think>") else {
            return (None, false, &self.source);
        };
        let Some(close_rel) = after_open.find("</think>") else {
            // Still streaming the think block; body is empty.
            return (Some(after_open), false, "");
        };
        let think_inner = &after_open[..close_rel];
        // Body starts after `</think>`. Use byte offsets relative
        // to `self.source` to keep lifetimes tied to &self.
        let open_tag_len = "<think>".len();
        let close_tag_len = "</think>".len();
        let body_start = leading_ws + open_tag_len + close_rel + close_tag_len;
        let body = self.source[body_start..].trim_start_matches('\n');
        (Some(think_inner), true, body)
    }
}

impl HistoryCell for AssistantCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let (think_inner, think_closed, body) = self.split_think();

        // Branch 1: no <think> block. Original render path.
        let Some(think) = think_inner else {
            return render_body_with_gutter(&self.source, width, self.live);
        };

        // Branch 2: <think> still open (streaming). Show a dim
        // thinking header + the partial think content so the user
        // sees progress instead of a frozen screen. No body yet.
        if !think_closed {
            return render_live_thinking(think, width);
        }

        // Branch 3: <think> closed. Collapse to a one-line
        // `✻ Thought (N lines)` header, then render the body.
        let think_lines = think.trim().lines().count().max(1);
        let mut out = thought_header_lines(think_lines);
        out.extend(render_body_with_gutter(body, width, self.live));
        out
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

/// Render the reply body the classic way — `┃ ` accent gutter on
/// every line, optional blink cursor on the last line while live.
/// Factored out so the `<think>`-aware `display_lines` can reuse
/// it for the post-`</think>` body without duplicating layout.
fn render_body_with_gutter(source: &str, width: u16, live: bool) -> Vec<Line<'static>> {
    if source.trim().is_empty() {
        return Vec::new();
    }
    // Reserve two columns for the `┃ ` gutter so tables, horizontal
    // rules, and code blocks don't overflow the terminal and wrap
    // mid-border. The width floor (20) keeps the renderer sane on
    // very small terminals.
    let inner_w = (width as usize).saturating_sub(2).max(20);
    let text = render_markdown_text_with_width(source, Some(inner_w));
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
            if live && i == last_idx {
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

/// Collapsed one-line header shown in place of a `<think>` block
/// once the closing tag has arrived.
///
/// Example: `  ✻ Thought (12 lines)`
fn thought_header_lines(line_count: usize) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let label = if line_count == 1 {
        "Thought (1 line)".to_string()
    } else {
        format!("Thought ({line_count} lines)")
    };
    vec![Line::from(vec![
        Span::raw("  "),
        Span::styled("✻ ", dim),
        Span::styled(label, dim),
    ])]
}

/// While `<think>` is open and the model is still streaming,
/// show a dim one-line "Thinking…" indicator plus the most recent
/// line of thinking content so the user can see something is
/// happening. Full thinking body stays hidden to avoid dumping
/// 40+ lines of internal monologue on screen; the closed-think
/// header is what the user sees once the block finishes.
fn render_live_thinking(think_partial: &str, _width: u16) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    // Last non-blank line is the most useful "where are we now"
    // preview. Truncate to fit without wrapping to keep the
    // viewport stable while content scrolls in.
    let last_line = think_partial
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    const MAX_PREVIEW: usize = 80;
    let preview: String = if last_line.chars().count() > MAX_PREVIEW {
        let truncated: String = last_line.chars().take(MAX_PREVIEW).collect();
        format!("{truncated}…")
    } else {
        last_line.to_string()
    };

    let mut out = vec![Line::from(vec![
        Span::raw("  "),
        Span::styled("✻ ", dim),
        Span::styled("Thinking…", dim),
    ])];
    if !preview.is_empty() {
        out.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(preview, dim),
        ]));
    }
    out
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

    // ── <think> block rendering ──────────────────────────────────

    #[test]
    fn leading_think_block_is_not_shown_raw_in_body() {
        // Regression: session 881fc081 dumped entire `<think>…</think>`
        // into scrollback as literal text. Splitting + collapsing
        // must be the only way a closed think block surfaces.
        let c = AssistantCell::from_markdown(
            "<think>\nThe user is asking X.\nI should do Y.\n</think>\n\nI am Astra.",
        );
        let out = render(&c, 60, 5);
        assert!(
            !out.contains("<think>") && !out.contains("</think>"),
            "think tags must not leak to the rendered body: {out}"
        );
        assert!(
            !out.contains("The user is asking X"),
            "think INNER content must be hidden once closed: {out}"
        );
        assert!(
            out.contains("Thought (2 lines)"),
            "collapsed header missing: {out}"
        );
        assert!(out.contains("I am Astra"), "body missing: {out}");
    }

    #[test]
    fn closed_think_header_pluralisation_handles_single_line() {
        let c = AssistantCell::from_markdown("<think>just one</think>\n\nbody");
        let out = render(&c, 60, 3);
        assert!(
            out.contains("Thought (1 line)"),
            "singular form for one-line think: {out}"
        );
    }

    #[test]
    fn mid_body_think_tag_is_not_treated_as_metadata() {
        // The model might discuss HTML tags. Only a LEADING `<think>`
        // at the very start of the reply counts; a later one is
        // just prose (which pulldown-cmark escapes anyway).
        let c = AssistantCell::from_markdown("The token <think> is used for…");
        let out = render(&c, 60, 2);
        assert!(
            !out.contains("Thought"),
            "non-leading think tag shouldn't trigger collapse: {out}"
        );
    }

    #[test]
    fn streaming_think_shows_indicator_and_latest_line() {
        // While the think block is still open, we show "Thinking…"
        // + the most recent line of internal monologue so the
        // user sees motion.
        let mut c = AssistantCell::new_streaming();
        c.push_delta("<think>\nstep one\nstep two in progres");
        let out = render(&c, 80, 3);
        assert!(out.contains("Thinking…"), "live indicator missing: {out}");
        assert!(
            out.contains("step two in progres"),
            "latest thinking line missing: {out}"
        );
        assert!(
            !out.contains("step one"),
            "only the most recent line should preview (keeps viewport stable): {out}"
        );
        // Body hasn't arrived yet — no `┃ ` gutter.
        assert!(
            !out.contains('┃'),
            "body gutter shouldn't render while still thinking: {out}"
        );
    }

    #[test]
    fn transition_from_open_to_closed_think_swaps_to_collapsed_header() {
        let mut c = AssistantCell::new_streaming();
        c.push_delta("<think>one\ntwo\nthree</think>\n\nanswer");
        let out = render(&c, 60, 5);
        assert!(
            out.contains("Thought (3 lines)"),
            "collapsed header with line count: {out}"
        );
        assert!(out.contains("answer"), "body still visible: {out}");
    }

    #[test]
    fn think_block_stays_in_persisted_markdown() {
        // Think content is part of the on-disk transcript — a
        // future `/think on` or analysis tool might surface it.
        // The render layer collapses, but persistence is raw.
        let c = AssistantCell::from_markdown("<think>reason</think>\n\nbody");
        match c.to_persist().unwrap() {
            TurnEvent::Assistant { markdown, .. } => {
                assert!(markdown.contains("<think>"), "disk should keep tags");
                assert!(markdown.contains("reason"), "disk should keep inner");
            }
            _ => panic!("wrong variant"),
        }
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

    #[test]
    fn snapshot_closed_think_then_body_60() {
        let c = AssistantCell::from_markdown(
            "<think>\nThe user is asking a question.\nI will answer briefly.\n</think>\n\nHello — happy to help.",
        );
        insta::assert_snapshot!("assistant_think_closed_60", render(&c, 60, 3));
    }

    #[test]
    fn snapshot_streaming_think_preview_80() {
        let mut c = AssistantCell::new_streaming();
        c.push_delta("<think>\nFirst, let me think about the structure.\nActually, the user wants a concise answer.");
        insta::assert_snapshot!("assistant_think_streaming_80", render(&c, 80, 3));
    }
}
