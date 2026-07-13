//! Assistant-reply history cell — the final model answer.
//!
//! Owns the **raw markdown source** for the whole reply. Growing
//! the reply during a stream is `push_delta`; finalising freezes
//! the source. `display_lines` caches the exact source+width Markdown
//! layout, invalidating it on mutation. Width changes still re-render
//! correctly without reparsing an unchanged reply on every frame.
//!
//! This supersedes the four-way split that the old TUI used
//! (`AssistantChatCell` + `StreamController` + `AgentMessageCell` +
//! thinking window). One source of truth, one render path. See
//! `docs/design/tui-refactor.md` §3.3.
//!
//! Reply body keeps a stable left accent gutter once settled in
//! scrollback, so long numbered answers still scan like one unified
//! assistant block. Live cells still use the active-slot wrapper for
//! structure, but committed replies retain the visual anchor the old
//! TUI had. A
//! active response is kept visually distinct by the surrounding status line;
//! its content itself stays free of transient transport counters.
//!
//! ## Inline thinking handling
//!
//! Some providers bundle the model's chain-of-thought into the
//! main token stream as `<think>…</think>` or `<thinking>…</thinking>` rather than using the
//! separate thinking protocol (which would build a `ReasoningCell`
//! instead). This cell detects a leading `<think>` block in its
//! source and renders:
//!
//! - **Live, pre-`</think>`** — partial thinking content shown dim
//!   so the user knows the model isn't frozen. Session 881fc081
//!   demonstrated the failure mode without this: 60-second-long
//!   thinks would show as raw `<think>The user is asking…` text.
//! - **After `</think>`** — collapsed to a one-line
//!   `Thought · N lines · N tokens` dim header; the body markdown starts
//!   immediately under it.
//!
//! Thinking content stays in `source` and is persisted unchanged
//! so resume + future `/think toggle` can surface it.

use std::any::Any;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::HistoryCell;
use crate::tui::markdown_render::render_markdown_text_with_width;
use crate::tui::render::line_utils::line_to_static;
use crate::tui::turn_event::TurnEvent;

/// The expensive, width-sensitive part of an assistant reply. The renderer
/// receives one final draw at the same inner width used while streaming, so a
/// completed reply can reuse its last live layout instead of reparsing a long
/// markdown document on the input/event-loop path.
#[derive(Debug)]
struct MarkdownLayoutCache {
    revision: u64,
    width: usize,
    /// `source` is sometimes the reply body after a closed `<think>` block,
    /// not always `self.source`. A pointer + length distinguishes those views
    /// without retaining a second full copy merely for cache matching.
    source_ptr: usize,
    source_len: usize,
    lines: Vec<Line<'static>>,
}

/// Bounded live Markdown projection. It intentionally lags a burst by a
/// fraction of a second rather than ever falling back to raw Markdown, while
/// keeping per-frame work bounded for long answers.
#[derive(Debug)]
struct LiveMarkdownLayoutCache {
    revision: u64,
    width: usize,
    refreshed_at: Instant,
    lines: Vec<Line<'static>>,
}

#[derive(Debug, Clone)]
pub(crate) struct AssistantCell {
    /// Raw markdown. Growing during stream via `push_delta`.
    source: String,
    /// `true` while tokens can still arrive. Flipped to false by
    /// `finalize()`.
    live: bool,
    ts: Option<String>,
    /// Monotonic source revision. Cache matching by revision avoids an
    /// O(reply-size) string comparison on every redraw of a long reply.
    render_revision: u64,
    /// One exact source+width layout is enough for the active reply. Sharing
    /// it across view snapshots is safe because rendered lines are immutable;
    /// a source mutation invalidates it before the next draw.
    render_cache: Arc<Mutex<Option<MarkdownLayoutCache>>>,
    /// Latest bounded rich projection while a reply is streaming. Unlike the
    /// final layout cache it is deliberately retained across token deltas for
    /// a short refresh interval, preventing an O(n²) reparse without showing
    /// users raw Markdown between formatted frames.
    live_render_cache: Arc<Mutex<Option<LiveMarkdownLayoutCache>>>,
}

impl AssistantCell {
    pub fn new_streaming() -> Self {
        Self {
            source: String::new(),
            live: true,
            ts: None,
            render_revision: 0,
            render_cache: Arc::new(Mutex::new(None)),
            live_render_cache: Arc::new(Mutex::new(None)),
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
            render_revision: 0,
            render_cache: Arc::new(Mutex::new(None)),
            live_render_cache: Arc::new(Mutex::new(None)),
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
                render_revision: 0,
                render_cache: Arc::new(Mutex::new(None)),
                live_render_cache: Arc::new(Mutex::new(None)),
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
        self.render_revision = self.render_revision.wrapping_add(1);
        *astra_core::sync_poison::recover_mutex_lock(&self.render_cache) = None;
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
    /// 1. Matched `<think>…</think>` or `<thinking>…</thinking>`: inner is
    ///    between the tags. Leading tags only; a mid-body tag is just a
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
    fn split_think(&self) -> (Option<&str>, bool, &str) {
        let source = self.source.as_str();
        let trimmed = source.trim_start();
        if trimmed.is_empty() {
            return (None, false, source);
        }
        const THINK_TAGS: [(&str, &str); 2] =
            [("<think>", "</think>"), ("<thinking>", "</thinking>")];

        // Case 1 / 3: an explicit thinking envelope at the very start.
        if let Some((open_tag, close_tag, after_open)) = THINK_TAGS
            .iter()
            .find_map(|&(open, close)| trimmed.strip_prefix(open).map(|after| (open, close, after)))
        {
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
        if let Some(close_tag) = THINK_TAGS
            .iter()
            .map(|(_, close)| *close)
            .find(|close| source.trim_end().ends_with(close))
        {
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

    fn layout_matches(
        cache: &MarkdownLayoutCache,
        source: &str,
        width: usize,
        revision: u64,
    ) -> bool {
        cache.width == width
            && cache.revision == revision
            && cache.source_ptr == source.as_ptr() as usize
            && cache.source_len == source.len()
    }

    /// Prepare the full semantic Markdown layout exactly once. This is called
    /// by the background scrollback worker for completed replies; live display
    /// deliberately uses a bounded plain-text tail instead.
    fn ensure_markdown_layout(&self, source: &str, width: usize) {
        let mut cache = astra_core::sync_poison::recover_mutex_lock(&self.render_cache);
        if cache
            .as_ref()
            .is_some_and(|cache| Self::layout_matches(cache, source, width, self.render_revision))
        {
            return;
        }

        let lines = render_markdown_text_with_width(source, Some(width))
            .lines
            .iter()
            .map(line_to_static)
            .collect::<Vec<_>>();
        *cache = Some(MarkdownLayoutCache {
            revision: self.render_revision,
            width,
            source_ptr: source.as_ptr() as usize,
            source_len: source.len(),
            lines,
        });
    }

    fn markdown_layout_window(
        &self,
        source: &str,
        width: usize,
        start: usize,
        maximum: usize,
    ) -> (Vec<Line<'static>>, usize) {
        self.ensure_markdown_layout(source, width);
        let cache = astra_core::sync_poison::recover_mutex_lock(&self.render_cache);
        let cache = cache
            .as_ref()
            .expect("markdown layout is installed before it is read");
        debug_assert!(Self::layout_matches(
            cache,
            source,
            width,
            self.render_revision
        ));
        let total = cache.lines.len();
        let start = start.min(total);
        let end = start.saturating_add(maximum).min(total);
        (cache.lines[start..end].to_vec(), total)
    }

    fn markdown_layout(&self, source: &str, width: usize) -> Vec<Line<'static>> {
        self.markdown_layout_window(source, width, 0, usize::MAX).0
    }

    fn scrollback_body_lines(
        &self,
        source: &str,
        width: u16,
        start: usize,
        maximum: usize,
    ) -> (Vec<Line<'static>>, usize) {
        if source.is_empty() || maximum == 0 {
            return (Vec::new(), 0);
        }
        let inner_width = (width as usize).saturating_sub(2).max(20);
        let (rendered, total) = self.markdown_layout_window(source, inner_width, start, maximum);
        let theme = crate::tui::theme::current();
        let gutter_style = Style::default().fg(theme.gutter_frozen);
        let lines = rendered
            .into_iter()
            .map(|line| {
                let mut spans = Vec::with_capacity(line.spans.len() + 1);
                spans.push(Span::styled("█ ", gutter_style));
                spans.extend(line.spans);
                Line::from(spans)
            })
            .collect();
        (lines, total)
    }

    fn scrollback_body_source(&self) -> &str {
        let (think_inner, think_closed, body) = self.split_think();
        if think_inner.is_some() && think_closed {
            body
        } else {
            self.source.as_str()
        }
    }

    /// Whether final scrollback can read its full Markdown layout without work
    /// on the caller's thread.
    pub(crate) fn has_scrollback_layout(&self, width: u16) -> bool {
        let source = self.scrollback_body_source();
        if source.is_empty() {
            return true;
        }
        let inner_width = (width as usize).saturating_sub(2).max(20);
        astra_core::sync_poison::recover_mutex_lock(&self.render_cache)
            .as_ref()
            .is_some_and(|cache| {
                Self::layout_matches(cache, source, inner_width, self.render_revision)
            })
    }

    /// Perform the expensive final Markdown layout. `TerminalGuard` invokes
    /// this from `spawn_blocking` after the cell has finalized, never from the
    /// keyboard/render event loop.
    pub(crate) fn prepare_scrollback_layout(&self, width: u16) {
        let source = self.scrollback_body_source();
        if source.is_empty() {
            return;
        }
        let inner_width = (width as usize).saturating_sub(2).max(20);
        self.ensure_markdown_layout(source, inner_width);
    }

    /// Cheap live projection for the finite terminal viewport. Rendering a
    /// growing Markdown document from byte zero on every token made long
    /// replies O(n²) on the UI thread. While a reply is mutable we instead
    /// show its newest visible text window; the canonical final Markdown is
    /// prepared asynchronously after `finalize()`.
    pub(crate) fn live_viewport_lines(&self, width: u16, maximum: usize) -> Vec<Line<'static>> {
        debug_assert!(self.live, "only mutable replies use the live projection");
        if maximum == 0 {
            return Vec::new();
        }

        let (think_inner, think_closed, body) = self.split_think();
        if let Some(think) = think_inner
            && !think_closed
        {
            return render_live_thinking(think, width)
                .into_iter()
                .rev()
                .take(maximum)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
        }

        let mut out = if let Some(think) = think_inner {
            thought_header_lines(
                think.trim().lines().count().max(1),
                approx_tokens(think.chars().count() as u64),
            )
        } else {
            Vec::new()
        };
        let body_budget = maximum.saturating_sub(out.len()).max(1);
        let source = if think_inner.is_some() && think_closed {
            body
        } else {
            self.source.as_str()
        };
        out.extend(self.live_markdown_tail_lines(source, width as usize, body_budget));
        if out.len() > maximum {
            out.drain(..out.len() - maximum);
        }
        out
    }

    /// Render a bounded rich Markdown suffix for a mutable reply. The cache
    /// refreshes at most once per short interval during token bursts, so the
    /// live view never exposes raw Markdown and never reparses an unbounded
    /// growing document on every frame.
    fn live_markdown_tail_lines(
        &self,
        source: &str,
        width: usize,
        maximum: usize,
    ) -> Vec<Line<'static>> {
        const MAX_LIVE_SOURCE_BYTES: usize = 16 * 1024;
        const LIVE_MARKDOWN_REFRESH: std::time::Duration = std::time::Duration::from_millis(80);

        if source.is_empty() || maximum == 0 {
            return Vec::new();
        }
        let (tail, truncated) = utf8_tail_window(source, MAX_LIVE_SOURCE_BYTES);
        // A tail can begin in the middle of a line. Discard that fragment so
        // partial Markdown syntax at the boundary cannot corrupt the visible
        // projection; the leading ellipsis remains the honest continuity cue.
        let markdown_tail = if truncated {
            tail.split_once('\n').map(|(_, rest)| rest).unwrap_or(tail)
        } else {
            tail
        };
        let width = width.max(20);
        let now = Instant::now();
        let mut cache = astra_core::sync_poison::recover_mutex_lock(&self.live_render_cache);
        let lines = match cache.as_ref() {
            Some(cache) if cache.width == width && cache.revision == self.render_revision => {
                cache.lines.clone()
            }
            Some(cache)
                if cache.width == width
                    && now.duration_since(cache.refreshed_at) < LIVE_MARKDOWN_REFRESH =>
            {
                // A formatted frame that is a few tokens behind is preferable
                // to a raw Markdown frame or an input-loop reparse storm.
                cache.lines.clone()
            }
            _ => {
                let lines = render_markdown_text_with_width(markdown_tail, Some(width))
                    .lines
                    .iter()
                    .map(line_to_static)
                    .collect::<Vec<_>>();
                *cache = Some(LiveMarkdownLayoutCache {
                    revision: self.render_revision,
                    width,
                    refreshed_at: now,
                    lines: lines.clone(),
                });
                lines
            }
        };
        drop(cache);

        let visible_start = lines.len().saturating_sub(maximum);
        let mut out = lines.into_iter().skip(visible_start).collect::<Vec<_>>();
        if out.is_empty() {
            out.push(Line::default());
        }
        if truncated && let Some(first) = out.first_mut() {
            first.spans.insert(
                0,
                Span::styled("… ", Style::default().fg(crate::tui::theme::current().dim)),
            );
        }
        out
    }

    /// Materialize only a bounded portion of a final reply for native
    /// scrollback. Long final messages are inserted over several frames so
    /// the event loop can continue accepting composer input between chunks.
    pub(crate) fn scrollback_lines_chunk(
        &self,
        width: u16,
        start: usize,
        maximum: usize,
    ) -> (Vec<Line<'static>>, usize, bool) {
        debug_assert!(!self.live, "live assistant cells stay in the viewport");
        if maximum == 0 {
            return (Vec::new(), start, false);
        }

        let (think_inner, think_closed, body) = self.split_think();
        let headers = match (think_inner, think_closed) {
            (Some(think), true) => thought_header_lines(
                think.trim().lines().count().max(1),
                approx_tokens(think.chars().count() as u64),
            ),
            _ => Vec::new(),
        };
        let body_source = if think_inner.is_some() && think_closed {
            body
        } else {
            self.source.as_str()
        };

        let mut lines = headers
            .iter()
            .skip(start)
            .take(maximum)
            .cloned()
            .collect::<Vec<_>>();
        let body_start = start.saturating_sub(headers.len());
        let remaining = maximum.saturating_sub(lines.len());
        let (body_lines, body_total) =
            self.scrollback_body_lines(body_source, width, body_start, remaining);
        lines.extend(body_lines);
        let next = start.saturating_add(lines.len());
        let total = headers.len().saturating_add(body_total);
        (lines, next, next >= total)
    }

    fn render_body_with_gutter(&self, source: &str, width: u16, live: bool) -> Vec<Line<'static>> {
        if source.trim().is_empty() {
            return Vec::new();
        }
        // Settled cells prepend `█ ` (2 cols) to every line, so wrap text
        // at `width - 2` to avoid terminal hard-wrap overflow. Live cells
        // already receive `width - 2` from `active_viewport` (the
        // `LiveFramedCell` wrapper paints its own gutter column). This means
        // the final scrollback render uses the same markdown width as the
        // last active draw and can reuse the cached layout.
        let prepend_gutter = !live;
        let inner_w = if prepend_gutter {
            (width as usize).saturating_sub(2).max(20)
        } else {
            (width as usize).max(20)
        };
        let rendered = self.markdown_layout(source, inner_w);

        if rendered.is_empty() {
            return Vec::new();
        }

        let theme = crate::tui::theme::current();
        let gutter_style = Style::default().fg(theme.gutter_frozen);
        rendered
            .into_iter()
            .map(|line| {
                let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 1);
                if prepend_gutter {
                    spans.push(Span::styled("█ ", gutter_style));
                }
                spans.extend(line.spans.iter().cloned());
                Line::from(spans)
            })
            .collect()
    }
}

impl HistoryCell for AssistantCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let (think_inner, think_closed, body) = self.split_think();

        // Branch 1: no <think> block. Original render path.
        let Some(think) = think_inner else {
            return self.render_body_with_gutter(&self.source, width, self.live);
        };

        // Branch 2: <think> still open (streaming). Show a dim
        // thinking header + the partial think content so the user
        // sees progress instead of a frozen screen. No body yet.
        if !think_closed {
            return render_live_thinking(think, width);
        }

        // Branch 3: <think> closed. Collapse to a one-line
        // `Thought · N lines · N tokens` header, then render the body.
        let think_lines = think.trim().lines().count().max(1);
        let think_tokens = approx_tokens(think.chars().count() as u64);
        let mut out = thought_header_lines(think_lines, think_tokens);
        out.extend(self.render_body_with_gutter(body, width, self.live));
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

/// Collapsed one-line header shown in place of a `<think>` block
/// once the closing tag has arrived.
///
/// Example: `• Thought · 12 lines · 50 tokens`
fn thought_header_lines(line_count: usize, token_count: u64) -> Vec<Line<'static>> {
    let theme = crate::tui::theme::current();
    let dim = Style::default().fg(theme.dim);
    let stat = dim; // stat labels in dim — secondary but readable
    let label = if line_count == 1 {
        "1 line".to_string()
    } else {
        format!("{line_count} lines")
    };
    let tok_label = if token_count == 1 {
        "1 token".to_string()
    } else {
        format!("{token_count} tokens")
    };
    vec![Line::from(vec![
        Span::styled("• ", dim),
        thought_gradient("Thought", theme),
        Span::styled(" · ", stat),
        Span::styled(label, stat),
        Span::styled(" · ", stat),
        Span::styled(tok_label, stat),
    ])]
}

/// Build the compact Thought heading from the semantic accent.
///
/// `theme.fg` is deliberately `Reset` for terminal compatibility, so blending
/// an accent toward it produced an implementation-dependent gray title on
/// several terminals. Thinking is secondary information, but it still needs
/// a reliable visual anchor next to its dim statistics.
pub(super) fn thought_gradient(word: &str, theme: &crate::tui::theme::Theme) -> Span<'static> {
    Span::styled(
        word.to_string(),
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )
}

/// Max body rows shown beneath the `Thinking` header while the
/// `<think>` block is open. Mirrors `ReasoningCell::LIVE_PREVIEW_MAX_ROWS`:
/// once the window fills, the oldest row scrolls off and a
/// `⋯ +N more` counter takes the top slot, so the preview stays
/// fixed-height instead of pushing the composer off-screen on a
/// long internal monologue.
const LIVE_THINK_PREVIEW_MAX_ROWS: usize = 4;

/// While `<think>` is open and the model is still streaming, show a
/// calm `Thought · N lines · N tokens` header plus a fixed-height scrolling
/// preview of the latest thinking content. Matches `ReasoningCell`'s
/// live-preview grammar so the two paths (inline `<think>` vs.
/// provider reasoning channel) feel consistent to the user. Full
/// thinking body stays hidden — once `</think>` arrives this whole
/// block collapses to a one-line `Thought · N lines · N tokens` header.
fn render_live_thinking(think_partial: &str, width: u16) -> Vec<Line<'static>> {
    let theme = crate::tui::theme::current();
    let dim = Style::default().fg(theme.dim);
    let stat = dim; // stat labels in dim but without DIM modifier
    // Body preview text: fg dim only — readable but visually subordinate.
    let preview_text = Style::default().fg(theme.dim);

    let line_count = think_partial.trim().lines().count().max(1);
    let token_count = approx_tokens(think_partial.chars().count() as u64);
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

    // Header line: bold Thought with accent blend, then dim stats.
    let mut out = vec![Line::from(vec![
        Span::styled("• ", dim),
        thought_gradient("Thought", theme),
        Span::styled(" · ", stat),
        Span::styled(line_label, stat),
        Span::styled(" · ", stat),
        Span::styled(tok_label, stat),
    ])];

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
            Span::styled(format!("⋯ +{hidden} more"), stat),
        ]));
        total - tail
    } else {
        0
    };
    for row in body_rows.into_iter().skip(visible_start) {
        out.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(row, preview_text),
        ]));
    }
    out
}

fn utf8_tail_window(source: &str, maximum_bytes: usize) -> (&str, bool) {
    if source.len() <= maximum_bytes {
        return (source, false);
    }
    let mut start = source.len().saturating_sub(maximum_bytes);
    while !source.is_char_boundary(start) {
        start += 1;
    }
    (&source[start..], true)
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

/// Approximate token count from characters: chars / 4, ceiling.
/// Mirrors [`crate::tui::status_indicator::approx_tokens`].
fn approx_tokens(chars: u64) -> u64 {
    chars.div_ceil(4)
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
    fn live_viewport_keeps_only_the_bounded_tail_of_a_long_reply() {
        let mut c = AssistantCell::new_streaming();
        c.push_delta(
            &(0..2_000)
                .map(|index| format!("line-{index}\n"))
                .collect::<String>(),
        );

        let lines = c.live_viewport_lines(80, 3);
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(lines.len() <= 3, "live frame must stay bounded");
        assert!(
            text.contains("line-1999"),
            "latest text stays visible: {text}"
        );
        assert!(
            !text.contains("line-0\n"),
            "old text stays out of the live frame"
        );
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
    fn final_scrollback_reuses_the_last_live_markdown_layout() {
        let mut c = AssistantCell::new_streaming();
        c.push_delta("# Result\n\n- one\n- two\n\n`cargo test`");

        // `active_viewport` gives a live assistant the terminal width minus
        // its frame gutter. The committed cell adds that gutter itself, so
        // its scrollback width resolves to the same cached markdown layout.
        let live = c.display_lines(78);
        assert!(!live.is_empty());
        {
            let cache = astra_core::sync_poison::recover_mutex_lock(&c.render_cache);
            assert!(matches!(
                cache.as_ref(),
                Some(MarkdownLayoutCache { width: 78, .. })
            ));
        }

        c.finalize();
        let settled = c.display_lines(80);
        assert!(settled.iter().all(|line| !line.spans.is_empty()));
        let cache = astra_core::sync_poison::recover_mutex_lock(&c.render_cache);
        assert!(matches!(
            cache.as_ref(),
            Some(MarkdownLayoutCache { width: 78, .. })
        ));
    }

    #[test]
    fn final_scrollback_materializes_only_the_requested_line_window() {
        let c = AssistantCell::from_markdown(
            (0..32)
                .map(|index| format!("paragraph {index}"))
                .collect::<Vec<_>>()
                .join("\n\n"),
        );

        let (first, next, complete) = c.scrollback_lines_chunk(80, 0, 3);
        assert_eq!(first.len(), 3);
        assert_eq!(next, 3);
        assert!(!complete);

        let (second, next, complete) = c.scrollback_lines_chunk(80, next, 3);
        assert_eq!(second.len(), 3);
        assert_eq!(next, 6);
        assert!(!complete);
        assert!(first[0].spans[0].content.starts_with("█ "));
        assert!(second[0].spans[0].content.starts_with("█ "));
    }

    #[test]
    fn prepared_final_layout_is_reused_by_scrollback_chunks() {
        let c = AssistantCell::from_markdown("# Result\n\n- first\n- second");
        assert!(!c.has_scrollback_layout(80));

        c.prepare_scrollback_layout(80);

        assert!(c.has_scrollback_layout(80));
        let (lines, _, complete) = c.scrollback_lines_chunk(80, 0, 8);
        assert!(complete);
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("Result"))
        }));
    }

    #[test]
    fn from_markdown_is_not_live() {
        let c = AssistantCell::from_markdown("# Complete");
        assert!(!c.is_live());
    }

    // ── Render ───────────────────────────────────────────────────

    #[test]
    fn settled_replies_render_as_plain_body_text() {
        let c = AssistantCell::from_markdown("first\n\nsecond");
        let out = render(&c, 60, 4);
        for row in out.lines().filter(|l| !l.is_empty()) {
            assert!(
                !row.starts_with('│'),
                "settled reply rows should read as plain body text: {row:?}"
            );
        }
    }

    #[test]
    fn live_reply_content_has_no_transport_progress_suffix() {
        let mut c = AssistantCell::new_streaming();
        c.push_delta("answer");
        let lines = c.display_lines(60);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].content, "answer");
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
            out.contains("Thought") && out.contains("2 lines") && out.contains("token"),
            "collapsed header missing: {out}"
        );
        assert!(out.contains("I am Astra"), "body missing: {out}");
    }

    #[test]
    fn closed_think_header_pluralisation_handles_single_line() {
        let c = AssistantCell::from_markdown("<think>just one</think>\n\nbody");
        let out = render(&c, 60, 3);
        assert!(
            out.contains("Thought") && out.contains("1 line") && out.contains("token"),
            "singular form for one-line think: {out}"
        );
    }

    #[test]
    fn thinking_envelope_never_leaks_as_assistant_markdown() {
        let c = AssistantCell::from_markdown(
            "<thinking>inspect the runtime state first</thinking>\n\n# Result\n\n- Ready",
        );
        let out = render(&c, 80, 5);
        assert!(
            !out.contains("<thinking>") && !out.contains("</thinking>"),
            "thinking protocol tags must not reach the transcript: {out}"
        );
        assert!(
            !out.contains("inspect the runtime state first"),
            "closed thinking content must collapse: {out}"
        );
        assert!(out.contains("Thought") && out.contains("Result") && out.contains("Ready"));
    }

    #[test]
    fn live_projection_uses_markdown_before_finalization() {
        let mut c = AssistantCell::new_streaming();
        c.push_delta("# Result\n\n- ship the patch\n- run the tests");
        let lines = c.live_viewport_lines(80, 8);
        let rendered = buffer_to_string(&draw_widget(
            ratatui::widgets::Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
            80,
            8,
        ));
        assert!(rendered.contains("Result"), "{rendered}");
        assert!(
            !rendered.contains("# Result"),
            "live text must not briefly fall back to raw Markdown: {rendered}"
        );
        assert!(rendered.contains("ship the patch") && rendered.contains("run the tests"));
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
        // `Thought · N lines · N tokens` plus a scrolling preview of the latest
        // rows of internal monologue (see `LIVE_THINK_PREVIEW_MAX_ROWS`).
        // With only two lines of content, the whole body fits in
        // the preview window — no overflow counter, both rows
        // visible.
        let mut c = AssistantCell::new_streaming();
        c.push_delta("<think>\nstep one\nstep two in progres");
        let out = render(&c, 80, 5);
        assert!(out.contains("Thought"), "live indicator missing: {out}");
        assert!(out.contains("2 lines"), "line count missing: {out}");
        assert!(out.contains("token"), "token count missing: {out}");
        assert!(
            out.contains("step two in progres"),
            "latest thinking line missing: {out}"
        );
        assert!(
            out.contains("step one"),
            "earlier line must stay visible under the window cap: {out}"
        );
        // Body hasn't arrived yet — no settled-body guide gutter.
        assert!(
            !out.contains('│'),
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
            out.contains("Thought") && out.contains("3 lines") && out.contains("token"),
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
