use super::{
    GraphRevision, WorkBranchId, WorkBranchRevision, WorkBranchSubjectRevision, WorkChangeRef,
    WorkContentHash, WorkDomainError, WorkEventKind, WorkId, WorkOwnerId, WorkPatchArtifactId,
    WorkProviderInvocationRef, WorkSubjectRef,
};
use astra_core::SharedPool;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{Row, query};
use thiserror::Error;
use uuid::Uuid;

pub const WORK_PATCH_COMMIT_SCHEMA_VERSION: u16 = 1;
pub const WORK_PATCH_COMMIT_PAGE_MAX_ITEMS: u16 = 50;
pub const SERVER_GIT_WORKTREE_COMMIT_PROVIDER_REF: &str = "server-git-worktree-commit-v1";
const EXECUTOR_LEASE_MICROS: i64 = 30_000_000;
const RECOVERY_RETRY_MICROS: i64 = 10_000_000;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WorkPatchCommitId(String);

impl WorkPatchCommitId {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        super::validate_resource_identity("work_patch_commit_id", &value, 64)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WorkPatchCommitProviderRef(String);

impl WorkPatchCommitProviderRef {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        super::validate_identity("work_patch_commit_provider_ref", &value, 128)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkPatchCommitRequest {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub request_id: WorkChangeRef,
    pub target_branch_id: WorkBranchId,
    pub patch_artifact_id: WorkPatchArtifactId,
    pub expected_target_branch_revision: WorkBranchRevision,
    pub expected_target_graph_revision: GraphRevision,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    pub provider_ref: WorkPatchCommitProviderRef,
    pub policy_decision_ref: WorkChangeRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkPatchCommitCommitted {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub operation_id: WorkPatchCommitId,
    pub executor_token: String,
    pub provider_invocation_ref: WorkProviderInvocationRef,
    pub commit_sha: String,
    pub observed_subject_revision: WorkContentHash,
    pub index_reconciled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkPatchCommitFailure {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub operation_id: WorkPatchCommitId,
    pub executor_token: String,
    pub provider_invocation_ref: WorkProviderInvocationRef,
    pub failure_code: WorkPatchCommitFailureCode,
    pub observed_subject_revision: Option<WorkContentHash>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkPatchCommitState {
    Pending,
    Aborted,
    Succeeded,
    Conflict,
    Failed,
}

impl WorkPatchCommitState {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "aborted" => Some(Self::Aborted),
            "succeeded" => Some(Self::Succeeded),
            "conflict" => Some(Self::Conflict),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkPatchCommitPhase {
    AwaitingDispatch,
    Committing,
    Reconciling,
    Complete,
}

impl WorkPatchCommitPhase {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "awaiting_dispatch" => Some(Self::AwaitingDispatch),
            "committing" => Some(Self::Committing),
            "reconciling" => Some(Self::Reconciling),
            "complete" => Some(Self::Complete),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkPatchCommitFailureCode {
    AuthorizationDenied,
    WorkspaceUnavailable,
    ProviderUnavailable,
    InvalidMetadata,
    BaseChanged,
    ResultChanged,
    PatchRejected,
    CommitRejected,
    RefConflict,
}

impl WorkPatchCommitFailureCode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::AuthorizationDenied => "authorization_denied",
            Self::WorkspaceUnavailable => "workspace_unavailable",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::InvalidMetadata => "invalid_metadata",
            Self::BaseChanged => "base_changed",
            Self::ResultChanged => "result_changed",
            Self::PatchRejected => "patch_rejected",
            Self::CommitRejected => "commit_rejected",
            Self::RefConflict => "ref_conflict",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "authorization_denied" => Some(Self::AuthorizationDenied),
            "workspace_unavailable" => Some(Self::WorkspaceUnavailable),
            "provider_unavailable" => Some(Self::ProviderUnavailable),
            "invalid_metadata" => Some(Self::InvalidMetadata),
            "base_changed" => Some(Self::BaseChanged),
            "result_changed" => Some(Self::ResultChanged),
            "patch_rejected" => Some(Self::PatchRejected),
            "commit_rejected" => Some(Self::CommitRejected),
            "ref_conflict" => Some(Self::RefConflict),
            _ => None,
        }
    }

    const fn is_conflict(self) -> bool {
        matches!(
            self,
            Self::BaseChanged | Self::ResultChanged | Self::RefConflict
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkPatchCommitOperation {
    pub schema_version: u16,
    pub operation_id: WorkPatchCommitId,
    pub work_id: WorkId,
    pub request_id: WorkChangeRef,
    pub patch_artifact_id: WorkPatchArtifactId,
    pub source_branch_id: WorkBranchId,
    pub target_branch_id: WorkBranchId,
    pub target_branch_revision: WorkBranchRevision,
    pub target_graph_revision: GraphRevision,
    #[serde(skip_serializing)]
    pub target_subject_record_revision: WorkBranchSubjectRevision,
    #[serde(skip_serializing)]
    pub subject_ref: WorkSubjectRef,
    pub base_subject_revision: WorkContentHash,
    pub result_subject_revision: WorkContentHash,
    pub payload_hash: WorkContentHash,
    pub message: String,
    #[serde(skip_serializing)]
    pub author_name: String,
    #[serde(skip_serializing)]
    pub author_email: String,
    pub provider_ref: WorkPatchCommitProviderRef,
    pub policy_decision_ref: WorkChangeRef,
    pub state: WorkPatchCommitState,
    pub phase: WorkPatchCommitPhase,
    pub commit_invocation_ref: Option<WorkProviderInvocationRef>,
    pub commit_sha: Option<String>,
    pub observed_subject_revision: Option<WorkContentHash>,
    pub index_reconciled: Option<bool>,
    pub failure_code: Option<WorkPatchCommitFailureCode>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub struct WorkPatchCommitRecoveryItem {
    pub owner_id: WorkOwnerId,
    pub operation: WorkPatchCommitOperation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkPatchCommitPageLimit(u16);

impl WorkPatchCommitPageLimit {
    pub fn new(value: u16) -> Result<Self, WorkPatchCommitError> {
        if value == 0 || value > WORK_PATCH_COMMIT_PAGE_MAX_ITEMS {
            return Err(WorkPatchCommitError::InvalidPage);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkPatchCommitCursor {
    pub created_at: DateTime<Utc>,
    pub operation_id: WorkPatchCommitId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkPatchCommitQuery {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub target_branch_id: WorkBranchId,
    pub before: Option<WorkPatchCommitCursor>,
    pub limit: WorkPatchCommitPageLimit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkPatchCommitPage {
    pub schema_version: u16,
    pub work_id: WorkId,
    pub target_branch_id: WorkBranchId,
    pub operations: Vec<WorkPatchCommitOperation>,
    pub next_cursor: Option<WorkPatchCommitCursor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkPatchCommitConflict {
    RequestIdentity,
    TargetOperation,
    TargetBranchRevision,
    TargetGraphRevision,
    TargetSubject,
}

#[derive(Debug, Error)]
pub enum WorkPatchCommitError {
    #[error("Work patch commit page request is invalid")]
    InvalidPage,
    #[error("Work, branch, or patch was not found")]
    NotFound,
    #[error("Work patch commit conflict: {0:?}")]
    Conflict(WorkPatchCommitConflict),
    #[error("Work patch commit message is invalid")]
    InvalidMessage,
    #[error("Work patch commit is owned by another active executor")]
    ExecutorConflict,
    #[error("Work patch commit transition is not valid from its current phase")]
    InvalidTransition,
    #[error("corrupt persisted Work patch commit: {0}")]
    NeedsRepair(String),
    #[error("Work patch commit persistence failed: {0}")]
    Database(#[from] sqlx::Error),
}

#[derive(Clone, Debug)]
pub struct DatabaseWorkPatchCommitService {
    pool: SharedPool,
}

impl DatabaseWorkPatchCommitService {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    pub async fn admit(
        &self,
        request: &WorkPatchCommitRequest,
    ) -> Result<WorkPatchCommitOperation, WorkPatchCommitError> {
        if !valid_field(&request.message, 4_096, true)
            || !valid_field(&request.author_name, 256, false)
            || !valid_field(&request.author_email, 320, false)
        {
            return Err(WorkPatchCommitError::InvalidMessage);
        }
        let digest = request_digest(request);
        let mut transaction = self.pool.get().begin().await?;
        if let Some(row) = query(
            "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                    DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
             FROM work_patch_commit_operations
             WHERE owner_id = ? AND work_id = ? AND request_id = ? LIMIT 1",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.request_id.as_str())
        .fetch_optional(&mut *transaction)
        .await?
        {
            if row.try_get::<String, _>("request_digest")? != digest {
                return Err(WorkPatchCommitError::Conflict(
                    WorkPatchCommitConflict::RequestIdentity,
                ));
            }
            let operation = decode(&row)?;
            transaction.commit().await?;
            return Ok(operation);
        }
        let pending: Option<i8> = sqlx::query_scalar(
            "SELECT 1 FROM work_patch_commit_operations
             WHERE owner_id = ? AND work_id = ? AND target_branch_id = ?
               AND operation_state = 'pending' LIMIT 1 FOR UPDATE",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.target_branch_id.as_str())
        .fetch_optional(&mut *transaction)
        .await?;
        if pending.is_some() {
            return Err(WorkPatchCommitError::Conflict(
                WorkPatchCommitConflict::TargetOperation,
            ));
        }
        let row = query(
            "SELECT target.branch_revision, target.current_graph_revision,
                    subject.subject_record_revision, subject.branch_revision AS subject_branch_revision,
                    subject.graph_revision AS subject_graph_revision, subject.subject_ref,
                    subject.subject_revision, patch.branch_id AS source_branch_id,
                    patch.base_subject_revision, patch.result_subject_revision, patch.payload_hash
             FROM works w
             JOIN work_branches target
               ON target.owner_id = w.owner_id AND target.work_id = w.work_id
              AND target.branch_id = ? AND target.archived_at IS NULL
             JOIN work_branches source
               ON source.owner_id = w.owner_id AND source.work_id = w.work_id
              AND source.archived_at IS NULL
             JOIN work_branch_subjects subject
               ON subject.owner_id = target.owner_id AND subject.work_id = target.work_id
              AND subject.branch_id = target.branch_id
             JOIN work_patch_artifacts patch
               ON patch.owner_id = source.owner_id AND patch.work_id = source.work_id
              AND patch.branch_id = source.branch_id AND patch.patch_artifact_id = ?
             WHERE w.owner_id = ? AND w.work_id = ? AND w.archived_at IS NULL
             FOR UPDATE",
        )
        .bind(request.target_branch_id.as_str())
        .bind(request.patch_artifact_id.as_str())
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(WorkPatchCommitError::NotFound)?;
        let branch_revision = WorkBranchRevision::new(row.try_get("branch_revision")?)
            .map_err(|error| repair(error.to_string()))?;
        let graph_revision = GraphRevision::new(row.try_get("current_graph_revision")?)
            .map_err(|error| repair(error.to_string()))?;
        if branch_revision != request.expected_target_branch_revision {
            return Err(WorkPatchCommitError::Conflict(
                WorkPatchCommitConflict::TargetBranchRevision,
            ));
        }
        if graph_revision != request.expected_target_graph_revision {
            return Err(WorkPatchCommitError::Conflict(
                WorkPatchCommitConflict::TargetGraphRevision,
            ));
        }
        let subject_branch_revision =
            WorkBranchRevision::new(row.try_get("subject_branch_revision")?)
                .map_err(|error| repair(error.to_string()))?;
        let subject_graph_revision = GraphRevision::new(row.try_get("subject_graph_revision")?)
            .map_err(|error| repair(error.to_string()))?;
        let result_subject_revision =
            WorkContentHash::parse(row.try_get::<String, _>("result_subject_revision")?)
                .map_err(|error| repair(error.to_string()))?;
        let current_subject_revision =
            WorkContentHash::parse(row.try_get::<String, _>("subject_revision")?)
                .map_err(|error| repair(error.to_string()))?;
        if subject_branch_revision != branch_revision
            || subject_graph_revision != graph_revision
            || current_subject_revision != result_subject_revision
        {
            return Err(WorkPatchCommitError::Conflict(
                WorkPatchCommitConflict::TargetSubject,
            ));
        }
        let operation_id = WorkPatchCommitId::parse(Uuid::now_v7().to_string())
            .expect("UUID is a canonical commit operation identity");
        query(
            "INSERT INTO work_patch_commit_operations
             (owner_id, work_id, operation_id, request_id, request_digest,
              patch_artifact_id, source_branch_id, target_branch_id,
              active_target_branch_id,
              target_branch_revision, target_graph_revision,
              target_subject_record_revision, subject_ref, base_subject_revision,
              result_subject_revision, payload_hash, commit_message,
              commit_author_name, commit_author_email, provider_ref,
              policy_decision_ref,
              operation_state, operation_phase)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                     'pending', 'awaiting_dispatch')",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(operation_id.as_str())
        .bind(request.request_id.as_str())
        .bind(digest)
        .bind(request.patch_artifact_id.as_str())
        .bind(row.try_get::<String, _>("source_branch_id")?)
        .bind(request.target_branch_id.as_str())
        .bind(request.target_branch_id.as_str())
        .bind(branch_revision.get())
        .bind(graph_revision.get())
        .bind(row.try_get::<i64, _>("subject_record_revision")?)
        .bind(row.try_get::<String, _>("subject_ref")?)
        .bind(row.try_get::<String, _>("base_subject_revision")?)
        .bind(result_subject_revision.as_str())
        .bind(row.try_get::<String, _>("payload_hash")?)
        .bind(&request.message)
        .bind(&request.author_name)
        .bind(&request.author_email)
        .bind(request.provider_ref.as_str())
        .bind(request.policy_decision_ref.as_str())
        .execute(&mut *transaction)
        .await?;
        let operation = load_in_tx(
            &mut transaction,
            &request.owner_id,
            &request.work_id,
            &operation_id,
        )
        .await?;
        transaction.commit().await?;
        Ok(operation)
    }

    pub async fn load(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        target_branch_id: &WorkBranchId,
        operation_id: &WorkPatchCommitId,
    ) -> Result<WorkPatchCommitOperation, WorkPatchCommitError> {
        let row = query(
            "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                    DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
             FROM work_patch_commit_operations
             WHERE owner_id = ? AND work_id = ? AND target_branch_id = ?
               AND operation_id = ? LIMIT 1",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(target_branch_id.as_str())
        .bind(operation_id.as_str())
        .fetch_optional(self.pool.get())
        .await?
        .ok_or(WorkPatchCommitError::NotFound)?;
        decode(&row)
    }

    pub async fn list_for_target(
        &self,
        request: WorkPatchCommitQuery,
    ) -> Result<WorkPatchCommitPage, WorkPatchCommitError> {
        let branch_exists: Option<i8> = sqlx::query_scalar(
            "SELECT 1 FROM work_branches b
             JOIN works w ON w.owner_id = b.owner_id AND w.work_id = b.work_id
             WHERE b.owner_id = ? AND b.work_id = ? AND b.branch_id = ?
               AND b.archived_at IS NULL AND w.archived_at IS NULL LIMIT 1",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.target_branch_id.as_str())
        .fetch_optional(self.pool.get())
        .await?;
        if branch_exists.is_none() {
            return Err(WorkPatchCommitError::NotFound);
        }
        let cursor_time = request
            .before
            .as_ref()
            .map(|cursor| cursor.created_at.naive_utc());
        let cursor_id = request
            .before
            .as_ref()
            .map(|cursor| cursor.operation_id.as_str());
        let fetch_limit = i64::from(request.limit.get()) + 1;
        let mut rows = query(
            "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                    DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
             FROM work_patch_commit_operations
             WHERE owner_id = ? AND work_id = ? AND target_branch_id = ?
               AND (? IS NULL OR created_at < ?
                    OR (created_at = ? AND operation_id < ?))
             ORDER BY created_at DESC, operation_id DESC LIMIT ?",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.target_branch_id.as_str())
        .bind(cursor_time)
        .bind(cursor_time)
        .bind(cursor_time)
        .bind(cursor_id)
        .bind(fetch_limit)
        .fetch_all(self.pool.get())
        .await?;
        let has_more = rows.len() > usize::from(request.limit.get());
        if has_more {
            rows.pop();
        }
        let operations = rows.iter().map(decode).collect::<Result<Vec<_>, _>>()?;
        let next_cursor = has_more.then(|| {
            let last = operations
                .last()
                .expect("a commit page with more rows is non-empty");
            WorkPatchCommitCursor {
                created_at: last.created_at,
                operation_id: last.operation_id.clone(),
            }
        });
        Ok(WorkPatchCommitPage {
            schema_version: WORK_PATCH_COMMIT_SCHEMA_VERSION,
            work_id: request.work_id,
            target_branch_id: request.target_branch_id,
            operations,
            next_cursor,
        })
    }

    pub async fn recovery_cycle_upper_bound(
        &self,
    ) -> Result<Option<WorkPatchCommitId>, WorkPatchCommitError> {
        let operation_id: Option<String> = query(
            "SELECT MAX(operation_id) AS operation_id
             FROM work_patch_commit_operations
             WHERE operation_state = 'pending' AND recovery_after <= NOW(6)
               AND (operation_phase = 'awaiting_dispatch'
                    OR (operation_phase IN ('committing', 'reconciling')
                        AND executor_lease_expires_at <= NOW(6)))",
        )
        .fetch_one(self.pool.get())
        .await?
        .try_get("operation_id")?;
        operation_id
            .map(WorkPatchCommitId::parse)
            .transpose()
            .map_err(|error| repair(error.to_string()))
    }

    pub async fn list_pending_for_recovery(
        &self,
        limit: u16,
        after_operation_id: Option<&WorkPatchCommitId>,
        through_operation_id: &WorkPatchCommitId,
    ) -> Result<Vec<WorkPatchCommitRecoveryItem>, WorkPatchCommitError> {
        let bounded_limit = i64::from(limit.clamp(1, 64));
        let rows = match after_operation_id {
            Some(cursor) => query(
                "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                        DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
                 FROM work_patch_commit_operations
                 WHERE operation_state = 'pending' AND recovery_after <= NOW(6)
                   AND (operation_phase = 'awaiting_dispatch'
                        OR (operation_phase IN ('committing', 'reconciling')
                            AND executor_lease_expires_at <= NOW(6)))
                   AND operation_id > ? AND operation_id <= ?
                 ORDER BY operation_id ASC LIMIT ?",
            )
            .bind(cursor.as_str())
            .bind(through_operation_id.as_str())
            .bind(bounded_limit)
            .fetch_all(self.pool.get())
            .await?,
            None => query(
                "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                        DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
                 FROM work_patch_commit_operations
                 WHERE operation_state = 'pending' AND recovery_after <= NOW(6)
                   AND (operation_phase = 'awaiting_dispatch'
                        OR (operation_phase IN ('committing', 'reconciling')
                            AND executor_lease_expires_at <= NOW(6)))
                   AND operation_id <= ?
                 ORDER BY operation_id ASC LIMIT ?",
            )
            .bind(through_operation_id.as_str())
            .bind(bounded_limit)
            .fetch_all(self.pool.get())
            .await?,
        };
        rows.into_iter()
            .map(|row| {
                let owner_id = WorkOwnerId::parse(row.try_get::<String, _>("owner_id")?)
                    .map_err(|error| repair(error.to_string()))?;
                Ok(WorkPatchCommitRecoveryItem {
                    owner_id,
                    operation: decode(&row)?,
                })
            })
            .collect()
    }

    pub async fn defer_recovery(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        operation_id: &WorkPatchCommitId,
    ) -> Result<(), WorkPatchCommitError> {
        query(
            "UPDATE work_patch_commit_operations
             SET recovery_after = DATE_ADD(NOW(6), INTERVAL ? MICROSECOND)
             WHERE owner_id = ? AND work_id = ? AND operation_id = ?
               AND operation_state = 'pending' AND operation_phase = 'awaiting_dispatch'",
        )
        .bind(RECOVERY_RETRY_MICROS)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(operation_id.as_str())
        .execute(self.pool.get())
        .await?;
        Ok(())
    }

    pub async fn load_patch_payload(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        operation_id: &WorkPatchCommitId,
    ) -> Result<Vec<u8>, WorkPatchCommitError> {
        let row = query(
            "SELECT o.payload_hash AS operation_payload_hash,
                    p.payload_hash, p.payload_bytes,
                    a.artifact_kind, a.status AS artifact_status, a.content_json
             FROM work_patch_commit_operations o
             JOIN work_patch_artifacts p
               ON p.owner_id = o.owner_id AND p.work_id = o.work_id
              AND p.patch_artifact_id = o.patch_artifact_id
             JOIN session_artifacts a
               ON a.user_id = p.owner_id AND a.session_id = p.session_id
              AND a.artifact_id = p.payload_artifact_id
             WHERE o.owner_id = ? AND o.work_id = ? AND o.operation_id = ? LIMIT 1",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(operation_id.as_str())
        .fetch_optional(self.pool.get())
        .await?
        .ok_or(WorkPatchCommitError::NotFound)?;
        if row.try_get::<String, _>("artifact_kind")? != "patch"
            || row.try_get::<String, _>("artifact_status")? != "active"
        {
            return Err(repair("patch payload artifact is not active".into()));
        }
        let content: Value = serde_json::from_str(&row.try_get::<String, _>("content_json")?)
            .map_err(|error| repair(error.to_string()))?;
        let text = |field: &'static str| {
            content
                .get(field)
                .and_then(Value::as_str)
                .ok_or_else(|| repair(format!("patch payload is missing {field}")))
        };
        if text("kind")? != "patch"
            || text("content_type")? != "text/x-diff"
            || text("encoding")? != "utf-8"
        {
            return Err(repair("patch payload contract is unsupported".into()));
        }
        let data = text("data")?.as_bytes();
        let declared_bytes = content
            .get("byte_size")
            .and_then(Value::as_u64)
            .ok_or_else(|| repair("patch payload is missing byte_size".into()))?;
        let persisted_bytes = u64::try_from(row.try_get::<i64, _>("payload_bytes")?)
            .map_err(|_| repair("patch payload byte count is negative".into()))?;
        if declared_bytes != data.len() as u64
            || persisted_bytes != declared_bytes
            || declared_bytes > super::WORK_PATCH_ARTIFACT_MAX_BYTES
        {
            return Err(repair("patch payload byte count is incoherent".into()));
        }
        let digest = format!("sha256:{:x}", Sha256::digest(data));
        if text("sha256")? != &digest[7..]
            || row.try_get::<String, _>("payload_hash")? != digest
            || row.try_get::<String, _>("operation_payload_hash")? != digest
        {
            return Err(repair("patch payload digest is incoherent".into()));
        }
        Ok(data.to_vec())
    }

    pub async fn claim_committing(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        operation_id: &WorkPatchCommitId,
        executor_token: &str,
        provider_invocation_ref: &WorkProviderInvocationRef,
    ) -> Result<WorkPatchCommitOperation, WorkPatchCommitError> {
        validate_executor_token(executor_token)?;
        let updated = query(
            "UPDATE work_patch_commit_operations
             SET operation_phase = 'committing', executor_token = ?,
                 executor_lease_expires_at = DATE_ADD(NOW(6), INTERVAL ? MICROSECOND),
                 commit_invocation_ref = ?
             WHERE owner_id = ? AND work_id = ? AND operation_id = ?
               AND operation_state = 'pending'
               AND ((operation_phase = 'awaiting_dispatch' AND executor_token IS NULL
                     AND commit_invocation_ref IS NULL)
                    OR (operation_phase = 'committing' AND executor_token = ?
                        AND executor_lease_expires_at > NOW(6)
                        AND commit_invocation_ref = ?))",
        )
        .bind(executor_token)
        .bind(EXECUTOR_LEASE_MICROS)
        .bind(provider_invocation_ref.as_str())
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(operation_id.as_str())
        .bind(executor_token)
        .bind(provider_invocation_ref.as_str())
        .execute(self.pool.get())
        .await?;
        if updated.rows_affected() != 1 {
            return Err(self.executor_miss(owner_id, work_id, operation_id).await?);
        }
        self.load_by_identity(owner_id, work_id, operation_id).await
    }

    pub async fn record_committed(
        &self,
        report: &WorkPatchCommitCommitted,
    ) -> Result<WorkPatchCommitOperation, WorkPatchCommitError> {
        validate_executor_token(&report.executor_token)?;
        if !valid_git_object_id(&report.commit_sha) {
            return Err(repair(
                "provider returned an invalid commit object identity".into(),
            ));
        }
        let mut tx = self.pool.get().begin().await?;
        let row = query(
            "SELECT *, executor_lease_expires_at > NOW(6) AS executor_lease_active,
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                    DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
             FROM work_patch_commit_operations
             WHERE owner_id = ? AND work_id = ? AND operation_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(report.owner_id.as_str())
        .bind(report.work_id.as_str())
        .bind(report.operation_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(WorkPatchCommitError::NotFound)?;
        let operation = decode(&row)?;
        if operation.state != WorkPatchCommitState::Pending {
            if matches!(
                operation.state,
                WorkPatchCommitState::Succeeded | WorkPatchCommitState::Conflict
            ) && operation.commit_invocation_ref.as_ref()
                == Some(&report.provider_invocation_ref)
                && operation.commit_sha.as_deref() == Some(report.commit_sha.as_str())
                && operation.observed_subject_revision.as_ref()
                    == Some(&report.observed_subject_revision)
                && operation.index_reconciled == Some(report.index_reconciled)
            {
                tx.commit().await?;
                return Ok(operation);
            }
            return Err(WorkPatchCommitError::Conflict(
                WorkPatchCommitConflict::RequestIdentity,
            ));
        }
        require_active_executor(
            &row,
            &operation,
            &report.executor_token,
            &report.provider_invocation_ref,
        )?;

        let target = query(
            "SELECT b.branch_revision, b.current_graph_revision,
                    CASE WHEN w.archived_at IS NULL AND b.archived_at IS NULL
                              AND b.deletion_operation_id IS NULL
                         THEN 0 ELSE 1 END AS unavailable,
                    s.subject_record_revision, s.branch_revision AS subject_branch_revision,
                    s.graph_revision AS subject_graph_revision, s.subject_ref, s.subject_revision
             FROM works w
             JOIN work_branches b
               ON b.owner_id = w.owner_id AND b.work_id = w.work_id AND b.branch_id = ?
             LEFT JOIN work_branch_subjects s
               ON s.owner_id = b.owner_id AND s.work_id = b.work_id AND s.branch_id = b.branch_id
             WHERE w.owner_id = ? AND w.work_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(operation.target_branch_id.as_str())
        .bind(report.owner_id.as_str())
        .bind(report.work_id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let target_matches = target.as_ref().is_some_and(|target| {
            target.try_get::<i64, _>("unavailable").ok() == Some(0)
                && target.try_get::<i64, _>("branch_revision").ok()
                    == Some(operation.target_branch_revision.get())
                && target.try_get::<i64, _>("current_graph_revision").ok()
                    == Some(operation.target_graph_revision.get())
                && target.try_get::<i64, _>("subject_record_revision").ok()
                    == Some(operation.target_subject_record_revision.get())
                && target.try_get::<i64, _>("subject_branch_revision").ok()
                    == Some(operation.target_branch_revision.get())
                && target.try_get::<i64, _>("subject_graph_revision").ok()
                    == Some(operation.target_graph_revision.get())
                && target.try_get::<String, _>("subject_ref").ok().as_deref()
                    == Some(operation.subject_ref.as_str())
                && target
                    .try_get::<String, _>("subject_revision")
                    .ok()
                    .as_deref()
                    == Some(operation.result_subject_revision.as_str())
        });
        let terminal_state = if target_matches {
            self.advance_committed_subject(&mut tx, &operation, report)
                .await?;
            "succeeded"
        } else {
            "conflict"
        };
        query(
            "UPDATE work_patch_commit_operations
             SET operation_state = ?, operation_phase = 'complete',
                 active_target_branch_id = NULL, executor_token = NULL,
                 executor_lease_expires_at = NULL, commit_sha = ?,
                 observed_subject_revision = ?, index_reconciled = ?, completed_at = NOW(6)
             WHERE owner_id = ? AND work_id = ? AND operation_id = ?",
        )
        .bind(terminal_state)
        .bind(&report.commit_sha)
        .bind(report.observed_subject_revision.as_str())
        .bind(i8::from(report.index_reconciled))
        .bind(report.owner_id.as_str())
        .bind(report.work_id.as_str())
        .bind(report.operation_id.as_str())
        .execute(&mut *tx)
        .await?;
        let terminal = load_in_tx(
            &mut tx,
            &report.owner_id,
            &report.work_id,
            &report.operation_id,
        )
        .await?;
        tx.commit().await?;
        Ok(terminal)
    }

    async fn advance_committed_subject(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        operation: &WorkPatchCommitOperation,
        report: &WorkPatchCommitCommitted,
    ) -> Result<(), WorkPatchCommitError> {
        let next_branch_revision = operation
            .target_branch_revision
            .checked_next()
            .map_err(|error| repair(error.to_string()))?;
        let next_subject_record_revision = operation
            .target_subject_record_revision
            .checked_next()
            .map_err(|error| repair(error.to_string()))?;
        let source_ref =
            WorkChangeRef::parse(format!("patch-commit:{}", operation.operation_id.as_str()))
                .map_err(|error| repair(error.to_string()))?;
        let branch_update = query(
            "UPDATE work_branches SET branch_revision = ?, updated_at = NOW(6)
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND branch_revision = ? AND current_graph_revision = ?
               AND archived_at IS NULL AND deletion_operation_id IS NULL",
        )
        .bind(next_branch_revision.get())
        .bind(report.owner_id.as_str())
        .bind(report.work_id.as_str())
        .bind(operation.target_branch_id.as_str())
        .bind(operation.target_branch_revision.get())
        .bind(operation.target_graph_revision.get())
        .execute(&mut **tx)
        .await?;
        if branch_update.rows_affected() != 1 {
            return Err(repair("locked commit target missed its branch CAS".into()));
        }
        let subject_update = query(
            "UPDATE work_branch_subjects
             SET subject_record_revision = ?, branch_revision = ?, subject_revision = ?,
                 source_ref = ?, updated_at = NOW(6)
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND subject_record_revision = ? AND branch_revision = ?
               AND graph_revision = ? AND subject_ref = ? AND subject_revision = ?",
        )
        .bind(next_subject_record_revision.get())
        .bind(next_branch_revision.get())
        .bind(report.observed_subject_revision.as_str())
        .bind(source_ref.as_str())
        .bind(report.owner_id.as_str())
        .bind(report.work_id.as_str())
        .bind(operation.target_branch_id.as_str())
        .bind(operation.target_subject_record_revision.get())
        .bind(operation.target_branch_revision.get())
        .bind(operation.target_graph_revision.get())
        .bind(operation.subject_ref.as_str())
        .bind(operation.result_subject_revision.as_str())
        .execute(&mut **tx)
        .await?;
        if subject_update.rows_affected() != 1 {
            return Err(repair("locked commit target missed its subject CAS".into()));
        }
        super::events_repository::append_event(
            tx,
            &super::events::NewWorkEvent {
                owner_id: report.owner_id.clone(),
                work_id: report.work_id.clone(),
                branch_id: Some(operation.target_branch_id.clone()),
                kind: WorkEventKind::SubjectChanged,
                work_revision: None,
                goal_revision: None,
                criterion_set_revision: None,
                branch_revision: Some(next_branch_revision),
                graph_revision: Some(operation.target_graph_revision),
                source_ref,
            },
        )
        .await
        .map_err(|error| repair(error.to_string()))?;
        Ok(())
    }

    pub async fn record_failure(
        &self,
        report: &WorkPatchCommitFailure,
    ) -> Result<WorkPatchCommitOperation, WorkPatchCommitError> {
        validate_executor_token(&report.executor_token)?;
        let mut tx = self.pool.get().begin().await?;
        let row = query(
            "SELECT *, executor_lease_expires_at > NOW(6) AS executor_lease_active,
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                    DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
             FROM work_patch_commit_operations
             WHERE owner_id = ? AND work_id = ? AND operation_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(report.owner_id.as_str())
        .bind(report.work_id.as_str())
        .bind(report.operation_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(WorkPatchCommitError::NotFound)?;
        let operation = decode(&row)?;
        if operation.state != WorkPatchCommitState::Pending {
            if operation.commit_invocation_ref.as_ref() == Some(&report.provider_invocation_ref)
                && operation.failure_code == Some(report.failure_code)
                && operation.observed_subject_revision == report.observed_subject_revision
            {
                tx.commit().await?;
                return Ok(operation);
            }
            return Err(WorkPatchCommitError::Conflict(
                WorkPatchCommitConflict::RequestIdentity,
            ));
        }
        require_active_executor(
            &row,
            &operation,
            &report.executor_token,
            &report.provider_invocation_ref,
        )?;
        let terminal_state = if report.failure_code.is_conflict() {
            "conflict"
        } else {
            "failed"
        };
        query(
            "UPDATE work_patch_commit_operations
             SET operation_state = ?, operation_phase = 'complete',
                 active_target_branch_id = NULL, executor_token = NULL,
                 executor_lease_expires_at = NULL, observed_subject_revision = ?,
                 failure_code = ?, completed_at = NOW(6)
             WHERE owner_id = ? AND work_id = ? AND operation_id = ?",
        )
        .bind(terminal_state)
        .bind(
            report
                .observed_subject_revision
                .as_ref()
                .map(WorkContentHash::as_str),
        )
        .bind(report.failure_code.as_str())
        .bind(report.owner_id.as_str())
        .bind(report.work_id.as_str())
        .bind(report.operation_id.as_str())
        .execute(&mut *tx)
        .await?;
        let terminal = load_in_tx(
            &mut tx,
            &report.owner_id,
            &report.work_id,
            &report.operation_id,
        )
        .await?;
        tx.commit().await?;
        Ok(terminal)
    }

    pub async fn abort(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        target_branch_id: &WorkBranchId,
        operation_id: &WorkPatchCommitId,
    ) -> Result<WorkPatchCommitOperation, WorkPatchCommitError> {
        let mut tx = self.pool.get().begin().await?;
        let row = query(
            "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                    DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
             FROM work_patch_commit_operations
             WHERE owner_id = ? AND work_id = ? AND target_branch_id = ?
               AND operation_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(target_branch_id.as_str())
        .bind(operation_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(WorkPatchCommitError::NotFound)?;
        let operation = decode(&row)?;
        if operation.state == WorkPatchCommitState::Aborted {
            tx.commit().await?;
            return Ok(operation);
        }
        if operation.state != WorkPatchCommitState::Pending
            || operation.phase != WorkPatchCommitPhase::AwaitingDispatch
            || operation.commit_invocation_ref.is_some()
        {
            return Err(WorkPatchCommitError::InvalidTransition);
        }
        query(
            "UPDATE work_patch_commit_operations
             SET operation_state = 'aborted', operation_phase = 'complete',
                 active_target_branch_id = NULL, completed_at = NOW(6)
             WHERE owner_id = ? AND work_id = ? AND operation_id = ?",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(operation_id.as_str())
        .execute(&mut *tx)
        .await?;
        let terminal = load_in_tx(&mut tx, owner_id, work_id, operation_id).await?;
        tx.commit().await?;
        Ok(terminal)
    }

    pub async fn claim_reconciliation(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        operation_id: &WorkPatchCommitId,
        executor_token: &str,
        provider_invocation_ref: &WorkProviderInvocationRef,
    ) -> Result<WorkPatchCommitOperation, WorkPatchCommitError> {
        validate_executor_token(executor_token)?;
        let updated = query(
            "UPDATE work_patch_commit_operations
             SET operation_phase = 'reconciling', executor_token = ?,
                 executor_lease_expires_at = DATE_ADD(NOW(6), INTERVAL ? MICROSECOND)
             WHERE owner_id = ? AND work_id = ? AND operation_id = ?
               AND operation_state = 'pending' AND commit_invocation_ref = ?
               AND operation_phase IN ('committing', 'reconciling')
               AND (executor_lease_expires_at <= NOW(6)
                    OR (operation_phase = 'reconciling' AND executor_token = ?))",
        )
        .bind(executor_token)
        .bind(EXECUTOR_LEASE_MICROS)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(operation_id.as_str())
        .bind(provider_invocation_ref.as_str())
        .bind(executor_token)
        .execute(self.pool.get())
        .await?;
        if updated.rows_affected() != 1 {
            return Err(self.executor_miss(owner_id, work_id, operation_id).await?);
        }
        self.load_by_identity(owner_id, work_id, operation_id).await
    }

    async fn executor_miss(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        operation_id: &WorkPatchCommitId,
    ) -> Result<WorkPatchCommitError, WorkPatchCommitError> {
        let exists: Option<i8> = sqlx::query_scalar(
            "SELECT 1 FROM work_patch_commit_operations
             WHERE owner_id = ? AND work_id = ? AND operation_id = ?",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(operation_id.as_str())
        .fetch_optional(self.pool.get())
        .await?;
        Ok(if exists.is_some() {
            WorkPatchCommitError::ExecutorConflict
        } else {
            WorkPatchCommitError::NotFound
        })
    }

    async fn load_by_identity(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        operation_id: &WorkPatchCommitId,
    ) -> Result<WorkPatchCommitOperation, WorkPatchCommitError> {
        let row = query(
            "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                    DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
             FROM work_patch_commit_operations
             WHERE owner_id = ? AND work_id = ? AND operation_id = ? LIMIT 1",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(operation_id.as_str())
        .fetch_optional(self.pool.get())
        .await?
        .ok_or(WorkPatchCommitError::NotFound)?;
        decode(&row)
    }
}

async fn load_in_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    operation_id: &WorkPatchCommitId,
) -> Result<WorkPatchCommitOperation, WorkPatchCommitError> {
    let row = query(
        "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
         FROM work_patch_commit_operations
         WHERE owner_id = ? AND work_id = ? AND operation_id = ? LIMIT 1",
    )
    .bind(owner_id.as_str())
    .bind(work_id.as_str())
    .bind(operation_id.as_str())
    .fetch_one(&mut **transaction)
    .await?;
    decode(&row)
}

fn decode(row: &sqlx::mysql::MySqlRow) -> Result<WorkPatchCommitOperation, WorkPatchCommitError> {
    let text = |field: &'static str| -> Result<String, WorkPatchCommitError> {
        row.try_get::<String, _>(field)
            .map_err(WorkPatchCommitError::from)
    };
    let optional_hash =
        |field: &'static str| -> Result<Option<WorkContentHash>, WorkPatchCommitError> {
            row.try_get::<Option<String>, _>(field)?
                .map(WorkContentHash::parse)
                .transpose()
                .map_err(|error| repair(error.to_string()))
        };
    let state_text = text("operation_state")?;
    let phase_text = text("operation_phase")?;
    let operation = WorkPatchCommitOperation {
        schema_version: WORK_PATCH_COMMIT_SCHEMA_VERSION,
        operation_id: WorkPatchCommitId::parse(text("operation_id")?)
            .map_err(|error| repair(error.to_string()))?,
        work_id: WorkId::parse(text("work_id")?).map_err(|error| repair(error.to_string()))?,
        request_id: WorkChangeRef::parse(text("request_id")?)
            .map_err(|error| repair(error.to_string()))?,
        patch_artifact_id: WorkPatchArtifactId::parse(text("patch_artifact_id")?)
            .map_err(|error| repair(error.to_string()))?,
        source_branch_id: WorkBranchId::parse(text("source_branch_id")?)
            .map_err(|error| repair(error.to_string()))?,
        target_branch_id: WorkBranchId::parse(text("target_branch_id")?)
            .map_err(|error| repair(error.to_string()))?,
        target_branch_revision: WorkBranchRevision::new(row.try_get("target_branch_revision")?)
            .map_err(|error| repair(error.to_string()))?,
        target_graph_revision: GraphRevision::new(row.try_get("target_graph_revision")?)
            .map_err(|error| repair(error.to_string()))?,
        target_subject_record_revision: WorkBranchSubjectRevision::new(
            row.try_get("target_subject_record_revision")?,
        )
        .map_err(|error| repair(error.to_string()))?,
        subject_ref: WorkSubjectRef::parse(text("subject_ref")?)
            .map_err(|error| repair(error.to_string()))?,
        base_subject_revision: WorkContentHash::parse(text("base_subject_revision")?)
            .map_err(|error| repair(error.to_string()))?,
        result_subject_revision: WorkContentHash::parse(text("result_subject_revision")?)
            .map_err(|error| repair(error.to_string()))?,
        payload_hash: WorkContentHash::parse(text("payload_hash")?)
            .map_err(|error| repair(error.to_string()))?,
        message: text("commit_message")?,
        author_name: text("commit_author_name")?,
        author_email: text("commit_author_email")?,
        provider_ref: WorkPatchCommitProviderRef::parse(text("provider_ref")?)
            .map_err(|error| repair(error.to_string()))?,
        policy_decision_ref: WorkChangeRef::parse(text("policy_decision_ref")?)
            .map_err(|error| repair(error.to_string()))?,
        state: WorkPatchCommitState::parse(&state_text)
            .ok_or_else(|| repair("unknown commit state".into()))?,
        phase: WorkPatchCommitPhase::parse(&phase_text)
            .ok_or_else(|| repair("unknown commit phase".into()))?,
        commit_invocation_ref: row
            .try_get::<Option<String>, _>("commit_invocation_ref")?
            .map(WorkProviderInvocationRef::parse)
            .transpose()
            .map_err(|error| repair(error.to_string()))?,
        commit_sha: row.try_get("commit_sha")?,
        observed_subject_revision: optional_hash("observed_subject_revision")?,
        index_reconciled: row
            .try_get::<Option<i8>, _>("index_reconciled")?
            .map(|value| match value {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(repair("invalid index reconciliation flag".into())),
            })
            .transpose()?,
        failure_code: row
            .try_get::<Option<String>, _>("failure_code")?
            .map(|value| {
                WorkPatchCommitFailureCode::parse(&value)
                    .ok_or_else(|| repair("unknown commit failure code".into()))
            })
            .transpose()?,
        created_at: text("operation_created_at")?
            .parse()
            .map_err(|error: chrono::ParseError| repair(error.to_string()))?,
        completed_at: row
            .try_get::<Option<String>, _>("operation_completed_at")?
            .map(|value| value.parse())
            .transpose()
            .map_err(|error: chrono::ParseError| repair(error.to_string()))?,
    };
    if (operation.state == WorkPatchCommitState::Pending)
        != (operation.phase != WorkPatchCommitPhase::Complete && operation.completed_at.is_none())
    {
        return Err(repair("incoherent patch commit lifecycle".into()));
    }
    if !valid_field(&operation.message, 4_096, true)
        || !valid_field(&operation.author_name, 256, false)
        || !valid_field(&operation.author_email, 320, false)
    {
        return Err(repair("invalid persisted commit metadata".into()));
    }
    Ok(operation)
}

fn require_active_executor(
    row: &sqlx::mysql::MySqlRow,
    operation: &WorkPatchCommitOperation,
    executor_token: &str,
    provider_invocation_ref: &WorkProviderInvocationRef,
) -> Result<(), WorkPatchCommitError> {
    let active = matches!(
        operation.phase,
        WorkPatchCommitPhase::Committing | WorkPatchCommitPhase::Reconciling
    ) && row
        .try_get::<Option<String>, _>("executor_token")?
        .as_deref()
        == Some(executor_token)
        && row.try_get::<Option<i64>, _>("executor_lease_active")? == Some(1)
        && operation.commit_invocation_ref.as_ref() == Some(provider_invocation_ref);
    if active {
        Ok(())
    } else {
        Err(WorkPatchCommitError::ExecutorConflict)
    }
}

fn validate_executor_token(value: &str) -> Result<(), WorkPatchCommitError> {
    super::validate_identity("work_patch_commit_executor_token", value, 128)
        .map_err(|_| WorkPatchCommitError::ExecutorConflict)
}

fn valid_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_field(value: &str, max_bytes: usize, allow_newline: bool) -> bool {
    !value.trim().is_empty()
        && value.len() <= max_bytes
        && value.chars().all(|character| {
            character != '\0'
                && (!character.is_control()
                    || character == '\t'
                    || (allow_newline && character == '\n'))
        })
}

fn request_digest(request: &WorkPatchCommitRequest) -> String {
    let mut hasher = Sha256::new();
    for value in [
        request.owner_id.as_str(),
        request.work_id.as_str(),
        request.request_id.as_str(),
        request.target_branch_id.as_str(),
        request.patch_artifact_id.as_str(),
        request.message.as_str(),
        request.author_name.as_str(),
        request.author_email.as_str(),
        request.provider_ref.as_str(),
        request.policy_decision_ref.as_str(),
    ] {
        hasher.update(value.len().to_be_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.update(request.expected_target_branch_revision.get().to_be_bytes());
    hasher.update(request.expected_target_graph_revision.get().to_be_bytes());
    format!("{:x}", hasher.finalize())
}

fn repair(message: String) -> WorkPatchCommitError {
    WorkPatchCommitError::NeedsRepair(message)
}
