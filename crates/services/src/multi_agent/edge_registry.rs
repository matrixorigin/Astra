//! Edge agent registry: register, list, and heartbeat edge agents.
//!
//! Split from the monolithic `multi_agent.rs`.

use std::sync::atomic::Ordering;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::metrics::SharedMultiAgentMetrics;
use crate::db_row::RowExt as EdgeRegistryDbRow;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EdgeAgentRecord {
    pub registry_id: String,
    pub user_id: String,
    pub edge_agent_id: String,
    pub edge_id: String,
    pub hostname: Option<String>,
    pub worktree_path: Option<String>,
    pub capabilities: Option<serde_json::Value>,
    /// Owning workspace (provider_scope_id from edge-registration token binding).
    /// None for legacy rows written before this field was added.
    pub workspace_id: Option<String>,
    pub registered_at: String,
    pub last_heartbeat_at: String,
}

/// Result of claiming an edge registry generation.
///
/// `previous` is the exact published generation replaced by `current`.
/// Database-backed implementations hold `claim_id` until the in-memory pool
/// commit, serializing this setup window across pods.
#[derive(Clone, Debug)]
pub struct EdgeRegistrationLease {
    pub current: EdgeAgentRecord,
    pub previous: Option<EdgeAgentRecord>,
    /// Database claim held across durable registration and in-memory pool
    /// commit. `None` for backends that do not implement cross-pod fencing.
    pub claim_id: Option<String>,
}

/// Structured error for `EdgeRegistryService::heartbeat`.
///
/// Callers must treat these two variants differently:
/// - `Superseded`: this connection's `edge_id` no longer owns the DB row — a
///   newer connection has taken over. Close the WebSocket immediately.
/// - `StorageFailure`: transient DB problem (network blip, pool exhaustion).
///   Log and allow a limited number of retries before closing.
#[derive(Debug)]
pub enum HeartbeatError {
    /// A newer connection has replaced this one in the registry.
    Superseded,
    /// Transient storage failure; the connection may still be valid.
    StorageFailure(String),
}

impl std::fmt::Display for HeartbeatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeartbeatError::Superseded => {
                write!(f, "edge connection superseded by newer registration")
            }
            HeartbeatError::StorageFailure(e) => write!(f, "edge heartbeat storage failure: {e}"),
        }
    }
}

#[async_trait]
pub trait EdgeRegistryService: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn register_or_update(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id_header: &str,
        hostname: Option<&str>,
        worktree_path: Option<&str>,
        capabilities: Option<serde_json::Value>,
        workspace_id: Option<&str>,
    ) -> Result<EdgeAgentRecord, String>;

    /// Claim the registry generation and retain the exact predecessor for a
    /// conditional rollback. Backends without durable generation support fall
    /// back to ordinary registration and report no predecessor.
    #[allow(clippy::too_many_arguments)]
    async fn register_or_update_with_lease(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id_header: &str,
        hostname: Option<&str>,
        worktree_path: Option<&str>,
        capabilities: Option<serde_json::Value>,
        workspace_id: Option<&str>,
    ) -> Result<EdgeRegistrationLease, String> {
        let current = self
            .register_or_update(
                user_id,
                edge_agent_id,
                edge_id_header,
                hostname,
                worktree_path,
                capabilities,
                workspace_id,
            )
            .await?;
        Ok(EdgeRegistrationLease {
            current,
            previous: None,
            claim_id: None,
        })
    }

    /// Undo a claimed generation only if it still owns the registry row.
    /// Returns false when a newer generation has already taken over.
    async fn rollback_registration(&self, lease: &EdgeRegistrationLease) -> Result<bool, String> {
        match &lease.previous {
            Some(previous) => {
                self.restore_superseded_edge_id(
                    &lease.current.user_id,
                    &lease.current.edge_agent_id,
                    &previous.edge_id,
                    &lease.current.edge_id,
                )
                .await
            }
            None => {
                self.unregister_generation(
                    &lease.current.user_id,
                    &lease.current.edge_agent_id,
                    &lease.current.edge_id,
                )
                .await
            }
        }
    }

    /// Release the cross-pod setup claim after the connection is published.
    /// Backends without durable claim support have nothing to release.
    async fn release_registration(&self, _lease: &EdgeRegistrationLease) -> Result<bool, String> {
        Ok(false)
    }

    /// Finalize the durable generation while retaining the cross-pod claim.
    /// The default registration path is already final, so non-claiming
    /// backends have nothing else to do.
    async fn finalize_registration(&self, _lease: &EdgeRegistrationLease) -> Result<bool, String> {
        Ok(true)
    }

    async fn heartbeat(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id_header: &str,
    ) -> Result<(), HeartbeatError>;

    /// Find the most-recently-active registry record for a given edge_agent_id,
    /// scoped to a workspace when `workspace_id` is `Some`.  Used by the
    /// cross-pod dispatch path to locate a sandbox edge registered under a
    /// service-account user.
    ///
    /// Workspace isolation is fail-closed:
    /// - `Some(ws)` only matches rows with the same `workspace_id = ws`.
    /// - `None` only matches legacy/unscoped rows where `workspace_id IS NULL`.
    ///
    /// A request without workspace context cannot resolve a workspace-bound
    /// sandbox edge; pass `Some` whenever workspace context is available.
    async fn find_by_agent_id_and_workspace(
        &self,
        edge_agent_id: &str,
        workspace_id: Option<&str>,
    ) -> Result<Option<EdgeAgentRecord>, String>;

    /// List all registered edge agents for a user (for cross-pod dispatch routing).
    async fn list_by_user(&self, user_id: &str) -> Result<Vec<EdgeAgentRecord>, String>;

    /// Remove only the exact connection incarnation registered by this socket.
    /// Returns false when a newer connection already replaced it.
    async fn unregister_generation(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id_header: &str,
    ) -> Result<bool, String>;

    /// Return the `edge_id` that currently owns the registry row for this
    /// `(user_id, edge_agent_id)`, or `None` if no row exists. A reconnect
    /// captures this BEFORE `register_or_update` (which overwrites it) so it can
    /// restore the previous owner if its own socket turns out to be dead.
    ///
    /// The default returns `Ok(None)` for backends without a durable registry.
    async fn current_edge_id(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }

    /// Restore the registry row's `edge_id` to a previously-captured owner, but
    /// only while `expected_current_edge_id` still owns it (so a newer reconnect
    /// that has since taken over is never clobbered). Used to undo a reconnect
    /// whose socket died after it had already overwritten the previous owner's
    /// row, preventing that healthy previous connection from being superseded.
    /// Returns whether the row was restored.
    ///
    /// The default returns `Ok(false)` for backends without a durable registry.
    async fn restore_superseded_edge_id(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
        _restore_to_edge_id: &str,
        _expected_current_edge_id: &str,
    ) -> Result<bool, String> {
        Ok(false)
    }
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

    #[allow(clippy::too_many_arguments)]
    async fn claim_registration(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id_header: &str,
        hostname: Option<&str>,
        worktree_path: Option<&str>,
        capabilities: Option<serde_json::Value>,
        workspace_id: Option<&str>,
    ) -> Result<EdgeRegistrationLease, String> {
        let cap_json = capabilities
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| format!("capabilities json: {e}"))?;
        const MAX_RETRIES: u32 = 5;
        let claim_id = uuid::Uuid::new_v4().to_string();

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                if let Some(ref metrics) = self.metrics {
                    metrics.registry_retry_total.fetch_add(1, Ordering::Relaxed);
                }
                tokio::time::sleep(std::time::Duration::from_millis(25 * (1 << (attempt - 1))))
                    .await;
            }

            let row = sqlx::query(
                "SELECT registry_id, user_id, edge_agent_id, edge_id, hostname, worktree_path, \
                 capabilities_json, workspace_id, registration_state, \
                 CAST(registered_at AS CHAR) AS registered_at, \
                 CAST(last_heartbeat_at AS CHAR) AS last_heartbeat_at \
                 FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?",
            )
            .bind(user_id)
            .bind(edge_agent_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| format!("edge_registry lease lookup (attempt {attempt}): {e}"))?;
            let previous = row.as_ref().map(decode_edge_agent_record).transpose()?;
            let registration_state = row
                .as_ref()
                .map(|row| row.i8_column("registration_state"))
                .transpose()
                .map_err(|e| {
                    edge_registry_decode_error("lease lookup row", "registration_state", e)
                })?
                .unwrap_or(1);

            if let Some(previous) = previous {
                // Acquire only the setup claim. Keep every active routing field
                // unchanged until finalize_registration(), so the published
                // predecessor remains heartbeatable and routable while setup is
                // pending.
                let updated = sqlx::query(
                    "UPDATE edge_agent_registry \
                     SET registration_claim_id = ?, \
                         registration_claim_expires_at = DATE_ADD(NOW(6), INTERVAL 120 SECOND) \
                     WHERE user_id = ? AND registry_id = ? AND edge_id = ? \
                       AND (registration_claim_id IS NULL \
                            OR registration_claim_expires_at < NOW(6))",
                )
                .bind(&claim_id)
                .bind(user_id)
                .bind(&previous.registry_id)
                .bind(&previous.edge_id)
                .execute(&self.pool)
                .await
                .map_err(|e| format!("edge_registry lease update (attempt {attempt}): {e}"))?
                .rows_affected();
                if updated == 0 {
                    continue;
                }

                let now = chrono::Utc::now()
                    .format("%Y-%m-%d %H:%M:%S%.6f")
                    .to_string();
                // State 1 is the only published state. State 0 is a never-
                // published insert and state 2 is a finalized generation whose
                // owner crashed before releasing its claim; neither is safe to
                // resurrect as a rollback target.
                let published_previous = (registration_state == 1).then_some(previous.clone());
                let current = EdgeAgentRecord {
                    registry_id: previous.registry_id.clone(),
                    user_id: user_id.to_string(),
                    edge_agent_id: edge_agent_id.to_string(),
                    edge_id: edge_id_header.to_string(),
                    hostname: hostname.map(ToString::to_string),
                    worktree_path: worktree_path.map(ToString::to_string),
                    capabilities: capabilities.clone(),
                    workspace_id: workspace_id.map(ToString::to_string).or_else(|| {
                        published_previous
                            .as_ref()
                            .and_then(|record| record.workspace_id.clone())
                    }),
                    registered_at: previous.registered_at.clone(),
                    last_heartbeat_at: now,
                };
                return Ok(EdgeRegistrationLease {
                    current,
                    previous: published_previous,
                    claim_id: Some(claim_id),
                });
            }

            let registry_id = uuid::Uuid::new_v4().to_string();
            let inserted = sqlx::query(
                "INSERT INTO edge_agent_registry \
                 (registry_id, user_id, edge_agent_id, edge_id, hostname, worktree_path, \
                  capabilities_json, workspace_id, registered_at, last_heartbeat_at, \
                  registration_claim_id, registration_claim_expires_at, registration_state) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6), ?, \
                         DATE_ADD(NOW(6), INTERVAL 120 SECOND), 0)",
            )
            .bind(&registry_id)
            .bind(user_id)
            .bind(edge_agent_id)
            .bind(edge_id_header)
            .bind(hostname)
            .bind(worktree_path)
            .bind(&cap_json)
            .bind(workspace_id)
            .bind(&claim_id)
            .execute(&self.pool)
            .await;
            match inserted {
                Ok(_) => {
                    let now = chrono::Utc::now()
                        .format("%Y-%m-%d %H:%M:%S%.6f")
                        .to_string();
                    return Ok(EdgeRegistrationLease {
                        current: EdgeAgentRecord {
                            registry_id,
                            user_id: user_id.to_string(),
                            edge_agent_id: edge_agent_id.to_string(),
                            edge_id: edge_id_header.to_string(),
                            hostname: hostname.map(ToString::to_string),
                            worktree_path: worktree_path.map(ToString::to_string),
                            capabilities: capabilities.clone(),
                            workspace_id: workspace_id.map(ToString::to_string),
                            registered_at: now.clone(),
                            last_heartbeat_at: now,
                        },
                        previous: None,
                        claim_id: Some(claim_id),
                    });
                }
                Err(error) => {
                    let message = error.to_string();
                    if message.contains("1062") || message.contains("Duplicate entry") {
                        continue;
                    }
                    return Err(format!("edge_registry lease insert: {error}"));
                }
            }
        }

        Err("edge_registry: exhausted generation-claim retries".to_string())
    }
}

fn edge_registry_decode_error(context: &str, column: &'static str, error: sqlx::Error) -> String {
    format!("edge_registry {context} decode `{column}`: {error}")
}

fn decode_edge_agent_record(row: &impl EdgeRegistryDbRow) -> Result<EdgeAgentRecord, String> {
    let capabilities = row
        .optional_string_column("capabilities_json")
        .map_err(|e| edge_registry_decode_error("list_by_user row", "capabilities_json", e))?;
    let capabilities = capabilities
        .map(|raw| {
            serde_json::from_str(&raw)
                .map_err(|e| format!("edge_registry list_by_user decode `capabilities_json`: {e}"))
        })
        .transpose()?;

    Ok(EdgeAgentRecord {
        registry_id: row
            .string_column("registry_id")
            .map_err(|e| edge_registry_decode_error("list_by_user row", "registry_id", e))?,
        user_id: row
            .string_column("user_id")
            .map_err(|e| edge_registry_decode_error("list_by_user row", "user_id", e))?,
        edge_agent_id: row
            .string_column("edge_agent_id")
            .map_err(|e| edge_registry_decode_error("list_by_user row", "edge_agent_id", e))?,
        edge_id: row
            .string_column("edge_id")
            .map_err(|e| edge_registry_decode_error("list_by_user row", "edge_id", e))?,
        hostname: row
            .optional_string_column("hostname")
            .map_err(|e| edge_registry_decode_error("list_by_user row", "hostname", e))?,
        worktree_path: row
            .optional_string_column("worktree_path")
            .map_err(|e| edge_registry_decode_error("list_by_user row", "worktree_path", e))?,
        capabilities,
        workspace_id: row
            .optional_string_column("workspace_id")
            .map_err(|e| edge_registry_decode_error("list_by_user row", "workspace_id", e))?,
        registered_at: row
            .string_column("registered_at")
            .map_err(|e| edge_registry_decode_error("list_by_user row", "registered_at", e))?,
        last_heartbeat_at: row
            .string_column("last_heartbeat_at")
            .map_err(|e| edge_registry_decode_error("list_by_user row", "last_heartbeat_at", e))?,
    })
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
        workspace_id: Option<&str>,
    ) -> Result<EdgeAgentRecord, String> {
        let capabilities_for_record = capabilities.clone();
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
                         capabilities_json = ?, \
                         workspace_id = COALESCE(?, workspace_id), \
                         last_heartbeat_at = NOW(6), registration_claim_id = NULL, \
                         registration_claim_expires_at = NULL, registration_state = 1, \
                         registration_previous_edge_id = NULL \
                     WHERE user_id = ? AND registry_id = ? \
                       AND (registration_claim_id IS NULL \
                            OR registration_claim_expires_at < NOW(6))",
                )
                .bind(edge_id_header)
                .bind(hostname)
                .bind(worktree_path)
                .bind(&cap_json)
                .bind(workspace_id)
                .bind(user_id)
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
                    capabilities: capabilities_for_record.clone(),
                    workspace_id: workspace_id.map(|s| s.to_string()),
                    registered_at,
                    last_heartbeat_at: now,
                });
            }

            // No existing row — try INSERT.
            let registry_id = uuid::Uuid::new_v4().to_string();
            match sqlx::query(
                "INSERT INTO edge_agent_registry \
                 (registry_id, user_id, edge_agent_id, edge_id, hostname, worktree_path, \
                  capabilities_json, workspace_id, registered_at, last_heartbeat_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
            )
            .bind(&registry_id)
            .bind(user_id)
            .bind(edge_agent_id)
            .bind(edge_id_header)
            .bind(hostname)
            .bind(worktree_path)
            .bind(&cap_json)
            .bind(workspace_id)
            .execute(&self.pool)
            .await
            {
                Ok(_) => {
                    // The row is now persisted. Read back DB-authored timestamps
                    // for consistency, but do NOT fail the whole call if only the
                    // readback fails: the INSERT already succeeded, and returning
                    // Err here would break the caller's contract ("Err ⟹ not
                    // persisted") — the edge WS handler would keep the previous
                    // connection while the DB already points at this new one,
                    // leaving them inconsistent. Fall back to a local timestamp.
                    let (registered_at, last_heartbeat_at) = match sqlx::query_as::<
                        _,
                        (String, String),
                    >(
                        "SELECT CAST(registered_at AS CHAR), \
                             CAST(last_heartbeat_at AS CHAR) \
                             FROM edge_agent_registry WHERE user_id = ? AND registry_id = ?",
                    )
                    .bind(user_id)
                    .bind(&registry_id)
                    .fetch_one(&self.pool)
                    .await
                    {
                        Ok(timestamps) => timestamps,
                        Err(e) => {
                            tracing::warn!(
                                user_id,
                                edge_agent_id,
                                registry_id = %registry_id,
                                error = %e,
                                "edge_registry: INSERT succeeded but timestamp readback failed; \
                                 returning local timestamps"
                            );
                            let now = chrono::Utc::now()
                                .format("%Y-%m-%d %H:%M:%S%.6f")
                                .to_string();
                            (now.clone(), now)
                        }
                    };
                    return Ok(EdgeAgentRecord {
                        registry_id,
                        user_id: user_id.to_string(),
                        edge_agent_id: edge_agent_id.to_string(),
                        edge_id: edge_id_header.to_string(),
                        hostname: hostname.map(|s| s.to_string()),
                        worktree_path: worktree_path.map(|s| s.to_string()),
                        capabilities: capabilities_for_record.clone(),
                        workspace_id: workspace_id.map(|s| s.to_string()),
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

    #[tracing::instrument(skip(self, capabilities), fields(user_id = %user_id, edge_agent_id = %edge_agent_id))]
    async fn register_or_update_with_lease(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id_header: &str,
        hostname: Option<&str>,
        worktree_path: Option<&str>,
        capabilities: Option<serde_json::Value>,
        workspace_id: Option<&str>,
    ) -> Result<EdgeRegistrationLease, String> {
        self.claim_registration(
            user_id,
            edge_agent_id,
            edge_id_header,
            hostname,
            worktree_path,
            capabilities,
            workspace_id,
        )
        .await
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id, edge_agent_id = %edge_agent_id))]
    async fn heartbeat(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id_header: &str,
    ) -> Result<(), HeartbeatError> {
        // Guard on edge_id so a stale connection cannot refresh (or resurrect)
        // the row after a newer connection has replaced it. register_or_update
        // already set edge_id to the current connection's value, so we only
        // touch last_heartbeat_at and never rewrite edge_id here. If a newer
        // connection has taken over (edge_id differs), this matches 0 rows and
        // the stale connection's heartbeat correctly returns Superseded.
        let n = sqlx::query(
            "UPDATE edge_agent_registry SET last_heartbeat_at = NOW(6) \
             WHERE user_id = ? AND edge_agent_id = ? \
               AND ((registration_state = 1 AND edge_id = ?) \
                    OR (registration_state = 2 AND registration_previous_edge_id = ?))",
        )
        .bind(user_id)
        .bind(edge_agent_id)
        .bind(edge_id_header)
        .bind(edge_id_header)
        .execute(&self.pool)
        .await
        .map_err(|e| HeartbeatError::StorageFailure(format!("edge heartbeat: {e}")))?
        .rows_affected();
        if n == 0 {
            // The row is gone or belongs to a newer connection — this connection
            // has been superseded and must not keep the DB entry alive.
            tracing::warn!(
                edge_id = %edge_id_header,
                "edge_registry: heartbeat matched no row (unregistered or superseded by newer connection)"
            );
            return Err(HeartbeatError::Superseded);
        }
        Ok(())
    }
    #[tracing::instrument(skip(self), fields(user_id = %user_id, edge_agent_id = %edge_agent_id))]
    async fn unregister_generation(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id_header: &str,
    ) -> Result<bool, String> {
        let deleted = sqlx::query(
            "DELETE FROM edge_agent_registry \
             WHERE user_id = ? AND edge_agent_id = ? AND edge_id = ?",
        )
        .bind(user_id)
        .bind(edge_agent_id)
        .bind(edge_id_header)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge_registry unregister: {e}"))?;
        Ok(deleted.rows_affected() > 0)
    }

    async fn rollback_registration(&self, lease: &EdgeRegistrationLease) -> Result<bool, String> {
        let Some(claim_id) = lease.claim_id.as_deref() else {
            return EdgeRegistryService::unregister_generation(
                self,
                &lease.current.user_id,
                &lease.current.edge_agent_id,
                &lease.current.edge_id,
            )
            .await;
        };
        let Some(previous) = &lease.previous else {
            let deleted = sqlx::query(
                "DELETE FROM edge_agent_registry \
                 WHERE user_id = ? AND registry_id = ? AND registration_claim_id = ?",
            )
            .bind(&lease.current.user_id)
            .bind(&lease.current.registry_id)
            .bind(claim_id)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("edge_registry rollback inserted registration: {e}"))?;
            return Ok(deleted.rows_affected() > 0);
        };
        let capabilities_json = previous
            .capabilities
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| format!("edge_registry rollback capabilities json: {e}"))?;
        let restored = sqlx::query(
            "UPDATE edge_agent_registry \
             SET edge_id = ?, hostname = ?, worktree_path = ?, capabilities_json = ?, \
                 workspace_id = ?, last_heartbeat_at = NOW(6), \
                 registration_claim_id = NULL, registration_claim_expires_at = NULL, \
                 registration_state = 1, registration_previous_edge_id = NULL \
             WHERE user_id = ? AND registry_id = ? AND registration_claim_id = ?",
        )
        .bind(&previous.edge_id)
        .bind(&previous.hostname)
        .bind(&previous.worktree_path)
        .bind(&capabilities_json)
        .bind(&previous.workspace_id)
        .bind(&lease.current.user_id)
        .bind(&lease.current.registry_id)
        .bind(claim_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge_registry rollback registration: {e}"))?;
        Ok(restored.rows_affected() > 0)
    }

    async fn finalize_registration(&self, lease: &EdgeRegistrationLease) -> Result<bool, String> {
        let Some(claim_id) = lease.claim_id.as_deref() else {
            return Ok(true);
        };
        let capabilities_json = lease
            .current
            .capabilities
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| format!("edge_registry finalize capabilities json: {e}"))?;
        let finalized = sqlx::query(
            "UPDATE edge_agent_registry \
             SET edge_id = ?, hostname = ?, worktree_path = ?, capabilities_json = ?, \
                 workspace_id = ?, last_heartbeat_at = NOW(6), registration_state = 2, \
                 registration_previous_edge_id = ? \
             WHERE user_id = ? AND registry_id = ? AND registration_claim_id = ?",
        )
        .bind(&lease.current.edge_id)
        .bind(&lease.current.hostname)
        .bind(&lease.current.worktree_path)
        .bind(&capabilities_json)
        .bind(&lease.current.workspace_id)
        .bind(
            lease
                .previous
                .as_ref()
                .map(|previous| previous.edge_id.as_str()),
        )
        .bind(&lease.current.user_id)
        .bind(&lease.current.registry_id)
        .bind(claim_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge_registry finalize registration: {e}"))?;
        Ok(finalized.rows_affected() > 0)
    }

    async fn release_registration(&self, lease: &EdgeRegistrationLease) -> Result<bool, String> {
        let Some(claim_id) = lease.claim_id.as_deref() else {
            return Ok(false);
        };
        let released = sqlx::query(
            "UPDATE edge_agent_registry \
             SET registration_claim_id = NULL, registration_claim_expires_at = NULL, \
                 registration_state = 1, registration_previous_edge_id = NULL \
             WHERE user_id = ? AND registry_id = ? AND edge_id = ? \
               AND registration_claim_id = ? AND registration_state = 2",
        )
        .bind(&lease.current.user_id)
        .bind(&lease.current.registry_id)
        .bind(&lease.current.edge_id)
        .bind(claim_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge_registry release registration claim: {e}"))?;
        Ok(released.rows_affected() > 0)
    }

    async fn current_edge_id(
        &self,
        user_id: &str,
        edge_agent_id: &str,
    ) -> Result<Option<String>, String> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT edge_id FROM edge_agent_registry \
             WHERE user_id = ? AND edge_agent_id = ?",
        )
        .bind(user_id)
        .bind(edge_agent_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("edge_registry current_edge_id: {e}"))?;
        Ok(row.map(|(edge_id,)| edge_id))
    }

    async fn restore_superseded_edge_id(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        restore_to_edge_id: &str,
        expected_current_edge_id: &str,
    ) -> Result<bool, String> {
        // Restore only while our incarnation still owns the row (guard on
        // edge_id = expected_current), so a newer reconnect is never clobbered.
        let restored = sqlx::query(
            "UPDATE edge_agent_registry SET edge_id = ?, last_heartbeat_at = NOW(6) \
             WHERE user_id = ? AND edge_agent_id = ? AND edge_id = ?",
        )
        .bind(restore_to_edge_id)
        .bind(user_id)
        .bind(edge_agent_id)
        .bind(expected_current_edge_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge_registry restore_superseded_edge_id: {e}"))?;
        Ok(restored.rows_affected() > 0)
    }

    #[tracing::instrument(skip(self), fields(edge_agent_id = %edge_agent_id))]
    async fn find_by_agent_id_and_workspace(
        &self,
        edge_agent_id: &str,
        workspace_id: Option<&str>,
    ) -> Result<Option<EdgeAgentRecord>, String> {
        // Fail-closed workspace isolation:
        //   request has workspace_id  → edge.workspace_id must match exactly
        //   request has no workspace_id → only reach edges that are also unscoped (workspace_id IS NULL)
        // This prevents a request without workspace context from resolving a
        // workspace-bound sandbox edge (e.g. when workspace_record is None on
        // the MOI provider-authorized path).
        let row = sqlx::query(
            "SELECT registry_id, user_id, edge_agent_id, edge_id, hostname, worktree_path, \
             capabilities_json, workspace_id, \
             CAST(registered_at AS CHAR) AS registered_at, \
             CAST(last_heartbeat_at AS CHAR) AS last_heartbeat_at \
             FROM edge_agent_registry \
             WHERE edge_agent_id = ? \
               AND registration_state = 1 \
               AND ((? IS NOT NULL AND workspace_id = ?) OR (? IS NULL AND workspace_id IS NULL)) \
             ORDER BY last_heartbeat_at DESC LIMIT 1",
        )
        .bind(edge_agent_id)
        .bind(workspace_id)
        .bind(workspace_id)
        .bind(workspace_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("edge_registry find_by_agent_id_and_workspace: {e}"))?;

        row.as_ref().map(decode_edge_agent_record).transpose()
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id))]
    async fn list_by_user(&self, user_id: &str) -> Result<Vec<EdgeAgentRecord>, String> {
        let rows = sqlx::query(
            "SELECT registry_id, user_id, edge_agent_id, edge_id, hostname, worktree_path, \
             capabilities_json, workspace_id, \
             CAST(registered_at AS CHAR) AS registered_at, \
             CAST(last_heartbeat_at AS CHAR) AS last_heartbeat_at \
             FROM edge_agent_registry \
             WHERE user_id = ? AND registration_state = 1 \
             ORDER BY last_heartbeat_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("edge_registry list_by_user: {e}"))?;

        rows.iter().map(decode_edge_agent_record).collect()
    }
}

pub struct UnconfiguredEdgeRegistryService;

#[async_trait]
impl EdgeRegistryService for UnconfiguredEdgeRegistryService {
    /// When no cross-pod registry is configured (single-pod deployment), edge
    /// registration is a successful no-op: the connection is tracked in the
    /// in-memory pool and there is no cross-pod source of truth to fail. This is
    /// distinct from a *configured* registry (e.g. DB-backed) whose failure
    /// returns an error and — per the edge WS handler — rejects the connection.
    async fn register_or_update(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id_header: &str,
        hostname: Option<&str>,
        worktree_path: Option<&str>,
        capabilities: Option<serde_json::Value>,
        workspace_id: Option<&str>,
    ) -> Result<EdgeAgentRecord, String> {
        let now = chrono::Utc::now()
            .format("%Y-%m-%d %H:%M:%S%.6f")
            .to_string();
        Ok(EdgeAgentRecord {
            registry_id: edge_id_header.to_string(),
            user_id: user_id.to_string(),
            edge_agent_id: edge_agent_id.to_string(),
            edge_id: edge_id_header.to_string(),
            hostname: hostname.map(|s| s.to_string()),
            worktree_path: worktree_path.map(|s| s.to_string()),
            capabilities,
            workspace_id: workspace_id.map(|s| s.to_string()),
            registered_at: now.clone(),
            last_heartbeat_at: now,
        })
    }

    async fn find_by_agent_id_and_workspace(
        &self,
        _edge_agent_id: &str,
        _workspace_id: Option<&str>,
    ) -> Result<Option<EdgeAgentRecord>, String> {
        Ok(None)
    }

    /// No-op success: there is no cross-pod row to refresh.
    async fn heartbeat(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
        _edge_id_header: &str,
    ) -> Result<(), HeartbeatError> {
        Ok(())
    }

    async fn list_by_user(&self, _user_id: &str) -> Result<Vec<EdgeAgentRecord>, String> {
        Ok(Vec::new())
    }

    /// No-op success: nothing was persisted, so nothing can be removed.
    async fn unregister_generation(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
        _edge_id_header: &str,
    ) -> Result<bool, String> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeEdgeRegistryRow {
        failed_column: Option<&'static str>,
        capabilities_json: Option<&'static str>,
        hostname: Option<&'static str>,
        worktree_path: Option<&'static str>,
    }

    impl FakeEdgeRegistryRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                capabilities_json: Some(r#"{"tools":["agent_fanout"]}"#),
                hostname: Some("edge-host"),
                worktree_path: Some("/worktree"),
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn without_optional_fields() -> Self {
            Self {
                failed_column: None,
                capabilities_json: None,
                hostname: None,
                worktree_path: None,
            }
        }

        fn with_capabilities_json(capabilities_json: &'static str) -> Self {
            Self {
                capabilities_json: Some(capabilities_json),
                ..Self::complete()
            }
        }
    }

    impl EdgeRegistryDbRow for FakeEdgeRegistryRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            Ok(match column {
                "registry_id" => "registry-1",
                "user_id" => "user-1",
                "edge_agent_id" => "edge-agent-1",
                "edge_id" => "edge-transport-1",
                "registered_at" => "2026-06-26 10:00:00.000000",
                "last_heartbeat_at" => "2026-06-26 10:01:00.000000",
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            Ok(match column {
                "hostname" => self.hostname,
                "worktree_path" => self.worktree_path,
                "capabilities_json" => self.capabilities_json,
                "workspace_id" => None,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .map(str::to_string))
        }
    }

    #[test]
    fn edge_agent_record_decode_preserves_database_values() {
        let record = decode_edge_agent_record(&FakeEdgeRegistryRow::complete()).unwrap();

        assert_eq!(record.registry_id, "registry-1");
        assert_eq!(record.user_id, "user-1");
        assert_eq!(record.edge_agent_id, "edge-agent-1");
        assert_eq!(record.edge_id, "edge-transport-1");
        assert_eq!(record.hostname.as_deref(), Some("edge-host"));
        assert_eq!(record.worktree_path.as_deref(), Some("/worktree"));
        assert_eq!(
            record.capabilities.as_ref().and_then(|v| v.get("tools")),
            Some(&serde_json::json!(["agent_fanout"]))
        );
        assert_eq!(record.registered_at, "2026-06-26 10:00:00.000000");
        assert_eq!(record.last_heartbeat_at, "2026-06-26 10:01:00.000000");
    }

    #[test]
    fn edge_agent_record_decode_preserves_sql_null_optional_fields() {
        let record = decode_edge_agent_record(&FakeEdgeRegistryRow::without_optional_fields())
            .expect("SQL NULL optional columns are valid");

        assert_eq!(record.hostname, None);
        assert_eq!(record.worktree_path, None);
        assert_eq!(record.capabilities, None);
    }

    #[test]
    fn edge_agent_record_decode_fails_loudly_on_any_column_error() {
        for column in [
            "registry_id",
            "user_id",
            "edge_agent_id",
            "edge_id",
            "hostname",
            "worktree_path",
            "capabilities_json",
            "registered_at",
            "last_heartbeat_at",
        ] {
            let error =
                decode_edge_agent_record(&FakeEdgeRegistryRow::fail_on(column)).unwrap_err();
            assert!(
                error.contains("edge_registry list_by_user") && error.contains(column),
                "decode error should identify `{column}`: {error}"
            );
        }
    }

    #[test]
    fn edge_agent_record_decode_fails_loudly_on_invalid_capabilities_json() {
        let error =
            decode_edge_agent_record(&FakeEdgeRegistryRow::with_capabilities_json("not-json"))
                .unwrap_err();

        assert!(
            error.contains("edge_registry list_by_user decode `capabilities_json`"),
            "invalid capabilities JSON should not be silently dropped: {error}"
        );
    }
}
