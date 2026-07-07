//! Task-board diff events.
//!
//! The observer refreshes its snapshot on the usual 5 s (or 250 ms
//! after a broadcast) cadence. Between two snapshots we want to know
//! *what changed* — a new task appeared, a status flipped,
//! something was removed — so the renderer can flash a "just
//! happened" highlight on the affected row for a short TTL.
//!
//! This mirrors the feedback reference-agent gets from its task-state
//! reducer but keeps it server-local: no dedicated event bus, just a
//! pure `diff(prev, new) -> Vec<TaskBoardEvent>` call in the observer
//! after each fetch plus a small ring buffer of recent events.
//!
//! The diff is id-stable: tasks are matched by `SessionTask::id`
//! rather than position, so a board reshuffled by priority doesn't
//! spuriously fire Created/Removed pairs.

use astra_tools::task_mgmt::{SessionTask, SessionTaskStatusKind};

/// One thing the observer detected between two snapshots. Ordered
/// oldest first so the ring buffer trims from the front.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskBoardEvent {
    /// Task appeared on this tick's snapshot but was absent on the
    /// previous one. `title` is carried for logging; the UI reads
    /// `task_id` to look the row up.
    Created { task_id: String, title: String },
    /// Task's `status` column differed between prev and new.
    StatusChanged {
        task_id: String,
        title: String,
        from: SessionTaskStatusKind,
        to: SessionTaskStatusKind,
    },
    /// Task disappeared from the snapshot (deleted / cancelled +
    /// pruned, etc.).
    Removed { task_id: String, title: String },
}

impl TaskBoardEvent {
    pub fn task_id(&self) -> &str {
        match self {
            Self::Created { task_id, .. }
            | Self::StatusChanged { task_id, .. }
            | Self::Removed { task_id, .. } => task_id,
        }
    }
}

/// Pure snapshot diff. Match by `id`, so a board reordered between
/// fetches produces zero noise.
pub(crate) fn diff(prev: &[SessionTask], new: &[SessionTask]) -> Vec<TaskBoardEvent> {
    let mut events = Vec::new();

    // Created / StatusChanged: walk the new list, look up each by id
    // in the previous snapshot.
    for task in new {
        match prev.iter().find(|p| p.id == task.id) {
            None => events.push(TaskBoardEvent::Created {
                task_id: task.id.clone(),
                title: task.title.clone(),
            }),
            Some(p) if p.status != task.status => {
                events.push(TaskBoardEvent::StatusChanged {
                    task_id: task.id.clone(),
                    title: task.title.clone(),
                    from: p.status,
                    to: task.status,
                });
            }
            _ => {}
        }
    }

    // Removed: whatever was in prev but is absent from new.
    for p in prev {
        if !new.iter().any(|t| t.id == p.id) {
            events.push(TaskBoardEvent::Removed {
                task_id: p.id.clone(),
                title: p.title.clone(),
            });
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::{TaskBoardEvent, diff};
    use astra_tools::task_mgmt::SessionTask;

    fn task(id: &str, title: &str, status: &str) -> SessionTask {
        SessionTask {
            archived_at: None,
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

    #[test]
    fn empty_to_empty_is_silent() {
        assert!(diff(&[], &[]).is_empty());
    }

    #[test]
    fn new_task_emits_created() {
        let prev: Vec<SessionTask> = vec![];
        let new = vec![task("task-1", "write tests", "pending")];
        let ev = diff(&prev, &new);
        assert_eq!(ev.len(), 1);
        assert!(matches!(&ev[0], TaskBoardEvent::Created { task_id, title }
                if task_id == "task-1" && title == "write tests"));
    }

    #[test]
    fn status_flip_emits_status_changed() {
        let prev = vec![task("task-1", "a", "pending")];
        let new = vec![task("task-1", "a", "in_progress")];
        let ev = diff(&prev, &new);
        assert_eq!(ev.len(), 1);
        match &ev[0] {
            TaskBoardEvent::StatusChanged {
                task_id, from, to, ..
            } => {
                assert_eq!(task_id, "task-1");
                assert_eq!(
                    from,
                    &astra_tools::task_mgmt::SessionTaskStatusKind::Pending
                );
                assert_eq!(
                    to,
                    &astra_tools::task_mgmt::SessionTaskStatusKind::InProgress
                );
            }
            other => panic!("expected StatusChanged, got {other:?}"),
        }
    }

    #[test]
    fn removed_task_emits_removed() {
        let prev = vec![task("task-1", "gone", "pending")];
        let new: Vec<SessionTask> = vec![];
        let ev = diff(&prev, &new);
        assert_eq!(ev.len(), 1);
        assert!(matches!(&ev[0], TaskBoardEvent::Removed { task_id, .. } if task_id == "task-1"));
    }

    #[test]
    fn reorder_only_produces_no_events() {
        // Regression guard: the old observer used positional equality
        // (same_board length check) which would silently swallow a
        // reorder as "no change", but a naive vec equality diff would
        // have produced Created+Removed pairs. id-based matching
        // must emit zero events for a pure reshuffle.
        let prev = vec![
            task("task-1", "a", "pending"),
            task("task-2", "b", "in_progress"),
        ];
        let new = vec![
            task("task-2", "b", "in_progress"),
            task("task-1", "a", "pending"),
        ];
        assert!(
            diff(&prev, &new).is_empty(),
            "pure reorder must not fire diff events: {:?}",
            diff(&prev, &new)
        );
    }

    #[test]
    fn mixed_changes_emit_events_in_stable_order() {
        // New task, status-change on an existing one, and a removal
        // — diff must surface all three. Created/StatusChanged walk
        // the NEW list (so they appear in the new-snapshot's order);
        // Removed fires after, in prev-list order.
        let prev = vec![
            task("task-1", "keep changing", "pending"),
            task("task-2", "will vanish", "pending"),
        ];
        let new = vec![
            task("task-1", "keep changing", "completed"),
            task("task-3", "arrived", "pending"),
        ];
        let ev = diff(&prev, &new);
        assert_eq!(ev.len(), 3, "expected 3 events: {ev:?}");
        assert!(
            matches!(&ev[0], TaskBoardEvent::StatusChanged { task_id, .. } if task_id == "task-1")
        );
        assert!(matches!(&ev[1], TaskBoardEvent::Created { task_id, .. } if task_id == "task-3"));
        assert!(matches!(&ev[2], TaskBoardEvent::Removed { task_id, .. } if task_id == "task-2"));
    }

    #[test]
    fn unchanged_task_produces_no_event() {
        let t = task("task-1", "a", "pending");
        let slice = std::slice::from_ref(&t);
        assert!(diff(slice, slice).is_empty());
    }

    #[test]
    fn task_id_accessor_returns_underlying_id() {
        let a = TaskBoardEvent::Created {
            task_id: "task-1".into(),
            title: "x".into(),
        };
        let b = TaskBoardEvent::StatusChanged {
            task_id: "task-2".into(),
            title: "y".into(),
            from: "pending".into(),
            to: "completed".into(),
        };
        let c = TaskBoardEvent::Removed {
            task_id: "task-3".into(),
            title: "z".into(),
        };
        assert_eq!(a.task_id(), "task-1");
        assert_eq!(b.task_id(), "task-2");
        assert_eq!(c.task_id(), "task-3");
    }
}
