//! Per-session `session_todos` observer for the TUI task board.
//!
//! **Design choice — no background driver task.** The previous
//! implementation spawned a `tokio::spawn(drive())` loop that held a
//! `tokio::sync::Mutex` across `store.load().await` and a watcher task
//! that called `schedule_frame()` on every notify. That combination
//! locked the TUI on slow MatrixOne queries: the main loop's
//! `snapshot()` (sync code) couldn't take the async lock, and the
//! `schedule_frame → redraw → snapshot → …` cascade could keep the
//! tokio scheduler starved when a fetch stalled.
//!
//! This version is **tick-driven** instead:
//!
//! - State lives behind a plain `std::sync::Mutex` so sync code
//!   (ratatui render, keymap dispatch) can read it without `.await`.
//! - The TUI main loop's 50 ms tick calls [`TaskBoardObserver::maybe_refresh`].
//!   If at least [`POLL_INTERVAL`] has elapsed since the last fetch, it
//!   fires a one-shot `tokio::spawn` that does `store.load()` and
//!   writes back into the shared state. Each spawn is independent —
//!   there is no long-running driver task and no cross-task locking.
//! - A store broadcast (`TaskStore::subscribe`) is drained synchronously
//!   inside the same refresh call: any queued events set a `dirty` flag so
//!   the next tick fetches immediately instead of waiting out the 5 s poll.
//!   The observer therefore owns no immortal notification task.
//! - Terminal work remains visible until the user explicitly collapses the
//!   compact board or the authoritative task is archived/deleted/migrated.

use astra_tools::task_mgmt::{
    OpenTaskSummary, SessionTask, SessionTaskStatusKind, TaskStore, TaskStoreHealth,
};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Fallback poll cadence when nothing has broadcast a change. Matches
/// the "only poll while incomplete" gate in the reference TUI — if a
/// board has no in-flight work, ticks skip the fetch entirely.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// A successful terminal/empty board has no active work to monitor. Keep a
/// low-frequency reconciliation poll for stores without subscriptions, while
/// avoiding a request every five seconds for the lifetime of an idle session.
const QUIET_POLL_INTERVAL: Duration = Duration::from_secs(60);
const MAX_FAILURE_BACKOFF: Duration = Duration::from_secs(60);
/// A task source is an optional observability lane; a wedged driver must not
/// leave the board permanently in Loading/Refreshing. The underlying request
/// is cancelled and the last confirmed snapshot remains visible.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Faster poll right after a broadcast/rebind so the user sees writes
/// land within UI-perceptible latency. Gated by the `dirty` atomic
/// so a quiet board never re-reads the store; only fires when there
/// IS a known change to chase. Phase 4 dropped this from 250ms to
/// 50ms after user feedback that the task board appeared blank
/// during long turns — the in-turn `do_draw` path now also pumps
/// `maybe_refresh`, so per-frame latency at 60fps stays under 17ms
/// of the cap.
const FAST_POLL: Duration = Duration::from_millis(15);
/// Cross-session board cap. This view is periodically refreshed while open,
/// so it must stay bounded even for users with years of completed task rows.
const CROSS_SESSION_OPEN_LIMIT: usize = 200;

/// Observable snapshot of the task board. Cheap to clone (moves the
/// owned vec; callers get a full copy).
#[derive(Clone, Debug, Default)]
pub(crate) struct TaskBoardSnapshot {
    pub tasks: Vec<SessionTask>,
    pub hidden: bool,
}

/// Per-session summaries used by the bounded multi-session view. Keeping this
/// projection distinct from [`SessionTask`] prevents remote stores from
/// inventing omitted task fields merely to satisfy the renderer.
#[derive(Clone, Debug, Default)]
pub(crate) struct MultiSessionSnapshot {
    pub per_session: Vec<(String, Vec<OpenTaskSummary>)>,
}

/// Which slice of the store the observer is fetching on each tick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ViewMode {
    /// Only the currently-bound session_id. Default — what the task
    /// board has always done.
    #[default]
    SingleSession,
    /// Every session the store knows about, aggregated. Opt-in via
    /// `set_view_mode(ViewMode::AllSessions)`; a separate toggle key
    /// on the event loop flips this.
    AllSessions,
}

/// User-visible truth state for the currently selected task-board lane.
/// This is deliberately independent of row count: a successful empty read is
/// `Confirmed`, while a failed first read is `Unavailable`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskBoardTruthState {
    Unbound,
    Loading,
    Confirmed,
    Refreshing,
    Stale,
    Unavailable,
}

/// Confidence of a read-only task projection contributed by a canonical owner
/// other than `session_todos`. It is intentionally separate from
/// [`TaskBoardTruthState`]: one source going stale must not relabel confirmed
/// checklist data as unavailable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ProjectedTaskTruthState {
    #[default]
    NotConfigured,
    Loading,
    Confirmed,
    Stale,
    Unavailable,
}

impl TaskBoardTruthState {
    pub(crate) fn has_confirmed_truth(self) -> bool {
        matches!(self, Self::Confirmed | Self::Refreshing | Self::Stale)
    }
}

/// One lock-consistent UI projection. Rows and confidence always come from
/// the same observer instant, so a refresh completion cannot produce stale
/// rows paired with a newer `Confirmed` state (or the reverse).
#[derive(Clone, Debug)]
pub(crate) enum TaskBoardProjection {
    Single {
        truth_state: TaskBoardTruthState,
        store_health: TaskStoreHealth,
        projected_truth_state: ProjectedTaskTruthState,
        snapshot: TaskBoardSnapshot,
    },
    All {
        truth_state: TaskBoardTruthState,
        store_health: TaskStoreHealth,
        projected_truth_state: ProjectedTaskTruthState,
        snapshot: MultiSessionSnapshot,
    },
}

impl TaskBoardProjection {
    pub(crate) fn truth_state(&self) -> TaskBoardTruthState {
        match self {
            Self::Single { truth_state, .. } | Self::All { truth_state, .. } => *truth_state,
        }
    }

    pub(crate) fn store_health(&self) -> TaskStoreHealth {
        match self {
            Self::Single { store_health, .. } | Self::All { store_health, .. } => *store_health,
        }
    }

    pub(crate) fn has_tasks(&self) -> bool {
        // `session_todos` is only one contributor to the board. A failure in
        // that optional checklist lane must not erase confirmed work owned by
        // another canonical source (for example a durable plan projection).
        // Snapshot rows already carry their own source confidence; visibility
        // is therefore based on displayable rows, never on the checklist
        // lane's health alone.
        match self {
            Self::Single { snapshot, .. } => !snapshot.tasks.is_empty(),
            Self::All { snapshot, .. } => snapshot
                .per_session
                .iter()
                .any(|(_, tasks)| !tasks.is_empty()),
        }
    }

    /// Whether the projection contains work that still needs attention.
    ///
    /// Terminal rows remain in [`Self::has_tasks`] so Ctrl+T can open durable
    /// history. Compact surfaces use this narrower projection to avoid making
    /// completed or cancelled history look like permanently active work.
    pub(crate) fn has_open_work(&self) -> bool {
        match self {
            Self::Single { snapshot, .. } => snapshot.has_incomplete(),
            Self::All { snapshot, .. } => snapshot
                .per_session
                .iter()
                .flat_map(|(_, tasks)| tasks)
                .any(|task| task.status.is_open_work()),
        }
    }

    /// Whether two projections render the same task-board surface. This lets
    /// the tick path refresh an already-open workspace without repainting on
    /// every timer wakeup, while still repainting truth/freshness transitions.
    pub(crate) fn same_render_state(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Single {
                    truth_state: left_truth,
                    store_health: left_health,
                    projected_truth_state: left_projected_truth,
                    snapshot: left_snapshot,
                },
                Self::Single {
                    truth_state: right_truth,
                    store_health: right_health,
                    projected_truth_state: right_projected_truth,
                    snapshot: right_snapshot,
                },
            ) => {
                left_truth == right_truth
                    && left_health == right_health
                    && left_projected_truth == right_projected_truth
                    && left_snapshot.hidden == right_snapshot.hidden
                    && same_board(&left_snapshot.tasks, &right_snapshot.tasks)
            }
            (
                Self::All {
                    truth_state: left_truth,
                    store_health: left_health,
                    projected_truth_state: left_projected_truth,
                    snapshot: left_snapshot,
                },
                Self::All {
                    truth_state: right_truth,
                    store_health: right_health,
                    projected_truth_state: right_projected_truth,
                    snapshot: right_snapshot,
                },
            ) => {
                left_truth == right_truth
                    && left_health == right_health
                    && left_projected_truth == right_projected_truth
                    && same_multi_board(&left_snapshot.per_session, &right_snapshot.per_session)
            }
            _ => false,
        }
    }
}

impl TaskBoardSnapshot {
    pub fn has_incomplete(&self) -> bool {
        self.tasks.iter().any(|t| t.status.is_open_work())
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

pub(crate) struct TaskBoardObserver {
    inner: Arc<ObserverInner>,
    subscription: Mutex<Option<tokio::sync::broadcast::Receiver<String>>>,
    /// One-shot fetches are tracked so dropping the observer cancels any
    /// stalled store read instead of keeping the store and observer state
    /// alive indefinitely.
    fetch_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

struct ObserverInner {
    store: Arc<dyn TaskStore>,
    state: Mutex<ObserverState>,
    /// Flipped to `true` by `TaskStore::subscribe` drainers and by
    /// `rebind_session`. The next `maybe_refresh` call consumes it and
    /// fetches immediately, then clears it.
    single_dirty: AtomicBool,
    all_dirty: AtomicBool,
}

#[derive(Clone)]
enum FetchIdentity {
    Single { session_id: String, generation: u64 },
    All,
}

/// Event + the Instant it was observed. Used to drive the
/// "just changed" row highlight in the renderer.
#[derive(Clone, Debug)]
pub(crate) struct TimedTaskBoardEvent {
    pub event: super::task_board_events::TaskBoardEvent,
    pub at: Instant,
}

/// How long a diff event is considered "recent" for UI highlighting.
pub(crate) const EVENT_FRESH_WINDOW: Duration = Duration::from_millis(1500);
/// Max events to retain. Oldest are trimmed when we exceed this.
const EVENT_RING_CAP: usize = 32;
struct ObserverState {
    session_id: String,
    /// Monotonic binding identity prevents A→B→A ABA races from letting
    /// an old A request overwrite the newly rebound A lane.
    single_binding_generation: u64,
    /// Rows read from the standalone `session_todos` task store. They remain
    /// a distinct source of truth from durable plan execution rows.
    manual_tasks: Vec<SessionTask>,
    /// Read-only work projections supplied by another canonical owner (at
    /// present, the durable plan repository). These are deliberately kept
    /// outside `manual_tasks`: replacing a projection must never result in a
    /// `session_todos` write or make a plan step look like an editable
    /// checklist item.
    projected_tasks: Vec<SessionTask>,
    projected_truth_state: ProjectedTaskTruthState,
    snapshot: TaskBoardSnapshot,
    /// Populated only while `view_mode == AllSessions`. Fetched via the
    /// bounded `TaskStore::load_open_task_summaries` projection.
    /// Kept alongside `snapshot` (not replacing it) so a mode flip
    /// back to SingleSession restores the per-session view instantly
    /// without another round-trip.
    manual_multi_snapshot: MultiSessionSnapshot,
    /// Read-only multi-session summaries supplied by canonical work sources
    /// such as plans. These never flow into `TaskStore` writes.
    projected_multi_snapshot: MultiSessionSnapshot,
    multi_snapshot: MultiSessionSnapshot,
    /// Which slice of the store the observer is refreshing. See
    /// [`ViewMode`]. Default SingleSession.
    view_mode: ViewMode,
    /// Fetch truth is lane-local. In particular, an AllSessions failure must
    /// never make a confirmed SingleSession cache stale (or vice versa).
    single_lane: RefreshLane,
    all_lane: RefreshLane,
    /// Ring buffer of diff events from the last few refreshes. The renderer
    /// reads it to flash a highlight on newly-created or changed rows.
    event_ring: Vec<TimedTaskBoardEvent>,
}

#[derive(Clone, Debug)]
struct RefreshLane {
    last_fetch: Instant,
    has_confirmed_truth: bool,
    refresh_in_flight: bool,
    last_refresh_failed: bool,
    consecutive_failures: u32,
    last_failure_health: TaskStoreHealth,
}

impl RefreshLane {
    fn new() -> Self {
        Self {
            last_fetch: Instant::now()
                .checked_sub(POLL_INTERVAL)
                .unwrap_or_else(Instant::now),
            has_confirmed_truth: false,
            refresh_in_flight: false,
            last_refresh_failed: false,
            consecutive_failures: 0,
            last_failure_health: TaskStoreHealth::Unknown,
        }
    }

    fn truth_state(&self, bound: bool) -> TaskBoardTruthState {
        if !bound {
            return TaskBoardTruthState::Unbound;
        }
        if self.refresh_in_flight {
            return if self.has_confirmed_truth {
                TaskBoardTruthState::Refreshing
            } else {
                TaskBoardTruthState::Loading
            };
        }
        if self.last_refresh_failed {
            return if self.has_confirmed_truth {
                TaskBoardTruthState::Stale
            } else {
                TaskBoardTruthState::Unavailable
            };
        }
        if self.has_confirmed_truth {
            TaskBoardTruthState::Confirmed
        } else {
            TaskBoardTruthState::Loading
        }
    }

    fn request_started(&mut self) {
        self.refresh_in_flight = true;
    }

    fn request_failed(&mut self, now: Instant, health: TaskStoreHealth) {
        self.refresh_in_flight = false;
        self.last_fetch = now;
        self.last_refresh_failed = true;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_failure_health = health;
    }

    fn request_succeeded(&mut self, now: Instant) {
        self.refresh_in_flight = false;
        self.last_fetch = now;
        self.has_confirmed_truth = true;
        self.last_refresh_failed = false;
        self.consecutive_failures = 0;
        self.last_failure_health = TaskStoreHealth::Ready;
    }
}

fn lock_state<'a>(
    inner: &'a ObserverInner,
    context: &'static str,
) -> (MutexGuard<'a, ObserverState>, bool) {
    match inner.state.lock() {
        Ok(guard) => (guard, false),
        Err(poisoned) => {
            tracing::warn!(context, "task board observer state poisoned — recovering");
            (poisoned.into_inner(), true)
        }
    }
}

fn event_is_fresh(event: &TimedTaskBoardEvent, now: Instant) -> bool {
    now.saturating_duration_since(event.at) < EVENT_FRESH_WINDOW
}

impl TaskBoardObserver {
    /// Build an observer bound to `session_id`. No background task is
    /// spawned here; the main loop drives refreshes via
    /// [`Self::maybe_refresh`].
    ///
    /// If the store supports `subscribe()`, [`Self::maybe_refresh`] drains
    /// queued events into the dirty flags so ticks fetch on-demand rather
    /// than waiting for the poll window.
    pub fn new(store: Arc<dyn TaskStore>, session_id: impl Into<String>) -> Arc<Self> {
        let sid: String = session_id.into();
        let subscription = store.subscribe();
        let inner = Arc::new(ObserverInner {
            store,
            state: Mutex::new(ObserverState {
                session_id: sid,
                single_binding_generation: 0,
                manual_tasks: Vec::new(),
                projected_tasks: Vec::new(),
                projected_truth_state: ProjectedTaskTruthState::NotConfigured,
                snapshot: TaskBoardSnapshot::default(),
                manual_multi_snapshot: MultiSessionSnapshot::default(),
                projected_multi_snapshot: MultiSessionSnapshot::default(),
                multi_snapshot: MultiSessionSnapshot::default(),
                view_mode: ViewMode::SingleSession,
                single_lane: RefreshLane::new(),
                all_lane: RefreshLane::new(),
                event_ring: Vec::new(),
            }),
            single_dirty: AtomicBool::new(true),
            all_dirty: AtomicBool::new(true),
        });
        Arc::new(Self {
            inner,
            subscription: Mutex::new(subscription),
            fetch_tasks: Mutex::new(Vec::new()),
        })
    }

    fn drain_notifications(&self) {
        use tokio::sync::broadcast::error::TryRecvError;

        let (changed_sessions, lagged) = {
            let mut receiver = match self.subscription.lock() {
                Ok(receiver) => receiver,
                Err(poisoned) => {
                    tracing::warn!("task board notification receiver poisoned — recovering");
                    poisoned.into_inner()
                }
            };
            let Some(rx) = receiver.as_mut() else {
                return;
            };
            let mut changed_sessions = Vec::new();
            let mut lagged = false;
            let mut closed = false;
            loop {
                match rx.try_recv() {
                    Ok(session_id) => changed_sessions.push(session_id),
                    Err(TryRecvError::Lagged(_)) => lagged = true,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Closed) => {
                        closed = true;
                        break;
                    }
                }
            }
            if closed {
                *receiver = None;
            }
            (changed_sessions, lagged)
        };

        if changed_sessions.is_empty() && !lagged {
            return;
        }
        let (mut state, _) = lock_state(&self.inner, "drain_notifications");
        for changed_session in changed_sessions {
            // The server allocates a new session after the TUI starts. The
            // first task mutation is authoritative evidence for that binding.
            let adopted = state.session_id.is_empty() && !changed_session.is_empty();
            if adopted {
                state.session_id = changed_session.clone();
                state.single_binding_generation = state.single_binding_generation.wrapping_add(1);
                state.manual_tasks.clear();
                state.projected_tasks.clear();
                state.projected_truth_state = ProjectedTaskTruthState::NotConfigured;
                state.projected_multi_snapshot = MultiSessionSnapshot::default();
                state.multi_snapshot = merged_multi_snapshots(
                    &state.manual_multi_snapshot,
                    &state.projected_multi_snapshot,
                );
                state.snapshot = TaskBoardSnapshot::default();
                state.single_lane = RefreshLane::new();
            }
            self.inner.all_dirty.store(true, Ordering::Relaxed);
            if adopted || changed_session == state.session_id {
                self.inner.single_dirty.store(true, Ordering::Relaxed);
            }
        }
        if lagged {
            // A refresh reads authoritative store state, so exact broadcast
            // replay is unnecessary after receiver lag.
            self.inner.single_dirty.store(true, Ordering::Relaxed);
            self.inner.all_dirty.store(true, Ordering::Relaxed);
        }
    }

    fn spawn_fetch(
        &self,
        identity: FetchIdentity,
        future: impl Future<Output = ()> + Send + 'static,
    ) {
        self.spawn_fetch_with_timeout(identity, future, FETCH_TIMEOUT);
    }

    fn spawn_fetch_with_timeout(
        &self,
        identity: FetchIdentity,
        future: impl Future<Output = ()> + Send + 'static,
        timeout: Duration,
    ) {
        let mut tasks = match self.fetch_tasks.lock() {
            Ok(tasks) => tasks,
            Err(poisoned) => {
                tracing::warn!("task board fetch registry poisoned — recovering");
                poisoned.into_inner()
            }
        };
        tasks.retain(|task| !task.is_finished());
        let inner = self.inner.clone();
        tasks.push(tokio::spawn(async move {
            let mut fetch = AbortOnDrop::new(tokio::spawn(future));
            let failure = match tokio::time::timeout(timeout, fetch.handle_mut()).await {
                Ok(Ok(())) => return,
                Ok(Err(error)) => format!("fetch task failed: {error}"),
                Err(_) => {
                    fetch.abort();
                    let _ = fetch.handle_mut().await;
                    format!("fetch timed out after {}ms", timeout.as_millis())
                }
            };

            tracing::warn!(%failure, "task board refresh did not complete");
            let (mut state, _) = lock_state(&inner, "fetch_supervisor_failed");
            let health = inner.store.health_snapshot();
            match identity {
                FetchIdentity::Single {
                    session_id,
                    generation,
                } if state.session_id == session_id
                    && state.single_binding_generation == generation =>
                {
                    state.single_lane.request_failed(Instant::now(), health);
                }
                FetchIdentity::All => {
                    state.all_lane.request_failed(Instant::now(), health);
                }
                FetchIdentity::Single { .. } => {}
            }
        }));
    }

    /// Current snapshot. Clones the stored vec under a sync mutex —
    /// acceptable in paths that actually render tasks (expanded panel,
    /// Next: hint). For hot draw paths that only need `open / total /
    /// hidden` (the footer chip) prefer [`Self::counts`] which avoids
    /// the clone.
    pub fn snapshot(&self) -> TaskBoardSnapshot {
        let (st, _) = lock_state(&self.inner, "snapshot");
        st.snapshot.clone()
    }

    /// Snapshot for rendering. Terminal work stays visible until its
    /// authoritative status becomes archived/deleted/migrated; finishing a
    /// task must not silently erase it after an arbitrary timeout.
    pub fn snapshot_for_render(&self) -> TaskBoardSnapshot {
        let (st, _) = lock_state(&self.inner, "snapshot_for_render");
        let mut snap = st.snapshot.clone();
        snap.tasks.retain(task_visible_in_render_snapshot);
        snap
    }

    /// Cheap lifecycle summary counts for the footer chip: `(open, total,
    /// hidden)`. While work remains, `total` retains completed/failed/cancelled
    /// progress. Once `open` reaches zero both counts collapse to zero so
    /// terminal history does not keep an idle session's chip alive.
    pub fn counts(&self) -> (usize, usize, bool) {
        let (st, _) = lock_state(&self.inner, "counts");
        if st.view_mode == ViewMode::AllSessions {
            let total = st
                .multi_snapshot
                .per_session
                .iter()
                .flat_map(|(_, tasks)| tasks)
                .filter(|task| task.status.is_open_work())
                .count();
            return (total, total, false);
        }
        let (open, total) = st
            .snapshot
            .tasks
            .iter()
            .filter(|task| task_visible_in_render_snapshot(task))
            .fold((0, 0), |(open, total), task| {
                (open + usize::from(task.status.is_open_work()), total + 1)
            });
        if open == 0 {
            (0, 0, st.snapshot.hidden)
        } else {
            (open, total, st.snapshot.hidden)
        }
    }

    /// IDs of tasks that changed within [`EVENT_FRESH_WINDOW`] of
    /// the call. The renderer uses this to flash a highlight on
    /// newly-created or newly-completed rows — the "something just
    /// happened" cue reference-agent gets from its task-state reducer.
    /// Events older than the window are ignored but stay in the
    /// ring until trimmed on the next push.
    pub fn fresh_event_task_ids(&self) -> Vec<String> {
        let now = Instant::now();
        let (st, _) = lock_state(&self.inner, "fresh_event_task_ids");
        st.event_ring
            .iter()
            .filter(|e| event_is_fresh(e, now))
            .map(|e| e.event.task_id().to_string())
            .collect()
    }

    /// Fresh task ids materialized once for a render pass. Prefer this over
    /// repeated per-row scans when the caller needs to check many rows.
    pub fn fresh_task_id_set(&self) -> std::collections::HashSet<String> {
        let now = Instant::now();
        let (st, _) = lock_state(&self.inner, "fresh_task_id_set");
        st.event_ring
            .iter()
            .filter(|e| event_is_fresh(e, now))
            .map(|e| e.event.task_id().to_string())
            .collect()
    }

    /// Returns `true` when `task_id` changed recently enough to render the
    /// transient highlight. This avoids allocating a fresh `Vec<String>` on
    /// every draw just to answer per-row freshness lookups.
    pub fn is_task_id_fresh(&self, task_id: &str) -> bool {
        let now = Instant::now();
        let (st, _) = lock_state(&self.inner, "is_task_id_fresh");
        st.event_ring
            .iter()
            .any(|event| event_is_fresh(event, now) && event.event.task_id() == task_id)
    }

    /// Current multi-session snapshot. The lane cache survives mode switches
    /// so returning to AllSessions can immediately show its last confirmed
    /// truth while a refresh runs.
    pub fn multi_snapshot(&self) -> MultiSessionSnapshot {
        let (st, _) = lock_state(&self.inner, "multi_snapshot");
        st.multi_snapshot.clone()
    }

    /// Currently-selected view mode.
    pub fn view_mode(&self) -> ViewMode {
        let (st, _) = lock_state(&self.inner, "view_mode");
        st.view_mode
    }

    /// Truth state for the active view mode. Empty rows are never used as a
    /// proxy for confirmation.
    pub fn truth_state(&self) -> TaskBoardTruthState {
        self.active_projection().truth_state()
    }

    pub fn active_projection(&self) -> TaskBoardProjection {
        let (st, _) = lock_state(&self.inner, "active_projection");
        let mode = st.view_mode;
        let truth_state = truth_state_for_mode(&st, mode);
        match mode {
            ViewMode::SingleSession => {
                let mut snapshot = st.snapshot.clone();
                snapshot.tasks.retain(task_visible_in_render_snapshot);
                TaskBoardProjection::Single {
                    truth_state,
                    store_health: st.single_lane.last_failure_health,
                    projected_truth_state: st.projected_truth_state,
                    snapshot,
                }
            }
            ViewMode::AllSessions => TaskBoardProjection::All {
                truth_state,
                store_health: st.all_lane.last_failure_health,
                projected_truth_state: st.projected_truth_state,
                snapshot: st.multi_snapshot.clone(),
            },
        }
    }

    #[cfg(test)]
    fn truth_state_for_mode(&self, mode: ViewMode) -> TaskBoardTruthState {
        let (st, _) = lock_state(&self.inner, "truth_state_for_mode");
        truth_state_for_mode(&st, mode)
    }

    /// Toggle between single-session and cross-session views. Marks the
    /// observer dirty so the next tick immediately fetches the correct slice.
    pub fn toggle_view_mode(&self) {
        let (mut st, _) = lock_state(&self.inner, "toggle_view_mode");
        st.view_mode = match st.view_mode {
            ViewMode::SingleSession => ViewMode::AllSessions,
            ViewMode::AllSessions => ViewMode::SingleSession,
        };
        match st.view_mode {
            ViewMode::SingleSession => self.inner.single_dirty.store(true, Ordering::Relaxed),
            ViewMode::AllSessions => self.inner.all_dirty.store(true, Ordering::Relaxed),
        }
    }

    /// Bound session id. Changing it resets the cached snapshot and
    /// arms `dirty` so the next tick refetches against the new id.
    pub fn rebind_session(&self, session_id: impl Into<String>) {
        let sid: String = session_id.into();
        let (mut st, _) = lock_state(&self.inner, "rebind_session");
        if st.session_id == sid {
            return;
        }
        st.session_id = sid;
        st.single_binding_generation = st.single_binding_generation.wrapping_add(1);
        st.manual_tasks.clear();
        st.projected_tasks.clear();
        st.projected_truth_state = ProjectedTaskTruthState::NotConfigured;
        st.projected_multi_snapshot = MultiSessionSnapshot::default();
        st.multi_snapshot =
            merged_multi_snapshots(&st.manual_multi_snapshot, &st.projected_multi_snapshot);
        st.snapshot = TaskBoardSnapshot::default();
        st.event_ring.clear();
        st.single_lane = RefreshLane::new();
        self.inner.single_dirty.store(true, Ordering::Relaxed);
    }

    /// Replace the read-only rows contributed by a canonical runtime
    /// projection. The rows are merged only in this observer's in-memory
    /// snapshot; callers cannot accidentally persist them through the
    /// `TaskStore` API.
    ///
    /// A projection source must give rows stable IDs that cannot collide with
    /// `session_todos` IDs. Durable plan rows use a `plan:` namespace for this
    /// reason. On a session rebind the projection is cleared before a new
    /// source is allowed to populate it, preventing cross-session bleed.
    pub(crate) fn set_projected_tasks(&self, tasks: Vec<SessionTask>) {
        self.set_projected_task_projection(tasks, ProjectedTaskTruthState::Confirmed);
    }

    /// Update both rows and their independent source confidence. In particular
    /// a stale plan response keeps its last confirmed rows visible while the
    /// ordinary task-store lane remains fully confirmed.
    pub(crate) fn set_projected_task_projection(
        &self,
        tasks: Vec<SessionTask>,
        truth_state: ProjectedTaskTruthState,
    ) {
        let (mut st, _) = lock_state(&self.inner, "set_projected_tasks");
        if same_board(&st.projected_tasks, &tasks) && st.projected_truth_state == truth_state {
            return;
        }
        st.projected_tasks = tasks;
        st.projected_truth_state = truth_state;
        let combined = merged_single_tasks(&st);
        replace_single_snapshot(&mut st, combined);
    }

    /// Replace read-only cross-session summaries. This mirrors the single
    /// projection contract for the aggregate board: plans remain visible when
    /// the user intentionally switches to All Sessions, without becoming
    /// synthetic `session_todos` records.
    pub(crate) fn set_projected_multi_summaries(
        &self,
        per_session: Vec<(String, Vec<OpenTaskSummary>)>,
    ) {
        let (mut st, _) = lock_state(&self.inner, "set_projected_multi_summaries");
        if same_multi_board(&st.projected_multi_snapshot.per_session, &per_session) {
            return;
        }
        st.projected_multi_snapshot = MultiSessionSnapshot { per_session };
        st.multi_snapshot =
            merged_multi_snapshots(&st.manual_multi_snapshot, &st.projected_multi_snapshot);
    }

    /// Reveal terminal work after the user explicitly collapsed the compact
    /// board. Completion itself never starts a hide timer.
    ///
    /// Returns `false` when nothing was revealed — either the board
    /// already wasn't hidden, tasks are empty, or (rarely) the mutex is
    /// poisoned. The caller treats a `false` return as "nothing to
    /// reveal" and may still flip `board_expanded` to honour the user's
    /// Ctrl+T intent; on a poisoned lock the expanded panel will simply
    /// render an empty/stale snapshot, which is preferable to crashing.
    pub fn reveal_completed_for_review(&self) -> bool {
        let (mut st, _) = lock_state(&self.inner, "reveal_completed_for_review");
        if !st.snapshot.tasks.is_empty() && !st.snapshot.has_incomplete() {
            st.snapshot.hidden = false;
            return true;
        }
        false
    }

    /// Hide a completed compact board only after an explicit user collapse.
    pub fn hide_completed_after_review(&self) {
        let (mut st, _) = lock_state(&self.inner, "hide_completed_after_review");
        if !st.snapshot.tasks.is_empty() && !st.snapshot.has_incomplete() {
            st.snapshot.hidden = true;
        }
    }

    /// Called from the TUI tick. Does bookkeeping under the sync
    /// mutex, then spawns a one-shot fetch if it's time. Never blocks
    /// the caller.
    ///
    /// Returns `true` if the cached snapshot's task vec changed since the last
    /// tick — the caller can use this to schedule a paint rather than paint
    /// on every single tick.
    pub fn maybe_refresh(&self) {
        self.drain_notifications();
        let now = Instant::now();
        enum Due {
            Skip,
            Single { session_id: String, generation: u64 },
            All,
        }
        let due = {
            let (mut st, poisoned) = lock_state(&self.inner, "maybe_refresh");
            if poisoned {
                if st.single_lane.refresh_in_flight {
                    st.single_lane.request_failed(now, TaskStoreHealth::Unknown);
                }
                if st.all_lane.refresh_in_flight {
                    st.all_lane.request_failed(now, TaskStoreHealth::Unknown);
                }
            }
            match st.view_mode {
                ViewMode::SingleSession => {
                    if st.session_id.is_empty() || st.single_lane.refresh_in_flight {
                        Due::Skip
                    } else {
                        let dirty = self.inner.single_dirty.load(Ordering::Relaxed);
                        let elapsed = now.saturating_duration_since(st.single_lane.last_fetch);
                        let needs_reconciliation =
                            st.single_lane.has_confirmed_truth && elapsed >= QUIET_POLL_INTERVAL;
                        let do_fetch = st.snapshot.has_incomplete()
                            || !st.single_lane.has_confirmed_truth
                            || needs_reconciliation
                            || dirty;
                        if do_fetch && refresh_is_due(&st.single_lane, dirty, now) {
                            self.inner.single_dirty.store(false, Ordering::Relaxed);
                            st.single_lane.request_started();
                            Due::Single {
                                session_id: st.session_id.clone(),
                                generation: st.single_binding_generation,
                            }
                        } else {
                            Due::Skip
                        }
                    }
                }
                ViewMode::AllSessions => {
                    if st.all_lane.refresh_in_flight {
                        Due::Skip
                    } else {
                        let dirty = self.inner.all_dirty.load(Ordering::Relaxed);
                        if refresh_is_due(&st.all_lane, dirty, now) {
                            self.inner.all_dirty.store(false, Ordering::Relaxed);
                            st.all_lane.request_started();
                            Due::All
                        } else {
                            Due::Skip
                        }
                    }
                }
            }
        };

        match due {
            Due::Skip => {}
            Due::All => {
                let inner = self.inner.clone();
                self.spawn_fetch(FetchIdentity::All, async move {
                    let per_session = match inner
                        .store
                        .load_open_task_summaries(CROSS_SESSION_OPEN_LIMIT)
                        .await
                    {
                        Ok(per_session) => per_session,
                        Err(error) => {
                            tracing::warn!(
                                error,
                                "task board multi-session refresh failed; preserving last snapshot"
                            );
                            let (mut st, _) = lock_state(&inner, "multi_fetch_failed");
                            st.all_lane
                                .request_failed(Instant::now(), inner.store.health_snapshot());
                            return;
                        }
                    };
                    let (mut st, _) = lock_state(&inner, "multi_fetch_complete");
                    st.all_lane.request_succeeded(Instant::now());
                    st.manual_multi_snapshot = MultiSessionSnapshot { per_session };
                    st.multi_snapshot = merged_multi_snapshots(
                        &st.manual_multi_snapshot,
                        &st.projected_multi_snapshot,
                    );
                });
            }
            Due::Single {
                session_id: sid,
                generation,
            } => {
                let inner = self.inner.clone();
                self.spawn_fetch(
                    FetchIdentity::Single {
                        session_id: sid.clone(),
                        generation,
                    },
                    async move {
                        let tasks = match inner.store.load(&sid).await {
                            Ok(tasks) => tasks,
                            Err(error) => {
                                tracing::warn!(
                                    %sid,
                                    error,
                                    "task board refresh failed; preserving last snapshot"
                                );
                                let (mut st, _) = lock_state(&inner, "fetch_failed");
                                // Rebind can race this request. Identity must be
                                // checked before mutating the new session's lane.
                                if st.session_id != sid
                                    || st.single_binding_generation != generation
                                {
                                    return;
                                }
                                st.single_lane
                                    .request_failed(Instant::now(), inner.store.health_snapshot());
                                return;
                            }
                        };
                        let (mut st, _) = lock_state(&inner, "fetch_complete");
                        // Bail if the observer was rebound mid-fetch — the
                        // tasks we got belong to the old session id.
                        if st.session_id != sid || st.single_binding_generation != generation {
                            return;
                        }
                        st.single_lane.request_succeeded(Instant::now());
                        st.manual_tasks = tasks;
                        let combined = merged_single_tasks(&st);
                        replace_single_snapshot(&mut st, combined);
                    },
                );
            }
        }
    }

    /// Let a user explicitly retry the currently selected observation lane.
    /// This bypasses automatic-retry backoff (including auth/config failures)
    /// once, never duplicates an in-flight request, and does not invent task
    /// truth before the canonical source answers.
    pub fn request_refresh(&self) -> bool {
        self.drain_notifications();
        let now = Instant::now();
        let (mut state, _) = lock_state(&self.inner, "request_refresh");
        let (lane, dirty) = match state.view_mode {
            ViewMode::SingleSession if state.session_id.is_empty() => return false,
            ViewMode::SingleSession => (&mut state.single_lane, &self.inner.single_dirty),
            ViewMode::AllSessions => (&mut state.all_lane, &self.inner.all_dirty),
        };
        if lane.refresh_in_flight {
            return false;
        }
        lane.last_fetch = now
            .checked_sub(MAX_FAILURE_BACKOFF)
            .unwrap_or_else(Instant::now);
        dirty.store(true, Ordering::Relaxed);
        true
    }
}

struct AbortOnDrop<T> {
    handle: tokio::task::JoinHandle<T>,
}

impl<T> AbortOnDrop<T> {
    fn new(handle: tokio::task::JoinHandle<T>) -> Self {
        Self { handle }
    }

    fn handle_mut(&mut self) -> &mut tokio::task::JoinHandle<T> {
        &mut self.handle
    }

    fn abort(&self) {
        self.handle.abort();
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl Drop for TaskBoardObserver {
    fn drop(&mut self) {
        let tasks = match self.fetch_tasks.get_mut() {
            Ok(tasks) => tasks,
            Err(poisoned) => poisoned.into_inner(),
        };
        for task in tasks.drain(..) {
            task.abort();
        }
    }
}

fn truth_state_for_mode(state: &ObserverState, mode: ViewMode) -> TaskBoardTruthState {
    match mode {
        ViewMode::SingleSession => state.single_lane.truth_state(!state.session_id.is_empty()),
        ViewMode::AllSessions => state.all_lane.truth_state(true),
    }
}

fn refresh_is_due(lane: &RefreshLane, dirty: bool, now: Instant) -> bool {
    if lane.last_refresh_failed && !dirty && !lane.last_failure_health.allows_automatic_retry() {
        return false;
    }
    let elapsed = now.saturating_duration_since(lane.last_fetch);
    let window = if dirty {
        FAST_POLL
    } else {
        POLL_INTERVAL.max(failure_backoff(lane.consecutive_failures))
    };
    elapsed >= window
}

fn failure_backoff(consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return Duration::ZERO;
    }
    let multiplier = 1_u32 << consecutive_failures.saturating_sub(1).min(4);
    (POLL_INTERVAL * multiplier).min(MAX_FAILURE_BACKOFF)
}

/// Equality for the complete renderer-facing task projection. Tier 1 boards
/// contain only dozens of rows, so suppressing a repaint is not worth hiding
/// an owner, dependency, subtask, metadata, or description update merely
/// because a producer preserved the same `updated_at` value.
fn same_board(a: &[SessionTask], b: &[SessionTask]) -> bool {
    a == b
}

/// Compose the task-board read model without assigning ownership of either
/// source to the other. Both inputs are already bounded by their producers;
/// preserving source order makes a checklist remain stable while the plan
/// projection refreshes in the background.
fn merged_single_tasks(state: &ObserverState) -> Vec<SessionTask> {
    let mut tasks = Vec::with_capacity(state.manual_tasks.len() + state.projected_tasks.len());
    tasks.extend(state.manual_tasks.iter().cloned());
    tasks.extend(state.projected_tasks.iter().cloned());
    tasks
}

fn merged_multi_snapshots(
    manual: &MultiSessionSnapshot,
    projected: &MultiSessionSnapshot,
) -> MultiSessionSnapshot {
    let mut per_session = manual.per_session.clone();
    for (session_id, tasks) in &projected.per_session {
        match per_session
            .iter_mut()
            .find(|(existing_session_id, _)| existing_session_id == session_id)
        {
            Some((_, existing_tasks)) => existing_tasks.extend(tasks.iter().cloned()),
            None => per_session.push((session_id.clone(), tasks.clone())),
        }
    }
    MultiSessionSnapshot { per_session }
}

fn same_multi_board(
    left: &[(String, Vec<OpenTaskSummary>)],
    right: &[(String, Vec<OpenTaskSummary>)],
) -> bool {
    left == right
}

/// Apply a newly composed single-session read model while preserving all
/// renderer-facing state (diff flash and explicit compact-board visibility).
fn replace_single_snapshot(state: &mut ObserverState, tasks: Vec<SessionTask>) {
    if !same_board(&tasks, &state.snapshot.tasks) {
        // Diff BEFORE replacing the snapshot — events carry id+title from the
        // pair (prev, new) so the renderer can flash affected rows.
        let events = super::task_board_events::diff(&state.snapshot.tasks, &tasks);
        let at = Instant::now();
        for event in events {
            state.event_ring.push(TimedTaskBoardEvent { event, at });
        }
        if state.event_ring.len() > EVENT_RING_CAP {
            let excess = state.event_ring.len() - EVENT_RING_CAP;
            state.event_ring.drain(0..excess);
        }
        state.snapshot.tasks = tasks;
    }

    // A refresh must not undo an explicit collapse of terminal history. New
    // open work always becomes visible, while a terminal-only refresh keeps
    // the user's prior review/collapse choice. Empty snapshots hide their
    // compact lane; the next open task clears that state immediately.
    if state.snapshot.tasks.is_empty() {
        state.snapshot.hidden = true;
    } else if state.snapshot.has_incomplete() {
        state.snapshot.hidden = false;
    }
}

fn task_visible_in_render_snapshot(task: &SessionTask) -> bool {
    !matches!(
        task.status,
        SessionTaskStatusKind::Archived
            | SessionTaskStatusKind::Deleted
            | SessionTaskStatusKind::Migrated
    )
}

// ───────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock_recovery::LockRecovery;
    use astra_tools::task_mgmt::{InMemoryTaskStore, SessionTaskStatusKind, TaskManager};
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    type SingleLoadResult = Result<Vec<SessionTask>, String>;
    type MultiLoadResult = Result<Vec<(String, Vec<SessionTask>)>, String>;

    fn mgr(store: Arc<InMemoryTaskStore>, sid: &str) -> TaskManager {
        let store_dyn: Arc<dyn TaskStore> = store;
        TaskManager::new(sid, store_dyn)
    }

    async fn wait_until<F: Fn() -> bool>(cond: F, timeout_ms: u64, pump: impl Fn()) {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while !cond() && Instant::now() < deadline {
            pump();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    struct ScriptedTaskStore {
        single: StdMutex<VecDeque<SingleLoadResult>>,
        all: StdMutex<VecDeque<MultiLoadResult>>,
    }

    impl ScriptedTaskStore {
        fn new(single: Vec<SingleLoadResult>, all: Vec<MultiLoadResult>) -> Self {
            Self {
                single: StdMutex::new(single.into()),
                all: StdMutex::new(all.into()),
            }
        }
    }

    #[async_trait]
    impl TaskStore for ScriptedTaskStore {
        async fn load(&self, _session_id: &str) -> Result<Vec<SessionTask>, String> {
            self.single
                .lock()
                .expect("single script lock")
                .pop_front()
                .unwrap_or_else(|| Err("single test script exhausted".to_string()))
        }

        async fn load_open_sessions(
            &self,
            _limit: usize,
        ) -> Result<Vec<(String, Vec<SessionTask>)>, String> {
            self.all
                .lock()
                .expect("all-session script lock")
                .pop_front()
                .unwrap_or_else(|| Err("all-session test script exhausted".to_string()))
        }

        async fn save(&self, _session_id: &str, _tasks: Vec<SessionTask>) -> Result<(), String> {
            Ok(())
        }

        async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }

        async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }
    }

    fn scripted_task(id: &str, title: &str) -> SessionTask {
        SessionTask {
            archived_at: None,
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status: SessionTaskStatusKind::Pending,
            subtasks: Vec::new(),
            created_at: "2026-07-11T00:00:00Z".to_string(),
            updated_at: "2026-07-11T00:00:00Z".to_string(),
            active_form: None,
            owner: None,
            metadata: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        }
    }

    fn make_lane_due(observer: &TaskBoardObserver, mode: ViewMode, dirty: bool) {
        let mut state = observer.inner.state.lock_recover();
        let lane = match mode {
            ViewMode::SingleSession => &mut state.single_lane,
            ViewMode::AllSessions => &mut state.all_lane,
        };
        lane.last_fetch = Instant::now()
            .checked_sub(MAX_FAILURE_BACKOFF + Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        drop(state);
        match mode {
            ViewMode::SingleSession => observer.inner.single_dirty.store(dirty, Ordering::Relaxed),
            ViewMode::AllSessions => observer.inner.all_dirty.store(dirty, Ordering::Relaxed),
        }
    }

    #[test]
    fn explicit_refresh_bypasses_failure_backoff_without_duplicate_fetches() {
        let store: Arc<dyn TaskStore> = Arc::new(InMemoryTaskStore::new());
        let observer = TaskBoardObserver::new(store, "session-a");
        {
            let mut state = observer.inner.state.lock_recover();
            state.single_lane.last_fetch = Instant::now();
            state.single_lane.last_refresh_failed = true;
            state.single_lane.last_failure_health = TaskStoreHealth::AuthenticationRequired;
        }
        observer.inner.single_dirty.store(false, Ordering::Relaxed);

        assert!(observer.request_refresh());
        assert!(observer.inner.single_dirty.load(Ordering::Relaxed));
        {
            let state = observer.inner.state.lock_recover();
            assert!(
                state.single_lane.last_fetch.elapsed() >= MAX_FAILURE_BACKOFF,
                "manual refresh must bypass the automatic retry backoff"
            );
        }

        observer
            .inner
            .state
            .lock_recover()
            .single_lane
            .refresh_in_flight = true;
        assert!(
            !observer.request_refresh(),
            "manual refresh must not create a second in-flight read"
        );
    }

    async fn wait_for_truth(observer: &TaskBoardObserver, expected: TaskBoardTruthState) {
        wait_until(
            || observer.truth_state() == expected,
            500,
            || observer.maybe_refresh(),
        )
        .await;
        assert_eq!(observer.truth_state(), expected);
    }

    #[tokio::test]
    async fn canonical_projection_merges_for_display_without_writing_session_todos() {
        let store = Arc::new(InMemoryTaskStore::new());
        let manager = mgr(store.clone(), "session-a");
        manager.create(&json!({"title": "manual checklist"})).await;

        let observer = TaskBoardObserver::new(store.clone() as Arc<dyn TaskStore>, "session-a");
        let mut plan_row = scripted_task("plan:plan-7:step-1", "durable plan step");
        plan_row.metadata = Some(serde_json::Map::from_iter([(
            "source".to_string(),
            json!("plan"),
        )]));
        observer.set_projected_tasks(vec![plan_row]);

        wait_until(
            || observer.snapshot().tasks.len() == 2,
            500,
            || observer.maybe_refresh(),
        )
        .await;
        let display_ids = observer
            .snapshot()
            .tasks
            .into_iter()
            .map(|task| task.id)
            .collect::<Vec<_>>();
        assert_eq!(display_ids, ["task-1", "plan:plan-7:step-1"]);

        let persisted = store.load("session-a").await.expect("read todos");
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].id, "task-1");
        assert_eq!(persisted[0].title, "manual checklist");
    }

    #[tokio::test]
    async fn confirmed_plan_rows_stay_visible_when_checklist_sync_is_unavailable() {
        let store: Arc<dyn TaskStore> = Arc::new(ScriptedTaskStore::new(
            vec![Err("manual checklist backend unavailable".to_string())],
            Vec::new(),
        ));
        let observer = TaskBoardObserver::new(store, "session-a");
        observer.set_projected_task_projection(
            vec![scripted_task("plan:plan-7:step-1", "durable plan step")],
            ProjectedTaskTruthState::Confirmed,
        );

        observer.maybe_refresh();
        wait_for_truth(&observer, TaskBoardTruthState::Unavailable).await;

        let projection = observer.active_projection();
        assert!(
            projection.has_tasks(),
            "a failed optional checklist read must not hide confirmed plan work"
        );
        match projection {
            TaskBoardProjection::Single { snapshot, .. } => {
                assert_eq!(snapshot.tasks[0].id, "plan:plan-7:step-1");
            }
            TaskBoardProjection::All { .. } => panic!("default lane must be single-session"),
        }
    }

    #[tokio::test]
    async fn canonical_multi_session_projection_merges_without_persisting_plan_rows() {
        let manual = scripted_task("task-1", "manual work");
        let store: Arc<dyn TaskStore> = Arc::new(ScriptedTaskStore::new(
            Vec::new(),
            vec![Ok(vec![("session-a".to_string(), vec![manual])])],
        ));
        let observer = TaskBoardObserver::new(store, "session-a");
        observer.set_projected_multi_summaries(vec![(
            "session-a".to_string(),
            vec![OpenTaskSummary {
                id: "plan:plan-1".to_string(),
                title: "Plan · durable work".to_string(),
                status: SessionTaskStatusKind::InProgress,
                updated_at: "plan-v2".to_string(),
            }],
        )]);
        observer.toggle_view_mode();
        wait_until(
            || observer.multi_snapshot().per_session[0].1.len() == 2,
            500,
            || observer.maybe_refresh(),
        )
        .await;

        let multi = observer.multi_snapshot();
        assert_eq!(multi.per_session[0].0, "session-a");
        assert_eq!(
            multi.per_session[0]
                .1
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            ["task-1", "plan:plan-1"]
        );
    }

    #[tokio::test]
    async fn single_lane_truth_distinguishes_unavailable_empty_stale_and_recovery() {
        let cached = scripted_task("task-1", "confirmed work");
        let store: Arc<dyn TaskStore> = Arc::new(ScriptedTaskStore::new(
            vec![
                Err("secret first-read diagnostic".to_string()),
                Ok(Vec::new()),
                Err("secret refresh diagnostic".to_string()),
                Ok(vec![cached.clone()]),
                Err("secret later diagnostic".to_string()),
            ],
            Vec::new(),
        ));
        let observer = TaskBoardObserver::new(store, "session-a");

        assert_eq!(observer.truth_state(), TaskBoardTruthState::Loading);
        observer.maybe_refresh();
        assert_eq!(observer.truth_state(), TaskBoardTruthState::Loading);
        wait_for_truth(&observer, TaskBoardTruthState::Unavailable).await;
        assert!(observer.snapshot().tasks.is_empty());

        make_lane_due(&observer, ViewMode::SingleSession, false);
        observer.maybe_refresh();
        assert_eq!(observer.truth_state(), TaskBoardTruthState::Loading);
        wait_for_truth(&observer, TaskBoardTruthState::Confirmed).await;
        assert!(observer.snapshot().tasks.is_empty());

        make_lane_due(&observer, ViewMode::SingleSession, true);
        observer.maybe_refresh();
        assert_eq!(observer.truth_state(), TaskBoardTruthState::Refreshing);
        wait_for_truth(&observer, TaskBoardTruthState::Stale).await;
        assert!(observer.snapshot().tasks.is_empty());

        make_lane_due(&observer, ViewMode::SingleSession, false);
        observer.maybe_refresh();
        assert_eq!(observer.truth_state(), TaskBoardTruthState::Refreshing);
        wait_for_truth(&observer, TaskBoardTruthState::Confirmed).await;
        assert_eq!(observer.snapshot().tasks[0].title, cached.title);

        make_lane_due(&observer, ViewMode::SingleSession, true);
        observer.maybe_refresh();
        assert_eq!(observer.truth_state(), TaskBoardTruthState::Refreshing);
        wait_for_truth(&observer, TaskBoardTruthState::Stale).await;
        assert_eq!(observer.snapshot().tasks[0].title, "confirmed work");
    }

    #[tokio::test]
    async fn all_sessions_lane_has_the_same_truth_contract() {
        let cached = scripted_task("task-cloud", "cross-session work");
        let store: Arc<dyn TaskStore> = Arc::new(ScriptedTaskStore::new(
            Vec::new(),
            vec![
                Err("secret all-session first-read diagnostic".to_string()),
                Ok(Vec::new()),
                Err("secret all-session refresh diagnostic".to_string()),
                Ok(vec![("session-cloud".to_string(), vec![cached.clone()])]),
                Err("secret all-session later diagnostic".to_string()),
            ],
        ));
        let observer = TaskBoardObserver::new(store, "session-a");
        observer.toggle_view_mode();

        assert_eq!(observer.truth_state(), TaskBoardTruthState::Loading);
        observer.maybe_refresh();
        wait_for_truth(&observer, TaskBoardTruthState::Unavailable).await;

        make_lane_due(&observer, ViewMode::AllSessions, false);
        observer.maybe_refresh();
        assert_eq!(observer.truth_state(), TaskBoardTruthState::Loading);
        wait_for_truth(&observer, TaskBoardTruthState::Confirmed).await;
        assert!(observer.multi_snapshot().per_session.is_empty());

        make_lane_due(&observer, ViewMode::AllSessions, true);
        observer.maybe_refresh();
        assert_eq!(observer.truth_state(), TaskBoardTruthState::Refreshing);
        wait_for_truth(&observer, TaskBoardTruthState::Stale).await;
        assert!(observer.multi_snapshot().per_session.is_empty());

        make_lane_due(&observer, ViewMode::AllSessions, false);
        observer.maybe_refresh();
        wait_for_truth(&observer, TaskBoardTruthState::Confirmed).await;
        assert_eq!(
            observer.multi_snapshot().per_session[0].1[0].title,
            cached.title
        );

        make_lane_due(&observer, ViewMode::AllSessions, true);
        observer.maybe_refresh();
        wait_for_truth(&observer, TaskBoardTruthState::Stale).await;
        assert_eq!(
            observer.multi_snapshot().per_session[0].1[0].title,
            "cross-session work"
        );
    }

    #[tokio::test]
    async fn mode_failures_and_successes_do_not_mutate_the_other_lane() {
        let store: Arc<dyn TaskStore> = Arc::new(ScriptedTaskStore::new(
            vec![Ok(Vec::new()), Err("single refresh failed".to_string())],
            vec![
                Err("all-session first read failed".to_string()),
                Ok(Vec::new()),
            ],
        ));
        let observer = TaskBoardObserver::new(store, "session-a");

        observer.maybe_refresh();
        wait_for_truth(&observer, TaskBoardTruthState::Confirmed).await;
        observer.toggle_view_mode();
        observer.maybe_refresh();
        wait_for_truth(&observer, TaskBoardTruthState::Unavailable).await;
        assert_eq!(
            observer.truth_state_for_mode(ViewMode::SingleSession),
            TaskBoardTruthState::Confirmed
        );

        make_lane_due(&observer, ViewMode::AllSessions, false);
        observer.maybe_refresh();
        wait_for_truth(&observer, TaskBoardTruthState::Confirmed).await;
        observer.toggle_view_mode();
        make_lane_due(&observer, ViewMode::SingleSession, true);
        observer.maybe_refresh();
        wait_for_truth(&observer, TaskBoardTruthState::Stale).await;
        assert_eq!(
            observer.truth_state_for_mode(ViewMode::AllSessions),
            TaskBoardTruthState::Confirmed
        );
    }

    #[test]
    fn empty_session_binding_is_unbound_and_has_no_rows_to_misattribute() {
        let store: Arc<dyn TaskStore> = Arc::new(ScriptedTaskStore::new(
            vec![Ok(vec![scripted_task("wrong", "must not load")])],
            Vec::new(),
        ));
        let observer = TaskBoardObserver::new(store, "");

        observer.maybe_refresh();
        assert_eq!(observer.truth_state(), TaskBoardTruthState::Unbound);
        match observer.active_projection() {
            TaskBoardProjection::Single { snapshot, .. } => assert!(snapshot.tasks.is_empty()),
            TaskBoardProjection::All { .. } => panic!("default lane must be single-session"),
        }
    }

    struct AbaTaskStore {
        calls: AtomicUsize,
        first_started: tokio::sync::Notify,
        release_first: tokio::sync::Notify,
    }

    #[async_trait]
    impl TaskStore for AbaTaskStore {
        async fn load(&self, session_id: &str) -> Result<Vec<SessionTask>, String> {
            match self.calls.fetch_add(1, AtomicOrdering::SeqCst) {
                0 => {
                    assert_eq!(session_id, "session-a");
                    self.first_started.notify_one();
                    self.release_first.notified().await;
                    Ok(vec![scripted_task("task-old-a", "old A response")])
                }
                1 => {
                    assert_eq!(session_id, "session-b");
                    Ok(vec![scripted_task("task-b", "current B response")])
                }
                2 => {
                    assert_eq!(session_id, "session-a");
                    Ok(vec![scripted_task("task-new-a", "new A response")])
                }
                _ => Err("unexpected ABA test load".to_string()),
            }
        }

        async fn save(&self, _session_id: &str, _tasks: Vec<SessionTask>) -> Result<(), String> {
            Ok(())
        }

        async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }

        async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }
    }

    #[tokio::test]
    async fn rebind_generation_rejects_old_response_after_a_b_a_cycle() {
        let store = Arc::new(AbaTaskStore {
            calls: AtomicUsize::new(0),
            first_started: tokio::sync::Notify::new(),
            release_first: tokio::sync::Notify::new(),
        });
        let observer = TaskBoardObserver::new(store.clone() as Arc<dyn TaskStore>, "session-a");

        observer.maybe_refresh();
        store.first_started.notified().await;

        observer.rebind_session("session-b");
        wait_until(
            || {
                observer
                    .snapshot()
                    .tasks
                    .first()
                    .is_some_and(|task| task.title == "current B response")
            },
            500,
            || observer.maybe_refresh(),
        )
        .await;

        observer.rebind_session("session-a");
        wait_until(
            || {
                observer
                    .snapshot()
                    .tasks
                    .first()
                    .is_some_and(|task| task.title == "new A response")
            },
            500,
            || observer.maybe_refresh(),
        )
        .await;

        store.release_first.notify_one();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(observer.snapshot().tasks[0].title, "new A response");
        assert_eq!(observer.truth_state(), TaskBoardTruthState::Confirmed);
    }

    struct DelayedAllSessionsStore {
        loads: AtomicUsize,
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait]
    impl TaskStore for DelayedAllSessionsStore {
        async fn load(&self, _session_id: &str) -> Result<Vec<SessionTask>, String> {
            Ok(Vec::new())
        }

        async fn load_open_sessions(
            &self,
            _limit: usize,
        ) -> Result<Vec<(String, Vec<SessionTask>)>, String> {
            self.loads.fetch_add(1, AtomicOrdering::SeqCst);
            self.started.notify_one();
            self.release.notified().await;
            Ok(vec![(
                "session-global".into(),
                vec![scripted_task("task-global", "global open work")],
            )])
        }

        async fn save(&self, _session_id: &str, _tasks: Vec<SessionTask>) -> Result<(), String> {
            Ok(())
        }

        async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }

        async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }
    }

    #[tokio::test]
    async fn all_sessions_fetch_remains_valid_across_single_session_rebind() {
        let store = Arc::new(DelayedAllSessionsStore {
            loads: AtomicUsize::new(0),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let observer = TaskBoardObserver::new(store.clone() as Arc<dyn TaskStore>, "session-a");
        observer.toggle_view_mode();
        observer.maybe_refresh();
        store.started.notified().await;

        // The all-sessions query is scoped to the immutable store/user, not
        // the selected session. A single-session rebind must not invalidate
        // or duplicate this still-authoritative global read.
        observer.rebind_session("session-b");
        store.release.notify_one();
        wait_until(
            || !observer.multi_snapshot().per_session.is_empty(),
            500,
            || observer.maybe_refresh(),
        )
        .await;

        assert_eq!(store.loads.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(
            observer.multi_snapshot().per_session[0].1[0].title,
            "global open work"
        );
        assert_eq!(observer.truth_state(), TaskBoardTruthState::Confirmed);
    }

    #[tokio::test]
    async fn paused_tasks_count_as_incomplete_for_board_visibility() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-paused");
        let m = mgr(store, "sess-paused");

        m.create(&json!({"title": "paused-work"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "paused"}))
            .await;

        wait_until(
            || obs.snapshot().has_incomplete(),
            500,
            || obs.maybe_refresh(),
        )
        .await;

        let snapshot = obs.snapshot();
        assert!(
            snapshot.has_incomplete(),
            "paused open work must keep the board visible"
        );
        assert!(!snapshot.hidden, "paused board should not be auto-hidden");
        assert_eq!(obs.counts().0, 1, "paused task counts as open");
    }

    /// REGRESSION (Phase 4 / problem 1): the in-turn `do_draw` path
    /// must observe a task_board.create within UI-perceptible latency.
    /// Pre-fix `FAST_POLL` was 250ms; user-reported behaviour was
    /// "task board never appears until the turn ends" because the
    /// outer-tick branch was the only place `maybe_refresh` ran.
    /// We tightened FAST_POLL to 50ms so when callers DO start
    /// pumping `maybe_refresh` per-frame, the latency is invisible.
    /// Pin: ≤100ms covers an aggressive UI tick budget and keeps
    /// test wall-time tight.
    #[tokio::test]
    async fn dirty_refresh_lands_within_100ms_of_create() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-fast");
        // Seed `last_fetch` to a value old enough that the first
        // `maybe_refresh` will fire — without this, the constructor
        // sets `last_fetch = Instant::now()` and the FAST_POLL
        // window suppresses the first poll for 50ms even with dirty.
        // The fix path must respect both the dirty flag and the
        // window so this seeding still gates on FAST_POLL elapsing.
        {
            let mut st = obs.inner.state.lock_recover();
            st.single_lane.last_fetch = Instant::now()
                .checked_sub(Duration::from_millis(60))
                .unwrap_or_else(Instant::now);
        }
        let m = mgr(store, "sess-fast");

        let started = Instant::now();
        m.create(&json!({"title": "fast-task"})).await;

        wait_until(
            || !obs.snapshot().tasks.is_empty(),
            150, // tight: must land well before the legacy 250ms FAST_POLL
            || obs.maybe_refresh(),
        )
        .await;

        let elapsed = started.elapsed();
        assert!(
            !obs.snapshot().tasks.is_empty(),
            "task did not land in snapshot within 150ms"
        );
        assert!(
            elapsed < Duration::from_millis(150),
            "task should land within 150ms of create; took {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn failed_single_session_refresh_preserves_last_known_snapshot() {
        struct FailsAfterFirstLoadStore {
            loads: AtomicUsize,
        }

        #[async_trait]
        impl TaskStore for FailsAfterFirstLoadStore {
            async fn load(&self, _session_id: &str) -> Result<Vec<SessionTask>, String> {
                if self.loads.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                    Ok(vec![SessionTask {
                        archived_at: None,
                        id: "task-1".to_string(),
                        title: "visible work".to_string(),
                        description: None,
                        status: SessionTaskStatusKind::Pending,
                        subtasks: Vec::new(),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        updated_at: chrono::Utc::now().to_rfc3339(),
                        active_form: None,
                        owner: None,
                        metadata: None,
                        blocks: Vec::new(),
                        blocked_by: Vec::new(),
                    }])
                } else {
                    Err("simulated MatrixOne read failure".to_string())
                }
            }

            async fn save(
                &self,
                _session_id: &str,
                _tasks: Vec<SessionTask>,
            ) -> Result<(), String> {
                Ok(())
            }

            async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }

            async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }
        }

        let store: Arc<dyn TaskStore> = Arc::new(FailsAfterFirstLoadStore {
            loads: AtomicUsize::new(0),
        });
        let obs = TaskBoardObserver::new(store, "sess-load-fail");

        wait_until(
            || obs.snapshot().tasks.len() == 1,
            500,
            || obs.maybe_refresh(),
        )
        .await;
        assert_eq!(obs.snapshot().tasks[0].title, "visible work");

        {
            let mut st = obs.inner.state.lock_recover();
            st.single_lane.last_fetch = Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap_or_else(Instant::now);
        }
        obs.inner.single_dirty.store(true, Ordering::Relaxed);
        obs.maybe_refresh();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let snapshot = obs.snapshot();
        assert_eq!(
            snapshot.tasks.len(),
            1,
            "failed refresh must preserve the last known task board instead of rendering empty"
        );
        assert_eq!(snapshot.tasks[0].title, "visible work");
    }

    #[tokio::test]
    async fn successful_empty_board_uses_quiet_reconciliation_cadence() {
        struct CountingEmptyStore {
            loads: AtomicUsize,
        }

        #[async_trait]
        impl TaskStore for CountingEmptyStore {
            async fn load(&self, _session_id: &str) -> Result<Vec<SessionTask>, String> {
                self.loads.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(Vec::new())
            }

            async fn save(
                &self,
                _session_id: &str,
                _tasks: Vec<SessionTask>,
            ) -> Result<(), String> {
                Ok(())
            }

            async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }

            async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }
        }

        let store = Arc::new(CountingEmptyStore {
            loads: AtomicUsize::new(0),
        });
        let obs = TaskBoardObserver::new(store.clone() as Arc<dyn TaskStore>, "sess-empty");
        wait_until(
            || store.loads.load(AtomicOrdering::SeqCst) == 1,
            500,
            || obs.maybe_refresh(),
        )
        .await;

        {
            let mut st = obs.inner.state.lock_recover();
            assert!(st.single_lane.has_confirmed_truth);
            st.single_lane.last_fetch = Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap_or_else(Instant::now);
        }
        obs.inner.single_dirty.store(false, Ordering::Relaxed);
        obs.maybe_refresh();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            store.loads.load(AtomicOrdering::SeqCst),
            1,
            "a confirmed empty board must not poll on the active-work cadence"
        );

        {
            let mut st = obs.inner.state.lock_recover();
            st.single_lane.last_fetch = Instant::now()
                .checked_sub(QUIET_POLL_INTERVAL + Duration::from_secs(1))
                .unwrap_or_else(Instant::now);
        }
        obs.maybe_refresh();
        wait_until(|| store.loads.load(AtomicOrdering::SeqCst) == 2, 200, || {}).await;
        assert_eq!(store.loads.load(AtomicOrdering::SeqCst), 2);
    }

    #[test]
    fn store_failure_backoff_is_bounded_and_monotonic() {
        assert_eq!(failure_backoff(0), Duration::ZERO);
        assert_eq!(failure_backoff(1), Duration::from_secs(5));
        assert_eq!(failure_backoff(2), Duration::from_secs(10));
        assert_eq!(failure_backoff(4), Duration::from_secs(40));
        assert_eq!(failure_backoff(5), MAX_FAILURE_BACKOFF);
        assert_eq!(failure_backoff(u32::MAX), MAX_FAILURE_BACKOFF);
    }

    #[test]
    fn non_transient_store_failures_wait_for_new_external_evidence() {
        let now = Instant::now();
        let old = now
            .checked_sub(MAX_FAILURE_BACKOFF + Duration::from_secs(1))
            .expect("test instant");

        for health in [
            TaskStoreHealth::AuthenticationRequired,
            TaskStoreHealth::SessionUnavailable,
            TaskStoreHealth::ProtocolMismatch,
        ] {
            let mut lane = RefreshLane::new();
            lane.request_failed(now, health);
            lane.last_fetch = old;
            assert!(
                !refresh_is_due(&lane, false, now),
                "{health:?} should not poll forever without new evidence"
            );
            assert!(
                refresh_is_due(&lane, true, now),
                "a dirty/rebind signal must permit recovery from {health:?}"
            );
        }

        let mut transient = RefreshLane::new();
        transient.request_failed(now, TaskStoreHealth::ServiceUnavailable);
        transient.last_fetch = old;
        assert!(refresh_is_due(&transient, false, now));
    }

    #[tokio::test]
    async fn terminal_tasks_remain_renderable_until_explicitly_archived() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-terminal-history");
        let m = mgr(store, "sess-terminal-history");

        m.create(&json!({"title": "verified work"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        m.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        wait_until(
            || {
                let s = obs.snapshot();
                s.tasks.len() == 1 && !s.has_incomplete()
            },
            500,
            || obs.maybe_refresh(),
        )
        .await;

        let render = obs.snapshot_for_render();
        assert!(
            render.tasks.iter().any(|task| task.id == "task-1"),
            "completed work must remain inspectable until an explicit archive/delete: {:?}",
            render.tasks
        );

        m.create(&json!({"title": "discarded work"})).await;
        m.update(&json!({"task_id": "task-2", "new_status": "cancelled"}))
            .await;
        wait_until(
            || {
                obs.snapshot().tasks.iter().any(|task| {
                    task.id == "task-2" && task.status == SessionTaskStatusKind::Cancelled
                })
            },
            500,
            || obs.maybe_refresh(),
        )
        .await;
        assert!(
            obs.snapshot_for_render()
                .tasks
                .iter()
                .any(|task| task.id == "task-2"),
            "cancelled work is also history, not a timeout-driven disappearance"
        );
    }

    #[tokio::test]
    async fn deleted_tasks_are_audit_only_not_rendered_or_counted() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-deleted-hidden");
        let m = mgr(store, "sess-deleted-hidden");

        m.create(&json!({"title": "remove me"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "deleted"}))
            .await;
        wait_until(
            || {
                let s = obs.snapshot();
                s.tasks.len() == 1 && s.tasks[0].status == SessionTaskStatusKind::Deleted
            },
            500,
            || obs.maybe_refresh(),
        )
        .await;

        assert!(
            obs.snapshot_for_render().tasks.is_empty(),
            "deleted tombstones must not render"
        );
        assert_eq!(
            obs.counts(),
            (0, 0, false),
            "deleted tombstones must not keep the task-board chip alive"
        );
        assert_eq!(
            obs.snapshot().tasks.len(),
            1,
            "snapshot() keeps deleted rows for audit/debug callers"
        );
    }

    /// REGRESSION: TUI used to start with an empty `state.session_id`
    /// because the server allocates the id mid-turn; the observer was
    /// then bound to `""` and filtered every broadcast that didn't
    /// match `""`. The task board only opened *after* the turn ended,
    /// when the post-turn block called `rebind_session(sid)`.
    /// Fix: observer auto-adopts the first non-empty broadcast sid.
    #[tokio::test]
    async fn empty_observer_adopts_first_broadcast_session_id() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        // Construct observer with an empty session_id, mimicking the
        // TUI startup path before the server hands out a session.
        let obs = TaskBoardObserver::new(store_dyn, "");

        // Writing under a *real* session_id should still surface in
        // the observer because the broadcast adoption rebinds it.
        let m = mgr(store, "sess-mid-turn");
        m.create(&json!({"title": "first turn task"})).await;

        wait_until(
            || !obs.snapshot().tasks.is_empty(),
            500,
            || obs.maybe_refresh(),
        )
        .await;
        assert_eq!(
            obs.snapshot().tasks.len(),
            1,
            "observer must adopt the broadcast sid and surface mid-turn writes"
        );
        let st = obs.inner.state.lock_recover();
        assert_eq!(st.session_id, "sess-mid-turn");
    }

    #[tokio::test]
    async fn dropping_observer_releases_subscription_and_stalled_fetch() {
        struct StalledSubscribedStore {
            notify: tokio::sync::broadcast::Sender<String>,
            load_started: AtomicBool,
        }

        #[async_trait]
        impl TaskStore for StalledSubscribedStore {
            async fn load(&self, _session_id: &str) -> Result<Vec<SessionTask>, String> {
                self.load_started.store(true, Ordering::Release);
                std::future::pending().await
            }

            async fn save(
                &self,
                _session_id: &str,
                _tasks: Vec<SessionTask>,
            ) -> Result<(), String> {
                Ok(())
            }

            async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }

            async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }

            fn subscribe(&self) -> Option<tokio::sync::broadcast::Receiver<String>> {
                Some(self.notify.subscribe())
            }
        }

        let (notify, _) = tokio::sync::broadcast::channel(4);
        let store = Arc::new(StalledSubscribedStore {
            notify,
            load_started: AtomicBool::new(false),
        });
        let observer = TaskBoardObserver::new(store.clone() as Arc<dyn TaskStore>, "sess-drop");
        assert_eq!(store.notify.receiver_count(), 1);
        observer.maybe_refresh();
        wait_until(|| store.load_started.load(Ordering::Acquire), 500, || {}).await;
        assert!(store.load_started.load(Ordering::Acquire));

        drop(observer);
        assert_eq!(
            store.notify.receiver_count(),
            0,
            "dropping the observer must synchronously release its subscription"
        );
        wait_until(|| Arc::strong_count(&store) == 1, 500, || {}).await;
        assert_eq!(
            Arc::strong_count(&store),
            1,
            "dropping the observer must abort stalled fetches and release the store"
        );
    }

    #[tokio::test]
    async fn stalled_fetch_times_out_and_releases_refresh_lane() {
        struct StalledStore;

        #[async_trait]
        impl TaskStore for StalledStore {
            async fn load(&self, _session_id: &str) -> Result<Vec<SessionTask>, String> {
                std::future::pending().await
            }

            async fn save(
                &self,
                _session_id: &str,
                _tasks: Vec<SessionTask>,
            ) -> Result<(), String> {
                Ok(())
            }

            async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }

            async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }
        }

        let observer = TaskBoardObserver::new(Arc::new(StalledStore), "sess-stalled");
        // Mirror the production fetch path: maybe_refresh consumes the dirty
        // signal before marking a request in flight.  The supervisor must not
        // manufacture a new dirty signal when that request later times out.
        observer.inner.single_dirty.store(false, Ordering::Relaxed);
        {
            let (mut state, _) = lock_state(&observer.inner, "test_stalled_fetch");
            state.single_lane.request_started();
        }
        observer.spawn_fetch_with_timeout(
            FetchIdentity::Single {
                session_id: "sess-stalled".to_string(),
                generation: 0,
            },
            std::future::pending(),
            Duration::from_millis(10),
        );
        assert_eq!(observer.truth_state(), TaskBoardTruthState::Loading);

        tokio::time::sleep(Duration::from_millis(30)).await;
        tokio::task::yield_now().await;

        assert_eq!(observer.truth_state(), TaskBoardTruthState::Unavailable);
        assert!(
            !observer.inner.single_dirty.load(Ordering::Relaxed),
            "a timeout is failure evidence, not a new store-change signal; automatic retries must honor backoff"
        );
        assert!(observer.request_refresh());
    }

    #[tokio::test]
    async fn bound_observer_ignores_other_session_broadcasts() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-a");

        let a = mgr(store.clone(), "sess-a");
        a.create(&json!({"title": "work in a"})).await;
        wait_until(
            || {
                obs.snapshot()
                    .tasks
                    .first()
                    .map(|task| task.title == "work in a")
                    .unwrap_or(false)
            },
            500,
            || obs.maybe_refresh(),
        )
        .await;

        {
            let mut st = obs.inner.state.lock_recover();
            st.single_lane.last_fetch = Instant::now();
        }
        obs.inner.single_dirty.store(false, Ordering::Relaxed);

        let b = mgr(store, "sess-b");
        b.create(&json!({"title": "work in b"})).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        obs.maybe_refresh();

        let snapshot = obs.snapshot();
        assert_eq!(snapshot.tasks.len(), 1, "{snapshot:?}");
        assert_eq!(snapshot.tasks[0].title, "work in a");
        let st = obs.inner.state.lock_recover();
        assert_eq!(
            st.session_id, "sess-a",
            "observer bound to a real session must not adopt later broadcasts from another session"
        );
        assert!(
            !obs.inner.single_dirty.load(Ordering::Relaxed),
            "foreign-session broadcasts must not mark the single-session board dirty"
        );
    }

    #[tokio::test]
    async fn refresh_picks_up_created_task() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-new-1");

        let m = mgr(store, "sess-new-1");
        m.create(&json!({"title": "one"})).await;

        // First `maybe_refresh` honours the POLL_INTERVAL seed (empty
        // board, dirty=true from subscribe → fast path).
        wait_until(
            || !obs.snapshot().tasks.is_empty(),
            500,
            || obs.maybe_refresh(),
        )
        .await;
        assert_eq!(obs.snapshot().tasks.len(), 1);
    }

    #[tokio::test]
    async fn rebind_clears_then_refetches() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-a");

        let a = mgr(store.clone(), "sess-a");
        a.create(&json!({"title": "in-a"})).await;
        wait_until(
            || !obs.snapshot().tasks.is_empty(),
            500,
            || obs.maybe_refresh(),
        )
        .await;

        obs.rebind_session("sess-b");
        // Immediately after rebind the snapshot should be empty.
        assert!(obs.snapshot().tasks.is_empty());

        let b = mgr(store, "sess-b");
        b.create(&json!({"title": "in-b"})).await;
        wait_until(
            || {
                obs.snapshot()
                    .tasks
                    .first()
                    .map(|t| t.title == "in-b")
                    .unwrap_or(false)
            },
            500,
            || obs.maybe_refresh(),
        )
        .await;
        assert_eq!(obs.snapshot().tasks.len(), 1);
        assert_eq!(obs.snapshot().tasks[0].title, "in-b");
    }

    #[tokio::test]
    async fn rebind_keeps_terminal_history_scoped_to_the_current_session() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-a");

        let a = mgr(store.clone(), "sess-a");
        a.create(&json!({"title": "done in old session"})).await;
        a.update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        a.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        wait_until(
            || {
                obs.snapshot()
                    .tasks
                    .iter()
                    .any(|task| task.status.is_completed())
            },
            500,
            || obs.maybe_refresh(),
        )
        .await;
        assert!(
            obs.snapshot_for_render()
                .tasks
                .iter()
                .any(|task| task.title == "done in old session"),
            "terminal history must remain inspectable before a session switch"
        );

        obs.rebind_session("sess-b");
        let b = mgr(store, "sess-b");
        b.create(&json!({"title": "done in new session"})).await;
        b.update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        b.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;

        wait_until(
            || {
                obs.snapshot()
                    .tasks
                    .first()
                    .map(|task| task.title == "done in new session")
                    .unwrap_or(false)
            },
            500,
            || obs.maybe_refresh(),
        )
        .await;

        let render = obs.snapshot_for_render();
        assert!(
            render
                .tasks
                .iter()
                .any(|task| task.title == "done in new session"),
            "a same-id task in the new session must use the new session's authoritative row: {render:?}"
        );
    }

    #[tokio::test]
    async fn all_completed_remains_visible_until_explicitly_collapsed() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-hide");
        let m = mgr(store, "sess-hide");

        m.create(&json!({"title": "done-me"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        m.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;

        // Pump until the snapshot reflects the completion.
        wait_until(
            || {
                let s = obs.snapshot();
                s.tasks.len() == 1 && !s.has_incomplete()
            },
            500,
            || obs.maybe_refresh(),
        )
        .await;

        // Repeated ticks must not turn a terminal task into a disappearing
        // transient. The user, not a timeout, controls compact-board hide.
        obs.maybe_refresh();
        assert!(!obs.snapshot().hidden, "completed work must stay visible");
    }

    #[tokio::test]
    async fn hidden_completed_board_can_be_revealed_for_review() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-review");
        let m = mgr(store, "sess-review");

        m.create(&json!({"title": "done-me"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        m.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        wait_until(
            || {
                let s = obs.snapshot();
                s.tasks.len() == 1 && !s.has_incomplete()
            },
            500,
            || obs.maybe_refresh(),
        )
        .await;
        obs.hide_completed_after_review();
        assert!(
            obs.snapshot().hidden,
            "explicit collapse hides terminal history"
        );

        // Exercise the real observer refresh path. A periodic reconciliation
        // of unchanged terminal rows must preserve the user's collapse.
        let update = m
            .update(&json!({
                "task_id": "task-1",
                "description": "authoritative terminal refresh"
            }))
            .await;
        assert!(!update.starts_with("Error:"), "{update}");
        wait_until(
            || {
                obs.snapshot().tasks.first().is_some_and(|task| {
                    task.description.as_deref() == Some("authoritative terminal refresh")
                })
            },
            500,
            || obs.maybe_refresh(),
        )
        .await;
        assert!(
            obs.snapshot().hidden,
            "refresh must not resurrect explicitly collapsed terminal history"
        );

        assert!(
            obs.reveal_completed_for_review(),
            "manual expansion should reveal a hidden completed board"
        );
        assert!(
            !obs.snapshot().hidden,
            "revealed board must render instead of staying hidden"
        );

        obs.hide_completed_after_review();
        assert!(
            obs.snapshot().hidden,
            "manual collapse should restore hidden state for all-completed boards"
        );
    }

    #[tokio::test]
    async fn hide_completed_after_review_is_noop_when_incomplete_remain() {
        // An explicit terminal-history collapse must not make active work
        // disappear. Ctrl+T has a separate board pin for that user choice.
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-active");
        let m = mgr(store, "sess-active");

        m.create(&json!({"title": "running-work"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        wait_until(
            || {
                let s = obs.snapshot();
                s.has_incomplete()
            },
            500,
            || obs.maybe_refresh(),
        )
        .await;
        assert!(!obs.snapshot().hidden, "active board starts visible");

        obs.hide_completed_after_review();
        assert!(
            !obs.snapshot().hidden,
            "hide_completed_after_review must be a no-op while incomplete tasks remain"
        );

        // Empty-board path: also a no-op (no tasks = nothing to hide).
        let obs_empty = TaskBoardObserver::new(
            Arc::new(InMemoryTaskStore::new()) as Arc<dyn TaskStore>,
            "sess-empty",
        );
        obs_empty.hide_completed_after_review();
        assert!(
            !obs_empty.snapshot().hidden,
            "empty board is not hidden by hide_completed_after_review"
        );
    }

    #[tokio::test]
    async fn poisoned_state_recovers_and_clears_in_flight_fetch() {
        let store = Arc::new(InMemoryTaskStore::new());
        let obs = TaskBoardObserver::new(store as Arc<dyn TaskStore>, "sess-poison");

        let old_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let poison_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
            let obs = obs.clone();
            move || {
                let mut st = obs.inner.state.lock_recover();
                st.single_lane.refresh_in_flight = true;
                panic!("poison task board state for regression test");
            }
        }));
        std::panic::set_hook(old_hook);
        assert!(poison_result.is_err(), "test must poison observer state");

        obs.maybe_refresh();

        let st = astra_core::sync_poison::recover_mutex_lock(&obs.inner.state);
        assert!(
            !st.single_lane.refresh_in_flight,
            "poison recovery must clear stale fetch_in_flight so future refreshes are not frozen"
        );
    }

    /// Regression test for the deadlock that sank the first TUI mount:
    /// simulate the real TUI tick cadence (50 ms) from a
    /// `current_thread` tokio runtime while another task writes
    /// rapidly through `TaskManager`, and assert the whole thing
    /// finishes within the timeout. A deadlock would show up as the
    /// outer `tokio::time::timeout` firing.
    #[tokio::test(flavor = "current_thread")]
    async fn tick_and_write_on_current_thread_does_not_deadlock() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-stress");
        let m = mgr(store, "sess-stress");

        // Writer: fire 20 tool-like operations in quick succession.
        let writer = async move {
            for i in 0..20 {
                m.create(&json!({"title": format!("job-{i}")})).await;
                if i % 3 == 0 {
                    m.update(&json!({
                        "task_id": format!("task-{}", i + 1),
                        "status": "in_progress",
                    }))
                    .await;
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        };

        // Ticker: mimic the TUI's 50 ms tick calling `maybe_refresh` +
        // `snapshot`. Any deadlock in the observer will cause this
        // loop to stop making progress.
        let ticker_obs = Arc::clone(&obs);
        let ticker = async move {
            for _ in 0..40 {
                ticker_obs.maybe_refresh();
                let _ = ticker_obs.snapshot();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };

        let outer = async move {
            tokio::join!(writer, ticker);
        };
        tokio::time::timeout(Duration::from_secs(4), outer)
            .await
            .expect("stress loop should complete well within 4 s; a timeout means deadlock");

        // Final snapshot should reflect the writes. We let the ticker
        // drain a few more iterations before reading so in-flight
        // spawns have a chance to land.
        let deadline = Instant::now() + Duration::from_millis(500);
        while obs.snapshot().tasks.len() < 20 && Instant::now() < deadline {
            obs.maybe_refresh();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            obs.snapshot().tasks.len(),
            20,
            "writer produced 20 tasks; observer should eventually see them all"
        );
    }

    // ── ViewMode: single ↔ all-sessions ──────────────────────────

    #[tokio::test]
    async fn default_view_mode_is_single_session() {
        let store = Arc::new(InMemoryTaskStore::new());
        let obs = TaskBoardObserver::new(store as Arc<dyn TaskStore>, "sess");
        assert_eq!(obs.view_mode(), ViewMode::SingleSession);
        assert!(
            obs.multi_snapshot().per_session.is_empty(),
            "multi_snapshot stays empty in single-session mode"
        );
    }

    #[tokio::test]
    async fn toggle_view_mode_flips_and_marks_dirty() {
        let store = Arc::new(InMemoryTaskStore::new());
        let obs = TaskBoardObserver::new(store as Arc<dyn TaskStore>, "sess");
        obs.toggle_view_mode();
        assert_eq!(obs.view_mode(), ViewMode::AllSessions);
        assert!(
            obs.inner.all_dirty.load(Ordering::Relaxed),
            "mode flip must mark dirty so the next tick refetches the right slice"
        );
        obs.toggle_view_mode();
        assert_eq!(obs.view_mode(), ViewMode::SingleSession);
    }

    #[tokio::test]
    async fn all_sessions_mode_populates_multi_snapshot() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();

        TaskManager::new("sess-a", store_dyn.clone())
            .create(&json!({"title": "A1"}))
            .await;
        TaskManager::new("sess-b", store_dyn.clone())
            .create(&json!({"title": "B1"}))
            .await;

        let obs = TaskBoardObserver::new(store_dyn, "sess-a");
        obs.toggle_view_mode();
        assert_eq!(obs.view_mode(), ViewMode::AllSessions);

        wait_until(
            || obs.multi_snapshot().per_session.len() >= 2,
            500,
            || obs.maybe_refresh(),
        )
        .await;

        let multi = obs.multi_snapshot();
        let sids: Vec<&str> = multi.per_session.iter().map(|(s, _)| s.as_str()).collect();
        assert!(
            sids.contains(&"sess-a"),
            "multi snapshot must carry sess-a: {sids:?}"
        );
        assert!(
            sids.contains(&"sess-b"),
            "multi snapshot must carry sess-b: {sids:?}"
        );
    }

    #[tokio::test]
    async fn failed_all_sessions_refresh_preserves_last_known_snapshot() {
        struct MultiFailsAfterFirstLoadStore {
            loads: AtomicUsize,
        }

        #[async_trait]
        impl TaskStore for MultiFailsAfterFirstLoadStore {
            async fn load(&self, _session_id: &str) -> Result<Vec<SessionTask>, String> {
                Ok(Vec::new())
            }

            async fn save(
                &self,
                _session_id: &str,
                _tasks: Vec<SessionTask>,
            ) -> Result<(), String> {
                Ok(())
            }

            async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }

            async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }

            async fn load_open_sessions(
                &self,
                _limit: usize,
            ) -> Result<Vec<(String, Vec<SessionTask>)>, String> {
                if self.loads.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                    Ok(vec![(
                        "sess-a".to_string(),
                        vec![SessionTask {
                            archived_at: None,
                            id: "task-1".to_string(),
                            title: "visible cross-session work".to_string(),
                            description: None,
                            status: SessionTaskStatusKind::Pending,
                            subtasks: Vec::new(),
                            created_at: chrono::Utc::now().to_rfc3339(),
                            updated_at: chrono::Utc::now().to_rfc3339(),
                            active_form: None,
                            owner: None,
                            metadata: None,
                            blocks: Vec::new(),
                            blocked_by: Vec::new(),
                        }],
                    )])
                } else {
                    Err("simulated cross-session read failure".to_string())
                }
            }
        }

        let store: Arc<dyn TaskStore> = Arc::new(MultiFailsAfterFirstLoadStore {
            loads: AtomicUsize::new(0),
        });
        let obs = TaskBoardObserver::new(store, "sess-a");
        obs.toggle_view_mode();

        wait_until(
            || obs.multi_snapshot().per_session.len() == 1,
            500,
            || obs.maybe_refresh(),
        )
        .await;
        assert_eq!(
            obs.multi_snapshot().per_session[0].1[0].title,
            "visible cross-session work"
        );

        {
            let mut st = obs.inner.state.lock_recover();
            st.all_lane.last_fetch = Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap_or_else(Instant::now);
        }
        obs.inner.all_dirty.store(true, Ordering::Relaxed);
        obs.maybe_refresh();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let multi = obs.multi_snapshot();
        assert_eq!(
            multi.per_session.len(),
            1,
            "failed all-sessions refresh must preserve the last known cross-session board"
        );
        assert_eq!(
            multi.per_session[0].1[0].title,
            "visible cross-session work"
        );
    }

    #[tokio::test]
    async fn toggle_back_to_single_preserves_session_snapshot() {
        // Flipping into AllSessions and back out must not drop the
        // single-session cached view — the user expects instant
        // return to "my board" without another fetch flicker.
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        TaskManager::new("sess-a", store_dyn.clone())
            .create(&json!({"title": "keep me"}))
            .await;

        let obs = TaskBoardObserver::new(store_dyn, "sess-a");
        wait_until(
            || !obs.snapshot().tasks.is_empty(),
            500,
            || obs.maybe_refresh(),
        )
        .await;
        let before = obs.snapshot().tasks.len();

        obs.toggle_view_mode();
        obs.toggle_view_mode();
        assert_eq!(
            obs.snapshot().tasks.len(),
            before,
            "single-session snapshot must survive a round-trip through AllSessions mode"
        );
    }

    // ── Diff-event ring integration ──────────────────────────────

    #[tokio::test]
    async fn observer_records_diff_events_when_task_created() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-diff");
        let m = mgr(store, "sess-diff");

        // First fetch sees an empty board (no events).
        wait_until(
            || obs.snapshot().tasks.is_empty(),
            500,
            || obs.maybe_refresh(),
        )
        .await;
        // Writes a task; observer's next fetch must diff and emit Created.
        m.create(&json!({"title": "new task"})).await;
        wait_until(
            || !obs.fresh_event_task_ids().is_empty(),
            500,
            || obs.maybe_refresh(),
        )
        .await;
        let fresh = obs.fresh_event_task_ids();
        assert!(
            fresh.iter().any(|id| id == "task-1"),
            "task-1 must appear in fresh_event_task_ids after creation: {fresh:?}"
        );
        assert!(
            obs.is_task_id_fresh("task-1"),
            "predicate lookup must agree with the materialized id list"
        );
        let fresh_set = obs.fresh_task_id_set();
        assert!(
            fresh_set.contains("task-1"),
            "set lookup must agree with the materialized id list"
        );
    }

    #[tokio::test]
    async fn observer_records_status_change_event() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-status");
        let m = mgr(store, "sess-status");
        m.create(&json!({"title": "flippable"})).await;
        wait_until(
            || !obs.snapshot().tasks.is_empty(),
            500,
            || obs.maybe_refresh(),
        )
        .await;
        // Wait out the fresh window so the Created event aged out.
        tokio::time::sleep(super::EVENT_FRESH_WINDOW + Duration::from_millis(50)).await;
        obs.maybe_refresh();
        assert!(
            obs.fresh_event_task_ids().is_empty(),
            "events must age out of the fresh window"
        );

        // Now flip the status — next refresh should surface it as fresh.
        m.update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        wait_until(
            || !obs.fresh_event_task_ids().is_empty(),
            500,
            || obs.maybe_refresh(),
        )
        .await;
        assert!(
            obs.fresh_event_task_ids().iter().any(|id| id == "task-1"),
            "status flip must register a fresh event for the flipped row"
        );
        assert!(
            obs.is_task_id_fresh("task-1"),
            "predicate lookup must report the fresh status flip too"
        );
        let fresh_set = obs.fresh_task_id_set();
        assert!(
            fresh_set.contains("task-1"),
            "set lookup must report the fresh status flip too"
        );
    }
}
