use super::proposal_repository::decode_recorded;
use super::repository::{
    DatabaseWorkRepository, WorkProposalBasisResource, WorkRepositoryError, invalid_mutation,
    rollback_transaction,
};
use super::{
    NewWorkPlanProposal, RecordedWorkPlanProposal, WorkChangeRef, WorkGraphChange,
    WorkGraphItemChange, WorkPlanProposalAcceptance, WorkPlanProposalResolution,
    WorkProposalStatus,
};
use chrono::{DateTime, Utc};
use sqlx::{MySql, Row, query};
use std::collections::{BTreeMap, BTreeSet};

fn validate_acceptance_identity(
    proposal: &RecordedWorkPlanProposal,
    acceptance: &WorkPlanProposalAcceptance,
) -> Result<(), WorkRepositoryError> {
    let stored = &proposal.proposal;
    if stored.branch_id != acceptance.branch_id {
        return Err(WorkRepositoryError::InvalidWorkProposalBasis {
            resource: WorkProposalBasisResource::BranchIdentity,
        });
    }
    for (matches, resource) in [
        (
            proposal.payload_hash == acceptance.payload_hash,
            WorkProposalBasisResource::ProposalPayloadHash,
        ),
        (
            stored.expected_work_revision == acceptance.expected_work_revision,
            WorkProposalBasisResource::WorkRevision,
        ),
        (
            stored.expected_goal_revision == acceptance.expected_goal_revision,
            WorkProposalBasisResource::GoalRevision,
        ),
        (
            stored.expected_criteria_set_revision == acceptance.expected_criteria_set_revision,
            WorkProposalBasisResource::CriterionSetRevision,
        ),
        (
            stored.expected_branch_revision == acceptance.expected_branch_revision,
            WorkProposalBasisResource::BranchRevision,
        ),
        (
            stored.expected_graph_revision == acceptance.expected_graph_revision,
            WorkProposalBasisResource::GraphRevision,
        ),
    ] {
        if !matches {
            return Err(WorkRepositoryError::InvalidWorkProposalBasis { resource });
        }
    }
    Ok(())
}

async fn lock_acceptance_work_basis(
    transaction: &mut sqlx::Transaction<'_, MySql>,
    proposal: &NewWorkPlanProposal,
) -> Result<(), WorkRepositoryError> {
    let row = query(
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
    .map_err(|source| WorkRepositoryError::persistence("lock proposal Work basis", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let integer = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("plan proposal Work basis", source))
    };
    let optional_integer = |field: &'static str| {
        row.try_get::<Option<i64>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("plan proposal Work basis", source))
    };
    if integer("work_archived")? != 0 {
        return Err(WorkRepositoryError::Archived);
    }
    for (matches, resource) in [
        (
            integer("work_revision")? == proposal.expected_work_revision.get(),
            WorkProposalBasisResource::WorkRevision,
        ),
        (
            integer("current_goal_revision")? == proposal.expected_goal_revision.get(),
            WorkProposalBasisResource::GoalRevision,
        ),
        (
            integer("current_criteria_set_revision")?
                == proposal.expected_criteria_set_revision.get(),
            WorkProposalBasisResource::CriterionSetRevision,
        ),
    ] {
        if !matches {
            return Err(WorkRepositoryError::InvalidWorkProposalBasis { resource });
        }
    }
    if optional_integer("materialized_goal_revision")?.is_none() {
        return Err(WorkRepositoryError::corrupt(
            "Work goal revision",
            std::io::Error::other("current Work goal revision is missing"),
        ));
    }
    if optional_integer("materialized_criteria_set_revision")?.is_none() {
        return Err(WorkRepositoryError::corrupt(
            "Work criterion set revision",
            std::io::Error::other("current Work criterion set revision is missing"),
        ));
    }
    Ok(())
}

async fn load_acceptance_graph_change(
    transaction: &mut sqlx::Transaction<'_, MySql>,
    proposal: &NewWorkPlanProposal,
) -> Result<WorkGraphChange, WorkRepositoryError> {
    let row = query(
        "SELECT b.branch_revision, b.goal_revision_ref, b.criteria_set_revision_ref,
                b.current_graph_revision,
                CASE WHEN b.archived_at IS NULL THEN 0 ELSE 1 END AS branch_archived,
                gr.item_revision_manifest_json, gr.item_count,
                gr.edge_manifest_json, gr.edge_count, gr.manifest_hash
         FROM work_branches b
         LEFT JOIN work_graph_revisions gr
           ON gr.owner_id = b.owner_id AND gr.work_id = b.work_id
          AND gr.revision = b.current_graph_revision
         WHERE b.owner_id = ? AND b.work_id = ? AND b.branch_id = ? LIMIT 1",
    )
    .bind(proposal.owner_id.as_str())
    .bind(proposal.work_id.as_str())
    .bind(proposal.branch_id.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("load proposal graph basis", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let integer = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("plan proposal branch basis", source))
    };
    if integer("branch_archived")? != 0 {
        return Err(WorkRepositoryError::Archived);
    }
    for (matches, resource) in [
        (
            integer("branch_revision")? == proposal.expected_branch_revision.get(),
            WorkProposalBasisResource::BranchRevision,
        ),
        (
            integer("goal_revision_ref")? == proposal.expected_goal_revision.get(),
            WorkProposalBasisResource::BranchGoalRevision,
        ),
        (
            integer("criteria_set_revision_ref")? == proposal.expected_criteria_set_revision.get(),
            WorkProposalBasisResource::BranchCriterionSetRevision,
        ),
        (
            integer("current_graph_revision")? == proposal.expected_graph_revision.get(),
            WorkProposalBasisResource::GraphRevision,
        ),
    ] {
        if !matches {
            return Err(WorkRepositoryError::InvalidWorkProposalBasis { resource });
        }
    }
    let current = super::graph_repository::decode_persisted_graph(
        &row.try_get::<String, _>("item_revision_manifest_json")
            .map_err(|source| WorkRepositoryError::corrupt("Work graph", source))?,
        row.try_get("item_count")
            .map_err(|source| WorkRepositoryError::corrupt("Work graph", source))?,
        &row.try_get::<String, _>("edge_manifest_json")
            .map_err(|source| WorkRepositoryError::corrupt("Work graph", source))?,
        row.try_get("edge_count")
            .map_err(|source| WorkRepositoryError::corrupt("Work graph", source))?,
    )?;
    super::graph_repository::validate_persisted_graph_hash(
        &current,
        &row.try_get::<String, _>("manifest_hash")
            .map_err(|source| WorkRepositoryError::corrupt("Work graph", source))?,
    )?;
    let mut current_items = current
        .item_refs
        .into_iter()
        .map(|reference| (reference.item_id.clone(), reference))
        .collect::<BTreeMap<_, _>>();
    let mut revised = BTreeMap::new();
    for revision in &proposal.revisions {
        if current_items.get(&revision.item_id) != Some(&revision.expected_ref()) {
            return Err(WorkRepositoryError::InvalidWorkProposalBasis {
                resource: WorkProposalBasisResource::WorkItemRevision,
            });
        }
        current_items.remove(&revision.item_id);
        revised.insert(revision.item_id.clone(), revision.clone());
    }
    let mut items = current_items
        .into_values()
        .map(WorkGraphItemChange::Existing)
        .collect::<Vec<_>>();
    items.extend(revised.into_values().map(WorkGraphItemChange::Revised));
    items.extend(
        proposal
            .additions
            .iter()
            .cloned()
            .map(WorkGraphItemChange::New),
    );
    let mut edges = current.edges.into_iter().collect::<BTreeSet<_>>();
    for removal in &proposal.dependency_removals {
        if !edges.remove(removal) {
            return Err(WorkRepositoryError::InvalidWorkProposalBasis {
                resource: WorkProposalBasisResource::DependencyIdentity,
            });
        }
    }
    for addition in &proposal.dependencies {
        if !edges.insert(addition.clone()) {
            return Err(WorkRepositoryError::InvalidWorkProposalBasis {
                resource: WorkProposalBasisResource::DependencyIdentity,
            });
        }
    }
    Ok(WorkGraphChange {
        owner_id: proposal.owner_id.clone(),
        work_id: proposal.work_id.clone(),
        branch_id: proposal.branch_id.clone(),
        expected_branch_revision: proposal.expected_branch_revision,
        expected_graph_revision: proposal.expected_graph_revision,
        items,
        edges: edges.into_iter().collect(),
        source_ref: WorkChangeRef::parse(proposal.proposal_id.as_str())
            .map_err(invalid_mutation)?,
        reason: Some(proposal.reason.clone()),
    })
}

pub(super) async fn accept_plan_proposal(
    repository: &DatabaseWorkRepository,
    acceptance: WorkPlanProposalAcceptance,
) -> Result<RecordedWorkPlanProposal, WorkRepositoryError> {
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin plan proposal acceptance", source)
    })?;
    query(
        "SELECT last_proposal_seq FROM work_proposal_sequences
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? FOR UPDATE",
    )
    .bind(acceptance.owner_id.as_str())
    .bind(acceptance.work_id.as_str())
    .bind(acceptance.branch_id.as_str())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("lock plan proposal sequence", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let row = query(
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
         WHERE owner_id = ? AND work_id = ? AND proposal_id = ? LIMIT 1 FOR UPDATE",
    )
    .bind(acceptance.owner_id.as_str())
    .bind(acceptance.work_id.as_str())
    .bind(acceptance.proposal_id.as_str())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("lock plan proposal", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let mut recorded = decode_recorded(&row)?;
    validate_acceptance_identity(&recorded, &acceptance)?;
    if recorded.status == WorkProposalStatus::Accepted
        && recorded
            .resolution
            .as_ref()
            .is_some_and(|resolution| resolution.resolution_ref == acceptance.resolution_ref)
    {
        transaction.rollback().await.map_err(|source| {
            WorkRepositoryError::persistence("release accepted proposal lock", source)
        })?;
        return Ok(recorded);
    }
    if recorded.status != WorkProposalStatus::Pending {
        return Err(rollback_transaction(
            transaction,
            "rollback resolved proposal acceptance",
            WorkRepositoryError::WorkProposalAlreadyResolved {
                status: recorded.status,
            },
        )
        .await);
    }
    let resolved_at = DateTime::<Utc>::from_timestamp_micros(Utc::now().timestamp_micros())
        .expect("current timestamp");
    if recorded.expires_at <= resolved_at {
        let expired = query(
            "UPDATE work_proposals
             SET status = 'expired', resolution_ref = 'proposal-retention-expiry', resolved_at = ?
             WHERE owner_id = ? AND work_id = ? AND proposal_id = ? AND status = 'pending'",
        )
        .bind(resolved_at.naive_utc())
        .bind(acceptance.owner_id.as_str())
        .bind(acceptance.work_id.as_str())
        .bind(acceptance.proposal_id.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(|source| WorkRepositoryError::persistence("expire plan proposal", source))?;
        if expired.rows_affected() != 1 {
            return Err(rollback_transaction(
                transaction,
                "rollback invalid proposal expiry",
                WorkRepositoryError::corrupt(
                    "plan proposal",
                    std::io::Error::other("locked pending proposal was not expired exactly once"),
                ),
            )
            .await);
        }
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit plan proposal expiry", source)
        })?;
        return Err(WorkRepositoryError::WorkProposalAlreadyResolved {
            status: WorkProposalStatus::Expired,
        });
    }
    let result = async {
        lock_acceptance_work_basis(&mut transaction, &recorded.proposal).await?;
        let graph_change =
            load_acceptance_graph_change(&mut transaction, &recorded.proposal).await?;
        let prepared =
            super::graph_repository::prepare_graph_change(&mut transaction, graph_change).await?;
        let branch = super::graph_repository::apply_prepared_graph_change(
            &mut transaction,
            &prepared,
            recorded.proposal.source_kind.as_str(),
            recorded.proposal.proposal_id.as_str(),
            Some(recorded.payload_hash.as_str()),
        )
        .await?;
        super::graph_repository::retire_revised_item_attempts(&mut transaction, &prepared).await?;
        let branch_revision = branch.parts().branch_revision;
        let graph_revision = branch.parts().current_graph_revision;
        let update = query(
            "UPDATE work_proposals
             SET status = 'accepted', resolution_ref = ?, resolved_at = ?,
                 result_branch_revision = ?, result_graph_revision = ?
             WHERE owner_id = ? AND work_id = ? AND proposal_id = ? AND status = 'pending'",
        )
        .bind(acceptance.resolution_ref.as_str())
        .bind(resolved_at.naive_utc())
        .bind(branch_revision.get())
        .bind(graph_revision.get())
        .bind(acceptance.owner_id.as_str())
        .bind(acceptance.work_id.as_str())
        .bind(acceptance.proposal_id.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(|source| WorkRepositoryError::persistence("accept plan proposal", source))?;
        if update.rows_affected() != 1 {
            return Err(WorkRepositoryError::corrupt(
                "plan proposal",
                std::io::Error::other("locked pending proposal was not accepted exactly once"),
            ));
        }
        Ok::<_, WorkRepositoryError>((branch_revision, graph_revision))
    }
    .await;
    let (branch_revision, graph_revision) = match result {
        Ok(result) => result,
        Err(error) => {
            return Err(rollback_transaction(
                transaction,
                "rollback plan proposal acceptance",
                error,
            )
            .await);
        }
    };
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit plan proposal acceptance", source)
    })?;
    recorded.status = WorkProposalStatus::Accepted;
    recorded.resolution = Some(WorkPlanProposalResolution {
        resolution_ref: acceptance.resolution_ref,
        resolved_at,
        result_branch_revision: Some(branch_revision),
        result_graph_revision: Some(graph_revision),
    });
    Ok(recorded)
}
