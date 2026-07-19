//! Pure status-line composition.

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
    /// One request's usable context-window occupancy.
    pub context_window: Option<astra_turn_types::ContextWindowUsage>,
    /// The displayed occupancy belongs to the preceding completed request
    /// while the next request is still being assembled. Keeping it visible
    /// avoids a false "unknown/zero" gap, while this bit prevents presenting
    /// stale evidence as the current request.
    pub context_window_is_previous: bool,
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
    /// Counts of the BackgroundTaskRegistry states that need user
    /// visibility. `None` when the registry has no live/attention
    /// state to surface.
    pub bg_task_counts: Option<BackgroundTaskCounts>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct BackgroundTaskCounts {
    /// Running shell tasks.
    pub running: usize,
    /// Tasks with an accepted cancellation request awaiting a terminal result.
    pub stopping: usize,
    /// Shell tasks waiting for input.
    pub waiting: usize,
    /// Last-observed task snapshots without a live runtime handle.
    pub stale_snapshots: usize,
    /// Failed typed tasks needing attention.
    pub failed_shells: usize,
    pub failed_local_agents: usize,
    pub failed_cloud_sessions: usize,
    pub failed_main_sessions: usize,
    pub failed_monitors: usize,
    pub unavailable_shells: usize,
    pub unavailable_local_agents: usize,
    pub unavailable_cloud_sessions: usize,
    pub unavailable_main_sessions: usize,
    pub unavailable_monitors: usize,
    pub local_agents: usize,
    pub cloud_sessions: usize,
    pub main_sessions: usize,
    pub monitors: usize,
}

impl BackgroundTaskCounts {
    pub(crate) fn failed_total(self) -> usize {
        self.failed_shells
            + self.failed_local_agents
            + self.failed_cloud_sessions
            + self.failed_main_sessions
            + self.failed_monitors
    }

    pub(crate) fn unavailable_total(self) -> usize {
        self.unavailable_shells
            + self.unavailable_local_agents
            + self.unavailable_cloud_sessions
            + self.unavailable_main_sessions
            + self.unavailable_monitors
    }

    pub(crate) fn is_empty(self) -> bool {
        self.running == 0
            && self.stopping == 0
            && self.waiting == 0
            && self.stale_snapshots == 0
            && self.failed_total() == 0
            && self.unavailable_total() == 0
            && self.local_agents == 0
            && self.cloud_sessions == 0
            && self.main_sessions == 0
            && self.monitors == 0
    }

    pub(crate) fn has_local_agent_rows(self) -> bool {
        self.local_agents > 0 || self.failed_local_agents > 0 || self.unavailable_local_agents > 0
    }

    pub(crate) fn from_rows(
        rows: &[crate::tui::bottom_pane::background_task_view::BackgroundTaskRow],
    ) -> Self {
        use crate::tui::bottom_pane::background_task_view::{
            BackgroundTaskKind, BackgroundTaskStatus, LiveControlState,
        };
        let mut counts = Self::default();
        for row in rows {
            if matches!(row.live_control, LiveControlState::StaleHandle) {
                counts.stale_snapshots += 1;
            }
            match row.status {
                BackgroundTaskStatus::Running | BackgroundTaskStatus::Pending => match row.kind {
                    BackgroundTaskKind::Shell => counts.running += 1,
                    BackgroundTaskKind::LocalAgent => counts.local_agents += 1,
                    BackgroundTaskKind::CloudSession => counts.cloud_sessions += 1,
                    BackgroundTaskKind::MainSession => counts.main_sessions += 1,
                    BackgroundTaskKind::Monitor => counts.monitors += 1,
                },
                BackgroundTaskStatus::WaitingForInput => counts.waiting += 1,
                BackgroundTaskStatus::Stopping => counts.stopping += 1,
                BackgroundTaskStatus::Interrupted | BackgroundTaskStatus::Failed => {
                    match row.kind {
                        BackgroundTaskKind::Shell => counts.failed_shells += 1,
                        BackgroundTaskKind::LocalAgent => counts.failed_local_agents += 1,
                        BackgroundTaskKind::CloudSession => counts.failed_cloud_sessions += 1,
                        BackgroundTaskKind::MainSession => counts.failed_main_sessions += 1,
                        BackgroundTaskKind::Monitor => counts.failed_monitors += 1,
                    }
                }
                BackgroundTaskStatus::Unavailable => match row.kind {
                    BackgroundTaskKind::Shell => counts.unavailable_shells += 1,
                    BackgroundTaskKind::LocalAgent => counts.unavailable_local_agents += 1,
                    BackgroundTaskKind::CloudSession => counts.unavailable_cloud_sessions += 1,
                    BackgroundTaskKind::MainSession => counts.unavailable_main_sessions += 1,
                    BackgroundTaskKind::Monitor => counts.unavailable_monitors += 1,
                },
                BackgroundTaskStatus::Cancelled
                | BackgroundTaskStatus::Completed
                | BackgroundTaskStatus::CompletedWithIssues => {}
            }
        }
        counts
    }
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

fn pluralize_with_count(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

fn background_task_count_parts(counts: BackgroundTaskCounts) -> Vec<String> {
    let mut parts = Vec::new();
    for (count, singular, plural) in [
        (counts.failed_shells, "shell", "shells"),
        (counts.failed_local_agents, "local agent", "local agents"),
        (
            counts.failed_cloud_sessions,
            "cloud session",
            "cloud sessions",
        ),
        (counts.failed_main_sessions, "main session", "main sessions"),
        (counts.failed_monitors, "monitor", "monitors"),
    ] {
        if count > 0 {
            parts.push(format!(
                "{} failed",
                pluralize_with_count(count, singular, plural)
            ));
        }
    }
    if counts.waiting > 0 {
        parts.push(pluralize_with_count(
            counts.waiting,
            "needs input",
            "need input",
        ));
    }
    if counts.stopping > 0 {
        parts.push(format!("{} stopping", counts.stopping));
    }
    if counts.stale_snapshots > 0 {
        parts.push(pluralize_with_count(
            counts.stale_snapshots,
            "stale snapshot",
            "stale snapshots",
        ));
    }
    for (count, singular, plural) in [
        (counts.unavailable_shells, "shell", "shells"),
        (
            counts.unavailable_local_agents,
            "local agent",
            "local agents",
        ),
        (
            counts.unavailable_cloud_sessions,
            "cloud session",
            "cloud sessions",
        ),
        (
            counts.unavailable_main_sessions,
            "main session",
            "main sessions",
        ),
        (counts.unavailable_monitors, "monitor", "monitors"),
    ] {
        if count > 0 {
            parts.push(format!(
                "{} unavailable",
                pluralize_with_count(count, singular, plural)
            ));
        }
    }

    let kind_counts = [
        (counts.running, "shell", "shells"),
        (counts.local_agents, "local agent", "local agents"),
        (counts.cloud_sessions, "cloud session", "cloud sessions"),
        (counts.main_sessions, "main session", "main sessions"),
        (counts.monitors, "monitor", "monitors"),
    ];
    let visible_kinds: Vec<_> = kind_counts
        .into_iter()
        .filter(|(count, _, _)| *count > 0)
        .collect();
    if visible_kinds.len() >= 3 {
        let total = visible_kinds
            .iter()
            .map(|(count, _, _)| *count)
            .sum::<usize>();
        parts.push(pluralize_with_count(
            total,
            "background task",
            "background tasks",
        ));
    } else {
        parts.extend(
            visible_kinds
                .into_iter()
                .map(|(count, singular, plural)| pluralize_with_count(count, singular, plural)),
        );
    }

    parts
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
    mode.chip_text()
}

impl StatusLine {
    /// Build a status line from context.
    pub fn from_context(ctx: &StatusContext) -> Self {
        let theme = crate::tui::theme::current();
        let muted = Style::default().fg(theme.dim);
        let mut out = Self::default();

        // ── Left: stable agent context, not a second live-status bar ─
        if let Some(model) = ctx.model.as_deref() {
            out.left.push(Segment::styled(
                format_model_label(model, MODEL_MAX_WIDTH),
                Style::default().fg(theme.path_file),
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
                    Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
                ));
            }
            PermissionMode::Bypass => {
                out.left.push(Segment::styled(
                    permission_mode_label(ctx.permission_mode),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            PermissionMode::Plan => {
                out.left.push(Segment::styled(
                    permission_mode_label(ctx.permission_mode),
                    Style::default()
                        .fg(theme.command)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            PermissionMode::AcceptEdits => {
                out.left.push(Segment::styled(
                    permission_mode_label(ctx.permission_mode),
                    Style::default().fg(theme.link).add_modifier(Modifier::BOLD),
                ));
            }
            PermissionMode::Deny => {
                out.left.push(Segment::styled(
                    permission_mode_label(ctx.permission_mode),
                    Style::default()
                        .fg(theme.error)
                        .add_modifier(Modifier::BOLD),
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
                .push(Segment::styled(text, Style::default().fg(theme.warn)));
        }

        // Task-board chip. `▶` = collapsed (Ctrl+T to expand), `▼` =
        // expanded. The observer reports lifecycle progress while work is
        // open and removes the chip once only terminal history remains.
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
                        Style::default().fg(theme.success),
                    )
                } else {
                    (format!("{glyph} {open}/{total}"), muted)
                };
                out.left.push(Segment::styled(text, style));
            }
        }

        // BackgroundTaskRegistry chip. Running tasks are informational;
        // needs-input/failed tasks are attention states and stay visible
        // even after no shell is making forward progress. Fanout title,
        // target accounting, elapsed time, and slot detail belong in the
        // Shift+Down task surface. Repeating them here made one work unit look
        // like two unrelated status systems (for example a long group summary
        // followed by "2 local agents"). Keep the footer as a compact
        // discoverability pill, matching the user's background-task mental
        // model.
        if let Some(counts) = ctx.bg_task_counts
            && !counts.is_empty()
        {
            let mut parts = background_task_count_parts(counts);
            if counts.has_local_agent_rows() {
                parts
                    .push(crate::tui::background_shortcut::agent_workbench_open_hint().to_string());
            }
            parts.push(crate::tui::background_shortcut::background_task_open_hint().to_string());
            let style = if counts.failed_total() > 0 {
                Style::default().fg(theme.error)
            } else if counts.waiting > 0 {
                Style::default().fg(theme.warn)
            } else {
                muted
            };
            out.left.push(Segment::styled(parts.join(" · "), style));
        }

        // ── Right: cwd · budget · branch · cost ────────────────────
        if let Some(cwd) = ctx.cwd.as_deref() {
            out.right.push(Segment::styled(
                truncate_cwd(cwd, CWD_MAX_WIDTH),
                Style::default().fg(theme.path_file),
            ));
        }

        if let Some(usage) = ctx.context_window {
            if usage.limit_tokens > 0 {
                let pct = (usage.used_tokens as f32 / usage.limit_tokens as f32) * 100.0;
                let style = if pct >= BUDGET_ERROR_PERCENT {
                    Style::default().fg(theme.error)
                } else if pct >= BUDGET_WARN_PERCENT {
                    Style::default().fg(theme.warn)
                } else {
                    muted
                };
                let approximation = matches!(
                    usage.source,
                    astra_turn_types::ContextWindowUsageSource::Estimated
                )
                .then_some("~")
                .unwrap_or("");
                let scope = if ctx.context_window_is_previous {
                    "last "
                } else {
                    ""
                };
                out.right.push(Segment::styled(
                    format!(
                        "Ctx {scope}{approximation}{pct:.0}% · {}/{}",
                        format_tokens_compact(usage.used_tokens),
                        format_tokens_compact(usage.limit_tokens),
                    ),
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
        let sep = Span::styled(
            " · ",
            Style::default().fg(crate::tui::theme::current().dim).bg(bg),
        );

        let mut right_segments = self.right.clone();
        while right_segments.len() > 1
            && status_line_total_width(&self.left, &right_segments) > usize::from(area.width)
        {
            // Context capacity is operational state. Drop optional decoration
            // by meaning, not by its incidental position in the vector.
            let remove_index = right_segments
                .iter()
                .rposition(|segment| segment.text.starts_with('$'))
                .or_else(|| {
                    right_segments
                        .iter()
                        .rposition(|segment| segment.text.starts_with("⎇ "))
                })
                .or_else(|| {
                    right_segments
                        .iter()
                        .position(|segment| !segment.text.starts_with("Ctx "))
                });
            let Some(remove_index) = remove_index else {
                break;
            };
            right_segments.remove(remove_index);
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
            if compact_last_context_segment(&mut ordered, area.width) {
                continue;
            }
            if ordered
                .last()
                .is_some_and(|segment| segment.text.starts_with("Ctx "))
                && ordered.len() > 2
            {
                // Keep the primary left identity and capacity signal; shed a
                // secondary left chip before discarding context altogether.
                ordered.remove(ordered.len() - 2);
                continue;
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
    if ctx.context_window.is_some() {
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

fn status_line_total_width(left: &[Segment], right: &[Segment]) -> usize {
    let segments = left.len() + right.len();
    if segments == 0 {
        return 0;
    }
    let segment_width: usize = left
        .iter()
        .chain(right.iter())
        .map(|seg| seg.text.width())
        .sum();
    let separator_width = segments.saturating_sub(1) * " · ".width();
    2 + segment_width + separator_width + 2
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
    if left.is_empty() || right.is_empty() || right[0].text.starts_with("Ctx ") {
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

fn compact_last_context_segment(ordered: &mut [Segment], total_width: u16) -> bool {
    let Some(last_text) = ordered.last().map(|seg| seg.text.as_str()) else {
        return false;
    };
    if !last_text.starts_with("Ctx ") {
        return false;
    }

    let sep_w = " · ".width();
    let lead_indent = 2usize;
    let trailing_margin = 2usize;
    let fixed_width: usize = ordered
        .iter()
        .take(ordered.len().saturating_sub(1))
        .map(|seg| seg.text.width())
        .sum::<usize>()
        + ordered.len().saturating_sub(1) * sep_w
        + lead_indent
        + trailing_margin;
    let available = usize::from(total_width).saturating_sub(fixed_width);
    if last_text.width() <= available {
        return false;
    }

    let compact = compact_context_text(last_text, available);
    if compact == last_text {
        return false;
    }
    if let Some(last) = ordered.last_mut() {
        last.text = compact;
    }
    true
}

fn compact_context_text(text: &str, max_width: usize) -> String {
    if text.width() <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }

    let Some((prefix, tokens)) = text.rsplit_once(" · ") else {
        return truncate_end(text, max_width);
    };
    let limit = tokens
        .rsplit_once('/')
        .map(|(_, limit)| limit)
        .unwrap_or(tokens);
    let limit_only = format!("…/{limit}");
    if limit_only.width() > max_width {
        return truncate_end(&limit_only, max_width);
    }

    let with_prefix = format!("{prefix} · {limit_only}");
    if with_prefix.width() <= max_width {
        with_prefix
    } else {
        limit_only
    }
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

/// "25000" → "25k"; preserves exact count under 1k.
fn format_tokens_compact(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        let rounded_k = (n + 500) / 1_000;
        if rounded_k >= 1_000 {
            "1.0M".to_string()
        } else {
            format!("{rounded_k}k")
        }
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}
