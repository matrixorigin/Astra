//! Durable storage for immutable Work recovery points.
//!
//! Canonical capture verifies the immutable Work/Session basis and current
//! execution identity in one transaction. A `captured` point is logical
//! progress, not a promise that files or an unfinished Run can be restored.
//! Workspace publication verifies its sealed Artifact and links its retention
//! edge in that same transaction; the published point remains `captured`.

use astra_core::SharedPool;
use astra_turn_types::{
    RecoveryPointArtifactReferenceV1, RecoveryPointBindingStateV1, RecoveryPointExecutionBindingV1,
    RecoveryPointExecutorKindV1, RecoveryPointManifestV1, RecoveryPointReasonV1,
    RecoveryPointWorkspaceReferenceV1, SessionKeyV1,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{MySql, QueryBuilder, Row, query};

use crate::runs::durable_run_status_blocks_session;
use crate::session_context_coordinator::{
    SessionContextCoordinatorError, SessionExecutionBindingStateV1, SessionExecutionBindingV1,
    lock_recovery_context_in_transaction,
};

use super::events::{NewWorkEvent, WorkEventKind};
use super::plan_context_repository::load_recovery_basis_in_transaction;
use super::repository::{DatabaseWorkRepository, WorkConflictResource, WorkRepositoryError};
use super::{WorkBranchId, WorkChangeRef, WorkContentHash, WorkId, WorkOwnerId};

pub const WORK_RECOVERY_POINT_SCHEMA_VERSION: u32 = 1;
const RECOVERY_POINT_ID_MAX_BYTES: usize = 128;
const RECOVERY_POINT_PAGE_MAX_ITEMS: u16 = 256;
const REQUEST_HASH_SCHEMA_VERSION: u16 = 1;
const RECOVERY_POINT_SELECT_SQL: &str =
    "SELECT owner_id, work_id, branch_id, recovery_point_id, request_id,
            request_hash, status, manifest_json, manifest_hash, failure_reason,
            DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at,
            DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS updated_at,
            DATE_FORMAT(ready_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS ready_at
     FROM work_recovery_points";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkRecoveryPointStatus {
    Preparing,
    Captured,
    Ready,
    Failed,
    Aborted,
}

impl WorkRecoveryPointStatus {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "preparing" => Some(Self::Preparing),
            "captured" => Some(Self::Captured),
            "ready" => Some(Self::Ready),
            "failed" => Some(Self::Failed),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }
}

/// A request to record a canonical, between-Run progress boundary.
///
/// The caller supplies only stable request identity and the revisions it saw;
/// the manifest is built from authoritative rows inside one transaction. This
/// prevents a client from claiming that a workspace, Run, or Artifact is
/// restorable when no such payload has been verified.
#[derive(Debug, Clone)]
pub struct WorkRecoveryPointCaptureRequest {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub request_id: WorkChangeRef,
    pub expected_work_revision: u64,
    pub expected_branch_revision: u64,
    pub reason: RecoveryPointReasonV1,
}

/// A canonical capture request that includes one already sealed and verified
/// workspace package. The package is still only a portable file restore
/// capability; it does not make an unfinished Run or conversation restorable.
#[derive(Debug, Clone)]
pub struct WorkWorkspaceRecoveryPointCaptureRequest {
    pub base: WorkRecoveryPointCaptureRequest,
    pub workspace: RecoveryPointWorkspaceReferenceV1,
    pub artifact: RecoveryPointArtifactReferenceV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkRecoveryPointRecord {
    pub schema_version: u32,
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub recovery_point_id: String,
    pub request_id: WorkChangeRef,
    pub request_hash: WorkContentHash,
    pub status: WorkRecoveryPointStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<RecoveryPointManifestV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_hash: Option<WorkContentHash>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkRecoveryPointQuery {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: Option<WorkBranchId>,
    pub before: Option<WorkRecoveryPointCursor>,
    pub limit: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkRecoveryPointCursor {
    pub created_at: DateTime<Utc>,
    pub recovery_point_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkRecoveryPointPage {
    pub records: Vec<WorkRecoveryPointRecord>,
    pub next_cursor: Option<WorkRecoveryPointCursor>,
}

impl WorkRecoveryPointQuery {
    pub fn new(owner_id: WorkOwnerId, work_id: WorkId) -> Self {
        Self {
            owner_id,
            work_id,
            branch_id: None,
            before: None,
            limit: 32,
        }
    }

    pub fn branch(mut self, branch_id: WorkBranchId) -> Self {
        self.branch_id = Some(branch_id);
        self
    }

    pub fn before(mut self, before: WorkRecoveryPointCursor) -> Self {
        self.before = Some(before);
        self
    }

    pub fn limit(mut self, limit: u16) -> Self {
        self.limit = limit;
        self
    }
}

#[derive(Clone, Debug)]
pub struct DatabaseWorkRecoveryPointRepository {
    pool: SharedPool,
}

impl DatabaseWorkRecoveryPointRepository {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    /// Capture the current canonical Work/Session boundary atomically.
    ///
    /// There is no asynchronous payload upload in this path. A single
    /// transaction prevents a retry from observing a manifest generated from
    /// a different clock or context head between separate commits.
    /// The request fingerprint contains only stable caller parameters. A
    /// replay therefore returns the original immutable point even after the
    /// Work has advanced.
    pub async fn capture_canonical(
        &self,
        request: WorkRecoveryPointCaptureRequest,
    ) -> Result<WorkRecoveryPointRecord, WorkRepositoryError> {
        self.capture_canonical_inner(request, None, None).await
    }

    /// Publish a new immutable recovery point whose workspace package has
    /// already been streamed, sealed, and verified by the caller. The
    /// artifact reachability edge is added in the same transaction as the
    /// recovery row and Work event, so retention GC cannot observe a published
    /// point without its bytes.
    pub async fn capture_workspace_canonical(
        &self,
        request: WorkWorkspaceRecoveryPointCaptureRequest,
    ) -> Result<WorkRecoveryPointRecord, WorkRepositoryError> {
        self.capture_canonical_inner(
            request.base,
            Some(request.workspace),
            Some(request.artifact),
        )
        .await
    }

    async fn capture_canonical_inner(
        &self,
        request: WorkRecoveryPointCaptureRequest,
        workspace: Option<RecoveryPointWorkspaceReferenceV1>,
        artifact: Option<RecoveryPointArtifactReferenceV1>,
    ) -> Result<WorkRecoveryPointRecord, WorkRepositoryError> {
        validate_capture_request(&request)?;
        if workspace.is_some() != artifact.is_some() {
            return Err(WorkRepositoryError::corrupt(
                "Work workspace recovery capture",
                std::io::Error::other(
                    "workspace and artifact references must be provided together",
                ),
            ));
        }
        let request_hash = if workspace.is_none() {
            capture_request_hash(&request)?
        } else {
            capture_request_hash_with_workspace(&request, workspace.as_ref(), artifact.as_ref())?
        };
        let recovery_point_id = capture_recovery_point_id(&request_hash)?;

        let mut transaction = self.pool.get().begin().await.map_err(|source| {
            WorkRepositoryError::persistence("begin canonical Work recovery capture", source)
        })?;

        // Promotion of an existing Session takes the Session authority before
        // inserting Work. Read the branch's Session identity without a lock,
        // then acquire Session → slot/Run → Work/branch in the same order so a
        // concurrent promotion cannot form a Work↔Session lock cycle.
        let branch_session_id = query(
            "SELECT b.session_id
             FROM works w
             INNER JOIN work_branches b
               ON b.owner_id = w.owner_id AND b.work_id = w.work_id
              AND b.branch_id = ?
             WHERE w.owner_id = ? AND w.work_id = ?
             LIMIT 1",
        )
        .bind(request.branch_id.as_str())
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("read canonical Work recovery Session", source)
        })?
        .ok_or(WorkRepositoryError::NotFound)?
        .try_get::<String, _>("session_id")
        .map_err(|source| WorkRepositoryError::corrupt("Work recovery basis", source))?;
        if branch_session_id.is_empty() {
            return Err(WorkRepositoryError::corrupt(
                "Work recovery basis",
                std::io::Error::other("branch has no Session identity"),
            ));
        }

        // The execution slot is the canonical between-Run fence. It is locked
        // before Work/branch rows, matching promotion's Session-first order.
        crate::storage::admit_session_execution_write(
            &mut transaction,
            &branch_session_id,
            request.owner_id.as_str(),
        )
        .await
        .map_err(|source| {
            if matches!(source, sqlx::Error::RowNotFound) {
                WorkRepositoryError::RecoveryPointNotCapturable {
                    reason: super::repository::WorkRecoveryPointBlocker::SessionUnavailable,
                }
            } else {
                WorkRepositoryError::persistence("lock Work Session execution authority", source)
            }
        })?;

        let branch_row = query(
            "SELECT b.session_id, b.branch_revision,
                    b.deletion_operation_id,
                    CASE WHEN b.archived_at IS NULL THEN 0 ELSE 1 END AS branch_archived,
                    CASE WHEN w.archived_at IS NULL THEN 0 ELSE 1 END AS work_archived
             FROM works w
             INNER JOIN work_branches b
               ON b.owner_id = w.owner_id AND b.work_id = w.work_id
              AND b.branch_id = ?
             WHERE w.owner_id = ? AND w.work_id = ?
             LIMIT 1
             FOR UPDATE",
        )
        .bind(request.branch_id.as_str())
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("lock canonical Work recovery basis", source)
        })?
        .ok_or(WorkRepositoryError::NotFound)?;
        let locked_branch_session_id = branch_row
            .try_get::<String, _>("session_id")
            .map_err(|source| WorkRepositoryError::corrupt("Work recovery basis", source))?;
        if locked_branch_session_id != branch_session_id {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: super::repository::WorkRecoveryPointBlocker::SessionChanged,
            });
        }

        // The request row is checked before any mutable-state admission. This
        // is the idempotency boundary after a lost HTTP response.
        if let Some(row) = query(sqlx::AssertSqlSafe(format!(
            "{RECOVERY_POINT_SELECT_SQL}
                 WHERE owner_id = ? AND work_id = ? AND request_id = ?
                 LIMIT 1 FOR UPDATE"
        )))
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.request_id.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("load canonical recovery request", source)
        })? {
            let stored_hash = row
                .try_get::<String, _>("request_hash")
                .map_err(|source| WorkRepositoryError::corrupt("Work recovery point", source))?;
            if stored_hash != request_hash.as_str() {
                return Err(WorkRepositoryError::Conflict {
                    resource: WorkConflictResource::RecoveryPointRequest,
                });
            }
            let record = decode_record(row)?;
            transaction.commit().await.map_err(|source| {
                WorkRepositoryError::persistence("commit idempotent recovery capture", source)
            })?;
            return Ok(record);
        }

        if branch_row
            .try_get::<i64, _>("work_archived")
            .map_err(|source| WorkRepositoryError::corrupt("Work recovery basis", source))?
            != 0
            || branch_row
                .try_get::<i64, _>("branch_archived")
                .map_err(|source| WorkRepositoryError::corrupt("Work recovery basis", source))?
                != 0
        {
            return Err(WorkRepositoryError::Archived);
        }
        if branch_row
            .try_get::<Option<String>, _>("deletion_operation_id")
            .map_err(|source| WorkRepositoryError::corrupt("Work recovery basis", source))?
            .is_some()
        {
            return Err(WorkRepositoryError::BranchDeleting);
        }

        let basis = load_recovery_basis_in_transaction(
            &mut transaction,
            &request.owner_id,
            &request.work_id,
            &request.branch_id,
        )
        .await?;
        if u64::try_from(basis.work_revision.get()).ok() != Some(request.expected_work_revision)
            || u64::try_from(basis.branch_revision.get()).ok()
                != Some(request.expected_branch_revision)
        {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: super::repository::WorkRecoveryPointBlocker::BasisChanged,
            });
        }
        if basis.branch_goal_revision != basis.goal_revision
            || basis.branch_criteria_set_revision != basis.criteria_set_revision
        {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: super::repository::WorkRecoveryPointBlocker::BranchBasisChanged,
            });
        }
        if branch_session_id.is_empty() {
            return Err(WorkRepositoryError::corrupt(
                "Work recovery basis",
                std::io::Error::other("branch has no Session identity"),
            ));
        }

        // The Session/slot lock above prevents a new Run from racing this
        // capture. Inspect the slot owner while that fence is held.
        let active_run = query(
            "SELECT r.run_id, r.status, r.waiting_for, r.parent_run_id,
                    r.work_id, r.work_branch_id
             FROM agent_session_execution_slots slot
             LEFT JOIN agent_runs r
               ON r.user_id = slot.user_id AND r.run_id = slot.run_id
             WHERE slot.user_id = ? AND slot.session_id = ?
             LIMIT 1
             FOR UPDATE",
        )
        .bind(request.owner_id.as_str())
        .bind(&branch_session_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|source| WorkRepositoryError::persistence("check active Work Run", source))?;
        if let Some(run) = active_run {
            let run_id = run
                .try_get::<Option<String>, _>("run_id")
                .map_err(|source| WorkRepositoryError::corrupt("Work recovery Run", source))?
                .ok_or(WorkRepositoryError::RecoveryPointNotCapturable {
                    reason: super::repository::WorkRecoveryPointBlocker::DanglingRunSlot,
                })?;
            let status = run
                .try_get::<Option<String>, _>("status")
                .map_err(|source| WorkRepositoryError::corrupt("Work recovery Run", source))?
                .ok_or(WorkRepositoryError::RecoveryPointNotCapturable {
                    reason: super::repository::WorkRecoveryPointBlocker::RunStatusMissing,
                })?;
            let waiting_for = run
                .try_get::<Option<String>, _>("waiting_for")
                .map_err(|source| WorkRepositoryError::corrupt("Work recovery Run", source))?;
            let work_id = run
                .try_get::<Option<String>, _>("work_id")
                .map_err(|source| WorkRepositoryError::corrupt("Work recovery Run", source))?;
            let work_branch_id = run
                .try_get::<Option<String>, _>("work_branch_id")
                .map_err(|source| WorkRepositoryError::corrupt("Work recovery Run", source))?;
            let parent_run_id = run
                .try_get::<Option<String>, _>("parent_run_id")
                .map_err(|source| WorkRepositoryError::corrupt("Work recovery Run", source))?;
            if parent_run_id.is_some()
                || work_id.as_deref() != Some(request.work_id.as_str())
                || work_branch_id.as_deref() != Some(request.branch_id.as_str())
            {
                return Err(WorkRepositoryError::corrupt(
                    "Work recovery Run",
                    std::io::Error::other("execution slot owner is not this Work root Run"),
                ));
            }
            if durable_run_status_blocks_session(&status, waiting_for.as_deref()) {
                return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                    reason: super::repository::WorkRecoveryPointBlocker::ActiveRun,
                });
            }
            tracing::debug!(
                run_id,
                "ignoring non-blocking terminal Run during recovery capture"
            );
        }

        let key = SessionKeyV1::owner_session(
            "server",
            request.owner_id.as_str(),
            &branch_session_id,
            astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
        );
        let context = lock_recovery_context_in_transaction(&mut transaction, &key)
            .await
            .map_err(map_context_verification_error)?;
        if context.has_active_reservation {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: super::repository::WorkRecoveryPointBlocker::ActiveReservation,
            });
        }
        if context.has_unresolved_invocations {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: super::repository::WorkRecoveryPointBlocker::UnresolvedInvocation,
            });
        }
        let execution = canonical_execution_binding(&key, context.execution_binding.as_ref())?;
        if execution.binding_state != RecoveryPointBindingStateV1::Ready {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: super::repository::WorkRecoveryPointBlocker::ExecutionChanging,
            });
        }
        if let (Some(workspace), Some(artifact)) = (&workspace, &artifact) {
            let current_execution_binding =
                context.execution_binding.clone().unwrap_or_else(|| {
                    SessionExecutionBindingV1::server_work_default(format!(
                        "session:{}:branch:{}",
                        key.session_id, key.branch_id
                    ))
                });
            verify_sealed_workspace_artifact_in_transaction(
                &mut transaction,
                WorkspaceArtifactVerificationInput {
                    owner_id: request.owner_id.as_str(),
                    session_id: &branch_session_id,
                    work_id: request.work_id.as_str(),
                    branch_id: request.branch_id.as_str(),
                    basis: &basis,
                    context: &context,
                    current_execution_binding: &current_execution_binding,
                    workspace,
                    artifact,
                },
            )
            .await?;
        }
        let created_at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let manifest = RecoveryPointManifestV1 {
            schema_version: astra_turn_types::RECOVERY_POINT_MANIFEST_SCHEMA_VERSION,
            recovery_point_id: recovery_point_id.clone(),
            owner_id: request.owner_id.as_str().to_owned(),
            work_id: request.work_id.as_str().to_owned(),
            branch_id: request.branch_id.as_str().to_owned(),
            work_revision: request.expected_work_revision,
            branch_revision: request.expected_branch_revision,
            graph_revision: u64::try_from(basis.graph_revision.get()).map_err(|_| {
                WorkRepositoryError::corrupt(
                    "Work recovery basis",
                    std::io::Error::other("graph revision exceeds recovery manifest width"),
                )
            })?,
            goal_revision: u64::try_from(basis.goal_revision.get()).map_err(|_| {
                WorkRepositoryError::corrupt(
                    "Work recovery basis",
                    std::io::Error::other("goal revision exceeds recovery manifest width"),
                )
            })?,
            criteria_set_revision: u64::try_from(basis.criteria_set_revision.get()).map_err(
                |_| {
                    WorkRepositoryError::corrupt(
                        "Work recovery basis",
                        std::io::Error::other("criteria revision exceeds recovery manifest width"),
                    )
                },
            )?,
            session_key: key,
            session_cursor: context.head.cursor.clone(),
            context_head: context.head,
            run: None,
            execution,
            workspace,
            artifacts: artifact.iter().cloned().collect(),
            environment: Default::default(),
            reason: request.reason,
            created_at,
        };
        manifest
            .validate()
            .map_err(|source| WorkRepositoryError::corrupt("Work recovery manifest", source))?;
        let manifest_hash = manifest_hash(&manifest)?;
        query(
            "INSERT INTO work_recovery_points
             (owner_id, work_id, branch_id, recovery_point_id, request_id,
              request_hash, status, manifest_json, manifest_hash, failure_reason,
              created_at, updated_at, ready_at)
             VALUES (?, ?, ?, ?, ?, ?, 'captured', ?, ?, NULL, NOW(6), NOW(6), NULL)",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .bind(&recovery_point_id)
        .bind(request.request_id.as_str())
        .bind(request_hash.as_str())
        .bind(serde_json::to_string(&manifest).map_err(|source| {
            WorkRepositoryError::ManifestEncoding {
                entity: "Work recovery point manifest",
                source,
            }
        })?)
        .bind(manifest_hash.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::insert(
                "capture canonical Work recovery point",
                WorkConflictResource::RecoveryPointIdentity,
                source,
            )
        })?;
        if let Some(artifact) = &artifact {
            query(
                "INSERT IGNORE INTO session_artifact_references
                 (user_id, session_id, artifact_id, reference_kind, reference_id, created_at)
                 VALUES (?, ?, ?, 'recovery_point', ?, NOW(6))",
            )
            .bind(request.owner_id.as_str())
            .bind(&branch_session_id)
            .bind(artifact.artifact_id.as_str())
            .bind(&recovery_point_id)
            .execute(&mut *transaction)
            .await
            .map_err(|source| {
                WorkRepositoryError::persistence("retain workspace recovery artifact", source)
            })?;
        }
        // Advance the shared Work clock in the same transaction. Web and TUI
        // observers use that clock to invalidate their bounded projections;
        // without this event a newly saved point could remain invisible until
        // an unrelated Work mutation arrives.
        super::events_repository::append_event_with_payload_hash(
            &mut transaction,
            &NewWorkEvent {
                owner_id: request.owner_id.clone(),
                work_id: request.work_id.clone(),
                branch_id: Some(request.branch_id.clone()),
                kind: WorkEventKind::RecoveryPointCaptured,
                work_revision: Some(basis.work_revision),
                goal_revision: Some(basis.goal_revision),
                criterion_set_revision: Some(basis.criteria_set_revision),
                branch_revision: Some(basis.branch_revision),
                graph_revision: Some(basis.graph_revision),
                source_ref: request.request_id.clone(),
            },
            Some(&manifest_hash),
        )
        .await?;
        let row = query(sqlx::AssertSqlSafe(format!(
            "{RECOVERY_POINT_SELECT_SQL}
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND recovery_point_id = ?
             LIMIT 1"
        )))
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .bind(&recovery_point_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("load captured canonical recovery point", source)
        })?;
        let record = decode_record(row)?;
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit canonical Work recovery point", source)
        })?;
        Ok(record)
    }

    pub async fn load(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        recovery_point_id: &str,
    ) -> Result<Option<WorkRecoveryPointRecord>, WorkRepositoryError> {
        let sql = format!(
            "{RECOVERY_POINT_SELECT_SQL} WHERE owner_id = ? AND work_id = ? AND recovery_point_id = ? LIMIT 1"
        );
        let row = query(sqlx::AssertSqlSafe(sql))
            .bind(owner_id.as_str())
            .bind(work_id.as_str())
            .bind(recovery_point_id)
            .fetch_optional(self.pool.get())
            .await
            .map_err(|source| {
                WorkRepositoryError::persistence("load Work recovery point", source)
            })?;
        row.map(decode_record).transpose()
    }

    pub async fn list(
        &self,
        query_value: WorkRecoveryPointQuery,
    ) -> Result<Vec<WorkRecoveryPointRecord>, WorkRepositoryError> {
        Ok(self.list_page(query_value).await?.records)
    }

    pub async fn list_page(
        &self,
        query_value: WorkRecoveryPointQuery,
    ) -> Result<WorkRecoveryPointPage, WorkRepositoryError> {
        if query_value.limit == 0 || query_value.limit > RECOVERY_POINT_PAGE_MAX_ITEMS {
            return Err(WorkRepositoryError::corrupt(
                "Work recovery point query",
                std::io::Error::other(format!(
                    "limit must be between 1 and {RECOVERY_POINT_PAGE_MAX_ITEMS}"
                )),
            ));
        }
        let mut builder = QueryBuilder::<MySql>::new(RECOVERY_POINT_SELECT_SQL);
        let fetch_limit = i64::from(query_value.limit) + 1;
        builder
            .push(" WHERE owner_id = ")
            .push_bind(query_value.owner_id.as_str())
            .push(" AND work_id = ")
            .push_bind(query_value.work_id.as_str());
        if let Some(branch_id) = &query_value.branch_id {
            builder
                .push(" AND branch_id = ")
                .push_bind(branch_id.as_str());
        }
        if let Some(cursor) = query_value.before.as_ref() {
            let cursor_time = cursor.created_at.naive_utc();
            builder
                .push(" AND (created_at < ")
                .push_bind(cursor_time)
                .push(" OR (created_at = ")
                .push_bind(cursor_time)
                .push(" AND recovery_point_id < ")
                .push_bind(cursor.recovery_point_id.as_str())
                .push("))");
        }
        builder
            .push(" ORDER BY created_at DESC, recovery_point_id DESC LIMIT ")
            .push_bind(fetch_limit);
        let mut rows = builder
            .build()
            .fetch_all(self.pool.get())
            .await
            .map_err(|source| {
                WorkRepositoryError::persistence("list Work recovery points", source)
            })?;
        let has_more = rows.len() > usize::from(query_value.limit);
        if has_more {
            rows.pop();
        }
        let records = rows
            .into_iter()
            .map(decode_record)
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = has_more.then(|| {
            let last = records
                .last()
                .expect("a recovery point page with more rows is non-empty");
            WorkRecoveryPointCursor {
                created_at: last.created_at,
                recovery_point_id: last.recovery_point_id.clone(),
            }
        });
        Ok(WorkRecoveryPointPage {
            records,
            next_cursor,
        })
    }
}

impl DatabaseWorkRepository {
    pub fn recovery_points(&self) -> DatabaseWorkRecoveryPointRepository {
        DatabaseWorkRecoveryPointRepository::new(self.pool.clone())
    }
}

struct WorkspaceArtifactVerificationInput<'a> {
    owner_id: &'a str,
    session_id: &'a str,
    work_id: &'a str,
    branch_id: &'a str,
    basis: &'a super::WorkPlanBasis,
    context: &'a crate::session_context_coordinator::RecoveryContextFactsV1,
    current_execution_binding: &'a SessionExecutionBindingV1,
    workspace: &'a RecoveryPointWorkspaceReferenceV1,
    artifact: &'a RecoveryPointArtifactReferenceV1,
}

async fn verify_sealed_workspace_artifact_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::MySql>,
    input: WorkspaceArtifactVerificationInput<'_>,
) -> Result<(), WorkRepositoryError> {
    let WorkspaceArtifactVerificationInput {
        owner_id,
        session_id,
        work_id,
        branch_id,
        basis,
        context,
        current_execution_binding,
        workspace,
        artifact,
    } = input;
    if artifact.artifact_type != "workspace_snapshot_package_v1"
        || !workspace.complete
        || artifact.artifact_id.trim().is_empty()
        || workspace.logical_workspace_id.trim().is_empty()
    {
        return Err(WorkRepositoryError::RecoveryPointNotCapturable {
            reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
        });
    }
    let row = query(
        "SELECT artifact_kind, content_json, CAST(metadata AS CHAR) AS metadata_json, status
         FROM session_artifacts
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?
         LIMIT 1 FOR UPDATE",
    )
    .bind(owner_id)
    .bind(session_id)
    .bind(artifact.artifact_id.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("verify workspace artifact", source))?
    .ok_or(WorkRepositoryError::RecoveryPointNotCapturable {
        reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactUnavailable,
    })?;
    let status = row
        .try_get::<String, _>("status")
        .map_err(|source| WorkRepositoryError::corrupt("workspace artifact", source))?;
    let artifact_kind = row
        .try_get::<String, _>("artifact_kind")
        .map_err(|source| WorkRepositoryError::corrupt("workspace artifact", source))?;
    let metadata = row
        .try_get::<Option<String>, _>("metadata_json")
        .map_err(|source| WorkRepositoryError::corrupt("workspace artifact", source))?
        .and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok());
    let content_json = row
        .try_get::<String, _>("content_json")
        .map_err(|source| WorkRepositoryError::corrupt("workspace artifact", source))?;
    let content: serde_json::Value = serde_json::from_str(&content_json).map_err(|source| {
        WorkRepositoryError::ManifestEncoding {
            entity: "workspace artifact descriptor",
            source,
        }
    })?;
    let descriptor = content.get("content").cloned().ok_or_else(|| {
        WorkRepositoryError::RecoveryPointNotCapturable {
            reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
        }
    })?;
    let descriptor: crate::session_artifact_store::SessionArtifactContentDescriptorV1 =
        serde_json::from_value(descriptor).map_err(|_| {
            WorkRepositoryError::RecoveryPointNotCapturable {
                reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
            }
        })?;
    descriptor
        .validate()
        .map_err(|_| WorkRepositoryError::RecoveryPointNotCapturable {
            reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
        })?;
    let manifest_value = content.get("manifest").cloned().ok_or_else(|| {
        WorkRepositoryError::RecoveryPointNotCapturable {
            reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
        }
    })?;
    let package_manifest: astra_runtime_env::WorkspaceSnapshotManifestV1 =
        serde_json::from_value(manifest_value).map_err(|_| {
            WorkRepositoryError::RecoveryPointNotCapturable {
                reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
            }
        })?;
    package_manifest
        .validate()
        .map_err(|_| WorkRepositoryError::RecoveryPointNotCapturable {
            reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
        })?;
    let package_manifest_hash = package_manifest.content_hash().map_err(|_| {
        WorkRepositoryError::RecoveryPointNotCapturable {
            reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
        }
    })?;
    let sealed = descriptor.sealed;
    let digest = descriptor.digest.as_str();
    let byte_size = descriptor.byte_size;
    let context_head_matches = serde_json::to_value(&context.head)
        .ok()
        .and_then(|expected| {
            metadata
                .as_ref()
                .and_then(|value| value.get("context_head"))
                .map(|actual| actual == &expected)
        })
        == Some(true);
    let execution_binding_matches = serde_json::to_value(current_execution_binding)
        .ok()
        .and_then(|expected| {
            metadata
                .as_ref()
                .and_then(|value| value.get("execution_binding"))
                .map(|actual| actual == &expected)
        })
        == Some(true);
    let basis_metadata_matches = metadata
        .as_ref()
        .and_then(|value| value.get("work_revision"))
        .and_then(serde_json::Value::as_u64)
        == u64::try_from(basis.work_revision.get()).ok()
        && metadata
            .as_ref()
            .and_then(|value| value.get("branch_revision"))
            .and_then(serde_json::Value::as_u64)
            == u64::try_from(basis.branch_revision.get()).ok()
        && metadata
            .as_ref()
            .and_then(|value| value.get("graph_revision"))
            .and_then(serde_json::Value::as_u64)
            == u64::try_from(basis.graph_revision.get()).ok();
    let metadata_matches = metadata
        .as_ref()
        .and_then(|value| value.get("logical_workspace_id"))
        .and_then(serde_json::Value::as_str)
        == Some(workspace.logical_workspace_id.as_str())
        && metadata
            .as_ref()
            .and_then(|value| value.get("manifest_hash"))
            .and_then(serde_json::Value::as_str)
            == Some(workspace.manifest_hash.as_str())
        && metadata
            .as_ref()
            .and_then(|value| value.get("work_id"))
            .and_then(serde_json::Value::as_str)
            == Some(work_id)
        && metadata
            .as_ref()
            .and_then(|value| value.get("branch_id"))
            .and_then(serde_json::Value::as_str)
            == Some(branch_id)
        && context_head_matches
        && execution_binding_matches
        && basis_metadata_matches
        && artifact.location_ref.as_deref()
            == Some(&format!("session-artifact:{}", artifact.artifact_id));
    let package_identity_matches = artifact_kind == "workspace_snapshot_package_v1"
        && status != "expired"
        && sealed
        && descriptor.schema_version
            == crate::session_artifact_store::SESSION_ARTIFACT_CONTENT_SCHEMA_VERSION
        && descriptor.backend
            == crate::session_artifact_store::SESSION_ARTIFACT_CONTENT_BACKEND_MATRIXONE_CHUNKS_V1
        && digest == artifact.digest.as_str()
        && byte_size == workspace.byte_size
        && package_manifest.snapshot_id == workspace.snapshot_id
        && package_manifest.logical_workspace_id == workspace.logical_workspace_id
        && package_manifest_hash == workspace.manifest_hash
        && package_manifest.content.content_root == workspace.content_root
        && package_manifest.content.total_bytes >= workspace.byte_size;
    if !package_identity_matches {
        return Err(WorkRepositoryError::RecoveryPointNotCapturable {
            reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
        });
    }

    // The package is immutable after begin/seal.  If the current canonical
    // Work/Session boundary moved while it was being uploaded, retrying the
    // old capture can never make that package represent the new boundary.
    // Return the existing typed drift blockers so HTTP can tell the caller to
    // refresh the basis and capture a new package instead of retrying forever.
    let has_basis_metadata = metadata.as_ref().is_some_and(|value| {
        value
            .get("work_revision")
            .and_then(serde_json::Value::as_u64)
            .is_some()
            && value
                .get("branch_revision")
                .and_then(serde_json::Value::as_u64)
                .is_some()
            && value
                .get("graph_revision")
                .and_then(serde_json::Value::as_u64)
                .is_some()
    });
    let has_context_metadata = metadata
        .as_ref()
        .is_some_and(|value| value.get("context_head").is_some());
    let has_execution_metadata = metadata
        .as_ref()
        .is_some_and(|value| value.get("execution_binding").is_some());
    if has_basis_metadata && !basis_metadata_matches {
        return Err(WorkRepositoryError::RecoveryPointNotCapturable {
            reason: super::repository::WorkRecoveryPointBlocker::BasisChanged,
        });
    }
    if has_context_metadata && !context_head_matches {
        return Err(WorkRepositoryError::RecoveryPointNotCapturable {
            reason: super::repository::WorkRecoveryPointBlocker::ContextChanged,
        });
    }
    if has_execution_metadata && !execution_binding_matches {
        return Err(WorkRepositoryError::RecoveryPointNotCapturable {
            reason: super::repository::WorkRecoveryPointBlocker::ExecutionChanged,
        });
    }
    if !metadata_matches {
        return Err(WorkRepositoryError::RecoveryPointNotCapturable {
            reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
        });
    }

    // Re-verify the typed payload at the canonical publication boundary. A
    // generic sealed byte artifact may carry the right catalog metadata while
    // containing a different manifest or a different ordered set of blobs;
    // metadata alone is therefore never a portable-workspace proof.
    let mut layout = std::collections::BTreeMap::<String, (String, u64)>::new();
    let mut digest_to_ref = std::collections::BTreeMap::<String, (String, u64)>::new();
    for entry in &package_manifest.entries {
        let (Some(blob_ref), Some(entry_digest)) = (&entry.blob_ref, &entry.digest) else {
            continue;
        };
        if let Some((existing_digest, existing_size)) = layout.get(blob_ref) {
            if existing_digest != entry_digest || *existing_size != entry.size {
                return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                    reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
                });
            }
        } else {
            if let Some((existing_ref, _)) =
                digest_to_ref.insert(entry_digest.clone(), (blob_ref.clone(), entry.size))
                && existing_ref != *blob_ref
            {
                return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                    reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
                });
            }
            layout.insert(blob_ref.clone(), (entry_digest.clone(), entry.size));
        }
    }
    if descriptor.chunk_count != layout.len() as u64 {
        return Err(WorkRepositoryError::RecoveryPointNotCapturable {
            reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
        });
    }
    // Lock artifact-local references first, then shared content rows through
    // the canonical content-store helper.  The helper sorts shared digest
    // locks globally and restores logical order for hashing, so two
    // publishers that mention the same blobs in reverse file order cannot
    // deadlock each other.
    let chunk_refs = crate::session_artifact_store::load_content_chunk_refs(
        transaction,
        owner_id,
        session_id,
        artifact.artifact_id.as_str(),
    )
    .await
    .map_err(map_workspace_content_verification_error)?;
    if chunk_refs.len() != layout.len() {
        return Err(WorkRepositoryError::RecoveryPointNotCapturable {
            reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
        });
    }
    let stored_chunks = crate::session_artifact_store::load_and_verify_content_chunks(
        transaction,
        owner_id,
        &chunk_refs,
    )
    .await
    .map_err(map_workspace_content_verification_error)?;
    let mut blobs = std::collections::BTreeMap::new();
    let expected_order = layout.values().collect::<Vec<_>>();
    let mut aggregate = Sha256::new();
    let mut aggregate_size = 0_u64;
    for (index, chunk) in stored_chunks.into_iter().enumerate() {
        let chunk_index = chunk.chunk_index;
        let chunk_digest = chunk.digest;
        let bytes = chunk.bytes;
        let Some((expected_digest, expected_size)) = expected_order.get(index) else {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
            });
        };
        let Some((blob_ref, _)) = digest_to_ref.get(&chunk_digest) else {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
            });
        };
        if chunk_index != index as u64
            || chunk_digest != *expected_digest
            || bytes.len() as u64 != *expected_size
        {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
            });
        }
        aggregate.update(&bytes);
        aggregate_size = aggregate_size
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| WorkRepositoryError::RecoveryPointNotCapturable {
                reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
            })?;
        blobs.insert(blob_ref.clone(), bytes);
    }
    if format!("sha256:{:x}", aggregate.finalize()) != descriptor.digest
        || aggregate_size != descriptor.byte_size
    {
        return Err(WorkRepositoryError::RecoveryPointNotCapturable {
            reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
        });
    }
    astra_runtime_env::WorkspaceSnapshotPackage {
        manifest: package_manifest,
        blobs,
    }
    .verify()
    .map_err(|_| WorkRepositoryError::RecoveryPointNotCapturable {
        reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
    })?;
    Ok(())
}

fn map_workspace_content_verification_error(
    error: crate::session_artifact_store::SessionArtifactStoreError,
) -> WorkRepositoryError {
    match error {
        crate::session_artifact_store::SessionArtifactStoreError::Database(source) => {
            WorkRepositoryError::persistence("verify workspace content", source)
        }
        _ => WorkRepositoryError::RecoveryPointNotCapturable {
            reason: super::repository::WorkRecoveryPointBlocker::WorkspaceArtifactInvalid,
        },
    }
}

fn canonical_execution_binding(
    key: &SessionKeyV1,
    current: Option<&SessionExecutionBindingV1>,
) -> Result<RecoveryPointExecutionBindingV1, WorkRepositoryError> {
    let logical_workspace_id = format!("session:{}:branch:{}", key.session_id, key.branch_id);
    let fallback = SessionExecutionBindingV1::server_work_default(logical_workspace_id);
    let current = current.unwrap_or(&fallback);
    let binding_state = match current.state {
        SessionExecutionBindingStateV1::Ready => RecoveryPointBindingStateV1::Ready,
        SessionExecutionBindingStateV1::Switching => RecoveryPointBindingStateV1::Switching,
        SessionExecutionBindingStateV1::NeedsAttention => {
            RecoveryPointBindingStateV1::NeedsAttention
        }
    };
    let (executor_kind, executor_id) = match current.executor.kind {
        crate::runs::ExecutorBindingRequestKind::ServerLocal => {
            (RecoveryPointExecutorKindV1::Server, "server".to_owned())
        }
        crate::runs::ExecutorBindingRequestKind::EdgeAgent => (
            RecoveryPointExecutorKindV1::Edge,
            current.executor.executor_id.clone().ok_or(
                WorkRepositoryError::RecoveryPointNotCapturable {
                    reason: super::repository::WorkRecoveryPointBlocker::EdgeIdentityMissing,
                },
            )?,
        ),
        _ => {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: super::repository::WorkRecoveryPointBlocker::UnsupportedExecutor,
            });
        }
    };
    let mut binding = RecoveryPointExecutionBindingV1 {
        binding_generation: current.generation,
        binding_state,
        logical_workspace_id: current.logical_workspace_id.clone(),
        executor_kind,
        executor_id,
        binding_hash: String::new(),
        physical_workspace_id: current.physical_workspace_id.clone(),
    };
    binding.binding_hash = binding.content_hash();
    Ok(binding)
}

fn map_context_verification_error(error: SessionContextCoordinatorError) -> WorkRepositoryError {
    match error {
        SessionContextCoordinatorError::Database { operation, source } => {
            WorkRepositoryError::persistence(operation, source)
        }
        SessionContextCoordinatorError::DatabaseJson { entity, source } => {
            WorkRepositoryError::ManifestEncoding { entity, source }
        }
        _ => WorkRepositoryError::RecoveryPointNotCapturable {
            reason: super::repository::WorkRecoveryPointBlocker::ContextUnavailable,
        },
    }
}

#[derive(Serialize)]
struct CanonicalCaptureRequestHashInput<'a> {
    schema_version: u16,
    owner_id: &'a WorkOwnerId,
    work_id: &'a WorkId,
    branch_id: &'a WorkBranchId,
    request_id: &'a WorkChangeRef,
    expected_work_revision: u64,
    expected_branch_revision: u64,
    reason: RecoveryPointReasonV1,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<&'a RecoveryPointWorkspaceReferenceV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact: Option<&'a RecoveryPointArtifactReferenceV1>,
}

fn validate_capture_request(
    request: &WorkRecoveryPointCaptureRequest,
) -> Result<(), WorkRepositoryError> {
    if request.expected_work_revision == 0 || request.expected_branch_revision == 0 {
        return Err(WorkRepositoryError::corrupt(
            "Work recovery capture request",
            std::io::Error::other("expected revisions must be positive"),
        ));
    }
    if request.request_id.as_str().is_empty()
        || request
            .request_id
            .as_str()
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(WorkRepositoryError::corrupt(
            "Work recovery capture request",
            std::io::Error::other("request_id is not a safe identity"),
        ));
    }
    Ok(())
}

fn capture_request_hash(
    request: &WorkRecoveryPointCaptureRequest,
) -> Result<WorkContentHash, WorkRepositoryError> {
    let payload = serde_json::to_vec(&CanonicalCaptureRequestHashInput {
        schema_version: REQUEST_HASH_SCHEMA_VERSION,
        owner_id: &request.owner_id,
        work_id: &request.work_id,
        branch_id: &request.branch_id,
        request_id: &request.request_id,
        expected_work_revision: request.expected_work_revision,
        expected_branch_revision: request.expected_branch_revision,
        reason: request.reason,
        workspace: None,
        artifact: None,
    })
    .map_err(|source| WorkRepositoryError::ManifestEncoding {
        entity: "Work recovery capture request",
        source,
    })?;
    WorkContentHash::parse(format!("sha256:{:x}", Sha256::digest(payload))).map_err(|source| {
        WorkRepositoryError::corrupt(
            "Work recovery capture request hash",
            std::io::Error::other(source),
        )
    })
}

fn capture_request_hash_with_workspace(
    request: &WorkRecoveryPointCaptureRequest,
    workspace: Option<&RecoveryPointWorkspaceReferenceV1>,
    artifact: Option<&RecoveryPointArtifactReferenceV1>,
) -> Result<WorkContentHash, WorkRepositoryError> {
    let payload = serde_json::to_vec(&CanonicalCaptureRequestHashInput {
        schema_version: REQUEST_HASH_SCHEMA_VERSION,
        owner_id: &request.owner_id,
        work_id: &request.work_id,
        branch_id: &request.branch_id,
        request_id: &request.request_id,
        expected_work_revision: request.expected_work_revision,
        expected_branch_revision: request.expected_branch_revision,
        reason: request.reason,
        workspace,
        artifact,
    })
    .map_err(|source| WorkRepositoryError::ManifestEncoding {
        entity: "Work workspace recovery capture request",
        source,
    })?;
    WorkContentHash::parse(format!("sha256:{:x}", Sha256::digest(payload))).map_err(|source| {
        WorkRepositoryError::corrupt(
            "Work workspace recovery capture request hash",
            std::io::Error::other(source),
        )
    })
}

fn capture_recovery_point_id(
    request_hash: &WorkContentHash,
) -> Result<String, WorkRepositoryError> {
    let digest = request_hash
        .as_str()
        .strip_prefix("sha256:")
        .ok_or_else(|| {
            WorkRepositoryError::corrupt(
                "Work recovery capture request hash",
                std::io::Error::other("missing sha256 prefix"),
            )
        })?;
    Ok(format!("rp-{digest}"))
}

fn manifest_hash(
    manifest: &RecoveryPointManifestV1,
) -> Result<WorkContentHash, WorkRepositoryError> {
    let value = manifest
        .content_hash()
        .map_err(|source| WorkRepositoryError::corrupt("Work recovery point manifest", source))?;
    WorkContentHash::parse(value).map_err(|source| {
        WorkRepositoryError::corrupt(
            "Work recovery point manifest hash",
            std::io::Error::other(source),
        )
    })
}

fn decode_record(
    row: sqlx::mysql::MySqlRow,
) -> Result<WorkRecoveryPointRecord, WorkRepositoryError> {
    let text = |field: &'static str| {
        row.try_get::<String, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work recovery point", source))
    };
    let optional_text = |field: &'static str| {
        row.try_get::<Option<String>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work recovery point", source))
    };
    let owner_id = WorkOwnerId::parse(text("owner_id")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work recovery point", source))?;
    let work_id = WorkId::parse(text("work_id")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work recovery point", source))?;
    let branch_id = WorkBranchId::parse(text("branch_id")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work recovery point", source))?;
    let request_id = WorkChangeRef::parse(text("request_id")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work recovery point", source))?;
    let status_name = text("status")?;
    let status = WorkRecoveryPointStatus::parse(&status_name).ok_or_else(|| {
        WorkRepositoryError::corrupt(
            "Work recovery point",
            std::io::Error::other("unknown recovery point status"),
        )
    })?;
    let request_hash = WorkContentHash::parse(text("request_hash")?).map_err(|source| {
        WorkRepositoryError::corrupt(
            "Work recovery point request hash",
            std::io::Error::other(source),
        )
    })?;
    let manifest_hash = optional_text("manifest_hash")?
        .map(|value| {
            WorkContentHash::parse(value).map_err(|source| {
                WorkRepositoryError::corrupt(
                    "Work recovery point manifest hash",
                    std::io::Error::other(source),
                )
            })
        })
        .transpose()?;
    let recovery_point_id = text("recovery_point_id")?;
    if recovery_point_id.len() > RECOVERY_POINT_ID_MAX_BYTES
        || recovery_point_id.is_empty()
        || recovery_point_id
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(WorkRepositoryError::corrupt(
            "Work recovery point",
            std::io::Error::other("invalid recovery point identity"),
        ));
    }
    let manifest = optional_text("manifest_json")?
        .map(|value| {
            let manifest: RecoveryPointManifestV1 =
                serde_json::from_str(&value).map_err(|source| {
                    WorkRepositoryError::corrupt("Work recovery point manifest", source)
                })?;
            manifest.validate().map_err(|source| {
                WorkRepositoryError::corrupt("Work recovery point manifest", source)
            })?;
            if manifest.owner_id != owner_id.as_str()
                || manifest.work_id != work_id.as_str()
                || manifest.branch_id != branch_id.as_str()
                || manifest.recovery_point_id != recovery_point_id
            {
                return Err(WorkRepositoryError::corrupt(
                    "Work recovery point manifest",
                    std::io::Error::other("manifest identity does not match storage owner"),
                ));
            }
            if manifest_hash.as_ref().is_none_or(|expected| {
                manifest
                    .content_hash()
                    .ok()
                    .and_then(|hash| WorkContentHash::parse(hash).ok())
                    .as_ref()
                    != Some(expected)
            }) {
                return Err(WorkRepositoryError::corrupt(
                    "Work recovery point manifest",
                    std::io::Error::other("manifest hash does not match manifest content"),
                ));
            }
            Ok(manifest)
        })
        .transpose()?;
    if status == WorkRecoveryPointStatus::Ready && (manifest.is_none() || manifest_hash.is_none()) {
        return Err(WorkRepositoryError::corrupt(
            "Work recovery point",
            std::io::Error::other("ready recovery point has no complete manifest"),
        ));
    }
    let created_at = super::repository::decode_timestamp(
        "Work recovery point",
        "created_at",
        text("created_at")?,
    )?;
    let updated_at = super::repository::decode_timestamp(
        "Work recovery point",
        "updated_at",
        text("updated_at")?,
    )?;
    let ready_at = optional_text("ready_at")?
        .map(|value| super::repository::decode_timestamp("Work recovery point", "ready_at", value))
        .transpose()?;
    Ok(WorkRecoveryPointRecord {
        schema_version: WORK_RECOVERY_POINT_SCHEMA_VERSION,
        owner_id,
        work_id,
        branch_id,
        recovery_point_id,
        request_id,
        request_hash,
        status,
        manifest,
        manifest_hash,
        failure_reason: optional_text("failure_reason")?,
        created_at,
        updated_at,
        ready_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_defaults_to_a_small_user_facing_page() {
        let query = WorkRecoveryPointQuery::new(
            WorkOwnerId::parse("owner").unwrap(),
            WorkId::parse("work").unwrap(),
        );
        assert_eq!(query.limit, 32);
        assert!(query.branch_id.is_none());
    }

    #[test]
    fn captured_status_is_decoded_as_a_logical_boundary() {
        assert_eq!(
            WorkRecoveryPointStatus::parse("captured"),
            Some(WorkRecoveryPointStatus::Captured)
        );
        assert_ne!(
            WorkRecoveryPointStatus::Captured,
            WorkRecoveryPointStatus::Ready
        );
    }

    #[test]
    fn missing_execution_binding_uses_the_canonical_server_default() {
        let key = SessionKeyV1::owner_session("tenant", "owner", "session", "branch");
        let binding = canonical_execution_binding(&key, None).expect("server default binding");
        assert_eq!(binding.executor_kind, RecoveryPointExecutorKindV1::Server);
        assert_eq!(binding.executor_id, "server");
        assert_eq!(
            binding.logical_workspace_id,
            "session:session:branch:branch"
        );
        assert_eq!(binding.binding_state, RecoveryPointBindingStateV1::Ready);
        assert_eq!(binding.binding_hash, binding.content_hash());
    }
}
