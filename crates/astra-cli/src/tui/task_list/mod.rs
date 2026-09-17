//! TUI rendering for the Tier 1 session task board.
//!
//! Renders the current session's canonical Work Task Graph projection.
//!
//! Rendering rules:
//!
//! - **Visibility**: hidden when `rows <= 10`.
//! - **maxDisplay**: `min(10, max(3, rows - 14))`.
//! - **Icons by status**: `✓` (completed, green), `■` (in_progress,
//!   accent), `□` (pending, dim).
//! - **Subject style**: bold for in_progress, strikethrough for
//!   completed, dim for completed or blocked.
//! - **Blocked-by badge**: appended `· waiting on #1, #3` when the task
//!   has any unresolved blockers.
//! - **Standalone header**: optional `Tasks · K working · M queued
//!   · J done` line above the list.
//! - **Truncation** when `tasks.len() > maxDisplay`: prioritize
//!   in-progress → pending (blocked last within pending) → paused →
//!   completed → terminal diagnostics;
//!   append `· N more: M working, K queued, J done` when any tasks
//!   are hidden. Recent-completed TTL state lives in the observer.
//! - **Responsive subject truncation** gated behind available columns.

use super::work_board_projection::{
    SessionTask, SessionTaskStatusKind, unresolved_task_blocker_ids,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::{HashMap, HashSet};
use unicode_width::UnicodeWidthStr;

const TASK_BOARD_TOGGLE_HINT: &str = " · Ctrl+T toggle";

/// Colour triple the widget reads for all status rendering. Built from
/// `tui::theme::current()` at render time so light and dark terminals
/// get readable palettes instead of hardcoded ANSI Cyan/Green.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TaskBoardColors {
    /// In-progress icon foreground (mirrors `theme.accent`).
    pub accent: Color,
    /// Completed icon foreground (mirrors `theme.success`).
    pub success: Color,
    /// Dim foreground used for pending icons, blocked-by badges, and
    /// the hidden-summary line (mirrors `theme.dim`).
    pub dim: Color,
    /// Paused/cancelled icon foreground (mirrors `theme.warn`).
    pub warn: Color,
    /// Failed icon foreground (mirrors `theme.error`).
    pub error: Color,
}

/// Full task-list render metadata for canvases that own keyboard focus and
/// scrolling. The row identity is carried structurally from `SessionTask.id`;
/// callers never recover it by searching rendered text.
pub(crate) struct TaskListRender {
    pub lines: Vec<Line<'static>>,
    pub selected_line_index: Option<usize>,
}

impl TaskBoardColors {
    /// Build from the process-wide theme.
    pub fn from_theme() -> Self {
        let t = crate::tui::theme::current();
        Self {
            accent: t.accent,
            success: t.success,
            dim: t.dim,
            warn: t.warn,
            error: t.error,
        }
    }

    /// Build from a specific theme preset — used by snapshot tests to
    /// pin the light/dark palette without mutating `OnceLock` state.
    #[cfg(test)]
    pub fn from_preset(theme: &crate::tui::theme::Theme) -> Self {
        Self {
            accent: theme.accent,
            success: theme.success,
            dim: theme.dim,
            warn: theme.warn,
            error: theme.error,
        }
    }
}

/// Per-status palette. Glyphs mirror the reference TUI's `figures`
/// library (tick / squareSmallFilled / squareSmall) so users coming
/// from that interface read the list the same way.
fn status_icon_and_color(
    status: &SessionTaskStatusKind,
    colors: TaskBoardColors,
) -> (&'static str, Color) {
    match status {
        SessionTaskStatusKind::Completed => ("✔", colors.success),
        SessionTaskStatusKind::InProgress => ("•", colors.accent),
        SessionTaskStatusKind::Paused => ("⏸", colors.warn),
        SessionTaskStatusKind::Pending | SessionTaskStatusKind::Other => ("◻", colors.dim),
        SessionTaskStatusKind::Failed => ("✖", colors.error),
        SessionTaskStatusKind::Cancelled => ("■", colors.warn),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkTaskPresentation {
    Planned,
    Working,
    Waiting,
    Paused,
    Executed,
    OutcomeUnreported,
    Blocked,
    Verified,
    NeedsRecheck,
    CheckFailed,
    Failed,
    Cancelled,
    Replaced,
}

impl WorkTaskPresentation {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Planned => "Planned",
            Self::Working => "Working",
            Self::Waiting => "Waiting",
            Self::Paused => "Paused",
            // A delivered result is complete from the user's perspective.
            // Verification evidence is useful provenance, but its absence is
            // not a request to resume or repair this task.
            Self::Executed => "Completed",
            Self::OutcomeUnreported => "Result not reported",
            Self::Blocked => "Blocked",
            Self::Verified => "Verified",
            Self::NeedsRecheck => "Needs recheck",
            Self::CheckFailed => "Check failed",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
            Self::Replaced => "Replaced",
        }
    }

    const fn board_status(self) -> SessionTaskStatusKind {
        match self {
            Self::Planned => SessionTaskStatusKind::Pending,
            Self::Working => SessionTaskStatusKind::InProgress,
            Self::Waiting
            | Self::Paused
            | Self::OutcomeUnreported
            | Self::Blocked
            | Self::NeedsRecheck => SessionTaskStatusKind::Paused,
            Self::Executed | Self::Verified => SessionTaskStatusKind::Completed,
            Self::CheckFailed | Self::Failed => SessionTaskStatusKind::Failed,
            Self::Cancelled | Self::Replaced => SessionTaskStatusKind::Cancelled,
        }
    }
}

/// Derive display state only from the canonical Work row namespace and the
/// typed graph projection. Titles, descriptions, transcript text, and error
/// strings never participate in task-state inference.
pub(crate) fn work_task_presentation(task: &SessionTask) -> Option<WorkTaskPresentation> {
    if !task.id.starts_with("work:") {
        return None;
    }
    let metadata = task.metadata.as_ref()?;
    if metadata.get("source")?.as_str()? != "work_task_graph" {
        return None;
    }
    match metadata.get("declaration_state")?.as_str()? {
        "cancelled" => return Some(WorkTaskPresentation::Cancelled),
        "superseded" => return Some(WorkTaskPresentation::Replaced),
        "active" => {}
        _ => return None,
    }
    let execution = metadata.get("execution_status")?.as_str()?;
    if execution != "completed" {
        return match execution {
            "not_started" => Some(WorkTaskPresentation::Planned),
            "running" | "delegated" => Some(WorkTaskPresentation::Working),
            "waiting" => Some(WorkTaskPresentation::Waiting),
            "paused" => Some(WorkTaskPresentation::Paused),
            "failed" => Some(WorkTaskPresentation::Failed),
            "cancelled" => Some(WorkTaskPresentation::Cancelled),
            _ => None,
        };
    }
    match metadata.get("delivery_status")?.as_str()? {
        "unreported" => return Some(WorkTaskPresentation::OutcomeUnreported),
        "blocked" => return Some(WorkTaskPresentation::Blocked),
        "failed" => return Some(WorkTaskPresentation::Failed),
        "delivered" => {}
        _ => return None,
    }
    match metadata.get("verification_status")?.as_str()? {
        "unknown" => Some(WorkTaskPresentation::Executed),
        "stale_evidence" => Some(WorkTaskPresentation::NeedsRecheck),
        "evidence_available" => {
            let freshness = metadata
                .get("check_freshness")
                .and_then(|value| value.as_str());
            let outcome = metadata
                .get("check_outcome")
                .and_then(|value| value.as_str());
            let coverage = metadata
                .get("check_coverage")
                .and_then(|value| value.as_str());
            let evidence_count = metadata
                .get("check_evidence_ref_count")
                .and_then(|value| value.as_u64());
            match (freshness, outcome, coverage, evidence_count) {
                (Some("current"), Some("passed"), Some("complete"), Some(count)) if count > 0 => {
                    Some(WorkTaskPresentation::Verified)
                }
                (Some("current"), Some("failed" | "error" | "cancelled"), _, _) => {
                    Some(WorkTaskPresentation::CheckFailed)
                }
                // Once delivery is known, incomplete or malformed evidence can
                // only prove that verification is still required. It must not
                // fall through to the raw completed execution status.
                _ => Some(WorkTaskPresentation::Executed),
            }
        }
        _ => None,
    }
}

fn board_status(task: &SessionTask) -> SessionTaskStatusKind {
    work_task_presentation(task)
        .map(WorkTaskPresentation::board_status)
        .unwrap_or(task.status)
}

pub(crate) fn task_needs_attention(task: &SessionTask) -> bool {
    board_status(task).is_open_work()
}

/// Compute how many task lines fit in the given terminal height. Mirrors
/// the reference TUI's `maxDisplay = rows <= 10 ? 0 : min(10, max(3, rows - 14))`.
fn max_display(rows: u16) -> usize {
    if rows <= 10 {
        return 0;
    }
    let ceiling = 10_usize;
    let floor = 3_usize;
    let derived = rows.saturating_sub(14) as usize;
    ceiling.min(floor.max(derived))
}

/// Conservative per-line subject width. Matches the reference TUI's
/// `max(15, columns - 15 - ownerWidth)` before subtracting any
/// per-row owner badge width.
fn max_subject_width(columns: u16) -> usize {
    (columns as usize).saturating_sub(15).max(15)
}

fn owner_badge(task: &SessionTask) -> Option<String> {
    let owner = task.owner.as_deref()?.trim();
    if owner.is_empty() {
        return None;
    }
    let owner = owner.strip_prefix('@').unwrap_or(owner);
    Some(format!(" (@{})", truncate_to_width(owner, 14)))
}

/// Truncate a string to fit within `max_cols` display columns, adding a
/// single `…` when truncated. Caller has already stripped control
/// characters.
fn truncate_to_width(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if s.width() <= max_cols {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > max_cols.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TaskStatusCounts {
    completed: usize,
    in_progress: usize,
    pending: usize,
    waiting: usize,
    blocked: usize,
    paused: usize,
    failed: usize,
    cancelled: usize,
    other: usize,
    needs_check: usize,
    outcome_unreported: usize,
}

impl TaskStatusCounts {
    fn open_work(self) -> usize {
        self.in_progress
            + self.pending
            + self.waiting
            + self.blocked
            + self.paused
            + self.needs_check
            + self.outcome_unreported
    }

    fn status_parts(self) -> Vec<String> {
        let mut parts = Vec::new();
        if self.in_progress > 0 {
            parts.push(format!("{} working", self.in_progress));
        }
        if self.pending > 0 {
            parts.push(format!("{} queued", self.pending));
        }
        if self.waiting > 0 {
            parts.push(format!("{} waiting", self.waiting));
        }
        if self.blocked > 0 {
            parts.push(format!("{} blocked", self.blocked));
        }
        if self.paused > 0 {
            parts.push(format!("{} paused", self.paused));
        }
        if self.needs_check > 0 {
            parts.push(format!("{} to verify", self.needs_check));
        }
        if self.outcome_unreported > 0 {
            parts.push(format!("{} result not reported", self.outcome_unreported));
        }
        if self.completed > 0 {
            parts.push(format!("{} done", self.completed));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if self.cancelled > 0 {
            parts.push(format!("{} cancelled", self.cancelled));
        }
        if self.other > 0 {
            parts.push(format!("{} other", self.other));
        }
        parts
    }
}

fn counts(tasks: &[SessionTask]) -> TaskStatusCounts {
    let mut counts = TaskStatusCounts::default();
    for task in tasks {
        count_task(&mut counts, task);
    }
    counts
}

fn count_task(counts: &mut TaskStatusCounts, task: &SessionTask) {
    match work_task_presentation(task) {
        Some(WorkTaskPresentation::Planned) => counts.pending += 1,
        Some(WorkTaskPresentation::Working) => counts.in_progress += 1,
        Some(WorkTaskPresentation::Waiting) => counts.waiting += 1,
        Some(WorkTaskPresentation::Blocked) => counts.blocked += 1,
        Some(WorkTaskPresentation::Paused) => counts.paused += 1,
        Some(WorkTaskPresentation::NeedsRecheck) => counts.needs_check += 1,
        Some(WorkTaskPresentation::OutcomeUnreported) => counts.outcome_unreported += 1,
        Some(WorkTaskPresentation::Executed | WorkTaskPresentation::Verified) => {
            counts.completed += 1
        }
        Some(WorkTaskPresentation::CheckFailed | WorkTaskPresentation::Failed) => {
            counts.failed += 1
        }
        Some(WorkTaskPresentation::Cancelled | WorkTaskPresentation::Replaced) => {
            counts.cancelled += 1
        }
        None => match board_status(task) {
            SessionTaskStatusKind::Completed => counts.completed += 1,
            SessionTaskStatusKind::InProgress => counts.in_progress += 1,
            SessionTaskStatusKind::Pending => counts.pending += 1,
            SessionTaskStatusKind::Paused => counts.paused += 1,
            SessionTaskStatusKind::Failed => counts.failed += 1,
            SessionTaskStatusKind::Cancelled => counts.cancelled += 1,
            SessionTaskStatusKind::Other => counts.other += 1,
        },
    }
}

/// Aggregate (done, total) over every subtask across `tasks`. Used by
/// the standalone header to surface the real progress when one task
/// fans out into many subtasks — otherwise the top-level "1 task in
/// progress" hides the fact that 2/5 subtasks already shipped.
fn subtask_counts(tasks: &[SessionTask]) -> (usize, usize) {
    let mut done = 0usize;
    let mut total = 0usize;
    for task in tasks {
        for sub in &task.subtasks {
            total += 1;
            if sub.status.is_completed() {
                done += 1;
            }
        }
    }
    (done, total)
}

/// Stable id-asc sort that falls back to string order when ids aren't
/// `task-<n>` shaped.
fn sort_by_id_asc(mut tasks: Vec<&SessionTask>) -> Vec<&SessionTask> {
    tasks.sort_by(|a, b| {
        let ap = a.id.trim_start_matches("task-").parse::<u32>().ok();
        let bp = b.id.trim_start_matches("task-").parse::<u32>().ok();
        match (ap, bp) {
            (Some(x), Some(y)) => x.cmp(&y),
            _ => a.id.cmp(&b.id),
        }
    });
    tasks
}

/// Order `tasks` by the reference TUI's display priority:
/// in_progress → pending (open blockers last) → paused → completed →
/// cancelled/failed/unknown diagnostics. Tombstones (archived/deleted/migrated)
/// are audit rows and never belong on the live task board.
fn prioritize<'a>(
    tasks: &'a [SessionTask],
    unresolved_by_task: &HashMap<String, Vec<String>>,
) -> Vec<&'a SessionTask> {
    let in_progress = sort_by_id_asc(
        tasks
            .iter()
            .filter(|task| board_status(task).is_in_progress())
            .collect(),
    );
    let mut pending: Vec<&SessionTask> = tasks
        .iter()
        .filter(|task| board_status(task).is_pending())
        .collect();
    pending.sort_by(|a, b| {
        let a_blocked = unresolved_by_task
            .get(&a.id)
            .is_some_and(|blockers| !blockers.is_empty());
        let b_blocked = unresolved_by_task
            .get(&b.id)
            .is_some_and(|blockers| !blockers.is_empty());
        match (a_blocked, b_blocked) {
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => {
                let ap = a.id.trim_start_matches("task-").parse::<u32>().ok();
                let bp = b.id.trim_start_matches("task-").parse::<u32>().ok();
                match (ap, bp) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    _ => a.id.cmp(&b.id),
                }
            }
        }
    });
    let paused = sort_by_id_asc(
        tasks
            .iter()
            .filter(|task| board_status(task) == SessionTaskStatusKind::Paused)
            .collect(),
    );
    let completed = sort_by_id_asc(
        tasks
            .iter()
            .filter(|task| board_status(task).is_completed())
            .collect(),
    );
    let terminal = sort_by_id_asc(
        tasks
            .iter()
            .filter(|t| {
                matches!(
                    board_status(t),
                    SessionTaskStatusKind::Cancelled | SessionTaskStatusKind::Failed
                )
            })
            .collect(),
    );
    let diagnostics = sort_by_id_asc(
        tasks
            .iter()
            .filter(|task| matches!(board_status(task), SessionTaskStatusKind::Other))
            .collect(),
    );
    let mut out: Vec<&SessionTask> = Vec::with_capacity(tasks.len());
    out.extend(in_progress);
    out.extend(pending);
    out.extend(paused);
    out.extend(completed);
    out.extend(terminal);
    out.extend(diagnostics);
    out
}

/// Return the stable identity of the first row in the board's actual display
/// order. Primary-canvas consumers use this to make details and run inspection
/// immediately available without maintaining a second, subtly different
/// notion of which task matters first.
pub(crate) fn preferred_task_id(tasks: &[SessionTask]) -> Option<&str> {
    let unresolved_by_task: HashMap<String, Vec<String>> = tasks
        .iter()
        .map(|task| (task.id.clone(), unresolved_task_blocker_ids(tasks, task)))
        .collect();
    prioritize(tasks, &unresolved_by_task)
        .into_iter()
        .next()
        .map(|task| task.id.as_str())
}

fn render_task_line(
    task: &SessionTask,
    open_blockers: &[String],
    columns: u16,
    colors: TaskBoardColors,
) -> Line<'static> {
    let display_status = board_status(task);
    let presentation = work_task_presentation(task);
    let (icon, color) = status_icon_and_color(&display_status, colors);
    let is_completed = display_status.is_completed();
    let is_in_progress = display_status.is_in_progress();
    let is_blocked = !open_blockers.is_empty();

    let owner_badge = owner_badge(task);
    let subject = truncate_to_width(
        &task.title,
        max_subject_width(columns).saturating_sub(
            owner_badge
                .as_deref()
                .map(unicode_width::UnicodeWidthStr::width)
                .unwrap_or(0),
        ),
    );

    let mut spans: Vec<Span<'static>> = Vec::new();
    // In-progress and completed get BOLD; pending and fallback statuses
    // already map to `colors.dim`, so adding DIM on top of dim would be
    // invisible — skip the modifier in that case.
    let icon_style = match display_status {
        SessionTaskStatusKind::InProgress | SessionTaskStatusKind::Completed => {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        }
        SessionTaskStatusKind::Failed => Style::default().fg(color).add_modifier(Modifier::BOLD),
        SessionTaskStatusKind::Cancelled => Style::default().fg(color).add_modifier(Modifier::BOLD),
        SessionTaskStatusKind::Paused => Style::default().fg(color).add_modifier(Modifier::BOLD),
        SessionTaskStatusKind::Pending | SessionTaskStatusKind::Other => {
            Style::default().fg(color).add_modifier(Modifier::DIM)
        }
    };
    spans.push(Span::styled(format!("{} ", icon), icon_style));

    let mut subject_style = Style::default();
    if is_in_progress {
        subject_style = subject_style.add_modifier(Modifier::BOLD);
    }
    if is_completed {
        subject_style = subject_style.add_modifier(Modifier::CROSSED_OUT);
    }
    if is_completed || is_blocked {
        subject_style = subject_style.add_modifier(Modifier::DIM);
    }
    spans.push(Span::styled(subject, subject_style));

    if let Some(presentation) = presentation {
        spans.push(Span::styled(
            format!(" · {}", presentation.label()),
            Style::default().fg(color),
        ));
    }

    if let Some(owner_badge) = owner_badge {
        spans.push(Span::styled(
            owner_badge,
            Style::default().fg(colors.dim).add_modifier(Modifier::DIM),
        ));
    }

    if is_blocked {
        let mut ids: Vec<&String> = open_blockers.iter().collect();
        ids.sort_by(|a, b| {
            let ap = a.trim_start_matches("task-").parse::<u32>().ok();
            let bp = b.trim_start_matches("task-").parse::<u32>().ok();
            match (ap, bp) {
                (Some(x), Some(y)) => x.cmp(&y),
                _ => a.cmp(b),
            }
        });
        let rendered = ids
            .iter()
            .map(|id| format!("#{}", id.trim_start_matches("task-")))
            .collect::<Vec<_>>()
            .join(", ");
        spans.push(Span::styled(
            format!(" · waiting on {}", rendered),
            Style::default().fg(colors.dim).add_modifier(Modifier::DIM),
        ));
    }

    Line::from(spans)
}

/// Render one indented line per subtask under its parent. Mirrors
/// the parent's `render_task_line` styling but uses 4-col indent and a
/// slightly dimmer subject so the eye scans subtask groups together.
fn render_subtask_lines(
    parent: &SessionTask,
    columns: u16,
    colors: TaskBoardColors,
) -> Vec<Line<'static>> {
    if parent.subtasks.is_empty() {
        return Vec::new();
    }
    // Resolve which subtask ids are still open so depends_on chains
    // can grey out blocked siblings — same logic as parent-level
    // blockers, just scoped to this task's subtasks.
    let unresolved: HashSet<String> = parent
        .subtasks
        .iter()
        .filter(|s| !s.status.is_completed())
        .map(|s| s.id.clone())
        .collect();

    let mut out = Vec::with_capacity(parent.subtasks.len());
    let indent = "    ";
    let subject_w = (columns as usize).saturating_sub(indent.len() + 4).max(10);
    for sub in &parent.subtasks {
        let (icon, color) = status_icon_and_color(&sub.status, colors);
        let is_completed = sub.status.is_completed();
        let is_in_progress = sub.status.is_in_progress();
        let blocked = sub.depends_on.iter().any(|id| unresolved.contains(id));

        let icon_style = match sub.status {
            SessionTaskStatusKind::InProgress | SessionTaskStatusKind::Completed => {
                Style::default().fg(color).add_modifier(Modifier::BOLD)
            }
            SessionTaskStatusKind::Failed => {
                Style::default().fg(color).add_modifier(Modifier::BOLD)
            }
            SessionTaskStatusKind::Cancelled => {
                Style::default().fg(color).add_modifier(Modifier::BOLD)
            }
            SessionTaskStatusKind::Paused => {
                Style::default().fg(color).add_modifier(Modifier::BOLD)
            }
            SessionTaskStatusKind::Pending | SessionTaskStatusKind::Other => {
                Style::default().fg(color).add_modifier(Modifier::DIM)
            }
        };
        let mut subject_style = Style::default().add_modifier(Modifier::DIM);
        if is_in_progress {
            subject_style = subject_style.add_modifier(Modifier::BOLD);
        }
        if is_completed {
            subject_style = subject_style.add_modifier(Modifier::CROSSED_OUT);
        }
        let subject = truncate_to_width(&sub.title, subject_w);
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
        spans.push(Span::raw(indent.to_string()));
        spans.push(Span::styled(format!("{} ", icon), icon_style));
        spans.push(Span::styled(subject, subject_style));
        if blocked {
            spans.push(Span::styled(
                " · waiting".to_string(),
                Style::default().fg(colors.dim).add_modifier(Modifier::DIM),
            ));
        }
        out.push(Line::from(spans));
    }
    out
}

fn render_hidden_summary(hidden: &[&SessionTask]) -> Option<Line<'static>> {
    if hidden.is_empty() {
        return None;
    }
    let counts = hidden
        .iter()
        .fold(TaskStatusCounts::default(), |mut counts, task| {
            count_task(&mut counts, task);
            counts
        });
    let parts = counts.status_parts();
    let text = if parts.len() == 1 {
        format!("… {} more {}", hidden.len(), parts[0])
    } else if parts.is_empty() {
        format!("… {} more", hidden.len())
    } else {
        format!("… {} more: {}", hidden.len(), parts.join(", "))
    };
    Some(Line::from(Span::styled(
        text,
        Style::default().add_modifier(Modifier::DIM),
    )))
}

/// Render the task board into `Vec<Line<'static>>`. Empty output when
/// nothing should display (hidden terminal, no tasks).
///
/// `standalone = true` prepends a summary header line, matching the
/// reference TUI's `isStandalone` mode. Colours come from the current
/// `tui::theme`. Tests that need a deterministic palette should call
/// [`render_with_colors`] directly.
pub fn render(
    tasks: &[SessionTask],
    columns: u16,
    rows: u16,
    standalone: bool,
) -> Vec<Line<'static>> {
    render_with_colors(
        tasks,
        columns,
        rows,
        standalone,
        TaskBoardColors::from_theme(),
    )
}

/// Same as [`render`] but also highlights rows whose id is in
/// `fresh_ids` — used for "just created" / "just completed" flash
/// feedback driven by the observer's diff ring.
pub fn render_with_fresh(
    tasks: &[SessionTask],
    columns: u16,
    rows: u16,
    standalone: bool,
    fresh_ids: &[String],
) -> Vec<Line<'static>> {
    if fresh_ids.is_empty() {
        return render_with_colors(
            tasks,
            columns,
            rows,
            standalone,
            TaskBoardColors::from_theme(),
        );
    }
    render_with_fresh_predicate(tasks, columns, rows, standalone, |task_id| {
        fresh_ids.iter().any(|id| id == task_id)
    })
}

pub(crate) fn render_with_fresh_predicate<F>(
    tasks: &[SessionTask],
    columns: u16,
    rows: u16,
    standalone: bool,
    is_fresh: F,
) -> Vec<Line<'static>>
where
    F: FnMut(&str) -> bool,
{
    render_with_colors_and_fresh_predicate(
        tasks,
        columns,
        max_display(rows),
        standalone,
        TaskBoardColors::from_theme(),
        is_fresh,
        None,
    )
    .lines
}

/// Same as [`render`] but with explicit colours. Separate entry point
/// so snapshots can pin either preset without mutating the process-wide
/// `OnceLock` theme.
pub fn render_with_colors(
    tasks: &[SessionTask],
    columns: u16,
    rows: u16,
    standalone: bool,
    colors: TaskBoardColors,
) -> Vec<Line<'static>> {
    render_with_colors_and_fresh_predicate(
        tasks,
        columns,
        max_display(rows),
        standalone,
        colors,
        |_| false,
        None,
    )
    .lines
}

/// Render every already-fetched task row. The primary task board owns the
/// viewport, so compact-list row caps must not hide work from navigation.
pub(crate) fn render_full(
    tasks: &[SessionTask],
    columns: u16,
    standalone: bool,
) -> Vec<Line<'static>> {
    render_full_focused(tasks, columns, standalone, None).lines
}

/// Primary-canvas variant that carries the stable focused task identity into
/// rendering. The task board owns selection; this renderer only paints the
/// corresponding row and never infers focus from title text.
pub(crate) fn render_full_focused(
    tasks: &[SessionTask],
    columns: u16,
    standalone: bool,
    selected_task_id: Option<&str>,
) -> TaskListRender {
    render_with_colors_and_fresh_predicate(
        tasks,
        columns,
        tasks.len(),
        standalone,
        TaskBoardColors::from_theme(),
        |_| false,
        selected_task_id,
    )
}

/// Render the board with an identity-based freshness overlay.  The overlay
/// belongs in this structural rendering pass: matching rendered text would be
/// ambiguous for tasks with the same title and couples state to presentation.
fn render_with_colors_and_fresh_predicate<F>(
    tasks: &[SessionTask],
    columns: u16,
    task_cap: usize,
    standalone: bool,
    colors: TaskBoardColors,
    mut is_fresh: F,
    selected_task_id: Option<&str>,
) -> TaskListRender
where
    F: FnMut(&str) -> bool,
{
    let cap = task_cap;
    if cap == 0 || tasks.is_empty() {
        return TaskListRender {
            lines: Vec::new(),
            selected_line_index: None,
        };
    }
    let counts = counts(tasks);
    let unresolved_by_task: HashMap<String, Vec<String>> = tasks
        .iter()
        .map(|task| (task.id.clone(), unresolved_task_blocker_ids(tasks, task)))
        .collect();

    let prioritized = prioritize(tasks, &unresolved_by_task);
    let (visible, hidden): (Vec<&SessionTask>, Vec<&SessionTask>) = if prioritized.len() > cap {
        let visible = prioritized.iter().take(cap).copied().collect();
        let hidden = prioritized.iter().skip(cap).copied().collect();
        (visible, hidden)
    } else {
        (prioritized, Vec::new())
    };

    let mut out: Vec<Line<'static>> = Vec::with_capacity(visible.len() + 2);
    let mut selected_line_index = None;

    if standalone {
        let mut header_spans: Vec<Span<'static>> = Vec::new();
        header_spans.push(Span::styled(
            "Tasks",
            Style::default().add_modifier(Modifier::BOLD),
        ));

        let has_classic_counts =
            counts.in_progress > 0 || counts.pending > 0 || counts.completed > 0;
        let full_suffix = if has_classic_counts {
            let mut suffix = format!(
                " · {} working · {} queued · {} done",
                counts.in_progress, counts.pending, counts.completed
            );
            for part in (TaskStatusCounts {
                in_progress: 0,
                pending: 0,
                completed: 0,
                ..counts
            })
            .status_parts()
            {
                suffix.push_str(&format!(" · {part}"));
            }
            suffix
        } else {
            let parts = counts.status_parts();
            if parts.is_empty() {
                String::new()
            } else {
                format!(" · {}", parts.join(" · "))
            }
        };
        let open_parts = {
            let mut parts = Vec::new();
            if counts.in_progress > 0 {
                parts.push(format!("{} working", counts.in_progress));
            }
            if counts.pending > 0 {
                parts.push(format!("{} queued", counts.pending));
            }
            if counts.waiting > 0 {
                parts.push(format!("{} waiting", counts.waiting));
            }
            if counts.blocked > 0 {
                parts.push(format!("{} blocked", counts.blocked));
            }
            if counts.paused > 0 {
                parts.push(format!("{} paused", counts.paused));
            }
            parts
        };
        let queued_done = {
            let mut parts = Vec::new();
            if counts.pending > 0 {
                parts.push(format!("{} queued", counts.pending));
            }
            if counts.completed > 0 {
                parts.push(format!("{} done", counts.completed));
            }
            if counts.waiting > 0 {
                parts.push(format!("{} waiting", counts.waiting));
            }
            if counts.blocked > 0 {
                parts.push(format!("{} blocked", counts.blocked));
            }
            if counts.paused > 0 {
                parts.push(format!("{} paused", counts.paused));
            }
            parts
        };
        let header_variants = [
            full_suffix,
            if open_parts.is_empty() {
                String::new()
            } else {
                format!(" · {}", open_parts.join(" · "))
            },
            if queued_done.is_empty() {
                String::new()
            } else {
                format!(" · {}", queued_done.join(" · "))
            },
            if counts.in_progress > 0 {
                format!(" · {} working", counts.in_progress)
            } else {
                String::new()
            },
            if counts.pending > 0 {
                format!(" · {} queued", counts.pending)
            } else if counts.waiting > 0 {
                format!(" · {} waiting", counts.waiting)
            } else if counts.blocked > 0 {
                format!(" · {} blocked", counts.blocked)
            } else if counts.paused > 0 {
                format!(" · {} paused", counts.paused)
            } else {
                String::new()
            },
        ];
        let prefix_width = "Tasks".width();
        let suffix = header_variants
            .iter()
            .find(|variant| prefix_width + variant.width() <= columns as usize)
            .cloned()
            .unwrap_or_default();
        header_spans.push(Span::styled(
            suffix,
            Style::default().add_modifier(Modifier::DIM),
        ));
        let header_width: usize = header_spans.iter().map(|s| s.content.width()).sum();
        if header_width + TASK_BOARD_TOGGLE_HINT.width() <= columns as usize {
            header_spans.push(Span::styled(
                TASK_BOARD_TOGGLE_HINT,
                Style::default().fg(colors.dim).add_modifier(Modifier::DIM),
            ));
        }

        // Subtask roll-up: when any task fans out into subtasks, show
        // aggregate progress so a "1 task in progress" header doesn't
        // hide the 2/5 subtasks that already shipped.
        let (sub_done, sub_total) = subtask_counts(tasks);
        if sub_total > 0 {
            header_spans.push(Span::styled(
                format!(" · {sub_done}/{sub_total} done"),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        out.push(Line::from(header_spans));
    }

    // Total board lines stay bounded: parent task lines + a global
    // subtask budget. Without this, a 10-parent × 5-subtask board
    // would push 60+ rows and starve the streaming region.
    let max_total_subtask_rows: usize = (cap * 2).max(8);
    let mut subtask_rows_emitted = 0usize;
    let mut hidden_subtask_total = 0usize;
    for task in &visible {
        let open_blockers = unresolved_by_task
            .get(&task.id)
            .cloned()
            .unwrap_or_default();
        let mut task_line = render_task_line(task, &open_blockers, columns, colors);
        if is_fresh(&task.id) {
            task_line.spans.insert(
                0,
                Span::styled(
                    "↻ ",
                    Style::default()
                        .fg(colors.accent)
                        .add_modifier(Modifier::BOLD),
                ),
            );
        }
        if selected_task_id == Some(task.id.as_str()) {
            selected_line_index = Some(out.len());
            task_line.spans.insert(
                0,
                Span::styled(
                    "› ",
                    Style::default()
                        .fg(colors.accent)
                        .add_modifier(Modifier::BOLD),
                ),
            );
        }
        out.push(task_line);
        // Per-parent cap (keeps one runaway parent from monopolising
        // the global budget) plus the global cap above. reference-agent
        // shows every subtask inline; we cap to keep terminal layout
        // sane on tight rows.
        let max_subs_per_parent = 8usize;
        let remaining_global = max_total_subtask_rows.saturating_sub(subtask_rows_emitted);
        let local_budget = max_subs_per_parent.min(remaining_global);
        let sub_lines = render_subtask_lines(task, columns, colors);
        let n = sub_lines.len();
        let take = n.min(local_budget);
        for line in sub_lines.into_iter().take(take) {
            out.push(line);
            subtask_rows_emitted += 1;
        }
        if n > take {
            hidden_subtask_total += n - take;
        }
    }
    if hidden_subtask_total > 0 {
        out.push(Line::from(Span::styled(
            format!("    … +{hidden_subtask_total} more subtasks"),
            Style::default().fg(colors.dim).add_modifier(Modifier::DIM),
        )));
    }

    if let Some(summary) = render_hidden_summary(&hidden) {
        out.push(summary);
    }
    TaskListRender {
        lines: out,
        selected_line_index,
    }
}

/// One-line "compact" summary used as the default board view while the
/// user hasn't pressed Ctrl+T. Replaces the full panel during running
/// turns so the spinner / streaming region stays uncluttered.
///
/// Format: `• Tasks · K working · J done · <title>`
/// (the title segment shows the in-progress task title, falling
/// back to the next pending item when nothing is in progress yet; subtask roll-up
/// only appears when any subtask exists).
///
/// Returns `None` for empty task lists — caller renders nothing in
/// that case.
pub fn render_collapsed_summary(tasks: &[SessionTask], columns: u16) -> Option<Line<'static>> {
    if tasks.is_empty() {
        return None;
    }
    let counts = counts(tasks);
    let total = tasks.len();
    let current_task = tasks
        .iter()
        .find(|task| board_status(task).is_in_progress())
        .or_else(|| tasks.iter().find(|task| board_status(task).is_pending()))
        .or_else(|| {
            tasks
                .iter()
                .find(|task| board_status(task) == SessionTaskStatusKind::Paused)
        });
    let (sub_done, sub_total) = subtask_counts(tasks);

    let theme = crate::tui::theme::current();
    let has_waiting = counts.waiting > 0 || counts.blocked > 0 || counts.paused > 0;
    let icon = if counts.in_progress > 0 {
        "•"
    } else if has_waiting {
        "⏸"
    } else if counts.completed == total {
        "✔"
    } else {
        "·"
    };
    let icon_color = if counts.in_progress > 0 {
        theme.accent
    } else if has_waiting {
        theme.warn
    } else if counts.completed == total {
        theme.success
    } else {
        theme.dim
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        format!("{icon} "),
        Style::default().fg(icon_color).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        "Tasks",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if counts.in_progress > 0 {
        spans.push(Span::styled(
            format!(" · {} working", counts.in_progress),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    if counts.waiting > 0 {
        spans.push(Span::styled(
            format!(" · {} waiting", counts.waiting),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    if counts.blocked > 0 {
        spans.push(Span::styled(
            format!(" · {} blocked", counts.blocked),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    if counts.paused > 0 {
        spans.push(Span::styled(
            format!(" · {} paused", counts.paused),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    if counts.needs_check > 0 {
        spans.push(Span::styled(
            format!(" · {} to verify", counts.needs_check),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    if counts.outcome_unreported > 0 {
        spans.push(Span::styled(
            format!(" · {} result not reported", counts.outcome_unreported),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    if counts.failed > 0 {
        spans.push(Span::styled(
            format!(" · {} failed", counts.failed),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    if counts.cancelled > 0 {
        spans.push(Span::styled(
            format!(" · {} cancelled", counts.cancelled),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    spans.push(Span::styled(
        format!(" · {} done", counts.completed),
        Style::default().add_modifier(Modifier::DIM),
    ));

    // Show in-progress task title BEFORE subtask roll-up and toggle hint.
    // Space priority: title > toggle hint > subtask counts > status breakdown.
    // The user must always see what's being worked on.
    if let Some(task) = current_task {
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        let sep = " · ";
        let title_budget = (columns as usize).saturating_sub(used + sep.width());
        if title_budget > 0 {
            let title = truncate_to_width(&task.title, title_budget);
            spans.push(Span::styled(
                sep.to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ));
            spans.push(Span::styled(title, Style::default()));
        }
    }

    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    if used + TASK_BOARD_TOGGLE_HINT.width() <= columns as usize {
        spans.push(Span::styled(
            TASK_BOARD_TOGGLE_HINT,
            Style::default().fg(theme.dim).add_modifier(Modifier::DIM),
        ));
    }

    if sub_total > 0 {
        let label = format!(" · {sub_done}/{sub_total} done");
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        if used + label.width() <= columns as usize {
            spans.push(Span::styled(
                label,
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
    }

    Some(Line::from(spans))
}

/// Compact projection for the always-on active surface. Terminal rows stay in
/// `tasks` so completed dependencies can be resolved correctly, but only open
/// work is eligible for the summary. Ready work is preferred within a status;
/// when the selected item is blocked, the reason is visible rather than
/// presenting it as an executable "next" task.
pub fn render_collapsed_active_summary(
    tasks: &[SessionTask],
    columns: u16,
) -> Option<Line<'static>> {
    let mut active = tasks
        .iter()
        .filter(|task| board_status(task).is_open_work())
        .cloned()
        .collect::<Vec<_>>();
    active.sort_by_key(|task| !unresolved_task_blocker_ids(tasks, task).is_empty());
    if active.is_empty() {
        return None;
    }
    let selected = active
        .iter()
        .find(|task| board_status(task).is_in_progress())
        .or_else(|| active.iter().find(|task| board_status(task).is_pending()))
        .or_else(|| {
            active
                .iter()
                .find(|task| board_status(task) == SessionTaskStatusKind::Paused)
        });
    let blockers = selected
        .map(|task| unresolved_task_blocker_ids(tasks, task))
        .unwrap_or_default();
    let blocker_suffix = if blockers.is_empty() || columns < 40 {
        None
    } else {
        let mut ids = blockers
            .iter()
            .take(2)
            .map(|id| format!("#{}", id.trim_start_matches("task-")))
            .collect::<Vec<_>>();
        if blockers.len() > 2 {
            ids.push(format!("+{}", blockers.len() - 2));
        }
        Some(truncate_to_width(
            &format!(" · waiting on {}", ids.join(", ")),
            columns as usize / 2,
        ))
    };
    let summary_width = blocker_suffix.as_ref().map_or(columns, |suffix| {
        columns.saturating_sub(suffix.width() as u16)
    });
    // Preserve lifecycle progress in a mixed board while keeping terminal
    // rows ineligible for current/next selection. Putting the ordered open
    // rows first lets `render_collapsed_summary` select the same ready task;
    // appending terminal history keeps done/failed/cancelled counts truthful.
    let mut lifecycle = active;
    lifecycle.extend(
        tasks
            .iter()
            .filter(|task| !board_status(task).is_open_work())
            .cloned(),
    );
    let mut line = render_collapsed_summary(&lifecycle, summary_width)?;
    if let Some(suffix) = blocker_suffix {
        line.spans.push(Span::styled(
            suffix,
            Style::default()
                .fg(crate::tui::theme::current().warn)
                .add_modifier(Modifier::DIM),
        ));
    }
    Some(line)
}

/// One-line "Focus · <subject>" nudge for use when `expanded_view` is not
/// `Tasks` but a task is in flight. Matches the reference TUI's Spinner
/// fallback at `components/Spinner.tsx:296`.
pub fn render_next_hint(tasks: &[SessionTask], columns: u16) -> Option<Line<'static>> {
    let mut active = tasks
        .iter()
        .filter(|task| board_status(task).is_open_work())
        .map(|task| (task, unresolved_task_blocker_ids(tasks, task)))
        .collect::<Vec<_>>();
    active.sort_by_key(|(task, blockers)| {
        (board_status(task).active_priority(), !blockers.is_empty())
    });
    let (candidate, blockers) = active.first()?;
    let label = if !blockers.is_empty() {
        "Waiting · "
    } else if board_status(candidate) == SessionTaskStatusKind::Paused {
        "Resume · "
    } else {
        "Focus · "
    };
    let subject = truncate_to_width(
        &candidate.title,
        max_subject_width(columns).saturating_sub(label.width()),
    );
    Some(Line::from(Span::styled(
        format!("{label}{subject}"),
        Style::default().add_modifier(Modifier::DIM),
    )))
}

// ───────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod snapshot_tests;

#[cfg(test)]
mod tests {
    use super::super::work_board_projection::SessionTask;
    use super::*;

    fn mk_task(id: &str, title: &str, status: &str) -> SessionTask {
        SessionTask {
            id: id.into(),
            title: title.into(),
            description: None,
            status: status.into(),
            subtasks: vec![],
            created_at: "now".into(),
            updated_at: "now".into(),
            active_form: None,
            owner: None,
            metadata: None,
            blocks: vec![],
            blocked_by: vec![],
        }
    }

    fn mk_work_task(execution: &str, verification: &str) -> SessionTask {
        let mut task = mk_task("work:work-7:main:item-1", "migrate state", "completed");
        task.metadata = Some(serde_json::Map::from_iter([
            ("source".into(), serde_json::json!("work_task_graph")),
            ("declaration_state".into(), serde_json::json!("active")),
            ("execution_status".into(), serde_json::json!(execution)),
            ("delivery_status".into(), serde_json::json!("delivered")),
            (
                "verification_status".into(),
                serde_json::json!(verification),
            ),
        ]));
        task
    }

    #[test]
    fn completed_run_without_typed_settlement_is_not_presented_as_delivery() {
        let mut task = mk_work_task("completed", "unknown");
        task.metadata
            .as_mut()
            .expect("Work metadata")
            .insert("delivery_status".into(), serde_json::json!("unreported"));

        assert_eq!(
            work_task_presentation(&task),
            Some(WorkTaskPresentation::OutcomeUnreported)
        );
        assert!(task_needs_attention(&task));

        let mut blocked = task;
        let metadata = blocked.metadata.as_mut().expect("Work metadata");
        metadata.insert("delivery_status".into(), serde_json::json!("blocked"));
        metadata.insert(
            "delivery_blocker_kind".into(),
            serde_json::json!("capability_unavailable"),
        );
        metadata.insert(
            "unavailable_capabilities".into(),
            serde_json::json!(["web_fetch"]),
        );
        assert_eq!(
            work_task_presentation(&blocked),
            Some(WorkTaskPresentation::Blocked)
        );
        assert!(task_needs_attention(&blocked));
    }

    #[test]
    fn canonical_blocker_is_never_summarized_as_paused_or_done() {
        let mut blocked = mk_work_task("completed", "unknown");
        blocked.title = "provider capability unavailable".into();
        blocked
            .metadata
            .as_mut()
            .expect("Work metadata")
            .insert("delivery_status".into(), serde_json::json!("blocked"));

        let expanded = render(std::slice::from_ref(&blocked), 100, 40, true);
        let header = spans_text(&expanded[0]);
        assert!(header.contains("1 blocked"), "{header}");
        assert!(!header.contains("paused"), "{header}");
        assert!(!header.contains("done"), "{header}");

        let collapsed = spans_text(
            &render_collapsed_summary(&[blocked], 100).expect("blocked Work remains visible"),
        );
        assert!(collapsed.contains("1 blocked"), "{collapsed}");
        assert!(!collapsed.contains("paused"), "{collapsed}");
    }

    fn spans_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn max_display_follows_rows() {
        assert_eq!(max_display(5), 0);
        assert_eq!(max_display(10), 0);
        assert_eq!(max_display(11), 3);
        assert_eq!(max_display(17), 3);
        assert_eq!(max_display(18), 4);
        assert_eq!(max_display(24), 10);
        assert_eq!(max_display(100), 10);
    }

    #[test]
    fn truncate_keeps_ellipsis() {
        assert_eq!(truncate_to_width("short", 20), "short");
        let t = truncate_to_width("this subject is definitely too long", 12);
        assert!(t.ends_with('…'));
        assert!(t.width() <= 12);
    }

    #[test]
    fn hidden_when_rows_too_small() {
        let tasks = vec![mk_task("task-1", "a", "pending")];
        let lines = render(&tasks, 80, 8, false);
        assert!(lines.is_empty(), "hidden when rows <= 10");
    }

    #[test]
    fn empty_list_renders_nothing() {
        let lines = render(&[], 80, 40, true);
        assert!(lines.is_empty());
    }

    #[test]
    fn standalone_header_shows_counts() {
        let tasks = vec![
            mk_task("task-1", "alpha", "completed"),
            mk_task("task-2", "beta", "in_progress"),
            mk_task("task-3", "gamma", "pending"),
        ];
        let lines = render(&tasks, 80, 40, true);
        assert!(!lines.is_empty());
        let header = spans_text(&lines[0]);
        assert!(header.contains("Tasks"), "header: {header}");
        assert!(header.contains("1 done"));
        assert!(header.contains("1 working"));
        assert!(header.contains("1 queued"));
    }

    #[test]
    fn standalone_header_does_not_count_paused_or_terminal_as_working() {
        let tasks = vec![
            mk_task("task-1", "paused-work", "paused"),
            mk_task("task-2", "failed-work", "failed"),
            mk_task("task-3", "cancelled-work", "cancelled"),
        ];
        let lines = render(&tasks, 100, 40, true);
        let header = spans_text(&lines[0]);
        assert!(header.contains("1 paused"), "header: {header}");
        assert!(header.contains("1 failed"), "header: {header}");
        assert!(header.contains("1 cancelled"), "header: {header}");
        assert!(!header.contains("working"), "header: {header}");
    }

    #[test]
    fn owner_badge_is_rendered_inline() {
        let mut task = mk_task("task-1", "delegate audit", "in_progress");
        task.owner = Some("agent-7".into());

        let line = render_task_line(&task, &[], 80, TaskBoardColors::from_theme());
        let text = spans_text(&line);
        assert!(text.contains("delegate audit"));
        assert!(text.contains("(@agent-7)"), "{text}");
    }

    #[test]
    fn delivered_work_is_completed_without_verification_evidence() {
        let task = mk_work_task("completed", "unknown");

        let line = render_task_line(&task, &[], 80, TaskBoardColors::from_theme());
        let text = spans_text(&line);
        assert!(text.contains("Completed"), "{text}");
        assert!(!text.contains("Needs verification"), "{text}");
        assert!(!text.contains("Verified"), "{text}");
        assert!(!task_needs_attention(&task));

        let task_counts = counts(&[task]);
        assert_eq!(task_counts.needs_check, 0);
        assert_eq!(task_counts.completed, 1);
    }

    #[test]
    fn current_complete_pass_is_the_only_verified_presentation() {
        let mut task = mk_work_task("completed", "evidence_available");
        let metadata = task.metadata.as_mut().expect("fixture metadata");
        metadata.insert("check_freshness".into(), serde_json::json!("current"));
        metadata.insert("check_outcome".into(), serde_json::json!("passed"));
        metadata.insert("check_coverage".into(), serde_json::json!("complete"));
        metadata.insert("check_evidence_ref_count".into(), serde_json::json!(1));

        let line = render_task_line(&task, &[], 80, TaskBoardColors::from_theme());
        assert!(spans_text(&line).contains("Verified"));
        assert_eq!(counts(&[task]).completed, 1);
    }

    #[test]
    fn verification_requires_durable_evidence_not_only_a_pass_claim() {
        let mut task = mk_work_task("completed", "evidence_available");
        let metadata = task.metadata.as_mut().expect("fixture metadata");
        metadata.insert("check_freshness".into(), serde_json::json!("current"));
        metadata.insert("check_outcome".into(), serde_json::json!("passed"));
        metadata.insert("check_coverage".into(), serde_json::json!("complete"));
        metadata.insert("check_evidence_ref_count".into(), serde_json::json!(0));

        assert_eq!(
            work_task_presentation(&task),
            Some(WorkTaskPresentation::Executed)
        );
        let task_counts = counts(std::slice::from_ref(&task));
        assert_eq!(task_counts.needs_check, 0);
        assert_eq!(task_counts.completed, 1);

        task.metadata
            .as_mut()
            .expect("fixture metadata")
            .remove("check_evidence_ref_count");
        assert_eq!(
            work_task_presentation(&task),
            Some(WorkTaskPresentation::Executed)
        );
    }

    #[test]
    fn arbitrary_metadata_cannot_spoof_canonical_work_state() {
        let mut task = mk_task("task-1", "ordinary checklist", "completed");
        task.metadata = mk_work_task("completed", "unknown").metadata;

        assert_eq!(work_task_presentation(&task), None);
        let line = render_task_line(&task, &[], 80, TaskBoardColors::from_theme());
        assert!(!spans_text(&line).contains("Needs verification"));
    }

    #[test]
    fn priority_order_is_in_progress_then_pending_then_paused_then_completed() {
        let tasks = vec![
            mk_task("task-1", "first-completed", "completed"),
            mk_task("task-2", "first-pending", "pending"),
            mk_task("task-3", "first-in-progress", "in_progress"),
            mk_task("task-4", "first-paused", "paused"),
        ];
        let lines = render(&tasks, 80, 40, false);
        // Three visible lines, no header (standalone=false), no truncation.
        assert_eq!(lines.len(), 4);
        let texts: Vec<String> = lines.iter().map(spans_text).collect();
        let pos = |needle: &str| texts.iter().position(|l| l.contains(needle)).unwrap();
        assert!(pos("first-in-progress") < pos("first-pending"));
        assert!(pos("first-pending") < pos("first-paused"));
        assert!(pos("first-paused") < pos("first-completed"));
    }

    #[test]
    fn preferred_task_matches_first_rendered_canonical_work_row() {
        let tasks = vec![
            mk_task("task-1", "done", "completed"),
            mk_task("task-2", "queued", "pending"),
            mk_task("task-3", "working", "in_progress"),
        ];

        assert_eq!(preferred_task_id(&tasks), Some("task-3"));
        let lines = render_full_focused(&tasks, 80, false, preferred_task_id(&tasks));
        assert_eq!(lines.selected_line_index, Some(0));
        assert!(spans_text(&lines.lines[0]).contains("working"));
    }

    #[test]
    fn truncation_adds_hidden_summary_line() {
        let tasks: Vec<_> = (1..=15)
            .map(|i| mk_task(&format!("task-{i}"), &format!("task number {i}"), "pending"))
            .collect();
        let lines = render(&tasks, 80, 24, false);
        // cap=10 for rows=24 → 10 visible + 1 hidden-summary
        assert_eq!(lines.len(), 11);
        let summary = spans_text(&lines[10]);
        assert!(summary.contains("more"), "summary: {summary}");
        assert!(summary.contains("5 queued"), "summary: {summary}");
    }

    #[test]
    fn hidden_summary_names_paused_and_terminal_statuses() {
        let tasks = vec![
            mk_task("task-1", "visible", "in_progress"),
            mk_task("task-2", "visible-pending-a", "pending"),
            mk_task("task-3", "visible-pending-b", "pending"),
            mk_task("task-4", "hidden-paused", "paused"),
            mk_task("task-5", "hidden-failed", "failed"),
            mk_task("task-6", "hidden-cancelled", "cancelled"),
        ];
        let lines = render(&tasks, 80, 11, false);
        let summary = spans_text(lines.last().expect("hidden summary"));
        assert!(summary.contains("1 paused"), "summary: {summary}");
        assert!(summary.contains("1 failed"), "summary: {summary}");
        assert!(summary.contains("1 cancelled"), "summary: {summary}");
        assert!(!summary.contains("working"), "summary: {summary}");
    }

    #[test]
    fn unknown_projected_status_remains_visible_but_non_actionable() {
        let tasks = vec![mk_task("task-1", "future status", "future_status")];
        let lines = render(&tasks, 80, 40, true);
        assert_eq!(lines.len(), 2, "unknown status must remain diagnosable");
        assert!(spans_text(&lines[0]).contains("1 other"));
        assert_eq!(preferred_task_id(&tasks), Some("task-1"));
        assert!(render_next_hint(&tasks, 80).is_none());
    }

    #[test]
    fn blocked_tasks_sort_after_unblocked_within_pending() {
        let a = mk_task("task-1", "free", "pending");
        let mut b = mk_task("task-2", "blocked", "pending");
        b.blocked_by = vec!["task-3".into()];
        let c = mk_task("task-3", "blocker-in-progress", "in_progress");
        // Add a 4th pending that is free to ensure sort is stable.
        let d = mk_task("task-4", "another-free", "pending");
        let tasks = vec![c, b, a, d];
        // Force status for clippy
        let _ = &mut tasks.clone();

        let tasks_for_render = tasks.clone();
        let lines = render(&tasks_for_render, 80, 40, false);
        // Expect: in_progress, free-pending (task-1), another-free (task-4),
        // blocked-pending (task-2).
        let texts: Vec<String> = lines.iter().map(spans_text).collect();
        let pos = |needle: &str| texts.iter().position(|l| l.contains(needle)).unwrap();
        assert!(pos("blocker-in-progress") < pos("free"));
        assert!(pos("free") < pos("blocked"));
        assert!(pos("another-free") < pos("blocked"));
    }

    #[test]
    fn blocked_line_shows_blocker_ids() {
        let mut blocked = mk_task("task-2", "blocked", "pending");
        blocked.blocked_by = vec!["task-1".into(), "task-3".into()];
        let blocker_a = mk_task("task-1", "first", "in_progress");
        let blocker_c = mk_task("task-3", "third", "pending");
        let tasks = vec![blocker_a, blocked, blocker_c];
        let lines = render(&tasks, 80, 40, false);
        let blocked_line = lines
            .iter()
            .find(|l| spans_text(l).contains("blocked"))
            .expect("blocked line present");
        let text = spans_text(blocked_line);
        assert!(text.contains("waiting on"), "{text}");
        assert!(text.contains("#1"), "{text}");
        assert!(text.contains("#3"), "{text}");
    }

    #[test]
    fn blocker_badge_uses_dependency_state_not_edge_presence() {
        let completed = mk_task("task-1", "finished prerequisite", "completed");
        let mut ready = mk_task("task-2", "ready", "pending");
        ready.blocked_by = vec!["task-1".into()];
        let mut dangling = mk_task("task-3", "dangling", "pending");
        dangling.blocked_by = vec!["task-missing".into()];

        let lines = render(&[completed, ready, dangling], 100, 40, false);
        let ready_line = lines
            .iter()
            .find(|line| spans_text(line).contains("ready"))
            .map(spans_text)
            .expect("ready row");
        let dangling_line = lines
            .iter()
            .find(|line| spans_text(line).contains("dangling"))
            .map(spans_text)
            .expect("dangling row");

        assert!(
            !ready_line.contains("waiting on"),
            "completed prerequisites must not look unresolved: {ready_line}"
        );
        assert!(
            dangling_line.contains("waiting on #missing"),
            "missing prerequisites must fail closed instead of disappearing: {dangling_line}"
        );
    }

    #[test]
    fn next_hint_picks_in_progress_first() {
        let tasks = vec![
            mk_task("task-1", "done-thing", "completed"),
            mk_task("task-2", "pending-thing", "pending"),
            mk_task("task-3", "running-thing", "in_progress"),
        ];
        let hint = render_next_hint(&tasks, 80).expect("some");
        let text = spans_text(&hint);
        assert!(text.contains("running-thing"), "{text}");
        assert!(text.starts_with("Focus · "), "{text}");
    }

    #[test]
    fn next_hint_returns_none_when_all_completed() {
        let tasks = vec![mk_task("task-1", "done", "completed")];
        assert!(render_next_hint(&tasks, 80).is_none());
    }

    #[test]
    fn next_hint_does_not_present_blocked_work_as_focus() {
        let blocker = mk_task("task-1", "prerequisite", "pending");
        let mut blocked = mk_task("task-2", "dependent", "pending");
        blocked.blocked_by = vec!["task-1".into()];
        let hint = render_next_hint(&[blocker, blocked], 80).expect("ready blocker hint");
        assert_eq!(spans_text(&hint), "Focus · prerequisite");

        let mut missing = mk_task("task-3", "needs missing input", "pending");
        missing.blocked_by = vec!["task-missing".into()];
        let waiting = render_next_hint(&[missing], 80).expect("blocked hint");
        assert_eq!(spans_text(&waiting), "Waiting · needs missing input");

        let paused = mk_task("task-4", "paused for review", "paused");
        let resume = render_next_hint(&[paused], 80).expect("paused hint");
        assert_eq!(spans_text(&resume), "Resume · paused for review");
    }

    #[test]
    fn collapsed_summary_shows_counts_and_current_task() {
        let tasks = vec![
            mk_task("task-1", "alpha-done", "completed"),
            mk_task("task-2", "beta-running", "in_progress"),
            mk_task("task-3", "gamma-pending", "pending"),
        ];
        let line = render_collapsed_summary(&tasks, 100).expect("non-empty");
        let text = spans_text(&line);
        assert!(text.contains("Tasks"), "{text}");
        assert!(text.contains("1 done"), "{text}");
        assert!(text.contains("1 working"), "{text}");
        assert!(!text.contains("total"), "{text}");
        // The current-task title should be the in_progress one, not
        // the completed one.
        assert!(text.contains("beta-running"), "{text}");
        assert!(!text.contains("alpha-done"), "{text}");
        assert!(text.contains("Ctrl+T toggle"), "{text}");
    }

    #[test]
    fn collapsed_summary_omits_ctrl_t_hint_when_narrow() {
        let tasks = vec![
            mk_task("task-1", "beta-running", "in_progress"),
            mk_task("task-2", "gamma-pending", "pending"),
        ];
        let line = render_collapsed_summary(&tasks, 40).expect("non-empty");
        let text = spans_text(&line);
        assert!(!text.contains("Ctrl+T"), "{text}");
    }

    #[test]
    fn collapsed_summary_surfaces_paused_without_calling_it_working() {
        let tasks = vec![mk_task("task-1", "paused-thing", "paused")];
        let line = render_collapsed_summary(&tasks, 100).expect("non-empty");
        let text = spans_text(&line);
        assert!(text.contains("1 paused"), "{text}");
        assert!(text.contains("paused-thing"), "{text}");
        assert!(!text.contains("working"), "{text}");
    }

    #[test]
    fn collapsed_active_summary_prefers_ready_work_and_explains_blocked_work() {
        let blocker = mk_task("task-1", "prerequisite", "pending");
        let mut blocked = mk_task("task-2", "blocked next", "pending");
        blocked.blocked_by = vec!["task-1".into()];
        let ready = mk_task("task-3", "ready next", "pending");
        let tasks = vec![blocker, blocked, ready];
        let ready_summary =
            spans_text(&render_collapsed_active_summary(&tasks, 100).expect("active summary"));
        assert!(ready_summary.contains("prerequisite"), "{ready_summary}");
        assert!(!ready_summary.contains("blocked next"), "{ready_summary}");

        let completed = mk_task("task-1", "finished prerequisite", "completed");
        let mut dependent = mk_task("task-2", "dependent", "pending");
        dependent.blocked_by = vec!["task-1".into()];
        let resolved_summary = spans_text(
            &render_collapsed_active_summary(&[completed, dependent], 100)
                .expect("resolved active summary"),
        );
        assert!(resolved_summary.contains("dependent"), "{resolved_summary}");
        assert!(
            resolved_summary.contains("1 done"),
            "mixed compact boards must retain terminal lifecycle progress: {resolved_summary}"
        );
        assert!(
            !resolved_summary.contains("waiting on"),
            "{resolved_summary}"
        );

        let mut dangling = mk_task("task-4", "dangling", "pending");
        dangling.blocked_by = vec!["task-missing".into()];
        let blocked_summary = spans_text(
            &render_collapsed_active_summary(&[dangling], 100).expect("blocked active summary"),
        );
        assert!(
            blocked_summary.contains("waiting on #missing"),
            "{blocked_summary}"
        );
        let mut long_blocker = mk_task("task-4", "blocked with long id", "pending");
        long_blocker.blocked_by = vec![format!("task-{}", "x".repeat(128))];
        let narrow = spans_text(
            &render_collapsed_active_summary(&[long_blocker], 40).expect("narrow blocked summary"),
        );
        assert!(narrow.width() <= 40, "summary overflowed: {narrow}");
        assert!(
            render_collapsed_active_summary(&[mk_task("task-5", "done", "completed")], 80)
                .is_none()
        );
    }

    #[test]
    fn collapsed_summary_includes_subtask_rollup_when_present() {
        use super::super::work_board_projection::SessionSubtask;
        let mut parent = mk_task("task-1", "parent", "in_progress");
        parent.subtasks = vec![
            SessionSubtask {
                id: "s1".into(),
                title: "first".into(),
                description: None,
                status: "completed".into(),
                depends_on: vec![],
                owner: None,
                reason: None,
            },
            SessionSubtask {
                id: "s2".into(),
                title: "second".into(),
                description: None,
                status: "in_progress".into(),
                depends_on: vec![],
                owner: None,
                reason: None,
            },
        ];
        let line = render_collapsed_summary(&[parent], 100).expect("non-empty");
        let text = spans_text(&line);
        assert!(text.contains("1/2 done"), "{text}");
    }

    #[test]
    fn collapsed_summary_never_overflows_columns_with_long_title_and_subtasks() {
        use super::super::work_board_projection::SessionSubtask;
        let mut parent = mk_task(
            "task-1",
            "this is a very long current task title that must be clipped before optional suffixes",
            "in_progress",
        );
        parent.subtasks = vec![
            SessionSubtask {
                id: "s1".into(),
                title: "first".into(),
                description: None,
                status: "completed".into(),
                depends_on: vec![],
                owner: None,
                reason: None,
            },
            SessionSubtask {
                id: "s2".into(),
                title: "second".into(),
                description: None,
                status: "pending".into(),
                depends_on: vec![],
                owner: None,
                reason: None,
            },
        ];

        let line = render_collapsed_summary(&[parent], 40).expect("non-empty");
        let text = spans_text(&line);
        assert!(
            unicode_width::UnicodeWidthStr::width(text.as_str()) <= 40,
            "collapsed summary must fit its render width: {text:?}"
        );
    }

    #[test]
    fn collapsed_summary_is_none_for_empty_list() {
        assert!(render_collapsed_summary(&[], 80).is_none());
    }

    /// REGRESSION: model emits one parent task with 5 subtasks via
    /// `task_board.create({subtasks: [...]})`, but the dashboard only ever
    /// rendered the parent line — subtasks were invisible. This test
    /// pins inline rendering: parent line first, then one indented
    /// row per subtask, with status icons reflecting each subtask's
    /// state.
    #[test]
    fn subtasks_render_indented_under_parent() {
        use super::super::work_board_projection::SessionSubtask;
        let mut parent = mk_task("task-1", "Build expense report system", "in_progress");
        parent.subtasks = vec![
            SessionSubtask {
                id: "exp-1".into(),
                title: "Create project structure".into(),
                description: None,
                status: "completed".into(),
                depends_on: vec![],
                owner: None,
                reason: None,
            },
            SessionSubtask {
                id: "exp-2".into(),
                title: "Implement database layer".into(),
                description: None,
                status: "in_progress".into(),
                depends_on: vec!["exp-1".into()],
                owner: None,
                reason: None,
            },
            SessionSubtask {
                id: "exp-3".into(),
                title: "Create REST API".into(),
                description: None,
                status: "pending".into(),
                depends_on: vec!["exp-2".into()],
                owner: None,
                reason: None,
            },
        ];
        let lines = render(&[parent], 80, 40, true);
        let texts: Vec<String> = lines.iter().map(spans_text).collect();
        // Header carries the subtask roll-up.
        assert!(
            texts[0].contains("1/3 done"),
            "header missing subtask aggregate: {}",
            texts[0]
        );
        // Parent first, then 3 subtasks (4-col indent).
        assert!(
            texts[1].contains("Build expense report system"),
            "parent line: {}",
            texts[1]
        );
        let subtask_lines: Vec<&String> = texts.iter().filter(|t| t.starts_with("    ")).collect();
        assert_eq!(
            subtask_lines.len(),
            3,
            "expected 3 indented subtask rows, got: {texts:#?}"
        );
        assert!(
            subtask_lines
                .iter()
                .any(|t| t.contains("Create project structure"))
        );
        assert!(
            subtask_lines
                .iter()
                .any(|t| t.contains("Implement database layer"))
        );
        // Subtasks waiting on an unfinished dep get a "· waiting"
        // suffix so the user sees why exp-3 isn't running.
        let waiting = subtask_lines
            .iter()
            .find(|t| t.contains("Create REST API"))
            .expect("REST API subtask must render");
        assert!(
            waiting.contains("waiting"),
            "exp-3 depends on exp-2 (in_progress) — expected `waiting` marker: {waiting}"
        );
    }

    #[test]
    fn subtask_global_budget_caps_total_rows() {
        // Many subtasks across multiple parents must be bounded.
        use super::super::work_board_projection::SessionSubtask;
        let mut parents: Vec<SessionTask> = (1..=3)
            .map(|i| mk_task(&format!("task-{i}"), &format!("parent-{i}"), "in_progress"))
            .collect();
        for (idx, parent) in parents.iter_mut().enumerate() {
            parent.subtasks = (0..10)
                .map(|s| SessionSubtask {
                    id: format!("p{idx}-s{s}"),
                    title: format!("sub p{idx}-{s}"),
                    description: None,
                    status: "pending".into(),
                    depends_on: vec![],
                    owner: None,
                    reason: None,
                })
                .collect();
        }
        let lines = render(&parents, 80, 40, true);
        // Sanity: there must be a roll-up footer summarising hidden subtasks.
        let footer = lines
            .iter()
            .map(spans_text)
            .find(|t| t.contains("more subtasks"));
        assert!(
            footer.is_some(),
            "30 total subtasks should overflow and emit a footer: {:#?}",
            lines.iter().map(spans_text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn light_theme_swaps_icon_colours() {
        use crate::tui::theme::Theme;
        let tasks = vec![
            mk_task("task-1", "done", "completed"),
            mk_task("task-2", "running", "in_progress"),
        ];
        let light = TaskBoardColors::from_preset(&Theme::light());
        let lines = render_with_colors(&tasks, 80, 24, false, light);
        // Find the in_progress line and confirm its
        // icon span carries the light-theme accent (deep blue) rather
        // than dark's Cyan.
        let running = lines
            .iter()
            .find(|l| spans_text(l).contains("running"))
            .expect("in_progress line present");
        let icon_span = running.spans.first().expect("icon span");
        assert_eq!(icon_span.style.fg, Some(Theme::light().accent));
        assert_ne!(icon_span.style.fg, Some(Theme::dark().accent));
    }

    // Render-with-fresh just-changed row flash.
    #[test]
    fn render_with_fresh_marks_matching_row_with_flash_glyph() {
        let tasks = vec![
            mk_task("task-1", "cool feature", "in_progress"),
            mk_task("task-2", "boring chore", "pending"),
        ];
        let fresh = vec!["task-1".to_string()];
        let lines = render_with_fresh(&tasks, 80, 40, true, &fresh);
        let texts: Vec<String> = lines.iter().map(spans_text).collect();
        assert!(
            texts
                .iter()
                .any(|t| t.contains("↻") && t.contains("cool feature")),
            "fresh task must be marked with ↻: {texts:?}"
        );
        assert!(
            !texts
                .iter()
                .any(|t| t.contains("↻") && t.contains("boring chore")),
            "non-fresh task must NOT be marked: {texts:?}"
        );
    }

    #[test]
    fn render_with_fresh_empty_list_is_identity_with_render() {
        let tasks = vec![mk_task("task-1", "untouched", "pending")];
        let baseline = render(&tasks, 80, 40, true);
        let overlaid = render_with_fresh(&tasks, 80, 40, true, &[]);
        assert_eq!(
            baseline.len(),
            overlaid.len(),
            "empty fresh list must produce identical line count to render()"
        );
    }

    #[test]
    fn render_with_fresh_predicate_marks_matching_row_without_materializing_ids() {
        let tasks = vec![
            mk_task("task-1", "cool feature", "in_progress"),
            mk_task("task-2", "boring chore", "pending"),
        ];
        let lines =
            render_with_fresh_predicate(&tasks, 80, 40, true, |task_id| task_id == "task-2");
        let texts: Vec<String> = lines.iter().map(spans_text).collect();
        assert!(
            texts
                .iter()
                .any(|t| t.contains("↻") && t.contains("boring chore")),
            "predicate path must mark the matching task: {texts:?}"
        );
        assert!(
            !texts
                .iter()
                .any(|t| t.contains("↻") && t.contains("cool feature")),
            "predicate path must not mark non-matching tasks: {texts:?}"
        );
    }

    #[test]
    fn fresh_marker_uses_task_identity_when_titles_are_duplicated() {
        let mut first = mk_task("task-1", "review migration", "in_progress");
        first.owner = Some("alpha".into());
        let mut second = mk_task("task-2", "review migration", "pending");
        second.owner = Some("beta".into());

        let lines = render_with_fresh_predicate(&[first, second], 100, 40, true, |task_id| {
            task_id == "task-2"
        });
        let row_for_owner = |owner: &str| {
            lines
                .iter()
                .find(|line| {
                    line.spans
                        .iter()
                        .any(|span| span.content.as_ref() == format!(" (@{owner})"))
                })
                .expect("task row for owner")
        };

        assert_ne!(
            row_for_owner("alpha")
                .spans
                .first()
                .map(|span| span.content.as_ref()),
            Some("↻ "),
            "the non-fresh duplicate-title row must stay unmarked"
        );
        assert_eq!(
            row_for_owner("beta")
                .spans
                .first()
                .map(|span| span.content.as_ref()),
            Some("↻ "),
            "the fresh duplicate-title row must be marked by its stable ID"
        );
    }
}
