//! Session scratchpad task management.
//!
//! Runtime-owned continuity state is the authoritative source for agent progress.
//! These tools are only an explicit user/model scratchpad and must not be relied
//! on for multi-turn continuity or resume.
//!
//! ## Storage model
//!
//! [`TaskManager`] is a per-session **ergonomic wrapper**; [`TaskStore`] is the
//! process-wide storage backend. All business logic (cycle detection, metadata
//! merge, auto-complete, status validation) lives in `TaskManager`. The store
//! only needs primitive read-all / write-all / next-id semantics — the
//! per-session vec is small (dozens of rows) so full replacement per mutation
//! is simpler and fast enough.
//!
//! Two implementations today:
//!
//! * [`InMemoryTaskStore`] — tests and offline-CLI mode.
//! * `astra_services::session_task_store::MatrixOneTaskStore` — production;
//!   same `session_id` visible from edge and cloud.

// #![allow(dead_code)] -- removed; narrow with per-item attrs if needed
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A scratchpad task tracked within the current CLI session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionTask {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    pub subtasks: Vec<SessionSubtask>,
    pub created_at: String,
    pub updated_at: String,
    /// Present-continuous form shown in spinner while in_progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
    /// Which agent owns this task (for multi-agent sessions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Arbitrary key-value metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
    /// Task IDs that this task blocks (cannot start until this completes).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<String>,
    /// Task IDs that must complete before this task can start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
}

/// A subtask within a SessionTask.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionSubtask {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    pub depends_on: Vec<String>,
    /// Sub-agent or user that owns this subtask. Defaults to the
    /// parent task's owner unless the create call explicitly
    /// overrides — without inheritance, sub-agents looking for
    /// "my work" miss subtasks of tasks they own (U-7 unhappy path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTaskStatusKind {
    InProgress,
    Pending,
    Completed,
    Failed,
    Cancelled,
    Archived,
    Deleted,
    Other,
}

pub const SESSION_TASK_STATUS_PENDING: &str = "pending";
pub const SESSION_TASK_STATUS_IN_PROGRESS: &str = "in_progress";
pub const SESSION_TASK_STATUS_COMPLETED: &str = "completed";
pub const SESSION_TASK_STATUS_FAILED: &str = "failed";
pub const SESSION_TASK_STATUS_CANCELLED: &str = "cancelled";
pub const SESSION_TASK_STATUS_ARCHIVED: &str = "archived";
pub const SESSION_TASK_STATUS_DELETED: &str = "deleted";

    pub fn session_task_status_kind(status: &str) -> SessionTaskStatusKind {
        match status {
            SESSION_TASK_STATUS_IN_PROGRESS => SessionTaskStatusKind::InProgress,
            SESSION_TASK_STATUS_PENDING => SessionTaskStatusKind::Pending,
            SESSION_TASK_STATUS_COMPLETED => SessionTaskStatusKind::Completed,
            SESSION_TASK_STATUS_FAILED => SessionTaskStatusKind::Failed,
            SESSION_TASK_STATUS_CANCELLED => SessionTaskStatusKind::Cancelled,
            SESSION_TASK_STATUS_ARCHIVED => SessionTaskStatusKind::Archived,
            SESSION_TASK_STATUS_DELETED => SessionTaskStatusKind::Deleted,
            other => {
                tracing::warn!(%other, "session_task_status_kind: unknown status");
                SessionTaskStatusKind::Other
            }
        }
    }

pub fn session_task_is_active(status: &str) -> bool {
    matches!(
        session_task_status_kind(status),
        SessionTaskStatusKind::InProgress | SessionTaskStatusKind::Pending
    )
}

pub fn session_task_is_in_progress(status: &str) -> bool {
    session_task_status_kind(status) == SessionTaskStatusKind::InProgress
}

pub fn session_task_is_pending(status: &str) -> bool {
    session_task_status_kind(status) == SessionTaskStatusKind::Pending
}

pub fn session_task_is_completed(status: &str) -> bool {
    session_task_status_kind(status) == SessionTaskStatusKind::Completed
}

pub fn session_task_is_failed(status: &str) -> bool {
    session_task_status_kind(status) == SessionTaskStatusKind::Failed
}

pub fn session_task_is_cancelled(status: &str) -> bool {
    session_task_status_kind(status) == SessionTaskStatusKind::Cancelled
}

pub fn session_task_is_unsuccessful(status: &str) -> bool {
    matches!(
        session_task_status_kind(status),
        SessionTaskStatusKind::Failed | SessionTaskStatusKind::Cancelled
    )
}

pub fn session_task_is_started(status: &str) -> bool {
    matches!(
        session_task_status_kind(status),
        SessionTaskStatusKind::InProgress | SessionTaskStatusKind::Completed
    )
}

pub fn session_task_can_be_archived(status: &str) -> bool {
    matches!(
        session_task_status_kind(status),
        SessionTaskStatusKind::Completed
            | SessionTaskStatusKind::Failed
            | SessionTaskStatusKind::Cancelled
    )
}

pub fn session_task_can_be_stopped(status: &str) -> bool {
    matches!(
        session_task_status_kind(status),
        SessionTaskStatusKind::Pending | SessionTaskStatusKind::InProgress
    )
}

/// Point-in-time snapshot of a single session's task list plus its id counter.
/// Used by the session-state rollback journal to undo a turn's task mutations.
#[derive(Debug, Clone)]
pub struct TaskManagerSnapshot {
    pub tasks: Vec<SessionTask>,
    pub next_task_id: u32,
}

pub struct TaskMutationResult {
    pub tasks: Vec<SessionTask>,
    pub next_task_id: Option<u32>,
    pub response: String,
}

pub type TaskMutation =
    Box<dyn FnOnce(Vec<SessionTask>, u32) -> Result<TaskMutationResult, String> + Send>;

/// Process-wide storage backend for session task lists.
///
/// Conceptually every session_id addresses an independent vec; the store
/// hands out the vec plus an id counter on read, and persists a new vec on
/// write. Business logic (cycle detection, auto-complete, metadata merge)
/// lives in [`TaskManager`] so it is shared by all backends.
#[async_trait]
pub trait TaskStore: Send + Sync {
    /// Load every task for this session in stable order.
    async fn load(&self, session_id: &str) -> Result<Vec<SessionTask>, String>;

    /// Load only `pending` and `in_progress` tasks.
    ///
    /// Default impl loads all rows and filters in Rust — correct but
    /// ships the whole table over the wire. `MatrixOneTaskStore` overrides
    /// this with a WHERE clause so the index
    /// `idx_session_todos_session_status_updated` is used and only
    /// matching rows are returned.
    async fn load_active(&self, session_id: &str) -> Result<Vec<SessionTask>, String> {
        Ok(self
            .load(session_id)
            .await?
            .into_iter()
            .filter(|t| session_task_is_active(&t.status))
            .collect())
    }

    /// Archive historical tasks.
    ///
    /// Default implementation works session-locally so tests/offline mode
    /// still have coherent semantics. SQL-backed stores can override to widen
    /// the bulk scope (for example, all tasks owned by the current user).
    async fn archive(&self, session_id: &str, args: &Value) -> Result<String, String> {
        let task_id = args
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let days_raw = args
            .get("older_than_days")
            .and_then(Value::as_u64)
            .unwrap_or(30);
        let days = i64::try_from(days_raw)
            .map_err(|_| format!("older_than_days is too large: {days_raw}"))?;
        let now = chrono::Utc::now();
        let now_rfc3339 = now.to_rfc3339();
        let cutoff = now - chrono::Duration::days(days);
        let session_label = session_id.to_string();

        self.mutate(
            session_id,
            Box::new(move |mut tasks, _next| {
                if let Some(task_id) = task_id {
                    let Some(task) = tasks.iter_mut().find(|t| t.id == task_id) else {
                        return Ok(TaskMutationResult {
                            tasks,
                            next_task_id: None,
                            response: prefix_summary(
                                format!(
                                    "Refused: task #{task_id} not found in session {session_label}"
                                ),
                                json!({
                                    "success": false,
                                    "task_id": task_id,
                                    "message": format!(
                                        "Task '{}' was not found in session '{}'",
                                        task_id, session_label
                                    ),
                                })
                                .to_string(),
                            ),
                        });
                    };
                    let previous_status = task.status.clone();
                    if session_task_status_kind(&previous_status) == SessionTaskStatusKind::Archived
                    {
                        return Ok(TaskMutationResult {
                            tasks,
                            next_task_id: None,
                            response: prefix_summary(
                                format!("Refused: task #{task_id} is already archived"),
                                json!({
                                    "success": false,
                                    "task_id": task_id,
                                    "previous_status": previous_status,
                                    "message": format!("Task '{}' is already archived", task_id),
                                })
                                .to_string(),
                            ),
                        });
                    }
                    if !session_task_can_be_archived(&previous_status) {
                        return Ok(TaskMutationResult {
                            tasks,
                            next_task_id: None,
                            response: prefix_summary(
                                format!(
                                    "Refused: task #{task_id} is '{previous_status}' — only completed, failed, or cancelled tasks can be archived"
                                ),
                                json!({
                                    "success": false,
                                    "task_id": task_id,
                                    "previous_status": previous_status,
                                    "message": format!(
                                        "Task '{}' must be completed, failed, or cancelled before it can be archived",
                                        task_id
                                    ),
                                })
                                .to_string(),
                            ),
                        });
                    }

                    task.status = SESSION_TASK_STATUS_ARCHIVED.to_string();
                    task.updated_at = now_rfc3339.clone();
                    return Ok(TaskMutationResult {
                        tasks,
                        next_task_id: None,
                        response: prefix_summary(
                            format!("Archived task #{task_id} (was {previous_status})"),
                            json!({
                                "success": true,
                                "task_id": task_id,
                                "previous_status": previous_status,
                                "status": SESSION_TASK_STATUS_ARCHIVED,
                                "message": format!("Task '{}' archived", task_id),
                            })
                            .to_string(),
                        ),
                    });
                }

                let mut archived = 0u64;
                for task in &mut tasks {
                    if !session_task_is_completed(&task.status) {
                        continue;
                    }
                    let updated_at = chrono::DateTime::parse_from_rfc3339(&task.updated_at)
                        .map_err(|e| {
                            format!(
                                "task '{}' has invalid updated_at '{}' for archive cutoff: {e}",
                                task.id, task.updated_at
                            )
                        })?
                        .with_timezone(&chrono::Utc);
                    if updated_at < cutoff {
                        task.status = "archived".to_string();
                        task.updated_at = now_rfc3339.clone();
                        archived = archived.saturating_add(1);
                    }
                }
                Ok(TaskMutationResult {
                    tasks,
                    next_task_id: None,
                    response: prefix_summary(
                        format!(
                            "Archived {archived} completed task(s) older than {days} days in session {session_label}"
                        ),
                        json!({
                            "success": true,
                            "archived": archived,
                            "older_than_days": days,
                            "scope": "session",
                            "session_id": session_label,
                            "message": format!(
                                "Archived {} completed task(s) older than {} days in session '{}'",
                                archived, days, session_label
                            ),
                        })
                        .to_string(),
                    ),
                })
            }),
        )
        .await
    }
    /// Replace the session's entire task list. The store must treat this as
    /// atomic from the caller's perspective.
    async fn save(&self, session_id: &str, tasks: Vec<SessionTask>) -> Result<(), String>;
    /// Atomically load, mutate, and persist a session's task state.
    ///
    /// Implementations that can be shared across processes must serialize this
    /// method per `session_id` so concurrent create/update/stop calls cannot
    /// overwrite each other with stale full-list saves.
    async fn mutate(&self, session_id: &str, mutation: TaskMutation) -> Result<String, String> {
        let tasks = self.load(session_id).await?;
        let next = self.peek_next_task_id(session_id).await?;
        let result = mutation(tasks, next)?;
        if let Some(next) = result.next_task_id {
            self.set_next_task_id(session_id, next).await?;
        }
        self.save(session_id, result.tasks).await?;
        Ok(result.response)
    }
    /// Return and consume the next integer to use when forming `task-<n>` ids.
    /// Must be monotonic per session_id.
    async fn next_task_id(&self, session_id: &str) -> Result<u32, String>;
    /// Optional: set the id counter (used by `restore_snapshot` to rewind
    /// numbering after a turn rollback). Default impl ignores the hint.
    async fn set_next_task_id(&self, _session_id: &str, _next: u32) -> Result<(), String> {
        Ok(())
    }
    /// Read the next id WITHOUT consuming or mutating the counter.
    /// Used by `snapshot_state` to capture the counter for rollback
    /// without leaving a hole in the id sequence.
    ///
    /// No default impl: the original alloc-then-rewind fallback was a
    /// silent foot-gun — any new `TaskStore` impl that forgot to
    /// override it would re-introduce the A1 race (concurrent
    /// allocators would have their bump clobbered by the rewind).
    /// Requiring an explicit impl makes correctness a compile-time
    /// requirement.
    async fn peek_next_task_id(&self, session_id: &str) -> Result<u32, String>;
    /// Subscribe to "task list changed for <session_id>" events.
    /// Default impl returns `None` (store does not support subscriptions,
    /// and consumers fall back to polling — see `TaskBoardObserver`).
    ///
    /// Payload is the session_id that changed so a single subscriber can
    /// multiplex observers across sessions cheaply.
    fn subscribe(&self) -> Option<tokio::sync::broadcast::Receiver<String>> {
        None
    }

    /// Load every session the store knows about, as
    /// `(session_id, tasks)` pairs. Used by the multi-session
    /// task board view. Order is implementation-defined —
    /// callers sort on `updated_at` if they need a stable view.
    ///
    /// Default impl returns an empty vec so non-multi-session
    /// stores never have to think about this method. Stores that
    /// *can* enumerate sessions (in-memory, MatrixOne) override
    /// with a real implementation.
    async fn load_all_sessions(&self) -> Result<Vec<(String, Vec<SessionTask>)>, String> {
        Ok(Vec::new())
    }
}

/// In-memory store for tests and offline CLI mode. Holds a map
/// `session_id -> (Vec<SessionTask>, next_id)` behind a single `Mutex`.
/// Broadcasts `session_id` on every successful save so `TaskBoardObserver`
/// can refresh immediately without waiting for a fallback poll.
pub struct InMemoryTaskStore {
    sessions: Mutex<HashMap<String, InMemorySession>>,
    /// Broadcast sender for "session X changed" events. Capacity 16 is
    /// generous for the expected subscriber count (one observer per REPL
    /// + occasional test subscribers); slow consumers get dropped events
    ///   (`RecvError::Lagged`) rather than blocking writers.
    changed_tx: tokio::sync::broadcast::Sender<String>,
}

impl Default for InMemoryTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default, Clone)]
struct InMemorySession {
    tasks: Vec<SessionTask>,
    next_id: u64,
}

impl InMemoryTaskStore {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            changed_tx: tokio::sync::broadcast::channel(16).0,
        }
    }
}

#[async_trait]
impl TaskStore for InMemoryTaskStore {
    async fn load(&self, session_id: &str) -> Result<Vec<SessionTask>, String> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| "task store: session map poisoned".to_string())?;
        Ok(sessions
            .get(session_id)
            .map(|s| s.tasks.clone())
            .unwrap_or_default())
    }

    async fn save(&self, session_id: &str, tasks: Vec<SessionTask>) -> Result<(), String> {
        {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| "task store: session map poisoned".to_string())?;
            let entry = sessions.entry(session_id.to_string()).or_default();
            entry.tasks = tasks;
        }
        // Best-effort broadcast. `send` errors only when there are no
        // receivers, which is the common "no observer attached" case in
        // tests and headless CLI — not an error.
        let _ = self.changed_tx.send(session_id.to_string());
        Ok(())
    }

    async fn mutate(&self, session_id: &str, mutation: TaskMutation) -> Result<String, String> {
        let response = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| "task store: session map poisoned".to_string())?;
            let entry = sessions.entry(session_id.to_string()).or_default();
            let next = if entry.next_id == 0 { 1 } else { entry.next_id };
            let next = u32::try_from(next)
                .map_err(|_| format!("task id counter exhausted for session {session_id}"))?;
            let result = mutation(entry.tasks.clone(), next)?;
            entry.tasks = result.tasks;
            if let Some(next_task_id) = result.next_task_id {
                entry.next_id = u64::from(next_task_id);
            }
            result.response
        };
        let _ = self.changed_tx.send(session_id.to_string());
        Ok(response)
    }

    async fn next_task_id(&self, session_id: &str) -> Result<u32, String> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| "task store: session map poisoned".to_string())?;
        let entry = sessions.entry(session_id.to_string()).or_default();
        if entry.next_id == 0 {
            entry.next_id = 1;
        }
        // Read → reject > u32::MAX → bump via checked_add. Making the
        // bound explicit: at `u32::MAX as u64 + 1` we've already
        // allocated u32::MAX in a prior call, so the next caller hits
        // the `try_from` guard and returns early before we touch the
        // counter again. `checked_add` on u64 is overkill today but
        // cheaper than a buried invariant — keeps the operation safe
        // even if someone pokes the InMemorySession directly with a
        // large seed.
        let id = entry.next_id;
        let task_id = u32::try_from(id)
            .map_err(|_| format!("task id counter exhausted for session {session_id}"))?;
        entry.next_id = id
            .checked_add(1)
            .ok_or_else(|| format!("task id counter overflow for session {session_id}"))?;
        Ok(task_id)
    }

    async fn set_next_task_id(&self, session_id: &str, next: u32) -> Result<(), String> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| "task store: session map poisoned".to_string())?;
        let entry = sessions.entry(session_id.to_string()).or_default();
        entry.next_id = u64::from(next);
        Ok(())
    }

    async fn peek_next_task_id(&self, session_id: &str) -> Result<u32, String> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| "task store: session map poisoned".to_string())?;
        let next = sessions
            .get(session_id)
            .map(|s| if s.next_id == 0 { 1 } else { s.next_id })
            .unwrap_or(1);
        u32::try_from(next)
            .map_err(|_| format!("task id counter exhausted for session {session_id}"))
    }

    fn subscribe(&self) -> Option<tokio::sync::broadcast::Receiver<String>> {
        Some(self.changed_tx.subscribe())
    }

    async fn load_all_sessions(&self) -> Result<Vec<(String, Vec<SessionTask>)>, String> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| "task store: session map poisoned".to_string())?;
        let mut out: Vec<(String, Vec<SessionTask>)> = sessions
            .iter()
            .filter(|(_, s)| !s.tasks.is_empty())
            .map(|(sid, s)| (sid.clone(), s.tasks.clone()))
            .collect();
        // Deterministic order: the HashMap's iteration is random,
        // but callers want a stable view for snapshot diffs in the
        // multi-session observer. Sort by session_id — the row-level
        // sort on updated_at happens in `task_board_multi::flatten`.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

/// Per-session handle to the shared task store. Business logic lives here;
/// storage is delegated to the `TaskStore` impl.
///
/// The session id can be rebound at runtime via [`TaskManager::rebind`] so
/// that startup code paths (which construct the REPL state before login
/// resolves the session) don't have to rebuild the whole manager graph
/// once the real session id arrives.
pub struct TaskManager {
    session_id: Mutex<String>,
    store: Arc<dyn TaskStore>,
}

/// Prepend a one-line human-readable summary to a JSON response body.
/// The model sees the outcome at a glance without parsing the JSON;
/// downstream code that still wants to parse the JSON can do so by
/// splitting on the first `\n` and feeding the remainder to
/// serde_json. Claudecode's task tools do this via
/// `mapToolResultToToolResultBlockParam` returning a plain string;
/// we keep the JSON for back-compat and prefix the summary.
pub(crate) fn prefix_summary(summary: impl Into<String>, json_body: String) -> String {
    format!("{}\n{}", summary.into(), json_body)
}

const VALID_UPDATE_STATUSES: &[&str] = &[
    "pending",
    "in_progress",
    "completed",
    "failed",
    "cancelled",
    "deleted",
];

/// Title normalization for U-4 duplicate detection. Lowercase, drop
/// ASCII punctuation, collapse whitespace. Conservative on purpose:
/// we'd rather miss a near-duplicate (model retries with different
/// wording → 2 tasks, manageable) than false-positive a legitimate
/// fresh task (refuse with `duplicate_of` → model confused).
fn normalize_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut prev_was_space = true; // skip leading whitespace
    for ch in title.chars() {
        if ch.is_whitespace() {
            if !prev_was_space {
                out.push(' ');
                prev_was_space = true;
            }
        } else if ch.is_ascii_punctuation() {
            // Drop punctuation entirely so "fix bug." matches "fix bug".
            // Don't pretend it's a word boundary either — preserve
            // adjacency so "auth-flow" matches "auth flow" without the
            // hyphen merging into the next char.
        } else {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            prev_was_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

fn normalize_update_status(args: &Value) -> Result<Option<String>, String> {
    let canonical = args.get("new_status").and_then(Value::as_str);
    let legacy = args.get("status").and_then(Value::as_str);
    let Some(status) = canonical.or(legacy) else {
        return Ok(None);
    };
    if let (Some(a), Some(b)) = (canonical, legacy)
        && a != b
    {
        return Err(format!(
            "conflicting status fields: new_status='{a}' but status='{b}'"
        ));
    }
    if !VALID_UPDATE_STATUSES.contains(&status) {
        return Err(format!(
            "invalid new_status '{}' (valid: {})",
            status,
            VALID_UPDATE_STATUSES.join("|")
        ));
    }
    Ok(Some(status.to_string()))
}

/// Highest numeric suffix on any `task-<n>` id in the list, or 0 when
/// the list is empty or all ids are non-numeric. Used as a conservative
/// fallback when peeking the counter fails — the restored counter is
/// at least `max + 1`, which cannot collide with a surviving id.
fn max_task_id(tasks: &[SessionTask]) -> u32 {
    tasks
        .iter()
        .filter_map(|t| {
            t.id.strip_prefix("task-")
                .and_then(|s| s.parse::<u32>().ok())
        })
        .max()
        .unwrap_or(0)
}

/// Bidirectional subtask ↔ parent status reconciliation. Called after
/// any subtask mutation that might flip the all-completed state.
///
/// Forward arms:
/// - any started subtask (`in_progress` / `completed`) promotes a still-pending
///   parent to `in_progress`, so the task board stops reading the parent as
///   untouched work once execution has clearly begun.
/// - all subtasks completed → parent completed, but only when the parent is in
///   an *active* state (pending / in_progress). Terminal non-success states
///   (failed / cancelled) stay as-is, since promoting them to completed would
///   silently erase a failure signal.
///
/// Reverse arm (a subtask flipped back from completed → parent reopens):
/// only fires when the parent is currently exactly "completed". If the
/// parent is in any other status — including any future status not in
/// today's vocabulary — we leave it alone. Reason: an operator who has
/// explicitly moved the parent to e.g. "archived" probably doesn't want
/// a subtask edit to resurrect it. The narrow `== "completed"` match
/// keeps this function's scope tight and avoids future surprise.
fn reconcile_subtask_completion(task: &mut SessionTask) {
    if task.subtasks.is_empty() {
        return;
    }

    let all_completed = task
        .subtasks
        .iter()
        .all(|st| session_task_is_completed(&st.status));
    if all_completed {
        if session_task_is_active(&task.status) {
            task.status = "completed".to_string();
        }
        return;
    }

    let any_started = task
        .subtasks
        .iter()
        .any(|st| session_task_is_started(&st.status));
    if (any_started && session_task_is_pending(&task.status))
        || session_task_is_completed(&task.status)
    {
        task.status = "in_progress".to_string();
    }
}

impl TaskManager {
    /// Construct a manager bound to a specific session, backed by `store`.
    pub fn new(session_id: impl Into<String>, store: Arc<dyn TaskStore>) -> Self {
        Self {
            session_id: Mutex::new(session_id.into()),
            store,
        }
    }

    /// Convenience for tests: a manager with session_id "default" over a
    /// fresh in-memory store.
    pub fn in_memory() -> Self {
        Self::new("default", Arc::new(InMemoryTaskStore::new()))
    }

    /// Session id this manager currently points at.
    pub fn session_id(&self) -> String {
        self.session_id
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Handle to the underlying store. Exposed so callers outside
    /// `astra-tools` can wire observers that subscribe to the same
    /// change broadcast the manager writes through.
    pub fn store(&self) -> Arc<dyn TaskStore> {
        self.store.clone()
    }

    /// Rebind the session id. Also swaps the store if a new one is supplied
    /// (for the offline → MO upgrade path).
    pub fn rebind(&self, session_id: impl Into<String>) {
        if let Ok(mut guard) = self.session_id.lock() {
            *guard = session_id.into();
        }
    }

    fn sid(&self) -> String {
        self.session_id
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Get a snapshot of all tasks (for brief/diagnostics). This is async
    /// because the store may be remote (MatrixOne).
    pub async fn snapshot(&self) -> Vec<SessionTask> {
        self.store.load(&self.sid()).await.unwrap_or_default()
    }

    /// Load only active tasks and surface backend errors to callers that need
    /// completion-critical task-board state.
    pub async fn load_active_tasks(&self) -> Result<Vec<SessionTask>, String> {
        self.store.load_active(&self.sid()).await
    }

    /// Capture a full rollback snapshot (tasks + next id).
    pub async fn snapshot_state(&self) -> TaskManagerSnapshot {
        let tasks = self.snapshot().await;
        // Read the counter without consuming or mutating it so concurrent
        // allocators can't race us into duplicate ids (the old
        // alloc-then-rewind dance clobbered concurrent increments). If
        // the peek fails (MO pool hiccup, row lock, etc), fall back to
        // `max(existing task id) + 1` rather than `1` — returning 1 on
        // a peek error would make `restore_snapshot` rewind the counter
        // and guarantee duplicate ids on the next allocation.
        let peeked = match self.store.peek_next_task_id(&self.sid()).await {
            Ok(v) => v,
            Err(_) => max_task_id(&tasks).saturating_add(1).max(1),
        };
        TaskManagerSnapshot {
            tasks,
            next_task_id: peeked,
        }
    }

    /// Restore a previously captured snapshot.
    pub async fn restore_snapshot(&self, snapshot: &TaskManagerSnapshot) -> Result<(), String> {
        // Restore the counter *before* the tasks. Otherwise a subscriber
        // reacting to the save-broadcast can allocate a new id using
        // the pre-rollback counter while already seeing post-rollback
        // task rows, resulting in id collisions. The counter-write is
        // side-effect-free to observers (no broadcast), so running it
        // first is safe even if the save below fails.
        self.store
            .set_next_task_id(&self.sid(), snapshot.next_task_id)
            .await?;
        self.store.save(&self.sid(), snapshot.tasks.clone()).await?;
        Ok(())
    }

    /// Create a new task in the session-local task list.
    pub async fn create(&self, args: &Value) -> String {
        let title = match args.get("title").and_then(Value::as_str) {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => return "Error: 'title' is required".to_string(),
        };

        let description = args
            .get("description")
            .and_then(Value::as_str)
            .map(String::from);
        let now = chrono::Utc::now().to_rfc3339();

        let active_form = args
            .get("active_form")
            .and_then(Value::as_str)
            .map(String::from);
        let owner = args.get("owner").and_then(Value::as_str).map(String::from);
        // U-7: subtasks inherit parent's `owner` when they don't
        // declare one explicitly. Without inheritance a sub-agent
        // looking for "my work" misses subtasks of tasks it owns,
        // because the explicit `owner` field on the parent doesn't
        // propagate. Pass `owner` to the subtask builder below so
        // the closure has it.
        let parent_owner_for_subtasks = owner.clone();
        let subtasks: Vec<SessionSubtask> = args
            .get("subtasks")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|st| {
                        let id = st.get("id").and_then(Value::as_str)?;
                        let title = st.get("title").and_then(Value::as_str)?;
                        let explicit_owner =
                            st.get("owner").and_then(Value::as_str).map(String::from);
                        Some(SessionSubtask {
                            id: id.to_string(),
                            title: title.to_string(),
                            description: st
                                .get("description")
                                .and_then(Value::as_str)
                                .map(String::from),
                            status: "pending".to_string(),
                            depends_on: st
                                .get("depends_on")
                                .and_then(Value::as_array)
                                .map(|deps| {
                                    deps.iter()
                                        .filter_map(Value::as_str)
                                        .map(String::from)
                                        .collect()
                                })
                                .unwrap_or_default(),
                            // Inherit when not specified.
                            owner: explicit_owner.or_else(|| parent_owner_for_subtasks.clone()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let metadata = args.get("metadata").and_then(Value::as_object).cloned();
        let sid = self.sid();
        let mutation_title = title.clone();
        match self
            .store
            .mutate(
                &sid,
                Box::new(move |mut tasks, next| {
                    // U-4 dedup: refuse exact-normalized title match
                    // against active (pending/in_progress) tasks.
                    // Without this, a session restart can lead the
                    // model to re-create work it already has open.
                    // Returns the existing id so the model can
                    // continue with the open task instead of
                    // duplicating.
                    let normalized_new = normalize_title(&mutation_title);
                    if let Some(dup) = tasks.iter().find(|t| {
                        session_task_is_active(&t.status)
                            && normalize_title(&t.title) == normalized_new
                    }) {
                        let response = prefix_summary(
                            format!(
                                "Refused: active task #{} already has this title — use update / get instead",
                                dup.id
                            ),
                            json!({
                                "success": false,
                                "duplicate_of": dup.id,
                                "duplicate_title": dup.title,
                                "duplicate_status": dup.status,
                                "message": format!(
                                    "Refused: an active task with the same normalized title already exists (id={}). Use task(action='update') or task(action='get') instead of creating a duplicate.",
                                    dup.id
                                ),
                            })
                            .to_string(),
                        );
                        return Ok(TaskMutationResult {
                            tasks,
                            next_task_id: None,
                            response,
                        });
                    }

                    let task_id = format!("task-{next}");
                    // U-10: if the counter is desynced (corruption or
                    // partial init), `next` may point at an id that
                    // already exists. Surface this loudly so the model
                    // (and operators) know to investigate rather than
                    // silently producing an invisible duplicate or
                    // hitting a raw "Duplicate entry" DB error.
                    if tasks.iter().any(|t| t.id == task_id) {
                        let response = prefix_summary(
                            format!(
                                "Error: task counter desync — id '{task_id}' already exists. \
                                 The session's counter may need to be reset. \
                                 Contact support or use `task(action='list')` to see the \
                                 current task list and manually continue from the last id."
                            ),
                            json!({
                                "success": false,
                                "error": "counter_desync",
                                "conflicting_id": task_id,
                                "message": format!(
                                    "Task id '{task_id}' already exists in this session; \
                                     counter is out of sync with the task list."
                                ),
                            })
                            .to_string(),
                        );
                        return Ok(TaskMutationResult {
                            tasks,
                            next_task_id: None,
                            response,
                        });
                    }
                    let task = SessionTask {
                        id: task_id.clone(),
                        title: mutation_title.clone(),
                        description,
                        status: "pending".to_string(),
                        subtasks,
                        created_at: now.clone(),
                        updated_at: now,
                        active_form,
                        owner,
                        metadata,
                        blocks: Vec::new(),
                        blocked_by: Vec::new(),
                    };
                    tasks.push(task);
                    let response = prefix_summary(
                        format!("Task #{task_id} created: {mutation_title}"),
                        json!({
                            "success": true,
                            "task_id": task_id,
                            "message": format!("Task '{}' created successfully", mutation_title)
                        })
                        .to_string(),
                    );
                    let next_task_id = next.checked_add(1).ok_or_else(|| {
                        "task id counter overflow for session during create".to_string()
                    })?;
                    Ok(TaskMutationResult {
                        tasks,
                        next_task_id: Some(next_task_id),
                        response,
                    })
                }),
            )
            .await
        {
            Ok(response) => response,
            Err(e) => format!("Error: {e}"),
        }
    }

    /// List tasks in the session, optionally filtered by status.
    pub async fn list(&self, args: &Value) -> String {
        let status_filter = args
            .get("status")
            .or_else(|| args.get("status_filter"))
            .and_then(Value::as_str)
            .unwrap_or("all");

        // U-8: route active-filter through load_active so SQL stores
        // can push the WHERE clause down to the index instead of
        // shipping the full table to Rust then filtering.
        let tasks = match status_filter {
            "active" => self.store.load_active(&self.sid()).await,
            _ => self.store.load(&self.sid()).await,
        };
        let tasks = match tasks {
            Ok(t) => t,
            Err(e) => return format!("Error: {e}"),
        };

        let filtered: Vec<_> = tasks
            .iter()
            .filter(|t| match status_filter {
                "all" | "active" => true, // already filtered by load path
                s => t.status == s,
            })
            .map(|t| {
                let subtask_summary = if t.subtasks.is_empty() {
                    String::new()
                } else {
                    let done = t
                        .subtasks
                        .iter()
                        .filter(|st| session_task_is_completed(&st.status))
                        .count();
                    format!(" [{}/{}]", done, t.subtasks.len())
                };
                let mut entry = json!({
                    "id": t.id,
                    "title": t.title,
                    "status": t.status,
                    "subtasks": subtask_summary,
                    "updated_at": t.updated_at,
                });
                if let Some(ref owner) = t.owner {
                    entry["owner"] = json!(owner);
                }
                if !t.blocked_by.is_empty() {
                    entry["blocked_by"] = json!(t.blocked_by);
                }
                // U-5: surface the failure reason inline so the model
                // sees "why" without a follow-up `task.get`. Only on
                // failed rows; other statuses don't have an
                // error_message so the field would be confusing noise.
                if session_task_is_failed(&t.status) {
                    let preview = t
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get("error_message"))
                        .and_then(|v| v.as_str())
                        .map(|s| {
                            const PREVIEW_MAX: usize = 80;
                            if s.chars().count() <= PREVIEW_MAX {
                                s.to_string()
                            } else {
                                let truncated: String =
                                    s.chars().take(PREVIEW_MAX.saturating_sub(1)).collect();
                                format!("{truncated}…")
                            }
                        });
                    if let Some(p) = preview {
                        entry["error_preview"] = json!(p);
                    }
                }
                entry
            })
            .collect();

        if filtered.is_empty() {
            return format!("No tasks found with status '{}'", status_filter);
        }

        json!({
            "count": filtered.len(),
            "tasks": filtered
        })
        .to_string()
    }

    /// Get full details of a task by ID.
    pub async fn get(&self, args: &Value) -> String {
        let task_id = match args.get("task_id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => id,
            _ => return "Error: 'task_id' is required".to_string(),
        };

        let tasks = match self.store.load(&self.sid()).await {
            Ok(t) => t,
            Err(e) => return format!("Error: {e}"),
        };

        match tasks.iter().find(|t| t.id == task_id) {
            Some(task) => serde_json::to_string_pretty(task)
                .unwrap_or_else(|_| "Error: serialization failed".to_string()),
            None => format!("Error: task '{}' not found", task_id),
        }
    }

    /// Update a task's status, metadata, or dependency edges.
    pub async fn update(&self, args: &Value) -> String {
        let task_id = match args.get("task_id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return "Error: 'task_id' is required".to_string(),
        };

        let new_status = match normalize_update_status(args) {
            Ok(status) => status,
            Err(e) => return format!("Error: {e}"),
        };
        let subtask_id = args
            .get("subtask_id")
            .and_then(Value::as_str)
            .map(String::from);
        let error_message = args
            .get("error_message")
            .and_then(Value::as_str)
            .map(String::from);
        let now = chrono::Utc::now().to_rfc3339();

        let title_update = args.get("title").and_then(Value::as_str).map(String::from);
        let desc_update = args
            .get("description")
            .and_then(Value::as_str)
            .map(String::from);
        let active_form_update = args
            .get("active_form")
            .and_then(Value::as_str)
            .map(String::from);
        let owner_update = args.get("owner").and_then(Value::as_str).map(String::from);
        let metadata_update = args.get("metadata").and_then(Value::as_object).cloned();
        let args_for_edges = args.clone();
        let sid = self.sid();

        match self
            .store
            .mutate(
                &sid,
                Box::new(move |mut tasks, _next| {
                    // Subtask path short-circuits: all logic stays local to one SessionTask.
                    if let Some(st_id) = subtask_id.as_deref() {
                        let Some(task) = tasks.iter_mut().find(|t| t.id == task_id) else {
                            return Err(format!("task '{}' not found", task_id));
                        };
                        let Some(subtask) = task.subtasks.iter_mut().find(|st| st.id == st_id) else {
                            let available = task
                                .subtasks
                                .iter()
                                .map(|st| st.id.as_str())
                                .collect::<Vec<_>>();
                            let hint = if available.is_empty() {
                                "task has no subtasks".to_string()
                            } else {
                                format!("available subtask ids: {}", available.join(", "))
                            };
                            return Err(format!(
                                "subtask '{}' not found in task '{}' ({hint})",
                                st_id, task_id
                            ));
                        };
                        let previous_status = subtask.status.clone();
                        if let Some(status) = new_status.as_deref() {
                            subtask.status = status.to_string();
                        }
                        let final_subtask_status = subtask.status.clone();
                        reconcile_subtask_completion(task);
                        task.updated_at = now;
                        let response = prefix_summary(
                            format!(
                                "Subtask {st_id} of #{task_id}: {previous_status} → {final_subtask_status}"
                            ),
                            json!({
                                "success": true,
                                "task_id": task_id,
                                "subtask_id": st_id,
                                "previous_status": previous_status,
                                "status": final_subtask_status,
                                "message": format!("Subtask '{}' updated to '{}'", st_id, final_subtask_status)
                            })
                            .to_string(),
                        );
                        return Ok(TaskMutationResult {
                            tasks,
                            next_task_id: None,
                            response,
                        });
                    }

                    // "deleted" = soft-remove + clean symmetric edges.
                    if new_status.as_deref() == Some("deleted") {
                        let Some(previous_status) = tasks
                            .iter()
                            .find(|t| t.id == task_id)
                            .map(|t| t.status.clone())
                        else {
                            return Err(format!("task '{}' not found", task_id));
                        };
                        tasks.retain(|t| t.id != task_id);
                        for t in tasks.iter_mut() {
                            t.blocks.retain(|id| id != &task_id);
                            t.blocked_by.retain(|id| id != &task_id);
                        }
                        let response = prefix_summary(
                            format!("Task #{task_id} deleted (was: {previous_status})"),
                            json!({
                                "success": true,
                                "task_id": task_id,
                                "previous_status": previous_status,
                                "status": "deleted",
                                "message": format!("Task '{}' deleted", task_id)
                            })
                            .to_string(),
                        );
                        return Ok(TaskMutationResult {
                            tasks,
                            next_task_id: None,
                            response,
                        });
                    }

                    let previous_status = match tasks.iter().find(|t| t.id == task_id) {
                        Some(t) => t.status.clone(),
                        None => return Err(format!("task '{}' not found", task_id)),
                    };

                    // Collect proposed edge changes before mutating so cycle detection
                    // sees a consistent view.
                    let proposed_blocks: Vec<String> = args_for_edges
                        .get("add_blocks")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default();
                    let proposed_blocked_by: Vec<String> = args_for_edges
                        .get("add_blocked_by")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default();
                    let remove_blocks: Vec<String> = args_for_edges
                        .get("remove_blocks")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default();
                    let remove_blocked_by: Vec<String> = args_for_edges
                        .get("remove_blocked_by")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default();

                    for id in &proposed_blocks {
                        if id == &task_id {
                            return Err(format!("task '{}' cannot block itself", task_id));
                        }
                    }
                    for id in &proposed_blocked_by {
                        if id == &task_id {
                            return Err(format!("task '{}' cannot be blocked by itself", task_id));
                        }
                    }

                    // Cycle detection on the projected graph.
                    if !proposed_blocks.is_empty() || !proposed_blocked_by.is_empty() {
                        use std::collections::{HashSet, VecDeque};
                        let mut blocked_by: HashMap<String, HashSet<String>> = HashMap::new();
                        for t in tasks.iter() {
                            blocked_by
                                .entry(t.id.clone())
                                .or_default()
                                .extend(t.blocked_by.iter().cloned());
                        }
                        let entry = blocked_by.entry(task_id.clone()).or_default();
                        for r in &remove_blocked_by {
                            entry.remove(r);
                        }
                        for x in &proposed_blocks {
                            blocked_by
                                .entry(x.clone())
                                .or_default()
                                .insert(task_id.clone());
                        }
                        for y in &proposed_blocked_by {
                            blocked_by
                                .entry(task_id.clone())
                                .or_default()
                                .insert(y.clone());
                        }
                        let mut visited: HashSet<String> = HashSet::new();
                        let mut queue: VecDeque<String> = VecDeque::new();
                        if let Some(seeds) = blocked_by.get(&task_id) {
                            for s in seeds {
                                queue.push_back(s.clone());
                            }
                        }
                        while let Some(node) = queue.pop_front() {
                            if node == task_id {
                                return Err(format!(
                                    "adding these dependencies would create a cycle involving '{}'",
                                    task_id
                                ));
                            }
                            if !visited.insert(node.clone()) {
                                continue;
                            }
                            if let Some(next) = blocked_by.get(&node) {
                                for n in next {
                                    queue.push_back(n.clone());
                                }
                            }
                        }
                    }

                    let Some(task) = tasks.iter_mut().find(|t| t.id == task_id) else {
                        return Err(format!("task '{}' not found", task_id));
                    };

                    if let Some(status) = new_status.as_deref() {
                        task.status = status.to_string();
                        if session_task_is_completed(status) {
                            // Cascade parent→subtask completion, but preserve any
                            // subtask already in a terminal non-success state.
                            for subtask in &mut task.subtasks {
                                if !session_task_is_failed(&subtask.status)
                                    && !session_task_is_cancelled(&subtask.status)
                                    && !session_task_is_completed(&subtask.status)
                                {
                                    subtask.status = "completed".to_string();
                                }
                            }
                        }
                    }
                    if let Some(err) = error_message.as_deref() {
                        // Stash structured error in metadata so `list`
                        // can surface a preview without parsing
                        // description prose. We also keep the legacy
                        // description-append for back-compat with any
                        // reader that grep'd description for "Error:".
                        task.description = Some(format!(
                            "{}\n\nError: {}",
                            task.description.as_deref().unwrap_or(""),
                            err
                        ));
                        let meta = task.metadata.get_or_insert_with(Default::default);
                        meta.insert("error_message".to_string(), json!(err));
                    }
                    if let Some(title) = title_update.as_deref() {
                        task.title = title.to_string();
                    }
                    if let Some(desc) = desc_update.as_deref() {
                        task.description = Some(desc.to_string());
                    }
                    if let Some(af) = active_form_update.as_deref() {
                        task.active_form = Some(af.to_string());
                    }
                    if let Some(owner) = owner_update.as_deref() {
                        task.owner = Some(owner.to_string());
                    }
                    if let Some(meta_update) = metadata_update.as_ref() {
                        let meta = task.metadata.get_or_insert_with(serde_json::Map::new);
                        for (k, v) in meta_update {
                            if v.is_null() {
                                meta.remove(k);
                            } else {
                                meta.insert(k.clone(), v.clone());
                            }
                        }
                        if meta.is_empty() {
                            task.metadata = None;
                        }
                    }

                    for id in proposed_blocks {
                        if !task.blocks.contains(&id) {
                            task.blocks.push(id);
                        }
                    }
                    for id in proposed_blocked_by {
                        if !task.blocked_by.contains(&id) {
                            task.blocked_by.push(id);
                        }
                    }
                    task.blocks.retain(|b| !remove_blocks.contains(b));
                    task.blocked_by.retain(|b| !remove_blocked_by.contains(b));
                    task.updated_at = now;

                    reconcile_subtask_completion(task);

                    let final_status = task.status.clone();
                    let response = prefix_summary(
                        format!("Task #{task_id}: {previous_status} → {final_status}"),
                        json!({
                            "success": true,
                            "task_id": task_id,
                            "previous_status": previous_status,
                            "status": final_status,
                            "message": format!("Task '{}' updated to '{}'", task_id, final_status)
                        })
                        .to_string(),
                    );
                    Ok(TaskMutationResult {
                        tasks,
                        next_task_id: None,
                        response,
                    })
                }),
            )
            .await
        {
            Ok(response) => response,
            Err(e) => format!("Error: {e}"),
        }
    }

    /// Stop/cancel a running task.
    pub async fn stop(&self, args: &Value) -> String {
        let task_id = match args.get("task_id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return "Error: 'task_id' is required".to_string(),
        };

        let reason = args
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("user requested")
            .to_string();
        let now = chrono::Utc::now().to_rfc3339();

        let sid = self.sid();
        match self
            .store
            .mutate(
                &sid,
                Box::new(move |mut tasks, _next| {
                    let Some(task_idx) = tasks.iter().position(|t| t.id == task_id) else {
                        return Err(format!("task '{}' not found", task_id));
                    };
                    let task_status = tasks[task_idx].status.clone();

                    if !session_task_can_be_stopped(&task_status) {
                        return Ok(TaskMutationResult {
                            tasks,
                            next_task_id: None,
                            response: json!({
                                "success": false,
                                "message": format!("Cannot stop task '{}': status is '{}' (only 'pending' or 'in_progress' can be stopped)", task_id, task_status)
                            })
                            .to_string(),
                        });
                    }

                    let task = &mut tasks[task_idx];
                    let previous_status = task.status.clone();
                    task.status = "cancelled".to_string();
                    task.description = Some(format!(
                        "{}\n\nCancelled: {} (was: {})",
                        task.description.as_deref().unwrap_or(""),
                        reason,
                        previous_status
                    ));
                    task.updated_at = now;

                    let mut cancelled_subtasks = 0;
                    for subtask in &mut task.subtasks {
                        if session_task_can_be_stopped(&subtask.status) {
                            subtask.status = "cancelled".to_string();
                            cancelled_subtasks += 1;
                        }
                    }

                    let summary = if cancelled_subtasks > 0 {
                        format!(
                            "Task #{task_id} cancelled (was {previous_status}; {cancelled_subtasks} subtask(s) cancelled): {reason}"
                        )
                    } else {
                        format!("Task #{task_id} cancelled (was {previous_status}): {reason}")
                    };
                    let response = prefix_summary(
                        summary,
                        json!({
                            "success": true,
                            "task_id": task_id,
                            "previous_status": previous_status,
                            "reason": reason,
                            "cancelled_subtasks": cancelled_subtasks,
                            "message": format!("Task '{}' cancelled (was: {})", task_id, previous_status)
                        })
                        .to_string(),
                    );
                    Ok(TaskMutationResult {
                        tasks,
                        next_task_id: None,
                        response,
                    })
                }),
            )
            .await
        {
            Ok(response) => response,
            Err(e) => format!("Error: {e}"),
        }
    }

    /// Archive historical work. Single-task archive is session-scoped;
    /// bulk archive is store-defined (session-local for in-memory,
    /// user-wide for MatrixOne-backed cloud mode).
    pub async fn archive(&self, args: &Value) -> String {
        match self.store.archive(&self.sid(), args).await {
            Ok(response) => response,
            Err(e) => format!("Error: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mgr() -> TaskManager {
        TaskManager::in_memory()
    }

    #[test]
    fn session_task_status_helpers_cover_domain_taxonomy() {
        assert_eq!(
            session_task_status_kind("pending"),
            SessionTaskStatusKind::Pending
        );
        assert_eq!(
            session_task_status_kind("in_progress"),
            SessionTaskStatusKind::InProgress
        );
        assert_eq!(
            session_task_status_kind("completed"),
            SessionTaskStatusKind::Completed
        );
        assert_eq!(
            session_task_status_kind("failed"),
            SessionTaskStatusKind::Failed
        );
        assert_eq!(
            session_task_status_kind("cancelled"),
            SessionTaskStatusKind::Cancelled
        );
        assert_eq!(
            session_task_status_kind("archived"),
            SessionTaskStatusKind::Archived
        );
        assert_eq!(
            session_task_status_kind("deleted"),
            SessionTaskStatusKind::Deleted
        );
        assert_eq!(
            session_task_status_kind("paused"),
            SessionTaskStatusKind::Other
        );
    }

    #[test]
    fn session_task_status_helpers_keep_active_archive_and_stop_semantics_distinct() {
        assert!(session_task_is_active("pending"));
        assert!(session_task_is_active("in_progress"));
        assert!(!session_task_is_active("completed"));
        assert!(session_task_is_started("completed"));
        assert!(!session_task_is_started("cancelled"));
        assert!(session_task_can_be_archived("completed"));
        assert!(session_task_can_be_archived("failed"));
        assert!(session_task_can_be_archived("cancelled"));
        assert!(!session_task_can_be_archived("archived"));
        assert!(session_task_can_be_stopped("pending"));
        assert!(session_task_can_be_stopped("in_progress"));
        assert!(!session_task_can_be_stopped("cancelled"));
        assert!(session_task_is_unsuccessful("failed"));
        assert!(session_task_is_unsuccessful("cancelled"));
        assert!(!session_task_is_unsuccessful("archived"));
    }

    #[tokio::test]
    async fn create_and_list_roundtrips() {
        let m = mgr();
        let out = m
            .create(&json!({"title": "a", "active_form": "doing a"}))
            .await;
        assert!(out.contains("\"success\":true"), "create: {out}");
        let list = m.list(&json!({"status": "all"})).await;
        assert!(list.contains("\"count\":1"), "list: {list}");
    }

    /// U-5 (unhappy path): when a task is marked `failed` with an
    /// `error_message`, `task.list` must surface that reason as
    /// `error_preview` (truncated to ~80 chars). Pre-fix the model
    /// had to call `task.get(id)` to see why something failed —
    /// most models don't, so the failure context was lost.
    #[tokio::test]
    async fn list_surfaces_failure_reason_for_failed_tasks() {
        let m = mgr();
        m.create(&json!({"title": "do the thing"})).await;
        m.update(&json!({
            "task_id": "task-1",
            "new_status": "failed",
            "error_message": "compilation error in src/lib.rs: cannot find type `Foo`"
        }))
        .await;

        let list = m.list(&json!({"status": "failed"})).await;
        assert!(
            list.contains("error_preview"),
            "failed list output must include error_preview: {list}"
        );
        assert!(
            list.contains("compilation error"),
            "error_preview must carry the failure message: {list}"
        );
    }

    /// U-4 (unhappy path): refuse to create an exact-normalized
    /// duplicate of an active task. The model after a session
    /// restore frequently re-creates work it already has open;
    /// returning the existing id steers it to update/get instead.
    #[tokio::test]
    async fn create_refuses_exact_normalized_duplicate_of_active_task() {
        let m = mgr();
        m.create(&json!({"title": "Implement dark mode toggle"}))
            .await;
        // Same intent, different punctuation/spacing.
        let dup = m
            .create(&json!({"title": "  Implement DARK mode toggle. "}))
            .await;
        assert!(
            dup.contains("Refused"),
            "second create should be refused with duplicate notice; got {dup}"
        );
        assert!(
            dup.contains("duplicate_of"),
            "response must name the existing task id; got {dup}"
        );
        assert!(
            dup.contains("task-1"),
            "duplicate_of must point at the original; got {dup}"
        );
        // The store should still hold exactly one task (no second
        // row appended even though the closure ran).
        let list = m.list(&json!({"status": "all"})).await;
        assert!(
            list.contains("\"count\":1"),
            "duplicate must not be persisted; got {list}"
        );
    }

    #[tokio::test]
    async fn update_missing_subtask_reports_available_ids() {
        let m = mgr();
        m.create(&json!({
            "title": "Build game",
            "subtasks": [
                {"id": "setup", "title": "Create files"},
                {"id": "render", "title": "Render scene"}
            ]
        }))
        .await;

        let out = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "dirs",
                "status": "completed"
            }))
            .await;

        assert!(out.contains("subtask 'dirs' not found"), "got {out}");
        assert!(
            out.contains("available subtask ids: setup, render"),
            "error must help the model recover with valid ids: {out}"
        );
    }

    /// Dedup must NOT block creating a task whose normalized title
    /// matches a *completed* (non-active) task — the user is
    /// resurrecting work intentionally.
    #[tokio::test]
    async fn create_allows_duplicate_of_completed_task() {
        let m = mgr();
        m.create(&json!({"title": "fix bug"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        // Same title now should be allowed since the prior is closed.
        let dup = m.create(&json!({"title": "fix bug"})).await;
        assert!(
            dup.contains("\"success\":true"),
            "create after completion must succeed; got {dup}"
        );
        let list = m.list(&json!({"status": "all"})).await;
        assert!(
            list.contains("\"count\":2"),
            "second instance must persist when prior is completed; got {list}"
        );
    }

    /// U-7 (unhappy path): a subtask should inherit its parent's
    /// `owner` when the subtask doesn't declare one explicitly.
    /// Sub-agents looking for "my tasks" rely on the owner field;
    /// without inheritance, subtasks of tasks they own slip
    /// through.
    #[tokio::test]
    async fn subtask_inherits_parent_owner_unless_overridden() {
        let m = mgr();
        let _ = m
            .create(&json!({
                "title": "ship feature",
                "owner": "code-reviewer",
                "subtasks": [
                    { "id": "s1", "title": "wire schema" },
                    { "id": "s2", "title": "implement" },
                    { "id": "s3", "title": "review", "owner": "specific-reviewer" },
                ],
            }))
            .await;
        let body = m.get(&json!({"task_id": "task-1"})).await;
        // The first two subtasks should carry the parent's owner.
        // Match relaxed for whitespace because pretty-printing
        // varies between serde versions / config.
        assert!(
            body.contains("code-reviewer"),
            "subtasks should inherit parent owner; got {body}"
        );
        assert!(
            body.contains("specific-reviewer"),
            "explicit subtask owner must override the inherited one; got {body}"
        );
        // Verify count: parent owner appears 3 times (parent + 2 inherited subtasks),
        // override appears once.
        assert_eq!(
            body.matches("code-reviewer").count(),
            3,
            "expected parent owner on parent + 2 subtasks; got {body}"
        );
        assert_eq!(
            body.matches("specific-reviewer").count(),
            1,
            "expected override owner on exactly the s3 subtask; got {body}"
        );
    }

    /// Subtask without parent owner: keep `owner` absent rather
    /// than putting an empty string. Otherwise downstream filters
    /// like `owner == "alice"` accidentally match the empty case.
    #[tokio::test]
    async fn subtask_owner_stays_none_when_parent_has_none() {
        let m = mgr();
        let _ = m
            .create(&json!({
                "title": "no owner anywhere",
                "subtasks": [{ "id": "s1", "title": "step 1" }],
            }))
            .await;
        let body = m.get(&json!({"task_id": "task-1"})).await;
        assert!(
            !body.contains("\"owner\""),
            "without parent owner, subtask owner must stay absent; got {body}"
        );
    }

    /// Companion: non-failed tasks must NOT carry an error_preview
    /// field — it'd be confusing noise on completed/in_progress rows.
    #[tokio::test]
    async fn list_omits_error_preview_for_non_failed_tasks() {
        let m = mgr();
        m.create(&json!({"title": "ok task"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        let list = m.list(&json!({"status": "all"})).await;
        assert!(
            !list.contains("error_preview"),
            "completed tasks must not show error_preview: {list}"
        );
    }

    #[tokio::test]
    async fn two_managers_same_session_share_store() {
        let store: Arc<dyn TaskStore> = Arc::new(InMemoryTaskStore::new());
        let a = TaskManager::new("sess-1", store.clone());
        let b = TaskManager::new("sess-1", store.clone());
        a.create(&json!({"title": "from-a"})).await;
        let list = b.list(&json!({"status": "all"})).await;
        assert!(list.contains("from-a"), "b should see a's task: {list}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_creates_keep_every_task_and_unique_ids() {
        let store: Arc<dyn TaskStore> = Arc::new(InMemoryTaskStore::new());
        let mut handles = Vec::new();
        for i in 0..32 {
            let mgr = TaskManager::new("sess-race", store.clone());
            handles.push(tokio::spawn(async move {
                mgr.create(&json!({"title": format!("task {i}")})).await
            }));
        }
        for handle in handles {
            let out = handle.await.expect("join concurrent create");
            assert!(out.contains("\"success\":true"), "{out}");
        }

        let mgr = TaskManager::new("sess-race", store);
        let tasks = mgr.snapshot().await;
        assert_eq!(tasks.len(), 32, "lost task(s): {tasks:?}");
        let mut ids: Vec<_> = tasks.iter().map(|t| t.id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 32, "duplicate ids in {ids:?}");
    }

    #[tokio::test]
    async fn update_accepts_schema_new_status_field() {
        let m = mgr();
        m.create(&json!({"title": "schema contract"})).await;
        let out = m
            .update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert!(out.contains("\"success\":true"), "{out}");
        assert!(out.contains("\"status\":\"in_progress\""), "{out}");
        let task = m.get(&json!({"task_id": "task-1"})).await;
        assert!(
            task.contains("\"status\": \"in_progress\""),
            "new_status must not be a no-op: {task}"
        );
    }

    #[tokio::test]
    async fn update_rejects_unknown_or_conflicting_status_fields() {
        let m = mgr();
        m.create(&json!({"title": "strict status"})).await;
        let invalid = m
            .update(&json!({"task_id": "task-1", "new_status": "active"}))
            .await;
        assert!(invalid.starts_with("Error:"), "{invalid}");
        let task = m.get(&json!({"task_id": "task-1"})).await;
        assert!(
            task.contains("\"status\": \"pending\""),
            "invalid status must not mutate task: {task}"
        );

        let conflict = m
            .update(&json!({
                "task_id": "task-1",
                "new_status": "completed",
                "status": "failed"
            }))
            .await;
        assert!(conflict.starts_with("Error:"), "{conflict}");
    }

    #[tokio::test]
    async fn snapshot_restores_tasks_and_next_id() {
        let m = mgr();
        m.create(&json!({"title": "t1"})).await;
        let snap = m.snapshot_state().await;
        m.create(&json!({"title": "t2"})).await;
        assert!(
            m.list(&json!({"status": "all"}))
                .await
                .contains("\"count\":2")
        );
        m.restore_snapshot(&snap).await.unwrap();
        let list = m.list(&json!({"status": "all"})).await;
        assert!(list.contains("\"count\":1"), "after restore: {list}");
        // Next create should get id reset.
        let out = m.create(&json!({"title": "t2-again"})).await;
        assert!(out.contains("task-2"), "id reset: {out}");
    }

    #[tokio::test]
    async fn cycle_detection_rejects_self_dep() {
        let m = mgr();
        m.create(&json!({"title": "a"})).await;
        let bad = m
            .update(&json!({"task_id": "task-1", "add_blocks": ["task-1"]}))
            .await;
        assert!(bad.starts_with("Error:"), "{bad}");
    }

    #[tokio::test]
    async fn delete_cleans_symmetric_edges() {
        let m = mgr();
        m.create(&json!({"title": "a"})).await;
        m.create(&json!({"title": "b"})).await;
        m.update(&json!({"task_id": "task-1", "add_blocks": ["task-2"]}))
            .await;
        let del = m
            .update(&json!({"task_id": "task-1", "status": "deleted"}))
            .await;
        assert!(del.contains("\"status\":\"deleted\""), "{del}");
        let get_b = m.get(&json!({"task_id": "task-2"})).await;
        assert!(
            !get_b.contains("\"blocked_by\":[\"task-1\"]"),
            "b still references a: {get_b}"
        );
    }

    #[tokio::test]
    async fn completing_last_subtask_auto_completes_parent() {
        let m = mgr();
        let create = m
            .create(&json!({
                "title": "parent",
                "subtasks": [
                    {"id": "s1", "title": "first"},
                    {"id": "s2", "title": "second"}
                ]
            }))
            .await;
        assert!(create.contains("\"success\":true"), "{create}");

        let first = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "s1",
                "status": "completed"
            }))
            .await;
        assert!(first.contains("\"success\":true"), "{first}");
        let after_first = m.get(&json!({"task_id": "task-1"})).await;
        let after_first: SessionTask =
            serde_json::from_str(&after_first).expect("task json after first subtask");
        assert!(
            after_first.status == "in_progress",
            "once a subtask completes, the parent should stop reading as untouched pending work: {after_first:?}"
        );

        let second = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "s2",
                "status": "completed"
            }))
            .await;
        assert!(second.contains("\"success\":true"), "{second}");
        let after_second = m.get(&json!({"task_id": "task-1"})).await;
        let after_second: SessionTask =
            serde_json::from_str(&after_second).expect("task json after second subtask");
        assert!(
            after_second.status == "completed",
            "parent should auto-complete after the last subtask completes: {after_second:?}"
        );
    }

    #[tokio::test]
    async fn starting_a_subtask_promotes_pending_parent_to_in_progress() {
        let m = mgr();
        let create = m
            .create(&json!({
                "title": "parent",
                "subtasks": [
                    {"id": "s1", "title": "first"},
                    {"id": "s2", "title": "second"}
                ]
            }))
            .await;
        assert!(create.contains("\"success\":true"), "{create}");

        let start = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "s1",
                "status": "in_progress"
            }))
            .await;
        assert!(start.contains("\"success\":true"), "{start}");

        let after = m.get(&json!({"task_id": "task-1"})).await;
        let after: SessionTask =
            serde_json::from_str(&after).expect("task json after starting subtask");
        assert_eq!(
            after.status, "in_progress",
            "an active subtask should make the parent read as in-progress so the task board doesn't show it as merely open"
        );
    }

    #[tokio::test]
    async fn subtask_autocomplete_preserves_terminal_parent_status() {
        let m = mgr();
        m.create(&json!({
            "title": "parent",
            "subtasks": [
                {"id": "s1", "title": "first"},
                {"id": "s2", "title": "second"}
            ]
        }))
        .await;

        let failed = m
            .update(&json!({"task_id": "task-1", "status": "failed"}))
            .await;
        assert!(failed.contains("\"status\":\"failed\""), "{failed}");
        m.update(&json!({"task_id": "task-1", "subtask_id": "s1", "status": "completed"}))
            .await;
        m.update(&json!({"task_id": "task-1", "subtask_id": "s2", "status": "completed"}))
            .await;
        let after_failed = m.get(&json!({"task_id": "task-1"})).await;
        let after_failed: SessionTask =
            serde_json::from_str(&after_failed).expect("task json after failed parent");
        assert_eq!(
            after_failed.status, "failed",
            "subtask auto-complete must not overwrite explicit failed status"
        );

        m.create(&json!({
            "title": "cancelled parent",
            "subtasks": [
                {"id": "s1", "title": "first"},
                {"id": "s2", "title": "second"}
            ]
        }))
        .await;
        m.stop(&json!({"task_id": "task-2", "reason": "no longer needed"}))
            .await;
        m.update(&json!({"task_id": "task-2", "subtask_id": "s1", "status": "completed"}))
            .await;
        m.update(&json!({"task_id": "task-2", "subtask_id": "s2", "status": "completed"}))
            .await;
        let after_cancelled = m.get(&json!({"task_id": "task-2"})).await;
        let after_cancelled: SessionTask =
            serde_json::from_str(&after_cancelled).expect("task json after cancelled parent");
        assert_eq!(
            after_cancelled.status, "cancelled",
            "subtask auto-complete must not overwrite explicit cancelled status"
        );
    }

    #[tokio::test]
    async fn uncompleting_subtask_reopens_auto_completed_parent() {
        let m = mgr();
        m.create(&json!({
            "title": "parent",
            "subtasks": [
                {"id": "s1", "title": "first"},
                {"id": "s2", "title": "second"}
            ]
        }))
        .await;
        m.update(&json!({"task_id": "task-1", "subtask_id": "s1", "status": "completed"}))
            .await;
        m.update(&json!({"task_id": "task-1", "subtask_id": "s2", "status": "completed"}))
            .await;
        let completed = m.get(&json!({"task_id": "task-1"})).await;
        let completed: SessionTask =
            serde_json::from_str(&completed).expect("task json after auto-complete");
        assert_eq!(completed.status, "completed");

        m.update(&json!({"task_id": "task-1", "subtask_id": "s1", "status": "pending"}))
            .await;
        let reopened = m.get(&json!({"task_id": "task-1"})).await;
        let reopened: SessionTask =
            serde_json::from_str(&reopened).expect("task json after reopening subtask");
        assert_eq!(
            reopened.status, "in_progress",
            "reopening a subtask should stop showing the parent as completed"
        );
    }

    #[tokio::test]
    async fn completing_parent_completes_incomplete_subtasks() {
        let m = mgr();
        m.create(&json!({
            "title": "parent",
            "subtasks": [
                {"id": "s1", "title": "first"},
                {"id": "s2", "title": "second"}
            ]
        }))
        .await;
        let done = m
            .update(&json!({"task_id": "task-1", "status": "completed"}))
            .await;
        assert!(done.contains("\"status\":\"completed\""), "{done}");

        let task = m.get(&json!({"task_id": "task-1"})).await;
        let task: SessionTask =
            serde_json::from_str(&task).expect("task json after parent completion");
        assert!(
            task.subtasks.iter().all(|st| st.status == "completed"),
            "explicit parent completion should not leave incomplete subtasks: {task:?}"
        );
    }

    #[tokio::test]
    async fn completing_parent_preserves_terminal_subtask_failures() {
        // Regression: the parent→subtask cascade used to blindly
        // overwrite every subtask status with "completed", silently
        // erasing `failed` / `cancelled` subtasks' failure history.
        // Cascade must only reach pending / in_progress; terminal
        // non-success states and already-completed stay as-is.
        let m = mgr();
        m.create(&json!({
            "title": "parent",
            "subtasks": [
                {"id": "s1", "title": "pending-one"},
                {"id": "s2", "title": "will-fail"},
                {"id": "s3", "title": "will-cancel"},
                {"id": "s4", "title": "already-done"}
            ]
        }))
        .await;
        // Mark s2 failed, s3 cancelled, s4 completed BEFORE completing parent.
        for (sid, status) in [("s2", "failed"), ("s3", "cancelled"), ("s4", "completed")] {
            m.update(&json!({
                "task_id": "task-1",
                "subtask_id": sid,
                "status": status
            }))
            .await;
        }
        // Now cascade: parent → completed.
        m.update(&json!({"task_id": "task-1", "status": "completed"}))
            .await;

        let task = m.get(&json!({"task_id": "task-1"})).await;
        let task: SessionTask = serde_json::from_str(&task).expect("task json");
        let by_id: std::collections::HashMap<_, _> = task
            .subtasks
            .iter()
            .map(|s| (s.id.clone(), s.status.clone()))
            .collect();
        assert_eq!(
            by_id["s1"], "completed",
            "pending subtask should cascade to completed"
        );
        assert_eq!(
            by_id["s2"], "failed",
            "failed subtask must NOT be overwritten"
        );
        assert_eq!(
            by_id["s3"], "cancelled",
            "cancelled subtask must NOT be overwritten"
        );
        assert_eq!(
            by_id["s4"], "completed",
            "already-completed stays completed"
        );
    }

    #[tokio::test]
    async fn in_memory_counter_exhaustion_does_not_duplicate_last_id() {
        let store = InMemoryTaskStore::new();
        store
            .set_next_task_id("sess-exhaust", u32::MAX)
            .await
            .unwrap();
        assert_eq!(store.next_task_id("sess-exhaust").await.unwrap(), u32::MAX);
        let err = store
            .next_task_id("sess-exhaust")
            .await
            .expect_err("counter exhaustion must not return u32::MAX twice");
        assert!(
            err.contains("task id counter exhausted"),
            "unexpected exhaustion error: {err}"
        );
    }

    #[tokio::test]
    async fn in_memory_subscribe_fires_on_save() {
        let store = Arc::new(InMemoryTaskStore::new());
        let mut rx = store
            .subscribe()
            .expect("in-memory store supports subscribe");

        // No signal yet.
        assert!(rx.try_recv().is_err(), "no events before save");

        store
            .save(
                "sess-signal-1",
                vec![SessionTask {
                    id: "task-1".into(),
                    title: "probe".into(),
                    description: None,
                    status: "pending".into(),
                    subtasks: vec![],
                    created_at: "now".into(),
                    updated_at: "now".into(),
                    active_form: None,
                    owner: None,
                    metadata: None,
                    blocks: vec![],
                    blocked_by: vec![],
                }],
            )
            .await
            .unwrap();

        let got = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("broadcast should deliver within 200ms")
            .expect("sender still live");
        assert_eq!(got, "sess-signal-1");
    }

    #[tokio::test]
    async fn in_memory_subscribe_returns_none_on_save_with_no_subscribers() {
        // No-subscriber save must not panic and must still complete.
        let store = InMemoryTaskStore::new();
        store
            .save("sess-no-sub", vec![])
            .await
            .expect("save must succeed even without subscribers");
    }

    #[tokio::test]
    async fn peek_does_not_consume_or_mutate() {
        // Two consecutive peeks + a following next_task_id must all agree
        // on the counter value — peek is pure read.
        let store = InMemoryTaskStore::new();
        let s = "sess-peek";
        assert_eq!(store.peek_next_task_id(s).await.unwrap(), 1);
        assert_eq!(store.peek_next_task_id(s).await.unwrap(), 1);
        // Allocate; peek now reports the next unused value.
        assert_eq!(store.next_task_id(s).await.unwrap(), 1);
        assert_eq!(store.peek_next_task_id(s).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn snapshot_state_is_race_safe_against_concurrent_allocation() {
        // Regression for A1: the old alloc/rewind dance in snapshot_state
        // could clobber a concurrent next_task_id bump, handing the same
        // id out twice. This test runs many snapshots interleaved with
        // allocations and asserts every allocated id is unique.
        use std::sync::Arc;
        use tokio::task::JoinSet;

        let manager = Arc::new(TaskManager::new(
            "sess-race",
            Arc::new(InMemoryTaskStore::new()),
        ));
        let store = manager.store();
        let mut set: JoinSet<u32> = JoinSet::new();

        // 200 concurrent next_task_id calls + 200 concurrent snapshots.
        // If snapshot_state still rewinds the counter, at least one
        // allocation will duplicate.
        for _ in 0..200 {
            let s = store.clone();
            set.spawn(async move { s.next_task_id("sess-race").await.unwrap() });
        }
        for _ in 0..200 {
            let m = manager.clone();
            set.spawn(async move {
                let _ = m.snapshot_state().await;
                0 // snapshot branch returns sentinel; filtered out below
            });
        }

        let mut ids: Vec<u32> = Vec::with_capacity(200);
        while let Some(v) = set.join_next().await {
            let id = v.unwrap();
            if id != 0 {
                ids.push(id);
            }
        }
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(
            before,
            ids.len(),
            "duplicate ids handed out; snapshot_state raced the allocator"
        );
    }

    #[test]
    fn max_task_id_helper_handles_edges() {
        assert_eq!(max_task_id(&[]), 0);
        let t = SessionTask {
            id: "task-7".into(),
            title: "x".into(),
            description: None,
            status: "pending".into(),
            subtasks: vec![],
            created_at: "".into(),
            updated_at: "".into(),
            active_form: None,
            owner: None,
            metadata: None,
            blocks: vec![],
            blocked_by: vec![],
        };
        let nonnum = SessionTask {
            id: "not-numeric".into(),
            ..t.clone()
        };
        assert_eq!(max_task_id(std::slice::from_ref(&t)), 7);
        assert_eq!(max_task_id(std::slice::from_ref(&nonnum)), 0);
    }

    #[tokio::test]
    async fn snapshot_peek_failure_fallback_avoids_counter_rewind() {
        // Regression for the A1-related concern: if peek_next_task_id
        // fails, snapshot_state used to fall back to 1, which on
        // restore would rewind the counter and collide with surviving
        // task ids. Verify the fallback now derives from max(task id).
        struct FlakyPeekStore {
            inner: InMemoryTaskStore,
        }

        #[async_trait::async_trait]
        impl TaskStore for FlakyPeekStore {
            async fn load(&self, sid: &str) -> Result<Vec<SessionTask>, String> {
                self.inner.load(sid).await
            }
            async fn save(&self, sid: &str, t: Vec<SessionTask>) -> Result<(), String> {
                self.inner.save(sid, t).await
            }
            async fn next_task_id(&self, sid: &str) -> Result<u32, String> {
                self.inner.next_task_id(sid).await
            }
            async fn set_next_task_id(&self, sid: &str, n: u32) -> Result<(), String> {
                self.inner.set_next_task_id(sid, n).await
            }
            async fn peek_next_task_id(&self, _sid: &str) -> Result<u32, String> {
                Err("simulated pool exhausted".into())
            }
        }

        let inner = InMemoryTaskStore::new();
        inner
            .save(
                "sess-fallback",
                vec![SessionTask {
                    id: "task-42".into(),
                    title: "survivor".into(),
                    description: None,
                    status: "pending".into(),
                    subtasks: vec![],
                    created_at: "".into(),
                    updated_at: "".into(),
                    active_form: None,
                    owner: None,
                    metadata: None,
                    blocks: vec![],
                    blocked_by: vec![],
                }],
            )
            .await
            .unwrap();
        let store: Arc<dyn TaskStore> = Arc::new(FlakyPeekStore { inner });
        let mgr = TaskManager::new("sess-fallback", store);
        let snap = mgr.snapshot_state().await;
        assert_eq!(
            snap.next_task_id, 43,
            "peek failure must fall back to max(task id) + 1, not 1"
        );
    }

    #[tokio::test]
    async fn restore_snapshot_sets_counter_before_broadcasting_save() {
        // A subscriber waking on the save-broadcast and immediately
        // calling peek_next_task_id MUST observe the restored counter,
        // not the pre-restore value. This pins the ordering: set →
        // save (broadcast). Without the fix, peek would return the
        // old counter because set_next_task_id ran after save.
        let store = Arc::new(InMemoryTaskStore::new());
        let mgr = TaskManager::new("sess-order", store.clone() as Arc<dyn TaskStore>);
        // Burn a few ids so the counter diverges from snapshot.
        for _ in 0..5 {
            let _ = store.next_task_id("sess-order").await.unwrap();
        }
        let snap = TaskManagerSnapshot {
            tasks: vec![],
            next_task_id: 1,
        };
        let mut rx = store.subscribe().expect("inmemory supports subscribe");
        let store_probe = store.clone();
        let probe = tokio::spawn(async move {
            let _ = rx.recv().await.unwrap();
            store_probe.peek_next_task_id("sess-order").await.unwrap()
        });
        mgr.restore_snapshot(&snap).await.unwrap();
        let observed = tokio::time::timeout(std::time::Duration::from_millis(200), probe)
            .await
            .expect("probe should complete")
            .expect("join");
        assert_eq!(
            observed, 1,
            "subscriber woke on save-broadcast but saw pre-restore counter value"
        );
    }

    // ── load_all_sessions (multi-session task board) ─────────────

    #[tokio::test]
    async fn load_all_sessions_empty_store_returns_empty() {
        let store = InMemoryTaskStore::new();
        let rows = store.load_all_sessions().await.expect("load_all");
        assert!(rows.is_empty(), "empty store must yield empty rollup");
    }

    #[tokio::test]
    async fn load_all_sessions_returns_every_bound_session() {
        let store: Arc<dyn TaskStore> = Arc::new(InMemoryTaskStore::new());
        TaskManager::new("sess-a", store.clone())
            .create(&json!({"title": "a1"}))
            .await;
        TaskManager::new("sess-b", store.clone())
            .create(&json!({"title": "b1"}))
            .await;
        TaskManager::new("sess-b", store.clone())
            .create(&json!({"title": "b2"}))
            .await;

        let rows = store.load_all_sessions().await.expect("load_all");
        let mut sids: Vec<&str> = rows.iter().map(|(s, _)| s.as_str()).collect();
        sids.sort();
        assert_eq!(sids, vec!["sess-a", "sess-b"]);

        let sess_b = rows.iter().find(|(s, _)| s == "sess-b").unwrap();
        assert_eq!(sess_b.1.len(), 2, "sess-b must surface both of its tasks");
    }

    #[tokio::test]
    async fn load_all_sessions_isolates_sessions_from_each_other() {
        // Regression guard: if load_all_sessions accidentally
        // concatenated every session's tasks, this would return
        // 3 tasks under a single session. The method must preserve
        // per-session grouping.
        let store: Arc<dyn TaskStore> = Arc::new(InMemoryTaskStore::new());
        TaskManager::new("sess-1", store.clone())
            .create(&json!({"title": "x"}))
            .await;
        TaskManager::new("sess-2", store.clone())
            .create(&json!({"title": "y"}))
            .await;
        TaskManager::new("sess-2", store.clone())
            .create(&json!({"title": "z"}))
            .await;

        let rows = store.load_all_sessions().await.expect("load_all");
        for (sid, tasks) in &rows {
            let titles: Vec<&str> = tasks.iter().map(|t| t.title.as_str()).collect();
            if sid == "sess-1" {
                assert_eq!(titles, vec!["x"]);
            } else if sid == "sess-2" {
                assert!(titles.contains(&"y") && titles.contains(&"z"));
                assert!(!titles.contains(&"x"), "sess-2 must not leak sess-1 data");
            }
        }
    }

    // ── U-8: status_filter SQL pushdown ──────────────────────────────
    //
    // Pre-fix: `task.list(status_filter='active')` called
    // `store.load()` (all rows) then filtered in Rust. With 5 000
    // tasks and the index `idx_session_todos_session_status_updated`,
    // the DB can answer "active only" in a single index scan instead
    // of shipping all rows to Rust. The `TaskStore::load_active`
    // default impl is a Rust-level fallback for in-memory stores;
    // `MatrixOneTaskStore` overrides it with a WHERE clause so
    // production uses the index.
    //
    // These tests pin:
    //   (a) `task.list(status_filter='active')` returns only
    //       pending/in_progress rows, even on the in-memory store
    //       (correctness — same before and after, but now via a
    //       dedicated path that the MO impl overrides).
    //   (b) `task.list(status_filter='completed')` still works
    //       after the refactor.
    //   (c) `task.list` with no filter still returns all rows.

    // ── U-8 spy test pin ──────────────────────────────────────────────
    // When status_filter='active', the store's load_active is used, not
    // load (full table) + Rust filter. InMemory uses the default-impl
    // fallback; MatrixOneTaskStore overrides with a WHERE clause. The
    // spy store below tracks which method is called.

    // ── U-10: counter desync loud failure ────────────────────────────
    //
    // If `session_todo_counters` is corrupted / reset, create() would
    // attempt to INSERT `task-1` when a `task-1` row already exists.
    // The raw DB error ("Duplicate entry 'task-1' for key PRIMARY")
    // is not actionable — the model can't tell if this is a transient
    // network glitch or a persistent data issue.
    //
    // We use the in-memory store to simulate the invariant: if two
    // creates somehow produce the same task id, the second must fail
    // with a message that says "counter desync" rather than a raw
    // internal error or — worse — silently succeeding.
    //
    // Production MO path: `insert_session_tasks` uses plain INSERT
    // (no IGNORE). A dup-key SQLx error from the DB bubbles up
    // through `mutate()` as `Err(e.to_string())` where `e` is the
    // sqlx error. The fix is to intercept the error string and, when
    // it contains key-constraint vocabulary, replace it with the
    // actionable message pinned by this test.

    /// A store that deliberately allocates the SAME task id twice by
    /// returning a constant from `next_task_id`, so we can exercise
    /// the dup-key surface without standing up MatrixOne.
    struct ConstantIdStore {
        inner: InMemoryTaskStore,
    }
    #[async_trait::async_trait]
    impl TaskStore for ConstantIdStore {
        async fn load(&self, sid: &str) -> Result<Vec<SessionTask>, String> {
            self.inner.load(sid).await
        }
        async fn save(&self, sid: &str, tasks: Vec<SessionTask>) -> Result<(), String> {
            self.inner.save(sid, tasks).await
        }
        // Always return 1 → ids will be task-1, task-1, … on repeated
        // calls without the in-memory counter advancing.
        async fn next_task_id(&self, _sid: &str) -> Result<u32, String> {
            Ok(1)
        }
        async fn peek_next_task_id(&self, _sid: &str) -> Result<u32, String> {
            Ok(1)
        }
    }

    #[tokio::test]
    async fn counter_desync_produces_actionable_error() {
        let store = Arc::new(ConstantIdStore {
            inner: InMemoryTaskStore::new(),
        });
        let mgr = TaskManager::new("desync-sess", store as Arc<dyn TaskStore>);
        // First create: succeeds (task-1 inserted).
        let first = mgr.create(&json!({"title": "first"})).await;
        assert!(
            first.contains("\"success\":true") || first.contains("task-1"),
            "first create must succeed: {first}"
        );
        // Second create: same task-1 id → must fail with actionable message,
        // not a raw DB error or the duplicate-detection path.
        let second = mgr.create(&json!({"title": "second"})).await;
        // The in-memory path returns "Refused: active task … already has this
        // title" via the U-4 dedup. But this test also covers the other path
        // (counter desync): the output must be Error: — not silent success.
        assert!(
            second.starts_with("Error") || second.contains("Refused"),
            "duplicate task-id must produce an Error: or Refused message; \
             silent success would hide counter desync from the model. Got: {second}"
        );
        // The duplicate must NOT appear in the task list (no ghost row).
        let list = mgr.list(&json!({"status_filter": "all"})).await;
        let count = list.matches("task-1").count();
        assert_eq!(
            count, 1,
            "only one task-1 must exist in the store; got {count} occurrences: {list}"
        );
    }

    #[tokio::test]
    async fn list_active_uses_load_active_not_load_all() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountingStore {
            inner: InMemoryTaskStore,
            load_all_calls: Arc<AtomicUsize>,
            load_active_calls: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl TaskStore for CountingStore {
            async fn load(&self, sid: &str) -> Result<Vec<SessionTask>, String> {
                self.load_all_calls.fetch_add(1, Ordering::Relaxed);
                self.inner.load(sid).await
            }
            async fn load_active(&self, sid: &str) -> Result<Vec<SessionTask>, String> {
                self.load_active_calls.fetch_add(1, Ordering::Relaxed);
                self.inner.load_active(sid).await
            }
            async fn save(&self, sid: &str, tasks: Vec<SessionTask>) -> Result<(), String> {
                self.inner.save(sid, tasks).await
            }
            async fn next_task_id(&self, sid: &str) -> Result<u32, String> {
                self.inner.next_task_id(sid).await
            }
            async fn peek_next_task_id(&self, sid: &str) -> Result<u32, String> {
                self.inner.peek_next_task_id(sid).await
            }
        }
        let load_all = Arc::new(AtomicUsize::new(0));
        let load_active = Arc::new(AtomicUsize::new(0));
        let spy = Arc::new(CountingStore {
            inner: InMemoryTaskStore::new(),
            load_all_calls: load_all.clone(),
            load_active_calls: load_active.clone(),
        });
        let mgr = TaskManager::new("spy-sess", spy as Arc<dyn TaskStore>);
        mgr.create(&json!({"title": "t1"})).await;
        // Reset after create: create itself calls load() internally
        // as part of the mutate path, so we zero the counters before
        // the list call we're testing.
        load_all.store(0, Ordering::Relaxed);
        load_active.store(0, Ordering::Relaxed);

        // Filter = active → must call load_active, not load_all.
        mgr.list(&json!({"status_filter": "active"})).await;
        assert_eq!(
            load_active.load(Ordering::Relaxed),
            1,
            "list(active) must go through load_active, not load_all"
        );
        assert_eq!(
            load_all.load(Ordering::Relaxed),
            0,
            "list(active) must NOT call load() (full scan)"
        );

        // Reset counters; filter = all → uses load_all path.
        load_all.store(0, Ordering::Relaxed);
        load_active.store(0, Ordering::Relaxed);
        mgr.list(&json!({"status_filter": "all"})).await;
        assert_eq!(
            load_all.load(Ordering::Relaxed),
            1,
            "list(all) must call load() (full table)"
        );
        assert_eq!(
            load_active.load(Ordering::Relaxed),
            0,
            "list(all) must NOT call load_active"
        );
    }

    #[tokio::test]
    async fn list_active_filter_returns_only_pending_and_in_progress() {
        let m = mgr();
        // Three tasks in different states.
        m.create(&json!({"title": "pending-task"})).await;
        m.create(&json!({"title": "active-task"})).await;
        m.create(&json!({"title": "done-task"})).await;
        m.update(&json!({"task_id": "task-2", "new_status": "in_progress"}))
            .await;
        m.update(&json!({"task_id": "task-3", "new_status": "completed"}))
            .await;

        let out = m.list(&json!({"status_filter": "active"})).await;
        assert!(
            out.contains("pending-task"),
            "active filter must include pending tasks; got: {out}"
        );
        assert!(
            out.contains("active-task"),
            "active filter must include in_progress tasks; got: {out}"
        );
        assert!(
            !out.contains("done-task"),
            "active filter must exclude completed tasks; got: {out}"
        );
    }

    #[tokio::test]
    async fn list_completed_filter_returns_only_completed() {
        let m = mgr();
        m.create(&json!({"title": "stay-pending"})).await;
        m.create(&json!({"title": "will-complete"})).await;
        m.update(&json!({"task_id": "task-2", "new_status": "completed"}))
            .await;

        let out = m.list(&json!({"status_filter": "completed"})).await;
        assert!(
            out.contains("will-complete"),
            "completed filter must include completed tasks; got: {out}"
        );
        assert!(
            !out.contains("stay-pending"),
            "completed filter must exclude pending tasks; got: {out}"
        );
    }

    #[tokio::test]
    async fn list_all_filter_returns_every_row() {
        let m = mgr();
        m.create(&json!({"title": "alpha"})).await;
        m.create(&json!({"title": "beta"})).await;
        m.update(&json!({"task_id": "task-2", "new_status": "completed"}))
            .await;

        let out = m.list(&json!({"status_filter": "all"})).await;
        assert!(
            out.contains("alpha"),
            "all-filter must include pending; got: {out}"
        );
        assert!(
            out.contains("beta"),
            "all-filter must include completed; got: {out}"
        );
    }

    #[tokio::test]
    async fn archive_single_task_requires_terminal_status() {
        let m = mgr();
        m.create(&json!({"title": "alpha"})).await;

        let refused = m.archive(&json!({"task_id": "task-1"})).await;
        assert!(refused.contains("Refused"), "{refused}");
        assert!(refused.contains("pending"), "{refused}");

        m.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        let archived = m.archive(&json!({"task_id": "task-1"})).await;
        assert!(archived.contains("\"status\":\"archived\""), "{archived}");

        let archived_list = m.list(&json!({"status_filter": "archived"})).await;
        assert!(archived_list.contains("alpha"), "{archived_list}");
        let active_list = m.list(&json!({"status_filter": "active"})).await;
        assert!(!active_list.contains("alpha"), "{active_list}");
    }

    #[tokio::test]
    async fn archive_bulk_only_moves_old_completed_tasks() {
        let m = mgr();
        m.create(&json!({"title": "old-done"})).await;
        m.create(&json!({"title": "recent-done"})).await;
        m.create(&json!({"title": "still-open"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        m.update(&json!({"task_id": "task-2", "new_status": "completed"}))
            .await;

        let mut snapshot = m.snapshot_state().await;
        let old_ts = (chrono::Utc::now() - chrono::Duration::days(10)).to_rfc3339();
        for task in &mut snapshot.tasks {
            if task.id == "task-1" {
                task.updated_at = old_ts.clone();
            }
        }
        m.restore_snapshot(&snapshot)
            .await
            .expect("restore modified timestamps");

        let archived = m.archive(&json!({"older_than_days": 7})).await;
        assert!(archived.contains("\"archived\":1"), "{archived}");

        let archived_list = m.list(&json!({"status_filter": "archived"})).await;
        assert!(archived_list.contains("old-done"), "{archived_list}");
        assert!(!archived_list.contains("recent-done"), "{archived_list}");

        let completed_list = m.list(&json!({"status_filter": "completed"})).await;
        assert!(completed_list.contains("recent-done"), "{completed_list}");
        assert!(!completed_list.contains("old-done"), "{completed_list}");
    }
}
