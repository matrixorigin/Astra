//! Edge agent registry: register, list, and heartbeat edge agents.
//!
//! Split from the monolithic `multi_agent.rs`.

use std::sync::atomic::Ordering;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::metrics::SharedMultiAgentMetrics;
use crate::db_row::RowExt as EdgeRegistryDbRow;

const CURRENT_READ_MAX_ATTEMPTS: u32 = 6;
const CURRENT_READ_BASE_BACKOFF_MS: u64 = 25;

// MatrixOne exposes JSON columns with SQL type JSON, while RowExt intentionally
// decodes this optional payload as text before serde_json validation. Wrapping
// the parsed value in a one-element JSON array lets JSON_UNQUOTE produce
// unbounded text without stripping quotes from a top-level JSON string;
// SUBSTRING removes only the wrapper brackets. JSON_EXTRACT first normalizes
// both the canonical TEXT schema and legacy JSON-typed columns to the same JSON
// value. Keep every EdgeAgentRecord read on this projection so no query asks
// sqlx to decode JSON directly or uses VARCHAR(65535)-bounded CAST(... AS CHAR).
const EDGE_AGENT_RECORD_COLUMNS: &str = "registry_id, user_id, edge_agent_id, edge_id, hostname, worktree_path, \
     CASE WHEN capabilities_json IS NULL THEN NULL ELSE \
       SUBSTRING(JSON_UNQUOTE(JSON_ARRAY(JSON_EXTRACT(capabilities_json, '$'))), 2, \
         CHAR_LENGTH(JSON_UNQUOTE(JSON_ARRAY(JSON_EXTRACT(capabilities_json, '$')))) - 2) \
     END AS capabilities_json, workspace_id, \
     CAST(registered_at AS CHAR) AS registered_at, \
     CAST(last_heartbeat_at AS CHAR) AS last_heartbeat_at";

// One generation-scoped cleanup statement covers both possible ownership
// positions during a reconnect:
// - `edge_id = target`: the target is the current/predecessor generation. Its
//   private metadata is scrubbed and it becomes inactive. A successor claim is
//   preserved while state is 0/1 so setup can still complete.
// - `registration_previous_edge_id = target`: the successor is finalized but
//   unpublished. Clearing only the predecessor marker records that rollback
//   must not resurrect the disconnected target.
// Assignment order is intentional because MySQL-compatible engines evaluate
// single-table UPDATE assignments from left to right.
const DEACTIVATE_EDGE_GENERATION_SQL: &str = "UPDATE edge_agent_registry \
    SET registration_claim_id = CASE \
            WHEN (registration_state IN (0, 1) AND edge_id = ?) \
              OR (registration_state = 2 AND registration_previous_edge_id = ?) \
            THEN registration_claim_id ELSE NULL END, \
        registration_claim_expires_at = CASE \
            WHEN (registration_state IN (0, 1) AND edge_id = ?) \
              OR (registration_state = 2 AND registration_previous_edge_id = ?) \
            THEN registration_claim_expires_at ELSE NULL END, \
        hostname = CASE WHEN edge_id = ? THEN NULL ELSE hostname END, \
        worktree_path = CASE WHEN edge_id = ? THEN NULL ELSE worktree_path END, \
        capabilities_json = CASE WHEN edge_id = ? THEN NULL ELSE capabilities_json END, \
        workspace_id = CASE WHEN edge_id = ? THEN NULL ELSE workspace_id END, \
        registration_state = CASE \
            WHEN registration_state = 2 AND registration_previous_edge_id = ? \
            THEN registration_state ELSE 0 END, \
        registration_previous_edge_id = NULL \
    WHERE user_id = ? AND edge_agent_id = ? \
      AND (edge_id = ? \
           OR (registration_state = 2 AND registration_previous_edge_id = ?))";

fn deactivate_edge_generation_query<'q>(
    user_id: &'q str,
    edge_agent_id: &'q str,
    edge_id: &'q str,
) -> sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments> {
    sqlx::query(DEACTIVATE_EDGE_GENERATION_SQL)
        .bind(edge_id)
        .bind(edge_id)
        .bind(edge_id)
        .bind(edge_id)
        .bind(edge_id)
        .bind(edge_id)
        .bind(edge_id)
        .bind(edge_id)
        .bind(edge_id)
        .bind(user_id)
        .bind(edge_agent_id)
        .bind(edge_id)
        .bind(edge_id)
}

fn serialize_edge_capabilities(
    capabilities: Option<&serde_json::Value>,
) -> Result<Option<String>, String> {
    capabilities
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| format!("capabilities json: {error}"))
}

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
    /// None only for explicitly unscoped first-party registrations.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegistrationTransition {
    Finalize,
    Release,
    Rollback,
}

impl RegistrationTransition {
    fn operation(self) -> &'static str {
        match self {
            Self::Finalize => "finalize",
            Self::Release => "release",
            Self::Rollback => "rollback",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RegistrationState {
    edge_id: String,
    claim_id: Option<String>,
    state: i8,
    previous_edge_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RegistrationGenerationState {
    edge_id: String,
    claim_id: Option<String>,
    previous_edge_id: Option<String>,
    state: i8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegistrationTransitionDecision {
    AlreadyApplied,
    Apply,
    Superseded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegistrationAttemptDecision {
    Retry,
    Transition(RegistrationTransitionDecision),
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GenerationMutation {
    Heartbeat,
    Unregister,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GenerationMutationOutcome {
    Applied,
    AlreadyApplied,
    Superseded,
}

fn heartbeat_generation_is_owned(
    row: &RegistrationGenerationState,
    edge_id: &str,
    claim_id: Option<&str>,
) -> bool {
    (row.state == 1 && row.edge_id == edge_id)
        || (row.state == 2
            && (row.previous_edge_id.as_deref() == Some(edge_id)
                || (row.edge_id == edge_id
                    && claim_id.is_some()
                    && row.claim_id.as_deref() == claim_id)))
}

fn unregister_generation_is_owned(row: &RegistrationGenerationState, edge_id: &str) -> bool {
    row.edge_id == edge_id || (row.state == 2 && row.previous_edge_id.as_deref() == Some(edge_id))
}

fn unregister_generation_is_already_applied(
    row: &RegistrationGenerationState,
    edge_id: &str,
) -> bool {
    row.edge_id == edge_id && row.state == 0
}

fn heartbeat_generation_query<'q>(
    user_id: &'q str,
    edge_agent_id: &'q str,
    edge_id: &'q str,
    claim_id: Option<&'q str>,
) -> sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments> {
    sqlx::query(
        "UPDATE edge_agent_registry \
         SET last_heartbeat_at = NOW(6), \
             registration_claim_expires_at = CASE \
                 WHEN registration_state = 2 AND edge_id = ? \
                      AND registration_claim_id = ? \
                 THEN DATE_ADD(NOW(6), INTERVAL 120 SECOND) \
                 ELSE registration_claim_expires_at END \
         WHERE user_id = ? AND edge_agent_id = ? \
           AND ((registration_state = 1 AND edge_id = ?) \
                OR (registration_state = 2 AND registration_previous_edge_id = ?) \
                OR (registration_state = 2 AND edge_id = ? \
                    AND registration_claim_id = ?))",
    )
    .bind(edge_id)
    .bind(claim_id)
    .bind(user_id)
    .bind(edge_agent_id)
    .bind(edge_id)
    .bind(edge_id)
    .bind(edge_id)
    .bind(claim_id)
}

fn registration_predecessor_is_live(row: &RegistrationState, predecessor_edge_id: &str) -> bool {
    (row.state == 1 && row.edge_id == predecessor_edge_id)
        || (row.state == 2 && row.previous_edge_id.as_deref() == Some(predecessor_edge_id))
}

fn registration_transition_decision(
    transition: RegistrationTransition,
    lease: &EdgeRegistrationLease,
    claim_id: &str,
    row: Option<&RegistrationState>,
) -> RegistrationTransitionDecision {
    match transition {
        RegistrationTransition::Finalize => match row {
            Some(row)
                if row.edge_id == lease.current.edge_id
                    && row.claim_id.as_deref() == Some(claim_id)
                    && row.state == 2 =>
            {
                RegistrationTransitionDecision::AlreadyApplied
            }
            Some(row) if row.claim_id.as_deref() == Some(claim_id) => {
                RegistrationTransitionDecision::Apply
            }
            Some(row) if row.claim_id.as_deref() != Some(claim_id) => {
                RegistrationTransitionDecision::Superseded
            }
            None | Some(_) => RegistrationTransitionDecision::Superseded,
        },
        RegistrationTransition::Release => match row {
            Some(row)
                if row.edge_id == lease.current.edge_id
                    && row.claim_id.is_none()
                    && row.state == 1 =>
            {
                RegistrationTransitionDecision::AlreadyApplied
            }
            Some(row) if row.claim_id.as_deref() == Some(claim_id) => {
                RegistrationTransitionDecision::Apply
            }
            Some(row) if row.claim_id.as_deref() != Some(claim_id) => {
                RegistrationTransitionDecision::Superseded
            }
            None | Some(_) => RegistrationTransitionDecision::Superseded,
        },
        RegistrationTransition::Rollback => match (&lease.previous, row) {
            (_, Some(row))
                if row.edge_id == lease.current.edge_id
                    && row.claim_id.is_none()
                    && row.state == 0 =>
            {
                RegistrationTransitionDecision::AlreadyApplied
            }
            (Some(previous), Some(row))
                if row.edge_id == previous.edge_id && row.claim_id.is_none() && row.state == 1 =>
            {
                RegistrationTransitionDecision::AlreadyApplied
            }
            (_, Some(row)) if row.claim_id.as_deref() == Some(claim_id) => {
                RegistrationTransitionDecision::Apply
            }
            (_, Some(_)) | (_, None) => RegistrationTransitionDecision::Superseded,
        },
    }
}

fn registration_before_mutation_decision(
    transition: RegistrationTransition,
    lease: &EdgeRegistrationLease,
    claim_id: &str,
    row: Option<&RegistrationState>,
    attempt: u32,
    max_attempts: u32,
) -> RegistrationAttemptDecision {
    if row.is_none() {
        if attempt + 1 < max_attempts {
            return RegistrationAttemptDecision::Retry;
        }
        return RegistrationAttemptDecision::OutcomeUnknown;
    }
    RegistrationAttemptDecision::Transition(registration_transition_decision(
        transition, lease, claim_id, row,
    ))
}

async fn transition_error_with_rollback(
    transaction: sqlx::Transaction<'_, sqlx::MySql>,
    operation: &str,
    registry_id: &str,
    error: impl std::fmt::Display,
) -> String {
    match transaction.rollback().await {
        Ok(()) => format!("edge_registry {operation} registration {registry_id}: {error}"),
        Err(rollback_error) => format!(
            "edge_registry {operation} registration {registry_id}: {error}; transaction rollback failed: {rollback_error}"
        ),
    }
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
    /// Durable claiming backends return true when rollback is applied or was
    /// already applied, false only after verifying another generation, and an
    /// error when ownership cannot be established. Non-claiming backends use
    /// their ordinary generation-scoped cleanup result.
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
    /// Durable claiming backends return true when release is applied or was
    /// already applied, false only after verifying another generation, and an
    /// error when ownership cannot be established. Backends without durable
    /// claim support have nothing to release.
    async fn release_registration(&self, _lease: &EdgeRegistrationLease) -> Result<bool, String> {
        Ok(false)
    }

    /// Finalize the durable generation while retaining the cross-pod claim.
    /// Durable claiming backends return true when finalization is applied or
    /// was already applied, false only after verifying another generation, and
    /// an error when ownership cannot be established. The default registration
    /// path is already final, so non-claiming backends have nothing else to do.
    async fn finalize_registration(&self, _lease: &EdgeRegistrationLease) -> Result<bool, String> {
        Ok(true)
    }

    /// Refresh this exact connection generation. While a finalized durable
    /// claim is awaiting release, `registration_claim_id` also fences and
    /// renews that claim; ordinary published/non-durable registrations pass
    /// `None`.
    async fn heartbeat(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id_header: &str,
        registration_claim_id: Option<&str>,
    ) -> Result<(), HeartbeatError>;

    /// Find the most-recently-active registry record for a given edge_agent_id,
    /// scoped to a workspace when `workspace_id` is `Some`.  Used by the
    /// cross-pod dispatch path to locate a sandbox edge registered under a
    /// service-account user.
    ///
    /// Workspace isolation is fail-closed:
    /// - `Some(ws)` only matches rows with the same `workspace_id = ws`.
    /// - `None` only matches explicitly unscoped rows where `workspace_id IS NULL`.
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
    /// Durable backends retain an inactive, non-routable owner row so a later
    /// retry has authoritative idempotence evidence; they return false only
    /// after verifying that a newer connection replaced it. Non-durable
    /// backends may return false because there is no persistent generation to
    /// deactivate.
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

    async fn load_registration_state_for_update(
        transaction: &mut sqlx::Transaction<'_, sqlx::MySql>,
        user_id: &str,
        registry_id: &str,
    ) -> Result<Option<RegistrationState>, sqlx::Error> {
        let row: Option<(String, Option<String>, i8, Option<String>)> = sqlx::query_as(
            "SELECT edge_id, registration_claim_id, registration_state, \
                    registration_previous_edge_id \
             FROM edge_agent_registry WHERE user_id = ? AND registry_id = ? FOR UPDATE",
        )
        .bind(user_id)
        .bind(registry_id)
        .fetch_optional(&mut **transaction)
        .await?;
        Ok(row.map(
            |(edge_id, claim_id, state, previous_edge_id)| RegistrationState {
                edge_id,
                claim_id,
                state,
                previous_edge_id,
            },
        ))
    }

    /// Establish a write-write conflict boundary before trusting a registry
    /// observation. MatrixOne optimistic transactions do not acquire a current
    /// row lock for `SELECT ... FOR UPDATE`, while a no-op UPDATE participates
    /// in commit-time conflict detection.
    async fn establish_registry_current_read(
        transaction: &mut sqlx::Transaction<'_, sqlx::MySql>,
        user_id: &str,
        registry_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE edge_agent_registry SET last_heartbeat_at = last_heartbeat_at \
             WHERE user_id = ? AND registry_id = ?",
        )
        .bind(user_id)
        .bind(registry_id)
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }

    async fn establish_generation_current_read(
        transaction: &mut sqlx::Transaction<'_, sqlx::MySql>,
        user_id: &str,
        edge_agent_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE edge_agent_registry SET last_heartbeat_at = last_heartbeat_at \
             WHERE user_id = ? AND edge_agent_id = ?",
        )
        .bind(user_id)
        .bind(edge_agent_id)
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }

    async fn load_generation_state(
        transaction: &mut sqlx::Transaction<'_, sqlx::MySql>,
        user_id: &str,
        edge_agent_id: &str,
    ) -> Result<Option<RegistrationGenerationState>, sqlx::Error> {
        let row: Option<(String, Option<String>, Option<String>, i8)> = sqlx::query_as(
            "SELECT edge_id, registration_claim_id, registration_previous_edge_id, registration_state \
             FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?",
        )
        .bind(user_id)
        .bind(edge_agent_id)
        .fetch_optional(&mut **transaction)
        .await?;
        Ok(row.map(
            |(edge_id, claim_id, previous_edge_id, state)| RegistrationGenerationState {
                edge_id,
                claim_id,
                previous_edge_id,
                state,
            },
        ))
    }

    async fn settle_generation_mutation_after_miss(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id: &str,
        registration_claim_id: Option<&str>,
        mutation: GenerationMutation,
    ) -> Result<GenerationMutationOutcome, String> {
        let operation = match mutation {
            GenerationMutation::Heartbeat => "heartbeat",
            GenerationMutation::Unregister => "unregister",
        };
        for attempt in 0..CURRENT_READ_MAX_ATTEMPTS {
            if attempt > 0 {
                if let Some(ref metrics) = self.metrics {
                    metrics.registry_retry_total.fetch_add(1, Ordering::Relaxed);
                }
                tokio::time::sleep(std::time::Duration::from_millis(
                    CURRENT_READ_BASE_BACKOFF_MS * (1 << (attempt - 1)),
                ))
                .await;
            }

            let mut transaction = self.pool.begin().await.map_err(|error| {
                format!("edge_registry {operation} verification begin (attempt {attempt}): {error}")
            })?;
            if let Err(error) =
                Self::establish_generation_current_read(&mut transaction, user_id, edge_agent_id)
                    .await
            {
                return Err(transition_error_with_rollback(
                    transaction,
                    operation,
                    edge_agent_id,
                    format!("current-read barrier failed on attempt {attempt}: {error}"),
                )
                .await);
            }
            let row =
                match Self::load_generation_state(&mut transaction, user_id, edge_agent_id).await {
                    Ok(row) => row,
                    Err(error) => {
                        return Err(transition_error_with_rollback(
                            transaction,
                            operation,
                            edge_agent_id,
                            format!("ownership lookup failed on attempt {attempt}: {error}"),
                        )
                        .await);
                    }
                };

            let Some(row) = row else {
                if attempt + 1 < CURRENT_READ_MAX_ATTEMPTS {
                    transaction.rollback().await.map_err(|error| {
                        format!(
                            "edge_registry {operation} retry rollback (attempt {attempt}): {error}"
                        )
                    })?;
                    continue;
                }
                transaction.rollback().await.map_err(|error| {
                    format!(
                        "edge_registry {operation} absent-state rollback (attempt {attempt}): {error}"
                    )
                })?;
                return Err(format!(
                    "edge_registry generation {operation} remained absent after {CURRENT_READ_MAX_ATTEMPTS} current-read attempts"
                ));
            };
            if mutation == GenerationMutation::Unregister
                && unregister_generation_is_already_applied(&row, edge_id)
            {
                transaction.commit().await.map_err(|error| {
                    format!("edge_registry {operation} idempotent verification commit: {error}")
                })?;
                return Ok(GenerationMutationOutcome::AlreadyApplied);
            }
            let owned = match mutation {
                GenerationMutation::Heartbeat => {
                    heartbeat_generation_is_owned(&row, edge_id, registration_claim_id)
                }
                GenerationMutation::Unregister => unregister_generation_is_owned(&row, edge_id),
            };
            if !owned {
                transaction.commit().await.map_err(|error| {
                    format!("edge_registry {operation} supersession verification commit: {error}")
                })?;
                return Ok(GenerationMutationOutcome::Superseded);
            }

            let affected = match mutation {
                GenerationMutation::Heartbeat => {
                    heartbeat_generation_query(
                        user_id,
                        edge_agent_id,
                        edge_id,
                        registration_claim_id,
                    )
                    .execute(&mut *transaction)
                    .await
                }
                GenerationMutation::Unregister => {
                    deactivate_edge_generation_query(user_id, edge_agent_id, edge_id)
                        .execute(&mut *transaction)
                        .await
                }
            }
            .map_err(|error| {
                format!("edge_registry {operation} retry mutation (attempt {attempt}): {error}")
            })?
            .rows_affected();
            if affected == 0 {
                transaction.rollback().await.map_err(|error| {
                    format!("edge_registry {operation} retry rollback (attempt {attempt}): {error}")
                })?;
                continue;
            }
            transaction.commit().await.map_err(|error| {
                format!(
                    "edge_registry {operation} verification commit (attempt {attempt}): {error}"
                )
            })?;
            return Ok(GenerationMutationOutcome::Applied);
        }

        Err(format!(
            "edge_registry generation {operation} remained ambiguous after {CURRENT_READ_MAX_ATTEMPTS} current-read attempts"
        ))
    }

    async fn settle_registration_transition(
        &self,
        lease: &EdgeRegistrationLease,
        transition: RegistrationTransition,
    ) -> Result<bool, String> {
        let operation = transition.operation();
        let registry_id = lease.current.registry_id.as_str();
        let claim_id = lease.claim_id.as_deref().ok_or_else(|| {
            format!("edge_registry {operation} registration {registry_id} has no durable claim")
        })?;
        let current_capabilities =
            serialize_edge_capabilities(lease.current.capabilities.as_ref())?;
        let previous_capabilities = match &lease.previous {
            Some(previous) => serialize_edge_capabilities(previous.capabilities.as_ref())?,
            None => None,
        };

        for attempt in 0..CURRENT_READ_MAX_ATTEMPTS {
            if attempt > 0 {
                if let Some(ref metrics) = self.metrics {
                    metrics.registry_retry_total.fetch_add(1, Ordering::Relaxed);
                }
                tokio::time::sleep(std::time::Duration::from_millis(
                    CURRENT_READ_BASE_BACKOFF_MS * (1 << (attempt - 1)),
                ))
                .await;
            }

            let mut transaction = self.pool.begin().await.map_err(|error| {
                format!(
                    "edge_registry {operation} registration {registry_id} begin (attempt {attempt}): {error}"
                )
            })?;
            if let Err(error) = Self::establish_registry_current_read(
                &mut transaction,
                &lease.current.user_id,
                registry_id,
            )
            .await
            {
                return Err(transition_error_with_rollback(
                    transaction,
                    operation,
                    registry_id,
                    format!("current-read barrier failed on attempt {attempt}: {error}"),
                )
                .await);
            }
            let before = match Self::load_registration_state_for_update(
                &mut transaction,
                &lease.current.user_id,
                registry_id,
            )
            .await
            {
                Ok(row) => row,
                Err(error) => {
                    return Err(transition_error_with_rollback(
                        transaction,
                        operation,
                        registry_id,
                        format!("state lookup failed on attempt {attempt}: {error}"),
                    )
                    .await);
                }
            };

            let before_decision = registration_before_mutation_decision(
                transition,
                lease,
                claim_id,
                before.as_ref(),
                attempt,
                CURRENT_READ_MAX_ATTEMPTS,
            );
            match before_decision {
                RegistrationAttemptDecision::Retry => {
                    transaction.rollback().await.map_err(|error| {
                        format!(
                            "edge_registry {operation} registration {registry_id} retry rollback (attempt {attempt}): {error}"
                        )
                    })?;
                    continue;
                }
                RegistrationAttemptDecision::OutcomeUnknown => {
                    transaction.rollback().await.map_err(|error| {
                        format!(
                            "edge_registry {operation} registration {registry_id} absent-state rollback: {error}"
                        )
                    })?;
                    return Err(format!(
                        "edge_registry {operation} registration {registry_id} remained absent after {CURRENT_READ_MAX_ATTEMPTS} current-read attempts"
                    ));
                }
                RegistrationAttemptDecision::Transition(
                    RegistrationTransitionDecision::AlreadyApplied,
                ) => {
                    transaction.commit().await.map_err(|error| {
                        format!(
                            "edge_registry {operation} registration {registry_id} verification commit: {error}"
                        )
                    })?;
                    return Ok(true);
                }
                RegistrationAttemptDecision::Transition(
                    RegistrationTransitionDecision::Superseded,
                ) => {
                    // Commit the no-op write barrier before reporting a newer
                    // owner. On an optimistic snapshot, a stale predecessor
                    // observation conflicts here instead of becoming a false
                    // supersession result.
                    transaction.commit().await.map_err(|error| {
                        format!(
                            "edge_registry {operation} registration {registry_id} superseded verification commit: {error}"
                        )
                    })?;
                    return Ok(false);
                }
                RegistrationAttemptDecision::Transition(RegistrationTransitionDecision::Apply) => {}
            }

            let owned_before = before
                .as_ref()
                .expect("an applicable registration transition requires an observed owner row");
            let live_previous = lease.previous.as_ref().filter(|previous| {
                registration_predecessor_is_live(owned_before, &previous.edge_id)
            });

            let execution = match transition {
                RegistrationTransition::Finalize => {
                    sqlx::query(
                        "UPDATE edge_agent_registry \
                         SET edge_id = ?, hostname = ?, worktree_path = ?, capabilities_json = ?, \
                             workspace_id = ?, last_heartbeat_at = NOW(6), registration_state = 2, \
                             registration_previous_edge_id = ? \
                         WHERE user_id = ? AND registry_id = ? AND registration_claim_id = ?",
                    )
                    .bind(&lease.current.edge_id)
                    .bind(&lease.current.hostname)
                    .bind(&lease.current.worktree_path)
                    .bind(&current_capabilities)
                    .bind(&lease.current.workspace_id)
                    .bind(live_previous.map(|previous| previous.edge_id.as_str()))
                    .bind(&lease.current.user_id)
                    .bind(registry_id)
                    .bind(claim_id)
                    .execute(&mut *transaction)
                    .await
                }
                RegistrationTransition::Release => {
                    sqlx::query(
                        "UPDATE edge_agent_registry \
                         SET registration_claim_id = NULL, registration_claim_expires_at = NULL, \
                             registration_state = 1, registration_previous_edge_id = NULL \
                         WHERE user_id = ? AND registry_id = ? AND edge_id = ? \
                           AND registration_claim_id = ? AND registration_state = 2",
                    )
                    .bind(&lease.current.user_id)
                    .bind(registry_id)
                    .bind(&lease.current.edge_id)
                    .bind(claim_id)
                    .execute(&mut *transaction)
                    .await
                }
                RegistrationTransition::Rollback => match live_previous {
                    Some(previous) => {
                        sqlx::query(
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
                        .bind(&previous_capabilities)
                        .bind(&previous.workspace_id)
                        .bind(&lease.current.user_id)
                        .bind(registry_id)
                        .bind(claim_id)
                        .execute(&mut *transaction)
                        .await
                    }
                    None => {
                        // A first registration, or a replacement whose
                        // predecessor disconnected during setup, rolls back to
                        // a skeletal inactive owner. It is excluded from
                        // routing but remains durable settlement evidence.
                        sqlx::query(
                            "UPDATE edge_agent_registry \
                             SET edge_id = ?, hostname = NULL, worktree_path = NULL, \
                                 capabilities_json = NULL, workspace_id = NULL, \
                                 last_heartbeat_at = NOW(6), \
                                 registration_claim_id = NULL, registration_claim_expires_at = NULL, \
                                 registration_state = 0, registration_previous_edge_id = NULL \
                             WHERE user_id = ? AND registry_id = ? AND registration_claim_id = ?",
                        )
                        .bind(&lease.current.edge_id)
                        .bind(&lease.current.user_id)
                        .bind(registry_id)
                        .bind(claim_id)
                        .execute(&mut *transaction)
                        .await
                    }
                },
            };
            if let Err(error) = execution {
                return Err(transition_error_with_rollback(
                    transaction,
                    operation,
                    registry_id,
                    format!("mutation failed on attempt {attempt}: {error}"),
                )
                .await);
            }

            let after = match Self::load_registration_state_for_update(
                &mut transaction,
                &lease.current.user_id,
                registry_id,
            )
            .await
            {
                Ok(row) => row,
                Err(error) => {
                    return Err(transition_error_with_rollback(
                        transaction,
                        operation,
                        registry_id,
                        format!("state verification failed on attempt {attempt}: {error}"),
                    )
                    .await);
                }
            };
            if after.is_none() {
                return Err(transition_error_with_rollback(
                    transaction,
                    operation,
                    registry_id,
                    format!("mutation produced an unexpected absent state on attempt {attempt}"),
                )
                .await);
            }
            let after_decision =
                registration_transition_decision(transition, lease, claim_id, after.as_ref());
            match after_decision {
                RegistrationTransitionDecision::AlreadyApplied => {
                    transaction.commit().await.map_err(|error| {
                        format!(
                            "edge_registry {operation} registration {registry_id} commit (attempt {attempt}): {error}"
                        )
                    })?;
                    return Ok(true);
                }
                RegistrationTransitionDecision::Superseded => {
                    transaction.commit().await.map_err(|error| {
                        format!(
                            "edge_registry {operation} registration {registry_id} superseded verification commit (attempt {attempt}): {error}"
                        )
                    })?;
                    return Ok(false);
                }
                RegistrationTransitionDecision::Apply => {
                    transaction.rollback().await.map_err(|error| {
                        format!(
                            "edge_registry {operation} registration {registry_id} retry rollback (attempt {attempt}): {error}"
                        )
                    })?;
                }
            }
        }

        Err(format!(
            "edge_registry {operation} registration {registry_id} remained owned but did not reach its target state after {CURRENT_READ_MAX_ATTEMPTS} attempts"
        ))
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
        serialize_edge_capabilities(capabilities.as_ref())?;
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

            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(|e| format!("edge_registry lease begin (attempt {attempt}): {e}"))?;
            let lookup_sql = format!(
                "SELECT {EDGE_AGENT_RECORD_COLUMNS}, registration_state \
                 FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?"
            );
            let row = sqlx::query(&lookup_sql)
                .bind(user_id)
                .bind(edge_agent_id)
                .fetch_optional(&mut *transaction)
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
                         registration_claim_expires_at = DATE_ADD(NOW(6), INTERVAL 120 SECOND), \
                         hostname = CASE WHEN registration_state = 1 THEN hostname ELSE NULL END, \
                         worktree_path = CASE WHEN registration_state = 1 THEN worktree_path ELSE NULL END, \
                         capabilities_json = CASE WHEN registration_state = 1 THEN capabilities_json ELSE NULL END, \
                         workspace_id = CASE WHEN registration_state = 1 THEN workspace_id ELSE NULL END \
                     WHERE user_id = ? AND registry_id = ? AND edge_id = ? \
                       AND (registration_claim_id IS NULL \
                            OR registration_claim_expires_at < NOW(6))",
                )
                .bind(&claim_id)
                .bind(user_id)
                .bind(&previous.registry_id)
                .bind(&previous.edge_id)
                .execute(&mut *transaction)
                .await
                .map_err(|e| format!("edge_registry lease update (attempt {attempt}): {e}"))?
                .rows_affected();
                if updated == 0 {
                    transaction.rollback().await.map_err(|e| {
                        format!("edge_registry lease rollback (attempt {attempt}): {e}")
                    })?;
                    continue;
                }

                let now = chrono::Utc::now()
                    .format("%Y-%m-%d %H:%M:%S%.6f")
                    .to_string();
                // State 1 is the only published state. State 0 is an inactive
                // owner (either never published or disconnected while its
                // successor holds the claim), and state 2 is a finalized
                // generation whose claim is not released; neither is safe to
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
                let lease = EdgeRegistrationLease {
                    current,
                    previous: published_previous,
                    claim_id: Some(claim_id),
                };
                transaction
                    .commit()
                    .await
                    .map_err(|e| format!("edge_registry lease commit (attempt {attempt}): {e}"))?;
                return Ok(lease);
            }

            let registry_id = uuid::Uuid::new_v4().to_string();
            let inserted = sqlx::query(
                "INSERT INTO edge_agent_registry \
                 (registry_id, user_id, edge_agent_id, edge_id, registered_at, last_heartbeat_at, \
                  registration_claim_id, registration_claim_expires_at, registration_state) \
                 VALUES (?, ?, ?, ?, NOW(6), NOW(6), ?, \
                         DATE_ADD(NOW(6), INTERVAL 120 SECOND), 0)",
            )
            .bind(&registry_id)
            .bind(user_id)
            .bind(edge_agent_id)
            .bind(edge_id_header)
            .bind(&claim_id)
            .execute(&mut *transaction)
            .await;
            match inserted {
                Ok(_) => {
                    let now = chrono::Utc::now()
                        .format("%Y-%m-%d %H:%M:%S%.6f")
                        .to_string();
                    let lease = EdgeRegistrationLease {
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
                    };
                    transaction.commit().await.map_err(|e| {
                        format!("edge_registry lease commit (attempt {attempt}): {e}")
                    })?;
                    return Ok(lease);
                }
                Err(error) => {
                    transaction.rollback().await.map_err(|e| {
                        format!("edge_registry lease rollback (attempt {attempt}): {e}")
                    })?;
                    if is_duplicate_key_error(&error) {
                        continue;
                    }
                    return Err(format!("edge_registry lease insert: {error}"));
                }
            }
        }

        Err("edge_registry: exhausted generation-claim retries".to_string())
    }
}

fn is_duplicate_key_error(error: &sqlx::Error) -> bool {
    let sqlx::Error::Database(database) = error else {
        return false;
    };
    database
        .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
        .is_some_and(|error| error.number() == 1062)
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
        let cap_json = serialize_edge_capabilities(capabilities.as_ref())?;

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
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(|e| format!("edge_registry begin (attempt {attempt}): {e}"))?;
            // Fetch registry_id + registered_at in one query; construct the
            // response from in-memory data to eliminate the TOCTOU final SELECT.
            let existing: Option<(String, String)> = sqlx::query_as(
                "SELECT registry_id, CAST(registered_at AS CHAR) AS registered_at \
                 FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?",
            )
            .bind(user_id)
            .bind(edge_agent_id)
            .fetch_optional(&mut *transaction)
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
                .execute(&mut *transaction)
                .await
                .map_err(|e| format!("edge_registry update (attempt {attempt}): {e}"))?
                .rows_affected();
                if n == 0 {
                    transaction
                        .rollback()
                        .await
                        .map_err(|e| format!("edge_registry rollback (attempt {attempt}): {e}"))?;
                    continue; // deleted between SELECT and UPDATE
                }
                let now = chrono::Utc::now()
                    .format("%Y-%m-%d %H:%M:%S%.6f")
                    .to_string();
                let record = EdgeAgentRecord {
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
                };
                transaction
                    .commit()
                    .await
                    .map_err(|e| format!("edge_registry commit (attempt {attempt}): {e}"))?;
                return Ok(record);
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
            .execute(&mut *transaction)
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
                    .fetch_one(&mut *transaction)
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
                    let record = EdgeAgentRecord {
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
                    };
                    transaction
                        .commit()
                        .await
                        .map_err(|e| format!("edge_registry commit (attempt {attempt}): {e}"))?;
                    return Ok(record);
                }
                Err(e) => {
                    transaction.rollback().await.map_err(|rollback_error| {
                        format!(
                            "edge_registry rollback after insert failure (attempt {attempt}): {rollback_error}"
                        )
                    })?;
                    if is_duplicate_key_error(&e) {
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
        registration_claim_id: Option<&str>,
    ) -> Result<(), HeartbeatError> {
        // Guard on edge_id so a stale connection cannot refresh (or resurrect)
        // the row after a newer connection has replaced it. register_or_update
        // already set edge_id to the current connection's value, so we only
        // touch last_heartbeat_at and never rewrite edge_id here. If a newer
        // connection has taken over (edge_id differs), this matches 0 rows. A
        // finalized current generation also remains healthy if releasing its
        // durable claim had an outcome-unknown storage failure.
        let n = heartbeat_generation_query(
            user_id,
            edge_agent_id,
            edge_id_header,
            registration_claim_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| HeartbeatError::StorageFailure(format!("edge heartbeat: {e}")))?
        .rows_affected();
        if n == 0 {
            tracing::warn!(
                edge_id = %edge_id_header,
                "edge_registry: heartbeat matched no row; verifying durable generation ownership"
            );
            return match self
                .settle_generation_mutation_after_miss(
                    user_id,
                    edge_agent_id,
                    edge_id_header,
                    registration_claim_id,
                    GenerationMutation::Heartbeat,
                )
                .await
                .map_err(HeartbeatError::StorageFailure)?
            {
                GenerationMutationOutcome::Applied | GenerationMutationOutcome::AlreadyApplied => {
                    Ok(())
                }
                GenerationMutationOutcome::Superseded => Err(HeartbeatError::Superseded),
            };
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
        let deactivated = deactivate_edge_generation_query(user_id, edge_agent_id, edge_id_header)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("edge_registry unregister: {e}"))?;
        if deactivated.rows_affected() > 0 {
            return Ok(true);
        }
        match self
            .settle_generation_mutation_after_miss(
                user_id,
                edge_agent_id,
                edge_id_header,
                None,
                GenerationMutation::Unregister,
            )
            .await?
        {
            GenerationMutationOutcome::Applied | GenerationMutationOutcome::AlreadyApplied => {
                Ok(true)
            }
            GenerationMutationOutcome::Superseded => Ok(false),
        }
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
        debug_assert!(!claim_id.is_empty());
        self.settle_registration_transition(lease, RegistrationTransition::Rollback)
            .await
    }

    async fn finalize_registration(&self, lease: &EdgeRegistrationLease) -> Result<bool, String> {
        let Some(claim_id) = lease.claim_id.as_deref() else {
            return Ok(true);
        };
        debug_assert!(!claim_id.is_empty());
        self.settle_registration_transition(lease, RegistrationTransition::Finalize)
            .await
    }

    async fn release_registration(&self, lease: &EdgeRegistrationLease) -> Result<bool, String> {
        let Some(claim_id) = lease.claim_id.as_deref() else {
            return Ok(false);
        };
        debug_assert!(!claim_id.is_empty());
        self.settle_registration_transition(lease, RegistrationTransition::Release)
            .await
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
        let lookup_sql = format!(
            "SELECT {EDGE_AGENT_RECORD_COLUMNS} FROM edge_agent_registry \
             WHERE edge_agent_id = ? \
               AND registration_state = 1 \
               AND ((? IS NOT NULL AND workspace_id = ?) OR (? IS NULL AND workspace_id IS NULL)) \
             ORDER BY last_heartbeat_at DESC LIMIT 1"
        );
        let row = sqlx::query(&lookup_sql)
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
        let list_sql = format!(
            "SELECT {EDGE_AGENT_RECORD_COLUMNS} FROM edge_agent_registry \
             WHERE user_id = ? AND registration_state = 1 \
             ORDER BY last_heartbeat_at DESC"
        );
        let rows = sqlx::query(&list_sql)
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
        _registration_claim_id: Option<&str>,
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

    fn registration_lease(previous_edge_id: Option<&str>) -> EdgeRegistrationLease {
        let record = |edge_id: &str| EdgeAgentRecord {
            registry_id: "registry-1".to_string(),
            user_id: "user-1".to_string(),
            edge_agent_id: "agent-1".to_string(),
            edge_id: edge_id.to_string(),
            hostname: None,
            worktree_path: None,
            capabilities: None,
            workspace_id: Some("workspace-1".to_string()),
            registered_at: "2026-09-02 00:00:00.000000".to_string(),
            last_heartbeat_at: "2026-09-02 00:00:00.000000".to_string(),
        };
        EdgeRegistrationLease {
            current: record("edge-new"),
            previous: previous_edge_id.map(record),
            claim_id: Some("claim-1".to_string()),
        }
    }

    fn registration_state(edge_id: &str, claim_id: Option<&str>, state: i8) -> RegistrationState {
        RegistrationState {
            edge_id: edge_id.to_string(),
            claim_id: claim_id.map(ToString::to_string),
            state,
            previous_edge_id: None,
        }
    }

    fn registration_state_with_previous(
        edge_id: &str,
        claim_id: Option<&str>,
        state: i8,
        previous_edge_id: Option<&str>,
    ) -> RegistrationState {
        RegistrationState {
            previous_edge_id: previous_edge_id.map(ToString::to_string),
            ..registration_state(edge_id, claim_id, state)
        }
    }

    #[test]
    fn release_retries_an_owned_finalized_claim_and_accepts_idempotent_success() {
        let lease = registration_lease(Some("edge-old"));
        let before_finalize = registration_state("edge-old", Some("claim-1"), 1);
        assert_eq!(
            registration_transition_decision(
                RegistrationTransition::Release,
                &lease,
                "claim-1",
                Some(&before_finalize),
            ),
            RegistrationTransitionDecision::Apply,
            "the same claim may still expose its pre-finalize row through another DB session"
        );

        let owned = registration_state("edge-new", Some("claim-1"), 2);
        assert_eq!(
            registration_transition_decision(
                RegistrationTransition::Release,
                &lease,
                "claim-1",
                Some(&owned),
            ),
            RegistrationTransitionDecision::Apply
        );

        let released = registration_state("edge-new", None, 1);
        assert_eq!(
            registration_transition_decision(
                RegistrationTransition::Release,
                &lease,
                "claim-1",
                Some(&released),
            ),
            RegistrationTransitionDecision::AlreadyApplied
        );
    }

    #[test]
    fn registration_transition_only_reports_superseded_for_a_different_claim() {
        let lease = registration_lease(Some("edge-old"));
        let successor = registration_state("edge-successor", Some("claim-2"), 1);
        for transition in [
            RegistrationTransition::Finalize,
            RegistrationTransition::Release,
            RegistrationTransition::Rollback,
        ] {
            assert_eq!(
                registration_transition_decision(transition, &lease, "claim-1", Some(&successor),),
                RegistrationTransitionDecision::Superseded
            );
        }
    }

    #[test]
    fn rollback_accepts_both_restored_and_inactive_idempotent_states() {
        let replacement = registration_lease(Some("edge-old"));
        let restored = registration_state("edge-old", None, 1);
        assert_eq!(
            registration_transition_decision(
                RegistrationTransition::Rollback,
                &replacement,
                "claim-1",
                Some(&restored),
            ),
            RegistrationTransitionDecision::AlreadyApplied
        );

        let first_registration = registration_lease(None);
        let inactive = registration_state("edge-new", None, 0);
        assert_eq!(
            registration_transition_decision(
                RegistrationTransition::Rollback,
                &first_registration,
                "claim-1",
                Some(&inactive),
            ),
            RegistrationTransitionDecision::AlreadyApplied
        );

        let replacement_without_predecessor = registration_state("edge-new", None, 0);
        assert_eq!(
            registration_transition_decision(
                RegistrationTransition::Rollback,
                &replacement,
                "claim-1",
                Some(&replacement_without_predecessor),
            ),
            RegistrationTransitionDecision::AlreadyApplied
        );
    }

    #[test]
    fn predecessor_liveness_tracks_disconnects_before_and_after_finalize() {
        let pending_live = registration_state("edge-old", Some("claim-1"), 1);
        assert!(registration_predecessor_is_live(&pending_live, "edge-old"));

        let pending_disconnected = registration_state("edge-old", Some("claim-1"), 0);
        assert!(!registration_predecessor_is_live(
            &pending_disconnected,
            "edge-old"
        ));

        let finalized_live =
            registration_state_with_previous("edge-new", Some("claim-1"), 2, Some("edge-old"));
        assert!(registration_predecessor_is_live(
            &finalized_live,
            "edge-old"
        ));

        let finalized_disconnected =
            registration_state_with_previous("edge-new", Some("claim-1"), 2, None);
        assert!(!registration_predecessor_is_live(
            &finalized_disconnected,
            "edge-old"
        ));
    }

    #[test]
    fn first_registration_rollback_retries_a_stale_empty_read_before_deactivating_owned_claim() {
        let lease = registration_lease(None);
        let owned = registration_state("edge-new", Some("claim-1"), 0);

        assert_eq!(
            registration_before_mutation_decision(
                RegistrationTransition::Rollback,
                &lease,
                "claim-1",
                None,
                0,
                CURRENT_READ_MAX_ATTEMPTS,
            ),
            RegistrationAttemptDecision::Retry
        );
        assert_eq!(
            registration_before_mutation_decision(
                RegistrationTransition::Rollback,
                &lease,
                "claim-1",
                Some(&owned),
                1,
                CURRENT_READ_MAX_ATTEMPTS,
            ),
            RegistrationAttemptDecision::Transition(RegistrationTransitionDecision::Apply),
            "the retried current read must recover the committed pending claim"
        );
        assert_eq!(
            registration_before_mutation_decision(
                RegistrationTransition::Rollback,
                &lease,
                "claim-1",
                None,
                CURRENT_READ_MAX_ATTEMPTS - 1,
                CURRENT_READ_MAX_ATTEMPTS,
            ),
            RegistrationAttemptDecision::OutcomeUnknown,
            "absence cannot prove that a committed owner row was removed"
        );
        let inactive = registration_state("edge-new", None, 0);
        assert_eq!(
            registration_transition_decision(
                RegistrationTransition::Rollback,
                &lease,
                "claim-1",
                Some(&inactive),
            ),
            RegistrationTransitionDecision::AlreadyApplied,
            "the retained inactive owner row is authoritative rollback evidence"
        );
    }

    #[test]
    fn generation_ownership_covers_release_unknown_and_rejects_a_successor() {
        let release_unknown = RegistrationGenerationState {
            edge_id: "edge-new".to_string(),
            claim_id: Some("claim-1".to_string()),
            previous_edge_id: Some("edge-old".to_string()),
            state: 2,
        };
        assert!(heartbeat_generation_is_owned(
            &release_unknown,
            "edge-new",
            Some("claim-1")
        ));
        assert!(!heartbeat_generation_is_owned(
            &release_unknown,
            "edge-new",
            Some("claim-2")
        ));
        assert!(heartbeat_generation_is_owned(
            &release_unknown,
            "edge-old",
            None
        ));
        assert!(unregister_generation_is_owned(&release_unknown, "edge-new"));
        assert!(unregister_generation_is_owned(&release_unknown, "edge-old"));

        let inactive = RegistrationGenerationState {
            edge_id: "edge-new".to_string(),
            claim_id: None,
            previous_edge_id: None,
            state: 0,
        };
        assert!(unregister_generation_is_already_applied(
            &inactive, "edge-new"
        ));
        assert!(!heartbeat_generation_is_owned(&inactive, "edge-new", None));

        let successor = RegistrationGenerationState {
            edge_id: "edge-successor".to_string(),
            claim_id: None,
            previous_edge_id: None,
            state: 1,
        };
        assert!(!heartbeat_generation_is_owned(&successor, "edge-new", None));
        assert!(!unregister_generation_is_owned(&successor, "edge-new"));
    }

    #[test]
    fn edge_agent_record_projection_extracts_matrixone_json_as_unbounded_text() {
        assert!(
            EDGE_AGENT_RECORD_COLUMNS
                .contains("JSON_UNQUOTE(JSON_ARRAY(JSON_EXTRACT(capabilities_json, '$')))"),
        );
        assert!(EDGE_AGENT_RECORD_COLUMNS.contains("CHAR_LENGTH(JSON_UNQUOTE"));
        assert!(EDGE_AGENT_RECORD_COLUMNS.contains("END AS capabilities_json"));
        assert!(!EDGE_AGENT_RECORD_COLUMNS.contains("CAST(capabilities_json AS CHAR)"));
        assert!(!EDGE_AGENT_RECORD_COLUMNS.contains("\n         LENGTH(JSON_UNQUOTE"));
    }

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
