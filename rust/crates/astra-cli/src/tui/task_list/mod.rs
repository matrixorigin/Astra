//! TUI rendering for the Tier 1 session task board.
//!
//! Port of the reference TUI's `TaskListV2.tsx`. Takes a `&[SessionTask]` and
//! produces `Vec<Line<'static>>` that callers merge into their own render
//! region (mounted under chat_widget's standalone area and under the
//! spinner's footer — see `docs/plans/tui-task-board.md` §3.3).
//!
//! Rendering rules (matching the reference TUI):
//!
//! - **Visibility**: hidden when `rows <= 10`.
//! - **maxDisplay**: `min(10, max(3, rows - 14))`.
//! - **Icons by status**: `✓` (completed, green), `■` (in_progress,
//!   accent), `□` (pending, dim).
//! - **Subject style**: bold for in_progress, strikethrough for
//!   completed, dim for completed or blocked.
//! - **Blocked-by badge**: appended `› blocked by #1, #3` when the task
//!   has any unresolved blockers.
//! - **Standalone header**: optional `N tasks (K done, M in progress, J
//!   open)` line above the list.
//! - **Truncation** when `tasks.len() > maxDisplay`: prioritize
//!   in-progress → pending (blocked last within pending) → completed;
//!   append `… +N in progress, M pending, K completed` when any tasks
//!   are hidden. Recent-completed 30s TTL tracking is Phase 4.2.
//! - **Responsive subject truncation** gated behind available columns.
//!
//! Phase 4.1 explicitly skips:
//!
//! - Owner badge (`(@agent)`) — depends on active-teammate tracking in
//!   TUI state that we don't expose yet.
//! - Activity line (rolled-up recent tool calls) — depends on the same.
//! - Recent-completed prioritization with 30s TTL — widget is stateless
//!   here; the observer tracks completion timestamps in Phase 4.2.

use astra_tools::task_mgmt::SessionTask;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::HashSet;
use unicode_width::UnicodeWidthStr;

use crate::cli::session_task_surface::SessionTaskStatusKind;

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
}

impl TaskBoardColors {
    /// Build from the process-wide theme.
    pub fn from_theme() -> Self {
        let t = crate::tui::theme::current();
        Self {
            accent: t.accent,
            success: t.success,
            dim: t.dim,
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
        }
    }
}

/// Per-status palette. Glyphs mirror the reference TUI's `figures`
/// library (tick / squareSmallFilled / squareSmall) so users coming
/// from that interface read the list the same way.
/// Spinner frames for in_progress tasks. Cycles every 100ms.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner_frame() -> &'static str {
    #[cfg(test)]
    {
        SPINNER_FRAMES[0] // deterministic in tests
    }
    #[cfg(not(test))]
    {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let idx = (ms / 100) as usize % SPINNER_FRAMES.len();
        SPINNER_FRAMES[idx]
    }
}

fn status_icon_and_color(status: &SessionTaskStatusKind, colors: TaskBoardColors) -> (&'static str, Color) {
    match status {
        SessionTaskStatusKind::Completed => ("✔", colors.success),
        SessionTaskStatusKind::InProgress => (spinner_frame(), colors.accent),
        SessionTaskStatusKind::Pending
        | SessionTaskStatusKind::Archived
        | SessionTaskStatusKind::Deleted
        | SessionTaskStatusKind::Other => ("◻", colors.dim),
        SessionTaskStatusKind::Failed => ("✖", Color::Red),
        SessionTaskStatusKind::Cancelled => ("■", Color::Yellow),
    }
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

fn counts(tasks: &[SessionTask]) -> (usize, usize, usize) {
    let completed = tasks
        .iter()
        .filter(|t| t.status.is_completed())
        .count();
    let pending = tasks
        .iter()
        .filter(|t| t.status.is_pending())
        .count();
    let in_progress = tasks.len().saturating_sub(completed + pending);
    (completed, in_progress, pending)
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
/// in_progress → pending (open blockers last) → completed.
fn prioritize<'a>(tasks: &'a [SessionTask], unresolved: &HashSet<String>) -> Vec<&'a SessionTask> {
    let in_progress = sort_by_id_asc(
        tasks
            .iter()
            .filter(|t| t.status.is_in_progress())
            .collect(),
    );
    let mut pending: Vec<&SessionTask> = tasks
        .iter()
        .filter(|t| t.status.is_pending())
        .collect();
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
    let completed = sort_by_id_asc(
        tasks
            .iter()
            .filter(|t| t.status.is_completed())
            .collect(),
    );
    let mut out: Vec<&SessionTask> = Vec::with_capacity(tasks.len());
    out.extend(in_progress);
    out.extend(pending);
    out.extend(completed);
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
    let icon_style = match task.status {
        SessionTaskStatusKind::InProgress | SessionTaskStatusKind::Completed => {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        }
        SessionTaskStatusKind::Failed => Style::default().fg(color).add_modifier(Modifier::BOLD),
        SessionTaskStatusKind::Cancelled => Style::default().fg(color).add_modifier(Modifier::BOLD),
        SessionTaskStatusKind::Pending
        | SessionTaskStatusKind::Archived
        | SessionTaskStatusKind::Deleted
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
            format!(" › blocked by {}", rendered),
            Style::default().add_modifier(Modifier::DIM),
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
            SessionTaskStatusKind::Pending
            | SessionTaskStatusKind::Archived
            | SessionTaskStatusKind::Deleted
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
    let (completed, in_progress, pending) = {
        let c = hidden
            .iter()
            .filter(|t| t.status.is_completed())
            .count();
        let p = hidden
            .iter()
            .filter(|t| t.status.is_pending())
            .count();
        let ip = hidden.len().saturating_sub(c + p);
        (c, ip, p)
    };
    let mut parts: Vec<String> = Vec::new();
    if in_progress > 0 {
        parts.push(format!("{} in progress", in_progress));
    }
    if pending > 0 {
        parts.push(format!("{} pending", pending));
    }
    if completed > 0 {
        parts.push(format!("{} completed", completed));
    }
    Some(Line::from(Span::styled(
        format!(" … +{}", parts.join(", ")),
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
    mut is_fresh: F,
) -> Vec<Line<'static>>
where
    F: FnMut(&str) -> bool,
{
    let mut lines = render_with_colors(
        tasks,
        columns,
        rows,
        standalone,
        TaskBoardColors::from_theme(),
    );
    // Row positions aren't directly keyed by task id, so the
    // simplest safe approach is to re-scan the same prioritized
    // order the renderer used and flip ↺ glyph in for any row whose
    // title matches a fresh id's title. The renderer doesn't expose
    // the mapping, so we do a lightweight second pass that finds
    // lines matching fresh task titles and prepends a flash marker.
    //
    // This is deliberately conservative: we never reshape an
    // existing line's layout, just swap the two-space pad before
    // the bullet for a ↺ glyph tinted accent.
    let theme = crate::tui::theme::current();
    let flash_color = theme.accent;
    for task in tasks {
        if !is_fresh(&task.id) {
            continue;
        }
        for line in lines.iter_mut() {
            // A task row starts with the status-icon span — "✔ " /
            // "◼ " / "◻ " / "· ". Inspect the first span to avoid
            // accidentally matching the summary header line ("N
            // tasks (K done, …)") which also contains titles later.
            let first_text = line.spans.first().map(|s| s.content.as_ref()).unwrap_or("");
            let is_task_row = first_text == "✔ "
                || first_text == "◻ "
                || first_text == "· "
                || (first_text.ends_with(' ')
                    && first_text.trim().len() <= 4
                    && SPINNER_FRAMES.iter().any(|f| first_text.starts_with(f)));
            if !is_task_row {
                continue;
            }
            let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if joined.contains(&task.title) {
                // Prepend a flash glyph. Keeps existing spans intact
                // so colours/modifiers on the icon + title don't drift.
                line.spans.insert(
                    0,
                    Span::styled(
                        "↻ ",
                        Style::default()
                            .fg(flash_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                );
                break;
            }
        }
    }
    lines
}

/// Cross-session render. Walks the `per_session` vec (as produced by
/// [`TaskStore::load_all_sessions`]) and emits a dim header row per
/// session followed by that session's active tasks. Returns empty
/// when every session is empty of active work or when the terminal
/// is too short to fit any tasks.
pub fn render_multi(
    per_session: &[(String, Vec<SessionTask>)],
    columns: u16,
    rows: u16,
) -> Vec<Line<'static>> {
    render_multi_with_colors(per_session, columns, rows, TaskBoardColors::from_theme())
}

pub fn render_multi_with_colors(
    per_session: &[(String, Vec<SessionTask>)],
    columns: u16,
    rows: u16,
    colors: TaskBoardColors,
) -> Vec<Line<'static>> {
    // Reuse the single-session capacity formula per session: we
    // render a header line + up to N task lines per group, and cut
    // the list once we've burned our total row budget.
    let total_cap = max_display(rows);
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
        let active: Vec<&SessionTask> = tasks
            .iter()
            .filter(|t| t.status.is_active())
            .collect();
        if active.is_empty() {
            continue;
        }

        // Header row: dim short session id + active count.
        let short: String = session_id.chars().take(8).collect();
        let header = Line::from(vec![
            Span::styled(
                format!("▸ {short}"),
                Style::default().fg(colors.dim).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ({} active)", active.len()),
                Style::default().fg(colors.dim),
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
                Span::styled(icon.to_string(), Style::default().fg(icon_color)),
                Span::raw(" "),
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
            *last = Line::from(vec![Span::styled("  …", Style::default().fg(colors.dim))]);
        } else {
            out.push(Line::from(vec![Span::styled(
                "  …",
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
    let cap = max_display(rows);
    if cap == 0 || tasks.is_empty() {
        return Vec::new();
    }
    let (completed, in_progress, pending) = counts(tasks);
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

    if standalone {
        let mut header_spans: Vec<Span<'static>> = Vec::new();
        header_spans.push(Span::styled(
            format!("{}", tasks.len()),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        header_spans.push(Span::styled(
            " tasks (".to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ));
        header_spans.push(Span::styled(
            format!("{}", completed),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        header_spans.push(Span::styled(
            " done".to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ));
        if in_progress > 0 {
            header_spans.push(Span::styled(
                ", ".to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ));
            header_spans.push(Span::styled(
                format!("{}", in_progress),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            header_spans.push(Span::styled(
                " in progress".to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        header_spans.push(Span::styled(
            ", ".to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ));
        header_spans.push(Span::styled(
            format!("{}", pending),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        header_spans.push(Span::styled(
            " open)".to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ));
        // Subtask roll-up: when any task fans out into subtasks, show
        // aggregate progress so a "1 task in progress" header doesn't
        // hide the 2/5 subtasks that already shipped.
        let (sub_done, sub_total) = subtask_counts(tasks);
        if sub_total > 0 {
            header_spans.push(Span::styled(
                format!(" · {sub_done}/{sub_total} subtasks done"),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        // Ctrl+T collapse hint. Reference TUI appends this to the
        // standalone header so new users discover the toggle without
        // hunting help. Drop when columns are too narrow to fit it.
        let hint = "  Ctrl+T to collapse";
        let header_w: usize = header_spans.iter().map(|s| s.content.width()).sum();
        if header_w + hint.width() < columns as usize {
            header_spans.push(Span::styled(
                hint.to_string(),
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
        out.push(render_task_line(task, &open_blockers, columns, colors));
        // Per-parent cap (keeps one runaway parent from monopolising
        // the global budget) plus the global cap above. claude-code
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
    out
}

/// One-line "compact" summary used as the default board view while the
/// user hasn't pressed Ctrl+T. Replaces the full panel during running
/// turns so the spinner / streaming region stays uncluttered.
///
/// Format: `⠋ N tasks · current: <title> · K/M subtasks · Ctrl+T expand`
/// (the "current" segment shows the in-progress task title, falling
/// back to "next" when nothing is in progress yet; subtask roll-up
/// only appears when any subtask exists).
///
/// Returns `None` for empty task lists — caller renders nothing in
/// that case.
pub fn render_collapsed_summary(tasks: &[SessionTask], columns: u16) -> Option<Line<'static>> {
    if tasks.is_empty() {
        return None;
    }
    let (completed, in_progress, _pending) = counts(tasks);
    let total = tasks.len();
    let current_task = tasks
        .iter()
        .find(|t| t.status.is_in_progress())
        .or_else(|| tasks.iter().find(|t| t.status.is_pending()));
    let (sub_done, sub_total) = subtask_counts(tasks);

    let theme = crate::tui::theme::current();
    let icon = if in_progress > 0 {
        spinner_frame()
    } else if completed == total {
        "✔"
    } else {
        "·"
    };
    let icon_color = if in_progress > 0 {
        theme.accent
    } else if completed == total {
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
        format!("{total} task{}", if total == 1 { "" } else { "s" }),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        format!(" ({completed} done"),
        Style::default().add_modifier(Modifier::DIM),
    ));
    if in_progress > 0 {
        spans.push(Span::styled(
            format!(", {in_progress} running"),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    spans.push(Span::styled(
        ")".to_string(),
        Style::default().add_modifier(Modifier::DIM),
    ));

    if sub_total > 0 {
        spans.push(Span::styled(
            format!(" · {sub_done}/{sub_total} subtasks"),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }

    if let Some(task) = current_task {
        // Trim the title to whatever space is left after the rest of
        // the line so we don't blow past `columns`.
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        let hint = "  Ctrl+T expand";
        let reserved = used + " · ".width() + hint.width();
        let title_budget = (columns as usize).saturating_sub(reserved).max(8);
        let title = truncate_to_width(&task.title, title_budget);
        spans.push(Span::styled(
            " · ".to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ));
        spans.push(Span::styled(title, Style::default()));
    }

    let hint = "  Ctrl+T expand";
    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    if used + hint.width() < columns as usize {
        spans.push(Span::styled(
            hint.to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }

    Some(Line::from(spans))
}

/// One-line "Next: <subject>" nudge for use when `expanded_view` is not
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
        max_subject_width(columns).saturating_sub(6), // "Next: "
    );
    Some(Line::from(Span::styled(
        format!("Next: {}", subject),
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
        assert!(header.contains("3 tasks"), "header: {header}");
        assert!(header.contains("1 done"));
        assert!(header.contains("1 in progress"));
        assert!(header.contains("1 open"));
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
    fn priority_order_is_in_progress_then_pending_then_completed() {
        let tasks = vec![
            mk_task("task-1", "first-completed", "completed"),
            mk_task("task-2", "first-pending", "pending"),
            mk_task("task-3", "first-in-progress", "in_progress"),
        ];
        let lines = render(&tasks, 80, 40, false);
        // Three visible lines, no header (standalone=false), no truncation.
        assert_eq!(lines.len(), 3);
        let texts: Vec<String> = lines.iter().map(spans_text).collect();
        let pos = |needle: &str| texts.iter().position(|l| l.contains(needle)).unwrap();
        assert!(pos("first-in-progress") < pos("first-pending"));
        assert!(pos("first-pending") < pos("first-completed"));
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
        assert!(summary.contains("+"), "summary: {summary}");
        assert!(summary.contains("5 pending"), "summary: {summary}");
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
        assert!(text.contains("› blocked by"), "{text}");
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
        assert!(text.starts_with("Next: "), "{text}");
    }

    #[test]
    fn next_hint_returns_none_when_all_completed() {
        let tasks = vec![mk_task("task-1", "done", "completed")];
        assert!(render_next_hint(&tasks, 80).is_none());
    }

    #[test]
    fn collapsed_summary_shows_counts_current_and_hint() {
        let tasks = vec![
            mk_task("task-1", "alpha-done", "completed"),
            mk_task("task-2", "beta-running", "in_progress"),
            mk_task("task-3", "gamma-pending", "pending"),
        ];
        let line = render_collapsed_summary(&tasks, 100).expect("non-empty");
        let text = spans_text(&line);
        assert!(text.contains("3 tasks"), "{text}");
        assert!(text.contains("1 done"), "{text}");
        assert!(text.contains("1 running"), "{text}");
        // The current-task title should be the in_progress one, not
        // the completed one.
        assert!(text.contains("beta-running"), "{text}");
        assert!(!text.contains("alpha-done"), "{text}");
        assert!(text.contains("Ctrl+T expand"), "{text}");
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
            },
            SessionSubtask {
                id: "s2".into(),
                title: "second".into(),
                description: None,
                status: "in_progress".into(),
                depends_on: vec![],
                owner: None,
            },
        ];
        let line = render_collapsed_summary(&[parent], 100).expect("non-empty");
        let text = spans_text(&line);
        assert!(text.contains("1/2 subtasks"), "{text}");
    }

    #[test]
    fn collapsed_summary_is_none_for_empty_list() {
        assert!(render_collapsed_summary(&[], 80).is_none());
    }

    /// REGRESSION: model emits one parent task with 5 subtasks via
    /// `task.create({subtasks: [...]})`, but the dashboard only ever
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
            },
            SessionSubtask {
                id: "exp-2".into(),
                title: "Implement database layer".into(),
                description: None,
                status: "in_progress".into(),
                depends_on: vec!["exp-1".into()],
                owner: None,
            },
            SessionSubtask {
                id: "exp-3".into(),
                title: "Create REST API".into(),
                description: None,
                status: "pending".into(),
                depends_on: vec!["exp-2".into()],
                owner: None,
            },
        ];
        let lines = render(&[parent], 80, 40, true);
        let texts: Vec<String> = lines.iter().map(spans_text).collect();
        // Header carries the subtask roll-up.
        assert!(
            texts[0].contains("1/3 subtasks done"),
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
        // Find the in_progress line (bolded "running") and confirm its
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
    fn render_multi_skips_sessions_with_no_active_work() {
        // All-completed sessions are still open on disk but contribute
        // nothing actionable; the cross-session view prunes them so
        // the row budget isn't burned on dim history.
        let input = vec![(
            "sess-done".to_string(),
            vec![mk_task("task-1", "finished", "completed")],
        )];
        let out = render_multi_with_colors(&input, 80, 40, fixture_colors());
        assert!(
            out.is_empty(),
            "all-completed session must be pruned: {:?}",
            out.iter().map(spans_text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn render_multi_emits_session_header_then_active_tasks() {
        let input = vec![(
            "0123456789ab".to_string(),
            vec![
                mk_task("task-1", "open one", "pending"),
                mk_task("task-2", "done one", "completed"),
                mk_task("task-3", "busy one", "in_progress"),
            ],
        )];
        let out = render_multi_with_colors(&input, 80, 40, fixture_colors());
        let texts: Vec<String> = out.iter().map(spans_text).collect();
        assert!(
            texts.iter().any(|t| t.contains("01234567")),
            "short session id header missing: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("2 active")),
            "active count missing from header: {texts:?}"
        );
        assert!(texts.iter().any(|t| t.contains("open one")));
        assert!(texts.iter().any(|t| t.contains("busy one")));
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
                    mk_task("a1", "a1", "pending"),
                    mk_task("a2", "a2", "pending"),
                ],
            ),
            (
                "sess-b".to_string(),
                vec![
                    mk_task("b1", "b1", "pending"),
                    mk_task("b2", "b2", "pending"),
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
    fn render_multi_empty_when_terminal_too_short() {
        let input = vec![("sess".to_string(), vec![mk_task("a1", "a", "pending")])];
        let out = render_multi_with_colors(&input, 80, 8, fixture_colors());
        assert!(
            out.is_empty(),
            "rows<=10 must render nothing (same invariant as single-session)"
        );
    }

    // ── render_with_fresh: just-changed row flash ────────────────

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
}
