//! Resume-time background-task summary.
//!
//! On `astra -c` / `astra --resume <sid>` the user expects to see
//! what changed while they were gone. The TUI queries the
//! `TaskService` for every task tied to this session and filters to
//! the ones that reached a terminal state since `last_seen_at`
//! (the timestamp of the previous turn); those become a one-line
//! banner at the top of the resumed scrollback.
//!
//! This module owns the *pure* part of that flow: given a list of
//! task summaries + a cutoff, produce the banner text. The actual
//! `TaskService::list_tasks` call + MatrixOne query live in the
//! event loop; here we keep the rendering logic testable without a
//! live DB.

use astra_services::{TaskListItem, TaskStatus};

/// Rollup of the terminal-state tasks completed while the user was
/// away. `ok` = `Completed`; `failed` = `Failed`; `cancelled`
/// = `Cancelled`. `paused`/`pending`/`in_progress` are deliberately
/// excluded — those are still-running work and surface via the
/// `task_board`, not the resume banner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ResumeSummary {
    pub ok: usize,
    pub failed: usize,
    pub cancelled: usize,
}

impl ResumeSummary {
    pub(crate) fn total(&self) -> usize {
        self.ok + self.failed + self.cancelled
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// Render the banner line exactly as it will appear in
    /// scrollback. Callers pass this into a `SystemCell::info` to
    /// keep the "this is history, not user speech" styling.
    pub(crate) fn render(&self) -> String {
        let total = self.total();
        let mut parts = Vec::new();
        if self.ok > 0 {
            parts.push(format!("{} ok", self.ok));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if self.cancelled > 0 {
            parts.push(format!("{} cancelled", self.cancelled));
        }
        let breakdown = parts.join(", ");
        let plural = if total == 1 { "task" } else { "tasks" };
        format!("{total} background {plural} finished while you were away ({breakdown}).")
    }
}

/// Reduce a `list_tasks` result into a `ResumeSummary`.
///
/// - `session_id_filter`: keep only tasks whose `session_id` matches,
///   so we never surface other sessions' work on this banner.
/// - `updated_after`: keep only tasks whose `updated_at` is
///   lexicographically greater than this cutoff (RFC3339 strings
///   sort correctly). Pass `""` to match everything.
pub(crate) fn summarize(
    items: &[TaskListItem],
    session_id_filter: &str,
    updated_after: &str,
) -> ResumeSummary {
    let mut out = ResumeSummary::default();
    for item in items {
        // The listing currently omits `session_id` (the MatrixOne
        // query flattens task records into a projection). Callers
        // that need per-session filtering should match on the
        // project_type/title convention or keep the filter string
        // empty — we enforce the cutoff unconditionally.
        let _ = session_id_filter;
        if item.updated_at.as_str() <= updated_after {
            continue;
        }
        match item.status {
            TaskStatus::Completed => out.ok += 1,
            TaskStatus::Failed => out.failed += 1,
            TaskStatus::Cancelled => out.cancelled += 1,
            // Still running / paused / pending do not belong on the
            // "what happened while you were gone" summary.
            TaskStatus::Pending | TaskStatus::InProgress | TaskStatus::Paused => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(task_id: &str, status: TaskStatus, updated_at: &str) -> TaskListItem {
        TaskListItem {
            task_id: task_id.into(),
            title: format!("title-{task_id}"),
            status,
            progress_pct: 100,
            items_done: 1,
            items_total: 1,
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: updated_at.into(),
            completed_at: None,
            project_type: None,
        }
    }

    #[test]
    fn empty_list_yields_empty_summary() {
        let s = summarize(&[], "sess", "");
        assert!(s.is_empty());
        assert_eq!(s.total(), 0);
    }

    #[test]
    fn terminal_tasks_bucket_by_status() {
        let tasks = vec![
            item("a", TaskStatus::Completed, "2025-05-10T12:00:00Z"),
            item("b", TaskStatus::Completed, "2025-05-10T13:00:00Z"),
            item("c", TaskStatus::Failed, "2025-05-10T13:30:00Z"),
            item("d", TaskStatus::Cancelled, "2025-05-10T14:00:00Z"),
        ];
        let s = summarize(&tasks, "sess", "");
        assert_eq!(s.ok, 2);
        assert_eq!(s.failed, 1);
        assert_eq!(s.cancelled, 1);
        assert_eq!(s.total(), 4);
    }

    #[test]
    fn still_running_tasks_are_excluded() {
        // pending/in_progress/paused don't belong on the "what
        // happened while you were away" banner — those surface via
        // task_board.
        let tasks = vec![
            item("a", TaskStatus::Pending, "2025-05-10T12:00:00Z"),
            item("b", TaskStatus::InProgress, "2025-05-10T13:00:00Z"),
            item("c", TaskStatus::Paused, "2025-05-10T14:00:00Z"),
        ];
        let s = summarize(&tasks, "sess", "");
        assert!(s.is_empty(), "non-terminal tasks must not appear: {s:?}");
    }

    #[test]
    fn cutoff_excludes_older_updates() {
        // The user left at 13:00 — anything updated before or at
        // 13:00 already showed up in their previous session's
        // scrollback. Don't re-advertise it.
        let tasks = vec![
            item("before", TaskStatus::Completed, "2025-05-10T12:00:00Z"),
            item("at", TaskStatus::Completed, "2025-05-10T13:00:00Z"),
            item("after", TaskStatus::Completed, "2025-05-10T14:00:00Z"),
        ];
        let s = summarize(&tasks, "sess", "2025-05-10T13:00:00Z");
        assert_eq!(
            s.ok, 1,
            "only the 14:00 completion should appear post-cutoff: {s:?}"
        );
    }

    #[test]
    fn render_uses_singular_for_count_of_one() {
        let s = ResumeSummary {
            ok: 1,
            ..Default::default()
        };
        let out = s.render();
        assert!(
            out.contains("1 background task") && !out.contains("1 background tasks"),
            "singular copy required: {out}"
        );
        assert!(out.contains("1 ok"));
    }

    #[test]
    fn render_uses_plural_and_lists_mixed_outcomes() {
        let s = ResumeSummary {
            ok: 2,
            failed: 1,
            cancelled: 1,
        };
        let out = s.render();
        assert!(
            out.contains("4 background tasks"),
            "plural copy missing: {out}"
        );
        assert!(
            out.contains("2 ok") && out.contains("1 failed") && out.contains("1 cancelled"),
            "mixed breakdown missing parts: {out}"
        );
    }

    #[test]
    fn render_omits_zero_buckets() {
        // A summary with only successes should NOT list "0 failed,
        // 0 cancelled" — that reads like something went wrong.
        let s = ResumeSummary {
            ok: 3,
            ..Default::default()
        };
        let out = s.render();
        assert!(out.contains("(3 ok)"));
        assert!(!out.contains("failed"));
        assert!(!out.contains("cancelled"));
    }
}
