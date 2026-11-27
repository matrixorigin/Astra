use super::{WorkContentHash, WorkDomainError, WorkEventSeq, WorkId, WorkOwnerId};
use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct WorkAttentionReceiptRevision(i64);

impl WorkAttentionReceiptRevision {
    pub const INITIAL: Self = Self(1);

    pub fn new(value: i64) -> Result<Self, WorkDomainError> {
        if value < 1 {
            return Err(WorkDomainError::InvalidRevision {
                field: "work attention receipt",
                value,
            });
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkAttentionCursorAdvance {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub through_event_seq: WorkEventSeq,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkAttentionReceipt {
    pub revision: WorkAttentionReceiptRevision,
    pub through_event_seq: WorkEventSeq,
    pub receipt_hash: WorkContentHash,
    pub updated_at: DateTime<Utc>,
}
