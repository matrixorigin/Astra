//! Multi-session rollup for the task board.
//!
//! The current task board is scoped to the active session id —
//! reasonable for the default "what am I doing right now" use case.
//! Power users want a toggled cross-session view: "all open tasks
//! across every session I've touched in the last day". This module
//! owns the pure flattening logic so the observer / renderer can
//! stay session-scoped and the multi-session mode is a view filter
//! over the full fetch.
//!
//! The actual `TaskStore::load_all_for_user` call is deferred to
//! follow-up work — MatrixOne needs a new SQL index and the local
//! in-memory store needs a multi-session constructor. This module
//! pins the output shape so the view layer and the storage layer
//! can land in parallel.

use crate::cli::session_task_surface::session_task_is_active;
use astra_tools::task_mgmt::SessionTask;

/// A flat row in the cross-session task view. Mirrors the
/// session-scoped `SessionTask` but adds a `session_id` column and
/// a short session label so the renderer can group visually
/// without touching storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MultiSessionRow {
    pub session_id: String,
    pub session_short: String,
    pub task_id: String,
    pub title: String,
    pub status: String,
    pub updated_at: String,
}

/// Flatten a list of `(session_id, tasks)` pairs into the
/// cross-session view. Filters to "active" tasks (pending /
/// in_progress) by default — the board has always been
/// active-work-focused; completed cross-session work is a
/// different surface.
pub(crate) fn flatten_active<'a, I>(per_session: I) -> Vec<MultiSessionRow>
where
    I: IntoIterator<Item = (&'a str, &'a [SessionTask])>,
{
    let mut rows = Vec::new();
    for (sid, tasks) in per_session {
        let short = sid.chars().take(8).collect::<String>();
        for t in tasks {
            if !session_task_is_active(&t.status) {
                continue;
            }
            rows.push(MultiSessionRow {
                session_id: sid.to_string(),
                session_short: short.clone(),
                task_id: t.id.clone(),
                title: t.title.clone(),
                status: t.status.clone(),
                updated_at: t.updated_at.clone(),
            });
        }
    }
    // Stable order: newest updated_at first so the user sees
    // recently-touched work at the top. Tie-break on session_id so
    // the result is deterministic across fetches.
    rows.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    rows
}

/// Group rows by session_id in the order they first appear. The
/// render path can walk this to emit per-session headers without
/// re-sorting.
pub(crate) fn group_by_session(rows: &[MultiSessionRow]) -> Vec<(String, Vec<MultiSessionRow>)> {
    let mut out: Vec<(String, Vec<MultiSessionRow>)> = Vec::new();
    for row in rows {
        if let Some(last) = out.last_mut()
            && last.0 == row.session_id
        {
            last.1.push(row.clone());
            continue;
        }
        if let Some(existing) = out.iter_mut().find(|(sid, _)| sid == &row.session_id) {
            existing.1.push(row.clone());
        } else {
            out.push((row.session_id.clone(), vec![row.clone()]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, title: &str, status: &str, updated_at: &str) -> SessionTask {
        SessionTask {
            id: id.into(),
            title: title.into(),
            description: None,
            active_form: None,
            status: status.into(),
            owner: None,
            metadata: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            subtasks: Vec::new(),
            created_at: "2025-05-10T12:00:00Z".into(),
            updated_at: updated_at.into(),
        }
    }

    #[test]
    fn flatten_filters_to_active_only() {
        let sid_a: Vec<SessionTask> = vec![
            task("t1", "open", "pending", "2025-05-10T12:00:00Z"),
            task("t2", "done", "completed", "2025-05-10T12:00:00Z"),
            task("t3", "mid", "in_progress", "2025-05-10T12:00:00Z"),
        ];
        let rows = flatten_active([("sess-a", sid_a.as_slice())]);
        assert_eq!(rows.len(), 2, "completed must be filtered out: {rows:?}");
        let ids: Vec<&str> = rows.iter().map(|r| r.task_id.as_str()).collect();
        assert!(ids.contains(&"t1"));
        assert!(ids.contains(&"t3"));
    }

    #[test]
    fn flatten_sorts_newest_first_across_sessions() {
        let sid_a: Vec<SessionTask> = vec![task("ta", "A", "pending", "2025-05-10T10:00:00Z")];
        let sid_b: Vec<SessionTask> = vec![task("tb", "B", "pending", "2025-05-10T14:00:00Z")];
        let rows = flatten_active([("sess-a", sid_a.as_slice()), ("sess-b", sid_b.as_slice())]);
        assert_eq!(rows[0].task_id, "tb", "newer wins: {rows:?}");
        assert_eq!(rows[1].task_id, "ta");
    }

    #[test]
    fn flatten_stamps_session_short_label_with_first_8_chars() {
        let sid: Vec<SessionTask> = vec![task("t1", "T", "pending", "2025-05-10T12:00:00Z")];
        let rows = flatten_active([("01234567-abcd-abcd", sid.as_slice())]);
        assert_eq!(rows[0].session_short, "01234567");
    }

    #[test]
    fn flatten_handles_short_session_ids() {
        // Session shorter than 8 chars — take everything we have
        // rather than panic on char index.
        let sid: Vec<SessionTask> = vec![task("t1", "T", "pending", "2025-05-10T12:00:00Z")];
        let rows = flatten_active([("abc", sid.as_slice())]);
        assert_eq!(rows[0].session_short, "abc");
    }

    #[test]
    fn group_by_session_preserves_first_seen_order() {
        let rows = vec![
            MultiSessionRow {
                session_id: "sess-b".into(),
                session_short: "sess-b".into(),
                task_id: "t1".into(),
                title: "first".into(),
                status: "pending".into(),
                updated_at: "2025-05-10T14:00:00Z".into(),
            },
            MultiSessionRow {
                session_id: "sess-a".into(),
                session_short: "sess-a".into(),
                task_id: "t2".into(),
                title: "second".into(),
                status: "pending".into(),
                updated_at: "2025-05-10T12:00:00Z".into(),
            },
            MultiSessionRow {
                session_id: "sess-b".into(),
                session_short: "sess-b".into(),
                task_id: "t3".into(),
                title: "third".into(),
                status: "pending".into(),
                updated_at: "2025-05-10T10:00:00Z".into(),
            },
        ];
        let groups = group_by_session(&rows);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "sess-b");
        assert_eq!(groups[0].1.len(), 2, "sess-b must have both rows");
        assert_eq!(groups[1].0, "sess-a");
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let rows = flatten_active(std::iter::empty::<(&str, &[SessionTask])>());
        assert!(rows.is_empty());
        let groups = group_by_session(&[]);
        assert!(groups.is_empty());
    }
}
