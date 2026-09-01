use super::proposal::PLAN_PROPOSAL_MAX_PAYLOAD_BYTES;
use super::proposal_queue::{
    ProposalAdmission, SELECT_WORK_PROPOSAL, WorkProposalEnvelope, admit_proposal,
};
use super::repository::{
    DatabaseWorkRepository, WorkConflictResource, WorkProposalBasisResource, WorkRepositoryError,
    invalid_mutation,
};
use super::{
    CriterionSetRevision, GoalRevision, GraphRevision, NewWorkItem, NewWorkPlanProposal,
    RecordedWorkPlanProposal, WorkBranchId, WorkBranchRevision, WorkChangeReason, WorkChangeRef,
    WorkContentHash, WorkEventKind, WorkGraphItemChange, WorkId, WorkItemDeclarationState,
    WorkItemEdge, WorkItemEdgeKind, WorkItemId, WorkItemKind, WorkItemRevision,
    WorkItemRevisionChange, WorkItemText, WorkOwnerId, WorkPlanProposalResolution, WorkProposalId,
    WorkProposalKind, WorkProposalSourceKind, WorkProposalStatus, WorkRevision,
};
use serde::Deserialize;
use sqlx::mysql::MySqlRow;
use sqlx::{MySql, QueryBuilder, Row, query};
use std::collections::BTreeSet;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemEdgeWire {
    predecessor_item_id: String,
    successor_item_id: String,
    kind: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewWorkItemWire {
    item_id: String,
    kind: String,
    objective: String,
    expected_result: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevisedWorkItemWire {
    item_id: String,
    expected_revision: i64,
    kind: String,
    objective: String,
    expected_result: String,
    declaration_state: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanProposalPayloadWire {
    owner_id: String,
    work_id: String,
    branch_id: String,
    proposal_id: String,
    expected_work_revision: i64,
    expected_goal_revision: i64,
    expected_criteria_set_revision: i64,
    expected_branch_revision: i64,
    expected_graph_revision: i64,
    additions: Vec<NewWorkItemWire>,
    revisions: Vec<RevisedWorkItemWire>,
    dependencies: Vec<ItemEdgeWire>,
    dependency_removals: Vec<ItemEdgeWire>,
    reason: String,
    source_kind: String,
    source_ref: String,
}

fn corrupt_proposal_payload(
    source: impl std::error::Error + Send + Sync + 'static,
) -> WorkRepositoryError {
    WorkRepositoryError::corrupt("plan proposal payload", source)
}

fn corrupt_plan_proposal(
    source: impl std::error::Error + Send + Sync + 'static,
) -> WorkRepositoryError {
    WorkRepositoryError::corrupt("plan proposal", source)
}

fn decode_plan_proposal_payload(
    payload_json: &str,
) -> Result<NewWorkPlanProposal, WorkRepositoryError> {
    let wire: PlanProposalPayloadWire = serde_json::from_str(payload_json)
        .map_err(|source| WorkRepositoryError::corrupt("plan proposal payload", source))?;
    let additions = wire
        .additions
        .into_iter()
        .map(|item| {
            let kind = match item.kind.as_str() {
                "milestone" => WorkItemKind::Milestone,
                "task" => WorkItemKind::Task,
                _ => {
                    return Err(corrupt_proposal_payload(std::io::Error::other(
                        "unknown proposed WorkItem kind",
                    )));
                }
            };
            Ok(NewWorkItem {
                item_id: WorkItemId::parse(item.item_id).map_err(corrupt_proposal_payload)?,
                kind,
                objective: WorkItemText::parse(item.objective).map_err(corrupt_proposal_payload)?,
                expected_result: WorkItemText::parse(item.expected_result)
                    .map_err(corrupt_proposal_payload)?,
            })
        })
        .collect::<Result<Vec<_>, WorkRepositoryError>>()?;
    let dependencies = wire
        .dependencies
        .into_iter()
        .map(|edge| {
            if edge.kind != "dependency" {
                return Err(corrupt_proposal_payload(std::io::Error::other(
                    "unknown proposed WorkItem edge kind",
                )));
            }
            Ok(WorkItemEdge {
                predecessor_item_id: WorkItemId::parse(edge.predecessor_item_id)
                    .map_err(corrupt_proposal_payload)?,
                successor_item_id: WorkItemId::parse(edge.successor_item_id)
                    .map_err(corrupt_proposal_payload)?,
                kind: WorkItemEdgeKind::Dependency,
            })
        })
        .collect::<Result<Vec<_>, WorkRepositoryError>>()?;
    let revisions = wire
        .revisions
        .into_iter()
        .map(|item| {
            let kind = WorkItemKind::from_persisted(&item.kind).ok_or_else(|| {
                corrupt_proposal_payload(std::io::Error::other("unknown revised WorkItem kind"))
            })?;
            let declaration_state = WorkItemDeclarationState::from_persisted(
                &item.declaration_state,
            )
            .ok_or_else(|| {
                corrupt_proposal_payload(std::io::Error::other(
                    "unknown revised WorkItem declaration state",
                ))
            })?;
            Ok(WorkItemRevisionChange::new(
                WorkItemId::parse(item.item_id).map_err(corrupt_proposal_payload)?,
                WorkItemRevision::new(item.expected_revision).map_err(corrupt_proposal_payload)?,
                kind,
                WorkItemText::parse(item.objective).map_err(corrupt_proposal_payload)?,
                WorkItemText::parse(item.expected_result).map_err(corrupt_proposal_payload)?,
                declaration_state,
            ))
        })
        .collect::<Result<Vec<_>, WorkRepositoryError>>()?;
    let dependency_removals = wire
        .dependency_removals
        .into_iter()
        .map(|edge| {
            if edge.kind != "dependency" {
                return Err(corrupt_proposal_payload(std::io::Error::other(
                    "unknown removed WorkItem edge kind",
                )));
            }
            Ok(WorkItemEdge {
                predecessor_item_id: WorkItemId::parse(edge.predecessor_item_id)
                    .map_err(corrupt_proposal_payload)?,
                successor_item_id: WorkItemId::parse(edge.successor_item_id)
                    .map_err(corrupt_proposal_payload)?,
                kind: WorkItemEdgeKind::Dependency,
            })
        })
        .collect::<Result<Vec<_>, WorkRepositoryError>>()?;
    let source_kind =
        WorkProposalSourceKind::from_persisted(&wire.source_kind).ok_or_else(|| {
            corrupt_proposal_payload(std::io::Error::other("unknown plan proposal source kind"))
        })?;
    let proposal = NewWorkPlanProposal {
        owner_id: WorkOwnerId::parse(wire.owner_id).map_err(corrupt_proposal_payload)?,
        work_id: WorkId::parse(wire.work_id).map_err(corrupt_proposal_payload)?,
        branch_id: WorkBranchId::parse(wire.branch_id).map_err(corrupt_proposal_payload)?,
        proposal_id: WorkProposalId::parse(wire.proposal_id).map_err(corrupt_proposal_payload)?,
        expected_work_revision: WorkRevision::new(wire.expected_work_revision)
            .map_err(corrupt_proposal_payload)?,
        expected_goal_revision: GoalRevision::new(wire.expected_goal_revision)
            .map_err(corrupt_proposal_payload)?,
        expected_criteria_set_revision: CriterionSetRevision::new(
            wire.expected_criteria_set_revision,
        )
        .map_err(corrupt_proposal_payload)?,
        expected_branch_revision: WorkBranchRevision::new(wire.expected_branch_revision)
            .map_err(corrupt_proposal_payload)?,
        expected_graph_revision: GraphRevision::new(wire.expected_graph_revision)
            .map_err(corrupt_proposal_payload)?,
        additions,
        revisions,
        dependencies,
        dependency_removals,
        reason: WorkChangeReason::parse(wire.reason).map_err(corrupt_proposal_payload)?,
        source_kind,
        source_ref: WorkChangeRef::parse(wire.source_ref).map_err(corrupt_proposal_payload)?,
    }
    .canonicalized()
    .map_err(corrupt_proposal_payload)?;
    let canonical = super::repository::canonical_json("plan proposal payload", &proposal)?;
    if canonical != payload_json {
        return Err(corrupt_proposal_payload(std::io::Error::other(
            "persisted plan proposal payload is not canonical",
        )));
    }
    Ok(proposal)
}

async fn validate_basis(
    repository: &DatabaseWorkRepository,
    proposal: &NewWorkPlanProposal,
) -> Result<(), WorkRepositoryError> {
    let row = query(
        "SELECT w.work_revision, w.current_goal_revision, w.current_criteria_set_revision,
                CASE WHEN w.archived_at IS NULL THEN 0 ELSE 1 END AS work_archived,
                g.revision AS materialized_goal_revision,
                cs.revision AS materialized_criteria_set_revision,
                b.branch_revision, b.goal_revision_ref, b.criteria_set_revision_ref,
                b.current_graph_revision,
                CASE WHEN b.archived_at IS NULL THEN 0 ELSE 1 END AS branch_archived,
                gr.item_revision_manifest_json, gr.item_count,
                gr.edge_manifest_json, gr.edge_count, gr.manifest_hash
         FROM works w
         LEFT JOIN work_branches b
           ON b.owner_id = w.owner_id AND b.work_id = w.work_id AND b.branch_id = ?
         LEFT JOIN work_goal_revisions g
           ON g.owner_id = w.owner_id AND g.work_id = w.work_id
          AND g.revision = w.current_goal_revision
         LEFT JOIN work_criterion_sets cs
           ON cs.owner_id = w.owner_id AND cs.work_id = w.work_id
          AND cs.revision = w.current_criteria_set_revision
         LEFT JOIN work_graph_revisions gr
           ON gr.owner_id = b.owner_id AND gr.work_id = b.work_id
          AND gr.revision = b.current_graph_revision
         WHERE w.owner_id = ? AND w.work_id = ? LIMIT 1",
    )
    .bind(proposal.branch_id.as_str())
    .bind(proposal.owner_id.as_str())
    .bind(proposal.work_id.as_str())
    .fetch_optional(repository.pool.get())
    .await
    .map_err(|source| WorkRepositoryError::persistence("validate plan proposal basis", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let integer = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("plan proposal basis", source))
    };
    let optional_integer = |field: &'static str| {
        row.try_get::<Option<i64>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("plan proposal basis", source))
    };
    if integer("work_archived")? != 0 {
        return Err(WorkRepositoryError::Archived);
    }
    if optional_integer("branch_revision")?.is_none() {
        return Err(WorkRepositoryError::NotFound);
    }
    if optional_integer("branch_archived")?.unwrap_or(1) != 0 {
        return Err(WorkRepositoryError::Archived);
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
        (
            optional_integer("goal_revision_ref")? == Some(proposal.expected_goal_revision.get()),
            WorkProposalBasisResource::BranchGoalRevision,
        ),
        (
            optional_integer("criteria_set_revision_ref")?
                == Some(proposal.expected_criteria_set_revision.get()),
            WorkProposalBasisResource::BranchCriterionSetRevision,
        ),
        (
            optional_integer("branch_revision")? == Some(proposal.expected_branch_revision.get()),
            WorkProposalBasisResource::BranchRevision,
        ),
        (
            optional_integer("current_graph_revision")?
                == Some(proposal.expected_graph_revision.get()),
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

    let addition_ids = proposal
        .additions
        .iter()
        .map(|item| item.item_id.as_str())
        .collect::<Vec<_>>();
    let mut collisions = BTreeSet::new();
    if !addition_ids.is_empty() {
        let mut builder =
            QueryBuilder::<MySql>::new("SELECT item_id FROM work_items WHERE owner_id = ");
        builder
            .push_bind(proposal.owner_id.as_str())
            .push(" AND work_id = ")
            .push_bind(proposal.work_id.as_str())
            .push(" AND item_id IN (");
        let mut separated = builder.separated(", ");
        for item_id in addition_ids {
            separated.push_bind(item_id);
        }
        separated.push_unseparated(")");
        for row in builder
            .build()
            .fetch_all(repository.pool.get())
            .await
            .map_err(|source| {
                WorkRepositoryError::persistence("validate proposed WorkItem identities", source)
            })?
        {
            collisions.insert(
                row.try_get::<String, _>("item_id")
                    .map_err(|source| WorkRepositoryError::corrupt("WorkItem identity", source))?,
            );
        }
    }
    if !collisions.is_empty() {
        return Err(WorkRepositoryError::InvalidWorkProposalBasis {
            resource: WorkProposalBasisResource::NewItemIdentity,
        });
    }

    let mut current_items = current
        .item_refs
        .into_iter()
        .map(|reference| (reference.item_id.clone(), reference))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut revised = std::collections::BTreeMap::new();
    for revision in &proposal.revisions {
        if current_items.get(&revision.item_id) != Some(&revision.expected_ref()) {
            return Err(WorkRepositoryError::InvalidWorkProposalBasis {
                resource: WorkProposalBasisResource::WorkItemRevision,
            });
        }
        current_items.remove(&revision.item_id);
        let mut provisional = revision.clone();
        provisional.assign_result_revision(
            revision
                .expected_revision
                .checked_next()
                .map_err(invalid_mutation)?,
        );
        revised.insert(revision.item_id.clone(), provisional);
    }
    let mut combined_items = current_items
        .into_values()
        .map(WorkGraphItemChange::Existing)
        .collect::<Vec<_>>();
    combined_items.extend(revised.into_values().map(WorkGraphItemChange::Revised));
    combined_items.extend(
        proposal
            .additions
            .iter()
            .cloned()
            .map(WorkGraphItemChange::New),
    );
    let mut combined_edges = current.edges.into_iter().collect::<BTreeSet<_>>();
    for removal in &proposal.dependency_removals {
        if !combined_edges.remove(removal) {
            return Err(WorkRepositoryError::InvalidWorkProposalBasis {
                resource: WorkProposalBasisResource::DependencyIdentity,
            });
        }
    }
    for addition in &proposal.dependencies {
        if !combined_edges.insert(addition.clone()) {
            return Err(WorkRepositoryError::InvalidWorkProposalBasis {
                resource: WorkProposalBasisResource::DependencyIdentity,
            });
        }
    }
    let combined_edges = combined_edges.into_iter().collect::<Vec<_>>();
    super::graph::validate_and_canonicalize_graph(&combined_items, &combined_edges).map_err(
        |source| match source {
            super::WorkDomainError::UnknownWorkItemEdgeEndpoint { .. } => {
                WorkRepositoryError::InvalidWorkProposalBasis {
                    resource: WorkProposalBasisResource::DependencyEndpoint,
                }
            }
            other => invalid_mutation(other),
        },
    )?;
    Ok(())
}

fn payload(
    proposal: &NewWorkPlanProposal,
) -> Result<(String, WorkContentHash), WorkRepositoryError> {
    let json = super::repository::canonical_json("plan proposal payload", proposal)?;
    if json.len() > PLAN_PROPOSAL_MAX_PAYLOAD_BYTES {
        return Err(invalid_mutation(
            super::WorkDomainError::InvalidPlanProposal {
                violation: super::WorkPlanProposalViolation::PayloadTooLarge,
            },
        ));
    }
    let hash =
        WorkContentHash::parse(super::repository::content_hash(&json)).map_err(|message| {
            WorkRepositoryError::corrupt("plan proposal payload", std::io::Error::other(message))
        })?;
    Ok((json, hash))
}

async fn find_existing(
    repository: &DatabaseWorkRepository,
    proposal: &NewWorkPlanProposal,
    expected_hash: &WorkContentHash,
) -> Result<Option<RecordedWorkPlanProposal>, WorkRepositoryError> {
    let row = query(SELECT_WORK_PROPOSAL)
        .bind(proposal.owner_id.as_str())
        .bind(proposal.work_id.as_str())
        .bind(proposal.proposal_id.as_str())
        .fetch_optional(repository.pool.get())
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("load existing plan proposal", source)
        })?;
    let Some(row) = row else { return Ok(None) };
    let recorded = decode_recorded(&row)?;
    if &recorded.payload_hash != expected_hash || &recorded.proposal != proposal {
        return Err(WorkRepositoryError::Conflict {
            resource: WorkConflictResource::WorkProposalIdentity,
        });
    }
    Ok(Some(recorded))
}

pub(super) async fn load_plan_proposal(
    repository: &DatabaseWorkRepository,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    proposal_id: &WorkProposalId,
) -> Result<Option<RecordedWorkPlanProposal>, WorkRepositoryError> {
    query(SELECT_WORK_PROPOSAL)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(proposal_id.as_str())
        .fetch_optional(repository.pool.get())
        .await
        .map_err(|source| WorkRepositoryError::persistence("load plan proposal", source))?
        .as_ref()
        .map(decode_recorded)
        .transpose()
}

pub(super) fn decode_recorded(
    row: &MySqlRow,
) -> Result<RecordedWorkPlanProposal, WorkRepositoryError> {
    let proposal_kind = row
        .try_get::<String, _>("proposal_kind")
        .map_err(|source| WorkRepositoryError::corrupt("plan proposal", source))?;
    if WorkProposalKind::from_persisted(&proposal_kind) != Some(WorkProposalKind::PlanPatch) {
        return Err(WorkRepositoryError::Conflict {
            resource: WorkConflictResource::WorkProposalIdentity,
        });
    }
    let stored_hash = WorkContentHash::parse(
        row.try_get::<String, _>("payload_hash")
            .map_err(|source| WorkRepositoryError::corrupt("plan proposal", source))?,
    )
    .map_err(|message| {
        WorkRepositoryError::corrupt("plan proposal", std::io::Error::other(message))
    })?;
    let stored_payload = row
        .try_get::<String, _>("payload_json")
        .map_err(|source| WorkRepositoryError::corrupt("plan proposal payload", source))?;
    if super::repository::content_hash(&stored_payload) != stored_hash.as_str() {
        return Err(WorkRepositoryError::corrupt(
            "plan proposal payload",
            std::io::Error::other("stored payload does not match its content hash"),
        ));
    }
    let proposal = decode_plan_proposal_payload(&stored_payload)?;
    let string = |field: &'static str| {
        row.try_get::<String, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("plan proposal", source))
    };
    let integer = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("plan proposal", source))
    };
    let identity_is_coherent = string("owner_id")? == proposal.owner_id.as_str()
        && string("work_id")? == proposal.work_id.as_str()
        && string("proposal_id")? == proposal.proposal_id.as_str()
        && string("branch_id")? == proposal.branch_id.as_str()
        && integer("expected_work_revision")? == proposal.expected_work_revision.get()
        && integer("expected_goal_revision")? == proposal.expected_goal_revision.get()
        && integer("expected_criteria_set_revision")?
            == proposal.expected_criteria_set_revision.get()
        && integer("expected_branch_revision")? == proposal.expected_branch_revision.get()
        && integer("expected_graph_revision")? == proposal.expected_graph_revision.get()
        && string("source_kind")? == proposal.source_kind.as_str()
        && string("source_ref")? == proposal.source_ref.as_str();
    if !identity_is_coherent {
        return Err(WorkRepositoryError::corrupt(
            "plan proposal",
            std::io::Error::other("row identity or basis differs from its canonical payload"),
        ));
    }
    let item_change_count = row
        .try_get::<Option<i32>, _>("item_change_count")
        .map_err(|source| WorkRepositoryError::corrupt("plan proposal", source))?;
    let dependency_change_count = row
        .try_get::<Option<i32>, _>("dependency_change_count")
        .map_err(|source| WorkRepositoryError::corrupt("plan proposal", source))?;
    let criterion_count = row
        .try_get::<Option<i32>, _>("criterion_count")
        .map_err(|source| WorkRepositoryError::corrupt("plan proposal", source))?;
    if item_change_count != i32::try_from(proposal.additions.len() + proposal.revisions.len()).ok()
        || dependency_change_count
            != i32::try_from(proposal.dependencies.len() + proposal.dependency_removals.len()).ok()
        || criterion_count.is_some()
    {
        return Err(WorkRepositoryError::corrupt(
            "plan proposal",
            std::io::Error::other("payload summary counts do not match the canonical payload"),
        ));
    }
    let timestamp = |field: &'static str| {
        super::repository::decode_timestamp(
            "plan proposal",
            field,
            row.try_get(field)
                .map_err(|source| WorkRepositoryError::corrupt("plan proposal", source))?,
        )
    };
    let status = row
        .try_get::<String, _>("status")
        .map_err(|source| WorkRepositoryError::corrupt("plan proposal", source))?;
    let status = WorkProposalStatus::from_persisted(&status).ok_or_else(|| {
        WorkRepositoryError::corrupt(
            "plan proposal",
            std::io::Error::other("unknown proposal status"),
        )
    })?;
    let resolution_ref = row
        .try_get::<Option<String>, _>("resolution_ref")
        .map_err(|source| WorkRepositoryError::corrupt("plan proposal", source))?
        .map(WorkChangeRef::parse)
        .transpose()
        .map_err(corrupt_plan_proposal)?;
    let resolved_at = row
        .try_get::<Option<String>, _>("resolved_at")
        .map_err(|source| WorkRepositoryError::corrupt("plan proposal", source))?
        .map(|value| super::repository::decode_timestamp("plan proposal", "resolved_at", value))
        .transpose()?;
    let result_branch_revision = row
        .try_get::<Option<i64>, _>("result_branch_revision")
        .map_err(|source| WorkRepositoryError::corrupt("plan proposal", source))?
        .map(WorkBranchRevision::new)
        .transpose()
        .map_err(corrupt_plan_proposal)?;
    let result_work_revision = row
        .try_get::<Option<i64>, _>("result_work_revision")
        .map_err(|source| WorkRepositoryError::corrupt("plan proposal", source))?;
    let result_criteria_set_revision = row
        .try_get::<Option<i64>, _>("result_criteria_set_revision")
        .map_err(|source| WorkRepositoryError::corrupt("plan proposal", source))?;
    if result_work_revision.is_some() || result_criteria_set_revision.is_some() {
        return Err(WorkRepositoryError::corrupt(
            "plan proposal",
            std::io::Error::other("plan proposal contains criterion-set result revisions"),
        ));
    }
    let result_graph_revision = row
        .try_get::<Option<i64>, _>("result_graph_revision")
        .map_err(|source| WorkRepositoryError::corrupt("plan proposal", source))?
        .map(GraphRevision::new)
        .transpose()
        .map_err(corrupt_plan_proposal)?;
    let proposed_at = timestamp("proposed_at")?;
    let expires_at = timestamp("expires_at")?;
    if expires_at <= proposed_at
        || resolved_at
            .as_ref()
            .is_some_and(|resolved| resolved < &proposed_at)
    {
        return Err(WorkRepositoryError::corrupt(
            "plan proposal",
            std::io::Error::other("proposal timestamps violate lifecycle ordering"),
        ));
    }
    let accepted_result_is_coherent = if status == WorkProposalStatus::Accepted {
        let accepted_branch_revision = proposal
            .expected_branch_revision
            .checked_next()
            .map_err(corrupt_plan_proposal)?;
        result_branch_revision == Some(accepted_branch_revision)
            && result_graph_revision
                .is_some_and(|revision| revision > proposal.expected_graph_revision)
    } else {
        false
    };
    let resolution = match status {
        WorkProposalStatus::Pending
            if resolution_ref.is_none()
                && resolved_at.is_none()
                && result_branch_revision.is_none()
                && result_graph_revision.is_none() =>
        {
            None
        }
        WorkProposalStatus::Accepted
            if resolution_ref.is_some()
                && resolved_at.is_some()
                && result_branch_revision.is_some()
                && result_graph_revision.is_some()
                && accepted_result_is_coherent =>
        {
            Some(WorkPlanProposalResolution {
                resolution_ref: resolution_ref.expect("matched Some"),
                resolved_at: resolved_at.expect("matched Some"),
                result_branch_revision,
                result_graph_revision,
            })
        }
        WorkProposalStatus::Rejected
        | WorkProposalStatus::Stale
        | WorkProposalStatus::Superseded
        | WorkProposalStatus::Expired
            if resolution_ref.is_some()
                && resolved_at.is_some()
                && result_branch_revision.is_none()
                && result_graph_revision.is_none() =>
        {
            Some(WorkPlanProposalResolution {
                resolution_ref: resolution_ref.expect("matched Some"),
                resolved_at: resolved_at.expect("matched Some"),
                result_branch_revision: None,
                result_graph_revision: None,
            })
        }
        _ => {
            return Err(WorkRepositoryError::corrupt(
                "plan proposal",
                std::io::Error::other("proposal status and resolution fields are incoherent"),
            ));
        }
    };
    let proposal_seq: i64 = row
        .try_get("proposal_seq")
        .map_err(|source| WorkRepositoryError::corrupt("plan proposal", source))?;
    if proposal_seq < 1 {
        return Err(WorkRepositoryError::corrupt(
            "plan proposal",
            std::io::Error::other("proposal sequence must be positive"),
        ));
    }
    Ok(RecordedWorkPlanProposal {
        proposal,
        proposal_seq,
        payload_hash: stored_hash,
        status,
        proposed_at,
        expires_at,
        resolution,
    })
}

pub(super) async fn propose_plan(
    repository: &DatabaseWorkRepository,
    proposal: NewWorkPlanProposal,
) -> Result<RecordedWorkPlanProposal, WorkRepositoryError> {
    let proposal = proposal.canonicalized().map_err(invalid_mutation)?;
    let (payload_json, payload_hash) = payload(&proposal)?;
    if let Some(existing) = find_existing(repository, &proposal, &payload_hash).await? {
        return Ok(existing);
    }
    validate_basis(repository, &proposal).await?;
    match admit_proposal(
        repository,
        WorkProposalEnvelope {
            owner_id: &proposal.owner_id,
            work_id: &proposal.work_id,
            branch_id: &proposal.branch_id,
            proposal_id: &proposal.proposal_id,
            proposal_kind: WorkProposalKind::PlanPatch,
            expected_work_revision: proposal.expected_work_revision,
            expected_goal_revision: proposal.expected_goal_revision,
            expected_criteria_set_revision: proposal.expected_criteria_set_revision,
            expected_branch_revision: proposal.expected_branch_revision,
            expected_graph_revision: proposal.expected_graph_revision,
            payload_json: &payload_json,
            payload_hash: &payload_hash,
            item_change_count: Some(
                i32::try_from(proposal.additions.len() + proposal.revisions.len())
                    .expect("bounded item changes"),
            ),
            dependency_change_count: Some(
                i32::try_from(proposal.dependencies.len() + proposal.dependency_removals.len())
                    .expect("bounded dependency changes"),
            ),
            criterion_count: None,
            source_kind: proposal.source_kind,
            source_ref: &proposal.source_ref,
            event_kind: WorkEventKind::PlanProposed,
        },
    )
    .await?
    {
        ProposalAdmission::Existing(row) => {
            let recorded = decode_recorded(&row)?;
            if recorded.payload_hash == payload_hash && recorded.proposal == proposal {
                Ok(recorded)
            } else {
                Err(WorkRepositoryError::Conflict {
                    resource: WorkConflictResource::WorkProposalIdentity,
                })
            }
        }
        ProposalAdmission::Inserted(inserted) => Ok(RecordedWorkPlanProposal {
            proposal,
            proposal_seq: inserted.proposal_seq,
            payload_hash,
            status: WorkProposalStatus::Pending,
            proposed_at: inserted.proposed_at,
            expires_at: inserted.expires_at,
            resolution: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        CriterionSetRevision, GoalRevision, GraphRevision, WorkBranchId, WorkBranchRevision,
        WorkChangeRef, WorkDomainError, WorkId, WorkItemKind, WorkItemText, WorkOwnerId,
        WorkPlanProposalViolation, WorkProposalId, WorkProposalSourceKind, WorkRevision,
    };
    use super::*;

    #[test]
    fn encoded_payload_has_a_hard_byte_bound() {
        let text = WorkItemText::parse("x".repeat(super::super::graph::WORK_ITEM_TEXT_MAX_BYTES))
            .expect("maximum item text");
        let additions = (0..super::super::proposal::PLAN_PROPOSAL_MAX_ADDITIONS)
            .map(|index| super::super::NewWorkItem {
                item_id: WorkItemId::parse(format!("task-{index}")).expect("item"),
                kind: WorkItemKind::Task,
                objective: text.clone(),
                expected_result: text.clone(),
            })
            .collect();
        let proposal = NewWorkPlanProposal {
            owner_id: WorkOwnerId::parse("owner").expect("owner"),
            work_id: WorkId::parse("work").expect("work"),
            branch_id: WorkBranchId::parse("branch").expect("branch"),
            proposal_id: WorkProposalId::parse("proposal").expect("proposal"),
            expected_work_revision: WorkRevision::INITIAL,
            expected_goal_revision: GoalRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_graph_revision: GraphRevision::INITIAL,
            additions,
            revisions: Vec::new(),
            dependencies: Vec::new(),
            dependency_removals: Vec::new(),
            reason: WorkChangeReason::parse("Exercise the proposal size bound").expect("reason"),
            source_kind: WorkProposalSourceKind::Model,
            source_ref: WorkChangeRef::parse("model-invocation").expect("source"),
        }
        .canonicalized()
        .expect("structurally bounded proposal");

        assert!(matches!(
            payload(&proposal),
            Err(WorkRepositoryError::InvalidMutation {
                source: WorkDomainError::InvalidPlanProposal {
                    violation: WorkPlanProposalViolation::PayloadTooLarge
                }
            })
        ));
    }

    #[test]
    fn persisted_graph_rejects_duplicate_item_identity_even_across_revisions() {
        let error = match super::super::graph_repository::decode_persisted_graph(
            r#"[{"item_id":"task","revision":1},{"item_id":"task","revision":2}]"#,
            2,
            "[]",
            0,
        ) {
            Ok(_) => panic!("one graph cannot contain two revisions of the same item"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            WorkRepositoryError::Corrupt {
                entity: "Work graph item manifest",
                ..
            }
        ));
    }
}
