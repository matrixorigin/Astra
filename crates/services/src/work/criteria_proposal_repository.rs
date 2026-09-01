use super::criteria_proposal::CRITERIA_PROPOSAL_MAX_PAYLOAD_BYTES;
use super::proposal_queue::{
    ProposalAdmission, SELECT_WORK_PROPOSAL, WorkProposalEnvelope, admit_proposal,
};
use super::repository::{
    DatabaseWorkRepository, WorkConflictResource, WorkProposalBasisResource, WorkRepositoryError,
    invalid_mutation,
};
use super::{
    CriterionCommand, CriterionDefinition, CriterionId, CriterionRevision, CriterionSetRevision,
    CriterionStatement, GoalRevision, GraphRevision, NewWorkCriteriaProposal,
    RecordedWorkCriteriaProposal, WorkBranchId, WorkBranchRevision, WorkChangeRef, WorkContentHash,
    WorkCriteriaProposalMember, WorkCriteriaProposalResolution, WorkEventKind, WorkId, WorkOwnerId,
    WorkProposalId, WorkProposalKind, WorkProposalSourceKind, WorkProposalStatus, WorkRevision,
};
use serde::Deserialize;
use sqlx::mysql::MySqlRow;
use sqlx::{MySql, QueryBuilder, Row, query};
use std::collections::BTreeSet;

const SELECT_PENDING_CRITERIA_PROPOSALS: &str =
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
     WHERE owner_id = ? AND work_id = ? AND branch_id = ?
       AND proposal_kind = 'criteria_set' AND status = 'pending' AND expires_at > NOW(6)
     ORDER BY proposal_seq ASC
     LIMIT 8";

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CriterionDefinitionWire {
    CommandCheck { statement: String, command: String },
    TestCheck { statement: String, command: String },
    HumanReview { statement: String },
}

#[derive(Deserialize)]
#[serde(tag = "member_kind", rename_all = "snake_case", deny_unknown_fields)]
enum CriteriaProposalMemberWire {
    Existing {
        criterion_id: String,
        revision: i64,
    },
    New {
        criterion_id: String,
        definition: CriterionDefinitionWire,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CriteriaProposalPayloadWire {
    owner_id: String,
    work_id: String,
    branch_id: String,
    proposal_id: String,
    expected_work_revision: i64,
    expected_goal_revision: i64,
    expected_criteria_set_revision: i64,
    expected_branch_revision: i64,
    expected_graph_revision: i64,
    members: Vec<CriteriaProposalMemberWire>,
    source_kind: String,
    source_ref: String,
}

fn corrupt_payload(source: impl std::error::Error + Send + Sync + 'static) -> WorkRepositoryError {
    WorkRepositoryError::corrupt("criteria proposal payload", source)
}

fn corrupt_proposal(source: impl std::error::Error + Send + Sync + 'static) -> WorkRepositoryError {
    WorkRepositoryError::corrupt("criteria proposal", source)
}

fn decode_definition(
    wire: CriterionDefinitionWire,
) -> Result<CriterionDefinition, WorkRepositoryError> {
    match wire {
        CriterionDefinitionWire::CommandCheck { statement, command } => {
            Ok(CriterionDefinition::CommandCheck {
                statement: CriterionStatement::parse(statement).map_err(corrupt_payload)?,
                command: CriterionCommand::parse(command).map_err(corrupt_payload)?,
            })
        }
        CriterionDefinitionWire::TestCheck { statement, command } => {
            Ok(CriterionDefinition::TestCheck {
                statement: CriterionStatement::parse(statement).map_err(corrupt_payload)?,
                command: CriterionCommand::parse(command).map_err(corrupt_payload)?,
            })
        }
        CriterionDefinitionWire::HumanReview { statement } => {
            Ok(CriterionDefinition::HumanReview {
                statement: CriterionStatement::parse(statement).map_err(corrupt_payload)?,
            })
        }
    }
}

fn decode_payload(payload_json: &str) -> Result<NewWorkCriteriaProposal, WorkRepositoryError> {
    let wire: CriteriaProposalPayloadWire =
        serde_json::from_str(payload_json).map_err(corrupt_payload)?;
    let members = wire
        .members
        .into_iter()
        .map(|member| match member {
            CriteriaProposalMemberWire::Existing {
                criterion_id,
                revision,
            } => Ok(WorkCriteriaProposalMember::Existing {
                criterion_id: CriterionId::parse(criterion_id).map_err(corrupt_payload)?,
                revision: CriterionRevision::new(revision).map_err(corrupt_payload)?,
            }),
            CriteriaProposalMemberWire::New {
                criterion_id,
                definition,
            } => Ok(WorkCriteriaProposalMember::New {
                criterion_id: CriterionId::parse(criterion_id).map_err(corrupt_payload)?,
                definition: decode_definition(definition)?,
            }),
        })
        .collect::<Result<Vec<_>, WorkRepositoryError>>()?;
    let source_kind =
        WorkProposalSourceKind::from_persisted(&wire.source_kind).ok_or_else(|| {
            corrupt_payload(std::io::Error::other(
                "unknown criteria proposal source kind",
            ))
        })?;
    let proposal = NewWorkCriteriaProposal {
        owner_id: WorkOwnerId::parse(wire.owner_id).map_err(corrupt_payload)?,
        work_id: WorkId::parse(wire.work_id).map_err(corrupt_payload)?,
        branch_id: WorkBranchId::parse(wire.branch_id).map_err(corrupt_payload)?,
        proposal_id: WorkProposalId::parse(wire.proposal_id).map_err(corrupt_payload)?,
        expected_work_revision: WorkRevision::new(wire.expected_work_revision)
            .map_err(corrupt_payload)?,
        expected_goal_revision: GoalRevision::new(wire.expected_goal_revision)
            .map_err(corrupt_payload)?,
        expected_criteria_set_revision: CriterionSetRevision::new(
            wire.expected_criteria_set_revision,
        )
        .map_err(corrupt_payload)?,
        expected_branch_revision: WorkBranchRevision::new(wire.expected_branch_revision)
            .map_err(corrupt_payload)?,
        expected_graph_revision: GraphRevision::new(wire.expected_graph_revision)
            .map_err(corrupt_payload)?,
        members,
        source_kind,
        source_ref: WorkChangeRef::parse(wire.source_ref).map_err(corrupt_payload)?,
    }
    .canonicalized()
    .map_err(corrupt_payload)?;
    let canonical = super::repository::canonical_json("criteria proposal payload", &proposal)?;
    if canonical != payload_json {
        return Err(corrupt_payload(std::io::Error::other(
            "persisted criteria proposal payload is not canonical",
        )));
    }
    Ok(proposal)
}

fn payload(
    proposal: &NewWorkCriteriaProposal,
) -> Result<(String, WorkContentHash), WorkRepositoryError> {
    let json = super::repository::canonical_json("criteria proposal payload", proposal)?;
    if json.len() > CRITERIA_PROPOSAL_MAX_PAYLOAD_BYTES {
        return Err(invalid_mutation(
            super::WorkDomainError::InvalidCriteriaProposal {
                violation: super::WorkCriteriaProposalViolation::PayloadTooLarge,
            },
        ));
    }
    let hash =
        WorkContentHash::parse(super::repository::content_hash(&json)).map_err(|message| {
            WorkRepositoryError::corrupt(
                "criteria proposal payload",
                std::io::Error::other(message),
            )
        })?;
    Ok((json, hash))
}

async fn validate_basis(
    repository: &DatabaseWorkRepository,
    proposal: &NewWorkCriteriaProposal,
) -> Result<(), WorkRepositoryError> {
    let row = query(
        "SELECT w.work_revision, w.current_goal_revision, w.current_criteria_set_revision,
                CASE WHEN w.archived_at IS NULL THEN 0 ELSE 1 END AS work_archived,
                g.revision AS materialized_goal_revision,
                cs.revision AS materialized_criteria_set_revision,
                b.branch_revision, b.goal_revision_ref, b.criteria_set_revision_ref,
                b.current_graph_revision,
                CASE WHEN b.archived_at IS NULL THEN 0 ELSE 1 END AS branch_archived,
                gr.revision AS materialized_graph_revision
         FROM works w
         LEFT JOIN work_goal_revisions g
           ON g.owner_id = w.owner_id AND g.work_id = w.work_id
          AND g.revision = w.current_goal_revision
         LEFT JOIN work_criterion_sets cs
           ON cs.owner_id = w.owner_id AND cs.work_id = w.work_id
          AND cs.revision = w.current_criteria_set_revision
         LEFT JOIN work_branches b
           ON b.owner_id = w.owner_id AND b.work_id = w.work_id AND b.branch_id = ?
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
    .map_err(|source| WorkRepositoryError::persistence("validate criteria proposal basis", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let integer = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("criteria proposal basis", source))
    };
    let optional_integer = |field: &'static str| {
        row.try_get::<Option<i64>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("criteria proposal basis", source))
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
    for field in [
        "materialized_goal_revision",
        "materialized_criteria_set_revision",
        "materialized_graph_revision",
    ] {
        if optional_integer(field)?.is_none() {
            return Err(WorkRepositoryError::corrupt(
                "criteria proposal basis",
                std::io::Error::other(format!("{field} is missing")),
            ));
        }
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

    let existing = proposal
        .members
        .iter()
        .filter_map(|member| match member {
            WorkCriteriaProposalMember::Existing {
                criterion_id,
                revision,
            } => Some((criterion_id, revision)),
            WorkCriteriaProposalMember::New { .. } => None,
        })
        .collect::<Vec<_>>();
    if !existing.is_empty() {
        let mut builder = QueryBuilder::<MySql>::new(
            "SELECT criterion_id, revision FROM work_criterion_revisions WHERE owner_id = ",
        );
        builder
            .push_bind(proposal.owner_id.as_str())
            .push(" AND work_id = ")
            .push_bind(proposal.work_id.as_str())
            .push(" AND (");
        for (index, (criterion_id, revision)) in existing.iter().enumerate() {
            if index > 0 {
                builder.push(" OR ");
            }
            builder
                .push("(criterion_id = ")
                .push_bind(criterion_id.as_str())
                .push(" AND revision = ")
                .push_bind(revision.get())
                .push(")");
        }
        builder.push(")");
        let found = builder
            .build()
            .fetch_all(repository.pool.get())
            .await
            .map_err(|source| {
                WorkRepositoryError::persistence(
                    "validate proposed existing criterion revisions",
                    source,
                )
            })?
            .into_iter()
            .map(|row| {
                Ok((
                    row.try_get::<String, _>("criterion_id").map_err(|source| {
                        WorkRepositoryError::corrupt("criterion revision", source)
                    })?,
                    row.try_get::<i64, _>("revision").map_err(|source| {
                        WorkRepositoryError::corrupt("criterion revision", source)
                    })?,
                ))
            })
            .collect::<Result<BTreeSet<_>, WorkRepositoryError>>()?;
        let missing = existing
            .into_iter()
            .filter(|(criterion_id, revision)| {
                !found.contains(&(criterion_id.as_str().to_string(), revision.get()))
            })
            .map(|(criterion_id, revision)| super::CriterionRevisionRef {
                criterion_id: criterion_id.clone(),
                revision: *revision,
            })
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(WorkRepositoryError::MissingCriterionRevisions { missing });
        }
    }

    let new_ids = proposal
        .members
        .iter()
        .filter_map(|member| match member {
            WorkCriteriaProposalMember::Existing { .. } => None,
            WorkCriteriaProposalMember::New { criterion_id, .. } => Some(criterion_id),
        })
        .collect::<Vec<_>>();
    if !new_ids.is_empty() {
        let mut builder =
            QueryBuilder::<MySql>::new("SELECT criterion_id FROM work_criteria WHERE owner_id = ");
        builder
            .push_bind(proposal.owner_id.as_str())
            .push(" AND work_id = ")
            .push_bind(proposal.work_id.as_str())
            .push(" AND criterion_id IN (");
        let mut separated = builder.separated(", ");
        for criterion_id in new_ids {
            separated.push_bind(criterion_id.as_str());
        }
        separated.push_unseparated(")");
        if builder
            .build()
            .fetch_optional(repository.pool.get())
            .await
            .map_err(|source| {
                WorkRepositoryError::persistence("validate proposed criterion identities", source)
            })?
            .is_some()
        {
            return Err(WorkRepositoryError::InvalidWorkProposalBasis {
                resource: WorkProposalBasisResource::NewCriterionIdentity,
            });
        }
    }
    Ok(())
}

async fn find_existing(
    repository: &DatabaseWorkRepository,
    proposal: &NewWorkCriteriaProposal,
    expected_hash: &WorkContentHash,
) -> Result<Option<RecordedWorkCriteriaProposal>, WorkRepositoryError> {
    let row = query(SELECT_WORK_PROPOSAL)
        .bind(proposal.owner_id.as_str())
        .bind(proposal.work_id.as_str())
        .bind(proposal.proposal_id.as_str())
        .fetch_optional(repository.pool.get())
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("load existing criteria proposal", source)
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

pub(super) fn decode_recorded(
    row: &MySqlRow,
) -> Result<RecordedWorkCriteriaProposal, WorkRepositoryError> {
    let proposal_kind = row
        .try_get::<String, _>("proposal_kind")
        .map_err(corrupt_proposal)?;
    if WorkProposalKind::from_persisted(&proposal_kind) != Some(WorkProposalKind::CriteriaSet) {
        return Err(WorkRepositoryError::Conflict {
            resource: WorkConflictResource::WorkProposalIdentity,
        });
    }
    let payload_json = row
        .try_get::<String, _>("payload_json")
        .map_err(corrupt_proposal)?;
    if payload_json.len() > CRITERIA_PROPOSAL_MAX_PAYLOAD_BYTES {
        return Err(corrupt_proposal(std::io::Error::other(
            "criteria proposal payload exceeds its hard bound",
        )));
    }
    let proposal = decode_payload(&payload_json)?;
    let payload_hash_text = row
        .try_get::<String, _>("payload_hash")
        .map_err(corrupt_proposal)?;
    if super::repository::content_hash(&payload_json) != payload_hash_text {
        return Err(corrupt_proposal(std::io::Error::other(
            "criteria proposal payload hash mismatch",
        )));
    }
    let payload_hash = WorkContentHash::parse(payload_hash_text)
        .map_err(|message| corrupt_proposal(std::io::Error::other(message)))?;
    if row
        .try_get::<Option<i32>, _>("item_change_count")
        .map_err(corrupt_proposal)?
        .is_some()
        || row
            .try_get::<Option<i32>, _>("dependency_change_count")
            .map_err(corrupt_proposal)?
            .is_some()
        || row
            .try_get::<Option<i32>, _>("criterion_count")
            .map_err(corrupt_proposal)?
            != i32::try_from(proposal.members.len()).ok()
    {
        return Err(corrupt_proposal(std::io::Error::other(
            "criteria proposal count columns disagree with its payload",
        )));
    }
    let string = |field: &'static str| row.try_get::<String, _>(field).map_err(corrupt_proposal);
    let integer = |field: &'static str| row.try_get::<i64, _>(field).map_err(corrupt_proposal);
    if string("owner_id")? != proposal.owner_id.as_str()
        || string("work_id")? != proposal.work_id.as_str()
        || string("proposal_id")? != proposal.proposal_id.as_str()
        || string("branch_id")? != proposal.branch_id.as_str()
        || integer("expected_work_revision")? != proposal.expected_work_revision.get()
        || integer("expected_goal_revision")? != proposal.expected_goal_revision.get()
        || integer("expected_criteria_set_revision")?
            != proposal.expected_criteria_set_revision.get()
        || integer("expected_branch_revision")? != proposal.expected_branch_revision.get()
        || integer("expected_graph_revision")? != proposal.expected_graph_revision.get()
        || string("source_kind")? != proposal.source_kind.as_str()
        || string("source_ref")? != proposal.source_ref.as_str()
    {
        return Err(corrupt_proposal(std::io::Error::other(
            "row identity or basis differs from its canonical payload",
        )));
    }
    let status = WorkProposalStatus::from_persisted(
        &row.try_get::<String, _>("status")
            .map_err(corrupt_proposal)?,
    )
    .ok_or_else(|| corrupt_proposal(std::io::Error::other("unknown proposal status")))?;
    let proposed_at = super::repository::decode_timestamp(
        "criteria proposal",
        "proposed_at",
        row.try_get("proposed_at").map_err(corrupt_proposal)?,
    )?;
    let expires_at = super::repository::decode_timestamp(
        "criteria proposal",
        "expires_at",
        row.try_get("expires_at").map_err(corrupt_proposal)?,
    )?;
    let resolved_at = super::repository::optional_timestamp(
        "criteria proposal",
        "resolved_at",
        row.try_get("resolved_at").map_err(corrupt_proposal)?,
    )?;
    if expires_at <= proposed_at || resolved_at.is_some_and(|resolved| resolved < proposed_at) {
        return Err(corrupt_proposal(std::io::Error::other(
            "proposal timestamps violate lifecycle ordering",
        )));
    }
    let resolution_ref = row
        .try_get::<Option<String>, _>("resolution_ref")
        .map_err(corrupt_proposal)?
        .map(WorkChangeRef::parse)
        .transpose()
        .map_err(corrupt_proposal)?;
    let result_work_revision = row
        .try_get::<Option<i64>, _>("result_work_revision")
        .map_err(corrupt_proposal)?
        .map(WorkRevision::new)
        .transpose()
        .map_err(corrupt_proposal)?;
    let result_criteria_set_revision = row
        .try_get::<Option<i64>, _>("result_criteria_set_revision")
        .map_err(corrupt_proposal)?
        .map(CriterionSetRevision::new)
        .transpose()
        .map_err(corrupt_proposal)?;
    if row
        .try_get::<Option<i64>, _>("result_branch_revision")
        .map_err(corrupt_proposal)?
        .is_some()
        || row
            .try_get::<Option<i64>, _>("result_graph_revision")
            .map_err(corrupt_proposal)?
            .is_some()
    {
        return Err(corrupt_proposal(std::io::Error::other(
            "criteria proposal contains graph result revisions",
        )));
    }
    let accepted_result_is_coherent = result_work_revision
        == proposal.expected_work_revision.checked_next().ok()
        && result_criteria_set_revision
            == proposal.expected_criteria_set_revision.checked_next().ok();
    let resolution = match status {
        WorkProposalStatus::Pending
            if resolution_ref.is_none()
                && resolved_at.is_none()
                && result_work_revision.is_none()
                && result_criteria_set_revision.is_none() =>
        {
            None
        }
        WorkProposalStatus::Accepted
            if resolution_ref.is_some() && resolved_at.is_some() && accepted_result_is_coherent =>
        {
            Some(WorkCriteriaProposalResolution {
                resolution_ref: resolution_ref.expect("checked"),
                resolved_at: resolved_at.expect("checked"),
                result_work_revision,
                result_criteria_set_revision,
            })
        }
        WorkProposalStatus::Rejected
        | WorkProposalStatus::Stale
        | WorkProposalStatus::Superseded
        | WorkProposalStatus::Expired
            if resolution_ref.is_some()
                && resolved_at.is_some()
                && result_work_revision.is_none()
                && result_criteria_set_revision.is_none() =>
        {
            Some(WorkCriteriaProposalResolution {
                resolution_ref: resolution_ref.expect("checked"),
                resolved_at: resolved_at.expect("checked"),
                result_work_revision: None,
                result_criteria_set_revision: None,
            })
        }
        _ => {
            return Err(corrupt_proposal(std::io::Error::other(
                "proposal resolution columns violate its status",
            )));
        }
    };
    let proposal_seq = row.try_get("proposal_seq").map_err(corrupt_proposal)?;
    if proposal_seq < 1 {
        return Err(corrupt_proposal(std::io::Error::other(
            "proposal sequence must be positive",
        )));
    }
    Ok(RecordedWorkCriteriaProposal {
        proposal,
        proposal_seq,
        payload_hash,
        status,
        proposed_at,
        expires_at,
        resolution,
    })
}

pub(super) async fn load_criteria_proposal(
    repository: &DatabaseWorkRepository,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    proposal_id: &WorkProposalId,
) -> Result<Option<RecordedWorkCriteriaProposal>, WorkRepositoryError> {
    query(SELECT_WORK_PROPOSAL)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(proposal_id.as_str())
        .fetch_optional(repository.pool.get())
        .await
        .map_err(|source| WorkRepositoryError::persistence("load criteria proposal", source))?
        .as_ref()
        .map(decode_recorded)
        .transpose()
}

pub(super) async fn list_pending_criteria_proposals(
    repository: &DatabaseWorkRepository,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    branch_id: &WorkBranchId,
) -> Result<Vec<RecordedWorkCriteriaProposal>, WorkRepositoryError> {
    query(SELECT_PENDING_CRITERIA_PROPOSALS)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .fetch_all(repository.pool.get())
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("list pending criteria proposals", source)
        })?
        .iter()
        .map(decode_recorded)
        .collect()
}

pub(super) async fn propose_criteria(
    repository: &DatabaseWorkRepository,
    proposal: NewWorkCriteriaProposal,
) -> Result<RecordedWorkCriteriaProposal, WorkRepositoryError> {
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
            proposal_kind: WorkProposalKind::CriteriaSet,
            expected_work_revision: proposal.expected_work_revision,
            expected_goal_revision: proposal.expected_goal_revision,
            expected_criteria_set_revision: proposal.expected_criteria_set_revision,
            expected_branch_revision: proposal.expected_branch_revision,
            expected_graph_revision: proposal.expected_graph_revision,
            payload_json: &payload_json,
            payload_hash: &payload_hash,
            item_change_count: None,
            dependency_change_count: None,
            criterion_count: Some(
                i32::try_from(proposal.members.len()).expect("bounded criteria members"),
            ),
            source_kind: proposal.source_kind,
            source_ref: &proposal.source_ref,
            event_kind: WorkEventKind::CriteriaProposed,
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
        ProposalAdmission::Inserted(inserted) => Ok(RecordedWorkCriteriaProposal {
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
