//! Edge agent registry: register, list, and heartbeat edge agents.
//!
//! Split from the monolithic `multi_agent.rs`.

use std::sync::atomic::Ordering;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use super::metrics::SharedMultiAgentMetrics;

#[derive(Clone, Debug, Serialize, Deserialize)]
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

    /// List all registered edge agents for a user (for cross-pod dispatch routing).
    async fn list_by_user(&self, user_id: &str) -> Result<Vec<EdgeAgentRecord>, String>;

    /// Remove an edge agent from the registry (on disconnect).
    async fn unregister(&self, user_id: &str, edge_agent_id: &str) -> Result<(), String>;
}

pub struct DatabaseEdgeRegistryService {
    pool: sqlx::Pool<sqlx::MySql>,
    metrics: Option<SharedMultiAgentMetrics>,
}

impl DatabaseEdgeRegistryService {
    pub fn new(pool: sqlx::Pool<sqlx::MySql>) -> Self {
        Self {
            pool,
            metrics: None,
        }
    }

    pub fn from_shared(shared: &astra_core::SharedPool) -> Self {
        Self {
            pool: shared.get().clone(),
            metrics: None,
        }
    }

    pub fn with_metrics(mut self, metrics: SharedMultiAgentMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }
}

#[async_trait]
impl EdgeRegistryService for DatabaseEdgeRegistryService {
    #[tracing::instrument(skip(self, capabilities), fields(user_id = %user_id, edge_agent_id = %edge_agent_id))]
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

        // MatrixOne does not reliably fire ON DUPLICATE KEY UPDATE for UNIQUE KEY
        // violations (only PRIMARY KEY). Use SELECT-then-UPDATE-or-INSERT instead.
        // Wrap in a retry loop to handle TOCTOU races: a concurrent unregister()
        // between SELECT and UPDATE, or two concurrent register_or_update() calls.
        const MAX_RETRIES: u32 = 3;
        const BASE_BACKOFF_MS: u64 = 50;
        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                if let Some(ref m) = self.metrics {
                    m.registry_retry_total.fetch_add(1, Ordering::Relaxed);
                }
                tracing::warn!(
                    attempt,
                    max_retries = MAX_RETRIES,
                    "edge_registry: retrying register_or_update after TOCTOU race"
                );
                tokio::time::sleep(std::time::Duration::from_millis(
                    BASE_BACKOFF_MS * (1 << (attempt - 1)),
                ))
                .await;
            }
            // Fetch registry_id + registered_at in one query; construct the
            // response from in-memory data to eliminate the TOCTOU final SELECT.
            let existing: Option<(String, String)> = sqlx::query_as(
                "SELECT registry_id, CAST(registered_at AS CHAR) AS registered_at \
                 FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?",
            )
            .bind(user_id)
            .bind(edge_agent_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| format!("edge_registry lookup (attempt {attempt}): {e}"))?;

            if let Some((reg_id, registered_at)) = existing {
                let n = sqlx::query(
                    "UPDATE edge_agent_registry \
                     SET edge_id = ?, hostname = ?, worktree_path = ?, \
                         capabilities_json = ?, last_heartbeat_at = NOW(6) \
                     WHERE registry_id = ?",
                )
                .bind(edge_id_header)
                .bind(hostname)
                .bind(worktree_path)
                .bind(&cap_json)
                .bind(&reg_id)
                .execute(&self.pool)
                .await
                .map_err(|e| format!("edge_registry update (attempt {attempt}): {e}"))?
                .rows_affected();
                if n == 0 {
                    continue; // deleted between SELECT and UPDATE
                }
                let now = chrono::Utc::now()
                    .format("%Y-%m-%d %H:%M:%S%.6f")
                    .to_string();
                return Ok(EdgeAgentRecord {
                    registry_id: reg_id,
                    user_id: user_id.to_string(),
                    edge_agent_id: edge_agent_id.to_string(),
                    edge_id: edge_id_header.to_string(),
                    hostname: hostname.map(|s| s.to_string()),
                    worktree_path: worktree_path.map(|s| s.to_string()),
                    registered_at,
                    last_heartbeat_at: now,
                });
            }

            // No existing row — try INSERT.
            let registry_id = uuid::Uuid::new_v4().to_string();
            match sqlx::query(
                "INSERT INTO edge_agent_registry \
                 (registry_id, user_id, edge_agent_id, edge_id, hostname, worktree_path, \
                  capabilities_json, registered_at, last_heartbeat_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
            )
            .bind(&registry_id)
            .bind(user_id)
            .bind(edge_agent_id)
            .bind(edge_id_header)
            .bind(hostname)
            .bind(worktree_path)
            .bind(&cap_json)
            .execute(&self.pool)
            .await
            {
                Ok(_) => {
                    // Read back DB timestamps for consistency (both INSERT and
                    // UPDATE paths return DB-authored timestamps).
                    let (registered_at, last_heartbeat_at): (String, String) = sqlx::query_as(
                        "SELECT CAST(registered_at AS CHAR), \
                             CAST(last_heartbeat_at AS CHAR) \
                             FROM edge_agent_registry WHERE registry_id = ?",
                    )
                    .bind(&registry_id)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(|e| format!("edge_registry timestamp readback: {e}"))?;
                    return Ok(EdgeAgentRecord {
                        registry_id,
                        user_id: user_id.to_string(),
                        edge_agent_id: edge_agent_id.to_string(),
                        edge_id: edge_id_header.to_string(),
                        hostname: hostname.map(|s| s.to_string()),
                        worktree_path: worktree_path.map(|s| s.to_string()),
                        registered_at,
                        last_heartbeat_at,
                    });
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("1062") || msg.contains("Duplicate entry") {
                        continue; // raced with concurrent insert
                    }
                    return Err(format!("edge_registry insert: {e}"));
                }
            }
        }

        Err("edge_registry: exhausted retries".into())
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id, edge_agent_id = %edge_agent_id))]
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
            tracing::warn!("edge_registry: heartbeat for unregistered agent");
            return Err("edge agent not registered".to_string());
        }
        Ok(())
    }
    #[tracing::instrument(skip(self), fields(user_id = %user_id, edge_agent_id = %edge_agent_id))]
    async fn unregister(&self, user_id: &str, edge_agent_id: &str) -> Result<(), String> {
        sqlx::query("DELETE FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?")
            .bind(user_id)
            .bind(edge_agent_id)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("edge_registry unregister: {e}"))?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id))]
    async fn list_by_user(&self, user_id: &str) -> Result<Vec<EdgeAgentRecord>, String> {
        let rows = sqlx::query(
            "SELECT registry_id, user_id, edge_agent_id, edge_id, hostname, worktree_path, \
             CAST(registered_at AS CHAR) AS registered_at, CAST(last_heartbeat_at AS CHAR) AS last_heartbeat_at \
             FROM edge_agent_registry WHERE user_id = ? ORDER BY last_heartbeat_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("edge_registry list_by_user: {e}"))?;

        rows.iter()
            .map(|r| {
                Ok(EdgeAgentRecord {
                    registry_id: r.try_get("registry_id").map_err(|e| e.to_string())?,
                    user_id: r.try_get("user_id").map_err(|e| e.to_string())?,
                    edge_agent_id: r.try_get("edge_agent_id").map_err(|e| e.to_string())?,
                    edge_id: r.try_get("edge_id").map_err(|e| e.to_string())?,
                    hostname: r.try_get("hostname").ok().flatten(),
                    worktree_path: r.try_get("worktree_path").ok().flatten(),
                    registered_at: r.try_get::<String, _>("registered_at").unwrap_or_default(),
                    last_heartbeat_at: r
                        .try_get::<String, _>("last_heartbeat_at")
                        .unwrap_or_default(),
                })
            })
            .collect()
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

    async fn list_by_user(&self, _user_id: &str) -> Result<Vec<EdgeAgentRecord>, String> {
        Err("edge registry service not configured".to_string())
    }

    async fn unregister(&self, _user_id: &str, _edge_agent_id: &str) -> Result<(), String> {
        Err("edge registry service not configured".to_string())
    }
}
