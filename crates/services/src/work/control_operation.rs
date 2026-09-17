use super::{WorkBranchId, WorkBranchRevision, WorkId, WorkOwnerId};
use crate::session_handoff::{
    IdleControllerMutationOutcomeV1, IdleControllerMutationV1, SessionControllerBasisV1,
    SessionHandoffError, load_controller_basis_in_transaction,
    mutate_idle_controller_in_transaction,
};
use astra_core::SharedPool;
use astra_turn_types::{DEFAULT_CONVERSATION_BRANCH_ID, SessionHandoffStateV1, SessionKeyV1};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{MySql, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

pub const WORK_BRANCH_CONTROL_OPERATION_SCHEMA_VERSION: u16 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchControlKind {
    AcquireBranchControl,
    ForceTakeover,
    ReleaseBranchControl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchControlState {
    Pending,
    Aborted,
    Succeeded,
    Conflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchControlOutcome {
    Pending,
    Aborted,
    Acquired,
    AlreadyControlled,
    TakenOver,
    Released,
    AlreadyReleased,
    WriterConflict,
    BranchRevisionConflict,
    HeadConflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchControlPhase {
    AwaitingReauthentication,
    Preparing,
    Fencing,
    SealingEffects,
    Activating,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkBranchControlProgress {
    pub phase: WorkBranchControlPhase,
    pub abortable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkBranchControlRequest {
    pub request_id: String,
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub attachment_id: String,
    pub expected_branch_revision: WorkBranchRevision,
    pub expected_basis: SessionControllerBasisV1,
    pub kind: WorkBranchControlKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkBranchControlOperation {
    pub schema_version: u16,
    pub operation_id: String,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub attachment_id: String,
    pub kind: WorkBranchControlKind,
    pub state: WorkBranchControlState,
    pub outcome: WorkBranchControlOutcome,
    pub branch_revision: WorkBranchRevision,
    pub control_basis: Option<SessionControllerBasisV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<WorkBranchControlProgress>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl WorkBranchControlOperation {
    pub fn observe_handoff_state(&mut self, state: SessionHandoffStateV1) {
        if self.state != WorkBranchControlState::Pending
            || self.kind != WorkBranchControlKind::ForceTakeover
        {
            return;
        }
        let phase = match state {
            SessionHandoffStateV1::Requested | SessionHandoffStateV1::Validating => {
                WorkBranchControlPhase::Preparing
            }
            SessionHandoffStateV1::Blocked => WorkBranchControlPhase::Preparing,
            SessionHandoffStateV1::Fencing => WorkBranchControlPhase::Fencing,
            SessionHandoffStateV1::Fenced => WorkBranchControlPhase::SealingEffects,
            SessionHandoffStateV1::Hydrating | SessionHandoffStateV1::Active => {
                WorkBranchControlPhase::Activating
            }
            SessionHandoffStateV1::Aborted => WorkBranchControlPhase::Preparing,
            _ => WorkBranchControlPhase::Preparing,
        };
        self.progress = Some(WorkBranchControlProgress {
            phase,
            abortable: force_handoff_is_abortable(state),
        });
    }
}

pub fn force_handoff_is_abortable(state: SessionHandoffStateV1) -> bool {
    matches!(
        state,
        SessionHandoffStateV1::Requested
            | SessionHandoffStateV1::Validating
            | SessionHandoffStateV1::Blocked
            | SessionHandoffStateV1::Aborted
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkBranchForceAdmission {
    pub operation: WorkBranchControlOperation,
    pub session_id: String,
    pub authorization_id: Option<String>,
    pub handoff_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkBranchForceContext {
    pub operation: WorkBranchControlOperation,
    pub session_id: String,
    pub handoff_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum WorkBranchControlError {
    #[error("Work branch was not found")]
    NotFound,
    #[error("branch control operation was not found")]
    OperationNotFound,
    #[error("control request identity was reused with different inputs")]
    IdempotencyMismatch,
    #[error("control operation state requires repair: {0}")]
    NeedsRepair(String),
    #[error(transparent)]
    Session(#[from] SessionHandoffError),
    #[error("control operation database step {operation} failed: {source}")]
    Database {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
}

#[derive(Clone)]
pub struct DatabaseWorkBranchControlService {
    pool: SharedPool,
}

impl DatabaseWorkBranchControlService {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    /// Claim the single background executor for a pending forced takeover.
    ///
    /// The lease makes HTTP retries cheap and prevents one client retry storm
    /// from running the authority transition concurrently. A crashed worker
    /// becomes reclaimable without an in-memory coordinator.
    pub async fn claim_force_executor(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        operation_id: &str,
        lease_seconds: u32,
    ) -> Result<Option<String>, WorkBranchControlError> {
        if lease_seconds == 0 || lease_seconds > 15 * 60 {
            return Err(WorkBranchControlError::NeedsRepair(
                "force executor lease is outside the supported bound".into(),
            ));
        }
        let token = Uuid::new_v4().to_string();
        let result = sqlx::query(
            "UPDATE work_branch_control_operations
             SET executor_token = ?,
                 executor_lease_until = DATE_ADD(NOW(6), INTERVAL ? SECOND)
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND operation_id = ? AND operation_kind = 'force_takeover'
               AND operation_state = 'pending'
               AND forced_authorization_id IS NOT NULL
               AND (executor_lease_until IS NULL OR executor_lease_until <= NOW(6))",
        )
        .bind(&token)
        .bind(lease_seconds)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| database_error("claim force executor", source))?;
        Ok((result.rows_affected() == 1).then_some(token))
    }

    pub async fn release_force_executor(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        operation_id: &str,
        token: &str,
    ) -> Result<(), WorkBranchControlError> {
        sqlx::query(
            "UPDATE work_branch_control_operations
             SET executor_token = NULL, executor_lease_until = NULL
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND operation_id = ? AND executor_token = ?
               AND operation_state = 'pending'",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .bind(token)
        .execute(self.pool.get())
        .await
        .map_err(|source| database_error("release force executor", source))?;
        Ok(())
    }

    pub async fn renew_force_executor(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        operation_id: &str,
        token: &str,
        lease_seconds: u32,
    ) -> Result<bool, WorkBranchControlError> {
        if lease_seconds == 0 || lease_seconds > 15 * 60 {
            return Err(WorkBranchControlError::NeedsRepair(
                "force executor lease is outside the supported bound".into(),
            ));
        }
        let result = sqlx::query(
            "UPDATE work_branch_control_operations
             SET executor_lease_until = DATE_ADD(NOW(6), INTERVAL ? SECOND)
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND operation_id = ? AND operation_kind = 'force_takeover'
               AND operation_state = 'pending' AND executor_token = ?",
        )
        .bind(lease_seconds)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .bind(token)
        .execute(self.pool.get())
        .await
        .map_err(|source| database_error("renew force executor", source))?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn execute(
        &self,
        request: &WorkBranchControlRequest,
    ) -> Result<WorkBranchControlOperation, WorkBranchControlError> {
        let request_hash = request_hash(request);
        let idempotency_hash = identity_hash(&request.request_id);
        let candidate_operation_id = Uuid::new_v4().to_string();
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin", source))?;
        let admitted = sqlx::query(
            "SELECT operation_id, request_hash, operation_state
             FROM work_branch_control_operations
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND idempotency_hash = ?
             FOR UPDATE",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .bind(&idempotency_hash)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("load admitted operation", source))?;
        let (operation_id, state) = if let Some(admitted) = admitted {
            if admitted
                .try_get::<String, _>("request_hash")
                .map_err(|source| database_error("decode request hash", source))?
                != request_hash
            {
                return Err(WorkBranchControlError::IdempotencyMismatch);
            }
            (
                admitted
                    .try_get("operation_id")
                    .map_err(|source| database_error("decode operation id", source))?,
                admitted
                    .try_get("operation_state")
                    .map_err(|source| database_error("decode operation state", source))?,
            )
        } else {
            sqlx::query(
                "INSERT INTO work_branch_control_operations
                 (owner_id, work_id, branch_id, operation_id, idempotency_hash,
                  request_hash, session_id, attachment_id, operation_kind,
                  operation_state, operation_outcome, expected_branch_revision,
                  expected_writer_epoch, expected_root_hash)
                 VALUES (?, ?, ?, ?, ?, ?, '', ?, ?, 'pending', 'pending', ?, ?, ?)",
            )
            .bind(request.owner_id.as_str())
            .bind(request.work_id.as_str())
            .bind(request.branch_id.as_str())
            .bind(&candidate_operation_id)
            .bind(&idempotency_hash)
            .bind(&request_hash)
            .bind(&request.attachment_id)
            .bind(kind_name(request.kind))
            .bind(request.expected_branch_revision.get())
            .bind(i64_from_u64(
                "expected writer epoch",
                request.expected_basis.writer_epoch,
            )?)
            .bind(request.expected_basis.canonical_root_hash.as_deref())
            .execute(&mut *tx)
            .await
            .map_err(|source| database_error("admit operation", source))?;
            (candidate_operation_id.clone(), "pending".to_string())
        };
        if operation_id != candidate_operation_id {
            if state == "pending" {
                return Err(WorkBranchControlError::NeedsRepair(
                    "committed control operation remained pending".into(),
                ));
            }
            let operation = load_operation_locked(
                &mut tx,
                &request.owner_id,
                &request.work_id,
                &request.branch_id,
                &operation_id,
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit retry", source))?;
            return Ok(operation);
        }

        lock_active_work(&mut tx, &request.owner_id, &request.work_id).await?;
        let branch = lock_branch(&mut tx, request).await?;
        let observed_revision = WorkBranchRevision::new(branch.branch_revision).map_err(|_| {
            WorkBranchControlError::NeedsRepair("invalid stored Work branch revision".into())
        })?;
        let (state, outcome, basis) = if observed_revision != request.expected_branch_revision {
            (
                WorkBranchControlState::Conflict,
                WorkBranchControlOutcome::BranchRevisionConflict,
                None,
            )
        } else {
            let key = SessionKeyV1::owner_session(
                "server",
                request.owner_id.as_str(),
                &branch.session_id,
                DEFAULT_CONVERSATION_BRANCH_ID,
            );
            let mutation = match request.kind {
                WorkBranchControlKind::AcquireBranchControl => IdleControllerMutationV1::Acquire,
                WorkBranchControlKind::ReleaseBranchControl => IdleControllerMutationV1::Release,
                WorkBranchControlKind::ForceTakeover => {
                    return Err(WorkBranchControlError::NeedsRepair(
                        "force takeover must use the durable handoff executor".into(),
                    ));
                }
            };
            match mutate_idle_controller_in_transaction(
                &mut tx,
                &key,
                &request.attachment_id,
                mutation,
                Some(&request.expected_basis),
            )
            .await?
            {
                IdleControllerMutationOutcomeV1::Acquired(_) => (
                    WorkBranchControlState::Succeeded,
                    WorkBranchControlOutcome::Acquired,
                    Some(request.expected_basis.clone()),
                ),
                IdleControllerMutationOutcomeV1::AlreadyControlled(_) => (
                    WorkBranchControlState::Succeeded,
                    WorkBranchControlOutcome::AlreadyControlled,
                    Some(request.expected_basis.clone()),
                ),
                IdleControllerMutationOutcomeV1::Released(_) => (
                    WorkBranchControlState::Succeeded,
                    WorkBranchControlOutcome::Released,
                    Some(request.expected_basis.clone()),
                ),
                IdleControllerMutationOutcomeV1::AlreadyReleased(_) => (
                    WorkBranchControlState::Succeeded,
                    WorkBranchControlOutcome::AlreadyReleased,
                    Some(request.expected_basis.clone()),
                ),
                IdleControllerMutationOutcomeV1::Conflict => (
                    WorkBranchControlState::Conflict,
                    WorkBranchControlOutcome::WriterConflict,
                    Some(request.expected_basis.clone()),
                ),
                IdleControllerMutationOutcomeV1::BasisConflict(observed) => (
                    WorkBranchControlState::Conflict,
                    WorkBranchControlOutcome::HeadConflict,
                    Some(observed),
                ),
            }
        };
        let result = sqlx::query(
            "UPDATE work_branch_control_operations
             SET session_id = ?, operation_state = ?, operation_outcome = ?,
                 observed_branch_revision = ?, observed_writer_epoch = ?,
                 observed_root_hash = ?, executor_token = NULL,
                 executor_lease_until = NULL, completed_at = NOW(6)
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND operation_id = ? AND operation_state = 'pending'",
        )
        .bind(&branch.session_id)
        .bind(state_name(state))
        .bind(outcome_name(outcome))
        .bind(observed_revision.get())
        .bind(
            basis
                .as_ref()
                .map(|basis| i64_from_u64("observed writer epoch", basis.writer_epoch))
                .transpose()?,
        )
        .bind(
            basis
                .as_ref()
                .and_then(|basis| basis.canonical_root_hash.as_deref()),
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .bind(&operation_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("complete operation", source))?;
        if result.rows_affected() != 1 {
            return Err(WorkBranchControlError::NeedsRepair(
                "control operation disappeared before completion".into(),
            ));
        }
        let operation = load_operation_locked(
            &mut tx,
            &request.owner_id,
            &request.work_id,
            &request.branch_id,
            &operation_id,
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit operation", source))?;
        Ok(operation)
    }

    /// Admit a force takeover without performing authority-bearing work in
    /// the catalog transaction. The returned pending operation is the crash-
    /// durable anchor used by the Server handoff executor.
    pub async fn admit_force_takeover(
        &self,
        request: &WorkBranchControlRequest,
    ) -> Result<WorkBranchForceAdmission, WorkBranchControlError> {
        if request.kind != WorkBranchControlKind::ForceTakeover {
            return Err(WorkBranchControlError::NeedsRepair(
                "force admission received a different command kind".into(),
            ));
        }
        let request_hash = request_hash(request);
        let idempotency_hash = identity_hash(&request.request_id);
        let candidate_operation_id = Uuid::new_v4().to_string();
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin force admission", source))?;
        let admitted = sqlx::query(
            "SELECT operation_id, request_hash, session_id, forced_authorization_id, handoff_id
             FROM work_branch_control_operations
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND idempotency_hash = ?
             FOR UPDATE",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .bind(&idempotency_hash)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("load force admission", source))?;
        let (operation_id, mut session_id, authorization_id, handoff_id) =
            if let Some(admitted) = admitted {
                if admitted
                    .try_get::<String, _>("request_hash")
                    .map_err(|source| database_error("decode force request hash", source))?
                    != request_hash
                {
                    return Err(WorkBranchControlError::IdempotencyMismatch);
                }
                (
                    admitted
                        .try_get("operation_id")
                        .map_err(|source| database_error("decode force operation id", source))?,
                    admitted
                        .try_get("session_id")
                        .map_err(|source| database_error("decode force session binding", source))?,
                    admitted
                        .try_get("forced_authorization_id")
                        .map_err(|source| database_error("decode force authorization", source))?,
                    admitted
                        .try_get("handoff_id")
                        .map_err(|source| database_error("decode force handoff binding", source))?,
                )
            } else {
                sqlx::query(
                    "INSERT INTO work_branch_control_operations
                     (owner_id, work_id, branch_id, operation_id, idempotency_hash,
                      request_hash, session_id, attachment_id, operation_kind,
                      operation_state, operation_outcome, expected_branch_revision,
                      expected_writer_epoch, expected_root_hash)
                     VALUES (?, ?, ?, ?, ?, ?, '', ?, 'force_takeover',
                             'pending', 'pending', ?, ?, ?)",
                )
                .bind(request.owner_id.as_str())
                .bind(request.work_id.as_str())
                .bind(request.branch_id.as_str())
                .bind(&candidate_operation_id)
                .bind(&idempotency_hash)
                .bind(&request_hash)
                .bind(&request.attachment_id)
                .bind(request.expected_branch_revision.get())
                .bind(i64_from_u64(
                    "expected writer epoch",
                    request.expected_basis.writer_epoch,
                )?)
                .bind(request.expected_basis.canonical_root_hash.as_deref())
                .execute(&mut *tx)
                .await
                .map_err(|source| database_error("admit force operation", source))?;
                (candidate_operation_id.clone(), String::new(), None, None)
            };
        if operation_id == candidate_operation_id {
            lock_active_work(&mut tx, &request.owner_id, &request.work_id).await?;
            let branch = lock_branch(&mut tx, request).await?;
            session_id = branch.session_id;
            let observed_revision =
                WorkBranchRevision::new(branch.branch_revision).map_err(|_| {
                    WorkBranchControlError::NeedsRepair(
                        "invalid stored Work branch revision".into(),
                    )
                })?;
            let key = SessionKeyV1::owner_session(
                "server",
                request.owner_id.as_str(),
                &session_id,
                DEFAULT_CONVERSATION_BRANCH_ID,
            );
            let observed_basis = load_controller_basis_in_transaction(&mut tx, &key).await?;
            let conflict = if observed_revision != request.expected_branch_revision {
                Some(WorkBranchControlOutcome::BranchRevisionConflict)
            } else if observed_basis != request.expected_basis {
                Some(WorkBranchControlOutcome::HeadConflict)
            } else {
                None
            };
            let result = if let Some(outcome) = conflict {
                sqlx::query(
                    "UPDATE work_branch_control_operations
                     SET session_id = ?, operation_state = 'conflict',
                         operation_outcome = ?, observed_branch_revision = ?,
                         observed_writer_epoch = ?, observed_root_hash = ?,
                         completed_at = NOW(6)
                     WHERE owner_id = ? AND work_id = ? AND branch_id = ?
                       AND operation_id = ? AND operation_state = 'pending'",
                )
                .bind(&session_id)
                .bind(outcome_name(outcome))
                .bind(observed_revision.get())
                .bind(i64_from_u64(
                    "observed writer epoch",
                    observed_basis.writer_epoch,
                )?)
                .bind(observed_basis.canonical_root_hash.as_deref())
                .bind(request.owner_id.as_str())
                .bind(request.work_id.as_str())
                .bind(request.branch_id.as_str())
                .bind(&operation_id)
                .execute(&mut *tx)
                .await
            } else {
                sqlx::query(
                    "UPDATE work_branch_control_operations
                     SET session_id = ?
                     WHERE owner_id = ? AND work_id = ? AND branch_id = ?
                       AND operation_id = ? AND operation_state = 'pending'",
                )
                .bind(&session_id)
                .bind(request.owner_id.as_str())
                .bind(request.work_id.as_str())
                .bind(request.branch_id.as_str())
                .bind(&operation_id)
                .execute(&mut *tx)
                .await
            }
            .map_err(|source| database_error("persist force admission", source))?;
            if result.rows_affected() != 1 {
                return Err(WorkBranchControlError::NeedsRepair(
                    "force admission disappeared before persistence".into(),
                ));
            }
        } else if session_id.is_empty() {
            return Err(WorkBranchControlError::NeedsRepair(
                "committed force operation has no session binding".into(),
            ));
        }
        let operation = load_operation_locked(
            &mut tx,
            &request.owner_id,
            &request.work_id,
            &request.branch_id,
            &operation_id,
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit force admission", source))?;
        Ok(WorkBranchForceAdmission {
            operation,
            session_id,
            authorization_id,
            handoff_id,
        })
    }

    pub async fn record_force_authorization(
        &self,
        request: &WorkBranchControlRequest,
        operation_id: &str,
        authorization_id: &str,
    ) -> Result<String, WorkBranchControlError> {
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin force authorization", source))?;
        sqlx::query(
            "UPDATE work_branch_control_operations
             SET forced_authorization_id = ?
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND operation_id = ? AND operation_kind = 'force_takeover'
               AND operation_state = 'pending' AND forced_authorization_id IS NULL",
        )
        .bind(authorization_id)
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .bind(operation_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("record force authorization", source))?;
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT forced_authorization_id
             FROM work_branch_control_operations
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND operation_id = ? AND operation_kind = 'force_takeover'
             FOR UPDATE",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .bind(operation_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("load force authorization", source))?
        .flatten();
        let stored = stored.ok_or_else(|| {
            WorkBranchControlError::NeedsRepair(
                "force operation has no durable authorization identity".into(),
            )
        })?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit force authorization", source))?;
        Ok(stored)
    }

    pub async fn complete_force_takeover(
        &self,
        request: &WorkBranchControlRequest,
        operation_id: &str,
        basis: &SessionControllerBasisV1,
    ) -> Result<WorkBranchControlOperation, WorkBranchControlError> {
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin force completion", source))?;
        lock_active_work(&mut tx, &request.owner_id, &request.work_id).await?;
        let branch = lock_branch(&mut tx, request).await?;
        let result = sqlx::query(
            "UPDATE work_branch_control_operations
             SET operation_state = 'succeeded', operation_outcome = 'taken_over',
                 observed_branch_revision = ?, observed_writer_epoch = ?,
                 observed_root_hash = ?, completed_at = NOW(6)
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND operation_id = ? AND operation_kind = 'force_takeover'
               AND operation_state = 'pending'",
        )
        .bind(branch.branch_revision)
        .bind(i64_from_u64("observed writer epoch", basis.writer_epoch)?)
        .bind(basis.canonical_root_hash.as_deref())
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .bind(operation_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("complete force operation", source))?;
        if result.rows_affected() == 0 {
            let existing = load_operation_locked(
                &mut tx,
                &request.owner_id,
                &request.work_id,
                &request.branch_id,
                operation_id,
            )
            .await?;
            if existing.state != WorkBranchControlState::Succeeded
                || existing.outcome != WorkBranchControlOutcome::TakenOver
            {
                return Err(WorkBranchControlError::NeedsRepair(
                    "force operation could not reach its terminal result".into(),
                ));
            }
            tx.commit()
                .await
                .map_err(|source| database_error("commit force completion replay", source))?;
            return Ok(existing);
        }
        let operation = load_operation_locked(
            &mut tx,
            &request.owner_id,
            &request.work_id,
            &request.branch_id,
            operation_id,
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit force completion", source))?;
        Ok(operation)
    }

    pub async fn record_force_handoff(
        &self,
        request: &WorkBranchControlRequest,
        operation_id: &str,
        handoff_id: &str,
    ) -> Result<(), WorkBranchControlError> {
        sqlx::query(
            "UPDATE work_branch_control_operations
             SET handoff_id = ?
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND operation_id = ? AND operation_kind = 'force_takeover'
               AND operation_state = 'pending'
               AND (handoff_id IS NULL OR handoff_id = ?)",
        )
        .bind(handoff_id)
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .bind(operation_id)
        .bind(handoff_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| database_error("record force handoff", source))?;
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT handoff_id FROM work_branch_control_operations
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND operation_id = ? AND operation_kind = 'force_takeover'",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .bind(operation_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load force handoff binding", source))?
        .flatten();
        if stored.as_deref() != Some(handoff_id) {
            return Err(WorkBranchControlError::NeedsRepair(
                "force operation rejected its handoff binding".into(),
            ));
        }
        Ok(())
    }

    pub async fn load_force_context(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        operation_id: &str,
    ) -> Result<WorkBranchForceContext, WorkBranchControlError> {
        let row = sqlx::query(
            "SELECT * FROM work_branch_control_operations
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load force context", source))?
        .ok_or(WorkBranchControlError::OperationNotFound)?;
        let operation = decode_operation(&row)?;
        if operation.kind != WorkBranchControlKind::ForceTakeover {
            return Ok(WorkBranchForceContext {
                operation,
                session_id: String::new(),
                handoff_id: None,
            });
        }
        Ok(WorkBranchForceContext {
            operation,
            session_id: row
                .try_get("session_id")
                .map_err(|source| database_error("decode force session binding", source))?,
            handoff_id: row
                .try_get("handoff_id")
                .map_err(|source| database_error("decode force handoff binding", source))?,
        })
    }

    pub async fn abort_force_takeover(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        operation_id: &str,
    ) -> Result<WorkBranchControlOperation, WorkBranchControlError> {
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin force abort", source))?;
        let row = sqlx::query(
            "SELECT expected_branch_revision FROM work_branch_control_operations
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND operation_id = ? AND operation_kind = 'force_takeover'
             FOR UPDATE",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("lock force abort", source))?
        .ok_or(WorkBranchControlError::OperationNotFound)?;
        let expected_revision: i64 = row
            .try_get("expected_branch_revision")
            .map_err(|source| database_error("decode force abort revision", source))?;
        sqlx::query(
            "UPDATE work_branch_control_operations
             SET operation_state = 'aborted', operation_outcome = 'aborted',
                 observed_branch_revision = ?, executor_token = NULL,
                 executor_lease_until = NULL, completed_at = NOW(6)
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND operation_id = ? AND operation_kind = 'force_takeover'
               AND operation_state = 'pending'",
        )
        .bind(expected_revision)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("abort force operation", source))?;
        let operation =
            load_operation_locked(&mut tx, owner_id, work_id, branch_id, operation_id).await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit force abort", source))?;
        Ok(operation)
    }

    pub async fn conflict_force_takeover(
        &self,
        request: &WorkBranchControlRequest,
        operation_id: &str,
        observed_basis: &SessionControllerBasisV1,
    ) -> Result<WorkBranchControlOperation, WorkBranchControlError> {
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error("begin force conflict", source))?;
        lock_active_work(&mut tx, &request.owner_id, &request.work_id).await?;
        let branch = lock_branch(&mut tx, request).await?;
        let outcome = if branch.branch_revision != request.expected_branch_revision.get() {
            WorkBranchControlOutcome::BranchRevisionConflict
        } else {
            WorkBranchControlOutcome::HeadConflict
        };
        sqlx::query(
            "UPDATE work_branch_control_operations
             SET operation_state = 'conflict', operation_outcome = ?,
                 observed_branch_revision = ?, observed_writer_epoch = ?,
                 observed_root_hash = ?, executor_token = NULL,
                 executor_lease_until = NULL, completed_at = NOW(6)
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND operation_id = ? AND operation_kind = 'force_takeover'
               AND operation_state = 'pending'",
        )
        .bind(outcome_name(outcome))
        .bind(branch.branch_revision)
        .bind(i64_from_u64(
            "observed writer epoch",
            observed_basis.writer_epoch,
        )?)
        .bind(observed_basis.canonical_root_hash.as_deref())
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .bind(operation_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("complete force conflict", source))?;
        let operation = load_operation_locked(
            &mut tx,
            &request.owner_id,
            &request.work_id,
            &request.branch_id,
            operation_id,
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit force conflict", source))?;
        Ok(operation)
    }

    pub async fn load(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        operation_id: &str,
    ) -> Result<WorkBranchControlOperation, WorkBranchControlError> {
        let row = sqlx::query(
            "SELECT * FROM work_branch_control_operations
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(operation_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load operation", source))?
        .ok_or(WorkBranchControlError::OperationNotFound)?;
        decode_operation(&row)
    }
}

struct LockedBranch {
    session_id: String,
    branch_revision: i64,
}

async fn lock_active_work(
    tx: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
) -> Result<(), WorkBranchControlError> {
    let active: Option<i64> = sqlx::query_scalar(
        "SELECT work_revision FROM works
         WHERE owner_id = ? AND work_id = ? AND archived_at IS NULL
         FOR UPDATE",
    )
    .bind(owner_id.as_str())
    .bind(work_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("lock Work", source))?;
    active.map(|_| ()).ok_or(WorkBranchControlError::NotFound)
}

async fn lock_branch(
    tx: &mut Transaction<'_, MySql>,
    request: &WorkBranchControlRequest,
) -> Result<LockedBranch, WorkBranchControlError> {
    let row = sqlx::query(
        "SELECT session_id, branch_revision FROM work_branches
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND archived_at IS NULL
           AND deletion_operation_id IS NULL
         FOR UPDATE",
    )
    .bind(request.owner_id.as_str())
    .bind(request.work_id.as_str())
    .bind(request.branch_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("lock Work branch", source))?
    .ok_or(WorkBranchControlError::NotFound)?;
    Ok(LockedBranch {
        session_id: row
            .try_get("session_id")
            .map_err(|source| database_error("decode session binding", source))?,
        branch_revision: row
            .try_get("branch_revision")
            .map_err(|source| database_error("decode branch revision", source))?,
    })
}

async fn load_operation_locked(
    tx: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    branch_id: &WorkBranchId,
    operation_id: &str,
) -> Result<WorkBranchControlOperation, WorkBranchControlError> {
    let row = sqlx::query(
        "SELECT * FROM work_branch_control_operations
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?
         FOR UPDATE",
    )
    .bind(owner_id.as_str())
    .bind(work_id.as_str())
    .bind(branch_id.as_str())
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("load completed operation", source))?
    .ok_or_else(|| {
        WorkBranchControlError::NeedsRepair("admitted control operation disappeared".into())
    })?;
    decode_operation(&row)
}

fn decode_operation(
    row: &sqlx::mysql::MySqlRow,
) -> Result<WorkBranchControlOperation, WorkBranchControlError> {
    let string = |column: &'static str| {
        row.try_get::<String, _>(column)
            .map_err(|source| database_error("decode operation", source))
    };
    let optional_string = |column: &'static str| {
        row.try_get::<Option<String>, _>(column)
            .map_err(|source| database_error("decode operation", source))
    };
    let state = parse_state(&string("operation_state")?)?;
    let outcome = parse_outcome(&string("operation_outcome")?)?;
    let observed_writer_epoch: Option<i64> = row
        .try_get("observed_writer_epoch")
        .map_err(|source| database_error("decode observed writer epoch", source))?;
    let observed_root = optional_string("observed_root_hash")?;
    let control_basis = match (observed_writer_epoch, observed_root) {
        (None, None) => None,
        (Some(epoch), root) => Some(SessionControllerBasisV1 {
            writer_epoch: u64::try_from(epoch).map_err(|_| {
                WorkBranchControlError::NeedsRepair("negative observed writer epoch".into())
            })?,
            canonical_root_hash: root,
        }),
        (None, Some(_)) => {
            return Err(WorkBranchControlError::NeedsRepair(
                "observed root has no writer epoch".into(),
            ));
        }
    };
    let expected_branch_revision: i64 = row
        .try_get("expected_branch_revision")
        .map_err(|source| database_error("decode expected branch revision", source))?;
    let observed_branch_revision: Option<i64> = row
        .try_get("observed_branch_revision")
        .map_err(|source| database_error("decode observed branch revision", source))?;
    let branch_revision = observed_branch_revision.unwrap_or(expected_branch_revision);
    let control_basis = if state == WorkBranchControlState::Pending && control_basis.is_none() {
        let expected_writer_epoch: i64 = row
            .try_get("expected_writer_epoch")
            .map_err(|source| database_error("decode expected writer epoch", source))?;
        Some(SessionControllerBasisV1 {
            writer_epoch: u64::try_from(expected_writer_epoch).map_err(|_| {
                WorkBranchControlError::NeedsRepair("negative expected writer epoch".into())
            })?,
            canonical_root_hash: optional_string("expected_root_hash")?,
        })
    } else {
        control_basis
    };
    let authorization_id = optional_string("forced_authorization_id")?;
    let handoff_id = optional_string("handoff_id")?;
    let progress = (state == WorkBranchControlState::Pending).then(|| {
        let phase = if authorization_id.is_none() {
            WorkBranchControlPhase::AwaitingReauthentication
        } else {
            WorkBranchControlPhase::Preparing
        };
        WorkBranchControlProgress {
            phase,
            abortable: handoff_id.is_none(),
        }
    });
    let completed_at: Option<DateTime<Utc>> = row
        .try_get("completed_at")
        .map_err(|source| database_error("decode operation time", source))?;
    if !coherent_operation_result(state, outcome, completed_at.is_some()) {
        return Err(WorkBranchControlError::NeedsRepair(
            "contradictory control operation state, outcome, or completion time".into(),
        ));
    }
    Ok(WorkBranchControlOperation {
        schema_version: WORK_BRANCH_CONTROL_OPERATION_SCHEMA_VERSION,
        operation_id: string("operation_id")?,
        work_id: WorkId::parse(string("work_id")?)
            .map_err(|_| WorkBranchControlError::NeedsRepair("invalid stored Work id".into()))?,
        branch_id: WorkBranchId::parse(string("branch_id")?)
            .map_err(|_| WorkBranchControlError::NeedsRepair("invalid stored branch id".into()))?,
        attachment_id: string("attachment_id")?,
        kind: parse_kind(&string("operation_kind")?)?,
        state,
        outcome,
        branch_revision: WorkBranchRevision::new(branch_revision).map_err(|_| {
            WorkBranchControlError::NeedsRepair("invalid observed branch revision".into())
        })?,
        control_basis,
        progress,
        created_at: row
            .try_get("created_at")
            .map_err(|source| database_error("decode operation time", source))?,
        completed_at,
    })
}

fn coherent_operation_result(
    state: WorkBranchControlState,
    outcome: WorkBranchControlOutcome,
    completed: bool,
) -> bool {
    match state {
        WorkBranchControlState::Pending => {
            !completed && outcome == WorkBranchControlOutcome::Pending
        }
        WorkBranchControlState::Aborted => {
            completed && outcome == WorkBranchControlOutcome::Aborted
        }
        WorkBranchControlState::Succeeded => {
            completed
                && matches!(
                    outcome,
                    WorkBranchControlOutcome::Acquired
                        | WorkBranchControlOutcome::AlreadyControlled
                        | WorkBranchControlOutcome::TakenOver
                        | WorkBranchControlOutcome::Released
                        | WorkBranchControlOutcome::AlreadyReleased
                )
        }
        WorkBranchControlState::Conflict => {
            completed
                && matches!(
                    outcome,
                    WorkBranchControlOutcome::WriterConflict
                        | WorkBranchControlOutcome::BranchRevisionConflict
                        | WorkBranchControlOutcome::HeadConflict
                )
        }
    }
}

fn request_hash(request: &WorkBranchControlRequest) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra.work-branch-control-request.v1\0");
    for value in [
        request.owner_id.as_str(),
        request.work_id.as_str(),
        request.branch_id.as_str(),
        request.attachment_id.as_str(),
        kind_name(request.kind),
    ] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    digest.update(request.expected_branch_revision.get().to_be_bytes());
    digest.update(request.expected_basis.writer_epoch.to_be_bytes());
    if let Some(root) = &request.expected_basis.canonical_root_hash {
        digest.update([1]);
        digest.update((root.len() as u64).to_be_bytes());
        digest.update(root.as_bytes());
    } else {
        digest.update([0]);
    }
    format!("{:x}", digest.finalize())
}

fn identity_hash(request_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra.work-branch-control-idempotency.v1\0");
    digest.update((request_id.len() as u64).to_be_bytes());
    digest.update(request_id.as_bytes());
    format!("{:x}", digest.finalize())
}

fn kind_name(kind: WorkBranchControlKind) -> &'static str {
    match kind {
        WorkBranchControlKind::AcquireBranchControl => "acquire_branch_control",
        WorkBranchControlKind::ForceTakeover => "force_takeover",
        WorkBranchControlKind::ReleaseBranchControl => "release_branch_control",
    }
}

fn state_name(state: WorkBranchControlState) -> &'static str {
    match state {
        WorkBranchControlState::Pending => "pending",
        WorkBranchControlState::Aborted => "aborted",
        WorkBranchControlState::Succeeded => "succeeded",
        WorkBranchControlState::Conflict => "conflict",
    }
}

fn outcome_name(outcome: WorkBranchControlOutcome) -> &'static str {
    match outcome {
        WorkBranchControlOutcome::Pending => "pending",
        WorkBranchControlOutcome::Aborted => "aborted",
        WorkBranchControlOutcome::Acquired => "acquired",
        WorkBranchControlOutcome::AlreadyControlled => "already_controlled",
        WorkBranchControlOutcome::TakenOver => "taken_over",
        WorkBranchControlOutcome::Released => "released",
        WorkBranchControlOutcome::AlreadyReleased => "already_released",
        WorkBranchControlOutcome::WriterConflict => "writer_conflict",
        WorkBranchControlOutcome::BranchRevisionConflict => "branch_revision_conflict",
        WorkBranchControlOutcome::HeadConflict => "head_conflict",
    }
}

fn parse_kind(value: &str) -> Result<WorkBranchControlKind, WorkBranchControlError> {
    match value {
        "acquire_branch_control" => Ok(WorkBranchControlKind::AcquireBranchControl),
        "force_takeover" => Ok(WorkBranchControlKind::ForceTakeover),
        "release_branch_control" => Ok(WorkBranchControlKind::ReleaseBranchControl),
        _ => Err(WorkBranchControlError::NeedsRepair(
            "invalid control operation kind".into(),
        )),
    }
}

fn parse_state(value: &str) -> Result<WorkBranchControlState, WorkBranchControlError> {
    match value {
        "pending" => Ok(WorkBranchControlState::Pending),
        "aborted" => Ok(WorkBranchControlState::Aborted),
        "succeeded" => Ok(WorkBranchControlState::Succeeded),
        "conflict" => Ok(WorkBranchControlState::Conflict),
        _ => Err(WorkBranchControlError::NeedsRepair(
            "invalid control operation state".into(),
        )),
    }
}

fn parse_outcome(value: &str) -> Result<WorkBranchControlOutcome, WorkBranchControlError> {
    match value {
        "pending" => Ok(WorkBranchControlOutcome::Pending),
        "aborted" => Ok(WorkBranchControlOutcome::Aborted),
        "acquired" => Ok(WorkBranchControlOutcome::Acquired),
        "already_controlled" => Ok(WorkBranchControlOutcome::AlreadyControlled),
        "taken_over" => Ok(WorkBranchControlOutcome::TakenOver),
        "released" => Ok(WorkBranchControlOutcome::Released),
        "already_released" => Ok(WorkBranchControlOutcome::AlreadyReleased),
        "writer_conflict" => Ok(WorkBranchControlOutcome::WriterConflict),
        "branch_revision_conflict" => Ok(WorkBranchControlOutcome::BranchRevisionConflict),
        "head_conflict" => Ok(WorkBranchControlOutcome::HeadConflict),
        _ => Err(WorkBranchControlError::NeedsRepair(
            "invalid control operation outcome".into(),
        )),
    }
}

fn i64_from_u64(field: &str, value: u64) -> Result<i64, WorkBranchControlError> {
    i64::try_from(value)
        .map_err(|_| WorkBranchControlError::NeedsRepair(format!("{field} exceeds BIGINT")))
}

fn database_error(operation: &'static str, source: sqlx::Error) -> WorkBranchControlError {
    WorkBranchControlError::Database { operation, source }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_handoff_abort_boundary_precedes_fencing() {
        for state in [
            SessionHandoffStateV1::Requested,
            SessionHandoffStateV1::Validating,
            SessionHandoffStateV1::Blocked,
            SessionHandoffStateV1::Aborted,
        ] {
            assert!(force_handoff_is_abortable(state), "{state:?}");
        }
        for state in [
            SessionHandoffStateV1::Draining,
            SessionHandoffStateV1::Checkpointed,
            SessionHandoffStateV1::Fencing,
            SessionHandoffStateV1::Fenced,
            SessionHandoffStateV1::Hydrating,
            SessionHandoffStateV1::Active,
            SessionHandoffStateV1::NeedsReconciliation,
        ] {
            assert!(!force_handoff_is_abortable(state), "{state:?}");
        }
    }
}
