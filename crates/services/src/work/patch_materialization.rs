use super::{
    CriterionSetRevision, GoalRevision, GraphRevision, WorkBranchId, WorkBranchRevision,
    WorkBranchSubjectRevision, WorkChangeRef, WorkContentHash, WorkDomainError, WorkEventKind,
    WorkEventSeq, WorkId, WorkOwnerId, WorkPatchArtifactId, WorkProviderInvocationRef,
    WorkRepositoryError, WorkRevision, WorkSubjectRef,
};
use astra_core::SharedPool;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{Row, query};
use thiserror::Error;
use uuid::Uuid;

pub const WORK_PATCH_MATERIALIZATION_SCHEMA_VERSION: u16 = 2;
pub const WORK_PATCH_MATERIALIZATION_PAGE_MAX_ITEMS: u16 = 50;
pub const SERVER_GIT_WORKTREE_MATERIALIZATION_PROVIDER_REF: &str = "server-git-worktree-v1";
const EXECUTOR_LEASE_MICROS: i64 = 30_000_000;
const RECOVERY_RETRY_MICROS: i64 = 10_000_000;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WorkPatchMaterializationId(String);

impl WorkPatchMaterializationId {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        super::validate_resource_identity("work_patch_materialization_id", &value, 64)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WorkMaterializationProviderRef(String);

impl WorkMaterializationProviderRef {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        super::validate_identity("work_materialization_provider_ref", &value, 128)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkPatchMaterializationRequest {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub request_id: WorkChangeRef,
    pub patch_artifact_id: WorkPatchArtifactId,
    pub target_branch_id: WorkBranchId,
    pub expected_target_branch_revision: WorkBranchRevision,
    pub expected_target_graph_revision: GraphRevision,
    pub provider_ref: WorkMaterializationProviderRef,
    pub policy_decision_ref: WorkChangeRef,
}

/// Typed provider report for a completed workspace apply attempt. The
/// invocation and resulting immutable subject revision come from the
/// provider boundary; no command output or diff text is interpreted here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkPatchMaterializationApplied {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub operation_id: WorkPatchMaterializationId,
    pub executor_token: String,
    pub provider_invocation_ref: WorkProviderInvocationRef,
    pub observed_subject_revision: WorkContentHash,
}

/// Typed provider evidence that an admitted invocation did not mutate the
/// target workspace. This is intentionally separate from command output: only
/// a provider that owns the invocation may assert the no-effect fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkPatchMaterializationNotApplied {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub operation_id: WorkPatchMaterializationId,
    pub executor_token: String,
    pub provider_invocation_ref: WorkProviderInvocationRef,
    pub failure_code: WorkPatchMaterializationFailureCode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkPatchMaterializationFailureCode {
    ProviderUnavailable,
    AuthorizationDenied,
    WorkspaceUnavailable,
    PatchRejected,
    InvocationCancelled,
    ProviderInternal,
}

impl WorkPatchMaterializationFailureCode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderUnavailable => "provider_unavailable",
            Self::AuthorizationDenied => "authorization_denied",
            Self::WorkspaceUnavailable => "workspace_unavailable",
            Self::PatchRejected => "patch_rejected",
            Self::InvocationCancelled => "invocation_cancelled",
            Self::ProviderInternal => "provider_internal",
        }
    }

    fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "provider_unavailable" => Some(Self::ProviderUnavailable),
            "authorization_denied" => Some(Self::AuthorizationDenied),
            "workspace_unavailable" => Some(Self::WorkspaceUnavailable),
            "patch_rejected" => Some(Self::PatchRejected),
            "invocation_cancelled" => Some(Self::InvocationCancelled),
            "provider_internal" => Some(Self::ProviderInternal),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkPatchMaterializationState {
    Pending,
    Aborted,
    Succeeded,
    Conflict,
    Failed,
}

impl WorkPatchMaterializationState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Aborted => "aborted",
            Self::Succeeded => "succeeded",
            Self::Conflict => "conflict",
            Self::Failed => "failed",
        }
    }

    fn from_persisted(value: &str) -> Option<Self> {
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
pub enum WorkPatchMaterializationPhase {
    AwaitingDispatch,
    Applying,
    Reconciling,
    Verifying,
    Complete,
}

impl WorkPatchMaterializationPhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingDispatch => "awaiting_dispatch",
            Self::Applying => "applying",
            Self::Reconciling => "reconciling",
            Self::Verifying => "verifying",
            Self::Complete => "complete",
        }
    }

    fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "awaiting_dispatch" => Some(Self::AwaitingDispatch),
            "applying" => Some(Self::Applying),
            "reconciling" => Some(Self::Reconciling),
            "verifying" => Some(Self::Verifying),
            "complete" => Some(Self::Complete),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkPatchMaterializationApplyOutcome {
    Applied,
    NotApplied,
    ResultMismatch,
    TargetChanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkPatchMaterializationVerificationOutcome {
    Passed,
    TargetChanged,
}

impl WorkPatchMaterializationVerificationOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::TargetChanged => "target_changed",
        }
    }

    fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "passed" => Some(Self::Passed),
            "target_changed" => Some(Self::TargetChanged),
            _ => None,
        }
    }
}

impl WorkPatchMaterializationApplyOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::NotApplied => "not_applied",
            Self::ResultMismatch => "result_mismatch",
            Self::TargetChanged => "target_changed",
        }
    }

    fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "applied" => Some(Self::Applied),
            "not_applied" => Some(Self::NotApplied),
            "result_mismatch" => Some(Self::ResultMismatch),
            "target_changed" => Some(Self::TargetChanged),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkPatchMaterializationOperation {
    pub schema_version: u16,
    pub operation_id: WorkPatchMaterializationId,
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
    pub provider_ref: WorkMaterializationProviderRef,
    pub policy_decision_ref: WorkChangeRef,
    pub state: WorkPatchMaterializationState,
    pub phase: WorkPatchMaterializationPhase,
    pub apply_invocation_ref: Option<WorkProviderInvocationRef>,
    pub observed_subject_revision: Option<WorkContentHash>,
    pub apply_outcome: Option<WorkPatchMaterializationApplyOutcome>,
    pub failure_code: Option<WorkPatchMaterializationFailureCode>,
    pub verification_evidence_hash: Option<WorkContentHash>,
    pub verification_outcome: Option<WorkPatchMaterializationVerificationOutcome>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub struct WorkPatchMaterializationRecoveryItem {
    pub owner_id: WorkOwnerId,
    pub operation: WorkPatchMaterializationOperation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkPatchMaterializationPageLimit(u16);

impl WorkPatchMaterializationPageLimit {
    pub fn new(value: u16) -> Result<Self, WorkPatchMaterializationError> {
        if value == 0 || value > WORK_PATCH_MATERIALIZATION_PAGE_MAX_ITEMS {
            return Err(WorkPatchMaterializationError::InvalidPage);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkPatchMaterializationCursor {
    pub created_at: DateTime<Utc>,
    pub operation_id: WorkPatchMaterializationId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkPatchMaterializationQuery {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub target_branch_id: WorkBranchId,
    pub source_branch_id: WorkBranchId,
    pub before: Option<WorkPatchMaterializationCursor>,
    pub limit: WorkPatchMaterializationPageLimit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkPatchMaterializationPage {
    pub schema_version: u16,
    pub work_id: WorkId,
    pub target_branch_id: WorkBranchId,
    pub source_branch_id: WorkBranchId,
    pub operations: Vec<WorkPatchMaterializationOperation>,
    pub next_cursor: Option<WorkPatchMaterializationCursor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkPatchMaterializationConflict {
    RequestIdentity,
    TargetOperation,
    ApplyReportIdentity,
    TargetBranchRevision,
    TargetGraphRevision,
    TargetWorkBasis,
    TargetSubject,
    TargetBase,
}

#[derive(Debug, Error)]
pub enum WorkPatchMaterializationError {
    #[error("Work patch materialization page request is invalid")]
    InvalidPage,
    #[error("Work or patch artifact was not found")]
    NotFound,
    #[error("Work patch materialization conflict: {0:?}")]
    Conflict(WorkPatchMaterializationConflict),
    #[error("Work patch materialization target is archived or deleting")]
    UnavailableTarget,
    #[error("Work patch materialization is owned by another active executor")]
    ExecutorConflict,
    #[error("Work patch materialization transition is not valid from its current phase")]
    InvalidTransition,
    #[error("Work patch materialization still requires fresh complete verification")]
    VerificationRequired,
    #[error("corrupt persisted Work patch materialization: {0}")]
    NeedsRepair(String),
    #[error("Work patch materialization persistence failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Work patch materialization repository update failed: {0}")]
    Repository(#[from] WorkRepositoryError),
}

#[derive(Clone, Debug)]
pub struct DatabaseWorkPatchMaterializationService {
    pool: SharedPool,
}

impl DatabaseWorkPatchMaterializationService {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    pub async fn load(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        target_branch_id: &WorkBranchId,
        operation_id: &WorkPatchMaterializationId,
    ) -> Result<WorkPatchMaterializationOperation, WorkPatchMaterializationError> {
        let row = query(
            "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                    DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
             FROM work_patch_materialization_operations
             WHERE owner_id = ? AND work_id = ? AND target_branch_id = ?
               AND operation_id = ? LIMIT 1",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(target_branch_id.as_str())
        .bind(operation_id.as_str())
        .fetch_optional(self.pool.get())
        .await?
        .ok_or(WorkPatchMaterializationError::NotFound)?;
        decode_operation(&row)
    }

    /// Reads durable progress for one source/target branch pair. The owner
    /// scope and immutable keyset keep refresh and reconnect bounded as a
    /// Work accumulates operation history.
    pub async fn list_for_source(
        &self,
        request: WorkPatchMaterializationQuery,
    ) -> Result<WorkPatchMaterializationPage, WorkPatchMaterializationError> {
        let mut transaction = self.pool.get().begin().await?;
        let branch_count: i64 = query(
            "SELECT COUNT(*) FROM work_branches target
             JOIN work_branches source
               ON source.owner_id = target.owner_id AND source.work_id = target.work_id
              AND source.branch_id = ? AND source.archived_at IS NULL
             JOIN works w ON w.owner_id = target.owner_id AND w.work_id = target.work_id
             WHERE target.owner_id = ? AND target.work_id = ? AND target.branch_id = ?
               AND target.archived_at IS NULL AND w.archived_at IS NULL",
        )
        .bind(request.source_branch_id.as_str())
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.target_branch_id.as_str())
        .fetch_one(&mut *transaction)
        .await?
        .try_get(0)?;
        if branch_count == 0 {
            return Err(WorkPatchMaterializationError::NotFound);
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
             FROM work_patch_materialization_operations
             WHERE owner_id = ? AND work_id = ? AND target_branch_id = ?
               AND source_branch_id = ?
               AND (? IS NULL OR created_at < ?
                    OR (created_at = ? AND operation_id < ?))
             ORDER BY created_at DESC, operation_id DESC LIMIT ?",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.target_branch_id.as_str())
        .bind(request.source_branch_id.as_str())
        .bind(cursor_time)
        .bind(cursor_time)
        .bind(cursor_time)
        .bind(cursor_id)
        .bind(fetch_limit)
        .fetch_all(&mut *transaction)
        .await?;
        let has_more = rows.len() > usize::from(request.limit.get());
        if has_more {
            rows.pop();
        }
        let operations = rows
            .iter()
            .map(decode_operation)
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = has_more.then(|| {
            let last = operations
                .last()
                .expect("a materialization page with more rows is non-empty");
            WorkPatchMaterializationCursor {
                created_at: last.created_at,
                operation_id: last.operation_id.clone(),
            }
        });
        transaction.commit().await?;
        Ok(WorkPatchMaterializationPage {
            schema_version: WORK_PATCH_MATERIALIZATION_SCHEMA_VERSION,
            work_id: request.work_id,
            target_branch_id: request.target_branch_id,
            source_branch_id: request.source_branch_id,
            operations,
            next_cursor,
        })
    }

    /// Bounded internal recovery scan. Per-operation CAS/leases remain the
    /// execution authority, so concurrent pods may safely observe the same
    /// row without duplicating a provider invocation.
    pub async fn list_pending_for_recovery(
        &self,
        limit: u16,
        after_operation_id: Option<&WorkPatchMaterializationId>,
        through_operation_id: &WorkPatchMaterializationId,
    ) -> Result<Vec<WorkPatchMaterializationRecoveryItem>, WorkPatchMaterializationError> {
        let bounded_limit = i64::from(limit.clamp(1, 64));
        let rows = match after_operation_id {
            Some(cursor) => query(
                "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                        DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
                 FROM work_patch_materialization_operations
                 WHERE operation_state = 'pending'
                   AND recovery_after <= NOW(6)
                   AND (operation_phase IN ('awaiting_dispatch', 'verifying')
                        OR (operation_phase IN ('applying', 'reconciling')
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
                 FROM work_patch_materialization_operations
                 WHERE operation_state = 'pending'
                   AND recovery_after <= NOW(6)
                   AND (operation_phase IN ('awaiting_dispatch', 'verifying')
                        OR (operation_phase IN ('applying', 'reconciling')
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
                Ok(WorkPatchMaterializationRecoveryItem {
                    owner_id,
                    operation: decode_operation(&row)?,
                })
            })
            .collect()
    }

    /// Freezes one recovery cycle at its current upper bound. New arrivals
    /// are picked up by the next cycle, so sustained admission cannot prevent
    /// the scanner from wrapping and retrying older pending operations.
    pub async fn recovery_cycle_upper_bound(
        &self,
    ) -> Result<Option<WorkPatchMaterializationId>, WorkPatchMaterializationError> {
        let operation_id: Option<String> = query(
            "SELECT MAX(operation_id) AS operation_id
             FROM work_patch_materialization_operations
             WHERE operation_state = 'pending'
               AND recovery_after <= NOW(6)
               AND (operation_phase IN ('awaiting_dispatch', 'verifying')
                    OR (operation_phase IN ('applying', 'reconciling')
                        AND executor_lease_expires_at <= NOW(6)))",
        )
        .fetch_one(self.pool.get())
        .await?
        .try_get("operation_id")?;
        operation_id
            .map(WorkPatchMaterializationId::parse)
            .transpose()
            .map_err(|error| repair(error.to_string()))
    }

    /// Applies a durable retry floor to non-executing recovery phases. This
    /// bounds multi-pod polling when verification evidence or infrastructure
    /// is temporarily unavailable without weakening executor leases.
    pub async fn defer_recovery(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        operation_id: &WorkPatchMaterializationId,
    ) -> Result<(), WorkPatchMaterializationError> {
        query(
            "UPDATE work_patch_materialization_operations
             SET recovery_after = DATE_ADD(NOW(6), INTERVAL ? MICROSECOND)
             WHERE owner_id = ? AND work_id = ? AND operation_id = ?
               AND operation_state = 'pending'
               AND operation_phase IN ('awaiting_dispatch', 'verifying')",
        )
        .bind(RECOVERY_RETRY_MICROS)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(operation_id.as_str())
        .execute(self.pool.get())
        .await?;
        Ok(())
    }

    /// Loads and revalidates the bounded immutable patch payload admitted by
    /// an operation. The execution layer never trusts artifact metadata alone.
    pub async fn load_patch_payload(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        operation_id: &WorkPatchMaterializationId,
    ) -> Result<Vec<u8>, WorkPatchMaterializationError> {
        let row = query(
            "SELECT o.payload_hash AS operation_payload_hash,
                    p.payload_hash, p.payload_bytes,
                    a.artifact_kind, a.status AS artifact_status, a.content_json
             FROM work_patch_materialization_operations o
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
        .ok_or(WorkPatchMaterializationError::NotFound)?;
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

    pub async fn admit(
        &self,
        request: &WorkPatchMaterializationRequest,
    ) -> Result<WorkPatchMaterializationOperation, WorkPatchMaterializationError> {
        let request_digest = request_digest(request);
        let mut tx = self.pool.get().begin().await?;
        query(
            "SELECT work_revision FROM works
             WHERE owner_id = ? AND work_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(WorkPatchMaterializationError::NotFound)?;
        if let Some(row) = query(
            "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                    DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
             FROM work_patch_materialization_operations
             WHERE owner_id = ? AND work_id = ? AND request_id = ? LIMIT 1",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.request_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        {
            let stored_digest: String = row.try_get("request_digest")?;
            if stored_digest != request_digest {
                return Err(WorkPatchMaterializationError::Conflict(
                    WorkPatchMaterializationConflict::RequestIdentity,
                ));
            }
            let operation = decode_operation(&row)?;
            tx.commit().await?;
            return Ok(operation);
        }
        let target_operation_exists: Option<i8> = sqlx::query_scalar(
            "SELECT 1 FROM work_patch_materialization_operations
             WHERE owner_id = ? AND work_id = ? AND target_branch_id = ?
               AND operation_state = 'pending' LIMIT 1 FOR UPDATE",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.target_branch_id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        if target_operation_exists.is_some() {
            return Err(WorkPatchMaterializationError::Conflict(
                WorkPatchMaterializationConflict::TargetOperation,
            ));
        }
        let row = query(
            "SELECT p.branch_id AS source_branch_id, p.subject_ref AS patch_subject_ref,
                    p.base_subject_revision, p.result_subject_revision, p.payload_hash,
                    b.branch_revision, b.current_graph_revision,
                    b.goal_revision_ref, b.criteria_set_revision_ref,
                    w.current_goal_revision, w.current_criteria_set_revision,
                    cs.member_count AS criteria_member_count,
                    CASE WHEN w.archived_at IS NULL AND b.archived_at IS NULL
                          AND b.deletion_operation_id IS NULL THEN 0 ELSE 1 END AS unavailable,
                    s.subject_record_revision, s.branch_revision AS subject_branch_revision,
                    s.graph_revision AS subject_graph_revision, s.subject_ref, s.subject_revision
             FROM works w
             JOIN work_patch_artifacts p
               ON p.owner_id = w.owner_id AND p.work_id = w.work_id
              AND p.patch_artifact_id = ?
             JOIN work_branches b
               ON b.owner_id = w.owner_id AND b.work_id = w.work_id AND b.branch_id = ?
             JOIN work_criterion_sets cs
               ON cs.owner_id = b.owner_id AND cs.work_id = b.work_id
              AND cs.revision = b.criteria_set_revision_ref
             LEFT JOIN work_branch_subjects s
               ON s.owner_id = b.owner_id AND s.work_id = b.work_id AND s.branch_id = b.branch_id
             WHERE w.owner_id = ? AND w.work_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(request.patch_artifact_id.as_str())
        .bind(request.target_branch_id.as_str())
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(WorkPatchMaterializationError::NotFound)?;
        if row.try_get::<i64, _>("unavailable")? != 0 {
            return Err(WorkPatchMaterializationError::UnavailableTarget);
        }
        let branch_revision = WorkBranchRevision::new(row.try_get("branch_revision")?)
            .map_err(|error| repair(error.to_string()))?;
        let graph_revision = GraphRevision::new(row.try_get("current_graph_revision")?)
            .map_err(|error| repair(error.to_string()))?;
        if branch_revision != request.expected_target_branch_revision {
            return Err(WorkPatchMaterializationError::Conflict(
                WorkPatchMaterializationConflict::TargetBranchRevision,
            ));
        }
        if graph_revision != request.expected_target_graph_revision {
            return Err(WorkPatchMaterializationError::Conflict(
                WorkPatchMaterializationConflict::TargetGraphRevision,
            ));
        }
        if row.try_get::<i64, _>("goal_revision_ref")?
            != row.try_get::<i64, _>("current_goal_revision")?
            || row.try_get::<i64, _>("criteria_set_revision_ref")?
                != row.try_get::<i64, _>("current_criteria_set_revision")?
        {
            return Err(WorkPatchMaterializationError::Conflict(
                WorkPatchMaterializationConflict::TargetWorkBasis,
            ));
        }
        if row.try_get::<i64, _>("criteria_member_count")? == 0 {
            return Err(WorkPatchMaterializationError::VerificationRequired);
        }
        let subject_record_revision = required_revision::<WorkBranchSubjectRevision>(
            &row,
            "subject_record_revision",
            WorkBranchSubjectRevision::new,
        )?;
        let subject_branch_revision = required_revision::<WorkBranchRevision>(
            &row,
            "subject_branch_revision",
            WorkBranchRevision::new,
        )?;
        let subject_graph_revision =
            required_revision::<GraphRevision>(&row, "subject_graph_revision", GraphRevision::new)?;
        let subject_ref = required_text(&row, "subject_ref")?;
        let subject_revision = required_text(&row, "subject_revision")?;
        if subject_branch_revision != branch_revision || subject_graph_revision != graph_revision {
            return Err(WorkPatchMaterializationError::Conflict(
                WorkPatchMaterializationConflict::TargetSubject,
            ));
        }
        let patch_subject_ref = required_text(&row, "patch_subject_ref")?;
        let base_subject_revision = required_text(&row, "base_subject_revision")?;
        if patch_subject_ref != subject_ref || base_subject_revision != subject_revision {
            return Err(WorkPatchMaterializationError::Conflict(
                WorkPatchMaterializationConflict::TargetBase,
            ));
        }
        let operation_id = WorkPatchMaterializationId::parse(Uuid::now_v7().to_string())
            .expect("UUID is a canonical operation identity");
        query(
            "INSERT INTO work_patch_materialization_operations
             (owner_id, work_id, operation_id, request_id, request_digest,
              patch_artifact_id, source_branch_id, target_branch_id,
              target_branch_revision, target_graph_revision, target_subject_record_revision,
              subject_ref, base_subject_revision, result_subject_revision, payload_hash,
              provider_ref, policy_decision_ref, operation_state, operation_phase)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending',
                     'awaiting_dispatch')",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(operation_id.as_str())
        .bind(request.request_id.as_str())
        .bind(&request_digest)
        .bind(request.patch_artifact_id.as_str())
        .bind(required_text(&row, "source_branch_id")?)
        .bind(request.target_branch_id.as_str())
        .bind(branch_revision.get())
        .bind(graph_revision.get())
        .bind(subject_record_revision.get())
        .bind(&subject_ref)
        .bind(&base_subject_revision)
        .bind(required_text(&row, "result_subject_revision")?)
        .bind(required_text(&row, "payload_hash")?)
        .bind(request.provider_ref.as_str())
        .bind(request.policy_decision_ref.as_str())
        .execute(&mut *tx)
        .await?;
        let operation_row = query(
            "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                    DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
             FROM work_patch_materialization_operations
             WHERE owner_id = ? AND work_id = ? AND operation_id = ? LIMIT 1",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(operation_id.as_str())
        .fetch_one(&mut *tx)
        .await?;
        let operation = decode_operation(&operation_row)?;
        tx.commit().await?;
        Ok(operation)
    }

    /// Claims or renews the applying phase after durably fixing the provider
    /// invocation identity. An expired mutation lease is never taken over as
    /// another apply: its effect is unknown until a reconciliation observes
    /// the provider/workspace state.
    pub async fn claim_applying(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        operation_id: &WorkPatchMaterializationId,
        executor_token: &str,
        provider_invocation_ref: &WorkProviderInvocationRef,
    ) -> Result<WorkPatchMaterializationOperation, WorkPatchMaterializationError> {
        validate_executor_token(executor_token)?;
        let mut tx = self.pool.get().begin().await?;
        let updated = query(
            "UPDATE work_patch_materialization_operations
             SET operation_phase = 'applying', executor_token = ?,
                 executor_lease_expires_at = DATE_ADD(NOW(6), INTERVAL ? MICROSECOND),
                 apply_invocation_ref = ?
             WHERE owner_id = ? AND work_id = ? AND operation_id = ?
               AND operation_state = 'pending'
               AND (
                 (operation_phase = 'awaiting_dispatch' AND executor_token IS NULL
                  AND apply_invocation_ref IS NULL)
                 OR
                 (operation_phase = 'applying'
                  AND executor_token = ? AND executor_lease_expires_at > NOW(6)
                  AND apply_invocation_ref = ?)
               )",
        )
        .bind(executor_token)
        .bind(EXECUTOR_LEASE_MICROS)
        .bind(provider_invocation_ref.as_str())
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(operation_id.as_str())
        .bind(executor_token)
        .bind(provider_invocation_ref.as_str())
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            let exists: Option<i8> = sqlx::query_scalar(
                "SELECT 1 FROM work_patch_materialization_operations
                 WHERE owner_id = ? AND work_id = ? AND operation_id = ?",
            )
            .bind(owner_id.as_str())
            .bind(work_id.as_str())
            .bind(operation_id.as_str())
            .fetch_optional(&mut *tx)
            .await?;
            return Err(if exists.is_some() {
                WorkPatchMaterializationError::ExecutorConflict
            } else {
                WorkPatchMaterializationError::NotFound
            });
        }
        let row = query(
            "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                    DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
             FROM work_patch_materialization_operations
             WHERE owner_id = ? AND work_id = ? AND operation_id = ? LIMIT 1",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(operation_id.as_str())
        .fetch_one(&mut *tx)
        .await?;
        let operation = decode_operation(&row)?;
        tx.commit().await?;
        Ok(operation)
    }

    /// Claims observation-only reconciliation after an applying executor may
    /// have disappeared. The caller must query the exact persisted provider
    /// invocation or observe the exact subject; it must not invoke apply again.
    pub async fn claim_reconciliation(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        operation_id: &WorkPatchMaterializationId,
        executor_token: &str,
        provider_invocation_ref: &WorkProviderInvocationRef,
    ) -> Result<WorkPatchMaterializationOperation, WorkPatchMaterializationError> {
        validate_executor_token(executor_token)?;
        let mut tx = self.pool.get().begin().await?;
        let updated = query(
            "UPDATE work_patch_materialization_operations
             SET operation_phase = 'reconciling', executor_token = ?,
                 executor_lease_expires_at = DATE_ADD(NOW(6), INTERVAL ? MICROSECOND)
             WHERE owner_id = ? AND work_id = ? AND operation_id = ?
               AND operation_state = 'pending' AND apply_invocation_ref = ?
               AND operation_phase IN ('applying', 'reconciling')
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
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            let exists: Option<i8> = sqlx::query_scalar(
                "SELECT 1 FROM work_patch_materialization_operations
                 WHERE owner_id = ? AND work_id = ? AND operation_id = ?",
            )
            .bind(owner_id.as_str())
            .bind(work_id.as_str())
            .bind(operation_id.as_str())
            .fetch_optional(&mut *tx)
            .await?;
            return Err(if exists.is_some() {
                WorkPatchMaterializationError::ExecutorConflict
            } else {
                WorkPatchMaterializationError::NotFound
            });
        }
        let result = load_operation_by_identity(&mut tx, owner_id, work_id, operation_id).await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Aborts only before a provider invocation exists. Once dispatch begins,
    /// cancellation cannot safely claim that the workspace was unchanged.
    pub async fn abort(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        target_branch_id: &WorkBranchId,
        operation_id: &WorkPatchMaterializationId,
    ) -> Result<WorkPatchMaterializationOperation, WorkPatchMaterializationError> {
        let mut tx = self.pool.get().begin().await?;
        let row = query(
            "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                    DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
             FROM work_patch_materialization_operations
             WHERE owner_id = ? AND work_id = ? AND target_branch_id = ?
               AND operation_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(target_branch_id.as_str())
        .bind(operation_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(WorkPatchMaterializationError::NotFound)?;
        let operation = decode_operation(&row)?;
        if operation.state == WorkPatchMaterializationState::Aborted {
            tx.commit().await?;
            return Ok(operation);
        }
        if operation.state != WorkPatchMaterializationState::Pending
            || operation.phase != WorkPatchMaterializationPhase::AwaitingDispatch
            || operation.apply_invocation_ref.is_some()
        {
            return Err(WorkPatchMaterializationError::InvalidTransition);
        }
        query(
            "UPDATE work_patch_materialization_operations
             SET operation_state = 'aborted', operation_phase = 'complete', completed_at = NOW(6)
             WHERE owner_id = ? AND work_id = ? AND target_branch_id = ? AND operation_id = ?",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(target_branch_id.as_str())
        .bind(operation_id.as_str())
        .execute(&mut *tx)
        .await?;
        let result = load_operation_by_identity(&mut tx, owner_id, work_id, operation_id).await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Commits a typed provider apply report against the exact admission
    /// basis. A matching result advances the branch subject and enters
    /// verification. A known, different result is still recorded as the new
    /// subject but terminates as a conflict. If the target changed while the
    /// provider was applying, the report is retained without overwriting the
    /// newer canonical branch state.
    pub async fn record_applied(
        &self,
        report: &WorkPatchMaterializationApplied,
    ) -> Result<WorkPatchMaterializationOperation, WorkPatchMaterializationError> {
        validate_executor_token(&report.executor_token)?;
        let mut tx = self.pool.get().begin().await?;
        let operation_row = query(
            "SELECT *, executor_lease_expires_at > NOW(6) AS executor_lease_active,
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                    DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
             FROM work_patch_materialization_operations
             WHERE owner_id = ? AND work_id = ? AND operation_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(report.owner_id.as_str())
        .bind(report.work_id.as_str())
        .bind(report.operation_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(WorkPatchMaterializationError::NotFound)?;
        let operation = decode_operation(&operation_row)?;
        if operation.phase == WorkPatchMaterializationPhase::Verifying
            || operation.state != WorkPatchMaterializationState::Pending
        {
            if operation.apply_invocation_ref.as_ref() == Some(&report.provider_invocation_ref)
                && operation.observed_subject_revision.as_ref()
                    == Some(&report.observed_subject_revision)
            {
                tx.commit().await?;
                return Ok(operation);
            }
            return Err(WorkPatchMaterializationError::Conflict(
                WorkPatchMaterializationConflict::ApplyReportIdentity,
            ));
        }
        let active_executor = matches!(
            operation.phase,
            WorkPatchMaterializationPhase::Applying | WorkPatchMaterializationPhase::Reconciling
        ) && operation_row
            .try_get::<Option<String>, _>("executor_token")?
            .as_deref()
            == Some(report.executor_token.as_str())
            && operation_row.try_get::<Option<i64>, _>("executor_lease_active")? == Some(1)
            && operation.apply_invocation_ref.as_ref() == Some(&report.provider_invocation_ref);
        if !active_executor {
            return Err(WorkPatchMaterializationError::ExecutorConflict);
        }

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
        let target_is_current = match target.as_ref() {
            Some(row) => {
                row.try_get::<i64, _>("unavailable")? == 0
                    && row.try_get::<i64, _>("branch_revision")?
                        == operation.target_branch_revision.get()
                    && row.try_get::<i64, _>("current_graph_revision")?
                        == operation.target_graph_revision.get()
                    && row.try_get::<Option<i64>, _>("subject_record_revision")?
                        == Some(operation.target_subject_record_revision.get())
                    && row.try_get::<Option<i64>, _>("subject_branch_revision")?
                        == Some(operation.target_branch_revision.get())
                    && row.try_get::<Option<i64>, _>("subject_graph_revision")?
                        == Some(operation.target_graph_revision.get())
                    && row.try_get::<Option<String>, _>("subject_ref")?.as_deref()
                        == Some(operation.subject_ref.as_str())
                    && row
                        .try_get::<Option<String>, _>("subject_revision")?
                        .as_deref()
                        == Some(operation.base_subject_revision.as_str())
            }
            None => false,
        };
        if !target_is_current {
            persist_apply_report(
                &mut tx,
                report,
                WorkPatchMaterializationState::Conflict,
                WorkPatchMaterializationPhase::Complete,
                WorkPatchMaterializationApplyOutcome::TargetChanged,
            )
            .await?;
            let result = load_operation_in_transaction(&mut tx, report).await?;
            tx.commit().await?;
            return Ok(result);
        }

        let next_branch_revision = operation
            .target_branch_revision
            .checked_next()
            .map_err(|error| repair(error.to_string()))?;
        let next_subject_record_revision = operation
            .target_subject_record_revision
            .checked_next()
            .map_err(|error| repair(error.to_string()))?;
        let branch_update = query(
            "UPDATE work_branches
             SET branch_revision = ?, updated_at = NOW(6)
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
        .execute(&mut *tx)
        .await?;
        if branch_update.rows_affected() != 1 {
            return Err(repair(
                "locked materialization target failed its branch CAS".into(),
            ));
        }
        let source_ref = WorkChangeRef::parse(report.operation_id.as_str().to_owned())
            .expect("materialization operation identity is a valid change reference");
        let subject_update = query(
            "UPDATE work_branch_subjects
             SET subject_record_revision = ?, branch_revision = ?,
                 subject_revision = ?, source_ref = ?, updated_at = NOW(6)
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
        .bind(operation.base_subject_revision.as_str())
        .execute(&mut *tx)
        .await?;
        if subject_update.rows_affected() != 1 {
            return Err(repair(
                "locked materialization target failed its subject CAS".into(),
            ));
        }
        super::events_repository::append_event(
            &mut tx,
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
        .await?;
        let (state, phase, outcome) =
            if report.observed_subject_revision == operation.result_subject_revision {
                (
                    WorkPatchMaterializationState::Pending,
                    WorkPatchMaterializationPhase::Verifying,
                    WorkPatchMaterializationApplyOutcome::Applied,
                )
            } else {
                (
                    WorkPatchMaterializationState::Conflict,
                    WorkPatchMaterializationPhase::Complete,
                    WorkPatchMaterializationApplyOutcome::ResultMismatch,
                )
            };
        persist_apply_report(&mut tx, report, state, phase, outcome).await?;
        let result = load_operation_in_transaction(&mut tx, report).await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Records a provider-owned, typed no-effect result. Transport loss or an
    /// unclassified error must not use this path; those remain reconciling
    /// because the workspace may already have changed.
    pub async fn record_not_applied(
        &self,
        report: &WorkPatchMaterializationNotApplied,
    ) -> Result<WorkPatchMaterializationOperation, WorkPatchMaterializationError> {
        validate_executor_token(&report.executor_token)?;
        let mut tx = self.pool.get().begin().await?;
        let row = query(
            "SELECT *, executor_lease_expires_at > NOW(6) AS executor_lease_active,
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                    DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
             FROM work_patch_materialization_operations
             WHERE owner_id = ? AND work_id = ? AND operation_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(report.owner_id.as_str())
        .bind(report.work_id.as_str())
        .bind(report.operation_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(WorkPatchMaterializationError::NotFound)?;
        let operation = decode_operation(&row)?;
        if operation.state == WorkPatchMaterializationState::Failed
            && operation.apply_invocation_ref.as_ref() == Some(&report.provider_invocation_ref)
            && operation.apply_outcome == Some(WorkPatchMaterializationApplyOutcome::NotApplied)
            && operation.failure_code == Some(report.failure_code)
        {
            tx.commit().await?;
            return Ok(operation);
        }
        let active_executor = operation.state == WorkPatchMaterializationState::Pending
            && matches!(
                operation.phase,
                WorkPatchMaterializationPhase::Applying
                    | WorkPatchMaterializationPhase::Reconciling
            )
            && operation.apply_invocation_ref.as_ref() == Some(&report.provider_invocation_ref)
            && row
                .try_get::<Option<String>, _>("executor_token")?
                .as_deref()
                == Some(report.executor_token.as_str())
            && row.try_get::<Option<i64>, _>("executor_lease_active")? == Some(1);
        if !active_executor {
            return Err(WorkPatchMaterializationError::ExecutorConflict);
        }
        query(
            "UPDATE work_patch_materialization_operations
             SET operation_state = 'failed', operation_phase = 'complete',
                 executor_token = NULL, executor_lease_expires_at = NULL,
                 apply_outcome = 'not_applied', failure_code = ?, completed_at = NOW(6)
             WHERE owner_id = ? AND work_id = ? AND operation_id = ?",
        )
        .bind(report.failure_code.as_str())
        .bind(report.owner_id.as_str())
        .bind(report.work_id.as_str())
        .bind(report.operation_id.as_str())
        .execute(&mut *tx)
        .await?;
        let result = load_operation_by_identity(
            &mut tx,
            &report.owner_id,
            &report.work_id,
            &report.operation_id,
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Completes materialization only when every accepted criterion on the
    /// current target basis has fresh, passed, complete Work check evidence.
    /// An empty criterion set never succeeds vacuously. The evidence manifest
    /// is content-addressed so retries and later audits bind to exact checks.
    pub async fn complete_verification(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        operation_id: &WorkPatchMaterializationId,
    ) -> Result<WorkPatchMaterializationOperation, WorkPatchMaterializationError> {
        let mut tx = self.pool.get().begin().await?;
        let row = query(
            "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                    DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
             FROM work_patch_materialization_operations
             WHERE owner_id = ? AND work_id = ? AND operation_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(operation_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(WorkPatchMaterializationError::NotFound)?;
        let operation = decode_operation(&row)?;
        if operation.state != WorkPatchMaterializationState::Pending {
            tx.commit().await?;
            return Ok(operation);
        }
        if operation.phase != WorkPatchMaterializationPhase::Verifying
            || operation.apply_outcome != Some(WorkPatchMaterializationApplyOutcome::Applied)
        {
            return Err(WorkPatchMaterializationError::VerificationRequired);
        }
        let expected_branch_revision = operation
            .target_branch_revision
            .checked_next()
            .map_err(|error| repair(error.to_string()))?;
        let expected_subject_record_revision = operation
            .target_subject_record_revision
            .checked_next()
            .map_err(|error| repair(error.to_string()))?;
        let basis_row = query(
            "SELECT w.work_revision, w.current_goal_revision, w.current_criteria_set_revision,
                    CASE WHEN w.archived_at IS NULL AND b.archived_at IS NULL
                              AND b.deletion_operation_id IS NULL
                         THEN 0 ELSE 1 END AS unavailable,
                    b.branch_revision, b.goal_revision_ref, b.criteria_set_revision_ref,
                    b.current_graph_revision,
                    s.subject_record_revision, s.branch_revision AS subject_branch_revision,
                    s.graph_revision AS subject_graph_revision, s.subject_ref, s.subject_revision,
                    cs.member_manifest_json, cs.member_count, es.last_event_seq
             FROM works w
             JOIN work_branches b
               ON b.owner_id = w.owner_id AND b.work_id = w.work_id AND b.branch_id = ?
             JOIN work_criterion_sets cs
               ON cs.owner_id = b.owner_id AND cs.work_id = b.work_id
              AND cs.revision = b.criteria_set_revision_ref
             JOIN work_event_sequences es
               ON es.owner_id = w.owner_id AND es.work_id = w.work_id
             LEFT JOIN work_branch_subjects s
               ON s.owner_id = b.owner_id AND s.work_id = b.work_id AND s.branch_id = b.branch_id
             WHERE w.owner_id = ? AND w.work_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(operation.target_branch_id.as_str())
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let basis_is_current = match basis_row.as_ref() {
            Some(row) => {
                row.try_get::<i64, _>("unavailable")? == 0
                    && row.try_get::<i64, _>("branch_revision")? == expected_branch_revision.get()
                    && row.try_get::<i64, _>("current_graph_revision")?
                        == operation.target_graph_revision.get()
                    && row.try_get::<i64, _>("goal_revision_ref")?
                        == row.try_get::<i64, _>("current_goal_revision")?
                    && row.try_get::<i64, _>("criteria_set_revision_ref")?
                        == row.try_get::<i64, _>("current_criteria_set_revision")?
                    && row.try_get::<Option<i64>, _>("subject_record_revision")?
                        == Some(expected_subject_record_revision.get())
                    && row.try_get::<Option<i64>, _>("subject_branch_revision")?
                        == Some(expected_branch_revision.get())
                    && row.try_get::<Option<i64>, _>("subject_graph_revision")?
                        == Some(operation.target_graph_revision.get())
                    && row.try_get::<Option<String>, _>("subject_ref")?.as_deref()
                        == Some(operation.subject_ref.as_str())
                    && row
                        .try_get::<Option<String>, _>("subject_revision")?
                        .as_deref()
                        == Some(operation.result_subject_revision.as_str())
            }
            None => false,
        };
        if !basis_is_current {
            persist_verification_outcome(
                &mut tx,
                owner_id,
                work_id,
                operation_id,
                WorkPatchMaterializationState::Conflict,
                WorkPatchMaterializationVerificationOutcome::TargetChanged,
                None,
            )
            .await?;
            let result =
                load_operation_by_identity(&mut tx, owner_id, work_id, operation_id).await?;
            tx.commit().await?;
            return Ok(result);
        }
        let basis_row = basis_row.expect("current materialization basis row");
        let criteria = super::repository::decode_criterion_set_manifest(
            &basis_row.try_get::<String, _>("member_manifest_json")?,
            basis_row.try_get::<i64, _>("member_count")?,
        )?;
        if criteria.is_empty() {
            return Err(WorkPatchMaterializationError::VerificationRequired);
        }
        let work_revision = WorkRevision::new(basis_row.try_get("work_revision")?)
            .map_err(|error| repair(error.to_string()))?;
        let goal_revision = GoalRevision::new(basis_row.try_get("current_goal_revision")?)
            .map_err(|error| repair(error.to_string()))?;
        let criterion_set_revision =
            CriterionSetRevision::new(basis_row.try_get("current_criteria_set_revision")?)
                .map_err(|error| repair(error.to_string()))?;
        let event_head = WorkEventSeq::new(basis_row.try_get("last_event_seq")?)
            .map_err(|error| repair(error.to_string()))?;
        let evidence_basis = super::observation_repository::DeliveryEvidenceBasis {
            owner_id,
            work_id,
            branch_id: &operation.target_branch_id,
            work_revision,
            goal_revision,
            branch_revision: expected_branch_revision,
            graph_revision: operation.target_graph_revision,
            criterion_set_revision,
            event_head,
            subject_ref: &operation.subject_ref,
            subject_revision: &operation.result_subject_revision,
        };
        let evidence = super::observation_repository::load_delivery_evidence_projection(
            &mut tx,
            Some(&evidence_basis),
            &criteria,
        )
        .await?;
        if evidence.fresh_checks.len() != criteria.len() {
            return Err(WorkPatchMaterializationError::VerificationRequired);
        }
        persist_verification_outcome(
            &mut tx,
            owner_id,
            work_id,
            operation_id,
            WorkPatchMaterializationState::Succeeded,
            WorkPatchMaterializationVerificationOutcome::Passed,
            Some(&evidence.manifest_hash),
        )
        .await?;
        let result = load_operation_by_identity(&mut tx, owner_id, work_id, operation_id).await?;
        tx.commit().await?;
        Ok(result)
    }
}

fn validate_executor_token(token: &str) -> Result<(), WorkPatchMaterializationError> {
    if token.is_empty() || token.len() > 128 || token.trim() != token {
        return Err(WorkPatchMaterializationError::ExecutorConflict);
    }
    Ok(())
}

async fn persist_apply_report(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    report: &WorkPatchMaterializationApplied,
    state: WorkPatchMaterializationState,
    phase: WorkPatchMaterializationPhase,
    outcome: WorkPatchMaterializationApplyOutcome,
) -> Result<(), WorkPatchMaterializationError> {
    let terminal = state != WorkPatchMaterializationState::Pending;
    let statement = if terminal {
        "UPDATE work_patch_materialization_operations
         SET operation_state = ?, operation_phase = ?, executor_token = NULL,
             executor_lease_expires_at = NULL, apply_invocation_ref = ?,
             observed_subject_revision = ?, apply_outcome = ?,
             recovery_after = NOW(6),
             completed_at = NOW(6)
         WHERE owner_id = ? AND work_id = ? AND operation_id = ?"
    } else {
        "UPDATE work_patch_materialization_operations
         SET operation_state = ?, operation_phase = ?, executor_token = NULL,
             executor_lease_expires_at = NULL, apply_invocation_ref = ?,
             observed_subject_revision = ?, apply_outcome = ?,
             recovery_after = NOW(6), completed_at = NULL
         WHERE owner_id = ? AND work_id = ? AND operation_id = ?"
    };
    let updated = query(statement)
        .bind(state.as_str())
        .bind(phase.as_str())
        .bind(report.provider_invocation_ref.as_str())
        .bind(report.observed_subject_revision.as_str())
        .bind(outcome.as_str())
        .bind(report.owner_id.as_str())
        .bind(report.work_id.as_str())
        .bind(report.operation_id.as_str())
        .execute(&mut **tx)
        .await?;
    if updated.rows_affected() != 1 {
        return Err(repair(
            "materialization apply report target disappeared".into(),
        ));
    }
    Ok(())
}

async fn load_operation_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    report: &WorkPatchMaterializationApplied,
) -> Result<WorkPatchMaterializationOperation, WorkPatchMaterializationError> {
    let row = query(
        "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
         FROM work_patch_materialization_operations
         WHERE owner_id = ? AND work_id = ? AND operation_id = ? LIMIT 1",
    )
    .bind(report.owner_id.as_str())
    .bind(report.work_id.as_str())
    .bind(report.operation_id.as_str())
    .fetch_one(&mut **tx)
    .await?;
    decode_operation(&row)
}

async fn persist_verification_outcome(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    operation_id: &WorkPatchMaterializationId,
    state: WorkPatchMaterializationState,
    outcome: WorkPatchMaterializationVerificationOutcome,
    evidence_hash: Option<&WorkContentHash>,
) -> Result<(), WorkPatchMaterializationError> {
    let updated = query(
        "UPDATE work_patch_materialization_operations
         SET operation_state = ?, operation_phase = 'complete',
             verification_evidence_hash = ?, verification_outcome = ?, completed_at = NOW(6)
         WHERE owner_id = ? AND work_id = ? AND operation_id = ?",
    )
    .bind(state.as_str())
    .bind(evidence_hash.map(WorkContentHash::as_str))
    .bind(outcome.as_str())
    .bind(owner_id.as_str())
    .bind(work_id.as_str())
    .bind(operation_id.as_str())
    .execute(&mut **tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(repair(
            "materialization verification target disappeared".into(),
        ));
    }
    Ok(())
}

async fn load_operation_by_identity(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    operation_id: &WorkPatchMaterializationId,
) -> Result<WorkPatchMaterializationOperation, WorkPatchMaterializationError> {
    let row = query(
        "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_created_at,
                DATE_FORMAT(completed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS operation_completed_at
         FROM work_patch_materialization_operations
         WHERE owner_id = ? AND work_id = ? AND operation_id = ? LIMIT 1",
    )
    .bind(owner_id.as_str())
    .bind(work_id.as_str())
    .bind(operation_id.as_str())
    .fetch_one(&mut **tx)
    .await?;
    decode_operation(&row)
}

fn required_revision<T>(
    row: &sqlx::mysql::MySqlRow,
    field: &'static str,
    parse: fn(i64) -> Result<T, WorkDomainError>,
) -> Result<T, WorkPatchMaterializationError> {
    let value = row
        .try_get::<Option<i64>, _>(field)?
        .ok_or_else(|| repair(format!("missing {field}")))?;
    parse(value).map_err(|error| repair(error.to_string()))
}

fn required_text(
    row: &sqlx::mysql::MySqlRow,
    field: &'static str,
) -> Result<String, WorkPatchMaterializationError> {
    row.try_get::<Option<String>, _>(field)?
        .ok_or_else(|| repair(format!("missing {field}")))
}

fn repair(message: String) -> WorkPatchMaterializationError {
    WorkPatchMaterializationError::NeedsRepair(message)
}

fn decode_operation(
    row: &sqlx::mysql::MySqlRow,
) -> Result<WorkPatchMaterializationOperation, WorkPatchMaterializationError> {
    let text = |field: &'static str| row.try_get::<String, _>(field);
    let revision = |field: &'static str| row.try_get::<i64, _>(field);
    let operation = WorkPatchMaterializationOperation {
        schema_version: WORK_PATCH_MATERIALIZATION_SCHEMA_VERSION,
        operation_id: WorkPatchMaterializationId::parse(text("operation_id")?)
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
        target_branch_revision: WorkBranchRevision::new(revision("target_branch_revision")?)
            .map_err(|error| repair(error.to_string()))?,
        target_graph_revision: GraphRevision::new(revision("target_graph_revision")?)
            .map_err(|error| repair(error.to_string()))?,
        target_subject_record_revision: WorkBranchSubjectRevision::new(revision(
            "target_subject_record_revision",
        )?)
        .map_err(|error| repair(error.to_string()))?,
        subject_ref: WorkSubjectRef::parse(text("subject_ref")?)
            .map_err(|error| repair(error.to_string()))?,
        base_subject_revision: WorkContentHash::parse(text("base_subject_revision")?)
            .map_err(|error| repair(error.to_string()))?,
        result_subject_revision: WorkContentHash::parse(text("result_subject_revision")?)
            .map_err(|error| repair(error.to_string()))?,
        payload_hash: WorkContentHash::parse(text("payload_hash")?)
            .map_err(|error| repair(error.to_string()))?,
        provider_ref: WorkMaterializationProviderRef::parse(text("provider_ref")?)
            .map_err(|error| repair(error.to_string()))?,
        policy_decision_ref: WorkChangeRef::parse(text("policy_decision_ref")?)
            .map_err(|error| repair(error.to_string()))?,
        state: WorkPatchMaterializationState::from_persisted(&text("operation_state")?)
            .ok_or_else(|| repair("unknown operation state".into()))?,
        phase: WorkPatchMaterializationPhase::from_persisted(&text("operation_phase")?)
            .ok_or_else(|| repair("unknown operation phase".into()))?,
        apply_invocation_ref: row
            .try_get::<Option<String>, _>("apply_invocation_ref")?
            .map(WorkProviderInvocationRef::parse)
            .transpose()
            .map_err(|error| repair(error.to_string()))?,
        observed_subject_revision: row
            .try_get::<Option<String>, _>("observed_subject_revision")?
            .map(WorkContentHash::parse)
            .transpose()
            .map_err(|error| repair(error.to_string()))?,
        apply_outcome: row
            .try_get::<Option<String>, _>("apply_outcome")?
            .map(|value| {
                WorkPatchMaterializationApplyOutcome::from_persisted(&value)
                    .ok_or_else(|| repair("unknown apply outcome".into()))
            })
            .transpose()?,
        failure_code: row
            .try_get::<Option<String>, _>("failure_code")?
            .map(|value| {
                WorkPatchMaterializationFailureCode::from_persisted(&value)
                    .ok_or_else(|| repair("unknown materialization failure code".into()))
            })
            .transpose()?,
        verification_evidence_hash: row
            .try_get::<Option<String>, _>("verification_evidence_hash")?
            .map(WorkContentHash::parse)
            .transpose()
            .map_err(|error| repair(error.to_string()))?,
        verification_outcome: row
            .try_get::<Option<String>, _>("verification_outcome")?
            .map(|value| {
                WorkPatchMaterializationVerificationOutcome::from_persisted(&value)
                    .ok_or_else(|| repair("unknown verification outcome".into()))
            })
            .transpose()?,
        created_at: super::repository::decode_timestamp(
            "Work patch materialization",
            "created_at",
            text("operation_created_at")?,
        )
        .map_err(|error| repair(error.to_string()))?,
        completed_at: row
            .try_get::<Option<String>, _>("operation_completed_at")?
            .map(|value| {
                super::repository::decode_timestamp(
                    "Work patch materialization",
                    "completed_at",
                    value,
                )
                .map_err(|error| repair(error.to_string()))
            })
            .transpose()?,
    };
    let lifecycle_is_coherent = if operation.state == WorkPatchMaterializationState::Pending {
        operation.phase != WorkPatchMaterializationPhase::Complete
            && operation.completed_at.is_none()
    } else {
        operation.phase == WorkPatchMaterializationPhase::Complete
            && operation.completed_at.is_some()
    };
    let verification_is_coherent = match operation.verification_outcome {
        None => operation.verification_evidence_hash.is_none(),
        Some(WorkPatchMaterializationVerificationOutcome::Passed) => {
            operation.verification_evidence_hash.is_some()
                && operation.state == WorkPatchMaterializationState::Succeeded
                && operation.apply_outcome == Some(WorkPatchMaterializationApplyOutcome::Applied)
        }
        Some(WorkPatchMaterializationVerificationOutcome::TargetChanged) => {
            operation.verification_evidence_hash.is_none()
                && operation.state == WorkPatchMaterializationState::Conflict
        }
    };
    if operation.base_subject_revision == operation.result_subject_revision
        || !lifecycle_is_coherent
        || operation.phase == WorkPatchMaterializationPhase::AwaitingDispatch
            && (operation.apply_invocation_ref.is_some()
                || operation.observed_subject_revision.is_some()
                || operation.apply_outcome.is_some()
                || operation.failure_code.is_some())
        || matches!(
            operation.phase,
            WorkPatchMaterializationPhase::Applying | WorkPatchMaterializationPhase::Reconciling
        ) && (operation.apply_invocation_ref.is_none()
            || operation.observed_subject_revision.is_some()
            || operation.apply_outcome.is_some()
            || operation.failure_code.is_some())
        || operation.phase == WorkPatchMaterializationPhase::Verifying
            && (operation.apply_invocation_ref.is_none()
                || operation.observed_subject_revision.is_none()
                || operation.apply_outcome != Some(WorkPatchMaterializationApplyOutcome::Applied)
                || operation.failure_code.is_some())
        || operation.apply_outcome == Some(WorkPatchMaterializationApplyOutcome::NotApplied)
            && (operation.state != WorkPatchMaterializationState::Failed
                || operation.observed_subject_revision.is_some()
                || operation.failure_code.is_none())
        || matches!(
            operation.apply_outcome,
            Some(
                WorkPatchMaterializationApplyOutcome::Applied
                    | WorkPatchMaterializationApplyOutcome::ResultMismatch
                    | WorkPatchMaterializationApplyOutcome::TargetChanged
            )
        ) && (operation.apply_invocation_ref.is_none()
            || operation.observed_subject_revision.is_none()
            || operation.failure_code.is_some())
        || operation.apply_outcome.is_none()
            && operation.state != WorkPatchMaterializationState::Pending
            && operation.state != WorkPatchMaterializationState::Aborted
        || operation.state == WorkPatchMaterializationState::Aborted
            && (operation.apply_invocation_ref.is_some()
                || operation.observed_subject_revision.is_some()
                || operation.apply_outcome.is_some()
                || operation.failure_code.is_some())
        || !verification_is_coherent
        || operation.state == WorkPatchMaterializationState::Succeeded
            && operation.verification_outcome
                != Some(WorkPatchMaterializationVerificationOutcome::Passed)
        || operation
            .completed_at
            .is_some_and(|completed_at| completed_at < operation.created_at)
    {
        return Err(repair("incoherent materialization lifecycle facts".into()));
    }
    Ok(operation)
}

fn request_digest(request: &WorkPatchMaterializationRequest) -> String {
    let mut hasher = Sha256::new();
    for value in [
        request.owner_id.as_str(),
        request.work_id.as_str(),
        request.request_id.as_str(),
        request.patch_artifact_id.as_str(),
        request.target_branch_id.as_str(),
        request.provider_ref.as_str(),
        request.policy_decision_ref.as_str(),
    ] {
        hasher.update(value.len().to_be_bytes());
        hasher.update(value.as_bytes());
    }
    for revision in [
        request.expected_target_branch_revision.get(),
        request.expected_target_graph_revision.get(),
    ] {
        hasher.update(revision.to_be_bytes());
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_digest_covers_policy_provider_and_every_public_target_basis() {
        let request = WorkPatchMaterializationRequest {
            owner_id: WorkOwnerId::parse("owner").expect("owner"),
            work_id: WorkId::parse("work").expect("work"),
            request_id: WorkChangeRef::parse("request").expect("request"),
            patch_artifact_id: WorkPatchArtifactId::parse("patch").expect("patch"),
            target_branch_id: WorkBranchId::parse("target").expect("target"),
            expected_target_branch_revision: WorkBranchRevision::INITIAL,
            expected_target_graph_revision: GraphRevision::INITIAL,
            provider_ref: WorkMaterializationProviderRef::parse("edge-1").expect("provider"),
            policy_decision_ref: WorkChangeRef::parse("policy-1").expect("policy"),
        };
        let first = request_digest(&request);
        let mut changed = request;
        changed.policy_decision_ref = WorkChangeRef::parse("policy-2").expect("policy");
        assert_ne!(first, request_digest(&changed));
    }

    #[test]
    fn materialization_page_limit_is_strictly_bounded() {
        assert!(WorkPatchMaterializationPageLimit::new(1).is_ok());
        assert!(
            WorkPatchMaterializationPageLimit::new(WORK_PATCH_MATERIALIZATION_PAGE_MAX_ITEMS)
                .is_ok()
        );
        assert!(WorkPatchMaterializationPageLimit::new(0).is_err());
        assert!(
            WorkPatchMaterializationPageLimit::new(WORK_PATCH_MATERIALIZATION_PAGE_MAX_ITEMS + 1)
                .is_err()
        );
    }
}
