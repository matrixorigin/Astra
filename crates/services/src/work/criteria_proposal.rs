use super::{
    CriterionDefinition, CriterionId, CriterionRevision, CriterionRevisionRef,
    CriterionSetMemberChange, CriterionSetRevision, GoalRevision, GraphRevision, NewWorkCriterion,
    WorkBranchId, WorkBranchRevision, WorkChangeRef, WorkContentHash, WorkDomainError, WorkId,
    WorkOwnerId, WorkProposalId, WorkProposalSourceKind, WorkProposalStatus, WorkRevision,
};
use chrono::{DateTime, Utc};
use serde::Serialize;

pub(crate) const CRITERIA_PROPOSAL_MAX_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WorkCriteriaProposalViolation {
    #[error("a criteria proposal must contain at least one explicit member")]
    EmptyMembers,
    #[error("the canonical criteria proposal payload exceeds two MiB")]
    PayloadTooLarge,
}

/// One member in a proposed complete criterion set.
///
/// The variant is structural; no statement text is inspected to decide
/// whether a criterion already exists or which verifier owns it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "member_kind", rename_all = "snake_case")]
pub enum WorkCriteriaProposalMember {
    Existing {
        criterion_id: CriterionId,
        revision: CriterionRevision,
    },
    New {
        criterion_id: CriterionId,
        definition: CriterionDefinition,
    },
}

impl WorkCriteriaProposalMember {
    fn criterion_id(&self) -> &CriterionId {
        match self {
            Self::Existing { criterion_id, .. } | Self::New { criterion_id, .. } => criterion_id,
        }
    }

    pub(crate) fn to_change(&self) -> CriterionSetMemberChange {
        match self {
            Self::Existing {
                criterion_id,
                revision,
            } => CriterionSetMemberChange::Existing(CriterionRevisionRef {
                criterion_id: criterion_id.clone(),
                revision: *revision,
            }),
            Self::New {
                criterion_id,
                definition,
            } => CriterionSetMemberChange::New(NewWorkCriterion {
                criterion_id: criterion_id.clone(),
                definition: definition.clone(),
            }),
        }
    }
}

/// A non-authoritative, revision-pinned replacement for the complete accepted
/// criterion set. It remains provisional until an explicit typed acceptance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NewWorkCriteriaProposal {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub proposal_id: WorkProposalId,
    pub expected_work_revision: WorkRevision,
    pub expected_goal_revision: GoalRevision,
    pub expected_criteria_set_revision: CriterionSetRevision,
    pub expected_branch_revision: WorkBranchRevision,
    pub expected_graph_revision: GraphRevision,
    pub members: Vec<WorkCriteriaProposalMember>,
    pub source_kind: WorkProposalSourceKind,
    pub source_ref: WorkChangeRef,
}

impl NewWorkCriteriaProposal {
    pub(crate) fn canonicalized(mut self) -> Result<Self, WorkDomainError> {
        if self.members.is_empty() {
            return Err(invalid(WorkCriteriaProposalViolation::EmptyMembers));
        }
        let changes = self.change_members();
        super::criteria::canonical_member_refs(&changes)?;
        self.members
            .sort_unstable_by(|left, right| left.criterion_id().cmp(right.criterion_id()));
        Ok(self)
    }

    pub(crate) fn change_members(&self) -> Vec<CriterionSetMemberChange> {
        self.members
            .iter()
            .map(WorkCriteriaProposalMember::to_change)
            .collect()
    }
}

fn invalid(violation: WorkCriteriaProposalViolation) -> WorkDomainError {
    WorkDomainError::InvalidCriteriaProposal { violation }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedWorkCriteriaProposal {
    pub proposal: NewWorkCriteriaProposal,
    pub proposal_seq: i64,
    pub payload_hash: WorkContentHash,
    pub status: WorkProposalStatus,
    pub proposed_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub resolution: Option<WorkCriteriaProposalResolution>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkCriteriaProposalResolution {
    pub resolution_ref: WorkChangeRef,
    pub resolved_at: DateTime<Utc>,
    pub result_work_revision: Option<WorkRevision>,
    pub result_criteria_set_revision: Option<CriterionSetRevision>,
}

/// Exact typed command that accepts one immutable proposal payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkCriteriaProposalAcceptance {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub proposal_id: WorkProposalId,
    pub payload_hash: WorkContentHash,
    pub expected_work_revision: WorkRevision,
    pub expected_goal_revision: GoalRevision,
    pub expected_criteria_set_revision: CriterionSetRevision,
    pub expected_branch_revision: WorkBranchRevision,
    pub expected_graph_revision: GraphRevision,
    pub resolution_ref: WorkChangeRef,
}

/// Exact typed command that rejects one immutable proposal without changing
/// the accepted criterion set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkCriteriaProposalRejection {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub proposal_id: WorkProposalId,
    pub payload_hash: WorkContentHash,
    pub expected_work_revision: WorkRevision,
    pub expected_goal_revision: GoalRevision,
    pub expected_criteria_set_revision: CriterionSetRevision,
    pub expected_branch_revision: WorkBranchRevision,
    pub expected_graph_revision: GraphRevision,
    pub resolution_ref: WorkChangeRef,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{CriterionCommand, CriterionStatement};

    fn proposal(members: Vec<WorkCriteriaProposalMember>) -> NewWorkCriteriaProposal {
        NewWorkCriteriaProposal {
            owner_id: WorkOwnerId::parse("owner").expect("owner"),
            work_id: WorkId::parse("work").expect("work"),
            branch_id: WorkBranchId::parse("branch").expect("branch"),
            proposal_id: WorkProposalId::parse("proposal").expect("proposal"),
            expected_work_revision: WorkRevision::INITIAL,
            expected_goal_revision: GoalRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_graph_revision: GraphRevision::INITIAL,
            members,
            source_kind: WorkProposalSourceKind::Model,
            source_ref: WorkChangeRef::parse("model-invocation").expect("source"),
        }
    }

    fn new(id: &str) -> WorkCriteriaProposalMember {
        WorkCriteriaProposalMember::New {
            criterion_id: CriterionId::parse(id).expect("criterion"),
            definition: CriterionDefinition::TestCheck {
                statement: CriterionStatement::parse(format!("Prove {id}")).expect("statement"),
                command: CriterionCommand::parse("cargo test").expect("command"),
            },
        }
    }

    #[test]
    fn criteria_proposal_is_a_canonical_complete_typed_set() {
        let canonical = proposal(vec![new("criterion-b"), new("criterion-a")])
            .canonicalized()
            .expect("canonical proposal");
        assert_eq!(canonical.members[0].criterion_id().as_str(), "criterion-a");
        assert_eq!(canonical.members[1].criterion_id().as_str(), "criterion-b");
        assert!(matches!(
            proposal(Vec::new()).canonicalized(),
            Err(WorkDomainError::InvalidCriteriaProposal {
                violation: WorkCriteriaProposalViolation::EmptyMembers
            })
        ));
        assert!(matches!(
            proposal(vec![new("same"), new("same")]).canonicalized(),
            Err(WorkDomainError::DuplicateCriterion { .. })
        ));
    }

    #[test]
    fn criteria_proposal_rejects_aggregate_definition_amplification() {
        let members = (0..16)
            .map(|index| WorkCriteriaProposalMember::New {
                criterion_id: CriterionId::parse(format!("criterion-{index:02}"))
                    .expect("criterion"),
                definition: CriterionDefinition::TestCheck {
                    statement: CriterionStatement::parse("Prove the bounded criterion set.")
                        .expect("statement"),
                    command: CriterionCommand::parse("x".repeat(64 * 1024)).expect("command"),
                },
            })
            .collect();
        assert!(matches!(
            proposal(members).canonicalized(),
            Err(WorkDomainError::CriteriaPayloadTooLarge {
                max_bytes
            }) if max_bytes == 1024 * 1024
        ));
    }
}
