use super::{
    GraphRevision, WorkBranchId, WorkBranchRevision, WorkDomainError, WorkEventSeq, WorkGoal,
    WorkId, WorkOwnerId, WorkRevision,
};
use chrono::{DateTime, Utc};
use serde::Serialize;

pub const WORK_CATALOG_PAGE_MAX_ITEMS: u16 = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkCatalogPageLimit(u16);

impl WorkCatalogPageLimit {
    pub fn new(value: u16) -> Result<Self, WorkDomainError> {
        if value == 0 || value > WORK_CATALOG_PAGE_MAX_ITEMS {
            return Err(WorkDomainError::InvalidCatalogPageLimit {
                value,
                maximum: WORK_CATALOG_PAGE_MAX_ITEMS,
            });
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkCatalogCursor {
    pub created_at: DateTime<Utc>,
    pub work_id: WorkId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkCatalogQuery {
    pub owner_id: WorkOwnerId,
    pub before: Option<WorkCatalogCursor>,
    pub limit: WorkCatalogPageLimit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkCatalogAttention {
    NeedsReview,
    Updated,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchActivity {
    Working,
    Waiting,
    Paused,
    Idle,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkCatalogEntry {
    pub work_id: WorkId,
    pub goal: WorkGoal,
    pub work_revision: WorkRevision,
    pub delivery_branch_id: WorkBranchId,
    pub delivery_branch_revision: WorkBranchRevision,
    pub graph_revision: GraphRevision,
    pub graph_item_count: u16,
    pub pending_decision_count: u16,
    pub event_head: WorkEventSeq,
    pub seen_through_event_seq: Option<WorkEventSeq>,
    pub unseen_event_count: u64,
    pub attention: WorkCatalogAttention,
    pub delivery_branch_activity: WorkBranchActivity,
    pub created_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkCatalogPage {
    pub entries: Vec<WorkCatalogEntry>,
    pub next_cursor: Option<WorkCatalogCursor>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_page_limit_is_bounded_independently_of_account_size() {
        assert!(WorkCatalogPageLimit::new(1).is_ok());
        assert!(WorkCatalogPageLimit::new(WORK_CATALOG_PAGE_MAX_ITEMS).is_ok());
        assert!(WorkCatalogPageLimit::new(0).is_err());
        assert!(WorkCatalogPageLimit::new(WORK_CATALOG_PAGE_MAX_ITEMS + 1).is_err());
    }
}
