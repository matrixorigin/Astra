use super::criteria_proposal_repository::decode_recorded;
use super::proposal_queue::SELECT_WORK_PROPOSAL;
use super::repository::{
    CriteriaAcceptanceMetadata, DatabaseWorkRepository, WorkProposalBasisResource,
    WorkRepositoryError, apply_prepared_criteria_change, prepare_criteria_change,
    rollback_transaction,
};
use super::{
    NewWorkCriteriaProposal, RecordedWorkCriteriaProposal, WorkCriteriaChange,
    WorkCriteriaProposalAcceptance, WorkCriteriaProposalRejection, WorkCriteriaProposalResolution,
    WorkEventKind, WorkProposalStatus,
};
use chrono::{DateTime, Utc};
use sqlx::{MySql, Row, query};

trait CriteriaProposalDecision {
    fn owner_id(&self) -> &super::WorkOwnerId;
    fn work_id(&self) -> &super::WorkId;
    fn branch_id(&self) -> &super::WorkBranchId;
    fn proposal_id(&self) -> &super::WorkProposalId;
    fn payload_hash(&self) -> &super::WorkContentHash;
    fn expected_work_revision(&self) -> super::WorkRevision;
    fn expected_goal_revision(&self) -> super::GoalRevision;
    fn expected_criteria_set_revision(&self) -> super::CriterionSetRevision;
    fn expected_branch_revision(&self) -> super::WorkBranchRevision;
    fn expected_graph_revision(&self) -> super::GraphRevision;
}

macro_rules! impl_decision {
    ($type:ty) => {
        impl CriteriaProposalDecision for $type {
            fn owner_id(&self) -> &super::WorkOwnerId {
                &self.owner_id
            }
            fn work_id(&self) -> &super::WorkId {
                &self.work_id
            }
            fn branch_id(&self) -> &super::WorkBranchId {
                &self.branch_id
            }
            fn proposal_id(&self) -> &super::WorkProposalId {
                &self.proposal_id
            }
            fn payload_hash(&self) -> &super::WorkContentHash {
                &self.payload_hash
            }
            fn expected_work_revision(&self) -> super::WorkRevision {
                self.expected_work_revision
            }
            fn expected_goal_revision(&self) -> super::GoalRevision {
                self.expected_goal_revision
            }
            fn expected_criteria_set_revision(&self) -> super::CriterionSetRevision {
                self.expected_criteria_set_revision
            }
            fn expected_branch_revision(&self) -> super::WorkBranchRevision {
                self.expected_branch_revision
            }
            fn expected_graph_revision(&self) -> super::GraphRevision {
                self.expected_graph_revision
            }
        }
    };
}

impl_decision!(WorkCriteriaProposalAcceptance);
impl_decision!(WorkCriteriaProposalRejection);

fn validate_decision_identity(
    recorded: &RecordedWorkCriteriaProposal,
    decision: &impl CriteriaProposalDecision,
) -> Result<(), WorkRepositoryError> {
    let stored = &recorded.proposal;
    if stored.branch_id != *decision.branch_id() {
        return Err(WorkRepositoryError::NotFound);
    }
    for (matches, resource) in [
        (
            recorded.payload_hash == *decision.payload_hash(),
            WorkProposalBasisResource::ProposalPayloadHash,
        ),
        (
            stored.expected_work_revision == decision.expected_work_revision(),
            WorkProposalBasisResource::WorkRevision,
        ),
        (
            stored.expected_goal_revision == decision.expected_goal_revision(),
            WorkProposalBasisResource::GoalRevision,
        ),
        (
            stored.expected_criteria_set_revision == decision.expected_criteria_set_revision(),
            WorkProposalBasisResource::CriterionSetRevision,
        ),
        (
            stored.expected_branch_revision == decision.expected_branch_revision(),
            WorkProposalBasisResource::BranchRevision,
        ),
        (
            stored.expected_graph_revision == decision.expected_graph_revision(),
            WorkProposalBasisResource::GraphRevision,
        ),
    ] {
        if !matches {
            return Err(WorkRepositoryError::InvalidWorkProposalBasis { resource });
        }
    }
    Ok(())
}

async fn lock_proposal(
    transaction: &mut sqlx::Transaction<'_, MySql>,
    decision: &impl CriteriaProposalDecision,
) -> Result<RecordedWorkCriteriaProposal, WorkRepositoryError> {
    query(
        "SELECT last_proposal_seq FROM work_proposal_sequences
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? FOR UPDATE",
    )
    .bind(decision.owner_id().as_str())
    .bind(decision.work_id().as_str())
    .bind(decision.branch_id().as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("lock Work proposal sequence", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let select = format!("{SELECT_WORK_PROPOSAL} FOR UPDATE");
    let row = query(&select)
        .bind(decision.owner_id().as_str())
        .bind(decision.work_id().as_str())
        .bind(decision.proposal_id().as_str())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(|source| WorkRepositoryError::persistence("lock criteria proposal", source))?
        .ok_or(WorkRepositoryError::NotFound)?;
    let recorded = decode_recorded(&row)?;
    validate_decision_identity(&recorded, decision)?;
    Ok(recorded)
}

async fn expire_if_needed(
    transaction: &mut sqlx::Transaction<'_, MySql>,
    recorded: &RecordedWorkCriteriaProposal,
    resolved_at: DateTime<Utc>,
) -> Result<bool, WorkRepositoryError> {
    if recorded.expires_at > resolved_at {
        return Ok(false);
    }
    let expired = query(
        "UPDATE work_proposals
         SET status = 'expired', resolution_ref = 'proposal-retention-expiry', resolved_at = ?
         WHERE owner_id = ? AND work_id = ? AND proposal_id = ? AND status = 'pending'",
    )
    .bind(resolved_at.naive_utc())
    .bind(recorded.proposal.owner_id.as_str())
    .bind(recorded.proposal.work_id.as_str())
    .bind(recorded.proposal.proposal_id.as_str())
    .execute(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("expire criteria proposal", source))?;
    if expired.rows_affected() != 1 {
        return Err(WorkRepositoryError::corrupt(
            "criteria proposal",
            std::io::Error::other("locked pending proposal was not expired exactly once"),
        ));
    }
    Ok(true)
}

async fn lock_current_basis(
    transaction: &mut sqlx::Transaction<'_, MySql>,
    proposal: &NewWorkCriteriaProposal,
) -> Result<(), WorkRepositoryError> {
    let work = query(
        "SELECT w.work_revision, w.current_goal_revision, w.current_criteria_set_revision,
                CASE WHEN w.archived_at IS NULL THEN 0 ELSE 1 END AS work_archived,
                g.revision AS materialized_goal_revision,
                cs.revision AS materialized_criteria_set_revision
         FROM works w
         LEFT JOIN work_goal_revisions g
           ON g.owner_id = w.owner_id AND g.work_id = w.work_id
          AND g.revision = w.current_goal_revision
         LEFT JOIN work_criterion_sets cs
           ON cs.owner_id = w.owner_id AND cs.work_id = w.work_id
          AND cs.revision = w.current_criteria_set_revision
         WHERE w.owner_id = ? AND w.work_id = ? LIMIT 1 FOR UPDATE",
    )
    .bind(proposal.owner_id.as_str())
    .bind(proposal.work_id.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::persistence("lock criteria proposal Work basis", source)
    })?
    .ok_or(WorkRepositoryError::NotFound)?;
    let work_integer = |field: &'static str| {
        work.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("criteria proposal Work basis", source))
    };
    let work_optional = |field: &'static str| {
        work.try_get::<Option<i64>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("criteria proposal Work basis", source))
    };
    if work_integer("work_archived")? != 0 {
        return Err(WorkRepositoryError::Archived);
    }
    for (matches, resource) in [
        (
            work_integer("work_revision")? == proposal.expected_work_revision.get(),
            WorkProposalBasisResource::WorkRevision,
        ),
        (
            work_integer("current_goal_revision")? == proposal.expected_goal_revision.get(),
            WorkProposalBasisResource::GoalRevision,
        ),
        (
            work_integer("current_criteria_set_revision")?
                == proposal.expected_criteria_set_revision.get(),
            WorkProposalBasisResource::CriterionSetRevision,
        ),
    ] {
        if !matches {
            return Err(WorkRepositoryError::InvalidWorkProposalBasis { resource });
        }
    }
    if work_optional("materialized_goal_revision")?.is_none()
        || work_optional("materialized_criteria_set_revision")?.is_none()
    {
        return Err(WorkRepositoryError::corrupt(
            "criteria proposal Work basis",
            std::io::Error::other("current Goal or criterion-set revision is missing"),
        ));
    }

    let branch = query(
        "SELECT b.branch_revision, b.goal_revision_ref, b.criteria_set_revision_ref,
                b.current_graph_revision,
                CASE WHEN b.archived_at IS NULL THEN 0 ELSE 1 END AS branch_archived,
                gr.revision AS materialized_graph_revision
         FROM work_branches b
         LEFT JOIN work_graph_revisions gr
           ON gr.owner_id = b.owner_id AND gr.work_id = b.work_id
          AND gr.revision = b.current_graph_revision
         WHERE b.owner_id = ? AND b.work_id = ? AND b.branch_id = ?
         LIMIT 1 FOR UPDATE",
    )
    .bind(proposal.owner_id.as_str())
    .bind(proposal.work_id.as_str())
    .bind(proposal.branch_id.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::persistence("lock criteria proposal branch basis", source)
    })?
    .ok_or(WorkRepositoryError::NotFound)?;
    let branch_integer = |field: &'static str| {
        branch.try_get::<i64, _>(field).map_err(|source| {
            WorkRepositoryError::corrupt("criteria proposal branch basis", source)
        })
    };
    if branch_integer("branch_archived")? != 0 {
        return Err(WorkRepositoryError::Archived);
    }
    for (matches, resource) in [
        (
            branch_integer("branch_revision")? == proposal.expected_branch_revision.get(),
            WorkProposalBasisResource::BranchRevision,
        ),
        (
            branch_integer("goal_revision_ref")? == proposal.expected_goal_revision.get(),
            WorkProposalBasisResource::BranchGoalRevision,
        ),
        (
            branch_integer("criteria_set_revision_ref")?
                == proposal.expected_criteria_set_revision.get(),
            WorkProposalBasisResource::BranchCriterionSetRevision,
        ),
        (
            branch_integer("current_graph_revision")? == proposal.expected_graph_revision.get(),
            WorkProposalBasisResource::GraphRevision,
        ),
        (
            branch_integer("materialized_graph_revision")?
                == proposal.expected_graph_revision.get(),
            WorkProposalBasisResource::GraphRevision,
        ),
    ] {
        if !matches {
            return Err(WorkRepositoryError::InvalidWorkProposalBasis { resource });
        }
    }
    Ok(())
}

pub(super) async fn accept_criteria_proposal(
    repository: &DatabaseWorkRepository,
    acceptance: WorkCriteriaProposalAcceptance,
) -> Result<RecordedWorkCriteriaProposal, WorkRepositoryError> {
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin criteria proposal acceptance", source)
    })?;
    let mut recorded = lock_proposal(&mut transaction, &acceptance).await?;
    if recorded.status == WorkProposalStatus::Accepted
        && recorded
            .resolution
            .as_ref()
            .is_some_and(|resolution| resolution.resolution_ref == acceptance.resolution_ref)
    {
        transaction.rollback().await.map_err(|source| {
            WorkRepositoryError::persistence("release accepted criteria proposal lock", source)
        })?;
        return Ok(recorded);
    }
    if recorded.status != WorkProposalStatus::Pending {
        return Err(rollback_transaction(
            transaction,
            "rollback resolved criteria proposal acceptance",
            WorkRepositoryError::WorkProposalAlreadyResolved {
                status: recorded.status,
            },
        )
        .await);
    }
    let resolved_at = DateTime::<Utc>::from_timestamp_micros(Utc::now().timestamp_micros())
        .expect("current timestamp");
    if expire_if_needed(&mut transaction, &recorded, resolved_at).await? {
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit criteria proposal expiry", source)
        })?;
        return Err(WorkRepositoryError::WorkProposalAlreadyResolved {
            status: WorkProposalStatus::Expired,
        });
    }

    let result = async {
        lock_current_basis(&mut transaction, &recorded.proposal).await?;
        let prepared = prepare_criteria_change(WorkCriteriaChange {
            owner_id: recorded.proposal.owner_id.clone(),
            work_id: recorded.proposal.work_id.clone(),
            expected_work_revision: recorded.proposal.expected_work_revision,
            expected_criteria_set_revision: recorded.proposal.expected_criteria_set_revision,
            members: recorded.proposal.change_members(),
            source_ref: acceptance.resolution_ref.clone(),
            reason: None,
        })?;
        let metadata = CriteriaAcceptanceMetadata {
            definition_source_kind: recorded.proposal.source_kind.as_str(),
            definition_source_ref: &recorded.proposal.source_ref,
            accepted_by_kind: "user",
            accepted_by_id: acceptance.owner_id.as_str(),
            event_source_ref: &acceptance.resolution_ref,
            reason: None,
        };
        apply_prepared_criteria_change(&mut transaction, &prepared, &metadata).await?;
        let update = query(
            "UPDATE work_proposals
             SET status = 'accepted', resolution_ref = ?, resolved_at = ?,
                 result_work_revision = ?, result_criteria_set_revision = ?
             WHERE owner_id = ? AND work_id = ? AND proposal_id = ? AND status = 'pending'",
        )
        .bind(acceptance.resolution_ref.as_str())
        .bind(resolved_at.naive_utc())
        .bind(prepared.next_work_revision.get())
        .bind(prepared.next_set_revision.get())
        .bind(acceptance.owner_id.as_str())
        .bind(acceptance.work_id.as_str())
        .bind(acceptance.proposal_id.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(|source| WorkRepositoryError::persistence("accept criteria proposal", source))?;
        if update.rows_affected() != 1 {
            return Err(WorkRepositoryError::corrupt(
                "criteria proposal",
                std::io::Error::other("locked pending proposal was not accepted exactly once"),
            ));
        }
        Ok((prepared.next_work_revision, prepared.next_set_revision))
    }
    .await;
    let (result_work_revision, result_criteria_set_revision) = match result {
        Ok(result) => result,
        Err(error) => {
            return Err(rollback_transaction(
                transaction,
                "rollback criteria proposal acceptance",
                error,
            )
            .await);
        }
    };
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit criteria proposal acceptance", source)
    })?;
    recorded.status = WorkProposalStatus::Accepted;
    recorded.resolution = Some(WorkCriteriaProposalResolution {
        resolution_ref: acceptance.resolution_ref,
        resolved_at,
        result_work_revision: Some(result_work_revision),
        result_criteria_set_revision: Some(result_criteria_set_revision),
    });
    Ok(recorded)
}

pub(super) async fn reject_criteria_proposal(
    repository: &DatabaseWorkRepository,
    rejection: WorkCriteriaProposalRejection,
) -> Result<RecordedWorkCriteriaProposal, WorkRepositoryError> {
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin criteria proposal rejection", source)
    })?;
    let mut recorded = lock_proposal(&mut transaction, &rejection).await?;
    if recorded.status == WorkProposalStatus::Rejected
        && recorded
            .resolution
            .as_ref()
            .is_some_and(|resolution| resolution.resolution_ref == rejection.resolution_ref)
    {
        transaction.rollback().await.map_err(|source| {
            WorkRepositoryError::persistence("release rejected criteria proposal lock", source)
        })?;
        return Ok(recorded);
    }
    if recorded.status != WorkProposalStatus::Pending {
        return Err(rollback_transaction(
            transaction,
            "rollback resolved criteria proposal rejection",
            WorkRepositoryError::WorkProposalAlreadyResolved {
                status: recorded.status,
            },
        )
        .await);
    }
    let resolved_at = DateTime::<Utc>::from_timestamp_micros(Utc::now().timestamp_micros())
        .expect("current timestamp");
    if expire_if_needed(&mut transaction, &recorded, resolved_at).await? {
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit criteria proposal expiry", source)
        })?;
        return Err(WorkRepositoryError::WorkProposalAlreadyResolved {
            status: WorkProposalStatus::Expired,
        });
    }
    let update = query(
        "UPDATE work_proposals
         SET status = 'rejected', resolution_ref = ?, resolved_at = ?
         WHERE owner_id = ? AND work_id = ? AND proposal_id = ? AND status = 'pending'",
    )
    .bind(rejection.resolution_ref.as_str())
    .bind(resolved_at.naive_utc())
    .bind(rejection.owner_id.as_str())
    .bind(rejection.work_id.as_str())
    .bind(rejection.proposal_id.as_str())
    .execute(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("reject criteria proposal", source))?;
    if update.rows_affected() != 1 {
        return Err(rollback_transaction(
            transaction,
            "rollback invalid criteria proposal rejection",
            WorkRepositoryError::corrupt(
                "criteria proposal",
                std::io::Error::other("locked pending proposal was not rejected exactly once"),
            ),
        )
        .await);
    }
    if let Err(error) = super::events_repository::append_event(
        &mut transaction,
        &super::events::NewWorkEvent {
            owner_id: recorded.proposal.owner_id.clone(),
            work_id: recorded.proposal.work_id.clone(),
            branch_id: Some(recorded.proposal.branch_id.clone()),
            kind: WorkEventKind::ProposalRejected,
            work_revision: Some(recorded.proposal.expected_work_revision),
            goal_revision: Some(recorded.proposal.expected_goal_revision),
            criterion_set_revision: Some(recorded.proposal.expected_criteria_set_revision),
            branch_revision: Some(recorded.proposal.expected_branch_revision),
            graph_revision: Some(recorded.proposal.expected_graph_revision),
            source_ref: rejection.resolution_ref.clone(),
        },
    )
    .await
    {
        return Err(rollback_transaction(
            transaction,
            "rollback criteria proposal rejection event",
            error,
        )
        .await);
    }
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit criteria proposal rejection", source)
    })?;
    recorded.status = WorkProposalStatus::Rejected;
    recorded.resolution = Some(WorkCriteriaProposalResolution {
        resolution_ref: rejection.resolution_ref,
        resolved_at,
        result_work_revision: None,
        result_criteria_set_revision: None,
    });
    Ok(recorded)
}
