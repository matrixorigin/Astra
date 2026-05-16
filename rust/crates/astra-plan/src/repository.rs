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
//! * At most one session has `active_plan_id = P` at any time — enforced by
//!   [`PlanRepository::set_active_plan`] clearing other sessions pointing at
//!   the same plan in one transaction.
//! * `plans.session_id` is a routing hint (most-recent executor); canonical
//!   cross-session audit lives in `plan_step_runs.session_id`.
//! * `plan_step_runs` is append-only. Every subtask attempt creates a new row
//!   — never UPDATE, never DELETE.

use crate::decompose::PlanModeState;
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

const MAX_PLAN_RESUME_GOAL_CHARS: usize = 160;
const MAX_PLAN_RESUME_SUBTASK_CHARS: usize = 80;

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
    pub goal: String,
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
    /// Persist a plan, inserting or updating by `plan_id`.
    ///
    /// `expected_version` enforces optimistic concurrency: pass the version
    /// observed at load time; the write fails with [`PlanLoadError::Conflict`]
    /// if the stored version has moved. Pass `None` for a first insert.
    async fn save(
        &self,
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: Option<u64>,
    ) -> Result<(), PlanLoadError>;

    /// Load a plan by id. Returns [`PlanLoadError::NotFound`] if missing.
    async fn load(&self, plan_id: &str) -> Result<PlanModeState, PlanLoadError>;

    /// Load a plan and verify `user_id` owns it. Returns [`PlanLoadError::NotFound`]
    /// for non-owned plans too — do not leak existence via a 403.
    async fn load_owned(
        &self,
        plan_id: &str,
        user_id: &str,
    ) -> Result<PlanModeState, PlanLoadError>;

    /// List plans for a user, optionally filtered by session or phase.
    async fn list_for_user(
        &self,
        user_id: &str,
        filter: PlanListFilter<'_>,
    ) -> Result<Vec<SavedPlanInfo>, PlanLoadError>;

    /// Delete a plan (and, for cloud, cascade its `plan_step_runs`).
    async fn delete(&self, plan_id: &str) -> Result<(), PlanLoadError>;

    /// Mark `plan_id` as the active plan for `session_id`, atomically clearing
    /// any other session currently pointing at the same plan. Passing
    /// `plan_id = None` clears the session's active plan.
    ///
    /// No-op (and returns Ok) if the session does not exist yet, so CLI can
    /// invoke it before a session row is created.
    async fn set_active_plan(
        &self,
        session_id: &str,
        plan_id: Option<&str>,
    ) -> Result<(), PlanLoadError>;

    /// Return the currently active `plan_id` for `session_id`, if any.
    async fn active_plan_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<String>, PlanLoadError>;

    /// Append a new step-run row. Returns the assigned `run_id`.
    async fn record_step_run(&self, input: NewStepRun<'_>) -> Result<String, PlanLoadError>;

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
        input: NewStepRun<'_>,
        error: Option<&str>,
        artifact_ref: Option<&str>,
    ) -> Result<String, PlanLoadError>;

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
        plan_id: &str,
        run_id: &str,
        status: TaskStatus,
        error: Option<&str>,
        artifact_ref: Option<&str>,
    ) -> Result<(), PlanLoadError>;

    /// Fetch a single step-run by `(run_id, plan_id)`. Returns `NotFound` if
    /// the row doesn't exist or belongs to a different plan.
    async fn get_step_run(&self, plan_id: &str, run_id: &str)
    -> Result<PlanStepRun, PlanLoadError>;

    /// List step runs for a plan (optionally one subtask), newest first.
    async fn list_step_runs(
        &self,
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
        plan_id: &str,
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
}

fn infer_phase_for_persist(state: &PlanModeState) -> &'static str {
    if state.plan.progress_pct() == 100 {
        "completed"
    } else if state.plan.subtasks.is_empty() {
        "planning"
    } else if state.plan.items_done() > 0 {
        "executing"
    } else {
        "refining"
    }
}

fn map_sqlx(err: sqlx::Error) -> PlanLoadError {
    PlanLoadError::Internal(format!("sql error: {err}"))
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

fn truncate_plan_resume_text(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

pub fn plan_resume_digest(state: &PlanModeState) -> Option<String> {
    let goal = state.goal.trim();
    let subtasks = &state.plan.subtasks;
    if goal.is_empty() && subtasks.is_empty() {
        return None;
    }

    let total = subtasks.len();
    let done = subtasks
        .iter()
        .filter(|subtask| subtask.status == TaskStatus::Completed)
        .count();
    let open = subtasks
        .iter()
        .filter(|subtask| !subtask.status.is_terminal() && subtask.status != TaskStatus::InProgress)
        .count();
    let in_progress_title = subtasks
        .iter()
        .find(|subtask| subtask.status == TaskStatus::InProgress)
        .map(|subtask| truncate_plan_resume_text(&subtask.title, MAX_PLAN_RESUME_SUBTASK_CHARS));

    let mut out = String::from("[plan-resume]");
    if !goal.is_empty() {
        out.push_str(&format!(
            " goal=\"{}\"",
            truncate_plan_resume_text(goal, MAX_PLAN_RESUME_GOAL_CHARS)
        ));
    }
    if let Some(title) = in_progress_title {
        out.push_str(&format!(" · in_progress=\"{title}\""));
    }
    if total > 0 {
        out.push_str(&format!(" · open={open} · done={done}/{total}"));
    }
    Some(out)
}

pub fn plan_resume_prompt_hint(state: &PlanModeState) -> Option<String> {
    let digest = plan_resume_digest(state)?;
    Some(format!(
        "\n\n## Active Plan\n{digest}\n\n\
         A plan is currently in-flight for this session. Treat the next turn as a \
         continuation — resume from the in-progress subtask, respect the approved \
         plan structure, and call `exit_plan_mode` only if the plan needs to be \
         abandoned before completion."
    ))
}

/// Translate the MySQL duplicate-key error (1062) raised by the unique
/// `(plan_id, subtask_id, attempt)` index on `plan_step_runs` into
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
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: Option<u64>,
    ) -> Result<(), PlanLoadError> {
        validate_plan_id(plan_id)?;
        let phase = infer_phase_for_persist(state);
        let progress = state.plan.progress_pct() as i32;
        let goal = state.goal.clone();
        let user_id = state
            .created_by
            .clone()
            .ok_or_else(|| PlanLoadError::Internal("plan has no owner (created_by=None)".into()))?;

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
        let current: Option<(i64,)> = sqlx::query_as("SELECT version FROM plans WHERE plan_id = ?")
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
                .bind(&user_id)
                .bind(state.session_hint.as_deref())
                .bind(&goal)
                .bind(phase)
                .bind(&plan_json)
                .bind(state.plan_md.as_deref())
                .bind(progress)
                .bind(subtask_count)
                .bind(&user_id)
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
                     WHERE plan_id = ? AND version = ?",
                )
                .bind(&goal)
                .bind(phase)
                .bind(next_version as i64)
                .bind(&plan_json)
                .bind(state.plan_md.as_deref())
                .bind(progress)
                .bind(subtask_count)
                .bind(plan_id)
                .bind(stored)
                .execute(&self.pool)
                .await
                .map_err(map_sqlx)?;

                if res.rows_affected() == 0 {
                    // Another writer moved the version under us. Read back
                    // the actual stored version so the error carries the
                    // real conflict pair, not the stale `stored` we read.
                    let latest: Option<(i64,)> =
                        sqlx::query_as("SELECT version FROM plans WHERE plan_id = ?")
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

    async fn load(&self, plan_id: &str) -> Result<PlanModeState, PlanLoadError> {
        validate_plan_id(plan_id)?;
        let row = sqlx::query(
            "SELECT plan_json, plan_md, session_id, version FROM plans WHERE plan_id = ?",
        )
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
        state.session_hint = session_hint;
        state.plan_md = plan_md.or(state.plan_md);
        // `plans.version` is the authoritative optimistic-concurrency value.
        // Old rows written before the save() ordering fix may have a stale
        // version inside plan_json; trust the column.
        state.version = version_col as u64;
        Ok(state)
    }

    async fn load_owned(
        &self,
        plan_id: &str,
        user_id: &str,
    ) -> Result<PlanModeState, PlanLoadError> {
        let state = self.load(plan_id).await?;
        match &state.created_by {
            Some(owner) if owner != user_id => Err(PlanLoadError::NotFound(plan_id.to_string())),
            _ => Ok(state),
        }
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
            "SELECT plan_id, goal, phase, progress_pct, subtask_count \
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
            let goal: String = row.try_get("goal").map_err(map_sqlx)?;
            let phase: String = row.try_get("phase").map_err(map_sqlx)?;
            let progress_pct: i32 = row.try_get("progress_pct").map_err(map_sqlx)?;
            let subtask_count: i32 = row.try_get("subtask_count").map_err(map_sqlx)?;
            out.push(SavedPlanInfo {
                name: plan_id,
                goal,
                progress_pct: progress_pct as u32,
                subtask_count: subtask_count as usize,
                status: phase,
            });
        }
        Ok(out)
    }

    async fn delete(&self, plan_id: &str) -> Result<(), PlanLoadError> {
        validate_plan_id(plan_id)?;
        let mut tx = self.pool.begin().await.map_err(map_sqlx)?;

        let result = sqlx::query("DELETE FROM plans WHERE plan_id = ?")
            .bind(plan_id)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;

        if result.rows_affected() == 0 {
            tx.rollback().await.map_err(map_sqlx)?;
            return Err(PlanLoadError::NotFound(plan_id.to_string()));
        }

        // plan_step_runs has no FK (HTAP keeps writes fast) — cascade here.
        sqlx::query("DELETE FROM plan_step_runs WHERE plan_id = ?")
            .bind(plan_id)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;

        // Any session still pointing at this plan must be cleared so we don't
        // strand a dangling foreign reference.
        sqlx::query("UPDATE agent_sessions SET active_plan_id = NULL WHERE active_plan_id = ?")
            .bind(plan_id)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;

        tx.commit().await.map_err(map_sqlx)?;
        Ok(())
    }

    async fn set_active_plan(
        &self,
        session_id: &str,
        plan_id: Option<&str>,
    ) -> Result<(), PlanLoadError> {
        let mut tx = self.pool.begin().await.map_err(map_sqlx)?;

        if let Some(pid) = plan_id {
            // Clear any OTHER session currently pointing at this plan.
            sqlx::query(
                "UPDATE agent_sessions SET active_plan_id = NULL \
                 WHERE active_plan_id = ? AND session_id <> ?",
            )
            .bind(pid)
            .bind(session_id)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;

            // Refresh the plans.session_id routing hint.
            sqlx::query("UPDATE plans SET session_id = ?, updated_at = NOW(6) WHERE plan_id = ?")
                .bind(session_id)
                .bind(pid)
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx)?;
        }

        // Session row may not exist yet — no-op in that case.
        sqlx::query("UPDATE agent_sessions SET active_plan_id = ? WHERE session_id = ?")
            .bind(plan_id)
            .bind(session_id)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;

        tx.commit().await.map_err(map_sqlx)?;
        Ok(())
    }

    async fn active_plan_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<String>, PlanLoadError> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT active_plan_id FROM agent_sessions WHERE session_id = ?")
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(map_sqlx)?;
        Ok(row.and_then(|(id,)| id))
    }

    async fn record_step_run(&self, input: NewStepRun<'_>) -> Result<String, PlanLoadError> {
        let run_id = Uuid::new_v4().to_string();
        let res = sqlx::query(
            "INSERT INTO plan_step_runs \
                 (run_id, plan_id, subtask_id, attempt, status, session_id, \
                  started_at, request_id) \
             VALUES (?, ?, ?, ?, ?, ?, NOW(6), ?)",
        )
        .bind(&run_id)
        .bind(input.plan_id)
        .bind(input.subtask_id)
        .bind(input.attempt)
        .bind(input.status.as_str())
        .bind(input.session_id)
        .bind(input.request_id)
        .execute(&self.pool)
        .await;
        match res {
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
        input: NewStepRun<'_>,
        error: Option<&str>,
        artifact_ref: Option<&str>,
    ) -> Result<String, PlanLoadError> {
        let run_id = Uuid::new_v4().to_string();
        let res = sqlx::query(
            "INSERT INTO plan_step_runs \
                 (run_id, plan_id, subtask_id, attempt, status, session_id, \
                  started_at, finished_at, request_id, error, artifact_ref) \
             VALUES (?, ?, ?, ?, ?, ?, NOW(6), NOW(6), ?, ?, ?)",
        )
        .bind(&run_id)
        .bind(input.plan_id)
        .bind(input.subtask_id)
        .bind(input.attempt)
        .bind(input.status.as_str())
        .bind(input.session_id)
        .bind(input.request_id)
        .bind(error)
        .bind(artifact_ref)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(run_id),
            Err(e) => Err(map_step_run_insert_error(
                e,
                input.plan_id,
                input.subtask_id,
                input.attempt,
            )),
        }
    }

    async fn finalize_step_run(
        &self,
        plan_id: &str,
        run_id: &str,
        status: TaskStatus,
        error: Option<&str>,
        artifact_ref: Option<&str>,
    ) -> Result<(), PlanLoadError> {
        let result = sqlx::query(
            "UPDATE plan_step_runs \
             SET status = ?, finished_at = NOW(6), error = ?, artifact_ref = ? \
             WHERE run_id = ? AND plan_id = ? AND finished_at IS NULL",
        )
        .bind(status.as_str())
        .bind(error)
        .bind(artifact_ref)
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
        plan_id: &str,
        run_id: &str,
    ) -> Result<PlanStepRun, PlanLoadError> {
        let row = sqlx::query(
            "SELECT run_id, plan_id, subtask_id, attempt, status, session_id, \
                    started_at, finished_at, request_id, error, artifact_ref \
             FROM plan_step_runs \
             WHERE run_id = ? AND plan_id = ?",
        )
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
            status: TaskStatus::parse_status(&r.try_get::<String, _>("status").map_err(map_sqlx)?),
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
        plan_id: &str,
        subtask_id: Option<&str>,
        limit: i32,
    ) -> Result<Vec<PlanStepRun>, PlanLoadError> {
        validate_plan_id(plan_id)?;
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
                 WHERE plan_id = ? AND subtask_id = ? \
                 ORDER BY started_at DESC, run_id ASC LIMIT ?",
            )
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
                 WHERE plan_id = ? \
                 ORDER BY started_at DESC, run_id ASC LIMIT ?",
            )
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
                status: TaskStatus::parse_status(
                    &r.try_get::<String, _>("status").map_err(map_sqlx)?,
                ),
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
             WHERE plan_id = ? AND finished_at IS NULL \
               AND subtask_id IN ({placeholders})"
        );
        let mut q = sqlx::query(&sql)
            .bind(TaskStatus::Cancelled.as_str())
            .bind(plan_id);
        for sid in subtask_ids {
            q = q.bind(sid);
        }
        let result = q.execute(&self.pool).await.map_err(map_sqlx)?;
        Ok(result.rows_affected())
    }
}

// ─── In-memory implementation ────────────────────────────────────────────────

#[derive(Debug, Default)]
struct InMemoryPlanRepositoryState {
    plans: HashMap<String, PlanModeState>,
    active_plans: HashMap<String, String>,
    step_runs: HashMap<String, PlanStepRun>,
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
        goal: state.goal.clone(),
        progress_pct: state.plan.progress_pct(),
        subtask_count: state.plan.subtasks.len(),
        status: status.to_string(),
    }
}

#[async_trait]
impl PlanRepository for InMemoryPlanRepository {
    async fn save(
        &self,
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: Option<u64>,
    ) -> Result<(), PlanLoadError> {
        validate_plan_id(plan_id)?;
        let mut guard = self.inner.write().unwrap_or_else(|p| p.into_inner());
        match (
            guard.plans.get(plan_id).map(|s| s.version),
            expected_version,
        ) {
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
        guard.plans.insert(plan_id.to_string(), state.clone());
        Ok(())
    }

    async fn load(&self, plan_id: &str) -> Result<PlanModeState, PlanLoadError> {
        validate_plan_id(plan_id)?;
        self.inner
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .plans
            .get(plan_id)
            .cloned()
            .ok_or_else(|| PlanLoadError::NotFound(plan_id.to_string()))
    }

    async fn load_owned(
        &self,
        plan_id: &str,
        user_id: &str,
    ) -> Result<PlanModeState, PlanLoadError> {
        let state = self.load(plan_id).await?;
        match &state.created_by {
            Some(owner) if owner != user_id => Err(PlanLoadError::NotFound(plan_id.to_string())),
            _ => Ok(state),
        }
    }

    async fn list_for_user(
        &self,
        user_id: &str,
        filter: PlanListFilter<'_>,
    ) -> Result<Vec<SavedPlanInfo>, PlanLoadError> {
        let guard = self.inner.read().unwrap_or_else(|p| p.into_inner());
        let mut plans = guard
            .plans
            .iter()
            .filter_map(|(plan_id, state)| {
                let owner = state.created_by.as_deref().unwrap_or(user_id);
                if owner != user_id {
                    return None;
                }
                if let Some(session_id) = filter.session_id
                    && state.session_hint.as_deref() != Some(session_id)
                {
                    return None;
                }
                if let Some(phase) = filter.phase
                    && infer_phase_for_persist(state) != phase
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

    async fn delete(&self, plan_id: &str) -> Result<(), PlanLoadError> {
        validate_plan_id(plan_id)?;
        let mut guard = self.inner.write().unwrap_or_else(|p| p.into_inner());
        guard.plans.remove(plan_id);
        guard.active_plans.retain(|_, active| active != plan_id);
        guard.step_runs.retain(|_, run| run.plan_id != plan_id);
        Ok(())
    }

    async fn set_active_plan(
        &self,
        session_id: &str,
        plan_id: Option<&str>,
    ) -> Result<(), PlanLoadError> {
        let mut guard = self.inner.write().unwrap_or_else(|p| p.into_inner());
        guard.active_plans.remove(session_id);
        if let Some(plan_id) = plan_id {
            validate_plan_id(plan_id)?;
            guard.active_plans.retain(|_, active| active != plan_id);
            guard
                .active_plans
                .insert(session_id.to_string(), plan_id.to_string());
        }
        Ok(())
    }

    async fn active_plan_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<String>, PlanLoadError> {
        Ok(self
            .inner
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .active_plans
            .get(session_id)
            .cloned())
    }

    async fn record_step_run(&self, input: NewStepRun<'_>) -> Result<String, PlanLoadError> {
        let run_id = Uuid::new_v4().to_string();
        let now = Utc::now();
        self.inner
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .step_runs
            .insert(
                run_id.clone(),
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
        input: NewStepRun<'_>,
        error: Option<&str>,
        artifact_ref: Option<&str>,
    ) -> Result<String, PlanLoadError> {
        let run_id = Uuid::new_v4().to_string();
        let now = Utc::now();
        self.inner
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .step_runs
            .insert(
                run_id.clone(),
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

    async fn finalize_step_run(
        &self,
        plan_id: &str,
        run_id: &str,
        status: TaskStatus,
        error: Option<&str>,
        artifact_ref: Option<&str>,
    ) -> Result<(), PlanLoadError> {
        let mut guard = self.inner.write().unwrap_or_else(|p| p.into_inner());
        let Some(run) = guard.step_runs.get_mut(run_id) else {
            return Err(PlanLoadError::NotFound(run_id.to_string()));
        };
        if run.plan_id != plan_id {
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
        plan_id: &str,
        run_id: &str,
    ) -> Result<PlanStepRun, PlanLoadError> {
        let guard = self.inner.read().unwrap_or_else(|p| p.into_inner());
        let Some(run) = guard.step_runs.get(run_id) else {
            return Err(PlanLoadError::NotFound(run_id.to_string()));
        };
        if run.plan_id != plan_id {
            return Err(PlanLoadError::NotFound(run_id.to_string()));
        }
        Ok(run.clone())
    }

    async fn list_step_runs(
        &self,
        plan_id: &str,
        subtask_id: Option<&str>,
        limit: i32,
    ) -> Result<Vec<PlanStepRun>, PlanLoadError> {
        let guard = self.inner.read().unwrap_or_else(|p| p.into_inner());
        let mut runs = guard
            .step_runs
            .values()
            .filter(|run| run.plan_id == plan_id)
            .filter(|run| subtask_id.is_none_or(|subtask_id| run.subtask_id == subtask_id))
            .cloned()
            .collect::<Vec<_>>();
        runs.sort_by(|a, b| {
            b.started_at
                .cmp(&a.started_at)
                .then_with(|| b.run_id.cmp(&a.run_id))
        });
        runs.truncate(limit.max(0) as usize);
        Ok(runs)
    }

    async fn abort_open_step_runs(
        &self,
        plan_id: &str,
        subtask_ids: &[String],
    ) -> Result<u64, PlanLoadError> {
        let mut guard = self.inner.write().unwrap_or_else(|p| p.into_inner());
        let now = Utc::now();
        let mut aborted = 0;
        for run in guard.step_runs.values_mut() {
            if run.plan_id == plan_id
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
}

/// Fetch the rendered system-prompt section for the session's active plan, if
/// one exists. Returns `None` when the session has no active plan. Swallows
/// any repo errors to `None` so that a transient DB hiccup does not block chat
/// turns — the worst-case failure mode is a missing hint on one turn.
pub async fn plan_resume_hint_for_session(
    repo: &dyn PlanRepository,
    session_id: &str,
) -> Option<String> {
    let plan_id = repo
        .active_plan_for_session(session_id)
        .await
        .ok()
        .flatten()?;
    let state = repo.load(&plan_id).await.ok()?;
    plan_resume_prompt_hint(&state)
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
    fn plan_resume_prompt_hint_returns_none_for_empty_state() {
        assert!(plan_resume_prompt_hint(&PlanModeState::new(String::new())).is_none());
    }

    #[test]
    fn plan_resume_prompt_hint_formats_goal_and_active_subtask() {
        let mut state = PlanModeState::new("Ship auth overhaul".into());
        state.plan.subtasks = vec![
            astra_services::task_orchestrator::SubtaskPlan {
                id: "a".into(),
                title: "schema".into(),
                status: TaskStatus::Completed,
                ..Default::default()
            },
            astra_services::task_orchestrator::SubtaskPlan {
                id: "b".into(),
                title: "middleware refactor".into(),
                status: TaskStatus::InProgress,
                ..Default::default()
            },
            astra_services::task_orchestrator::SubtaskPlan {
                id: "c".into(),
                title: "tests".into(),
                status: TaskStatus::Pending,
                ..Default::default()
            },
        ];

        let hint = plan_resume_prompt_hint(&state).expect("hint");
        assert!(hint.contains("## Active Plan"), "{hint}");
        assert!(hint.contains("goal=\"Ship auth overhaul\""), "{hint}");
        assert!(
            hint.contains("in_progress=\"middleware refactor\""),
            "{hint}"
        );
    }

    #[tokio::test]
    async fn in_memory_save_and_load_roundtrip() {
        let repo = InMemoryPlanRepository::new();
        let mut state = PlanModeState::new_with_owner("test goal".into(), "u-1".into());
        state.plan_md = Some("# test plan".into());
        repo.save("plan-1", &mut state, None).await.unwrap();

        let loaded = repo.load("plan-1").await.unwrap();
        assert_eq!(loaded.goal, "test goal");
        assert_eq!(loaded.created_by.as_deref(), Some("u-1"));
        assert_eq!(loaded.plan_md.as_deref(), Some("# test plan"));
    }

    #[tokio::test]
    async fn in_memory_load_owned_returns_not_found_for_wrong_user() {
        let repo = InMemoryPlanRepository::new();
        let mut state = PlanModeState::new_with_owner("goal".into(), "u-1".into());
        repo.save("plan-2", &mut state, None).await.unwrap();

        let err = repo.load_owned("plan-2", "u-other").await.unwrap_err();
        assert!(matches!(err, PlanLoadError::NotFound(_)));
    }

    #[tokio::test]
    async fn in_memory_step_run_roundtrip_and_abort() {
        let repo = InMemoryPlanRepository::new();
        let run_id = repo
            .record_step_run(NewStepRun {
                plan_id: "p",
                subtask_id: "s",
                attempt: 1,
                status: TaskStatus::InProgress,
                session_id: "sess",
                request_id: "req",
            })
            .await
            .unwrap();
        let open = repo.get_step_run("p", &run_id).await.unwrap();
        assert_eq!(open.status, TaskStatus::InProgress);
        assert!(open.finished_at.is_none());

        let aborted = repo
            .abort_open_step_runs("p", &[String::from("s")])
            .await
            .unwrap();
        assert_eq!(aborted, 1);

        let closed = repo.get_step_run("p", &run_id).await.unwrap();
        assert_eq!(closed.status, TaskStatus::Cancelled);
        assert!(closed.finished_at.is_some());
    }
}
