//! Pure status-line composition.

#![allow(dead_code)]

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

pub(crate) use crate::cli::permission_manager::PermissionMode;

/// Inputs that feed the status line. Owned so the struct is `Clone`
/// and easy to fixture.
#[derive(Debug, Clone, Default)]
pub(crate) struct StatusContext {
    pub model: Option<String>,
    pub cwd: Option<String>,
    /// Tuple `(used, limit)` in tokens.
    pub token_budget: Option<(u64, u64)>,
    pub current_objective: Option<String>,
    pub turn_elapsed: Option<Duration>,
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

/// Threshold below which the budget chip is dim, above which it warns.
const BUDGET_WARN_PERCENT: f32 = 75.0;
const BUDGET_ERROR_PERCENT: f32 = 90.0;
/// Max cwd segment width before the middle is elided.
const MODEL_MAX_WIDTH: usize = 22;
const BRANCH_MAX_WIDTH: usize = 20;
const CWD_MAX_WIDTH: usize = 24;

fn permission_mode_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Prompt => "Ask",
        PermissionMode::Auto => "Auto",
        PermissionMode::Plan => "Plan",
        PermissionMode::AcceptEdits => "Edits",
        PermissionMode::Deny => "Deny",
    }
}

impl StatusLine {
    /// Build a status line from context.
    pub fn from_context(ctx: &StatusContext) -> Self {
        let muted = Style::default().fg(Color::Gray);
        let mut out = Self::default();

        // ── Left: objective when active, otherwise just concise state ─
        let active_objective = ctx
            .turn_active
            .then(|| ctx.current_objective.clone())
            .flatten();
        let active_hint_style = Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);
        if let Some(objective) = active_objective {
            out.left.push(Segment::styled(objective, active_hint_style));
        }

        if ctx.turn_active
            && let Some(elapsed) = ctx.turn_elapsed
        {
            out.left
                .push(Segment::styled(format_duration_compact(elapsed), muted));
        }

        match ctx.permission_mode {
            PermissionMode::Prompt => {
                out.left.push(Segment::styled(
                    permission_mode_label(ctx.permission_mode),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            PermissionMode::Auto => {
                out.left.push(Segment::styled(
                    permission_mode_label(ctx.permission_mode),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            PermissionMode::Plan => {
                out.left.push(Segment::styled(
                    permission_mode_label(ctx.permission_mode),
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            PermissionMode::AcceptEdits => {
                out.left.push(Segment::styled(
                    permission_mode_label(ctx.permission_mode),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            PermissionMode::Deny => {
                out.left.push(Segment::styled(
                    permission_mode_label(ctx.permission_mode),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
            }
        }

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

        // ── Right: model · budget · cwd · branch · cost ───────────
        if let Some(model) = ctx.model.as_deref() {
            out.right.push(Segment::styled(
                truncate_middle(model, MODEL_MAX_WIDTH),
                Style::default().fg(Color::White),
            ));
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

        if let Some(cwd) = ctx.cwd.as_deref() {
            out.right
                .push(Segment::styled(truncate_cwd(cwd, CWD_MAX_WIDTH), muted));
        }

        if let Some(branch) = ctx.git_branch.as_deref() {
            out.right.push(Segment::styled(
                format!("⎇ {}", truncate_middle(branch, BRANCH_MAX_WIDTH)),
                muted,
            ));
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
        let surface = crate::tui::style::composer_surface_style();
        let bg = surface.bg.unwrap_or(Color::Reset);
        let sep = Span::styled(" · ", Style::default().fg(Color::Gray).bg(bg));

        let mut right_segments: &[Segment] = &self.right;
        let mut right_spans = join_segments(right_segments, &sep, 0, bg);

        loop {
            let right_w: usize = right_spans.iter().map(|s| s.content.width()).sum();
            let left_segments = truncate_left_segments_to_fit(&self.left, right_w, area.width);
            let left_spans = join_segments(&left_segments, &sep, 2 /* leading indent */, bg);
            let left_w: usize = left_spans.iter().map(|s| s.content.width()).sum();
            let total = left_w + right_w + 2; // 2-char trailing margin
            if total <= area.width as usize || right_segments.is_empty() {
                let padding = (area.width as usize).saturating_sub(left_w + right_w + 2);
                let mut all = left_spans;
                all.push(Span::styled(" ".repeat(padding), Style::default().bg(bg)));
                all.extend(right_spans);
                Widget::render(Line::from(all), area, buf);
                break;
            }
            // Drop the trailing right segment and retry.
            right_segments = &right_segments[..right_segments.len() - 1];
            right_spans = join_segments(right_segments, &sep, 0, bg);
        }
    }
}

fn join_segments(
    segments: &[Segment],
    sep: &Span<'_>,
    leading_spaces: usize,
    bg: Color,
) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(segments.len() * 2 + 1);
    if leading_spaces > 0 {
        spans.push(Span::styled(
            " ".repeat(leading_spaces),
            Style::default().bg(bg),
        ));
    }
    for (i, seg) in segments.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(sep.content.to_string(), sep.style));
        }
        spans.push(Span::styled(seg.text.clone(), seg.style.bg(bg)));
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

fn truncate_middle(text: &str, max_width: usize) -> String {
    let count = text.chars().count();
    if count <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let head_len = (max_width - 1) / 2;
    let tail_len = max_width - 1 - head_len;
    let head: String = text.chars().take(head_len).collect();
    let tail: String = text.chars().skip(count - tail_len).collect();
    format!("{head}…{tail}")
}

fn truncate_end(text: &str, max_width: usize) -> String {
    let count = text.chars().count();
    if count <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let head: String = text.chars().take(max_width - 1).collect();
    format!("{head}…")
}

fn truncate_left_segments_to_fit(
    segments: &[Segment],
    right_width: usize,
    total_width: u16,
) -> Vec<Segment> {
    if segments.is_empty() {
        return Vec::new();
    }

    let sep_w = " · ".width();
    let lead_indent = 2usize;
    let trailing_margin = 2usize;
    let other_width: usize = segments
        .iter()
        .skip(1)
        .map(|seg| seg.text.width() + sep_w)
        .sum();
    let available = usize::from(total_width)
        .saturating_sub(right_width + lead_indent + trailing_margin + other_width);

    if segments[0].text.width() <= available || available >= segments[0].text.width() {
        return segments.to_vec();
    }

    let mut out = segments.to_vec();
    let floor = if segments.len() == 1 { 10 } else { 14 };
    let target = available.max(floor);
    out[0].text = truncate_end(&out[0].text, target);
    out
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

fn format_duration_compact(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}
