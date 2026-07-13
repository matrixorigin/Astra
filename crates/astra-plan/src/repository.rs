//! Plan repository — storage abstraction over `plans` and `plan_step_runs`.
//!
//! The repository is the cloud-authoritative boundary for plan state. Everything
//! that reads or writes a plan (HTTP handlers, CLI, sync) goes through a
//! `PlanRepository` implementation.
//!
//! # Implementations
//!
//! * [`CloudPlanRepository`] — SQLx backed by the `plans` + `plan_step_runs`
//!   MatrixOne tables created in `astra-services::storage`. Source of truth.
//! * [`InMemoryPlanRepository`] — process-local fallback for tests and
//!   unconfigured runtime wiring. Mirrors the trait contract without reviving
//!   legacy filesystem persistence.
//!
//! # Invariants enforced here
//!
//! * `plans.user_id` NOT NULL — every plan is owned.
//! * For each owner, at most one session has `active_plan_id = P` at any time —
//!   enforced by [`PlanRepository::set_active_plan`] clearing that owner's other
//!   sessions pointing at the same plan in one transaction.
//! * `plans.session_id` is a routing hint (most-recent executor); canonical
//!   cross-session audit lives in `plan_step_runs.session_id`, and step-run
//!   reads/writes are always scoped by `(user_id, plan_id)`.
//! * `plan_step_runs` is append-only. Every subtask attempt creates a new row
//!   — never UPDATE, never DELETE.

use crate::state::PlanModeState;
use astra_services::task_orchestrator::TaskStatus;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, Pool, Row};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
use uuid::Uuid;

/// Typed errors for plan persistence operations.
#[derive(Debug, Clone)]
pub enum PlanLoadError {
    /// Plan ID contains illegal characters (path traversal, etc.)
    InvalidId(String),
    /// Plan does not exist in storage.
    NotFound(String),
    /// Stored plan payload is corrupted or unreadable.
    Corrupt(String),
    /// Optimistic-concurrency conflict.
    Conflict { expected: u64, actual: u64 },
    /// I/O or other unexpected error.
    Internal(String),
}

impl std::fmt::Display for PlanLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidId(msg) => write!(f, "invalid plan ID: {msg}"),
            Self::NotFound(msg) => write!(f, "plan not found: {msg}"),
            Self::Corrupt(msg) => write!(f, "plan corrupted: {msg}"),
            Self::Conflict { expected, actual } => {
                write!(f, "version conflict: expected {expected}, stored {actual}")
            }
            Self::Internal(msg) => write!(f, "plan error: {msg}"),
        }
    }
}

// ─── Step-run record ─────────────────────────────────────────────────────────

/// One row in `plan_step_runs` — an append-only attempt record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStepRun {
    pub run_id: String,
    pub plan_id: String,
    pub subtask_id: String,
    pub attempt: i32,
    pub status: TaskStatus,
    pub session_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub request_id: String,
    pub error: Option<String>,
    pub artifact_ref: Option<String>,
}

/// Input to start a new step attempt. `run_id` is assigned by the repository.
#[derive(Debug, Clone)]
pub struct NewStepRun<'a> {
    pub plan_id: &'a str,
    pub subtask_id: &'a str,
    pub attempt: i32,
    pub status: TaskStatus,
    pub session_id: &'a str,
    pub request_id: &'a str,
}

/// Summary info for a saved plan.
#[derive(Debug, Clone)]
pub struct SavedPlanInfo {
    pub name: String,
    pub session_id: Option<String>,
    pub goal: String,
    pub version: u64,
    pub progress_pct: u32,
    pub subtask_count: usize,
    pub status: String,
}

// ─── Trait ───────────────────────────────────────────────────────────────────

/// Repository over plan state and step-attempt history.
///
/// All operations are async and may fail with [`PlanLoadError`]. Implementations
/// must be `Send + Sync` so they can be stored in `AppState` and shared across
/// tokio tasks.
#[async_trait]
pub trait PlanRepository: Send + Sync {
    /// Persist a plan, inserting or updating by `(user_id, plan_id)`.
    ///
    /// `expected_version` enforces optimistic concurrency: pass the version
    /// observed at load time; the write fails with [`PlanLoadError::Conflict`]
    /// if the stored version has moved. Pass `None` for a first insert.
    async fn save(
        &self,
        user_id: &str,
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: Option<u64>,
    ) -> Result<(), PlanLoadError>;

    /// Load a plan by `(user_id, plan_id)`. Returns [`PlanLoadError::NotFound`] if missing.
    async fn load(&self, user_id: &str, plan_id: &str) -> Result<PlanModeState, PlanLoadError>;

    /// List plans for a user, optionally filtered by session or phase.
    async fn list_for_user(
        &self,
        user_id: &str,
        filter: PlanListFilter<'_>,
    ) -> Result<Vec<SavedPlanInfo>, PlanLoadError>;

    /// Delete a plan (and, for cloud, cascade its `plan_step_runs`).
    async fn delete(&self, user_id: &str, plan_id: &str) -> Result<(), PlanLoadError>;

    /// Mark `plan_id` as the active plan for an owned session, atomically
    /// clearing any other session currently pointing at the same plan. Passing
    /// `plan_id = None` clears the owned session's active plan.
    ///
    /// No-op (and returns Ok) if the session does not exist yet, so CLI can
    /// invoke it before a session row is created.
    async fn set_active_plan(
        &self,
        user_id: &str,
        session_id: &str,
        plan_id: Option<&str>,
    ) -> Result<(), PlanLoadError>;

    /// Return the currently active `plan_id` for an owned session, if any.
    async fn active_plan_for_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<String>, PlanLoadError>;

    /// Append a new step-run row. Returns the assigned `run_id`.
    async fn record_step_run(
        &self,
        _user_id: &str,
        input: NewStepRun<'_>,
    ) -> Result<String, PlanLoadError>;

    /// Record an attempt that already reached a terminal state in one write.
    ///
    /// The CLI executor's happy path always ends a subtask in a terminal
    /// status (completed/failed/cancelled) — the attempt never observes the
    /// `in_progress` state. Rather than forcing the executor to post two
    /// HTTP calls (start + finish), this shortcut inserts a finalized row
    /// with `started_at = finished_at = NOW()` in a single statement.
    ///
    /// Returns the assigned `run_id`. Callers must pass a terminal
    /// [`TaskStatus`] (Completed/Failed/Cancelled); `Pending`/`InProgress`
    /// is rejected at the HTTP boundary (this method itself doesn't gate).
    async fn record_completed_step_run(
        &self,
        user_id: &str,
        input: NewStepRun<'_>,
        error: Option<&str>,
        artifact_ref: Option<&str>,
    ) -> Result<String, PlanLoadError>;

    /// Atomically persist a plan-step transition to `in_progress` and append
    /// its open attempt. Implementations that own durable storage must update
    /// the plan version and the attempt row in one transaction: a taskboard
    /// projection must never observe a started attempt with a stale plan step
    /// (or the reverse).
    async fn save_existing_and_start_step_run(
        &self,
        _user_id: &str,
        _plan_id: &str,
        _state: &mut PlanModeState,
        _expected_version: u64,
        _input: NewStepRun<'_>,
    ) -> Result<String, PlanLoadError> {
        Err(PlanLoadError::Internal(
            "plan repository does not support atomic step-start persistence".to_string(),
        ))
    }

    /// Atomically persist a terminal plan-step transition and its completed
    /// attempt. See [`Self::save_existing_and_start_step_run`] for why this
    /// cannot be modelled as two best-effort writes.
    async fn save_existing_and_record_completed_step_run(
        &self,
        _user_id: &str,
        _plan_id: &str,
        _state: &mut PlanModeState,
        _expected_version: u64,
        _input: NewStepRun<'_>,
        _error: Option<&str>,
        _artifact_ref: Option<&str>,
    ) -> Result<String, PlanLoadError> {
        Err(PlanLoadError::Internal(
            "plan repository does not support atomic completed-step persistence".to_string(),
        ))
    }

    /// Atomically persist a terminal plan-step transition and finalize an
    /// existing open attempt.
    async fn save_existing_and_finalize_step_run(
        &self,
        _user_id: &str,
        _plan_id: &str,
        _state: &mut PlanModeState,
        _expected_version: u64,
        _run_id: &str,
        _status: TaskStatus,
        _error: Option<&str>,
        _artifact_ref: Option<&str>,
    ) -> Result<(), PlanLoadError> {
        Err(PlanLoadError::Internal(
            "plan repository does not support atomic step-finalization persistence".to_string(),
        ))
    }

    /// Finalize an existing step-run with its outcome. Status/finished_at/error
    /// are the mutable fields; everything else is immutable.
    ///
    /// `plan_id` pins the finalize to a specific plan — passing a `run_id`
    /// from a different plan returns `NotFound`. Without this pin, a caller
    /// authorized for plan A could finalize plan B's run by knowing its
    /// `run_id`. Handlers owner-check on `plan_id`, so requiring `plan_id`
    /// here closes the loop.
    async fn finalize_step_run(
        &self,
        user_id: &str,
        plan_id: &str,
        run_id: &str,
        status: TaskStatus,
        error: Option<&str>,
        artifact_ref: Option<&str>,
    ) -> Result<(), PlanLoadError>;

    /// Fetch a single step-run by `(user_id, run_id, plan_id)`. Returns `NotFound` if
    /// the row doesn't exist or belongs to a different plan.
    async fn get_step_run(
        &self,
        user_id: &str,
        plan_id: &str,
        run_id: &str,
    ) -> Result<PlanStepRun, PlanLoadError>;

    /// List step runs for a plan (optionally one subtask), newest first.
    async fn list_step_runs(
        &self,
        user_id: &str,
        plan_id: &str,
        subtask_id: Option<&str>,
        limit: i32,
    ) -> Result<Vec<PlanStepRun>, PlanLoadError>;

    /// Finalize every open (`finished_at IS NULL`) step-run for the given
    /// subtasks in one statement, marking them as `cancelled`.
    ///
    /// Called by `rewind` and `redo_step` handlers so resetting a subtask's
    /// in-process state also closes its open audit row — otherwise the run
    /// sits `in_progress` forever and the attempt counter in a later redo
    /// sees stale max-attempt values. Returns the number of rows closed.
    ///
    /// Already-finalized rows are never touched (the UPDATE filters on
    /// `finished_at IS NULL`), keeping the table append-only-with-
    /// terminal-edit semantics.
    async fn abort_open_step_runs(
        &self,
        user_id: &str,
        plan_id: &str,
        subtask_ids: &[String],
    ) -> Result<u64, PlanLoadError>;

    /// Persist an existing plan and cancel open step-runs for the supplied
    /// subtasks as one repository mutation. Cloud implementations must commit
    /// both changes in the same database transaction; in-memory/test
    /// implementations must protect both changes under the same write lock.
    async fn save_existing_and_abort_open_step_runs(
        &self,
        user_id: &str,
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: u64,
        subtask_ids: &[String],
    ) -> Result<u64, PlanLoadError>;
}

/// Filter for [`PlanRepository::list_for_user`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PlanListFilter<'a> {
    pub session_id: Option<&'a str>,
    pub phase: Option<&'a str>,
    pub limit: Option<i32>,
}

impl PlanLoadError {
    /// Optimistic-concurrency conflict — caller observed `expected_version`
    /// but the stored version is different.
    pub fn conflict(expected: u64, actual: u64) -> Self {
        Self::Conflict { expected, actual }
    }
}

// ─── Cloud (SQLx) implementation ─────────────────────────────────────────────

/// MatrixOne/MySQL-backed plan repository — the default in the runtime.
#[derive(Debug, Clone)]
pub struct CloudPlanRepository {
    pool: Pool<MySql>,
}

impl CloudPlanRepository {
    pub fn new(pool: Pool<MySql>) -> Self {
        Self { pool }
    }

    /// Update an existing plan under its optimistic version guard inside an
    /// already-open transaction. Step-run mutations call this helper so the
    /// plan projection and its attempt evidence commit or roll back together.
    async fn save_existing_in_transaction(
        tx: &mut sqlx::Transaction<'_, MySql>,
        user_id: &str,
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: u64,
    ) -> Result<(), PlanLoadError> {
        validate_plan_id(plan_id)?;
        ensure_state_owner(user_id, state)?;
        let next_version = expected_version
            .checked_add(1)
            .ok_or_else(|| PlanLoadError::Internal("plan version overflow".to_string()))?;
        let mut persisted_state = state.clone();
        persisted_state.version = next_version;
        let plan_json = serde_json::to_string(&persisted_state)
            .map_err(|error| PlanLoadError::Internal(error.to_string()))?;
        let update = sqlx::query(
            "UPDATE plans \
             SET goal = ?, phase = ?, version = ?, plan_json = ?, plan_md = ?, \
                 progress_pct = ?, subtask_count = ?, updated_at = NOW(6) \
             WHERE user_id = ? AND plan_id = ? AND version = ?",
        )
        .bind(&persisted_state.goal)
        .bind(persisted_state.infer_phase().as_str())
        .bind(next_version as i64)
        .bind(plan_json)
        .bind(persisted_state.plan_md.as_deref())
        .bind(persisted_state.plan.progress_pct() as i32)
        .bind(persisted_state.plan.subtasks.len() as i32)
        .bind(user_id)
        .bind(plan_id)
        .bind(expected_version as i64)
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx)?;
        if update.rows_affected() == 0 {
            let latest: Option<(i64,)> =
                sqlx::query_as("SELECT version FROM plans WHERE user_id = ? AND plan_id = ?")
                    .bind(user_id)
                    .bind(plan_id)
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(map_sqlx)?;
            return Err(PlanLoadError::conflict(
                expected_version,
                latest.map(|(version,)| version as u64).unwrap_or(0),
            ));
        }
        state.version = next_version;
        Ok(())
    }
}

fn map_sqlx(err: sqlx::Error) -> PlanLoadError {
    PlanLoadError::Internal(format!("sql error: {err}"))
}

fn parse_persisted_step_run_status(status: &str) -> Result<TaskStatus, PlanLoadError> {
    TaskStatus::parse_status(status)
        .ok_or_else(|| PlanLoadError::Corrupt(format!("unknown step_run status: {status}")))
}

fn validate_plan_id(plan_id: &str) -> Result<(), PlanLoadError> {
    if plan_id.is_empty() {
        return Err(PlanLoadError::InvalidId("plan ID must not be empty".into()));
    }
    if !plan_id
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(PlanLoadError::InvalidId(format!(
            "'{plan_id}': only alphanumeric, dash, and underscore allowed"
        )));
    }
    Ok(())
}

fn ensure_state_owner(user_id: &str, state: &PlanModeState) -> Result<(), PlanLoadError> {
    match state.created_by.as_deref() {
        Some(owner) if owner == user_id => Ok(()),
        Some(owner) => Err(PlanLoadError::Internal(format!(
            "plan owner mismatch: state.created_by={owner}, row user_id={user_id}"
        ))),
        None => Err(PlanLoadError::Internal(format!(
            "plan has no owner (created_by=None), expected {user_id}"
        ))),
    }
}

/// Translate the MySQL duplicate-key error (1062) raised by the unique
/// `(user_id, plan_id, subtask_id, attempt)` index on `plan_step_runs` into
/// [`PlanLoadError::Conflict`]. This happens when two concurrent redos
/// compute the same `next_attempt` and race to INSERT — exactly one must win.
fn map_step_run_insert_error(
    err: sqlx::Error,
    _plan_id: &str,
    _subtask_id: &str,
    attempt: i32,
) -> PlanLoadError {
    if let sqlx::Error::Database(db_err) = &err
        && let Some(my) = db_err.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
        && my.number() == 1062
    {
        return PlanLoadError::Conflict {
            expected: attempt as u64,
            actual: attempt as u64,
        };
    }
    map_sqlx(err)
}

#[async_trait]
impl PlanRepository for CloudPlanRepository {
    async fn save(
        &self,
        user_id: &str,
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: Option<u64>,
    ) -> Result<(), PlanLoadError> {
        validate_plan_id(plan_id)?;
        ensure_state_owner(user_id, state)?;
        let phase = state.infer_phase().as_str();
        let progress = state.plan.progress_pct() as i32;
        let goal = state.goal.clone();

        // Two concurrent writers at the same `expected_version` must never
        // both win. The original implementation did SELECT...FOR UPDATE then
        // an UPSERT on the pool — which grabbed a *different* connection and
        // released the row lock before the write, so under load 30+/32
        // contenders could all pass the version check and all UPSERT. The
        // fix is to do the version check + write in a single statement whose
        // atomicity MySQL guarantees: a conditional UPDATE gated on the
        // current version, plus an INSERT-first fallback for brand-new rows.
        //
        // For a new plan (no row yet) we INSERT; the PK uniqueness guarantees
        // exactly one INSERT wins. For an existing plan we UPDATE ... WHERE
        // version = expected_stored — only the writer whose expected matches
        // the stored value flips `rows_affected() == 1`; all others observe
        // 0 and report a conflict.

        // First, read the current row (without FOR UPDATE — we rely on the
        // conditional UPDATE below for the real guard).
        let current: Option<(i64,)> =
            sqlx::query_as("SELECT version FROM plans WHERE user_id = ? AND plan_id = ?")
                .bind(user_id)
                .bind(plan_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(map_sqlx)?;

        match (current, expected_version) {
            // Caller thinks row exists but doesn't → reject, even if expected=0
            // is passed (we reserve `None` for "first write only").
            (None, Some(expected)) if expected != 0 => {
                return Err(PlanLoadError::conflict(expected, 0));
            }
            // A None expected_version means "I am creating this row". If a row
            // already exists, the caller didn't observe it — accepting the
            // write would blindly overwrite a concurrent editor's progress.
            // The supported re-link path is: load() → save(version). Reject
            // any save(..., None) that lands on an existing plan_id.
            (Some((stored,)), None) => {
                return Err(PlanLoadError::conflict(0, stored as u64));
            }
            // New row: try INSERT; if another writer inserted the same id
            // concurrently our INSERT will hit a duplicate-key error and we
            // translate that to a conflict.
            (None, _) => {
                state.version = 1;
                let plan_json = serde_json::to_string(state)
                    .map_err(|e| PlanLoadError::Internal(e.to_string()))?;
                let subtask_count = state.plan.subtasks.len() as i32;
                let res = sqlx::query(
                    "INSERT INTO plans \
                         (plan_id, user_id, session_id, goal, phase, version, plan_json, plan_md, \
                          progress_pct, subtask_count, created_by, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
                )
                .bind(plan_id)
                .bind(user_id)
                .bind(state.session_hint.as_deref())
                .bind(&goal)
                .bind(phase)
                .bind(&plan_json)
                .bind(state.plan_md.as_deref())
                .bind(progress)
                .bind(subtask_count)
                .bind(user_id)
                .execute(&self.pool)
                .await;
                match res {
                    Ok(_) => Ok(()),
                    Err(sqlx::Error::Database(db_err))
                        if db_err
                            .code()
                            .map(|c| c == "23000" || c.starts_with("1062"))
                            .unwrap_or(false) =>
                    {
                        Err(PlanLoadError::conflict(expected_version.unwrap_or(0), 1))
                    }
                    Err(e) => Err(map_sqlx(e)),
                }
            }
            // Existing row: conditional UPDATE on the expected version.
            (Some((stored,)), _) => {
                // If the caller supplied an expected_version that doesn't
                // match the stored one, we can reject without touching the
                // DB (the UPDATE below would reject anyway, but this saves
                // a round-trip and yields the correct stored-vs-expected
                // error message).
                if let Some(expected) = expected_version
                    && (stored as u64) != expected
                {
                    return Err(PlanLoadError::conflict(expected, stored as u64));
                }
                let next_version = (stored as u64) + 1;
                state.version = next_version;
                let plan_json = serde_json::to_string(state)
                    .map_err(|e| PlanLoadError::Internal(e.to_string()))?;
                let subtask_count = state.plan.subtasks.len() as i32;
                // Conditional UPDATE: only succeeds when the stored version
                // is still `stored`. Concurrent writer that already bumped
                // the row to `stored + 1` causes our WHERE to miss, and
                // `rows_affected() == 0` → conflict. Session_id / user_id are
                // intentionally NOT in SET so routine saves don't clobber
                // hints set via set_active_plan.
                let res = sqlx::query(
                    "UPDATE plans \
                     SET goal = ?, phase = ?, version = ?, plan_json = ?, plan_md = ?, \
                         progress_pct = ?, subtask_count = ?, updated_at = NOW(6) \
                     WHERE user_id = ? AND plan_id = ? AND version = ?",
                )
                .bind(&goal)
                .bind(phase)
                .bind(next_version as i64)
                .bind(&plan_json)
                .bind(state.plan_md.as_deref())
                .bind(progress)
                .bind(subtask_count)
                .bind(user_id)
                .bind(plan_id)
                .bind(stored)
                .execute(&self.pool)
                .await
                .map_err(map_sqlx)?;

                if res.rows_affected() == 0 {
                    // Another writer moved the version under us. Read back
                    // the actual stored version so the error carries the
                    // real conflict pair, not the stale `stored` we read.
                    let latest: Option<(i64,)> = sqlx::query_as(
                        "SELECT version FROM plans WHERE user_id = ? AND plan_id = ?",
                    )
                    .bind(user_id)
                    .bind(plan_id)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(map_sqlx)?;
                    let actual = latest.map(|(v,)| v as u64).unwrap_or(0);
                    return Err(PlanLoadError::conflict(
                        expected_version.unwrap_or(stored as u64),
                        actual,
                    ));
                }
                Ok(())
            }
        }
    }

    async fn load(&self, user_id: &str, plan_id: &str) -> Result<PlanModeState, PlanLoadError> {
        validate_plan_id(plan_id)?;
        let row = sqlx::query(
            "SELECT plan_json, plan_md, session_id, version FROM plans WHERE user_id = ? AND plan_id = ?",
        )
        .bind(user_id)
        .bind(plan_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;
        let Some(row) = row else {
            return Err(PlanLoadError::NotFound(plan_id.to_string()));
        };
        let plan_json: String = row
            .try_get("plan_json")
            .map_err(|e| PlanLoadError::Corrupt(format!("read plan_json: {e}")))?;
        let session_hint: Option<String> = row
            .try_get("session_id")
            .map_err(|e| PlanLoadError::Corrupt(format!("read session_id: {e}")))?;
        let plan_md: Option<String> = row
            .try_get("plan_md")
            .map_err(|e| PlanLoadError::Corrupt(format!("read plan_md: {e}")))?;
        let version_col: i64 = row
            .try_get("version")
            .map_err(|e| PlanLoadError::Corrupt(format!("read version: {e}")))?;
        let mut state = serde_json::from_str::<PlanModeState>(&plan_json)
            .map_err(|e| PlanLoadError::Corrupt(format!("parse plan state: {e}")))?;
        if state.created_by.as_deref() != Some(user_id) {
            return Err(PlanLoadError::Corrupt(format!(
                "plan owner mismatch: state.created_by={:?}, row user_id={user_id}",
                state.created_by
            )));
        }
        state.session_hint = session_hint;
        state.plan_md = plan_md.or(state.plan_md);
        // `plans.version` is the authoritative optimistic-concurrency value.
        // Old rows written before the save() ordering fix may have a stale
        // version inside plan_json; trust the column.
        state.version = version_col as u64;
        Ok(state)
    }

    async fn list_for_user(
        &self,
        user_id: &str,
        filter: PlanListFilter<'_>,
    ) -> Result<Vec<SavedPlanInfo>, PlanLoadError> {
        let limit = filter.limit.unwrap_or(100).clamp(1, 500);

        // Reads only the denormalized summary columns — `plan_json` stays on
        // disk. `subtask_count` is maintained by `save()` so this avoids
        // parsing O(N) plan blobs just to render the list page.
        let mut sql = String::from(
            "SELECT plan_id, session_id, goal, phase, version, progress_pct, subtask_count \
             FROM plans WHERE user_id = ?",
        );
        if filter.session_id.is_some() {
            sql.push_str(" AND session_id = ?");
        }
        if filter.phase.is_some() {
            sql.push_str(" AND phase = ?");
        }
        sql.push_str(" ORDER BY updated_at DESC LIMIT ?");

        let mut q = sqlx::query(&sql).bind(user_id);
        if let Some(sid) = filter.session_id {
            q = q.bind(sid);
        }
        if let Some(p) = filter.phase {
            q = q.bind(p);
        }
        q = q.bind(limit);

        let rows = q.fetch_all(&self.pool).await.map_err(map_sqlx)?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let plan_id: String = row.try_get("plan_id").map_err(map_sqlx)?;
            let session_id: Option<String> = row.try_get("session_id").map_err(map_sqlx)?;
            let goal: String = row.try_get("goal").map_err(map_sqlx)?;
            let phase: String = row.try_get("phase").map_err(map_sqlx)?;
            let version: i64 = row.try_get("version").map_err(map_sqlx)?;
            let progress_pct: i32 = row.try_get("progress_pct").map_err(map_sqlx)?;
            let subtask_count: i32 = row.try_get("subtask_count").map_err(map_sqlx)?;
            out.push(SavedPlanInfo {
                name: plan_id,
                session_id,
                goal,
                version: version as u64,
                progress_pct: progress_pct as u32,
                subtask_count: subtask_count as usize,
                status: phase,
            });
        }
        Ok(out)
    }

    async fn delete(&self, user_id: &str, plan_id: &str) -> Result<(), PlanLoadError> {
        let mut tx = self.pool.begin().await.map_err(map_sqlx)?;

        let result = sqlx::query("DELETE FROM plans WHERE user_id = ? AND plan_id = ?")
            .bind(user_id)
            .bind(plan_id)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;

        if result.rows_affected() == 0 {
            tx.rollback().await.map_err(map_sqlx)?;
            return Err(PlanLoadError::NotFound(plan_id.to_string()));
        }

        // plan_step_runs has no FK (HTAP keeps writes fast) — cascade here.
        sqlx::query("DELETE FROM plan_step_runs WHERE user_id = ? AND plan_id = ?")
            .bind(user_id)
            .bind(plan_id)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;

        // Any session still pointing at this plan must be cleared so we don't
        // strand a dangling foreign reference.
        sqlx::query(
            "UPDATE agent_sessions SET active_plan_id = NULL \
             WHERE user_id = ? AND active_plan_id = ?",
        )
        .bind(user_id)
        .bind(plan_id)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        tx.commit().await.map_err(map_sqlx)?;
        Ok(())
    }

    async fn set_active_plan(
        &self,
        user_id: &str,
        session_id: &str,
        plan_id: Option<&str>,
    ) -> Result<(), PlanLoadError> {
        let mut tx = self.pool.begin().await.map_err(map_sqlx)?;

        if let Some(pid) = plan_id {
            validate_plan_id(pid)?;
            // Clear any OTHER session currently pointing at this plan.
            sqlx::query(
                "UPDATE agent_sessions SET active_plan_id = NULL \
                 WHERE user_id = ? AND active_plan_id = ? AND session_id <> ?",
            )
            .bind(user_id)
            .bind(pid)
            .bind(session_id)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;

            // Refresh the plans.session_id routing hint.
            let updated = sqlx::query(
                "UPDATE plans SET session_id = ?, updated_at = NOW(6) \
                 WHERE user_id = ? AND plan_id = ?",
            )
            .bind(session_id)
            .bind(user_id)
            .bind(pid)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
            if updated.rows_affected() == 0 {
                tx.rollback().await.map_err(map_sqlx)?;
                return Err(PlanLoadError::NotFound(pid.to_string()));
            }
        }

        // Session row may not exist yet — no-op in that case.
        sqlx::query(
            "UPDATE agent_sessions SET active_plan_id = ? WHERE user_id = ? AND session_id = ?",
        )
        .bind(plan_id)
        .bind(user_id)
        .bind(session_id)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        tx.commit().await.map_err(map_sqlx)?;
        Ok(())
    }

    async fn active_plan_for_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<String>, PlanLoadError> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT active_plan_id FROM agent_sessions WHERE user_id = ? AND session_id = ?",
        )
        .bind(user_id)
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(row.and_then(|(id,)| id))
    }

    async fn record_step_run(
        &self,
        user_id: &str,
        input: NewStepRun<'_>,
    ) -> Result<String, PlanLoadError> {
        let run_id = Uuid::new_v4().to_string();
        let res = sqlx::query(
            "INSERT INTO plan_step_runs \
                 (user_id, run_id, plan_id, subtask_id, attempt, status, session_id, \
                  started_at, request_id) \
             SELECT ?, ?, ?, ?, ?, ?, ?, NOW(6), ? \
             FROM plans WHERE user_id = ? AND plan_id = ?",
        )
        .bind(user_id)
        .bind(&run_id)
        .bind(input.plan_id)
        .bind(input.subtask_id)
        .bind(input.attempt)
        .bind(input.status.as_str())
        .bind(input.session_id)
        .bind(input.request_id)
        .bind(user_id)
        .bind(input.plan_id)
        .execute(&self.pool)
        .await;
        match res {
            Ok(result) if result.rows_affected() == 0 => {
                Err(PlanLoadError::NotFound(input.plan_id.to_string()))
            }
            Ok(_) => Ok(run_id),
            Err(e) => Err(map_step_run_insert_error(
                e,
                input.plan_id,
                input.subtask_id,
                input.attempt,
            )),
        }
    }

    async fn record_completed_step_run(
        &self,
        user_id: &str,
        input: NewStepRun<'_>,
        error: Option<&str>,
        artifact_ref: Option<&str>,
    ) -> Result<String, PlanLoadError> {
        let run_id = Uuid::new_v4().to_string();
        let res = sqlx::query(
            "INSERT INTO plan_step_runs \
                 (user_id, run_id, plan_id, subtask_id, attempt, status, session_id, \
                  started_at, finished_at, request_id, error, artifact_ref) \
             SELECT ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6), ?, ?, ? \
             FROM plans WHERE user_id = ? AND plan_id = ?",
        )
        .bind(user_id)
        .bind(&run_id)
        .bind(input.plan_id)
        .bind(input.subtask_id)
        .bind(input.attempt)
        .bind(input.status.as_str())
        .bind(input.session_id)
        .bind(input.request_id)
        .bind(error)
        .bind(artifact_ref)
        .bind(user_id)
        .bind(input.plan_id)
        .execute(&self.pool)
        .await;
        match res {
            Ok(result) if result.rows_affected() == 0 => {
                Err(PlanLoadError::NotFound(input.plan_id.to_string()))
            }
            Ok(_) => Ok(run_id),
            Err(e) => Err(map_step_run_insert_error(
                e,
                input.plan_id,
                input.subtask_id,
                input.attempt,
            )),
        }
    }

    async fn save_existing_and_start_step_run(
        &self,
        user_id: &str,
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: u64,
        input: NewStepRun<'_>,
    ) -> Result<String, PlanLoadError> {
        let run_id = Uuid::new_v4().to_string();
        let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
        Self::save_existing_in_transaction(&mut tx, user_id, plan_id, state, expected_version)
            .await?;
        let insert = sqlx::query(
            "INSERT INTO plan_step_runs \
                 (user_id, run_id, plan_id, subtask_id, attempt, status, session_id, \
                  started_at, request_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?, NOW(6), ?)",
        )
        .bind(user_id)
        .bind(&run_id)
        .bind(plan_id)
        .bind(input.subtask_id)
        .bind(input.attempt)
        .bind(input.status.as_str())
        .bind(input.session_id)
        .bind(input.request_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            map_step_run_insert_error(error, plan_id, input.subtask_id, input.attempt)
        })?;
        debug_assert_eq!(insert.rows_affected(), 1);
        tx.commit().await.map_err(map_sqlx)?;
        Ok(run_id)
    }

    async fn save_existing_and_record_completed_step_run(
        &self,
        user_id: &str,
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: u64,
        input: NewStepRun<'_>,
        error: Option<&str>,
        artifact_ref: Option<&str>,
    ) -> Result<String, PlanLoadError> {
        let run_id = Uuid::new_v4().to_string();
        let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
        Self::save_existing_in_transaction(&mut tx, user_id, plan_id, state, expected_version)
            .await?;
        let insert = sqlx::query(
            "INSERT INTO plan_step_runs \
                 (user_id, run_id, plan_id, subtask_id, attempt, status, session_id, \
                  started_at, finished_at, request_id, error, artifact_ref) \
             VALUES (?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6), ?, ?, ?)",
        )
        .bind(user_id)
        .bind(&run_id)
        .bind(plan_id)
        .bind(input.subtask_id)
        .bind(input.attempt)
        .bind(input.status.as_str())
        .bind(input.session_id)
        .bind(input.request_id)
        .bind(error)
        .bind(artifact_ref)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            map_step_run_insert_error(error, plan_id, input.subtask_id, input.attempt)
        })?;
        debug_assert_eq!(insert.rows_affected(), 1);
        tx.commit().await.map_err(map_sqlx)?;
        Ok(run_id)
    }

    async fn save_existing_and_finalize_step_run(
        &self,
        user_id: &str,
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: u64,
        run_id: &str,
        status: TaskStatus,
        error: Option<&str>,
        artifact_ref: Option<&str>,
    ) -> Result<(), PlanLoadError> {
        let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
        Self::save_existing_in_transaction(&mut tx, user_id, plan_id, state, expected_version)
            .await?;
        let result = sqlx::query(
            "UPDATE plan_step_runs \
             SET status = ?, finished_at = NOW(6), error = ?, artifact_ref = ? \
             WHERE user_id = ? AND run_id = ? AND plan_id = ? AND finished_at IS NULL",
        )
        .bind(status.as_str())
        .bind(error)
        .bind(artifact_ref)
        .bind(user_id)
        .bind(run_id)
        .bind(plan_id)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;
        if result.rows_affected() == 0 {
            return Err(PlanLoadError::NotFound(format!(
                "step_run {run_id} not found in plan {plan_id} or already finalized"
            )));
        }
        tx.commit().await.map_err(map_sqlx)?;
        Ok(())
    }

    async fn finalize_step_run(
        &self,
        user_id: &str,
        plan_id: &str,
        run_id: &str,
        status: TaskStatus,
        error: Option<&str>,
        artifact_ref: Option<&str>,
    ) -> Result<(), PlanLoadError> {
        let result = sqlx::query(
            "UPDATE plan_step_runs \
             SET status = ?, finished_at = NOW(6), error = ?, artifact_ref = ? \
             WHERE user_id = ? AND run_id = ? AND plan_id = ? AND finished_at IS NULL",
        )
        .bind(status.as_str())
        .bind(error)
        .bind(artifact_ref)
        .bind(user_id)
        .bind(run_id)
        .bind(plan_id)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;

        if result.rows_affected() == 0 {
            // Unknown id, wrong plan, or already finalized — all surface as
            // NotFound so a cross-plan probe can't tell which.
            return Err(PlanLoadError::NotFound(format!(
                "step_run {run_id} not found in plan {plan_id} or already finalized"
            )));
        }
        Ok(())
    }

    async fn get_step_run(
        &self,
        user_id: &str,
        plan_id: &str,
        run_id: &str,
    ) -> Result<PlanStepRun, PlanLoadError> {
        let row = sqlx::query(
            "SELECT run_id, plan_id, subtask_id, attempt, status, session_id, \
                    started_at, finished_at, request_id, error, artifact_ref \
             FROM plan_step_runs \
             WHERE user_id = ? AND run_id = ? AND plan_id = ?",
        )
        .bind(user_id)
        .bind(run_id)
        .bind(plan_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;
        let Some(r) = row else {
            return Err(PlanLoadError::NotFound(format!(
                "step_run {run_id} not found in plan {plan_id}"
            )));
        };
        Ok(PlanStepRun {
            run_id: r.try_get("run_id").map_err(map_sqlx)?,
            plan_id: r.try_get("plan_id").map_err(map_sqlx)?,
            subtask_id: r.try_get("subtask_id").map_err(map_sqlx)?,
            attempt: r.try_get("attempt").map_err(map_sqlx)?,
            status: parse_persisted_step_run_status(
                &r.try_get::<String, _>("status").map_err(map_sqlx)?,
            )?,
            session_id: r.try_get("session_id").map_err(map_sqlx)?,
            started_at: r.try_get("started_at").map_err(map_sqlx)?,
            finished_at: r.try_get("finished_at").map_err(map_sqlx)?,
            request_id: r.try_get("request_id").map_err(map_sqlx)?,
            error: r.try_get("error").map_err(map_sqlx)?,
            artifact_ref: r.try_get("artifact_ref").map_err(map_sqlx)?,
        })
    }

    async fn list_step_runs(
        &self,
        user_id: &str,
        plan_id: &str,
        subtask_id: Option<&str>,
        limit: i32,
    ) -> Result<Vec<PlanStepRun>, PlanLoadError> {
        let limit = limit.clamp(1, 1000);

        // Stable order: newest started_at first, `run_id` ASC as the
        // tiebreaker. Without this, two runs inserted in the same microsecond
        // (same NOW(6) under load) come back in an arbitrary order, so a
        // client paging by limit could see duplicates or skips.
        let rows = if let Some(sid) = subtask_id {
            sqlx::query(
                "SELECT run_id, plan_id, subtask_id, attempt, status, session_id, \
                        started_at, finished_at, request_id, error, artifact_ref \
                 FROM plan_step_runs \
                 WHERE user_id = ? AND plan_id = ? AND subtask_id = ? \
                 ORDER BY started_at DESC, run_id ASC LIMIT ?",
            )
            .bind(user_id)
            .bind(plan_id)
            .bind(sid)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query(
                "SELECT run_id, plan_id, subtask_id, attempt, status, session_id, \
                        started_at, finished_at, request_id, error, artifact_ref \
                 FROM plan_step_runs \
                 WHERE user_id = ? AND plan_id = ? \
                 ORDER BY started_at DESC, run_id ASC LIMIT ?",
            )
            .bind(user_id)
            .bind(plan_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(map_sqlx)?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(PlanStepRun {
                run_id: r.try_get("run_id").map_err(map_sqlx)?,
                plan_id: r.try_get("plan_id").map_err(map_sqlx)?,
                subtask_id: r.try_get("subtask_id").map_err(map_sqlx)?,
                attempt: r.try_get("attempt").map_err(map_sqlx)?,
                status: parse_persisted_step_run_status(
                    &r.try_get::<String, _>("status").map_err(map_sqlx)?,
                )?,
                session_id: r.try_get("session_id").map_err(map_sqlx)?,
                started_at: r.try_get("started_at").map_err(map_sqlx)?,
                finished_at: r.try_get("finished_at").map_err(map_sqlx)?,
                request_id: r.try_get("request_id").map_err(map_sqlx)?,
                error: r.try_get("error").map_err(map_sqlx)?,
                artifact_ref: r.try_get("artifact_ref").map_err(map_sqlx)?,
            });
        }
        Ok(out)
    }

    async fn abort_open_step_runs(
        &self,
        user_id: &str,
        plan_id: &str,
        subtask_ids: &[String],
    ) -> Result<u64, PlanLoadError> {
        validate_plan_id(plan_id)?;
        if subtask_ids.is_empty() {
            return Ok(0);
        }

        // Build an `IN (?, ?, ...)` clause sized to the input. All values are
        // bound via `.bind()` — only the placeholder count is string-
        // formatted, so no injection surface.
        let placeholders = std::iter::repeat_n("?", subtask_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "UPDATE plan_step_runs \
             SET status = ?, finished_at = NOW(6) \
             WHERE user_id = ? AND plan_id = ? AND finished_at IS NULL \
               AND subtask_id IN ({placeholders})"
        );
        let mut q = sqlx::query(&sql)
            .bind(TaskStatus::Cancelled.as_str())
            .bind(user_id)
            .bind(plan_id);
        for sid in subtask_ids {
            q = q.bind(sid);
        }
        let result = q.execute(&self.pool).await.map_err(map_sqlx)?;
        Ok(result.rows_affected())
    }

    async fn save_existing_and_abort_open_step_runs(
        &self,
        user_id: &str,
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: u64,
        subtask_ids: &[String],
    ) -> Result<u64, PlanLoadError> {
        ensure_state_owner(user_id, state)?;
        let phase = state.infer_phase().as_str();
        let progress = state.plan.progress_pct() as i32;
        let goal = state.goal.clone();
        let next_version = expected_version
            .checked_add(1)
            .ok_or_else(|| PlanLoadError::Internal("plan version overflow".into()))?;
        let mut persisted_state = state.clone();
        persisted_state.version = next_version;
        let plan_json = serde_json::to_string(&persisted_state)
            .map_err(|e| PlanLoadError::Internal(e.to_string()))?;
        let subtask_count = persisted_state.plan.subtasks.len() as i32;

        let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
        let update = sqlx::query(
            "UPDATE plans \
             SET goal = ?, phase = ?, version = ?, plan_json = ?, plan_md = ?, \
                 progress_pct = ?, subtask_count = ?, updated_at = NOW(6) \
             WHERE user_id = ? AND plan_id = ? AND version = ?",
        )
        .bind(&goal)
        .bind(phase)
        .bind(next_version as i64)
        .bind(&plan_json)
        .bind(persisted_state.plan_md.as_deref())
        .bind(progress)
        .bind(subtask_count)
        .bind(user_id)
        .bind(plan_id)
        .bind(expected_version as i64)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        if update.rows_affected() == 0 {
            let latest: Option<(i64,)> =
                sqlx::query_as("SELECT version FROM plans WHERE user_id = ? AND plan_id = ?")
                    .bind(user_id)
                    .bind(plan_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(map_sqlx)?;
            tx.rollback().await.map_err(map_sqlx)?;
            let actual = latest.map(|(v,)| v as u64).unwrap_or(0);
            return Err(PlanLoadError::conflict(expected_version, actual));
        }

        let aborted = if subtask_ids.is_empty() {
            0
        } else {
            let placeholders = std::iter::repeat_n("?", subtask_ids.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "UPDATE plan_step_runs \
                 SET status = ?, finished_at = NOW(6) \
                 WHERE user_id = ? AND plan_id = ? AND finished_at IS NULL \
                   AND subtask_id IN ({placeholders})"
            );
            let mut q = sqlx::query(&sql)
                .bind(TaskStatus::Cancelled.as_str())
                .bind(user_id)
                .bind(plan_id);
            for sid in subtask_ids {
                q = q.bind(sid);
            }
            q.execute(&mut *tx).await.map_err(map_sqlx)?.rows_affected()
        };

        tx.commit().await.map_err(map_sqlx)?;
        state.version = next_version;
        Ok(aborted)
    }
}

// ─── In-memory implementation ────────────────────────────────────────────────

#[derive(Debug, Default)]
struct InMemoryPlanRepositoryState {
    plans: HashMap<(String, String), PlanModeState>,
    active_plans: HashMap<(String, String), String>,
    step_runs: HashMap<(String, String), PlanStepRun>,
}

/// Process-local repository for tests and unconfigured runtime defaults.
///
/// Unlike the removed filesystem cache, this fallback keeps all plan state in
/// memory, so it cannot leak stale plan files across runs or workspaces.
#[derive(Debug, Clone, Default)]
pub struct InMemoryPlanRepository {
    inner: Arc<RwLock<InMemoryPlanRepositoryState>>,
}

impl InMemoryPlanRepository {
    pub fn new() -> Self {
        Self::default()
    }
}

fn saved_plan_info(plan_id: &str, state: &PlanModeState) -> SavedPlanInfo {
    let status = if state.plan.progress_pct() == 100 {
        "completed"
    } else if state.plan.items_done() > 0 {
        "in_progress"
    } else {
        "pending"
    };
    SavedPlanInfo {
        name: plan_id.to_string(),
        session_id: state.session_hint.clone(),
        goal: state.goal.clone(),
        version: state.version,
        progress_pct: state.plan.progress_pct(),
        subtask_count: state.plan.subtasks.len(),
        status: status.to_string(),
    }
}

#[async_trait]
impl PlanRepository for InMemoryPlanRepository {
    async fn save(
        &self,
        user_id: &str,
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: Option<u64>,
    ) -> Result<(), PlanLoadError> {
        validate_plan_id(plan_id)?;
        ensure_state_owner(user_id, state)?;
        let mut guard = astra_core::sync_poison::recover_rwlock_write(&self.inner);
        let key = (user_id.to_string(), plan_id.to_string());
        match (guard.plans.get(&key).map(|s| s.version), expected_version) {
            (None, Some(expected)) if expected != 0 => {
                return Err(PlanLoadError::conflict(expected, 0));
            }
            (Some(actual), None) => {
                return Err(PlanLoadError::conflict(0, actual));
            }
            (Some(actual), Some(expected)) if expected != actual => {
                return Err(PlanLoadError::conflict(expected, actual));
            }
            (Some(actual), _) => {
                state.version = actual + 1;
            }
            (None, _) => {
                state.version = 1;
            }
        }
        guard.plans.insert(key, state.clone());
        Ok(())
    }

    async fn load(&self, user_id: &str, plan_id: &str) -> Result<PlanModeState, PlanLoadError> {
        validate_plan_id(plan_id)?;
        let key = (user_id.to_string(), plan_id.to_string());
        self.inner
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .plans
            .get(&key)
            .cloned()
            .ok_or_else(|| PlanLoadError::NotFound(plan_id.to_string()))
    }

    async fn list_for_user(
        &self,
        user_id: &str,
        filter: PlanListFilter<'_>,
    ) -> Result<Vec<SavedPlanInfo>, PlanLoadError> {
        let guard = astra_core::sync_poison::recover_rwlock_read(&self.inner);
        let mut plans = guard
            .plans
            .iter()
            .filter_map(|((uid, plan_id), state)| {
                if uid != user_id {
                    return None;
                }
                if let Some(session_id) = filter.session_id
                    && state.session_hint.as_deref() != Some(session_id)
                {
                    return None;
                }
                if let Some(phase) = filter.phase
                    && state.infer_phase().as_str() != phase
                {
                    return None;
                }
                Some(saved_plan_info(plan_id, state))
            })
            .collect::<Vec<_>>();
        plans.sort_by(|a, b| a.name.cmp(&b.name));
        if let Some(limit) = filter.limit {
            plans.truncate(limit.max(0) as usize);
        }
        Ok(plans)
    }

    async fn delete(&self, user_id: &str, plan_id: &str) -> Result<(), PlanLoadError> {
        validate_plan_id(plan_id)?;
        let key = (user_id.to_string(), plan_id.to_string());
        let mut guard = astra_core::sync_poison::recover_rwlock_write(&self.inner);
        if guard.plans.remove(&key).is_none() {
            return Err(PlanLoadError::NotFound(plan_id.to_string()));
        }
        guard
            .active_plans
            .retain(|(uid, _), active| uid != user_id || active != plan_id);
        guard
            .step_runs
            .retain(|(uid, _), run| uid != user_id || run.plan_id != plan_id);
        Ok(())
    }

    async fn set_active_plan(
        &self,
        user_id: &str,
        session_id: &str,
        plan_id: Option<&str>,
    ) -> Result<(), PlanLoadError> {
        let mut guard = astra_core::sync_poison::recover_rwlock_write(&self.inner);
        guard
            .active_plans
            .remove(&(user_id.to_string(), session_id.to_string()));
        if let Some(plan_id) = plan_id {
            validate_plan_id(plan_id)?;
            if !guard
                .plans
                .contains_key(&(user_id.to_string(), plan_id.to_string()))
            {
                return Err(PlanLoadError::NotFound(plan_id.to_string()));
            }
            guard
                .active_plans
                .retain(|(uid, _), active| uid != user_id || active != plan_id);
            guard.active_plans.insert(
                (user_id.to_string(), session_id.to_string()),
                plan_id.to_string(),
            );
        }
        Ok(())
    }

    async fn active_plan_for_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<String>, PlanLoadError> {
        Ok(self
            .inner
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .active_plans
            .get(&(user_id.to_string(), session_id.to_string()))
            .cloned())
    }

    async fn record_step_run(
        &self,
        user_id: &str,
        input: NewStepRun<'_>,
    ) -> Result<String, PlanLoadError> {
        let run_id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let mut guard = astra_core::sync_poison::recover_rwlock_write(&self.inner);
        let plan_key = (user_id.to_string(), input.plan_id.to_string());
        if !guard.plans.contains_key(&plan_key) {
            return Err(PlanLoadError::NotFound(input.plan_id.to_string()));
        }
        if guard.step_runs.iter().any(|((uid, _), run)| {
            uid == user_id
                && run.plan_id == input.plan_id
                && run.subtask_id == input.subtask_id
                && run.attempt == input.attempt
        }) {
            return Err(PlanLoadError::Conflict {
                expected: input.attempt as u64,
                actual: input.attempt as u64,
            });
        }
        guard.step_runs.insert(
            (user_id.to_string(), run_id.clone()),
            PlanStepRun {
                run_id: run_id.clone(),
                plan_id: input.plan_id.to_string(),
                subtask_id: input.subtask_id.to_string(),
                attempt: input.attempt,
                status: input.status,
                session_id: input.session_id.to_string(),
                started_at: now,
                finished_at: None,
                request_id: input.request_id.to_string(),
                error: None,
                artifact_ref: None,
            },
        );
        Ok(run_id)
    }

    async fn record_completed_step_run(
        &self,
        user_id: &str,
        input: NewStepRun<'_>,
        error: Option<&str>,
        artifact_ref: Option<&str>,
    ) -> Result<String, PlanLoadError> {
        let run_id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let mut guard = astra_core::sync_poison::recover_rwlock_write(&self.inner);
        let plan_key = (user_id.to_string(), input.plan_id.to_string());
        if !guard.plans.contains_key(&plan_key) {
            return Err(PlanLoadError::NotFound(input.plan_id.to_string()));
        }
        if guard.step_runs.iter().any(|((uid, _), run)| {
            uid == user_id
                && run.plan_id == input.plan_id
                && run.subtask_id == input.subtask_id
                && run.attempt == input.attempt
        }) {
            return Err(PlanLoadError::Conflict {
                expected: input.attempt as u64,
                actual: input.attempt as u64,
            });
        }
        guard.step_runs.insert(
            (user_id.to_string(), run_id.clone()),
            PlanStepRun {
                run_id: run_id.clone(),
                plan_id: input.plan_id.to_string(),
                subtask_id: input.subtask_id.to_string(),
                attempt: input.attempt,
                status: input.status,
                session_id: input.session_id.to_string(),
                started_at: now,
                finished_at: Some(now),
                request_id: input.request_id.to_string(),
                error: error.map(str::to_string),
                artifact_ref: artifact_ref.map(str::to_string),
            },
        );
        Ok(run_id)
    }

    async fn save_existing_and_start_step_run(
        &self,
        user_id: &str,
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: u64,
        input: NewStepRun<'_>,
    ) -> Result<String, PlanLoadError> {
        validate_plan_id(plan_id)?;
        ensure_state_owner(user_id, state)?;
        let run_id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let mut guard = astra_core::sync_poison::recover_rwlock_write(&self.inner);
        let plan_key = (user_id.to_string(), plan_id.to_string());
        let Some(actual) = guard.plans.get(&plan_key).map(|stored| stored.version) else {
            return Err(PlanLoadError::conflict(expected_version, 0));
        };
        if actual != expected_version {
            return Err(PlanLoadError::conflict(expected_version, actual));
        }
        if guard.step_runs.iter().any(|((uid, _), run)| {
            uid == user_id
                && run.plan_id == plan_id
                && run.subtask_id == input.subtask_id
                && run.attempt == input.attempt
        }) {
            return Err(PlanLoadError::Conflict {
                expected: input.attempt as u64,
                actual: input.attempt as u64,
            });
        }
        state.version = expected_version + 1;
        guard.plans.insert(plan_key, state.clone());
        guard.step_runs.insert(
            (user_id.to_string(), run_id.clone()),
            PlanStepRun {
                run_id: run_id.clone(),
                plan_id: plan_id.to_string(),
                subtask_id: input.subtask_id.to_string(),
                attempt: input.attempt,
                status: input.status,
                session_id: input.session_id.to_string(),
                started_at: now,
                finished_at: None,
                request_id: input.request_id.to_string(),
                error: None,
                artifact_ref: None,
            },
        );
        Ok(run_id)
    }

    async fn save_existing_and_record_completed_step_run(
        &self,
        user_id: &str,
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: u64,
        input: NewStepRun<'_>,
        error: Option<&str>,
        artifact_ref: Option<&str>,
    ) -> Result<String, PlanLoadError> {
        validate_plan_id(plan_id)?;
        ensure_state_owner(user_id, state)?;
        let run_id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let mut guard = astra_core::sync_poison::recover_rwlock_write(&self.inner);
        let plan_key = (user_id.to_string(), plan_id.to_string());
        let Some(actual) = guard.plans.get(&plan_key).map(|stored| stored.version) else {
            return Err(PlanLoadError::conflict(expected_version, 0));
        };
        if actual != expected_version {
            return Err(PlanLoadError::conflict(expected_version, actual));
        }
        if guard.step_runs.iter().any(|((uid, _), run)| {
            uid == user_id
                && run.plan_id == plan_id
                && run.subtask_id == input.subtask_id
                && run.attempt == input.attempt
        }) {
            return Err(PlanLoadError::Conflict {
                expected: input.attempt as u64,
                actual: input.attempt as u64,
            });
        }
        state.version = expected_version + 1;
        guard.plans.insert(plan_key, state.clone());
        guard.step_runs.insert(
            (user_id.to_string(), run_id.clone()),
            PlanStepRun {
                run_id: run_id.clone(),
                plan_id: plan_id.to_string(),
                subtask_id: input.subtask_id.to_string(),
                attempt: input.attempt,
                status: input.status,
                session_id: input.session_id.to_string(),
                started_at: now,
                finished_at: Some(now),
                request_id: input.request_id.to_string(),
                error: error.map(str::to_string),
                artifact_ref: artifact_ref.map(str::to_string),
            },
        );
        Ok(run_id)
    }

    async fn save_existing_and_finalize_step_run(
        &self,
        user_id: &str,
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: u64,
        run_id: &str,
        status: TaskStatus,
        error: Option<&str>,
        artifact_ref: Option<&str>,
    ) -> Result<(), PlanLoadError> {
        validate_plan_id(plan_id)?;
        ensure_state_owner(user_id, state)?;
        let mut guard = astra_core::sync_poison::recover_rwlock_write(&self.inner);
        let plan_key = (user_id.to_string(), plan_id.to_string());
        let Some(actual) = guard.plans.get(&plan_key).map(|stored| stored.version) else {
            return Err(PlanLoadError::conflict(expected_version, 0));
        };
        if actual != expected_version {
            return Err(PlanLoadError::conflict(expected_version, actual));
        }
        let run_key = (user_id.to_string(), run_id.to_string());
        let Some(run) = guard.step_runs.get(&run_key) else {
            return Err(PlanLoadError::NotFound(run_id.to_string()));
        };
        if run.plan_id != plan_id || run.finished_at.is_some() {
            return Err(PlanLoadError::NotFound(run_id.to_string()));
        }
        state.version = expected_version + 1;
        guard.plans.insert(plan_key, state.clone());
        let run = guard
            .step_runs
            .get_mut(&run_key)
            .expect("checked plan step run must remain present under write lock");
        run.status = status;
        run.finished_at = Some(Utc::now());
        run.error = error.map(str::to_string);
        run.artifact_ref = artifact_ref.map(str::to_string);
        Ok(())
    }

    async fn finalize_step_run(
        &self,
        user_id: &str,
        plan_id: &str,
        run_id: &str,
        status: TaskStatus,
        error: Option<&str>,
        artifact_ref: Option<&str>,
    ) -> Result<(), PlanLoadError> {
        let mut guard = astra_core::sync_poison::recover_rwlock_write(&self.inner);
        let run_key = (user_id.to_string(), run_id.to_string());
        let Some(run) = guard.step_runs.get_mut(&run_key) else {
            return Err(PlanLoadError::NotFound(run_id.to_string()));
        };
        if run.plan_id != plan_id {
            return Err(PlanLoadError::NotFound(run_id.to_string()));
        }
        if run.finished_at.is_some() {
            return Err(PlanLoadError::NotFound(run_id.to_string()));
        }
        run.status = status;
        run.finished_at = Some(Utc::now());
        run.error = error.map(str::to_string);
        run.artifact_ref = artifact_ref.map(str::to_string);
        Ok(())
    }

    async fn get_step_run(
        &self,
        user_id: &str,
        plan_id: &str,
        run_id: &str,
    ) -> Result<PlanStepRun, PlanLoadError> {
        let guard = astra_core::sync_poison::recover_rwlock_read(&self.inner);
        let run_key = (user_id.to_string(), run_id.to_string());
        let Some(run) = guard.step_runs.get(&run_key) else {
            return Err(PlanLoadError::NotFound(run_id.to_string()));
        };
        if run.plan_id != plan_id {
            return Err(PlanLoadError::NotFound(run_id.to_string()));
        }
        Ok(run.clone())
    }

    async fn list_step_runs(
        &self,
        user_id: &str,
        plan_id: &str,
        subtask_id: Option<&str>,
        limit: i32,
    ) -> Result<Vec<PlanStepRun>, PlanLoadError> {
        let guard = astra_core::sync_poison::recover_rwlock_read(&self.inner);
        let mut runs = guard
            .step_runs
            .iter()
            .filter(|((uid, _), _)| uid == user_id)
            .map(|(_, run)| run)
            .filter(|run| run.plan_id == plan_id)
            .filter(|run| subtask_id.is_none_or(|subtask_id| run.subtask_id == subtask_id))
            .cloned()
            .collect::<Vec<_>>();
        runs.sort_by(|a, b| {
            b.started_at
                .cmp(&a.started_at)
                .then_with(|| a.run_id.cmp(&b.run_id))
        });
        runs.truncate(limit.max(0) as usize);
        Ok(runs)
    }

    async fn abort_open_step_runs(
        &self,
        user_id: &str,
        plan_id: &str,
        subtask_ids: &[String],
    ) -> Result<u64, PlanLoadError> {
        let mut guard = astra_core::sync_poison::recover_rwlock_write(&self.inner);
        let now = Utc::now();
        let mut aborted = 0;
        for ((uid, _), run) in guard.step_runs.iter_mut() {
            if uid == user_id
                && run.plan_id == plan_id
                && run.finished_at.is_none()
                && subtask_ids
                    .iter()
                    .any(|subtask_id| subtask_id == &run.subtask_id)
            {
                run.status = TaskStatus::Cancelled;
                run.finished_at = Some(now);
                aborted += 1;
            }
        }
        Ok(aborted)
    }

    async fn save_existing_and_abort_open_step_runs(
        &self,
        user_id: &str,
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: u64,
        subtask_ids: &[String],
    ) -> Result<u64, PlanLoadError> {
        validate_plan_id(plan_id)?;
        ensure_state_owner(user_id, state)?;
        let mut guard = astra_core::sync_poison::recover_rwlock_write(&self.inner);
        let key = (user_id.to_string(), plan_id.to_string());
        let Some(actual) = guard.plans.get(&key).map(|s| s.version) else {
            return Err(PlanLoadError::conflict(expected_version, 0));
        };
        if actual != expected_version {
            return Err(PlanLoadError::conflict(expected_version, actual));
        }

        let now = Utc::now();
        let mut aborted = 0;
        for ((uid, _), run) in guard.step_runs.iter_mut() {
            if uid == user_id
                && run.plan_id == plan_id
                && run.finished_at.is_none()
                && subtask_ids
                    .iter()
                    .any(|subtask_id| subtask_id == &run.subtask_id)
            {
                run.status = TaskStatus::Cancelled;
                run.finished_at = Some(now);
                aborted += 1;
            }
        }
        state.version = expected_version + 1;
        guard.plans.insert(key, state.clone());
        Ok(aborted)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_plan_id_rejects_unsafe_ids() {
        for id in [
            "",
            "../etc/passwd",
            "foo/../bar",
            "foo/bar",
            "foo\\bar",
            "plan.json",
            "id with space",
            "a;b",
            "a&b",
        ] {
            let err = validate_plan_id(id).unwrap_err();
            assert!(
                matches!(err, PlanLoadError::InvalidId(_)),
                "should reject {id}: {err}"
            );
        }
    }

    #[test]
    fn validate_plan_id_accepts_safe_ids() {
        for id in ["abc", "plan-123", "my_plan_v2", "ABC-xyz_01"] {
            assert!(validate_plan_id(id).is_ok(), "should accept {id}");
        }
    }

    #[test]
    fn persisted_step_run_status_rejects_unknown_values() {
        assert_eq!(
            parse_persisted_step_run_status("in_progress").unwrap(),
            TaskStatus::InProgress
        );
        let err = parse_persisted_step_run_status("unknown_status").unwrap_err();
        assert!(
            matches!(err, PlanLoadError::Corrupt(_)),
            "unknown persisted step-run status must fail closed: {err}"
        );
    }

    #[tokio::test]
    async fn in_memory_save_and_load_roundtrip() {
        let repo = InMemoryPlanRepository::new();
        let mut state = PlanModeState::new_with_owner("test goal".into(), "u-1".into());
        state.plan_md = Some("# test plan".into());
        repo.save("u-1", "plan-1", &mut state, None).await.unwrap();

        let loaded = repo.load("u-1", "plan-1").await.unwrap();
        assert_eq!(loaded.goal, "test goal");
        assert_eq!(loaded.created_by.as_deref(), Some("u-1"));
        assert_eq!(loaded.plan_md.as_deref(), Some("# test plan"));
    }

    #[tokio::test]
    async fn atomic_step_writes_keep_plan_projection_and_attempt_evidence_in_lockstep() {
        let repo = InMemoryPlanRepository::new();
        let mut initial = PlanModeState::new_with_owner("ship durable work".into(), "u-1".into());
        initial.plan.subtasks = vec![
            astra_services::task_orchestrator::SubtaskPlan {
                id: "build".into(),
                title: "Build projection".into(),
                status: TaskStatus::Pending,
                ..Default::default()
            },
            astra_services::task_orchestrator::SubtaskPlan {
                id: "verify".into(),
                title: "Verify projection".into(),
                status: TaskStatus::Pending,
                ..Default::default()
            },
        ];
        repo.save("u-1", "plan-atomic", &mut initial, None)
            .await
            .expect("seed plan");

        let mut started = repo.load("u-1", "plan-atomic").await.unwrap();
        started.plan.subtasks[0].status = TaskStatus::InProgress;
        let run_id = repo
            .save_existing_and_start_step_run(
                "u-1",
                "plan-atomic",
                &mut started,
                1,
                NewStepRun {
                    plan_id: "plan-atomic",
                    subtask_id: "build",
                    attempt: 1,
                    status: TaskStatus::InProgress,
                    session_id: "session-a",
                    request_id: "run-a",
                },
            )
            .await
            .expect("atomically start step");
        assert_eq!(started.version, 2);

        let mut finished = repo.load("u-1", "plan-atomic").await.unwrap();
        finished.plan.subtasks[0].status = TaskStatus::Completed;
        repo.save_existing_and_finalize_step_run(
            "u-1",
            "plan-atomic",
            &mut finished,
            2,
            &run_id,
            TaskStatus::Completed,
            None,
            Some("artifact://build"),
        )
        .await
        .expect("atomically finish step");

        let mut completed = repo.load("u-1", "plan-atomic").await.unwrap();
        completed.plan.subtasks[1].status = TaskStatus::Completed;
        let completed_run_id = repo
            .save_existing_and_record_completed_step_run(
                "u-1",
                "plan-atomic",
                &mut completed,
                3,
                NewStepRun {
                    plan_id: "plan-atomic",
                    subtask_id: "verify",
                    attempt: 1,
                    status: TaskStatus::Completed,
                    session_id: "session-a",
                    request_id: "run-b",
                },
                None,
                Some("artifact://verify"),
            )
            .await
            .expect("atomically record completed step");

        let persisted = repo.load("u-1", "plan-atomic").await.unwrap();
        assert_eq!(persisted.version, 4);
        assert!(
            persisted
                .plan
                .subtasks
                .iter()
                .all(|subtask| subtask.status == TaskStatus::Completed)
        );
        let started_run = repo
            .get_step_run("u-1", "plan-atomic", &run_id)
            .await
            .unwrap();
        assert_eq!(started_run.status, TaskStatus::Completed);
        assert!(started_run.finished_at.is_some());
        let completed_run = repo
            .get_step_run("u-1", "plan-atomic", &completed_run_id)
            .await
            .unwrap();
        assert_eq!(completed_run.status, TaskStatus::Completed);
        assert!(completed_run.finished_at.is_some());
    }

    #[tokio::test]
    async fn atomic_step_write_conflict_leaves_plan_and_attempts_unchanged() {
        let repo = InMemoryPlanRepository::new();
        let mut initial = PlanModeState::new_with_owner("goal".into(), "u-1".into());
        initial
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "step".into(),
                title: "Step".into(),
                status: TaskStatus::Pending,
                ..Default::default()
            });
        repo.save("u-1", "plan-conflict", &mut initial, None)
            .await
            .unwrap();
        let mut first = repo.load("u-1", "plan-conflict").await.unwrap();
        first.plan.subtasks[0].status = TaskStatus::InProgress;
        repo.save_existing_and_start_step_run(
            "u-1",
            "plan-conflict",
            &mut first,
            1,
            NewStepRun {
                plan_id: "plan-conflict",
                subtask_id: "step",
                attempt: 1,
                status: TaskStatus::InProgress,
                session_id: "session-a",
                request_id: "run-a",
            },
        )
        .await
        .unwrap();

        let mut stale = initial;
        stale.plan.subtasks[0].status = TaskStatus::Completed;
        let error = repo
            .save_existing_and_record_completed_step_run(
                "u-1",
                "plan-conflict",
                &mut stale,
                1,
                NewStepRun {
                    plan_id: "plan-conflict",
                    subtask_id: "step",
                    attempt: 2,
                    status: TaskStatus::Completed,
                    session_id: "session-a",
                    request_id: "run-b",
                },
                None,
                None,
            )
            .await
            .expect_err("stale plan version must not append an orphan attempt");
        assert!(matches!(error, PlanLoadError::Conflict { .. }));

        let persisted = repo.load("u-1", "plan-conflict").await.unwrap();
        assert_eq!(persisted.version, 2);
        assert_eq!(persisted.plan.subtasks[0].status, TaskStatus::InProgress);
        assert_eq!(
            repo.list_step_runs("u-1", "plan-conflict", None, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn in_memory_load_returns_not_found_for_wrong_user() {
        let repo = InMemoryPlanRepository::new();
        let mut state = PlanModeState::new_with_owner("goal".into(), "u-1".into());
        repo.save("u-1", "plan-2", &mut state, None).await.unwrap();

        let err = repo.load("u-other", "plan-2").await.unwrap_err();
        assert!(matches!(err, PlanLoadError::NotFound(_)));
    }

    #[tokio::test]
    async fn in_memory_reuses_plan_id_across_users_without_collision() {
        let repo = InMemoryPlanRepository::new();
        let mut state_a = PlanModeState::new_with_owner("goal A".into(), "u-1".into());
        let mut state_b = PlanModeState::new_with_owner("goal B".into(), "u-2".into());

        repo.save("u-1", "shared-plan", &mut state_a, None)
            .await
            .unwrap();
        repo.save("u-2", "shared-plan", &mut state_b, None)
            .await
            .unwrap();

        assert_eq!(
            repo.load("u-1", "shared-plan").await.unwrap().goal,
            "goal A"
        );
        assert_eq!(
            repo.load("u-2", "shared-plan").await.unwrap().goal,
            "goal B"
        );
    }

    #[tokio::test]
    async fn in_memory_save_rejects_owner_mismatch() {
        let repo = InMemoryPlanRepository::new();
        let mut state = PlanModeState::new_with_owner("goal".into(), "u-1".into());

        let err = repo
            .save("u-2", "plan-owner-mismatch", &mut state, None)
            .await
            .expect_err("row owner and plan_json owner must match");
        assert!(matches!(err, PlanLoadError::Internal(_)));
    }

    #[tokio::test]
    async fn in_memory_step_run_roundtrip_and_abort() {
        let repo = InMemoryPlanRepository::new();
        let mut state = PlanModeState::new_with_owner("step plan".into(), "u-1".into());
        repo.save("u-1", "p", &mut state, None).await.unwrap();
        let run_id = repo
            .record_step_run(
                "u-1",
                NewStepRun {
                    plan_id: "p",
                    subtask_id: "s",
                    attempt: 1,
                    status: TaskStatus::InProgress,
                    session_id: "sess",
                    request_id: "req",
                },
            )
            .await
            .unwrap();
        let open = repo.get_step_run("u-1", "p", &run_id).await.unwrap();
        assert_eq!(open.status, TaskStatus::InProgress);
        assert!(open.finished_at.is_none());

        let aborted = repo
            .abort_open_step_runs("u-1", "p", &[String::from("s")])
            .await
            .unwrap();
        assert_eq!(aborted, 1);

        let closed = repo.get_step_run("u-1", "p", &run_id).await.unwrap();
        assert_eq!(closed.status, TaskStatus::Cancelled);
        assert!(closed.finished_at.is_some());

        assert!(matches!(
            repo.get_step_run("u-other", "p", &run_id).await,
            Err(PlanLoadError::NotFound(_))
        ));
        assert!(
            repo.list_step_runs("u-other", "p", Some("s"), 10)
                .await
                .unwrap()
                .is_empty(),
            "step-run listing must stay owner-scoped"
        );
    }
}
