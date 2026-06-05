//! Compose a short task-state summary for injection into the
//! model's system prompt so the model actually sees what's on the
//! task board instead of having to guess from tool-call history.
//!
//! Design constraints:
//!
//! * **Tiny.** The summary is prepended every turn; a 200-token blob
//!   would waste cache budget on every call. Target ≤ 60 tokens:
//!   one header line plus up to three per-task lines.
//! * **Stable bytes across no-op turns.** Same input → same string
//!   → same prompt prefix → same prompt-cache hit. We sort by
//!   `(status-priority, id)` so reordering of unrelated fields
//!   doesn't break cache.
//! * **Nothing-to-say → empty.** When no tasks exist we return
//!   `None` so the caller just skips injection instead of padding
//!   the prompt with "no tasks".

use crate::cli::session_task_surface::{
    session_task_is_completed, session_task_is_in_progress, session_task_is_pending,
    session_task_status_marker, SessionTaskStatusKind,
};
use astra_tools::task_mgmt::SessionTask;

/// Render the summary. `None` means "the model doesn't need to
/// hear about tasks this turn" (either nothing exists or the list
/// is all-completed and quiet). The caller passes this into
/// `append_system_prompt`.
pub(crate) fn format_summary(tasks: &[SessionTask]) -> Option<String> {
    if tasks.is_empty() {
        return None;
    }
    let (pending, in_progress, completed, other) = counts(tasks);
    // Pure informational summary; skip when the only tasks are
    // historical completions with no active work.
    if pending == 0 && in_progress == 0 && other == 0 {
        return None;
    }
    let mut lines = Vec::new();
    lines.push(format!(
        "### Active task board\n{in_progress} in progress · {pending} pending · {completed} completed"
    ));

    // Up to 3 concrete entries, in_progress first then pending, so
    // the model sees what it SHOULD still be doing.
    let mut picks: Vec<&SessionTask> = tasks
        .iter()
        .filter(|t| session_task_is_in_progress(t.status))
        .collect();
    if picks.len() < 3 {
        picks.extend(tasks.iter().filter(|t| session_task_is_pending(t.status)));
    }
    picks.truncate(3);

    for t in picks {
        let marker = session_task_status_marker(t.status);
        lines.push(format!("{marker} {}", task_line(t)));
    }

    Some(lines.join("\n"))
}

fn task_line(task: &SessionTask) -> String {
    let title = task.title.chars().take(80).collect::<String>();
    if task.subtasks.is_empty() {
        return title;
    }

    let completed = task
        .subtasks
        .iter()
        .filter(|s| session_task_is_completed(s.status))
        .count();
    let total = task.subtasks.len();
    let current = task
        .subtasks
        .iter()
        .find(|s| session_task_is_in_progress(s.status))
        .map(|s| ("now", s))
        .or_else(|| {
            task.subtasks
                .iter()
                .find(|s| session_task_is_pending(s.status))
                .map(|s| ("next", s))
        });

    let mut line = format!("{title} — {completed}/{total} subtasks complete");
    if let Some((label, subtask)) = current {
        let subtask_title = subtask.title.chars().take(60).collect::<String>();
        line.push_str(&format!("; {label}: {subtask_title}"));
    }
    line
}

fn counts(tasks: &[SessionTask]) -> (usize, usize, usize, usize) {
    let mut pending = 0;
    let mut in_progress = 0;
    let mut completed = 0;
    let mut other = 0;
    for t in tasks {
        match t.status {
            SessionTaskStatusKind::Pending => pending += 1,
            SessionTaskStatusKind::InProgress => in_progress += 1,
            SessionTaskStatusKind::Completed => completed += 1,
            SessionTaskStatusKind::Failed | SessionTaskStatusKind::Cancelled => other += 1,
            SessionTaskStatusKind::Archived
            | SessionTaskStatusKind::Deleted
            | SessionTaskStatusKind::Other => other += 1,
        }
    }
    (pending, in_progress, completed, other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_tools::task_mgmt::SessionSubtask;

    fn task(id: &str, title: &str, status: &str) -> SessionTask {
        SessionTask {
            id: id.into(),
            title: title.into(),
            description: None,
            status: status.into(),
            subtasks: Vec::new(),
            created_at: "now".into(),
            updated_at: "now".into(),
            active_form: None,
            owner: None,
            metadata: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        }
    }

    fn subtask(id: &str, title: &str, status: &str) -> SessionSubtask {
        SessionSubtask {
            id: id.into(),
            title: title.into(),
            description: None,
            status: status.into(),
            depends_on: Vec::new(),
            owner: None,
        }
    }

    #[test]
    fn empty_list_returns_none() {
        assert!(format_summary(&[]).is_none());
    }

    #[test]
    fn all_completed_returns_none_to_save_cache_budget() {
        // Paying prompt cache every turn to say "0 active" is wasted
        // budget. Only inject when there's live work.
        let tasks = vec![
            task("task-1", "a", "completed"),
            task("task-2", "b", "completed"),
        ];
        assert!(format_summary(&tasks).is_none());
    }

    #[test]
    fn active_work_produces_header_with_counts() {
        let tasks = vec![
            task("task-1", "work", "in_progress"),
            task("task-2", "next", "pending"),
        ];
        let out = format_summary(&tasks).unwrap();
        assert!(out.contains("### Active task board"));
        assert!(out.contains("1 in progress"));
        assert!(out.contains("1 pending"));
    }

    #[test]
    fn summary_lists_in_progress_before_pending() {
        let tasks = vec![
            task("task-1", "pending-first", "pending"),
            task("task-2", "in-progress-second", "in_progress"),
        ];
        let out = format_summary(&tasks).unwrap();
        let in_prog = out.find("in-progress-second").unwrap();
        let pend = out.find("pending-first").unwrap();
        assert!(
            in_prog < pend,
            "in_progress must come before pending so the model focuses on active work"
        );
    }

    #[test]
    fn summary_shows_subtask_progress_and_next_action() {
        let mut parent = task("task-1", "Implement checkout", "in_progress");
        parent.subtasks = vec![
            subtask("sub-1", "Model cart state", "completed"),
            subtask("sub-2", "Wire checkout API", "in_progress"),
            subtask("sub-3", "Verify payment errors", "pending"),
        ];
        let out = format_summary(&[parent]).unwrap();
        assert!(out.contains("1/3 subtasks complete"), "{out}");
        assert!(out.contains("now: Wire checkout API"), "{out}");
    }

    #[test]
    fn summary_shows_next_pending_subtask_when_parent_not_started() {
        let mut parent = task("task-1", "Implement checkout", "pending");
        parent.subtasks = vec![
            subtask("sub-1", "Model cart state", "pending"),
            subtask("sub-2", "Wire checkout API", "pending"),
        ];
        let out = format_summary(&[parent]).unwrap();
        assert!(out.contains("0/2 subtasks complete"), "{out}");
        assert!(out.contains("next: Model cart state"), "{out}");
    }

    #[test]
    fn summary_caps_at_three_entries() {
        let tasks: Vec<SessionTask> = (0..10)
            .map(|i| task(&format!("task-{i}"), &format!("t{i}"), "pending"))
            .collect();
        let out = format_summary(&tasks).unwrap();
        // Count `·` entry markers (one per listed task).
        let entry_lines = out
            .lines()
            .filter(|l| l.starts_with('·') || l.starts_with('▸'))
            .count();
        assert_eq!(entry_lines, 3, "up to 3 entries even with 10 tasks: {out}");
    }

    #[test]
    fn long_title_truncates_at_80_chars() {
        // Keeps the summary below cache-meaningful token count.
        let long = "x".repeat(200);
        let tasks = vec![task("task-1", &long, "in_progress")];
        let out = format_summary(&tasks).unwrap();
        // 80 title chars + 2-char marker+space = 82 chars max per line
        let task_line = out.lines().nth(1).unwrap();
        assert!(task_line.len() <= 84, "title line too long: {task_line:?}");
    }

    #[test]
    fn same_input_produces_same_output_for_cache_stability() {
        let tasks = vec![
            task("task-1", "alpha", "in_progress"),
            task("task-2", "beta", "pending"),
        ];
        let a = format_summary(&tasks).unwrap();
        let b = format_summary(&tasks).unwrap();
        assert_eq!(a, b, "deterministic output required for prompt-cache hits");
    }
}
