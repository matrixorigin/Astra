//! Plan repository — storage abstraction over `plans`.
//!
//! The repository is the cloud-authoritative boundary for plan state. Everything
//! that reads or writes a plan (HTTP handlers, CLI, sync) goes through a
//! `PlanRepository` implementation.
//!
//! # Implementations
//!
//! * [`CloudPlanRepository`] — SQLx backed by the `plans` MatrixOne table
//!   created in `astra-services::storage`. Source of truth.
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
//! * `plans.session_id` is a routing hint for plan authoring. Durable execution
//!   and attempt evidence belong to Work and Run, not a parallel plan audit.

use crate::state::PlanModeState;
use astra_core::matrixone_statement_with_null_shape;
use async_trait::async_trait;
use sqlx::{MySql, Pool, Row};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

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

/// Repository over plan authoring state and active session bindings.
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

    /// Delete a plan and clear its active session bindings.
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
                let insert_sql = matrixone_statement_with_null_shape(
                    "INSERT INTO plans \
                         (plan_id, user_id, session_id, goal, phase, version, plan_json, plan_md, \
                          progress_pct, subtask_count, created_by, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
                    [state.session_hint.is_some(), state.plan_md.is_some()],
                );
                let res = sqlx::query(&insert_sql)
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
        } else {
            // Clear the routing hint of the exact plan this session is
            // leaving. `plans.session_id` is a denormalized discovery index;
            // retaining it after approval makes a completed authoring plan
            // look active again on the next resume.
            let active: Option<(Option<String>,)> = sqlx::query_as(
                "SELECT active_plan_id FROM agent_sessions WHERE user_id = ? AND session_id = ?",
            )
            .bind(user_id)
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?;
            if let Some(active_plan_id) = active.and_then(|(id,)| id) {
                sqlx::query(
                    "UPDATE plans SET session_id = NULL, updated_at = NOW(6) \
                     WHERE user_id = ? AND plan_id = ? AND session_id = ?",
                )
                .bind(user_id)
                .bind(active_plan_id)
                .bind(session_id)
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx)?;
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
}

// ─── In-memory implementation ────────────────────────────────────────────────

#[derive(Debug, Default)]
struct InMemoryPlanRepositoryState {
    plans: HashMap<(String, String), PlanModeState>,
    active_plans: HashMap<(String, String), String>,
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
        Ok(())
    }

    async fn set_active_plan(
        &self,
        user_id: &str,
        session_id: &str,
        plan_id: Option<&str>,
    ) -> Result<(), PlanLoadError> {
        let mut guard = astra_core::sync_poison::recover_rwlock_write(&self.inner);
        if let Some(plan_id) = plan_id {
            validate_plan_id(plan_id)?;
            if !guard
                .plans
                .contains_key(&(user_id.to_string(), plan_id.to_string()))
            {
                return Err(PlanLoadError::NotFound(plan_id.to_string()));
            }
        }

        let previous = guard
            .active_plans
            .remove(&(user_id.to_string(), session_id.to_string()));
        if plan_id.is_none()
            && let Some(previous) = previous
            && let Some(state) = guard.plans.get_mut(&(user_id.to_string(), previous))
            && state.session_hint.as_deref() == Some(session_id)
        {
            state.session_hint = None;
        }
        if let Some(plan_id) = plan_id {
            guard
                .active_plans
                .retain(|(uid, _), active| uid != user_id || active != plan_id);
            guard.active_plans.insert(
                (user_id.to_string(), session_id.to_string()),
                plan_id.to_string(),
            );
            if let Some(state) = guard
                .plans
                .get_mut(&(user_id.to_string(), plan_id.to_string()))
            {
                state.session_hint = Some(session_id.to_string());
            }
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
    async fn stale_save_preserves_current_plan_and_candidate_version() {
        let repo = InMemoryPlanRepository::new();
        let mut current = PlanModeState::new_with_owner("original".into(), "owner".into());
        repo.save("owner", "plan", &mut current, None)
            .await
            .unwrap();
        let mut stale = current.clone();
        let expected = current.version;
        current.goal = "accepted revision".into();
        repo.save("owner", "plan", &mut current, Some(expected))
            .await
            .unwrap();
        stale.goal = "stale revision".into();
        assert!(
            matches!(repo.save("owner", "plan", &mut stale, Some(expected)).await,
            Err(PlanLoadError::Conflict { expected: e, actual }) if e == expected && actual == current.version)
        );
        assert_eq!(stale.version, expected);
        let persisted = repo.load("owner", "plan").await.unwrap();
        assert_eq!(persisted.goal, current.goal);
        assert_eq!(persisted.version, current.version);
    }

    #[tokio::test]
    async fn clearing_active_plan_removes_session_discovery_hint() {
        let repo = InMemoryPlanRepository::new();
        let mut state = PlanModeState::new_with_owner("review plan".into(), "u-1".into());
        repo.save("u-1", "plan-review", &mut state, None)
            .await
            .unwrap();
        repo.set_active_plan("u-1", "session-1", Some("plan-review"))
            .await
            .unwrap();

        assert_eq!(
            repo.list_for_user(
                "u-1",
                PlanListFilter {
                    session_id: Some("session-1"),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .len(),
            1
        );

        repo.set_active_plan("u-1", "session-1", None)
            .await
            .unwrap();

        assert!(
            repo.list_for_user(
                "u-1",
                PlanListFilter {
                    session_id: Some("session-1"),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .is_empty(),
            "an approved plan must not be rediscovered as active through its stale session hint"
        );
    }

    #[tokio::test]
    async fn failed_active_plan_switch_preserves_previous_binding() {
        let repo = InMemoryPlanRepository::new();
        let mut state = PlanModeState::new_with_owner("current plan".into(), "u-1".into());
        repo.save("u-1", "current-plan", &mut state, None)
            .await
            .unwrap();
        repo.set_active_plan("u-1", "session-1", Some("current-plan"))
            .await
            .unwrap();

        assert!(matches!(
            repo.set_active_plan("u-1", "session-1", Some("missing-plan"))
                .await,
            Err(PlanLoadError::NotFound(_))
        ));
        assert_eq!(
            repo.active_plan_for_session("u-1", "session-1")
                .await
                .unwrap()
                .as_deref(),
            Some("current-plan"),
            "a failed switch must be atomic"
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
}
