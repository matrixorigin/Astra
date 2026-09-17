//! Lock-consistent TUI read model for the canonical Work Task Graph.
//!
//! Network observation belongs to `plan_task_observer`; this type owns only
//! renderer state (rows, truth, explicit visibility, and short-lived diff
//! highlights). It never merges independently-owned task models into one board.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::work_board_projection::{SessionTask, TaskStoreHealth};

use super::task_list::task_needs_attention;

/// Constant-size identity and intent for the Work shown by the board.
///
/// This is deliberately separate from executable rows: a goal or milestone
/// is navigation context, not a task attempt, and must never inflate task
/// counts or become an actionable row merely because both live in the
/// canonical Work Graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkBoardContext {
    pub work_id: String,
    pub branch_id: String,
    pub goal: String,
    pub graph_revision: i64,
    pub criteria_member_count: u16,
    pub milestone_count: u16,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TaskBoardSnapshot {
    pub work: Option<WorkBoardContext>,
    pub tasks: Vec<SessionTask>,
    pub hidden: bool,
}

/// A compact, server-issued Work lifecycle receipt projected by the current
/// stream. It is deliberately separate from the remote graph observer: this
/// path makes an already-durable mutation visible immediately, while the
/// observer later reconciles the richer canonical graph.
#[derive(Clone, Debug)]
pub(crate) enum LiveWorkTaskBoardUpdate {
    Snapshot {
        work: WorkBoardContext,
        tasks: Vec<SessionTask>,
    },
    Upsert {
        work_id: String,
        branch_id: String,
        graph_revision: Option<i64>,
        tasks: Vec<SessionTask>,
    },
}

impl TaskBoardSnapshot {
    pub fn has_incomplete(&self) -> bool {
        self.tasks.iter().any(task_needs_attention)
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskBoardTruthState {
    Unbound,
    Loading,
    Confirmed,
    Refreshing,
    Stale,
    Unavailable,
}

impl TaskBoardTruthState {
    pub(crate) fn has_confirmed_truth(self) -> bool {
        matches!(self, Self::Confirmed | Self::Refreshing | Self::Stale)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ProjectedTaskTruthState {
    #[default]
    NotConfigured,
    Loading,
    Confirmed,
    Stale,
    Unavailable,
}

#[derive(Clone, Debug)]
pub(crate) enum TaskBoardProjection {
    Single {
        truth_state: TaskBoardTruthState,
        store_health: TaskStoreHealth,
        projected_truth_state: ProjectedTaskTruthState,
        snapshot: TaskBoardSnapshot,
    },
}

impl TaskBoardProjection {
    pub(crate) fn truth_state(&self) -> TaskBoardTruthState {
        match self {
            Self::Single { truth_state, .. } => *truth_state,
        }
    }

    pub(crate) fn store_health(&self) -> TaskStoreHealth {
        match self {
            Self::Single { store_health, .. } => *store_health,
        }
    }

    pub(crate) fn has_tasks(&self) -> bool {
        match self {
            Self::Single { snapshot, .. } => !snapshot.tasks.is_empty(),
        }
    }

    pub(crate) fn has_open_work(&self) -> bool {
        match self {
            Self::Single { snapshot, .. } => snapshot.has_incomplete(),
        }
    }

    pub(crate) fn same_render_state(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Single {
                    truth_state: left_truth,
                    projected_truth_state: left_projected,
                    snapshot: left,
                    ..
                },
                Self::Single {
                    truth_state: right_truth,
                    projected_truth_state: right_projected,
                    snapshot: right,
                    ..
                },
            ) => {
                left_truth == right_truth
                    && left_projected == right_projected
                    && left.hidden == right.hidden
                    && left.work == right.work
                    && left.tasks == right.tasks
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TimedTaskBoardEvent {
    pub event: super::task_board_events::TaskBoardEvent,
    pub at: Instant,
}

pub(crate) const EVENT_FRESH_WINDOW: Duration = Duration::from_millis(1500);
const EVENT_RING_CAP: usize = 32;

pub(crate) struct TaskBoardObserver {
    state: Mutex<ObserverState>,
}

struct ObserverState {
    session_id: String,
    truth_state: ProjectedTaskTruthState,
    snapshot: TaskBoardSnapshot,
    /// A live receipt is newer than an asynchronous observer result that has
    /// not yet observed the just-committed mutation. Keep it until matching
    /// remote truth catches up or the session binding changes.
    live_work: Option<WorkBoardContext>,
    event_ring: Vec<TimedTaskBoardEvent>,
}

fn lock_state<'a>(
    observer: &'a TaskBoardObserver,
    context: &'static str,
) -> MutexGuard<'a, ObserverState> {
    match observer.state.lock() {
        Ok(state) => state,
        Err(poisoned) => {
            tracing::warn!(context, "Work task-board state poisoned; recovering");
            poisoned.into_inner()
        }
    }
}

fn board_truth(session_id: &str, state: ProjectedTaskTruthState) -> TaskBoardTruthState {
    if session_id.is_empty() {
        return TaskBoardTruthState::Unbound;
    }
    match state {
        ProjectedTaskTruthState::NotConfigured | ProjectedTaskTruthState::Loading => {
            TaskBoardTruthState::Loading
        }
        ProjectedTaskTruthState::Confirmed => TaskBoardTruthState::Confirmed,
        ProjectedTaskTruthState::Stale => TaskBoardTruthState::Stale,
        ProjectedTaskTruthState::Unavailable => TaskBoardTruthState::Unavailable,
    }
}

fn event_is_fresh(event: &TimedTaskBoardEvent, now: Instant) -> bool {
    now.saturating_duration_since(event.at) < EVENT_FRESH_WINDOW
}

impl TaskBoardObserver {
    pub fn new(session_id: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ObserverState {
                session_id: session_id.into(),
                truth_state: ProjectedTaskTruthState::NotConfigured,
                snapshot: TaskBoardSnapshot::default(),
                live_work: None,
                event_ring: Vec::new(),
            }),
        })
    }

    pub fn snapshot(&self) -> TaskBoardSnapshot {
        lock_state(self, "snapshot").snapshot.clone()
    }

    pub fn counts(&self) -> (usize, usize, bool) {
        let state = lock_state(self, "counts");
        let open = state
            .snapshot
            .tasks
            .iter()
            .filter(|task| task_needs_attention(task))
            .count();
        if open == 0 {
            (0, 0, state.snapshot.hidden)
        } else {
            (open, state.snapshot.tasks.len(), state.snapshot.hidden)
        }
    }

    pub fn fresh_event_task_ids(&self) -> Vec<String> {
        let now = Instant::now();
        lock_state(self, "fresh_event_task_ids")
            .event_ring
            .iter()
            .filter(|event| event_is_fresh(event, now))
            .map(|event| event.event.task_id().to_string())
            .collect()
    }

    pub fn truth_state(&self) -> TaskBoardTruthState {
        let state = lock_state(self, "truth_state");
        board_truth(&state.session_id, state.truth_state)
    }

    pub fn active_projection(&self) -> TaskBoardProjection {
        let state = lock_state(self, "active_projection");
        TaskBoardProjection::Single {
            truth_state: board_truth(&state.session_id, state.truth_state),
            store_health: TaskStoreHealth::Ready,
            projected_truth_state: state.truth_state,
            snapshot: state.snapshot.clone(),
        }
    }

    pub fn rebind_session(&self, session_id: impl Into<String>) {
        let session_id = session_id.into();
        let mut state = lock_state(self, "rebind_session");
        if state.session_id == session_id {
            return;
        }
        state.session_id = session_id;
        state.truth_state = ProjectedTaskTruthState::NotConfigured;
        state.snapshot = TaskBoardSnapshot::default();
        state.live_work = None;
        state.event_ring.clear();
    }

    pub(crate) fn set_projected_task_projection(
        &self,
        tasks: Vec<SessionTask>,
        truth_state: ProjectedTaskTruthState,
    ) -> bool {
        self.set_projected_work_projection(None, tasks, truth_state)
    }

    pub(crate) fn set_projected_work_projection(
        &self,
        work: Option<WorkBoardContext>,
        tasks: Vec<SessionTask>,
        truth_state: ProjectedTaskTruthState,
    ) -> bool {
        let mut state = lock_state(self, "set_projected_task_projection");
        if let Some(live_work) = state.live_work.as_ref() {
            let remote_caught_up = truth_state == ProjectedTaskTruthState::Confirmed
                && work.as_ref().is_some_and(|remote_work| {
                    remote_work.work_id == live_work.work_id
                        && remote_work.branch_id == live_work.branch_id
                        && remote_work.graph_revision >= live_work.graph_revision
                });
            if remote_caught_up {
                state.live_work = None;
            } else {
                // Loading, transport degradation, an unbound response, or an
                // older graph read must not erase a lifecycle update which the
                // server already acknowledged on this very stream.
                return false;
            }
        }
        let rows_changed = state.snapshot.tasks != tasks || state.snapshot.work != work;
        let truth_changed = state.truth_state != truth_state;
        if rows_changed {
            let now = Instant::now();
            let events = super::task_board_events::diff(&state.snapshot.tasks, &tasks);
            state.event_ring.extend(
                events
                    .into_iter()
                    .map(|event| TimedTaskBoardEvent { event, at: now }),
            );
            if state.event_ring.len() > EVENT_RING_CAP {
                let excess = state.event_ring.len() - EVENT_RING_CAP;
                state.event_ring.drain(0..excess);
            }
            state.snapshot.tasks = tasks;
            state.snapshot.work = work;
        }
        state.truth_state = truth_state;
        rows_changed || truth_changed
    }

    pub(crate) fn apply_live_work_update(&self, update: LiveWorkTaskBoardUpdate) -> bool {
        let mut state = lock_state(self, "apply_live_work_update");
        let (work, tasks) = match update {
            LiveWorkTaskBoardUpdate::Snapshot { work, tasks } => (work, tasks),
            LiveWorkTaskBoardUpdate::Upsert {
                work_id,
                branch_id,
                graph_revision,
                tasks,
            } => {
                let Some(mut work) = state.snapshot.work.clone() else {
                    // A delta cannot fabricate a goal or silently bind a
                    // different session. The remote observer will recover a
                    // missed snapshot on its normal bounded refresh path.
                    return false;
                };
                if work.work_id != work_id || work.branch_id != branch_id {
                    return false;
                }
                if let Some(graph_revision) = graph_revision {
                    if graph_revision < work.graph_revision {
                        return false;
                    }
                    work.graph_revision = graph_revision;
                }
                let mut merged = state.snapshot.tasks.clone();
                for task in tasks {
                    if let Some(existing) =
                        merged.iter_mut().find(|existing| existing.id == task.id)
                    {
                        // Lifecycle deltas carry task state, not graph-edge
                        // rewrites. Preserve topology from the last snapshot
                        // until the canonical graph observer confirms it.
                        let blocks = std::mem::take(&mut existing.blocks);
                        let blocked_by = std::mem::take(&mut existing.blocked_by);
                        *existing = task;
                        existing.blocks = blocks;
                        existing.blocked_by = blocked_by;
                    } else {
                        merged.push(task);
                    }
                }
                (work, merged)
            }
        };
        let rows_changed =
            state.snapshot.tasks != tasks || state.snapshot.work.as_ref() != Some(&work);
        if rows_changed {
            let now = Instant::now();
            let events = super::task_board_events::diff(&state.snapshot.tasks, &tasks);
            state.event_ring.extend(
                events
                    .into_iter()
                    .map(|event| TimedTaskBoardEvent { event, at: now }),
            );
            if state.event_ring.len() > EVENT_RING_CAP {
                let excess = state.event_ring.len() - EVENT_RING_CAP;
                state.event_ring.drain(0..excess);
            }
            state.snapshot.tasks = tasks;
            state.snapshot.work = Some(work.clone());
        }
        let truth_changed = state.truth_state != ProjectedTaskTruthState::Confirmed;
        state.truth_state = ProjectedTaskTruthState::Confirmed;
        state.live_work = Some(work);
        rows_changed || truth_changed
    }

    pub fn reveal_completed_for_review(&self) -> bool {
        let mut state = lock_state(self, "reveal_completed_for_review");
        if !state.snapshot.tasks.is_empty() && !state.snapshot.has_incomplete() {
            state.snapshot.hidden = false;
            true
        } else {
            false
        }
    }

    pub fn hide_completed_after_review(&self) {
        let mut state = lock_state(self, "hide_completed_after_review");
        if !state.snapshot.tasks.is_empty() && !state.snapshot.has_incomplete() {
            state.snapshot.hidden = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::work_board_projection::SessionTaskStatusKind;
    use super::*;

    fn task(id: &str, status: SessionTaskStatusKind) -> SessionTask {
        SessionTask {
            id: id.into(),
            title: id.into(),
            description: None,
            status,
            subtasks: Vec::new(),
            created_at: String::new(),
            updated_at: String::new(),
            active_form: None,
            owner: None,
            metadata: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        }
    }

    #[test]
    fn projection_rows_and_truth_change_atomically() {
        let observer = TaskBoardObserver::new("session-a");
        assert!(observer.set_projected_task_projection(
            vec![task("work:1:main:a", SessionTaskStatusKind::Pending)],
            ProjectedTaskTruthState::Confirmed,
        ));
        let projection = observer.active_projection();
        assert_eq!(projection.truth_state(), TaskBoardTruthState::Confirmed);
        assert!(projection.has_tasks());
    }

    #[test]
    fn projection_reports_only_render_relevant_changes() {
        let observer = TaskBoardObserver::new("session-a");
        let tasks = vec![task("work:1:main:a", SessionTaskStatusKind::Pending)];
        assert!(
            observer
                .set_projected_task_projection(tasks.clone(), ProjectedTaskTruthState::Confirmed,)
        );
        assert!(
            !observer.set_projected_task_projection(tasks, ProjectedTaskTruthState::Confirmed),
            "the frame loop must not redraw unchanged remote projections"
        );
        assert!(
            observer
                .set_projected_task_projection(Vec::new(), ProjectedTaskTruthState::Unavailable,)
        );
    }

    #[test]
    fn rebind_clears_rows_and_confirmation_without_cross_session_bleed() {
        let observer = TaskBoardObserver::new("session-a");
        observer.set_projected_task_projection(
            vec![task("work:1:main:a", SessionTaskStatusKind::Pending)],
            ProjectedTaskTruthState::Confirmed,
        );
        observer.rebind_session("session-b");
        assert!(observer.snapshot().is_empty());
        assert_eq!(observer.truth_state(), TaskBoardTruthState::Loading);
    }

    #[test]
    fn unbound_projection_is_distinct_from_confirmed_empty() {
        let observer = TaskBoardObserver::new("");
        assert_eq!(observer.truth_state(), TaskBoardTruthState::Unbound);
        observer.rebind_session("session-a");
        observer.set_projected_task_projection(Vec::new(), ProjectedTaskTruthState::Confirmed);
        assert_eq!(observer.truth_state(), TaskBoardTruthState::Confirmed);
        assert!(!observer.active_projection().has_tasks());
    }

    #[test]
    fn live_receipt_survives_remote_unbound_until_matching_graph_catches_up() {
        let observer = TaskBoardObserver::new("session-a");
        let work = WorkBoardContext {
            work_id: "work-1".into(),
            branch_id: "main".into(),
            goal: "Ship the bounded work surface".into(),
            graph_revision: 3,
            criteria_member_count: 0,
            milestone_count: 0,
        };
        let active = task("work:work-1:main:task-1", SessionTaskStatusKind::InProgress);
        assert!(
            observer.apply_live_work_update(LiveWorkTaskBoardUpdate::Snapshot {
                work: work.clone(),
                tasks: vec![active.clone()],
            })
        );

        assert!(
            !observer.set_projected_work_projection(
                None,
                Vec::new(),
                ProjectedTaskTruthState::Unavailable,
            ),
            "a failed or not-yet-bound observer read must not blank a durable live receipt"
        );
        assert_eq!(observer.snapshot().tasks, vec![active.clone()]);

        let completed = task("work:work-1:main:task-1", SessionTaskStatusKind::Completed);
        assert!(observer.set_projected_work_projection(
            Some(work),
            vec![completed.clone()],
            ProjectedTaskTruthState::Confirmed,
        ));
        assert_eq!(observer.snapshot().tasks, vec![completed]);
    }

    #[test]
    fn live_delta_preserves_snapshot_topology_and_only_changes_task_state() {
        let observer = TaskBoardObserver::new("session-a");
        let work = WorkBoardContext {
            work_id: "work-1".into(),
            branch_id: "main".into(),
            goal: "Ship the bounded work surface".into(),
            graph_revision: 3,
            criteria_member_count: 0,
            milestone_count: 0,
        };
        let mut first = task("work:work-1:main:task-1", SessionTaskStatusKind::InProgress);
        first.blocks = vec!["work:work-1:main:task-2".into()];
        let mut second = task("work:work-1:main:task-2", SessionTaskStatusKind::Pending);
        second.blocked_by = vec![first.id.clone()];
        observer.apply_live_work_update(LiveWorkTaskBoardUpdate::Snapshot {
            work,
            tasks: vec![first.clone(), second.clone()],
        });

        let first_completed = task("work:work-1:main:task-1", SessionTaskStatusKind::Completed);
        let second_active = task("work:work-1:main:task-2", SessionTaskStatusKind::InProgress);
        assert!(
            observer.apply_live_work_update(LiveWorkTaskBoardUpdate::Upsert {
                work_id: "work-1".into(),
                branch_id: "main".into(),
                graph_revision: None,
                tasks: vec![first_completed, second_active],
            })
        );
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.tasks[0].blocks, first.blocks);
        assert_eq!(snapshot.tasks[1].blocked_by, second.blocked_by);
        assert_eq!(snapshot.tasks[1].status, SessionTaskStatusKind::InProgress);
    }
}
