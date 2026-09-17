//! Durable storage for immutable Work recovery points.
//!
//! This repository owns the recovery-point record only. Edge/User Runner code
//! captures files and uploads content; this layer validates the declared
//! manifest and stores one owner-scoped, idempotent `preparing` record.
//! The server-authored publisher can promote a safe, server-visible boundary
//! to `published` after re-reading and locking the canonical Work/Session
//! facts. Uploaded content, active Run reconstruction, and cross-environment
//! restore remain outside this repository.

use astra_core::SharedPool;
use astra_turn_types::{
    DEFAULT_CONVERSATION_BRANCH_ID, RecoveryPointAssessmentV1, RecoveryPointBindingStateV1,
    RecoveryPointEnvironmentRequirementsV1, RecoveryPointExecutionBindingV1,
    RecoveryPointExecutorKindV1, RecoveryPointManifestV1, RecoveryPointReasonV1,
    SessionContextHeadV1, SessionKeyV1,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{MySql, QueryBuilder, Row, query};

use super::repository::{DatabaseWorkRepository, WorkConflictResource, WorkRepositoryError};
use super::{WorkBranchId, WorkChangeRef, WorkContentHash, WorkId, WorkOwnerId};
use crate::session_context_coordinator::{
    DatabaseSessionContextCoordinator, SessionContextCoordinator, SessionContextCoordinatorError,
    SessionExecutionBindingStateV1, SessionExecutionBindingV1, lock_recovery_boundary,
};

pub const WORK_RECOVERY_POINT_SCHEMA_VERSION: u32 = 1;
const RECOVERY_POINT_ID_MAX_BYTES: usize = 128;
const RECOVERY_POINT_PAGE_MAX_ITEMS: u16 = 256;
const REQUEST_HASH_SCHEMA_VERSION: u16 = 1;
const RECOVERY_POINT_SELECT_SQL: &str =
    "SELECT owner_id, work_id, branch_id, recovery_point_id, request_id,
            request_hash, status, manifest_json, manifest_hash, failure_reason,
            DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at,
            DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS updated_at,
            DATE_FORMAT(published_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS published_at
     FROM work_recovery_points";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkRecoveryPointStatus {
    Preparing,
    Published,
    Failed,
    Aborted,
}

/// The only request accepted by the server-authored publisher. A caller can
/// choose the reason and optimistic preconditions, but cannot provide a
/// manifest, binding hash, capability result, or local path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewServerWorkRecoveryPoint {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub request_id: WorkChangeRef,
    pub reason: RecoveryPointReasonV1,
    pub expected_work_revision: Option<u64>,
    pub expected_branch_revision: Option<u64>,
    pub expected_graph_revision: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerRecoveryAnchor {
    work_revision: u64,
    branch_revision: u64,
    graph_revision: u64,
    goal_revision: u64,
    criteria_set_revision: u64,
    session_id: String,
    context_head: SessionContextHeadV1,
    execution_binding: SessionExecutionBindingV1,
}

impl WorkRecoveryPointStatus {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "preparing" => Some(Self::Preparing),
            "published" => Some(Self::Published),
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
    pub published_at: Option<DateTime<Utc>>,
    /// Server-derived explanation of what this record proves. Callers cannot
    /// submit or override this assessment.
    pub assessment: RecoveryPointAssessmentV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkRecoveryPointQuery {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: Option<WorkBranchId>,
    pub limit: u16,
}

impl WorkRecoveryPointQuery {
    pub fn new(owner_id: WorkOwnerId, work_id: WorkId) -> Self {
        Self {
            owner_id,
            work_id,
            branch_id: None,
            limit: 32,
        }
    }

    pub fn branch(mut self, branch_id: WorkBranchId) -> Self {
        self.branch_id = Some(branch_id);
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

    /// Record a declared capture idempotently.  The Work and branch identity
    /// are checked in the same transaction as the insert, while the immutable
    /// manifest hash prevents a request id from being reused for a different
    /// capture.  This deliberately returns `preparing`; it must not expose a
    /// caller-supplied manifest as a restorable `published` point.
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
                    DATE_FORMAT(published_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS published_at
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
              created_at, updated_at, published_at)
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
                    DATE_FORMAT(published_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS published_at
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

    async fn load_server_anchor(
        &self,
        request: &NewServerWorkRecoveryPoint,
    ) -> Result<ServerRecoveryAnchor, WorkRepositoryError> {
        let row = query(
            "SELECT w.work_revision, w.current_goal_revision,
                    w.current_criteria_set_revision, b.branch_revision,
                    b.current_graph_revision, b.session_id
             FROM works w
             INNER JOIN work_branches b
               ON b.owner_id = w.owner_id AND b.work_id = w.work_id
              AND b.branch_id = ?
             WHERE w.owner_id = ? AND w.work_id = ?
               AND w.archived_at IS NULL AND b.archived_at IS NULL
               AND b.deletion_operation_id IS NULL
             LIMIT 1",
        )
        .bind(request.branch_id.as_str())
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| WorkRepositoryError::persistence("load Work recovery anchor", source))?
        .ok_or(WorkRepositoryError::NotFound)?;
        super::plan_context_repository::verify_recovery_anchor(
            &DatabaseWorkRepository::new(self.pool.clone()),
            &request.owner_id,
            &request.work_id,
            &request.branch_id,
        )
        .await?;
        let session_id = row
            .try_get::<String, _>("session_id")
            .map_err(|source| WorkRepositoryError::corrupt("Work recovery anchor", source))?;
        let key = server_session_key(request.owner_id.as_str(), &session_id);
        let coordinator = DatabaseSessionContextCoordinator::new(self.pool.clone());
        let admission = coordinator
            .load_admission_snapshot(&key)
            .await
            .map_err(map_session_recovery_error)?;
        if admission.active_writer.is_some() || admission.head.is_none() {
            if admission.active_writer.is_some() {
                return Err(WorkRepositoryError::SessionBusy);
            }
            return Err(WorkRepositoryError::RecoveryPointUnavailable {
                code: "context_head_missing",
            });
        }
        let context_head = admission.head.expect("checked above");
        // A JSON-shaped head is not enough evidence: materialization verifies
        // that the exact immutable manifest nodes and segments are reachable
        // and that their totals match the head used for the next prompt.
        coordinator
            .materialize(&context_head)
            .await
            .map_err(map_session_recovery_error)?;
        let execution_binding = coordinator
            .load_execution_binding(&key)
            .await
            .map_err(map_session_recovery_error)?
            .ok_or(WorkRepositoryError::RecoveryPointUnavailable {
                code: "execution_binding_missing",
            })?;
        execution_binding
            .validate()
            .map_err(map_session_recovery_error)?;
        if execution_binding.state != SessionExecutionBindingStateV1::Ready {
            return Err(WorkRepositoryError::SessionBusy);
        }
        Ok(ServerRecoveryAnchor {
            work_revision: positive_row_u64(&row, "work_revision")?,
            branch_revision: positive_row_u64(&row, "branch_revision")?,
            graph_revision: positive_row_u64(&row, "current_graph_revision")?,
            goal_revision: positive_row_u64(&row, "current_goal_revision")?,
            criteria_set_revision: positive_row_u64(&row, "current_criteria_set_revision")?,
            session_id,
            context_head,
            execution_binding,
        })
    }

    /// Capture and publish a server-authored recovery point at a safe
    /// boundary. The caller supplies intent and optional CAS preconditions;
    /// every identity and capability fact is read from the canonical Work,
    /// Session and execution stores. No caller manifest can reach this path.
    pub async fn publish_server_capture(
        &self,
        request: NewServerWorkRecoveryPoint,
    ) -> Result<WorkRecoveryPointRecord, WorkRepositoryError> {
        validate_server_request(&request)?;
        let request_hash = server_request_hash(&request)?;

        // Idempotency is checked before generating a recovery-point identity;
        // retries therefore return the original row rather than creating a
        // new timestamp/UUID and pretending that the request changed.
        let existing = query(
            "SELECT owner_id, work_id, branch_id, recovery_point_id, request_id,
                    request_hash, status, manifest_json, manifest_hash, failure_reason,
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at,
                    DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS updated_at,
                    DATE_FORMAT(published_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS published_at
             FROM work_recovery_points
             WHERE owner_id = ? AND work_id = ? AND request_id = ?
             LIMIT 1",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.request_id.as_str())
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("load Work recovery publication request", source)
        })?;
        if let Some(row) = existing {
            let stored_hash = row
                .try_get::<String, _>("request_hash")
                .map_err(|source| WorkRepositoryError::corrupt("Work recovery point", source))?;
            if stored_hash != request_hash.as_str() {
                return Err(WorkRepositoryError::Conflict {
                    resource: WorkConflictResource::RecoveryPointRequest,
                });
            }
            let record = decode_record(row)?;
            return Ok(record);
        }

        let anchor = self.load_server_anchor(&request).await?;

        let mut transaction = self.pool.get().begin().await.map_err(|source| {
            WorkRepositoryError::persistence("begin Work recovery point publication", source)
        })?;

        // Lock and re-read all mutable anchors in a deterministic order. The
        // preflight read above gives a useful error quickly; these checks make
        // publication fail closed if a Work/Session/binding changed while the
        // request was being assembled.
        let current = lock_server_anchor(&mut transaction, &request).await?;

        // The Work row is the first canonical lock in this transaction. That
        // lock serializes two publishers for the same branch, so a replay
        // that arrived while the preflight query was running must be checked
        // again before choosing an identity or attempting the insert. This
        // closes the race without holding a pool connection across the
        // preflight Session reads.
        if let Some(row) = query(
            "SELECT owner_id, work_id, branch_id, recovery_point_id, request_id,
                    request_hash, status, manifest_json, manifest_hash, failure_reason,
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at,
                    DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS updated_at,
                    DATE_FORMAT(published_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS published_at
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
            WorkRepositoryError::persistence("recheck Work recovery publication request", source)
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
                WorkRepositoryError::persistence(
                    "commit replayed Work recovery publication",
                    source,
                )
            })?;
            return Ok(record);
        }
        if request
            .expected_work_revision
            .is_some_and(|expected| expected != current.work_revision)
            || request
                .expected_branch_revision
                .is_some_and(|expected| expected != current.branch_revision)
            || request
                .expected_graph_revision
                .is_some_and(|expected| expected != current.graph_revision)
        {
            return Err(WorkRepositoryError::Conflict {
                resource: WorkConflictResource::RecoveryPointIdentity,
            });
        }
        if current != anchor {
            return Err(WorkRepositoryError::Conflict {
                resource: WorkConflictResource::RecoveryPointIdentity,
            });
        }
        let recovery_point_id = format!("rp-{}", &request_hash.as_str()[7..]);
        let manifest = build_server_manifest(&request, &current, &recovery_point_id)?;
        let manifest_hash = manifest_hash(&manifest)?;
        query(
            "INSERT INTO work_recovery_points
             (owner_id, work_id, branch_id, recovery_point_id, request_id,
              request_hash, status, manifest_json, manifest_hash, failure_reason,
              created_at, updated_at, published_at)
             VALUES (?, ?, ?, ?, ?, ?, 'published', ?, ?, NULL, NOW(6), NOW(6), NOW(6))",
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
                "publish Work recovery point",
                WorkConflictResource::RecoveryPointIdentity,
                source,
            )
        })?;
        let row = query(
            "SELECT owner_id, work_id, branch_id, recovery_point_id, request_id,
                    request_hash, status, manifest_json, manifest_hash, failure_reason,
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at,
                    DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS updated_at,
                    DATE_FORMAT(published_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS published_at
             FROM work_recovery_points
             WHERE owner_id = ? AND work_id = ? AND recovery_point_id = ?
             LIMIT 1",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(&recovery_point_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("load published Work recovery point", source)
        })?;
        let record = decode_record(row)?;
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit Work recovery point publication", source)
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
        self.list_with_status(query_value, None).await
    }

    /// List only terminal published points for a user-facing branch view.
    /// Filtering in SQL keeps a page full when many preparing captures are
    /// still being uploaded; filtering after a bounded query could otherwise
    /// hide older published points indefinitely.
    pub async fn list_published(
        &self,
        query_value: WorkRecoveryPointQuery,
    ) -> Result<Vec<WorkRecoveryPointRecord>, WorkRepositoryError> {
        self.list_with_status(query_value, Some("published")).await
    }

    async fn list_with_status(
        &self,
        query_value: WorkRecoveryPointQuery,
        status: Option<&str>,
    ) -> Result<Vec<WorkRecoveryPointRecord>, WorkRepositoryError> {
        if query_value.limit == 0 || query_value.limit > RECOVERY_POINT_PAGE_MAX_ITEMS {
            return Err(WorkRepositoryError::corrupt(
                "Work recovery point query",
                std::io::Error::other(format!(
                    "limit must be between 1 and {RECOVERY_POINT_PAGE_MAX_ITEMS}"
                )),
            ));
        }
        let mut builder = QueryBuilder::<MySql>::new(RECOVERY_POINT_SELECT_SQL);
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
        if let Some(status) = status {
            builder.push(" AND status = ").push_bind(status);
        }
        builder
            .push(" ORDER BY created_at DESC, recovery_point_id DESC LIMIT ")
            .push_bind(i64::from(query_value.limit));
        let rows = builder
            .build()
            .fetch_all(self.pool.get())
            .await
            .map_err(|source| {
                WorkRepositoryError::persistence("list Work recovery points", source)
            })?;
        Ok(rows
            .into_iter()
            .filter_map(|row| match decode_record(row) {
                Ok(record) => Some(record),
                Err(error) => {
                    // A malformed identity/timestamp cannot be represented as
                    // a safe typed record, but it must not poison healthy
                    // points in the same bounded page.
                    tracing::warn!(
                        error = %error,
                        "skipping structurally corrupt Work recovery point"
                    );
                    None
                }
            })
            .collect())
    }
}

async fn lock_server_anchor(
    transaction: &mut sqlx::Transaction<'_, MySql>,
    request: &NewServerWorkRecoveryPoint,
) -> Result<ServerRecoveryAnchor, WorkRepositoryError> {
    let row = query(
        "SELECT w.work_revision, w.current_goal_revision,
                w.current_criteria_set_revision, b.branch_revision,
                b.current_graph_revision, b.session_id
         FROM works w
         INNER JOIN work_branches b
           ON b.owner_id = w.owner_id AND b.work_id = w.work_id
          AND b.branch_id = ?
         WHERE w.owner_id = ? AND w.work_id = ?
           AND w.archived_at IS NULL AND b.archived_at IS NULL
           AND b.deletion_operation_id IS NULL
         LIMIT 1 FOR UPDATE",
    )
    .bind(request.branch_id.as_str())
    .bind(request.owner_id.as_str())
    .bind(request.work_id.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("lock Work recovery anchor", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let session_id = row
        .try_get::<String, _>("session_id")
        .map_err(|source| WorkRepositoryError::corrupt("Work recovery anchor", source))?;
    let key = server_session_key(request.owner_id.as_str(), &session_id);

    // Use the same session -> execution-slot fence as Run admission before
    // touching context or Run rows. The slot is the canonical answer to
    // whether a session has an execution owner; a best-effort run scan alone
    // would race a concurrent Run start.
    let slot_run_id = crate::storage::admit_session_execution_write(
        transaction,
        &session_id,
        request.owner_id.as_str(),
    )
    .await
    .map_err(|source| {
        if matches!(source, sqlx::Error::RowNotFound) {
            WorkRepositoryError::RecoveryPointUnavailable {
                code: "session_not_active",
            }
        } else {
            WorkRepositoryError::persistence("admit Work recovery Session execution", source)
        }
    })?;
    if slot_run_id.is_some() {
        return Err(WorkRepositoryError::SessionBusy);
    }

    // A terminal Run can outlive the execution slot. The invocation ledger is
    // still authoritative for effects whose provider boundary is prepared,
    // dispatched, or unknown, so do not publish a point that would make those
    // operations look safely replayable.
    if let Some(frontier) =
        crate::tool_invocation_ledger::DatabaseToolInvocationLedger::lock_recovery_effect_frontier(
            transaction,
            request.owner_id.as_str(),
            &session_id,
        )
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("check Work recovery effect frontier", source)
        })?
    {
        tracing::warn!(
            run_id = %frontier.run_id,
            state = %frontier.state,
            identity_key = %frontier.identity_key,
            "recovery publication blocked by unresolved tool invocation"
        );
        return Err(WorkRepositoryError::RecoveryPointUnavailable {
            code: "effect_review_required",
        });
    }

    let session_boundary = lock_recovery_boundary(transaction, &key)
        .await
        .map_err(map_session_recovery_error)?;
    if session_boundary
        .active_writer
        .as_ref()
        .is_some_and(|lease| lease.expires_at_unix_ms > session_boundary.database_now_unix_ms)
        || session_boundary
            .active_reservation
            .as_ref()
            .is_some_and(|reservation| {
                reservation.expires_at_unix_ms > session_boundary.database_now_unix_ms
            })
    {
        return Err(WorkRepositoryError::SessionBusy);
    }

    let active_run = query(
        "SELECT run_id FROM agent_runs
         WHERE user_id = ? AND session_id = ?
           AND status IN ('running', 'waiting', 'paused')
         ORDER BY updated_at DESC, run_id DESC
         LIMIT 1 FOR UPDATE",
    )
    .bind(request.owner_id.as_str())
    .bind(&session_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::persistence("check Work recovery Run boundary", source)
    })?;
    if active_run.is_some() {
        return Err(WorkRepositoryError::SessionBusy);
    }

    let execution_binding = session_boundary.execution_binding.ok_or(
        WorkRepositoryError::RecoveryPointUnavailable {
            code: "execution_binding_missing",
        },
    )?;
    if execution_binding.state != SessionExecutionBindingStateV1::Ready {
        return Err(WorkRepositoryError::SessionBusy);
    }
    Ok(ServerRecoveryAnchor {
        work_revision: positive_row_u64(&row, "work_revision")?,
        branch_revision: positive_row_u64(&row, "branch_revision")?,
        graph_revision: positive_row_u64(&row, "current_graph_revision")?,
        goal_revision: positive_row_u64(&row, "current_goal_revision")?,
        criteria_set_revision: positive_row_u64(&row, "current_criteria_set_revision")?,
        session_id,
        context_head: session_boundary.head,
        execution_binding,
    })
}

fn map_session_recovery_error(error: SessionContextCoordinatorError) -> WorkRepositoryError {
    match error {
        SessionContextCoordinatorError::Database { operation, source } => {
            WorkRepositoryError::Persistence { operation, source }
        }
        SessionContextCoordinatorError::DatabaseJson { entity, source } => {
            WorkRepositoryError::corrupt(entity, source)
        }
        SessionContextCoordinatorError::ExecutionBindingBusy
        | SessionContextCoordinatorError::ExecutionBindingNotReady(_) => {
            WorkRepositoryError::SessionBusy
        }
        SessionContextCoordinatorError::NeedsRepair(message)
            if message.contains("head is missing") =>
        {
            WorkRepositoryError::RecoveryPointUnavailable {
                code: "context_head_missing",
            }
        }
        SessionContextCoordinatorError::NeedsRepair(message) => WorkRepositoryError::corrupt(
            "Work recovery Session boundary",
            std::io::Error::other(message),
        ),
        SessionContextCoordinatorError::Invalid(message) => WorkRepositoryError::corrupt(
            "Work recovery Session boundary",
            std::io::Error::other(message),
        ),
        other => WorkRepositoryError::corrupt(
            "Work recovery Session boundary",
            std::io::Error::other(other.to_string()),
        ),
    }
}

impl DatabaseWorkRepository {
    pub fn recovery_points(&self) -> DatabaseWorkRecoveryPointRepository {
        DatabaseWorkRecoveryPointRepository::new(self.pool.clone())
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

#[derive(Serialize)]
struct ServerRequestHashInput<'a> {
    schema_version: u16,
    owner_id: &'a str,
    work_id: &'a str,
    branch_id: &'a str,
    request_id: &'a str,
    reason: RecoveryPointReasonV1,
    expected_work_revision: Option<u64>,
    expected_branch_revision: Option<u64>,
    expected_graph_revision: Option<u64>,
}

fn validate_server_request(
    request: &NewServerWorkRecoveryPoint,
) -> Result<(), WorkRepositoryError> {
    if request.request_id.as_str().is_empty() {
        return Err(WorkRepositoryError::corrupt(
            "Work recovery point request",
            std::io::Error::other("request_id must not be empty"),
        ));
    }
    for (field, value) in [
        ("expected_work_revision", request.expected_work_revision),
        ("expected_branch_revision", request.expected_branch_revision),
        ("expected_graph_revision", request.expected_graph_revision),
    ] {
        if value == Some(0) {
            return Err(WorkRepositoryError::corrupt(
                "Work recovery point request",
                std::io::Error::other(format!("{field} must be positive")),
            ));
        }
    }
    Ok(())
}

fn server_request_hash(
    request: &NewServerWorkRecoveryPoint,
) -> Result<WorkContentHash, WorkRepositoryError> {
    let payload = serde_json::to_vec(&ServerRequestHashInput {
        schema_version: REQUEST_HASH_SCHEMA_VERSION,
        owner_id: request.owner_id.as_str(),
        work_id: request.work_id.as_str(),
        branch_id: request.branch_id.as_str(),
        request_id: request.request_id.as_str(),
        reason: request.reason,
        expected_work_revision: request.expected_work_revision,
        expected_branch_revision: request.expected_branch_revision,
        expected_graph_revision: request.expected_graph_revision,
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

fn server_session_key(owner_id: &str, session_id: &str) -> SessionKeyV1 {
    SessionKeyV1::owner_session(
        "server",
        owner_id,
        session_id,
        DEFAULT_CONVERSATION_BRANCH_ID,
    )
}

fn positive_row_u64(
    row: &sqlx::mysql::MySqlRow,
    field: &'static str,
) -> Result<u64, WorkRepositoryError> {
    let value = row
        .try_get::<i64, _>(field)
        .map_err(|source| WorkRepositoryError::corrupt("Work recovery anchor", source))?;
    u64::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            WorkRepositoryError::corrupt(
                "Work recovery anchor",
                std::io::Error::other(format!("{field} must be positive")),
            )
        })
}

fn build_server_manifest(
    request: &NewServerWorkRecoveryPoint,
    anchor: &ServerRecoveryAnchor,
    recovery_point_id: &str,
) -> Result<RecoveryPointManifestV1, WorkRepositoryError> {
    let execution = project_execution_binding(&anchor.execution_binding)?;
    let manifest = RecoveryPointManifestV1 {
        schema_version: astra_turn_types::RECOVERY_POINT_MANIFEST_SCHEMA_VERSION,
        recovery_point_id: recovery_point_id.to_owned(),
        owner_id: request.owner_id.as_str().to_owned(),
        work_id: request.work_id.as_str().to_owned(),
        branch_id: request.branch_id.as_str().to_owned(),
        work_revision: anchor.work_revision,
        branch_revision: anchor.branch_revision,
        graph_revision: anchor.graph_revision,
        goal_revision: anchor.goal_revision,
        criteria_set_revision: anchor.criteria_set_revision,
        session_key: server_session_key(request.owner_id.as_str(), &anchor.session_id),
        session_cursor: anchor.context_head.cursor.clone(),
        context_head: anchor.context_head.clone(),
        run: None,
        execution,
        workspace: None,
        artifacts: Vec::new(),
        environment: RecoveryPointEnvironmentRequirementsV1::default(),
        reason: request.reason,
        created_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
    };
    manifest
        .validate()
        .map_err(|source| WorkRepositoryError::corrupt("Work recovery point manifest", source))?;
    Ok(manifest)
}

fn project_execution_binding(
    binding: &SessionExecutionBindingV1,
) -> Result<RecoveryPointExecutionBindingV1, WorkRepositoryError> {
    use crate::runs::ExecutorBindingRequestKind;
    let (executor_kind, executor_id) = match binding.executor.kind {
        ExecutorBindingRequestKind::ServerLocal => {
            (RecoveryPointExecutorKindV1::Server, "server".to_owned())
        }
        ExecutorBindingRequestKind::EdgeAgent => (
            RecoveryPointExecutorKindV1::Edge,
            binding.executor.executor_id.clone().ok_or_else(|| {
                WorkRepositoryError::corrupt(
                    "Work recovery execution binding",
                    std::io::Error::other("Edge binding has no executor identity"),
                )
            })?,
        ),
        _ => {
            return Err(WorkRepositoryError::corrupt(
                "Work recovery execution binding",
                std::io::Error::other("unsupported execution provider for a recovery point"),
            ));
        }
    };
    let binding_state = match binding.state {
        SessionExecutionBindingStateV1::Ready => RecoveryPointBindingStateV1::Ready,
        SessionExecutionBindingStateV1::Switching => RecoveryPointBindingStateV1::Switching,
        SessionExecutionBindingStateV1::NeedsAttention => {
            RecoveryPointBindingStateV1::NeedsAttention
        }
    };
    let canonical_bytes =
        serde_json::to_vec(binding).map_err(|source| WorkRepositoryError::ManifestEncoding {
            entity: "canonical Work execution binding",
            source,
        })?;
    let mut canonical_digest = Sha256::new();
    canonical_digest.update(b"astra.recovery-point.canonical-execution-binding.v1\0");
    canonical_digest.update((canonical_bytes.len() as u64).to_be_bytes());
    canonical_digest.update(canonical_bytes);
    let canonical_binding_hash = format!("sha256:{:x}", canonical_digest.finalize());
    let mut projection = RecoveryPointExecutionBindingV1 {
        binding_generation: binding.generation,
        binding_state,
        logical_workspace_id: binding.logical_workspace_id.clone(),
        executor_kind,
        executor_id,
        canonical_binding_hash,
        binding_hash: String::new(),
        physical_workspace_id: binding.physical_workspace_id.clone(),
    };
    projection.binding_hash = projection.content_hash();
    Ok(projection)
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
    // A damaged manifest must not make a whole list unreadable. Preserve the
    // trusted row identity and return a typed Corrupt assessment, but never
    // expose malformed JSON or an unverified hash to callers. Structural row
    // fields below remain strict because they are needed to identify a point.
    let mut manifest_corruption: Option<&'static str> = None;
    let mut manifest_hash = match optional_text("manifest_hash")? {
        Some(value) => match WorkContentHash::parse(value) {
            Ok(hash) => Some(hash),
            Err(_) => {
                manifest_corruption = Some("manifest_hash_invalid");
                None
            }
        },
        None => {
            if status == WorkRecoveryPointStatus::Published {
                manifest_corruption = Some("manifest_hash_missing");
            }
            None
        }
    };
    let manifest = match optional_text("manifest_json")? {
        Some(value) => match serde_json::from_str::<RecoveryPointManifestV1>(&value) {
            Ok(manifest) => {
                if manifest.validate().is_err() {
                    manifest_corruption.get_or_insert("manifest_invalid");
                    None
                } else if manifest.owner_id != owner_id.as_str()
                    || manifest.work_id != work_id.as_str()
                    || manifest.branch_id != branch_id.as_str()
                {
                    manifest_corruption.get_or_insert("manifest_identity_mismatch");
                    None
                } else if manifest.recovery_point_id != recovery_point_id {
                    manifest_corruption.get_or_insert("recovery_point_id_mismatch");
                    None
                } else {
                    let hash_matches = manifest_hash.as_ref().is_some_and(|expected| {
                        manifest
                            .content_hash()
                            .ok()
                            .and_then(|hash| WorkContentHash::parse(hash).ok())
                            .as_ref()
                            == Some(expected)
                    });
                    if !hash_matches {
                        manifest_corruption.get_or_insert("manifest_hash_mismatch");
                        None
                    } else {
                        Some(manifest)
                    }
                }
            }
            Err(_) => {
                manifest_corruption = Some("manifest_json_invalid");
                None
            }
        },
        None => {
            if status == WorkRecoveryPointStatus::Published {
                manifest_corruption.get_or_insert("manifest_missing");
            }
            None
        }
    };
    if status == WorkRecoveryPointStatus::Published
        && (manifest.is_none() || manifest_hash.is_none())
    {
        manifest_corruption.get_or_insert("published_manifest_incomplete");
    }
    if status == WorkRecoveryPointStatus::Published
        && !recovery_point_id
            .strip_prefix("rp-")
            .is_some_and(|suffix| suffix == &request_hash.as_str()[7..])
    {
        manifest_corruption.get_or_insert("published_recovery_point_id_mismatch");
    }
    if manifest_corruption.is_some() {
        // A hash without a verified manifest is not a useful integrity claim.
        manifest_hash = None;
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
    let published_at = optional_text("published_at")?
        .map(|value| {
            super::repository::decode_timestamp("Work recovery point", "published_at", value)
        })
        .transpose()?;
    let assessment = match manifest_corruption {
        Some(reason) => RecoveryPointAssessmentV1::corrupt(reason),
        None => match (status, manifest.as_ref()) {
            (WorkRecoveryPointStatus::Published, Some(manifest)) => {
                RecoveryPointAssessmentV1::published_without_workspace(manifest)
            }
            _ => RecoveryPointAssessmentV1::unverified(),
        },
    };
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
        published_at,
        assessment,
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
                canonical_binding_hash:
                    "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into(),
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
}
