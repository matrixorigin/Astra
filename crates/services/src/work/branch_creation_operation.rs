use super::{
    ForkCursorRef, InternalSessionId, WorkBranchId, WorkBranchRevision, WorkId, WorkOwnerId,
};
use astra_core::SharedPool;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{MySql, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

pub const WORK_BRANCH_CREATION_OPERATION_SCHEMA_VERSION: u16 = 1;
pub const WORK_ACTIVE_BRANCH_MAX: i64 = 32;
const REQUEST_ID_MAX_BYTES: usize = 256;
const EXECUTOR_LEASE_MICROS: i64 = 60 * 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchCreationState {
    Pending,
    Aborted,
    Succeeded,
    Conflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchCreationOutcome {
    Pending,
    Aborted,
    Created,
    BranchRevisionConflict,
    CursorConflict,
    CapacityExceeded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkBranchCreationRequest {
    pub request_id: String,
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub origin_branch_id: WorkBranchId,
    pub expected_branch_revision: WorkBranchRevision,
    pub fork_cursor: ForkCursorRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkBranchCreationOperation {
    pub schema_version: u16,
    pub operation_id: String,
    pub work_id: WorkId,
    pub origin_branch_id: WorkBranchId,
    pub child_branch_id: WorkBranchId,
    pub fork_cursor: ForkCursorRef,
    pub state: WorkBranchCreationState,
    pub outcome: WorkBranchCreationOutcome,
    pub origin_branch_revision: WorkBranchRevision,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkBranchCreationAdmission {
    pub operation: WorkBranchCreationOperation,
    pub origin_session_id: InternalSessionId,
    pub child_session_id: InternalSessionId,
    pub session_fork_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum WorkBranchCreationError {
    #[error("invalid branch creation request: {0}")]
    Invalid(String),
    #[error("Work branch was not found")]
    NotFound,
    #[error("Work or origin branch is archived")]
    Archived,
    #[error("origin branch has a durable deletion in progress")]
    Deleting,
    #[error("branch creation operation was not found")]
    OperationNotFound,
    #[error("branch creation request identity was reused with different inputs")]
    IdempotencyMismatch,
    #[error("branch creation operation conflicts with durable state")]
    Conflict,
    #[error("branch creation state requires repair: {0}")]
    NeedsRepair(String),
    #[error("branch creation database step {operation} failed: {source}")]
    Database {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
}

#[derive(Clone)]
pub struct DatabaseWorkBranchCreationService {
    pool: SharedPool,
}

impl DatabaseWorkBranchCreationService {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    pub async fn admit(
        &self,
        request: &WorkBranchCreationRequest,
    ) -> Result<WorkBranchCreationAdmission, WorkBranchCreationError> {
        validate_request(request)?;
        let idempotency_hash = identity_hash(&request.request_id);
        let request_hash = request_hash(request);
        let mut tx = self.begin("begin branch creation admission").await?;
        let work = sqlx::query(
            "SELECT archived_at FROM works
             WHERE owner_id = ? AND work_id = ? FOR UPDATE",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("lock Work for branch creation", source))?
        .ok_or(WorkBranchCreationError::NotFound)?;
        if work
            .try_get::<Option<DateTime<Utc>>, _>("archived_at")
            .map_err(|source| database_error("decode Work retention", source))?
            .is_some()
        {
            return Err(WorkBranchCreationError::Archived);
        }
        if let Some(row) = sqlx::query(
            "SELECT * FROM work_branch_creation_operations
             WHERE owner_id = ? AND work_id = ? AND origin_branch_id = ?
               AND idempotency_hash = ? FOR UPDATE",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.origin_branch_id.as_str())
        .bind(&idempotency_hash)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("load branch creation retry", source))?
        {
            if row
                .try_get::<String, _>("request_hash")
                .map_err(|source| database_error("decode branch creation request hash", source))?
                != request_hash
            {
                return Err(WorkBranchCreationError::IdempotencyMismatch);
            }
            let admission = decode_admission(&row)?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit branch creation retry", source))?;
            return Ok(admission);
        }
        let origin = sqlx::query(
            "SELECT branch_revision, session_id, goal_revision_ref,
                    criteria_set_revision_ref, current_graph_revision, archived_at,
                    deletion_operation_id
             FROM work_branches
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? FOR UPDATE",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.origin_branch_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("lock branch creation origin", source))?
        .ok_or(WorkBranchCreationError::NotFound)?;
        if origin
            .try_get::<Option<DateTime<Utc>>, _>("archived_at")
            .map_err(|source| database_error("decode origin retention", source))?
            .is_some()
        {
            return Err(WorkBranchCreationError::Archived);
        }
        if origin
            .try_get::<Option<String>, _>("deletion_operation_id")
            .map_err(|source| database_error("decode origin deletion", source))?
            .is_some()
        {
            return Err(WorkBranchCreationError::Deleting);
        }
        let observed_revision: i64 = origin
            .try_get("branch_revision")
            .map_err(|source| database_error("decode origin revision", source))?;
        let origin_session_id: String = origin
            .try_get("session_id")
            .map_err(|source| database_error("decode origin session", source))?;
        InternalSessionId::parse(origin_session_id.clone())
            .map_err(|error| WorkBranchCreationError::NeedsRepair(error.to_string()))?;
        let child_branch_id = format!("branch-{}", Uuid::new_v4().simple());
        let child_session_id = Uuid::new_v4().to_string();
        let mut state = WorkBranchCreationState::Pending;
        let mut outcome = WorkBranchCreationOutcome::Pending;
        if observed_revision != request.expected_branch_revision.get() {
            state = WorkBranchCreationState::Conflict;
            outcome = WorkBranchCreationOutcome::BranchRevisionConflict;
        } else {
            let active_branch_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM work_branches
                 WHERE owner_id = ? AND work_id = ? AND archived_at IS NULL",
            )
            .bind(request.owner_id.as_str())
            .bind(request.work_id.as_str())
            .fetch_one(&mut *tx)
            .await
            .map_err(|source| database_error("count active Work branches", source))?;
            let pending_creation_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM work_branch_creation_operations
                 WHERE owner_id = ? AND work_id = ? AND operation_state = 'pending'",
            )
            .bind(request.owner_id.as_str())
            .bind(request.work_id.as_str())
            .fetch_one(&mut *tx)
            .await
            .map_err(|source| database_error("count pending branch creations", source))?;
            if active_branch_count.saturating_add(pending_creation_count) >= WORK_ACTIVE_BRANCH_MAX
            {
                state = WorkBranchCreationState::Conflict;
                outcome = WorkBranchCreationOutcome::CapacityExceeded;
            }
        }
        let created_at = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT NOW(6)")
            .fetch_one(&mut *tx)
            .await
            .map_err(|source| database_error("load branch creation time", source))?;
        let completed_at = (state != WorkBranchCreationState::Pending).then_some(created_at);
        sqlx::query(
            "INSERT INTO work_branch_creation_operations
             (owner_id, work_id, origin_branch_id, operation_id,
              idempotency_hash, request_hash, child_branch_id,
              origin_session_id, child_session_id, fork_cursor,
              operation_state, operation_outcome, expected_branch_revision,
              observed_branch_revision, goal_revision_ref,
              criteria_set_revision_ref, graph_revision_ref, created_at, completed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.origin_branch_id.as_str())
        .bind(Uuid::new_v4().to_string())
        .bind(idempotency_hash)
        .bind(request_hash)
        .bind(child_branch_id)
        .bind(origin_session_id)
        .bind(child_session_id)
        .bind(request.fork_cursor.as_str())
        .bind(state_name(state))
        .bind(outcome_name(outcome))
        .bind(request.expected_branch_revision.get())
        .bind(observed_revision)
        .bind(
            origin
                .try_get::<i64, _>("goal_revision_ref")
                .map_err(|source| database_error("decode branch creation Goal basis", source))?,
        )
        .bind(
            origin
                .try_get::<i64, _>("criteria_set_revision_ref")
                .map_err(|source| {
                    database_error("decode branch creation criteria basis", source)
                })?,
        )
        .bind(
            origin
                .try_get::<i64, _>("current_graph_revision")
                .map_err(|source| database_error("decode branch creation graph basis", source))?,
        )
        .bind(created_at)
        .bind(completed_at)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("insert branch creation operation", source))?;
        let row = sqlx::query(
            "SELECT * FROM work_branch_creation_operations
             WHERE owner_id = ? AND work_id = ? AND origin_branch_id = ?
               AND idempotency_hash = ? FOR UPDATE",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.origin_branch_id.as_str())
        .bind(identity_hash(&request.request_id))
        .fetch_one(&mut *tx)
        .await
        .map_err(|source| database_error("load admitted branch creation", source))?;
        let admission = decode_admission(&row)?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit branch creation admission", source))?;
        Ok(admission)
    }

    pub async fn record_session_fork(
        &self,
        request: &WorkBranchCreationRequest,
        operation_id: &str,
        executor_token: &str,
        session_fork_id: &str,
    ) -> Result<(), WorkBranchCreationError> {
        if session_fork_id.is_empty() || session_fork_id.len() > 64 {
            return Err(WorkBranchCreationError::Invalid(
                "session fork identity is invalid".into(),
            ));
        }
        sqlx::query(
            "UPDATE work_branch_creation_operations
             SET session_fork_id = ?
             WHERE owner_id = ? AND work_id = ? AND origin_branch_id = ?
               AND operation_id = ? AND operation_state = 'pending'
               AND executor_token = ? AND executor_lease_expires_at > NOW(6)
               AND (session_fork_id IS NULL OR session_fork_id = ?)",
        )
        .bind(session_fork_id)
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.origin_branch_id.as_str())
        .bind(operation_id)
        .bind(executor_token)
        .bind(session_fork_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| database_error("record session fork binding", source))?;
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT session_fork_id FROM work_branch_creation_operations
             WHERE owner_id = ? AND work_id = ? AND origin_branch_id = ?
               AND operation_id = ? AND operation_state = 'pending'",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.origin_branch_id.as_str())
        .bind(operation_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load session fork binding", source))?
        .flatten();
        if stored.as_deref() != Some(session_fork_id) {
            return Err(WorkBranchCreationError::Conflict);
        }
        Ok(())
    }

    /// Claim the single executor for a pending operation. The lease is short
    /// and recoverable, so a process crash does not require a new user action.
    pub async fn claim_execution(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        origin_branch_id: &WorkBranchId,
        operation_id: &str,
    ) -> Result<Option<String>, WorkBranchCreationError> {
        let mut tx = self.begin("begin branch executor claim").await?;
        let row = load_operation_locked_by_identity(
            &mut tx,
            owner_id,
            work_id,
            origin_branch_id,
            operation_id,
        )
        .await?;
        let admission = decode_admission(&row)?;
        if admission.operation.state != WorkBranchCreationState::Pending {
            tx.commit()
                .await
                .map_err(|source| database_error("commit terminal executor claim", source))?;
            return Ok(None);
        }
        let executor_token: Option<String> = row
            .try_get("executor_token")
            .map_err(|source| database_error("decode branch executor", source))?;
        let executor_lease_expires_at: Option<DateTime<Utc>> = row
            .try_get("executor_lease_expires_at")
            .map_err(|source| database_error("decode branch executor lease", source))?;
        let now: DateTime<Utc> = sqlx::query_scalar("SELECT NOW(6)")
            .fetch_one(&mut *tx)
            .await
            .map_err(|source| database_error("load branch executor time", source))?;
        if executor_token.is_some()
            && executor_lease_expires_at.is_some_and(|expires_at| expires_at > now)
        {
            tx.commit()
                .await
                .map_err(|source| database_error("commit busy executor claim", source))?;
            return Ok(None);
        }
        let token = Uuid::new_v4().simple().to_string();
        let result = sqlx::query(
            "UPDATE work_branch_creation_operations
             SET executor_token = ?,
                 executor_lease_expires_at = DATE_ADD(NOW(6), INTERVAL ? MICROSECOND)
             WHERE owner_id = ? AND work_id = ? AND origin_branch_id = ?
               AND operation_id = ? AND operation_state = 'pending'",
        )
        .bind(&token)
        .bind(EXECUTOR_LEASE_MICROS)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(origin_branch_id.as_str())
        .bind(operation_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("claim branch executor", source))?;
        if result.rows_affected() != 1 {
            return Err(WorkBranchCreationError::Conflict);
        }
        tx.commit()
            .await
            .map_err(|source| database_error("commit branch executor claim", source))?;
        Ok(Some(token))
    }

    pub async fn release_execution(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        origin_branch_id: &WorkBranchId,
        operation_id: &str,
        executor_token: &str,
    ) -> Result<(), WorkBranchCreationError> {
        sqlx::query(
            "UPDATE work_branch_creation_operations
             SET executor_token = NULL, executor_lease_expires_at = NULL
             WHERE owner_id = ? AND work_id = ? AND origin_branch_id = ?
               AND operation_id = ? AND operation_state = 'pending'
               AND executor_token = ?",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(origin_branch_id.as_str())
        .bind(operation_id)
        .bind(executor_token)
        .execute(self.pool.get())
        .await
        .map_err(|source| database_error("release branch executor", source))?;
        Ok(())
    }

    pub async fn renew_execution(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        origin_branch_id: &WorkBranchId,
        operation_id: &str,
        executor_token: &str,
    ) -> Result<(), WorkBranchCreationError> {
        let result = sqlx::query(
            "UPDATE work_branch_creation_operations
             SET executor_lease_expires_at = DATE_ADD(NOW(6), INTERVAL ? MICROSECOND)
             WHERE owner_id = ? AND work_id = ? AND origin_branch_id = ?
               AND operation_id = ? AND operation_state = 'pending'
               AND executor_token = ? AND executor_lease_expires_at > NOW(6)",
        )
        .bind(EXECUTOR_LEASE_MICROS)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(origin_branch_id.as_str())
        .bind(operation_id)
        .bind(executor_token)
        .execute(self.pool.get())
        .await
        .map_err(|source| database_error("renew branch executor", source))?;
        if result.rows_affected() != 1 {
            return Err(WorkBranchCreationError::Conflict);
        }
        Ok(())
    }

    /// Terminalize a deterministic parent-cursor rejection. Transient
    /// coordinator failures deliberately remain pending so the exact request
    /// can recover without allocating another child identity.
    pub async fn reject_cursor(
        &self,
        request: &WorkBranchCreationRequest,
        operation_id: &str,
        executor_token: &str,
    ) -> Result<WorkBranchCreationOperation, WorkBranchCreationError> {
        let mut tx = self.begin("begin branch cursor rejection").await?;
        let row = load_operation_locked(&mut tx, request, operation_id).await?;
        let admission = decode_admission(&row)?;
        if admission.operation.outcome == WorkBranchCreationOutcome::CursorConflict {
            tx.commit()
                .await
                .map_err(|source| database_error("commit cursor rejection retry", source))?;
            return Ok(admission.operation);
        }
        if admission.operation.state != WorkBranchCreationState::Pending
            || admission.session_fork_id.is_some()
        {
            return Err(WorkBranchCreationError::Conflict);
        }
        verify_executor(&row, executor_token)?;
        let result = sqlx::query(
            "UPDATE work_branch_creation_operations
             SET operation_state = 'conflict', operation_outcome = 'cursor_conflict',
                 completed_at = NOW(6), executor_token = NULL,
                 executor_lease_expires_at = NULL
             WHERE owner_id = ? AND work_id = ? AND origin_branch_id = ?
               AND operation_id = ? AND operation_state = 'pending'
               AND session_fork_id IS NULL AND executor_token = ?
               AND executor_lease_expires_at > NOW(6)",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.origin_branch_id.as_str())
        .bind(operation_id)
        .bind(executor_token)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("reject branch creation cursor", source))?;
        if result.rows_affected() != 1 {
            return Err(WorkBranchCreationError::Conflict);
        }
        let operation =
            decode_operation(&load_operation_locked(&mut tx, request, operation_id).await?)?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit branch cursor rejection", source))?;
        Ok(operation)
    }

    pub async fn activate(
        &self,
        request: &WorkBranchCreationRequest,
        operation_id: &str,
        executor_token: &str,
    ) -> Result<WorkBranchCreationOperation, WorkBranchCreationError> {
        let mut tx = self.begin("begin branch creation activation").await?;
        let row = load_operation_locked(&mut tx, request, operation_id).await?;
        let admission = decode_admission(&row)?;
        if admission.operation.state == WorkBranchCreationState::Succeeded {
            tx.commit()
                .await
                .map_err(|source| database_error("commit branch activation retry", source))?;
            return Ok(admission.operation);
        }
        if admission.operation.state != WorkBranchCreationState::Pending {
            return Err(WorkBranchCreationError::Conflict);
        }
        verify_executor(&row, executor_token)?;
        let fork_id = admission.session_fork_id.as_deref().ok_or_else(|| {
            WorkBranchCreationError::NeedsRepair("session fork is not bound".into())
        })?;
        let active_fork = sqlx::query(
            "SELECT parent_session_id, child_session_id, state FROM session_forks
             WHERE isolation_domain = 'server' AND owner_user_id = ? AND fork_id = ?
             FOR UPDATE",
        )
        .bind(request.owner_id.as_str())
        .bind(fork_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("verify active session fork", source))?
        .ok_or_else(|| WorkBranchCreationError::NeedsRepair("session fork disappeared".into()))?;
        if active_fork
            .try_get::<String, _>("state")
            .map_err(|source| database_error("decode session fork state", source))?
            != "active"
            || active_fork
                .try_get::<String, _>("parent_session_id")
                .map_err(|source| database_error("decode session fork parent", source))?
                != admission.origin_session_id.as_str()
            || active_fork
                .try_get::<String, _>("child_session_id")
                .map_err(|source| database_error("decode session fork child", source))?
                != admission.child_session_id.as_str()
        {
            return Err(WorkBranchCreationError::Conflict);
        }
        let branch_insert = sqlx::query(
            "INSERT INTO work_branches
             (owner_id, work_id, branch_id, branch_revision, session_id,
              origin_branch_id, fork_cursor, goal_revision_ref,
              criteria_set_revision_ref, basis_graph_revision, current_graph_revision)
             SELECT owner_id, work_id, child_branch_id, 1, child_session_id,
                    origin_branch_id, fork_cursor, goal_revision_ref,
                    criteria_set_revision_ref, graph_revision_ref, graph_revision_ref
             FROM work_branch_creation_operations
             WHERE owner_id = ? AND work_id = ? AND origin_branch_id = ?
               AND operation_id = ? AND operation_state = 'pending'
               AND executor_token = ? AND executor_lease_expires_at > NOW(6)",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.origin_branch_id.as_str())
        .bind(operation_id)
        .bind(executor_token)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("activate Work child branch", source))?;
        if branch_insert.rows_affected() != 1 {
            return Err(WorkBranchCreationError::NeedsRepair(
                "pending operation did not materialize one child branch".into(),
            ));
        }
        sqlx::query(
            "INSERT INTO work_proposal_sequences
             (owner_id, work_id, branch_id, last_proposal_seq)
             VALUES (?, ?, ?, 0)",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(admission.operation.child_branch_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("initialize child proposal sequence", source))?;
        let completion = sqlx::query(
            "UPDATE work_branch_creation_operations
             SET operation_state = 'succeeded', operation_outcome = 'created',
                 completed_at = NOW(6), executor_token = NULL,
                 executor_lease_expires_at = NULL
             WHERE owner_id = ? AND work_id = ? AND origin_branch_id = ?
               AND operation_id = ? AND operation_state = 'pending'
               AND executor_token = ? AND executor_lease_expires_at > NOW(6)",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.origin_branch_id.as_str())
        .bind(operation_id)
        .bind(executor_token)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("complete branch creation", source))?;
        if completion.rows_affected() != 1 {
            return Err(WorkBranchCreationError::Conflict);
        }
        let operation =
            decode_operation(&load_operation_locked(&mut tx, request, operation_id).await?)?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit branch creation activation", source))?;
        Ok(operation)
    }

    pub async fn abort(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        origin_branch_id: &WorkBranchId,
        operation_id: &str,
        executor_token: &str,
    ) -> Result<WorkBranchCreationOperation, WorkBranchCreationError> {
        let mut tx = self.begin("begin branch creation abort").await?;
        let row = load_operation_locked_by_identity(
            &mut tx,
            owner_id,
            work_id,
            origin_branch_id,
            operation_id,
        )
        .await?;
        let admission = decode_admission(&row)?;
        if admission.operation.state == WorkBranchCreationState::Aborted {
            tx.commit()
                .await
                .map_err(|source| database_error("commit branch abort retry", source))?;
            return Ok(admission.operation);
        }
        if admission.operation.state != WorkBranchCreationState::Pending {
            return Err(WorkBranchCreationError::Conflict);
        }
        verify_executor(&row, executor_token)?;
        if let Some(fork_id) = admission.session_fork_id.as_deref() {
            let state: Option<String> = sqlx::query_scalar(
                "SELECT state FROM session_forks
                 WHERE isolation_domain = 'server' AND owner_user_id = ? AND fork_id = ?
                 FOR UPDATE",
            )
            .bind(owner_id.as_str())
            .bind(fork_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|source| database_error("verify aborted session fork", source))?;
            if state.as_deref() != Some("aborted") {
                return Err(WorkBranchCreationError::Conflict);
            }
        }
        let completion = sqlx::query(
            "UPDATE work_branch_creation_operations
             SET operation_state = 'aborted', operation_outcome = 'aborted',
                 completed_at = NOW(6), executor_token = NULL,
                 executor_lease_expires_at = NULL
             WHERE owner_id = ? AND work_id = ? AND origin_branch_id = ?
               AND operation_id = ? AND operation_state = 'pending'
               AND executor_token = ? AND executor_lease_expires_at > NOW(6)",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(origin_branch_id.as_str())
        .bind(operation_id)
        .bind(executor_token)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("abort branch creation", source))?;
        if completion.rows_affected() != 1 {
            return Err(WorkBranchCreationError::Conflict);
        }
        let operation = decode_operation(
            &load_operation_locked_by_identity(
                &mut tx,
                owner_id,
                work_id,
                origin_branch_id,
                operation_id,
            )
            .await?,
        )?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit branch creation abort", source))?;
        Ok(operation)
    }

    pub async fn load(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        origin_branch_id: &WorkBranchId,
        operation_id: &str,
    ) -> Result<WorkBranchCreationAdmission, WorkBranchCreationError> {
        let row = sqlx::query(
            "SELECT * FROM work_branch_creation_operations
             WHERE owner_id = ? AND work_id = ? AND origin_branch_id = ? AND operation_id = ?",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(origin_branch_id.as_str())
        .bind(operation_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load branch creation operation", source))?
        .ok_or(WorkBranchCreationError::OperationNotFound)?;
        decode_admission(&row)
    }

    async fn begin(
        &self,
        operation: &'static str,
    ) -> Result<Transaction<'_, MySql>, WorkBranchCreationError> {
        self.pool
            .get()
            .begin()
            .await
            .map_err(|source| database_error(operation, source))
    }
}

async fn load_operation_locked<'a>(
    tx: &mut Transaction<'a, MySql>,
    request: &WorkBranchCreationRequest,
    operation_id: &str,
) -> Result<sqlx::mysql::MySqlRow, WorkBranchCreationError> {
    let row = sqlx::query(
        "SELECT * FROM work_branch_creation_operations
         WHERE owner_id = ? AND work_id = ? AND origin_branch_id = ? AND operation_id = ?
         FOR UPDATE",
    )
    .bind(request.owner_id.as_str())
    .bind(request.work_id.as_str())
    .bind(request.origin_branch_id.as_str())
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("lock branch creation operation", source))?
    .ok_or(WorkBranchCreationError::OperationNotFound)?;
    if row
        .try_get::<String, _>("request_hash")
        .map_err(|source| database_error("decode branch creation request hash", source))?
        != request_hash(request)
    {
        return Err(WorkBranchCreationError::IdempotencyMismatch);
    }
    Ok(row)
}

async fn load_operation_locked_by_identity<'a>(
    tx: &mut Transaction<'a, MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    origin_branch_id: &WorkBranchId,
    operation_id: &str,
) -> Result<sqlx::mysql::MySqlRow, WorkBranchCreationError> {
    sqlx::query(
        "SELECT * FROM work_branch_creation_operations
         WHERE owner_id = ? AND work_id = ? AND origin_branch_id = ? AND operation_id = ?
         FOR UPDATE",
    )
    .bind(owner_id.as_str())
    .bind(work_id.as_str())
    .bind(origin_branch_id.as_str())
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("lock branch creation operation", source))?
    .ok_or(WorkBranchCreationError::OperationNotFound)
}

fn verify_executor(
    row: &sqlx::mysql::MySqlRow,
    executor_token: &str,
) -> Result<(), WorkBranchCreationError> {
    if executor_token.is_empty()
        || row
            .try_get::<Option<String>, _>("executor_token")
            .map_err(|source| database_error("decode branch executor", source))?
            .as_deref()
            != Some(executor_token)
    {
        return Err(WorkBranchCreationError::Conflict);
    }
    Ok(())
}

fn decode_admission(
    row: &sqlx::mysql::MySqlRow,
) -> Result<WorkBranchCreationAdmission, WorkBranchCreationError> {
    Ok(WorkBranchCreationAdmission {
        operation: decode_operation(row)?,
        origin_session_id: InternalSessionId::parse(row_string(row, "origin_session_id")?)
            .map_err(|error| WorkBranchCreationError::NeedsRepair(error.to_string()))?,
        child_session_id: InternalSessionId::parse(row_string(row, "child_session_id")?)
            .map_err(|error| WorkBranchCreationError::NeedsRepair(error.to_string()))?,
        session_fork_id: row
            .try_get("session_fork_id")
            .map_err(|source| database_error("decode session fork binding", source))?,
    })
}

fn decode_operation(
    row: &sqlx::mysql::MySqlRow,
) -> Result<WorkBranchCreationOperation, WorkBranchCreationError> {
    let state = parse_state(&row_string(row, "operation_state")?)?;
    let outcome = parse_outcome(&row_string(row, "operation_outcome")?)?;
    let completed_at: Option<DateTime<Utc>> = row
        .try_get("completed_at")
        .map_err(|source| database_error("decode branch creation completion", source))?;
    let created_at: DateTime<Utc> = row
        .try_get("created_at")
        .map_err(|source| database_error("decode branch creation time", source))?;
    let state_outcome_consistent = matches!(
        (state, outcome),
        (
            WorkBranchCreationState::Pending,
            WorkBranchCreationOutcome::Pending
        ) | (
            WorkBranchCreationState::Aborted,
            WorkBranchCreationOutcome::Aborted
        ) | (
            WorkBranchCreationState::Succeeded,
            WorkBranchCreationOutcome::Created
        ) | (
            WorkBranchCreationState::Conflict,
            WorkBranchCreationOutcome::BranchRevisionConflict
                | WorkBranchCreationOutcome::CursorConflict
                | WorkBranchCreationOutcome::CapacityExceeded
        )
    );
    if (state == WorkBranchCreationState::Pending) != completed_at.is_none()
        || !state_outcome_consistent
        || completed_at.is_some_and(|completed_at| completed_at < created_at)
    {
        return Err(WorkBranchCreationError::NeedsRepair(
            "operation state, outcome, and completion contradict".into(),
        ));
    }
    Ok(WorkBranchCreationOperation {
        schema_version: WORK_BRANCH_CREATION_OPERATION_SCHEMA_VERSION,
        operation_id: row_string(row, "operation_id")?,
        work_id: WorkId::parse(row_string(row, "work_id")?)
            .map_err(|error| WorkBranchCreationError::NeedsRepair(error.to_string()))?,
        origin_branch_id: WorkBranchId::parse(row_string(row, "origin_branch_id")?)
            .map_err(|error| WorkBranchCreationError::NeedsRepair(error.to_string()))?,
        child_branch_id: WorkBranchId::parse(row_string(row, "child_branch_id")?)
            .map_err(|error| WorkBranchCreationError::NeedsRepair(error.to_string()))?,
        fork_cursor: ForkCursorRef::parse(row_string(row, "fork_cursor")?)
            .map_err(|error| WorkBranchCreationError::NeedsRepair(error.to_string()))?,
        state,
        outcome,
        origin_branch_revision: WorkBranchRevision::new(
            row.try_get("observed_branch_revision")
                .map_err(|source| database_error("decode origin branch revision", source))?,
        )
        .map_err(|error| WorkBranchCreationError::NeedsRepair(error.to_string()))?,
        created_at,
        completed_at,
    })
}

fn validate_request(request: &WorkBranchCreationRequest) -> Result<(), WorkBranchCreationError> {
    if request.request_id.is_empty()
        || request.request_id.len() > REQUEST_ID_MAX_BYTES
        || request.request_id.chars().any(char::is_control)
    {
        return Err(WorkBranchCreationError::Invalid(
            "request identity is empty, unbounded, or contains control characters".into(),
        ));
    }
    Ok(())
}

#[derive(Serialize)]
struct RequestHashInput<'a> {
    owner_id: &'a str,
    work_id: &'a str,
    origin_branch_id: &'a str,
    expected_branch_revision: i64,
    fork_cursor: &'a str,
}

fn request_hash(request: &WorkBranchCreationRequest) -> String {
    let canonical = serde_json::to_vec(&RequestHashInput {
        owner_id: request.owner_id.as_str(),
        work_id: request.work_id.as_str(),
        origin_branch_id: request.origin_branch_id.as_str(),
        expected_branch_revision: request.expected_branch_revision.get(),
        fork_cursor: request.fork_cursor.as_str(),
    })
    .expect("branch creation request serialization is infallible");
    format!("{:x}", Sha256::digest(canonical))
}

fn identity_hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn row_string(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
) -> Result<String, WorkBranchCreationError> {
    row.try_get(column)
        .map_err(|source| database_error("decode branch creation operation", source))
}

fn state_name(state: WorkBranchCreationState) -> &'static str {
    match state {
        WorkBranchCreationState::Pending => "pending",
        WorkBranchCreationState::Aborted => "aborted",
        WorkBranchCreationState::Succeeded => "succeeded",
        WorkBranchCreationState::Conflict => "conflict",
    }
}

fn outcome_name(outcome: WorkBranchCreationOutcome) -> &'static str {
    match outcome {
        WorkBranchCreationOutcome::Pending => "pending",
        WorkBranchCreationOutcome::Aborted => "aborted",
        WorkBranchCreationOutcome::Created => "created",
        WorkBranchCreationOutcome::BranchRevisionConflict => "branch_revision_conflict",
        WorkBranchCreationOutcome::CursorConflict => "cursor_conflict",
        WorkBranchCreationOutcome::CapacityExceeded => "capacity_exceeded",
    }
}

fn parse_state(value: &str) -> Result<WorkBranchCreationState, WorkBranchCreationError> {
    match value {
        "pending" => Ok(WorkBranchCreationState::Pending),
        "aborted" => Ok(WorkBranchCreationState::Aborted),
        "succeeded" => Ok(WorkBranchCreationState::Succeeded),
        "conflict" => Ok(WorkBranchCreationState::Conflict),
        _ => Err(WorkBranchCreationError::NeedsRepair(
            "unknown branch creation state".into(),
        )),
    }
}

fn parse_outcome(value: &str) -> Result<WorkBranchCreationOutcome, WorkBranchCreationError> {
    match value {
        "pending" => Ok(WorkBranchCreationOutcome::Pending),
        "aborted" => Ok(WorkBranchCreationOutcome::Aborted),
        "created" => Ok(WorkBranchCreationOutcome::Created),
        "branch_revision_conflict" => Ok(WorkBranchCreationOutcome::BranchRevisionConflict),
        "cursor_conflict" => Ok(WorkBranchCreationOutcome::CursorConflict),
        "capacity_exceeded" => Ok(WorkBranchCreationOutcome::CapacityExceeded),
        _ => Err(WorkBranchCreationError::NeedsRepair(
            "unknown branch creation outcome".into(),
        )),
    }
}

fn database_error(operation: &'static str, source: sqlx::Error) -> WorkBranchCreationError {
    WorkBranchCreationError::Database { operation, source }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(request_id: &str) -> WorkBranchCreationRequest {
        WorkBranchCreationRequest {
            request_id: request_id.into(),
            owner_id: WorkOwnerId::parse("owner-1").expect("owner"),
            work_id: WorkId::parse("work-1").expect("work"),
            origin_branch_id: WorkBranchId::parse("branch-1").expect("branch"),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            fork_cursor: ForkCursorRef::parse("sha256:cursor-1").expect("cursor"),
        }
    }

    #[test]
    fn request_hash_binds_causal_fork_inputs_but_not_generated_child_identity() {
        let first = request("request-1");
        let mut changed = first.clone();
        changed.expected_branch_revision = WorkBranchRevision::new(2).expect("revision");
        assert_ne!(request_hash(&first), request_hash(&changed));
        assert_eq!(request_hash(&first), request_hash(&first.clone()));
    }

    #[test]
    fn request_identity_is_bounded_without_interpreting_user_text() {
        assert!(validate_request(&request("request-1")).is_ok());
        assert!(validate_request(&request("")).is_err());
        assert!(validate_request(&request(&"x".repeat(REQUEST_ID_MAX_BYTES + 1))).is_err());
        assert!(validate_request(&request("request\n2")).is_err());
    }
}
