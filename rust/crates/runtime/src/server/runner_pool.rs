use std::{collections::HashMap, net::IpAddr, sync::Arc, time::Duration};

use astra_core::SharedPool;
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use sqlx::Row;

use astra_runtime_env::{
    ExecutorBinding, ExecutorBindingKind, PolicyIntent, RunBinding, RunnerAckResponse,
    RunnerCapacity, RunnerDenial, RunnerDenialReason, RunnerDeploymentKind,
    RunnerExecuteToolRequest, RunnerExecuteToolResponse, RunnerHeartbeat, RunnerIdentity,
    RunnerPoolEntry, RunnerPrepareSessionRequest, RunnerPrepareSessionResponse,
    RunnerRegisterRequest, RunnerRegisterResponse, RunnerRpcEndpoint, RunnerScheduleDecision,
    RunnerScheduleDenial, RunnerScheduleDenialReason, RunnerScheduleRequest, RunnerScheduler,
    RunnerStatus, RuntimeBinding, RuntimeEnvironmentAdvertisement, RuntimeError,
    RuntimeIsolationBackend, RuntimeSessionManager, ToolRegistry, WorkspaceAuthority,
    WorkspaceBinding, WorkspaceBindingKind, WorkspaceOwnerScope, WorkspacePersistence,
    WorkspaceRecord, WorkspaceSource,
};
use astra_services::{
    DatabaseWorkspaceRecordStore, WorkspaceCleanupDebtEntry, WorkspaceCleanupDebtStore,
};

use super::*;

const RUNNER_LEASE_TTL_SECS: f64 = 30.0;
const RUNNER_LEASE_TTL_MS: i64 = 30_000;
/// Interval at which the reaper purges expired runner entries.
/// Must be ≤ RUNNER_LEASE_TTL_SECS so dead runners don't occupy slots
/// for longer than a single lease cycle.
const RUNNER_POOL_REAPER_INTERVAL_SECS: u64 = 30;
const RUNNER_POOL_REAPER_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;
const RUNNER_POOL_REAPER_BATCH_LIMIT: u64 = 1_000;
const SHARED_RUNNER_POOL_OWNER_ID: &str = "__astra_shared_runner_pool__";

/// Maximum total wait time for runner scheduling backoff (ms).
/// When all runners report capacity exhausted, we retry with exponential
/// backoff up to this ceiling before returning a rejection.
const RUNNER_SCHEDULE_BACKOFF_MAX_MS: u64 = 5_000;
/// Initial backoff delay (ms). Doubles each retry up to RUNNER_SCHEDULE_BACKOFF_MAX_MS.
const RUNNER_SCHEDULE_BACKOFF_INITIAL_MS: u64 = 100;
/// Maximum number of backoff retry iterations.
const RUNNER_SCHEDULE_BACKOFF_MAX_RETRIES: usize = 8;

#[derive(Clone, Debug)]
struct StoredRunnerPoolEntry {
    pool_owner_id: String,
    entry: Arc<RunnerPoolEntry>,
    lease_expires_at_ms: i64,
    active_session_ids: Vec<String>,
}

#[async_trait]
trait RunnerPoolStore: Send + Sync {
    async fn upsert(&self, entry: StoredRunnerPoolEntry) -> Result<(), String>;
    async fn get(&self, runner_id: &str) -> Result<Option<StoredRunnerPoolEntry>, String>;
    async fn list_for_owner(&self, owner_id: &str) -> Result<Vec<StoredRunnerPoolEntry>, String>;
    async fn list_expired_before(
        &self,
        lease_expires_before_ms: i64,
        limit: u64,
    ) -> Result<Vec<StoredRunnerPoolEntry>, String>;
    async fn delete_if_lease_matches(
        &self,
        runner_id: &str,
        lease_expires_at_ms: i64,
    ) -> Result<bool, String>;
    /// Atomically re-lease a runner if its lease hasn't changed.
    ///
    /// Returns `Some(updated_entry)` with a fresh lease on success,
    /// or `None` if the runner is missing or its lease was modified
    /// (another caller acquired it first).
    async fn try_acquire_lease(
        &self,
        runner_id: &str,
        expected_lease_ms: i64,
        new_lease_ms: i64,
    ) -> Result<Option<StoredRunnerPoolEntry>, String>;
}

#[derive(Clone, Default)]
struct InMemoryRunnerPoolStore {
    runners: Arc<tokio::sync::RwLock<HashMap<String, StoredRunnerPoolEntry>>>,
}

#[async_trait]
impl RunnerPoolStore for InMemoryRunnerPoolStore {
    async fn upsert(&self, entry: StoredRunnerPoolEntry) -> Result<(), String> {
        self.runners
            .write()
            .await
            .insert(entry.entry.identity.runner_id.clone(), entry);
        Ok(())
    }

    async fn get(&self, runner_id: &str) -> Result<Option<StoredRunnerPoolEntry>, String> {
        Ok(self.runners.read().await.get(runner_id).cloned())
    }

    async fn list_for_owner(&self, owner_id: &str) -> Result<Vec<StoredRunnerPoolEntry>, String> {
        let guard = self.runners.read().await;
        let mut runners: Vec<StoredRunnerPoolEntry> = guard
            .iter()
            .filter(|(_, entry)| runner_pool_entry_visible_to_user(entry, owner_id))
            .map(|(_, entry)| StoredRunnerPoolEntry {
                pool_owner_id: entry.pool_owner_id.clone(),
                entry: Arc::clone(&entry.entry),
                lease_expires_at_ms: entry.lease_expires_at_ms,
                active_session_ids: Vec::new(),
            })
            .collect();
        drop(guard);
        runners.sort_by(|a, b| a.entry.identity.runner_id.cmp(&b.entry.identity.runner_id));
        Ok(runners)
    }

    async fn list_expired_before(
        &self,
        lease_expires_before_ms: i64,
        limit: u64,
    ) -> Result<Vec<StoredRunnerPoolEntry>, String> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let runners = self.runners.read().await;
        let mut expired: Vec<_> = runners
            .iter()
            .filter(|(_, entry)| entry.lease_expires_at_ms <= lease_expires_before_ms)
            .map(|(runner_id, entry)| (entry.lease_expires_at_ms, runner_id.clone()))
            .collect();
        expired.sort_by(|(left_expiry, left_id), (right_expiry, right_id)| {
            left_expiry
                .cmp(right_expiry)
                .then_with(|| left_id.cmp(right_id))
        });
        Ok(expired
            .into_iter()
            .take(limit as usize)
            .filter_map(|(_, runner_id)| runners.get(&runner_id).cloned())
            .collect())
    }

    async fn delete_if_lease_matches(
        &self,
        runner_id: &str,
        lease_expires_at_ms: i64,
    ) -> Result<bool, String> {
        let mut runners = self.runners.write().await;
        if runners
            .get(runner_id)
            .is_some_and(|entry| entry.lease_expires_at_ms == lease_expires_at_ms)
        {
            runners.remove(runner_id);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn try_acquire_lease(
        &self,
        runner_id: &str,
        expected_lease_ms: i64,
        new_lease_ms: i64,
    ) -> Result<Option<StoredRunnerPoolEntry>, String> {
        let mut runners = self.runners.write().await;
        let Some(entry) = runners.get_mut(runner_id) else {
            return Ok(None);
        };
        if entry.lease_expires_at_ms != expected_lease_ms {
            return Ok(None);
        }
        entry.lease_expires_at_ms = new_lease_ms;
        Ok(Some(entry.clone()))
    }
}

#[derive(Clone)]
struct DatabaseRunnerPoolStore {
    pool: sqlx::Pool<sqlx::MySql>,
}

impl DatabaseRunnerPoolStore {
    fn new(shared_pool: &SharedPool) -> Self {
        Self {
            pool: shared_pool.get().clone(),
        }
    }

    async fn ensure_tables(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS runner_pool (
                runner_id           VARCHAR(255) PRIMARY KEY,
                owner_id            VARCHAR(255) NOT NULL,
                status              VARCHAR(64)  NOT NULL,
                identity_json       LONGTEXT     NOT NULL,
                capacity_json       LONGTEXT     NOT NULL,
                advertisement_json  LONGTEXT     NOT NULL,
                rpc_endpoint_json   LONGTEXT     NULL,
                active_session_ids_json LONGTEXT  NULL,
                lease_expires_at_ms  BIGINT       NOT NULL DEFAULT 0,
                registered_at       DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
                updated_at          DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
                last_heartbeat_at   DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;
        ensure_runner_pool_column(
            &self.pool,
            "ALTER TABLE runner_pool ADD COLUMN lease_expires_at_ms BIGINT NOT NULL DEFAULT 0",
        )
        .await?;
        ensure_runner_pool_column(
            &self.pool,
            "ALTER TABLE runner_pool ADD COLUMN active_session_ids_json LONGTEXT NULL",
        )
        .await?;
        Ok(())
    }
}

async fn ensure_runner_pool_column(
    pool: &sqlx::Pool<sqlx::MySql>,
    sql: &str,
) -> Result<(), sqlx::Error> {
    match sqlx::query(sql).execute(pool).await {
        Ok(_) => Ok(()),
        Err(error) => {
            let message = error.to_string();
            if message.contains("1060")
                || message.contains("Duplicate column")
                || message.contains("duplicate column")
            {
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

#[async_trait]
impl RunnerPoolStore for DatabaseRunnerPoolStore {
    async fn upsert(&self, entry: StoredRunnerPoolEntry) -> Result<(), String> {
        let runner_id = entry.entry.identity.runner_id.clone();
        let owner_id = entry.pool_owner_id.clone();
        let identity_json = serde_json::to_string(&entry.entry.identity)
            .map_err(|error| format!("runner identity serialize: {error}"))?;
        let capacity_json = serde_json::to_string(&entry.entry.capacity)
            .map_err(|error| format!("runner capacity serialize: {error}"))?;
        let advertisement_json = serde_json::to_string(&entry.entry.advertisement)
            .map_err(|error| format!("runner advertisement serialize: {error}"))?;
        let rpc_endpoint_json = entry
            .entry
            .rpc_endpoint
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| format!("runner rpc endpoint serialize: {error}"))?;
        let active_session_ids_json = serde_json::to_string(&entry.active_session_ids)
            .map_err(|error| format!("runner active sessions serialize: {error}"))?;
        let status = runner_status_to_db(entry.entry.status);
        let lease_expires_at_ms = entry.lease_expires_at_ms;

        // Atomic INSERT … ON DUPLICATE KEY UPDATE — runner_id is PRIMARY KEY so
        // MatrixOne fires the UPDATE clause reliably.  1 round-trip instead of 2-6.
        sqlx::query(
            "INSERT INTO runner_pool \
             (runner_id, owner_id, status, identity_json, capacity_json, \
              advertisement_json, rpc_endpoint_json, active_session_ids_json, \
              lease_expires_at_ms, \
              registered_at, updated_at, last_heartbeat_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6), NOW(6)) \
             ON DUPLICATE KEY UPDATE \
                 owner_id = VALUES(owner_id), \
                 status = VALUES(status), \
                 identity_json = VALUES(identity_json), \
                 capacity_json = VALUES(capacity_json), \
                 advertisement_json = VALUES(advertisement_json), \
                 rpc_endpoint_json = VALUES(rpc_endpoint_json), \
                 active_session_ids_json = VALUES(active_session_ids_json), \
                 lease_expires_at_ms = VALUES(lease_expires_at_ms), \
                 updated_at = NOW(6), \
                 last_heartbeat_at = NOW(6)",
        )
        .bind(&runner_id)
        .bind(&owner_id)
        .bind(status)
        .bind(&identity_json)
        .bind(&capacity_json)
        .bind(&advertisement_json)
        .bind(&rpc_endpoint_json)
        .bind(&active_session_ids_json)
        .bind(lease_expires_at_ms)
        .execute(&self.pool)
        .await
        .map_err(|error| format!("runner_pool upsert: {error}"))?;

        Ok(())
    }

    async fn get(&self, runner_id: &str) -> Result<Option<StoredRunnerPoolEntry>, String> {
        let row = sqlx::query(
            "SELECT CAST(identity_json AS CHAR) AS identity_json, \
                    status, \
                    CAST(capacity_json AS CHAR) AS capacity_json, \
                    CAST(advertisement_json AS CHAR) AS advertisement_json, \
                    CAST(rpc_endpoint_json AS CHAR) AS rpc_endpoint_json, \
                    CAST(active_session_ids_json AS CHAR) AS active_session_ids_json, \
                    lease_expires_at_ms, \
                    owner_id \
             FROM runner_pool WHERE runner_id = ?",
        )
        .bind(runner_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| format!("runner_pool get: {error}"))?;
        row.map(runner_entry_from_row).transpose()
    }

    async fn list_for_owner(&self, owner_id: &str) -> Result<Vec<StoredRunnerPoolEntry>, String> {
        let rows = sqlx::query(
            "SELECT CAST(identity_json AS CHAR) AS identity_json, \
                    status, \
                    CAST(capacity_json AS CHAR) AS capacity_json, \
                    CAST(advertisement_json AS CHAR) AS advertisement_json, \
                    CAST(rpc_endpoint_json AS CHAR) AS rpc_endpoint_json, \
                    lease_expires_at_ms, \
                    owner_id \
             FROM runner_pool WHERE owner_id = ? OR owner_id = ? ORDER BY runner_id ASC",
        )
        .bind(owner_id)
        .bind(SHARED_RUNNER_POOL_OWNER_ID)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| format!("runner_pool list: {error}"))?;
        rows.into_iter()
            .map(runner_entry_from_row_no_sessions)
            .collect()
    }

    async fn list_expired_before(
        &self,
        lease_expires_before_ms: i64,
        limit: u64,
    ) -> Result<Vec<StoredRunnerPoolEntry>, String> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = sqlx::query(
            "SELECT CAST(identity_json AS CHAR) AS identity_json, \
                    status, \
                    CAST(capacity_json AS CHAR) AS capacity_json, \
                    CAST(advertisement_json AS CHAR) AS advertisement_json, \
                    CAST(rpc_endpoint_json AS CHAR) AS rpc_endpoint_json, \
                    CAST(active_session_ids_json AS CHAR) AS active_session_ids_json, \
                    lease_expires_at_ms, \
                    owner_id \
             FROM runner_pool \
             WHERE lease_expires_at_ms <= ? \
             ORDER BY lease_expires_at_ms ASC, runner_id ASC \
             LIMIT ?",
        )
        .bind(lease_expires_before_ms)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| format!("runner_pool reaper select: {error}"))?;
        rows.into_iter().map(runner_entry_from_row).collect()
    }

    async fn delete_if_lease_matches(
        &self,
        runner_id: &str,
        lease_expires_at_ms: i64,
    ) -> Result<bool, String> {
        let rows = sqlx::query(
            "DELETE FROM runner_pool \
             WHERE runner_id = ? AND lease_expires_at_ms = ?",
        )
        .bind(runner_id)
        .bind(lease_expires_at_ms)
        .execute(&self.pool)
        .await
        .map_err(|error| format!("runner_pool reaper delete {runner_id}: {error}"))?
        .rows_affected();
        Ok(rows > 0)
    }

    async fn try_acquire_lease(
        &self,
        runner_id: &str,
        expected_lease_ms: i64,
        new_lease_ms: i64,
    ) -> Result<Option<StoredRunnerPoolEntry>, String> {
        let rows = sqlx::query(
            "UPDATE runner_pool \
             SET lease_expires_at_ms = ?, updated_at = NOW(6) \
             WHERE runner_id = ? AND lease_expires_at_ms = ?",
        )
        .bind(new_lease_ms)
        .bind(runner_id)
        .bind(expected_lease_ms)
        .execute(&self.pool)
        .await
        .map_err(|error| format!("runner_pool lease acquire {runner_id}: {error}"))?
        .rows_affected();
        if rows == 0 {
            return Ok(None);
        }
        // Re-read to return the updated entry.
        self.get(runner_id).await
    }
}

#[derive(Clone)]
pub(crate) struct ServerRunnerPool {
    store: Arc<dyn RunnerPoolStore>,
    scheduler: RunnerScheduler,
    http_client: reqwest::Client,
    endpoint_policy: RunnerEndpointTrustPolicy,
}

#[async_trait]
pub(crate) trait ServerRunnerScheduler: Send + Sync {
    async fn schedule_for_user(
        &self,
        user_id: &str,
        request: RunnerScheduleRequest,
    ) -> Result<RunnerScheduleDecision, String>;
}

impl ServerRunnerPool {
    pub(crate) fn new(scheduler: RunnerScheduler) -> Result<Self, reqwest::Error> {
        Self::with_endpoint_trust_policy(scheduler, RunnerEndpointTrustPolicy::from_env())
    }

    pub(crate) fn with_endpoint_trust_policy(
        scheduler: RunnerScheduler,
        endpoint_policy: RunnerEndpointTrustPolicy,
    ) -> Result<Self, reqwest::Error> {
        Self::with_store(
            scheduler,
            endpoint_policy,
            Arc::new(InMemoryRunnerPoolStore::default()),
        )
    }

    fn with_store(
        scheduler: RunnerScheduler,
        endpoint_policy: RunnerEndpointTrustPolicy,
        store: Arc<dyn RunnerPoolStore>,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            store,
            scheduler,
            http_client: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(120))
                .build()?,
            endpoint_policy,
        })
    }

    pub(crate) async fn database(
        shared_pool: &SharedPool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let store = DatabaseRunnerPoolStore::new(shared_pool);
        store.ensure_tables().await?;
        Ok(Self::with_store(
            RunnerScheduler::default(),
            RunnerEndpointTrustPolicy::from_env(),
            Arc::new(store),
        )?)
    }

    pub(crate) async fn register_for_user(
        &self,
        user_id: &str,
        mut request: RunnerRegisterRequest,
    ) -> Result<RunnerRegisterResponse, String> {
        let runner_id = request.identity.runner_id.clone();
        if let Err(denial) = authorize_user_runner_identity(user_id, &request.identity) {
            return Ok(RunnerRegisterResponse::denied(runner_id, denial));
        }
        if let Some(endpoint) = request.rpc_endpoint.as_ref() {
            match self.endpoint_policy.validate(endpoint) {
                Ok(normalized) => {
                    request.rpc_endpoint = Some(RunnerRpcEndpoint::new(normalized));
                }
                Err(denial) => {
                    return Ok(RunnerRegisterResponse::denied(runner_id, denial));
                }
            }
        }

        match RunnerPoolEntry::from_register_request(request, RunnerStatus::Idle) {
            Ok(entry) => {
                self.store
                    .upsert(stored_with_fresh_lease(entry, user_id.to_string()))
                    .await?;
                Ok(RunnerRegisterResponse::accepted(
                    runner_id,
                    RUNNER_LEASE_TTL_SECS,
                ))
            }
            Err(denial) => Ok(RunnerRegisterResponse::denied(runner_id, denial)),
        }
    }

    pub(crate) async fn register_for_operator(
        &self,
        mut request: RunnerRegisterRequest,
    ) -> Result<RunnerRegisterResponse, String> {
        let runner_id = request.identity.runner_id.clone();
        if let Err(denial) = authorize_operator_runner_identity(&request.identity) {
            return Ok(RunnerRegisterResponse::denied(runner_id, denial));
        }
        if let Some(endpoint) = request.rpc_endpoint.as_ref() {
            match self.endpoint_policy.validate(endpoint) {
                Ok(normalized) => {
                    request.rpc_endpoint = Some(RunnerRpcEndpoint::new(normalized));
                }
                Err(denial) => {
                    return Ok(RunnerRegisterResponse::denied(runner_id, denial));
                }
            }
        }

        match RunnerPoolEntry::from_register_request(request, RunnerStatus::Idle) {
            Ok(entry) => {
                self.store
                    .upsert(stored_with_fresh_lease(
                        entry,
                        SHARED_RUNNER_POOL_OWNER_ID.to_string(),
                    ))
                    .await?;
                Ok(RunnerRegisterResponse::accepted(
                    runner_id,
                    RUNNER_LEASE_TTL_SECS,
                ))
            }
            Err(denial) => Ok(RunnerRegisterResponse::denied(runner_id, denial)),
        }
    }

    pub(crate) async fn heartbeat_for_user(
        &self,
        user_id: &str,
        heartbeat: RunnerHeartbeat,
    ) -> Result<RunnerAckResponse, String> {
        let Some(existing) = self.store.get(&heartbeat.runner_id).await? else {
            return Ok(RunnerAckResponse::Rejected {
                error: RuntimeError::runtime_unavailable(format!(
                    "runner '{}' is not registered",
                    heartbeat.runner_id
                )),
            });
        };
        if let Err(denial) = authorize_user_runner_identity(user_id, &existing.entry.identity) {
            return Ok(RunnerAckResponse::Rejected {
                error: runner_denial_to_runtime_error(denial),
            });
        }

        let validation = RunnerRegisterRequest::new(
            existing.entry.identity.clone(),
            heartbeat.capacity,
            heartbeat.advertisement.clone(),
        )
        .validate();
        if let Err(denial) = validation {
            return Ok(RunnerAckResponse::Rejected {
                error: runner_denial_to_runtime_error(denial),
            });
        }

        let updated = RunnerPoolEntry::new(
            existing.entry.identity.clone(),
            heartbeat.status,
            heartbeat.capacity,
            heartbeat.advertisement,
        )
        .with_rpc_endpoint(existing.entry.rpc_endpoint.clone());
        self.store
            .upsert(stored_with_fresh_lease_and_sessions(
                updated,
                existing.pool_owner_id,
                heartbeat.active_session_ids,
            ))
            .await?;
        Ok(RunnerAckResponse::Accepted)
    }

    pub(crate) async fn heartbeat_for_operator(
        &self,
        heartbeat: RunnerHeartbeat,
    ) -> Result<RunnerAckResponse, String> {
        let Some(existing) = self.store.get(&heartbeat.runner_id).await? else {
            return Ok(RunnerAckResponse::Rejected {
                error: RuntimeError::runtime_unavailable(format!(
                    "runner '{}' is not registered",
                    heartbeat.runner_id
                )),
            });
        };
        if existing.pool_owner_id != SHARED_RUNNER_POOL_OWNER_ID {
            return Ok(RunnerAckResponse::Rejected {
                error: runner_denial_to_runtime_error(RunnerDenial::new(
                    RunnerDenialReason::AuthenticationFailed,
                    "operator heartbeat is only valid for shared runner pool entries",
                )),
            });
        }
        if let Err(denial) = authorize_operator_runner_identity(&existing.entry.identity) {
            return Ok(RunnerAckResponse::Rejected {
                error: runner_denial_to_runtime_error(denial),
            });
        }

        let validation = RunnerRegisterRequest::new(
            existing.entry.identity.clone(),
            heartbeat.capacity,
            heartbeat.advertisement.clone(),
        )
        .validate();
        if let Err(denial) = validation {
            return Ok(RunnerAckResponse::Rejected {
                error: runner_denial_to_runtime_error(denial),
            });
        }

        let updated = RunnerPoolEntry::new(
            existing.entry.identity.clone(),
            heartbeat.status,
            heartbeat.capacity,
            heartbeat.advertisement,
        )
        .with_rpc_endpoint(existing.entry.rpc_endpoint.clone());
        self.store
            .upsert(stored_with_fresh_lease_and_sessions(
                updated,
                SHARED_RUNNER_POOL_OWNER_ID.to_string(),
                heartbeat.active_session_ids,
            ))
            .await?;
        Ok(RunnerAckResponse::Accepted)
    }

    pub(crate) async fn list_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<RunnerPoolEntry>, String> {
        let now_ms = now_epoch_ms();
        Ok(self
            .store
            .list_for_owner(user_id)
            .await?
            .into_iter()
            .map(|stored| entry_for_visibility(stored, now_ms))
            .collect())
    }

    pub(crate) async fn schedule_for_user(
        &self,
        user_id: &str,
        request: RunnerScheduleRequest,
    ) -> Result<RunnerScheduleDecision, String> {
        // Outer backoff loop: when all runners report capacity exhausted,
        // wait with exponential backoff and retry instead of immediately
        // rejecting.  This avoids unnecessary fallback to local execution
        // when a runner slot frees up within a short window.
        let mut backoff_ms = RUNNER_SCHEDULE_BACKOFF_INITIAL_MS;
        let mut total_waited_ms: u64 = 0;
        let mut attempt: usize = 0;

        loop {
            let now_ms = now_epoch_ms();
            let stored_candidates = self.store.list_for_owner(user_id).await?;
            let mut transport_denials = Vec::new();
            struct Candidate {
                arc: Arc<RunnerPoolEntry>,
                lease_expires_at_ms: i64,
            }
            let mut survivors: Vec<Candidate> = Vec::new();
            let mut lease_map: std::collections::HashMap<String, i64> =
                std::collections::HashMap::new();
            for stored in stored_candidates {
                let expired = stored_is_expired(&stored, now_ms);
                let runner_id = stored.entry.identity.runner_id.clone();
                if expired {
                    tracing::debug!(
                        runner_id = %runner_id,
                        lease_expires_at_ms = stored.lease_expires_at_ms,
                        now_ms,
                        "skipping expired runner in schedule_for_user"
                    );
                    continue;
                }
                match stored
                    .entry
                    .rpc_endpoint
                    .as_ref()
                    .map(|endpoint| self.endpoint_policy.validate(endpoint))
                {
                    Some(Ok(_)) => {
                        lease_map.insert(runner_id, stored.lease_expires_at_ms);
                        survivors.push(Candidate {
                            arc: stored.entry,
                            lease_expires_at_ms: stored.lease_expires_at_ms,
                        });
                    }
                    Some(Err(denial)) => transport_denials.push(RunnerScheduleDenial {
                        runner_id,
                        reason: RunnerScheduleDenialReason::TransportUnavailable,
                        message: denial.message,
                    }),
                    None => transport_denials.push(RunnerScheduleDenial {
                        runner_id,
                        reason: RunnerScheduleDenialReason::TransportUnavailable,
                        message: "runner has no RPC endpoint for server transport".to_string(),
                    }),
                }
            }

            // Fast path: no runners at all — reject immediately (no amount
            // of waiting will conjure a runner that doesn't exist).
            if survivors.is_empty() && transport_denials.is_empty() {
                return Ok(RunnerScheduleDecision::rejected(Vec::new()));
            }

            let mut schedulable: Vec<RunnerPoolEntry> = survivors
                .into_iter()
                .map(|c| {
                    let mut entry = Arc::try_unwrap(c.arc).unwrap_or_else(|arc| (*arc).clone());
                    entry.lease = Some(astra_runtime_env::RunnerLease {
                        expires_at_ms: c.lease_expires_at_ms,
                    });
                    entry
                })
                .collect();

            const MAX_ACQUIRE_RETRIES: usize = 3;
            let mut schedule_denials: Vec<RunnerScheduleDenial> = Vec::new();
            for _ in 0..MAX_ACQUIRE_RETRIES {
                let mut decision = self
                    .scheduler
                    .schedule_at_ms(&request, &schedulable, now_ms);

                if let Some(ref target) = decision.selected {
                    let expected = lease_map.get(&target.runner_id).copied().unwrap_or(0);
                    let new_lease = fresh_lease_expires_at_ms();
                    match self
                        .store
                        .try_acquire_lease(&target.runner_id, expected, new_lease)
                        .await
                    {
                        Ok(Some(_)) => {
                            if !transport_denials.is_empty() {
                                transport_denials.extend(decision.denials);
                                decision.denials = transport_denials;
                            }
                            if !schedule_denials.is_empty() {
                                schedule_denials.extend(decision.denials);
                                decision.denials = schedule_denials;
                            }
                            if attempt > 0 {
                                tracing::info!(
                                    target: "astra_runtime::runner_pool",
                                    attempt = attempt + 1,
                                    total_waited_ms,
                                    runner_id = %target.runner_id,
                                    "runner acquired after backoff retry"
                                );
                            }
                            return Ok(decision);
                        }
                        Ok(None) => {
                            schedule_denials.push(RunnerScheduleDenial {
                                runner_id: target.runner_id.clone(),
                                reason: RunnerScheduleDenialReason::RunnerLeaseExpired,
                                message: "runner lease was consumed by another concurrent request"
                                    .to_string(),
                            });
                            schedulable.retain(|c| c.identity.runner_id != target.runner_id);
                            lease_map.remove(&target.runner_id);
                            continue;
                        }
                        Err(error) => {
                            schedule_denials.push(RunnerScheduleDenial {
                                runner_id: target.runner_id.clone(),
                                reason: RunnerScheduleDenialReason::TransportUnavailable,
                                message: format!("lease acquisition failed: {error}"),
                            });
                            schedulable.retain(|c| c.identity.runner_id != target.runner_id);
                            lease_map.remove(&target.runner_id);
                            continue;
                        }
                    }
                }

                // Merge transport denials into the decision for a complete picture.
                let mut merged = decision;
                if !transport_denials.is_empty() {
                    merged.denials.extend(transport_denials.clone());
                }
                if !schedule_denials.is_empty() {
                    merged.denials.extend(schedule_denials.clone());
                }

                // Check if all denials are capacity-related — only then is retry useful.
                let all_capacity = !merged.denials.is_empty()
                    && merged.denials.iter().all(|d| {
                        matches!(
                            d.reason,
                            RunnerScheduleDenialReason::RunnerCapacityExhausted
                        )
                    });

                if all_capacity
                    && attempt < RUNNER_SCHEDULE_BACKOFF_MAX_RETRIES
                    && total_waited_ms < RUNNER_SCHEDULE_BACKOFF_MAX_MS
                {
                    let delay = Duration::from_millis(backoff_ms);
                    tracing::debug!(
                        target: "astra_runtime::runner_pool",
                        attempt = attempt + 1,
                        delay_ms = backoff_ms,
                        total_waited_ms,
                        denials_count = merged.denials.len(),
                        "backoff: all runners at capacity, waiting before retry"
                    );
                    tokio::time::sleep(delay).await;
                    total_waited_ms += backoff_ms;
                    backoff_ms = (backoff_ms * 2).min(RUNNER_SCHEDULE_BACKOFF_MAX_MS);
                    attempt += 1;
                    break; // restart outer loop with fresh candidate list
                }

                // Not capacity-related, or out of retries — return rejection.
                return Ok(merged);
            }

            // Exhausted acquire retries without success — check once more
            // for capacity-backoff eligibility.
            if attempt >= RUNNER_SCHEDULE_BACKOFF_MAX_RETRIES
                || total_waited_ms >= RUNNER_SCHEDULE_BACKOFF_MAX_MS
            {
                let final_denial =
                    RunnerScheduleDecision::rejected(if !transport_denials.is_empty() {
                        transport_denials
                    } else {
                        schedule_denials
                    });
                return Ok(final_denial);
            }
            // Otherwise: break hit → restart outer loop with backoff delay already applied.
            continue;
        }
    }

    async fn runner_endpoint(&self, executor_id: &str) -> Result<String, RuntimeError> {
        let entry = self.store.get(executor_id).await.map_err(|error| {
            RuntimeError::runner_protocol(format!("runner pool lookup failed: {error}"))
        })?;
        let Some(entry) = entry else {
            return Err(RuntimeError::runtime_unavailable(format!(
                "runner '{executor_id}' is not registered"
            )));
        };
        if stored_is_expired(&entry, now_epoch_ms()) {
            return Err(RuntimeError::runner_lease_expired(format!(
                "runner '{executor_id}' lease has expired"
            )));
        }
        let Some(endpoint) = entry.entry.rpc_endpoint.as_ref() else {
            return Err(RuntimeError::transport_unavailable(format!(
                "runner '{executor_id}' has no RPC endpoint"
            )));
        };
        self.endpoint_policy
            .validate(endpoint)
            .map_err(runner_denial_to_runtime_error)
    }
}

#[async_trait]
impl ServerRunnerScheduler for ServerRunnerPool {
    async fn schedule_for_user(
        &self,
        user_id: &str,
        request: RunnerScheduleRequest,
    ) -> Result<RunnerScheduleDecision, String> {
        ServerRunnerPool::schedule_for_user(self, user_id, request).await
    }
}

#[async_trait]
impl crate::server::tool_transport::RunnerRpcTransport for ServerRunnerPool {
    async fn prepare_session(
        &self,
        executor_id: &str,
        request: RunnerPrepareSessionRequest,
    ) -> Result<RunnerPrepareSessionResponse, RuntimeError> {
        let endpoint = self.runner_endpoint(executor_id).await?;
        self.http_client
            .post(format!("{endpoint}/v1/sessions/prepare"))
            .json(&request)
            .send()
            .await
            .map_err(|error| {
                RuntimeError::runner_protocol(format!("runner prepare request failed: {error}"))
            })?
            .error_for_status()
            .map_err(|error| {
                RuntimeError::runner_protocol(format!("runner prepare HTTP error: {error}"))
            })?
            .json::<RunnerPrepareSessionResponse>()
            .await
            .map_err(|error| {
                RuntimeError::runner_protocol(format!(
                    "runner prepare response decode failed: {error}"
                ))
            })
    }

    async fn execute_tool(
        &self,
        executor_id: &str,
        request: RunnerExecuteToolRequest,
    ) -> Result<RunnerExecuteToolResponse, RuntimeError> {
        let endpoint = self.runner_endpoint(executor_id).await?;
        self.http_client
            .post(format!("{endpoint}/v1/tools/execute"))
            .json(&request)
            .send()
            .await
            .map_err(|error| {
                RuntimeError::runner_protocol(format!("runner execute request failed: {error}"))
            })?
            .error_for_status()
            .map_err(|error| {
                RuntimeError::runner_protocol(format!("runner execute HTTP error: {error}"))
            })?
            .json::<RunnerExecuteToolResponse>()
            .await
            .map_err(|error| {
                RuntimeError::runner_protocol(format!(
                    "runner execute response decode failed: {error}"
                ))
            })
    }
}

impl Default for ServerRunnerPool {
    fn default() -> Self {
        Self::new(RunnerScheduler::default()).expect("default HTTP client build")
    }
}

pub(crate) async fn run_runner_pool_reaper_once(
    shared_pool: SharedPool,
    retention: Duration,
    limit: u64,
) -> Result<u64, String> {
    let store = DatabaseRunnerPoolStore::new(&shared_pool);
    store
        .ensure_tables()
        .await
        .map_err(|error| format!("runner_pool reaper ensure tables: {error}"))?;
    let cleanup_store = DatabaseWorkspaceRecordStore::new(shared_pool);
    cleanup_store
        .ensure_tables()
        .await
        .map_err(|error| format!("runner_pool reaper ensure cleanup tables: {error}"))?;
    reap_expired_runner_entries_with_cleanup_debt(&store, Some(&cleanup_store), retention, limit)
        .await
}

pub(crate) fn spawn_runner_pool_reaper(
    shared_pool: SharedPool,
    lease: Arc<crate::server::sweeper_lease::SweeperLease>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(Duration::from_secs(RUNNER_POOL_REAPER_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    'tick: {
                        match lease.check_leader().await {
                            crate::server::sweeper_lease::LeaderStatus::Leader => {}
                            crate::server::sweeper_lease::LeaderStatus::NotLeader => break 'tick,
                            crate::server::sweeper_lease::LeaderStatus::Unavailable(error) => {
                                tracing::warn!(
                                    target: "astra_runtime::runner_pool_reaper",
                                    error = %error,
                                    "sweeper lease check unavailable, skipping runner pool reaper"
                                );
                                break 'tick;
                            }
                        }

                        match run_runner_pool_reaper_once(
                            shared_pool.clone(),
                            runner_pool_reaper_retention(),
                            RUNNER_POOL_REAPER_BATCH_LIMIT,
                        )
                        .await
                        {
                            Ok(0) => {}
                            Ok(deleted) => {
                                tracing::info!(
                                    target: "astra_runtime::runner_pool_reaper",
                                    deleted = deleted,
                                    "removed stale expired runner pool entries"
                                );
                            }
                            Err(error) => {
                                tracing::warn!(
                                    target: "astra_runtime::runner_pool_reaper",
                                    error = %error,
                                    "runner pool reaper failed"
                                );
                            }
                        }
                    }
                }
            }
        }
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RunnerEndpointTrustPolicy {
    mode: RunnerEndpointTrustMode,
    allowed_hosts: Option<Vec<RunnerEndpointHostPattern>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunnerEndpointTrustMode {
    PublicHttps,
    LocalDevelopment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RunnerEndpointHostPattern {
    Exact(String),
    Suffix(String),
}

impl RunnerEndpointTrustPolicy {
    pub(crate) fn public_https() -> Self {
        Self {
            mode: RunnerEndpointTrustMode::PublicHttps,
            allowed_hosts: None,
        }
    }

    pub(crate) fn local_development() -> Self {
        Self {
            mode: RunnerEndpointTrustMode::LocalDevelopment,
            allowed_hosts: None,
        }
    }

    fn from_env() -> Self {
        let mut policy = match std::env::var("ASTRA_RUNNER_ENDPOINT_TRUST")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "local" | "local_dev" | "local_development" | "insecure_local" => {
                Self::local_development()
            }
            _ => Self::public_https(),
        };
        if matches!(policy.mode, RunnerEndpointTrustMode::PublicHttps) {
            policy.allowed_hosts = runner_endpoint_allowed_hosts_from_env();
        }
        policy
    }

    #[cfg(test)]
    fn public_https_with_allowed_hosts(hosts: &[&str]) -> Self {
        Self {
            mode: RunnerEndpointTrustMode::PublicHttps,
            allowed_hosts: Some(
                hosts
                    .iter()
                    .map(|host| runner_endpoint_host_pattern(host))
                    .collect(),
            ),
        }
    }

    fn validate(&self, endpoint: &RunnerRpcEndpoint) -> Result<String, RunnerDenial> {
        let parsed = parse_runner_rpc_endpoint(endpoint)?;
        match self.mode {
            RunnerEndpointTrustMode::PublicHttps => {
                validate_public_https_runner_endpoint(&parsed, self.allowed_hosts.as_deref())?
            }
            RunnerEndpointTrustMode::LocalDevelopment => {}
        }
        Ok(parsed.as_str().trim_end_matches('/').to_string())
    }
}

#[derive(Serialize)]
pub(crate) struct RunnerListResponse {
    pub(crate) runners: Vec<RunnerPoolEntry>,
}

pub(crate) async fn post_runner_register_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RunnerRegisterRequest>,
) -> Result<Json<RunnerRegisterResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .runner_pool
        .register_for_user(&user.user_id, body)
        .await
        .map(Json)
        .map_err(runner_pool_internal_error)
}

pub(crate) async fn post_runner_operator_register_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RunnerRegisterRequest>,
) -> Result<Json<RunnerRegisterResponse>, (StatusCode, Json<ErrorResponse>)> {
    state.admin.authorizer.require_admin(&headers).await?;
    state
        .runner_pool
        .register_for_operator(body)
        .await
        .map(Json)
        .map_err(runner_pool_internal_error)
}

pub(crate) async fn post_runner_heartbeat_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RunnerHeartbeat>,
) -> Result<Json<RunnerAckResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .runner_pool
        .heartbeat_for_user(&user.user_id, body)
        .await
        .map(Json)
        .map_err(runner_pool_internal_error)
}

pub(crate) async fn post_runner_operator_heartbeat_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RunnerHeartbeat>,
) -> Result<Json<RunnerAckResponse>, (StatusCode, Json<ErrorResponse>)> {
    state.admin.authorizer.require_admin(&headers).await?;
    state
        .runner_pool
        .heartbeat_for_operator(body)
        .await
        .map(Json)
        .map_err(runner_pool_internal_error)
}

pub(crate) async fn list_runners_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<RunnerListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .runner_pool
        .list_for_user(&user.user_id)
        .await
        .map(|runners| Json(RunnerListResponse { runners }))
        .map_err(runner_pool_internal_error)
}

pub(crate) async fn post_runner_schedule_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RunnerScheduleRequest>,
) -> Result<Json<RunnerScheduleDecision>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .runner_pool
        .schedule_for_user(&user.user_id, body)
        .await
        .map(Json)
        .map_err(runner_pool_internal_error)
}

fn runner_pool_internal_error(error: String) -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("runner pool storage error: {error}"),
    )
}

fn authorize_user_runner_identity(
    user_id: &str,
    identity: &RunnerIdentity,
) -> Result<(), RunnerDenial> {
    if identity.deployment == RunnerDeploymentKind::Personal
        && runner_owned_by_user(identity, user_id)
    {
        return Ok(());
    }
    Err(RunnerDenial::new(
        RunnerDenialReason::AuthenticationFailed,
        "personal runner registration requires personal deployment and owner_id matching the authenticated user",
    ))
}

fn runner_owned_by_user(identity: &RunnerIdentity, user_id: &str) -> bool {
    identity.owner_id.as_deref() == Some(user_id)
}

fn authorize_operator_runner_identity(identity: &RunnerIdentity) -> Result<(), RunnerDenial> {
    if identity.owner_id.is_some() {
        return Err(RunnerDenial::new(
            RunnerDenialReason::AuthenticationFailed,
            "operator runner registration requires an ownerless runner identity",
        ));
    }
    match identity.deployment {
        RunnerDeploymentKind::HostedPool
        | RunnerDeploymentKind::EnterpriseDedicated
        | RunnerDeploymentKind::EnterpriseShared
        | RunnerDeploymentKind::EphemeralJob => Ok(()),
        RunnerDeploymentKind::Personal => Err(RunnerDenial::new(
            RunnerDenialReason::AuthenticationFailed,
            "personal runner registration must use the user-owned runner endpoint",
        )),
        _ => Err(RunnerDenial::new(
            RunnerDenialReason::AuthenticationFailed,
            "unknown deployment kind rejected for operator runner",
        )),
    }
}

fn runner_pool_entry_visible_to_user(entry: &StoredRunnerPoolEntry, user_id: &str) -> bool {
    entry.pool_owner_id == user_id || entry.pool_owner_id == SHARED_RUNNER_POOL_OWNER_ID
}

fn now_epoch_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn fresh_lease_expires_at_ms() -> i64 {
    now_epoch_ms().saturating_add(RUNNER_LEASE_TTL_MS)
}

fn runner_pool_reaper_retention() -> Duration {
    std::env::var("ASTRA_RUNNER_POOL_REAPER_RETENTION_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(RUNNER_POOL_REAPER_RETENTION_SECS))
}

fn duration_millis_i64(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn stale_runner_cutoff_ms(retention: Duration, now_ms: i64) -> i64 {
    now_ms.saturating_sub(duration_millis_i64(retention))
}

#[cfg(test)]
async fn reap_expired_runner_entries(
    store: &dyn RunnerPoolStore,
    retention: Duration,
    limit: u64,
) -> Result<u64, String> {
    reap_expired_runner_entries_with_cleanup_debt(store, None, retention, limit).await
}

async fn reap_expired_runner_entries_with_cleanup_debt(
    store: &dyn RunnerPoolStore,
    cleanup_store: Option<&dyn WorkspaceCleanupDebtStore>,
    retention: Duration,
    limit: u64,
) -> Result<u64, String> {
    let cutoff_ms = stale_runner_cutoff_ms(retention, now_epoch_ms());
    let expired = store.list_expired_before(cutoff_ms, limit).await?;
    let mut deleted = 0u64;
    for stored in expired {
        record_runner_cleanup_debts(cleanup_store, &stored).await?;
        if store
            .delete_if_lease_matches(&stored.entry.identity.runner_id, stored.lease_expires_at_ms)
            .await?
        {
            deleted = deleted.saturating_add(1);
        }
    }
    Ok(deleted)
}

fn stored_with_fresh_lease(
    entry: RunnerPoolEntry,
    pool_owner_id: impl Into<String>,
) -> StoredRunnerPoolEntry {
    stored_with_fresh_lease_and_sessions(entry, pool_owner_id, Vec::new())
}

fn stored_with_fresh_lease_and_sessions(
    entry: RunnerPoolEntry,
    pool_owner_id: impl Into<String>,
    active_session_ids: Vec<String>,
) -> StoredRunnerPoolEntry {
    let lease_expires_at_ms = fresh_lease_expires_at_ms();
    StoredRunnerPoolEntry {
        pool_owner_id: pool_owner_id.into(),
        entry: Arc::new(entry.with_lease_expires_at_ms(lease_expires_at_ms)),
        lease_expires_at_ms,
        active_session_ids,
    }
}

fn stored_is_expired(entry: &StoredRunnerPoolEntry, now_ms: i64) -> bool {
    entry.lease_expires_at_ms <= now_ms
}

fn entry_for_visibility(stored: StoredRunnerPoolEntry, now_ms: i64) -> RunnerPoolEntry {
    let StoredRunnerPoolEntry {
        entry,
        pool_owner_id: _,
        lease_expires_at_ms,
        active_session_ids: _,
    } = stored;
    let entry = Arc::try_unwrap(entry).unwrap_or_else(|arc| (*arc).clone());
    let expired = lease_expires_at_ms <= now_ms;
    if expired {
        RunnerPoolEntry::new(
            entry.identity,
            RunnerStatus::Offline,
            entry.capacity,
            entry.advertisement,
        )
        .with_rpc_endpoint(entry.rpc_endpoint)
        .with_lease_expires_at_ms(lease_expires_at_ms)
    } else {
        entry.with_lease_expires_at_ms(lease_expires_at_ms)
    }
}

async fn record_runner_cleanup_debts(
    cleanup_store: Option<&dyn WorkspaceCleanupDebtStore>,
    stored: &StoredRunnerPoolEntry,
) -> Result<(), String> {
    let Some(cleanup_store) = cleanup_store else {
        return Ok(());
    };
    for debt in runner_cleanup_debts(stored) {
        cleanup_store
            .record_cleanup_debt(debt)
            .await
            .map_err(|error| format!("runner cleanup debt record failed: {error}"))?;
    }
    Ok(())
}

fn runner_cleanup_debts(stored: &StoredRunnerPoolEntry) -> Vec<WorkspaceCleanupDebtEntry> {
    let Some(record) = runner_workspace_record_for_cleanup(stored) else {
        return Vec::new();
    };
    let session_ids = if stored.active_session_ids.is_empty() {
        vec![None]
    } else {
        stored
            .active_session_ids
            .iter()
            .map(|session_id| Some(session_id.clone()))
            .collect()
    };

    session_ids
        .into_iter()
        .map(|session_id| {
            let mut debt = WorkspaceCleanupDebtEntry::new(
                stored.pool_owner_id.clone(),
                session_id.clone(),
                None,
                record.clone(),
                astra_runtime_env::CleanupReason::LeaseExpired,
                runner_cleanup_debt_message(stored, session_id.as_deref()),
            );
            debt.debt_id =
                runner_cleanup_debt_id(stored, &record.workspace_id, session_id.as_deref());
            debt
        })
        .collect()
}

fn runner_workspace_record_for_cleanup(stored: &StoredRunnerPoolEntry) -> Option<WorkspaceRecord> {
    let workspace = &stored.entry.advertisement.binding.workspace;
    if workspace.authority == WorkspaceAuthority::None
        || workspace.kind == WorkspaceBindingKind::None
    {
        return None;
    }
    if !matches!(
        workspace.kind,
        WorkspaceBindingKind::CloudWorkspace | WorkspaceBindingKind::ServerSandbox
    ) {
        return None;
    }
    let root_or_volume_ref = workspace.cwd.clone()?;
    let runner_id = &stored.entry.identity.runner_id;
    let first_session_id = stored.active_session_ids.first().cloned();
    Some(WorkspaceRecord {
        workspace_id: synthetic_runner_workspace_id(runner_id, &root_or_volume_ref),
        owner_scope: WorkspaceOwnerScope::Runner,
        kind: workspace.kind,
        authority: workspace.authority,
        root_or_volume_ref,
        source: runner_workspace_source(workspace.kind, runner_id, first_session_id.as_deref()),
        persistence: if workspace.persistent {
            WorkspacePersistence::Session
        } else {
            WorkspacePersistence::Ephemeral
        },
        revision: format!("runner-lease-expired-{}", stored.lease_expires_at_ms),
        display_name: workspace.display_name.clone(),
    })
}

fn runner_workspace_source(
    kind: WorkspaceBindingKind,
    runner_id: &str,
    session_id: Option<&str>,
) -> WorkspaceSource {
    match kind {
        WorkspaceBindingKind::ServerSandbox => WorkspaceSource::ServerSandbox {
            session_id: session_id.unwrap_or(runner_id).to_string(),
        },
        _ => WorkspaceSource::ProviderManaged {
            provider: "runner_pool".to_string(),
            reference: runner_id.to_string(),
        },
    }
}

fn synthetic_runner_workspace_id(runner_id: &str, root_or_volume_ref: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(runner_id.as_bytes());
    hasher.update([0]);
    hasher.update(root_or_volume_ref.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("runner-ws-{}", &digest[..16])
}

fn runner_cleanup_debt_id(
    stored: &StoredRunnerPoolEntry,
    workspace_id: &str,
    session_id: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(stored.entry.identity.runner_id.as_bytes());
    hasher.update([0]);
    hasher.update(workspace_id.as_bytes());
    hasher.update([0]);
    hasher.update(session_id.unwrap_or("unknown-session").as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("runner-lease-expired-{workspace_id}-{}", &digest[..16])
}

fn runner_cleanup_debt_message(stored: &StoredRunnerPoolEntry, session_id: Option<&str>) -> String {
    match session_id {
        Some(session_id) => format!(
            "runner '{}' lease expired while session '{}' was active; verify runtime and workspace cleanup before deleting underlying resources",
            stored.entry.identity.runner_id, session_id
        ),
        None => format!(
            "runner '{}' lease expired while advertising a managed workspace; verify runtime and workspace cleanup before deleting underlying resources",
            stored.entry.identity.runner_id
        ),
    }
}

fn runner_status_to_db(status: RunnerStatus) -> &'static str {
    match status {
        RunnerStatus::Starting => "starting",
        RunnerStatus::Idle => "idle",
        RunnerStatus::Busy => "busy",
        RunnerStatus::Draining => "draining",
        RunnerStatus::Offline => "offline",
        RunnerStatus::Degraded => "degraded",
    }
}

fn runner_status_from_db(raw: &str) -> Result<RunnerStatus, String> {
    match raw {
        "starting" => Ok(RunnerStatus::Starting),
        "idle" => Ok(RunnerStatus::Idle),
        "busy" => Ok(RunnerStatus::Busy),
        "draining" => Ok(RunnerStatus::Draining),
        "offline" => Ok(RunnerStatus::Offline),
        "degraded" => Ok(RunnerStatus::Degraded),
        other => Err(format!("unknown runner status '{other}'")),
    }
}

fn runner_entry_from_row(row: sqlx::mysql::MySqlRow) -> Result<StoredRunnerPoolEntry, String> {
    let identity_json: String = row
        .try_get("identity_json")
        .map_err(|error: sqlx::Error| format!("runner identity column: {error}"))?;
    let status: String = row
        .try_get("status")
        .map_err(|error: sqlx::Error| format!("runner status column: {error}"))?;
    let capacity_json: String = row
        .try_get("capacity_json")
        .map_err(|error: sqlx::Error| format!("runner capacity column: {error}"))?;
    let advertisement_json: String = row
        .try_get("advertisement_json")
        .map_err(|error: sqlx::Error| format!("runner advertisement column: {error}"))?;
    let rpc_endpoint_json: Option<String> = row
        .try_get("rpc_endpoint_json")
        .map_err(|error: sqlx::Error| format!("runner rpc endpoint column: {error}"))?;
    let active_session_ids_json: Option<String> = row
        .try_get("active_session_ids_json")
        .map_err(|error: sqlx::Error| format!("runner active sessions column: {error}"))?;
    let lease_expires_at_ms: i64 = row
        .try_get("lease_expires_at_ms")
        .map_err(|error: sqlx::Error| format!("runner lease column: {error}"))?;
    let pool_owner_id: String = row
        .try_get("owner_id")
        .map_err(|error: sqlx::Error| format!("runner owner column: {error}"))?;

    let identity: RunnerIdentity = serde_json::from_str(&identity_json)
        .map_err(|error| format!("runner identity decode: {error}"))?;
    let capacity: RunnerCapacity = serde_json::from_str(&capacity_json)
        .map_err(|error| format!("runner capacity decode: {error}"))?;
    let advertisement: RuntimeEnvironmentAdvertisement = serde_json::from_str(&advertisement_json)
        .map_err(|error| format!("runner advertisement decode: {error}"))?;
    let rpc_endpoint = rpc_endpoint_json
        .filter(|raw| !raw.trim().is_empty())
        .map(|raw| serde_json::from_str::<RunnerRpcEndpoint>(&raw))
        .transpose()
        .map_err(|error| format!("runner rpc endpoint decode: {error}"))?;
    let active_session_ids = active_session_ids_json
        .filter(|raw| !raw.trim().is_empty())
        .map(|raw| serde_json::from_str::<Vec<String>>(&raw))
        .transpose()
        .map_err(|error| format!("runner active sessions decode: {error}"))?
        .unwrap_or_default();

    Ok(StoredRunnerPoolEntry {
        pool_owner_id,
        entry: Arc::new(
            RunnerPoolEntry::new(
                identity,
                runner_status_from_db(status.as_str())?,
                capacity,
                advertisement,
            )
            .with_rpc_endpoint(rpc_endpoint)
            .with_lease_expires_at_ms(lease_expires_at_ms),
        ),
        lease_expires_at_ms,
        active_session_ids,
    })
}

/// Lighter row parser that skips active_session_ids_json (not needed by list_for_owner callers).
fn runner_entry_from_row_no_sessions(
    row: sqlx::mysql::MySqlRow,
) -> Result<StoredRunnerPoolEntry, String> {
    let identity_json: String = row
        .try_get("identity_json")
        .map_err(|error: sqlx::Error| format!("runner identity column: {error}"))?;
    let status: String = row
        .try_get("status")
        .map_err(|error: sqlx::Error| format!("runner status column: {error}"))?;
    let capacity_json: String = row
        .try_get("capacity_json")
        .map_err(|error: sqlx::Error| format!("runner capacity column: {error}"))?;
    let advertisement_json: String = row
        .try_get("advertisement_json")
        .map_err(|error: sqlx::Error| format!("runner advertisement column: {error}"))?;
    let rpc_endpoint_json: Option<String> = row
        .try_get("rpc_endpoint_json")
        .map_err(|error: sqlx::Error| format!("runner rpc endpoint column: {error}"))?;
    let lease_expires_at_ms: i64 = row
        .try_get("lease_expires_at_ms")
        .map_err(|error: sqlx::Error| format!("runner lease column: {error}"))?;
    let pool_owner_id: String = row
        .try_get("owner_id")
        .map_err(|error: sqlx::Error| format!("runner owner column: {error}"))?;

    let identity: RunnerIdentity = serde_json::from_str(&identity_json)
        .map_err(|error| format!("runner identity decode: {error}"))?;
    let capacity: RunnerCapacity = serde_json::from_str(&capacity_json)
        .map_err(|error| format!("runner capacity decode: {error}"))?;
    let advertisement: RuntimeEnvironmentAdvertisement = serde_json::from_str(&advertisement_json)
        .map_err(|error| format!("runner advertisement decode: {error}"))?;
    let rpc_endpoint = rpc_endpoint_json
        .filter(|raw| !raw.trim().is_empty())
        .map(|raw| serde_json::from_str::<RunnerRpcEndpoint>(&raw))
        .transpose()
        .map_err(|error| format!("runner rpc endpoint decode: {error}"))?;

    Ok(StoredRunnerPoolEntry {
        pool_owner_id,
        entry: Arc::new(
            RunnerPoolEntry::new(
                identity,
                runner_status_from_db(status.as_str())?,
                capacity,
                advertisement,
            )
            .with_rpc_endpoint(rpc_endpoint)
            .with_lease_expires_at_ms(lease_expires_at_ms),
        ),
        lease_expires_at_ms,
        active_session_ids: Vec::new(),
    })
}

fn parse_runner_rpc_endpoint(endpoint: &RunnerRpcEndpoint) -> Result<reqwest::Url, RunnerDenial> {
    let normalized = endpoint.normalized_base_url();
    if normalized.is_empty() {
        return Err(RunnerDenial::new(
            RunnerDenialReason::InvalidEndpoint,
            "runner RPC endpoint must not be empty",
        ));
    }
    let parsed = reqwest::Url::parse(&normalized).map_err(|error| {
        RunnerDenial::new(
            RunnerDenialReason::InvalidEndpoint,
            format!("runner RPC endpoint must be a valid URL: {error}"),
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(RunnerDenial::new(
            RunnerDenialReason::InvalidEndpoint,
            "runner RPC endpoint must use http or https",
        ));
    }
    if parsed.host_str().is_none() {
        return Err(RunnerDenial::new(
            RunnerDenialReason::InvalidEndpoint,
            "runner RPC endpoint must include a host",
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(RunnerDenial::new(
            RunnerDenialReason::InvalidEndpoint,
            "runner RPC endpoint must not contain credentials",
        ));
    }
    if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(RunnerDenial::new(
            RunnerDenialReason::InvalidEndpoint,
            "runner RPC endpoint must be a base URL without path, query, or fragment",
        ));
    }
    Ok(parsed)
}

fn validate_public_https_runner_endpoint(
    parsed: &reqwest::Url,
    allowed_hosts: Option<&[RunnerEndpointHostPattern]>,
) -> Result<(), RunnerDenial> {
    if parsed.scheme() != "https" {
        return Err(RunnerDenial::new(
            RunnerDenialReason::InvalidEndpoint,
            "public runner RPC endpoint must use https",
        ));
    }
    let raw_host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    let host_without_trailing_dot = raw_host.trim_end_matches('.');
    if let Ok(ip) = host_without_trailing_dot.parse::<IpAddr>() {
        if !ip_is_allowed_public_runner_endpoint(ip) {
            return Err(RunnerDenial::new(
                RunnerDenialReason::InvalidEndpoint,
                "public runner RPC endpoint must not target private, loopback, link-local, multicast, or reserved IP ranges",
            ));
        }
        return Ok(());
    }

    let host = canonical_public_runner_dns_host(&raw_host)?;
    if host == "localhost" || host.ends_with(".localhost") {
        return Err(RunnerDenial::new(
            RunnerDenialReason::InvalidEndpoint,
            "public runner RPC endpoint must not target localhost",
        ));
    }
    if !host.contains('.') {
        return Err(RunnerDenial::new(
            RunnerDenialReason::InvalidEndpoint,
            "public runner RPC endpoint host must be fully qualified",
        ));
    }
    if host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".lan")
        || host.ends_with(".home")
    {
        return Err(RunnerDenial::new(
            RunnerDenialReason::InvalidEndpoint,
            "public runner RPC endpoint host must not use local/private DNS suffixes",
        ));
    }
    if let Some(allowed_hosts) = allowed_hosts {
        if !allowed_hosts
            .iter()
            .any(|pattern| pattern.matches(host.as_str()))
        {
            return Err(RunnerDenial::new(
                RunnerDenialReason::InvalidEndpoint,
                "public runner RPC endpoint host is not in the configured allowlist",
            ));
        }
    }
    Ok(())
}

fn canonical_public_runner_dns_host(raw_host: &str) -> Result<String, RunnerDenial> {
    let host = raw_host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host.len() > 253 {
        return Err(RunnerDenial::new(
            RunnerDenialReason::InvalidEndpoint,
            "public runner RPC endpoint host must be a valid DNS name",
        ));
    }
    for label in host.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(RunnerDenial::new(
                RunnerDenialReason::InvalidEndpoint,
                "public runner RPC endpoint host must be a valid DNS name",
            ));
        }
    }
    Ok(host)
}

fn runner_endpoint_allowed_hosts_from_env() -> Option<Vec<RunnerEndpointHostPattern>> {
    let raw = std::env::var("ASTRA_RUNNER_ENDPOINT_ALLOWED_HOSTS").ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(
        raw.split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(runner_endpoint_host_pattern)
            .collect(),
    )
}

fn runner_endpoint_host_pattern(raw: &str) -> RunnerEndpointHostPattern {
    let trimmed = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if let Some(suffix) = trimmed.strip_prefix("*.") {
        RunnerEndpointHostPattern::Suffix(suffix.to_string())
    } else if let Some(suffix) = trimmed.strip_prefix('.') {
        RunnerEndpointHostPattern::Suffix(suffix.to_string())
    } else {
        RunnerEndpointHostPattern::Exact(trimmed)
    }
}

impl RunnerEndpointHostPattern {
    fn matches(&self, host: &str) -> bool {
        match self {
            Self::Exact(exact) => host == exact,
            Self::Suffix(suffix) => {
                host.len() > suffix.len()
                    && host.ends_with(suffix)
                    && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
            }
        }
    }
}

fn ip_is_allowed_public_runner_endpoint(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || octets[0] == 0
                || octets[0] >= 240
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
                || (octets[0] == 198 && octets[1] == 18)
                || (octets[0] == 198 && octets[1] == 19)
                || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
                || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113))
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_multicast()
                || (segments[0] & 0xffc0) == 0xfe80
                || (segments[0] & 0xff00) == 0x0100
                || (segments[0] & 0xff00) == 0x0200)
        }
    }
}

fn runner_denial_to_runtime_error(denial: RunnerDenial) -> RuntimeError {
    match denial.reason {
        RunnerDenialReason::RuntimeUnavailable => RuntimeError::runtime_unavailable(denial.message),
        RunnerDenialReason::CapacityExhausted => RuntimeError::capacity_exhausted(denial.message),
        RunnerDenialReason::InvalidEndpoint => RuntimeError::transport_unavailable(denial.message),
        _ => RuntimeError::runner_protocol(denial.message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tool_transport::RunnerRpcTransport;
    use astra_runtime_env::{
        ExecutorStatus, RunnerRequestedTool, ToolTransportKind, WorkspaceBindingKind,
    };
    use astra_services::{InMemoryWorkspaceRecordStore, WorkspaceCleanupDebtStore};
    use async_trait::async_trait;
    use axum::{
        body::{self, Body},
        http::{HeaderMap, Request, StatusCode},
        routing::post,
        Json, Router,
    };
    use serde_json::json;
    use std::sync::Arc;
    use tower::util::ServiceExt;

    struct AlwaysHealthy;

    #[async_trait]
    impl HealthChecker for AlwaysHealthy {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    struct TestAdminAuthorizer;

    #[async_trait]
    impl astra_services::AdminAuthorizer for TestAdminAuthorizer {
        async fn require_admin(
            &self,
            headers: &HeaderMap,
        ) -> Result<astra_services::AuthenticatedUser, (StatusCode, Json<astra_core::ErrorResponse>)>
        {
            if headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                == Some("Bearer admin-token")
            {
                Ok(astra_services::AuthenticatedUser {
                    user_id: "admin-1".to_string(),
                    username: Some("admin".to_string()),
                })
            } else {
                Err((
                    StatusCode::FORBIDDEN,
                    Json(astra_core::ErrorResponse::new("Admin role required")),
                ))
            }
        }
    }

    fn register_request(
        owner_id: &str,
        runner_id: &str,
        workspace: WorkspaceBinding,
        runtime: RuntimeBinding,
        policy: PolicyIntent,
    ) -> RunnerRegisterRequest {
        let registry = ToolRegistry::builtins();
        let binding = RunBinding::resolve(
            workspace,
            ExecutorBinding::personal_runner(runner_id),
            runtime,
            policy,
            &registry,
        );
        RunnerRegisterRequest::new(
            RunnerIdentity::personal(runner_id, owner_id),
            RunnerCapacity {
                max_sessions: 2,
                active_sessions: 0,
            },
            RuntimeEnvironmentAdvertisement::new(binding),
        )
    }

    fn local_workspace() -> WorkspaceBinding {
        WorkspaceBinding::local_filesystem("/workspace/astra", WorkspaceAuthority::ReadWrite)
    }

    fn cloud_workspace() -> WorkspaceBinding {
        WorkspaceBinding::cloud_workspace("/workspace/astra", WorkspaceAuthority::ReadWrite)
    }

    fn executor_binding(kind: ExecutorBindingKind, runner_id: &str) -> ExecutorBinding {
        ExecutorBinding {
            kind,
            executor_id: runner_id.to_string(),
            display_name: runner_id.to_string(),
            transport: ToolTransportKind::RunnerRpc,
            status: ExecutorStatus::Online,
        }
    }

    fn valid_register_request(owner_id: &str, runner_id: &str) -> RunnerRegisterRequest {
        register_request(
            owner_id,
            runner_id,
            local_workspace(),
            RuntimeBinding::host_process(format!("{runner_id}-runtime")),
            PolicyIntent::local_developer(),
        )
    }

    fn valid_register_request_with_endpoint(
        owner_id: &str,
        runner_id: &str,
        endpoint: impl Into<String>,
    ) -> RunnerRegisterRequest {
        valid_register_request(owner_id, runner_id)
            .with_rpc_endpoint(astra_runtime_env::RunnerRpcEndpoint::new(endpoint))
    }

    fn hosted_pool_register_request(runner_id: &str) -> RunnerRegisterRequest {
        let registry = ToolRegistry::builtins();
        let binding = RunBinding::resolve(
            cloud_workspace(),
            ExecutorBinding::hosted_runner(runner_id),
            RuntimeBinding::oci_container(format!("{runner_id}-runtime")),
            PolicyIntent::local_developer(),
            &registry,
        );
        RunnerRegisterRequest::new(
            RunnerIdentity::hosted_pool(runner_id),
            RunnerCapacity {
                max_sessions: 2,
                active_sessions: 0,
            },
            RuntimeEnvironmentAdvertisement::new(binding),
        )
    }

    fn enterprise_runner_register_request(runner_id: &str) -> RunnerRegisterRequest {
        let registry = ToolRegistry::builtins();
        let binding = RunBinding::resolve(
            cloud_workspace(),
            executor_binding(ExecutorBindingKind::EnterpriseRunner, runner_id),
            RuntimeBinding::oci_container(format!("{runner_id}-runtime")),
            PolicyIntent::local_developer(),
            &registry,
        );
        RunnerRegisterRequest::new(
            RunnerIdentity::enterprise_dedicated(runner_id),
            RunnerCapacity {
                max_sessions: 2,
                active_sessions: 0,
            },
            RuntimeEnvironmentAdvertisement::new(binding),
        )
    }

    fn hosted_pool_register_request_with_endpoint(
        runner_id: &str,
        endpoint: impl Into<String>,
    ) -> RunnerRegisterRequest {
        hosted_pool_register_request(runner_id)
            .with_rpc_endpoint(astra_runtime_env::RunnerRpcEndpoint::new(endpoint))
    }

    fn public_runner_endpoint() -> &'static str {
        "https://runner.example.com"
    }

    fn local_dev_runner_pool() -> ServerRunnerPool {
        ServerRunnerPool::with_endpoint_trust_policy(
            RunnerScheduler::default(),
            RunnerEndpointTrustPolicy::local_development(),
        )
        .unwrap()
    }

    fn runner_pool_with_store(store: InMemoryRunnerPoolStore) -> ServerRunnerPool {
        ServerRunnerPool::with_store(
            RunnerScheduler::default(),
            RunnerEndpointTrustPolicy::public_https(),
            Arc::new(store),
        )
        .unwrap()
    }

    async fn insert_stored_runner(
        store: &InMemoryRunnerPoolStore,
        entry: RunnerPoolEntry,
        lease_expires_at_ms: i64,
    ) {
        insert_stored_runner_with_sessions(store, entry, lease_expires_at_ms, Vec::new()).await;
    }

    async fn insert_stored_runner_with_sessions(
        store: &InMemoryRunnerPoolStore,
        entry: RunnerPoolEntry,
        lease_expires_at_ms: i64,
        active_session_ids: Vec<&str>,
    ) {
        let pool_owner_id = entry
            .identity
            .owner_id
            .clone()
            .unwrap_or_else(|| SHARED_RUNNER_POOL_OWNER_ID.to_string());
        store
            .upsert(StoredRunnerPoolEntry {
                pool_owner_id,
                entry: Arc::new(entry),
                lease_expires_at_ms,
                active_session_ids: active_session_ids.into_iter().map(str::to_string).collect(),
            })
            .await
            .expect("insert stored runner");
    }

    async fn register(
        pool: &ServerRunnerPool,
        user_id: &str,
        request: RunnerRegisterRequest,
    ) -> RunnerRegisterResponse {
        pool.register_for_user(user_id, request)
            .await
            .expect("register runner")
    }

    async fn send_heartbeat(
        pool: &ServerRunnerPool,
        user_id: &str,
        request: RunnerHeartbeat,
    ) -> RunnerAckResponse {
        pool.heartbeat_for_user(user_id, request)
            .await
            .expect("runner heartbeat")
    }

    async fn list(pool: &ServerRunnerPool, user_id: &str) -> Vec<RunnerPoolEntry> {
        pool.list_for_user(user_id).await.expect("list runners")
    }

    async fn schedule_runner(
        pool: &ServerRunnerPool,
        user_id: &str,
        request: RunnerScheduleRequest,
    ) -> RunnerScheduleDecision {
        pool.schedule_for_user(user_id, request)
            .await
            .expect("schedule runner")
    }

    fn test_app() -> Router {
        crate::server::build_test_router(
            AppState::new(ServiceInfo::default(), Arc::new(AlwaysHealthy))
                .with_auth_service(Arc::new(astra_services::auth::StubAuthService)),
        )
    }

    fn test_app_with_admin() -> Router {
        crate::server::build_test_router(
            AppState::new(ServiceInfo::default(), Arc::new(AlwaysHealthy))
                .with_auth_service(Arc::new(astra_services::auth::StubAuthService))
                .with_admin_authorizer(Arc::new(TestAdminAuthorizer)),
        )
    }

    fn json_request(method: &str, path: &str, value: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", "Bearer test-token")
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .unwrap()
    }

    fn admin_json_request(method: &str, path: &str, value: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", "Bearer admin-token")
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .unwrap()
    }

    fn empty_request(method: &str, path: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", "Bearer test-token")
            .body(Body::empty())
            .unwrap()
    }

    async fn response_json<T: serde::de::DeserializeOwned>(
        response: axum::response::Response,
    ) -> T {
        let bytes = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        serde_json::from_slice(&bytes).expect("json response")
    }

    async fn spawn_runner_rpc_server() -> (String, tokio::task::JoinHandle<()>) {
        async fn prepare(
            Json(request): Json<RunnerPrepareSessionRequest>,
        ) -> Json<RunnerPrepareSessionResponse> {
            Json(RunnerPrepareSessionResponse::Prepared {
                handle: Box::new(astra_runtime_env::RuntimeSessionHandle::from_spec(
                    &request.spec,
                )),
            })
        }

        async fn execute(
            Json(request): Json<RunnerExecuteToolRequest>,
        ) -> Json<RunnerExecuteToolResponse> {
            Json(RunnerExecuteToolResponse::Completed {
                outcome: astra_runtime_env::RuntimeToolOutcome::completed(
                    &request.invocation,
                    "http-runner-result",
                    &request.session,
                ),
            })
        }

        let app = Router::new()
            .route("/v1/sessions/prepare", post(prepare))
            .route("/v1/tools/execute", post(execute));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test runner");
        let addr = listener.local_addr().expect("test runner addr");
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("test runner serve");
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn register_accepts_owned_runner_and_lists_it_for_owner() {
        let pool = ServerRunnerPool::default();
        let response = register(&pool, "user-1", valid_register_request("user-1", "r1")).await;

        assert!(response.accepted);
        assert_eq!(response.runner_id, "r1");
        assert_eq!(list(&pool, "user-1").await.len(), 1);
        assert!(list(&pool, "user-2").await.is_empty());
    }

    #[tokio::test]
    async fn register_rejects_owner_mismatch_without_inserting_runner() {
        let pool = ServerRunnerPool::default();
        let response = register(&pool, "user-1", valid_register_request("user-2", "r1")).await;

        assert!(!response.accepted);
        assert_eq!(
            response.denial.as_ref().map(|d| &d.reason),
            Some(&RunnerDenialReason::AuthenticationFailed)
        );
        assert!(list(&pool, "user-1").await.is_empty());
        assert!(list(&pool, "user-2").await.is_empty());
    }

    #[tokio::test]
    async fn user_register_rejects_ownerless_hosted_runner_without_inserting_runner() {
        let pool = ServerRunnerPool::default();
        let response = register(&pool, "user-1", hosted_pool_register_request("hosted-r1")).await;

        assert!(!response.accepted);
        assert_eq!(
            response.denial.as_ref().map(|d| &d.reason),
            Some(&RunnerDenialReason::AuthenticationFailed)
        );
        assert!(list(&pool, "user-1").await.is_empty());
    }

    #[tokio::test]
    async fn operator_registers_shared_hosted_runner_for_user_scheduling() {
        let pool = ServerRunnerPool::default();
        let response = pool
            .register_for_operator(hosted_pool_register_request_with_endpoint(
                "hosted-r1",
                public_runner_endpoint(),
            ))
            .await
            .expect("operator register");

        assert!(response.accepted);
        let listed = list(&pool, "user-1").await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].identity.runner_id, "hosted-r1");
        assert_eq!(listed[0].identity.owner_id, None);

        let decision = schedule_runner(
            &pool,
            "user-1",
            RunnerScheduleRequest::new(
                "session-1",
                "run-1",
                cloud_workspace(),
                PolicyIntent::local_developer(),
            )
            .with_requested_tools(["read_file"])
            .require_executor_kind(ExecutorBindingKind::HostedRunner),
        )
        .await;

        assert_eq!(
            decision
                .selected
                .as_ref()
                .map(|target| target.runner_id.as_str()),
            Some("hosted-r1")
        );
    }

    #[tokio::test]
    async fn operator_register_rejects_personal_runner_identity() {
        let pool = ServerRunnerPool::default();
        let response = pool
            .register_for_operator(valid_register_request("user-1", "personal-r1"))
            .await
            .expect("operator register");

        assert!(!response.accepted);
        assert_eq!(
            response.denial.as_ref().map(|d| &d.reason),
            Some(&RunnerDenialReason::AuthenticationFailed)
        );
        assert!(list(&pool, "user-1").await.is_empty());
    }

    #[tokio::test]
    async fn operator_heartbeat_renews_shared_runner_while_user_heartbeat_is_rejected() {
        let store = InMemoryRunnerPoolStore::default();
        let pool = runner_pool_with_store(store.clone());
        let request =
            hosted_pool_register_request_with_endpoint("hosted-r1", public_runner_endpoint());
        let entry = RunnerPoolEntry::from_register_request(request.clone(), RunnerStatus::Idle)
            .expect("valid hosted entry");
        insert_stored_runner(&store, entry, now_epoch_ms().saturating_sub(1)).await;
        let heartbeat = RunnerHeartbeat {
            runner_id: "hosted-r1".to_string(),
            status: RunnerStatus::Idle,
            capacity: RunnerCapacity::single_session(),
            active_session_ids: Vec::new(),
            advertisement: request.advertisement.clone(),
        };

        let user_response = send_heartbeat(&pool, "user-1", heartbeat.clone()).await;
        assert!(matches!(user_response, RunnerAckResponse::Rejected { .. }));

        let operator_response = pool
            .heartbeat_for_operator(heartbeat)
            .await
            .expect("operator heartbeat");
        assert!(matches!(operator_response, RunnerAckResponse::Accepted));
        let decision = schedule_runner(
            &pool,
            "user-1",
            RunnerScheduleRequest::new(
                "session-1",
                "run-1",
                cloud_workspace(),
                PolicyIntent::local_developer(),
            )
            .with_requested_tools(["read_file"])
            .require_executor_kind(ExecutorBindingKind::HostedRunner),
        )
        .await;
        assert_eq!(
            decision
                .selected
                .as_ref()
                .map(|target| target.runner_id.as_str()),
            Some("hosted-r1")
        );
    }

    #[tokio::test]
    async fn operator_registers_enterprise_runner_for_enterprise_scheduling() {
        let pool = ServerRunnerPool::default();
        let response = pool
            .register_for_operator(
                enterprise_runner_register_request("enterprise-r1")
                    .with_rpc_endpoint(RunnerRpcEndpoint::new(public_runner_endpoint())),
            )
            .await
            .expect("operator register");

        assert!(response.accepted);
        let decision = schedule_runner(
            &pool,
            "user-1",
            RunnerScheduleRequest::new(
                "session-1",
                "run-1",
                cloud_workspace(),
                PolicyIntent::local_developer(),
            )
            .with_requested_tools(["read_file"])
            .require_executor_kind(ExecutorBindingKind::EnterpriseRunner),
        )
        .await;
        assert_eq!(
            decision
                .selected
                .as_ref()
                .map(|target| target.runner_id.as_str()),
            Some("enterprise-r1")
        );
    }

    #[tokio::test]
    async fn register_rejects_control_plane_advertisement_without_runtime_executor_capability() {
        let registry = ToolRegistry::builtins();
        let request = RunnerRegisterRequest::new(
            RunnerIdentity::personal("control-plane", "user-1"),
            RunnerCapacity::single_session(),
            RuntimeEnvironmentAdvertisement::new(RunBinding::cloud_control_plane(&registry)),
        );
        let pool = ServerRunnerPool::default();

        let response = register(&pool, "user-1", request).await;

        assert!(!response.accepted);
        assert_eq!(
            response.denial.as_ref().map(|d| &d.reason),
            Some(&RunnerDenialReason::CapabilityTooWeak)
        );
        assert!(list(&pool, "user-1").await.is_empty());
    }

    #[tokio::test]
    async fn heartbeat_for_unknown_runner_is_rejected_and_does_not_insert() {
        let pool = ServerRunnerPool::default();
        let heartbeat = RunnerHeartbeat {
            runner_id: "missing".to_string(),
            status: RunnerStatus::Idle,
            capacity: RunnerCapacity::single_session(),
            active_session_ids: Vec::new(),
            advertisement: valid_register_request("user-1", "missing").advertisement,
        };

        let response = send_heartbeat(&pool, "user-1", heartbeat).await;

        assert!(matches!(response, RunnerAckResponse::Rejected { .. }));
        assert!(list(&pool, "user-1").await.is_empty());
    }

    #[tokio::test]
    async fn heartbeat_rejects_invalid_advertisement_without_replacing_existing_entry() {
        let pool = ServerRunnerPool::default();
        let register_response =
            register(&pool, "user-1", valid_register_request("user-1", "r1")).await;
        assert!(register_response.accepted);
        let registry = ToolRegistry::builtins();
        let heartbeat = RunnerHeartbeat {
            runner_id: "r1".to_string(),
            status: RunnerStatus::Idle,
            capacity: RunnerCapacity::single_session(),
            active_session_ids: Vec::new(),
            advertisement: RuntimeEnvironmentAdvertisement::new(RunBinding::cloud_control_plane(
                &registry,
            )),
        };

        let response = send_heartbeat(&pool, "user-1", heartbeat).await;

        assert!(matches!(response, RunnerAckResponse::Rejected { .. }));
        let entry = list(&pool, "user-1")
            .await
            .into_iter()
            .next()
            .expect("original runner entry");
        assert_eq!(
            entry.advertisement.binding.runtime.session_manager,
            RuntimeSessionManager::HostProcess
        );
        assert_eq!(
            entry.advertisement.binding.runtime.isolation_backend,
            RuntimeIsolationBackend::HostProcess
        );
    }

    #[tokio::test]
    async fn heartbeat_preserves_registered_rpc_endpoint() {
        let pool = local_dev_runner_pool();
        let response = register(
            &pool,
            "user-1",
            valid_register_request_with_endpoint("user-1", "r1", "http://127.0.0.1:3847/"),
        )
        .await;
        assert!(response.accepted);
        let heartbeat = RunnerHeartbeat {
            runner_id: "r1".to_string(),
            status: RunnerStatus::Busy,
            capacity: RunnerCapacity {
                max_sessions: 2,
                active_sessions: 1,
            },
            active_session_ids: vec!["session-1".to_string()],
            advertisement: valid_register_request("user-1", "r1").advertisement,
        };

        let response = send_heartbeat(&pool, "user-1", heartbeat).await;

        assert!(matches!(response, RunnerAckResponse::Accepted));
        let entry = list(&pool, "user-1")
            .await
            .into_iter()
            .next()
            .expect("runner entry");
        assert_eq!(
            entry
                .rpc_endpoint
                .as_ref()
                .map(astra_runtime_env::RunnerRpcEndpoint::normalized_base_url),
            Some("http://127.0.0.1:3847".to_string())
        );
    }

    #[tokio::test]
    async fn register_rejects_empty_rpc_endpoint_without_inserting_runner() {
        let pool = ServerRunnerPool::default();

        let response = register(
            &pool,
            "user-1",
            valid_register_request_with_endpoint("user-1", "r1", "   "),
        )
        .await;

        assert!(!response.accepted);
        assert_eq!(
            response.denial.as_ref().map(|denial| &denial.reason),
            Some(&RunnerDenialReason::InvalidEndpoint)
        );
        assert!(list(&pool, "user-1").await.is_empty());
    }

    #[tokio::test]
    async fn register_rejects_rpc_endpoint_with_credentials_or_path() {
        let pool = ServerRunnerPool::default();

        let credentials_response = register(
            &pool,
            "user-1",
            valid_register_request_with_endpoint("user-1", "r1", "https://user:pass@example.com"),
        )
        .await;
        let path_response = register(
            &pool,
            "user-1",
            valid_register_request_with_endpoint("user-1", "r2", "https://example.com/runner"),
        )
        .await;

        assert!(!credentials_response.accepted);
        assert_eq!(
            credentials_response
                .denial
                .as_ref()
                .map(|denial| &denial.reason),
            Some(&RunnerDenialReason::InvalidEndpoint)
        );
        assert!(!path_response.accepted);
        assert_eq!(
            path_response.denial.as_ref().map(|denial| &denial.reason),
            Some(&RunnerDenialReason::InvalidEndpoint)
        );
        assert!(list(&pool, "user-1").await.is_empty());
    }

    #[tokio::test]
    async fn public_endpoint_policy_rejects_loopback_http_and_private_ip() {
        let pool = ServerRunnerPool::default();

        let loopback_response = register(
            &pool,
            "user-1",
            valid_register_request_with_endpoint("user-1", "r1", "http://127.0.0.1:3847"),
        )
        .await;
        let private_ip_response = register(
            &pool,
            "user-1",
            valid_register_request_with_endpoint("user-1", "r2", "https://10.0.0.8"),
        )
        .await;

        assert!(!loopback_response.accepted);
        assert_eq!(
            loopback_response
                .denial
                .as_ref()
                .map(|denial| &denial.reason),
            Some(&RunnerDenialReason::InvalidEndpoint)
        );
        assert!(!private_ip_response.accepted);
        assert_eq!(
            private_ip_response
                .denial
                .as_ref()
                .map(|denial| &denial.reason),
            Some(&RunnerDenialReason::InvalidEndpoint)
        );
        assert!(list(&pool, "user-1").await.is_empty());
    }

    #[test]
    fn public_endpoint_policy_rejects_trailing_dot_local_and_invalid_dns_hosts() {
        let policy = RunnerEndpointTrustPolicy::public_https();
        for endpoint in [
            "https://localhost.",
            "https://runner.local.",
            "https://runner..example.com",
            "https://-runner.example.com",
            "https://runner-.example.com",
        ] {
            let denial = policy
                .validate(&RunnerRpcEndpoint::new(endpoint))
                .expect_err("endpoint should be rejected");
            assert_eq!(denial.reason, RunnerDenialReason::InvalidEndpoint);
        }
    }

    #[test]
    fn public_endpoint_policy_enforces_configured_host_allowlist() {
        let policy = RunnerEndpointTrustPolicy::public_https_with_allowed_hosts(&[
            "runners.example.com",
            "*.trusted.example.com",
        ]);

        assert!(policy
            .validate(&RunnerRpcEndpoint::new("https://runners.example.com"))
            .is_ok());
        assert!(policy
            .validate(&RunnerRpcEndpoint::new(
                "https://pool-a.trusted.example.com/"
            ))
            .is_ok());
        let root_denial = policy
            .validate(&RunnerRpcEndpoint::new("https://trusted.example.com"))
            .expect_err("suffix pattern should not match root domain");
        assert_eq!(root_denial.reason, RunnerDenialReason::InvalidEndpoint);
        let outside_denial = policy
            .validate(&RunnerRpcEndpoint::new("https://runner.evil.example.com"))
            .expect_err("outside host should be rejected");
        assert_eq!(outside_denial.reason, RunnerDenialReason::InvalidEndpoint);
    }

    #[tokio::test]
    async fn local_development_endpoint_policy_accepts_loopback_http() {
        let pool = local_dev_runner_pool();

        let response = register(
            &pool,
            "user-1",
            valid_register_request_with_endpoint("user-1", "r1", "http://127.0.0.1:3847/"),
        )
        .await;

        assert!(response.accepted);
        let entry = list(&pool, "user-1")
            .await
            .into_iter()
            .next()
            .expect("runner entry");
        assert_eq!(
            entry
                .rpc_endpoint
                .as_ref()
                .map(astra_runtime_env::RunnerRpcEndpoint::normalized_base_url),
            Some("http://127.0.0.1:3847".to_string())
        );
    }

    #[tokio::test]
    async fn schedule_selects_registered_runner_for_matching_workspace_and_tool() {
        let pool = ServerRunnerPool::default();
        register(
            &pool,
            "user-1",
            valid_register_request_with_endpoint("user-1", "r1", public_runner_endpoint()),
        )
        .await;
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            local_workspace(),
            PolicyIntent::local_developer(),
        )
        .with_requested_tools(["bash"])
        .require_executor_kind(ExecutorBindingKind::PersonalRunner);

        let decision = schedule_runner(&pool, "user-1", request).await;

        let selected = decision.selected.expect("runner selected");
        assert_eq!(selected.runner_id, "r1");
        assert_eq!(
            selected.binding.executor.kind,
            ExecutorBindingKind::PersonalRunner
        );
        assert_eq!(selected.session_spec.requested_tools, vec!["bash"]);
    }

    #[tokio::test]
    async fn schedule_rejects_registered_runner_without_rpc_endpoint() {
        let pool = ServerRunnerPool::default();
        register(&pool, "user-1", valid_register_request("user-1", "r1")).await;
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            local_workspace(),
            PolicyIntent::local_developer(),
        )
        .with_requested_tools(["bash"])
        .require_executor_kind(ExecutorBindingKind::PersonalRunner);

        let decision = schedule_runner(&pool, "user-1", request).await;

        assert!(decision.selected.is_none());
        assert!(
            decision.denials.iter().any(|denial| {
                denial.reason == RunnerScheduleDenialReason::TransportUnavailable
            }),
            "expected transport denial, got {:?}",
            decision.denials
        );
    }

    #[tokio::test]
    async fn expired_runner_is_listed_offline_and_not_scheduled() {
        let store = InMemoryRunnerPoolStore::default();
        let pool = runner_pool_with_store(store.clone());
        let entry = RunnerPoolEntry::from_register_request(
            valid_register_request_with_endpoint("user-1", "r1", public_runner_endpoint()),
            RunnerStatus::Idle,
        )
        .expect("valid entry");
        insert_stored_runner(&store, entry, now_epoch_ms().saturating_sub(1)).await;

        let listed = list(&pool, "user-1").await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, RunnerStatus::Offline);

        let decision = schedule_runner(
            &pool,
            "user-1",
            RunnerScheduleRequest::new(
                "session-1",
                "run-1",
                local_workspace(),
                PolicyIntent::local_developer(),
            )
            .with_requested_tools(["bash"])
            .require_executor_kind(ExecutorBindingKind::PersonalRunner),
        )
        .await;

        // Expired runners are excluded from scheduling entirely — no
        // candidate means no selection and no denial for the expired runner.
        assert!(decision.selected.is_none());
        assert!(
            decision
                .denials
                .iter()
                .all(|denial| denial.reason != RunnerScheduleDenialReason::RunnerLeaseExpired),
            "expired runners must be excluded, not listed in denials: {:?}",
            decision.denials
        );
    }

    #[tokio::test]
    async fn heartbeat_renews_expired_runner_lease() {
        let store = InMemoryRunnerPoolStore::default();
        let pool = runner_pool_with_store(store.clone());
        let request =
            valid_register_request_with_endpoint("user-1", "r1", public_runner_endpoint());
        let entry = RunnerPoolEntry::from_register_request(request.clone(), RunnerStatus::Idle)
            .expect("valid entry");
        insert_stored_runner(&store, entry, now_epoch_ms().saturating_sub(1)).await;
        let heartbeat = RunnerHeartbeat {
            runner_id: "r1".to_string(),
            status: RunnerStatus::Idle,
            capacity: RunnerCapacity::single_session(),
            active_session_ids: Vec::new(),
            advertisement: request.advertisement,
        };

        let response = send_heartbeat(&pool, "user-1", heartbeat).await;

        assert!(matches!(response, RunnerAckResponse::Accepted));
        let decision = schedule_runner(
            &pool,
            "user-1",
            RunnerScheduleRequest::new(
                "session-1",
                "run-1",
                local_workspace(),
                PolicyIntent::local_developer(),
            )
            .with_requested_tools(["bash"])
            .require_executor_kind(ExecutorBindingKind::PersonalRunner),
        )
        .await;
        assert_eq!(
            decision
                .selected
                .as_ref()
                .map(|target| target.runner_id.as_str()),
            Some("r1")
        );
    }

    #[tokio::test]
    async fn runner_pool_reaper_deletes_only_expired_entries_past_retention() {
        let store = InMemoryRunnerPoolStore::default();
        let pool = runner_pool_with_store(store.clone());
        let now_ms = now_epoch_ms();
        let retention = std::time::Duration::from_secs(60);
        let old_expired = RunnerPoolEntry::from_register_request(
            valid_register_request_with_endpoint("user-1", "old", public_runner_endpoint()),
            RunnerStatus::Idle,
        )
        .expect("old entry");
        let recoverable_expired = RunnerPoolEntry::from_register_request(
            valid_register_request_with_endpoint("user-1", "recoverable", public_runner_endpoint()),
            RunnerStatus::Idle,
        )
        .expect("recoverable entry");
        let active = RunnerPoolEntry::from_register_request(
            valid_register_request_with_endpoint("user-1", "active", public_runner_endpoint()),
            RunnerStatus::Idle,
        )
        .expect("active entry");
        insert_stored_runner(&store, old_expired, now_ms.saturating_sub(61_000)).await;
        insert_stored_runner(&store, recoverable_expired, now_ms.saturating_sub(1_000)).await;
        insert_stored_runner(&store, active, now_ms.saturating_add(60_000)).await;

        let deleted = reap_expired_runner_entries(&store, retention, 100)
            .await
            .expect("reap expired runners");

        assert_eq!(deleted, 1);
        let listed = list(&pool, "user-1").await;
        let ids: Vec<_> = listed
            .iter()
            .map(|entry| (entry.identity.runner_id.as_str(), entry.status))
            .collect();
        assert_eq!(
            ids,
            vec![
                ("active", RunnerStatus::Idle),
                ("recoverable", RunnerStatus::Offline)
            ]
        );

        let heartbeat = RunnerHeartbeat {
            runner_id: "old".to_string(),
            status: RunnerStatus::Idle,
            capacity: RunnerCapacity::single_session(),
            active_session_ids: Vec::new(),
            advertisement: valid_register_request("user-1", "old").advertisement,
        };
        let response = send_heartbeat(&pool, "user-1", heartbeat).await;
        assert!(matches!(response, RunnerAckResponse::Rejected { .. }));
    }

    #[tokio::test]
    async fn runner_pool_reaper_records_cleanup_debt_for_managed_workspace_before_delete() {
        let store = InMemoryRunnerPoolStore::default();
        let pool = runner_pool_with_store(store.clone());
        let cleanup_store = InMemoryWorkspaceRecordStore::new();
        let now_ms = now_epoch_ms();
        let entry = RunnerPoolEntry::from_register_request(
            register_request(
                "user-1",
                "cloud-r1",
                cloud_workspace(),
                RuntimeBinding::oci_container("cloud-runtime"),
                PolicyIntent::local_developer(),
            )
            .with_rpc_endpoint(RunnerRpcEndpoint::new(public_runner_endpoint())),
            RunnerStatus::Busy,
        )
        .expect("cloud entry");
        insert_stored_runner_with_sessions(
            &store,
            entry,
            now_ms.saturating_sub(120_000),
            vec!["session-1"],
        )
        .await;

        let deleted = reap_expired_runner_entries_with_cleanup_debt(
            &store,
            Some(&cleanup_store),
            std::time::Duration::from_secs(60),
            100,
        )
        .await
        .expect("reap expired runners");

        assert_eq!(deleted, 1);
        assert!(list(&pool, "user-1").await.is_empty());
        let debts = cleanup_store
            .list_cleanup_debts("user-1", 10)
            .await
            .expect("list cleanup debts");
        assert_eq!(debts.len(), 1);
        assert_eq!(debts[0].session_id.as_deref(), Some("session-1"));
        assert_eq!(
            debts[0].reason,
            astra_runtime_env::CleanupReason::LeaseExpired
        );
        assert_eq!(debts[0].record.kind, WorkspaceBindingKind::CloudWorkspace);
        assert!(matches!(
            &debts[0].record.source,
            WorkspaceSource::ProviderManaged { provider, reference }
                if provider == "runner_pool" && reference == "cloud-r1"
        ));
        assert!(debts[0].message.contains("cloud-r1"));
    }

    #[tokio::test]
    async fn runner_pool_reaper_does_not_record_cleanup_debt_for_local_workspace() {
        let store = InMemoryRunnerPoolStore::default();
        let pool = runner_pool_with_store(store.clone());
        let cleanup_store = InMemoryWorkspaceRecordStore::new();
        let now_ms = now_epoch_ms();
        let entry = RunnerPoolEntry::from_register_request(
            valid_register_request_with_endpoint("user-1", "local-r1", public_runner_endpoint()),
            RunnerStatus::Busy,
        )
        .expect("local entry");
        insert_stored_runner_with_sessions(
            &store,
            entry,
            now_ms.saturating_sub(120_000),
            vec!["session-1"],
        )
        .await;

        let deleted = reap_expired_runner_entries_with_cleanup_debt(
            &store,
            Some(&cleanup_store),
            std::time::Duration::from_secs(60),
            100,
        )
        .await
        .expect("reap expired runners");

        assert_eq!(deleted, 1);
        assert!(list(&pool, "user-1").await.is_empty());
        assert!(cleanup_store
            .list_cleanup_debts("user-1", 10)
            .await
            .expect("list cleanup debts")
            .is_empty());
    }

    struct FailingCleanupDebtStore;

    #[async_trait]
    impl WorkspaceCleanupDebtStore for FailingCleanupDebtStore {
        async fn record_cleanup_debt(
            &self,
            _entry: astra_services::WorkspaceCleanupDebtEntry,
        ) -> Result<(), astra_services::WorkspaceCleanupDebtStoreError> {
            Err(astra_services::WorkspaceCleanupDebtStoreError::Unavailable(
                "injected cleanup debt failure".to_string(),
            ))
        }

        async fn list_cleanup_debts(
            &self,
            _owner_id: &str,
            _limit: u32,
        ) -> Result<
            Vec<astra_services::WorkspaceCleanupDebtEntry>,
            astra_services::WorkspaceCleanupDebtStoreError,
        > {
            Ok(Vec::new())
        }

        async fn resolve_cleanup_debt(
            &self,
            _owner_id: &str,
            _debt_id: &str,
        ) -> Result<bool, astra_services::WorkspaceCleanupDebtStoreError> {
            Ok(false)
        }

        async fn list_all_unresolved_debts(
            &self,
        ) -> Result<
            Vec<astra_services::WorkspaceCleanupDebtEntry>,
            astra_services::WorkspaceCleanupDebtStoreError,
        > {
            Err(astra_services::WorkspaceCleanupDebtStoreError::Unavailable(
                "injected cleanup debt failure".to_string(),
            ))
        }

        async fn increment_debt_attempts(
            &self,
            _debt_id: &str,
        ) -> Result<(), astra_services::WorkspaceCleanupDebtStoreError> {
            Err(astra_services::WorkspaceCleanupDebtStoreError::Unavailable(
                "injected cleanup debt failure".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn runner_pool_reaper_keeps_runner_when_cleanup_debt_recording_fails() {
        let store = InMemoryRunnerPoolStore::default();
        let pool = runner_pool_with_store(store.clone());
        let now_ms = now_epoch_ms();
        let entry = RunnerPoolEntry::from_register_request(
            register_request(
                "user-1",
                "cloud-r1",
                cloud_workspace(),
                RuntimeBinding::oci_container("cloud-runtime"),
                PolicyIntent::local_developer(),
            )
            .with_rpc_endpoint(RunnerRpcEndpoint::new(public_runner_endpoint())),
            RunnerStatus::Busy,
        )
        .expect("cloud entry");
        insert_stored_runner_with_sessions(
            &store,
            entry,
            now_ms.saturating_sub(120_000),
            vec!["session-1"],
        )
        .await;

        let error = reap_expired_runner_entries_with_cleanup_debt(
            &store,
            Some(&FailingCleanupDebtStore),
            std::time::Duration::from_secs(60),
            100,
        )
        .await
        .expect_err("cleanup debt failure should fail reaper");

        assert!(error.contains("cleanup debt"));
        let listed = list(&pool, "user-1").await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, RunnerStatus::Offline);
    }

    #[tokio::test]
    async fn runner_pool_reaper_honors_batch_limit() {
        let store = InMemoryRunnerPoolStore::default();
        let pool = runner_pool_with_store(store.clone());
        let now_ms = now_epoch_ms();
        for runner_id in ["old-1", "old-2", "old-3"] {
            let entry = RunnerPoolEntry::from_register_request(
                valid_register_request_with_endpoint("user-1", runner_id, public_runner_endpoint()),
                RunnerStatus::Idle,
            )
            .expect("old entry");
            insert_stored_runner(&store, entry, now_ms.saturating_sub(120_000)).await;
        }

        let deleted = reap_expired_runner_entries(&store, std::time::Duration::from_secs(60), 2)
            .await
            .expect("reap expired runners");

        assert_eq!(deleted, 2);
        let listed = list(&pool, "user-1").await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, RunnerStatus::Offline);
    }

    #[tokio::test]
    async fn schedule_does_not_borrow_runner_workspace_for_no_workspace_request() {
        let pool = ServerRunnerPool::default();
        register(
            &pool,
            "user-1",
            valid_register_request_with_endpoint("user-1", "r1", public_runner_endpoint()),
        )
        .await;
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            WorkspaceBinding::none(),
            PolicyIntent::cloud_control_plane(),
        )
        .with_requested_tool_calls([RunnerRequestedTool::new("bash", json!({"cmd": "pwd"}))]);

        let decision = schedule_runner(&pool, "user-1", request).await;

        assert!(decision.selected.is_none());
        assert!(
            decision.denials.iter().any(|denial| {
                denial.reason
                    == astra_runtime_env::RunnerScheduleDenialReason::WorkspaceIncompatible
            }),
            "expected workspace denial, got {:?}",
            decision.denials
        );
    }

    #[tokio::test]
    async fn schedule_filters_other_users_runners() {
        let pool = ServerRunnerPool::default();
        register(&pool, "user-1", valid_register_request("user-1", "r1")).await;
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            local_workspace(),
            PolicyIntent::local_developer(),
        )
        .with_requested_tools(["bash"]);

        let decision = schedule_runner(&pool, "user-2", request).await;

        assert!(decision.selected.is_none());
        assert!(decision.denials.is_empty());
    }

    #[tokio::test]
    async fn registered_runner_preserves_workspace_binding_kind() {
        let pool = ServerRunnerPool::default();
        register(&pool, "user-1", valid_register_request("user-1", "r1")).await;

        let entry = list(&pool, "user-1")
            .await
            .into_iter()
            .next()
            .expect("runner entry");

        assert_eq!(
            entry.advertisement.binding.workspace.kind,
            WorkspaceBindingKind::LocalFilesystem
        );
    }

    #[tokio::test]
    async fn http_register_list_and_schedule_round_trip() {
        let app = test_app();
        let register_payload = serde_json::to_value(valid_register_request_with_endpoint(
            "test-user",
            "http-r1",
            public_runner_endpoint(),
        ))
        .unwrap();
        let register_response = app
            .clone()
            .oneshot(json_request("POST", "/runners/register", register_payload))
            .await
            .expect("register response");
        assert_eq!(register_response.status(), StatusCode::OK);
        let register_body: RunnerRegisterResponse = response_json(register_response).await;
        assert!(register_body.accepted);

        let list_response = app
            .clone()
            .oneshot(empty_request("GET", "/runners"))
            .await
            .expect("list response");
        assert_eq!(list_response.status(), StatusCode::OK);
        let list_body: serde_json::Value = response_json(list_response).await;
        assert_eq!(list_body["runners"].as_array().unwrap().len(), 1);

        let schedule = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            local_workspace(),
            PolicyIntent::local_developer(),
        )
        .with_requested_tools(["bash"])
        .require_executor_kind(ExecutorBindingKind::PersonalRunner);
        let schedule_response = app
            .oneshot(json_request(
                "POST",
                "/runners/schedule",
                serde_json::to_value(schedule).unwrap(),
            ))
            .await
            .expect("schedule response");
        assert_eq!(schedule_response.status(), StatusCode::OK);
        let decision: RunnerScheduleDecision = response_json(schedule_response).await;
        assert_eq!(
            decision
                .selected
                .as_ref()
                .map(|target| target.runner_id.as_str()),
            Some("http-r1")
        );
    }

    #[tokio::test]
    async fn http_register_rejects_owner_mismatch_and_keeps_list_empty() {
        let app = test_app();
        let register_payload =
            serde_json::to_value(valid_register_request("other-user", "http-r1")).unwrap();
        let register_response = app
            .clone()
            .oneshot(json_request("POST", "/runners/register", register_payload))
            .await
            .expect("register response");
        assert_eq!(register_response.status(), StatusCode::OK);
        let register_body: RunnerRegisterResponse = response_json(register_response).await;
        assert!(!register_body.accepted);
        assert_eq!(
            register_body.denial.as_ref().map(|d| &d.reason),
            Some(&RunnerDenialReason::AuthenticationFailed)
        );

        let list_response = app
            .oneshot(empty_request("GET", "/runners"))
            .await
            .expect("list response");
        assert_eq!(list_response.status(), StatusCode::OK);
        let list_body: serde_json::Value = response_json(list_response).await;
        assert_eq!(list_body["runners"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn http_operator_register_requires_admin_authorization() {
        let app = test_app_with_admin();
        let payload = serde_json::to_value(hosted_pool_register_request_with_endpoint(
            "hosted-http-r1",
            public_runner_endpoint(),
        ))
        .unwrap();

        let response = app
            .oneshot(json_request("POST", "/runners/operator/register", payload))
            .await
            .expect("operator register response");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn http_operator_register_list_and_schedule_shared_runner() {
        let app = test_app_with_admin();
        let payload = serde_json::to_value(hosted_pool_register_request_with_endpoint(
            "hosted-http-r1",
            public_runner_endpoint(),
        ))
        .unwrap();
        let register_response = app
            .clone()
            .oneshot(admin_json_request(
                "POST",
                "/runners/operator/register",
                payload,
            ))
            .await
            .expect("operator register response");
        assert_eq!(register_response.status(), StatusCode::OK);
        let body: RunnerRegisterResponse = response_json(register_response).await;
        assert!(body.accepted);

        let list_response = app
            .clone()
            .oneshot(empty_request("GET", "/runners"))
            .await
            .expect("list response");
        assert_eq!(list_response.status(), StatusCode::OK);
        let list_body: serde_json::Value = response_json(list_response).await;
        assert_eq!(list_body["runners"].as_array().unwrap().len(), 1);

        let schedule = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            cloud_workspace(),
            PolicyIntent::local_developer(),
        )
        .with_requested_tools(["read_file"])
        .require_executor_kind(ExecutorBindingKind::HostedRunner);
        let schedule_response = app
            .oneshot(json_request(
                "POST",
                "/runners/schedule",
                serde_json::to_value(schedule).unwrap(),
            ))
            .await
            .expect("schedule response");
        assert_eq!(schedule_response.status(), StatusCode::OK);
        let decision: RunnerScheduleDecision = response_json(schedule_response).await;
        assert_eq!(
            decision
                .selected
                .as_ref()
                .map(|target| target.runner_id.as_str()),
            Some("hosted-http-r1")
        );
    }

    #[tokio::test]
    async fn runner_pool_http_transport_prepares_and_executes_registered_endpoint() {
        let (endpoint, server) = spawn_runner_rpc_server().await;
        let pool = local_dev_runner_pool();
        let response = register(
            &pool,
            "user-1",
            valid_register_request_with_endpoint("user-1", "r1", endpoint),
        )
        .await;
        assert!(response.accepted);
        let schedule = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            local_workspace(),
            PolicyIntent::local_developer(),
        )
        .with_requested_tools(["read_file"])
        .require_executor_kind(ExecutorBindingKind::PersonalRunner);
        let decision = schedule_runner(&pool, "user-1", schedule).await;
        let target = decision.selected.expect("runner selected");

        let prepare = RunnerRpcTransport::prepare_session(
            &pool,
            "r1",
            RunnerPrepareSessionRequest {
                request_id: "prepare-1".to_string(),
                spec: target.session_spec,
            },
        )
        .await
        .expect("prepare response");
        let handle = match prepare {
            RunnerPrepareSessionResponse::Prepared { handle } => handle,
            RunnerPrepareSessionResponse::Rejected { error } => {
                panic!("unexpected reject: {error}")
            }
        };
        let invocation = astra_runtime_env::RuntimeToolInvocation::new(
            "call-1",
            "read_file",
            json!({"path": "README.md"}),
            target.binding,
            handle.policy.revision,
        );
        let execute = RunnerRpcTransport::execute_tool(
            &pool,
            "r1",
            RunnerExecuteToolRequest {
                request_id: "execute-1".to_string(),
                session: *handle,
                invocation,
                idempotency_key: "idem-1".to_string(),
            },
        )
        .await
        .expect("execute response");

        match execute {
            RunnerExecuteToolResponse::Completed { outcome } => {
                assert_eq!(outcome.output, "http-runner-result");
                assert!(!outcome.is_error);
            }
            RunnerExecuteToolResponse::Rejected { error } => panic!("unexpected reject: {error}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn runner_pool_transport_rejects_registered_runner_without_endpoint() {
        let pool = ServerRunnerPool::default();
        let request = valid_register_request("user-1", "r1");
        let binding = request.advertisement.binding.clone();
        let response = register(&pool, "user-1", request).await;
        assert!(response.accepted);
        let session_spec =
            astra_runtime_env::RuntimeSessionSpec::new("session-1", "run-1", binding)
                .with_requested_tools(["read_file"]);

        let error = RunnerRpcTransport::prepare_session(
            &pool,
            "r1",
            RunnerPrepareSessionRequest {
                request_id: "prepare-1".to_string(),
                spec: session_spec,
            },
        )
        .await
        .expect_err("missing endpoint should reject");

        assert_eq!(
            error.kind,
            astra_runtime_env::RuntimeErrorKind::TransportUnavailable
        );
        assert_eq!(
            error.next_action,
            astra_runtime_env::RuntimeRecoveryAction::ReconnectRunner
        );
        assert!(error.message.contains("no RPC endpoint"));
    }

    #[tokio::test]
    async fn runner_pool_transport_rejects_expired_runner_lease() {
        let store = InMemoryRunnerPoolStore::default();
        let pool = runner_pool_with_store(store.clone());
        let request =
            valid_register_request_with_endpoint("user-1", "r1", public_runner_endpoint());
        let entry = RunnerPoolEntry::from_register_request(request.clone(), RunnerStatus::Idle)
            .expect("valid entry");
        let binding = entry.advertisement.binding.clone();
        insert_stored_runner(&store, entry, now_epoch_ms().saturating_sub(1)).await;
        let session_spec =
            astra_runtime_env::RuntimeSessionSpec::new("session-1", "run-1", binding)
                .with_requested_tools(["read_file"]);

        let error = RunnerRpcTransport::prepare_session(
            &pool,
            "r1",
            RunnerPrepareSessionRequest {
                request_id: "prepare-1".to_string(),
                spec: session_spec,
            },
        )
        .await
        .expect_err("expired runner lease should reject");

        assert_eq!(
            error.kind,
            astra_runtime_env::RuntimeErrorKind::RunnerLeaseExpired
        );
        assert_eq!(
            error.next_action,
            astra_runtime_env::RuntimeRecoveryAction::ReconnectRunner
        );
        assert!(error.message.contains("lease has expired"));
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn database_runner_pool_persists_registration_across_pool_instances() {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        let settings = crate::MatrixOneSettings::from_env();
        let shared = SharedPool::new(&settings)
            .await
            .expect("connect to MatrixOne");
        let runner_id = "db-runner-persist-test";
        sqlx::query("DELETE FROM runner_pool WHERE runner_id = ?")
            .bind(runner_id)
            .execute(shared.get())
            .await
            .ok();

        let first = ServerRunnerPool::database(&shared)
            .await
            .expect("first runner pool");
        let response = register(
            &first,
            "user-1",
            valid_register_request_with_endpoint("user-1", runner_id, public_runner_endpoint()),
        )
        .await;
        assert!(response.accepted);

        let second = ServerRunnerPool::database(&shared)
            .await
            .expect("second runner pool");
        let listed = list(&second, "user-1").await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].identity.runner_id, runner_id);

        let decision = schedule_runner(
            &second,
            "user-1",
            RunnerScheduleRequest::new(
                "session-1",
                "run-1",
                local_workspace(),
                PolicyIntent::local_developer(),
            )
            .with_requested_tools(["bash"])
            .require_executor_kind(ExecutorBindingKind::PersonalRunner),
        )
        .await;
        assert_eq!(
            decision
                .selected
                .as_ref()
                .map(|target| target.runner_id.as_str()),
            Some(runner_id)
        );

        sqlx::query("DELETE FROM runner_pool WHERE runner_id = ?")
            .bind(runner_id)
            .execute(shared.get())
            .await
            .ok();
    }
}
