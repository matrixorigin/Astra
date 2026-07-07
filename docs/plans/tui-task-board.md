# TUI task board — design plan

**Status**: Design draft after reading the reference TUI's `TaskListV2.tsx` +
`useTasksV2.ts` (2026-05-10). No code yet.

## 1. What the reference TUI actually does

Three moving parts:

### 1.1 Storage (reference: `utils/tasks.ts`)

- Tasks are JSON files in `<sessions dir>/<taskListId>/<id>.json`.
- `taskListId` = session id by default, or team name for leaders.
- A `.highwatermark` file ensures ids never reuse after reset.
- Writers call `notifyTasksUpdated()` (an in-process signal) **after**
  writing, so same-process readers refresh immediately. `fs.watch`
  handles cross-process cases.

### 1.2 Observer store (reference: `hooks/useTasksV2.ts`)

`TasksV2Store` is a **single** process-wide React-external store:

- Holds the current `Task[]` snapshot + a `#hidden` flag.
- Uses `fs.watch` on the tasks dir for cross-process changes.
- Uses `onTasksUpdated()` (in-process signal) for same-process changes.
- Uses a 5-second fallback poll **only when incomplete tasks exist** —
  watcher + signal cover steady state; poll is a safety net.
- Debounces fetches by 50ms.
- Hides the list 5 seconds after all tasks are completed, then resets
  the list (clear files + highwatermark).
- Reference-stable snapshots for `useSyncExternalStore` — UI doesn't
  re-render unless the vec changes.

### 1.3 Render (reference: `components/TaskListV2.tsx`)

One responsive widget:

- `maxDisplay = min(10, max(3, rows - 14))` — scales with terminal
  height; hides entirely when `rows <= 10`.
- When `tasks.len > maxDisplay`, prioritize:
  `recent-completed (30s TTL) → in-progress → pending → older-completed`;
  show `… +N in progress, M pending, K completed` suffix.
- Per-task line:
  - Icon by status: `✓` (completed, green), `■` (in_progress, accent
    color), `□` (pending, dim).
  - Subject: bold if in_progress, strikethrough if completed, dim if
    completed or blocked.
  - Owner badge `(@owner)` only when `columns >= 60` AND owner is still
    active (present in the `appState.tasks` "teammate running" set).
  - Activity line `  <tool summary>…` under in-progress tasks whose
    owner is active — rolled up from the last few tool calls.
  - Blocked-by badge `▸ blocked by #3, #5` when any blocker is
    unresolved.
- Responsive truncation: `maxSubjectWidth = max(15, columns - 15 -
  ownerWidth)`.
- Standalone mode renders a summary line: `N tasks (K done, M in
  progress, J open)` above the list.

### 1.4 Mount points + visibility rules (confirmed by reading REPL + Spinner)

`TaskListV2` is **not** always on. It renders **only when**
`expandedView === 'tasks'`. Spinner renders `Next: <task.subject>` on a
single dim line otherwise.

Transitions:

- **User presses Ctrl+T** → toggle expandedView (`none ↔ tasks`, plus
  `teammates` if any teammate is running).
- **TaskCreate / TaskUpdate tool runs** → auto-set
  `expandedView = 'tasks'` (see `tools/TaskCreateTool.ts:115-119` and
  `tools/TaskUpdateTool.ts:141-142`). This is the key "task board
  pops up the moment the model touches it" behaviour.
- **All tasks completed for 5s** → `TasksV2Store` sets `#hidden = true`;
  `useTasksV2WithCollapseEffect` observes the hidden transition and
  resets `expandedView = 'none'` via an effect that runs **once**
  (REPL-level).

Mount sites:

- **Spinner (per-turn footer)**: shows the standalone TaskListV2 only
  while `expandedView === 'tasks' && tasksV2.length > 0 &&
  !showSpinnerTree`. Otherwise falls through to `Next: <task.subject>`
  one-line nudge.
- **REPL standalone area**: `!showSpinner && !toolJSX?.isLocalJSXCommand
  && showExpandedTodos && tasksV2 && tasksV2.length > 0` — full
  `TaskListV2 isStandalone={true}`, lives in the scrollable area
  between messages and PromptInput.
- **PromptInputFooterLeftSide**: no TaskListV2; just a `ModeIndicator`
  chip that reflects `tasksSelected` for the Ctrl+T hint.

Only the REPL top-level component subscribes via
`useTasksV2WithCollapseEffect` so the 5-second hide reset runs once.

## 2. What astra has today

- Storage: `session_todos` MO table (authoritative) + per-host
  `TaskManager` cache.
- Signal for same-process updates: **none**. A write to
  `MatrixOneTaskStore::save()` does not notify anyone. Cross-process
  discovery requires polling.
- TUI rendering: **none**. Zero references to `task_manager` /
  `session_todos` in `crates/astra-cli/src/tui/`.

## 3. Design

Three crates touched, three primitives added:

### 3.1 Notify-on-write

`TaskStore::save()` should emit a signal when rows actually change.
Since we have **two** impls:

- `InMemoryTaskStore`: add a `tokio::sync::broadcast` channel; emit
  `session_id` on every successful save.
- `MatrixOneTaskStore`: same — emit locally. Cross-node notification
  would need MO pubsub; we deliberately punt that to Phase 2 (Plan
  §4.1). For now cross-node updates are visible via polling only, which
  matches MO's current write frequency (dozens of writes per minute at
  peak).

API:

```rust
// astra-tools/src/task_mgmt.rs
#[async_trait]
pub trait TaskStore: Send + Sync {
    // ... existing methods ...

    /// Subscribe to task-changed events for any session. `None` return
    /// means this store doesn't support subscriptions (acceptable; the
    /// observer falls back to polling).
    fn subscribe(&self) -> Option<tokio::sync::broadcast::Receiver<String>> {
        None
    }
}
```

`String` payload is the session_id so subscribers filter cheaply.

### 3.2 TaskBoardObserver

A per-session observer that mirrors `TasksV2Store`:

```rust
// astra-cli/src/tui/task_board_observer.rs
pub struct TaskBoardObserver {
    store: Arc<dyn TaskStore>,
    session_id: Arc<Mutex<String>>,    // follows state.session_id rebinds
    tasks: Arc<Mutex<Vec<SessionTask>>>,
    hidden: Arc<Mutex<bool>>,
    changed: tokio::sync::Notify,      // one-level fan-out to TUI
}

impl TaskBoardObserver {
    pub fn spawn(store: Arc<dyn TaskStore>, session_id: String) -> Arc<Self>;
    pub fn snapshot(&self) -> Vec<SessionTask>;
    pub fn is_hidden(&self) -> bool;
    pub fn changed(&self) -> &tokio::sync::Notify;
    pub fn rebind_session(&self, session_id: String);
}
```

Background loop inside `spawn`:

1. On start: fetch once, notify.
2. Subscribe to `store.subscribe()` if available.
3. Poll every 5s **only if any task is `in_progress` or `pending`**
   — matches the reference TUI's `hasIncomplete` gate.
4. On any signal: debounce 50ms, refetch, diff, set `hidden` flag if
   all tasks completed for ≥ 5s, call `changed.notify_waiters()`.

### 3.3 TaskListWidget (ratatui)

A `ratatui::widgets::Widget` with the same truncation + prioritization
as `TaskListV2.tsx`. Roughly ~300 lines including responsive width
calc and the recent-completed 30s TTL tracker.

```rust
pub struct TaskListWidget<'a> {
    tasks: &'a [SessionTask],
    columns: u16,
    rows: u16,
    recent_completed: &'a HashMap<String, Instant>,  // id → when-observed
}

impl<'a> Widget for TaskListWidget<'a> { /* ... */ }
```

Mount points in this repo (equivalent to the reference TUI's):

1. **`chat_widget`** — show the task board after the current agent
   response when `observer.snapshot().is_empty() == false`.
2. **`status_indicator`** (spinner) — compact header line `N tasks
   (K done, M active)` while the agent is running.
3. **`/task list`** slash command — route through the observer's
   snapshot instead of re-calling the tool so it's instant and
   matches the live display.

## 4. Phased rollout

**Phase 4.1 — observer + widget + auto-expand (MVP, ~550 LOC)**

Shipped in commit 4.1a (building blocks, pure functions + background task):

- ✅ `TaskStore::subscribe` default impl returning `None`; `InMemoryTaskStore`
  broadcast channel (capacity 16) emitting `session_id` on every save.
- ✅ `TaskBoardObserver` per session (tokio task): subscribes to the
  store broadcast, debounces 50 ms, falls back to 5 s poll when any
  task is pending/in_progress, shorter 250 ms post-rebind poll, 5 s
  "all-completed → hidden" timer. `rebind_session` uses `Notify::notify_one`
  (permit-storing) so the driver wakes even when parked in a different
  `select!` branch.
- ✅ `task_list::render()` pure fn returning `Vec<Line<'static>>`:
  status icons, bold/strikethrough/dim by status, blocked-by badge,
  responsive subject truncation, priority-ordered truncation with
  `… +N in progress, M pending, K completed` summary. `render_next_hint`
  for the collapsed-state one-line nudge.

Follow-up (call it **Phase 4.1.5 — mount**, not yet shipped):

Deliverables:

- `TaskStore::subscribe` default impl returning `None`.
- `InMemoryTaskStore::subscribe` with a broadcast channel (payload =
  session_id).
- `TaskBoardObserver` per session:
  - holds snapshot, `hidden` flag, `changed` Notify.
  - 50ms debounced refetch on signal.
  - 5s fallback poll only when any task is pending/in_progress.
  - 5s "all completed → hidden" timer, mirroring
    `TasksV2Store::#onHideTimerFired`.
- `TaskListWidget` basic layout (status icon + subject + `blocked by`
  badge; no owner/activity — Phase 4.3).
- `TuiTaskBoardState` mirror of the reference TUI's `expandedView`:
  - Ctrl+T toggles between `Tasks` and `None`.
  - `task_create` / `task_update` tool call in server dispatch
    auto-sets `expandedView = Tasks`. Hook point is inside
    `TaskManager::create/update` — signal the observer, observer
    forwards the "expanded" hint to the TUI. *Cross-process caveat*:
    when server-side ServerToolExecutor does the write, the TUI side
    sees the signal through the MO poll; auto-expand fires after
    ≤5s, not immediately. Acceptable for MVP.
- Mount in REPL's standalone area and under the spinner, following the
  exact visibility rules in §1.4.
- Hide-triggers-collapse effect at the REPL top level (one place only,
  matching `useTasksV2WithCollapseEffect`).

**Phase 4.2 — subscription + instant auto-expand**

- `MatrixOneTaskStore::subscribe` using MO pubsub (requires Phase 2
  bg_tasks pubsub plumbing from the main task system plan). Makes
  auto-expand fire within ~10ms of a tool call even when the write
  happens server-side.
- Responsive truncation matching `TaskItem` (subject width calc,
  columns-based show/hide owner).
- Recent-completed 30s TTL prioritization when truncated.

**Phase 4.3 — full the reference TUI parity**

- Owner badge (requires exposing spawner active-agent set into TUI app
  state — separate plumbing).
- Activity line (rolled-up recent tool calls per teammate — requires
  `summarizeRecentActivities` equivalent).
- Teammate tree integration when running spawned agents.

## 5. Explicit non-goals in this PR

- No cross-node real-time update. Edge agent seeing changes from a
  cloud-side mutation still needs a 5-second poll. This is acceptable
  because right now only one agent writes to a session at a time.
- No team / teammate colour rendering. Owner display gated behind
  active-teammate data we haven't exposed to TUI yet.
- No `task` expanded view (standalone mode). `/task list` just prints
  text.

## 6. Estimated LOC

Phase 4.1 scope (MVP):

- `TaskStore::subscribe` default + `InMemoryTaskStore` impl: ~50
- `TaskBoardObserver` (snapshot + poll + hide timer + signal): ~220
- `TaskListWidget` (ratatui, basic layout, no responsive owner/activity): ~180
- TUI expanded-view state + Ctrl+T handler + auto-expand hook: ~80
- Mount in REPL scrollable area + spinner footer: ~60
- Tests (observer lifecycle, widget snapshot, expanded-view toggle): ~200
- **Total ~790**

Phase 4.2 adds ~300 (MO subscription wiring + responsive truncation +
recent-completed tracker).

Phase 4.3 adds ~400 (owner/activity rendering + teammate tree).
