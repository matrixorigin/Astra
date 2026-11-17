//! One-line live indicator rendered above the composer while a
//! turn is running. Replaces the previous `orbiter_line` + framed
//! thinking-window combo with a single compact activity widget that
//! lives **outside** scrollback.
//!
//! Shape:
//!
//! ```text
//! • Working  16s · ~148 streamed tok                         Ctrl+C to stop
//! • Working  18s · ~148 streamed tok · Bash                  Ctrl+C to stop
//! ```
//!
//! Design rules:
//!
//! - Never part of the scrollback / `HistoryCell` chain. It's
//!   ephemeral — if a user scrolls back they see the result
//!   (tool cell, assistant cell, turn summary), not a stale
//!   status header.
//! - The primary label is turn-stable. Tool activity can appear as
//!   secondary context, but the main verb must not flip between
//!   Thinking / Writing / Reading mid-turn.
//! - The primary label stays static and bold-accent. Earlier
//!   truecolor-only shimmer looked clever in isolation but added visual
//!   noise once paired with the active-cell gutter and footer.
//! - Star rotates on a 500 ms wall-clock tick. Slow heartbeat
//!   beats strobing.
//! - Elapsed time rounds to whole seconds so the counter ticks
//!   once per second instead of jittering sub-second.

use std::time::{Duration, Instant};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// What the model / tool layer is currently doing.
#[derive(Debug, Clone)]
pub(crate) enum IndicatorState {
    /// No turn active — render nothing.
    Idle,
    /// Accepted by the local UI and being dispatched to the selected action
    /// or run path. No remote/model acknowledgement has arrived yet.
    Dispatching { started_at: Instant },
    /// A user stop request is visible locally while the run and any child
    /// control-plane updates converge on a terminal state.
    Cancelling { started_at: Instant },
    /// The process is preserving the current turn and finalizing its session
    /// before returning control to the shell.
    Exiting { started_at: Instant },
    /// Turn is running, model is thinking / producing tokens.
    Thinking { started_at: Instant },
    /// Tool is executing mid-turn.
    Tool { name: String, started_at: Instant },
    /// Model is being called but hasn't responded yet (pre-SSE).
    WaitingModel { started_at: Instant },
    /// Disposable last report from the execution owner, not durable run state.
    WaitingInput { started_at: Instant },
    /// Queued on an approval prompt.
    AwaitingApproval { started_at: Instant },
}

impl IndicatorState {
    fn started_at(&self) -> Option<Instant> {
        match self {
            IndicatorState::Idle => None,
            IndicatorState::Dispatching { started_at }
            | IndicatorState::Cancelling { started_at }
            | IndicatorState::Exiting { started_at }
            | IndicatorState::Thinking { started_at }
            | IndicatorState::Tool { started_at, .. }
            | IndicatorState::WaitingInput { started_at }
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
#[derive(Debug, Default, Clone)]
pub(crate) struct StatusIndicator {
    state: IndicatorState,
    bash_background_hint_enabled: bool,
    /// Per-turn answer-channel characters, used only for a rough display
    /// estimate. This is not provider usage or a billable token count.
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

const DEFAULT_TURN_LABEL: &str = "Working";

impl StatusIndicator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> &IndicatorState {
        &self.state
    }

    pub fn set_bash_background_hint_enabled(&mut self, enabled: bool) {
        self.bash_background_hint_enabled = enabled;
    }

    pub fn set_state(&mut self, state: IndicatorState) {
        // Only the submission boundary may open a turn. Provider progress can
        // arrive after a terminal event because input and stream producers use
        // independent queues; accepting it from Idle resurrects a finished
        // turn and leaves the composer stuck in follow-up mode.
        if !matches!(state, IndicatorState::Idle) && self.turn_started_at.is_none() {
            return;
        }
        // Stream events queued before Ctrl+C can arrive during shutdown. They
        // are progress evidence, not authority to revoke the user's cancel
        // intent, so keep Stopping monotonic until terminal settlement.
        if matches!(
            self.state,
            IndicatorState::Cancelling { .. } | IndicatorState::Exiting { .. }
        ) && !matches!(
            state,
            IndicatorState::Cancelling { .. }
                | IndicatorState::Exiting { .. }
                | IndicatorState::Idle
        ) {
            return;
        }
        // The stream counter resets across state changes only when
        // entering a *brand new turn*. Within a turn, tool ↔ thinking
        // transitions preserve `stream_chars` so the qualified estimate keeps
        // climbing instead of flashing back to 0 each time the model
        // fires a tool. `begin_turn` is the explicit reset point.
        // The Ctrl+B hint is bash-tool-scoped — drop it on any state
        // that isn't a bash tool so a stale flag can't render under
        // a different tool or thinking state.
        let entering_bash_tool = matches!(
            &state,
            IndicatorState::Tool { name, .. } if name == "bash"
        );
        if !entering_bash_tool {
            self.bash_background_hint_enabled = false;
        }
        if matches!(state, IndicatorState::Idle) {
            self.stream_chars = 0;
            self.turn_started_at = None;
            self.turn_label = None;
        } else if matches!(
            state,
            IndicatorState::Cancelling { .. } | IndicatorState::Exiting { .. }
        ) {
            self.turn_label = Some("Stopping");
        }
        self.state = state;
    }

    pub fn turn_is_open(&self) -> bool {
        self.turn_started_at.is_some()
    }

    pub fn is_visible(&self) -> bool {
        self.state.is_active()
    }

    /// Mark the start of a new turn. Drives the elapsed counter that
    /// renders in the spinner suffix (`7s · Ctrl+C to stop`).
    /// Called at the same moment the host transitions out of Idle —
    /// usually right alongside `set_state(Thinking { ... })` or
    /// `set_state(WaitingModel { ... })`.
    pub fn begin_turn(&mut self, at: Instant) {
        self.turn_started_at = Some(at);
        self.turn_label = Some(DEFAULT_TURN_LABEL);
        self.stream_chars = 0;
    }

    /// Begin the local-acceptance phase before authentication, network, or
    /// action dispatch. The subsequent runtime event promotes this label to
    /// the stable `Working` turn label without resetting elapsed time.
    pub fn begin_dispatch(&mut self, at: Instant) {
        self.turn_started_at = Some(at);
        self.turn_label = Some("Sending");
        self.stream_chars = 0;
        self.state = IndicatorState::Dispatching { started_at: at };
    }

    /// Project an accepted process-exit request immediately. Unlike
    /// cancellation, this is also valid while idle, when no turn start exists.
    pub fn begin_exit(&mut self, at: Instant) {
        self.turn_started_at.get_or_insert(at);
        self.turn_label = Some("Stopping");
        self.state = IndicatorState::Exiting { started_at: at };
    }

    pub fn mark_dispatched(&mut self) {
        if matches!(self.state, IndicatorState::Dispatching { .. }) {
            self.turn_label = Some(DEFAULT_TURN_LABEL);
        }
    }

    pub fn bump_stream_chars(&mut self, n: usize) {
        self.stream_chars = self.stream_chars.saturating_add(n as u64);
    }

    /// Produce the single line to render. `None` means "don't
    /// draw anything" — the caller should reserve zero rows.
    #[cfg(test)]
    pub fn render(&self, width: u16) -> Option<Line<'static>> {
        self.render_at_width(Instant::now(), width)
    }

    pub fn render_with_context(
        &self,
        width: u16,
        approval_pending: bool,
        can_interrupt: bool,
    ) -> Option<Line<'static>> {
        self.render_at_with_context(Instant::now(), width, approval_pending, can_interrupt)
    }

    /// Like `render` but with an explicit `now`. Lets tests pin the
    /// clock without mocking `Instant`. Production callers use
    /// `render()`.
    #[cfg(test)]
    pub fn render_at(&self, now: Instant) -> Option<Line<'static>> {
        self.render_at_width(now, 120)
    }

    #[cfg(test)]
    fn render_at_width(&self, now: Instant, width: u16) -> Option<Line<'static>> {
        self.render_at_with_context(now, width, false, true)
    }

    fn render_at_with_context(
        &self,
        now: Instant,
        width: u16,
        approval_pending: bool,
        can_interrupt: bool,
    ) -> Option<Line<'static>> {
        render_for_with_bash_hint(
            &self.state,
            self.stream_chars,
            self.turn_started_at,
            self.turn_label,
            now,
            RenderContext {
                bash_background_hint_enabled: self.bash_background_hint_enabled,
                width,
                approval_pending,
                can_interrupt,
            },
        )
    }
}

/// Rendering extracted to a free fn with explicit `now` so tests
/// can pin the clock without mocking `Instant`. `turn_started_at`
/// (when set) wins over the per-state `started_at` so a tool
/// dispatch mid-turn doesn't reset the visible elapsed counter.
#[cfg(test)]
fn render_for(
    state: &IndicatorState,
    stream_chars: u64,
    turn_started_at: Option<Instant>,
    turn_label: Option<&'static str>,
    now: Instant,
) -> Option<Line<'static>> {
    render_for_with_bash_hint(
        state,
        stream_chars,
        turn_started_at,
        turn_label,
        now,
        RenderContext {
            bash_background_hint_enabled: false,
            width: 120,
            approval_pending: false,
            can_interrupt: true,
        },
    )
}

struct RenderContext {
    bash_background_hint_enabled: bool,
    width: u16,
    approval_pending: bool,
    can_interrupt: bool,
}

fn render_for_with_bash_hint(
    state: &IndicatorState,
    stream_chars: u64,
    turn_started_at: Option<Instant>,
    turn_label: Option<&'static str>,
    now: Instant,
    context: RenderContext,
) -> Option<Line<'static>> {
    let RenderContext {
        bash_background_hint_enabled,
        width,
        approval_pending,
        can_interrupt,
    } = context;
    if !state.is_active() {
        return None;
    }

    let elapsed = turn_started_at
        .or_else(|| state.started_at())
        .and_then(|t| now.checked_duration_since(t));

    let theme = crate::tui::theme::current();
    let approval_pending = approval_pending
        && !matches!(
            state,
            IndicatorState::Cancelling { .. } | IndicatorState::Exiting { .. }
        );
    let state_color = if approval_pending {
        theme.warn
    } else {
        indicator_state_color(state, theme)
    };
    let star_style = Style::default()
        .fg(state_color)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(theme.fg).add_modifier(Modifier::BOLD);
    let text_style = Style::default().fg(theme.fg);
    let dim = Style::default().fg(theme.dim);

    let state_label: String = if approval_pending {
        "Approval needed".into()
    } else {
        match state {
            IndicatorState::Dispatching { .. } => "Sending".into(),
            IndicatorState::Cancelling { .. } => "Stopping".into(),
            IndicatorState::Exiting { .. } => "Stopping".into(),
            IndicatorState::Thinking { .. } => "Thinking".into(),
            IndicatorState::Tool { name, .. } => label_for_tool(name),
            IndicatorState::WaitingModel { .. } => "Starting".into(),
            IndicatorState::WaitingInput { started_at } => {
                if now.saturating_duration_since(*started_at) < Duration::from_secs(5) {
                    "Agent input (last report)".into()
                } else {
                    DEFAULT_TURN_LABEL.into()
                }
            }
            IndicatorState::AwaitingApproval { .. } => "Approval needed".into(),
            IndicatorState::Idle => return None,
        }
    };
    let label = turn_label.unwrap_or(&state_label);
    let activity =
        if state_label == label || (label == DEFAULT_TURN_LABEL && state_label == "Thinking") {
            None
        } else {
            Some(state_label.as_str())
        };

    let width = usize::from(width);
    let elapsed = elapsed.map(fmt_duration_coarse);
    let core_width = label.width() + elapsed.as_ref().map_or(0, |text| 2 + text.width());
    let attention = approval_pending
        || matches!(
            state,
            IndicatorState::AwaitingApproval { .. }
                | IndicatorState::Cancelling { .. }
                | IndicatorState::Exiting { .. }
        );
    let symbol = if attention { "!" } else { star_frame(now) };
    let mut spans = Vec::new();
    let mut used = 0;
    if core_width.saturating_add(2) <= width {
        spans.push(Span::styled(format!("{symbol} "), star_style));
        used += 2;
    }
    let visible_label = clip_cells(label, width.saturating_sub(used));
    used += visible_label.width();
    spans.push(Span::styled(visible_label, label_style));
    if let Some(elapsed) = elapsed
        && used + 2 + elapsed.width() <= width
    {
        used += 2 + elapsed.width();
        spans.push(Span::styled(format!("  {elapsed}"), text_style));
    }

    // Approval is actionable; keep it ahead of the approximate counter.
    if (approval_pending || matches!(state, IndicatorState::AwaitingApproval { .. }))
        && let Some(activity) = activity
    {
        append_field(&mut spans, &mut used, width, activity, dim, text_style);
    }
    if stream_chars > 0 {
        let count = fmt_tokens(approx_tokens(stream_chars));
        let full = format!("~{count} streamed tok");
        let compact = format!("~{count} tok");
        let value = if used + 3 + full.width() <= width {
            &full
        } else {
            &compact
        };
        append_field(&mut spans, &mut used, width, value, dim, text_style);
    }
    if !approval_pending
        && !matches!(state, IndicatorState::AwaitingApproval { .. })
        && let Some(activity) = activity
    {
        let available = width.saturating_sub(used + 3);
        if available >= 4 {
            let activity = clip_cells(activity, available);
            append_field(&mut spans, &mut used, width, &activity, dim, text_style);
        }
    }

    if !matches!(
        state,
        IndicatorState::Cancelling { .. } | IndicatorState::Exiting { .. }
    ) {
        let stop = "Ctrl+C to stop";
        let background = (bash_background_hint_enabled
            && matches!(state, IndicatorState::Tool { name, .. } if name == "bash"))
        .then(|| {
            format!(
                "{} to background",
                crate::tui::background_shortcut::ctrl_b_background_shortcut()
            )
        });
        let combined = background
            .as_ref()
            .filter(|_| can_interrupt)
            .map(|background| {
                format!(
                    "{background}{}{stop}",
                    crate::tui::glyphs::current().status_separator
                )
            });
        let hint = combined
            .as_deref()
            .filter(|hint| used + 2 + hint.width() <= width)
            .or({
                if can_interrupt {
                    Some(stop)
                } else {
                    background.as_deref()
                }
            });
        if let Some(hint) = hint
            && used + 2 + hint.width() <= width
        {
            spans.push(Span::raw(" ".repeat(width - used - hint.width())));
            spans.push(Span::styled(hint.to_string(), dim));
        }
    }
    Some(Line::from(spans))
}

fn append_field(
    spans: &mut Vec<Span<'static>>,
    used: &mut usize,
    width: usize,
    text: &str,
    separator: Style,
    style: Style,
) {
    let glyphs = crate::tui::glyphs::current();
    let needed = glyphs.status_separator.width() + text.width();
    if *used + needed <= width {
        spans.push(Span::styled(glyphs.status_separator, separator));
        spans.push(Span::styled(text.to_string(), style));
        *used += needed;
    }
}

fn clip_cells(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let next = ch.width().unwrap_or(0);
        if used + next > width.saturating_sub(1) {
            break;
        }
        result.push(ch);
        used += next;
    }
    result.push_str(crate::tui::glyphs::current().truncation);
    result
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
        "bash" => "Bash".into(),
        "write_file" => "Writing".into(),
        "read_file" => "Reading".into(),
        "str_replace" => "Editing".into(),
        "grep" | "glob" => "Searching".into(),
        "list_dir" => "Listing".into(),
        "memory" => "Recalling".into(),
        "tool_search" => "Loading tool".into(),
        _ => {
            let safe = crate::tui::render::line_utils::sanitize_terminal_text(name);
            let name = safe.split_whitespace().collect::<Vec<_>>().join(" ");
            format!("Running {name}")
        }
    }
}

fn indicator_state_color(state: &IndicatorState, theme: &crate::tui::theme::Theme) -> Color {
    match state {
        IndicatorState::AwaitingApproval { .. } => theme.warn,
        IndicatorState::Cancelling { .. } | IndicatorState::Exiting { .. } => theme.warn,
        IndicatorState::Dispatching { .. } => theme.accent,
        IndicatorState::Thinking { .. }
        | IndicatorState::Tool { .. }
        | IndicatorState::WaitingInput { .. }
        | IndicatorState::WaitingModel { .. } => theme.gutter,
        IndicatorState::Idle => theme.dim,
    }
}

fn fmt_duration_coarse(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3_600 {
        format!("{}h {:02}m", secs / 3_600, secs % 3_600 / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
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
    let bucket = (crate::tui::shimmer::time_at(now).max(0.0) * 2.0).floor() as usize;
    let frames = crate::tui::glyphs::current().activity_frames;
    frames[bucket % frames.len()]
}

#[cfg(test)]
mod tests {
    use super::{
        IndicatorState, StatusIndicator, approx_tokens, fmt_duration_coarse, fmt_tokens,
        indicator_state_color, render_for, star_frame,
    };
    use ratatui::text::Line;
    use std::time::{Duration, Instant};

    fn text_of(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.to_string()).collect()
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
        assert!(s.render(80).is_none());
    }

    #[test]
    fn exit_is_visible_without_an_active_turn() {
        let mut indicator = StatusIndicator::new();
        let started_at = Instant::now();

        indicator.begin_exit(started_at);

        let rendered = text_of(
            &indicator
                .render_at(started_at + Duration::from_secs(1))
                .expect("accepted exit remains visible during finalization"),
        );
        assert!(rendered.contains("Stopping"), "{rendered}");
        assert!(!rendered.contains("Ctrl+C to stop"), "{rendered}");
    }

    #[test]
    fn thinking_contains_star_label_and_elapsed() {
        let mut s = StatusIndicator::new();
        let t0 = Instant::now();
        s.begin_turn(t0);
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
        assert!(
            crate::tui::glyphs::current()
                .activity_frames
                .iter()
                .any(|g| text.contains(g)),
            "indicator dot missing: {text}"
        );
        assert!(text.contains("Thinking"));
        assert!(text.contains("3s"));
        assert!(text.contains("Ctrl+C to stop"));
    }

    #[test]
    fn elapsed_time_does_not_invent_stall_severity() {
        let theme = crate::tui::theme::Theme::dark();
        assert_eq!(
            indicator_state_color(
                &IndicatorState::Thinking {
                    started_at: Instant::now(),
                },
                &theme,
            ),
            theme.gutter
        );
    }

    #[test]
    fn long_running_activity_stays_neutral_without_typed_failure_evidence() {
        let mut s = StatusIndicator::new();
        let t0 = Instant::now();
        s.begin_turn(t0);
        s.set_state(IndicatorState::Thinking { started_at: t0 });
        let line = s.render_at(t0 + Duration::from_secs(600)).unwrap();
        assert_eq!(
            line.spans[0].style.fg,
            Some(crate::tui::theme::current().gutter)
        );
    }

    #[test]
    fn approval_state_uses_warning_color_from_typed_state() {
        let theme = crate::tui::theme::Theme::dark();
        assert_eq!(
            indicator_state_color(
                &IndicatorState::AwaitingApproval {
                    started_at: Instant::now(),
                },
                &theme,
            ),
            theme.warn
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
        let text = text_of(&line);
        assert!(text.contains("Bash"));
    }

    #[test]
    fn bash_tool_can_surface_ctrl_b_hint() {
        let t0 = Instant::now();
        let mut s = StatusIndicator::new();
        s.begin_turn(t0);
        s.set_state(IndicatorState::Tool {
            name: "bash".into(),
            started_at: t0,
        });
        s.set_bash_background_hint_enabled(true);

        let text = text_of(&s.render_at(t0 + Duration::from_secs(18)).unwrap());
        assert!(text.contains("Bash"), "{text}");
        assert!(
            text.contains(&format!(
                "{} to background",
                crate::tui::background_shortcut::ctrl_b_background_shortcut()
            )),
            "{text}"
        );
        assert!(text.contains("Ctrl+C to stop"), "{text}");

        let with_draft = text_of(
            &s.render_at_with_context(t0 + Duration::from_secs(18), 120, false, false)
                .unwrap(),
        );
        assert!(with_draft.contains("to background"), "{with_draft}");
        assert!(!with_draft.contains("Ctrl+C to stop"), "{with_draft}");

        s.set_state(IndicatorState::Idle);
        assert!(
            !s.bash_background_hint_enabled,
            "idle transition must clear stale Ctrl+B affordance"
        );
    }

    /// Bash-only hint state must not survive a transition to any other
    /// indicator state — even within the same turn. Without this guard,
    /// a bash → read_file transition would leave `bash_background_hint`
    /// flipped to true, and any future renderer that forgets to gate on
    /// `IndicatorState::Tool { name == "bash" }` would draw the Ctrl+B
    /// affordance under an unrelated tool. Better to drop the flag at
    /// the source.
    #[test]
    fn non_bash_state_transition_clears_ctrl_b_hint() {
        let t0 = Instant::now();
        let mut s = StatusIndicator::new();
        s.begin_turn(t0);
        s.set_state(IndicatorState::Tool {
            name: "bash".into(),
            started_at: t0,
        });
        s.set_bash_background_hint_enabled(true);
        assert!(s.bash_background_hint_enabled);

        // bash → another tool: hint must drop.
        s.set_state(IndicatorState::Tool {
            name: "read_file".into(),
            started_at: t0,
        });
        assert!(
            !s.bash_background_hint_enabled,
            "non-bash tool transition must clear stale Ctrl+B affordance"
        );

        // re-arming, then bash → thinking must also drop.
        s.set_state(IndicatorState::Tool {
            name: "bash".into(),
            started_at: t0,
        });
        s.set_bash_background_hint_enabled(true);
        s.set_state(IndicatorState::Thinking { started_at: t0 });
        assert!(
            !s.bash_background_hint_enabled,
            "bash → thinking must clear stale Ctrl+B affordance"
        );
    }

    #[test]
    fn primary_label_stays_turn_stable_during_tool_state() {
        let mut s = StatusIndicator::new();
        let t0 = Instant::now();
        s.begin_turn(t0);
        s.set_state(IndicatorState::Thinking { started_at: t0 });

        let thinking = text_of(&s.render_at(t0 + Duration::from_secs(1)).unwrap());
        assert!(
            thinking.contains("Working"),
            "initial turn label should be Working: {thinking}"
        );

        s.set_state(IndicatorState::Tool {
            name: "bash".into(),
            started_at: t0 + Duration::from_secs(2),
        });
        let tool = text_of(&s.render_at(t0 + Duration::from_secs(3)).unwrap());
        assert!(
            tool.contains("Working"),
            "primary label must not switch away from the turn verb: {tool}"
        );
        assert!(
            tool.contains("Bash"),
            "tool activity should remain visible as secondary context: {tool}"
        );
    }

    #[test]
    fn activity_label_stays_stable_across_heartbeat_frames() {
        let origin = crate::tui::shimmer::process_start();
        let mut indicator = StatusIndicator::new();
        indicator.begin_turn(origin);
        indicator.set_state(IndicatorState::Thinking { started_at: origin });
        let label = |at| {
            indicator
                .render_at(at)
                .unwrap()
                .spans
                .into_iter()
                .find(|span| span.content == "Working")
                .unwrap()
        };
        assert_eq!(label(origin), label(origin + Duration::from_millis(600)));
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
            ("bash", "Bash"),
            ("write_file", "Writing"),
            ("read_file", "Reading"),
            ("str_replace", "Editing"),
            ("grep", "Searching"),
            ("glob", "Searching"),
            ("list_dir", "Listing"),
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
        let w_text = text_of(&w);
        assert!(w_text.contains("Starting"));

        let a = render_for(
            &IndicatorState::AwaitingApproval { started_at: t0 },
            0,
            None,
            None,
            t0 + Duration::from_secs(0),
        )
        .unwrap();
        let a_text = text_of(&a);
        assert!(a_text.contains("Approval needed"));
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
            !text.contains('~'),
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
        assert!(text.contains("~5.0k streamed tok"), "{text}");
        assert!(
            !text.contains("↓"),
            "token counter should now be calmer text without arrows: {text}"
        );
    }

    #[test]
    fn tool_state_keeps_qualified_stream_estimate() {
        let t0 = Instant::now();
        let line = render_for(
            &IndicatorState::Tool {
                name: "bash".into(),
                started_at: t0,
            },
            20_000,
            None,
            None,
            t0 + Duration::from_secs(5),
        )
        .unwrap();
        let text = text_of(&line);
        assert!(
            text.contains("Bash"),
            "tool activity should stay visible: {text}"
        );
        assert!(
            text.contains("~5.0k streamed tok"),
            "tool state should retain the answer-stream estimate: {text}"
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
        // Invariant: each turn starts without a displayed estimate. Mid-turn
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

    #[test]
    fn star_frame_advances_across_half_second_buckets() {
        let origin = crate::tui::shimmer::process_start();
        let a = star_frame(origin);
        let b = star_frame(origin + Duration::from_millis(600));
        let c = star_frame(origin + Duration::from_millis(1100));
        assert_ne!(a, b, "indicator frame should advance after 600ms");
        assert_ne!(b, c, "indicator frame should continue advancing");
    }

    #[test]
    fn progress_cannot_resurrect_a_terminal_turn() {
        let mut s = StatusIndicator::new();
        let t0 = Instant::now();
        s.set_state(IndicatorState::Thinking { started_at: t0 });
        assert!(matches!(s.state(), IndicatorState::Idle));
        assert!(s.turn_started_at.is_none());

        s.begin_turn(t0);
        s.set_state(IndicatorState::Thinking { started_at: t0 });
        s.set_state(IndicatorState::Idle);
        s.set_state(IndicatorState::WaitingModel {
            started_at: t0 + Duration::from_secs(1),
        });
        assert!(matches!(s.state(), IndicatorState::Idle));
        assert!(s.render_at(t0 + Duration::from_secs(2)).is_none());
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

    #[test]
    fn cancelling_replaces_working_and_removes_stale_stop_affordance() {
        let mut indicator = StatusIndicator::new();
        let started_at = Instant::now();
        indicator.begin_turn(started_at);
        indicator.set_state(IndicatorState::Thinking { started_at });
        indicator.set_state(IndicatorState::Cancelling { started_at });
        indicator.set_state(IndicatorState::Tool {
            name: "agent_fanout".into(),
            started_at: started_at + Duration::from_secs(1),
        });

        let rendered = text_of(
            &indicator
                .render_at(started_at + Duration::from_secs(2))
                .expect("cancelling remains visible until settlement"),
        );
        assert!(rendered.contains("Stopping"), "{rendered}");
        assert!(!rendered.contains("Working"), "{rendered}");
        assert!(!rendered.contains("Ctrl+C to stop"), "{rendered}");
    }

    #[test]
    fn narrow_status_is_cell_bounded_and_never_invents_usage() {
        let start = Instant::now();
        let mut indicator = StatusIndicator::new();
        indicator.begin_turn(start);
        indicator.set_state(IndicatorState::Thinking { started_at: start });
        for width in [0, 1, 8, 12, 24, 40, 80] {
            let line = indicator
                .render_at_width(start + Duration::from_secs(18), width)
                .unwrap();
            assert!(
                line.width() <= usize::from(width),
                "width={width}: {line:?}"
            );
            assert!(!text_of(&line).contains("tok"));
        }
        indicator.bump_stream_chars(1);
        let line = indicator
            .render_at_width(start + Duration::from_secs(18), 40)
            .unwrap();
        assert!(text_of(&line).contains("~1 streamed tok"));
        indicator.set_state(IndicatorState::Tool {
            name: "插件\x1b[31m工具\nextra".into(),
            started_at: start,
        });
        for width in [12, 24, 40, 80] {
            let line = indicator
                .render_at_width(start + Duration::from_secs(19), width)
                .unwrap();
            let text = text_of(&line);
            assert!(line.width() <= usize::from(width), "{text:?}");
            assert!(!text.contains('\x1b') && !text.contains('\n'), "{text:?}");
        }
    }

    #[test]
    fn late_dispatch_does_not_relabel_cancellation_and_exit_keeps_turn_time() {
        let start = Instant::now();
        let mut indicator = StatusIndicator::new();
        indicator.begin_dispatch(start);
        indicator.set_state(IndicatorState::Cancelling {
            started_at: start + Duration::from_secs(3),
        });
        indicator.mark_dispatched();
        indicator.set_state(IndicatorState::Thinking {
            started_at: start + Duration::from_secs(4),
        });
        indicator.begin_exit(start + Duration::from_secs(5));
        let text = text_of(&indicator.render_at(start + Duration::from_secs(9)).unwrap());
        assert!(text.contains("Stopping") && text.contains("9s"), "{text}");
        assert!(!text.contains("Working"), "{text}");
    }

    // ── formatting ───────────────────────────────────────────────

    #[test]
    fn fmt_duration_rounds_to_whole_seconds_under_a_minute() {
        assert_eq!(fmt_duration_coarse(Duration::from_millis(400)), "0s");
        assert_eq!(fmt_duration_coarse(Duration::from_millis(1_999)), "1s");
        assert_eq!(fmt_duration_coarse(Duration::from_secs(59)), "59s");
        assert_eq!(fmt_duration_coarse(Duration::from_secs(60)), "1m 00s");
        assert_eq!(fmt_duration_coarse(Duration::from_secs(125)), "2m 05s");
        assert_eq!(fmt_duration_coarse(Duration::from_secs(3_600)), "1h 00m");
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
