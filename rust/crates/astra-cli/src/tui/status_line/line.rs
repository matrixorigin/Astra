//! Pure status-line composition.

#![allow(dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

/// Permission mode expressed as an enum rather than a string so the
/// status line can't silently render typo'd values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PermissionMode {
    #[default]
    Ask,
    Auto,
    Plan,
    AcceptEdits,
    Deny,
}

impl PermissionMode {
    pub fn chip_text(self) -> &'static str {
        // Plain words — emojis varied wildly across themes and got
        // flagged as visually noisy ("🔍" and "✎" never landed). The
        // chip's colour already carries the urgency signal: blue for
        // plan, cyan for edit, yellow for auto, red for deny.
        match self {
            Self::Ask => "default",
            Self::Auto => "auto",
            Self::Plan => "plan",
            Self::AcceptEdits => "edit",
            Self::Deny => "deny",
        }
    }
}

/// Inputs that feed the status line. Owned so the struct is `Clone`
/// and easy to fixture.
#[derive(Debug, Clone, Default)]
pub(crate) struct StatusContext {
    pub model: Option<String>,
    pub cwd: Option<String>,
    /// Tuple `(used, limit)` in tokens.
    pub token_budget: Option<(u64, u64)>,
    pub permission_mode: PermissionMode,
    pub turn_active: bool,
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
    pub git_branch: Option<String>,
    /// Number of approvals currently awaiting a user decision.
    pub pending_approvals: usize,
    /// `(open, total)` task counts for the footer task-board chip.
    /// `None` when the board has no tasks — chip hides rather than
    /// wasting space with `0/0`.
    pub task_counts: Option<(usize, usize)>,
    /// Whether the user has Ctrl+T-expanded the task board. Controls
    /// the chip glyph (`▼` expanded, `▶` collapsed) so the key's
    /// target state is visible.
    pub task_board_expanded: bool,
    /// `(running, stalled)` counts of the BackgroundTaskRegistry.
    /// `None` when the registry has no live state to surface; the
    /// chip also hides on `Some((0, 0))` so a long-lived registry
    /// with no live tasks doesn't waste status-line width.
    pub bg_task_counts: Option<(usize, usize)>,
}

/// A styled text fragment that appears on either side of the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Segment {
    pub text: String,
    pub style: Style,
}

impl Segment {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
        }
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

/// Composed status line. `left` / `right` hold the segments; a caller
/// renders them in their preferred layout (or uses [`StatusLine::plain`]
/// for a single left-joined-right-joined string).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct StatusLine {
    pub left: Vec<Segment>,
    pub right: Vec<Segment>,
}

/// Idle hint: make each prefix legible instead of showing raw symbols.
/// These prefixes change the meaning of the next keystroke, so the
/// footer should label them directly rather than expecting the user to
/// remember `/ @ $` by heart.
pub(crate) const IDLE_HINT_FULL: &str = "/commands @mention $shell";
/// Same hint, but compressed enough to preserve the right-side model/cwd
/// chips on 80-column terminals.
pub(crate) const IDLE_HINT_SHORT: &str = "/cmd @mention $sh";
/// Same hint collapsed for narrow terminals so the model chip still fits.
pub(crate) const IDLE_HINT_TINY: &str = "/ @ $";

/// Threshold below which the budget chip is dim, above which it warns.
const BUDGET_WARN_PERCENT: f32 = 75.0;
const BUDGET_ERROR_PERCENT: f32 = 90.0;
/// Max cwd segment width before the middle is elided.
const CWD_MAX_WIDTH: usize = 28;

impl StatusLine {
    /// Build a status line from context.
    pub fn from_context(ctx: &StatusContext) -> Self {
        let muted = Style::default().fg(Color::Gray);
        let hint = Style::default().fg(Color::White);
        let mut out = Self::default();

        // ── Left: short hint, then permission chip if non-default ─
        //
        // Idle hint is built in two forms so the renderer can swap to the
        // short form when the terminal is too narrow for the full one.
        // `render()` picks between them based on remaining width.
        let hint_text = if ctx.turn_active {
            "Ctrl+C interrupt".to_string()
        } else {
            IDLE_HINT_FULL.to_string()
        };
        out.left.push(Segment::styled(hint_text, hint));

        match ctx.permission_mode {
            PermissionMode::Ask => {
                out.left.push(Segment::styled(
                    PermissionMode::Ask.chip_text(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            PermissionMode::Auto => {
                out.left.push(Segment::styled(
                    PermissionMode::Auto.chip_text(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            PermissionMode::Plan => {
                out.left.push(Segment::styled(
                    PermissionMode::Plan.chip_text(),
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            PermissionMode::AcceptEdits => {
                out.left.push(Segment::styled(
                    PermissionMode::AcceptEdits.chip_text(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            PermissionMode::Deny => {
                out.left.push(Segment::styled(
                    PermissionMode::Deny.chip_text(),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
            }
        }

        // Hint that the chip itself is the cycle anchor — without
        // this the user has no surface clue that ⇧Tab moves between
        // modes (the previous global "⇧Tab mode" hint was deleted
        // for being repetitive and unanchored). Dim so it reads as
        // metadata of the chip, not an action of its own.
        out.left.push(Segment::styled("⇧Tab", muted));

        if ctx.pending_approvals > 0 {
            let text = if ctx.pending_approvals == 1 {
                "⏸ 1 pending".to_string()
            } else {
                format!("⏸ {} pending", ctx.pending_approvals)
            };
            out.left
                .push(Segment::styled(text, Style::default().fg(Color::Yellow)));
        }

        // Task-board chip. `▶` = collapsed (Ctrl+T to expand),
        // `▼` = expanded. Count is `open/total` for mixed boards and
        // `total done` when nothing is open.
        if let Some((open, total)) = ctx.task_counts {
            if total > 0 {
                let glyph = if ctx.task_board_expanded {
                    "▼"
                } else {
                    "▶"
                };
                let (text, style) = if open == 0 {
                    (
                        format!("{glyph} {total} done"),
                        Style::default().fg(Color::Green),
                    )
                } else {
                    (format!("{glyph} {open}/{total}"), muted)
                };
                out.left.push(Segment::styled(text, style));
            }
        }

        // BackgroundTaskRegistry chip. Surfaces fire-and-poll work
        // (agent_job.shell / agent_job.agent) so the user knows how
        // many bg jobs are live without opening a separate view.
        // Style:
        //   - any stalled → yellow (alarm: process likely waiting on
        //     interactive input; user should kill or acknowledge)
        //   - running only → dim (informational)
        //   - both 0 → chip hidden (registry exists but is idle)
        if let Some((running, stalled)) = ctx.bg_task_counts {
            if running > 0 || stalled > 0 {
                let mut text = format!("BG: {running} running");
                if stalled > 0 {
                    text.push_str(&format!(" · {stalled} stalled"));
                }
                let style = if stalled > 0 {
                    Style::default().fg(Color::Yellow)
                } else {
                    muted
                };
                out.left.push(Segment::styled(text, style));
            }
        }

        // ── Right: model · branch · cwd · tokens · cost ───────────
        if let Some(model) = ctx.model.as_deref() {
            out.right.push(Segment::styled(
                model.to_string(),
                Style::default().fg(Color::White),
            ));
        }

        if let Some(branch) = ctx.git_branch.as_deref() {
            out.right
                .push(Segment::styled(format!("⎇ {branch}"), muted));
        }

        if let Some(cwd) = ctx.cwd.as_deref() {
            out.right
                .push(Segment::styled(truncate_cwd(cwd, CWD_MAX_WIDTH), muted));
        }

        if let Some((used, limit)) = ctx.token_budget {
            if limit > 0 {
                let pct = (used as f32 / limit as f32) * 100.0;
                let style = if pct >= BUDGET_ERROR_PERCENT {
                    Style::default().fg(Color::Red)
                } else if pct >= BUDGET_WARN_PERCENT {
                    Style::default().fg(Color::Yellow)
                } else {
                    muted
                };
                out.right.push(Segment::styled(
                    format!("{pct:.0}% ({})", format_tokens_compact(used)),
                    style,
                ));
            }
        }

        if let Some(cost) = ctx.cost_usd {
            out.right
                .push(Segment::styled(format!("${cost:.2}"), muted));
        }

        out
    }

    /// Render to a simple string for testing: left joined by ' · ',
    /// two-space gap, right joined by ' · '.
    pub fn plain(&self) -> String {
        let left: Vec<&str> = self.left.iter().map(|s| s.text.as_str()).collect();
        let right: Vec<&str> = self.right.iter().map(|s| s.text.as_str()).collect();
        let l = left.join(" · ");
        let r = right.join(" · ");
        if l.is_empty() {
            r
        } else if r.is_empty() {
            l
        } else {
            format!("{l}  {r}")
        }
    }

    /// Draw into `area` of `buf`. Left side sticks to the left edge; right
    /// side is right-aligned. When the terminal is too narrow for both,
    /// right-side segments are dropped one at a time (tail first) until
    /// the line fits.
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let sep = Span::styled(" · ", Style::default().fg(Color::Gray));

        // Narrow-width degradation of the idle hint. When the full hint
        // won't leave room for the model name / key stats on the right,
        // swap to the short form, and ultimately to the tiny form. Only
        // triggers if segment 0 is one of our known hint strings, so we
        // never mutate user-supplied content.
        let left_segments = self.narrowed_left_segments(area.width);

        let left_spans = join_segments(&left_segments, &sep, 2 /* leading indent */);
        let mut right_segments: &[Segment] = &self.right;
        let mut right_spans;
        let left_w: usize = left_spans.iter().map(|s| s.content.width()).sum();

        loop {
            right_spans = join_segments(right_segments, &sep, 0);
            let right_w: usize = right_spans.iter().map(|s| s.content.width()).sum();
            let total = left_w + right_w + 2; // 2-char trailing margin
            if total <= area.width as usize || right_segments.is_empty() {
                break;
            }
            // Drop the trailing right segment and retry.
            right_segments = &right_segments[..right_segments.len() - 1];
        }

        let right_w: usize = right_spans.iter().map(|s| s.content.width()).sum();
        let padding = (area.width as usize).saturating_sub(left_w + right_w + 2);

        let mut all = left_spans;
        all.push(Span::raw(" ".repeat(padding)));
        all.extend(right_spans);

        Widget::render(Line::from(all), area, buf);
    }

    /// Return `self.left` with the lead hint swapped for a shorter
    /// variant if the full form plus non-droppable right segments
    /// wouldn't fit. The "floor" right width is the first right segment
    /// only (typically the model name) — we want to protect that above
    /// the nice-to-have hint detail.
    fn narrowed_left_segments(&self, width: u16) -> Vec<Segment> {
        let Some(first) = self.left.first() else {
            return self.left.clone();
        };
        // Only degrade the known idle-hint strings. `Ctrl+C interrupt`
        // during an active turn is already short; leave untouched.
        if first.text != IDLE_HINT_FULL {
            return self.left.clone();
        }
        // Width of everything except the lead hint: rest of left segs +
        // separators, all of right (floor) + margins. The lead hint
        // itself is what we're shrinking, so exclude it from this sum.
        let sep_w = " · ".chars().count();
        let lead_indent = 2;
        let trailing_margin = 2;

        let other_left_w: usize = self
            .left
            .iter()
            .skip(1)
            .map(|s| s.text.chars().count() + sep_w)
            .sum();
        // Prefer preserving the current right-side context (model, branch,
        // cwd, budget) before spending width on a verbose legend.
        let right_desired: usize = self
            .right
            .iter()
            .enumerate()
            .map(|(idx, s)| s.text.chars().count() + if idx > 0 { sep_w } else { 0 })
            .sum();

        let overhead = lead_indent + other_left_w + right_desired + trailing_margin;
        let available = (width as usize).saturating_sub(overhead);

        let chosen = if IDLE_HINT_FULL.chars().count() <= available {
            IDLE_HINT_FULL
        } else if IDLE_HINT_SHORT.chars().count() <= available {
            IDLE_HINT_SHORT
        } else {
            IDLE_HINT_TINY
        };

        let mut out = self.left.clone();
        out[0] = Segment::styled(chosen, first.style);
        out
    }
}

fn join_segments(
    segments: &[Segment],
    sep: &Span<'_>,
    leading_spaces: usize,
) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(segments.len() * 2 + 1);
    if leading_spaces > 0 {
        spans.push(Span::raw(" ".repeat(leading_spaces)));
    }
    for (i, seg) in segments.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(sep.content.to_string(), sep.style));
        }
        spans.push(Span::styled(seg.text.clone(), seg.style));
    }
    spans
}

/// Shorten `cwd` to at most `max_width` characters by replacing the
/// middle with an ellipsis, keeping the meaningful tail visible.
fn truncate_cwd(cwd: &str, max_width: usize) -> String {
    let count = cwd.chars().count();
    if count <= max_width {
        return cwd.to_string();
    }
    // Keep the last `max_width - 1` characters, prefixed with "…".
    let tail: String = cwd.chars().skip(count - (max_width - 1)).collect();
    format!("…{tail}")
}

/// "25000" → "25k"; preserves exact count under 1k.
fn format_tokens_compact(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{}k", n / 1_000)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}
