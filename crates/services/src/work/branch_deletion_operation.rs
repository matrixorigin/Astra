use super::{
    InternalSessionId, WorkBranchId, WorkBranchRevision, WorkId, WorkOwnerId, WorkRevision,
};
use astra_core::SharedPool;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::Row;
use thiserror::Error;
use uuid::Uuid;

pub const WORK_BRANCH_DELETION_OPERATION_SCHEMA_VERSION: u16 = 1;
const REQUEST_ID_MAX_BYTES: usize = 256;
const EXECUTOR_LEASE_MICROS: i64 = 60 * 1_000_000;
const MAX_RECOVERY_CLAIM_BATCH: u16 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchDeletionState {
    Pending,
    Succeeded,
    Conflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchDeletionPhase {
    Fence,
    SessionCleanup,
    LineageGc,
    BranchCleanup,
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchDeletionOutcome {
    Pending,
    Deleted,
    DeliveryBranchProtected,
    WorkRevisionConflict,
    BranchRevisionConflict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkBranchDeletionRequest {
    pub request_id: String,
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub expected_work_revision: WorkRevision,
    pub expected_branch_revision: WorkBranchRevision,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkBranchDeletionOperation {
    pub schema_version: u16,
    pub operation_id: String,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub state: WorkBranchDeletionState,
    pub phase: WorkBranchDeletionPhase,
    pub outcome: WorkBranchDeletionOutcome,
    pub work_revision: WorkRevision,
    pub branch_revision: WorkBranchRevision,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkBranchDeletionAdmission {
    pub operation: WorkBranchDeletionOperation,
    pub session_id: InternalSessionId,
}

#[derive(Clone, Debug)]
pub struct WorkBranchDeletionExecutionClaim {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub session_id: InternalSessionId,
    pub operation: WorkBranchDeletionOperation,
    pub executor_token: String,
}

#[derive(Debug, Error)]
pub enum WorkBranchDeletionError {
    #[error("invalid branch deletion request: {0}")]
    Invalid(String),
    #[error("Work branch was not found")]
    NotFound,
    #[error("branch deletion request identity was reused with different inputs")]
    IdempotencyMismatch,
    #[error("another deletion operation already owns the branch")]
    DeletionInProgress,
    #[error("branch session still has active runs")]
    ActiveRuns,
    #[error("branch deletion executor lease was lost")]
    ExecutorConflict,
    #[error("branch session cleanup failed: {0}")]
    SessionCleanup(String),
    #[error(
        "branch lineage cleanup is not terminal (manifests={orphaned_manifests}, pins={orphaned_pins}, forks={orphaned_forks}, references={orphaned_references})"
    )]
    LineagePending {
        orphaned_manifests: i64,
        orphaned_pins: i64,
        orphaned_forks: i64,
        orphaned_references: i64,
    },
    #[error("branch deletion operation was not found")]
    OperationNotFound,
    #[error("branch deletion state requires repair: {0}")]
    NeedsRepair(String),
    #[error("branch deletion database step {operation} failed: {source}")]
    Database {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
}

#[derive(Clone)]
pub struct DatabaseWorkBranchDeletionService {
    pool: SharedPool,
}

impl DatabaseWorkBranchDeletionService {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    /// Atomically establishes the sole durable owner of branch deletion.
    ///
    /// Admission does not delete anything. It advances the Work/branch CAS
    /// basis and leaves the branch visible with a deletion marker until a
    /// separately leased executor proves every cleanup phase terminal.
    pub async fn admit(
        &self,
        request: &WorkBranchDeletionRequest,
    ) -> Result<WorkBranchDeletionAdmission, WorkBranchDeletionError> {
        validate_request(request)?;
        let idempotency_hash = digest(request.request_id.as_bytes());
        let request_hash = request_digest(request);
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin branch deletion admission", source))?;
        let work = sqlx::query(
            "SELECT work_revision, delivery_branch_id FROM works
             WHERE owner_id = ? AND work_id = ? FOR UPDATE",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("lock Work for branch deletion", source))?
        .ok_or(WorkBranchDeletionError::NotFound)?;
        if let Some(row) = sqlx::query(
            "SELECT * FROM work_branch_deletion_operations
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND idempotency_hash = ? FOR UPDATE",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .bind(&idempotency_hash)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("load branch deletion retry", source))?
        {
            if row
                .try_get::<String, _>("request_hash")
                .map_err(|source| database_error("decode branch deletion request hash", source))?
                != request_hash
            {
                return Err(WorkBranchDeletionError::IdempotencyMismatch);
            }
            let admission = decode_admission(&row)?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit branch deletion retry", source))?;
            return Ok(admission);
        }
        let branch = sqlx::query(
            "SELECT branch_revision, session_id, deletion_operation_id
             FROM work_branches
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? FOR UPDATE",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("lock branch for deletion", source))?
        .ok_or(WorkBranchDeletionError::NotFound)?;
        if branch
            .try_get::<Option<String>, _>("deletion_operation_id")
            .map_err(|source| database_error("decode branch deletion owner", source))?
            .is_some()
        {
            return Err(WorkBranchDeletionError::DeletionInProgress);
        }
        let observed_work_revision: i64 = work
            .try_get("work_revision")
            .map_err(|source| database_error("decode Work revision", source))?;
        let observed_branch_revision: i64 = branch
            .try_get("branch_revision")
            .map_err(|source| database_error("decode branch revision", source))?;
        let session_id: String = branch
            .try_get("session_id")
            .map_err(|source| database_error("decode branch session", source))?;
        InternalSessionId::parse(session_id.clone())
            .map_err(|error| WorkBranchDeletionError::NeedsRepair(error.to_string()))?;
        let delivery_branch_id: String = work
            .try_get("delivery_branch_id")
            .map_err(|source| database_error("decode delivery branch", source))?;

        let operation_id = Uuid::new_v4().to_string();
        let created_at: DateTime<Utc> = sqlx::query_scalar("SELECT NOW(6)")
            .fetch_one(&mut *tx)
            .await
            .map_err(|source| database_error("load branch deletion time", source))?;
        let (state, phase, outcome, applied_work_revision, applied_branch_revision) =
            if observed_work_revision != request.expected_work_revision.get() {
                (
                    WorkBranchDeletionState::Conflict,
                    WorkBranchDeletionPhase::Complete,
                    WorkBranchDeletionOutcome::WorkRevisionConflict,
                    observed_work_revision,
                    observed_branch_revision,
                )
            } else if observed_branch_revision != request.expected_branch_revision.get() {
                (
                    WorkBranchDeletionState::Conflict,
                    WorkBranchDeletionPhase::Complete,
                    WorkBranchDeletionOutcome::BranchRevisionConflict,
                    observed_work_revision,
                    observed_branch_revision,
                )
            } else if delivery_branch_id == request.branch_id.as_str() {
                (
                    WorkBranchDeletionState::Conflict,
                    WorkBranchDeletionPhase::Complete,
                    WorkBranchDeletionOutcome::DeliveryBranchProtected,
                    observed_work_revision,
                    observed_branch_revision,
                )
            } else {
                let next_work = request
                    .expected_work_revision
                    .checked_next()
                    .map_err(|error| WorkBranchDeletionError::Invalid(error.to_string()))?;
                let next_branch = request
                    .expected_branch_revision
                    .checked_next()
                    .map_err(|error| WorkBranchDeletionError::Invalid(error.to_string()))?;
                let work_update = sqlx::query(
                    "UPDATE works SET work_revision = ?, updated_at = NOW(6)
                     WHERE owner_id = ? AND work_id = ? AND work_revision = ?",
                )
                .bind(next_work.get())
                .bind(request.owner_id.as_str())
                .bind(request.work_id.as_str())
                .bind(request.expected_work_revision.get())
                .execute(&mut *tx)
                .await
                .map_err(|source| database_error("advance Work deletion basis", source))?;
                let branch_update = sqlx::query(
                    "UPDATE work_branches
                     SET branch_revision = ?, deletion_operation_id = ?,
                         deletion_requested_at = ?, updated_at = NOW(6)
                     WHERE owner_id = ? AND work_id = ? AND branch_id = ?
                       AND branch_revision = ? AND deletion_operation_id IS NULL",
                )
                .bind(next_branch.get())
                .bind(&operation_id)
                .bind(created_at)
                .bind(request.owner_id.as_str())
                .bind(request.work_id.as_str())
                .bind(request.branch_id.as_str())
                .bind(request.expected_branch_revision.get())
                .execute(&mut *tx)
                .await
                .map_err(|source| database_error("mark branch deletion pending", source))?;
                if work_update.rows_affected() != 1 || branch_update.rows_affected() != 1 {
                    return Err(WorkBranchDeletionError::NeedsRepair(
                        "locked deletion basis changed inside one transaction".into(),
                    ));
                }
                (
                    WorkBranchDeletionState::Pending,
                    WorkBranchDeletionPhase::Fence,
                    WorkBranchDeletionOutcome::Pending,
                    next_work.get(),
                    next_branch.get(),
                )
            };
        let completed_at = (state != WorkBranchDeletionState::Pending).then_some(created_at);
        sqlx::query(
            "INSERT INTO work_branch_deletion_operations
             (owner_id, work_id, branch_id, operation_id, idempotency_hash,
              request_hash, session_id, operation_state, operation_phase,
              operation_outcome, expected_work_revision, expected_branch_revision,
              observed_work_revision, observed_branch_revision, created_at, completed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .bind(&operation_id)
        .bind(idempotency_hash)
        .bind(request_hash)
        .bind(&session_id)
        .bind(state_name(state))
        .bind(phase_name(phase))
        .bind(outcome_name(outcome))
        .bind(request.expected_work_revision.get())
        .bind(request.expected_branch_revision.get())
        .bind(applied_work_revision)
        .bind(applied_branch_revision)
        .bind(created_at)
        .bind(completed_at)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("insert branch deletion operation", source))?;
        let row = sqlx::query(
            "SELECT * FROM work_branch_deletion_operations
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .bind(operation_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|source| database_error("load admitted branch deletion", source))?;
        let admission = decode_admission(&row)?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit branch deletion admission", source))?;
        Ok(admission)
    }

    pub async fn load(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        operation_id: &str,
    ) -> Result<WorkBranchDeletionOperation, WorkBranchDeletionError> {
        let row = sqlx::query(
            "SELECT * FROM work_branch_deletion_operations
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load branch deletion operation", source))?
        .ok_or(WorkBranchDeletionError::OperationNotFound)?;
        Ok(decode_admission(&row)?.operation)
    }

    /// Claims one recoverable executor lease for a pending deletion.
    pub async fn claim_execution(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        operation_id: &str,
    ) -> Result<Option<String>, WorkBranchDeletionError> {
        let token = Uuid::new_v4().simple().to_string();
        let result = sqlx::query(
            "UPDATE work_branch_deletion_operations
             SET executor_token = ?,
                 executor_lease_expires_at = DATE_ADD(NOW(6), INTERVAL ? MICROSECOND)
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?
               AND operation_state = 'pending'
               AND (executor_lease_expires_at IS NULL OR executor_lease_expires_at <= NOW(6))",
        )
        .bind(&token)
        .bind(EXECUTOR_LEASE_MICROS)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| database_error("claim branch deletion executor", source))?;
        Ok((result.rows_affected() == 1).then_some(token))
    }

    /// Claims a bounded, globally ordered recovery batch.
    ///
    /// The covering pending index keeps the scan independent of tenant and
    /// session cardinality. Per-operation CAS claims make concurrent server
    /// workers disjoint without relying on process-local coordination.
    pub async fn claim_pending_executions(
        &self,
        limit: u16,
    ) -> Result<Vec<WorkBranchDeletionExecutionClaim>, WorkBranchDeletionError> {
        if limit == 0 || limit > MAX_RECOVERY_CLAIM_BATCH {
            return Err(WorkBranchDeletionError::Invalid(format!(
                "recovery claim limit must be between 1 and {MAX_RECOVERY_CLAIM_BATCH}"
            )));
        }
        let candidates = sqlx::query(
            "SELECT owner_id, work_id, branch_id, operation_id
             FROM work_branch_deletion_operations
             WHERE operation_state = 'pending'
               AND (executor_lease_expires_at IS NULL OR executor_lease_expires_at <= NOW(6))
             ORDER BY created_at ASC, operation_id ASC
             LIMIT ?",
        )
        .bind(i64::from(limit))
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| database_error("scan pending branch deletions", source))?;
        let mut claims = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let owner_id = WorkOwnerId::parse(
                candidate
                    .try_get::<String, _>("owner_id")
                    .map_err(|source| database_error("decode deletion recovery owner", source))?,
            )
            .map_err(|error| WorkBranchDeletionError::NeedsRepair(error.to_string()))?;
            let work_id = WorkId::parse(
                candidate
                    .try_get::<String, _>("work_id")
                    .map_err(|source| database_error("decode deletion recovery Work", source))?,
            )
            .map_err(|error| WorkBranchDeletionError::NeedsRepair(error.to_string()))?;
            let branch_id = WorkBranchId::parse(
                candidate
                    .try_get::<String, _>("branch_id")
                    .map_err(|source| database_error("decode deletion recovery branch", source))?,
            )
            .map_err(|error| WorkBranchDeletionError::NeedsRepair(error.to_string()))?;
            let operation_id = candidate
                .try_get::<String, _>("operation_id")
                .map_err(|source| database_error("decode deletion recovery operation", source))?;
            let Some(executor_token) = self
                .claim_execution(&owner_id, &work_id, &branch_id, &operation_id)
                .await?
            else {
                continue;
            };
            let admission = async {
                let row = sqlx::query(
                    "SELECT * FROM work_branch_deletion_operations
                     WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?",
                )
                .bind(owner_id.as_str())
                .bind(work_id.as_str())
                .bind(branch_id.as_str())
                .bind(&operation_id)
                .fetch_one(self.pool.get())
                .await
                .map_err(|source| database_error("load claimed branch deletion", source))?;
                decode_admission(&row)
            }
            .await;
            let admission = match admission {
                Ok(admission) => admission,
                Err(error) => {
                    if let Err(release_error) = self
                        .release_execution(
                            &owner_id,
                            &work_id,
                            &branch_id,
                            &operation_id,
                            &executor_token,
                        )
                        .await
                    {
                        tracing::warn!(
                            %release_error,
                            operation_id,
                            "failed to release undecodable deletion recovery claim"
                        );
                    }
                    return Err(error);
                }
            };
            claims.push(WorkBranchDeletionExecutionClaim {
                owner_id,
                work_id,
                branch_id,
                session_id: admission.session_id,
                operation: admission.operation,
                executor_token,
            });
        }
        Ok(claims)
    }

    pub async fn release_execution(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        operation_id: &str,
        executor_token: &str,
    ) -> Result<(), WorkBranchDeletionError> {
        sqlx::query(
            "UPDATE work_branch_deletion_operations
             SET executor_token = NULL, executor_lease_expires_at = NULL
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?
               AND operation_state = 'pending' AND executor_token = ?",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .bind(executor_token)
        .execute(self.pool.get())
        .await
        .map_err(|source| database_error("release branch deletion executor", source))?;
        Ok(())
    }

    /// Fences the session only after its durable execution slot is empty.
    ///
    /// The session row and context head share this transaction with the
    /// operation phase. Run-slot admission locks the same session row, so no
    /// new run can enter between the empty-slot proof and the writer epoch
    /// advance. Old writer leases and turn reservations then fail closed.
    pub async fn fence_session(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        operation_id: &str,
        executor_token: &str,
    ) -> Result<WorkBranchDeletionOperation, WorkBranchDeletionError> {
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin branch deletion fence", source))?;
        let row = sqlx::query(
            "SELECT * FROM work_branch_deletion_operations
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?
             FOR UPDATE",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("lock branch deletion operation", source))?
        .ok_or(WorkBranchDeletionError::OperationNotFound)?;
        let admission = decode_admission(&row)?;
        if admission.operation.state != WorkBranchDeletionState::Pending
            || admission.operation.phase != WorkBranchDeletionPhase::Fence
        {
            tx.commit()
                .await
                .map_err(|source| database_error("commit replayed deletion fence", source))?;
            return Ok(admission.operation);
        }
        verify_executor(&row, executor_token)?;
        let session = sqlx::query(
            "SELECT status FROM agent_sessions
             WHERE user_id = ? AND session_id = ? FOR UPDATE",
        )
        .bind(owner_id.as_str())
        .bind(admission.session_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("lock branch session for deletion", source))?
        .ok_or_else(|| {
            WorkBranchDeletionError::NeedsRepair(
                "branch deletion session disappeared before fencing".into(),
            )
        })?;
        let _status: String = session
            .try_get("status")
            .map_err(|source| database_error("decode branch session status", source))?;
        let active_slot: Option<String> = sqlx::query_scalar(
            "SELECT run_id FROM agent_session_execution_slots
             WHERE user_id = ? AND session_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(owner_id.as_str())
        .bind(admission.session_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("check branch session execution slot", source))?;
        if active_slot.is_some() {
            return Err(WorkBranchDeletionError::ActiveRuns);
        }
        sqlx::query(
            "UPDATE agent_sessions
             SET status = 'deleting', ended_at = COALESCE(ended_at, NOW(6)),
                 updated_at = NOW(6)
             WHERE user_id = ? AND session_id = ?",
        )
        .bind(owner_id.as_str())
        .bind(admission.session_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("mark branch session deleting", source))?;
        sqlx::query(
            "UPDATE session_context_heads
             SET writer_epoch = writer_epoch + 1,
                 active_writer_json = NULL, active_writer_expires_at_ms = NULL,
                 active_reservation_json = NULL, active_reservation_expires_at_ms = NULL,
                 updated_at = NOW(6)
             WHERE isolation_domain = 'server' AND owner_user_id = ? AND session_id = ?",
        )
        .bind(owner_id.as_str())
        .bind(admission.session_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("fence branch session writer", source))?;
        let updated = sqlx::query(
            "UPDATE work_branch_deletion_operations
             SET operation_phase = 'session_cleanup',
                 executor_lease_expires_at = DATE_ADD(NOW(6), INTERVAL ? MICROSECOND)
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?
               AND operation_state = 'pending' AND operation_phase = 'fence'
               AND executor_token = ? AND executor_lease_expires_at > NOW(6)",
        )
        .bind(EXECUTOR_LEASE_MICROS)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .bind(executor_token)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("advance branch deletion fence", source))?;
        if updated.rows_affected() != 1 {
            return Err(WorkBranchDeletionError::ExecutorConflict);
        }
        let operation = decode_admission(
            &sqlx::query(
                "SELECT * FROM work_branch_deletion_operations
                 WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?",
            )
            .bind(owner_id.as_str())
            .bind(work_id.as_str())
            .bind(branch_id.as_str())
            .bind(operation_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|source| database_error("load fenced branch deletion", source))?,
        )?
        .operation;
        tx.commit()
            .await
            .map_err(|source| database_error("commit branch deletion fence", source))?;
        Ok(operation)
    }

    /// Hard-deletes the fenced session and advances only after a fresh
    /// database read proves it absent. If a process crashes after the session
    /// transaction commits but before this phase update, a new executor sees
    /// the absence and resumes without requiring the destructive call to
    /// pretend that "not found" was a successful first attempt.
    pub async fn cleanup_session(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        operation_id: &str,
        executor_token: &str,
    ) -> Result<WorkBranchDeletionOperation, WorkBranchDeletionError> {
        let row = sqlx::query(
            "SELECT * FROM work_branch_deletion_operations
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load branch deletion session cleanup", source))?
        .ok_or(WorkBranchDeletionError::OperationNotFound)?;
        let admission = decode_admission(&row)?;
        if admission.operation.state != WorkBranchDeletionState::Pending
            || admission.operation.phase != WorkBranchDeletionPhase::SessionCleanup
        {
            return Ok(admission.operation);
        }
        verify_executor(&row, executor_token)?;
        let renewed = sqlx::query(
            "UPDATE work_branch_deletion_operations
             SET executor_lease_expires_at = DATE_ADD(NOW(6), INTERVAL ? MICROSECOND)
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?
               AND operation_state = 'pending' AND operation_phase = 'session_cleanup'
               AND executor_token = ? AND executor_lease_expires_at > NOW(6)",
        )
        .bind(EXECUTOR_LEASE_MICROS)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .bind(executor_token)
        .execute(self.pool.get())
        .await
        .map_err(|source| database_error("renew branch deletion session cleanup", source))?;
        if renewed.rows_affected() != 1 {
            return Err(WorkBranchDeletionError::ExecutorConflict);
        }
        let session_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                 SELECT 1 FROM agent_sessions WHERE user_id = ? AND session_id = ?
             )",
        )
        .bind(owner_id.as_str())
        .bind(admission.session_id.as_str())
        .fetch_one(self.pool.get())
        .await
        .map_err(|source| database_error("check branch deletion session", source))?;
        if session_exists {
            let outcome = crate::session_lifecycle::hard_delete_session(
                self.pool.get(),
                admission.session_id.as_str(),
                owner_id.as_str(),
            )
            .await
            .map_err(WorkBranchDeletionError::SessionCleanup)?;
            for error in outcome.cleanup_errors {
                tracing::warn!(
                    owner_id = owner_id.as_str(),
                    work_id = work_id.as_str(),
                    branch_id = branch_id.as_str(),
                    operation_id,
                    %error,
                    "branch deletion database cleanup committed with external cleanup debt"
                );
            }
        }
        let mut tx =
            self.pool.get().begin().await.map_err(|source| {
                database_error("begin branch deletion session completion", source)
            })?;
        let still_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                 SELECT 1 FROM agent_sessions WHERE user_id = ? AND session_id = ?
             )",
        )
        .bind(owner_id.as_str())
        .bind(admission.session_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|source| database_error("verify branch session deleted", source))?;
        if still_exists {
            return Err(WorkBranchDeletionError::NeedsRepair(
                "session cleanup returned while the durable session still exists".into(),
            ));
        }
        let updated = sqlx::query(
            "UPDATE work_branch_deletion_operations
             SET operation_phase = 'lineage_gc',
                 executor_lease_expires_at = DATE_ADD(NOW(6), INTERVAL ? MICROSECOND)
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?
               AND operation_state = 'pending' AND operation_phase = 'session_cleanup'
               AND executor_token = ? AND executor_lease_expires_at > NOW(6)",
        )
        .bind(EXECUTOR_LEASE_MICROS)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .bind(executor_token)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("complete branch session cleanup", source))?;
        if updated.rows_affected() != 1 {
            return Err(WorkBranchDeletionError::ExecutorConflict);
        }
        let operation = decode_admission(
            &sqlx::query(
                "SELECT * FROM work_branch_deletion_operations
                 WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?",
            )
            .bind(owner_id.as_str())
            .bind(work_id.as_str())
            .bind(branch_id.as_str())
            .bind(operation_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|source| database_error("load cleaned branch deletion", source))?,
        )?
        .operation;
        tx.commit()
            .await
            .map_err(|source| database_error("commit branch session cleanup", source))?;
        Ok(operation)
    }

    /// Proves that every retained lineage object is still owned by a valid
    /// descendant fork before branch-local records may disappear. Shared
    /// immutable payloads are intentionally not deleted here: owner-level
    /// segment GC remains bounded in the production reaper and may be shared
    /// by unrelated branches.
    pub async fn reconcile_lineage(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        operation_id: &str,
        executor_token: &str,
    ) -> Result<WorkBranchDeletionOperation, WorkBranchDeletionError> {
        let mut tx = self.pool.get().begin().await.map_err(|source| {
            database_error("begin branch deletion lineage reconciliation", source)
        })?;
        let row = sqlx::query(
            "SELECT * FROM work_branch_deletion_operations
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?
             FOR UPDATE",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("lock branch deletion lineage", source))?
        .ok_or(WorkBranchDeletionError::OperationNotFound)?;
        let admission = decode_admission(&row)?;
        if admission.operation.state != WorkBranchDeletionState::Pending
            || admission.operation.phase != WorkBranchDeletionPhase::LineageGc
        {
            tx.commit()
                .await
                .map_err(|source| database_error("commit replayed lineage cleanup", source))?;
            return Ok(admission.operation);
        }
        verify_executor(&row, executor_token)?;
        let orphaned_manifests: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_manifest_nodes node
             WHERE node.owner_user_id = ? AND node.session_id = ?
               AND NOT EXISTS (
                   SELECT 1 FROM conversation_manifest_pins pin
                   JOIN session_forks fork
                     ON fork.isolation_domain = pin.isolation_domain
                    AND fork.owner_user_id = pin.owner_user_id
                    AND fork.fork_id = pin.pin_id
                   WHERE pin.isolation_domain = node.isolation_domain
                     AND pin.owner_user_id = node.owner_user_id
                     AND pin.parent_session_id = node.session_id
                     AND pin.parent_branch_id = node.branch_id
                     AND (pin.pin_state IN ('prepared', 'active')
                          OR (pin.pin_state = 'grace'
                              AND pin.grace_expires_at_ms >
                                  CAST(UNIX_TIMESTAMP(NOW(6)) * 1000 AS SIGNED)))
               )",
        )
        .bind(owner_id.as_str())
        .bind(admission.session_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|source| database_error("verify retained branch manifests", source))?;
        let orphaned_pins: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_manifest_pins pin
             WHERE pin.owner_user_id = ? AND pin.parent_session_id = ?
               AND NOT EXISTS (
                   SELECT 1 FROM session_forks fork
                   WHERE fork.isolation_domain = pin.isolation_domain
                     AND fork.owner_user_id = pin.owner_user_id
                     AND fork.fork_id = pin.pin_id
               )",
        )
        .bind(owner_id.as_str())
        .bind(admission.session_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|source| database_error("verify retained branch pins", source))?;
        let orphaned_forks: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_forks fork
             WHERE fork.owner_user_id = ? AND fork.child_session_id = ?",
        )
        .bind(owner_id.as_str())
        .bind(admission.session_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|source| database_error("verify deleted child forks", source))?;
        let orphaned_references: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_manifest_segments segment
             WHERE segment.owner_user_id = ? AND segment.session_id = ?
               AND NOT EXISTS (
                   SELECT 1 FROM conversation_manifest_nodes node
                   WHERE node.isolation_domain = segment.isolation_domain
                     AND node.owner_user_id = segment.owner_user_id
                     AND node.session_id = segment.session_id
                     AND node.branch_id = segment.branch_id
                     AND node.manifest_root = segment.manifest_root
               )",
        )
        .bind(owner_id.as_str())
        .bind(admission.session_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|source| database_error("verify branch manifest references", source))?;
        if orphaned_manifests != 0
            || orphaned_pins != 0
            || orphaned_forks != 0
            || orphaned_references != 0
        {
            return Err(WorkBranchDeletionError::LineagePending {
                orphaned_manifests,
                orphaned_pins,
                orphaned_forks,
                orphaned_references,
            });
        }
        let updated = sqlx::query(
            "UPDATE work_branch_deletion_operations
             SET operation_phase = 'branch_cleanup',
                 executor_lease_expires_at = DATE_ADD(NOW(6), INTERVAL ? MICROSECOND)
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?
               AND operation_state = 'pending' AND operation_phase = 'lineage_gc'
               AND executor_token = ? AND executor_lease_expires_at > NOW(6)",
        )
        .bind(EXECUTOR_LEASE_MICROS)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .bind(executor_token)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("complete branch lineage cleanup", source))?;
        if updated.rows_affected() != 1 {
            return Err(WorkBranchDeletionError::ExecutorConflict);
        }
        let operation = decode_admission(
            &sqlx::query(
                "SELECT * FROM work_branch_deletion_operations
                 WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?",
            )
            .bind(owner_id.as_str())
            .bind(work_id.as_str())
            .bind(branch_id.as_str())
            .bind(operation_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|source| database_error("load reconciled branch deletion", source))?,
        )?
        .operation;
        tx.commit()
            .await
            .map_err(|source| database_error("commit branch lineage cleanup", source))?;
        Ok(operation)
    }

    /// Removes branch-owned Work projections and terminalizes the tombstone
    /// in one transaction. Immutable Work-level graph/criteria revisions and
    /// the bounded Work event audit are deliberately retained because sibling
    /// branches can still reference them.
    pub async fn complete_branch_cleanup(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        operation_id: &str,
        executor_token: &str,
    ) -> Result<WorkBranchDeletionOperation, WorkBranchDeletionError> {
        let mut tx =
            self.pool.get().begin().await.map_err(|source| {
                database_error("begin Work branch deletion completion", source)
            })?;
        let row = sqlx::query(
            "SELECT * FROM work_branch_deletion_operations
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?
             FOR UPDATE",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("lock Work branch deletion completion", source))?
        .ok_or(WorkBranchDeletionError::OperationNotFound)?;
        let admission = decode_admission(&row)?;
        if admission.operation.state != WorkBranchDeletionState::Pending
            || admission.operation.phase != WorkBranchDeletionPhase::BranchCleanup
        {
            tx.commit()
                .await
                .map_err(|source| database_error("commit replayed Work branch deletion", source))?;
            return Ok(admission.operation);
        }
        verify_executor(&row, executor_token)?;
        // Commit admission and completion lock commit operations before the
        // canonical Work/branch rows. Preserve that order here so branch
        // cleanup cannot deadlock with an in-flight provider receipt.
        sqlx::query(
            "DELETE FROM work_patch_commit_operations
             WHERE owner_id = ? AND work_id = ?
               AND ? IN (source_branch_id, target_branch_id)",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("delete branch patch commit operations", source))?;
        let work = sqlx::query(
            "SELECT delivery_branch_id FROM works
             WHERE owner_id = ? AND work_id = ? FOR UPDATE",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("lock Work deletion guard", source))?
        .ok_or_else(|| {
            WorkBranchDeletionError::NeedsRepair(
                "Work disappeared before branch deletion terminalized".into(),
            )
        })?;
        let delivery_branch: String = work
            .try_get("delivery_branch_id")
            .map_err(|source| database_error("decode Work deletion guard", source))?;
        if delivery_branch == branch_id.as_str() {
            return Err(WorkBranchDeletionError::NeedsRepair(
                "deleting branch became the delivery branch after admission".into(),
            ));
        }
        let marker: Option<String> = sqlx::query_scalar(
            "SELECT deletion_operation_id FROM work_branches
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? FOR UPDATE",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("lock deleting Work branch", source))?
        .flatten();
        if marker.as_deref() != Some(operation_id) {
            return Err(WorkBranchDeletionError::NeedsRepair(
                "branch deletion owner marker is missing or incoherent".into(),
            ));
        }
        for (operation, statement) in [
            (
                "delete branch patch materialization operations",
                "DELETE FROM work_patch_materialization_operations
                 WHERE owner_id = ? AND work_id = ?
                   AND ? IN (source_branch_id, target_branch_id)",
            ),
            (
                "delete branch patch artifacts",
                "DELETE FROM work_patch_artifacts
                 WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
            ),
            (
                "delete branch gap acceptances",
                "DELETE FROM work_current_gap_acceptances
                 WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
            ),
            (
                "delete branch acceptance decisions",
                "DELETE FROM work_acceptance_decisions
                 WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
            ),
            (
                "delete branch terminal cuts",
                "DELETE FROM work_terminal_cuts
                 WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
            ),
            (
                "delete branch item attempts",
                "DELETE FROM work_item_attempts
                 WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
            ),
            (
                "delete branch check runs",
                "DELETE FROM work_check_runs
                 WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
            ),
            (
                "delete branch proposals",
                "DELETE FROM work_proposals
                 WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
            ),
            (
                "delete branch proposal sequence",
                "DELETE FROM work_proposal_sequences
                 WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
            ),
            (
                "delete branch subject",
                "DELETE FROM work_branch_subjects
                 WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
            ),
            (
                "delete branch control operations",
                "DELETE FROM work_branch_control_operations
                 WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
            ),
        ] {
            sqlx::query(statement)
                .bind(owner_id.as_str())
                .bind(work_id.as_str())
                .bind(branch_id.as_str())
                .execute(&mut *tx)
                .await
                .map_err(|source| database_error(operation, source))?;
        }
        let deleted = sqlx::query(
            "DELETE FROM work_branches
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND deletion_operation_id = ?",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("delete Work branch", source))?;
        if deleted.rows_affected() != 1 {
            return Err(WorkBranchDeletionError::NeedsRepair(
                "marked Work branch did not delete exactly once".into(),
            ));
        }
        let terminalized = sqlx::query(
            "UPDATE work_branch_deletion_operations
             SET operation_state = 'succeeded', operation_phase = 'complete',
                 operation_outcome = 'deleted', completed_at = NOW(6),
                 executor_token = NULL, executor_lease_expires_at = NULL
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?
               AND operation_state = 'pending' AND operation_phase = 'branch_cleanup'
               AND executor_token = ? AND executor_lease_expires_at > NOW(6)",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .bind(executor_token)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("terminalize Work branch deletion", source))?;
        if terminalized.rows_affected() != 1 {
            return Err(WorkBranchDeletionError::ExecutorConflict);
        }
        let operation = decode_admission(
            &sqlx::query(
                "SELECT * FROM work_branch_deletion_operations
                 WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?",
            )
            .bind(owner_id.as_str())
            .bind(work_id.as_str())
            .bind(branch_id.as_str())
            .bind(operation_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|source| database_error("load completed Work branch deletion", source))?,
        )?
        .operation;
        tx.commit()
            .await
            .map_err(|source| database_error("commit Work branch deletion", source))?;
        Ok(operation)
    }
}

fn verify_executor(
    row: &sqlx::mysql::MySqlRow,
    executor_token: &str,
) -> Result<(), WorkBranchDeletionError> {
    let stored: Option<String> = row
        .try_get("executor_token")
        .map_err(|source| database_error("decode branch deletion executor", source))?;
    if stored.as_deref() != Some(executor_token) {
        return Err(WorkBranchDeletionError::ExecutorConflict);
    }
    Ok(())
}

fn validate_request(request: &WorkBranchDeletionRequest) -> Result<(), WorkBranchDeletionError> {
    if request.request_id.is_empty()
        || request.request_id.len() > REQUEST_ID_MAX_BYTES
        || request.request_id.trim() != request.request_id
    {
        return Err(WorkBranchDeletionError::Invalid(
            "request_id must be 1..=256 bytes without surrounding whitespace".into(),
        ));
    }
    Ok(())
}

fn request_digest(request: &WorkBranchDeletionRequest) -> String {
    let mut hasher = Sha256::new();
    for value in [
        request.owner_id.as_str(),
        request.work_id.as_str(),
        request.branch_id.as_str(),
        request.request_id.as_str(),
    ] {
        hasher.update(value.len().to_be_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.update(request.expected_work_revision.get().to_be_bytes());
    hasher.update(request.expected_branch_revision.get().to_be_bytes());
    format!("{:x}", hasher.finalize())
}

fn digest(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

fn decode_admission(
    row: &sqlx::mysql::MySqlRow,
) -> Result<WorkBranchDeletionAdmission, WorkBranchDeletionError> {
    let string = |column: &'static str| {
        row.try_get::<String, _>(column)
            .map_err(|source| database_error("decode branch deletion operation", source))
    };
    let work_id = WorkId::parse(string("work_id")?)
        .map_err(|error| WorkBranchDeletionError::NeedsRepair(error.to_string()))?;
    let branch_id = WorkBranchId::parse(string("branch_id")?)
        .map_err(|error| WorkBranchDeletionError::NeedsRepair(error.to_string()))?;
    let session_id = InternalSessionId::parse(string("session_id")?)
        .map_err(|error| WorkBranchDeletionError::NeedsRepair(error.to_string()))?;
    let work_revision = WorkRevision::new(
        row.try_get("observed_work_revision")
            .map_err(|source| database_error("decode branch deletion Work revision", source))?,
    )
    .map_err(|error| WorkBranchDeletionError::NeedsRepair(error.to_string()))?;
    let branch_revision = WorkBranchRevision::new(
        row.try_get("observed_branch_revision")
            .map_err(|source| database_error("decode branch deletion branch revision", source))?,
    )
    .map_err(|error| WorkBranchDeletionError::NeedsRepair(error.to_string()))?;
    Ok(WorkBranchDeletionAdmission {
        operation: WorkBranchDeletionOperation {
            schema_version: WORK_BRANCH_DELETION_OPERATION_SCHEMA_VERSION,
            operation_id: string("operation_id")?,
            work_id,
            branch_id,
            state: parse_state(&string("operation_state")?)?,
            phase: parse_phase(&string("operation_phase")?)?,
            outcome: parse_outcome(&string("operation_outcome")?)?,
            work_revision,
            branch_revision,
            created_at: row
                .try_get("created_at")
                .map_err(|source| database_error("decode branch deletion created time", source))?,
            completed_at: row.try_get("completed_at").map_err(|source| {
                database_error("decode branch deletion completion time", source)
            })?,
        },
        session_id,
    })
}

fn state_name(value: WorkBranchDeletionState) -> &'static str {
    match value {
        WorkBranchDeletionState::Pending => "pending",
        WorkBranchDeletionState::Succeeded => "succeeded",
        WorkBranchDeletionState::Conflict => "conflict",
    }
}

fn phase_name(value: WorkBranchDeletionPhase) -> &'static str {
    match value {
        WorkBranchDeletionPhase::Fence => "fence",
        WorkBranchDeletionPhase::SessionCleanup => "session_cleanup",
        WorkBranchDeletionPhase::LineageGc => "lineage_gc",
        WorkBranchDeletionPhase::BranchCleanup => "branch_cleanup",
        WorkBranchDeletionPhase::Complete => "complete",
    }
}

fn outcome_name(value: WorkBranchDeletionOutcome) -> &'static str {
    match value {
        WorkBranchDeletionOutcome::Pending => "pending",
        WorkBranchDeletionOutcome::Deleted => "deleted",
        WorkBranchDeletionOutcome::DeliveryBranchProtected => "delivery_branch_protected",
        WorkBranchDeletionOutcome::WorkRevisionConflict => "work_revision_conflict",
        WorkBranchDeletionOutcome::BranchRevisionConflict => "branch_revision_conflict",
    }
}

fn parse_state(value: &str) -> Result<WorkBranchDeletionState, WorkBranchDeletionError> {
    match value {
        "pending" => Ok(WorkBranchDeletionState::Pending),
        "succeeded" => Ok(WorkBranchDeletionState::Succeeded),
        "conflict" => Ok(WorkBranchDeletionState::Conflict),
        other => Err(WorkBranchDeletionError::NeedsRepair(format!(
            "unknown operation state {other}"
        ))),
    }
}

fn parse_phase(value: &str) -> Result<WorkBranchDeletionPhase, WorkBranchDeletionError> {
    match value {
        "fence" => Ok(WorkBranchDeletionPhase::Fence),
        "session_cleanup" => Ok(WorkBranchDeletionPhase::SessionCleanup),
        "lineage_gc" => Ok(WorkBranchDeletionPhase::LineageGc),
        "branch_cleanup" => Ok(WorkBranchDeletionPhase::BranchCleanup),
        "complete" => Ok(WorkBranchDeletionPhase::Complete),
        other => Err(WorkBranchDeletionError::NeedsRepair(format!(
            "unknown operation phase {other}"
        ))),
    }
}

fn parse_outcome(value: &str) -> Result<WorkBranchDeletionOutcome, WorkBranchDeletionError> {
    match value {
        "pending" => Ok(WorkBranchDeletionOutcome::Pending),
        "deleted" => Ok(WorkBranchDeletionOutcome::Deleted),
        "delivery_branch_protected" => Ok(WorkBranchDeletionOutcome::DeliveryBranchProtected),
        "work_revision_conflict" => Ok(WorkBranchDeletionOutcome::WorkRevisionConflict),
        "branch_revision_conflict" => Ok(WorkBranchDeletionOutcome::BranchRevisionConflict),
        other => Err(WorkBranchDeletionError::NeedsRepair(format!(
            "unknown operation outcome {other}"
        ))),
    }
}

fn database_error(operation: &'static str, source: sqlx::Error) -> WorkBranchDeletionError {
    WorkBranchDeletionError::Database { operation, source }
}
