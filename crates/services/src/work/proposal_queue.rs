use super::proposal::{
    WORK_PROPOSAL_MAX_PENDING_PER_BRANCH, WORK_PROPOSAL_RETAINED_TERMINAL_PER_BRANCH,
    WORK_PROPOSAL_TTL_DAYS,
};
use super::repository::{
    DatabaseWorkRepository, WorkConflictResource, WorkRepositoryError, invalid_mutation,
    rollback_transaction,
};
use super::{
    CriterionSetRevision, GoalRevision, GraphRevision, WorkBranchId, WorkBranchRevision,
    WorkChangeRef, WorkContentHash, WorkEventKind, WorkId, WorkOwnerId, WorkProposalId,
    WorkProposalKind, WorkProposalSourceKind, WorkRevision,
};
use chrono::{DateTime, Duration, Utc};
use sqlx::mysql::MySqlRow;
use sqlx::{Row, query};

pub(super) const SELECT_WORK_PROPOSAL: &str =
    "SELECT owner_id, work_id, proposal_id, branch_id, proposal_seq, proposal_kind,
            expected_work_revision, expected_goal_revision,
            expected_criteria_set_revision, expected_branch_revision,
            expected_graph_revision, payload_json, payload_hash,
            item_change_count, dependency_change_count, criterion_count, source_kind, source_ref,
            status, resolution_ref,
            result_work_revision, result_criteria_set_revision,
            result_branch_revision, result_graph_revision,
            DATE_FORMAT(proposed_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS proposed_at,
            DATE_FORMAT(expires_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS expires_at,
            DATE_FORMAT(resolved_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS resolved_at
     FROM work_proposals
     WHERE owner_id = ? AND work_id = ? AND proposal_id = ? LIMIT 1";

pub(super) struct WorkProposalEnvelope<'a> {
    pub(super) owner_id: &'a WorkOwnerId,
    pub(super) work_id: &'a WorkId,
    pub(super) branch_id: &'a WorkBranchId,
    pub(super) proposal_id: &'a WorkProposalId,
    pub(super) proposal_kind: WorkProposalKind,
    pub(super) expected_work_revision: WorkRevision,
    pub(super) expected_goal_revision: GoalRevision,
    pub(super) expected_criteria_set_revision: CriterionSetRevision,
    pub(super) expected_branch_revision: WorkBranchRevision,
    pub(super) expected_graph_revision: GraphRevision,
    pub(super) payload_json: &'a str,
    pub(super) payload_hash: &'a WorkContentHash,
    pub(super) item_change_count: Option<i32>,
    pub(super) dependency_change_count: Option<i32>,
    pub(super) criterion_count: Option<i32>,
    pub(super) source_kind: WorkProposalSourceKind,
    pub(super) source_ref: &'a WorkChangeRef,
    pub(super) event_kind: WorkEventKind,
}

pub(super) struct InsertedProposal {
    pub(super) proposal_seq: i64,
    pub(super) proposed_at: DateTime<Utc>,
    pub(super) expires_at: DateTime<Utc>,
}

pub(super) enum ProposalAdmission {
    Existing(MySqlRow),
    Inserted(InsertedProposal),
}

async fn load_existing(
    repository: &DatabaseWorkRepository,
    envelope: &WorkProposalEnvelope<'_>,
) -> Result<Option<MySqlRow>, WorkRepositoryError> {
    query(SELECT_WORK_PROPOSAL)
        .bind(envelope.owner_id.as_str())
        .bind(envelope.work_id.as_str())
        .bind(envelope.proposal_id.as_str())
        .fetch_optional(repository.pool.get())
        .await
        .map_err(|source| WorkRepositoryError::persistence("load Work proposal", source))
}

pub(super) async fn admit_proposal(
    repository: &DatabaseWorkRepository,
    envelope: WorkProposalEnvelope<'_>,
) -> Result<ProposalAdmission, WorkRepositoryError> {
    let proposed_at = DateTime::<Utc>::from_timestamp_micros(Utc::now().timestamp_micros())
        .expect("current timestamp");
    let expires_at = proposed_at + Duration::days(WORK_PROPOSAL_TTL_DAYS);
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin Work proposal transaction", source)
    })?;
    let sequence = query(
        "SELECT last_proposal_seq FROM work_proposal_sequences
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? FOR UPDATE",
    )
    .bind(envelope.owner_id.as_str())
    .bind(envelope.work_id.as_str())
    .bind(envelope.branch_id.as_str())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("lock Work proposal sequence", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let last_seq: i64 = sequence
        .try_get("last_proposal_seq")
        .map_err(|source| WorkRepositoryError::corrupt("Work proposal sequence", source))?;
    let existing = query(SELECT_WORK_PROPOSAL)
        .bind(envelope.owner_id.as_str())
        .bind(envelope.work_id.as_str())
        .bind(envelope.proposal_id.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|source| WorkRepositoryError::persistence("recheck Work proposal", source))?;
    if let Some(row) = existing {
        transaction.rollback().await.map_err(|source| {
            WorkRepositoryError::persistence("release Work proposal sequence lock", source)
        })?;
        return Ok(ProposalAdmission::Existing(row));
    }
    query(
        "UPDATE work_proposals
         SET status = 'expired', resolution_ref = 'proposal-retention-expiry', resolved_at = ?
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?
           AND status = 'pending' AND expires_at <= ?",
    )
    .bind(proposed_at.naive_utc())
    .bind(envelope.owner_id.as_str())
    .bind(envelope.work_id.as_str())
    .bind(envelope.branch_id.as_str())
    .bind(proposed_at.naive_utc())
    .execute(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("expire pending Work proposals", source))?;
    let active_pending: i64 = query(
        "SELECT COUNT(*) AS active_pending FROM work_proposals
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND status = 'pending'",
    )
    .bind(envelope.owner_id.as_str())
    .bind(envelope.work_id.as_str())
    .bind(envelope.branch_id.as_str())
    .fetch_one(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("count pending Work proposals", source))?
    .try_get("active_pending")
    .map_err(|source| WorkRepositoryError::corrupt("Work proposal queue", source))?;
    if active_pending >= WORK_PROPOSAL_MAX_PENDING_PER_BRANCH {
        return Err(rollback_transaction(
            transaction,
            "rollback full Work proposal queue",
            WorkRepositoryError::WorkProposalCapacityExceeded,
        )
        .await);
    }
    let proposal_seq = last_seq.checked_add(1).ok_or_else(|| {
        WorkRepositoryError::corrupt(
            "Work proposal sequence",
            std::io::Error::other("proposal sequence is exhausted"),
        )
    })?;
    let sequence_update = query(
        "UPDATE work_proposal_sequences
         SET last_proposal_seq = ?, updated_at = ?
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(proposal_seq)
    .bind(proposed_at.naive_utc())
    .bind(envelope.owner_id.as_str())
    .bind(envelope.work_id.as_str())
    .bind(envelope.branch_id.as_str())
    .execute(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("advance Work proposal sequence", source))?;
    if sequence_update.rows_affected() != 1 {
        return Err(rollback_transaction(
            transaction,
            "rollback invalid Work proposal sequence update",
            WorkRepositoryError::corrupt(
                "Work proposal sequence",
                std::io::Error::other("locked proposal sequence disappeared during admission"),
            ),
        )
        .await);
    }
    let insert = query(
        "INSERT INTO work_proposals
         (owner_id, work_id, proposal_id, branch_id, proposal_seq, proposal_kind,
          expected_work_revision, expected_goal_revision, expected_criteria_set_revision,
          expected_branch_revision, expected_graph_revision, payload_json, payload_hash,
          item_change_count, dependency_change_count, criterion_count, source_kind, source_ref, status,
          proposed_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?)",
    )
    .bind(envelope.owner_id.as_str())
    .bind(envelope.work_id.as_str())
    .bind(envelope.proposal_id.as_str())
    .bind(envelope.branch_id.as_str())
    .bind(proposal_seq)
    .bind(envelope.proposal_kind.as_str())
    .bind(envelope.expected_work_revision.get())
    .bind(envelope.expected_goal_revision.get())
    .bind(envelope.expected_criteria_set_revision.get())
    .bind(envelope.expected_branch_revision.get())
    .bind(envelope.expected_graph_revision.get())
    .bind(envelope.payload_json)
    .bind(envelope.payload_hash.as_str())
    .bind(envelope.item_change_count)
    .bind(envelope.dependency_change_count)
    .bind(envelope.criterion_count)
    .bind(envelope.source_kind.as_str())
    .bind(envelope.source_ref.as_str())
    .bind(proposed_at.naive_utc())
    .bind(expires_at.naive_utc())
    .execute(&mut *transaction)
    .await;
    if let Err(source) = insert {
        let unique = source
            .as_database_error()
            .is_some_and(|error| error.is_unique_violation());
        let error = WorkRepositoryError::insert(
            "insert Work proposal",
            WorkConflictResource::WorkProposalIdentity,
            source,
        );
        transaction.rollback().await.map_err(|source| {
            WorkRepositoryError::persistence("rollback Work proposal transaction", source)
        })?;
        if unique && let Some(existing) = load_existing(repository, &envelope).await? {
            return Ok(ProposalAdmission::Existing(existing));
        }
        return Err(error);
    }
    let event_source =
        WorkChangeRef::parse(envelope.proposal_id.as_str()).map_err(invalid_mutation)?;
    if let Err(error) = super::events_repository::append_event(
        &mut transaction,
        &super::events::NewWorkEvent {
            owner_id: envelope.owner_id.clone(),
            work_id: envelope.work_id.clone(),
            branch_id: Some(envelope.branch_id.clone()),
            kind: envelope.event_kind,
            work_revision: Some(envelope.expected_work_revision),
            goal_revision: Some(envelope.expected_goal_revision),
            criterion_set_revision: Some(envelope.expected_criteria_set_revision),
            branch_revision: Some(envelope.expected_branch_revision),
            graph_revision: Some(envelope.expected_graph_revision),
            source_ref: event_source,
        },
    )
    .await
    {
        return Err(rollback_transaction(
            transaction,
            "rollback Work proposal event transaction",
            error,
        )
        .await);
    }
    query(
        "DELETE FROM work_proposals
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?
           AND status <> 'pending' AND proposal_seq <= ?",
    )
    .bind(envelope.owner_id.as_str())
    .bind(envelope.work_id.as_str())
    .bind(envelope.branch_id.as_str())
    .bind(proposal_seq - WORK_PROPOSAL_RETAINED_TERMINAL_PER_BRANCH)
    .execute(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("prune terminal Work proposals", source))?;
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit Work proposal transaction", source)
    })?;
    Ok(ProposalAdmission::Inserted(InsertedProposal {
        proposal_seq,
        proposed_at,
        expires_at,
    }))
}
