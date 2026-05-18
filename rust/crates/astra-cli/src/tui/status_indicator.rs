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
//! - The primary label is turn-stable. Tool activity can appear as
//!   secondary context, but the main verb must not flip between
//!   Thinking / Writing / Reading mid-turn.
//! - Truecolor terminals get a slow shimmer over the primary label;
//!   non-truecolor terminals keep a static bold-accent label.
//! - Star rotates on a 500 ms wall-clock tick. Slow heartbeat
//!   beats strobing.
//! - Elapsed time rounds to whole seconds so the counter ticks
//!   once per second instead of jittering sub-second.

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
    /// When the *turn* started — not the current state. Survives
    /// Thinking ↔ Tool ↔ Thinking transitions so the `(Ns)` elapsed
    /// counter grows monotonically across the whole turn instead of
    /// flashing back to 0 every time a tool fires. Set by
    /// [`Self::begin_turn`] at turn start; cleared by transitioning
    /// to `Idle` (or implicitly by the next `begin_turn`).
    turn_started_at: Option<Instant>,
    turn_label: Option<&'static str>,
}

/// How long the model can be silent in `Thinking` state before the
/// spinner shows the `· thought for Ns` chip. Tuned to match the
/// "this is taking longer than usual" expectation: short enough to
/// reassure the user during Bedrock extended-thinking pauses,
/// long enough to avoid flickering in/out for a normal prompt.
const SILENT_WINDOW_BEFORE_THOUGHT_CHIP: Duration = Duration::from_secs(5);
const STALL_WARN_AFTER: Duration = Duration::from_secs(5);
const STALL_ERROR_AFTER: Duration = Duration::from_secs(10);
const DEFAULT_TURN_LABEL: &str = "Thinking";

impl StatusIndicator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> &IndicatorState {
        &self.state
    }

    pub fn set_state(&mut self, state: IndicatorState) {
        // The stream counter resets across state changes only when
        // entering a *brand new turn*. Within a turn, tool ↔ thinking
        // transitions preserve `stream_chars` so `↓ Nk tokens` keeps
        // climbing instead of flashing back to 0 each time the model
        // fires a tool. `begin_turn` is the explicit reset point.
        if matches!(state, IndicatorState::Idle) {
            self.stream_chars = 0;
            self.turn_started_at = None;
            self.turn_label = None;
        } else if self.turn_started_at.is_none() {
            // Auto-start a turn when transitioning out of Idle. Lets
            // existing callers that drive `set_state(Thinking{...})`
            // directly (and forget to invoke `begin_turn`) still get
            // the correct turn-stable elapsed counter. The state's
            // own `started_at` is the truthy turn-start signal — it
            // was set by the caller at `Instant::now()` for exactly
            // this transition.
            if let Some(t) = state.started_at() {
                self.turn_started_at = Some(t);
                self.turn_label = Some(DEFAULT_TURN_LABEL);
                self.stream_chars = 0;
            }
        }
        self.state = state;
    }

    /// Mark the start of a new turn. Drives the elapsed counter that
    /// renders in the spinner suffix (`(7s · esc to interrupt)`).
    /// Called at the same moment the host transitions out of Idle —
    /// usually right alongside `set_state(Thinking { ... })` or
    /// `set_state(WaitingModel { ... })`.
    pub fn begin_turn(&mut self, at: Instant) {
        self.turn_started_at = Some(at);
        self.turn_label = Some(DEFAULT_TURN_LABEL);
        self.stream_chars = 0;
    }

    pub fn bump_stream_chars(&mut self, n: usize) {
        self.stream_chars = self.stream_chars.saturating_add(n as u64);
    }

    /// Produce the single line to render. `None` means "don't
    /// draw anything" — the caller should reserve zero rows.
    pub fn render(&self) -> Option<Line<'static>> {
        self.render_at(Instant::now())
    }

    /// Like `render` but with an explicit `now`. Lets tests pin the
    /// clock without mocking `Instant`. Production callers use
    /// `render()`.
    pub fn render_at(&self, now: Instant) -> Option<Line<'static>> {
        render_for(
            &self.state,
            self.stream_chars,
            self.turn_started_at,
            self.turn_label,
            now,
        )
    }
}

/// Rendering extracted to a free fn with explicit `now` so tests
/// can pin the clock without mocking `Instant`. `turn_started_at`
/// (when set) wins over the per-state `started_at` so a tool
/// dispatch mid-turn doesn't reset the visible elapsed counter.
fn render_for(
    state: &IndicatorState,
    stream_chars: u64,
    turn_started_at: Option<Instant>,
    turn_label: Option<&'static str>,
    now: Instant,
) -> Option<Line<'static>> {
    if !state.is_active() {
        return None;
    }

    let elapsed_origin = turn_started_at.or_else(|| state.started_at());
    let elapsed = elapsed_origin.and_then(|t| now.checked_duration_since(t));

    let theme = crate::tui::theme::current();
    let intensity_color = stall_intensity_color(elapsed, theme);
    let star_style = Style::default()
        .fg(intensity_color)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default()
        .fg(intensity_color)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);

    let state_label: String = match state {
        IndicatorState::Thinking { .. } => "Thinking".into(),
        IndicatorState::Tool { name, .. } => label_for_tool(name),
        IndicatorState::WaitingModel { .. } => "Waiting for model".into(),
        IndicatorState::AwaitingApproval { .. } => "Awaiting approval".into(),
        IndicatorState::Idle => return None,
    };
    let label = turn_label.unwrap_or(&state_label);
    let activity = (state_label != label).then_some(state_label.as_str());

    // Surface a `· thought for Ns` chip during long silent stretches
    // in Thinking state. Bedrock extended-thinking can churn 60-120s
    // without any SSE delta — this chip is the only signal the user
    // has that the model is alive on the other end.
    let thought_for = thought_for_duration(state, stream_chars, elapsed);

    let mut spans = vec![Span::styled(format!("{} ", star_frame(now)), star_style)];
    spans.extend(label_spans(label, label_style, now));
    spans.push(Span::styled(
        suffix(elapsed, stream_chars, thought_for, activity),
        dim,
    ));
    Some(Line::from(spans))
}

/// Map an in-progress tool name to the spinner verb the user sees.
/// Well-known tools get a single action word that describes what the
/// tool is actually doing right now — `Writing` is more informative
/// than `Running write_file`. `bash` is special: keeping the literal
/// name preserves the "what shell command" signal the user expects.
/// Unknown tools (plugins, MCP servers) fall back to `Running <name>`.
///
/// Pinned by `tool_name_maps_to_human_verb` so renaming a verb is a
/// deliberate one-line change with a corresponding test update,
/// not a drive-by rewrite.
fn label_for_tool(name: &str) -> String {
    match name {
        "bash" => "Running bash".into(),
        "write_file" => "Writing".into(),
        "read_file" => "Reading".into(),
        "str_replace" => "Editing".into(),
        "grep" | "glob" => "Searching".into(),
        "list_dir" => "Listing".into(),
        "task" => "Tracking".into(),
        "memory" => "Recalling".into(),
        "tool_search" => "Loading tool".into(),
        _ => format!("Running {name}"),
    }
}

/// Returns `Some(elapsed)` when the spinner should display the
/// `· thought for Ns` chip. Conditions:
/// 1. State is Thinking (chip is meaningless for tool execution etc.)
/// 2. No tokens have streamed yet (token counter takes over once any
///    have arrived — the counter itself proves the model is active)
/// 3. We've been silent past the [`SILENT_WINDOW_BEFORE_THOUGHT_CHIP`]
///    threshold
fn thought_for_duration(
    state: &IndicatorState,
    stream_chars: u64,
    elapsed: Option<Duration>,
) -> Option<Duration> {
    if !matches!(state, IndicatorState::Thinking { .. }) {
        return None;
    }
    if stream_chars > 0 {
        return None;
    }
    let d = elapsed?;
    if d >= SILENT_WINDOW_BEFORE_THOUGHT_CHIP {
        Some(d)
    } else {
        None
    }
}

fn stall_intensity_color(elapsed: Option<Duration>, theme: &crate::tui::theme::Theme) -> Color {
    match elapsed {
        Some(d) if d >= STALL_ERROR_AFTER => theme.error,
        Some(d) if d >= STALL_WARN_AFTER => theme.warn,
        _ => theme.accent,
    }
}

/// Parenthesised suffix: `(elapsed · thought for Ns · ↓ N tokens · esc to interrupt)`.
/// Sections elide when they'd be meaningless (token counter
/// before first delta, no-yet elapsed at state flip, thought-for
/// chip during normal streaming).
fn suffix(
    elapsed: Option<Duration>,
    stream_chars: u64,
    thought_for: Option<Duration>,
    activity: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(d) = elapsed {
        parts.push(fmt_duration_coarse(d));
    }
    if let Some(activity) = activity {
        parts.push(activity.to_string());
    }
    if let Some(d) = thought_for {
        parts.push(format!("thought for {}", fmt_duration_coarse(d)));
    }
    if stream_chars > 0 {
        parts.push(format!(
            "↓ {} tokens",
            fmt_tokens(approx_tokens(stream_chars))
        ));
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

fn label_spans(label: &str, style: Style, now: Instant) -> Vec<Span<'static>> {
    let truecolor = supports_color::on_cached(supports_color::Stream::Stdout)
        .map(|level| level.has_16m)
        .unwrap_or(false);
    label_spans_for_mode(label, style, now, truecolor)
}

fn label_spans_for_mode(
    label: &str,
    style: Style,
    now: Instant,
    truecolor: bool,
) -> Vec<Span<'static>> {
    if !truecolor {
        return vec![Span::styled(label.to_string(), style)];
    }

    let chars: Vec<char> = label.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }

    let base = style
        .fg
        .and_then(color_to_rgb_approx)
        .unwrap_or((150, 120, 255));
    let highlight = crate::tui::terminal_palette::default_fg().unwrap_or((255, 255, 255));
    let padding = 2usize;
    let period = chars.len() + padding * 2;
    let phase = ((crate::tui::shimmer::time_at(now).max(0.0) * 1000.0) as u64 / 200) as usize;
    let center = (phase % period) as isize - padding as isize;
    let band_half_width = 1.5_f32;

    chars
        .into_iter()
        .enumerate()
        .map(|(i, ch)| {
            let dist = (i as isize - center).unsigned_abs() as f32;
            let intensity = if dist <= band_half_width {
                let x = std::f32::consts::PI * (dist / band_half_width);
                0.5 * (1.0 + x.cos())
            } else {
                0.0
            };
            let (r, g, b) = crate::tui::color::blend(highlight, base, intensity * 0.85);
            Span::styled(
                ch.to_string(),
                style.fg(Color::Rgb(r, g, b)).add_modifier(Modifier::BOLD),
            )
        })
        .collect()
}

fn color_to_rgb_approx(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Black => Some((0, 0, 0)),
        Color::Red => Some((205, 49, 49)),
        Color::Green => Some((13, 188, 121)),
        Color::Yellow => Some((229, 229, 16)),
        Color::Blue => Some((36, 114, 200)),
        Color::Magenta => Some((188, 63, 188)),
        Color::Cyan => Some((17, 168, 205)),
        Color::Gray => Some((204, 204, 204)),
        Color::DarkGray => Some((118, 118, 118)),
        Color::LightRed => Some((241, 76, 76)),
        Color::LightGreen => Some((35, 209, 139)),
        Color::LightYellow => Some((245, 245, 67)),
        Color::LightBlue => Some((59, 142, 234)),
        Color::LightMagenta => Some((214, 112, 214)),
        Color::LightCyan => Some((41, 184, 219)),
        Color::White => Some((255, 255, 255)),
        Color::Indexed(_) | Color::Reset => None,
    }
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

    /// REGRESSION: a multi-tool turn used to look like the spinner
    /// "kept disappearing and reappearing" because every Thinking ↔
    /// Tool transition reset `started_at` and the elapsed counter
    /// flashed back to 0s. The cure: separate the per-state
    /// `started_at` from a turn-level start instant that survives
    /// state transitions. Test pins that elapsed grows monotonically
    /// across a full Thinking → Tool → Thinking cycle.
    #[test]
    fn elapsed_does_not_reset_on_tool_switch() {
        let mut s = StatusIndicator::new();
        let turn_start = Instant::now();
        s.begin_turn(turn_start);
        s.set_state(IndicatorState::Thinking {
            started_at: turn_start,
        });

        // 3s into the turn — model is thinking.
        let line = s.render_at(turn_start + Duration::from_secs(3)).unwrap();
        let t1 = text_of(&line);
        assert!(t1.contains("3s"), "expected 3s elapsed, got: {t1}");

        // Model fires a tool at t=4s. Tool runs for 2s.
        s.set_state(IndicatorState::Tool {
            name: "bash".into(),
            started_at: turn_start + Duration::from_secs(4),
        });
        let line = s.render_at(turn_start + Duration::from_secs(5)).unwrap();
        let t2 = text_of(&line);
        assert!(
            t2.contains("5s"),
            "tool-state elapsed must reflect TURN time, not state time; got: {t2}"
        );

        // Tool completes, back to thinking.
        s.set_state(IndicatorState::Thinking {
            started_at: turn_start + Duration::from_secs(6),
        });
        let line = s.render_at(turn_start + Duration::from_secs(7)).unwrap();
        let t3 = text_of(&line);
        assert!(
            t3.contains("7s"),
            "post-tool Thinking must keep growing the turn timer; got: {t3}"
        );
    }

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
            None,
            None,
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
    fn thinking_stall_intensity_warns_at_5s_and_errors_at_10s() {
        let theme = crate::tui::theme::Theme::dark();
        assert_eq!(
            stall_intensity_color(Some(Duration::from_secs(4)), &theme),
            theme.accent
        );
        assert_eq!(
            stall_intensity_color(Some(Duration::from_secs(5)), &theme),
            theme.warn
        );
        assert_eq!(
            stall_intensity_color(Some(Duration::from_secs(10)), &theme),
            theme.error
        );
    }

    #[test]
    fn rendered_spinner_uses_stall_color_on_first_span() {
        let mut s = StatusIndicator::new();
        let t0 = Instant::now();
        s.set_state(IndicatorState::Thinking { started_at: t0 });
        let line = s.render_at(t0 + Duration::from_secs(10)).unwrap();
        assert_eq!(
            line.spans[0].style.fg,
            Some(crate::tui::theme::current().error)
        );
    }

    #[test]
    fn tool_includes_tool_name() {
        let t0 = Instant::now();
        let state = IndicatorState::Tool {
            name: "bash".into(),
            started_at: t0,
        };
        let line = render_for(&state, 0, None, None, t0 + Duration::from_secs(1)).unwrap();
        assert!(text_of(&line).contains("Running bash"));
    }

    #[test]
    fn primary_label_stays_turn_stable_during_tool_state() {
        let mut s = StatusIndicator::new();
        let t0 = Instant::now();
        s.begin_turn(t0);
        s.set_state(IndicatorState::Thinking { started_at: t0 });

        let thinking = text_of(&s.render_at(t0 + Duration::from_secs(1)).unwrap());
        assert!(
            thinking.contains("Thinking"),
            "initial turn label should be Thinking: {thinking}"
        );

        s.set_state(IndicatorState::Tool {
            name: "bash".into(),
            started_at: t0 + Duration::from_secs(2),
        });
        let tool = text_of(&s.render_at(t0 + Duration::from_secs(3)).unwrap());
        assert!(
            tool.contains("Thinking"),
            "primary label must not switch away from the turn verb: {tool}"
        );
        assert!(
            tool.contains("Running bash"),
            "tool activity should remain visible as secondary context: {tool}"
        );
    }

    #[test]
    fn truecolor_shimmer_splits_label_into_character_spans() {
        let t0 = Instant::now();
        let spans = label_spans_for_mode(
            "Thinking",
            Style::default().fg(Color::Rgb(120, 80, 220)),
            t0 + Duration::from_millis(200),
            true,
        );
        assert_eq!(spans.len(), "Thinking".chars().count());
        assert!(
            spans
                .iter()
                .any(|span| matches!(span.style.fg, Some(Color::Rgb(_, _, _)))),
            "truecolor shimmer should paint per-character RGB spans: {spans:?}"
        );
    }

    #[test]
    fn shimmer_is_disabled_without_truecolor() {
        let t0 = Instant::now();
        let spans = label_spans_for_mode("Thinking", Style::default(), t0, false);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "Thinking");
    }

    #[test]
    fn truecolor_shimmer_phase_is_quantized_to_200ms() {
        fn fg_colors(spans: &[Span<'_>]) -> Vec<Option<Color>> {
            spans.iter().map(|span| span.style.fg).collect()
        }

        let origin = crate::tui::shimmer::process_start();
        let style = Style::default().fg(Color::Rgb(120, 80, 220));
        let at_10ms =
            label_spans_for_mode("Thinking", style, origin + Duration::from_millis(10), true);
        let at_199ms =
            label_spans_for_mode("Thinking", style, origin + Duration::from_millis(199), true);
        let at_200ms =
            label_spans_for_mode("Thinking", style, origin + Duration::from_millis(200), true);

        assert_eq!(
            fg_colors(&at_10ms),
            fg_colors(&at_199ms),
            "shimmer must not advance inside a 200ms phase"
        );
        assert_ne!(
            fg_colors(&at_10ms),
            fg_colors(&at_200ms),
            "shimmer should advance on the next 200ms phase"
        );
    }

    /// Pure spinner verbs — replaces the `Running <name>` fallback
    /// for the well-known tools with an action verb that says what's
    /// actually happening. `bash` keeps the literal name (the user
    /// expects to see the shell name); the unknown-tool fallback
    /// (`Running {name}`) is preserved for plugin tools we don't
    /// have a dedicated verb for. Pinning the table here keeps
    /// future contributors from rewording verbs ad-hoc.
    #[test]
    fn tool_name_maps_to_human_verb() {
        let t0 = Instant::now();
        let cases: &[(&str, &str)] = &[
            ("bash", "Running bash"),
            ("write_file", "Writing"),
            ("read_file", "Reading"),
            ("str_replace", "Editing"),
            ("grep", "Searching"),
            ("glob", "Searching"),
            ("list_dir", "Listing"),
            ("task", "Tracking"),
            ("memory", "Recalling"),
            ("tool_search", "Loading tool"),
            // Unknown plugin tool — fall back to Running.
            ("astra_custom_xyz", "Running astra_custom_xyz"),
        ];
        for (name, expected) in cases {
            let state = IndicatorState::Tool {
                name: (*name).into(),
                started_at: t0,
            };
            let line = render_for(&state, 0, None, None, t0 + Duration::from_secs(1)).unwrap();
            let text = text_of(&line);
            assert!(
                text.contains(expected),
                "tool {name}: expected `{expected}` in line, got `{text}`"
            );
        }
    }

    #[test]
    fn waiting_and_awaiting_render_distinct_labels() {
        let t0 = Instant::now();
        let w = render_for(
            &IndicatorState::WaitingModel { started_at: t0 },
            0,
            None,
            None,
            t0 + Duration::from_secs(0),
        )
        .unwrap();
        assert!(text_of(&w).contains("Waiting for model"));

        let a = render_for(
            &IndicatorState::AwaitingApproval { started_at: t0 },
            0,
            None,
            None,
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
            None,
            None,
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
            None,
            None,
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
    fn begin_turn_zeroes_stream_chars_for_fresh_turn() {
        // Invariant: each turn starts at `↓ 0 tokens`. Mid-turn
        // state changes (Thinking → Tool → Thinking) preserve the
        // counter so it climbs continuously across the whole turn;
        // only `begin_turn` (or transitioning to Idle) resets it.
        let mut s = StatusIndicator::new();
        let t0 = Instant::now();
        s.begin_turn(t0);
        s.set_state(IndicatorState::Thinking { started_at: t0 });
        s.bump_stream_chars(5_000);
        assert_eq!(s.stream_chars, 5_000);

        // Mid-turn tool dispatch must NOT reset the counter — that
        // was the old "spinner flashes 0 every tool call" bug.
        s.set_state(IndicatorState::Tool {
            name: "bash".into(),
            started_at: t0 + Duration::from_secs(1),
        });
        assert_eq!(
            s.stream_chars, 5_000,
            "tool dispatch within a turn must preserve stream_chars"
        );

        // Starting a fresh turn does reset.
        s.begin_turn(t0 + Duration::from_secs(60));
        assert_eq!(s.stream_chars, 0, "new turn must zero the counter");
    }

    /// Bedrock extended-thinking can churn for 60-120s with zero
    /// SSE deltas. Pre-fix the user saw `✶ Thinking (Ns · esc to
    /// interrupt)` with N climbing but no other signal that the
    /// model was actually working — indistinguishable from a
    /// hung UI. Surface a `thought for Ns` chip after a short
    /// silence window so the user can tell the difference.
    #[test]
    fn thought_for_suffix_appears_after_silent_window() {
        let mut s = StatusIndicator::new();
        let t0 = Instant::now();
        s.set_state(IndicatorState::Thinking { started_at: t0 });

        // Just after start: no thought-for chip yet (well within
        // the silent-window threshold).
        let line = s.render_at(t0 + Duration::from_secs(1)).unwrap();
        assert!(
            !text_of(&line).contains("thought for"),
            "fresh thinking shouldn't show thought-for: {}",
            text_of(&line)
        );

        // Past the window with still no token streamed — chip lights up.
        let line = s.render_at(t0 + Duration::from_secs(7)).unwrap();
        let text = text_of(&line);
        assert!(
            text.contains("thought for") && text.contains("7s"),
            "after 7s of silence the chip should show: {text}"
        );
    }

    #[test]
    fn thought_for_suffix_clears_when_token_arrives() {
        // Once any tokens have streamed, we know the model is
        // actively producing — the silent-window chip would be
        // misleading. The token counter takes over as the activity
        // signal.
        let mut s = StatusIndicator::new();
        let t0 = Instant::now();
        s.set_state(IndicatorState::Thinking { started_at: t0 });

        // 7s in with no tokens — chip is showing.
        let line = s.render_at(t0 + Duration::from_secs(7)).unwrap();
        assert!(text_of(&line).contains("thought for"));

        // First token streams.
        s.bump_stream_chars(40); // ~10 tokens
        let line = s.render_at(t0 + Duration::from_secs(8)).unwrap();
        let text = text_of(&line);
        assert!(
            !text.contains("thought for"),
            "thought-for chip should hide once tokens stream: {text}"
        );
        assert!(text.contains("↓"), "token counter takes over: {text}");
    }

    #[test]
    fn thought_for_suffix_only_in_thinking_state() {
        // Tool / Waiting / AwaitingApproval already have their own
        // semantics — adding a "thought for Ns" chip would be
        // nonsense.
        let t0 = Instant::now();
        let line = render_for(
            &IndicatorState::Tool {
                name: "bash".into(),
                started_at: t0,
            },
            0,
            None,
            None,
            t0 + Duration::from_secs(15),
        )
        .unwrap();
        assert!(
            !text_of(&line).contains("thought for"),
            "thought-for chip is Thinking-only"
        );
    }

    #[test]
    fn first_active_set_state_auto_begins_turn() {
        // Production callers in event_loop.rs invoke
        // `set_state(Thinking { started_at: now })` directly without
        // remembering `begin_turn`. The auto-promote keeps the
        // turn-stable elapsed counter working without forcing every
        // call site to be updated.
        let mut s = StatusIndicator::new();
        let t0 = Instant::now();
        s.set_state(IndicatorState::Thinking { started_at: t0 });
        assert_eq!(
            s.turn_started_at,
            Some(t0),
            "first transition out of Idle must seed the turn origin"
        );

        // Subsequent state changes do NOT overwrite the turn origin.
        let later = t0 + Duration::from_secs(5);
        s.set_state(IndicatorState::Tool {
            name: "bash".into(),
            started_at: later,
        });
        assert_eq!(
            s.turn_started_at,
            Some(t0),
            "mid-turn state change must preserve the original turn origin"
        );
    }

    #[test]
    fn idle_state_clears_turn_origin() {
        // Idle terminates a turn — counter and turn origin both clear
        // so a stale next-frame render doesn't ghost the previous turn.
        let mut s = StatusIndicator::new();
        let t0 = Instant::now();
        s.begin_turn(t0);
        s.bump_stream_chars(2_000);
        s.set_state(IndicatorState::Idle);
        assert_eq!(s.stream_chars, 0);
        assert!(s.turn_started_at.is_none());
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
