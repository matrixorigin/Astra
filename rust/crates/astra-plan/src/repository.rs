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
//! * [`LocalCachePlanRepository`] — thin wrapper over the legacy on-disk
//!   `~/.astra/plans/{plan_id}.json` helpers on [`PlanModeState`]. Used in
//!   tests and as an offline fallback; it does not know about `plan_step_runs`.
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

use crate::decompose::{PlanLoadError, PlanModeState, SavedPlanInfo};
use astra_services::task_orchestrator::TaskStatus;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, Pool, Row};
use uuid::Uuid;

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

    /// List step runs for a plan (optionally one subtask), newest first.
    async fn list_step_runs(
        &self,
        plan_id: &str,
        subtask_id: Option<&str>,
        limit: i32,
    ) -> Result<Vec<PlanStepRun>, PlanLoadError>;
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

#[async_trait]
impl PlanRepository for CloudPlanRepository {
    async fn save(
        &self,
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: Option<u64>,
    ) -> Result<(), PlanLoadError> {
        PlanModeState::validate_plan_id(plan_id)?;
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
                     VALUES (?, ?, ?, ?, ?, 1, ?, NULL, ?, ?, ?, NOW(6), NOW(6))",
                )
                .bind(plan_id)
                .bind(&user_id)
                .bind(state.session_hint.as_deref())
                .bind(&goal)
                .bind(phase)
                .bind(&plan_json)
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
                // `rows_affected() == 0` → conflict. Session_id / plan_md /
                // user_id are intentionally NOT in SET so routine saves
                // don't clobber hints set via set_active_plan.
                let res = sqlx::query(
                    "UPDATE plans \
                     SET goal = ?, phase = ?, version = ?, plan_json = ?, \
                         progress_pct = ?, subtask_count = ?, updated_at = NOW(6) \
                     WHERE plan_id = ? AND version = ?",
                )
                .bind(&goal)
                .bind(phase)
                .bind(next_version as i64)
                .bind(&plan_json)
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
        PlanModeState::validate_plan_id(plan_id)?;
        let row = sqlx::query("SELECT plan_json, session_id, version FROM plans WHERE plan_id = ?")
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
        let version_col: i64 = row
            .try_get("version")
            .map_err(|e| PlanLoadError::Corrupt(format!("read version: {e}")))?;
        let mut state = serde_json::from_str::<PlanModeState>(&plan_json)
            .map_err(|e| PlanLoadError::Corrupt(format!("parse plan state: {e}")))?;
        state.session_hint = session_hint;
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
        PlanModeState::validate_plan_id(plan_id)?;
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
        sqlx::query(
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
        .await
        .map_err(map_sqlx)?;
        Ok(run_id)
    }

    async fn record_completed_step_run(
        &self,
        input: NewStepRun<'_>,
        error: Option<&str>,
        artifact_ref: Option<&str>,
    ) -> Result<String, PlanLoadError> {
        let run_id = Uuid::new_v4().to_string();
        sqlx::query(
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
        .await
        .map_err(map_sqlx)?;
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

    async fn list_step_runs(
        &self,
        plan_id: &str,
        subtask_id: Option<&str>,
        limit: i32,
    ) -> Result<Vec<PlanStepRun>, PlanLoadError> {
        PlanModeState::validate_plan_id(plan_id)?;
        let limit = limit.clamp(1, 1000);

        let rows = if let Some(sid) = subtask_id {
            sqlx::query(
                "SELECT run_id, plan_id, subtask_id, attempt, status, session_id, \
                        started_at, finished_at, request_id, error, artifact_ref \
                 FROM plan_step_runs \
                 WHERE plan_id = ? AND subtask_id = ? \
                 ORDER BY started_at DESC LIMIT ?",
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
                 ORDER BY started_at DESC LIMIT ?",
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
}

// ─── Local-cache (filesystem) implementation ─────────────────────────────────

/// Filesystem-backed repository — wraps the legacy `~/.astra/plans/*.json`
/// helpers on [`PlanModeState`]. Used as an offline fallback, in tests, and
/// as the in-process cache behind [`CloudPlanRepository`].
///
/// Does **not** persist `plan_step_runs` or `active_plan_id` — those are
/// cloud-only concepts. The corresponding trait methods are Ok no-ops so a
/// CLI running offline can still exercise the same codepaths.
#[derive(Debug, Clone, Default)]
pub struct LocalCachePlanRepository;

impl LocalCachePlanRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PlanRepository for LocalCachePlanRepository {
    async fn save(
        &self,
        plan_id: &str,
        state: &mut PlanModeState,
        expected_version: Option<u64>,
    ) -> Result<(), PlanLoadError> {
        if let Some(expected) = expected_version
            && state.version != expected
        {
            return Err(PlanLoadError::conflict(expected, state.version));
        }
        state
            .save_to_plans_dir_with_id(plan_id)
            .map_err(PlanLoadError::Internal)
    }

    async fn load(&self, plan_id: &str) -> Result<PlanModeState, PlanLoadError> {
        PlanModeState::load_from_plans_dir(plan_id)
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
        _filter: PlanListFilter<'_>,
    ) -> Result<Vec<SavedPlanInfo>, PlanLoadError> {
        Ok(PlanModeState::list_saved_plans_for_user(user_id))
    }

    async fn delete(&self, plan_id: &str) -> Result<(), PlanLoadError> {
        PlanModeState::delete_saved_plan(plan_id)
    }

    async fn set_active_plan(
        &self,
        _session_id: &str,
        _plan_id: Option<&str>,
    ) -> Result<(), PlanLoadError> {
        Ok(())
    }

    async fn active_plan_for_session(
        &self,
        _session_id: &str,
    ) -> Result<Option<String>, PlanLoadError> {
        Ok(None)
    }

    async fn record_step_run(&self, _input: NewStepRun<'_>) -> Result<String, PlanLoadError> {
        Ok(Uuid::new_v4().to_string())
    }

    async fn record_completed_step_run(
        &self,
        _input: NewStepRun<'_>,
        _error: Option<&str>,
        _artifact_ref: Option<&str>,
    ) -> Result<String, PlanLoadError> {
        Ok(Uuid::new_v4().to_string())
    }

    async fn finalize_step_run(
        &self,
        _plan_id: &str,
        _run_id: &str,
        _status: TaskStatus,
        _error: Option<&str>,
        _artifact_ref: Option<&str>,
    ) -> Result<(), PlanLoadError> {
        Ok(())
    }

    async fn list_step_runs(
        &self,
        _plan_id: &str,
        _subtask_id: Option<&str>,
        _limit: i32,
    ) -> Result<Vec<PlanStepRun>, PlanLoadError> {
        Ok(Vec::new())
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
    crate::plan_resume::plan_resume_system_prompt_section(&state)
}

// ─── Fork helper ─────────────────────────────────────────────────────────────

/// Duplicate a plan onto a new session.
///
/// Takes the parent plan's state, mints a new `plan_id` (derived from the
/// parent's goal so both are searchable), copies every subtask including
/// their completion status, pins the child session_hint, and atomically
/// points `agent_sessions.active_plan_id` at the fork for the child session.
///
/// The parent plan and its linkage are untouched — fork is non-destructive,
/// so the parent session can continue executing on its own copy.
///
/// `forked_at_subtask` is embedded in the new plan's timeline event so audit
/// tools can tell where the fork split, but is not used to prune subtasks;
/// callers that want to "restart from subtask X" should follow the fork with
/// a `rewind` call on the child.
///
/// Returns `Some(new_plan_id)` on success, `PlanLoadError::NotFound` if the
/// parent plan doesn't exist.
pub async fn fork_plan_for_session(
    repo: &dyn PlanRepository,
    parent_plan_id: &str,
    child_session_id: &str,
    forked_at_subtask: Option<&str>,
) -> Result<Option<String>, PlanLoadError> {
    let parent = repo.load(parent_plan_id).await?;

    // Mint a new plan_id that's distinct from the parent but still linkable.
    // Uses the parent's goal so `generate_plan_id` produces a human-readable
    // slug like `ship-feature-abcd` → `ship-feature-ef12`.
    let mut child = parent.clone();
    // Always append a short UUID suffix so successive forks from the same
    // parent never collide (with each other or with any unrelated plan that
    // happened to hash the same way via generate_plan_id). Keep the
    // goal-derived prefix so forked ids remain human-readable.
    let new_id = {
        let base = PlanModeState::generate_plan_id(&parent.goal);
        let suffix: String = Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(8)
            .collect();
        // Cap total length so MatrixOne's VARCHAR(64) can always hold it.
        let prefix_len = base.len().min(55);
        format!("{}-{}", &base[..prefix_len], suffix)
    };
    child.version = 0; // fresh row → save will bump to 1
    child.session_hint = Some(child_session_id.to_string());
    child
        .timeline
        .record(crate::decompose::TimelineEventKind::PlanCreated {
            subtask_count: child.plan.subtasks.len(),
        });
    // Record the fork point so audit trails can follow the lineage. Uses the
    // existing Replan event kind with a descriptive reason — keeps the schema
    // changes small and slots into existing UI rendering.
    child
        .timeline
        .record(crate::decompose::TimelineEventKind::Replan {
            reason: format!(
                "forked from plan {parent_plan_id}{}",
                forked_at_subtask
                    .map(|st| format!(" at subtask {st}"))
                    .unwrap_or_default()
            ),
            changes: "fork".to_string(),
        });

    repo.save(&new_id, &mut child, None).await?;
    repo.set_active_plan(child_session_id, Some(&new_id))
        .await?;
    Ok(Some(new_id))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompose::ProjectContext;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Global lock for tests that mutate `HOME`. `cargo test` runs these in
    /// parallel by default, so without serialization two tests can stamp on
    /// each other's plan directory and observe stale rows from a sibling.
    fn home_guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn temp_plans_dir() -> (tempfile::TempDir, MutexGuard<'static, ()>) {
        let guard = home_guard();
        let dir = tempfile::TempDir::new().unwrap();
        // SAFETY: home_guard() serializes access across tests so only one
        // holder at a time is writing HOME. Matches the pattern used by
        // existing filesystem tests in decompose.rs.
        unsafe {
            std::env::set_var("HOME", dir.path());
        }
        (dir, guard)
    }

    #[tokio::test]
    async fn local_cache_save_and_load_roundtrip() {
        let _guard = temp_plans_dir();
        // _guard holds both TempDir + MutexGuard for the duration of this test.
        let repo = LocalCachePlanRepository::new();
        let mut state = PlanModeState::new_with_owner(
            "test goal".into(),
            ProjectContext::default(),
            "u-1".into(),
        );
        repo.save("plan-1", &mut state, None).await.unwrap();

        let loaded = repo.load("plan-1").await.unwrap();
        assert_eq!(loaded.goal, "test goal");
        assert_eq!(loaded.created_by.as_deref(), Some("u-1"));
    }

    #[tokio::test]
    async fn local_cache_load_owned_returns_not_found_for_wrong_user() {
        let _guard = temp_plans_dir();
        // _guard holds both TempDir + MutexGuard for the duration of this test.
        let repo = LocalCachePlanRepository::new();
        let mut state =
            PlanModeState::new_with_owner("goal".into(), ProjectContext::default(), "u-1".into());
        repo.save("plan-2", &mut state, None).await.unwrap();

        let err = repo.load_owned("plan-2", "u-other").await.unwrap_err();
        assert!(matches!(err, PlanLoadError::NotFound(_)));
    }

    #[tokio::test]
    async fn local_cache_step_run_methods_are_noops() {
        let repo = LocalCachePlanRepository::new();
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
        assert!(!run_id.is_empty());

        // finalize on a noop run just returns Ok.
        repo.finalize_step_run("p", &run_id, TaskStatus::Completed, None, None)
            .await
            .unwrap();

        let runs = repo.list_step_runs("p", None, 10).await.unwrap();
        assert!(runs.is_empty());
    }
}
