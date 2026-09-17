//! Durable storage for immutable Work recovery points.
//!
//! This repository owns the recovery-point record only. Edge/User Runner code
//! captures files and uploads content; this layer validates the declared
//! manifest and stores one owner-scoped, idempotent `preparing` record.
//! A canonical verifier can advance it to `captured` after proving the
//! immutable Work/Session basis and the current execution identity. `captured`
//! is a logical progress boundary, not a promise that code, artifacts, or an
//! unfinished Run can be restored. A future `ready` state requires a separate
//! verifier for those durable payloads and effect receipts.

use astra_core::SharedPool;
use astra_turn_types::{
    RecoveryPointBindingStateV1, RecoveryPointExecutionBindingV1, RecoveryPointExecutorKindV1,
    RecoveryPointManifestV1, RecoveryPointReasonV1, SessionKeyV1,
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

/// A declared capture request.  Recording it is durable progress for an
/// upload, not proof that the capture can be restored.  The repository keeps
/// the status `preparing` until a canonical publication verifier exists.
#[derive(Debug, Clone)]
pub struct NewWorkRecoveryPoint {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub request_id: WorkChangeRef,
    pub manifest: RecoveryPointManifestV1,
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
    /// This is intentionally a single transaction rather than a public
    /// `record_preparing`/`mark_captured` wrapper: there is no asynchronous
    /// payload upload in this path, so exposing two commits would let a retry
    /// observe a manifest generated from a different clock or context head.
    /// The request fingerprint contains only stable caller parameters. A
    /// replay therefore returns the original immutable point even after the
    /// Work has advanced.
    pub async fn capture_canonical(
        &self,
        request: WorkRecoveryPointCaptureRequest,
    ) -> Result<WorkRecoveryPointRecord, WorkRepositoryError> {
        validate_capture_request(&request)?;
        let request_hash = capture_request_hash(&request)?;
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
        if let Some(row) = query(&format!(
            "{RECOVERY_POINT_SELECT_SQL}
                 WHERE owner_id = ? AND work_id = ? AND request_id = ?
                 LIMIT 1 FOR UPDATE"
        ))
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
            workspace: None,
            artifacts: Vec::new(),
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
        let row = query(&format!(
            "{RECOVERY_POINT_SELECT_SQL}
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND recovery_point_id = ?
             LIMIT 1"
        ))
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

    /// Record a declared capture idempotently.  The Work and branch identity
    /// are checked in the same transaction as the insert, while the immutable
    /// manifest hash prevents a request id from being reused for a different
    /// capture.  This deliberately returns `preparing`; it must not expose a
    /// caller-supplied manifest as a restorable `ready` point.
    pub async fn record_preparing(
        &self,
        request: NewWorkRecoveryPoint,
    ) -> Result<WorkRecoveryPointRecord, WorkRepositoryError> {
        validate_request(&request)?;
        let manifest_hash = manifest_hash(&request.manifest)?;
        let request_hash = request_hash(&request, &manifest_hash)?;

        let mut transaction = self.pool.get().begin().await.map_err(|source| {
            WorkRepositoryError::persistence("begin Work recovery point capture", source)
        })?;

        let branch_row = query(
            "SELECT b.session_id
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
            WorkRepositoryError::persistence("lock Work recovery point owner", source)
        })?;
        let Some(branch_row) = branch_row else {
            return Err(WorkRepositoryError::NotFound);
        };
        let branch_session_id = branch_row
            .try_get::<String, _>("session_id")
            .map_err(|source| WorkRepositoryError::corrupt("Work recovery point", source))?;
        if branch_session_id != request.manifest.session_key.session_id {
            return Err(WorkRepositoryError::Conflict {
                resource: WorkConflictResource::RecoveryPointIdentity,
            });
        }

        if let Some(row) = query(
            "SELECT owner_id, work_id, branch_id, recovery_point_id, request_id,
                    request_hash, status, manifest_json, manifest_hash, failure_reason,
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at,
                    DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS updated_at,
                    DATE_FORMAT(ready_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS ready_at
             FROM work_recovery_points
             WHERE owner_id = ? AND work_id = ? AND request_id = ?
             LIMIT 1 FOR UPDATE",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.request_id.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("load Work recovery point request", source)
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
                WorkRepositoryError::persistence("commit idempotent Work recovery point", source)
            })?;
            return Ok(record);
        }

        query(
            "INSERT INTO work_recovery_points
             (owner_id, work_id, branch_id, recovery_point_id, request_id,
              request_hash, status, manifest_json, manifest_hash, failure_reason,
              created_at, updated_at, ready_at)
             VALUES (?, ?, ?, ?, ?, ?, 'preparing', ?, ?, NULL, NOW(6), NOW(6), NULL)",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .bind(request.manifest.recovery_point_id.as_str())
        .bind(request.request_id.as_str())
        .bind(request_hash.as_str())
        .bind(serde_json::to_string(&request.manifest).map_err(|source| {
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
                "record Work recovery point capture",
                WorkConflictResource::RecoveryPointIdentity,
                source,
            )
        })?;

        let row = query(
            "SELECT owner_id, work_id, branch_id, recovery_point_id, request_id,
                    request_hash, status, manifest_json, manifest_hash, failure_reason,
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at,
                    DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS updated_at,
                    DATE_FORMAT(ready_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS ready_at
             FROM work_recovery_points
             WHERE owner_id = ? AND work_id = ? AND recovery_point_id = ?
             LIMIT 1",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.manifest.recovery_point_id.as_str())
        .fetch_one(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("load recorded Work recovery point", source)
        })?;
        let record = decode_record(row)?;
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit Work recovery point capture", source)
        })?;
        Ok(record)
    }

    /// Canonically capture the logical Work boundary represented by a
    /// preparing row. The caller supplies only the row identity; all
    /// authoritative revisions, context-head facts, and execution-binding
    /// identity are read while the Work, branch, recovery row, and Session
    /// context are locked in that order. No filesystem, Artifact, or active
    /// Run claim is made here, so the result is intentionally `captured` and
    /// carries no restore/continue capability by itself.
    pub async fn mark_captured(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        recovery_point_id: &str,
    ) -> Result<WorkRecoveryPointRecord, WorkRepositoryError> {
        if recovery_point_id.is_empty()
            || recovery_point_id.len() > RECOVERY_POINT_ID_MAX_BYTES
            || recovery_point_id
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        {
            return Err(WorkRepositoryError::corrupt(
                "Work recovery point",
                std::io::Error::other("invalid recovery point identity"),
            ));
        }

        let mut transaction = self.pool.get().begin().await.map_err(|source| {
            WorkRepositoryError::persistence("begin Work recovery point capture", source)
        })?;

        // Keep this lock order identical to capture admission and branch
        // deletion: Work/branch first, then the recovery row. Reversing it
        // would allow a publisher and deletion executor to deadlock.
        let branch_row = query(
            "SELECT b.session_id, b.branch_revision, b.goal_revision_ref,
                    b.criteria_set_revision_ref, b.current_graph_revision,
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
        .bind(branch_id.as_str())
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("lock Work recovery capture basis", source)
        })?
        .ok_or(WorkRepositoryError::NotFound)?;
        let branch_session_id =
            branch_row
                .try_get::<String, _>("session_id")
                .map_err(|source| {
                    WorkRepositoryError::corrupt("Work recovery capture basis", source)
                })?;
        let branch_archived =
            branch_row
                .try_get::<i64, _>("branch_archived")
                .map_err(|source| {
                    WorkRepositoryError::corrupt("Work recovery capture basis", source)
                })?
                != 0;
        let work_archived = branch_row
            .try_get::<i64, _>("work_archived")
            .map_err(|source| {
                WorkRepositoryError::corrupt("Work recovery capture basis", source)
            })?
            != 0;
        if work_archived || branch_archived {
            return Err(WorkRepositoryError::Archived);
        }
        if branch_row
            .try_get::<Option<String>, _>("deletion_operation_id")
            .map_err(|source| WorkRepositoryError::corrupt("Work recovery capture basis", source))?
            .is_some()
        {
            return Err(WorkRepositoryError::BranchDeleting);
        }

        let recovery_row = query(&format!(
            "{RECOVERY_POINT_SELECT_SQL}
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND recovery_point_id = ?
             LIMIT 1 FOR UPDATE"
        ))
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(recovery_point_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|source| WorkRepositoryError::persistence("lock Work recovery point", source))?
        .ok_or(WorkRepositoryError::NotFound)?;
        let record = decode_record(recovery_row)?;
        if record.status == WorkRecoveryPointStatus::Captured {
            transaction.commit().await.map_err(|source| {
                WorkRepositoryError::persistence("commit idempotent Work recovery capture", source)
            })?;
            return Ok(record);
        }
        if record.status != WorkRecoveryPointStatus::Preparing {
            return Err(WorkRepositoryError::Conflict {
                resource: WorkConflictResource::RecoveryPointIdentity,
            });
        }
        let manifest = record.manifest.as_ref().ok_or_else(|| {
            WorkRepositoryError::corrupt(
                "Work recovery point",
                std::io::Error::other("preparing recovery point has no manifest"),
            )
        })?;
        if manifest.session_key.session_id != branch_session_id {
            return Err(WorkRepositoryError::Conflict {
                resource: WorkConflictResource::RecoveryPointIdentity,
            });
        }

        let basis =
            load_recovery_basis_in_transaction(&mut transaction, owner_id, work_id, branch_id)
                .await?;
        if !revisions_match_manifest(&basis, manifest) {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: super::repository::WorkRecoveryPointBlocker::BasisChanged,
            });
        }
        if manifest.run.is_some() {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: super::repository::WorkRecoveryPointBlocker::RunFrontierUnavailable,
            });
        }

        let context = lock_recovery_context_in_transaction(&mut transaction, &manifest.session_key)
            .await
            .map_err(map_context_verification_error)?;
        if context.head != manifest.context_head || context.head.cursor != manifest.session_cursor {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: super::repository::WorkRecoveryPointBlocker::ContextChanged,
            });
        }
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
        let current_execution =
            canonical_execution_binding(&manifest.session_key, context.execution_binding.as_ref())?;
        if current_execution != manifest.execution {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: super::repository::WorkRecoveryPointBlocker::ExecutionChanged,
            });
        }

        let updated = query(
            "UPDATE work_recovery_points
             SET status = 'captured', updated_at = NOW(6), failure_reason = NULL, ready_at = NULL
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND recovery_point_id = ? AND status = 'preparing'",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(recovery_point_id)
        .execute(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("publish captured Work recovery point", source)
        })?;
        if updated.rows_affected() != 1 {
            return Err(WorkRepositoryError::Conflict {
                resource: WorkConflictResource::RecoveryPointIdentity,
            });
        }
        let captured_row = query(&format!(
            "{RECOVERY_POINT_SELECT_SQL}
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND recovery_point_id = ?
             LIMIT 1"
        ))
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(recovery_point_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("load captured Work recovery point", source)
        })?;
        let captured = decode_record(captured_row)?;
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit captured Work recovery point", source)
        })?;
        Ok(captured)
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
        let row = query(&sql)
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

fn revisions_match_manifest(
    basis: &super::WorkPlanBasis,
    manifest: &RecoveryPointManifestV1,
) -> bool {
    u64::try_from(basis.work_revision.get()).ok() == Some(manifest.work_revision)
        && u64::try_from(basis.branch_revision.get()).ok() == Some(manifest.branch_revision)
        && u64::try_from(basis.graph_revision.get()).ok() == Some(manifest.graph_revision)
        && u64::try_from(basis.goal_revision.get()).ok() == Some(manifest.goal_revision)
        && u64::try_from(basis.criteria_set_revision.get()).ok()
            == Some(manifest.criteria_set_revision)
        && basis.branch_goal_revision == basis.goal_revision
        && basis.branch_criteria_set_revision == basis.criteria_set_revision
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
struct RequestHashInput<'a> {
    schema_version: u16,
    owner_id: &'a WorkOwnerId,
    work_id: &'a WorkId,
    branch_id: &'a WorkBranchId,
    request_id: &'a WorkChangeRef,
    manifest_hash: &'a WorkContentHash,
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

fn validate_request(request: &NewWorkRecoveryPoint) -> Result<(), WorkRepositoryError> {
    request
        .manifest
        .validate()
        .map_err(|source| WorkRepositoryError::corrupt("Work recovery point manifest", source))?;
    if request.manifest.owner_id != request.owner_id.as_str()
        || request.manifest.work_id != request.work_id.as_str()
        || request.manifest.branch_id != request.branch_id.as_str()
    {
        return Err(WorkRepositoryError::Conflict {
            resource: WorkConflictResource::RecoveryPointIdentity,
        });
    }
    if request.manifest.recovery_point_id.len() > RECOVERY_POINT_ID_MAX_BYTES {
        return Err(WorkRepositoryError::corrupt(
            "Work recovery point manifest",
            std::io::Error::other("recovery_point_id exceeds storage width"),
        ));
    }
    Ok(())
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

fn request_hash(
    request: &NewWorkRecoveryPoint,
    manifest_hash: &WorkContentHash,
) -> Result<WorkContentHash, WorkRepositoryError> {
    let payload = serde_json::to_vec(&RequestHashInput {
        schema_version: REQUEST_HASH_SCHEMA_VERSION,
        owner_id: &request.owner_id,
        work_id: &request.work_id,
        branch_id: &request.branch_id,
        request_id: &request.request_id,
        manifest_hash,
    })
    .map_err(|source| WorkRepositoryError::ManifestEncoding {
        entity: "Work recovery point request",
        source,
    })?;
    WorkContentHash::parse(format!("sha256:{:x}", Sha256::digest(payload))).map_err(|source| {
        WorkRepositoryError::corrupt(
            "Work recovery point request hash",
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
    use astra_turn_types::{
        RECOVERY_POINT_MANIFEST_SCHEMA_VERSION, RecoveryPointBindingStateV1,
        RecoveryPointExecutionBindingV1, RecoveryPointExecutorKindV1, RecoveryPointReasonV1,
        SessionCursorV1, SessionKeyV1,
    };

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
    fn request_hash_changes_when_manifest_changes() {
        let owner = WorkOwnerId::parse("owner").unwrap();
        let work = WorkId::parse("work").unwrap();
        let branch = WorkBranchId::parse("branch").unwrap();
        let session_key = SessionKeyV1::owner_session("tenant", "owner", "session", "main");
        let mut manifest = RecoveryPointManifestV1 {
            schema_version: RECOVERY_POINT_MANIFEST_SCHEMA_VERSION,
            recovery_point_id: "rp".into(),
            owner_id: "owner".into(),
            work_id: "work".into(),
            branch_id: "branch".into(),
            work_revision: 1,
            branch_revision: 1,
            graph_revision: 1,
            goal_revision: 1,
            criteria_set_revision: 1,
            session_key: session_key.clone(),
            session_cursor: SessionCursorV1 {
                schema_version: 1,
                owner_id: "owner".into(),
                session_id: "session".into(),
                branch_id: "main".into(),
                completed_turn: 1,
                journal_event_seq: 1,
                conversation_seq: 1,
                canonical_root_hash: "a".repeat(64),
                projection_schema: 1,
                compaction_generation: 0,
                config_version_id: None,
            },
            context_head: astra_turn_types::SessionContextHeadV1 {
                schema_version: 1,
                key: session_key.clone(),
                cursor: SessionCursorV1 {
                    schema_version: 1,
                    owner_id: "owner".into(),
                    session_id: "session".into(),
                    branch_id: "main".into(),
                    completed_turn: 1,
                    journal_event_seq: 1,
                    conversation_seq: 1,
                    canonical_root_hash: "a".repeat(64),
                    projection_schema: 1,
                    compaction_generation: 0,
                    config_version_id: None,
                },
                latest_manifest_root: "a".repeat(64),
                total_canonical_bytes: 1,
                total_message_count: 1,
                writer_epoch: 1,
            },
            run: None,
            execution: RecoveryPointExecutionBindingV1 {
                binding_generation: 1,
                binding_state: RecoveryPointBindingStateV1::Ready,
                logical_workspace_id: "workspace".into(),
                executor_kind: RecoveryPointExecutorKindV1::Server,
                executor_id: "server".into(),
                binding_hash: String::new(),
                physical_workspace_id: None,
            },
            workspace: None,
            artifacts: vec![],
            environment: Default::default(),
            reason: RecoveryPointReasonV1::RunSettled,
            created_at: "2026-09-16T00:00:00Z".into(),
        };
        manifest.execution.binding_hash = manifest.execution.content_hash();
        let first = NewWorkRecoveryPoint {
            owner_id: owner.clone(),
            work_id: work.clone(),
            branch_id: branch.clone(),
            request_id: WorkChangeRef::parse("request").unwrap(),
            manifest: manifest.clone(),
        };
        validate_request(&first).unwrap();
        let first_manifest_hash = manifest_hash(&manifest).unwrap();
        let first_hash = request_hash(&first, &first_manifest_hash).unwrap();
        manifest.created_at = "2026-09-16T00:00:01Z".into();
        let second = NewWorkRecoveryPoint {
            manifest,
            ..first.clone()
        };
        let second_manifest_hash = manifest_hash(&second.manifest).unwrap();
        let second_hash = request_hash(&second, &second_manifest_hash).unwrap();
        assert_ne!(first_hash, second_hash);
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
