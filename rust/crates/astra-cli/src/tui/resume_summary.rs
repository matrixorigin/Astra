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
//! `TaskService::list_recent_tasks_for_session` call + MatrixOne query live in the
//! event loop; here we keep the rendering logic testable without a
//! live DB.

use crate::cli::surface::task_checkpoint_surface::task_list_item_outcome;
use astra_services::{TaskListItem, TaskOutcome, TaskStatus, session_journal::JournalEvent};

/// Rollup of the terminal-state tasks completed while the user was
/// away. `ok` = completed with success/unknown outcome; `partial` =
/// completed with partial outcome; `failed` = failed; `cancelled` =
/// cancelled. `paused`/`pending`/`in_progress` are deliberately
/// excluded — those are still-running work and surface via the
/// `task_board`, not the resume banner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ResumeSummary {
    pub ok: usize,
    pub partial: usize,
    pub failed: usize,
    pub cancelled: usize,
}

impl ResumeSummary {
    pub(crate) fn total(&self) -> usize {
        self.ok + self.partial + self.failed + self.cancelled
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
        if self.partial > 0 {
            parts.push(format!("{} partial", self.partial));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if self.cancelled > 0 {
            parts.push(format!("{} cancelled", self.cancelled));
        }
        let breakdown = parts.join(", ");
        let noun = if total == 1 {
            "background job"
        } else {
            "background jobs"
        };
        format!("While you were away: {total} {noun} finished ({breakdown}).")
    }
}

/// Reduce a `list_recent_tasks_for_session` result into a `ResumeSummary`.
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
        if !session_id_filter.is_empty() && item.session_id.as_deref() != Some(session_id_filter) {
            continue;
        }
        if item.updated_at.as_str() <= updated_after {
            continue;
        }
        match task_list_item_outcome(item) {
            Some(TaskOutcome::Partial) => out.partial += 1,
            Some(TaskOutcome::Failed) => out.failed += 1,
            Some(TaskOutcome::Cancelled) => out.cancelled += 1,
            Some(TaskOutcome::Success) => out.ok += 1,
            None => match item.status {
                TaskStatus::Completed => out.ok += 1,
                TaskStatus::Failed => out.failed += 1,
                TaskStatus::Cancelled => out.cancelled += 1,
                // Still running / paused / pending do not belong on the
                // "what happened while you were gone" summary.
                TaskStatus::Pending | TaskStatus::InProgress | TaskStatus::Paused => {}
            },
        }
    }
    out
}

pub(crate) fn last_seen_cutoff(last_turn_event: Option<&JournalEvent>) -> Option<&str> {
    last_turn_event.map(|event| event.ts.as_str())
}

#[cfg(test)]
mod tests {
    use super::{ResumeSummary, last_seen_cutoff, summarize};
    use astra_services::{TaskListItem, TaskOutcome, TaskStatus, session_journal::JournalEvent};

    fn item(task_id: &str, status: TaskStatus, updated_at: &str) -> TaskListItem {
        TaskListItem {
            task_id: task_id.into(),
            title: format!("title-{task_id}"),
            session_id: Some("sess-1".into()),
            status,
            progress_pct: 100,
            items_done: 1,
            items_total: 1,
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: updated_at.into(),
            completed_at: None,
            outcome: None,
            error_message: None,
            project_type: None,
            claimability: None,
        }
    }

    #[test]
    fn summarize_prefers_structured_failure_over_completed_status() {
        let mut task = item("a", TaskStatus::Completed, "2025-05-10T12:00:00Z");
        task.outcome = Some(TaskOutcome::Success);
        task.error_message = Some(
            crate::cli::surface::task_checkpoint_surface::encode_task_failure_message(
                "persistence_error",
                "failed to append turn event",
            ),
        );
        let summary = summarize(&[task], "", "");
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.ok, 0);
    }

    #[test]
    fn summarize_filters_foreign_session_items() {
        let local = item("a", TaskStatus::Completed, "2025-05-10T12:00:00Z");
        let mut foreign = item("b", TaskStatus::Failed, "2025-05-10T13:00:00Z");
        foreign.session_id = Some("sess-2".into());
        let summary = summarize(&[local, foreign], "sess-1", "");
        assert_eq!(summary.ok, 1);
        assert_eq!(summary.failed, 0);
    }

    #[test]
    fn empty_list_yields_empty_summary() {
        let s = summarize(&[], "sess-1", "");
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
        let s = summarize(&tasks, "sess-1", "");
        assert_eq!(s.ok, 2);
        assert_eq!(s.partial, 0);
        assert_eq!(s.failed, 1);
        assert_eq!(s.cancelled, 1);
        assert_eq!(s.total(), 4);
    }

    #[test]
    fn partial_completed_tasks_are_not_counted_as_ok() {
        let mut task = item("a", TaskStatus::Completed, "2025-05-10T12:00:00Z");
        task.outcome = Some(TaskOutcome::Partial);
        let s = summarize(&[task], "sess-1", "");
        assert_eq!(s.ok, 0);
        assert_eq!(s.partial, 1);
        assert_eq!(s.failed, 0);
        assert_eq!(s.cancelled, 0);
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
        let s = summarize(&tasks, "sess-1", "");
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
        let s = summarize(&tasks, "sess-1", "2025-05-10T13:00:00Z");
        assert_eq!(
            s.ok, 1,
            "only the 14:00 completion should appear post-cutoff: {s:?}"
        );
    }

    #[test]
    fn last_seen_cutoff_prefers_last_turn_timestamp() {
        let event = JournalEvent::turn(
            Some("sess-1"),
            3,
            Some("gpt-5"),
            "hi",
            "hello",
            0,
            10,
            5,
            100,
        );
        assert_eq!(last_seen_cutoff(Some(&event)), Some(event.ts.as_str()));
        assert_eq!(last_seen_cutoff(None), None);
    }

    #[test]
    fn render_uses_singular_for_count_of_one() {
        let s = ResumeSummary {
            ok: 1,
            ..Default::default()
        };
        let out = s.render();
        assert!(
            out.contains("1 background job") && !out.contains("1 background jobs"),
            "singular copy required: {out}"
        );
        assert!(out.starts_with("While you were away:"));
        assert!(out.contains("1 ok"));
    }

    #[test]
    fn render_uses_plural_and_lists_mixed_outcomes() {
        let s = ResumeSummary {
            ok: 2,
            partial: 1,
            failed: 1,
            cancelled: 1,
        };
        let out = s.render();
        assert!(
            out.contains("5 background jobs"),
            "plural copy missing: {out}"
        );
        assert!(
            out.contains("2 ok")
                && out.contains("1 partial")
                && out.contains("1 failed")
                && out.contains("1 cancelled"),
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
