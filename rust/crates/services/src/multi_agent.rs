//! Phase 3: edge agent registry, task leases (transaction + row lock), and task JSON packs for sync.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::task_orchestrator::{AGENT_TASK_SELECT_COLUMNS, MatrixOneTaskService, TaskRecord};

/// Default maximum number of tasks to return in a pack pull.
/// Can be overridden per-call using `pull_tasks_pack_mysql_with_limit`.
pub const DEFAULT_TASKS_PACK_LIMIT: u32 = 2000;

// ─── Hold cache (process-local hint for lease-aware TaskAdapter export) ───────

/// Best-effort map of which task IDs this process has successfully leased for each `agent_id`.
#[derive(Default)]
pub struct TaskLeaseHoldCache {
    inner: Mutex<HashMap<String, HashSet<String>>>,
}

impl TaskLeaseHoldCache {
    pub fn record_hold(&self, agent_id: &str, task_id: &str) {
        if let Ok(mut g) = self.inner.lock() {
            g.entry(agent_id.to_string())
                .or_default()
                .insert(task_id.to_string());
        }
    }

    pub fn release_hold(&self, agent_id: &str, task_id: &str) {
        if let Ok(mut g) = self.inner.lock()
            && let Some(set) = g.get_mut(agent_id)
        {
            set.remove(task_id);
        }
    }

    pub fn held_task_ids_for_agent(&self, agent_id: &str) -> HashSet<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.get(agent_id).cloned())
            .unwrap_or_default()
    }
}

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
        "SELECT {AGENT_TASK_SELECT_COLUMNS} FROM agent_tasks WHERE user_id = ? ORDER BY updated_at DESC LIMIT {limit}"
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

    for t in tasks {
        if t.user_id != user_id {
            rejected += 1;
            continue;
        }
        let holder: Option<String> = sqlx::query_scalar(
            "SELECT holder_agent_id FROM task_leases \
             WHERE task_id = ? AND user_id = ? AND expires_at > NOW(6)",
        )
        .bind(&t.task_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("push_tasks_pack lease: {e}"))?;
        match holder {
            Some(h) if h == holder_agent_id => {}
            _ => {
                rejected += 1;
                continue;
            }
        }

        let plan_json = t.plan.as_ref().and_then(|p| serde_json::to_string(p).ok());
        let ckpt_json = t
            .checkpoint
            .as_ref()
            .and_then(|c| serde_json::to_string(c).ok());

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
        .execute(pool)
        .await
        .map_err(|e| format!("push_tasks_pack update: {e}"))?
        .rows_affected();

        if n > 0 {
            applied += 1;
        } else {
            rejected += 1;
        }
    }

    Ok(TasksPackPushResult { applied, rejected })
}

// ─── Edge registry ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct EdgeAgentRecord {
    pub registry_id: String,
    pub user_id: String,
    pub edge_agent_id: String,
    pub edge_id: String,
    pub hostname: Option<String>,
    pub worktree_path: Option<String>,
    pub registered_at: String,
    pub last_heartbeat_at: String,
}

#[async_trait]
pub trait EdgeRegistryService: Send + Sync {
    async fn register_or_update(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id_header: &str,
        hostname: Option<&str>,
        worktree_path: Option<&str>,
        capabilities: Option<serde_json::Value>,
    ) -> Result<EdgeAgentRecord, String>;

    async fn heartbeat(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id_header: &str,
    ) -> Result<(), String>;
}

pub struct DatabaseEdgeRegistryService {
    pool: sqlx::Pool<sqlx::MySql>,
}

impl DatabaseEdgeRegistryService {
    pub fn new(pool: sqlx::Pool<sqlx::MySql>) -> Self {
        Self { pool }
    }

    pub fn from_shared(shared: &astra_core::SharedPool) -> Self {
        Self {
            pool: shared.get().clone(),
        }
    }
}

#[async_trait]
impl EdgeRegistryService for DatabaseEdgeRegistryService {
    async fn register_or_update(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id_header: &str,
        hostname: Option<&str>,
        worktree_path: Option<&str>,
        capabilities: Option<serde_json::Value>,
    ) -> Result<EdgeAgentRecord, String> {
        let cap_json = capabilities
            .map(|v| serde_json::to_string(&v))
            .transpose()
            .map_err(|e| format!("capabilities json: {e}"))?;

        let updated = sqlx::query(
            "UPDATE edge_agent_registry SET \
             edge_id = ?, hostname = ?, worktree_path = ?, capabilities_json = ?, last_heartbeat_at = NOW(6) \
             WHERE user_id = ? AND edge_agent_id = ?",
        )
        .bind(edge_id_header)
        .bind(hostname)
        .bind(worktree_path)
        .bind(&cap_json)
        .bind(user_id)
        .bind(edge_agent_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge_registry update: {e}"))?
        .rows_affected();

        let registry_id = if updated == 0 {
            let rid = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO edge_agent_registry \
                 (registry_id, user_id, edge_agent_id, edge_id, hostname, worktree_path, capabilities_json, registered_at, last_heartbeat_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
            )
            .bind(&rid)
            .bind(user_id)
            .bind(edge_agent_id)
            .bind(edge_id_header)
            .bind(hostname)
            .bind(worktree_path)
            .bind(&cap_json)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("edge_registry insert: {e}"))?;
            rid
        } else {
            let row = sqlx::query(
                "SELECT registry_id FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?",
            )
            .bind(user_id)
            .bind(edge_agent_id)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| format!("edge_registry re-read: {e}"))?;
            row.try_get::<String, _>("registry_id")
                .map_err(|e| format!("registry_id: {e}"))?
        };

        let row = sqlx::query(
            "SELECT registry_id, user_id, edge_agent_id, edge_id, hostname, worktree_path, \
             CAST(registered_at AS CHAR) AS registered_at, CAST(last_heartbeat_at AS CHAR) AS last_heartbeat_at \
             FROM edge_agent_registry WHERE registry_id = ?",
        )
        .bind(&registry_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| format!("edge_registry fetch: {e}"))?;

        Ok(EdgeAgentRecord {
            registry_id: row.try_get("registry_id").map_err(|e| e.to_string())?,
            user_id: row.try_get("user_id").map_err(|e| e.to_string())?,
            edge_agent_id: row.try_get("edge_agent_id").map_err(|e| e.to_string())?,
            edge_id: row.try_get("edge_id").map_err(|e| e.to_string())?,
            hostname: row.try_get("hostname").ok().flatten(),
            worktree_path: row.try_get("worktree_path").ok().flatten(),
            registered_at: row
                .try_get::<String, _>("registered_at")
                .unwrap_or_default(),
            last_heartbeat_at: row
                .try_get::<String, _>("last_heartbeat_at")
                .unwrap_or_default(),
        })
    }

    async fn heartbeat(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id_header: &str,
    ) -> Result<(), String> {
        let n = sqlx::query(
            "UPDATE edge_agent_registry SET edge_id = ?, last_heartbeat_at = NOW(6) \
             WHERE user_id = ? AND edge_agent_id = ?",
        )
        .bind(edge_id_header)
        .bind(user_id)
        .bind(edge_agent_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge heartbeat: {e}"))?
        .rows_affected();
        if n == 0 {
            return Err("edge agent not registered".to_string());
        }
        Ok(())
    }
}

pub struct UnconfiguredEdgeRegistryService;

#[async_trait]
impl EdgeRegistryService for UnconfiguredEdgeRegistryService {
    async fn register_or_update(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
        _edge_id_header: &str,
        _hostname: Option<&str>,
        _worktree_path: Option<&str>,
        _capabilities: Option<serde_json::Value>,
    ) -> Result<EdgeAgentRecord, String> {
        Err("edge registry service not configured".to_string())
    }

    async fn heartbeat(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
        _edge_id_header: &str,
    ) -> Result<(), String> {
        Err("edge registry service not configured".to_string())
    }
}

// ─── Task leases ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct TaskLeaseView {
    pub task_id: String,
    pub holder_agent_id: String,
    pub holder_edge_id: Option<String>,
    pub expires_at: String,
    pub lease_version: i64,
}

#[derive(Debug, Clone, Serialize)]
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

pub(crate) fn clamp_ttl_sec(ttl_sec: i64) -> i64 {
    ttl_sec.clamp(30, 86_400)
}

pub struct DatabaseTaskLeaseService {
    pool: sqlx::Pool<sqlx::MySql>,
    hold_cache: std::sync::Arc<TaskLeaseHoldCache>,
}

impl DatabaseTaskLeaseService {
    pub fn new(
        pool: sqlx::Pool<sqlx::MySql>,
        hold_cache: std::sync::Arc<TaskLeaseHoldCache>,
    ) -> Self {
        Self { pool, hold_cache }
    }

    pub fn from_shared(
        shared: &astra_core::SharedPool,
        hold_cache: std::sync::Arc<TaskLeaseHoldCache>,
    ) -> Self {
        Self {
            pool: shared.get().clone(),
            hold_cache,
        }
    }
}

#[async_trait]
pub trait TaskLeaseService: Send + Sync {
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
}

#[async_trait]
impl TaskLeaseService for DatabaseTaskLeaseService {
    async fn try_claim_lease(
        &self,
        user_id: &str,
        task_id: &str,
        agent_id: &str,
        edge_id: &str,
        ttl_sec: i64,
    ) -> Result<LeaseClaimResult, String> {
        let ttl = clamp_ttl_sec(ttl_sec);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| format!("lease tx begin: {e}"))?;

        let owner: Option<String> =
            sqlx::query_scalar("SELECT user_id FROM agent_tasks WHERE task_id = ? FOR UPDATE")
                .bind(task_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| format!("lease task lock: {e}"))?;

        let Some(owner_uid) = owner else {
            return Err("task not found".to_string());
        };
        if owner_uid != user_id {
            return Err("task not owned by user".to_string());
        }

        let lease_row = sqlx::query(
            "SELECT holder_agent_id, CAST(expires_at AS CHAR) AS expires_at, \
             (expires_at > NOW(6)) AS is_active \
             FROM task_leases WHERE task_id = ? AND user_id = ? FOR UPDATE",
        )
        .bind(task_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| format!("lease select: {e}"))?;

        if let Some(ref r) = lease_row {
            let holder: String = r.try_get("holder_agent_id").map_err(|e| e.to_string())?;
            let exp: String = r.try_get("expires_at").map_err(|e| e.to_string())?;
            let active: i8 = r.try_get("is_active").unwrap_or(0);
            if active != 0 && holder != agent_id {
                tx.commit().await.ok();
                return Ok(LeaseClaimResult::Contested {
                    holder_agent_id: holder,
                    expires_at: exp,
                });
            }
        }

        // Insert or update lease (same agent renews; expired or missing → claim)
        if lease_row.is_some() {
            sqlx::query(
                "UPDATE task_leases SET \
                 holder_agent_id = ?, holder_edge_id = ?, \
                 expires_at = DATE_ADD(NOW(6), INTERVAL ? SECOND), \
                 lease_version = lease_version + 1, updated_at = NOW(6) \
                 WHERE task_id = ? AND user_id = ?",
            )
            .bind(agent_id)
            .bind(edge_id)
            .bind(ttl)
            .bind(task_id)
            .bind(user_id)
            .execute(&mut *tx)
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
            .bind(ttl)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("lease insert: {e}"))?;
        }

        sqlx::query("UPDATE agent_tasks SET agent_id = ?, updated_at = NOW(6) WHERE task_id = ?")
            .bind(agent_id)
            .bind(task_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("agent_tasks agent_id: {e}"))?;

        let ver: i64 =
            sqlx::query_scalar("SELECT lease_version FROM task_leases WHERE task_id = ?")
                .bind(task_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| format!("lease version read: {e}"))?;

        let exp: String = sqlx::query_scalar(
            "SELECT CAST(expires_at AS CHAR) FROM task_leases WHERE task_id = ?",
        )
        .bind(task_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| format!("lease exp read: {e}"))?;

        tx.commit()
            .await
            .map_err(|e| format!("lease commit: {e}"))?;

        self.hold_cache.record_hold(agent_id, task_id);

        Ok(LeaseClaimResult::Granted {
            lease_version: ver,
            expires_at: exp,
        })
    }

    async fn release_lease(
        &self,
        user_id: &str,
        task_id: &str,
        agent_id: &str,
    ) -> Result<bool, String> {
        let n = sqlx::query(
            "DELETE FROM task_leases WHERE task_id = ? AND user_id = ? AND holder_agent_id = ?",
        )
        .bind(task_id)
        .bind(user_id)
        .bind(agent_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("lease release: {e}"))?
        .rows_affected();

        if n > 0 {
            let _ = sqlx::query(
                "UPDATE agent_tasks SET agent_id = NULL WHERE task_id = ? AND user_id = ? AND agent_id = ?",
            )
            .bind(task_id)
            .bind(user_id)
            .bind(agent_id)
            .execute(&self.pool)
            .await;
            self.hold_cache.release_hold(agent_id, task_id);
            Ok(true)
        } else {
            Ok(false)
        }
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

    async fn renew_lease(
        &self,
        user_id: &str,
        task_id: &str,
        agent_id: &str,
        edge_id: &str,
        ttl_sec: i64,
    ) -> Result<Option<TaskLeaseView>, String> {
        let ttl = clamp_ttl_sec(ttl_sec);
        let n = sqlx::query(
            "UPDATE task_leases SET \
             holder_edge_id = ?, \
             expires_at = DATE_ADD(NOW(6), INTERVAL ? SECOND), \
             lease_version = lease_version + 1, updated_at = NOW(6) \
             WHERE task_id = ? AND user_id = ? AND holder_agent_id = ? AND expires_at > NOW(6)",
        )
        .bind(edge_id)
        .bind(ttl)
        .bind(task_id)
        .bind(user_id)
        .bind(agent_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("renew_lease: {e}"))?
        .rows_affected();

        if n == 0 {
            return Ok(None);
        }
        self.get_lease(user_id, task_id).await
    }
}

pub struct UnconfiguredTaskLeaseService;

#[async_trait]
impl TaskLeaseService for UnconfiguredTaskLeaseService {
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
mod unit_tests {
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
        assert!(s.try_claim_lease("u", "t", "a", "e", 60).await.is_err());
    }
}
