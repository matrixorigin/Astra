//! Pure status-line composition.

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
    pub permission_mode: PermissionMode,
    pub git_branch: Option<String>,
    /// Number of approvals currently awaiting a user decision.
    pub pending_approvals: usize,
    /// `(open, total)` task counts for the footer task-board chip.
    /// `None` when the board has no tasks — chip hides rather than
    /// wasting space with `0/0`.
    pub task_counts: Option<(usize, usize)>,
    /// Whether the user has Ctrl+T-expanded the task board. The draw layer
    /// uses this to choose compact versus detailed task content.
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

/// Max cwd segment width before the middle is elided.
const MODEL_MAX_WIDTH: usize = 24;
const BRANCH_MAX_WIDTH: usize = 16;
const CWD_MAX_WIDTH: usize = 26;

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
            // Prompt is the safe default. Repeating it forever adds no new
            // information; modes that materially change execution remain
            // visible and colour-coded.
            PermissionMode::Prompt => {}
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
                        .fg(theme.error)
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

        // Task-board chip. Name the action instead of relying on an
        // unexplained disclosure glyph: task navigation is only useful when
        // users can discover it from the state they are looking at.
        if let Some((open, total)) = ctx.task_counts {
            if total > 0 {
                let (text, style) = if open == 0 {
                    (
                        format!("Tasks {total} done · Ctrl+T"),
                        Style::default().fg(theme.success),
                    )
                } else {
                    (format!("Tasks {open}/{total} · Ctrl+T"), muted)
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
                parts.push("Ctrl+T tasks".to_string());
            }
            if !counts.has_local_agent_rows() {
                parts
                    .push(crate::tui::background_shortcut::background_task_open_hint().to_string());
            }
            let style = if counts.failed_total() > 0 {
                Style::default().fg(theme.error)
            } else if counts.waiting > 0 {
                Style::default().fg(theme.warn)
            } else {
                muted
            };
            out.left.push(Segment::styled(parts.join(" · "), style));
        }

        // ── Right: quiet workspace identity ───────────────────────
        if let Some(cwd) = ctx.cwd.as_deref() {
            out.right.push(Segment::styled(
                truncate_cwd(cwd, CWD_MAX_WIDTH),
                Style::default().fg(theme.path_file),
            ));
        }

        if let Some(branch) = ctx.git_branch.as_deref() {
            out.right.push(Segment::styled(
                format!("⎇ {}", truncate_middle(branch, BRANCH_MAX_WIDTH)),
                muted,
            ));
        }

        out
    }

    /// Render to a simple string for testing. Colour carries grouping in the
    /// actual TUI; two spaces preserve that rhythm in plain text.
    pub fn plain(&self) -> String {
        ordered_render_segments(&self.left, &self.right)
            .into_iter()
            .map(|seg| seg.text)
            .collect::<Vec<_>>()
            .join("  ")
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
        const MARGIN: usize = 2;
        const INNER_GAP: &str = "  ";
        let available = usize::from(area.width).saturating_sub(MARGIN * 2);
        if available == 0 {
            return;
        }
        let mut left_segments = self.left.clone();
        let mut right_segments = self.right.clone();
        fit_clusters(&mut left_segments, &mut right_segments, available);

        let left_spans = join_segments(&left_segments, INNER_GAP, bg);
        let right_spans = join_segments(&right_segments, INNER_GAP, bg);
        let left_width = spans_width(&left_spans);
        let right_width = spans_width(&right_spans);

        if !left_spans.is_empty() {
            Widget::render(
                Line::from(left_spans),
                Rect::new(
                    area.x.saturating_add(MARGIN as u16),
                    area.y,
                    u16::try_from(left_width.min(available)).unwrap_or(area.width),
                    1,
                ),
                buf,
            );
        }
        if !right_spans.is_empty() {
            let right_x = area
                .x
                .saturating_add(area.width)
                .saturating_sub(MARGIN as u16)
                .saturating_sub(u16::try_from(right_width).unwrap_or(area.width));
            Widget::render(
                Line::from(right_spans),
                Rect::new(
                    right_x,
                    area.y,
                    u16::try_from(right_width.min(available)).unwrap_or(area.width),
                    1,
                ),
                buf,
            );
        }
    }
}

fn join_segments(segments: &[Segment], separator: &str, bg: Color) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(segments.len() * 2);
    for (i, seg) in segments.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(separator.to_string(), Style::default().bg(bg)));
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

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

fn cluster_width(segments: &[Segment]) -> usize {
    segments
        .iter()
        .map(|segment| segment.text.width())
        .sum::<usize>()
        + segments.len().saturating_sub(1) * 2
}

fn fit_clusters(left: &mut Vec<Segment>, right: &mut Vec<Segment>, available: usize) {
    const CLUSTER_GAP: usize = 3;
    let total = |left: &[Segment], right: &[Segment]| {
        cluster_width(left)
            + cluster_width(right)
            + usize::from(!left.is_empty() && !right.is_empty()) * CLUSTER_GAP
    };

    // Branch is decoration; cwd is the primary workspace anchor.
    while right.len() > 1 && total(left, right) > available {
        right.pop();
    }
    // Secondary left chips yield before model identity on very narrow screens.
    while left.len() > 1 && total(left, right) > available {
        left.pop();
    }
    if total(left, right) > available && !right.is_empty() {
        if left.is_empty() {
            right[0].text = truncate_end(&right[0].text, available.max(1));
            right.truncate(1);
        } else {
            right.clear();
        }
    }
    if total(left, right) > available
        && let Some(primary) = left.first_mut()
    {
        primary.text = truncate_end(&primary.text, available.max(1));
        left.truncate(1);
    }
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
