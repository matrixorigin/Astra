use super::{WorkBranchId, WorkBranchRevision, WorkChangeRef, WorkId, WorkOwnerId, WorkRevision};
use serde::Serialize;

pub const WORK_BRANCH_RETENTION_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchRetentionKind {
    Archive,
    Restore,
}

impl WorkBranchRetentionKind {
    pub(crate) const fn event_kind(self) -> super::WorkEventKind {
        match self {
            Self::Archive => super::WorkEventKind::BranchArchived,
            Self::Restore => super::WorkEventKind::BranchRestored,
        }
    }

    pub(crate) const fn wants_archived(self) -> bool {
        matches!(self, Self::Archive)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkBranchRetentionBasisResource {
    RequestPayload,
    WorkRevision,
    BranchRevision,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkBranchRetentionChange {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub request_id: WorkChangeRef,
    pub kind: WorkBranchRetentionKind,
    pub expected_work_revision: WorkRevision,
    pub expected_branch_revision: WorkBranchRevision,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchRetentionOutcome {
    Applied,
    AlreadyInState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkBranchRetentionReceipt {
    pub schema_version: u32,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub request_id: WorkChangeRef,
    pub kind: WorkBranchRetentionKind,
    pub work_revision: WorkRevision,
    pub branch_revision: WorkBranchRevision,
    pub outcome: WorkBranchRetentionOutcome,
}
