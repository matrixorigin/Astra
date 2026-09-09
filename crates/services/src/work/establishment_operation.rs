//! Durable operation state for runtime-owned Work establishment.
//!
//! Work genesis, initial graph planning, and the first assignment are
//! user-visible phases of one operation, but the repository currently commits
//! them through more than one transaction.  This table is the recovery
//! authority between those transactions.  It is deliberately separate from a
//! provider invocation: one operation may have several delivery attempts,
//! while each invocation remains one-shot in the invocation ledger.

use super::{InternalSessionId, WorkBranchId, WorkId, WorkOwnerId};
use astra_core::SharedPool;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{MySql, Row, Transaction};
use thiserror::Error;

pub const WORK_ESTABLISHMENT_OPERATION_SCHEMA_VERSION: u16 = 2;
const OPERATION_ID_MAX_BYTES: usize = 256;
const RUN_ID_MAX_BYTES: usize = 256;
const TURN_CHAIN_ID_MAX_BYTES: usize = 256;
const REQUEST_HASH_MAX_BYTES: usize = 71;
const PAYLOAD_MAX_BYTES: usize = 1024 * 1024;
const ERROR_MAX_BYTES: usize = 2048;

pub(crate) const WORK_ESTABLISHMENT_OPERATIONS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_establishment_operations (
    owner_id VARCHAR(128) NOT NULL,
    operation_id VARCHAR(256) NOT NULL,
    request_hash CHAR(71) NOT NULL,
    payload_json LONGTEXT NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    branch_id VARCHAR(64) NOT NULL,
    session_id VARCHAR(64) NOT NULL,
    run_id VARCHAR(256) NOT NULL,
    activation VARCHAR(16) NOT NULL,
    operation_state VARCHAR(16) NOT NULL,
    operation_phase VARCHAR(32) NOT NULL,
    last_error VARCHAR(2048) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, operation_id),
    INDEX idx_work_establishment_recovery
      (operation_state, operation_phase, updated_at, owner_id),
    INDEX idx_work_establishment_session_recovery
      (owner_id, session_id, operation_state, updated_at),
    CONSTRAINT chk_work_establishment_state CHECK
      (operation_state IN ('pending', 'complete', 'aborted', 'failed', 'cancelled')),
    CONSTRAINT chk_work_establishment_phase CHECK
      (operation_phase IN ('awaiting_genesis', 'awaiting_plan',
                           'awaiting_assignment', 'complete')),
    CONSTRAINT chk_work_establishment_terminal CHECK (
      (operation_state = 'pending' AND operation_phase <> 'complete')
      OR (operation_state <> 'pending' AND operation_phase = 'complete')
    ),
    CONSTRAINT chk_work_establishment_activation CHECK
      (activation IN ('start', 'defer'))
)";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkEstablishmentActivation {
    Start,
    Defer,
}

impl WorkEstablishmentActivation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Defer => "defer",
        }
    }

    fn parse(value: &str) -> Result<Self, WorkEstablishmentError> {
        match value {
            "start" => Ok(Self::Start),
            "defer" => Ok(Self::Defer),
            _ => Err(WorkEstablishmentError::NeedsRepair(format!(
                "invalid persisted Work establishment activation '{value}'"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkEstablishmentState {
    Pending,
    Complete,
    Aborted,
    Failed,
    Cancelled,
}

impl WorkEstablishmentState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Complete => "complete",
            Self::Aborted => "aborted",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Result<Self, WorkEstablishmentError> {
        match value {
            "pending" => Ok(Self::Pending),
            "complete" => Ok(Self::Complete),
            "aborted" => Ok(Self::Aborted),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(WorkEstablishmentError::NeedsRepair(format!(
                "invalid persisted Work establishment state '{value}'"
            ))),
        }
    }

    fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkEstablishmentPhase {
    AwaitingGenesis,
    AwaitingPlan,
    AwaitingAssignment,
    Complete,
}

impl WorkEstablishmentPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingGenesis => "awaiting_genesis",
            Self::AwaitingPlan => "awaiting_plan",
            Self::AwaitingAssignment => "awaiting_assignment",
            Self::Complete => "complete",
        }
    }

    fn parse(value: &str) -> Result<Self, WorkEstablishmentError> {
        match value {
            "awaiting_genesis" => Ok(Self::AwaitingGenesis),
            "awaiting_plan" => Ok(Self::AwaitingPlan),
            "awaiting_assignment" => Ok(Self::AwaitingAssignment),
            "complete" => Ok(Self::Complete),
            _ => Err(WorkEstablishmentError::NeedsRepair(format!(
                "invalid persisted Work establishment phase '{value}'"
            ))),
        }
    }

    fn successor(self) -> Option<Self> {
        match self {
            Self::AwaitingGenesis => Some(Self::AwaitingPlan),
            Self::AwaitingPlan => Some(Self::AwaitingAssignment),
            Self::AwaitingAssignment => Some(Self::Complete),
            Self::Complete => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkEstablishmentRequest {
    pub operation_id: String,
    pub request_hash: String,
    pub payload_json: String,
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub session_id: InternalSessionId,
    /// Current delivery run for audit/diagnostics. It is deliberately not
    /// part of the operation idempotency comparison: a later run must be able
    /// to resume an operation admitted by an earlier crashed run.
    pub run_id: String,
    pub activation: WorkEstablishmentActivation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkEstablishmentOperation {
    pub schema_version: u16,
    pub operation_id: String,
    pub request_hash: String,
    pub payload_json: String,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub session_id: InternalSessionId,
    pub run_id: String,
    pub activation: WorkEstablishmentActivation,
    pub state: WorkEstablishmentState,
    pub phase: WorkEstablishmentPhase,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl WorkEstablishmentOperation {
    pub fn is_complete(&self) -> bool {
        self.state == WorkEstablishmentState::Complete
            && self.phase == WorkEstablishmentPhase::Complete
    }

    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    pub fn is_aborted(&self) -> bool {
        matches!(
            self.state,
            WorkEstablishmentState::Aborted
                | WorkEstablishmentState::Failed
                | WorkEstablishmentState::Cancelled
        )
    }
}

/// Durable result of an admission attempt.  The operation row is idempotent,
/// but the delivery boundary must still know whether this process created the
/// pending operation or is observing one left by an earlier invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkEstablishmentAdmissionDisposition {
    Created,
    Existing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkEstablishmentAdmission {
    pub operation: WorkEstablishmentOperation,
    pub disposition: WorkEstablishmentAdmissionDisposition,
}

#[derive(Debug, Error)]
pub enum WorkEstablishmentError {
    #[error("invalid Work establishment operation: {0}")]
    Invalid(String),
    #[error("Work establishment request identity was reused with different inputs")]
    IdempotencyMismatch,
    #[error("Work establishment operation was not found")]
    NotFound,
    #[error("another pending Work establishment already owns this session")]
    PendingSessionConflict,
    #[error("Work establishment operation requires repair: {0}")]
    NeedsRepair(String),
    #[error("Work establishment database step {operation} failed: {source}")]
    Database {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
}

#[derive(Clone)]
pub struct DatabaseWorkEstablishmentService {
    pool: SharedPool,
}

impl DatabaseWorkEstablishmentService {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    /// Admit one canonical Work-establishment operation. Repeating the same
    /// operation id returns the original immutable payload; a changed hash is
    /// rejected instead of creating a second Work identity.
    pub async fn admit(
        &self,
        request: &WorkEstablishmentRequest,
    ) -> Result<WorkEstablishmentOperation, WorkEstablishmentError> {
        Ok(self.admit_with_disposition(request).await?.operation)
    }

    /// Admit one operation and retain the created-versus-existing fact from
    /// the same transaction that enforces the one-pending-operation session
    /// invariant.  Callers use `Created` for the first canonical carrier and
    /// `Existing` to resume a durable operation after a restart/retry.
    pub async fn admit_with_disposition(
        &self,
        request: &WorkEstablishmentRequest,
    ) -> Result<WorkEstablishmentAdmission, WorkEstablishmentError> {
        validate_request(request)?;
        let mut tx = self.begin("begin Work establishment admission").await?;

        // MatrixOne uses optimistic/SI semantics where a `SELECT ... FOR
        // UPDATE` over an empty predicate does not serialize two inserts.
        // The canonical agent session is the stable row for this lifecycle;
        // take a write barrier on it before inspecting or inserting the
        // operation so concurrent admissions for one session share one
        // authority boundary.
        lock_session_admission_fence(&mut tx, &request.owner_id, &request.session_id).await?;

        // A prior row with the same operation id is a replay, regardless of
        // whether it is still pending or already complete.  Its immutable
        // request payload remains the authority for idempotency validation,
        // even when another operation is currently pending on this session.
        // Operation identity wins over the session-level single-pending
        // guard: recovery must be able to replay a completed operation while
        // a later operation is waiting for the same session.
        match load_operation_locked(&mut tx, &request.owner_id, &request.operation_id).await {
            Ok(operation) => {
                validate_same_request(&operation, request)?;
                tx.commit().await.map_err(|source| {
                    database_error("commit existing Work establishment admission", source)
                })?;
                return Ok(WorkEstablishmentAdmission {
                    operation,
                    disposition: WorkEstablishmentAdmissionDisposition::Existing,
                });
            }
            Err(WorkEstablishmentError::NotFound) => {}
            Err(error) => return Err(error),
        }

        // Only a genuinely new operation is subject to the session-level
        // pending invariant. Without this check an idempotent operation
        // upsert could hide an older pending operation (or allow a second
        // turn-chain to create one), and the caller could not safely choose a
        // fresh carrier versus recovery.
        let pending =
            load_pending_for_session_locked(&mut tx, &request.owner_id, &request.session_id)
                .await?;
        if pending
            .iter()
            .any(|operation| operation.operation_id != request.operation_id)
        {
            return Err(WorkEstablishmentError::PendingSessionConflict);
        }

        sqlx::query(
            "INSERT INTO work_establishment_operations
             (owner_id, operation_id, request_hash, payload_json, work_id,
              branch_id, session_id, run_id, activation, operation_state,
              operation_phase, last_error)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', 'awaiting_genesis', NULL)
             -- MatrixOne rejects an upsert clause that writes a primary-key
             -- column, even when the value is unchanged.  Keep admission
             -- idempotent with a non-key no-op; the locked read below remains
             -- the authority for the existing immutable request.
             ON DUPLICATE KEY UPDATE updated_at = updated_at",
        )
        .bind(request.owner_id.as_str())
        .bind(&request.operation_id)
        .bind(&request.request_hash)
        .bind(&request.payload_json)
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .bind(request.session_id.as_str())
        .bind(&request.run_id)
        .bind(request.activation.as_str())
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("admit Work establishment operation", source))?;

        let operation =
            load_operation_locked(&mut tx, &request.owner_id, &request.operation_id).await?;
        validate_same_request(&operation, request)?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit Work establishment admission", source))?;
        Ok(WorkEstablishmentAdmission {
            operation,
            disposition: WorkEstablishmentAdmissionDisposition::Created,
        })
    }

    /// Load the unfinished establishment operations for one canonical
    /// session. A session should have at most one pending establishment
    /// operation; callers enforce that invariant before hydrating a carrier.
    pub async fn load_pending_for_session(
        &self,
        owner_id: &WorkOwnerId,
        session_id: &InternalSessionId,
    ) -> Result<Vec<WorkEstablishmentOperation>, WorkEstablishmentError> {
        let rows = sqlx::query(
            "SELECT operation_id, request_hash, payload_json, work_id, branch_id,
                    session_id, run_id, activation, operation_state, operation_phase, last_error,
                    created_at, updated_at
             FROM work_establishment_operations
             WHERE owner_id = ? AND session_id = ? AND operation_state = 'pending'
             ORDER BY updated_at ASC, operation_id ASC",
        )
        .bind(owner_id.as_str())
        .bind(session_id.as_str())
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| database_error("load pending Work establishment operations", source))?;
        rows.iter().map(decode_operation).collect()
    }

    /// Load one exact operation identity. Recovery callers use the persisted
    /// immutable request instead of rebuilding it from a later physical tool
    /// invocation or another semantic classification.
    pub async fn load(
        &self,
        owner_id: &WorkOwnerId,
        operation_id: &str,
    ) -> Result<WorkEstablishmentOperation, WorkEstablishmentError> {
        validate_bounded("operation id", operation_id, OPERATION_ID_MAX_BYTES)?;
        let row = sqlx::query(
            "SELECT operation_id, request_hash, payload_json, work_id, branch_id, session_id,
                    run_id, activation, operation_state, operation_phase, last_error, created_at,
                    updated_at
             FROM work_establishment_operations
             WHERE owner_id = ? AND operation_id = ?",
        )
        .bind(owner_id.as_str())
        .bind(operation_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load Work establishment operation", source))?
        .ok_or(WorkEstablishmentError::NotFound)?;
        decode_operation(&row)
    }

    /// Advance the durable phase monotonically. The operation remains pending
    /// on a transient handler/database failure, so a later invocation can
    /// resume the exact phase without replaying an already terminal call.
    pub async fn advance_phase(
        &self,
        request: &WorkEstablishmentRequest,
        expected_phase: WorkEstablishmentPhase,
        next_phase: WorkEstablishmentPhase,
    ) -> Result<WorkEstablishmentOperation, WorkEstablishmentError> {
        validate_request(request)?;
        if next_phase < expected_phase {
            return Err(WorkEstablishmentError::Invalid(
                "Work establishment phase cannot move backwards".to_string(),
            ));
        }
        if next_phase != expected_phase && Some(next_phase) != expected_phase.successor() {
            return Err(WorkEstablishmentError::Invalid(
                "Work establishment phase can advance only one step".to_string(),
            ));
        }
        let mut tx = self.begin("begin Work establishment phase advance").await?;
        let operation =
            load_operation_locked(&mut tx, &request.owner_id, &request.operation_id).await?;
        validate_same_request(&operation, request)?;
        if operation.phase < expected_phase {
            return Err(WorkEstablishmentError::NeedsRepair(format!(
                "durable Work establishment phase is behind requested {:?}",
                expected_phase
            )));
        }
        if operation.state != WorkEstablishmentState::Pending {
            if operation.is_complete() {
                tx.commit().await.map_err(|source| {
                    database_error("commit completed Work establishment phase", source)
                })?;
                return Ok(operation);
            }
            return Err(WorkEstablishmentError::NeedsRepair(format!(
                "Work establishment is already terminal in state {:?}",
                operation.state
            )));
        }
        if operation.phase > next_phase {
            if operation.phase == WorkEstablishmentPhase::Complete {
                tx.commit().await.map_err(|source| {
                    database_error("commit completed Work establishment phase", source)
                })?;
                return Ok(operation);
            }
            return Err(WorkEstablishmentError::NeedsRepair(format!(
                "durable Work establishment phase is ahead of requested {:?}",
                next_phase
            )));
        }
        if operation.phase < next_phase {
            sqlx::query(
                "UPDATE work_establishment_operations
                 SET operation_phase = ?,
                     operation_state = IF(? = 'complete', 'complete', 'pending'),
                     last_error = NULL,
                     updated_at = NOW(6)
                 WHERE owner_id = ? AND operation_id = ?
                   AND request_hash = ? AND operation_state = 'pending'",
            )
            .bind(next_phase.as_str())
            .bind(next_phase.as_str())
            .bind(request.owner_id.as_str())
            .bind(&request.operation_id)
            .bind(&request.request_hash)
            .execute(&mut *tx)
            .await
            .map_err(|source| database_error("advance Work establishment phase", source))?;
        }
        let updated =
            load_operation_locked(&mut tx, &request.owner_id, &request.operation_id).await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit Work establishment phase", source))?;
        Ok(updated)
    }

    pub async fn record_error(
        &self,
        request: &WorkEstablishmentRequest,
        error: &str,
    ) -> Result<(), WorkEstablishmentError> {
        validate_request(request)?;
        let error = truncate_error(error);
        sqlx::query(
            "UPDATE work_establishment_operations
             SET last_error = ?, updated_at = NOW(6)
             WHERE owner_id = ? AND operation_id = ? AND request_hash = ?
               AND operation_state = 'pending'",
        )
        .bind(error)
        .bind(request.owner_id.as_str())
        .bind(&request.operation_id)
        .bind(&request.request_hash)
        .execute(self.pool.get())
        .await
        .map_err(|source| database_error("record Work establishment error", source))?;
        Ok(())
    }

    /// Close a pending establishment without claiming that the Work graph was
    /// successfully created.  This is the durable escape hatch for a user
    /// replacement or an explicit run cancellation; it is a CAS over the
    /// operation identity and therefore cannot resurrect a later operation.
    pub async fn abort(
        &self,
        request: &WorkEstablishmentRequest,
        error: &str,
    ) -> Result<WorkEstablishmentOperation, WorkEstablishmentError> {
        self.transition_terminal(request, WorkEstablishmentState::Aborted, error)
            .await
    }

    pub async fn fail(
        &self,
        request: &WorkEstablishmentRequest,
        error: &str,
    ) -> Result<WorkEstablishmentOperation, WorkEstablishmentError> {
        self.transition_terminal(request, WorkEstablishmentState::Failed, error)
            .await
    }

    pub async fn cancel(
        &self,
        request: &WorkEstablishmentRequest,
        error: &str,
    ) -> Result<WorkEstablishmentOperation, WorkEstablishmentError> {
        self.transition_terminal(request, WorkEstablishmentState::Cancelled, error)
            .await
    }

    /// Cancel pending operations whose canonical turn-chain differs from the
    /// current user turn.  The run/session owner serializes this operation;
    /// a new turn therefore supersedes an abandoned carrier instead of being
    /// permanently blocked by it.  Same-turn recovery remains untouched.
    pub async fn cancel_pending_for_new_turn(
        &self,
        owner_id: &WorkOwnerId,
        session_id: &InternalSessionId,
        current_turn_chain_id: &str,
        reason: &str,
    ) -> Result<bool, WorkEstablishmentError> {
        validate_turn_chain_id(current_turn_chain_id)?;
        let mut tx = self
            .begin("begin Work establishment turn replacement")
            .await?;
        let cancelled = cancel_pending_for_new_turn_tx(
            &mut tx,
            owner_id,
            session_id,
            current_turn_chain_id,
            reason,
        )
        .await?;
        tx.commit().await.map_err(|source| {
            database_error("commit Work establishment turn replacement", source)
        })?;
        Ok(cancelled)
    }

    async fn transition_terminal(
        &self,
        request: &WorkEstablishmentRequest,
        state: WorkEstablishmentState,
        error: &str,
    ) -> Result<WorkEstablishmentOperation, WorkEstablishmentError> {
        validate_request(request)?;
        if !state.is_terminal() || state == WorkEstablishmentState::Pending {
            return Err(WorkEstablishmentError::Invalid(
                "terminal Work establishment state is required".to_string(),
            ));
        }
        let mut tx = self
            .begin("begin terminal Work establishment transition")
            .await?;
        let operation =
            load_operation_locked(&mut tx, &request.owner_id, &request.operation_id).await?;
        validate_same_request(&operation, request)?;
        if operation.state != WorkEstablishmentState::Pending {
            tx.commit().await.map_err(|source| {
                database_error("commit existing terminal Work establishment", source)
            })?;
            return Ok(operation);
        }
        sqlx::query(
            "UPDATE work_establishment_operations
             SET operation_state = ?, operation_phase = 'complete',
                 last_error = ?, updated_at = NOW(6)
             WHERE owner_id = ? AND operation_id = ?
               AND request_hash = ? AND operation_state = 'pending'",
        )
        .bind(state.as_str())
        .bind(truncate_error(error))
        .bind(request.owner_id.as_str())
        .bind(&request.operation_id)
        .bind(&request.request_hash)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("terminalize Work establishment", source))?;
        let updated =
            load_operation_locked(&mut tx, &request.owner_id, &request.operation_id).await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit terminal Work establishment", source))?;
        Ok(updated)
    }

    async fn begin(
        &self,
        operation: &'static str,
    ) -> Result<Transaction<'_, MySql>, WorkEstablishmentError> {
        self.pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error(operation, source))
    }
}

/// Apply the Work-carrier half of a durable user-intent transition inside the
/// caller's transaction. This is crate-visible so the canonical run-intent
/// store can commit `user_intent_applied` and supersession atomically.
pub(crate) async fn cancel_pending_for_new_turn_tx(
    tx: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    session_id: &InternalSessionId,
    current_turn_chain_id: &str,
    reason: &str,
) -> Result<bool, WorkEstablishmentError> {
    validate_turn_chain_id(current_turn_chain_id)?;
    lock_session_admission_fence(tx, owner_id, session_id).await?;
    let pending = load_pending_for_session_locked(tx, owner_id, session_id).await?;
    let mut cancelled = false;
    let reason = truncate_error(reason);
    for operation in pending {
        let persisted_turn_chain = canonical_payload_turn_chain_id(&operation.payload_json)?;
        if persisted_turn_chain == current_turn_chain_id {
            continue;
        }
        let updated = sqlx::query(
            "UPDATE work_establishment_operations
             SET operation_state = 'cancelled', operation_phase = 'complete',
                 last_error = ?, updated_at = NOW(6)
             WHERE owner_id = ? AND operation_id = ?
               AND request_hash = ? AND operation_state = 'pending'",
        )
        .bind(&reason)
        .bind(owner_id.as_str())
        .bind(&operation.operation_id)
        .bind(&operation.request_hash)
        .execute(&mut **tx)
        .await
        .map_err(|source| database_error("cancel superseded Work establishment", source))?;
        if updated.rows_affected() != 1 {
            return Err(WorkEstablishmentError::NeedsRepair(
                "pending Work establishment changed during durable supersession".to_string(),
            ));
        }
        cancelled = true;
    }
    Ok(cancelled)
}

fn validate_turn_chain_id(value: &str) -> Result<(), WorkEstablishmentError> {
    validate_bounded("turn-chain id", value, TURN_CHAIN_ID_MAX_BYTES)?;
    if value.trim().is_empty() {
        return Err(WorkEstablishmentError::Invalid(
            "current turn-chain identity is required to supersede pending Work establishment"
                .to_string(),
        ));
    }
    Ok(())
}

fn canonical_payload_turn_chain_id(payload_json: &str) -> Result<String, WorkEstablishmentError> {
    let payload: serde_json::Value = serde_json::from_str(payload_json).map_err(|error| {
        WorkEstablishmentError::NeedsRepair(format!(
            "invalid canonical Work establishment payload: {error}"
        ))
    })?;
    if payload
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        != Some(2)
    {
        return Err(WorkEstablishmentError::NeedsRepair(
            "unsupported canonical Work establishment payload schema".to_string(),
        ));
    }
    payload
        .get("turn_chain_id")
        .and_then(serde_json::Value::as_str)
        .filter(|turn_chain_id| !turn_chain_id.trim().is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| {
            WorkEstablishmentError::NeedsRepair(
                "canonical Work establishment payload has no turn-chain identity".to_string(),
            )
        })
}

async fn load_pending_for_session_locked(
    tx: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    session_id: &InternalSessionId,
) -> Result<Vec<WorkEstablishmentOperation>, WorkEstablishmentError> {
    let rows = sqlx::query(
        "SELECT operation_id, request_hash, payload_json, work_id, branch_id,
                session_id, run_id, activation, operation_state, operation_phase, last_error,
                created_at, updated_at
         FROM work_establishment_operations
         WHERE owner_id = ? AND session_id = ? AND operation_state = 'pending'
         ORDER BY updated_at ASC, operation_id ASC
         FOR UPDATE",
    )
    .bind(owner_id.as_str())
    .bind(session_id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(|source| database_error("lock pending Work establishment operations", source))?;
    rows.iter().map(decode_operation).collect()
}

async fn lock_session_admission_fence(
    tx: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    session_id: &InternalSessionId,
) -> Result<(), WorkEstablishmentError> {
    let exists = sqlx::query(
        "SELECT session_id
         FROM agent_sessions
         WHERE user_id = ? AND session_id = ?
         FOR UPDATE",
    )
    .bind(owner_id.as_str())
    .bind(session_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("lock Work session admission fence", source))?;
    if exists.is_none() {
        return Err(WorkEstablishmentError::NeedsRepair(
            "canonical agent session is missing for Work establishment".to_string(),
        ));
    }

    // Keep the no-op write in the transaction.  This is intentional: on
    // MatrixOne it establishes the write/write barrier that a read lock on a
    // stable row alone does not provide under optimistic isolation.
    sqlx::query(
        "UPDATE agent_sessions
         SET updated_at = updated_at
         WHERE user_id = ? AND session_id = ?",
    )
    .bind(owner_id.as_str())
    .bind(session_id.as_str())
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("write Work session admission fence", source))?;
    Ok(())
}

async fn load_operation_locked(
    tx: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    operation_id: &str,
) -> Result<WorkEstablishmentOperation, WorkEstablishmentError> {
    let row = sqlx::query(
        "SELECT operation_id, request_hash, payload_json, work_id, branch_id, session_id,
                run_id, activation, operation_state, operation_phase, last_error, created_at,
                updated_at
         FROM work_establishment_operations
         WHERE owner_id = ? AND operation_id = ? FOR UPDATE",
    )
    .bind(owner_id.as_str())
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("load Work establishment operation", source))?
    .ok_or(WorkEstablishmentError::NotFound)?;
    decode_operation(&row)
}

fn decode_operation(
    row: &sqlx::mysql::MySqlRow,
) -> Result<WorkEstablishmentOperation, WorkEstablishmentError> {
    let string = |column: &'static str| {
        row.try_get::<String, _>(column)
            .map_err(|source| database_error("decode Work establishment operation", source))
    };
    let optional_string = |column: &'static str| {
        row.try_get::<Option<String>, _>(column)
            .map_err(|source| database_error("decode Work establishment operation", source))
    };
    Ok(WorkEstablishmentOperation {
        schema_version: WORK_ESTABLISHMENT_OPERATION_SCHEMA_VERSION,
        operation_id: string("operation_id")?,
        request_hash: string("request_hash")?,
        payload_json: string("payload_json")?,
        work_id: WorkId::parse(string("work_id")?)
            .map_err(|error| WorkEstablishmentError::NeedsRepair(error.to_string()))?,
        branch_id: WorkBranchId::parse(string("branch_id")?)
            .map_err(|error| WorkEstablishmentError::NeedsRepair(error.to_string()))?,
        session_id: InternalSessionId::parse(string("session_id")?)
            .map_err(|error| WorkEstablishmentError::NeedsRepair(error.to_string()))?,
        run_id: string("run_id")?,
        activation: WorkEstablishmentActivation::parse(&string("activation")?)?,
        state: WorkEstablishmentState::parse(&string("operation_state")?)?,
        phase: WorkEstablishmentPhase::parse(&string("operation_phase")?)?,
        last_error: optional_string("last_error")?,
        created_at: row
            .try_get("created_at")
            .map_err(|source| database_error("decode Work establishment time", source))?,
        updated_at: row
            .try_get("updated_at")
            .map_err(|source| database_error("decode Work establishment time", source))?,
    })
}

fn validate_same_request(
    operation: &WorkEstablishmentOperation,
    request: &WorkEstablishmentRequest,
) -> Result<(), WorkEstablishmentError> {
    if operation.request_hash != request.request_hash
        || operation.payload_json != request.payload_json
        || operation.work_id != request.work_id
        || operation.branch_id != request.branch_id
        || operation.session_id != request.session_id
        || operation.activation != request.activation
    {
        return Err(WorkEstablishmentError::IdempotencyMismatch);
    }
    Ok(())
}

fn validate_request(request: &WorkEstablishmentRequest) -> Result<(), WorkEstablishmentError> {
    validate_bounded(
        "operation id",
        &request.operation_id,
        OPERATION_ID_MAX_BYTES,
    )?;
    validate_bounded("run id", &request.run_id, RUN_ID_MAX_BYTES)?;
    validate_bounded(
        "request hash",
        &request.request_hash,
        REQUEST_HASH_MAX_BYTES,
    )?;
    validate_bounded("payload", &request.payload_json, PAYLOAD_MAX_BYTES)?;
    if request.operation_id.trim().is_empty()
        || request.request_hash.trim().is_empty()
        || request.payload_json.trim().is_empty()
    {
        return Err(WorkEstablishmentError::Invalid(
            "operation identity and canonical payload are required".to_string(),
        ));
    }
    if request.run_id.trim().is_empty() {
        return Err(WorkEstablishmentError::Invalid(
            "run identity is required".to_string(),
        ));
    }
    Ok(())
}

fn validate_bounded(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), WorkEstablishmentError> {
    if value.len() > max_bytes {
        return Err(WorkEstablishmentError::Invalid(format!(
            "{field} exceeds {max_bytes} bytes"
        )));
    }
    Ok(())
}

fn truncate_error(value: &str) -> String {
    if value.len() <= ERROR_MAX_BYTES {
        return value.to_string();
    }
    let mut end = ERROR_MAX_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn database_error(operation: &'static str, source: sqlx::Error) -> WorkEstablishmentError {
    WorkEstablishmentError::Database { operation, source }
}

#[cfg(test)]
mod tests {
    use super::WorkEstablishmentState;

    #[test]
    fn establishment_state_is_explicitly_terminal_or_pending() {
        assert!(!WorkEstablishmentState::Pending.is_terminal());
        for state in [
            WorkEstablishmentState::Complete,
            WorkEstablishmentState::Aborted,
            WorkEstablishmentState::Failed,
            WorkEstablishmentState::Cancelled,
        ] {
            assert!(state.is_terminal());
        }
    }

    #[test]
    fn establishment_state_wire_names_round_trip() {
        for (state, wire) in [
            (WorkEstablishmentState::Pending, "pending"),
            (WorkEstablishmentState::Complete, "complete"),
            (WorkEstablishmentState::Aborted, "aborted"),
            (WorkEstablishmentState::Failed, "failed"),
            (WorkEstablishmentState::Cancelled, "cancelled"),
        ] {
            assert_eq!(state.as_str(), wire);
            assert_eq!(
                WorkEstablishmentState::parse(wire).expect("valid state"),
                state
            );
        }
    }
}
