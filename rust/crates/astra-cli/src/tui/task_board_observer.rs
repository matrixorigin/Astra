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
//! - A store broadcast (`TaskStore::subscribe`) is drained inside the
//!   same refresh call: any queued events set a `dirty` flag so the
//!   next tick fetches immediately instead of waiting out the 5 s poll.
//! - Hide-timer logic (5 s "all completed → collapse") is pure in-loop
//!   bookkeeping: we record a `hide_at: Instant` when the last tick
//!   saw all-completed non-empty state and, on every subsequent tick,
//!   flip `hidden = true` once `hide_at` is past.

use astra_tools::task_mgmt::{SessionTask, TaskStore};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Fallback poll cadence when nothing has broadcast a change. Matches
/// the "only poll while incomplete" gate in the reference TUI — if a
/// board has no in-flight work, ticks skip the fetch entirely.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Faster poll right after a broadcast/rebind so the user sees writes
/// land within UI-perceptible latency. Gated by the `dirty` atomic
/// so a quiet board never re-reads the store; only fires when there
/// IS a known change to chase. Phase 4 dropped this from 250ms to
/// 50ms after user feedback that the task board appeared blank
/// during long turns — the in-turn `do_draw` path now also pumps
/// `maybe_refresh`, so per-frame latency at 60fps stays under 17ms
/// of the cap.
const FAST_POLL: Duration = Duration::from_millis(50);
/// How long the board stays painted after the last incomplete task
/// closes out before `hidden` flips.
const HIDE_DELAY: Duration = Duration::from_secs(5);

/// Observable snapshot of the task board. Cheap to clone (moves the
/// owned vec; callers get a full copy).
#[derive(Clone, Debug, Default)]
pub(crate) struct TaskBoardSnapshot {
    pub tasks: Vec<SessionTask>,
    pub hidden: bool,
}

/// Per-session pair used by the multi-session view. Mirrors what
/// `TaskStore::load_all_sessions` returns.
#[derive(Clone, Debug, Default)]
pub(crate) struct MultiSessionSnapshot {
    pub per_session: Vec<(String, Vec<SessionTask>)>,
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

impl TaskBoardSnapshot {
    pub fn has_incomplete(&self) -> bool {
        self.tasks.iter().any(|t| t.status.is_active())
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

pub(crate) struct TaskBoardObserver {
    inner: Arc<ObserverInner>,
}

struct ObserverInner {
    store: Arc<dyn TaskStore>,
    state: Mutex<ObserverState>,
    /// Flipped to `true` by `TaskStore::subscribe` drainers and by
    /// `rebind_session`. The next `maybe_refresh` call consumes it and
    /// fetches immediately, then clears it.
    dirty: AtomicBool,
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
/// How long a `completed` task lingers on the board after the
/// transition. Beyond this, the renderer drops the row from the
/// expanded panel — counts in the header still reflect it. Picked
/// to be long enough that the user can glance and verify the
/// completion landed (claudecode uses ~30s for the same purpose),
/// short enough that a long agentic run doesn't accumulate dozens of
/// completed rows above the spinner.
pub(crate) const COMPLETED_TASK_TTL: Duration = Duration::from_secs(30);

struct ObserverState {
    session_id: String,
    snapshot: TaskBoardSnapshot,
    /// Populated only while `view_mode == AllSessions`. Fetched via
    /// `TaskStore::load_all_sessions` on the usual tick cadence.
    /// Kept alongside `snapshot` (not replacing it) so a mode flip
    /// back to SingleSession restores the per-session view instantly
    /// without another round-trip.
    multi_snapshot: MultiSessionSnapshot,
    /// Which slice of the store the observer is refreshing. See
    /// [`ViewMode`]. Default SingleSession.
    view_mode: ViewMode,
    /// When we saw all-completed non-empty, the Instant at which
    /// `hidden` should flip. `None` means no pending hide timer.
    hide_at: Option<Instant>,
    /// User explicitly revealed an all-completed board for review. While set,
    /// automatic all-complete hiding stays disabled until the user collapses
    /// the board or new active/empty state arrives.
    manual_review_visible: bool,
    /// When the last `store.load()` returned. Used to gate polling.
    last_fetch: Instant,
    /// Whether a fetch is currently in flight. Prevents ticks from
    /// firing concurrent spawns.
    fetch_in_flight: bool,
    /// Ring buffer of diff events from the last few refreshes. The
    /// renderer reads `recent_events()` to flash a highlight on
    /// newly-created / newly-completed rows. Trimmed to
    /// `EVENT_RING_CAP` entries.
    event_ring: Vec<TimedTaskBoardEvent>,
    /// When each task transitioned to `completed`. Renderer treats
    /// entries older than [`COMPLETED_TASK_TTL`] as eligible for the
    /// "fade out" rule — they stay in counts but stop occupying rows.
    /// Keys for tasks that re-open (in_progress again) are dropped so
    /// the timer restarts on the next completion.
    completed_at: std::collections::HashMap<String, Instant>,
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
    /// If the store supports `subscribe()`, a tiny helper task drains
    /// events into the `dirty` flag so ticks fetch on-demand rather
    /// than waiting for the poll window.
    pub fn new(store: Arc<dyn TaskStore>, session_id: impl Into<String>) -> Arc<Self> {
        let sid: String = session_id.into();
        let inner = Arc::new(ObserverInner {
            store: store.clone(),
            state: Mutex::new(ObserverState {
                session_id: sid,
                snapshot: TaskBoardSnapshot::default(),
                multi_snapshot: MultiSessionSnapshot::default(),
                view_mode: ViewMode::SingleSession,
                hide_at: None,
                manual_review_visible: false,
                // `Instant::now() - POLL_INTERVAL - 1s` would force the
                // first tick to fetch; simpler to seed with `now()` so
                // the fetch happens a tick later, after the main loop
                // has settled.
                last_fetch: Instant::now()
                    .checked_sub(POLL_INTERVAL)
                    .unwrap_or_else(Instant::now),
                fetch_in_flight: false,
                event_ring: Vec::new(),
                completed_at: std::collections::HashMap::new(),
            }),
            dirty: AtomicBool::new(true),
        });
        let observer = Arc::new(Self {
            inner: inner.clone(),
        });

        // Drain the store's broadcast into the dirty flag. This is a
        // lightweight loop: just `recv().await` + atomic store. No
        // locking. On store drop (dyn TaskStore is still alive via
        // `inner.store` as long as the observer lives) the sender
        // closes and the loop exits.
        if let Some(mut rx) = store.subscribe() {
            let inner2 = inner.clone();
            tokio::spawn(async move {
                use tokio::sync::broadcast::error::RecvError;
                loop {
                    match rx.recv().await {
                        Ok(changed_sid) => {
                            // Self-heal during the first turn of a brand-new
                            // session: the observer is constructed with an
                            // empty `session_id` (TUI starts before the
                            // server allocates one), so without this branch
                            // every broadcast during the first turn is
                            // dropped by the strict `==` filter and the
                            // task board never opens until the turn ends.
                            // When `current` is empty, adopt whatever sid
                            // arrives — `route_task_action` only ever
                            // broadcasts for the executor's active session.
                            let mut adopted = false;
                            if let Ok(mut st) = inner2.state.lock() {
                                if st.session_id.is_empty() && !changed_sid.is_empty() {
                                    st.session_id = changed_sid.clone();
                                    st.snapshot = TaskBoardSnapshot::default();
                                    st.hide_at = None;
                                    st.manual_review_visible = false;
                                    adopted = true;
                                }
                            }
                            let current = inner2
                                .state
                                .lock()
                                .map(|s| s.session_id.clone())
                                .unwrap_or_default();
                            if adopted || changed_sid == current {
                                inner2.dirty.store(true, Ordering::Relaxed);
                            }
                        }
                        // Lagged means we dropped `n` events under burst
                        // pressure. We don't need the exact events —
                        // flip dirty unconditionally so `maybe_refresh`
                        // re-reads the store, then keep going. Returning
                        // here would permanently kill the fast-path and
                        // leave the UI relying on the 5s poll.
                        Err(RecvError::Lagged(_)) => {
                            inner2.dirty.store(true, Ordering::Relaxed);
                            continue;
                        }
                        // Closed = sender dropped; observer is alive
                        // longer than the store (shouldn't happen in
                        // production), nothing left to wait for.
                        Err(RecvError::Closed) => return,
                    }
                }
            });
        }

        observer
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

    /// Snapshot with completed tasks past [`COMPLETED_TASK_TTL`]
    /// removed. Header counts (rendered from [`Self::counts`]) still
    /// reflect the full set so the user can audit "12 done" without
    /// the rows monopolising scrollback. Use this in the renderer's
    /// row loop; use [`Self::snapshot`] when you need the truth.
    pub fn snapshot_for_render(&self) -> TaskBoardSnapshot {
        let (st, _) = lock_state(&self.inner, "snapshot_for_render");
        let now = Instant::now();
        let mut snap = st.snapshot.clone();
        snap.tasks.retain(|task| {
            if !task.status.is_completed() {
                return true;
            }
            match st.completed_at.get(&task.id) {
                Some(at) => now.saturating_duration_since(*at) < COMPLETED_TASK_TTL,
                None => true, // unseen completion → safer to keep until next tick
            }
        });
        snap
    }

    /// Test-only: pretend `task_id`'s completion happened `back_by` ago
    /// so the TTL filter treats it as expired without sleeping in tests.
    /// No-op when the task isn't in the completed map.
    #[doc(hidden)]
    pub fn testing_force_completed_at_past(&self, task_id: &str, back_by: Duration) {
        let (mut st, _) = lock_state(&self.inner, "testing_force_completed_at_past");
        let stale = Instant::now()
            .checked_sub(back_by)
            .unwrap_or_else(Instant::now);
        st.completed_at.insert(task_id.to_string(), stale);
    }

    /// Cheap summary counts for the footer chip: `(open, total,
    /// hidden)`. Reads under the sync mutex without cloning the task
    /// vec — at ~60 draws/sec with 100 tasks the clone savings are
    /// measurable.
    pub fn counts(&self) -> (usize, usize, bool) {
        let (st, _) = lock_state(&self.inner, "counts");
        let total = st.snapshot.tasks.len();
        let open = st
            .snapshot
            .tasks
            .iter()
            .filter(|t| t.status.is_active())
            .count();
        (open, total, st.snapshot.hidden)
    }

    /// IDs of tasks that changed within [`EVENT_FRESH_WINDOW`] of
    /// the call. The renderer uses this to flash a highlight on
    /// newly-created or newly-completed rows — the "something just
    /// happened" cue claude-code gets from its task-state reducer.
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

    /// Current multi-session snapshot. Empty while the observer is
    /// in SingleSession mode. The board renderer reads this when
    /// `view_mode() == AllSessions` and flattens via
    /// `task_board_multi::flatten_active`.
    pub fn multi_snapshot(&self) -> MultiSessionSnapshot {
        let (st, _) = lock_state(&self.inner, "multi_snapshot");
        st.multi_snapshot.clone()
    }

    /// Currently-selected view mode.
    pub fn view_mode(&self) -> ViewMode {
        let (st, _) = lock_state(&self.inner, "view_mode");
        st.view_mode
    }

    /// Toggle between single-session and cross-session views. Marks
    /// the observer dirty so the next tick immediately fetches the
    /// correct slice; clears any pending auto-hide so the new mode
    /// starts from a known baseline.
    pub fn toggle_view_mode(&self) {
        let (mut st, _) = lock_state(&self.inner, "toggle_view_mode");
        st.view_mode = match st.view_mode {
            ViewMode::SingleSession => ViewMode::AllSessions,
            ViewMode::AllSessions => ViewMode::SingleSession,
        };
        st.hide_at = None;
        st.manual_review_visible = false;
        self.inner.dirty.store(true, Ordering::Relaxed);
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
        st.snapshot = TaskBoardSnapshot::default();
        st.hide_at = None;
        st.manual_review_visible = false;
        self.inner.dirty.store(true, Ordering::Relaxed);
    }

    /// Reveal an all-completed board that was hidden by the idle timer so a
    /// manual Ctrl+T expansion can show the completed work for review.
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
            st.hide_at = None;
            st.manual_review_visible = true;
            return true;
        }
        false
    }

    /// Return a manually reviewed all-completed board to hidden state when the
    /// user collapses it, preserving the uncluttered idle UI.
    pub fn hide_completed_after_review(&self) {
        let (mut st, _) = lock_state(&self.inner, "hide_completed_after_review");
        if !st.snapshot.tasks.is_empty() && !st.snapshot.has_incomplete() {
            st.snapshot.hidden = true;
            st.hide_at = None;
            st.manual_review_visible = false;
        }
    }

    /// Called from the TUI tick. Does bookkeeping under the sync
    /// mutex, then spawns a one-shot fetch if it's time. Never blocks
    /// the caller.
    ///
    /// Returns `true` if the cached snapshot's `hidden` flag or task
    /// vec changed since the last tick — the caller can use this to
    /// schedule a paint rather than paint on every single tick.
    pub fn maybe_refresh(&self) {
        let now = Instant::now();
        enum Due {
            Skip,
            Single(String),
            All,
        }
        let due = {
            let (mut st, poisoned) = lock_state(&self.inner, "maybe_refresh");
            if poisoned && st.fetch_in_flight {
                st.fetch_in_flight = false;
                st.last_fetch = now;
            }
            // Advance the hide timer if it's armed and ripe.
            if let Some(hide_at) = st.hide_at {
                if now >= hide_at {
                    st.snapshot.hidden = true;
                    st.hide_at = None;
                }
            }
            if st.fetch_in_flight {
                Due::Skip
            } else {
                // Peek at `dirty` without consuming it; we only swap to
                // false once we're committed to firing the fetch, so a
                // dirty bit that comes in while we're gated by the poll
                // window survives to the next tick.
                let dirty = self.inner.dirty.load(Ordering::Relaxed);
                let elapsed = now.saturating_duration_since(st.last_fetch);
                let window = if dirty { FAST_POLL } else { POLL_INTERVAL };
                if elapsed < window {
                    Due::Skip
                } else {
                    // AllSessions bypasses the "skip when no incomplete
                    // work" gate — cross-session view may have new
                    // activity in sessions we're not tracking locally.
                    let do_fetch = match st.view_mode {
                        ViewMode::AllSessions => true,
                        ViewMode::SingleSession => {
                            let has_incomplete = st.snapshot.has_incomplete();
                            let never_fetched_or_empty = st.snapshot.tasks.is_empty();
                            has_incomplete || never_fetched_or_empty || dirty
                        }
                    };
                    if !do_fetch {
                        Due::Skip
                    } else {
                        self.inner.dirty.store(false, Ordering::Relaxed);
                        st.fetch_in_flight = true;
                        match st.view_mode {
                            ViewMode::AllSessions => Due::All,
                            ViewMode::SingleSession => Due::Single(st.session_id.clone()),
                        }
                    }
                }
            }
        };

        match due {
            Due::Skip => {}
            Due::All => {
                let inner = self.inner.clone();
                tokio::spawn(async move {
                    let per_session = inner.store.load_all_sessions().await.unwrap_or_default();
                    let (mut st, _) = lock_state(&inner, "multi_fetch_complete");
                    st.fetch_in_flight = false;
                    st.last_fetch = Instant::now();
                    // Bail if the user toggled back to single mode
                    // while our query was in flight.
                    if st.view_mode != ViewMode::AllSessions {
                        return;
                    }
                    st.multi_snapshot = MultiSessionSnapshot { per_session };
                });
            }
            Due::Single(sid) if sid.is_empty() => {
                let (mut st, _) = lock_state(&self.inner, "maybe_refresh_empty_session");
                st.fetch_in_flight = false;
                st.last_fetch = now;
            }
            Due::Single(sid) => {
                let inner = self.inner.clone();
                tokio::spawn(async move {
                    let tasks = inner.store.load(&sid).await.unwrap_or_default();
                    let (mut st, _) = lock_state(&inner, "fetch_complete");
                    st.fetch_in_flight = false;
                    st.last_fetch = Instant::now();
                    // Bail if the observer was rebound mid-fetch — the
                    // tasks we got belong to the old session id.
                    if st.session_id != sid {
                        return;
                    }
                    if !same_board(&tasks, &st.snapshot.tasks) {
                        // Diff BEFORE replacing the snapshot — events
                        // carry id+title from the pair (prev, new)
                        // so the renderer can flash the affected row
                        // for EVENT_FRESH_WINDOW. Same diff feeds the
                        // per-task TTL map so a `completed` transition
                        // arms the 30s linger window; a re-open clears
                        // the timer so the next completion restarts it.
                        let events = super::task_board_events::diff(&st.snapshot.tasks, &tasks);
                        let at = Instant::now();
                        for event in &events {
                            if let super::task_board_events::TaskBoardEvent::StatusChanged {
                                task_id,
                                to,
                                ..
                            } = event
                            {
                                if to.is_completed() {
                                    st.completed_at.insert(task_id.clone(), at);
                                } else {
                                    st.completed_at.remove(task_id);
                                }
                            }
                        }
                        // Garbage-collect entries for tasks that
                        // disappeared (Removed event) so the map
                        // doesn't grow without bound across long
                        // sessions.
                        for event in &events {
                            if let super::task_board_events::TaskBoardEvent::Removed {
                                task_id,
                                ..
                            } = event
                            {
                                st.completed_at.remove(task_id);
                            }
                        }
                        for event in events {
                            st.event_ring.push(TimedTaskBoardEvent { event, at });
                        }
                        if st.event_ring.len() > EVENT_RING_CAP {
                            let excess = st.event_ring.len() - EVENT_RING_CAP;
                            st.event_ring.drain(0..excess);
                        }
                        st.snapshot.tasks = tasks;
                        // Backfill the TTL map for tasks that arrived
                        // already-completed (e.g. resume into a session
                        // whose work finished long ago). Without this,
                        // those rows would never expire because no
                        // StatusChanged transition exists. We treat
                        // them as "freshly completed" relative to this
                        // observer's lifetime — fine, they age out 30s
                        // later just like any other.
                        let backfill_ids: Vec<String> = st
                            .snapshot
                            .tasks
                            .iter()
                            .filter(|t| t.status.is_completed())
                            .filter(|t| !st.completed_at.contains_key(&t.id))
                            .map(|t| t.id.clone())
                            .collect();
                        for id in backfill_ids {
                            st.completed_at.insert(id, at);
                        }
                    }
                    let has_incomplete = st.snapshot.has_incomplete();
                    let empty = st.snapshot.tasks.is_empty();
                    if has_incomplete || empty {
                        // Fresh work or cleared — drop any pending hide.
                        st.hide_at = None;
                        st.manual_review_visible = false;
                        st.snapshot.hidden = empty;
                    } else {
                        // All complete. Arm the hide timer if not already.
                        if !st.manual_review_visible && st.hide_at.is_none() {
                            st.hide_at = Some(Instant::now() + HIDE_DELAY);
                        }
                    }
                });
            }
        }
    }
}

/// Cheap equality check for Tier 1 boards (dozens of rows): compare
/// `(id, status, title, updated_at)` tuples. Avoids forcing PartialEq
/// on the public SessionTask struct in astra-tools.
fn same_board(a: &[SessionTask], b: &[SessionTask]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(x, y)| {
        x.id == y.id && x.status == y.status && x.title == y.title && x.updated_at == y.updated_at
    })
}

// ───────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock_recovery::LockRecovery;
    use astra_tools::task_mgmt::{InMemoryTaskStore, TaskManager};
    use serde_json::json;

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

    /// REGRESSION (Phase 4 / problem 1): the in-turn `do_draw` path
    /// must observe a task.create within UI-perceptible latency.
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
            st.last_fetch = Instant::now()
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

    /// REGRESSION: completed tasks used to linger on the board until
    /// the user opened a new session. With per-task TTL, a task that
    /// has been `completed` for longer than `COMPLETED_TASK_TTL` drops
    /// out of `snapshot_for_render`'s task list — header counts stay
    /// honest via `snapshot()`.
    #[tokio::test]
    async fn completed_tasks_age_out_of_render_snapshot_after_ttl() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-ttl");
        let m = mgr(store, "sess-ttl");

        m.create(&json!({"title": "shipping work"})).await;
        m.update(&json!({"task_id": "task-1", "status": "completed"}))
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

        // Immediately after completion the row stays — user wants to
        // see the ✔ land.
        assert_eq!(
            obs.snapshot_for_render().tasks.len(),
            1,
            "freshly completed task must still render"
        );

        // Force the TTL clock backwards so the row is older than the
        // window without a 30s sleep in unit tests.
        {
            let mut st = obs.inner.state.lock_recover();
            st.completed_at.insert(
                "task-1".to_string(),
                Instant::now()
                    .checked_sub(COMPLETED_TASK_TTL + Duration::from_secs(1))
                    .unwrap_or_else(Instant::now),
            );
        }

        let render = obs.snapshot_for_render();
        assert!(
            render.tasks.is_empty(),
            "completed task aged past TTL must drop from render snapshot: {:?}",
            render.tasks
        );
        // Truth snapshot still reports the row so /task list and counts
        // remain accurate.
        let truth = obs.snapshot();
        assert_eq!(truth.tasks.len(), 1, "snapshot() must keep aged rows");
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
    async fn all_completed_arms_hide_timer() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-hide");
        let m = mgr(store, "sess-hide");

        m.create(&json!({"title": "done-me"})).await;
        m.update(&json!({"task_id": "task-1", "status": "completed"}))
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

        // `hidden` is still false — the 5 s timer hasn't elapsed. We
        // don't want to sleep 5 s in a unit test; instead, reach into
        // state and force `hide_at` into the past, then one more tick
        // flips `hidden`.
        {
            let mut st = obs.inner.state.lock_recover();
            st.hide_at = Some(
                Instant::now()
                    .checked_sub(Duration::from_secs(1))
                    .unwrap_or_else(Instant::now),
            );
        }
        obs.maybe_refresh();
        assert!(obs.snapshot().hidden, "hide timer should have elapsed");
    }

    #[tokio::test]
    async fn hidden_completed_board_can_be_revealed_for_review() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-review");
        let m = mgr(store, "sess-review");

        m.create(&json!({"title": "done-me"})).await;
        m.update(&json!({"task_id": "task-1", "status": "completed"}))
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
        {
            let mut st = obs.inner.state.lock_recover();
            st.snapshot.hidden = true;
        }

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
    async fn manual_reveal_does_not_rearm_auto_hide_on_refresh() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-pinned-review");
        let m = mgr(store, "sess-pinned-review");

        m.create(&json!({"title": "done-me"})).await;
        m.update(&json!({"task_id": "task-1", "status": "completed"}))
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
        {
            let mut st = obs.inner.state.lock_recover();
            st.snapshot.hidden = true;
            st.hide_at = None;
            st.last_fetch = Instant::now()
                .checked_sub(POLL_INTERVAL + Duration::from_secs(1))
                .unwrap_or_else(Instant::now);
        }
        assert!(obs.reveal_completed_for_review());

        obs.inner.dirty.store(true, Ordering::Relaxed);
        wait_until(
            || {
                obs.inner
                    .state
                    .lock()
                    .map(|st| {
                        !st.fetch_in_flight && st.last_fetch.elapsed() < Duration::from_secs(1)
                    })
                    .unwrap_or(false)
            },
            500,
            || obs.maybe_refresh(),
        )
        .await;

        let st = obs.inner.state.lock_recover();
        assert!(
            st.hide_at.is_none(),
            "manual reveal should pin completed board open until the user collapses it"
        );
        assert!(!st.snapshot.hidden, "manual reveal should remain visible");
    }

    #[tokio::test]
    async fn manual_reveal_during_hide_grace_cancels_pending_auto_hide() {
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-grace-review");
        let m = mgr(store, "sess-grace-review");

        m.create(&json!({"title": "done-me"})).await;
        m.update(&json!({"task_id": "task-1", "status": "completed"}))
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
        {
            let mut st = obs.inner.state.lock_recover();
            st.snapshot.hidden = false;
            st.hide_at = Some(Instant::now() + HIDE_DELAY);
        }

        assert!(
            obs.reveal_completed_for_review(),
            "manual expansion during hide grace should still pin the board"
        );

        let st = obs.inner.state.lock_recover();
        assert!(
            st.hide_at.is_none(),
            "pending auto-hide should be cancelled"
        );
        assert!(!st.snapshot.hidden, "board should remain visible");
    }

    #[tokio::test]
    async fn hide_completed_after_review_is_noop_when_incomplete_remain() {
        // Inverse of the above: if the board still has incomplete
        // tasks, manual collapse must NOT mark it hidden — only
        // all-completed boards participate in the auto-hide grace
        // window. Without this guard, Ctrl+T collapse on an active
        // board would make it render blank on the next tick.
        let store = Arc::new(InMemoryTaskStore::new());
        let store_dyn: Arc<dyn TaskStore> = store.clone();
        let obs = TaskBoardObserver::new(store_dyn, "sess-active");
        let m = mgr(store, "sess-active");

        m.create(&json!({"title": "running-work"})).await;
        m.update(&json!({"task_id": "task-1", "status": "in_progress"}))
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
                st.fetch_in_flight = true;
                panic!("poison task board state for regression test");
            }
        }));
        std::panic::set_hook(old_hook);
        assert!(poison_result.is_err(), "test must poison observer state");

        obs.maybe_refresh();

        let st = astra_core::sync_poison::recover_mutex_lock(&obs.inner.state);
        assert!(
            !st.fetch_in_flight,
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
            obs.inner.dirty.load(Ordering::Relaxed),
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
        m.update(&json!({"task_id": "task-1", "status": "in_progress"}))
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
