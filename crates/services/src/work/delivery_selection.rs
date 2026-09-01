use super::{
    CriterionSetRevision, GoalRevision, GraphRevision, WorkBranchId, WorkBranchRevision,
    WorkChangeRef, WorkContentHash, WorkId, WorkOwnerId, WorkRevision, WorkSubjectRef,
};
use serde::Serialize;

pub const WORK_DELIVERY_SELECTION_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkDeliverySelectionSubject {
    pub graph_revision: GraphRevision,
    pub subject_ref: WorkSubjectRef,
    pub subject_revision: WorkContentHash,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkDeliverySelection {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub request_id: WorkChangeRef,
    pub branch_id: WorkBranchId,
    pub expected_work_revision: WorkRevision,
    pub expected_branch_revision: WorkBranchRevision,
    pub expected_goal_revision: GoalRevision,
    pub expected_criteria_set_revision: CriterionSetRevision,
    pub expected_graph_revision: GraphRevision,
    pub expected_subject: Option<WorkDeliverySelectionSubject>,
    pub expected_evidence_manifest_hash: WorkContentHash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkDeliverySelectionOutcome {
    Selected,
    AlreadySelected,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkDeliverySelectionReceipt {
    pub schema_version: u16,
    pub work_id: WorkId,
    pub request_id: WorkChangeRef,
    pub delivery_branch_id: WorkBranchId,
    pub work_revision: WorkRevision,
    pub branch_revision: WorkBranchRevision,
    pub graph_revision: GraphRevision,
    pub evidence_manifest_hash: WorkContentHash,
    pub outcome: WorkDeliverySelectionOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkDeliverySelectionBasisResource {
    RequestPayload,
    WorkRevision,
    WorkDefinition,
    BranchRevision,
    GoalRevision,
    CriterionSetRevision,
    GraphRevision,
    Subject,
    Evidence,
}
