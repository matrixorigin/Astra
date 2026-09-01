use super::events::{NewWorkEvent, WorkEventKind, WorkEventSeq, retained_from};
use super::repository::{WorkConflictResource, WorkRepositoryError};
use sqlx::{MySql, Row, Transaction, query};

pub(super) async fn insert_genesis_event(
    transaction: &mut Transaction<'_, MySql>,
    event: &NewWorkEvent,
) -> Result<WorkEventSeq, WorkRepositoryError> {
    let event_seq = WorkEventSeq::INITIAL;
    query(
        "INSERT INTO work_event_sequences
         (owner_id, work_id, last_event_seq, retained_from_event_seq)
         VALUES (?, ?, 1, 1)",
    )
    .bind(event.owner_id.as_str())
    .bind(event.work_id.as_str())
    .execute(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::insert(
            "insert Work event sequence",
            WorkConflictResource::WorkEventSequence,
            source,
        )
    })?;
    insert_event(transaction, event, event_seq, None).await?;
    Ok(event_seq)
}

pub(super) async fn append_event(
    transaction: &mut Transaction<'_, MySql>,
    event: &NewWorkEvent,
) -> Result<WorkEventSeq, WorkRepositoryError> {
    append_event_with_payload_hash(transaction, event, None).await
}

pub(super) async fn append_event_with_payload_hash(
    transaction: &mut Transaction<'_, MySql>,
    event: &NewWorkEvent,
    payload_hash: Option<&super::WorkContentHash>,
) -> Result<WorkEventSeq, WorkRepositoryError> {
    let updated = query(
        "UPDATE work_event_sequences
         SET retained_from_event_seq = CASE
                 WHEN last_event_seq >= ? THEN last_event_seq - ? ELSE 1
             END,
             last_event_seq = last_event_seq + 1,
             updated_at = NOW(6)
         WHERE owner_id = ? AND work_id = ? AND last_event_seq < ?",
    )
    .bind(super::events::WORK_EVENT_RETENTION_PER_WORK - 1)
    .bind(super::events::WORK_EVENT_RETENTION_PER_WORK - 2)
    .bind(event.owner_id.as_str())
    .bind(event.work_id.as_str())
    .bind(i64::MAX)
    .execute(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("allocate Work event sequence", source))?;
    if updated.rows_affected() != 1 {
        return Err(WorkRepositoryError::corrupt(
            "Work event sequence",
            std::io::Error::other("sequence is missing or exhausted"),
        ));
    }
    let sequence = query(
        "SELECT last_event_seq, retained_from_event_seq FROM work_event_sequences
         WHERE owner_id = ? AND work_id = ? LIMIT 1",
    )
    .bind(event.owner_id.as_str())
    .bind(event.work_id.as_str())
    .fetch_one(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("read Work event sequence", source))?;
    let head = sequence
        .try_get::<i64, _>("last_event_seq")
        .map_err(|source| WorkRepositoryError::corrupt("Work event sequence", source))?;
    let event_seq = WorkEventSeq::new(head)
        .map_err(|source| WorkRepositoryError::corrupt("Work event sequence", source))?;
    let retained_floor = sequence
        .try_get::<i64, _>("retained_from_event_seq")
        .map_err(|source| WorkRepositoryError::corrupt("Work event retention", source))?;
    if retained_floor != retained_from(event_seq) {
        return Err(WorkRepositoryError::corrupt(
            "Work event retention",
            std::io::Error::other("retained floor does not match the fixed retention window"),
        ));
    }
    insert_event(transaction, event, event_seq, payload_hash).await?;
    if retained_floor > 1 {
        prune_expired_event_fact(transaction, event, retained_floor - 1).await?;
        let deleted = query(
            "DELETE FROM work_events
             WHERE owner_id = ? AND work_id = ? AND event_seq = ?",
        )
        .bind(event.owner_id.as_str())
        .bind(event.work_id.as_str())
        .bind(retained_floor - 1)
        .execute(&mut **transaction)
        .await
        .map_err(|source| WorkRepositoryError::persistence("prune retained Work event", source))?;
        if deleted.rows_affected() != 1 {
            return Err(WorkRepositoryError::corrupt(
                "Work event retention",
                std::io::Error::other("event leaving the retention window is missing"),
            ));
        }
    }
    Ok(event_seq)
}

/// Prunes detail owned by an event only after a bounded canonical projection
/// exists for facts that must outlive history. Check detail is intentionally
/// allowed to expire; current gap acceptance preserves its exact payload hash.
async fn prune_expired_event_fact(
    transaction: &mut Transaction<'_, MySql>,
    event: &NewWorkEvent,
    expired_event_seq: i64,
) -> Result<(), WorkRepositoryError> {
    let row = query(
        "SELECT event_kind, source_ref FROM work_events
         WHERE owner_id = ? AND work_id = ? AND event_seq = ? LIMIT 1",
    )
    .bind(event.owner_id.as_str())
    .bind(event.work_id.as_str())
    .bind(expired_event_seq)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("load expiring Work event", source))?
    .ok_or_else(|| {
        WorkRepositoryError::corrupt(
            "Work event retention",
            std::io::Error::other("event leaving the retention window is missing"),
        )
    })?;
    let kind = row
        .try_get::<String, _>("event_kind")
        .map_err(|source| WorkRepositoryError::corrupt("Work event", source))?;
    let kind = WorkEventKind::from_persisted(&kind).ok_or_else(|| {
        WorkRepositoryError::corrupt(
            "Work event",
            std::io::Error::other("unknown event kind at retention boundary"),
        )
    })?;
    let source_ref = row
        .try_get::<String, _>("source_ref")
        .map_err(|source| WorkRepositoryError::corrupt("Work event", source))?;
    let deletion = match kind {
        WorkEventKind::CheckRecorded => Some((
            "DELETE FROM work_check_runs
             WHERE owner_id = ? AND work_id = ? AND check_run_id = ?",
            "expired check run",
        )),
        WorkEventKind::GapsAccepted => Some((
            "DELETE FROM work_acceptance_decisions
             WHERE owner_id = ? AND work_id = ? AND decision_id = ?",
            "expired acceptance decision",
        )),
        WorkEventKind::WorkCreated
        | WorkEventKind::GoalRevised
        | WorkEventKind::CriteriaAccepted
        | WorkEventKind::BranchBasisAdopted
        | WorkEventKind::GraphReplaced
        | WorkEventKind::DeliveryBranchSelected
        | WorkEventKind::BranchArchived
        | WorkEventKind::BranchRestored
        | WorkEventKind::SubjectChanged
        | WorkEventKind::PatchArtifactExported
        | WorkEventKind::PlanProposed
        | WorkEventKind::CriteriaProposed
        | WorkEventKind::ProposalRejected
        | WorkEventKind::RunCompleted
        | WorkEventKind::RunDelegated
        | WorkEventKind::RunFailed
        | WorkEventKind::RunCancelled
        | WorkEventKind::RuntimeEventsExpired => None,
    };
    if let Some((statement, entity)) = deletion {
        let deleted = query(statement)
            .bind(event.owner_id.as_str())
            .bind(event.work_id.as_str())
            .bind(source_ref)
            .execute(&mut **transaction)
            .await
            .map_err(|source| WorkRepositoryError::persistence(entity, source))?;
        if deleted.rows_affected() != 1 {
            return Err(WorkRepositoryError::corrupt(
                entity,
                std::io::Error::other("semantic event has no matching retained detail"),
            ));
        }
    }
    Ok(())
}

async fn insert_event(
    transaction: &mut Transaction<'_, MySql>,
    event: &NewWorkEvent,
    event_seq: WorkEventSeq,
    payload_hash: Option<&super::WorkContentHash>,
) -> Result<(), WorkRepositoryError> {
    query(
        "INSERT INTO work_events
         (owner_id, work_id, event_seq, branch_id, event_kind, work_revision,
          goal_revision, criterion_set_revision, branch_revision, graph_revision, source_ref,
          payload_hash)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(event.owner_id.as_str())
    .bind(event.work_id.as_str())
    .bind(event_seq.get())
    .bind(event.branch_id.as_ref().map(super::WorkBranchId::as_str))
    .bind(event.kind.as_str())
    .bind(event.work_revision.map(|revision| revision.get()))
    .bind(event.goal_revision.map(|revision| revision.get()))
    .bind(event.criterion_set_revision.map(|revision| revision.get()))
    .bind(event.branch_revision.map(|revision| revision.get()))
    .bind(event.graph_revision.map(|revision| revision.get()))
    .bind(event.source_ref.as_str())
    .bind(payload_hash.map(super::WorkContentHash::as_str))
    .execute(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::insert(
            "insert Work event",
            WorkConflictResource::WorkEventIdentity,
            source,
        )
    })?;
    Ok(())
}
