use super::{
    CriterionSetRevision, GoalRevision, WorkBranchId, WorkBranchRevision, WorkChangeRef, WorkId,
    WorkOwnerId, WorkRevision,
};

/// Explicit, revision-pinned adoption of the current Work definition by one
/// branch. Goal and Done-when changes never rewrite branch bases implicitly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkBranchBasisChange {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub expected_work_revision: WorkRevision,
    pub expected_branch_revision: WorkBranchRevision,
    pub expected_goal_revision: GoalRevision,
    pub expected_criteria_set_revision: CriterionSetRevision,
    pub target_goal_revision: GoalRevision,
    pub target_criteria_set_revision: CriterionSetRevision,
    pub source_ref: WorkChangeRef,
}
