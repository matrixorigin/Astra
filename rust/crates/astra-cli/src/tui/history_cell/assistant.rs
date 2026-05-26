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
//! Reply body is rendered with a `█ ` accent-gutter block-style,
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
use std::time::Instant;

use ratatui::style::{Color, Modifier, Style};
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
    /// First `push_delta` timestamp. Used with `token_estimate` to
    /// show tokens/s on a live cell. `None` for non-streaming
    /// constructors (resume, `from_markdown`) so replayed replies
    /// don't show a stale rate.
    started_at: Option<Instant>,
    /// CJK-aware token estimate accumulated as chars arrive.
    /// CJK ideographs ≈ 0.5 token each, other chars ≈ 0.25. Not an
    /// exact count (the real tokenizer lives server-side and the
    /// number only returns in TurnStats) but close enough that the
    /// on-screen "42 tok/s · 1.2k" matches reality within ±15%.
    token_estimate: f64,
    /// Stamped by `finalize()`. Lets the active-slot gradient gutter
    /// pin its phase at the freeze moment instead of snapping to
    /// `t = 0` on the post-freeze frame.
    frozen_at: super::FreezeStamp,
}

impl AssistantCell {
    pub fn new_streaming() -> Self {
        Self {
            source: String::new(),
            live: true,
            ts: None,
            started_at: None,
            token_estimate: 0.0,
            frozen_at: super::FreezeStamp::default(),
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
            started_at: None,
            token_estimate: 0.0,
            frozen_at: super::FreezeStamp::default(),
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
                started_at: None,
                token_estimate: 0.0,
                // Resumed from persistence — already settled. See
                // `FreezeStamp::revived` for the launch-independent
                // phase rationale.
                frozen_at: super::FreezeStamp::revived(),
            }),
            _ => None,
        }
    }

    /// Append streamed content. Deliberately takes `&str` (not
    /// char-by-char) so callers can buffer per-token or per-SSE
    /// event; the render pipeline wraps internally.
    pub fn push_delta(&mut self, delta: &str) {
        debug_assert!(self.live, "push_delta on finalised AssistantCell");
        if self.started_at.is_none() {
            self.started_at = Some(Instant::now());
        }
        self.token_estimate += estimate_tokens(delta);
        self.source.push_str(delta);
    }

    #[allow(dead_code)]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Split the raw source into the optional thinking prefix and
    /// the reply body. Returns `(think_inner, think_closed, body)`:
    ///
    /// - `think_inner`: the aggregated thinking content (may come
    ///   from an explicit `<think>` tag OR from prose that precedes
    ///   a bare terminal `</think>` — see below). `None` iff the
    ///   source has no thinking markers we recognise.
    /// - `think_closed`: `true` once `</think>` has arrived.
    /// - `body`: everything after the close tag, if any; the full
    ///   source otherwise.
    ///
    /// The tricky case: MiniMax / DeepSeek / GLM strip the opening
    /// `<think>` before the edge sees it (the server routes the
    /// reasoning content via the separate thinking channel), but
    /// the prose body still carries a bare `</think>` to close the
    /// window. Cases handled:
    ///
    /// 1. Matched `<think>…</think>`: inner is between the tags.
    ///    Leading-`<think>` only; a mid-body `<think>` is just a
    ///    prose reference.
    /// 2. Source ENDS with `</think>`: the whole prose is leaked
    ///    thinking (inner = prose before the close, body = empty).
    ///    The "ends with" test is the critical guard against
    ///    false-positives — regular prose that mentions `</think>`
    ///    mid-sentence (e.g. a code review of this very function)
    ///    would otherwise get truncated at the last tag mention.
    /// 3. Source has `<think>` without a matching close: streaming
    ///    think.
    /// 4. Source has `</think>` but doesn't end with it: treated as
    ///    plain prose (the tag is a reference, not a terminator).
    /// 5. No tags: plain prose.
    ///
    /// Real-world MiniMax emits thinking-only cells that either
    /// contain just `</think>` or end with `</think>` — the body
    /// always lands in a later cell. So "ends with close" captures
    /// every legitimate leaked-thinking pattern without mangling
    /// prose that mentions the tag.
    /// Compose the ` · N tok/s · X` status span that trails the
    /// final rendered line while the cell is live. Returns `None`
    /// when the cell hasn't received a delta yet (would divide by
    /// zero) or when the cell is no longer streaming.
    fn rate_suffix_span(&self) -> Option<String> {
        if !self.live {
            return None;
        }
        let started = self.started_at?;
        let elapsed = started.elapsed().as_secs_f64();
        if elapsed < 0.3 || self.token_estimate <= 0.0 {
            return Some(format!(" · {}", format_token_estimate(self.token_estimate)));
        }
        let tok_per_s = self.token_estimate / elapsed;
        Some(format!(
            " · {} tok/s · {}",
            tok_per_s.round() as u64,
            format_token_estimate(self.token_estimate),
        ))
    }

    fn split_think(&self) -> (Option<&str>, bool, &str) {
        let source = self.source.as_str();
        let trimmed = source.trim_start();
        if trimmed.is_empty() {
            return (None, false, source);
        }
        let close_tag = "</think>";
        let open_tag = "<think>";

        // Case 1 / 3: explicit `<think>` at the very start.
        if let Some(after_open) = trimmed.strip_prefix(open_tag) {
            if let Some(close_rel) = after_open.find(close_tag) {
                let think_inner = &after_open[..close_rel];
                let leading_ws = source.len() - trimmed.len();
                let body_start = leading_ws + open_tag.len() + close_rel + close_tag.len();
                let body = source[body_start..].trim_start_matches('\n');
                return (Some(think_inner), true, body);
            }
            // Still streaming the think block — no close yet.
            return (Some(after_open), false, "");
        }

        // Case 2: source ends with `</think>` → all of it is leaked
        // thinking. Only ONE `</think>` position is considered (the
        // trailing one); multiple `</think>` in prose is so unusual
        // we don't try to be cleverer about it.
        //
        // The test has to handle trailing whitespace from the
        // streaming wire (partial chunks often end in `\n`).
        if source.trim_end().ends_with(close_tag) {
            let trimmed_end = source.trim_end();
            let last_close = trimmed_end.len() - close_tag.len();
            let think_inner = trimmed_end[..last_close].trim();
            return (Some(think_inner), true, "");
        }

        // Cases 4 & 5: `</think>` mid-body is prose, NOT metadata.
        // Don't collapse — the reply includes prose references to
        // the tag (e.g. a code review discussing this function).
        (None, false, source)
    }
}

impl HistoryCell for AssistantCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let (think_inner, think_closed, body) = self.split_think();
        let rate_suffix = self.rate_suffix_span();

        // Branch 1: no <think> block. Original render path.
        let Some(think) = think_inner else {
            return render_body_with_gutter(&self.source, width, self.live, rate_suffix);
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
        out.extend(render_body_with_gutter(body, width, self.live, rate_suffix));
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
        self.frozen_at.stamp_now();
    }

    fn frozen_phase(&self) -> Option<f32> {
        self.frozen_at.phase()
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

/// CJK-aware token estimator. Roughly the same heuristic the
/// `tiktoken` family's BPE produces on mixed-language prose:
/// Han ideographs + Hiragana/Katakana ≈ 0.5 token/char (one glyph
/// usually splits into one or two BPE pieces), other scripts ≈
/// 0.25 token/char (4 chars/token on avg for English). Non-BMP
/// emoji count as full tokens each. Good to ±15% which is all we
/// need for "42 tok/s" feedback.
fn estimate_tokens(s: &str) -> f64 {
    let mut total = 0.0_f64;
    for ch in s.chars() {
        let c = ch as u32;
        let w = if (0x3040..=0x30FF).contains(&c)
            || (0x4E00..=0x9FFF).contains(&c)
            || (0x3400..=0x4DBF).contains(&c)
            || (0xAC00..=0xD7AF).contains(&c)
        {
            0.5
        } else if c >= 0x10000 {
            1.0
        } else {
            0.25
        };
        total += w;
    }
    total
}

/// Human format for a running token estimate: `420`, `5.1k`,
/// `12.4k`. Stays short so the status span fits on the final
/// rendered row next to the prose.
fn format_token_estimate(tokens: f64) -> String {
    if tokens < 1_000.0 {
        format!("{:.0}", tokens)
    } else {
        format!("{:.1}k", tokens / 1_000.0)
    }
}

/// Three-dot rhythm indicator frames. Each dot toggles on/off at
/// a different phase so the indicator reads as "tokens arriving"
/// rather than a static spinner. Frame rate is wall-clock based
/// via the shared shimmer clock.
fn rhythm_dots_span() -> Span<'static> {
    let t = crate::tui::shimmer::elapsed_since_start().as_millis() as u64;
    // 450 ms cycle: each of the three phases gets one-third. Use
    // '·' (mid-dot) for "on" and ' ' for "off" so the line width
    // stays constant (4 display cells: " · · ·" collapsed).
    const FRAMES: [&str; 6] = [
        "·    ", //
        "· ·  ", //
        "· · ·", //
        " · · ", //
        "   · ", //
        "     ", //
    ];
    let idx = ((t / 120) % FRAMES.len() as u64) as usize;
    Span::styled(
        FRAMES[idx].to_string(),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
}

/// Render the reply body. Settled (scrollback) cells get a static
/// `█ ` accent gutter on every line. Live cells drop the gutter —
/// the active-slot wrapper (`tui::LiveFramedCell`) paints its own
/// gradient `█` at the same column, and stacking both produces a
/// visible double bar.
fn render_body_with_gutter(
    source: &str,
    width: u16,
    live: bool,
    rate_suffix: Option<String>,
) -> Vec<Line<'static>> {
    if source.trim().is_empty() {
        return Vec::new();
    }
    // Settled cells prepend `█ ` (2 cols) to every line, so wrap text
    // at `width - 2` to avoid terminal hard-wrap overflow. Live cells
    // already receive `width - 2` from `active_viewport` (the
    // `LiveFramedCell` wrapper paints its own gutter column) — so the
    // asymmetry below is intentional. Do *not* "unify" the arms by
    // adding `saturating_sub(2)` to both: that double-subtracts on
    // the live path and re-introduces a one-column re-wrap when a
    // cell transitions live → settled near the floor boundary.
    let prepend_gutter = !live;
    let inner_w = if prepend_gutter {
        (width as usize).saturating_sub(2).max(20)
    } else {
        (width as usize).max(20)
    };
    let text = render_markdown_text_with_width(source, Some(inner_w));
    let rendered: Vec<Line<'static>> = text.lines.iter().map(line_to_static).collect();

    if rendered.is_empty() {
        return Vec::new();
    }

    let theme = crate::tui::theme::current();
    let gutter_style = Style::default().fg(theme.gutter_frozen);
    let dim = Style::default().fg(Color::DarkGray);
    let last_idx = rendered.len() - 1;

    rendered
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 3);
            if prepend_gutter {
                spans.push(Span::styled("█ ", gutter_style));
            }
            spans.extend(line.spans.iter().cloned());
            // Trailing rhythm-dot indicator + tok/s suffix on the
            // final row while live. Replaces the old `▎` slow-blink
            // cursor — a three-dot staggered rhythm reads as
            // "tokens arriving" rather than "generic spinner".
            if live && i == last_idx {
                spans.push(Span::raw(" "));
                spans.push(rhythm_dots_span());
                if let Some(ref suffix) = rate_suffix {
                    spans.push(Span::styled(suffix.clone(), dim));
                }
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

/// Max body rows shown beneath the `✻ Thinking…` header while the
/// `<think>` block is open. Mirrors `ReasoningCell::LIVE_PREVIEW_MAX_ROWS`:
/// once the window fills, the oldest row scrolls off and a
/// `⋯ +N more` counter takes the top slot, so the preview stays
/// fixed-height instead of pushing the composer off-screen on a
/// long internal monologue.
const LIVE_THINK_PREVIEW_MAX_ROWS: usize = 6;

/// While `<think>` is open and the model is still streaming, show a
/// shimmering "Thinking…" header plus a fixed-height scrolling
/// preview of the latest thinking content. Matches `ReasoningCell`'s
/// live-preview grammar so the two paths (inline `<think>` vs.
/// provider reasoning channel) feel consistent to the user. Full
/// thinking body stays hidden — once `</think>` arrives this whole
/// block collapses to a one-line `✻ Thought (N lines)` header.
fn render_live_thinking(think_partial: &str, width: u16) -> Vec<Line<'static>> {
    let dim_italic = Style::default()
        .add_modifier(Modifier::DIM)
        .add_modifier(Modifier::ITALIC);

    // Header line — shimmered so there's a visible "still working"
    // cue even if the preview body happens to be empty for a tick.
    let mut header_spans: Vec<Span<'static>> = vec![Span::raw("  ")];
    header_spans.push(Span::styled("✻ ", dim_italic));
    header_spans.extend(crate::tui::shimmer::shimmer_spans("Thinking…"));
    let mut out = vec![Line::from(header_spans)];

    // Soft-wrap the partial thinking content, then render the most
    // recent N rows, with a `⋯ +M more` counter in the first slot
    // once overflow starts. Rendered at width-6 because we prefix
    // each body row with "    " (4 cols of indent) and the outer
    // frame already reserves 2 cols for its border — total 6.
    let inner_w = (width as usize).saturating_sub(6).max(10);
    let mut body_rows: Vec<String> = Vec::new();
    for logical in think_partial.lines() {
        if logical.trim().is_empty() {
            continue;
        }
        for row in soft_wrap_preview(logical, inner_w) {
            body_rows.push(row);
        }
    }

    let total = body_rows.len();
    let visible_start = if total > LIVE_THINK_PREVIEW_MAX_ROWS {
        // Row 0 of the body shows the overflow counter; the window
        // displays the last `LIVE_THINK_PREVIEW_MAX_ROWS - 1` rows.
        let tail = LIVE_THINK_PREVIEW_MAX_ROWS - 1;
        let hidden = total - tail;
        out.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(format!("⋯ +{hidden} more"), dim_italic),
        ]));
        total - tail
    } else {
        0
    };
    for row in body_rows.into_iter().skip(visible_start) {
        out.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(row, dim_italic),
        ]));
    }
    out
}

/// Break a logical line into visual rows at `width` display cells —
/// same behaviour as `ReasoningCell::soft_wrap`, duplicated here so
/// the assistant cell can preview leaked-`<think>` content without
/// pulling in the reasoning module.
fn soft_wrap_preview(input: &str, width: usize) -> Vec<String> {
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
                row.starts_with('█'),
                "every rendered row needs the █ gutter: {row:?}"
            );
        }
    }

    #[test]
    fn live_cell_rhythm_indicator_appears_on_last_line_only() {
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
        // The rhythm-dot indicator contains at least one '·'
        // character. Token counter text (" · N tok/s") also has
        // them — so we check that the LAST row has both the
        // prose and a trailing dot sequence, while the first row
        // ends on prose alone.
        assert!(
            !rows[0].trim_end().ends_with('·'),
            "rhythm indicator leaked on first line: {:?}",
            rows[0]
        );
        assert!(
            rows.last().unwrap().contains('·'),
            "rhythm indicator missing on last line: {:?}",
            rows.last()
        );
    }

    #[test]
    fn finalised_cell_has_no_live_indicator() {
        let mut c = AssistantCell::new_streaming();
        c.push_delta("answer");
        c.finalize();
        let out = render(&c, 60, 2);
        // Old cursor `▎` must be gone and no tok/s suffix should
        // appear on a finalised cell.
        assert!(
            !out.contains('▎'),
            "old cursor must vanish after finalize: {out}"
        );
        assert!(
            !out.contains("tok/s"),
            "rate suffix must not render on a finalised cell: {out}"
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
    fn streaming_think_shows_indicator_and_recent_lines() {
        // While the `<think>` block is still open we show
        // `✻ Thinking…` plus a scrolling preview of the latest
        // rows of internal monologue (see `LIVE_THINK_PREVIEW_MAX_ROWS`).
        // With only two lines of content, the whole body fits in
        // the preview window — no overflow counter, both rows
        // visible.
        let mut c = AssistantCell::new_streaming();
        c.push_delta("<think>\nstep one\nstep two in progres");
        let out = render(&c, 80, 5);
        assert!(out.contains("Thinking…"), "live indicator missing: {out}");
        assert!(
            out.contains("step two in progres"),
            "latest thinking line missing: {out}"
        );
        assert!(
            out.contains("step one"),
            "earlier line must stay visible under the window cap: {out}"
        );
        // Body hasn't arrived yet — no `█ ` gutter.
        assert!(
            !out.contains('█'),
            "body gutter shouldn't render while still thinking: {out}"
        );
    }

    #[test]
    fn streaming_think_preview_caps_and_shows_counter_on_overflow() {
        // Once the running think body exceeds the preview window,
        // the oldest rows drop off and a `⋯ +N more` counter takes
        // the first slot so the user sees there's hidden content.
        let mut c = AssistantCell::new_streaming();
        let total_rows = LIVE_THINK_PREVIEW_MAX_ROWS + 4;
        let padding: Vec<String> = (1..=total_rows).map(|i| format!("row {i}")).collect();
        c.push_delta(&format!("<think>\n{}", padding.join("\n")));
        let out = render(&c, 80, (LIVE_THINK_PREVIEW_MAX_ROWS + 2) as u16);
        let hidden = total_rows - (LIVE_THINK_PREVIEW_MAX_ROWS - 1);
        assert!(
            out.contains(&format!("⋯ +{hidden} more")),
            "overflow counter missing: {out}"
        );
        assert!(
            !out.contains("row 1 "),
            "oldest row must have scrolled off: {out}"
        );
        assert!(
            out.contains(&format!("row {total_rows}")),
            "most recent row must be visible: {out}"
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
    fn bare_trailing_close_tag_treats_prose_as_thinking() {
        // Regression: MiniMax / GLM / DeepSeek strip the opening
        // `<think>` tag before the CLI sees it (thinking content
        // comes through the separate reasoning channel) but the
        // closing `</think>` tag leaks into the main body stream.
        // So a cell that looks like `preamble prose</think>` is
        // entirely thinking — the "prose" is leaked reasoning.
        let c = AssistantCell::from_markdown(
            "The user wants me to review the commit. Let me fetch the diff.</think>",
        );
        let out = render(&c, 80, 3);
        assert!(
            out.contains("Thought"),
            "bare trailing `</think>` must trigger collapse: {out}"
        );
        assert!(
            !out.contains("The user wants me to review"),
            "leaked thinking prose must not show: {out}"
        );
    }

    #[test]
    fn mid_body_close_tag_is_not_treated_as_thinking_terminator() {
        // Regression: a code review of the `<think>` handling code
        // literally contains the string `</think>` inline (e.g.
        // `` `</think>` `` in backticks). If we collapsed on any
        // `</think>` the review would get truncated mid-body.
        // Real leaked-thinking cells END with `</think>`; prose
        // references appear mid-sentence, so "ends with" is the
        // discriminator.
        let c = AssistantCell::from_markdown(
            "The review of the `</think>` handler found a bug: the splitter\n\
             treats bare `</think>` mentions as if they were real terminators,\n\
             which truncates prose that discusses the tag.",
        );
        let out = render(&c, 80, 4);
        assert!(
            !out.contains("Thought"),
            "mid-body `</think>` must NOT trigger collapse: {out}"
        );
        assert!(
            out.contains("truncates prose"),
            "entire prose must render — no silent truncation: {out}"
        );
    }

    #[test]
    fn leading_close_tag_alone_renders_empty_thought_header() {
        // First assistant cell in a turn can be just `</think>` —
        // the model closed a reasoning window but hasn't emitted
        // body content yet. We still want a header so the user
        // sees that thinking happened.
        let c = AssistantCell::from_markdown("</think>");
        let out = render(&c, 60, 2);
        assert!(
            out.contains("Thought"),
            "bare leading `</think>` should still collapse: {out}"
        );
        assert!(!out.contains("</think>"), "tag must not appear raw: {out}");
    }

    #[test]
    fn thinking_cell_that_ends_with_close_tag_collapses_entirely() {
        // Real MiniMax pattern: each thinking segment is its own
        // assistant cell whose content ENDS with `</think>`. The
        // body — if any — arrives in a later cell without tags.
        let c = AssistantCell::from_markdown(
            "The user wants a commit review. Let me fetch the diff first.</think>",
        );
        let out = render(&c, 80, 2);
        assert!(
            out.contains("Thought"),
            "source that ends with `</think>` is fully leaked thinking: {out}"
        );
        assert!(
            !out.contains("The user wants a commit review"),
            "leaked thinking prose must not render: {out}"
        );
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
        crate::tui::testing::assert_tui_snapshot!("assistant_paragraph_60", render(&c, 60, 5));
    }

    #[test]
    fn snapshot_with_inline_code_60() {
        let c = AssistantCell::from_markdown("Use `cargo build` to compile the project.");
        crate::tui::testing::assert_tui_snapshot!("assistant_inline_code_60", render(&c, 60, 2));
    }

    #[test]
    fn snapshot_closed_think_then_body_60() {
        let c = AssistantCell::from_markdown(
            "<think>\nThe user is asking a question.\nI will answer briefly.\n</think>\n\nHello — happy to help.",
        );
        crate::tui::testing::assert_tui_snapshot!("assistant_think_closed_60", render(&c, 60, 3));
    }

    // ── Token estimate (CJK-aware) ──────────────────────────────

    #[test]
    fn token_estimate_ascii_prose_uses_quarter_ratio() {
        // 12 ASCII chars ≈ 3 tokens at 0.25 each.
        let t = super::estimate_tokens("hello, world");
        assert!(
            (2.5..=3.5).contains(&t),
            "expected ~3 tokens for 12 ASCII chars, got {t}"
        );
    }

    #[test]
    fn token_estimate_cjk_uses_half_ratio() {
        // 4 CJK ideographs → ~2 tokens.
        let t = super::estimate_tokens("你好世界");
        assert!(
            (1.8..=2.2).contains(&t),
            "expected ~2 tokens for 4 CJK chars, got {t}"
        );
    }

    #[test]
    fn token_estimate_mixed_splits_correctly() {
        // "hello 你好" = 6 ASCII (incl. space) + 2 CJK
        //             = 6 * 0.25 + 2 * 0.5 = 1.5 + 1.0 = 2.5
        let t = super::estimate_tokens("hello 你好");
        assert!(
            (2.3..=2.7).contains(&t),
            "expected ~2.5 tokens for mixed, got {t}"
        );
    }

    #[test]
    fn format_token_estimate_compacts_thousands() {
        assert_eq!(super::format_token_estimate(420.0), "420");
        assert_eq!(super::format_token_estimate(1_234.0), "1.2k");
        assert_eq!(super::format_token_estimate(12_345.0), "12.3k");
    }

    #[test]
    fn rate_suffix_none_when_finalised() {
        let mut c = AssistantCell::new_streaming();
        c.push_delta("some answer text");
        c.finalize();
        assert!(
            c.rate_suffix_span().is_none(),
            "no rate suffix on finalised cells"
        );
    }

    #[test]
    fn rate_suffix_shows_cumulative_before_300ms() {
        // Freshly streaming cell: elapsed < 300ms so we only show
        // the cumulative estimate, not a volatile tok/s figure.
        let mut c = AssistantCell::new_streaming();
        c.push_delta("hi");
        let s = c.rate_suffix_span().expect("live cell with delta");
        assert!(!s.contains("tok/s"), "tok/s too early: {s}");
        assert!(s.contains(" · "), "cumulative estimate missing: {s}");
    }

    #[test]
    fn snapshot_streaming_think_preview_80() {
        let mut c = AssistantCell::new_streaming();
        c.push_delta("<think>\nFirst, let me think about the structure.\nActually, the user wants a concise answer.");
        // Height 4: header + 2 preview rows (under cap) + one spare
        // row. The preview no longer single-lines — it scrolls like
        // `ReasoningCell` — so both thinking rows must appear.
        crate::tui::testing::assert_tui_snapshot!(
            "assistant_think_streaming_80",
            render(&c, 80, 4)
        );
    }
}
