//! Task lease service: transactional lease-based task claiming.
//!
//! Uses transactional lease rows to atomically claim tasks across concurrent
//! pods. Includes the hold cache for lease-aware pack sync and the task pack
//! import/export helpers.
//!
//! Split from the monolithic `multi_agent.rs`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use super::hold_cache::TaskLeaseHoldCache;
use super::metrics::SharedMultiAgentMetrics;
use crate::task_orchestrator::{
    AGENT_TASK_DETAIL_SELECT_COLUMNS, MatrixOneTaskService, TaskListItem, TaskRecord, TaskStatus,
};

/// Default maximum number of tasks to return in a pack pull.
pub const DEFAULT_TASKS_PACK_LIMIT: u32 = 2000;

/// Max candidate tasks to scan when claiming the next lease.
/// Balances database load against starvation risk: if all candidates in
/// one batch are contested by concurrent pods, the caller retries in the
/// next poll cycle.
pub const CLAIM_CANDIDATE_SCAN_LIMIT: u32 = 100;

// ─── Task pack sync (MatrixOne) ──────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TasksPackPushResult {
    pub applied: u32,
    pub rejected: u32,
}

pub async fn pull_tasks_pack_mysql(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
) -> Result<String, String> {
    pull_tasks_pack_mysql_with_limit(pool, user_id, DEFAULT_TASKS_PACK_LIMIT).await
}

/// Pull tasks pack with a configurable limit.
pub async fn pull_tasks_pack_mysql_with_limit(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    limit: u32,
) -> Result<String, String> {
    let q = format!(
        "SELECT {AGENT_TASK_DETAIL_SELECT_COLUMNS} FROM agent_tasks WHERE user_id = ? ORDER BY updated_at DESC LIMIT {limit}"
    );
    let rows = sqlx::query(&q)
        .bind(user_id)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("pull_tasks_pack: {e}"))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(MatrixOneTaskService::parse_mysql_row(&row)?);
    }
    serde_json::to_string(&out).map_err(|e| format!("pull_tasks_pack json: {e}"))
}

pub async fn push_tasks_pack_held_mysql(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    holder_agent_id: &str,
    pack_json: &str,
) -> Result<TasksPackPushResult, String> {
    let tasks: Vec<TaskRecord> =
        serde_json::from_str(pack_json).map_err(|e| format!("push_tasks_pack parse: {e}"))?;
    let mut applied = 0u32;
    let mut rejected = 0u32;
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("push_tasks_pack tx begin: {e}"))?;

    for t in tasks {
        if t.user_id != user_id {
            rejected += 1;
            continue;
        }
        let plan_json = t
            .plan
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| format!("push_tasks_pack plan_json: {e}"))?;
        let ckpt_json = t
            .checkpoint
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| format!("push_tasks_pack checkpoint_json: {e}"))?;

        // Lock task_leases before agent_tasks to match claim/release ordering
        // and prevent a stale holder from updating after lease transfer.
        let holder: Option<String> = sqlx::query_scalar(
            "SELECT holder_agent_id FROM task_leases \
             WHERE task_id = ? AND user_id = ? AND expires_at >= NOW(6) \
             FOR UPDATE",
        )
        .bind(&t.task_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| format!("push_tasks_pack lease: {e}"))?;
        match holder {
            Some(h) if h == holder_agent_id => {}
            _ => {
                rejected += 1;
                continue;
            }
        }

        sqlx::query("SELECT task_id FROM agent_tasks WHERE task_id = ? AND user_id = ? FOR UPDATE")
            .bind(&t.task_id)
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| format!("push_tasks_pack task lock: {e}"))?;

        let n = sqlx::query(
            "UPDATE agent_tasks SET \
             session_id = ?, parent_task_id = ?, title = ?, description = ?, status = ?, \
             progress_pct = ?, items_done = ?, items_total = ?, plan_json = ?, checkpoint_json = ?, \
             error_message = ?, user_rating = ?, completion_time_sec = ?, replan_count = ?, \
             auto_adjustments = ?, outcome = ?, project_type = ?, goal_pattern = ?, \
             agent_id = ?, updated_at = NOW(6) \
             WHERE task_id = ? AND user_id = ?",
        )
        .bind(&t.session_id)
        .bind(&t.parent_task_id)
        .bind(&t.title)
        .bind(&t.description)
        .bind(t.status.as_str())
        .bind(t.progress_pct as i32)
        .bind(t.items_done as i32)
        .bind(t.items_total as i32)
        .bind(&plan_json)
        .bind(&ckpt_json)
        .bind(&t.error_message)
        .bind(t.user_rating.map(|r| r as i8))
        .bind(t.completion_time_sec)
        .bind(t.replan_count as i32)
        .bind(t.auto_adjustments as i32)
        .bind(t.outcome.map(|o| o.as_str().to_string()))
        .bind(&t.project_type)
        .bind(&t.goal_pattern)
        .bind(t.agent_id.as_deref().unwrap_or(holder_agent_id))
        .bind(&t.task_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("push_tasks_pack update: {e}"))?
        .rows_affected();

        if n > 0 {
            applied += 1;
        } else {
            rejected += 1;
        }
    }

    tx.commit()
        .await
        .map_err(|e| format!("push_tasks_pack commit: {e}"))?;

    Ok(TasksPackPushResult { applied, rejected })
}

// ─── Edge registry ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskLeaseView {
    pub task_id: String,
    pub holder_agent_id: String,
    pub holder_edge_id: Option<String>,
    pub expires_at: String,
    pub lease_version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LeaseClaimResult {
    Granted {
        lease_version: i64,
        expires_at: String,
    },
    Contested {
        holder_agent_id: String,
        expires_at: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum NextClaimableLeaseClaimResult {
    Granted {
        task_id: String,
        lease_version: i64,
        expires_at: String,
    },
    NoClaimableTasks,
    AllClaimableTasksLeased,
}

pub(crate) fn clamp_ttl_sec(ttl_sec: i64) -> i64 {
    ttl_sec.clamp(30, 86_400)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaimedTaskLease {
    task_id: String,
    lease_version: i64,
    expires_at: String,
}

fn task_status_is_claimable(status: &str) -> bool {
    TaskStatus::parse_status(status).is_some_and(|status| status.is_claimable())
}

const CLAIMABLE_TASK_STATUS_SQL: &str = "t.status IN ('pending', 'in_progress')";
const CLAIMABILITY_SELECT_SQL: &str = "CASE WHEN t.status = 'pending' THEN 'pending' ELSE 'recoverable_in_progress' END AS claimability";

fn classify_empty_claimable_result(has_unfinished_tasks: bool) -> NextClaimableLeaseClaimResult {
    if has_unfinished_tasks {
        NextClaimableLeaseClaimResult::AllClaimableTasksLeased
    } else {
        NextClaimableLeaseClaimResult::NoClaimableTasks
    }
}

pub(crate) async fn list_claimable_tasks_mysql(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    limit: usize,
) -> Result<Vec<TaskListItem>, String> {
    let sql_limit = limit.max(1);
    let rows = sqlx::query(&format!(
        "SELECT t.task_id, t.user_id, t.session_id, t.parent_task_id, t.title, \
                NULL AS description, t.status, t.progress_pct, t.items_done, t.items_total, \
                NULL AS plan_json, NULL AS checkpoint_json, t.error_message, \
                t.user_rating, t.completion_time_sec, t.replan_count, t.auto_adjustments, \
                t.outcome, t.project_type, t.goal_pattern, t.agent_id, \
                {CLAIMABILITY_SELECT_SQL}, \
                CAST(t.created_at AS CHAR) AS created_at, \
                CAST(t.updated_at AS CHAR) AS updated_at, \
                t.completed_at \
         FROM agent_tasks t \
         LEFT JOIN task_leases l \
           ON l.task_id = t.task_id AND l.user_id = t.user_id AND l.expires_at >= NOW(6) \
         WHERE t.user_id = ? \
           AND {CLAIMABLE_TASK_STATUS_SQL} \
           AND l.task_id IS NULL \
         ORDER BY created_at ASC LIMIT {}",
        sql_limit
    ))
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("list_claimable_tasks_for_worker: {e}"))?;

    rows.iter()
        .map(MatrixOneTaskService::parse_mysql_list_row)
        .collect()
}

async fn persist_granted_lease_locked(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    task_id: &str,
    agent_id: &str,
    edge_id: &str,
    ttl_sec: i64,
    has_existing_lease_row: bool,
) -> Result<ClaimedTaskLease, String> {
    if has_existing_lease_row {
        sqlx::query(
            "UPDATE task_leases SET \
             holder_agent_id = ?, holder_edge_id = ?, \
             expires_at = DATE_ADD(NOW(6), INTERVAL ? SECOND), \
             lease_version = lease_version + 1, updated_at = NOW(6) \
             WHERE task_id = ? AND user_id = ?",
        )
        .bind(agent_id)
        .bind(edge_id)
        .bind(ttl_sec)
        .bind(task_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("lease update: {e}"))?;
    } else {
        sqlx::query(
            "INSERT INTO task_leases \
             (task_id, user_id, holder_agent_id, holder_edge_id, expires_at, lease_version, created_at, updated_at) \
             VALUES (?, ?, ?, ?, DATE_ADD(NOW(6), INTERVAL ? SECOND), 1, NOW(6), NOW(6))",
        )
        .bind(task_id)
        .bind(user_id)
        .bind(agent_id)
        .bind(edge_id)
        .bind(ttl_sec)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("lease insert: {e}"))?;
    }

    sqlx::query("UPDATE agent_tasks SET agent_id = ?, updated_at = NOW(6) WHERE task_id = ?")
        .bind(agent_id)
        .bind(task_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("agent_tasks agent_id: {e}"))?;

    let ver: i64 = sqlx::query_scalar("SELECT lease_version FROM task_leases WHERE task_id = ?")
        .bind(task_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| format!("lease version read: {e}"))?;

    let exp: String =
        sqlx::query_scalar("SELECT CAST(expires_at AS CHAR) FROM task_leases WHERE task_id = ?")
            .bind(task_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(|e| format!("lease exp read: {e}"))?;

    Ok(ClaimedTaskLease {
        task_id: task_id.to_string(),
        lease_version: ver,
        expires_at: exp,
    })
}

pub struct DatabaseTaskLeaseService {
    pool: sqlx::Pool<sqlx::MySql>,
    hold_cache: std::sync::Arc<TaskLeaseHoldCache>,
    metrics: Option<SharedMultiAgentMetrics>,
}

impl DatabaseTaskLeaseService {
    pub fn new(
        pool: sqlx::Pool<sqlx::MySql>,
        hold_cache: std::sync::Arc<TaskLeaseHoldCache>,
    ) -> Self {
        Self {
            pool,
            hold_cache,
            metrics: None,
        }
    }

    pub fn from_shared(
        shared: &astra_core::SharedPool,
        hold_cache: std::sync::Arc<TaskLeaseHoldCache>,
    ) -> Self {
        Self {
            pool: shared.get().clone(),
            hold_cache,
            metrics: None,
        }
    }

    pub fn with_metrics(mut self, metrics: SharedMultiAgentMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    async fn renew_lease_update(
        &self,
        user_id: &str,
        task_id: &str,
        agent_id: &str,
        edge_id: &str,
        ttl_sec: i64,
    ) -> Result<bool, String> {
        let ttl = clamp_ttl_sec(ttl_sec);
        let n = match sqlx::query(
            "UPDATE task_leases SET \
             holder_edge_id = ?, \
             expires_at = DATE_ADD(NOW(6), INTERVAL ? SECOND), \
             lease_version = lease_version + 1, updated_at = NOW(6) \
             WHERE task_id = ? AND user_id = ? AND holder_agent_id = ? AND expires_at >= NOW(6)",
        )
        .bind(edge_id)
        .bind(ttl)
        .bind(task_id)
        .bind(user_id)
        .bind(agent_id)
        .execute(&self.pool)
        .await
        {
            Ok(done) => done.rows_affected(),
            Err(e) => {
                if let Some(ref m) = self.metrics {
                    m.lease_renewal_failure_total
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                return Err(format!("renew_lease: {e}"));
            }
        };

        if n == 0 {
            if let Some(ref m) = self.metrics {
                m.lease_renewal_failure_total
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            tracing::debug!("task_lease: renew skipped (lease expired or not held)");
            return Ok(false);
        }
        if let Some(ref m) = self.metrics {
            m.lease_renewal_success_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(true)
    }
}

#[async_trait]
pub trait TaskLeaseService: Send + Sync {
    async fn claim_next_claimable_lease(
        &self,
        user_id: &str,
        agent_id: &str,
        edge_id: &str,
        ttl_sec: i64,
    ) -> Result<NextClaimableLeaseClaimResult, String>;

    async fn try_claim_lease(
        &self,
        user_id: &str,
        task_id: &str,
        agent_id: &str,
        edge_id: &str,
        ttl_sec: i64,
    ) -> Result<LeaseClaimResult, String>;

    async fn release_lease(
        &self,
        user_id: &str,
        task_id: &str,
        agent_id: &str,
    ) -> Result<bool, String>;

    async fn get_lease(
        &self,
        user_id: &str,
        task_id: &str,
    ) -> Result<Option<TaskLeaseView>, String>;

    async fn renew_lease(
        &self,
        user_id: &str,
        task_id: &str,
        agent_id: &str,
        edge_id: &str,
        ttl_sec: i64,
    ) -> Result<Option<TaskLeaseView>, String>;

    async fn renew_lease_held(
        &self,
        user_id: &str,
        task_id: &str,
        agent_id: &str,
        edge_id: &str,
        ttl_sec: i64,
    ) -> Result<bool, String> {
        self.renew_lease(user_id, task_id, agent_id, edge_id, ttl_sec)
            .await
            .map(|view| view.is_some())
    }
}

#[async_trait]
impl TaskLeaseService for DatabaseTaskLeaseService {
    async fn claim_next_claimable_lease(
        &self,
        user_id: &str,
        agent_id: &str,
        edge_id: &str,
        ttl_sec: i64,
    ) -> Result<NextClaimableLeaseClaimResult, String> {
        let start = std::time::Instant::now();
        let candidate_task_ids = sqlx::query_scalar::<_, String>(&format!(
            "SELECT t.task_id \
             FROM agent_tasks t \
             LEFT JOIN task_leases l \
               ON l.task_id = t.task_id AND l.user_id = t.user_id \
             WHERE t.user_id = ? \
               AND {CLAIMABLE_TASK_STATUS_SQL} \
               AND (l.task_id IS NULL OR l.expires_at < NOW(6)) \
             ORDER BY t.created_at ASC, t.task_id ASC \
             LIMIT {CLAIM_CANDIDATE_SCAN_LIMIT}"
        ))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("claim_next_claimable_lease select candidates: {e}"))?;

        let mut first_candidate_error = None;
        for task_id in candidate_task_ids {
            match self
                .try_claim_lease(user_id, &task_id, agent_id, edge_id, ttl_sec)
                .await
            {
                Ok(LeaseClaimResult::Granted {
                    lease_version,
                    expires_at,
                }) => {
                    if let Some(ref m) = self.metrics {
                        m.lease_claim_latency.record(start.elapsed());
                    }
                    return Ok(NextClaimableLeaseClaimResult::Granted {
                        task_id,
                        lease_version,
                        expires_at,
                    });
                }
                Ok(LeaseClaimResult::Contested { .. }) => continue,
                Err(error) => {
                    tracing::warn!(
                        %task_id,
                        %error,
                        "claim_next_claimable_lease candidate claim failed; trying next candidate"
                    );
                    if first_candidate_error.is_none() {
                        first_candidate_error = Some(format!("{task_id}: {error}"));
                    }
                    continue;
                }
            }
        }
        let has_unfinished_tasks: Option<i8> = sqlx::query_scalar(&format!(
            "SELECT 1 FROM agent_tasks t \
             WHERE t.user_id = ? AND {CLAIMABLE_TASK_STATUS_SQL} \
             LIMIT 1"
        ))
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("claim_next_pending_lease unfinished probe: {e}"))?;

        if let (Some(error), Some(_)) = (first_candidate_error, has_unfinished_tasks) {
            return Err(format!(
                "claim_next_claimable_lease all candidates failed; first failure: {error}"
            ));
        }

        Ok(classify_empty_claimable_result(
            has_unfinished_tasks.is_some(),
        ))
    }

    #[tracing::instrument(skip(self), fields(
        user_id = %user_id,
        task_id = %task_id,
        agent_id = %agent_id,
        ttl_sec
    ))]
    async fn try_claim_lease(
        &self,
        user_id: &str,
        task_id: &str,
        agent_id: &str,
        edge_id: &str,
        ttl_sec: i64,
    ) -> Result<LeaseClaimResult, String> {
        let _start = std::time::Instant::now();
        let ttl = clamp_ttl_sec(ttl_sec);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| format!("lease tx begin: {e}"))?;

        // Lock task_leases FIRST — ensures release_lease (which locks
        // task_leases) wins the race: if release has the lock, claim
        // blocks until after the lease is deleted, then sees None → Granted.
        let lease_row = sqlx::query(
            "SELECT holder_agent_id, CAST(expires_at AS CHAR) AS expires_at, \
             CASE WHEN expires_at >= NOW(6) THEN 1 ELSE 0 END AS is_active \
             FROM task_leases WHERE task_id = ? AND user_id = ? FOR UPDATE",
        )
        .bind(task_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| format!("lease select: {e}"))?;

        let task_row =
            sqlx::query("SELECT user_id, status FROM agent_tasks WHERE task_id = ? FOR UPDATE")
                .bind(task_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| format!("lease task lock: {e}"))?;

        let Some(task_row) = task_row else {
            return Err("task not found".to_string());
        };
        let owner_uid: String = task_row
            .try_get("user_id")
            .map_err(|e| format!("lease task owner: {e}"))?;
        if owner_uid != user_id {
            return Err("task not owned by user".to_string());
        }
        let task_status: String = task_row
            .try_get("status")
            .map_err(|e| format!("lease task status: {e}"))?;
        if !task_status_is_claimable(&task_status) {
            return Err(format!("task is not claimable from status '{task_status}'"));
        }
        let has_existing_lease_row = lease_row.is_some();

        if let Some(ref r) = lease_row {
            let holder: String = r.try_get("holder_agent_id").map_err(|e| e.to_string())?;
            let exp: String = r.try_get("expires_at").map_err(|e| e.to_string())?;
            let active: i8 = r.try_get("is_active").unwrap_or(0);
            if active != 0 && holder != agent_id {
                // Read-only path: no changes to persist — rollback to
                // release the pool connection cleanly.
                tracing::warn!(
                    %holder,
                    %exp,
                    "task_lease: contested — held by another agent"
                );
                let _ = tx.rollback().await;
                return Ok(LeaseClaimResult::Contested {
                    holder_agent_id: holder,
                    expires_at: exp,
                });
            }
        }

        let claimed = persist_granted_lease_locked(
            &mut tx,
            user_id,
            task_id,
            agent_id,
            edge_id,
            ttl,
            has_existing_lease_row,
        )
        .await?;

        tx.commit()
            .await
            .map_err(|e| format!("lease commit: {e}"))?;

        self.hold_cache.record_hold(agent_id, task_id);

        if let Some(ref m) = self.metrics {
            m.lease_claim_latency.record(_start.elapsed());
        }

        Ok(LeaseClaimResult::Granted {
            lease_version: claimed.lease_version,
            expires_at: claimed.expires_at,
        })
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id, task_id = %task_id, agent_id = %agent_id))]
    async fn release_lease(
        &self,
        user_id: &str,
        task_id: &str,
        agent_id: &str,
    ) -> Result<bool, String> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| format!("lease release tx begin: {e}"))?;

        // Lock the lease row first to prevent concurrent acquire/release races.
        // SELECT FOR UPDATE ensures no other transaction can claim or release
        // this lease until we commit.
        let existing = sqlx::query(
            "SELECT holder_agent_id FROM task_leases \
             WHERE task_id = ? AND user_id = ? AND holder_agent_id = ? \
             FOR UPDATE",
        )
        .bind(task_id)
        .bind(user_id)
        .bind(agent_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| format!("lease release lock: {e}"))?;

        if existing.is_none() {
            tracing::warn!("task_lease: release on lease not held by agent");
            let _ = tx.rollback().await;
            return Ok(false);
        }

        // UPDATE agent_tasks first: clear agent_id BEFORE deleting the lease.
        // This prevents any race where another pod sees the lease deleted
        // but agent_id still set — the agent_id is NULL before the lease disappears.
        sqlx::query(
            "UPDATE agent_tasks SET agent_id = NULL, updated_at = NOW(6) \
             WHERE task_id = ? AND user_id = ? AND agent_id = ?",
        )
        .bind(task_id)
        .bind(user_id)
        .bind(agent_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("clear agent_id after lease release: {e}"))?;

        sqlx::query(
            "DELETE FROM task_leases WHERE task_id = ? AND user_id = ? AND holder_agent_id = ?",
        )
        .bind(task_id)
        .bind(user_id)
        .bind(agent_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("lease delete: {e}"))?;

        tx.commit()
            .await
            .map_err(|e| format!("lease release commit: {e}"))?;

        self.hold_cache.release_hold(agent_id, task_id);
        Ok(true)
    }

    async fn get_lease(
        &self,
        user_id: &str,
        task_id: &str,
    ) -> Result<Option<TaskLeaseView>, String> {
        let row = sqlx::query(
            "SELECT task_id, holder_agent_id, holder_edge_id, \
             CAST(expires_at AS CHAR) AS expires_at, lease_version \
             FROM task_leases WHERE task_id = ? AND user_id = ?",
        )
        .bind(task_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("get_lease: {e}"))?;

        let Some(r) = row else {
            return Ok(None);
        };
        Ok(Some(TaskLeaseView {
            task_id: r.try_get("task_id").map_err(|e| e.to_string())?,
            holder_agent_id: r.try_get("holder_agent_id").map_err(|e| e.to_string())?,
            holder_edge_id: r.try_get("holder_edge_id").ok().flatten(),
            expires_at: r.try_get("expires_at").map_err(|e| e.to_string())?,
            lease_version: r.try_get("lease_version").map_err(|e| e.to_string())?,
        }))
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id, task_id = %task_id, agent_id = %agent_id, ttl_sec))]
    async fn renew_lease(
        &self,
        user_id: &str,
        task_id: &str,
        agent_id: &str,
        edge_id: &str,
        ttl_sec: i64,
    ) -> Result<Option<TaskLeaseView>, String> {
        if !self
            .renew_lease_update(user_id, task_id, agent_id, edge_id, ttl_sec)
            .await?
        {
            return Ok(None);
        }
        self.get_lease(user_id, task_id).await
    }

    async fn renew_lease_held(
        &self,
        user_id: &str,
        task_id: &str,
        agent_id: &str,
        edge_id: &str,
        ttl_sec: i64,
    ) -> Result<bool, String> {
        self.renew_lease_update(user_id, task_id, agent_id, edge_id, ttl_sec)
            .await
    }
}

pub struct UnconfiguredTaskLeaseService;

#[async_trait]
impl TaskLeaseService for UnconfiguredTaskLeaseService {
    async fn claim_next_claimable_lease(
        &self,
        _user_id: &str,
        _agent_id: &str,
        _edge_id: &str,
        _ttl_sec: i64,
    ) -> Result<NextClaimableLeaseClaimResult, String> {
        Err("task lease service not configured".to_string())
    }

    async fn try_claim_lease(
        &self,
        _user_id: &str,
        _task_id: &str,
        _agent_id: &str,
        _edge_id: &str,
        _ttl_sec: i64,
    ) -> Result<LeaseClaimResult, String> {
        Err("task lease service not configured".to_string())
    }

    async fn release_lease(
        &self,
        _user_id: &str,
        _task_id: &str,
        _agent_id: &str,
    ) -> Result<bool, String> {
        Err("task lease service not configured".to_string())
    }

    async fn get_lease(
        &self,
        _user_id: &str,
        _task_id: &str,
    ) -> Result<Option<TaskLeaseView>, String> {
        Err("task lease service not configured".to_string())
    }

    async fn renew_lease(
        &self,
        _user_id: &str,
        _task_id: &str,
        _agent_id: &str,
        _edge_id: &str,
        _ttl_sec: i64,
    ) -> Result<Option<TaskLeaseView>, String> {
        Err("task lease service not configured".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::super::edge_registry::{EdgeRegistryService, UnconfiguredEdgeRegistryService};
    use super::*;

    #[test]
    fn task_lease_hold_cache_records_and_releases() {
        let c = TaskLeaseHoldCache::default();
        c.record_hold("a1", "t1");
        c.record_hold("a1", "t2");
        let s = c.held_task_ids_for_agent("a1");
        assert!(s.contains("t1") && s.contains("t2"));
        c.release_hold("a1", "t1");
        let s2 = c.held_task_ids_for_agent("a1");
        assert!(!s2.contains("t1") && s2.contains("t2"));
    }

    #[test]
    fn clamp_ttl_sec_bounds() {
        assert_eq!(clamp_ttl_sec(10), 30);
        assert_eq!(clamp_ttl_sec(60), 60);
        assert_eq!(clamp_ttl_sec(200_000), 86_400);
    }

    #[tokio::test]
    async fn classify_empty_claimable_result_distinguishes_empty_queue_from_leased_frontier() {
        assert_eq!(
            classify_empty_claimable_result(false),
            NextClaimableLeaseClaimResult::NoClaimableTasks
        );
        assert_eq!(
            classify_empty_claimable_result(true),
            NextClaimableLeaseClaimResult::AllClaimableTasksLeased
        );
    }

    #[test]
    fn task_status_is_claimable_accepts_only_unfinished_worker_states() {
        assert!(task_status_is_claimable("pending"));
        assert!(task_status_is_claimable("in_progress"));
        assert!(!task_status_is_claimable("paused"));
        assert!(!task_status_is_claimable("completed"));
        assert!(!task_status_is_claimable("failed"));
        assert!(!task_status_is_claimable("cancelled"));
    }

    #[tokio::test]
    async fn unconfigured_edge_registry_errors() {
        let s = UnconfiguredEdgeRegistryService;
        let r = s
            .register_or_update("u", "e1", "hdr", None, None, None)
            .await;
        assert!(r.is_err());
        let h = s.heartbeat("u", "e1", "hdr").await;
        assert!(h.is_err());
    }

    #[tokio::test]
    async fn unconfigured_task_lease_errors() {
        let s = UnconfiguredTaskLeaseService;
        assert!(
            s.claim_next_claimable_lease("u", "a", "e", 60)
                .await
                .is_err()
        );
        assert!(s.try_claim_lease("u", "t", "a", "e", 60).await.is_err());
    }
}
