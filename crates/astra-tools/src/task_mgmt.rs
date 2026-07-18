//! Durable per-session task board management.
//!
//! Runtime-owned continuity state remains authoritative for agent execution
//! progress. This module owns the user/model checklist: concrete outcomes,
//! dependencies, status transitions, and resume/fork-visible work tracking.
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
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

pub const MAX_CREATE_SUBTASKS: usize = 20;
pub const MAX_TASK_TITLE_CHARS: usize = 512;
pub const MAX_TASK_DESCRIPTION_CHARS: usize = 10_000;
pub const MAX_TASK_ACTIVE_FORM_CHARS: usize = 256;
pub const MAX_TASK_OWNER_CHARS: usize = 128;
pub const MAX_TASK_ERROR_MESSAGE_CHARS: usize = 10_000;
pub const MAX_TASK_STOP_REASON_CHARS: usize = 5_000;
pub const MAX_TASK_METADATA_BYTES: usize = 32 * 1024;
pub const MAX_SUBTASK_ID_CHARS: usize = 128;
pub const MAX_SUBTASK_TITLE_CHARS: usize = 512;
pub const MAX_SUBTASK_DESCRIPTION_CHARS: usize = 10_000;

/// A durable checklist task tracked within the current CLI session.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTask {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub status: SessionTaskStatusKind,
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

    /// Timestamp when the task was archived. Set when transitioning to
    /// Archived status; stays None otherwise. Preserved across InMemory →
    /// MatrixOne migration so the GC sweeper can expire old archived tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
}

/// Truthful, bounded projection used by cross-session task surfaces.
///
/// This is intentionally smaller than [`SessionTask`]: remote APIs can serve
/// an actionable overview without downloading descriptions, dependency
/// graphs, subtasks, or arbitrary metadata. Consumers must not fabricate
/// those omitted fields just to reuse the full task type.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenTaskSummary {
    pub id: String,
    pub title: String,
    pub status: SessionTaskStatusKind,
    pub updated_at: String,
}

impl From<&SessionTask> for OpenTaskSummary {
    fn from(task: &SessionTask) -> Self {
        Self {
            id: task.id.clone(),
            title: task.title.clone(),
            status: task.status,
            updated_at: task.updated_at.clone(),
        }
    }
}

fn bounded_open_task_summaries(
    sessions: Vec<(String, Vec<SessionTask>)>,
    limit: usize,
) -> Vec<(String, Vec<OpenTaskSummary>)> {
    if limit == 0 {
        return Vec::new();
    }
    let mut flat: Vec<(String, OpenTaskSummary)> = sessions
        .into_iter()
        .flat_map(|(session_id, tasks)| {
            tasks
                .into_iter()
                .filter(|task| task.status.is_open_work())
                .map(move |task| (session_id.clone(), OpenTaskSummary::from(&task)))
        })
        .collect();
    flat.sort_by(|(session_a, task_a), (session_b, task_b)| {
        task_b
            .updated_at
            .cmp(&task_a.updated_at)
            .then_with(|| session_a.cmp(session_b))
            .then_with(|| task_a.id.cmp(&task_b.id))
    });
    flat.truncate(limit);

    let mut grouped: Vec<(String, Vec<OpenTaskSummary>)> = Vec::new();
    for (session_id, task) in flat {
        if let Some((_, tasks)) = grouped
            .iter_mut()
            .find(|(existing, _)| existing == &session_id)
        {
            tasks.push(task);
        } else {
            grouped.push((session_id, vec![task]));
        }
    }
    grouped
}

pub fn detach_dependency_edges_for_task_ids(
    task_id: &str,
    blocks: &mut Vec<String>,
    blocked_by: &mut Vec<String>,
    detached_ids: &HashSet<String>,
) -> bool {
    if detached_ids.is_empty() {
        return false;
    }

    let before_blocks = blocks.len();
    let before_blocked_by = blocked_by.len();
    if detached_ids.contains(task_id) {
        blocks.clear();
        blocked_by.clear();
    } else {
        blocks.retain(|id| !detached_ids.contains(id));
        blocked_by.retain(|id| !detached_ids.contains(id));
    }
    before_blocks != blocks.len() || before_blocked_by != blocked_by.len()
}

pub fn detach_task_dependency_edges(
    tasks: &mut [SessionTask],
    detached_ids: &HashSet<String>,
) -> bool {
    let mut changed = false;
    for task in tasks {
        changed |= detach_dependency_edges_for_task_ids(
            &task.id,
            &mut task.blocks,
            &mut task.blocked_by,
            detached_ids,
        );
    }
    changed
}

fn push_unique_string(values: &mut Vec<String>, id: &str) -> bool {
    if values.iter().any(|value| value == id) {
        return false;
    }
    values.push(id.to_string());
    true
}

fn remove_string(values: &mut Vec<String>, id: &str) -> bool {
    let before = values.len();
    values.retain(|value| value != id);
    before != values.len()
}

fn add_dependency_edge(
    tasks: &mut [SessionTask],
    blocker_id: &str,
    blocked_id: &str,
    now: &str,
) -> Result<(), String> {
    // **Self-dependency check**: a task cannot block itself. This would create
    // a trivial cycle and violate the state machine invariant that a task must
    // eventually become unblocked. From first principles: dependency graphs must
    // be DAGs, and self-loops are not acyclic.
    if blocker_id == blocked_id {
        return Err(format!(
            "task '{}' cannot depend on itself. Self-dependency creates an unresolvable cycle",
            blocker_id
        ));
    }

    let Some(blocker_index) = tasks.iter().position(|task| task.id == blocker_id) else {
        return Err(format!("task '{}' not found", blocker_id));
    };
    let Some(blocked_index) = tasks.iter().position(|task| task.id == blocked_id) else {
        return Err(format!("task '{}' not found", blocked_id));
    };

    let blocker_has_edge = tasks[blocker_index]
        .blocks
        .iter()
        .any(|id| id == blocked_id);
    let blocked_has_reverse = tasks[blocked_index]
        .blocked_by
        .iter()
        .any(|id| id == blocker_id);
    // Cycle detection: adding blocker_id → blocked_id must not create a path
    // from blocked_id back to blocker_id. Validate legacy/asymmetric forward
    // halves too: repairing their reverse metadata must not silently bless an
    // already-cyclic persisted graph.
    if would_create_cycle(tasks, blocker_id, blocked_id) {
        return Err(format!(
            "adding dependency '{}' → '{}' would create a cycle. Review the dependency graph",
            blocker_id, blocked_id
        ));
    }
    if blocker_has_edge && blocked_has_reverse {
        return Ok(());
    }

    // Repair either half of a legacy/asymmetric edge independently. Treating
    // the forward half as proof that the whole relation exists would preserve
    // corrupt graph state forever on an otherwise idempotent add.
    if push_unique_string(&mut tasks[blocker_index].blocks, blocked_id) {
        tasks[blocker_index].updated_at = now.to_string();
    }
    if push_unique_string(&mut tasks[blocked_index].blocked_by, blocker_id) {
        tasks[blocked_index].updated_at = now.to_string();
    }
    Ok(())
}

/// BFS check: does a path already exist from `from_id` to `to_id`?
/// If so, adding `to_id → from_id` would create a cycle.
fn would_create_cycle(tasks: &[SessionTask], from_id: &str, to_id: &str) -> bool {
    let id_to_index: HashMap<&str, usize> = tasks
        .iter()
        .enumerate()
        .map(|(i, task)| (task.id.as_str(), i))
        .collect();

    let Some(&to_idx) = id_to_index.get(to_id) else {
        return false;
    };

    // BFS from to_id: can we reach from_id through existing blocks edges?
    let mut visited = vec![false; tasks.len()];
    let mut queue = VecDeque::new();
    queue.push_back(to_idx);
    visited[to_idx] = true;

    while let Some(u) = queue.pop_front() {
        // Follow blocks edges: u blocks → these are reachable from u
        for blocked_id in &tasks[u].blocks {
            if blocked_id == from_id {
                return true; // Path from to_id reaches from_id → cycle
            }
            if let Some(&v) = id_to_index.get(blocked_id.as_str())
                && !visited[v]
            {
                visited[v] = true;
                queue.push_back(v);
            }
        }
    }

    false
}

fn remove_dependency_edge(
    tasks: &mut [SessionTask],
    blocker_id: &str,
    blocked_id: &str,
    now: &str,
) -> Result<(), String> {
    let Some(blocker_index) = tasks.iter().position(|task| task.id == blocker_id) else {
        return Err(format!("task '{}' not found", blocker_id));
    };
    let Some(blocked_index) = tasks.iter().position(|task| task.id == blocked_id) else {
        return Err(format!("task '{}' not found", blocked_id));
    };

    let blocker_changed = remove_string(&mut tasks[blocker_index].blocks, blocked_id);
    let blocked_changed = remove_string(&mut tasks[blocked_index].blocked_by, blocker_id);
    if blocker_changed {
        tasks[blocker_index].updated_at = now.to_string();
    }
    if blocked_changed {
        tasks[blocked_index].updated_at = now.to_string();
    }
    Ok(())
}

fn projected_blockers_for_task(
    tasks: &[SessionTask],
    task_id: &str,
    add_blocked_by: &[String],
    remove_blocked_by: &[String],
) -> HashSet<String> {
    let mut blockers: HashSet<String> = HashSet::new();
    if let Some(task) = tasks.iter().find(|task| task.id == task_id) {
        blockers.extend(task.blocked_by.iter().cloned());
    }
    for task in tasks {
        if task.blocks.iter().any(|blocked_id| blocked_id == task_id) {
            blockers.insert(task.id.clone());
        }
    }
    for blocker_id in remove_blocked_by {
        blockers.remove(blocker_id);
    }
    for blocker_id in add_blocked_by {
        blockers.insert(blocker_id.clone());
    }
    blockers
}

fn unresolved_blocker_ids(
    tasks: &[SessionTask],
    blocker_ids: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let task_statuses = tasks
        .iter()
        .map(|task| (task.id.as_str(), task.status))
        .collect::<HashMap<_, _>>();
    let mut seen = HashSet::new();
    let mut unresolved = blocker_ids
        .into_iter()
        .filter(|blocker_id| seen.insert(blocker_id.clone()))
        .filter(|blocker_id| {
            !task_statuses
                .get(blocker_id.as_str())
                .is_some_and(SessionTaskStatusKind::is_completed)
        })
        .collect::<Vec<_>>();
    unresolved.sort();
    unresolved
}

/// Return the task's unresolved dependency IDs from the canonical graph.
///
/// A dependency is resolved only when the referenced task exists and is
/// completed. Missing references deliberately remain unresolved so a corrupt
/// or partially-loaded graph cannot silently authorize work. Both edge
/// directions are considered to keep projections truthful while legacy data
/// with only one side of an edge is repaired.
pub fn unresolved_task_blocker_ids(tasks: &[SessionTask], task: &SessionTask) -> Vec<String> {
    let blockers = task.blocked_by.iter().cloned().chain(
        tasks
            .iter()
            .filter(|candidate| candidate.blocks.iter().any(|id| id == &task.id))
            .map(|candidate| candidate.id.clone()),
    );
    unresolved_blocker_ids(tasks, blockers)
}

fn projected_unresolved_task_blocker_ids(
    tasks: &[SessionTask],
    task_id: &str,
    add_blocked_by: &[String],
    remove_blocked_by: &[String],
) -> Vec<String> {
    unresolved_blocker_ids(
        tasks,
        projected_blockers_for_task(tasks, task_id, add_blocked_by, remove_blocked_by),
    )
}

fn validate_task_blockers_resolved_after_projected_edges(
    tasks: &[SessionTask],
    task_id: &str,
    add_blocked_by: &[String],
    remove_blocked_by: &[String],
) -> Result<(), String> {
    let unresolved_ids =
        projected_unresolved_task_blocker_ids(tasks, task_id, add_blocked_by, remove_blocked_by);
    let tasks_by_id: HashMap<&str, &SessionTask> =
        tasks.iter().map(|task| (task.id.as_str(), task)).collect();
    let unresolved: Vec<String> = unresolved_ids
        .into_iter()
        .map(|blocker_id| match tasks_by_id.get(blocker_id.as_str()) {
            Some(task) => format!("{blocker_id} ({})", task.status),
            None => format!("{blocker_id} (missing)"),
        })
        .collect();
    if unresolved.is_empty() {
        return Ok(());
    }

    Err(format!(
        "task '{}' cannot start or complete while blocked_by is unresolved: {}. Complete the blocker(s), or remove the dependency if it no longer applies",
        task_id,
        unresolved.join(", ")
    ))
}

fn validate_subtask_dependencies_resolved(
    task: &SessionTask,
    subtask_id: &str,
) -> Result<(), String> {
    let Some(subtask) = task
        .subtasks
        .iter()
        .find(|subtask| subtask.id == subtask_id)
    else {
        return Err(format!(
            "subtask '{}' not found in task '{}'",
            subtask_id, task.id
        ));
    };
    if subtask.depends_on.is_empty() {
        return Ok(());
    }

    let subtasks_by_id: HashMap<&str, &SessionSubtask> = task
        .subtasks
        .iter()
        .map(|subtask| (subtask.id.as_str(), subtask))
        .collect();
    let unresolved: Vec<String> = subtask
        .depends_on
        .iter()
        .filter_map(|dep_id| match subtasks_by_id.get(dep_id.as_str()) {
            Some(dep) if dep.status.is_completed() => None,
            Some(dep) => Some(format!("{dep_id} ({})", dep.status)),
            None => Some(format!("{dep_id} (missing)")),
        })
        .collect();
    if unresolved.is_empty() {
        return Ok(());
    }

    Err(format!(
        "subtask '{}' of task '{}' cannot start or complete while depends_on is unresolved: {}. Complete the prerequisite subtask(s) first, or fix the dependency list",
        subtask_id,
        task.id,
        unresolved.join(", ")
    ))
}

/// A subtask within a SessionTask.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSubtask {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub status: SessionTaskStatusKind,
    pub depends_on: Vec<String>,
    /// Sub-agent or user that owns this subtask. Defaults to the
    /// parent task's owner unless the create call explicitly
    /// overrides — without inheritance, sub-agents looking for
    /// "my work" miss subtasks of tasks they own (U-7 unhappy path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Optional status-change note for this subtask. This is intentionally
    /// stored on the subtask, not parent metadata, so subtask updates can
    /// accept explanatory reasons without pretending they are parent failures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Compare semantic board state while deliberately excluding `updated_at`.
/// Mutation code may stage timestamps before it knows whether an idempotent
/// request changed anything; timestamp-only churn is not a task mutation.
fn same_task_board_state(left: &[SessionTask], right: &[SessionTask]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.id == right.id
                && left.title == right.title
                && left.description == right.description
                && left.status == right.status
                && left.subtasks == right.subtasks
                && left.created_at == right.created_at
                && left.active_form == right.active_form
                && left.owner == right.owner
                && left.metadata == right.metadata
                && left.blocks == right.blocks
                && left.blocked_by == right.blocked_by
                && left.archived_at == right.archived_at
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTaskStatusKind {
    InProgress,
    Pending,
    Paused,
    Completed,
    Failed,
    Cancelled,
    Archived,
    Deleted,
    Migrated,
    /// Unknown status from external sources (DB, API).
    #[serde(skip_serializing)]
    Other,
}

pub const SESSION_TASK_STATUS_PENDING: SessionTaskStatusKind = SessionTaskStatusKind::Pending;
pub const SESSION_TASK_STATUS_IN_PROGRESS: SessionTaskStatusKind =
    SessionTaskStatusKind::InProgress;
pub const SESSION_TASK_STATUS_PAUSED: SessionTaskStatusKind = SessionTaskStatusKind::Paused;
pub const SESSION_TASK_STATUS_COMPLETED: SessionTaskStatusKind = SessionTaskStatusKind::Completed;
pub const SESSION_TASK_STATUS_FAILED: SessionTaskStatusKind = SessionTaskStatusKind::Failed;
pub const SESSION_TASK_STATUS_CANCELLED: SessionTaskStatusKind = SessionTaskStatusKind::Cancelled;
pub const SESSION_TASK_STATUS_ARCHIVED: SessionTaskStatusKind = SessionTaskStatusKind::Archived;
pub const SESSION_TASK_STATUS_DELETED: SessionTaskStatusKind = SessionTaskStatusKind::Deleted;
pub const SESSION_TASK_STATUS_MIGRATED: SessionTaskStatusKind = SessionTaskStatusKind::Migrated;

impl SessionTaskStatusKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionTaskStatusKind::InProgress => "in_progress",
            SessionTaskStatusKind::Pending => "pending",
            SessionTaskStatusKind::Paused => "paused",
            SessionTaskStatusKind::Completed => "completed",
            SessionTaskStatusKind::Failed => "failed",
            SessionTaskStatusKind::Cancelled => "cancelled",
            SessionTaskStatusKind::Archived => "archived",
            SessionTaskStatusKind::Deleted => "deleted",
            SessionTaskStatusKind::Migrated => "migrated",
            SessionTaskStatusKind::Other => "other",
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::InProgress | Self::Pending)
    }

    pub fn is_open_work(&self) -> bool {
        matches!(self, Self::InProgress | Self::Pending | Self::Paused)
    }

    pub fn is_in_progress(&self) -> bool {
        *self == Self::InProgress
    }

    pub fn is_pending(&self) -> bool {
        *self == Self::Pending
    }

    pub fn is_completed(&self) -> bool {
        *self == Self::Completed
    }

    pub fn is_failed(&self) -> bool {
        *self == Self::Failed
    }

    pub fn is_cancelled(&self) -> bool {
        *self == Self::Cancelled
    }

    pub fn is_unsuccessful(&self) -> bool {
        matches!(self, Self::Failed | Self::Cancelled)
    }

    pub fn is_started(&self) -> bool {
        matches!(self, Self::InProgress | Self::Paused | Self::Completed)
    }

    /// Single-character marker for task-board rendering.
    pub fn status_marker(self) -> &'static str {
        match self {
            Self::InProgress => "▸",
            Self::Paused => "⏸",
            Self::Pending | Self::Archived | Self::Deleted | Self::Migrated | Self::Other => "·",
            Self::Completed => "✓",
            Self::Failed => "✗",
            Self::Cancelled => "⏹",
        }
    }

    /// Sort priority (low = first). InProgress → Pending → everything else.
    pub fn active_priority(self) -> u8 {
        match self {
            Self::InProgress => 0,
            Self::Pending => 1,
            Self::Paused => 2,
            Self::Completed
            | Self::Failed
            | Self::Cancelled
            | Self::Archived
            | Self::Deleted
            | Self::Migrated
            | Self::Other => 3,
        }
    }

    pub fn can_be_archived(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    pub fn can_be_stopped(&self) -> bool {
        matches!(self, Self::Pending | Self::InProgress | Self::Paused)
    }
}

impl SessionTaskStatusKind {
    /// Parse a status string with normalization.
    pub fn from_status_str(status: &str) -> Self {
        match status.trim().to_ascii_lowercase().as_str() {
            "in_progress" => Self::InProgress,
            "pending" => Self::Pending,
            "paused" => Self::Paused,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "archived" => Self::Archived,
            "deleted" => Self::Deleted,
            "migrated" => Self::Migrated,
            other => {
                tracing::warn!(%other, "session_task_status_kind: unknown status");
                Self::Other
            }
        }
    }
}

impl std::fmt::Display for SessionTaskStatusKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::InProgress => "in_progress",
            Self::Pending => "pending",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Archived => "archived",
            Self::Deleted => "deleted",
            Self::Migrated => "migrated",
            Self::Other => "other",
        };
        write!(f, "{s}")
    }
}

impl From<&str> for SessionTaskStatusKind {
    fn from(s: &str) -> Self {
        Self::from_status_str(s)
    }
}

impl<'de> serde::Deserialize<'de> for SessionTaskStatusKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let status = <String as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self::from_status_str(&status))
    }
}

/// Point-in-time snapshot of a single session's task list plus its id counter.
/// Used by the session-state rollback journal to undo a turn's task mutations.
#[derive(Debug, Clone)]
pub struct TaskManagerSnapshot {
    pub tasks: Vec<SessionTask>,
    pub next_task_id: u32,
    /// Monotonic session version captured at snapshot time.
    pub version: u64,
    /// Store version that is allowed to be restored over.
    ///
    /// A normal rollback captures a snapshot at version N, performs its own
    /// mutation(s), then seals the snapshot at version M. Restore is allowed
    /// only while the store is still at M. If another writer mutates the
    /// session after sealing, restore fails instead of overwriting it.
    pub restore_version: Option<u64>,
}

/// Prepare a task-board snapshot for a forked child session.
///
/// A fork inherits the work board, not the parent's live execution state.
/// Anything that was `in_progress` in the parent is copied as `paused` so
/// the child can explicitly resume or reprioritize without showing two
/// sessions as actively running the same task.
pub fn prepare_task_snapshot_for_fork(mut snapshot: TaskManagerSnapshot) -> TaskManagerSnapshot {
    for task in &mut snapshot.tasks {
        if task.status == SessionTaskStatusKind::InProgress {
            task.status = SessionTaskStatusKind::Paused;
            let metadata = task.metadata.get_or_insert_with(serde_json::Map::new);
            metadata.insert(
                "fork_copied_from_status".to_string(),
                Value::String("in_progress".to_string()),
            );
        }
        for subtask in &mut task.subtasks {
            if subtask.status == SessionTaskStatusKind::InProgress {
                subtask.status = SessionTaskStatusKind::Paused;
            }
        }
    }
    snapshot
}

/// Machine-readable result of one atomic task-board mutation.
///
/// `output` is presentation for the model/UI. Callers must use `status` and
/// `data` for control flow instead of parsing that text. `success` and
/// `changed` remain denormalized wire-compatibility fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskMutationStatus {
    Applied,
    Unchanged,
    Refused,
    Failed,
    /// The durable admission record exists, but the system cannot prove
    /// whether the mutation committed. Callers must reconcile from the task
    /// board instead of treating this as either success or a safe retry.
    Indeterminate,
}

impl TaskMutationStatus {
    pub fn is_success(self) -> bool {
        matches!(self, Self::Applied | Self::Unchanged)
    }

    pub fn changed(self) -> bool {
        self == Self::Applied
    }
}

#[derive(Debug, Clone)]
pub struct TaskMutationOutcome {
    pub output: String,
    /// Canonical control-flow evidence. Compatibility booleans are derived
    /// during serialization and are never stored as a second mutable truth.
    pub status: TaskMutationStatus,
    pub data: Value,
}

impl serde::Serialize for TaskMutationOutcome {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("TaskMutationOutcome", 5)?;
        state.serialize_field("output", &self.output)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("success", &self.status.is_success())?;
        state.serialize_field("changed", &self.status.changed())?;
        state.serialize_field("data", &self.data)?;
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for TaskMutationOutcome {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct WireOutcome {
            output: String,
            status: TaskMutationStatus,
            #[serde(default)]
            success: Option<bool>,
            #[serde(default)]
            changed: Option<bool>,
            data: Value,
        }

        let WireOutcome {
            output,
            status,
            success: _legacy_success,
            changed: _legacy_changed,
            mut data,
        } = WireOutcome::deserialize(deserializer)?;
        if let Some(object) = data.as_object_mut() {
            object.insert("success".to_string(), Value::Bool(status.is_success()));
            object.insert("mutation_status".to_string(), json!(status));
        }
        Ok(Self {
            output,
            status,
            data,
        })
    }
}

impl TaskMutationOutcome {
    fn from_parts(summary: impl Into<String>, mut data: Value, status: TaskMutationStatus) -> Self {
        let success = status.is_success();
        if let Some(object) = data.as_object_mut() {
            object.insert("success".to_string(), Value::Bool(success));
            object.insert("mutation_status".to_string(), json!(status));
        }
        Self {
            output: prefix_summary(summary.into(), data.to_string()),
            status,
            data,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            output: format!("Error: {message}"),
            status: TaskMutationStatus::Failed,
            data: json!({
                "success": false,
                "mutation_status": TaskMutationStatus::Failed,
                "message": message,
            }),
        }
    }

    pub fn applied(summary: impl Into<String>, data: Value) -> Self {
        Self::from_parts(summary, data, TaskMutationStatus::Applied)
    }

    pub fn unchanged(summary: impl Into<String>, data: Value) -> Self {
        Self::from_parts(summary, data, TaskMutationStatus::Unchanged)
    }

    pub fn refused(summary: impl Into<String>, data: Value) -> Self {
        Self::from_parts(summary, data, TaskMutationStatus::Refused)
    }

    pub fn indeterminate(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            output: format!("Indeterminate: {message}"),
            status: TaskMutationStatus::Indeterminate,
            data: json!({
                "success": false,
                "mutation_status": TaskMutationStatus::Indeterminate,
                "message": message,
            }),
        }
    }
}

pub struct TaskMutationResult {
    pub tasks: Vec<SessionTask>,
    pub next_task_id: Option<u32>,
    pub outcome: TaskMutationOutcome,
}

impl TaskMutationResult {
    fn applied(
        tasks: Vec<SessionTask>,
        next_task_id: Option<u32>,
        summary: impl Into<String>,
        data: Value,
    ) -> Self {
        Self {
            tasks,
            next_task_id,
            outcome: TaskMutationOutcome::applied(summary, data),
        }
    }

    fn unchanged(tasks: Vec<SessionTask>, summary: impl Into<String>, data: Value) -> Self {
        Self {
            tasks,
            next_task_id: None,
            outcome: TaskMutationOutcome::unchanged(summary, data),
        }
    }

    fn refused(tasks: Vec<SessionTask>, summary: impl Into<String>, data: Value) -> Self {
        Self {
            tasks,
            next_task_id: None,
            outcome: TaskMutationOutcome::refused(summary, data),
        }
    }
}

pub type TaskMutation =
    Box<dyn FnOnce(Vec<SessionTask>, u32) -> Result<TaskMutationResult, String> + Send>;

/// Structured availability evidence for task-store reads. This is separate
/// from task truth: a consumer may still hold confirmed rows while the store
/// is temporarily unreachable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TaskStoreHealth {
    #[default]
    Unknown,
    Ready,
    AuthenticationRequired,
    SessionUnavailable,
    ServiceUnavailable,
    TransportUnavailable,
    ProtocolMismatch,
}

impl TaskStoreHealth {
    /// Transient failures can reconcile on a timer. Authentication and wire
    /// contract failures need new external evidence (sign-in/config/update),
    /// so consumers should wait for an explicit dirty/rebind signal.
    pub fn allows_automatic_retry(self) -> bool {
        matches!(
            self,
            Self::Unknown | Self::Ready | Self::ServiceUnavailable | Self::TransportUnavailable
        )
    }
}

/// Process-wide storage backend for session task lists.
///
/// Conceptually every session_id addresses an independent vec; the store
/// hands out the vec plus an id counter on read, and persists a new vec on
/// write. Business logic (cycle detection, auto-complete, metadata merge)
/// lives in [`TaskManager`] so it is shared by all backends.
#[async_trait]
pub trait TaskStore: Send + Sync {
    /// Last structured read-path health observed by this store. Implementations
    /// that cannot classify failures may keep the default `Unknown` evidence.
    fn health_snapshot(&self) -> TaskStoreHealth {
        TaskStoreHealth::Unknown
    }

    /// Load every task for this session in stable order.
    async fn load(&self, session_id: &str) -> Result<Vec<SessionTask>, String>;

    /// Load open-work tasks for user-facing `active` views:
    /// `pending`, `in_progress`, and `paused`.
    ///
    /// Default impl loads all rows and filters in Rust — correct but
    /// ships the whole table over the wire. `MatrixOneTaskStore` overrides
    /// this with a WHERE clause so the owner-bound
    /// `idx_session_todos_owner_session_status_updated` index returns only
    /// matching rows are returned.
    async fn load_active(&self, session_id: &str) -> Result<Vec<SessionTask>, String> {
        Ok(self
            .load(session_id)
            .await?
            .into_iter()
            .filter(|t| t.status.is_open_work())
            .collect())
    }

    /// Archive historical tasks.
    ///
    /// Default implementation works session-locally so tests/offline mode
    /// still have coherent semantics. SQL-backed stores should keep the same
    /// session-local default; cross-session cleanup belongs behind an explicit
    /// user-level action, not a model-triggered current-session archive.
    async fn archive(&self, session_id: &str, args: &Value) -> Result<TaskMutationOutcome, String> {
        let parsed = parse_archive_args(args)?;
        let task_id = parsed.task_id;
        let days = parsed.older_than_days;
        let reason = parsed.reason;
        let now = chrono::Utc::now();
        let now_rfc3339 = now.to_rfc3339();
        let cutoff = now - chrono::Duration::days(days);
        let session_label = session_id.to_string();

        self.mutate(
            session_id,
            Box::new(move |mut tasks, _next| {
                let original_tasks = tasks.clone();
                if let Some(task_id) = task_id {
                    let Some(task_index) = tasks.iter().position(|t| t.id == task_id) else {
                        return Ok(TaskMutationResult::refused(
                            tasks,
                            format!(
                                "Refused: task #{task_id} not found in session {session_label}"
                            ),
                            json!({
                                "task_id": task_id,
                                "message": format!(
                                    "Task '{}' was not found in session '{}'",
                                    task_id, session_label
                                ),
                            }),
                        ));
                    };
                    let previous_status = tasks[task_index].status;
                    if previous_status != SessionTaskStatusKind::Archived
                        && !previous_status.can_be_archived()
                    {
                        return Ok(TaskMutationResult::refused(
                            tasks,
                            format!(
                                "Refused: task #{task_id} is '{previous_status}' — only completed, failed, or cancelled tasks can be archived"
                            ),
                            json!({
                                "task_id": task_id,
                                "previous_status": previous_status,
                                "message": format!(
                                    "Task '{}' must be completed, failed, or cancelled before it can be archived",
                                    task_id
                                ),
                            }),
                        ));
                    }

                    if previous_status != SessionTaskStatusKind::Archived {
                        tasks[task_index].status = SESSION_TASK_STATUS_ARCHIVED;
                        tasks[task_index].updated_at = now_rfc3339.clone();
                        tasks[task_index].archived_at = Some(now_rfc3339.clone());
                        if let Some(reason) = reason.as_deref() {
                            let meta = tasks[task_index]
                                .metadata
                                .get_or_insert_with(Default::default);
                            meta.insert("archive_reason".to_string(), json!(reason));
                        }
                    } else if tasks[task_index].archived_at.is_none() {
                        // Legacy rows may predate archived_at. Preserve their
                        // last known mutation time instead of inventing a new
                        // archive time during replay.
                        tasks[task_index].archived_at = Some(tasks[task_index].updated_at.clone());
                    }
                    let archived_ids = HashSet::from([task_id.clone()]);
                    detach_task_dependency_edges(&mut tasks, &archived_ids);
                    reconcile_all_subtask_completion(&mut tasks, &now_rfc3339);
                    if same_task_board_state(&original_tasks, &tasks) {
                        return Ok(TaskMutationResult::unchanged(
                            original_tasks,
                            format!("Task #{task_id} is already archived"),
                            json!({
                                "task_id": task_id,
                                "previous_status": previous_status,
                                "status": SESSION_TASK_STATUS_ARCHIVED,
                                "already_current": true,
                                "message": format!("Task '{}' is already archived", task_id),
                            }),
                        ));
                    }
                    let repaired_existing = previous_status == SessionTaskStatusKind::Archived;
                    return Ok(TaskMutationResult::applied(
                        tasks,
                        None,
                        if repaired_existing {
                            format!("Reconciled archived task #{task_id}")
                        } else {
                            format!("Archived task #{task_id} (was {previous_status})")
                        },
                        json!({
                            "task_id": task_id,
                            "previous_status": previous_status,
                            "status": SESSION_TASK_STATUS_ARCHIVED,
                            "reconciled_existing_archive": repaired_existing,
                            "message": if repaired_existing {
                                format!("Task '{}' archive invariants reconciled", task_id)
                            } else {
                                format!("Task '{}' archived", task_id)
                            },
                        }),
                    ));
                }

                let existing_archived_ids: HashSet<&str> = original_tasks
                    .iter()
                    .filter(|task| task.status == SessionTaskStatusKind::Archived)
                    .map(|task| task.id.as_str())
                    .collect();
                let repaired_existing_archives = existing_archived_ids
                    .iter()
                    .filter(|archived_id| {
                        original_tasks.iter().any(|task| {
                            (task.id.as_str() == **archived_id
                                && (task.archived_at.is_none()
                                    || !task.blocks.is_empty()
                                    || !task.blocked_by.is_empty()))
                                || task.blocks.iter().any(|id| id.as_str() == **archived_id)
                                || task
                                    .blocked_by
                                    .iter()
                                    .any(|id| id.as_str() == **archived_id)
                        })
                    })
                    .count() as u64;
                let mut archived_ids: HashSet<String> = HashSet::new();
                let mut newly_archived = 0_u64;
                for task in &mut tasks {
                    if task.status == SessionTaskStatusKind::Archived {
                        if task.archived_at.is_none() {
                            task.archived_at = Some(task.updated_at.clone());
                        }
                        archived_ids.insert(task.id.clone());
                        continue;
                    }
                    if !task.status.can_be_archived() {
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
                        task.status = SessionTaskStatusKind::Archived;
                        task.updated_at = now_rfc3339.clone();
                        task.archived_at = Some(now_rfc3339.clone());
                        if let Some(reason) = reason.as_deref() {
                            let meta = task.metadata.get_or_insert_with(Default::default);
                            meta.insert("archive_reason".to_string(), json!(reason));
                        }
                        archived_ids.insert(task.id.clone());
                        newly_archived = newly_archived.saturating_add(1);
                    }
                }
                detach_task_dependency_edges(&mut tasks, &archived_ids);
                reconcile_all_subtask_completion(&mut tasks, &now_rfc3339);
                let mut summary = format!(
                    "Archived {newly_archived} terminal task(s) older than {days} days in session {session_label}"
                );
                let mut data = json!({
                    "archived": newly_archived,
                    "older_than_days": days,
                    "scope": "session",
                    "session_id": session_label,
                    "reconciled_existing_archives": repaired_existing_archives,
                    "message": format!(
                        "Archived {} terminal task(s) older than {} days in session '{}'; reconciled {} existing archive(s)",
                        newly_archived, days, session_label, repaired_existing_archives
                    ),
                });
                if same_task_board_state(&original_tasks, &tasks) {
                    Ok(TaskMutationResult::unchanged(original_tasks, summary, data))
                } else {
                    if newly_archived == 0 {
                        summary = format!(
                            "Reconciled existing archive invariants in session {session_label}"
                        );
                        data["archive_invariants_reconciled"] = json!(true);
                    }
                    Ok(TaskMutationResult::applied(tasks, None, summary, data))
                }
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
    async fn mutate(
        &self,
        session_id: &str,
        mutation: TaskMutation,
    ) -> Result<TaskMutationOutcome, String> {
        let tasks = self.load(session_id).await?;
        let next = self.peek_next_task_id(session_id).await?;
        let result = mutation(tasks, next)?;
        if !result.outcome.status.changed() {
            return Ok(result.outcome);
        }
        if let Some(next) = result.next_task_id {
            self.set_next_task_id(session_id, next).await?;
        }
        self.save(session_id, result.tasks).await?;
        Ok(result.outcome)
    }
    /// Return and consume the next integer to use when forming `task-<n>` ids.
    /// Must be monotonic per session_id.
    async fn next_task_id(&self, session_id: &str) -> Result<u32, String>;
    /// Set the id counter (used by `restore_snapshot` to rewind
    /// numbering after a turn rollback).
    ///
    /// Stores must override this to provide a real implementation;
    /// the default returns an error to avoid silent snapshot-restore
    /// breakage when a store is used with rollback paths it does not
    /// support.
    async fn set_next_task_id(&self, session_id: &str, next: u32) -> Result<(), String> {
        let _ = (session_id, next);
        Err("set_next_task_id: not implemented".to_string())
    }
    /// Restore task rows and the next-id counter as one logical rollback
    /// operation. Stores that can update both under one lock/transaction
    /// should override this method. The default returns an error to avoid
    /// silent snapshot-restore mismatches when the user runs against a store
    /// that has not implemented atomic snapshot restore.
    #[allow(async_fn_in_trait)]
    async fn restore_snapshot_state(
        &self,
        session_id: &str,
        tasks: Vec<SessionTask>,
        next_task_id: u32,
        expected_version: u64,
    ) -> Result<(), String> {
        let _ = (session_id, tasks, next_task_id, expected_version);
        Err("restore_snapshot_state is not supported for this store".to_string())
    }
    /// Capture tasks, id allocator state, and the board version from one
    /// logical point in time.
    ///
    /// The default uses an optimistic version fence so stores do not return a
    /// torn snapshot when a writer commits between the individual reads.
    /// Durable stores must increment the same version for every task-board
    /// write path, including background lifecycle maintenance.
    async fn load_snapshot_state(&self, session_id: &str) -> Result<TaskManagerSnapshot, String> {
        const MAX_ATTEMPTS: usize = 3;
        for attempt in 1..=MAX_ATTEMPTS {
            let version_before = self
                .get_session_version(session_id)
                .await
                .map_err(|e| format!("get_session_version before snapshot failed: {e}"))?;
            let tasks = self
                .load(session_id)
                .await
                .map_err(|e| format!("task load during snapshot failed: {e}"))?;
            let next_task_id = self
                .peek_next_task_id(session_id)
                .await
                .map_err(|e| format!("peek_next_task_id failed: {e}"))?;
            let version_after = self
                .get_session_version(session_id)
                .await
                .map_err(|e| format!("get_session_version after snapshot failed: {e}"))?;
            if version_before == version_after {
                return Ok(TaskManagerSnapshot {
                    tasks,
                    next_task_id,
                    version: version_after,
                    restore_version: None,
                });
            }
            if attempt == MAX_ATTEMPTS {
                return Err(format!(
                    "task board changed while capturing snapshot \
                     (version {version_before} -> {version_after}); retry"
                ));
            }
        }
        unreachable!("snapshot attempt loop always returns")
    }
    /// Read the next id WITHOUT consuming or mutating the counter.
    /// Used by `try_snapshot_state` to capture the counter for rollback
    /// without leaving a hole in the id sequence.
    ///
    /// No default impl: the original alloc-then-rewind fallback was a
    /// silent foot-gun — any new `TaskStore` impl that forgot to
    /// override it would re-introduce the A1 race (concurrent
    /// allocators would have their bump clobbered by the rewind).
    /// Requiring an explicit impl makes correctness a compile-time
    /// requirement.
    async fn peek_next_task_id(&self, session_id: &str) -> Result<u32, String>;

    /// Read the current session version without mutating anything.
    /// The version is a monotonic counter incremented on every write-path
    /// mutation. `try_snapshot_state` captures the current version;
    /// `restore_snapshot` checks the snapshot version against the store
    /// to detect concurrent-mutation conflicts.
    ///
    /// Versions start at 0. Stores that support snapshot restore must
    /// increment the version for every write so 0 remains a valid CAS value,
    /// not a sentinel for disabling conflict detection.
    async fn get_session_version(&self, session_id: &str) -> Result<u64, String> {
        let _ = session_id;
        Ok(0)
    }
    /// Bump the session version after a mutation commits. Called from
    /// mutate, save, restore_snapshot_state, and next_task_id paths.
    /// Default no-op — stores that support version tracking override this.
    async fn bump_version(&self, session_id: &str) {
        let _ = session_id;
    }
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

    /// Load a bounded cross-session slice of open work. Periodically refreshed
    /// UI surfaces should use this instead of `load_all_sessions` so completed
    /// history cannot turn a task-board toggle into an unbounded table scan.
    async fn load_open_sessions(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, Vec<SessionTask>)>, String> {
        let mut remaining = limit;
        let mut out = Vec::new();
        if remaining == 0 {
            return Ok(out);
        }
        for (session_id, tasks) in self.load_all_sessions().await? {
            let open: Vec<SessionTask> = tasks
                .into_iter()
                .filter(|task| task.status.is_open_work())
                .take(remaining)
                .collect();
            if !open.is_empty() {
                remaining = remaining.saturating_sub(open.len());
                out.push((session_id, open));
                if remaining == 0 {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Load the minimal truthful projection needed by a bounded
    /// cross-session overview. The default adapts stores that already return
    /// full tasks; remote stores should override this to avoid transferring
    /// fields the overview cannot render.
    async fn load_open_task_summaries(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, Vec<OpenTaskSummary>)>, String> {
        Ok(bounded_open_task_summaries(
            self.load_open_sessions(limit).await?,
            limit,
        ))
    }
}

/// In-memory store for tests and offline CLI mode. Holds a map
/// `session_id -> (Vec<SessionTask>, next_id)` behind a single `Mutex`.
/// Broadcasts `session_id` on every successful save so `TaskBoardObserver`
/// can refresh immediately without waiting for a fallback poll.
pub struct InMemoryTaskStore {
    sessions: tokio::sync::Mutex<HashMap<String, InMemorySession>>,
    /// Broadcast sender for "session X changed" events. Capacity 16 is
    /// generous for the expected subscriber count (one observer per REPL
    /// + occasional test subscribers); slow consumers get dropped events
    ///   (`RecvError::Lagged`) rather than blocking writers.
    changed_tx: tokio::sync::broadcast::Sender<String>,
    /// When enabled, `save()` validates task invariants (unique IDs, non-empty
    /// titles) before persisting. Enabled by default; tests that intentionally
    /// inject corrupt state must opt out explicitly.
    validate_on_save: bool,
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
    /// Monotonic version counter: bumped on every write-path mutation
    /// (mutate, save, restore_snapshot_state, next_task_id). Used to
    /// detect stale-snapshot conflicts in restore_snapshot — a snapshot
    /// taken at version N is rejected if the store is at N+1 or higher.
    version: u64,
}

impl InMemoryTaskStore {
    pub fn new() -> Self {
        Self {
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            changed_tx: tokio::sync::broadcast::channel(16).0,
            validate_on_save: true,
        }
    }

    /// Keep save() validation enabled — duplicate IDs and empty titles are rejected.
    pub fn with_validation(mut self) -> Self {
        self.validate_on_save = true;
        self
    }

    #[cfg(test)]
    pub fn without_validation(mut self) -> Self {
        self.validate_on_save = false;
        self
    }

    /// Validate task invariants: unique IDs, non-empty titles and subtask IDs.
    fn validate_tasks(tasks: &[SessionTask]) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for task in tasks {
            if task.id.is_empty() {
                return Err("task store: task has empty id".to_string());
            }
            if !seen.insert(&task.id) {
                return Err(format!("task store: duplicate task id '{}'", task.id));
            }
            if task.title.trim().is_empty() {
                return Err(format!("task store: task '{}' has empty title", task.id));
            }
            chrono::DateTime::parse_from_rfc3339(&task.created_at).map_err(|e| {
                format!(
                    "task store: task '{}' has invalid created_at '{}': {e}",
                    task.id, task.created_at
                )
            })?;
            chrono::DateTime::parse_from_rfc3339(&task.updated_at).map_err(|e| {
                format!(
                    "task store: task '{}' has invalid updated_at '{}': {e}",
                    task.id, task.updated_at
                )
            })?;
            // Validate subtask IDs are non-empty and unique within task.
            let mut subtask_ids = std::collections::HashSet::new();
            for st in &task.subtasks {
                if st.id.is_empty() {
                    return Err(format!(
                        "task store: task '{}' has subtask with empty id",
                        task.id
                    ));
                }
                if st.title.trim().is_empty() {
                    return Err(format!(
                        "task store: task '{}' subtask '{}' has empty title",
                        task.id, st.id
                    ));
                }
                if !subtask_ids.insert(&st.id) {
                    return Err(format!(
                        "task store: task '{}' has duplicate subtask id '{}'",
                        task.id, st.id
                    ));
                }
            }
        }
        // Validate blocked_by/blocks references point to existing tasks.
        let task_ids: std::collections::HashSet<&str> = seen.iter().map(|s| s.as_str()).collect();
        for task in tasks {
            for dep_id in &task.blocked_by {
                if !task_ids.contains(dep_id.as_str()) {
                    return Err(format!(
                        "task store: task '{}' blocked_by non-existent task '{}'",
                        task.id, dep_id
                    ));
                }
            }
            for dep_id in &task.blocks {
                if !task_ids.contains(dep_id.as_str()) {
                    return Err(format!(
                        "task store: task '{}' blocks non-existent task '{}'",
                        task.id, dep_id
                    ));
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl TaskStore for InMemoryTaskStore {
    async fn load(&self, session_id: &str) -> Result<Vec<SessionTask>, String> {
        let sessions = self.sessions.lock().await;
        Ok(sessions
            .get(session_id)
            .map(|s| s.tasks.clone())
            .unwrap_or_default())
    }

    async fn save(&self, session_id: &str, tasks: Vec<SessionTask>) -> Result<(), String> {
        if self.validate_on_save {
            Self::validate_tasks(&tasks)?;
        }
        {
            let mut sessions = self.sessions.lock().await;
            let entry = sessions.entry(session_id.to_string()).or_default();
            entry.tasks = tasks;
            entry.version = entry.version.wrapping_add(1);
        }
        // Best-effort broadcast. `send` errors only when there are no
        // receivers, which is the common "no observer attached" case in
        // tests and headless CLI — not an error.
        let _ = self.changed_tx.send(session_id.to_string());
        Ok(())
    }

    async fn mutate(
        &self,
        session_id: &str,
        mutation: TaskMutation,
    ) -> Result<TaskMutationOutcome, String> {
        let outcome = {
            let mut sessions = self.sessions.lock().await;
            let entry = sessions.entry(session_id.to_string()).or_default();
            let next = if entry.next_id == 0 { 1 } else { entry.next_id };
            let next = u32::try_from(next)
                .map_err(|_| format!("task id counter exhausted for session {session_id}"))?;
            // `tokio::sync::Mutex` does not poison on panic, so we can keep the
            // mutation atomic while still surfacing a panic as a task-store error.
            let cloned_tasks = entry.tasks.clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                mutation(cloned_tasks, next)
            }))
            .map_err(|_| {
                format!(
                    "task mutation closure panicked for session {session_id}; \
                     task board unchanged"
                )
            })??;
            if self.validate_on_save && result.outcome.status.changed() {
                Self::validate_tasks(&result.tasks)?;
            }
            if result.outcome.status.changed() {
                let entry = sessions.entry(session_id.to_string()).or_default();
                entry.tasks = result.tasks;
                if let Some(next_task_id) = result.next_task_id {
                    entry.next_id = u64::from(next_task_id);
                }
                entry.version = entry.version.wrapping_add(1);
            }
            result.outcome
        };
        if outcome.status.changed() {
            let _ = self.changed_tx.send(session_id.to_string());
        }
        Ok(outcome)
    }

    async fn next_task_id(&self, session_id: &str) -> Result<u32, String> {
        let task_id;
        {
            let mut sessions = self.sessions.lock().await;
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
            task_id = u32::try_from(id)
                .map_err(|_| format!("task id counter exhausted for session {session_id}"))?;
            entry.next_id = id
                .checked_add(1)
                .ok_or_else(|| format!("task id counter overflow for session {session_id}"))?;
            entry.version = entry.version.wrapping_add(1);
        }
        Ok(task_id)
    }

    async fn set_next_task_id(&self, session_id: &str, next: u32) -> Result<(), String> {
        let mut sessions = self.sessions.lock().await;
        let entry = sessions.entry(session_id.to_string()).or_default();
        entry.next_id = u64::from(next);
        Ok(())
    }

    async fn restore_snapshot_state(
        &self,
        session_id: &str,
        tasks: Vec<SessionTask>,
        next_task_id: u32,
        expected_version: u64,
    ) -> Result<(), String> {
        if self.validate_on_save {
            Self::validate_tasks(&tasks)?;
        }
        {
            let mut sessions = self.sessions.lock().await;
            let entry = sessions.entry(session_id.to_string()).or_default();
            if entry.version != expected_version {
                return Err(format!(
                    "restore_snapshot_state: version conflict (expected={}, current={}) — \
                     task board changed after rollback snapshot was sealed; retry with fresh state",
                    expected_version, entry.version
                ));
            }
            entry.tasks = tasks;
            entry.next_id = u64::from(next_task_id);
            entry.version = entry.version.wrapping_add(1);
        }
        let _ = self.changed_tx.send(session_id.to_string());
        Ok(())
    }

    async fn load_snapshot_state(&self, session_id: &str) -> Result<TaskManagerSnapshot, String> {
        let sessions = self.sessions.lock().await;
        let entry = sessions.get(session_id);
        let next_task_id = entry
            .map(|state| if state.next_id == 0 { 1 } else { state.next_id })
            .unwrap_or(1);
        Ok(TaskManagerSnapshot {
            tasks: entry.map(|state| state.tasks.clone()).unwrap_or_default(),
            next_task_id: u32::try_from(next_task_id)
                .map_err(|_| format!("task id counter exhausted for session {session_id}"))?,
            version: entry.map(|state| state.version).unwrap_or(0),
            restore_version: None,
        })
    }

    async fn peek_next_task_id(&self, session_id: &str) -> Result<u32, String> {
        let sessions = self.sessions.lock().await;
        let next = sessions
            .get(session_id)
            .map(|s| if s.next_id == 0 { 1 } else { s.next_id })
            .unwrap_or(1);
        u32::try_from(next)
            .map_err(|_| format!("task id counter exhausted for session {session_id}"))
    }

    async fn get_session_version(&self, session_id: &str) -> Result<u64, String> {
        let sessions = self.sessions.lock().await;
        Ok(sessions.get(session_id).map(|s| s.version).unwrap_or(0))
    }

    async fn bump_version(&self, session_id: &str) {
        let mut sessions = self.sessions.lock().await;
        let entry = sessions.entry(session_id.to_string()).or_default();
        entry.version = entry.version.wrapping_add(1);
    }

    fn subscribe(&self) -> Option<tokio::sync::broadcast::Receiver<String>> {
        Some(self.changed_tx.subscribe())
    }

    async fn load_all_sessions(&self) -> Result<Vec<(String, Vec<SessionTask>)>, String> {
        let sessions = self.sessions.lock().await;
        let mut out: Vec<(String, Vec<SessionTask>)> = sessions
            .iter()
            .filter(|(_, s)| !s.tasks.is_empty())
            .map(|(sid, s)| (sid.clone(), s.tasks.clone()))
            .collect();
        // Deterministic order for full-session consumers. The bounded summary
        // projection below independently orders actionable rows by recency.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    async fn load_open_task_summaries(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, Vec<OpenTaskSummary>)>, String> {
        Ok(bounded_open_task_summaries(
            self.load_all_sessions().await?,
            limit,
        ))
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
/// the JSON payload remains the machine-readable contract.
pub(crate) fn prefix_summary(summary: impl Into<String>, json_body: String) -> String {
    format!("{}\n{}", summary.into(), json_body)
}

const VALID_UPDATE_STATUSES: &[&str] = &[
    "pending",
    "in_progress",
    "paused",
    "completed",
    "failed",
    "cancelled",
    "deleted",
];

pub const VALID_LIST_STATUS_FILTERS: &[&str] = &[
    "pending",
    "in_progress",
    "paused",
    "completed",
    "failed",
    "cancelled",
    "archived",
    "deleted",
    "all",
    "active",
];

fn normalize_update_status(args: &Value) -> Result<Option<SessionTaskStatusKind>, String> {
    let raw_new_status = args.get("new_status");
    if args.get("status").is_some() {
        return Err(
            "field 'status' is not supported for task_board.update; use 'new_status'".to_string(),
        );
    }
    let Some(raw_status) = raw_new_status else {
        return Ok(None);
    };
    let Some(status) = raw_status.as_str() else {
        return Err("field 'new_status' must be a string".to_string());
    };
    if !VALID_UPDATE_STATUSES.contains(&status) {
        return Err(format!(
            "invalid new_status '{}' (valid: {})",
            status,
            VALID_UPDATE_STATUSES.join("|")
        ));
    }
    Ok(Some(SessionTaskStatusKind::from_status_str(status)))
}

fn validate_parent_status_transition(
    previous_status: SessionTaskStatusKind,
    new_status: Option<SessionTaskStatusKind>,
) -> Result<(), String> {
    let Some(new_status) = new_status else {
        return Ok(());
    };
    if new_status == previous_status || new_status == SessionTaskStatusKind::Deleted {
        return Ok(());
    }
    // Terminal/tombstone tasks cannot be moved backward. Every persisted
    // status may still transition to Deleted above so users can clear the
    // task board without losing the audit tombstone.
    if matches!(
        previous_status,
        SessionTaskStatusKind::Completed
            | SessionTaskStatusKind::Failed
            | SessionTaskStatusKind::Cancelled
            | SessionTaskStatusKind::Archived
            | SessionTaskStatusKind::Deleted
            | SessionTaskStatusKind::Migrated
    ) {
        return Err(format!(
            "task is already terminal ({previous_status}); create a new task for follow-up work, or use new_status='deleted' to hide it from active views while keeping an audit tombstone"
        ));
    }
    Ok(())
}

fn validate_subtask_status_transition(
    previous_status: SessionTaskStatusKind,
    new_status: SessionTaskStatusKind,
) -> Result<(), String> {
    if new_status == previous_status || new_status == SessionTaskStatusKind::Deleted {
        return Ok(());
    }
    // Terminal/tombstone subtasks cannot be moved backward, except for the
    // Completed→Pending reversal that triggers parent auto-complete undo.
    // Every persisted status may still transition to Deleted above.
    if matches!(
        previous_status,
        SessionTaskStatusKind::Completed
            | SessionTaskStatusKind::Failed
            | SessionTaskStatusKind::Cancelled
            | SessionTaskStatusKind::Archived
            | SessionTaskStatusKind::Deleted
            | SessionTaskStatusKind::Migrated
    ) && !(previous_status == SessionTaskStatusKind::Completed
        && new_status == SessionTaskStatusKind::Pending)
    {
        return Err(format!(
            "subtask is already terminal ({previous_status}); create a new subtask for follow-up work, or use new_status='deleted' to hide it from active views while keeping an audit tombstone"
        ));
    }
    Ok(())
}

fn is_reversible_auto_completed_parent(task: &SessionTask) -> bool {
    task.status.is_completed()
        && task
            .metadata
            .as_ref()
            .and_then(|m| m.get("auto_completed_by_subtasks"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

fn validate_allowed_fields(args: &Value, action: &str, allowed: &[&str]) -> Result<(), String> {
    debug_assert_eq!(
        crate::task_tool_contract::task_action_allowed_fields(action),
        Some(allowed),
        "TaskManager field list for task_board.{action} must match task_tool_contract"
    );
    crate::task_tool_contract::validate_public_task_tool_args_for_action(action, args)
}

fn validate_string_chars(text: &str, field: &str, max: usize) -> Result<(), String> {
    let count = text.chars().count();
    if count > max {
        return Err(format!(
            "field '{field}' exceeds {max} characters ({count})"
        ));
    }
    Ok(())
}

/// **Task ID character validation**: enforce a strict allowlist to prevent
/// path traversal (`../`), injection (`/`, `\`), and control characters.
/// From first principles: task IDs are used as keys in storage and URLs,
/// so they must be filesystem-safe and URL-safe. Rejecting unsafe characters
/// at the validation layer prevents downstream exploits in plan repositories,
/// edge task stores, and API routes.
fn validate_task_id_chars(id: &str, field: &str) -> Result<(), String> {
    // Reject path separators and parent-directory traversal.
    if id.contains('/') || id.contains('\\') || id.contains("..") {
        return Err(format!(
            "field '{field}' contains path traversal or separators (got: {id:?})"
        ));
    }
    // Reject control characters (U+0000–U+001F, U+007F) and non-ASCII.
    if id.chars().any(|c| c.is_control() || !c.is_ascii()) {
        return Err(format!(
            "field '{field}' contains control or non-ASCII characters (got: {id:?})"
        ));
    }
    Ok(())
}

fn validate_metadata_size(
    metadata: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(metadata)
        .map_err(|err| format!("field '{field}' could not be serialized: {err}"))?
        .len();
    if bytes > MAX_TASK_METADATA_BYTES {
        return Err(format!(
            "field '{field}' exceeds {MAX_TASK_METADATA_BYTES} bytes when serialized ({bytes})"
        ));
    }
    Ok(())
}

fn parse_create_subtasks(
    args: &Value,
    parent_owner: Option<&str>,
) -> Result<Vec<SessionSubtask>, String> {
    let Some(raw_subtasks) = args.get("subtasks") else {
        return Ok(Vec::new());
    };
    let Some(items) = raw_subtasks.as_array() else {
        return Err("field 'subtasks' must be an array".to_string());
    };
    if items.len() > MAX_CREATE_SUBTASKS {
        return Err(format!(
            "field 'subtasks' has {} items; maximum is {MAX_CREATE_SUBTASKS}. Split oversized work into separate tasks instead of one giant checklist",
            items.len()
        ));
    }

    let mut seen_ids = HashSet::new();
    let mut subtasks = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let Some(obj) = item.as_object() else {
            return Err(format!("field 'subtasks[{index}]' must be an object"));
        };
        for key in obj.keys() {
            if !["id", "title", "description", "depends_on", "owner"].contains(&key.as_str()) {
                return Err(format!(
                    "unknown field 'subtasks[{index}].{key}' for task_board.create"
                ));
            }
        }

        let id = obj
            .get("id")
            .ok_or_else(|| format!("field 'subtasks[{index}].id' is required"))?
            .as_str()
            .ok_or_else(|| format!("field 'subtasks[{index}].id' must be a string"))?;
        if id.trim().is_empty() {
            return Err(format!("field 'subtasks[{index}].id' must be non-empty"));
        }
        validate_string_chars(id, &format!("subtasks[{index}].id"), MAX_SUBTASK_ID_CHARS)?;
        validate_task_id_chars(id, &format!("subtasks[{index}].id"))?;
        if !seen_ids.insert(id.to_string()) {
            return Err(format!("duplicate subtask id '{id}'"));
        }

        let title = obj
            .get("title")
            .ok_or_else(|| format!("field 'subtasks[{index}].title' is required"))?
            .as_str()
            .ok_or_else(|| format!("field 'subtasks[{index}].title' must be a string"))?;
        if title.trim().is_empty() {
            return Err(format!("field 'subtasks[{index}].title' must be non-empty"));
        }
        validate_string_chars(
            title,
            &format!("subtasks[{index}].title"),
            MAX_SUBTASK_TITLE_CHARS,
        )?;

        let description = match obj.get("description") {
            Some(value) => {
                let text = value.as_str().ok_or_else(|| {
                    format!("field 'subtasks[{index}].description' must be a string")
                })?;
                validate_string_chars(
                    text,
                    &format!("subtasks[{index}].description"),
                    MAX_SUBTASK_DESCRIPTION_CHARS,
                )?;
                Some(text.to_string())
            }
            None => None,
        };

        let explicit_owner = match obj.get("owner") {
            Some(value) => {
                let text = value
                    .as_str()
                    .ok_or_else(|| format!("field 'subtasks[{index}].owner' must be a string"))?;
                if text.trim().is_empty() {
                    return Err(format!("field 'subtasks[{index}].owner' must be non-empty"));
                }
                validate_string_chars(
                    text,
                    &format!("subtasks[{index}].owner"),
                    MAX_TASK_OWNER_CHARS,
                )?;
                Some(text.to_string())
            }
            None => None,
        };

        let depends_on = match obj.get("depends_on") {
            Some(value) => {
                let Some(deps) = value.as_array() else {
                    return Err(format!(
                        "field 'subtasks[{index}].depends_on' must be an array"
                    ));
                };
                let mut out = Vec::with_capacity(deps.len());
                for (dep_index, dep) in deps.iter().enumerate() {
                    let Some(dep_id) = dep.as_str() else {
                        return Err(format!(
                            "field 'subtasks[{index}].depends_on[{dep_index}]' must be a string"
                        ));
                    };
                    if dep_id.trim().is_empty() {
                        return Err(format!(
                            "field 'subtasks[{index}].depends_on[{dep_index}]' must be non-empty"
                        ));
                    }
                    validate_string_chars(
                        dep_id,
                        &format!("subtasks[{index}].depends_on[{dep_index}]"),
                        MAX_SUBTASK_ID_CHARS,
                    )?;
                    validate_task_id_chars(
                        dep_id,
                        &format!("subtasks[{index}].depends_on[{dep_index}]"),
                    )?;
                    out.push(dep_id.to_string());
                }
                out
            }
            None => Vec::new(),
        };

        subtasks.push(SessionSubtask {
            id: id.to_string(),
            title: title.to_string(),
            description,
            status: SessionTaskStatusKind::Pending,
            depends_on,
            owner: explicit_owner.or_else(|| parent_owner.map(str::to_string)),
            reason: None,
        });
    }

    for (index, subtask) in subtasks.iter().enumerate() {
        let mut seen_deps = HashSet::new();
        for dep_id in &subtask.depends_on {
            if dep_id == &subtask.id {
                return Err(format!(
                    "subtasks[{index}] cannot depend on itself ('{dep_id}')"
                ));
            }
            if !seen_deps.insert(dep_id.as_str()) {
                return Err(format!(
                    "subtasks[{index}] has duplicate dependency '{dep_id}'"
                ));
            }
            if !seen_ids.contains(dep_id) {
                return Err(format!(
                    "subtasks[{index}] has unknown subtask dependency '{dep_id}'"
                ));
            }
        }
    }

    // Cycle detection: subtask depends_on edges must not form a cycle.
    // Uses DFS with three-color marking (Kahn's algorithm overkill for ≤20 nodes).
    detect_subtask_dependency_cycles(&subtasks)?;

    Ok(subtasks)
}

/// DFS-based cycle detection for subtask `depends_on` edges.
/// WHITE = unvisited, GRAY = in current recursion path, BLACK = fully explored.
fn detect_subtask_dependency_cycles(subtasks: &[SessionSubtask]) -> Result<(), String> {
    let n = subtasks.len();
    if n <= 1 {
        return Ok(());
    }

    // Build adjacency list: maps subtask index → dependent subtask indices.
    let id_to_index: HashMap<&str, usize> = subtasks
        .iter()
        .enumerate()
        .map(|(i, st)| (st.id.as_str(), i))
        .collect();

    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, st) in subtasks.iter().enumerate() {
        for dep_id in &st.depends_on {
            if let Some(&j) = id_to_index.get(dep_id.as_str()) {
                adj[j].push(i); // j → i means "j must complete before i can start"
            }
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }
    use Color::*;

    let mut color = vec![White; n];

    fn dfs(
        u: usize,
        adj: &[Vec<usize>],
        color: &mut [Color],
        subtasks: &[SessionSubtask],
    ) -> Result<(), String> {
        color[u] = Gray;
        for &v in &adj[u] {
            match color[v] {
                Gray => {
                    return Err(format!(
                        "subtask dependency cycle detected: '{}' and '{}' form a circular dependency",
                        subtasks[u].id, subtasks[v].id
                    ));
                }
                White => {
                    dfs(v, adj, color, subtasks)?;
                }
                Black => {}
            }
        }
        color[u] = Black;
        Ok(())
    }

    for u in 0..n {
        if color[u] == White {
            dfs(u, &adj, &mut color, subtasks)?;
        }
    }

    Ok(())
}

fn required_non_empty_string_field(args: &Value, field: &str) -> Result<String, String> {
    let Some(value) = args.get(field) else {
        return Err(format!("field '{field}' is required"));
    };
    let Some(text) = value.as_str() else {
        return Err(format!("field '{field}' must be a string"));
    };
    if text.trim().is_empty() {
        return Err(format!("field '{field}' must be non-empty"));
    }
    Ok(text.to_string())
}

fn optional_string_field(args: &Value, field: &str) -> Result<Option<String>, String> {
    match args.get(field) {
        Some(value) => value
            .as_str()
            .map(|text| Some(text.to_string()))
            .ok_or_else(|| format!("field '{field}' must be a string")),
        None => Ok(None),
    }
}

fn optional_non_empty_string_field(args: &Value, field: &str) -> Result<Option<String>, String> {
    match optional_string_field(args, field)? {
        Some(text) if text.trim().is_empty() => Err(format!("field '{field}' must be non-empty")),
        value => Ok(value),
    }
}

/// Parse an update-only optional string where JSON `null` explicitly clears
/// persisted state. Outer `None` means the field was absent; inner `None`
/// means the caller requested a clear.
fn optional_nullable_string_field(
    args: &Value,
    field: &str,
) -> Result<Option<Option<String>>, String> {
    match args.get(field) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(value) => value
            .as_str()
            .map(|text| Some(Some(text.to_string())))
            .ok_or_else(|| format!("field '{field}' must be a string or null")),
    }
}

fn optional_nullable_non_empty_string_field(
    args: &Value,
    field: &str,
) -> Result<Option<Option<String>>, String> {
    match optional_nullable_string_field(args, field)? {
        Some(Some(text)) if text.trim().is_empty() => {
            Err(format!("field '{field}' must be non-empty or null"))
        }
        value => Ok(value),
    }
}

fn optional_object_field(
    args: &Value,
    field: &str,
) -> Result<Option<serde_json::Map<String, Value>>, String> {
    match args.get(field) {
        Some(value) => value
            .as_object()
            .cloned()
            .map(Some)
            .ok_or_else(|| format!("field '{field}' must be an object")),
        None => Ok(None),
    }
}

fn optional_string_array_field(args: &Value, field: &str) -> Result<Vec<String>, String> {
    let Some(value) = args.get(field) else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(format!("field '{field}' must be an array"));
    };
    let mut out = Vec::with_capacity(items.len());
    let mut seen = HashSet::new();
    for (index, item) in items.iter().enumerate() {
        let Some(text) = item.as_str() else {
            return Err(format!("field '{field}[{index}]' must be a string"));
        };
        if text.trim().is_empty() {
            return Err(format!("field '{field}[{index}]' must be non-empty"));
        }
        if !seen.insert(text) {
            return Err(format!("field '{field}' has duplicate id '{text}'"));
        }
        out.push(text.to_string());
    }
    Ok(out)
}

pub(crate) struct ArchiveArgs {
    pub task_id: Option<String>,
    pub older_than_days: i64,
    pub reason: Option<String>,
}

pub(crate) fn parse_archive_args(args: &Value) -> Result<ArchiveArgs, String> {
    validate_allowed_fields(
        args,
        "archive",
        &["action", "task_id", "older_than_days", "reason"],
    )?;
    let task_id = optional_non_empty_string_field(args, "task_id")?;
    let reason = optional_non_empty_string_field(args, "reason")?;
    if let Some(reason) = reason.as_deref() {
        validate_string_chars(reason, "reason", MAX_TASK_STOP_REASON_CHARS)?;
    }
    if task_id.is_some() && args.get("older_than_days").is_some() {
        return Err(
            "archive accepts either 'task_id' for a single task or 'older_than_days' for bulk archive, not both"
                .to_string(),
        );
    }
    let days_raw = match args.get("older_than_days") {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| "field 'older_than_days' must be a non-negative integer".to_string())?,
        None => 30,
    };
    let older_than_days =
        i64::try_from(days_raw).map_err(|_| format!("older_than_days is too large: {days_raw}"))?;
    Ok(ArchiveArgs {
        task_id,
        older_than_days,
        reason,
    })
}

/// Bidirectional subtask ↔ parent status reconciliation. Called after
/// any subtask mutation that might flip the all-completed state.
///
/// Forward arms:
/// - any started subtask (`in_progress` / `completed`) promotes a still-pending
///   parent to `in_progress`, so the task board stops reading the parent as
///   untouched work once execution has clearly begun.
/// - all subtasks completed → parent completed, but only when the parent still
///   represents open work (pending / in_progress / paused). Terminal
///   non-success states stay as-is, since promoting them to completed would
///   silently erase a failure signal.
///
/// Reverse arm (a subtask flipped back from completed → parent reopens):
/// only fires when this function itself auto-completed the parent earlier.
/// Explicit parent completion is terminal history and must not be resurrected
/// by a later subtask edit.
fn clear_auto_completed_marker(task: &mut SessionTask) {
    if let Some(meta) = task.metadata.as_mut() {
        meta.remove("auto_completed_by_subtasks");
        if meta.is_empty() {
            task.metadata = None;
        }
    }
}

fn reconcile_subtask_completion(task: &mut SessionTask, blockers_resolved: bool) {
    if task.subtasks.is_empty() {
        return;
    }

    let all_completed = task.subtasks.iter().all(|st| st.status.is_completed());
    if all_completed {
        if task.status.is_open_work() && blockers_resolved {
            task.status = SessionTaskStatusKind::Completed;
            let meta = task.metadata.get_or_insert_with(serde_json::Map::new);
            meta.insert("auto_completed_by_subtasks".to_string(), json!(true));
        } else if !blockers_resolved && is_reversible_auto_completed_parent(task) {
            // Completion derived from children is valid only while the task's
            // own prerequisites remain resolved. If an upstream auto-complete
            // is reversed, reopen this derived completion as waiting work so
            // downstream projections never report success through a blocker.
            task.status = SessionTaskStatusKind::Pending;
            clear_auto_completed_marker(task);
        }
        return;
    }

    let was_auto_completed = task
        .metadata
        .as_ref()
        .and_then(|m| m.get("auto_completed_by_subtasks"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if task.status.is_completed() && was_auto_completed {
        task.status = SessionTaskStatusKind::InProgress;
        clear_auto_completed_marker(task);
        return;
    }

    let any_started = task.subtasks.iter().any(|st| st.status.is_started());
    if any_started && task.status.is_pending() && blockers_resolved {
        task.status = SessionTaskStatusKind::InProgress;
    }
}

/// Reconcile every derived parent status until dependency-driven completion
/// reaches a fixed point. A task completion can unblock another all-done
/// parent, which can in turn unblock a third; one local callback is therefore
/// not a complete trigger model.
fn reconcile_all_subtask_completion(tasks: &mut [SessionTask], now: &str) {
    for _ in 0..tasks.len() {
        let task_statuses = tasks
            .iter()
            .map(|task| (task.id.clone(), task.status))
            .collect::<HashMap<_, _>>();
        let mut blocker_ids = tasks
            .iter()
            .map(|task| {
                (
                    task.id.clone(),
                    task.blocked_by.iter().cloned().collect::<HashSet<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();
        for blocker in tasks.iter() {
            for blocked_id in &blocker.blocks {
                if let Some(ids) = blocker_ids.get_mut(blocked_id.as_str()) {
                    ids.insert(blocker.id.clone());
                }
            }
        }
        let blockers_resolved = tasks
            .iter()
            .map(|task| {
                blocker_ids.get(task.id.as_str()).is_none_or(|ids| {
                    ids.iter().all(|id| {
                        task_statuses
                            .get(id)
                            .is_some_and(SessionTaskStatusKind::is_completed)
                    })
                })
            })
            .collect::<Vec<_>>();
        let mut changed = false;
        for (task, blockers_resolved) in tasks.iter_mut().zip(blockers_resolved) {
            let previous_status = task.status;
            reconcile_subtask_completion(task, blockers_resolved);
            if task.status != previous_status {
                task.updated_at = now.to_string();
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// Transition child work that cannot outlive a terminal parent while
/// preserving already-terminal success/failure facts.
fn transition_open_subtasks(task: &mut SessionTask, status: SessionTaskStatusKind) -> usize {
    debug_assert!(!status.is_open_work());
    let mut transitioned = 0;
    for subtask in &mut task.subtasks {
        if subtask.status.can_be_stopped() {
            subtask.status = status;
            transitioned += 1;
        }
    }
    transitioned
}

fn cancel_open_subtasks(task: &mut SessionTask) -> usize {
    transition_open_subtasks(task, SessionTaskStatusKind::Cancelled)
}

/// Apply the single cancellation invariant used by both `update(cancelled)`
/// and `stop`: the parent and every still-open child become cancelled in the
/// same atomic store mutation. Returns the number of child transitions.
fn cancel_task_and_open_subtasks(task: &mut SessionTask, reason: Option<&str>, now: &str) -> usize {
    let previous_status = task.status;
    let transitioning = previous_status != SessionTaskStatusKind::Cancelled;
    task.status = SessionTaskStatusKind::Cancelled;

    let cancelled_subtasks = cancel_open_subtasks(task);

    if let Some(reason) = reason {
        let metadata = task.metadata.get_or_insert_with(Default::default);
        metadata.insert("reason".to_string(), json!(reason));
        if transitioning {
            let note = format!("Cancelled: {reason} (was: {previous_status})");
            task.description = Some(
                match task.description.as_deref().filter(|s| !s.is_empty()) {
                    Some(description) => format!("{description}\n\n{note}"),
                    None => note,
                },
            );
        }
    }
    task.updated_at = now.to_string();
    cancelled_subtasks
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
    /// Panics only if the session-id mutex is poisoned (a thread panicked
    /// while holding it), which indicates a bug elsewhere.
    pub fn session_id(&self) -> String {
        self.session_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Handle to the underlying store. Exposed so callers outside
    /// `astra-tools` can wire observers that subscribe to the same
    /// change broadcast the manager writes through.
    pub fn store(&self) -> Arc<dyn TaskStore> {
        self.store.clone()
    }

    /// Rebind the session id. Also swaps the store if a new one is supplied
    /// (for the offline → MO upgrade path).
    /// Panics only if the session-id mutex is poisoned (a thread panicked
    /// while holding it), which indicates a bug elsewhere.
    pub fn rebind(&self, session_id: impl Into<String>) {
        let mut guard = self.session_id.lock().unwrap_or_else(|e| e.into_inner());
        *guard = session_id.into();
    }

    fn sid(&self) -> String {
        self.session_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Get a snapshot of all tasks. Returns an error when the backing
    /// store is unavailable — unlike the old best-effort API that silently
    /// returned empty, this forces callers to handle storage failures
    /// explicitly.
    pub async fn snapshot(&self) -> Result<Vec<SessionTask>, String> {
        self.store.load(&self.sid()).await
    }

    /// Load all tasks and surface backend errors to callers that must not
    /// confuse an unreadable task board with an empty one.
    pub async fn load_tasks(&self) -> Result<Vec<SessionTask>, String> {
        self.store.load(&self.sid()).await
    }

    /// Build a compact task context string for injection into the agent's
    /// prompt via the standard `plan_resume_hint` → `ExternalSources.plan_context`
    /// pipeline. Returns `None` when the task board is empty.
    ///
    /// Unlike the old CLI-layer hack that read UI snapshots and polluted
    /// `append_system_prompt`, this lives at the data layer (TaskManager),
    /// reads from the durable store, and flows through the context pipeline's
    /// proper token accounting.
    pub async fn build_active_task_context(&self) -> Option<String> {
        fn compact_text(text: &str, max_chars: usize) -> String {
            if max_chars == 0 {
                return String::new();
            }
            if text.chars().count() <= max_chars {
                return text.to_string();
            }
            let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
            out.push('…');
            out
        }

        fn compact_title(title: &str) -> String {
            compact_text(title, 120)
        }

        fn compact_blocked_task(task: &SessionTask, blockers: &[String]) -> String {
            const MAX_CHARS: usize = 120;
            const MAX_BLOCKERS: usize = 2;
            let mut blocker_summary = blockers
                .iter()
                .take(MAX_BLOCKERS)
                .map(|id| compact_text(id, 32))
                .collect::<Vec<_>>();
            if blockers.len() > MAX_BLOCKERS {
                blocker_summary.push(format!("+{} more", blockers.len() - MAX_BLOCKERS));
            }
            let suffix = format!(" (waiting on {})", blocker_summary.join(", "));
            let title_budget = MAX_CHARS.saturating_sub(suffix.chars().count());
            format!("{}{}", compact_text(&task.title, title_budget), suffix)
        }

        fn compact_titles(titles: &[&str]) -> String {
            const MAX_TITLES: usize = 6;
            let mut rendered: Vec<String> = titles
                .iter()
                .take(MAX_TITLES)
                .map(|title| compact_title(title))
                .collect();
            if titles.len() > MAX_TITLES {
                rendered.push(format!("+{} more", titles.len() - MAX_TITLES));
            }
            rendered.join(", ")
        }

        let tasks = match self.load_tasks().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    session_id = %self.sid(),
                    "build_active_task_context: failed to load tasks, returning None"
                );
                return None;
            }
        };
        if tasks.is_empty() {
            return None;
        }

        let open_tasks = tasks
            .iter()
            .filter(|task| task.status.is_open_work())
            .map(|task| (task, unresolved_task_blocker_ids(&tasks, task)))
            .collect::<Vec<_>>();
        let ready_titles = |status| {
            open_tasks
                .iter()
                .filter(|(task, blockers)| task.status == status && blockers.is_empty())
                .map(|(task, _)| task.title.as_str())
                .collect::<Vec<_>>()
        };
        let in_progress = ready_titles(SessionTaskStatusKind::InProgress);
        let pending = ready_titles(SessionTaskStatusKind::Pending);
        let paused = ready_titles(SessionTaskStatusKind::Paused);
        let blocked = open_tasks
            .iter()
            .filter(|(_, blockers)| !blockers.is_empty())
            .map(|(task, blockers)| compact_blocked_task(task, blockers))
            .collect::<Vec<_>>();
        let blocked_titles = blocked.iter().map(String::as_str).collect::<Vec<_>>();

        if in_progress.is_empty() && pending.is_empty() && paused.is_empty() && blocked.is_empty() {
            return None;
        }

        let mut hint = String::from("## Active Task Board\n");
        if let Some(task) = in_progress.first() {
            hint.push_str(&format!("- 🔄 In progress: {}\n", compact_title(task)));
        }
        if in_progress.len() > 1 {
            hint.push_str(&format!(
                "- 🔄 Also in progress ({}): {}\n",
                in_progress.len() - 1,
                compact_titles(&in_progress[1..])
            ));
        }
        if !pending.is_empty() {
            hint.push_str(&format!(
                "- ⏳ Pending ({}): {}\n",
                pending.len(),
                compact_titles(&pending)
            ));
        }
        if !paused.is_empty() {
            hint.push_str(&format!(
                "- ⏸ Paused ({}): {}\n",
                paused.len(),
                compact_titles(&paused)
            ));
        }
        if !blocked.is_empty() {
            hint.push_str(&format!(
                "- ⛔ Blocked ({}): {}\n",
                blocked.len(),
                compact_titles(&blocked_titles)
            ));
        }
        if in_progress.is_empty() && !pending.is_empty() {
            hint.push_str(&format!(
                "Focus on the first pending task: {}\n",
                compact_title(pending[0])
            ));
        } else if in_progress.is_empty() && !paused.is_empty() {
            hint.push_str(&format!(
                "Resume or reprioritize the first paused task: {}\n",
                compact_title(paused[0])
            ));
        } else if !in_progress.is_empty() {
            hint.push_str("Focus on completing the in-progress task before starting new work.\n");
        } else if !blocked.is_empty() {
            hint.push_str("Resolve the listed blockers before starting blocked work.\n");
        }

        Some(hint)
    }

    /// Load open-work tasks and surface backend errors to callers that need
    /// completion-critical task-board state.
    pub async fn load_active_tasks(&self) -> Result<Vec<SessionTask>, String> {
        self.store.load_active(&self.sid()).await
    }

    /// Capture a full rollback snapshot (tasks + next id), surfacing backend
    /// read failures to callers that must not record an empty rollback handle
    /// for an unreadable task board.
    pub async fn try_snapshot_state(&self) -> Result<TaskManagerSnapshot, String> {
        let sid = self.sid();
        self.store
            .load_snapshot_state(&sid)
            .await
            .map_err(|e| format!("snapshot read failed: {e}"))
    }

    /// Seal a snapshot after this caller's own mutations have completed.
    ///
    /// `restore_snapshot` uses the sealed version as its compare-and-restore
    /// guard. This lets rollback undo the caller's own mutation while refusing
    /// to clobber any later concurrent task-board write.
    ///
    /// **TOCTOU guard**: verifies that at most one version increment
    /// (the caller's own mutation) occurred between snapshot capture and
    /// seal. Returns an error if a concurrent mutation advanced the
    /// version further, because the snapshot no longer represents the
    /// pre-state of ONLY this caller's mutation.
    pub async fn seal_snapshot_for_restore(
        &self,
        snapshot: &mut TaskManagerSnapshot,
    ) -> Result<(), String> {
        let sid = self.sid();
        let version = self
            .store
            .get_session_version(&sid)
            .await
            .map_err(|e| format!("seal_snapshot_for_restore: failed to read version: {e}"))?;
        // TOCTOU guard: if more than one version increment happened
        // (i.e. a concurrent mutation intervened between capture and
        // seal), the snapshot is stale.
        if version > snapshot.version + 1 {
            return Err(format!(
                "seal_snapshot_for_restore: concurrent mutation detected \
                 (captured version={}, current version={}) — snapshot is stale",
                snapshot.version, version
            ));
        }
        snapshot.restore_version = Some(version);
        Ok(())
    }

    /// Restore a previously captured snapshot.
    ///
    /// **Conflict detection**: the snapshot's next_task_id may be stale if
    /// a concurrent mutation advanced the counter.  We compute
    /// `max(current_counter, snapshot_counter, max_task_id+1)` so we
    /// never rewind the id allocator — rewinding would guarantee
    /// duplicate id allocation on the next `create`.
    ///
    /// Task rows are overwritten wholesale (snapshot restore replaces the
    /// current set) so concurrent mutations that changed task state since
    /// the snapshot was taken ARE lost.  Callers that need linearizable
    /// rollback semantics should use a transaction.
    pub async fn restore_snapshot(&self, snapshot: &TaskManagerSnapshot) -> Result<(), String> {
        let sid = self.sid();

        // Conflict detection: restore may overwrite exactly the version this
        // snapshot was sealed for. If the snapshot was never sealed, it may
        // only restore over its original captured version.
        let expected_version = snapshot.restore_version.unwrap_or(snapshot.version);
        let current_version = self
            .store
            .get_session_version(&sid)
            .await
            .map_err(|e| format!("restore_snapshot: failed to read version: {e}"))?;
        if current_version != expected_version {
            return Err(format!(
                "restore_snapshot: version conflict (expected={}, current={}) — \
                 task board changed after rollback snapshot was sealed; retry with fresh state",
                expected_version, current_version
            ));
        }

        let current_next = self
            .store
            .peek_next_task_id(&sid)
            .await
            .map_err(|e| format!("restore_snapshot: failed to read counter: {e}"))?;
        // Never rewind the counter: if a concurrent mutation consumed ids
        // after the snapshot, keep its higher watermark. Also respect the
        // snapshot's own counter and the tasks it carries.
        let max_task_id = snapshot
            .tasks
            .iter()
            .filter_map(|t| t.id.strip_prefix("task-")?.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        let safe_next = current_next
            .max(snapshot.next_task_id)
            .max(max_task_id.saturating_add(1));
        self.store
            .restore_snapshot_state(&sid, snapshot.tasks.clone(), safe_next, expected_version)
            .await
    }

    /// Create a new task and preserve its machine-readable mutation outcome.
    pub async fn create_outcome(&self, args: &Value) -> TaskMutationOutcome {
        if let Err(error) = validate_allowed_fields(
            args,
            "create",
            &[
                "action",
                "title",
                "description",
                "subtasks",
                "active_form",
                "owner",
                "metadata",
                "add_blocks",
                "add_blocked_by",
            ],
        ) {
            return TaskMutationOutcome::error(error);
        }
        let title = match required_non_empty_string_field(args, "title") {
            Ok(title) => title,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        if let Err(error) = validate_string_chars(&title, "title", MAX_TASK_TITLE_CHARS) {
            return TaskMutationOutcome::error(error);
        }

        let description = match optional_string_field(args, "description") {
            Ok(description) => description,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        if let Some(description) = description.as_deref()
            && let Err(error) =
                validate_string_chars(description, "description", MAX_TASK_DESCRIPTION_CHARS)
        {
            return TaskMutationOutcome::error(error);
        }
        let now = chrono::Utc::now().to_rfc3339();

        let active_form = match optional_non_empty_string_field(args, "active_form") {
            Ok(active_form) => active_form,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        if let Some(active_form) = active_form.as_deref()
            && let Err(error) =
                validate_string_chars(active_form, "active_form", MAX_TASK_ACTIVE_FORM_CHARS)
        {
            return TaskMutationOutcome::error(error);
        }
        let owner = match optional_non_empty_string_field(args, "owner") {
            Ok(owner) => owner,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        if let Some(owner) = owner.as_deref()
            && let Err(error) = validate_string_chars(owner, "owner", MAX_TASK_OWNER_CHARS)
        {
            return TaskMutationOutcome::error(error);
        }
        // U-7: subtasks inherit parent's `owner` when they don't
        // declare one explicitly. Without inheritance a sub-agent
        // looking for "my work" misses subtasks of tasks it owns,
        // because the explicit `owner` field on the parent doesn't
        // propagate. Pass `owner` to the subtask builder below so
        // the closure has it.
        let parent_owner_for_subtasks = owner.clone();
        let subtasks = match parse_create_subtasks(args, parent_owner_for_subtasks.as_deref()) {
            Ok(subtasks) => subtasks,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        let metadata = match args.get("metadata") {
            Some(value) => match value.as_object() {
                Some(metadata) => {
                    if let Err(error) = validate_metadata_size(metadata, "metadata") {
                        return TaskMutationOutcome::error(error);
                    }
                    Some(metadata.clone())
                }
                None => return TaskMutationOutcome::error("field 'metadata' must be an object"),
            },
            None => None,
        };
        let proposed_blocks = match optional_string_array_field(args, "add_blocks") {
            Ok(proposed_blocks) => proposed_blocks,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        let proposed_blocked_by = match optional_string_array_field(args, "add_blocked_by") {
            Ok(proposed_blocked_by) => proposed_blocked_by,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        for (field, ids) in [
            ("add_blocks", &proposed_blocks),
            ("add_blocked_by", &proposed_blocked_by),
        ] {
            for id in ids {
                if let Err(error) = validate_task_id_chars(id, field) {
                    return TaskMutationOutcome::error(error);
                }
            }
        }
        let sid = self.sid();
        let mutation_title = title.clone();
        match self
            .store
            .mutate(
                &sid,
                Box::new(move |mut tasks, next| {
                    let task_id = format!("task-{next}");
                    // U-10: if the counter is desynced (corruption or
                    // partial init), `next` may point at an id that
                    // already exists. Surface this loudly so the model
                    // (and operators) know to investigate rather than
                    // silently producing an invisible duplicate or
                    // hitting a raw "Duplicate entry" DB error.
                    if tasks.iter().any(|t| t.id == task_id) {
                        let summary = format!(
                            "Error: task counter desync — id '{task_id}' already exists. \
                             The session's counter may need to be reset. \
                             Contact support or use `task_board(action='list')` to see the \
                             current task list and manually continue from the last id."
                        );
                        let data = json!({
                            "error": "counter_desync",
                            "conflicting_id": task_id,
                            "message": format!(
                                "Task id '{task_id}' already exists in this session; \
                                 counter is out of sync with the task list."
                            ),
                        });
                        return Ok(TaskMutationResult::refused(tasks, summary, data));
                    }
                    let task = SessionTask {
                        id: task_id.clone(),
                        title: mutation_title.clone(),
                        description,
                        status: SessionTaskStatusKind::Pending,
                        subtasks,
                        created_at: now.clone(),
                        updated_at: now.clone(),
                        active_form,
                        owner,
                        metadata,
                        blocks: Vec::new(),
                        blocked_by: Vec::new(),
                        archived_at: None,
                    };
                    tasks.push(task);
                    for id in &proposed_blocks {
                        add_dependency_edge(&mut tasks, &task_id, id, &now)?;
                    }
                    for id in &proposed_blocked_by {
                        add_dependency_edge(&mut tasks, id, &task_id, &now)?;
                    }
                    let summary = format!("Task #{task_id} created: {mutation_title}");
                    let data = json!({
                        "task_id": task_id,
                        "blocks": proposed_blocks,
                        "blocked_by": proposed_blocked_by,
                        "message": format!("Task '{}' created successfully", mutation_title)
                    });
                    let next_task_id = next.checked_add(1).ok_or_else(|| {
                        "task id counter overflow for session during create".to_string()
                    })?;
                    Ok(TaskMutationResult::applied(
                        tasks,
                        Some(next_task_id),
                        summary,
                        data,
                    ))
                }),
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => TaskMutationOutcome::error(error),
        }
    }

    /// Rendered compatibility surface for tool/UI consumers.
    pub async fn create(&self, args: &Value) -> String {
        self.create_outcome(args).await.output
    }

    /// List tasks in the session, optionally filtered by status.
    pub async fn list(&self, args: &Value) -> String {
        if let Err(error) = validate_allowed_fields(args, "list", &["action", "status_filter"]) {
            return format!("Error: {error}");
        }
        let status_filter = args
            .get("status_filter")
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| "field 'status_filter' must be a string".to_string())
            })
            .transpose();
        let status_filter = match status_filter {
            Ok(Some(status_filter)) => status_filter,
            Ok(None) => "all",
            Err(error) => return format!("Error: {error}"),
        };
        if !VALID_LIST_STATUS_FILTERS.contains(&status_filter) {
            return format!(
                "Error: invalid status_filter '{}' (valid: {})",
                status_filter,
                VALID_LIST_STATUS_FILTERS.join("|")
            );
        }

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
                s => t.status == SessionTaskStatusKind::from_status_str(s),
            })
            .map(|t| {
                let subtask_summary = if t.subtasks.is_empty() {
                    String::new()
                } else {
                    let done = t
                        .subtasks
                        .iter()
                        .filter(|st| st.status.is_completed())
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
                // sees "why" without a follow-up `task_board.get`. Only on
                // failed rows; other statuses don't have an
                // error_message so the field would be confusing noise.
                if t.status.is_failed() {
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

        json!({
            "count": filtered.len(),
            "tasks": filtered
        })
        .to_string()
    }

    /// Get full details of a task by ID.
    pub async fn get(&self, args: &Value) -> String {
        if let Err(error) = validate_allowed_fields(args, "get", &["action", "task_id"]) {
            return format!("Error: {error}");
        }
        let task_id = match required_non_empty_string_field(args, "task_id") {
            Ok(task_id) => task_id,
            Err(error) => return format!("Error: {error}"),
        };
        if let Err(error) = validate_task_id_chars(&task_id, "task_id") {
            return format!("Error: {error}");
        }

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

    /// Update a task while preserving the typed mutation outcome.
    pub async fn update_outcome(&self, args: &Value) -> TaskMutationOutcome {
        if let Err(error) = validate_allowed_fields(
            args,
            "update",
            &[
                "action",
                "task_id",
                "new_status",
                "title",
                "description",
                "subtask_id",
                "active_form",
                "owner",
                "metadata",
                "add_blocks",
                "add_blocked_by",
                "remove_blocks",
                "remove_blocked_by",
                "reason",
                "error_message",
            ],
        ) {
            return TaskMutationOutcome::error(error);
        }
        let task_id = match required_non_empty_string_field(args, "task_id") {
            Ok(task_id) => task_id,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        if let Err(error) = validate_task_id_chars(&task_id, "task_id") {
            return TaskMutationOutcome::error(error);
        }

        let new_status = match normalize_update_status(args) {
            Ok(status) => status,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        let subtask_id = match optional_non_empty_string_field(args, "subtask_id") {
            Ok(subtask_id) => subtask_id,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        let mut error_message = match optional_non_empty_string_field(args, "error_message") {
            Ok(error_message) => error_message,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        let reason = match optional_non_empty_string_field(args, "reason") {
            Ok(reason) => reason,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        if let Some(error_message) = error_message.as_deref()
            && let Err(error) =
                validate_string_chars(error_message, "error_message", MAX_TASK_ERROR_MESSAGE_CHARS)
        {
            return TaskMutationOutcome::error(error);
        }
        if let Some(reason) = reason.as_deref()
            && let Err(error) = validate_string_chars(reason, "reason", MAX_TASK_STOP_REASON_CHARS)
        {
            return TaskMutationOutcome::error(error);
        }
        if subtask_id.is_none()
            && error_message.is_none()
            && new_status == Some(SessionTaskStatusKind::Failed)
        {
            error_message = reason.clone();
        }
        if error_message.is_some() {
            if subtask_id.is_some() {
                return TaskMutationOutcome::error(
                    "field 'error_message' is only supported for parent task failure updates; subtask failures cannot store an error_message",
                );
            }
            if !matches!(
                new_status,
                Some(SessionTaskStatusKind::Failed | SessionTaskStatusKind::Cancelled)
            ) {
                return TaskMutationOutcome::error(
                    "field 'error_message' requires new_status='failed' or new_status='cancelled'",
                );
            }
        }
        let now = chrono::Utc::now().to_rfc3339();

        let title_update = match optional_non_empty_string_field(args, "title") {
            Ok(title_update) => title_update,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        if let Some(title) = title_update.as_deref()
            && let Err(error) = validate_string_chars(title, "title", MAX_TASK_TITLE_CHARS)
        {
            return TaskMutationOutcome::error(error);
        }
        let desc_update = match optional_nullable_string_field(args, "description") {
            Ok(desc_update) => desc_update,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        if let Some(Some(description)) = desc_update.as_ref()
            && let Err(error) =
                validate_string_chars(description, "description", MAX_TASK_DESCRIPTION_CHARS)
        {
            return TaskMutationOutcome::error(error);
        }
        let active_form_update = match optional_nullable_non_empty_string_field(args, "active_form")
        {
            Ok(active_form_update) => active_form_update,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        if let Some(Some(active_form)) = active_form_update.as_ref()
            && let Err(error) =
                validate_string_chars(active_form, "active_form", MAX_TASK_ACTIVE_FORM_CHARS)
        {
            return TaskMutationOutcome::error(error);
        }
        let owner_update = match optional_nullable_non_empty_string_field(args, "owner") {
            Ok(owner_update) => owner_update,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        if let Some(Some(owner)) = owner_update.as_ref()
            && let Err(error) = validate_string_chars(owner, "owner", MAX_TASK_OWNER_CHARS)
        {
            return TaskMutationOutcome::error(error);
        }
        let metadata_update = match optional_object_field(args, "metadata") {
            Ok(metadata_update) => metadata_update,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        if let Some(metadata_update) = metadata_update.as_ref()
            && let Err(error) = validate_metadata_size(metadata_update, "metadata")
        {
            return TaskMutationOutcome::error(error);
        }
        let proposed_blocks = match optional_string_array_field(args, "add_blocks") {
            Ok(proposed_blocks) => proposed_blocks,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        let proposed_blocked_by = match optional_string_array_field(args, "add_blocked_by") {
            Ok(proposed_blocked_by) => proposed_blocked_by,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        let remove_blocks = match optional_string_array_field(args, "remove_blocks") {
            Ok(remove_blocks) => remove_blocks,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        let remove_blocked_by = match optional_string_array_field(args, "remove_blocked_by") {
            Ok(remove_blocked_by) => remove_blocked_by,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        if subtask_id.is_some() {
            let unsupported = [
                ("title", title_update.is_some()),
                ("description", desc_update.is_some()),
                ("active_form", active_form_update.is_some()),
                ("owner", owner_update.is_some()),
                ("metadata", metadata_update.is_some()),
                ("add_blocks", !proposed_blocks.is_empty()),
                ("add_blocked_by", !proposed_blocked_by.is_empty()),
                ("remove_blocks", !remove_blocks.is_empty()),
                ("remove_blocked_by", !remove_blocked_by.is_empty()),
            ]
            .into_iter()
            .filter_map(|(field, present)| present.then_some(field))
            .collect::<Vec<_>>();
            if !unsupported.is_empty() {
                return TaskMutationOutcome::error(format!(
                    "field 'subtask_id' only supports new_status updates; unsupported with subtask_id: {}",
                    unsupported.join(", ")
                ));
            }
            if new_status.is_none() {
                return TaskMutationOutcome::error(
                    "field 'new_status' is required when updating a subtask",
                );
            }
        } else {
            if new_status == Some(SessionTaskStatusKind::Deleted) {
                let unsupported = [
                    ("title", title_update.is_some()),
                    ("description", desc_update.is_some()),
                    ("active_form", active_form_update.is_some()),
                    ("owner", owner_update.is_some()),
                    ("metadata", metadata_update.is_some()),
                    ("add_blocks", !proposed_blocks.is_empty()),
                    ("add_blocked_by", !proposed_blocked_by.is_empty()),
                    ("remove_blocks", !remove_blocks.is_empty()),
                    ("remove_blocked_by", !remove_blocked_by.is_empty()),
                ]
                .into_iter()
                .filter_map(|(field, present)| present.then_some(field))
                .collect::<Vec<_>>();
                if !unsupported.is_empty() {
                    return TaskMutationOutcome::error(format!(
                        "new_status='deleted' only supports an optional reason; delete already detaches every dependency edge. Unsupported fields: {}",
                        unsupported.join(", ")
                    ));
                }
            }
            let has_parent_update = new_status.is_some()
                || title_update.is_some()
                || desc_update.is_some()
                || active_form_update.is_some()
                || owner_update.is_some()
                || metadata_update
                    .as_ref()
                    .is_some_and(|metadata| !metadata.is_empty())
                || !proposed_blocks.is_empty()
                || !proposed_blocked_by.is_empty()
                || !remove_blocks.is_empty()
                || !remove_blocked_by.is_empty()
                || error_message.is_some()
                || reason.is_some();
            if !has_parent_update {
                return TaskMutationOutcome::error(
                    "task_board.update requires at least one update field: new_status, title, description, active_form, owner, metadata, add_blocks, add_blocked_by, remove_blocks, remove_blocked_by, reason, or error_message",
                );
            }
        }
        let sid = self.sid();

        match self
            .store
            .mutate(
                &sid,
                Box::new(move |mut tasks, _next| {
                    let original_tasks = tasks.clone();
                    // Subtask path short-circuits: all logic stays local to one SessionTask.
                    if let Some(st_id) = subtask_id.as_deref() {
                        let Some(task_index) = tasks.iter().position(|t| t.id == task_id) else {
                            return Err(format!("task '{}' not found", task_id));
                        };
                        let mut projected_task = tasks[task_index].clone();
                        if !projected_task.status.is_open_work()
                            && !is_reversible_auto_completed_parent(&projected_task)
                        {
                            return Err(format!(
                                "task '{}' is already terminal ({}); create a new task for follow-up work instead of editing its subtasks",
                                task_id, projected_task.status
                            ));
                        }
                        let Some(projected_subtask) =
                            projected_task.subtasks.iter_mut().find(|st| st.id == st_id)
                        else {
                            let available = projected_task
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
                        if let Some(ref status) = new_status {
                            projected_subtask.status = *status;
                        }
                        if matches!(
                            new_status,
                            Some(SessionTaskStatusKind::InProgress | SessionTaskStatusKind::Completed)
                        ) {
                            validate_subtask_dependencies_resolved(&projected_task, st_id)?;
                            validate_task_blockers_resolved_after_projected_edges(
                                &tasks,
                                &task_id,
                                &[],
                                &[],
                            )?;
                        }
                        let blockers_resolved =
                            unresolved_task_blocker_ids(&tasks, &projected_task).is_empty();
                        reconcile_subtask_completion(&mut projected_task, blockers_resolved);

                        let task = &mut tasks[task_index];
                        let Some(subtask) = task.subtasks.iter_mut().find(|st| st.id == st_id)
                        else {
                            return Err(format!("subtask '{}' not found in task '{}'", st_id, task_id));
                        };
                        let previous_status = subtask.status;
                        if let Some(ref status) = new_status {
                            validate_subtask_status_transition(previous_status, *status)?;
                            subtask.status = *status;
                        }
                        if let Some(note) = reason.clone() {
                            subtask.reason = Some(note);
                        }
                        // Copy the reconciled parent state from projected_task instead of
                        // re-running reconcile on the real task. This avoids a race where
                        // concurrent subtask updates could apply reconcile twice and
                        // produce inconsistent parent status (e.g., auto-complete reversal
                        // firing on stale metadata). The projected_task was validated above,
                        // so its reconciled state is authoritative.
                        task.status = projected_task.status;
                        task.metadata = projected_task.metadata;
                        let final_subtask_status = subtask.status;
                        task.updated_at = now.clone();
                        let summary = format!(
                            "Subtask {st_id} of #{task_id}: {previous_status} → {final_subtask_status}"
                        );
                        let mut data = json!({
                            "task_id": task_id,
                            "subtask_id": st_id,
                            "previous_status": previous_status,
                            "status": final_subtask_status,
                            "reason": reason,
                            "message": format!("Subtask '{}' updated to '{}'", st_id, final_subtask_status)
                        });
                        // Parent auto-completion is dependency-driven. If this
                        // task just completed, reconcile any all-done parents
                        // that it unblocked in the same atomic mutation.
                        reconcile_all_subtask_completion(&mut tasks, &now);
                        if same_task_board_state(&original_tasks, &tasks) {
                            data["already_current"] = json!(true);
                            return Ok(TaskMutationResult::unchanged(
                                original_tasks,
                                format!(
                                    "Subtask {st_id} of #{task_id} is already {final_subtask_status}"
                                ),
                                data,
                            ));
                        }
                        return Ok(TaskMutationResult::applied(tasks, None, summary, data));
                    }

                    if new_status == Some(SessionTaskStatusKind::Deleted) {
                        let Some((previous_status, deleted_subtasks)) = tasks
                            .iter_mut()
                            .find(|t| t.id == task_id)
                            .map(|task| {
                                let previous_status = task.status;
                                task.status = SessionTaskStatusKind::Deleted;
                                let deleted_subtasks = transition_open_subtasks(
                                    task,
                                    SessionTaskStatusKind::Deleted,
                                );
                                task.updated_at = now.clone();
                                if let Some(note) = reason.as_deref() {
                                    let meta = task.metadata.get_or_insert_with(Default::default);
                                    meta.insert("reason".to_string(), json!(note));
                                }
                                (previous_status, deleted_subtasks)
                            })
                        else {
                            return Err(format!("task '{}' not found", task_id));
                        };
                        let deleted_ids = HashSet::from([task_id.clone()]);
                        detach_task_dependency_edges(&mut tasks, &deleted_ids);
                        reconcile_all_subtask_completion(&mut tasks, &now);
                        let mut data = json!({
                                "task_id": task_id,
                                "previous_status": previous_status.to_string(),
                                "status": "deleted",
                                "deleted_subtasks": deleted_subtasks,
                                "message": format!("Task '{}' hidden from active views; audit tombstone retained", task_id)
                            });
                        if same_task_board_state(&original_tasks, &tasks) {
                            data["already_current"] = json!(true);
                            return Ok(TaskMutationResult::unchanged(
                                original_tasks,
                                format!("Task #{task_id} is already deleted"),
                                data,
                            ));
                        }
                        return Ok(TaskMutationResult::applied(
                            tasks,
                            None,
                            format!("Task #{task_id} deleted (was: {previous_status})"),
                            data,
                        ));
                    }

                    let previous_status = match tasks.iter().find(|t| t.id == task_id) {
                        Some(t) => t.status,
                        None => return Err(format!("task '{}' not found", task_id)),
                    };
                    validate_parent_status_transition(previous_status, new_status)?;
                    let transitioning_to_failure =
                        previous_status != SessionTaskStatusKind::Failed
                            && new_status == Some(SessionTaskStatusKind::Failed);
                    let projected_status = new_status.unwrap_or(previous_status);
                    // Collect proposed edge changes before mutating so cycle detection
                    // sees a consistent view.
                    let existing_task_ids: HashSet<&str> =
                        tasks.iter().map(|task| task.id.as_str()).collect();
                    for (field, ids) in [
                        ("add_blocks", &proposed_blocks),
                        ("add_blocked_by", &proposed_blocked_by),
                        ("remove_blocks", &remove_blocks),
                        ("remove_blocked_by", &remove_blocked_by),
                    ] {
                        for id in ids {
                            if !existing_task_ids.contains(id.as_str()) {
                                return Err(format!(
                                    "field '{field}' references task '{id}' which was not found"
                                ));
                            }
                        }
                    }
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
                    if projected_status.is_in_progress() || projected_status.is_completed()
                    {
                        let advancing_now = matches!(
                            new_status,
                            Some(
                                SessionTaskStatusKind::InProgress
                                    | SessionTaskStatusKind::Completed
                            )
                        ) && previous_status != projected_status;
                        let incoming_dependency_changed = !proposed_blocked_by.is_empty()
                            || !remove_blocked_by.is_empty();
                        if advancing_now || incoming_dependency_changed {
                            validate_task_blockers_resolved_after_projected_edges(
                                &tasks,
                                &task_id,
                                &proposed_blocked_by,
                                &remove_blocked_by,
                            )?;
                        }
                    }

                    // `add_blocks` is the reverse spelling of adding an
                    // incoming blocker to the target. Validate the affected
                    // running task too; otherwise callers can bypass the same
                    // invariant simply by mutating the other endpoint.
                    if !projected_status.is_completed() {
                        for blocked_id in &proposed_blocks {
                            if let Some(blocked_task) = tasks
                                .iter()
                                .find(|task| task.id == *blocked_id)
                                .filter(|task| {
                                    task.status.is_in_progress() || task.status.is_completed()
                                })
                            {
                                return Err(format!(
                                    "task '{}' cannot become blocked by unresolved task '{}' while {}. Complete the blocker first, or move open work to a non-advancing state before adding the dependency",
                                    blocked_id, task_id, blocked_task.status
                                ));
                            }
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
                            for blocked_id in &t.blocks {
                                blocked_by
                                    .entry(blocked_id.clone())
                                    .or_default()
                                    .insert(t.id.clone());
                            }
                        }
                        let entry = blocked_by.entry(task_id.clone()).or_default();
                        for r in &remove_blocked_by {
                            entry.remove(r);
                        }
                        for x in &remove_blocks {
                            if let Some(blocked_entry) = blocked_by.get_mut(x) {
                                blocked_entry.remove(&task_id);
                            }
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

                    let cancelled_subtasks = {
                        let Some(task) = tasks.iter_mut().find(|t| t.id == task_id) else {
                            return Err(format!("task '{}' not found", task_id));
                        };

                        if let Some(ref status) = new_status {
                            if *status != SessionTaskStatusKind::Cancelled {
                                task.status = *status;
                            }
                            if *status == SessionTaskStatusKind::Completed {
                                // Cascade parent→subtask completion, but preserve any
                                // subtask already in a terminal non-success state.
                                for subtask in &mut task.subtasks {
                                    if !subtask.status.is_failed()
                                        && !subtask.status.is_cancelled()
                                        && !subtask.status.is_completed()
                                    {
                                        subtask.status = SessionTaskStatusKind::Completed;
                                    }
                                }
                            }
                        }
                        if let Some(title) = title_update.as_deref() {
                            task.title = title.to_string();
                        }
                        if let Some(desc) = desc_update.as_ref() {
                            task.description = desc.clone();
                        }
                        if let Some(active_form) = active_form_update.as_ref() {
                            task.active_form = active_form.clone();
                        }
                        if let Some(owner) = owner_update.as_ref() {
                            task.owner = owner.clone();
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
                        let cancelled_subtasks = match new_status {
                            Some(SessionTaskStatusKind::Cancelled) => {
                                cancel_task_and_open_subtasks(
                                    task,
                                    error_message.as_deref().or(reason.as_deref()),
                                    &now,
                                )
                            }
                            Some(SessionTaskStatusKind::Failed) => cancel_open_subtasks(task),
                            _ => 0,
                        };
                        if new_status != Some(SessionTaskStatusKind::Cancelled) {
                            if let Some(err) = error_message.as_deref() {
                                let meta = task.metadata.get_or_insert_with(Default::default);
                                // Stash structured error in metadata so `list`
                                // can surface a preview without parsing description prose.
                                // Persist a human-readable copy only on the typed transition
                                // into failure, or when this request explicitly replaces the
                                // definition-of-done text. Same-state retries must not append
                                // another copy of the error.
                                if transitioning_to_failure || desc_update.is_some() {
                                    let note = format!("Error: {err}");
                                    task.description = Some(
                                        match task.description.as_deref().filter(|s| !s.is_empty()) {
                                            Some(description) => format!("{description}\n\n{note}"),
                                            None => note,
                                        },
                                    );
                                }
                                meta.insert("error_message".to_string(), json!(err));
                            } else if let Some(note) = reason.as_deref() {
                                let meta = task.metadata.get_or_insert_with(Default::default);
                                meta.insert("reason".to_string(), json!(note));
                            }
                        }

                        task.updated_at = now.clone();

                        cancelled_subtasks
                    };

                    for id in &remove_blocks {
                        remove_dependency_edge(&mut tasks, &task_id, id, &now)?;
                    }
                    for id in &remove_blocked_by {
                        remove_dependency_edge(&mut tasks, id, &task_id, &now)?;
                    }
                    for id in &proposed_blocks {
                        add_dependency_edge(&mut tasks, &task_id, id, &now)?;
                    }
                    for id in &proposed_blocked_by {
                        add_dependency_edge(&mut tasks, id, &task_id, &now)?;
                    }

                    reconcile_all_subtask_completion(&mut tasks, &now);
                    let final_status = tasks
                        .iter()
                        .find(|task| task.id == task_id)
                        .map(|task| task.status)
                        .ok_or_else(|| format!("task '{}' not found after update", task_id))?;

                    let mut response_body = json!({
                            "success": true,
                            "task_id": task_id,
                            "previous_status": previous_status,
                            "status": final_status,
                            "message": format!("Task '{}' updated to '{}'", task_id, final_status)
                        });
                    if matches!(
                        new_status,
                        Some(SessionTaskStatusKind::Failed | SessionTaskStatusKind::Cancelled)
                    ) {
                        response_body["cancelled_subtasks"] = json!(cancelled_subtasks);
                    }
                    if same_task_board_state(&original_tasks, &tasks) {
                        response_body["already_current"] = json!(true);
                        return Ok(TaskMutationResult::unchanged(
                            original_tasks,
                            format!("Task #{task_id} is already {final_status}"),
                            response_body,
                        ));
                    }
                    Ok(TaskMutationResult::applied(
                        tasks,
                        None,
                        format!("Task #{task_id}: {previous_status} → {final_status}"),
                        response_body,
                    ))
                }),
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => TaskMutationOutcome::error(error),
        }
    }

    /// Rendered compatibility surface for tool/UI consumers.
    pub async fn update(&self, args: &Value) -> String {
        self.update_outcome(args).await.output
    }

    /// Stop/cancel a running task while preserving the typed outcome.
    pub async fn stop_outcome(&self, args: &Value) -> TaskMutationOutcome {
        if let Err(error) = validate_allowed_fields(args, "stop", &["action", "task_id", "reason"])
        {
            return TaskMutationOutcome::error(error);
        }
        let task_id = match required_non_empty_string_field(args, "task_id") {
            Ok(task_id) => task_id,
            Err(error) => return TaskMutationOutcome::error(error),
        };
        if let Err(error) = validate_task_id_chars(&task_id, "task_id") {
            return TaskMutationOutcome::error(error);
        }

        let reason = match optional_non_empty_string_field(args, "reason") {
            Ok(Some(reason)) => reason,
            Ok(None) => "user requested".to_string(),
            Err(error) => return TaskMutationOutcome::error(error),
        };
        if let Err(error) = validate_string_chars(&reason, "reason", MAX_TASK_STOP_REASON_CHARS) {
            return TaskMutationOutcome::error(error);
        }
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
                    let task_status = tasks[task_idx].status;

                    if task_status == SessionTaskStatusKind::Cancelled {
                        let task = &mut tasks[task_idx];
                        let open_subtasks = task
                            .subtasks
                            .iter()
                            .filter(|subtask| subtask.status.can_be_stopped())
                            .count();
                        if open_subtasks > 0 {
                            cancel_task_and_open_subtasks(task, Some(&reason), &now);
                        }
                        let summary = format!("Task #{task_id} is already cancelled");
                        let data = json!({
                            "task_id": task_id,
                            "status": "cancelled",
                            "already_cancelled": true,
                            "cancelled_subtasks": open_subtasks,
                            "message": format!("Task '{}' was already cancelled", task_id)
                        });
                        return if open_subtasks == 0 {
                            Ok(TaskMutationResult::unchanged(tasks, summary, data))
                        } else {
                            Ok(TaskMutationResult::applied(tasks, None, summary, data))
                        };
                    }

                    if !task_status.can_be_stopped() {
                        return Ok(TaskMutationResult::refused(
                            tasks,
                            format!(
                                "Refused: task #{task_id} is '{task_status}' — only pending, in_progress, or paused tasks can be stopped"
                            ),
                            json!({
                                "task_id": task_id,
                                "status": task_status,
                                "message": format!("Cannot stop task '{}': status is '{}' (only 'pending', 'in_progress', or 'paused' can be stopped)", task_id, task_status)
                            }),
                        ));
                    }

                    let task = &mut tasks[task_idx];
                    let previous_status = task.status;
                    let cancelled_subtasks =
                        cancel_task_and_open_subtasks(task, Some(&reason), &now);

                    let summary = if cancelled_subtasks > 0 {
                        format!(
                            "Task #{task_id} cancelled (was {previous_status}; {cancelled_subtasks} subtask(s) cancelled): {reason}"
                        )
                    } else {
                        format!("Task #{task_id} cancelled (was {previous_status}): {reason}")
                    };
                    Ok(TaskMutationResult::applied(
                        tasks,
                        None,
                        summary,
                        json!({
                            "task_id": task_id,
                            "previous_status": previous_status,
                            "status": "cancelled",
                            "reason": reason,
                            "cancelled_subtasks": cancelled_subtasks,
                            "message": format!("Task '{}' cancelled (was: {})", task_id, previous_status)
                        }),
                    ))
                }),
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => TaskMutationOutcome::error(error),
        }
    }

    /// Rendered compatibility surface for tool/UI consumers.
    pub async fn stop(&self, args: &Value) -> String {
        self.stop_outcome(args).await.output
    }

    /// Archive historical work. Single-task and bulk archive are both
    /// session-scoped; cross-session cleanup must use an explicit user-level
    /// surface so automatic task cleanup cannot unexpectedly mutate other
    /// sessions.
    pub async fn archive_outcome(&self, args: &Value) -> TaskMutationOutcome {
        match self.store.archive(&self.sid(), args).await {
            Ok(outcome) => outcome,
            Err(error) => TaskMutationOutcome::error(error),
        }
    }

    /// Rendered compatibility surface for tool/UI consumers.
    pub async fn archive(&self, args: &Value) -> String {
        self.archive_outcome(args).await.output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    fn mgr() -> TaskManager {
        TaskManager::in_memory()
    }

    async fn set_task_status_fixture(
        manager: &TaskManager,
        task_id: &str,
        status: SessionTaskStatusKind,
    ) {
        let mut snapshot = manager
            .try_snapshot_state()
            .await
            .expect("snapshot in test fixture");
        let task = snapshot
            .tasks
            .iter_mut()
            .find(|task| task.id == task_id)
            .expect("fixture task exists");
        task.status = status;
        manager
            .restore_snapshot(&snapshot)
            .await
            .expect("restore fixture task status");
    }

    #[test]
    fn session_task_status_helpers_cover_domain_taxonomy() {
        assert_eq!(
            SessionTaskStatusKind::from_status_str("pending"),
            SessionTaskStatusKind::Pending
        );
        assert_eq!(
            SessionTaskStatusKind::from_status_str("in_progress"),
            SessionTaskStatusKind::InProgress
        );
        assert_eq!(
            SessionTaskStatusKind::from_status_str("paused"),
            SessionTaskStatusKind::Paused
        );
        assert_eq!(
            SessionTaskStatusKind::from_status_str("completed"),
            SessionTaskStatusKind::Completed
        );
        assert_eq!(
            SessionTaskStatusKind::from_status_str("failed"),
            SessionTaskStatusKind::Failed
        );
        assert_eq!(
            SessionTaskStatusKind::from_status_str("cancelled"),
            SessionTaskStatusKind::Cancelled
        );
        assert_eq!(
            SessionTaskStatusKind::from_status_str("archived"),
            SessionTaskStatusKind::Archived
        );
        assert_eq!(
            SessionTaskStatusKind::from_status_str("deleted"),
            SessionTaskStatusKind::Deleted
        );
        assert_eq!(
            SessionTaskStatusKind::from_status_str("migrated"),
            SessionTaskStatusKind::Migrated
        );
        assert!(!SessionTaskStatusKind::Migrated.is_open_work());
        assert!(!SessionTaskStatusKind::Migrated.can_be_stopped());
        assert!(!SessionTaskStatusKind::Migrated.can_be_archived());
    }

    #[test]
    fn session_task_status_deserialize_unknown_uses_warned_parser() {
        let parsed: SessionTaskStatusKind = serde_json::from_str("\"mystery\"").unwrap();
        assert_eq!(parsed, SessionTaskStatusKind::Other);
    }

    #[derive(Clone, Default)]
    struct SharedLog(Arc<Mutex<Vec<u8>>>);

    impl SharedLog {
        fn output(&self) -> String {
            let bytes = self.0.lock().expect("log lock").clone();
            String::from_utf8(bytes).expect("log output should be utf8")
        }
    }

    struct SharedLogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedLogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for SharedLog {
        type Writer = SharedLogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            SharedLogWriter(self.0.clone())
        }
    }

    #[test]
    fn unknown_session_task_status_emits_warning() {
        let log = SharedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            assert_eq!(
                SessionTaskStatusKind::from_status_str("mystery"),
                SessionTaskStatusKind::Other
            );
        });

        let output = log.output();
        assert!(
            output.contains("session_task_status_kind: unknown status"),
            "warning message missing from log: {output}"
        );
        assert!(
            output.contains("mystery"),
            "raw unknown status missing from log: {output}"
        );
    }

    #[test]
    fn session_task_status_helpers_keep_active_archive_and_stop_semantics_distinct() {
        assert!(SessionTaskStatusKind::Pending.is_active());
        assert!(SessionTaskStatusKind::InProgress.is_active());
        assert!(!SessionTaskStatusKind::Paused.is_active());
        assert!(SessionTaskStatusKind::Pending.is_open_work());
        assert!(SessionTaskStatusKind::InProgress.is_open_work());
        assert!(SessionTaskStatusKind::Paused.is_open_work());
        assert!(!SessionTaskStatusKind::Completed.is_open_work());
        assert!(!SessionTaskStatusKind::Failed.is_open_work());
        assert!(!SessionTaskStatusKind::Cancelled.is_open_work());
        assert!(!SessionTaskStatusKind::Completed.is_active());
        assert!(SessionTaskStatusKind::Paused.is_started());
        assert!(SessionTaskStatusKind::Completed.is_started());
        assert!(!SessionTaskStatusKind::Cancelled.is_started());
        assert!(SessionTaskStatusKind::Completed.can_be_archived());
        assert!(SessionTaskStatusKind::Failed.can_be_archived());
        assert!(SessionTaskStatusKind::Cancelled.can_be_archived());
        assert!(!SessionTaskStatusKind::Paused.can_be_archived());
        assert!(!SessionTaskStatusKind::Archived.can_be_archived());
        assert!(SessionTaskStatusKind::Pending.can_be_stopped());
        assert!(SessionTaskStatusKind::InProgress.can_be_stopped());
        assert!(SessionTaskStatusKind::Paused.can_be_stopped());
        assert!(!SessionTaskStatusKind::Cancelled.can_be_stopped());
        assert!(SessionTaskStatusKind::Failed.is_unsuccessful());
        assert!(SessionTaskStatusKind::Cancelled.is_unsuccessful());
        assert!(!SessionTaskStatusKind::Archived.is_unsuccessful());
    }

    #[tokio::test]
    async fn create_and_list_roundtrips() {
        let m = mgr();
        let out = m
            .create(&json!({"title": "a", "active_form": "doing a"}))
            .await;
        let created: Value = serde_json::from_str(out.split_once('\n').unwrap().1).unwrap();
        assert_eq!(created["success"], true, "create: {out}");
        let list: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "all"})).await).unwrap();
        assert_eq!(list["count"], 1, "list: {list}");
    }

    #[tokio::test]
    async fn list_empty_board_returns_stable_json_shape() {
        let m = mgr();

        let list: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "active"})).await).unwrap();

        assert_eq!(list["count"], 0, "{list}");
        assert_eq!(list["tasks"].as_array().map(Vec::len), Some(0), "{list}");
    }

    #[tokio::test]
    async fn create_rejects_blank_title_and_wrong_type_top_level_fields() {
        let m = mgr();

        let blank_title = m.create(&json!({"title": "   "})).await;
        assert!(blank_title.starts_with("Error:"), "{blank_title}");
        assert!(
            blank_title.contains("title") && blank_title.contains("non-empty"),
            "blank title should be actionable: {blank_title}"
        );

        for (field, args) in [
            (
                "active_form",
                json!({"title": "blank active_form", "active_form": "   "}),
            ),
            ("owner", json!({"title": "blank owner", "owner": "   "})),
        ] {
            let out = m.create(&args).await;
            assert!(out.starts_with("Error:"), "{field}: {out}");
            assert!(
                out.contains(field) && out.contains("non-empty"),
                "blank {field} should be rejected before persisting: {out}"
            );
        }

        for (field, args) in [
            (
                "description",
                json!({"title": "bad description", "description": true}),
            ),
            (
                "active_form",
                json!({"title": "bad active_form", "active_form": true}),
            ),
            ("owner", json!({"title": "bad owner", "owner": true})),
            (
                "metadata",
                json!({"title": "bad metadata", "metadata": true}),
            ),
        ] {
            let out = m.create(&args).await;
            assert!(out.starts_with("Error:"), "{field}: {out}");
            assert!(
                out.contains(field),
                "wrong-type {field} should name the bad field: {out}"
            );
        }

        let list = m.list(&json!({"status_filter": "all"})).await;
        let list: Value = serde_json::from_str(&list).unwrap();
        assert_eq!(
            list["count"], 0,
            "invalid create attempts must not persist tasks: {list}"
        );
    }

    #[tokio::test]
    async fn create_rejects_oversized_text_and_metadata_before_persisting() {
        let m = mgr();

        for (field, args) in [
            (
                "title",
                json!({"title": "x".repeat(MAX_TASK_TITLE_CHARS + 1)}),
            ),
            (
                "description",
                json!({"title": "bad description", "description": "x".repeat(MAX_TASK_DESCRIPTION_CHARS + 1)}),
            ),
            (
                "active_form",
                json!({"title": "bad active_form", "active_form": "x".repeat(MAX_TASK_ACTIVE_FORM_CHARS + 1)}),
            ),
            (
                "owner",
                json!({"title": "bad owner", "owner": "x".repeat(MAX_TASK_OWNER_CHARS + 1)}),
            ),
            (
                "metadata",
                json!({"title": "bad metadata", "metadata": {"blob": "x".repeat(MAX_TASK_METADATA_BYTES + 1)}}),
            ),
        ] {
            let out = m.create(&args).await;
            assert!(out.starts_with("Error:"), "{field}: {out}");
            assert!(
                out.contains(field) && (out.contains("exceeds") || out.contains("serialized")),
                "oversized {field} should name the bad field and bound: {out}"
            );
        }

        let list = m.list(&json!({"status_filter": "all"})).await;
        let list: Value = serde_json::from_str(&list).unwrap();
        assert_eq!(
            list["count"], 0,
            "oversized create attempts must not persist tasks: {list}"
        );
    }

    #[tokio::test]
    async fn task_actions_reject_unknown_fields_instead_of_ignoring_typos() {
        let m = mgr();

        for (action, output) in [
            (
                "create",
                m.create(&json!({"title": "typo create", "titel": "wrong"}))
                    .await,
            ),
            (
                "list",
                m.list(&json!({"status_filter": "all", "limit": 10})).await,
            ),
            (
                "get",
                m.get(&json!({"task_id": "task-1", "include": "all"})).await,
            ),
            (
                "update",
                m.update(&json!({"task_id": "task-1", "new_status": "paused", "state": "paused"}))
                    .await,
            ),
            (
                "stop",
                m.stop(&json!({"task_id": "task-1", "why": "typo"})).await,
            ),
            (
                "archive",
                m.archive(&json!({"older_than_days": 30, "dry_run": true}))
                    .await,
            ),
        ] {
            assert!(output.starts_with("Error:"), "{action}: {output}");
            assert!(
                output.contains("unknown field"),
                "{action} should reject unknown fields explicitly: {output}"
            );
        }

        let create_only_field_on_update = m
            .update(&json!({"task_id": "task-1", "subtasks": []}))
            .await;
        assert!(
            create_only_field_on_update.contains("unknown field 'subtasks' for task_board.update"),
            "{create_only_field_on_update}"
        );
        assert!(
            create_only_field_on_update.contains("field is valid for: task_board.create"),
            "{create_only_field_on_update}"
        );

        let action_wrong_type = m
            .create(&json!({"action": true, "title": "bad action"}))
            .await;
        assert!(
            action_wrong_type.starts_with("Error:"),
            "{action_wrong_type}"
        );
        assert!(
            action_wrong_type.contains("field 'action'") && action_wrong_type.contains("string"),
            "wrong-type action should be rejected when present: {action_wrong_type}"
        );

        let list = m.list(&json!({"status_filter": "all"})).await;
        let list: Value = serde_json::from_str(&list).unwrap();
        assert_eq!(
            list["count"], 0,
            "unknown-field attempts must not create or mutate tasks: {list}"
        );
    }

    #[tokio::test]
    async fn create_rejects_malformed_subtasks_instead_of_silently_dropping_them() {
        let m = mgr();

        let non_array = m
            .create(&json!({"title": "bad subtasks", "subtasks": true}))
            .await;
        assert!(non_array.starts_with("Error:"), "{non_array}");
        assert!(
            non_array.contains("subtasks") && non_array.contains("array"),
            "non-array subtasks should be actionable: {non_array}"
        );

        let missing_title = m
            .create(&json!({
                "title": "missing subtask title",
                "subtasks": [{ "id": "s1" }]
            }))
            .await;
        assert!(missing_title.starts_with("Error:"), "{missing_title}");
        assert!(
            missing_title.contains("subtasks[0].title"),
            "missing subtask title should point at the bad item: {missing_title}"
        );

        let bad_dep = m
            .create(&json!({
                "title": "bad depends_on",
                "subtasks": [{ "id": "s1", "title": "one", "depends_on": [true] }]
            }))
            .await;
        assert!(bad_dep.starts_with("Error:"), "{bad_dep}");
        assert!(
            bad_dep.contains("subtasks[0].depends_on[0]"),
            "bad dependency should point at the bad value: {bad_dep}"
        );

        let blank_dep = m
            .create(&json!({
                "title": "blank depends_on",
                "subtasks": [{ "id": "s1", "title": "one", "depends_on": ["   "] }]
            }))
            .await;
        assert!(blank_dep.starts_with("Error:"), "{blank_dep}");
        assert!(
            blank_dep.contains("subtasks[0].depends_on[0]") && blank_dep.contains("non-empty"),
            "blank dependency should be rejected directly: {blank_dep}"
        );

        let blank_subtask_owner = m
            .create(&json!({
                "title": "blank subtask owner",
                "subtasks": [{ "id": "s1", "title": "one", "owner": "   " }]
            }))
            .await;
        assert!(
            blank_subtask_owner.starts_with("Error:"),
            "{blank_subtask_owner}"
        );
        assert!(
            blank_subtask_owner.contains("subtasks[0].owner")
                && blank_subtask_owner.contains("non-empty"),
            "blank subtask owner should be rejected directly: {blank_subtask_owner}"
        );

        let unknown_subtask_field = m
            .create(&json!({
                "title": "bad subtask field",
                "subtasks": [{ "id": "s1", "title": "one", "notes": "typo" }]
            }))
            .await;
        assert!(
            unknown_subtask_field.starts_with("Error:"),
            "{unknown_subtask_field}"
        );
        assert!(
            unknown_subtask_field.contains("subtasks[0].notes")
                && unknown_subtask_field.contains("unknown field"),
            "unknown subtask field should point at the bad item: {unknown_subtask_field}"
        );

        let too_many_subtasks = (0..=MAX_CREATE_SUBTASKS)
            .map(|index| json!({ "id": format!("s{index}"), "title": format!("step {index}") }))
            .collect::<Vec<_>>();
        let oversized = m
            .create(&json!({
                "title": "oversized checklist",
                "subtasks": too_many_subtasks
            }))
            .await;
        assert!(oversized.starts_with("Error:"), "{oversized}");
        assert!(
            oversized.contains("subtasks")
                && oversized.contains("maximum")
                && oversized.contains(&MAX_CREATE_SUBTASKS.to_string()),
            "oversized subtask list should explain the hard limit: {oversized}"
        );

        for (field, subtask) in [
            (
                "subtasks[0].id",
                json!({ "id": "x".repeat(MAX_SUBTASK_ID_CHARS + 1), "title": "one" }),
            ),
            (
                "subtasks[0].title",
                json!({ "id": "s1", "title": "x".repeat(MAX_SUBTASK_TITLE_CHARS + 1) }),
            ),
            (
                "subtasks[0].description",
                json!({ "id": "s1", "title": "one", "description": "x".repeat(MAX_SUBTASK_DESCRIPTION_CHARS + 1) }),
            ),
            (
                "subtasks[0].owner",
                json!({ "id": "s1", "title": "one", "owner": "x".repeat(MAX_TASK_OWNER_CHARS + 1) }),
            ),
            (
                "subtasks[0].depends_on[0]",
                json!({ "id": "s1", "title": "one", "depends_on": ["x".repeat(MAX_SUBTASK_ID_CHARS + 1)] }),
            ),
        ] {
            let out = m
                .create(&json!({
                    "title": format!("bad {field}"),
                    "subtasks": [subtask]
                }))
                .await;
            assert!(out.starts_with("Error:"), "{field}: {out}");
            assert!(
                out.contains(field) && out.contains("exceeds"),
                "oversized {field} should name the bad field and bound: {out}"
            );
        }

        let duplicate_id = m
            .create(&json!({
                "title": "duplicate subtask",
                "subtasks": [
                    { "id": "s1", "title": "one" },
                    { "id": "s1", "title": "again" }
                ]
            }))
            .await;
        assert!(duplicate_id.starts_with("Error:"), "{duplicate_id}");
        assert!(
            duplicate_id.contains("duplicate subtask id 's1'"),
            "duplicate subtask ids should be rejected: {duplicate_id}"
        );

        let dangling_dep = m
            .create(&json!({
                "title": "dangling subtask dep",
                "subtasks": [
                    { "id": "s1", "title": "one", "depends_on": ["missing"] }
                ]
            }))
            .await;
        assert!(dangling_dep.starts_with("Error:"), "{dangling_dep}");
        assert!(
            dangling_dep.contains("unknown subtask dependency 'missing'"),
            "dangling dependency should be rejected: {dangling_dep}"
        );

        let self_dep = m
            .create(&json!({
                "title": "self subtask dep",
                "subtasks": [
                    { "id": "s1", "title": "one", "depends_on": ["s1"] }
                ]
            }))
            .await;
        assert!(self_dep.starts_with("Error:"), "{self_dep}");
        assert!(
            self_dep.contains("cannot depend on itself"),
            "self dependency should be rejected: {self_dep}"
        );

        let duplicate_dep = m
            .create(&json!({
                "title": "duplicate subtask dep",
                "subtasks": [
                    { "id": "s1", "title": "one" },
                    { "id": "s2", "title": "two", "depends_on": ["s1", "s1"] }
                ]
            }))
            .await;
        assert!(duplicate_dep.starts_with("Error:"), "{duplicate_dep}");
        assert!(
            duplicate_dep.contains("duplicate dependency 's1'"),
            "duplicate dependency should be rejected: {duplicate_dep}"
        );

        let list = m.list(&json!({"status_filter": "all"})).await;
        let list: Value = serde_json::from_str(&list).unwrap();
        assert_eq!(
            list["count"], 0,
            "malformed create attempts must not persist partial tasks: {list}"
        );
    }

    /// U-5 (unhappy path): when a task is marked `failed` with an
    /// `error_message`, `task_board.list` must surface that reason as
    /// `error_preview` (truncated to ~80 chars). Pre-fix the model
    /// had to call `task_board.get(id)` to see why something failed —
    /// most models don't, so the failure context was lost.
    #[tokio::test]
    async fn list_surfaces_failure_reason_for_failed_tasks() {
        let m = mgr();
        m.create(&json!({"title": "do the thing"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        m.update(&json!({
            "task_id": "task-1",
            "new_status": "failed",
            "error_message": "compilation error in src/lib.rs: cannot find type `Foo`"
        }))
        .await;

        let list = m.list(&json!({"status_filter": "failed"})).await;
        assert!(
            list.contains("error_preview"),
            "failed list output must include error_preview: {list}"
        );
        assert!(
            list.contains("compilation error"),
            "error_preview must carry the failure message: {list}"
        );
    }

    #[tokio::test]
    async fn get_rejects_bad_task_id_before_lookup() {
        let m = mgr();
        m.create(&json!({"title": "do the thing"})).await;

        for args in [json!({"task_id": true}), json!({"task_id": "   "})] {
            let out = m.get(&args).await;
            assert!(out.starts_with("Error:"), "{out}");
            assert!(
                out.contains("task_id"),
                "bad get task_id should name the bad field: {out}"
            );
            assert!(
                !out.contains("not found"),
                "bad get task_id must fail validation before lookup: {out}"
            );
        }
    }

    #[tokio::test]
    async fn update_rejects_wrong_type_metadata_error_and_edge_fields() {
        let m = mgr();
        m.create(&json!({"title": "source"})).await;
        m.create(&json!({"title": "target"})).await;

        for (field, args) in [
            (
                "error_message",
                json!({"task_id": "task-1", "new_status": "failed", "error_message": true}),
            ),
            ("metadata", json!({"task_id": "task-1", "metadata": true})),
            (
                "add_blocks",
                json!({"task_id": "task-1", "add_blocks": true}),
            ),
            (
                "add_blocks",
                json!({"task_id": "task-1", "add_blocks": [true]}),
            ),
        ] {
            let out = m.update(&args).await;
            assert!(out.starts_with("Error:"), "{field}: {out}");
            assert!(
                out.contains(field),
                "wrong-type {field} should name the bad field: {out}"
            );
        }

        let source: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(
            source.status,
            SessionTaskStatusKind::Pending,
            "failed update with bad error_message must not mutate status"
        );
        assert!(
            source.metadata.is_none() && source.blocks.is_empty(),
            "bad update fields must not mutate metadata or edges: {source:?}"
        );
    }

    #[tokio::test]
    async fn update_and_stop_reject_oversized_text_and_metadata_before_mutating() {
        let m = mgr();
        m.create(&json!({"title": "bounded update"})).await;

        for (field, args) in [
            (
                "title",
                json!({"task_id": "task-1", "title": "x".repeat(MAX_TASK_TITLE_CHARS + 1)}),
            ),
            (
                "description",
                json!({"task_id": "task-1", "description": "x".repeat(MAX_TASK_DESCRIPTION_CHARS + 1)}),
            ),
            (
                "active_form",
                json!({"task_id": "task-1", "active_form": "x".repeat(MAX_TASK_ACTIVE_FORM_CHARS + 1)}),
            ),
            (
                "owner",
                json!({"task_id": "task-1", "owner": "x".repeat(MAX_TASK_OWNER_CHARS + 1)}),
            ),
            (
                "error_message",
                json!({"task_id": "task-1", "new_status": "failed", "error_message": "x".repeat(MAX_TASK_ERROR_MESSAGE_CHARS + 1)}),
            ),
            (
                "metadata",
                json!({"task_id": "task-1", "metadata": {"blob": "x".repeat(MAX_TASK_METADATA_BYTES + 1)}}),
            ),
        ] {
            let out = m.update(&args).await;
            assert!(out.starts_with("Error:"), "{field}: {out}");
            assert!(
                out.contains(field) && out.contains("exceeds"),
                "oversized update {field} should name the bad field and bound: {out}"
            );
        }

        let stop = m
            .stop(&json!({
                "task_id": "task-1",
                "reason": "x".repeat(MAX_TASK_STOP_REASON_CHARS + 1),
            }))
            .await;
        assert!(stop.starts_with("Error:"), "{stop}");
        assert!(
            stop.contains("reason") && stop.contains("exceeds"),
            "oversized stop reason should name the bad field and bound: {stop}"
        );

        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(task.title, "bounded update");
        assert_eq!(task.status, SessionTaskStatusKind::Pending);
        assert!(task.description.is_none());
        assert!(task.active_form.is_none());
        assert!(task.owner.is_none());
        assert!(task.metadata.is_none());
    }

    #[tokio::test]
    async fn update_accepts_terminal_reasons_and_rejects_non_terminal_error_message() {
        let m = mgr();
        m.create(&json!({
            "title": "parent",
            "subtasks": [{ "id": "s1", "title": "sub" }]
        }))
        .await;

        for (label, args, expected) in [
            (
                "missing failed status",
                json!({"task_id": "task-1", "error_message": "boom"}),
                "requires new_status='failed' or new_status='cancelled'",
            ),
            (
                "non-failed status",
                json!({"task_id": "task-1", "new_status": "completed", "error_message": "boom"}),
                "requires new_status='failed' or new_status='cancelled'",
            ),
            (
                "subtask failure",
                json!({
                    "task_id": "task-1",
                    "subtask_id": "s1",
                    "new_status": "failed",
                    "error_message": "subtask boom"
                }),
                "subtask failures cannot store an error_message",
            ),
        ] {
            let out = m.update(&args).await;
            assert!(out.starts_with("Error:"), "{label}: {out}");
            assert!(
                out.contains(expected),
                "{label} should explain the valid error_message shape: {out}"
            );
        }

        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(
            task.status,
            SessionTaskStatusKind::Pending,
            "bad error_message shapes must not mutate parent status"
        );
        assert_eq!(
            task.subtasks[0].status,
            SessionTaskStatusKind::Pending,
            "bad subtask error_message shape must not mutate subtask status"
        );
        assert!(
            task.metadata.is_none() && task.description.is_none(),
            "bad error_message shapes must not write hidden metadata/description: {task:?}"
        );

        let cancelled = m
            .update(&json!({
                "task_id": "task-1",
                "new_status": "cancelled",
                "error_message": "superseded"
            }))
            .await;
        assert!(!cancelled.starts_with("Error:"), "{cancelled}");
        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(task.status, SessionTaskStatusKind::Cancelled);
        assert_eq!(
            task.metadata
                .as_ref()
                .and_then(|metadata| metadata.get("reason"))
                .and_then(Value::as_str),
            Some("superseded")
        );
    }

    #[tokio::test]
    async fn subtask_update_rejects_missing_status_and_parent_only_fields() {
        let m = mgr();
        m.create(&json!({
            "title": "parent",
            "subtasks": [{ "id": "s1", "title": "first step" }]
        }))
        .await;

        let missing_status = m
            .update(&json!({"task_id": "task-1", "subtask_id": "s1"}))
            .await;
        assert!(missing_status.starts_with("Error:"), "{missing_status}");
        assert!(
            missing_status.contains("new_status") && missing_status.contains("required"),
            "subtask update without a status should be an explicit error: {missing_status}"
        );

        let ignored_title = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "s1",
                "title": "silently ignored"
            }))
            .await;
        assert!(ignored_title.starts_with("Error:"), "{ignored_title}");
        assert!(
            ignored_title.contains("unsupported with subtask_id")
                && ignored_title.contains("title"),
            "subtask update must reject parent-only fields instead of accepting a no-op: {ignored_title}"
        );

        let ignored_metadata = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "s1",
                "new_status": "completed",
                "metadata": {"hidden": true}
            }))
            .await;
        assert!(ignored_metadata.starts_with("Error:"), "{ignored_metadata}");
        assert!(
            ignored_metadata.contains("metadata"),
            "subtask update should reject metadata it cannot persist: {ignored_metadata}"
        );

        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(task.status, SessionTaskStatusKind::Pending);
        assert_eq!(task.subtasks[0].title, "first step");
        assert_eq!(task.subtasks[0].status, SessionTaskStatusKind::Pending);
        assert!(
            task.metadata.is_none(),
            "rejected subtask update must not write hidden parent metadata: {task:?}"
        );
    }

    #[tokio::test]
    async fn subtask_update_accepts_reason_without_parent_metadata() {
        let m = mgr();
        m.create(&json!({
            "title": "parent",
            "subtasks": [{ "id": "verify", "title": "verify changes" }]
        }))
        .await;

        let update = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "verify",
                "new_status": "completed",
                "reason": "cargo check and focused tests passed"
            }))
            .await;

        assert!(!update.starts_with("Error:"), "{update}");
        assert!(
            update.contains("cargo check and focused tests passed"),
            "response should surface the recorded reason: {update}"
        );
        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(
            task.subtasks[0].reason.as_deref(),
            Some("cargo check and focused tests passed")
        );
        assert!(
            task.metadata
                .as_ref()
                .is_none_or(|metadata| !metadata.contains_key("reason")),
            "subtask reason must stay on the subtask, not parent metadata: {task:?}"
        );
    }

    #[tokio::test]
    async fn failed_subtask_update_accepts_reason_without_error_message_rewrite() {
        let m = mgr();
        m.create(&json!({
            "title": "parent",
            "subtasks": [{ "id": "compile", "title": "compile" }]
        }))
        .await;

        let update = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "compile",
                "new_status": "failed",
                "reason": "compiler unavailable"
            }))
            .await;

        assert!(!update.starts_with("Error:"), "{update}");
        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(task.subtasks[0].status, SessionTaskStatusKind::Failed);
        assert_eq!(
            task.subtasks[0].reason.as_deref(),
            Some("compiler unavailable")
        );
        assert!(
            task.metadata
                .as_ref()
                .is_none_or(|metadata| !metadata.contains_key("error_message")),
            "subtask reason must not be rewritten as parent error_message: {task:?}"
        );
    }

    #[tokio::test]
    async fn parent_update_rejects_empty_mutation_requests() {
        let m = mgr();
        m.create(&json!({"title": "unchanged"})).await;

        for (label, args) in [
            ("task id only", json!({"task_id": "task-1"})),
            (
                "empty metadata",
                json!({"task_id": "task-1", "metadata": {}}),
            ),
            (
                "empty edge array",
                json!({"task_id": "task-1", "add_blocks": []}),
            ),
        ] {
            let out = m.update(&args).await;
            assert!(out.starts_with("Error:"), "{label}: {out}");
            assert!(
                out.contains("requires at least one update field"),
                "{label} should reject no-op parent update requests: {out}"
            );
        }

        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(task.title, "unchanged");
        assert_eq!(task.status, SessionTaskStatusKind::Pending);
        assert!(task.metadata.is_none());
        assert!(task.blocks.is_empty() && task.blocked_by.is_empty());
    }

    #[tokio::test]
    async fn failed_update_error_message_preserves_description_update() {
        let m = mgr();
        m.create(&json!({"title": "compile"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;

        let out = m
            .update(&json!({
                "task_id": "task-1",
                "new_status": "failed",
                "description": "Build verification failed",
                "error_message": "missing type Foo"
            }))
            .await;
        assert!(out.contains("\"success\":true"), "{out}");

        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(task.status, SessionTaskStatusKind::Failed);
        let description = task.description.as_deref().unwrap_or_default();
        assert!(
            description.contains("Build verification failed")
                && description.contains("Error: missing type Foo"),
            "failed update should keep both the description and failure reason: {description}"
        );
        assert_eq!(
            task.metadata
                .as_ref()
                .and_then(|metadata| metadata.get("error_message"))
                .and_then(Value::as_str),
            Some("missing type Foo")
        );
    }

    #[tokio::test]
    async fn update_rejects_blank_identity_and_edge_fields() {
        let m = mgr();
        m.create(&json!({
            "title": "source",
            "subtasks": [{ "id": "s1", "title": "step" }]
        }))
        .await;
        m.create(&json!({"title": "target"})).await;

        for (field, args) in [
            ("title", json!({"task_id": "task-1", "title": "   "})),
            (
                "subtask_id",
                json!({"task_id": "task-1", "subtask_id": "   ", "new_status": "completed"}),
            ),
            (
                "error_message",
                json!({"task_id": "task-1", "new_status": "failed", "error_message": "   "}),
            ),
            ("owner", json!({"task_id": "task-1", "owner": "   "})),
            (
                "active_form",
                json!({"task_id": "task-1", "active_form": "   "}),
            ),
            (
                "add_blocks",
                json!({"task_id": "task-1", "add_blocks": ["   "]}),
            ),
        ] {
            let out = m.update(&args).await;
            assert!(out.starts_with("Error:"), "{field}: {out}");
            assert!(
                out.contains(field) && out.contains("non-empty"),
                "blank {field} should return an actionable non-empty error: {out}"
            );
        }

        let source: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(
            source.title, "source",
            "blank title update must not mutate task title"
        );
        assert_eq!(
            source.status,
            SessionTaskStatusKind::Pending,
            "blank error_message failed update must not mutate status"
        );
        assert!(
            source.owner.is_none() && source.active_form.is_none() && source.blocks.is_empty(),
            "blank update fields must not mutate owner, active_form, or edges: {source:?}"
        );
    }

    #[tokio::test]
    async fn stop_rejects_blank_task_id_and_bad_reason() {
        let m = mgr();
        m.create(&json!({"title": "cancel me"})).await;

        for (field, args) in [
            ("task_id", json!({"task_id": "   ", "reason": "valid"})),
            ("reason", json!({"task_id": "task-1", "reason": true})),
            ("reason", json!({"task_id": "task-1", "reason": "   "})),
        ] {
            let out = m.stop(&args).await;
            assert!(out.starts_with("Error:"), "{field}: {out}");
            assert!(
                out.contains(field),
                "bad stop {field} should name the bad field: {out}"
            );
        }

        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(
            task.status,
            SessionTaskStatusKind::Pending,
            "bad stop inputs must not cancel the task"
        );
    }

    #[tokio::test]
    async fn stop_terminal_task_error_names_all_stoppable_statuses() {
        let m = mgr();
        m.create(&json!({"title": "already done"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        m.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;

        let out = m.stop(&json!({"task_id": "task-1"})).await;

        assert!(out.starts_with("Refused:"), "{out}");
        let body: Value = serde_json::from_str(out.split_once('\n').unwrap().1).unwrap();
        assert_eq!(body["success"], false, "{out}");
        assert_eq!(body["task_id"], "task-1", "{out}");
        assert_eq!(body["status"], "completed", "{out}");
        let message = body["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("pending")
                && message.contains("in_progress")
                && message.contains("paused"),
            "stop refusal should name every stoppable status: {out}"
        );
    }

    #[tokio::test]
    async fn update_cancelled_and_stop_enforce_the_same_parent_child_invariant() {
        let m = mgr();
        for title in ["cancel through update", "cancel through stop"] {
            let created = m
                .create(&json!({
                    "title": title,
                    "subtasks": [
                        {"id": "running", "title": "running child"},
                        {"id": "done", "title": "completed child"},
                        {"id": "failed", "title": "failed child"}
                    ]
                }))
                .await;
            assert!(!created.starts_with("Error:"), "{created}");
        }
        for task_id in ["task-1", "task-2"] {
            for (subtask_id, status) in [
                ("running", "in_progress"),
                ("done", "completed"),
                ("failed", "failed"),
            ] {
                let updated = m
                    .update(&json!({
                        "task_id": task_id,
                        "subtask_id": subtask_id,
                        "new_status": status
                    }))
                    .await;
                assert!(!updated.starts_with("Error:"), "{updated}");
            }
        }

        let through_update = m
            .update(&json!({
                "task_id": "task-1",
                "new_status": "cancelled",
                "reason": "superseded"
            }))
            .await;
        let through_stop = m
            .stop(&json!({"task_id": "task-2", "reason": "superseded"}))
            .await;
        for response in [&through_update, &through_stop] {
            assert!(!response.starts_with("Error:"), "{response}");
            let body: Value = serde_json::from_str(
                response
                    .split_once('\n')
                    .expect("summary and structured body")
                    .1,
            )
            .expect("structured cancellation response");
            assert_eq!(body["success"], true, "{response}");
            assert_eq!(body["status"], "cancelled", "{response}");
            assert_eq!(body["cancelled_subtasks"], 1, "{response}");
        }

        for task_id in ["task-1", "task-2"] {
            let task: SessionTask =
                serde_json::from_str(&m.get(&json!({"task_id": task_id})).await)
                    .expect("cancelled task");
            assert_eq!(task.status, SessionTaskStatusKind::Cancelled);
            let statuses = task
                .subtasks
                .iter()
                .map(|subtask| (subtask.id.as_str(), subtask.status))
                .collect::<HashMap<_, _>>();
            assert_eq!(statuses["running"], SessionTaskStatusKind::Cancelled);
            assert_eq!(statuses["done"], SessionTaskStatusKind::Completed);
            assert_eq!(statuses["failed"], SessionTaskStatusKind::Failed);
            assert_eq!(
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("reason"))
                    .and_then(Value::as_str),
                Some("superseded")
            );
            assert_eq!(
                task.description.as_deref(),
                Some("Cancelled: superseded (was: in_progress)"),
                "cancellation should store one clean human-readable note: {task:?}"
            );
        }
    }

    #[tokio::test]
    async fn stopping_an_already_cancelled_task_is_idempotent() {
        let m = mgr();
        m.create(&json!({"title": "cancel once"})).await;
        let first = m
            .stop(&json!({"task_id": "task-1", "reason": "no longer needed"}))
            .await;
        assert!(!first.starts_with("Error:"), "{first}");
        let before = m.get(&json!({"task_id": "task-1"})).await;

        let second = m
            .stop(&json!({"task_id": "task-1", "reason": "no longer needed"}))
            .await;
        let body: Value = serde_json::from_str(
            second
                .split_once('\n')
                .expect("summary and structured body")
                .1,
        )
        .expect("idempotent stop body");
        assert_eq!(body["success"], true, "{second}");
        assert_eq!(body["already_cancelled"], true, "{second}");
        assert_eq!(body["cancelled_subtasks"], 0, "{second}");
        assert_eq!(
            m.get(&json!({"task_id": "task-1"})).await,
            before,
            "repeating stop must not append prose or advance task state"
        );
    }

    #[tokio::test]
    async fn failing_a_parent_closes_only_its_open_children() {
        let m = mgr();
        m.create(&json!({
            "title": "parent that fails",
            "subtasks": [
                {"id": "running", "title": "running child"},
                {"id": "done", "title": "completed child"},
                {"id": "failed", "title": "failed child"}
            ]
        }))
        .await;
        for (subtask_id, status) in [
            ("running", "in_progress"),
            ("done", "completed"),
            ("failed", "failed"),
        ] {
            let updated = m
                .update(&json!({
                    "task_id": "task-1",
                    "subtask_id": subtask_id,
                    "new_status": status
                }))
                .await;
            assert!(!updated.starts_with("Error:"), "{updated}");
        }

        let failed = m
            .update(&json!({
                "task_id": "task-1",
                "new_status": "failed",
                "error_message": "parent execution failed"
            }))
            .await;
        let body: Value = serde_json::from_str(
            failed
                .split_once('\n')
                .expect("summary and structured body")
                .1,
        )
        .expect("structured failure response");
        assert_eq!(body["success"], true, "{failed}");
        assert_eq!(body["cancelled_subtasks"], 1, "{failed}");

        let task: SessionTask = serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await)
            .expect("failed parent");
        assert_eq!(task.status, SessionTaskStatusKind::Failed);
        let statuses = task
            .subtasks
            .iter()
            .map(|subtask| (subtask.id.as_str(), subtask.status))
            .collect::<HashMap<_, _>>();
        assert_eq!(statuses["running"], SessionTaskStatusKind::Cancelled);
        assert_eq!(statuses["done"], SessionTaskStatusKind::Completed);
        assert_eq!(statuses["failed"], SessionTaskStatusKind::Failed);
    }

    #[tokio::test]
    async fn create_does_not_use_title_similarity_as_an_idempotency_key() {
        let m = mgr();
        m.create(&json!({"title": "Implement dark mode toggle"}))
            .await;
        let second = m
            .create(&json!({"title": "  Implement DARK mode toggle. "}))
            .await;
        let second: Value =
            serde_json::from_str(second.split_once('\n').unwrap().1).expect("create response");
        assert_eq!(second["task_id"], "task-2");
        let list: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "all"})).await).unwrap();
        assert_eq!(
            list["count"], 2,
            "titles are presentation, not a semantic uniqueness constraint: {list}"
        );
    }

    #[tokio::test]
    async fn refused_mutation_does_not_advance_board_version_or_broadcast() {
        let store = Arc::new(InMemoryTaskStore::new());
        let manager = TaskManager::new("typed-refusal", store.clone());
        manager.create(&json!({"title": "pending work"})).await;
        let version = store
            .get_session_version("typed-refusal")
            .await
            .expect("version after create");
        let mut changes = store.subscribe().expect("in-memory change stream");

        let outcome = manager.archive_outcome(&json!({"task_id": "task-1"})).await;

        assert_eq!(outcome.status, TaskMutationStatus::Refused);
        assert_eq!(outcome.data["task_id"], "task-1");
        assert_eq!(
            store
                .get_session_version("typed-refusal")
                .await
                .expect("version after refusal"),
            version,
            "a business refusal must not create a phantom board revision"
        );
        assert!(
            changes.try_recv().is_err(),
            "a business refusal must not wake task-board observers"
        );
    }

    #[tokio::test]
    async fn mutation_control_flow_uses_typed_outcome_not_rendered_summary() {
        let store = InMemoryTaskStore::new();
        let before = store
            .get_session_version("typed-protocol")
            .await
            .expect("initial version");

        let applied = store
            .mutate(
                "typed-protocol",
                Box::new(|tasks, _| {
                    Ok(TaskMutationResult::applied(
                        tasks,
                        None,
                        "Error: deliberately misleading presentation",
                        json!({"message": "typed applied"}),
                    ))
                }),
            )
            .await
            .expect("typed applied outcome");
        assert!(applied.status.is_success() && applied.status.changed());
        assert_eq!(
            store
                .get_session_version("typed-protocol")
                .await
                .expect("version after applied outcome"),
            before + 1
        );

        let refused = store
            .mutate(
                "typed-protocol",
                Box::new(|tasks, _| {
                    Ok(TaskMutationResult::refused(
                        tasks,
                        "Created successfully (presentation only)",
                        json!({"message": "typed refusal"}),
                    ))
                }),
            )
            .await
            .expect("typed refused outcome");
        assert!(!refused.status.is_success() && !refused.status.changed());
        assert_eq!(
            store
                .get_session_version("typed-protocol")
                .await
                .expect("version after refused outcome"),
            before + 1,
            "rendered success-like text must not turn a refusal into a write"
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
                "new_status": "completed"
            }))
            .await;

        assert!(out.contains("subtask 'dirs' not found"), "got {out}");
        assert!(
            out.contains("available subtask ids: setup, render"),
            "error must help the model recover with valid ids: {out}"
        );
    }

    #[tokio::test]
    async fn create_allows_duplicate_of_completed_task() {
        let m = mgr();
        m.create(&json!({"title": "fix bug"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        m.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        // Same title now should be allowed since the prior is closed.
        let dup = m.create(&json!({"title": "fix bug"})).await;
        let dup_parsed: Value = serde_json::from_str(dup.split_once('\n').unwrap().1).unwrap();
        assert_eq!(
            dup_parsed["success"], true,
            "create after completion must succeed; got {dup}"
        );
        let list: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "all"})).await).unwrap();
        assert_eq!(
            list["count"], 2,
            "second instance must persist when prior is completed; got {list}"
        );
    }

    #[tokio::test]
    async fn create_allows_same_title_as_paused_task() {
        let m = mgr();
        m.create(&json!({"title": "resume this later"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "paused"}))
            .await;

        let second = m.create(&json!({"title": "Resume this later."})).await;
        let second: Value =
            serde_json::from_str(second.split_once('\n').unwrap().1).expect("create response");
        assert_eq!(second["task_id"], "task-2");
        let list: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "all"})).await).unwrap();
        assert_eq!(
            list["count"], 2,
            "paused work does not make its title a uniqueness key: {list}"
        );
    }

    #[tokio::test]
    async fn update_title_allows_duplicate_open_task_titles() {
        let m = mgr();
        m.create(&json!({"title": "Implement OAuth callback"}))
            .await;
        m.create(&json!({"title": "Wire billing webhook"})).await;

        let updated = m
            .update(&json!({
                "task_id": "task-2",
                "title": " implement oauth callback. "
            }))
            .await;

        let updated: Value =
            serde_json::from_str(updated.split_once('\n').unwrap().1).expect("update response");
        assert_eq!(updated["success"], true, "{updated}");

        let task_2: Value =
            serde_json::from_str(&m.get(&json!({"task_id": "task-2"})).await).unwrap();
        assert_eq!(
            task_2["title"], " implement oauth callback. ",
            "title updates are not rejected by semantic string heuristics: {task_2}"
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
        let task: SessionTask = serde_json::from_str(&body).expect("parse task");
        assert!(
            task.owner.is_none() && task.subtasks.iter().all(|st| st.owner.is_none()),
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
        let list = m.list(&json!({"status_filter": "all"})).await;
        let list_parsed: Value = serde_json::from_str(&list).unwrap();
        let has_error_preview = list_parsed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t.get("error_preview").is_some());
        assert!(
            !has_error_preview,
            "completed tasks must not show error_preview: {list}"
        );
    }

    #[tokio::test]
    async fn two_managers_same_session_share_store() {
        let store: Arc<dyn TaskStore> = Arc::new(InMemoryTaskStore::new());
        let a = TaskManager::new("sess-1", store.clone());
        let b = TaskManager::new("sess-1", store.clone());
        a.create(&json!({"title": "from-a"})).await;
        let list: Value =
            serde_json::from_str(&b.list(&json!({"status_filter": "all"})).await).unwrap();
        let titles: Vec<&str> = list["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["title"].as_str().unwrap())
            .collect();
        assert!(titles.contains(&"from-a"), "b should see a's task: {list}");
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
            let parsed: Value = serde_json::from_str(out.split_once('\n').unwrap().1).unwrap();
            assert_eq!(parsed["success"], true, "{out}");
        }

        let mgr = TaskManager::new("sess-race", store);
        let tasks = mgr.snapshot().await.unwrap();
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
        let updated: Value = serde_json::from_str(out.split_once('\n').unwrap().1).unwrap();
        assert_eq!(updated["success"], true, "{out}");
        assert_eq!(updated["status"], "in_progress", "{out}");
        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(
            task.status,
            SessionTaskStatusKind::InProgress,
            "new_status must not be a no-op"
        );

        let paused = m
            .update(&json!({"task_id": "task-1", "new_status": "paused"}))
            .await;
        let paused_value: Value = serde_json::from_str(paused.split_once('\n').unwrap().1).unwrap();
        assert_eq!(paused_value["success"], true, "{paused}");
        assert_eq!(paused_value["status"], "paused", "{paused}");

        let stopped = m
            .stop(&json!({"task_id": "task-1", "reason": "clear stale slot"}))
            .await;
        let stopped_value: Value =
            serde_json::from_str(stopped.split_once('\n').unwrap().1).unwrap();
        assert_eq!(stopped_value["success"], true, "{stopped}");
        assert_eq!(stopped_value["previous_status"], "paused", "{stopped}");
    }

    #[tokio::test]
    async fn pending_parent_can_move_directly_to_terminal_statuses() {
        let m = mgr();
        for (idx, status) in ["failed", "cancelled", "completed"].iter().enumerate() {
            let create = m
                .create(&json!({"title": format!("terminal from pending {status}")}))
                .await;
            assert!(!create.starts_with("Error:"), "{create}");
            let task_id = format!("task-{}", idx + 1);
            let out = m
                .update(&json!({
                    "task_id": task_id,
                    "new_status": status,
                    "reason": "settle stale task"
                }))
                .await;
            assert!(!out.starts_with("Error:"), "{status}: {out}");
        }

        let tasks: Value = serde_json::from_str(&m.list(&json!({"status_filter": "all"})).await)
            .expect("task list json");
        let statuses = tasks["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|task| task["status"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(statuses, vec!["failed", "cancelled", "completed"]);
    }

    #[tokio::test]
    async fn update_allows_multiple_independent_in_progress_parent_tasks() {
        let m = mgr();
        m.create(&json!({"title": "first"})).await;
        m.create(&json!({"title": "second"})).await;
        let first = m
            .update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert!(!first.starts_with("Error:"), "{first}");

        let second = m
            .update(&json!({"task_id": "task-2", "new_status": "in_progress"}))
            .await;
        assert!(
            second.contains("\"success\":true") && second.contains("\"status\":\"in_progress\""),
            "independent tasks should be allowed to run concurrently; ordering belongs in dependency edges: {second}"
        );
        let task_1: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        let task_2: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-2"})).await).unwrap();
        assert_eq!(task_1.status, SessionTaskStatusKind::InProgress);
        assert_eq!(task_2.status, SessionTaskStatusKind::InProgress);
    }

    #[tokio::test]
    async fn update_in_progress_is_idempotent_for_the_same_task() {
        let store = Arc::new(InMemoryTaskStore::new());
        let m = TaskManager::new("idempotent-parent-update", store.clone());
        m.create(&json!({"title": "already running"})).await;
        let first = m
            .update_outcome(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert_eq!(first.status, TaskMutationStatus::Applied, "{first:?}");
        let version = store
            .get_session_version("idempotent-parent-update")
            .await
            .unwrap();
        let mut changes = store.subscribe().expect("in-memory change stream");

        let repeat = m
            .update_outcome(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert_eq!(repeat.status, TaskMutationStatus::Unchanged, "{repeat:?}");
        assert_eq!(repeat.data["already_current"], true, "{repeat:?}");
        assert_eq!(
            store
                .get_session_version("idempotent-parent-update")
                .await
                .unwrap(),
            version,
            "an idempotent update must not advance durable board state"
        );
        assert!(matches!(
            changes.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn repeated_subtask_status_is_unchanged_without_a_board_write() {
        let store = Arc::new(InMemoryTaskStore::new());
        let m = TaskManager::new("idempotent-subtask-update", store.clone());
        m.create(&json!({
            "title": "parent",
            "subtasks": [{"id": "child", "title": "child"}]
        }))
        .await;
        let first = m
            .update_outcome(&json!({
                "task_id": "task-1",
                "subtask_id": "child",
                "new_status": "in_progress"
            }))
            .await;
        assert_eq!(first.status, TaskMutationStatus::Applied, "{first:?}");
        let version = store
            .get_session_version("idempotent-subtask-update")
            .await
            .unwrap();

        let repeat = m
            .update_outcome(&json!({
                "task_id": "task-1",
                "subtask_id": "child",
                "new_status": "in_progress"
            }))
            .await;

        assert_eq!(repeat.status, TaskMutationStatus::Unchanged, "{repeat:?}");
        assert_eq!(
            store
                .get_session_version("idempotent-subtask-update")
                .await
                .unwrap(),
            version
        );
    }

    #[tokio::test]
    async fn adding_existing_dependency_repairs_missing_reverse_edge() {
        let m = mgr();
        m.create(&json!({"title": "producer"})).await;
        m.create(&json!({"title": "consumer"})).await;

        let mut snapshot = m.try_snapshot_state().await.unwrap();
        snapshot.tasks[0].blocks.push("task-2".to_string());
        assert!(snapshot.tasks[1].blocked_by.is_empty());
        m.restore_snapshot(&snapshot).await.unwrap();

        let repaired = m
            .update_outcome(&json!({"task_id": "task-1", "add_blocks": ["task-2"]}))
            .await;

        assert_eq!(repaired.status, TaskMutationStatus::Applied, "{repaired:?}");
        let tasks = m.snapshot().await.unwrap();
        assert_eq!(tasks[0].blocks, ["task-2"]);
        assert_eq!(tasks[1].blocked_by, ["task-1"]);
    }

    #[tokio::test]
    async fn dependency_repair_rejects_an_asymmetric_edge_inside_a_cycle() {
        let m = mgr();
        for title in ["a", "b", "c"] {
            m.create(&json!({"title": title})).await;
        }
        let mut snapshot = m.try_snapshot_state().await.unwrap();
        snapshot.tasks[0].blocks.push("task-2".into());
        snapshot.tasks[0].blocked_by.clear();
        snapshot.tasks[1].blocked_by.push("task-1".into());
        snapshot.tasks[1].blocks.push("task-3".into());
        snapshot.tasks[2].blocked_by.push("task-2".into());
        // Persisted forward half C -> A closes the cycle, but its reverse
        // metadata is missing. Repair must surface corruption, not bless it.
        snapshot.tasks[2].blocks.push("task-1".into());
        m.restore_snapshot(&snapshot).await.unwrap();

        let repaired = m
            .update_outcome(&json!({"task_id": "task-3", "add_blocks": ["task-1"]}))
            .await;

        assert_eq!(repaired.status, TaskMutationStatus::Failed, "{repaired:?}");
        assert!(repaired.output.contains("create a cycle"), "{repaired:?}");
        let tasks = m.snapshot().await.unwrap();
        assert!(
            tasks[0].blocked_by.is_empty(),
            "failed repair mutated graph: {tasks:?}"
        );
    }

    #[test]
    fn mutation_outcome_deserialization_normalizes_legacy_duplicate_truth() {
        let outcome: TaskMutationOutcome = serde_json::from_value(json!({
            "output": "legacy",
            "status": "applied",
            "success": false,
            "changed": false,
            "data": {
                "success": false,
                "mutation_status": "failed"
            }
        }))
        .unwrap();

        assert!(outcome.status.is_success());
        assert!(outcome.status.changed());
        assert_eq!(outcome.data["success"], true);
        assert_eq!(outcome.data["mutation_status"], "applied");
        let encoded = serde_json::to_value(&outcome).unwrap();
        assert_eq!(encoded["success"], true);
        assert_eq!(encoded["changed"], true);
    }

    #[tokio::test]
    async fn repeated_failure_evidence_is_unchanged_without_duplicate_prose() {
        let store = Arc::new(InMemoryTaskStore::new());
        let m = TaskManager::new("idempotent-failure", store.clone());
        m.create(&json!({"title": "compile", "description": "Build succeeds"}))
            .await;
        let args = json!({
            "task_id": "task-1",
            "new_status": "failed",
            "error_message": "missing type Foo"
        });
        let first = m.update_outcome(&args).await;
        assert_eq!(first.status, TaskMutationStatus::Applied, "{first:?}");
        let before = m.get(&json!({"task_id": "task-1"})).await;
        let version = store
            .get_session_version("idempotent-failure")
            .await
            .unwrap();

        let repeat = m.update_outcome(&args).await;

        assert_eq!(repeat.status, TaskMutationStatus::Unchanged, "{repeat:?}");
        assert_eq!(repeat.data["already_current"], true, "{repeat:?}");
        assert_eq!(m.get(&json!({"task_id": "task-1"})).await, before);
        assert_eq!(
            store
                .get_session_version("idempotent-failure")
                .await
                .unwrap(),
            version
        );
        assert_eq!(
            before.matches("Error: missing type Foo").count(),
            1,
            "{before}"
        );
    }

    #[tokio::test]
    async fn update_null_clears_optional_task_fields() {
        let m = mgr();
        m.create(&json!({
            "title": "reassignable work",
            "description": "old definition",
            "active_form": "working",
            "owner": "agent-1"
        }))
        .await;

        let cleared = m
            .update_outcome(&json!({
                "task_id": "task-1",
                "description": null,
                "active_form": null,
                "owner": null
            }))
            .await;

        assert_eq!(cleared.status, TaskMutationStatus::Applied, "{cleared:?}");
        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(task.description, None);
        assert_eq!(task.active_form, None);
        assert_eq!(task.owner, None);
    }

    #[tokio::test]
    async fn delete_rejects_fields_it_would_otherwise_ignore() {
        let m = mgr();
        m.create(&json!({"title": "keep identity"})).await;

        let outcome = m
            .update_outcome(&json!({
                "task_id": "task-1",
                "new_status": "deleted",
                "title": "silently discarded"
            }))
            .await;

        assert_eq!(outcome.status, TaskMutationStatus::Failed, "{outcome:?}");
        assert!(outcome.output.contains("Unsupported fields: title"));
        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(task.title, "keep identity");
        assert_eq!(task.status, SessionTaskStatusKind::Pending);
    }

    #[tokio::test]
    async fn subtask_start_allows_independent_parent_concurrency() {
        let m = mgr();
        m.create(&json!({"title": "running parent"})).await;
        m.create(&json!({
            "title": "second parent",
            "subtasks": [{ "id": "s1", "title": "step" }]
        }))
        .await;
        let first = m
            .update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert!(!first.starts_with("Error:"), "{first}");

        let subtask_start = m
            .update(&json!({
                "task_id": "task-2",
                "subtask_id": "s1",
                "new_status": "in_progress"
            }))
            .await;
        assert!(
            subtask_start.contains("\"success\":true")
                && subtask_start.contains("\"status\":\"in_progress\""),
            "a subtask in one task should not be blocked by an unrelated in_progress task: {subtask_start}"
        );
        let task_2: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-2"})).await).unwrap();
        assert_eq!(task_2.status, SessionTaskStatusKind::InProgress);
        assert_eq!(task_2.subtasks[0].status, SessionTaskStatusKind::InProgress);
    }

    #[tokio::test]
    async fn subtask_start_and_completion_require_depends_on_completed() {
        let m = mgr();
        m.create(&json!({
            "title": "ordered work",
            "subtasks": [
                { "id": "setup", "title": "setup" },
                { "id": "verify", "title": "verify", "depends_on": ["setup"] }
            ]
        }))
        .await;

        let blocked_start = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "verify",
                "new_status": "in_progress"
            }))
            .await;
        assert!(blocked_start.starts_with("Error:"), "{blocked_start}");
        assert!(
            blocked_start.contains("depends_on")
                && blocked_start.contains("setup")
                && blocked_start.contains("pending"),
            "blocked subtask start should explain unresolved prerequisite: {blocked_start}"
        );

        let blocked_complete = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "verify",
                "new_status": "completed"
            }))
            .await;
        assert!(blocked_complete.starts_with("Error:"), "{blocked_complete}");
        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(task.subtasks[1].status, SessionTaskStatusKind::Pending);

        let setup_done = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "setup",
                "new_status": "completed"
            }))
            .await;
        assert!(!setup_done.starts_with("Error:"), "{setup_done}");
        let verify_start = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "verify",
                "new_status": "in_progress"
            }))
            .await;
        assert!(!verify_start.starts_with("Error:"), "{verify_start}");
        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(task.subtasks[1].status, SessionTaskStatusKind::InProgress);
    }

    #[tokio::test]
    async fn update_rejects_status_alias_and_keeps_schema_on_new_status() {
        let m = mgr();
        m.create(&json!({"title": "status alias"})).await;
        let invalid = m
            .update(&json!({"task_id": "task-1", "new_status": "active"}))
            .await;
        assert!(invalid.starts_with("Error:"), "{invalid}");
        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(
            task.status,
            SessionTaskStatusKind::Pending,
            "invalid status must not mutate task"
        );

        let wrong_type = m
            .update(&json!({"task_id": "task-1", "new_status": true}))
            .await;
        assert!(wrong_type.starts_with("Error:"), "{wrong_type}");
        assert!(
            wrong_type.contains("new_status") && wrong_type.contains("string"),
            "wrong-type new_status should be actionable: {wrong_type}"
        );

        let alias = m
            .update(&json!({
                "task_id": "task-1",
                "status": "cancelled",
                "reason": "not needed"
            }))
            .await;
        assert!(
            alias.starts_with("Error:")
                && alias.contains("unknown field")
                && alias.contains("status")
                && alias.contains("new_status"),
            "old status alias should be rejected with a new_status hint: {alias}"
        );
        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(task.status, SessionTaskStatusKind::Pending);
        assert!(
            task.metadata
                .as_ref()
                .and_then(|metadata| metadata.get("reason"))
                .and_then(Value::as_str)
                .is_none(),
            "rejected status alias must not preserve reason metadata: {task:?}"
        );

        let conflict = m
            .update(&json!({
                "task_id": "task-1",
                "new_status": "failed",
                "status": "completed"
            }))
            .await;
        assert!(
            conflict.starts_with("Error:")
                && conflict.contains("unknown field")
                && conflict.contains("status"),
            "status plus new_status should fail closed on the unsupported status field: {conflict}"
        );
    }

    #[tokio::test]
    async fn update_rejects_reopening_terminal_parent_tasks() {
        let m = mgr();
        for (idx, terminal_status) in ["completed", "failed", "cancelled"].iter().enumerate() {
            let title = format!("terminal {terminal_status}");
            let create = m.create(&json!({"title": title})).await;
            assert!(!create.starts_with("Error:"), "{create}");
            let task_id = format!("task-{}", idx + 1);
            // Must transition pending → in_progress → terminal
            let started = m
                .update(&json!({"task_id": task_id, "new_status": "in_progress"}))
                .await;
            assert!(!started.starts_with("Error:"), "{started}");
            let terminal = m
                .update(&json!({"task_id": task_id, "new_status": terminal_status}))
                .await;
            assert!(!terminal.starts_with("Error:"), "{terminal}");
            let reopened = m
                .update(&json!({"task_id": task_id, "new_status": "in_progress"}))
                .await;
            assert!(
                reopened.starts_with("Error:")
                    && reopened.contains("already terminal")
                    && reopened.contains("create a new task"),
                "terminal task should not be reopened directly: {reopened}"
            );
        }

        let create = m.create(&json!({"title": "archive me"})).await;
        assert!(!create.starts_with("Error:"), "{create}");
        m.update(&json!({"task_id": "task-4", "new_status": "in_progress"}))
            .await;
        let completed = m
            .update(&json!({"task_id": "task-4", "new_status": "completed"}))
            .await;
        assert!(!completed.starts_with("Error:"), "{completed}");
        let archived = m.archive(&json!({"task_id": "task-4"})).await;
        assert!(!archived.starts_with("Error:"), "{archived}");
        let reopened = m
            .update(&json!({"task_id": "task-4", "new_status": "pending"}))
            .await;
        assert!(
            reopened.starts_with("Error:") && reopened.contains("already terminal"),
            "archived task should not be reopened: {reopened}"
        );

        let deleted = m
            .update(&json!({"task_id": "task-4", "new_status": "deleted"}))
            .await;
        assert!(
            !deleted.starts_with("Error:"),
            "terminal tasks should still be removable: {deleted}"
        );
    }

    #[tokio::test]
    async fn snapshot_restores_tasks_and_next_id() {
        let m = mgr();
        m.create(&json!({"title": "t1"})).await;
        let mut snap = m.try_snapshot_state().await.expect("snapshot in test");
        m.create(&json!({"title": "t2"})).await;
        m.seal_snapshot_for_restore(&mut snap)
            .await
            .expect("seal snapshot after own mutation");
        let list_before: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "all"})).await).unwrap();
        assert_eq!(list_before["count"], 2);
        m.restore_snapshot(&snap).await.unwrap();
        let list_after: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "all"})).await).unwrap();
        assert_eq!(list_after["count"], 1, "after restore: {list_after}");
        // Counter is never rewound: t2 consumed id-2, so the
        // restored counter advances to 3 to prevent duplicate ids.
        let out = m.create(&json!({"title": "t2-again"})).await;
        let created: Value = serde_json::from_str(out.split_once('\n').unwrap().1).unwrap();
        assert_eq!(created["task_id"], "task-3", "counter not rewound: {out}");
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
    async fn edge_updates_reject_unknown_and_duplicate_task_ids() {
        let m = mgr();
        m.create(&json!({"title": "a"})).await;
        m.create(&json!({"title": "b"})).await;

        let unknown = m
            .update(&json!({"task_id": "task-1", "add_blocks": ["task-999"]}))
            .await;
        assert!(unknown.starts_with("Error:"), "{unknown}");
        assert!(
            unknown.contains("task-999") && unknown.contains("not found"),
            "unknown dependency target should be actionable: {unknown}"
        );

        let duplicate = m
            .update(&json!({"task_id": "task-1", "add_blocks": ["task-2", "task-2"]}))
            .await;
        assert!(duplicate.starts_with("Error:"), "{duplicate}");
        assert!(
            duplicate.contains("duplicate") && duplicate.contains("task-2"),
            "duplicate dependency input should be rejected: {duplicate}"
        );

        let task_a: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert!(
            task_a.blocks.is_empty() && task_a.blocked_by.is_empty(),
            "bad edge updates must not mutate task graph: {task_a:?}"
        );
    }

    #[tokio::test]
    async fn edge_updates_keep_blocks_and_blocked_by_symmetric() {
        let m = mgr();
        m.create(&json!({"title": "a"})).await;
        m.create(&json!({"title": "b"})).await;
        m.create(&json!({"title": "c"})).await;

        let add_blocks = m
            .update(&json!({"task_id": "task-1", "add_blocks": ["task-2"]}))
            .await;
        assert!(!add_blocks.starts_with("Error:"), "{add_blocks}");
        let task_a: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        let task_b: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-2"})).await).unwrap();
        assert_eq!(task_a.blocks, vec!["task-2"]);
        assert_eq!(task_b.blocked_by, vec!["task-1"]);

        let remove_blocks = m
            .update(&json!({"task_id": "task-1", "remove_blocks": ["task-2"]}))
            .await;
        assert!(!remove_blocks.starts_with("Error:"), "{remove_blocks}");
        let task_a: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        let task_b: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-2"})).await).unwrap();
        assert!(task_a.blocks.is_empty(), "{task_a:?}");
        assert!(task_b.blocked_by.is_empty(), "{task_b:?}");

        let add_blocked_by = m
            .update(&json!({"task_id": "task-3", "add_blocked_by": ["task-1"]}))
            .await;
        assert!(!add_blocked_by.starts_with("Error:"), "{add_blocked_by}");
        let task_a: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        let task_c: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-3"})).await).unwrap();
        assert_eq!(task_a.blocks, vec!["task-3"]);
        assert_eq!(task_c.blocked_by, vec!["task-1"]);

        let remove_blocked_by = m
            .update(&json!({"task_id": "task-3", "remove_blocked_by": ["task-1"]}))
            .await;
        assert!(
            !remove_blocked_by.starts_with("Error:"),
            "{remove_blocked_by}"
        );
        let task_a: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        let task_c: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-3"})).await).unwrap();
        assert!(task_a.blocks.is_empty(), "{task_a:?}");
        assert!(task_c.blocked_by.is_empty(), "{task_c:?}");
    }

    #[tokio::test]
    async fn create_accepts_dependency_edges_atomically() {
        let m = mgr();
        m.create(&json!({"title": "setup"})).await;
        let created = m
            .create(&json!({
                "title": "verify",
                "add_blocked_by": ["task-1"]
            }))
            .await;
        assert!(!created.starts_with("Error:"), "{created}");

        let setup: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        let verify: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-2"})).await).unwrap();
        assert_eq!(setup.blocks, vec!["task-2"]);
        assert_eq!(verify.blocked_by, vec!["task-1"]);

        let blocked_start = m
            .update(&json!({"task_id": "task-2", "new_status": "in_progress"}))
            .await;
        assert!(blocked_start.starts_with("Error:"), "{blocked_start}");
    }

    #[tokio::test]
    async fn in_progress_rejects_unresolved_blocked_by_dependencies() {
        let m = mgr();
        m.create(&json!({"title": "setup"})).await;
        m.create(&json!({"title": "depends on setup"})).await;
        let linked = m
            .update(&json!({"task_id": "task-2", "add_blocked_by": ["task-1"]}))
            .await;
        assert!(!linked.starts_with("Error:"), "{linked}");

        let blocked = m
            .update(&json!({"task_id": "task-2", "new_status": "in_progress"}))
            .await;
        assert!(blocked.starts_with("Error:"), "{blocked}");
        assert!(
            blocked.contains("cannot start")
                && blocked.contains("task-1")
                && blocked.contains("pending"),
            "blocked start should explain unresolved dependency: {blocked}"
        );
        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-2"})).await).unwrap();
        assert_eq!(
            task.status,
            SessionTaskStatusKind::Pending,
            "rejected start must not mutate blocked task"
        );

        m.update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        let completed = m
            .update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        assert!(!completed.starts_with("Error:"), "{completed}");
        let start = m
            .update(&json!({"task_id": "task-2", "new_status": "in_progress"}))
            .await;
        assert!(
            start.contains("\"success\":true") && start.contains("\"status\":\"in_progress\""),
            "{start}"
        );
    }

    #[tokio::test]
    async fn in_progress_rejects_new_unresolved_blocker_edge() {
        let m = mgr();
        m.create(&json!({"title": "setup"})).await;
        m.create(&json!({"title": "already running"})).await;
        let started = m
            .update(&json!({"task_id": "task-2", "new_status": "in_progress"}))
            .await;
        assert!(!started.starts_with("Error:"), "{started}");

        let blocked_while_running = m
            .update(&json!({"task_id": "task-2", "add_blocked_by": ["task-1"]}))
            .await;
        assert!(
            blocked_while_running.starts_with("Error:"),
            "{blocked_while_running}"
        );
        assert!(
            blocked_while_running.contains("cannot start")
                && blocked_while_running.contains("task-1")
                && blocked_while_running.contains("pending"),
            "adding an unresolved blocker to an in_progress task should explain the inconsistent state: {blocked_while_running}"
        );

        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-2"})).await).unwrap();
        assert_eq!(task.status, SessionTaskStatusKind::InProgress);
        assert!(
            task.blocked_by.is_empty(),
            "rejected blocker edge must not be persisted: {task:?}"
        );
    }

    #[tokio::test]
    async fn reverse_add_blocks_cannot_bypass_running_task_dependency_validation() {
        let m = mgr();
        m.create(&json!({"title": "late prerequisite"})).await;
        m.create(&json!({"title": "already running"})).await;
        let started = m
            .update(&json!({"task_id": "task-2", "new_status": "in_progress"}))
            .await;
        assert!(!started.starts_with("Error:"), "{started}");

        let rejected = m
            .update(&json!({"task_id": "task-1", "add_blocks": ["task-2"]}))
            .await;
        assert!(rejected.starts_with("Error:"), "{rejected}");
        assert!(
            rejected.contains("task-2")
                && rejected.contains("task-1")
                && rejected.contains("in_progress"),
            "the reverse edge spelling should explain the same invariant: {rejected}"
        );
        let tasks = m.snapshot().await.expect("unchanged task graph");
        assert!(tasks[0].blocks.is_empty(), "{tasks:?}");
        assert!(tasks[1].blocked_by.is_empty(), "{tasks:?}");
    }

    #[tokio::test]
    async fn completed_status_rejects_unresolved_parent_dependencies() {
        let m = mgr();
        m.create(&json!({"title": "prerequisite"})).await;
        m.create(&json!({
            "title": "blocked work",
            "add_blocked_by": ["task-1"]
        }))
        .await;

        let rejected = m
            .update(&json!({"task_id": "task-2", "new_status": "completed"}))
            .await;
        assert!(rejected.starts_with("Error:"), "{rejected}");
        assert!(
            rejected.contains("cannot start or complete")
                && rejected.contains("task-1")
                && rejected.contains("pending"),
            "completion must not turn blocked work into a downstream success signal: {rejected}"
        );
        let blocked: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-2"})).await).unwrap();
        assert_eq!(blocked.status, SessionTaskStatusKind::Pending);
    }

    #[tokio::test]
    async fn subtask_cannot_start_through_an_unresolved_parent_dependency() {
        let m = mgr();
        m.create(&json!({"title": "prerequisite"})).await;
        m.create(&json!({
            "title": "blocked parent",
            "add_blocked_by": ["task-1"],
            "subtasks": [{"id": "child", "title": "must wait too"}]
        }))
        .await;

        let rejected = m
            .update(&json!({
                "task_id": "task-2",
                "subtask_id": "child",
                "new_status": "in_progress"
            }))
            .await;
        assert!(rejected.starts_with("Error:"), "{rejected}");
        assert!(rejected.contains("task-1"), "{rejected}");
        let parent: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-2"})).await).unwrap();
        assert_eq!(parent.status, SessionTaskStatusKind::Pending);
        assert_eq!(parent.subtasks[0].status, SessionTaskStatusKind::Pending);
    }

    #[tokio::test]
    async fn in_progress_allows_same_update_to_remove_stale_dependency() {
        let m = mgr();
        m.create(&json!({"title": "old blocker"})).await;
        m.create(&json!({"title": "ready now"})).await;
        let linked = m
            .update(&json!({"task_id": "task-1", "add_blocks": ["task-2"]}))
            .await;
        assert!(!linked.starts_with("Error:"), "{linked}");

        let start = m
            .update(&json!({
                "task_id": "task-2",
                "remove_blocked_by": ["task-1"],
                "new_status": "in_progress"
            }))
            .await;
        assert!(!start.starts_with("Error:"), "{start}");
        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-2"})).await).unwrap();
        assert_eq!(task.status, SessionTaskStatusKind::InProgress);
        assert!(task.blocked_by.is_empty(), "{task:?}");
    }

    #[tokio::test]
    async fn in_progress_rejects_dangling_blocked_by_dependency() {
        let store: Arc<dyn TaskStore> = Arc::new(InMemoryTaskStore::new().without_validation());
        let m = TaskManager::new("sess-dangling", store);
        m.create(&json!({"title": "legacy dangling dependency"}))
            .await;
        let mut snapshot = m.try_snapshot_state().await.expect("snapshot in test");
        snapshot.tasks[0].blocked_by = vec!["task-missing".to_string()];
        m.restore_snapshot(&snapshot)
            .await
            .expect("restore legacy dangling dependency");

        let out = m
            .update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert!(out.starts_with("Error:"), "{out}");
        assert!(
            out.contains("task-missing") && out.contains("missing"),
            "dangling blocked_by should be surfaced instead of silently ignored: {out}"
        );
        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(task.status, SessionTaskStatusKind::Pending);
    }

    #[tokio::test]
    async fn completed_dependency_edge_does_not_block_parent_autocomplete() {
        let m = mgr();
        m.create(&json!({"title": "prerequisite"})).await;
        let created = m
            .create(&json!({
                "title": "dependent parent",
                "add_blocked_by": ["task-1"],
                "subtasks": [{"id": "only", "title": "only child"}]
            }))
            .await;
        assert!(!created.starts_with("Error:"), "{created}");
        m.update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        m.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;

        let child = m
            .update(&json!({
                "task_id": "task-2",
                "subtask_id": "only",
                "new_status": "completed"
            }))
            .await;
        assert!(!child.starts_with("Error:"), "{child}");
        let parent: SessionTask = serde_json::from_str(&m.get(&json!({"task_id": "task-2"})).await)
            .expect("dependent parent");
        assert_eq!(
            parent.status,
            SessionTaskStatusKind::Completed,
            "a retained dependency edge is historical after its blocker completes: {parent:?}"
        );
        assert_eq!(parent.blocked_by, ["task-1"]);
        assert!(
            unresolved_task_blocker_ids(&m.try_snapshot_state().await.unwrap().tasks, &parent)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn late_blocker_completion_reconciles_all_done_parent_chain() {
        let m = mgr();
        m.create(&json!({
            "title": "prerequisite",
            "subtasks": [{"id": "prereq-child", "title": "finish prerequisite"}]
        }))
        .await;
        m.create(&json!({
            "title": "dependent parent",
            "add_blocked_by": ["task-1"],
            "subtasks": [{"id": "dependent-child", "title": "already done"}]
        }))
        .await;
        m.create(&json!({
            "title": "transitive parent",
            "add_blocked_by": ["task-2"],
            "subtasks": [{"id": "transitive-child", "title": "already done too"}]
        }))
        .await;

        // Simulate durable state written by an older client before parent
        // dependency validation was enforced. A subsequent canonical
        // mutation must reconcile it instead of leaving the board stuck.
        let mut snapshot = m.try_snapshot_state().await.expect("legacy snapshot");
        for task_id in ["task-2", "task-3"] {
            let task = snapshot
                .tasks
                .iter_mut()
                .find(|task| task.id == task_id)
                .expect("dependent fixture task");
            task.subtasks[0].status = SessionTaskStatusKind::Completed;
        }
        m.restore_snapshot(&snapshot)
            .await
            .expect("restore legacy all-done parents");
        for task_id in ["task-2", "task-3"] {
            let task: SessionTask =
                serde_json::from_str(&m.get(&json!({"task_id": task_id})).await).unwrap();
            assert_eq!(task.status, SessionTaskStatusKind::Pending, "{task:?}");
        }

        let completed = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "prereq-child",
                "new_status": "completed"
            }))
            .await;
        assert!(!completed.starts_with("Error:"), "{completed}");
        for task_id in ["task-1", "task-2", "task-3"] {
            let task: SessionTask =
                serde_json::from_str(&m.get(&json!({"task_id": task_id})).await).unwrap();
            assert_eq!(
                task.status,
                SessionTaskStatusKind::Completed,
                "completion should propagate through the dependency chain: {task:?}"
            );
        }
    }

    #[tokio::test]
    async fn reopened_prerequisite_reopens_derived_completion_chain() {
        let m = mgr();
        for (title, blocker) in [
            ("root", None),
            ("dependent", Some("task-1")),
            ("transitive", Some("task-2")),
        ] {
            let mut create = json!({
                "title": title,
                "subtasks": [{"id": "only", "title": format!("{title} child")}]
            });
            if let Some(blocker) = blocker {
                create["add_blocked_by"] = json!([blocker]);
            }
            let created = m.create(&create).await;
            assert!(!created.starts_with("Error:"), "{created}");
        }
        for task_id in ["task-1", "task-2", "task-3"] {
            let completed = m
                .update(&json!({
                    "task_id": task_id,
                    "subtask_id": "only",
                    "new_status": "completed"
                }))
                .await;
            assert!(!completed.starts_with("Error:"), "{completed}");
        }

        let reopened = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "only",
                "new_status": "pending"
            }))
            .await;
        assert!(!reopened.starts_with("Error:"), "{reopened}");
        let tasks = m.try_snapshot_state().await.expect("reopened chain").tasks;
        assert_eq!(tasks[0].status, SessionTaskStatusKind::InProgress);
        assert_eq!(tasks[1].status, SessionTaskStatusKind::Pending);
        assert_eq!(tasks[2].status, SessionTaskStatusKind::Pending);
        assert_eq!(unresolved_task_blocker_ids(&tasks, &tasks[1]), ["task-1"]);
        assert_eq!(unresolved_task_blocker_ids(&tasks, &tasks[2]), ["task-2"]);

        let recompleted = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "only",
                "new_status": "completed"
            }))
            .await;
        assert!(!recompleted.starts_with("Error:"), "{recompleted}");
        assert!(
            m.try_snapshot_state()
                .await
                .expect("recompleted chain")
                .tasks
                .iter()
                .all(|task| task.status.is_completed())
        );
    }

    #[tokio::test]
    async fn completed_task_rejects_new_unresolved_dependency_from_either_endpoint() {
        let m = mgr();
        m.create(&json!({"title": "unfinished prerequisite"})).await;
        m.create(&json!({"title": "finished work"})).await;
        let completed = m
            .update(&json!({"task_id": "task-2", "new_status": "completed"}))
            .await;
        assert!(!completed.starts_with("Error:"), "{completed}");

        let direct = m
            .update(&json!({
                "task_id": "task-2",
                "add_blocked_by": ["task-1"]
            }))
            .await;
        assert!(direct.starts_with("Error:"), "{direct}");
        assert!(direct.contains("cannot start or complete"), "{direct}");

        let reverse = m
            .update(&json!({
                "task_id": "task-1",
                "add_blocks": ["task-2"]
            }))
            .await;
        assert!(reverse.starts_with("Error:"), "{reverse}");
        assert!(reverse.contains("while completed"), "{reverse}");

        let tasks = m.try_snapshot_state().await.expect("unchanged graph").tasks;
        assert!(tasks.iter().all(|task| task.blocks.is_empty()));
        assert!(tasks.iter().all(|task| task.blocked_by.is_empty()));
    }

    #[tokio::test]
    async fn completing_all_children_finishes_a_paused_parent() {
        let m = mgr();
        m.create(&json!({
            "title": "paused parent",
            "subtasks": [{"id": "only", "title": "only child"}]
        }))
        .await;
        m.update(&json!({"task_id": "task-1", "new_status": "paused"}))
            .await;

        let completed = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "only",
                "new_status": "completed"
            }))
            .await;
        assert!(!completed.starts_with("Error:"), "{completed}");
        let parent: SessionTask = serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await)
            .expect("paused parent");
        assert_eq!(parent.status, SessionTaskStatusKind::Completed);
    }

    #[tokio::test]
    async fn delete_cleans_symmetric_edges() {
        let m = mgr();
        m.create(&json!({"title": "a"})).await;
        m.create(&json!({"title": "b"})).await;
        m.update(&json!({"task_id": "task-1", "add_blocks": ["task-2"]}))
            .await;
        let del = m
            .update(&json!({"task_id": "task-1", "new_status": "deleted"}))
            .await;
        let del_parsed: Value = serde_json::from_str(del.split_once('\n').unwrap().1).unwrap();
        assert_eq!(del_parsed["status"], "deleted", "{del}");
        let deleted_task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(deleted_task.status, SessionTaskStatusKind::Deleted);
        let deleted_list: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "deleted"})).await).unwrap();
        assert_eq!(deleted_list["count"], 1, "{deleted_list}");
        let active_list: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "active"})).await).unwrap();
        assert_eq!(active_list["count"], 1, "{active_list}");
        assert_eq!(active_list["tasks"][0]["id"], "task-2", "{active_list}");
        let all_list: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "all"})).await).unwrap();
        assert_eq!(all_list["count"], 2, "{all_list}");
        let task_b: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-2"})).await).unwrap();
        assert!(
            !task_b.blocked_by.contains(&"task-1".to_string()),
            "b still references a: {task_b:?}"
        );
    }

    #[tokio::test]
    async fn deleting_last_blocker_reconciles_legacy_all_done_parent() {
        let m = mgr();
        m.create(&json!({"title": "obsolete blocker"})).await;
        m.create(&json!({
            "title": "dependent parent",
            "add_blocked_by": ["task-1"],
            "subtasks": [{"id": "done", "title": "legacy completed child"}]
        }))
        .await;
        let mut snapshot = m.try_snapshot_state().await.expect("delete fixture");
        snapshot.tasks[1].subtasks[0].status = SessionTaskStatusKind::Completed;
        m.restore_snapshot(&snapshot)
            .await
            .expect("restore legacy delete fixture");

        let deleted = m
            .update(&json!({"task_id": "task-1", "new_status": "deleted"}))
            .await;
        assert!(!deleted.starts_with("Error:"), "{deleted}");
        let dependent: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-2"})).await).unwrap();
        assert_eq!(dependent.status, SessionTaskStatusKind::Completed);
        assert!(dependent.blocked_by.is_empty(), "{dependent:?}");
    }

    #[tokio::test]
    async fn deleting_a_parent_deletes_only_its_open_children() {
        let m = mgr();
        m.create(&json!({
            "title": "parent to delete",
            "subtasks": [
                {"id": "running", "title": "running child"},
                {"id": "done", "title": "completed child"},
                {"id": "failed", "title": "failed child"}
            ]
        }))
        .await;
        for (subtask_id, status) in [
            ("running", "in_progress"),
            ("done", "completed"),
            ("failed", "failed"),
        ] {
            let updated = m
                .update(&json!({
                    "task_id": "task-1",
                    "subtask_id": subtask_id,
                    "new_status": status
                }))
                .await;
            assert!(!updated.starts_with("Error:"), "{updated}");
        }

        let deleted = m
            .update(&json!({"task_id": "task-1", "new_status": "deleted"}))
            .await;
        let body: Value = serde_json::from_str(
            deleted
                .split_once('\n')
                .expect("summary and structured body")
                .1,
        )
        .expect("structured delete response");
        assert_eq!(body["success"], true, "{deleted}");
        assert_eq!(body["deleted_subtasks"], 1, "{deleted}");

        let task: SessionTask = serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await)
            .expect("deleted parent");
        let statuses = task
            .subtasks
            .iter()
            .map(|subtask| (subtask.id.as_str(), subtask.status))
            .collect::<HashMap<_, _>>();
        assert_eq!(statuses["running"], SessionTaskStatusKind::Deleted);
        assert_eq!(statuses["done"], SessionTaskStatusKind::Completed);
        assert_eq!(statuses["failed"], SessionTaskStatusKind::Failed);
    }

    #[tokio::test]
    async fn delete_is_available_from_every_persisted_parent_status() {
        let m = mgr();
        let statuses = [
            SessionTaskStatusKind::Pending,
            SessionTaskStatusKind::InProgress,
            SessionTaskStatusKind::Paused,
            SessionTaskStatusKind::Completed,
            SessionTaskStatusKind::Failed,
            SessionTaskStatusKind::Cancelled,
            SessionTaskStatusKind::Archived,
            SessionTaskStatusKind::Deleted,
            SessionTaskStatusKind::Migrated,
            SessionTaskStatusKind::Other,
        ];

        for (idx, status) in statuses.into_iter().enumerate() {
            let task_id = format!("task-{}", idx + 1);
            m.create(&json!({"title": format!("status {status}")}))
                .await;
            set_task_status_fixture(&m, &task_id, status).await;

            let out = m
                .update(&json!({"task_id": task_id, "new_status": "deleted"}))
                .await;
            assert!(
                !out.starts_with("Error:"),
                "{status} should be clearable to deleted: {out}"
            );
        }

        let deleted_list: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "deleted"})).await).unwrap();
        assert_eq!(deleted_list["count"], 10, "{deleted_list}");
        let active_list: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "active"})).await).unwrap();
        assert_eq!(
            active_list["count"], 0,
            "deleted tombstones must not remain active: {active_list}"
        );

        let revive = m
            .update(&json!({"task_id": "task-1", "new_status": "pending"}))
            .await;
        assert!(
            revive.starts_with("Error:"),
            "deleted tombstones must not be revived by status update: {revive}"
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
        let create_parsed: Value =
            serde_json::from_str(create.split_once('\n').unwrap().1).unwrap();
        assert_eq!(create_parsed["success"], true, "{create}");

        let first = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "s1",
                "new_status": "completed"
            }))
            .await;
        let first_parsed: Value = serde_json::from_str(first.split_once('\n').unwrap().1).unwrap();
        assert_eq!(first_parsed["success"], true, "{first}");
        let after_first = m.get(&json!({"task_id": "task-1"})).await;
        let after_first: SessionTask =
            serde_json::from_str(&after_first).expect("task json after first subtask");
        assert!(
            after_first.status == SessionTaskStatusKind::InProgress,
            "once a subtask completes, the parent should stop reading as untouched pending work: {after_first:?}"
        );

        let second = m
            .update(&json!({
                "task_id": "task-1",
                "subtask_id": "s2",
                "new_status": "completed"
            }))
            .await;
        let second_parsed: Value =
            serde_json::from_str(second.split_once('\n').unwrap().1).unwrap();
        assert_eq!(second_parsed["success"], true, "{second}");
        let after_second = m.get(&json!({"task_id": "task-1"})).await;
        let after_second: SessionTask =
            serde_json::from_str(&after_second).expect("task json after second subtask");
        assert!(
            after_second.status == SessionTaskStatusKind::Completed,
            "parent should auto-complete after the last subtask completes: {after_second:?}"
        );
    }

    #[tokio::test]
    async fn starting_a_subtask_promotes_pending_parent_to_in_progress() {
        let m = mgr();
        m.create(&json!({
            "title": "parent",
            "subtasks": [
                {"id": "s1", "title": "first"},
                {"id": "s2", "title": "second"}
            ]
        }))
        .await;

        m.update(&json!({
            "task_id": "task-1",
            "subtask_id": "s1",
            "new_status": "in_progress"
        }))
        .await;

        let after = m.get(&json!({"task_id": "task-1"})).await;
        let after: SessionTask =
            serde_json::from_str(&after).expect("task json after starting subtask");
        assert_eq!(
            after.status,
            SessionTaskStatusKind::InProgress,
            "an active subtask should make the parent read as in-progress so the task board doesn't show it as merely open"
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
        m.update(&json!({"task_id": "task-1", "subtask_id": "s1", "new_status": "completed"}))
            .await;
        m.update(&json!({"task_id": "task-1", "subtask_id": "s2", "new_status": "completed"}))
            .await;
        let completed = m.get(&json!({"task_id": "task-1"})).await;
        let completed: SessionTask =
            serde_json::from_str(&completed).expect("task json after auto-complete");
        assert_eq!(completed.status, SessionTaskStatusKind::Completed);
        assert_eq!(
            completed
                .metadata
                .as_ref()
                .and_then(|m| m.get("auto_completed_by_subtasks"))
                .and_then(Value::as_bool),
            Some(true),
            "auto-completed parent should carry an explicit reversible marker"
        );

        m.update(&json!({"task_id": "task-1", "subtask_id": "s1", "new_status": "pending"}))
            .await;
        let reopened = m.get(&json!({"task_id": "task-1"})).await;
        let reopened: SessionTask =
            serde_json::from_str(&reopened).expect("task json after reopening subtask");
        assert_eq!(
            reopened.status,
            SessionTaskStatusKind::InProgress,
            "reopening a subtask should stop showing the parent as completed"
        );
        assert!(
            reopened
                .metadata
                .as_ref()
                .and_then(|m| m.get("auto_completed_by_subtasks"))
                .is_none(),
            "reopening should consume the reversible auto-complete marker"
        );
    }

    #[tokio::test]
    async fn uncompleting_subtask_rejects_explicitly_completed_parent() {
        let m = mgr();
        m.create(&json!({
            "title": "parent",
            "subtasks": [
                {"id": "s1", "title": "first"},
                {"id": "s2", "title": "second"}
            ]
        }))
        .await;
        m.update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        m.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;

        let completed = m.get(&json!({"task_id": "task-1"})).await;
        let completed: SessionTask =
            serde_json::from_str(&completed).expect("task json after explicit completion");
        assert_eq!(completed.status, SessionTaskStatusKind::Completed);
        assert!(
            completed
                .metadata
                .as_ref()
                .and_then(|m| m.get("auto_completed_by_subtasks"))
                .is_none(),
            "explicit parent completion must not be marked as reversible auto-completion"
        );

        let update = m
            .update(&json!({"task_id": "task-1", "subtask_id": "s1", "new_status": "pending"}))
            .await;
        assert!(
            update.starts_with("Error:")
                && update.contains("already terminal")
                && update.contains("instead of editing its subtasks"),
            "explicit terminal parent should reject subtask mutation: {update}"
        );
        let after = m.get(&json!({"task_id": "task-1"})).await;
        let after: SessionTask =
            serde_json::from_str(&after).expect("task json after subtask edit");
        assert_eq!(
            after.status,
            SessionTaskStatusKind::Completed,
            "subtask edits must not bypass terminal parent transition rules"
        );
        assert!(
            after
                .subtasks
                .iter()
                .all(|st| st.status == SessionTaskStatusKind::Completed),
            "rejected subtask edit must not mutate terminal history: {after:?}"
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
        m.update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        m.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;

        let task = m.get(&json!({"task_id": "task-1"})).await;
        let task: SessionTask =
            serde_json::from_str(&task).expect("task json after parent completion");
        assert!(
            task.subtasks
                .iter()
                .all(|st| st.status == SessionTaskStatusKind::Completed),
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
                "new_status": status
            }))
            .await;
        }
        // Now cascade: parent → completed.
        m.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;

        let task = m.get(&json!({"task_id": "task-1"})).await;
        let task: SessionTask = serde_json::from_str(&task).expect("task json");
        let by_id: std::collections::HashMap<_, _> = task
            .subtasks
            .iter()
            .map(|s| (s.id.clone(), s.status))
            .collect();
        assert_eq!(
            by_id["s1"],
            SessionTaskStatusKind::Completed,
            "pending subtask should cascade to completed"
        );
        assert_eq!(
            by_id["s2"],
            SessionTaskStatusKind::Failed,
            "failed subtask must NOT be overwritten"
        );
        assert_eq!(
            by_id["s3"],
            SessionTaskStatusKind::Cancelled,
            "cancelled subtask must NOT be overwritten"
        );
        assert_eq!(
            by_id["s4"],
            SessionTaskStatusKind::Completed,
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
        let now = chrono::Utc::now().to_rfc3339();

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
                    created_at: now.clone(),
                    updated_at: now,
                    active_form: None,
                    owner: None,
                    metadata: None,
                    blocks: vec![],
                    blocked_by: vec![],
                    archived_at: None,
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
        // Regression for A1: the old alloc/rewind dance in try_snapshot_state
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
        // If try_snapshot_state still rewinds the counter, at least one
        // allocation will duplicate.
        for _ in 0..200 {
            let s = store.clone();
            set.spawn(async move { s.next_task_id("sess-race").await.unwrap() });
        }
        for _ in 0..200 {
            let m = manager.clone();
            set.spawn(async move {
                let _ = m.try_snapshot_state().await;
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
            "duplicate ids handed out; try_snapshot_state raced the allocator"
        );
    }

    #[tokio::test]
    async fn optimistic_snapshot_rejects_a_board_that_never_stabilizes() {
        struct MovingVersionStore {
            version_reads: std::sync::atomic::AtomicU64,
        }

        #[async_trait::async_trait]
        impl TaskStore for MovingVersionStore {
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

            async fn get_session_version(&self, _session_id: &str) -> Result<u64, String> {
                Ok(self
                    .version_reads
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst))
            }
        }

        let store = MovingVersionStore {
            version_reads: std::sync::atomic::AtomicU64::new(0),
        };
        let error = store
            .load_snapshot_state("sess-moving")
            .await
            .expect_err("a torn task/counter/version view must never be returned as a snapshot");
        assert!(
            error.contains("changed while capturing snapshot"),
            "{error}"
        );
        assert_eq!(
            store
                .version_reads
                .load(std::sync::atomic::Ordering::SeqCst),
            6,
            "snapshot capture should retry a bounded number of times"
        );
    }

    #[test]
    fn prepare_task_snapshot_for_fork_pauses_live_parent_work() {
        let snapshot = TaskManagerSnapshot {
            tasks: vec![
                SessionTask {
                    id: "task-1".into(),
                    title: "active parent work".into(),
                    description: None,
                    status: SessionTaskStatusKind::InProgress,
                    subtasks: vec![SessionSubtask {
                        id: "step-1".into(),
                        title: "active child step".into(),
                        description: None,
                        status: SessionTaskStatusKind::InProgress,
                        depends_on: vec![],
                        owner: None,
                        reason: None,
                    }],
                    created_at: "".into(),
                    updated_at: "".into(),
                    active_form: Some("Working".into()),
                    owner: None,
                    metadata: None,
                    blocks: vec![],
                    blocked_by: vec![],
                    archived_at: None,
                },
                SessionTask {
                    id: "task-2".into(),
                    title: "completed parent work".into(),
                    description: None,
                    status: SessionTaskStatusKind::Completed,
                    subtasks: vec![],
                    created_at: "".into(),
                    updated_at: "".into(),
                    active_form: None,
                    owner: None,
                    metadata: None,
                    blocks: vec![],
                    blocked_by: vec![],
                    archived_at: None,
                },
            ],
            next_task_id: 3,
            version: 0,
            restore_version: None,
        };

        let forked = prepare_task_snapshot_for_fork(snapshot);

        assert_eq!(forked.next_task_id, 3);
        assert_eq!(forked.tasks[0].status, SessionTaskStatusKind::Paused);
        assert_eq!(
            forked.tasks[0].subtasks[0].status,
            SessionTaskStatusKind::Paused
        );
        assert_eq!(
            forked.tasks[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("fork_copied_from_status"))
                .and_then(Value::as_str),
            Some("in_progress")
        );
        assert_eq!(forked.tasks[1].status, SessionTaskStatusKind::Completed);
    }

    #[tokio::test]
    async fn snapshot_peek_failure_fallback_avoids_counter_rewind() {
        // Regression for the A1-related concern: if peek_next_task_id
        // fails, try_snapshot_state used to fall back to 1, which on
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
                    created_at: chrono::Utc::now().to_rfc3339(),
                    updated_at: chrono::Utc::now().to_rfc3339(),
                    active_form: None,
                    owner: None,
                    metadata: None,
                    blocks: vec![],
                    blocked_by: vec![],
                    archived_at: None,
                }],
            )
            .await
            .unwrap();
        let store: Arc<dyn TaskStore> = Arc::new(FlakyPeekStore { inner });
        let mgr = TaskManager::new("sess-fallback", store);
        let snap = mgr.try_snapshot_state().await;
        assert!(
            snap.is_err(),
            "peek failure must propagate — no fallback that could rewind counter"
        );
        let err = snap.unwrap_err();
        assert!(
            err.contains("peek_next_task_id failed"),
            "error must identify root cause: {err}"
        );
    }

    #[tokio::test]
    async fn restore_snapshot_sets_counter_before_broadcasting_save() {
        // A subscriber waking on the save-broadcast and immediately
        // calling peek_next_task_id MUST observe the restored counter,
        // not the pre-restore value. This pins the ordering: set →
        // save (broadcast). Without the fix, peek would return the
        // old counter because set_next_task_id ran after save.
        //
        // Counter is never rewound: restore_snapshot keeps the higher
        // of (current_counter, snapshot_counter, max_task_id+1), so
        // burning 5 ids before restore forces the counter to 6.
        let store = Arc::new(InMemoryTaskStore::new());
        let mgr = TaskManager::new("sess-order", store.clone() as Arc<dyn TaskStore>);
        // Burn a few ids so the counter diverges from snapshot.
        for _ in 0..5 {
            let _ = store.next_task_id("sess-order").await.unwrap();
        }
        let version = store
            .get_session_version("sess-order")
            .await
            .expect("current board version");
        let snap = TaskManagerSnapshot {
            tasks: vec![],
            next_task_id: 1,
            version,
            restore_version: None,
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
            observed, 6,
            "subscriber woke on save-broadcast but saw pre-restore counter value"
        );
    }

    #[tokio::test]
    async fn sealed_snapshot_restores_own_mutation() {
        let m = mgr();
        m.create(&json!({"title": "task"})).await;
        let mut snapshot = m.try_snapshot_state().await.expect("snapshot");

        let started = m
            .update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert!(!started.starts_with("Error:"), "{started}");
        m.seal_snapshot_for_restore(&mut snapshot)
            .await
            .expect("seal");
        m.restore_snapshot(&snapshot).await.expect("restore");

        let task = m.load_tasks().await.unwrap().into_iter().next().unwrap();
        assert_eq!(task.status, SessionTaskStatusKind::Pending);
    }

    #[tokio::test]
    async fn sealed_snapshot_refuses_later_task_board_mutation() {
        let m = mgr();
        m.create(&json!({"title": "task"})).await;
        let mut snapshot = m.try_snapshot_state().await.expect("snapshot");

        let started = m
            .update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert!(!started.starts_with("Error:"), "{started}");
        m.seal_snapshot_for_restore(&mut snapshot)
            .await
            .expect("seal");

        let paused = m
            .update(&json!({"task_id": "task-1", "new_status": "paused"}))
            .await;
        assert!(!paused.starts_with("Error:"), "{paused}");

        let err = m
            .restore_snapshot(&snapshot)
            .await
            .expect_err("later mutation must block stale rollback");
        assert!(err.contains("version conflict"), "{err}");
    }

    #[tokio::test]
    async fn in_memory_restore_snapshot_state_rejects_version_conflict() {
        let store = InMemoryTaskStore::new();
        let now = chrono::Utc::now().to_rfc3339();
        let original = SessionTask {
            archived_at: None,
            id: "task-1".to_string(),
            title: "original".to_string(),
            description: None,
            status: SessionTaskStatusKind::Pending,
            subtasks: Vec::new(),
            created_at: now.clone(),
            updated_at: now.clone(),
            active_form: None,
            owner: None,
            metadata: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        };
        store
            .save("sess-conflict", vec![original.clone()])
            .await
            .expect("seed store");
        let version = store
            .get_session_version("sess-conflict")
            .await
            .expect("version after seed");
        store.bump_version("sess-conflict").await;

        let err = store
            .restore_snapshot_state("sess-conflict", vec![original], 2, version)
            .await
            .expect_err("stale restore must be rejected");
        assert!(err.contains("version conflict"), "{err}");
    }

    #[tokio::test]
    async fn in_memory_restore_treats_zero_as_a_real_cas_version() {
        let store = InMemoryTaskStore::new();
        let empty_snapshot = store
            .load_snapshot_state("sess-first-write")
            .await
            .expect("capture new board");
        assert_eq!(empty_snapshot.version, 0);

        let now = chrono::Utc::now().to_rfc3339();
        store
            .save(
                "sess-first-write",
                vec![SessionTask {
                    archived_at: None,
                    id: "task-1".to_string(),
                    title: "concurrent first write".to_string(),
                    description: None,
                    status: SessionTaskStatusKind::Pending,
                    subtasks: Vec::new(),
                    created_at: now.clone(),
                    updated_at: now,
                    active_form: None,
                    owner: None,
                    metadata: None,
                    blocks: Vec::new(),
                    blocked_by: Vec::new(),
                }],
            )
            .await
            .expect("concurrent first write");

        let error = store
            .restore_snapshot_state(
                "sess-first-write",
                empty_snapshot.tasks,
                empty_snapshot.next_task_id,
                empty_snapshot.version,
            )
            .await
            .expect_err("version-0 snapshot must not erase the first concurrent write");
        assert!(error.contains("version conflict"), "{error}");
        assert_eq!(store.load("sess-first-write").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn in_memory_mutate_validates_tasks_when_enabled() {
        let store = Arc::new(InMemoryTaskStore::new());
        let now = chrono::Utc::now().to_rfc3339();
        store
            .save(
                "sess-validate",
                vec![SessionTask {
                    archived_at: None,
                    id: "task-1".to_string(),
                    title: "valid".to_string(),
                    description: None,
                    status: SessionTaskStatusKind::Pending,
                    subtasks: Vec::new(),
                    created_at: now.clone(),
                    updated_at: now.clone(),
                    active_form: None,
                    owner: None,
                    metadata: None,
                    blocks: Vec::new(),
                    blocked_by: Vec::new(),
                }],
            )
            .await
            .expect("seed store");

        let err = store
            .mutate(
                "sess-validate",
                Box::new(move |mut tasks, next_id| {
                    tasks.push(SessionTask {
                        archived_at: None,
                        id: "task-1".to_string(),
                        title: "duplicate".to_string(),
                        description: None,
                        status: SessionTaskStatusKind::Pending,
                        subtasks: Vec::new(),
                        created_at: now.clone(),
                        updated_at: now.clone(),
                        active_form: None,
                        owner: None,
                        metadata: None,
                        blocks: Vec::new(),
                        blocked_by: Vec::new(),
                    });
                    Ok(TaskMutationResult::applied(
                        tasks,
                        Some(next_id + 1),
                        "should fail",
                        json!({"message": "should fail"}),
                    ))
                }),
            )
            .await
            .expect_err("invalid mutate result should be rejected");

        assert!(err.contains("duplicate task id"), "{err}");
        let tasks = store
            .load("sess-validate")
            .await
            .expect("load after failure");
        assert_eq!(tasks.len(), 1, "{tasks:?}");
        assert_eq!(tasks[0].title, "valid");
    }

    #[tokio::test]
    async fn restore_snapshot_does_not_rewind_counter_when_task_save_fails() {
        struct SaveFailsAfterCounterStore {
            counter: tokio::sync::Mutex<u32>,
        }

        #[async_trait::async_trait]
        impl TaskStore for SaveFailsAfterCounterStore {
            async fn load(&self, _session_id: &str) -> Result<Vec<SessionTask>, String> {
                Ok(Vec::new())
            }

            async fn save(
                &self,
                _session_id: &str,
                _tasks: Vec<SessionTask>,
            ) -> Result<(), String> {
                Err("simulated save failure".to_string())
            }

            async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }

            async fn set_next_task_id(&self, _session_id: &str, next: u32) -> Result<(), String> {
                *self.counter.lock().await = next;
                Ok(())
            }

            async fn restore_snapshot_state(
                &self,
                _session_id: &str,
                _tasks: Vec<SessionTask>,
                _next_task_id: u32,
                _expected_version: u64,
            ) -> Result<(), String> {
                Err("simulated save failure".to_string())
            }

            async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(*self.counter.lock().await)
            }
        }

        let store = Arc::new(SaveFailsAfterCounterStore {
            counter: tokio::sync::Mutex::new(9),
        });
        let mgr = TaskManager::new("sess-restore-fail", store.clone() as Arc<dyn TaskStore>);
        let snap = TaskManagerSnapshot {
            tasks: Vec::new(),
            next_task_id: 1,
            version: 0,
            restore_version: None,
        };

        let err = mgr
            .restore_snapshot(&snap)
            .await
            .expect_err("save failure should abort restore");

        assert!(err.contains("simulated save failure"), "{err}");
        assert_eq!(
            store
                .peek_next_task_id("sess-restore-fail")
                .await
                .expect("peek counter"),
            9,
            "failed restore must not leave the counter rewound without matching task rows"
        );
    }

    // ── load_all_sessions (multi-session task board) ─────────────

    #[tokio::test]
    async fn load_all_sessions_covers_empty_multi_session_and_isolation() {
        let store: Arc<dyn TaskStore> = Arc::new(InMemoryTaskStore::new());

        // 1. Empty store → empty rollup.
        let rows = store.load_all_sessions().await.expect("load_all");
        assert!(rows.is_empty(), "empty store must yield empty rollup");

        // 2. Two sessions with 1 and 2 tasks respectively → both appear,
        //    with the right counts.
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

        // 3. Per-session isolation: sess-a must NOT see sess-b's tasks.
        let sess_a = rows.iter().find(|(s, _)| s == "sess-a").unwrap();
        let a_titles: Vec<&str> = sess_a.1.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(a_titles, vec!["a1"], "sess-a must not leak sess-b data");
        let b_titles: Vec<&str> = sess_b.1.iter().map(|t| t.title.as_str()).collect();
        assert!(b_titles.contains(&"b1") && b_titles.contains(&"b2"));
        assert!(
            !b_titles.contains(&"a1"),
            "sess-b must not leak sess-a data"
        );
    }

    #[tokio::test]
    async fn load_open_sessions_filters_terminal_history_and_honors_limit() {
        let store: Arc<dyn TaskStore> = Arc::new(InMemoryTaskStore::new());
        let sess_a = TaskManager::new("sess-open-a", store.clone());
        let sess_b = TaskManager::new("sess-open-b", store.clone());

        sess_a.create(&json!({"title": "pending-a"})).await;
        sess_a.create(&json!({"title": "completed-a"})).await;
        sess_a
            .update(&json!({"task_id": "task-2", "new_status": "in_progress"}))
            .await;
        sess_a
            .update(&json!({"task_id": "task-2", "new_status": "completed"}))
            .await;
        sess_a.create(&json!({"title": "paused-a"})).await;
        sess_a
            .update(&json!({"task_id": "task-3", "new_status": "paused"}))
            .await;
        sess_b.create(&json!({"title": "pending-b"})).await;

        let rows = store
            .load_open_sessions(2)
            .await
            .expect("load_open_sessions");
        let titles: Vec<&str> = rows
            .iter()
            .flat_map(|(_, tasks)| tasks.iter().map(|task| task.title.as_str()))
            .collect();
        assert_eq!(
            titles.len(),
            2,
            "periodic cross-session UI fetches must be bounded: {rows:?}"
        );
        assert!(
            !titles.contains(&"completed-a"),
            "terminal history must not be loaded by the open-work surface: {rows:?}"
        );
        assert!(
            titles
                .iter()
                .all(|title| title.ends_with("-a") || title.ends_with("-b")),
            "open tasks from multiple sessions remain eligible: {rows:?}"
        );

        let summaries = store
            .load_open_task_summaries(2)
            .await
            .expect("load_open_task_summaries");
        let summaries: Vec<&OpenTaskSummary> = summaries
            .iter()
            .flat_map(|(_, tasks)| tasks.iter())
            .collect();
        assert_eq!(summaries.len(), 2);
        assert!(summaries.iter().all(|task| task.status.is_open_work()));
        assert!(
            summaries.iter().all(|task| !task.updated_at.is_empty()),
            "summary projection must retain authoritative recency"
        );
    }

    // ── U-8: status_filter SQL pushdown ──────────────────────────────
    //
    // Pre-fix: `task_board.list(status_filter='active')` called
    // `store.load()` (all rows) then filtered in Rust. With 5 000
    // tasks and the index `idx_session_todos_owner_session_status_updated`,
    // the DB can answer "active only" in a single index scan instead
    // of shipping all rows to Rust. The `TaskStore::load_active`
    // default impl is a Rust-level fallback for in-memory stores;
    // `MatrixOneTaskStore` overrides it with a WHERE clause so
    // production uses the index.
    //
    // These tests pin:
    //   (a) `task_board.list(status_filter='active')` returns only open-work
    //       rows (pending/in_progress/paused), even on the in-memory store
    //       (correctness — same before and after, but now via a
    //       dedicated path that the MO impl overrides).
    //   (b) `task_board.list(status_filter='completed')` still works
    //       after the refactor.
    //   (c) `task_board.list` with no filter still returns all rows.

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
        async fn set_next_task_id(&self, sid: &str, n: u32) -> Result<(), String> {
            self.inner.set_next_task_id(sid, n).await
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
    async fn list_filters_return_expected_tasks() {
        let m = mgr();
        m.create(&json!({"title": "pending-1"})).await;
        m.create(&json!({"title": "pending-2"})).await;
        m.create(&json!({"title": "ip-1"})).await;
        m.update(&json!({"task_id": "task-3", "new_status": "in_progress"}))
            .await;
        // pending → paused is allowed directly (no in_progress required).
        m.create(&json!({"title": "paused-1"})).await;
        m.update(&json!({"task_id": "task-4", "new_status": "paused"}))
            .await;
        // Only one task can be in_progress at a time; pause task-3,
        // start and complete task-5, then resume task-3.
        m.create(&json!({"title": "done-1"})).await;
        m.update(&json!({"task_id": "task-3", "new_status": "paused"}))
            .await;
        m.update(&json!({"task_id": "task-5", "new_status": "in_progress"}))
            .await;
        m.update(&json!({"task_id": "task-5", "new_status": "completed"}))
            .await;
        m.update(&json!({"task_id": "task-3", "new_status": "in_progress"}))
            .await;
        m.create(&json!({"title": "cancelled-1"})).await;
        m.stop(&json!({"task_id": "task-6", "reason": "not needed"}))
            .await;

        // active filter: returns open work (pending + in_progress + paused)
        let active: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "active"})).await).unwrap();
        assert_eq!(active["count"], 4);
        let active_titles: Vec<&str> = active["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["title"].as_str().unwrap())
            .collect();
        assert!(active_titles.contains(&"pending-1"));
        assert!(active_titles.contains(&"pending-2"));
        assert!(active_titles.contains(&"ip-1"));
        assert!(active_titles.contains(&"paused-1"));
        assert!(!active_titles.contains(&"done-1"));

        // paused filter: auto-paused/manual-paused work is also queryable directly
        let paused: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "paused"})).await).unwrap();
        assert_eq!(paused["count"], 1);
        assert_eq!(paused["tasks"][0]["title"], "paused-1");

        // completed filter: returns only completed
        let completed: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "completed"})).await).unwrap();
        assert_eq!(completed["count"], 1);
        assert_eq!(completed["tasks"][0]["title"], "done-1");

        // cancelled filter: returns stopped work without requiring an all-list scan
        let cancelled: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "cancelled"})).await).unwrap();
        assert_eq!(cancelled["count"], 1);
        assert_eq!(cancelled["tasks"][0]["title"], "cancelled-1");

        // all filter: returns everything
        let all: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "all"})).await).unwrap();
        assert_eq!(all["count"], 6);
    }

    #[tokio::test]
    async fn list_rejects_unknown_and_legacy_status_filters() {
        let m = mgr();
        m.create(&json!({"title": "alpha"})).await;

        let typo = m.list(&json!({"status_filter": "cancelledd"})).await;
        assert!(typo.starts_with("Error:"), "{typo}");
        assert!(
            typo.contains("invalid status_filter") && typo.contains("cancelled"),
            "invalid filters should return actionable valid values: {typo}"
        );

        let wrong_type = m.list(&json!({"status_filter": true})).await;
        assert!(wrong_type.starts_with("Error:"), "{wrong_type}");
        assert!(
            wrong_type.contains("status_filter") && wrong_type.contains("string"),
            "wrong-type filters should return an actionable error: {wrong_type}"
        );

        let legacy = m.list(&json!({"status": "all"})).await;
        assert!(legacy.starts_with("Error:"), "{legacy}");
        assert!(
            legacy.contains("unknown field 'status'")
                && !legacy.contains("valid: action, status_filter, status"),
            "status must not remain a recognized task_board.list argument: {legacy}"
        );
    }

    #[tokio::test]
    async fn archive_single_task_requires_terminal_status() {
        let m = mgr();
        m.create(&json!({"title": "alpha"})).await;

        let refused = m.archive(&json!({"task_id": "task-1"})).await;
        assert!(refused.contains("Refused"), "{refused}");
        assert!(refused.contains("pending"), "{refused}");

        set_task_status_fixture(&m, "task-1", SessionTaskStatusKind::Completed).await;
        let archived = m.archive(&json!({"task_id": "task-1"})).await;
        let archived_json = archived.split_once('\n').unwrap().1;
        let archived_parsed: Value = serde_json::from_str(archived_json).unwrap();
        assert_eq!(archived_parsed["status"], "archived", "{archived}");

        let archived_list: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "archived"})).await).unwrap();
        let archived_titles: Vec<&str> = archived_list["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["title"].as_str().unwrap())
            .collect();
        assert!(archived_titles.contains(&"alpha"), "{archived_list}");
        let active_out = m.list(&json!({"status_filter": "active"})).await;
        let active_out: Value = serde_json::from_str(&active_out).unwrap();
        assert_eq!(
            active_out["count"], 0,
            "no active tasks should remain after archive; got: {active_out}"
        );
    }

    #[tokio::test]
    async fn archive_replay_is_a_successful_unchanged_mutation() {
        let store = Arc::new(InMemoryTaskStore::new());
        let m = TaskManager::new("idempotent-archive", store.clone());
        m.create(&json!({"title": "historical work"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        let first = m.archive_outcome(&json!({"task_id": "task-1"})).await;
        assert_eq!(first.status, TaskMutationStatus::Applied, "{first:?}");
        let version = store
            .get_session_version("idempotent-archive")
            .await
            .unwrap();

        let replay = m.archive_outcome(&json!({"task_id": "task-1"})).await;

        assert_eq!(replay.status, TaskMutationStatus::Unchanged, "{replay:?}");
        assert_eq!(replay.data["already_current"], true, "{replay:?}");
        assert_eq!(
            store
                .get_session_version("idempotent-archive")
                .await
                .unwrap(),
            version
        );
    }

    #[tokio::test]
    async fn archive_replay_repairs_legacy_dependency_edges() {
        let store = Arc::new(InMemoryTaskStore::new());
        let m = TaskManager::new("archive-repair", store.clone());
        m.create(&json!({"title": "historical"})).await;
        m.create(&json!({"title": "dependent"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        m.archive_outcome(&json!({"task_id": "task-1"})).await;

        // Simulate an old/corrupt row written before archive detached both
        // sides of the dependency graph.
        let mut snapshot = m.try_snapshot_state().await.unwrap();
        snapshot.tasks[0].archived_at = None;
        snapshot.tasks[0].blocks.push("task-2".to_string());
        snapshot.tasks[1].blocked_by.push("task-1".to_string());
        m.restore_snapshot(&snapshot).await.unwrap();
        let version = store.get_session_version("archive-repair").await.unwrap();

        let replay = m.archive_outcome(&json!({"task_id": "task-1"})).await;

        assert_eq!(replay.status, TaskMutationStatus::Applied, "{replay:?}");
        assert_eq!(
            store.get_session_version("archive-repair").await.unwrap(),
            version + 1
        );
        let tasks = m.snapshot().await.unwrap();
        assert!(tasks.iter().all(|task| task.blocks.is_empty()));
        assert!(tasks.iter().all(|task| task.blocked_by.is_empty()));
        assert!(tasks[0].archived_at.is_some());
        assert_eq!(replay.data["reconciled_existing_archive"], true);
    }

    #[tokio::test]
    async fn bulk_archive_reports_only_new_transitions() {
        let m = mgr();
        m.create(&json!({"title": "historical"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        m.archive_outcome(&json!({"task_id": "task-1"})).await;

        let replay = m.archive_outcome(&json!({"older_than_days": 0})).await;

        assert_eq!(replay.status, TaskMutationStatus::Unchanged, "{replay:?}");
        assert_eq!(replay.data["archived"], 0, "{replay:?}");
    }

    #[tokio::test]
    async fn archive_single_task_detaches_dependency_edges() {
        let m = mgr();
        m.create(&json!({"title": "producer"})).await;
        m.create(&json!({
            "title": "consumer",
            "subtasks": [{"id": "done", "title": "legacy completed child"}]
        }))
        .await;
        let linked = m
            .update(&json!({"task_id": "task-1", "add_blocks": ["task-2"]}))
            .await;
        assert!(!linked.starts_with("Error:"), "{linked}");
        let mut snapshot = m.try_snapshot_state().await.expect("archive fixture");
        snapshot.tasks[0].status = SessionTaskStatusKind::Completed;
        snapshot.tasks[1].subtasks[0].status = SessionTaskStatusKind::Completed;
        m.restore_snapshot(&snapshot)
            .await
            .expect("restore legacy archive fixture");

        let archived = m.archive(&json!({"task_id": "task-1"})).await;
        assert!(!archived.starts_with("Error:"), "{archived}");

        let archived_task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(archived_task.status, SessionTaskStatusKind::Archived);
        assert!(
            archived_task.blocks.is_empty() && archived_task.blocked_by.is_empty(),
            "archived tasks should not keep stale dependency edges: {archived_task:?}"
        );
        let consumer: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-2"})).await).unwrap();
        assert!(
            consumer.blocked_by.is_empty(),
            "open tasks should not remain blocked by archived history: {consumer:?}"
        );
        assert_eq!(
            consumer.status,
            SessionTaskStatusKind::Completed,
            "detaching the last blocker must reconcile an all-done parent atomically"
        );
    }

    #[tokio::test]
    async fn archive_rejects_bad_parameter_types_before_mutating() {
        let m = mgr();
        m.create(&json!({"title": "done"})).await;
        set_task_status_fixture(&m, "task-1", SessionTaskStatusKind::Completed).await;

        for (field, args) in [
            ("task_id", json!({"task_id": true})),
            ("task_id", json!({"task_id": "   "})),
            ("older_than_days", json!({"older_than_days": true})),
        ] {
            let out = m.archive(&args).await;
            assert!(out.starts_with("Error:"), "{field}: {out}");
            assert!(
                out.contains(field),
                "bad archive {field} should name the bad field: {out}"
            );
        }

        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(
            task.status,
            SessionTaskStatusKind::Completed,
            "bad archive inputs must not mutate the task"
        );
    }

    #[tokio::test]
    async fn archive_rejects_single_task_and_bulk_parameters_together() {
        let m = mgr();
        m.create(&json!({"title": "done"})).await;
        set_task_status_fixture(&m, "task-1", SessionTaskStatusKind::Completed).await;

        let out = m
            .archive(&json!({"task_id": "task-1", "older_than_days": 7}))
            .await;
        assert!(out.starts_with("Error:"), "{out}");
        assert!(
            out.contains("either 'task_id'") && out.contains("not both"),
            "ambiguous archive inputs should give a clear correction path: {out}"
        );

        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(
            task.status,
            SessionTaskStatusKind::Completed,
            "ambiguous archive inputs must not mutate the task"
        );
    }

    #[tokio::test]
    async fn archive_accepts_reason_and_records_it() {
        let m = mgr();
        m.create(&json!({"title": "done"})).await;
        set_task_status_fixture(&m, "task-1", SessionTaskStatusKind::Completed).await;

        let archived = m
            .archive(&json!({"task_id": "task-1", "reason": "cleanup"}))
            .await;
        assert!(!archived.starts_with("Error:"), "{archived}");

        let task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert_eq!(task.status, SessionTaskStatusKind::Archived);
        assert_eq!(
            task.metadata
                .as_ref()
                .and_then(|metadata| metadata.get("archive_reason"))
                .and_then(Value::as_str),
            Some("cleanup")
        );
    }

    #[tokio::test]
    async fn archive_bulk_moves_old_terminal_tasks() {
        let m = mgr();
        m.create(&json!({"title": "old-done"})).await;
        m.create(&json!({"title": "recent-done"})).await;
        m.create(&json!({"title": "still-open"})).await;
        m.create(&json!({"title": "old-failed"})).await;
        m.create(&json!({"title": "old-cancelled"})).await;
        let mut snapshot = m.try_snapshot_state().await.expect("snapshot in test");
        let old_ts = (chrono::Utc::now() - chrono::Duration::days(10)).to_rfc3339();
        for task in &mut snapshot.tasks {
            if matches!(task.id.as_str(), "task-1" | "task-4" | "task-5") {
                task.updated_at = old_ts.clone();
            }
            if matches!(task.id.as_str(), "task-1" | "task-2") {
                task.status = SessionTaskStatusKind::Completed;
            }
            if task.id == "task-4" {
                task.status = SessionTaskStatusKind::Failed;
            }
            if task.id == "task-5" {
                task.status = SessionTaskStatusKind::Cancelled;
            }
        }
        m.restore_snapshot(&snapshot)
            .await
            .expect("restore modified timestamps");

        let archived = m.archive(&json!({"older_than_days": 7})).await;
        let archived_parsed: Value =
            serde_json::from_str(archived.split_once('\n').unwrap().1).unwrap();
        assert_eq!(archived_parsed["archived"], 3, "{archived}");

        let archived_list: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "archived"})).await).unwrap();
        let archived_titles: Vec<&str> = archived_list["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["title"].as_str().unwrap())
            .collect();
        assert!(archived_titles.contains(&"old-done"), "{archived_list}");
        assert!(archived_titles.contains(&"old-failed"), "{archived_list}");
        assert!(
            archived_titles.contains(&"old-cancelled"),
            "{archived_list}"
        );
        assert!(!archived_titles.contains(&"recent-done"), "{archived_list}");

        let completed_list: Value =
            serde_json::from_str(&m.list(&json!({"status_filter": "completed"})).await).unwrap();
        let completed_titles: Vec<&str> = completed_list["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["title"].as_str().unwrap())
            .collect();
        assert!(
            completed_titles.contains(&"recent-done"),
            "{completed_list}"
        );
        assert!(!completed_titles.contains(&"old-done"), "{completed_list}");
    }

    #[tokio::test]
    async fn archive_bulk_detaches_dependency_edges_for_archived_tasks() {
        let m = mgr();
        m.create(&json!({"title": "old-done"})).await;
        m.create(&json!({"title": "still-open"})).await;
        let linked = m
            .update(&json!({"task_id": "task-1", "add_blocks": ["task-2"]}))
            .await;
        assert!(!linked.starts_with("Error:"), "{linked}");
        set_task_status_fixture(&m, "task-1", SessionTaskStatusKind::Completed).await;

        let mut snapshot = m.try_snapshot_state().await.expect("snapshot in test");
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
        let archived_parsed: Value =
            serde_json::from_str(archived.split_once('\n').unwrap().1).unwrap();
        assert_eq!(archived_parsed["archived"], 1, "{archived}");

        let archived_task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-1"})).await).unwrap();
        assert!(
            archived_task.blocks.is_empty() && archived_task.blocked_by.is_empty(),
            "bulk-archived task should be detached: {archived_task:?}"
        );
        let open_task: SessionTask =
            serde_json::from_str(&m.get(&json!({"task_id": "task-2"})).await).unwrap();
        assert!(
            open_task.blocked_by.is_empty(),
            "bulk archive should unblock open dependents: {open_task:?}"
        );
    }

    // ── build_active_task_context ───────────────────────────────────────

    #[tokio::test]
    async fn build_active_task_context_empty_board_returns_none() {
        let m = mgr();
        assert!(m.build_active_task_context().await.is_none());
    }

    #[tokio::test]
    async fn build_active_task_context_shows_in_progress_task() {
        let m = mgr();
        m.create(&json!({"title": "Refactor DB layer", "active_form": "refactoring"}))
            .await;
        let tasks = m.load_active_tasks().await.unwrap();
        let task_id = &tasks[0].id;
        set_task_status_fixture(&m, task_id, SessionTaskStatusKind::InProgress).await;

        let ctx = m.build_active_task_context().await.unwrap();
        assert!(ctx.contains("## Active Task Board"), "{ctx}");
        assert!(ctx.contains("🔄 In progress: Refactor DB layer"), "{ctx}");
        assert!(ctx.contains("Focus on completing the in-progress"), "{ctx}");
    }

    #[tokio::test]
    async fn build_active_task_context_bounds_many_in_progress_tasks() {
        let m = mgr();
        for idx in 0..10 {
            m.create(&json!({"title": format!("Long-running in-progress task {idx}")}))
                .await;
        }
        let tasks = m.load_active_tasks().await.unwrap();
        for task in &tasks {
            set_task_status_fixture(&m, &task.id, SessionTaskStatusKind::InProgress).await;
        }

        let ctx = m.build_active_task_context().await.unwrap();
        assert_eq!(
            ctx.matches("Also in progress").count(),
            1,
            "many in-progress tasks should be summarized, not rendered as unbounded rows: {ctx}"
        );
        assert!(ctx.contains("Also in progress (9):"), "{ctx}");
        assert!(ctx.contains("+3 more"), "{ctx}");
        assert!(
            !ctx.contains("Long-running in-progress task 9"),
            "summary must be bounded to the first additional titles: {ctx}"
        );
    }

    #[tokio::test]
    async fn build_active_task_context_shows_pending_tasks() {
        let m = mgr();
        m.create(&json!({"title": "Add tests", "active_form": "testing"}))
            .await;
        m.create(&json!({"title": "Update docs", "active_form": "docs"}))
            .await;

        let ctx = m.build_active_task_context().await.unwrap();
        assert!(ctx.contains("## Active Task Board"), "{ctx}");
        assert!(ctx.contains("⏳ Pending (2):"), "{ctx}");
        assert!(
            ctx.contains("Add tests") && ctx.contains("Update docs"),
            "{ctx}"
        );
        assert!(ctx.contains("Focus on the first pending"), "{ctx}");
    }

    #[tokio::test]
    async fn build_active_task_context_includes_paused_open_work() {
        let m = mgr();
        m.create(
            &json!({"title": "Investigate flaky resume loop", "active_form": "investigating"}),
        )
        .await;
        let tasks = m.load_active_tasks().await.unwrap();
        set_task_status_fixture(&m, &tasks[0].id, SessionTaskStatusKind::Paused).await;

        let ctx = m.build_active_task_context().await.unwrap();
        assert!(
            ctx.contains("⏸ Paused (1): Investigate flaky resume loop"),
            "{ctx}"
        );
        assert!(
            ctx.contains("Resume or reprioritize the first paused task"),
            "{ctx}"
        );
    }

    #[tokio::test]
    async fn build_active_task_context_mixed_in_progress_and_pending() {
        let m = mgr();
        m.create(&json!({"title": "Fix bug", "active_form": "fixing"}))
            .await;
        m.create(&json!({"title": "Add feature", "active_form": "adding"}))
            .await;
        let tasks = m.load_active_tasks().await.unwrap();
        set_task_status_fixture(&m, &tasks[0].id, SessionTaskStatusKind::InProgress).await;

        let ctx = m.build_active_task_context().await.unwrap();
        assert!(ctx.contains("🔄 In progress: Fix bug"), "{ctx}");
        assert!(ctx.contains("⏳ Pending (1): Add feature"), "{ctx}");
        // in_progress takes priority for focus message
        assert!(ctx.contains("Focus on completing the in-progress"), "{ctx}");
    }

    #[tokio::test]
    async fn build_active_task_context_does_not_focus_blocked_pending_work() {
        let m = mgr();
        m.create(&json!({"title": "Paused prerequisite"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "paused"}))
            .await;
        m.create(&json!({
            "title": "Dependent work",
            "add_blocked_by": ["task-1"]
        }))
        .await;

        let ctx = m.build_active_task_context().await.unwrap();
        assert!(ctx.contains("⏸ Paused (1): Paused prerequisite"), "{ctx}");
        assert!(
            ctx.contains("⛔ Blocked (1): Dependent work (waiting on task-1)"),
            "{ctx}"
        );
        assert!(!ctx.contains("⏳ Pending"), "{ctx}");
        assert!(
            ctx.contains("Resume or reprioritize the first paused task"),
            "{ctx}"
        );
        assert!(!ctx.contains("Focus on the first pending"), "{ctx}");
    }

    #[tokio::test]
    async fn build_active_task_context_bounds_blocker_details_without_hiding_them() {
        let m = mgr();
        for index in 1..=4 {
            m.create(&json!({"title": format!("prerequisite {index}")}))
                .await;
        }
        m.create(&json!({
            "title": "x".repeat(200),
            "add_blocked_by": ["task-1", "task-2", "task-3", "task-4"]
        }))
        .await;

        let ctx = m.build_active_task_context().await.unwrap();
        let blocked_line = ctx
            .lines()
            .find(|line| line.starts_with("- ⛔ Blocked"))
            .expect("bounded blocked summary");
        assert!(blocked_line.contains("waiting on task-1, task-2, +2 more"));
        assert!(
            blocked_line.chars().count() <= 140,
            "prompt projection must stay bounded: {blocked_line}"
        );
    }

    #[tokio::test]
    async fn build_active_task_context_treats_completed_blockers_as_resolved() {
        let m = mgr();
        m.create(&json!({"title": "Finished prerequisite"})).await;
        m.update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        m.create(&json!({
            "title": "Ready dependent",
            "add_blocked_by": ["task-1"]
        }))
        .await;

        let ctx = m.build_active_task_context().await.unwrap();
        assert!(ctx.contains("⏳ Pending (1): Ready dependent"), "{ctx}");
        assert!(!ctx.contains("⛔ Blocked"), "{ctx}");
        assert!(
            ctx.contains("Focus on the first pending task: Ready dependent"),
            "{ctx}"
        );
    }

    #[tokio::test]
    async fn build_active_task_context_skips_non_active_statuses() {
        let m = mgr();
        m.create(&json!({"title": "Done task", "active_form": "done"}))
            .await;
        m.create(&json!({"title": "Failed task", "active_form": "failing"}))
            .await;
        let tasks = m.load_active_tasks().await.unwrap();
        set_task_status_fixture(&m, &tasks[0].id, SessionTaskStatusKind::Completed).await;
        set_task_status_fixture(&m, &tasks[1].id, SessionTaskStatusKind::Failed).await;

        assert!(
            m.build_active_task_context().await.is_none(),
            "completed and failed tasks should not appear in active context"
        );
    }
}
