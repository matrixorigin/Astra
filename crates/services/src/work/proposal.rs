use super::{
    CriterionSetRevision, GoalRevision, GraphRevision, NewWorkItem, WorkBranchId,
    WorkBranchRevision, WorkChangeReason, WorkChangeRef, WorkContentHash, WorkDomainError, WorkId,
    WorkItemEdge, WorkItemRevisionChange, WorkOwnerId, WorkRevision, validate_resource_identity,
};
use chrono::{DateTime, Utc};
use serde::Serialize;

const WORK_PROPOSAL_ID_MAX_CHARS: usize = 64;
pub(crate) const PLAN_PROPOSAL_MAX_ADDITIONS: usize = 64;
pub(crate) const PLAN_PROPOSAL_MAX_DEPENDENCIES: usize = 256;
pub(crate) const PLAN_PROPOSAL_MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
pub(crate) const WORK_PROPOSAL_MAX_PENDING_PER_BRANCH: i64 = 8;
pub(crate) const WORK_PROPOSAL_RETAINED_TERMINAL_PER_BRANCH: i64 = 64;
pub(crate) const WORK_PROPOSAL_TTL_DAYS: i64 = 7;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WorkProposalId(String);

impl WorkProposalId {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        validate_resource_identity("proposal_id", &value, WORK_PROPOSAL_ID_MAX_CHARS)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkProposalKind {
    PlanPatch,
    CriteriaSet,
}

impl WorkProposalKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PlanPatch => "plan_patch",
            Self::CriteriaSet => "criteria_set",
        }
    }

    pub(crate) fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "plan_patch" => Some(Self::PlanPatch),
            "criteria_set" => Some(Self::CriteriaSet),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkProposalSourceKind {
    Model,
    Reflection,
}

impl WorkProposalSourceKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Reflection => "reflection",
        }
    }

    pub(crate) fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "model" => Some(Self::Model),
            "reflection" => Some(Self::Reflection),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkProposalStatus {
    Pending,
    Accepted,
    Rejected,
    Stale,
    Superseded,
    Expired,
}

impl WorkProposalStatus {
    pub(crate) fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "accepted" => Some(Self::Accepted),
            "rejected" => Some(Self::Rejected),
            "stale" => Some(Self::Stale),
            "superseded" => Some(Self::Superseded),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WorkPlanProposalViolation {
    #[error("a plan proposal must contain at least one graph change")]
    EmptyPatch,
    #[error("a plan proposal changes more than 64 Work items")]
    TooManyItemChanges,
    #[error("a plan proposal changes more than 256 dependencies")]
    TooManyDependencies,
    #[error("a plan proposal repeats a new Work item identity")]
    DuplicateAddition,
    #[error("a plan proposal repeats an existing Work item revision")]
    DuplicateRevision,
    #[error("a plan proposal both adds and revises the same Work item identity")]
    ConflictingItemChange,
    #[error("a plan proposal repeats a dependency")]
    DuplicateDependency,
    #[error("a plan proposal repeats a dependency removal")]
    DuplicateDependencyRemoval,
    #[error("a plan proposal both adds and removes the same dependency")]
    ConflictingDependencyChange,
    #[error("a plan proposal contains a self dependency")]
    SelfDependency,
    #[error("the canonical proposal payload exceeds one MiB")]
    PayloadTooLarge,
}

/// Non-authoritative, revision-pinned graph patch. It cannot mutate the graph
/// until a separate deterministic acceptance action revalidates it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NewWorkPlanProposal {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub proposal_id: WorkProposalId,
    pub expected_work_revision: WorkRevision,
    pub expected_goal_revision: GoalRevision,
    pub expected_criteria_set_revision: CriterionSetRevision,
    pub expected_branch_revision: WorkBranchRevision,
    pub expected_graph_revision: GraphRevision,
    pub additions: Vec<NewWorkItem>,
    pub revisions: Vec<WorkItemRevisionChange>,
    pub dependencies: Vec<WorkItemEdge>,
    pub dependency_removals: Vec<WorkItemEdge>,
    pub reason: WorkChangeReason,
    pub source_kind: WorkProposalSourceKind,
    pub source_ref: WorkChangeRef,
}

impl NewWorkPlanProposal {
    pub(crate) fn canonicalized(mut self) -> Result<Self, WorkDomainError> {
        if self.additions.is_empty()
            && self.revisions.is_empty()
            && self.dependencies.is_empty()
            && self.dependency_removals.is_empty()
        {
            return Err(invalid(WorkPlanProposalViolation::EmptyPatch));
        }
        if self.additions.len() + self.revisions.len() > PLAN_PROPOSAL_MAX_ADDITIONS {
            return Err(invalid(WorkPlanProposalViolation::TooManyItemChanges));
        }
        if self.dependencies.len() + self.dependency_removals.len() > PLAN_PROPOSAL_MAX_DEPENDENCIES
        {
            return Err(invalid(WorkPlanProposalViolation::TooManyDependencies));
        }
        self.additions
            .sort_unstable_by(|left, right| left.item_id.cmp(&right.item_id));
        if self
            .additions
            .windows(2)
            .any(|pair| pair[0].item_id == pair[1].item_id)
        {
            return Err(invalid(WorkPlanProposalViolation::DuplicateAddition));
        }
        self.revisions
            .sort_unstable_by(|left, right| left.item_id.cmp(&right.item_id));
        if self
            .revisions
            .windows(2)
            .any(|pair| pair[0].item_id == pair[1].item_id)
        {
            return Err(invalid(WorkPlanProposalViolation::DuplicateRevision));
        }
        if self.additions.iter().any(|addition| {
            self.revisions
                .binary_search_by(|revision| revision.item_id.cmp(&addition.item_id))
                .is_ok()
        }) {
            return Err(invalid(WorkPlanProposalViolation::ConflictingItemChange));
        }
        self.dependencies.sort_unstable();
        if self.dependencies.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(invalid(WorkPlanProposalViolation::DuplicateDependency));
        }
        if self
            .dependencies
            .iter()
            .any(|edge| edge.predecessor_item_id == edge.successor_item_id)
        {
            return Err(invalid(WorkPlanProposalViolation::SelfDependency));
        }
        self.dependency_removals.sort_unstable();
        if self
            .dependency_removals
            .windows(2)
            .any(|pair| pair[0] == pair[1])
        {
            return Err(invalid(
                WorkPlanProposalViolation::DuplicateDependencyRemoval,
            ));
        }
        if self
            .dependency_removals
            .iter()
            .any(|edge| edge.predecessor_item_id == edge.successor_item_id)
        {
            return Err(invalid(WorkPlanProposalViolation::SelfDependency));
        }
        if self
            .dependencies
            .iter()
            .any(|edge| self.dependency_removals.binary_search(edge).is_ok())
        {
            return Err(invalid(
                WorkPlanProposalViolation::ConflictingDependencyChange,
            ));
        }
        Ok(self)
    }
}

fn invalid(violation: WorkPlanProposalViolation) -> WorkDomainError {
    WorkDomainError::InvalidPlanProposal { violation }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedWorkPlanProposal {
    pub proposal: NewWorkPlanProposal,
    pub proposal_seq: i64,
    pub payload_hash: WorkContentHash,
    pub status: WorkProposalStatus,
    pub proposed_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub resolution: Option<WorkPlanProposalResolution>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkPlanProposalResolution {
    pub resolution_ref: WorkChangeRef,
    pub resolved_at: DateTime<Utc>,
    pub result_branch_revision: Option<WorkBranchRevision>,
    pub result_graph_revision: Option<GraphRevision>,
}

/// Exact deterministic admission command for one durable plan proposal.
///
/// The caller repeats the immutable proposal basis rather than asking the
/// repository to apply whichever branch head happens to be current.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkPlanProposalAcceptance {
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
    use super::super::{
        CriterionSetRevision, GoalRevision, GraphRevision, WorkItemDeclarationState, WorkItemId,
        WorkItemKind, WorkItemRevision, WorkItemRevisionChange, WorkItemText,
    };
    use super::*;

    fn item(id: &str) -> NewWorkItem {
        NewWorkItem {
            item_id: WorkItemId::parse(id).expect("item id"),
            kind: WorkItemKind::Task,
            objective: WorkItemText::parse(format!("Implement {id}")).expect("objective"),
            expected_result: WorkItemText::parse(format!("Verify {id}")).expect("result"),
        }
    }

    fn edge(predecessor: &str, successor: &str) -> WorkItemEdge {
        WorkItemEdge {
            predecessor_item_id: WorkItemId::parse(predecessor).expect("predecessor"),
            successor_item_id: WorkItemId::parse(successor).expect("successor"),
            kind: super::super::WorkItemEdgeKind::Dependency,
        }
    }

    fn revision(id: &str, state: WorkItemDeclarationState) -> WorkItemRevisionChange {
        WorkItemRevisionChange::new(
            WorkItemId::parse(id).expect("item id"),
            WorkItemRevision::INITIAL,
            WorkItemKind::Task,
            WorkItemText::parse(format!("Revise {id}")).expect("objective"),
            WorkItemText::parse(format!("Verify revised {id}")).expect("result"),
            state,
        )
    }

    fn proposal(
        additions: Vec<NewWorkItem>,
        dependencies: Vec<WorkItemEdge>,
    ) -> NewWorkPlanProposal {
        NewWorkPlanProposal {
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
            dependencies,
            dependency_removals: Vec::new(),
            reason: WorkChangeReason::parse("Refine the executable Work plan").expect("reason"),
            source_kind: WorkProposalSourceKind::Model,
            source_ref: WorkChangeRef::parse("model-invocation").expect("source"),
        }
    }

    #[test]
    fn canonicalization_is_order_independent() {
        let canonical = proposal(
            vec![item("task-c"), item("task-b"), item("task-a")],
            vec![edge("task-b", "task-c"), edge("task-a", "task-b")],
        )
        .canonicalized()
        .expect("canonical proposal");
        assert_eq!(canonical.additions[0].item_id.as_str(), "task-a");
        assert_eq!(
            canonical.dependencies[0],
            edge("task-a", "task-b"),
            "the payload hash must not depend on caller ordering"
        );
    }

    #[test]
    fn malformed_collections_fail_before_persistence() {
        let duplicate_item = item("task-a");
        for (candidate, violation) in [
            (
                proposal(Vec::new(), Vec::new()),
                WorkPlanProposalViolation::EmptyPatch,
            ),
            (
                proposal(vec![duplicate_item.clone(), duplicate_item], Vec::new()),
                WorkPlanProposalViolation::DuplicateAddition,
            ),
            (
                proposal(
                    vec![item("task-a"), item("task-b")],
                    vec![edge("task-a", "task-b"), edge("task-a", "task-b")],
                ),
                WorkPlanProposalViolation::DuplicateDependency,
            ),
            (
                proposal(vec![item("task-a")], vec![edge("task-a", "task-a")]),
                WorkPlanProposalViolation::SelfDependency,
            ),
        ] {
            assert!(matches!(
                candidate.canonicalized(),
                Err(WorkDomainError::InvalidPlanProposal { violation: actual })
                    if actual == violation
            ));
        }
    }

    #[test]
    fn revision_and_dependency_removal_conflicts_fail_before_persistence() {
        let mut duplicate_revision = proposal(vec![item("other")], Vec::new());
        duplicate_revision.revisions = vec![
            revision("task-a", WorkItemDeclarationState::Active),
            revision("task-a", WorkItemDeclarationState::Cancelled),
        ];
        assert!(matches!(
            duplicate_revision.canonicalized(),
            Err(WorkDomainError::InvalidPlanProposal {
                violation: WorkPlanProposalViolation::DuplicateRevision
            })
        ));

        let mut conflicting_item = proposal(vec![item("task-a")], Vec::new());
        conflicting_item.revisions = vec![revision("task-a", WorkItemDeclarationState::Superseded)];
        assert!(matches!(
            conflicting_item.canonicalized(),
            Err(WorkDomainError::InvalidPlanProposal {
                violation: WorkPlanProposalViolation::ConflictingItemChange
            })
        ));

        let edge = edge("task-a", "task-b");
        let mut conflicting_edge = proposal(Vec::new(), vec![edge.clone()]);
        conflicting_edge.dependency_removals = vec![edge];
        assert!(matches!(
            conflicting_edge.canonicalized(),
            Err(WorkDomainError::InvalidPlanProposal {
                violation: WorkPlanProposalViolation::ConflictingDependencyChange
            })
        ));
    }

    #[test]
    fn collection_limits_are_hard_admission_bounds() {
        let additions = (0..=PLAN_PROPOSAL_MAX_ADDITIONS)
            .map(|index| item(&format!("task-{index}")))
            .collect();
        assert!(matches!(
            proposal(additions, Vec::new()).canonicalized(),
            Err(WorkDomainError::InvalidPlanProposal {
                violation: WorkPlanProposalViolation::TooManyItemChanges
            })
        ));

        let dependencies = (0..=PLAN_PROPOSAL_MAX_DEPENDENCIES)
            .map(|index| edge(&format!("from-{index}"), &format!("to-{index}")))
            .collect();
        assert!(matches!(
            proposal(vec![item("task")], dependencies).canonicalized(),
            Err(WorkDomainError::InvalidPlanProposal {
                violation: WorkPlanProposalViolation::TooManyDependencies
            })
        ));
    }
}
