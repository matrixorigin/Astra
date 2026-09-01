use super::{
    CriterionSetRevision, GoalRevision, GraphRevision, WorkBranchId, WorkBranchRevision,
    WorkChangeRef, WorkDomainError, WorkId, WorkOwnerId, WorkRevision,
};
use chrono::{DateTime, Utc};
use serde::Serialize;

pub(crate) const WORK_EVENT_RETENTION_PER_WORK: i64 = 10_000;
pub const WORK_EVENT_PAGE_MAX_ITEMS: u16 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct WorkEventSeq(i64);

impl WorkEventSeq {
    pub const INITIAL: Self = Self(1);

    pub fn new(value: i64) -> Result<Self, WorkDomainError> {
        if value < 1 {
            return Err(WorkDomainError::InvalidRevision {
                field: "work event",
                value,
            });
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkEventKind {
    WorkCreated,
    GoalRevised,
    CriteriaAccepted,
    BranchBasisAdopted,
    GraphReplaced,
    DeliveryBranchSelected,
    BranchArchived,
    BranchRestored,
    SubjectChanged,
    PatchArtifactExported,
    PlanProposed,
    CriteriaProposed,
    ProposalRejected,
    CheckRecorded,
    GapsAccepted,
    RunCompleted,
    RunDelegated,
    RunFailed,
    RunCancelled,
    RuntimeEventsExpired,
}

impl WorkEventKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::WorkCreated => "work_created",
            Self::GoalRevised => "goal_revised",
            Self::CriteriaAccepted => "criteria_accepted",
            Self::BranchBasisAdopted => "branch_basis_adopted",
            Self::GraphReplaced => "graph_replaced",
            Self::DeliveryBranchSelected => "delivery_branch_selected",
            Self::BranchArchived => "branch_archived",
            Self::BranchRestored => "branch_restored",
            Self::SubjectChanged => "subject_changed",
            Self::PatchArtifactExported => "patch_artifact_exported",
            Self::PlanProposed => "plan_proposed",
            Self::CriteriaProposed => "criteria_proposed",
            Self::ProposalRejected => "proposal_rejected",
            Self::CheckRecorded => "check_recorded",
            Self::GapsAccepted => "gaps_accepted",
            Self::RunCompleted => "run_completed",
            Self::RunDelegated => "run_delegated",
            Self::RunFailed => "run_failed",
            Self::RunCancelled => "run_cancelled",
            Self::RuntimeEventsExpired => "runtime_events_expired",
        }
    }

    pub(crate) fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "work_created" => Some(Self::WorkCreated),
            "goal_revised" => Some(Self::GoalRevised),
            "criteria_accepted" => Some(Self::CriteriaAccepted),
            "branch_basis_adopted" => Some(Self::BranchBasisAdopted),
            "graph_replaced" => Some(Self::GraphReplaced),
            "delivery_branch_selected" => Some(Self::DeliveryBranchSelected),
            "branch_archived" => Some(Self::BranchArchived),
            "branch_restored" => Some(Self::BranchRestored),
            "subject_changed" => Some(Self::SubjectChanged),
            "patch_artifact_exported" => Some(Self::PatchArtifactExported),
            "plan_proposed" => Some(Self::PlanProposed),
            "criteria_proposed" => Some(Self::CriteriaProposed),
            "proposal_rejected" => Some(Self::ProposalRejected),
            "check_recorded" => Some(Self::CheckRecorded),
            "gaps_accepted" => Some(Self::GapsAccepted),
            "run_completed" => Some(Self::RunCompleted),
            "run_delegated" => Some(Self::RunDelegated),
            "run_failed" => Some(Self::RunFailed),
            "run_cancelled" => Some(Self::RunCancelled),
            "runtime_events_expired" => Some(Self::RuntimeEventsExpired),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkEventPageLimit(u16);

impl WorkEventPageLimit {
    pub fn new(value: u16) -> Result<Self, WorkDomainError> {
        if value == 0 || value > WORK_EVENT_PAGE_MAX_ITEMS {
            return Err(WorkDomainError::InvalidEventPageLimit {
                value,
                maximum: WORK_EVENT_PAGE_MAX_ITEMS,
            });
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkEventQuery {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub after_event_seq: Option<WorkEventSeq>,
    pub limit: WorkEventPageLimit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkEventCoverage {
    Complete,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkEventRecord {
    pub event_seq: WorkEventSeq,
    pub branch_id: Option<WorkBranchId>,
    pub kind: WorkEventKind,
    pub work_revision: Option<WorkRevision>,
    pub goal_revision: Option<GoalRevision>,
    pub criterion_set_revision: Option<CriterionSetRevision>,
    pub branch_revision: Option<WorkBranchRevision>,
    pub graph_revision: Option<GraphRevision>,
    pub source_ref: WorkChangeRef,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkEventPage {
    pub work_id: WorkId,
    pub requested_after_event_seq: Option<WorkEventSeq>,
    pub next_after_event_seq: Option<WorkEventSeq>,
    pub event_head: WorkEventSeq,
    pub retained_from_event_seq: WorkEventSeq,
    pub seen_through_event_seq: Option<WorkEventSeq>,
    pub coverage: WorkEventCoverage,
    pub has_more: bool,
    pub events: Vec<WorkEventRecord>,
}

pub(crate) struct NewWorkEvent {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: Option<WorkBranchId>,
    pub kind: WorkEventKind,
    pub work_revision: Option<WorkRevision>,
    pub goal_revision: Option<GoalRevision>,
    pub criterion_set_revision: Option<CriterionSetRevision>,
    pub branch_revision: Option<WorkBranchRevision>,
    pub graph_revision: Option<GraphRevision>,
    pub source_ref: WorkChangeRef,
}

pub(crate) const fn retained_from(head: WorkEventSeq) -> i64 {
    let candidate = head.get() - WORK_EVENT_RETENTION_PER_WORK + 1;
    if candidate < 1 { 1 } else { candidate }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_window_is_constant_instead_of_session_length_dependent() {
        assert_eq!(retained_from(WorkEventSeq::INITIAL), 1);
        assert_eq!(
            retained_from(WorkEventSeq::new(WORK_EVENT_RETENTION_PER_WORK).expect("head")),
            1
        );
        assert_eq!(
            retained_from(WorkEventSeq::new(WORK_EVENT_RETENTION_PER_WORK + 1).expect("head")),
            2
        );
        assert_eq!(
            WorkEventSeq::new(i64::MAX)
                .map(retained_from)
                .expect("maximum sequence"),
            i64::MAX - WORK_EVENT_RETENTION_PER_WORK + 1
        );
    }

    #[test]
    fn event_page_limit_is_an_explicit_bounded_admission_fact() {
        assert!(WorkEventPageLimit::new(1).is_ok());
        assert!(WorkEventPageLimit::new(WORK_EVENT_PAGE_MAX_ITEMS).is_ok());
        assert!(matches!(
            WorkEventPageLimit::new(0),
            Err(WorkDomainError::InvalidEventPageLimit { value: 0, .. })
        ));
        assert!(WorkEventPageLimit::new(WORK_EVENT_PAGE_MAX_ITEMS + 1).is_err());
    }
}
