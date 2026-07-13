//! TUI rendering for the Tier 1 session task board.
//!
//! Renders full current-session tasks and the smaller, truthful
//! [`OpenTaskSummary`] projection used by cross-session views.
//!
//! Rendering rules (matching the reference TUI):
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
//!   in-progress → pending (blocked last within pending) → completed;
//!   append `· N more: M working, K queued, J done` when any tasks
//!   are hidden. Recent-completed TTL state lives in the observer.
//! - **Responsive subject truncation** gated behind available columns.

use astra_tools::task_mgmt::{OpenTaskSummary, SessionTask, SessionTaskStatusKind};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::HashSet;
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
        SessionTaskStatusKind::Pending
        | SessionTaskStatusKind::Archived
        | SessionTaskStatusKind::Deleted
        | SessionTaskStatusKind::Migrated
        | SessionTaskStatusKind::Other => ("◻", colors.dim),
        SessionTaskStatusKind::Failed => ("✖", colors.error),
        SessionTaskStatusKind::Cancelled => ("■", colors.warn),
    }
}

fn is_task_board_tombstone(status: SessionTaskStatusKind) -> bool {
    matches!(
        status,
        SessionTaskStatusKind::Archived
            | SessionTaskStatusKind::Deleted
            | SessionTaskStatusKind::Migrated
    )
}

fn task_is_renderable(task: &SessionTask) -> bool {
    !is_task_board_tombstone(task.status)
}

fn renderable_tasks(tasks: &[SessionTask]) -> Vec<SessionTask> {
    tasks
        .iter()
        .filter(|task| task_is_renderable(task))
        .cloned()
        .collect()
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

/// Display provenance only when both the stable row namespace and typed
/// metadata agree. Ordinary task metadata is user/model supplied, so metadata
/// alone must never let a checklist row impersonate a durable plan step.
fn source_badge(task: &SessionTask) -> Option<&'static str> {
    let source = task
        .metadata
        .as_ref()?
        .get("source")?
        .as_str()
        .map(str::trim)?;
    match (task.id.starts_with("plan:"), source) {
        (true, "plan") => Some(" [plan]"),
        _ => None,
    }
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
    paused: usize,
    failed: usize,
    cancelled: usize,
    other: usize,
}

impl TaskStatusCounts {
    fn open_work(self) -> usize {
        self.in_progress + self.pending + self.paused
    }

    fn status_parts(self) -> Vec<String> {
        let mut parts = Vec::new();
        if self.in_progress > 0 {
            parts.push(format!("{} working", self.in_progress));
        }
        if self.pending > 0 {
            parts.push(format!("{} queued", self.pending));
        }
        if self.paused > 0 {
            parts.push(format!("{} paused", self.paused));
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
        if !task_is_renderable(task) {
            continue;
        }
        match task.status {
            SessionTaskStatusKind::Completed => counts.completed += 1,
            SessionTaskStatusKind::InProgress => counts.in_progress += 1,
            SessionTaskStatusKind::Pending => counts.pending += 1,
            SessionTaskStatusKind::Paused => counts.paused += 1,
            SessionTaskStatusKind::Failed => counts.failed += 1,
            SessionTaskStatusKind::Cancelled => counts.cancelled += 1,
            SessionTaskStatusKind::Other => counts.other += 1,
            SessionTaskStatusKind::Archived
            | SessionTaskStatusKind::Deleted
            | SessionTaskStatusKind::Migrated => {}
        }
    }
    counts
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
fn prioritize<'a>(tasks: &'a [SessionTask], unresolved: &HashSet<String>) -> Vec<&'a SessionTask> {
    let in_progress = sort_by_id_asc(tasks.iter().filter(|t| t.status.is_in_progress()).collect());
    let mut pending: Vec<&SessionTask> = tasks.iter().filter(|t| t.status.is_pending()).collect();
    pending.sort_by(|a, b| {
        let a_blocked = a.blocked_by.iter().any(|id| unresolved.contains(id));
        let b_blocked = b.blocked_by.iter().any(|id| unresolved.contains(id));
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
            .filter(|t| t.status == SessionTaskStatusKind::Paused)
            .collect(),
    );
    let completed = sort_by_id_asc(tasks.iter().filter(|t| t.status.is_completed()).collect());
    let terminal = sort_by_id_asc(
        tasks
            .iter()
            .filter(|t| {
                matches!(
                    t.status,
                    SessionTaskStatusKind::Cancelled | SessionTaskStatusKind::Failed
                )
            })
            .collect(),
    );
    let diagnostics = sort_by_id_asc(
        tasks
            .iter()
            .filter(|t| matches!(t.status, SessionTaskStatusKind::Other))
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

fn render_task_line(
    task: &SessionTask,
    open_blockers: &[String],
    columns: u16,
    colors: TaskBoardColors,
) -> Line<'static> {
    let (icon, color) = status_icon_and_color(&task.status, colors);
    let is_completed = task.status.is_completed();
    let is_in_progress = task.status.is_in_progress();
    let is_blocked = !open_blockers.is_empty();

    let source_badge = source_badge(task);
    let owner_badge = owner_badge(task);
    let subject = truncate_to_width(
        &task.title,
        max_subject_width(columns).saturating_sub(
            source_badge
                .map(unicode_width::UnicodeWidthStr::width)
                .unwrap_or(0)
                + owner_badge
                    .as_deref()
                    .map(unicode_width::UnicodeWidthStr::width)
                    .unwrap_or(0),
        ),
    );

    let mut spans: Vec<Span<'static>> = Vec::new();
    // In-progress and completed get BOLD; pending and fallback statuses
    // already map to `colors.dim`, so adding DIM on top of dim would be
    // invisible — skip the modifier in that case.
    let icon_style = match task.status {
        SessionTaskStatusKind::InProgress | SessionTaskStatusKind::Completed => {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        }
        SessionTaskStatusKind::Failed => Style::default().fg(color).add_modifier(Modifier::BOLD),
        SessionTaskStatusKind::Cancelled => Style::default().fg(color).add_modifier(Modifier::BOLD),
        SessionTaskStatusKind::Paused => Style::default().fg(color).add_modifier(Modifier::BOLD),
        SessionTaskStatusKind::Pending
        | SessionTaskStatusKind::Archived
        | SessionTaskStatusKind::Deleted
        | SessionTaskStatusKind::Migrated
        | SessionTaskStatusKind::Other => Style::default().fg(color).add_modifier(Modifier::DIM),
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

    if let Some(source_badge) = source_badge {
        spans.push(Span::styled(
            source_badge,
            Style::default().fg(colors.dim).add_modifier(Modifier::DIM),
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
            SessionTaskStatusKind::Pending
            | SessionTaskStatusKind::Archived
            | SessionTaskStatusKind::Deleted
            | SessionTaskStatusKind::Migrated
            | SessionTaskStatusKind::Other => {
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
            if !task_is_renderable(task) {
                return counts;
            }
            match task.status {
                SessionTaskStatusKind::Completed => counts.completed += 1,
                SessionTaskStatusKind::InProgress => counts.in_progress += 1,
                SessionTaskStatusKind::Pending => counts.pending += 1,
                SessionTaskStatusKind::Paused => counts.paused += 1,
                SessionTaskStatusKind::Failed => counts.failed += 1,
                SessionTaskStatusKind::Cancelled => counts.cancelled += 1,
                SessionTaskStatusKind::Other => counts.other += 1,
                SessionTaskStatusKind::Archived
                | SessionTaskStatusKind::Deleted
                | SessionTaskStatusKind::Migrated => {}
            }
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

/// Cross-session render. Walks the `per_session` vec produced by
/// `TaskStore::load_open_task_summaries` and emits a dim header row per
/// session followed by that session's active tasks. Returns empty
/// when every session is empty of active work or when the terminal
/// is too short to fit any tasks.
pub fn render_multi(
    per_session: &[(String, Vec<OpenTaskSummary>)],
    columns: u16,
    rows: u16,
) -> Vec<Line<'static>> {
    render_multi_with_colors(per_session, columns, rows, TaskBoardColors::from_theme())
}

pub fn render_multi_with_colors(
    per_session: &[(String, Vec<OpenTaskSummary>)],
    columns: u16,
    rows: u16,
    colors: TaskBoardColors,
) -> Vec<Line<'static>> {
    render_multi_with_colors_and_cap(per_session, columns, max_display(rows), colors)
}

/// Render every already-fetched cross-session row. This is for the primary
/// task-board canvas, which owns scrolling; compact chat deliberately remains
/// bounded by terminal height through [`render_multi`].
pub(crate) fn render_multi_full(
    per_session: &[(String, Vec<OpenTaskSummary>)],
    columns: u16,
) -> Vec<Line<'static>> {
    render_multi_with_colors_and_cap(
        per_session,
        columns,
        usize::MAX,
        TaskBoardColors::from_theme(),
    )
}

fn render_multi_with_colors_and_cap(
    per_session: &[(String, Vec<OpenTaskSummary>)],
    columns: u16,
    total_cap: usize,
    colors: TaskBoardColors,
) -> Vec<Line<'static>> {
    // Reuse the single-session capacity formula per session: we
    // render a header line + up to N task lines per group, and cut
    // the list once we've burned our total row budget.
    if total_cap == 0 {
        return Vec::new();
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut rows_used = 0usize;
    // Track whether we had to drop entire sessions so the caller
    // can decide to render a trailing "…" marker. Without it, a
    // row-budget clip looks like the cross-session view is just
    // showing the first session.
    let mut overflow = false;

    for (session_id, tasks) in per_session {
        // Collapse each session's active subset — completed tasks
        // across sessions are not actionable and would clutter the
        // cross-session overview. The single-session board still
        // shows its own completed history.
        let active: Vec<&OpenTaskSummary> =
            tasks.iter().filter(|t| t.status.is_open_work()).collect();
        if active.is_empty() {
            continue;
        }

        // Header row: calm session label + working count.
        let counts = active
            .iter()
            .fold(TaskStatusCounts::default(), |mut counts, task| {
                match task.status {
                    SessionTaskStatusKind::InProgress => counts.in_progress += 1,
                    SessionTaskStatusKind::Pending => counts.pending += 1,
                    SessionTaskStatusKind::Paused => counts.paused += 1,
                    _ => {}
                }
                counts
            });
        let header_parts = counts.status_parts();
        let header_status = if header_parts.is_empty() {
            format!(" · {} open", active.len())
        } else {
            format!(" · {}", header_parts.join(" · "))
        };
        let short: String = session_id.chars().take(8).collect();
        let header = Line::from(vec![
            Span::styled(
                format!("Session {short}"),
                Style::default().fg(colors.dim).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                header_status,
                Style::default().fg(colors.dim).add_modifier(Modifier::DIM),
            ),
        ]);
        if rows_used >= total_cap {
            overflow = true;
            break;
        }
        out.push(header);
        rows_used += 1;

        // One row per active task (up to the remaining budget).
        for task in active {
            if rows_used >= total_cap {
                overflow = true;
                break;
            }
            let (icon, icon_color) = status_icon_and_color(&task.status, colors);
            let subject_w = (columns as usize).saturating_sub(6);
            let subject = truncate_to_width(&task.title, subject_w);
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("{icon} "),
                    Style::default().fg(icon_color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(subject),
            ]));
            rows_used += 1;
        }
        if overflow {
            break;
        }
    }

    if overflow {
        // Replace the last row with the "…" marker so the total
        // footprint stays at total_cap — otherwise one extra line
        // slips past the budget and the caller's layout math is
        // off by one on tight terminals.
        if out.len() >= total_cap
            && let Some(last) = out.last_mut()
        {
            *last = Line::from(vec![Span::styled(
                "  … more sessions",
                Style::default().fg(colors.dim),
            )]);
        } else {
            out.push(Line::from(vec![Span::styled(
                "  … more sessions",
                Style::default().fg(colors.dim),
            )]));
        }
    }

    out
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
    let filtered_tasks;
    let tasks = if tasks.iter().all(task_is_renderable) {
        tasks
    } else {
        filtered_tasks = renderable_tasks(tasks);
        filtered_tasks.as_slice()
    };
    let cap = task_cap;
    if cap == 0 || tasks.is_empty() {
        return TaskListRender {
            lines: Vec::new(),
            selected_line_index: None,
        };
    }
    let counts = counts(tasks);
    let unresolved: HashSet<String> = tasks
        .iter()
        .filter(|t| !t.status.is_completed())
        .map(|t| t.id.clone())
        .collect();

    let prioritized = prioritize(tasks, &unresolved);
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
        let open_blockers: Vec<String> = task
            .blocked_by
            .iter()
            .filter(|id| unresolved.contains(*id))
            .cloned()
            .collect();
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
    let filtered_tasks;
    let tasks = if tasks.iter().all(task_is_renderable) {
        tasks
    } else {
        filtered_tasks = renderable_tasks(tasks);
        filtered_tasks.as_slice()
    };
    if tasks.is_empty() {
        return None;
    }
    let counts = counts(tasks);
    let total = tasks.len();
    let current_task = tasks
        .iter()
        .find(|t| t.status.is_in_progress())
        .or_else(|| tasks.iter().find(|t| t.status.is_pending()))
        .or_else(|| {
            tasks
                .iter()
                .find(|t| t.status == SessionTaskStatusKind::Paused)
        });
    let (sub_done, sub_total) = subtask_counts(tasks);

    let theme = crate::tui::theme::current();
    let icon = if counts.in_progress > 0 {
        "•"
    } else if counts.paused > 0 {
        "⏸"
    } else if counts.completed == total {
        "✔"
    } else {
        "·"
    };
    let icon_color = if counts.in_progress > 0 {
        theme.accent
    } else if counts.paused > 0 {
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
    if counts.paused > 0 {
        spans.push(Span::styled(
            format!(" · {} paused", counts.paused),
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

/// One-line summary for the typed cross-session projection. Unlike the
/// full-task variant, this never implies that omitted history or subtasks were
/// loaded; it reports only the open rows the server confirmed.
pub fn render_collapsed_multi_summary(
    tasks: &[OpenTaskSummary],
    columns: u16,
) -> Option<Line<'static>> {
    let tasks: Vec<&OpenTaskSummary> = tasks
        .iter()
        .filter(|task| task.status.is_open_work())
        .collect();
    if tasks.is_empty() {
        return None;
    }
    let counts = tasks
        .iter()
        .fold(TaskStatusCounts::default(), |mut counts, task| {
            match task.status {
                SessionTaskStatusKind::InProgress => counts.in_progress += 1,
                SessionTaskStatusKind::Pending => counts.pending += 1,
                SessionTaskStatusKind::Paused => counts.paused += 1,
                _ => {}
            }
            counts
        });
    let current_task = tasks
        .iter()
        .find(|task| task.status.is_in_progress())
        .or_else(|| tasks.iter().find(|task| task.status.is_pending()))
        .or_else(|| tasks.first())
        .copied();
    let theme = crate::tui::theme::current();
    let (icon, icon_color) = if counts.in_progress > 0 {
        ("•", theme.accent)
    } else if counts.paused > 0 {
        ("⏸", theme.warn)
    } else {
        ("·", theme.dim)
    };
    let mut spans = vec![
        Span::styled(
            format!("{icon} "),
            Style::default().fg(icon_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled("Tasks", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            " · all sessions",
            Style::default().fg(theme.dim).add_modifier(Modifier::DIM),
        ),
    ];
    for (count, label) in [
        (counts.in_progress, "working"),
        (counts.pending, "queued"),
        (counts.paused, "paused"),
    ] {
        if count > 0 {
            spans.push(Span::styled(
                format!(" · {count} {label}"),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
    }
    if let Some(task) = current_task {
        let used: usize = spans.iter().map(|span| span.content.width()).sum();
        let separator = " · ";
        let title_budget = (columns as usize).saturating_sub(used + separator.width());
        if title_budget > 0 {
            spans.push(Span::styled(
                separator,
                Style::default().add_modifier(Modifier::DIM),
            ));
            spans.push(Span::raw(truncate_to_width(&task.title, title_budget)));
        }
    }
    let used: usize = spans.iter().map(|span| span.content.width()).sum();
    if used + TASK_BOARD_TOGGLE_HINT.width() <= columns as usize {
        spans.push(Span::styled(
            TASK_BOARD_TOGGLE_HINT,
            Style::default().fg(theme.dim).add_modifier(Modifier::DIM),
        ));
    }
    Some(Line::from(spans))
}

/// One-line "Focus · <subject>" nudge for use when `expanded_view` is not
/// `Tasks` but a task is in flight. Matches the reference TUI's Spinner
/// fallback at `components/Spinner.tsx:296`.
pub fn render_next_hint(tasks: &[SessionTask], columns: u16) -> Option<Line<'static>> {
    // Pick the first in-progress task, else first pending.
    let candidate = tasks
        .iter()
        .find(|t| t.status.is_in_progress())
        .or_else(|| tasks.iter().find(|t| t.status.is_pending()))?;
    let subject = truncate_to_width(
        &candidate.title,
        max_subject_width(columns).saturating_sub(9), // "Focus · "
    );
    Some(Line::from(Span::styled(
        format!("Focus · {}", subject),
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
    use super::*;
    use astra_tools::task_mgmt::{SessionSubtask as _S, SessionTask};

    fn mk_task(id: &str, title: &str, status: &str) -> SessionTask {
        SessionTask {
            archived_at: None,
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

    fn mk_summary(id: &str, title: &str, status: &str) -> OpenTaskSummary {
        OpenTaskSummary {
            id: id.into(),
            title: title.into(),
            status: status.into(),
            updated_at: "now".into(),
        }
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
    fn durable_plan_rows_are_visibly_distinct_from_checklist_rows() {
        let mut task = mk_task("plan:plan-7:step-1", "migrate state", "in_progress");
        task.metadata = Some(serde_json::Map::from_iter([(
            "source".to_string(),
            serde_json::Value::String("plan".to_string()),
        )]));

        let line = render_task_line(&task, &[], 80, TaskBoardColors::from_theme());
        assert!(spans_text(&line).contains("[plan]"));
    }

    #[test]
    fn checklist_metadata_cannot_spoof_a_durable_plan_row() {
        let mut task = mk_task("task-1", "ordinary checklist", "pending");
        task.metadata = Some(serde_json::Map::from_iter([(
            "source".to_string(),
            serde_json::Value::String("plan".to_string()),
        )]));

        let line = render_task_line(&task, &[], 80, TaskBoardColors::from_theme());
        assert!(!spans_text(&line).contains("[plan]"));
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
    fn deleted_archived_and_migrated_tasks_do_not_render_as_other() {
        let tasks = vec![
            mk_task("task-1", "deleted", "deleted"),
            mk_task("task-2", "archived", "archived"),
            mk_task("task-3", "migrated", "migrated"),
        ];
        let lines = render(&tasks, 80, 40, true);
        assert!(lines.is_empty(), "tombstones must not render: {lines:?}");

        let summary = render_collapsed_summary(&tasks, 80);
        assert!(
            summary.is_none(),
            "tombstones must not render collapsed summary"
        );
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
    fn collapsed_summary_includes_subtask_rollup_when_present() {
        use astra_tools::task_mgmt::SessionSubtask;
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
        use astra_tools::task_mgmt::SessionSubtask;
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
        use astra_tools::task_mgmt::SessionSubtask;
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
        use astra_tools::task_mgmt::SessionSubtask;
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

    // Suppress dead_code on SessionSubtask import noise.
    #[allow(dead_code)]
    fn _ensure_session_subtask_import(_: _S) {}

    // ── render_multi: cross-session layout ───────────────────────

    fn fixture_colors() -> TaskBoardColors {
        TaskBoardColors::from_preset(&crate::tui::theme::Theme::dark())
    }

    #[test]
    fn render_multi_empty_input_yields_empty() {
        let out = render_multi_with_colors(&[], 80, 40, fixture_colors());
        assert!(out.is_empty());
    }

    #[test]
    fn render_multi_skips_sessions_with_no_open_work() {
        // All-completed sessions are still open on disk but contribute
        // nothing actionable; the cross-session view prunes them so
        // the row budget isn't burned on dim history.
        let input = vec![(
            "sess-done".to_string(),
            vec![mk_summary("task-1", "finished", "completed")],
        )];
        let out = render_multi_with_colors(&input, 80, 40, fixture_colors());
        assert!(
            out.is_empty(),
            "all-completed session must be pruned: {:?}",
            out.iter().map(spans_text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn render_multi_emits_session_header_then_open_tasks() {
        let input = vec![(
            "0123456789ab".to_string(),
            vec![
                mk_summary("task-1", "open one", "pending"),
                mk_summary("task-2", "done one", "completed"),
                mk_summary("task-3", "busy one", "in_progress"),
                mk_summary("task-4", "paused one", "paused"),
            ],
        )];
        let out = render_multi_with_colors(&input, 80, 40, fixture_colors());
        let texts: Vec<String> = out.iter().map(spans_text).collect();
        assert!(
            texts.iter().any(|t| t.contains("01234567")),
            "short session id header missing: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("1 working")
                && t.contains("1 queued")
                && t.contains("1 paused")),
            "open-work counts missing from header: {texts:?}"
        );
        assert!(texts.iter().any(|t| t.contains("open one")));
        assert!(texts.iter().any(|t| t.contains("busy one")));
        assert!(texts.iter().any(|t| t.contains("paused one")));
        assert!(
            !texts.iter().any(|t| t.contains("done one")),
            "completed task must not appear on cross-session view: {texts:?}"
        );
    }

    #[test]
    fn render_multi_respects_row_budget_across_sessions() {
        // Small terminal → max_display=3. Two sessions × 2 active
        // tasks each = 4 task rows + 2 headers = 6 lines, but we
        // only have 3 slots. Expect truncation with "…" marker.
        let input = vec![
            (
                "sess-a".to_string(),
                vec![
                    mk_summary("a1", "a1", "pending"),
                    mk_summary("a2", "a2", "pending"),
                ],
            ),
            (
                "sess-b".to_string(),
                vec![
                    mk_summary("b1", "b1", "pending"),
                    mk_summary("b2", "b2", "pending"),
                ],
            ),
        ];
        let out = render_multi_with_colors(&input, 80, 17, fixture_colors());
        assert_eq!(
            out.len(),
            3,
            "budget must cap at max_display(17)=3 rows even across sessions: {}",
            out.len()
        );
        // Last row should be the "…" truncation marker so the user
        // knows there's more.
        let last = spans_text(out.last().unwrap());
        assert!(last.contains('…'), "truncation marker missing: {last}");
    }

    #[test]
    fn render_multi_full_leaves_viewport_paging_to_the_primary_board() {
        let input = vec![(
            "sess-full".to_string(),
            (1..=12)
                .map(|id| mk_summary(&format!("task-{id}"), &format!("row-{id:02}"), "pending"))
                .collect(),
        )];

        let out = render_multi_full(&input, 80);
        assert_eq!(
            out.len(),
            13,
            "header plus every open task must be available"
        );
        assert!(
            spans_text(out.last().expect("last task row")).contains("row-12"),
            "the primary board, not compact chat, owns paging"
        );
    }

    #[test]
    fn render_multi_empty_when_terminal_too_short() {
        let input = vec![("sess".to_string(), vec![mk_summary("a1", "a", "pending")])];
        let out = render_multi_with_colors(&input, 80, 8, fixture_colors());
        assert!(
            out.is_empty(),
            "rows<=10 must render nothing (same invariant as single-session)"
        );
    }

    // ── render_with_fresh: just-changed row flash ────────────────

    // Collapsed cross-session projection.
    #[test]
    fn collapsed_multi_summary_reports_only_confirmed_open_projection() {
        let tasks = vec![
            mk_summary("task-1", "current remote work", "in_progress"),
            mk_summary("task-2", "later remote work", "pending"),
            mk_summary("task-3", "paused remote work", "paused"),
            mk_summary("task-4", "historical row", "completed"),
        ];
        let line = render_collapsed_multi_summary(&tasks, 120).expect("summary");
        let text = spans_text(&line);
        assert!(text.contains("all sessions"), "{text}");
        assert!(text.contains("1 working"), "{text}");
        assert!(text.contains("1 queued"), "{text}");
        assert!(text.contains("1 paused"), "{text}");
        assert!(text.contains("current remote work"), "{text}");
        assert!(
            !text.contains("done") && !text.contains("historical row"),
            "the open summary must not imply unloaded history: {text}"
        );
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
