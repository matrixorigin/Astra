//! One-line live indicator rendered above the composer while a
//! turn is running. Replaces the previous `orbiter_line` + framed
//! thinking-window combo with a single Codex-style widget that
//! lives **outside** scrollback.
//!
//! Shape:
//!
//! ```text
//! ✶ Thinking (16s · ↓ 5.1k tokens · esc to interrupt)
//! ✶ Running bash (2s)
//! ```
//!
//! Design rules — drawn from Codex's `StatusIndicatorWidget`:
//!
//! - Never part of the scrollback / `HistoryCell` chain. It's
//!   ephemeral — if a user scrolls back they see the result
//!   (tool cell, assistant cell, turn summary), not a stale
//!   status header.
//! - Label is static bold-accent. A previous iteration ran the
//!   shimmer per-char and it looked like the word was melting at
//!   12 fps; Codex doesn't shimmer either.
//! - Star rotates on a 500 ms wall-clock tick. Slow heartbeat
//!   beats strobing.
//! - Elapsed time rounds to whole seconds so the counter ticks
//!   once per second instead of jittering sub-second.

#![allow(dead_code)] // wired into BottomPane in phase 3d.

use std::time::{Duration, Instant};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// What the model / tool layer is currently doing.
#[derive(Debug, Clone)]
pub(crate) enum IndicatorState {
    /// No turn active — render nothing.
    Idle,
    /// Turn is running, model is thinking / producing tokens.
    Thinking { started_at: Instant },
    /// Tool is executing mid-turn.
    Tool { name: String, started_at: Instant },
    /// Model is being called but hasn't responded yet (pre-SSE).
    WaitingModel { started_at: Instant },
    /// Queued on an approval prompt.
    AwaitingApproval { started_at: Instant },
}

impl IndicatorState {
    fn started_at(&self) -> Option<Instant> {
        match self {
            IndicatorState::Idle => None,
            IndicatorState::Thinking { started_at }
            | IndicatorState::Tool { started_at, .. }
            | IndicatorState::WaitingModel { started_at }
            | IndicatorState::AwaitingApproval { started_at } => Some(*started_at),
        }
    }

    fn is_active(&self) -> bool {
        !matches!(self, IndicatorState::Idle)
    }
}

impl Default for IndicatorState {
    fn default() -> Self {
        Self::Idle
    }
}

/// The rendered line. Owns its state so the bottom-pane widget
/// doesn't have to plumb context through on every frame.
#[derive(Debug, Default)]
pub(crate) struct StatusIndicator {
    state: IndicatorState,
    /// Per-turn streamed-char count (for the `↓ 5.1k tokens`
    /// counter). Reset on each new turn.
    stream_chars: u64,
}

impl StatusIndicator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> &IndicatorState {
        &self.state
    }

    pub fn set_state(&mut self, state: IndicatorState) {
        // Any transition into a new active state resets the
        // stream counter so the `↓` number reflects THIS turn,
        // not carryover. Transitioning to `Idle` also zeroes it
        // so a subsequent show-line (in an edge case where state
        // flips back) doesn't ghost stale tokens.
        self.stream_chars = 0;
        self.state = state;
    }

    pub fn bump_stream_chars(&mut self, n: usize) {
        self.stream_chars = self.stream_chars.saturating_add(n as u64);
    }

    /// Produce the single line to render. `None` means "don't
    /// draw anything" — the caller should reserve zero rows.
    pub fn render(&self) -> Option<Line<'static>> {
        render_for(&self.state, self.stream_chars, Instant::now())
    }
}

/// Rendering extracted to a free fn with explicit `now` so tests
/// can pin the clock without mocking `Instant`.
fn render_for(
    state: &IndicatorState,
    stream_chars: u64,
    now: Instant,
) -> Option<Line<'static>> {
    if !state.is_active() {
        return None;
    }

    let elapsed = state
        .started_at()
        .and_then(|t| now.checked_duration_since(t));

    let theme = crate::tui::theme::current();
    let star_style = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);

    let label: String = match state {
        IndicatorState::Thinking { .. } => "Thinking".into(),
        IndicatorState::Tool { name, .. } => format!("Running {name}"),
        IndicatorState::WaitingModel { .. } => "Waiting for model".into(),
        IndicatorState::AwaitingApproval { .. } => "Awaiting approval".into(),
        IndicatorState::Idle => return None,
    };

    Some(Line::from(vec![
        Span::styled(format!("{} ", star_frame(now)), star_style),
        Span::styled(label, label_style),
        Span::styled(suffix(elapsed, stream_chars), dim),
    ]))
}

/// Parenthesised suffix: `(elapsed · ↓ N tokens · esc to interrupt)`.
/// Sections elide when they'd be meaningless (token counter
/// before first delta, no-yet elapsed at state flip).
fn suffix(elapsed: Option<Duration>, stream_chars: u64) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(d) = elapsed {
        parts.push(fmt_duration_coarse(d));
    }
    if stream_chars > 0 {
        parts.push(format!("↓ {} tokens", fmt_tokens(approx_tokens(stream_chars))));
    }
    parts.push("esc to interrupt".into());
    format!(" ({})", parts.join(" · "))
}

fn fmt_duration_coarse(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
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

fn approx_tokens(chars: u64) -> u64 {
    chars.div_ceil(4)
}

/// Rotating star glyph. Time-keyed to a 500 ms window — slow
/// heartbeat, fast enough to reassure the terminal isn't frozen.
fn star_frame(now: Instant) -> &'static str {
    const FRAMES: [&str; 4] = ["✶", "✷", "✹", "✺"];
    // Instant has no fixed epoch, so key off a process-monotonic
    // bucket count. Wraps at u64::MAX ≈ never.
    let bucket = now
        .elapsed()
        .saturating_add(Duration::from_nanos(1))
        .as_millis() as u64
        / 500;
    FRAMES[(bucket as usize) % FRAMES.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    fn any_star(s: &str) -> bool {
        ["✶", "✷", "✹", "✺"].iter().any(|g| s.contains(g))
    }

    // ── render rules ─────────────────────────────────────────────

    #[test]
    fn idle_renders_none() {
        let s = StatusIndicator::new();
        assert!(s.render().is_none());
    }

    #[test]
    fn thinking_contains_star_label_and_elapsed() {
        let mut s = StatusIndicator::new();
        let t0 = Instant::now();
        s.set_state(IndicatorState::Thinking { started_at: t0 });
        let line = render_for(
            &s.state,
            s.stream_chars,
            t0 + Duration::from_secs(3),
        )
        .unwrap();
        let text = text_of(&line);
        assert!(any_star(&text), "star missing: {text}");
        assert!(text.contains("Thinking"));
        assert!(text.contains("3s"));
        assert!(text.contains("esc to interrupt"));
    }

    #[test]
    fn tool_includes_tool_name() {
        let t0 = Instant::now();
        let state = IndicatorState::Tool {
            name: "bash".into(),
            started_at: t0,
        };
        let line = render_for(&state, 0, t0 + Duration::from_secs(1)).unwrap();
        assert!(text_of(&line).contains("Running bash"));
    }

    #[test]
    fn waiting_and_awaiting_render_distinct_labels() {
        let t0 = Instant::now();
        let w = render_for(
            &IndicatorState::WaitingModel { started_at: t0 },
            0,
            t0 + Duration::from_secs(0),
        )
        .unwrap();
        assert!(text_of(&w).contains("Waiting for model"));

        let a = render_for(
            &IndicatorState::AwaitingApproval { started_at: t0 },
            0,
            t0 + Duration::from_secs(0),
        )
        .unwrap();
        assert!(text_of(&a).contains("Awaiting approval"));
    }

    // ── token counter ────────────────────────────────────────────

    #[test]
    fn token_counter_hides_when_zero() {
        let t0 = Instant::now();
        let line = render_for(
            &IndicatorState::Thinking { started_at: t0 },
            0,
            t0 + Duration::from_secs(1),
        )
        .unwrap();
        let text = text_of(&line);
        assert!(
            !text.contains("tokens"),
            "empty token count should not render: {text}"
        );
    }

    #[test]
    fn token_counter_shows_once_stream_starts() {
        let t0 = Instant::now();
        let line = render_for(
            &IndicatorState::Thinking { started_at: t0 },
            20_000, // ~5k tokens at 4 chars/token
            t0 + Duration::from_secs(5),
        )
        .unwrap();
        let text = text_of(&line);
        assert!(text.contains("↓"), "down arrow missing: {text}");
        // Rounding tolerance: 5.0k or 5.1k both acceptable.
        assert!(
            text.contains("5.0k") || text.contains("5.1k"),
            "unexpected token count: {text}"
        );
    }

    #[test]
    fn bump_stream_chars_increments_cumulative() {
        let mut s = StatusIndicator::new();
        s.bump_stream_chars(10);
        s.bump_stream_chars(20);
        assert_eq!(s.stream_chars, 30);
    }

    #[test]
    fn set_state_resets_stream_chars() {
        // Invariant: each turn starts at `↓ 0 tokens`. Without a
        // reset the counter would carry the previous turn's tail.
        let mut s = StatusIndicator::new();
        s.set_state(IndicatorState::Thinking {
            started_at: Instant::now(),
        });
        s.bump_stream_chars(5_000);
        assert_eq!(s.stream_chars, 5_000);
        s.set_state(IndicatorState::Thinking {
            started_at: Instant::now(),
        });
        assert_eq!(s.stream_chars, 0, "new turn must zero the counter");
    }

    // ── formatting ───────────────────────────────────────────────

    #[test]
    fn fmt_duration_rounds_to_whole_seconds_under_a_minute() {
        assert_eq!(fmt_duration_coarse(Duration::from_millis(400)), "0s");
        assert_eq!(fmt_duration_coarse(Duration::from_millis(1_999)), "1s");
        assert_eq!(fmt_duration_coarse(Duration::from_secs(59)), "59s");
        assert_eq!(fmt_duration_coarse(Duration::from_secs(60)), "1m 0s");
        assert_eq!(fmt_duration_coarse(Duration::from_secs(125)), "2m 5s");
    }

    #[test]
    fn fmt_tokens_scales_match_turn_summary() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_234), "1.2k");
        assert_eq!(fmt_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn approx_tokens_is_chars_div_4_ceil() {
        assert_eq!(approx_tokens(0), 0);
        assert_eq!(approx_tokens(4), 1);
        assert_eq!(approx_tokens(5), 2);
        assert_eq!(approx_tokens(20_001), 5_001);
    }
}
