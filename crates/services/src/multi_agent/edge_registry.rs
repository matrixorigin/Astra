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
                         capabilities_json = ?, last_heartbeat_at = NOW(6) \
                     WHERE user_id = ? AND registry_id = ?",
                )
                .bind(edge_id_header)
                .bind(hostname)
                .bind(worktree_path)
                .bind(&cap_json)
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
                             FROM edge_agent_registry WHERE user_id = ? AND registry_id = ?",
                    )
                    .bind(user_id)
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
                        capabilities: capabilities_for_record.clone(),
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
            "SELECT registry_id, user_id, edge_agent_id, edge_id, hostname, worktree_path, capabilities_json, \
             CAST(registered_at AS CHAR) AS registered_at, CAST(last_heartbeat_at AS CHAR) AS last_heartbeat_at \
             FROM edge_agent_registry WHERE user_id = ? ORDER BY last_heartbeat_at DESC",
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
