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
const MODEL_MAX_WIDTH: usize = 24;
const BRANCH_MAX_WIDTH: usize = 16;
const CWD_MAX_WIDTH: usize = 26;
const PRIMARY_LEFT_FLOOR_WIDTH: usize = 14;

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

        // ── Left: stable agent context, not a second live-status bar ─
        if let Some(model) = ctx.model.as_deref() {
            out.left.push(Segment::styled(
                format_model_label(model, MODEL_MAX_WIDTH),
                Style::default().fg(Color::White),
            ));
        }

        // Permission mode changes whether tools run automatically or ask first.
        // Keep it visible so `/mode` feedback matches the persistent status line.
        match ctx.permission_mode {
            PermissionMode::Prompt => {
                out.left.push(Segment::styled(
                    permission_mode_label(ctx.permission_mode),
                    muted.add_modifier(Modifier::BOLD),
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
                let mut text = if running == 1 {
                    "1 background job".to_string()
                } else {
                    format!("{running} background jobs")
                };
                if stalled > 0 {
                    text.push_str(&format!(" · {stalled} waiting"));
                }
                let style = if stalled > 0 {
                    Style::default().fg(Color::Yellow)
                } else {
                    muted
                };
                out.left.push(Segment::styled(text, style));
            }
        }

        // ── Right: cwd · budget · branch · cost ────────────────────
        if let Some(cwd) = ctx.cwd.as_deref() {
            out.right.push(Segment::styled(
                truncate_cwd(cwd, CWD_MAX_WIDTH),
                Style::default().fg(Color::Green),
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

        if let Some(branch) = ctx.git_branch.as_deref() {
            out.right.push(Segment::styled(
                format!("⎇ {}", truncate_middle(branch, BRANCH_MAX_WIDTH)),
                muted,
            ));
        }

        if should_render_cost(ctx) {
            let cost = ctx.cost_usd.expect("cost checked above");
            out.right
                .push(Segment::styled(format!("${cost:.2}"), muted));
        }

        out
    }

    /// Render to a simple string for testing: left joined by ' · ',
    /// two-space gap, right joined by ' · '.
    pub fn plain(&self) -> String {
        ordered_render_segments(&self.left, &self.right)
            .into_iter()
            .map(|seg| seg.text)
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// Draw into `area` of `buf`. Left side sticks to the left edge; right
    /// side is right-aligned. When the terminal is too narrow for both,
    /// right-side segments are dropped one at a time (tail first) until
    /// the line fits.
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let surface = crate::tui::style::footer_surface_style();
        let bg = surface.bg.unwrap_or(Color::Reset);
        let sep = Span::styled(" · ", Style::default().fg(Color::Gray).bg(bg));

        let mut right_segments = self.right.clone();
        if !should_render_budget_segment(area.width, &self.left, &right_segments)
            && right_segments.len() >= 2
        {
            right_segments.remove(1);
        }

        tighten_primary_right_segment(&self.left, &mut right_segments, area.width);
        let mut ordered = ordered_render_segments(&self.left, &right_segments);
        loop {
            let spans = join_segments(&ordered, &sep, 2 /* leading indent */, bg);
            let used: usize = spans.iter().map(|s| s.content.width()).sum();
            let total = used + 2; // trailing margin
            if total <= area.width as usize || ordered.len() <= 1 {
                let padding = (area.width as usize).saturating_sub(used + 2);
                let mut all = spans;
                all.push(Span::styled(" ".repeat(padding), Style::default().bg(bg)));
                Widget::render(Line::from(all), area, buf);
                break;
            }
            ordered.pop();
        }
    }
}

fn should_render_cost(ctx: &StatusContext) -> bool {
    ctx.cost_usd.is_some() && !is_dense_footer_context(ctx)
}

fn is_dense_footer_context(ctx: &StatusContext) -> bool {
    let mut signals = 0usize;
    if ctx.token_budget.is_some() {
        signals += 1;
    }
    if ctx.git_branch.is_some() {
        signals += 1;
    }
    if ctx.cost_usd.is_some() {
        signals += 1;
    }
    signals >= 2
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

fn ordered_render_segments(left: &[Segment], right: &[Segment]) -> Vec<Segment> {
    let mut ordered = Vec::with_capacity(left.len() + right.len());
    ordered.extend(left.iter().cloned());
    ordered.extend(right.iter().cloned());
    ordered
}

fn tighten_ordered_cwd_segment(ordered: &mut [Segment], total_width: u16) {
    if ordered.len() < 2 {
        return;
    }

    let sep_w = " · ".width();
    let lead_indent = 2usize;
    let trailing_margin = 2usize;
    let model_width = ordered[0].text.width();
    let other_width: usize = ordered
        .iter()
        .skip(2)
        .map(|seg| seg.text.width() + sep_w)
        .sum();
    let separator_count = ordered.len().saturating_sub(1);
    let separator_width = separator_count * sep_w;
    let available = usize::from(total_width)
        .saturating_sub(lead_indent + trailing_margin + model_width + other_width + separator_width)
        .max(8);

    if ordered[1].text.width() > available {
        ordered[1].text = truncate_cwd(&ordered[1].text, available);
    }
}

fn tighten_primary_right_segment(left: &[Segment], right: &mut [Segment], total_width: u16) {
    if left.is_empty() || right.is_empty() {
        return;
    }

    let sep_w = " · ".width();
    let lead_indent = 2usize;
    let trailing_margin = 2usize;
    let preferred_left = left[0]
        .text
        .width()
        .min(MODEL_MAX_WIDTH.max(PRIMARY_LEFT_FLOOR_WIDTH));
    let other_right_width: usize = right
        .iter()
        .skip(1)
        .map(|seg| seg.text.width() + sep_w)
        .sum();
    let available_for_primary_right = usize::from(total_width)
        .saturating_sub(lead_indent + trailing_margin + preferred_left + other_right_width);

    if right[0].text.width() <= available_for_primary_right {
        return;
    }

    let max_width = available_for_primary_right.max(8);
    right[0].text = truncate_cwd(&right[0].text, max_width);
}

/// Shorten `cwd` to at most `max_width` characters by replacing the
/// middle with an ellipsis, keeping the meaningful tail visible.
fn truncate_cwd(cwd: &str, max_width: usize) -> String {
    if cwd.width() <= max_width {
        return cwd.to_string();
    }
    if max_width <= 3 {
        return "…".to_string();
    }

    let (lead, rest) = if let Some(stripped) = cwd.strip_prefix("~/") {
        ("~/", stripped)
    } else if let Some(stripped) = cwd.strip_prefix('/') {
        ("/", stripped)
    } else {
        ("", cwd)
    };

    let parts: Vec<&str> = rest.split('/').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        return truncate_end(cwd, max_width);
    }

    let mut best: Option<String> = None;
    for keep_count in 1..=parts.len() {
        let tail = parts[parts.len() - keep_count..].join("/");
        let candidate = if keep_count < parts.len() {
            format!("{lead}…/{tail}")
        } else {
            format!("{lead}{tail}")
        };
        if candidate.width() > max_width {
            break;
        }
        best = Some(candidate);
    }

    best.unwrap_or_else(|| truncate_end(cwd, max_width))
}

fn format_model_label(model: &str, max_width: usize) -> String {
    if model.width() <= max_width {
        return model.to_string();
    }

    if let Some((base, suffix)) = split_model_suffix(model) {
        let compact_suffix = compact_model_suffix(suffix);
        let suffix_width = compact_suffix.width();
        if suffix_width < max_width {
            let base_budget = max_width.saturating_sub(suffix_width);
            let truncated_base = truncate_end(base, base_budget.max(8));
            let candidate = format!("{truncated_base}{compact_suffix}");
            if candidate.width() <= max_width {
                return candidate;
            }
        }
        return truncate_end(base, max_width);
    }

    truncate_end(model, max_width)
}

fn split_model_suffix(model: &str) -> Option<(&str, &str)> {
    if !model.ends_with(')') {
        return None;
    }
    let start = model.rfind('(')?;
    let base = &model[..start];
    if base.is_empty() {
        return None;
    }
    Some((base, &model[start..]))
}

fn compact_model_suffix(suffix: &str) -> String {
    let inner = suffix.trim_start_matches('(').trim_end_matches(')');
    if let Some(level) = inner.strip_prefix("thinking:") {
        format!("({level})")
    } else {
        suffix.to_string()
    }
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
    let mut kept = segments.to_vec();

    loop {
        let other_width: usize = kept
            .iter()
            .skip(1)
            .map(|seg| seg.text.width() + sep_w)
            .sum();
        let available = usize::from(total_width)
            .saturating_sub(right_width + lead_indent + trailing_margin + other_width);

        if kept[0].text.width() <= available {
            return kept;
        }

        if kept.len() > 1 {
            kept.pop();
            continue;
        }

        let floor = 10usize;
        kept[0].text = truncate_end(&kept[0].text, available.max(floor));
        return kept;
    }
}

fn should_render_budget_segment(total_width: u16, left: &[Segment], right: &[Segment]) -> bool {
    if right.len() < 2 {
        return true;
    }
    let left_primary = left.first().map(|seg| seg.text.width()).unwrap_or_default();
    let cwd_width = right
        .first()
        .map(|seg| seg.text.width())
        .unwrap_or_default();
    let budget_width = right.get(1).map(|seg| seg.text.width()).unwrap_or_default();
    let minimum_useful = 2 + left_primary + 2 + cwd_width.min(14) + 3 + budget_width;
    usize::from(total_width) >= minimum_useful
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
