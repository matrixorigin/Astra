use super::events::NewWorkEvent;
use super::repository::{DatabaseWorkRepository, WorkRepositoryError};
use super::{
    WorkBranchId, WorkBranchRetentionBasisResource, WorkBranchRetentionChange,
    WorkBranchRetentionOutcome, WorkBranchRetentionReceipt, WorkBranchRevision, WorkContentHash,
    WorkEventKind, WorkRevision,
};
use sqlx::{MySql, Row, Transaction, query};

fn stale(resource: WorkBranchRetentionBasisResource) -> WorkRepositoryError {
    WorkRepositoryError::StaleBranchRetention { resource }
}

fn payload_hash(
    change: &WorkBranchRetentionChange,
) -> Result<WorkContentHash, WorkRepositoryError> {
    let payload =
        serde_json::to_string(change).map_err(|source| WorkRepositoryError::ManifestEncoding {
            entity: "branch retention payload",
            source,
        })?;
    WorkContentHash::parse(super::repository::content_hash(&payload)).map_err(|source| {
        WorkRepositoryError::corrupt("branch retention payload", std::io::Error::other(source))
    })
}

async fn replay(
    transaction: &mut Transaction<'_, MySql>,
    change: &WorkBranchRetentionChange,
    expected_payload_hash: &WorkContentHash,
) -> Result<Option<WorkBranchRetentionReceipt>, WorkRepositoryError> {
    let row = query(
        "SELECT event_kind, branch_id, work_revision, branch_revision, payload_hash
         FROM work_events
         WHERE owner_id = ? AND work_id = ? AND source_ref = ?
           AND event_kind IN ('branch_archived', 'branch_restored')
         ORDER BY event_seq ASC LIMIT 1",
    )
    .bind(change.owner_id.as_str())
    .bind(change.work_id.as_str())
    .bind(change.request_id.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("load branch retention replay", source))?;
    let Some(row) = row else { return Ok(None) };
    let stored_hash = WorkContentHash::parse(
        row.try_get::<String, _>("payload_hash")
            .map_err(|source| WorkRepositoryError::corrupt("branch retention event", source))?,
    )
    .map_err(|source| {
        WorkRepositoryError::corrupt("branch retention event", std::io::Error::other(source))
    })?;
    if stored_hash != *expected_payload_hash {
        return Err(stale(WorkBranchRetentionBasisResource::RequestPayload));
    }
    let event_kind = WorkEventKind::from_persisted(
        &row.try_get::<String, _>("event_kind")
            .map_err(|source| WorkRepositoryError::corrupt("branch retention event", source))?,
    )
    .ok_or_else(|| {
        WorkRepositoryError::corrupt(
            "branch retention event",
            std::io::Error::other("retention event kind is unknown"),
        )
    })?;
    if event_kind != change.kind.event_kind() {
        return Err(stale(WorkBranchRetentionBasisResource::RequestPayload));
    }
    let branch_id = WorkBranchId::parse(
        row.try_get::<String, _>("branch_id")
            .map_err(|source| WorkRepositoryError::corrupt("branch retention event", source))?,
    )
    .map_err(|source| WorkRepositoryError::corrupt("branch retention event", source))?;
    let work_revision = WorkRevision::new(
        row.try_get::<i64, _>("work_revision")
            .map_err(|source| WorkRepositoryError::corrupt("branch retention event", source))?,
    )
    .map_err(|source| WorkRepositoryError::corrupt("branch retention event", source))?;
    let branch_revision = WorkBranchRevision::new(
        row.try_get::<i64, _>("branch_revision")
            .map_err(|source| WorkRepositoryError::corrupt("branch retention event", source))?,
    )
    .map_err(|source| WorkRepositoryError::corrupt("branch retention event", source))?;
    let applied_work_revision = change
        .expected_work_revision
        .checked_next()
        .map_err(super::repository::invalid_mutation)?;
    let applied_branch_revision = change
        .expected_branch_revision
        .checked_next()
        .map_err(super::repository::invalid_mutation)?;
    let outcome =
        if work_revision == applied_work_revision && branch_revision == applied_branch_revision {
            WorkBranchRetentionOutcome::Applied
        } else if work_revision == change.expected_work_revision
            && branch_revision == change.expected_branch_revision
        {
            WorkBranchRetentionOutcome::AlreadyInState
        } else {
            return Err(WorkRepositoryError::corrupt(
                "branch retention event",
                std::io::Error::other("receipt revisions violate the retention transition"),
            ));
        };
    Ok(Some(WorkBranchRetentionReceipt {
        schema_version: super::WORK_BRANCH_RETENTION_SCHEMA_VERSION,
        work_id: change.work_id.clone(),
        branch_id,
        request_id: change.request_id.clone(),
        kind: change.kind,
        work_revision,
        branch_revision,
        outcome,
    }))
}

pub(super) async fn change_branch_retention(
    repository: &DatabaseWorkRepository,
    change: WorkBranchRetentionChange,
) -> Result<WorkBranchRetentionReceipt, WorkRepositoryError> {
    let change_payload_hash = payload_hash(&change)?;
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin branch retention transaction", source)
    })?;
    let row = query(
        "SELECT w.work_revision, w.delivery_branch_id, w.archived_at AS work_archived_at,
                b.branch_revision, b.session_id, b.archived_at AS branch_archived_at,
                b.deletion_operation_id
         FROM works w
         JOIN work_branches b
           ON b.owner_id = w.owner_id AND b.work_id = w.work_id AND b.branch_id = ?
         WHERE w.owner_id = ? AND w.work_id = ? LIMIT 1 FOR UPDATE",
    )
    .bind(change.branch_id.as_str())
    .bind(change.owner_id.as_str())
    .bind(change.work_id.as_str())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("lock branch retention basis", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    if let Some(receipt) = replay(&mut transaction, &change, &change_payload_hash).await? {
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit branch retention replay", source)
        })?;
        return Ok(receipt);
    }
    if row
        .try_get::<Option<chrono::NaiveDateTime>, _>("work_archived_at")
        .map_err(|source| WorkRepositoryError::corrupt("branch retention basis", source))?
        .is_some()
    {
        return Err(WorkRepositoryError::Archived);
    }
    if row
        .try_get::<Option<String>, _>("deletion_operation_id")
        .map_err(|source| WorkRepositoryError::corrupt("branch retention basis", source))?
        .is_some()
    {
        return Err(WorkRepositoryError::BranchDeleting);
    }
    let work_revision = WorkRevision::new(
        row.try_get::<i64, _>("work_revision")
            .map_err(|source| WorkRepositoryError::corrupt("branch retention basis", source))?,
    )
    .map_err(|source| WorkRepositoryError::corrupt("branch retention basis", source))?;
    let branch_revision = WorkBranchRevision::new(
        row.try_get::<i64, _>("branch_revision")
            .map_err(|source| WorkRepositoryError::corrupt("branch retention basis", source))?,
    )
    .map_err(|source| WorkRepositoryError::corrupt("branch retention basis", source))?;
    if work_revision != change.expected_work_revision {
        return Err(stale(WorkBranchRetentionBasisResource::WorkRevision));
    }
    if branch_revision != change.expected_branch_revision {
        return Err(stale(WorkBranchRetentionBasisResource::BranchRevision));
    }
    let branch_is_archived = row
        .try_get::<Option<chrono::NaiveDateTime>, _>("branch_archived_at")
        .map_err(|source| WorkRepositoryError::corrupt("branch retention basis", source))?
        .is_some();
    let already_in_state = branch_is_archived == change.kind.wants_archived();
    if change.kind.wants_archived() {
        let delivery_branch_id = WorkBranchId::parse(
            row.try_get::<String, _>("delivery_branch_id")
                .map_err(|source| WorkRepositoryError::corrupt("branch retention basis", source))?,
        )
        .map_err(|source| WorkRepositoryError::corrupt("branch retention basis", source))?;
        if delivery_branch_id == change.branch_id {
            return Err(WorkRepositoryError::DeliveryBranchProtected);
        }
        let session_id = row
            .try_get::<String, _>("session_id")
            .map_err(|source| WorkRepositoryError::corrupt("branch retention basis", source))?;
        let has_active_run: bool = query(
            "SELECT EXISTS(
                SELECT 1 FROM agent_session_execution_slots
                WHERE user_id = ? AND session_id = ? LIMIT 1
             ) AS active",
        )
        .bind(change.owner_id.as_str())
        .bind(session_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|source| WorkRepositoryError::persistence("check active branch run", source))?
        .try_get("active")
        .map_err(|source| WorkRepositoryError::corrupt("active branch run", source))?;
        if has_active_run {
            return Err(WorkRepositoryError::BranchActive);
        }
    }
    let (result_work_revision, result_branch_revision, outcome) = if already_in_state {
        (
            work_revision,
            branch_revision,
            WorkBranchRetentionOutcome::AlreadyInState,
        )
    } else {
        let next_work_revision = work_revision
            .checked_next()
            .map_err(super::repository::invalid_mutation)?;
        let next_branch_revision = branch_revision
            .checked_next()
            .map_err(super::repository::invalid_mutation)?;
        let work_update = query(
            "UPDATE works SET work_revision = ?, updated_at = NOW(6)
             WHERE owner_id = ? AND work_id = ? AND work_revision = ? AND archived_at IS NULL",
        )
        .bind(next_work_revision.get())
        .bind(change.owner_id.as_str())
        .bind(change.work_id.as_str())
        .bind(work_revision.get())
        .execute(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("update branch retention Work", source)
        })?;
        if work_update.rows_affected() != 1 {
            return Err(stale(WorkBranchRetentionBasisResource::WorkRevision));
        }
        let archive_expression = if change.kind.wants_archived() {
            "NOW(6)"
        } else {
            "NULL"
        };
        let archive_predicate = if change.kind.wants_archived() {
            "archived_at IS NULL"
        } else {
            "archived_at IS NOT NULL"
        };
        let statement = format!(
            "UPDATE work_branches SET branch_revision = ?, archived_at = {archive_expression}, updated_at = NOW(6)
             WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND branch_revision = ?
               AND {archive_predicate}"
        );
        let branch_update = query(&statement)
            .bind(next_branch_revision.get())
            .bind(change.owner_id.as_str())
            .bind(change.work_id.as_str())
            .bind(change.branch_id.as_str())
            .bind(branch_revision.get())
            .execute(&mut *transaction)
            .await
            .map_err(|source| {
                WorkRepositoryError::persistence("update branch retention state", source)
            })?;
        if branch_update.rows_affected() != 1 {
            return Err(stale(WorkBranchRetentionBasisResource::BranchRevision));
        }
        (
            next_work_revision,
            next_branch_revision,
            WorkBranchRetentionOutcome::Applied,
        )
    };
    super::events_repository::append_event_with_payload_hash(
        &mut transaction,
        &NewWorkEvent {
            owner_id: change.owner_id.clone(),
            work_id: change.work_id.clone(),
            branch_id: Some(change.branch_id.clone()),
            kind: change.kind.event_kind(),
            work_revision: Some(result_work_revision),
            goal_revision: None,
            criterion_set_revision: None,
            branch_revision: Some(result_branch_revision),
            graph_revision: None,
            source_ref: change.request_id.clone(),
        },
        Some(&change_payload_hash),
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|source| WorkRepositoryError::persistence("commit branch retention", source))?;
    Ok(WorkBranchRetentionReceipt {
        schema_version: super::WORK_BRANCH_RETENTION_SCHEMA_VERSION,
        work_id: change.work_id,
        branch_id: change.branch_id,
        request_id: change.request_id,
        kind: change.kind,
        work_revision: result_work_revision,
        branch_revision: result_branch_revision,
        outcome,
    })
}
