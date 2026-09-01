use super::{
    GraphRevision, WorkBranchId, WorkBranchRevision, WorkChangeRef, WorkContentHash,
    WorkDomainError, WorkId, WorkOwnerId,
};
use chrono::{DateTime, Utc};
use serde::Serialize;

const WORK_SUBJECT_REF_MAX_CHARS: usize = 256;

/// Provider-neutral identity of the workspace, repository, artifact, or other
/// resource whose immutable revision is being delivered and verified.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WorkSubjectRef(String);

impl WorkSubjectRef {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        super::validate_identity("work_subject_ref", &value, WORK_SUBJECT_REF_MAX_CHARS)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WorkBranchSubjectRevision(i64);

impl WorkBranchSubjectRevision {
    pub const INITIAL: Self = Self(1);

    pub fn new(value: i64) -> Result<Self, WorkDomainError> {
        if value < 1 {
            return Err(WorkDomainError::InvalidRevision {
                field: "work branch subject",
                value,
            });
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> i64 {
        self.0
    }

    pub fn checked_next(self) -> Result<Self, WorkDomainError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(WorkDomainError::RevisionExhausted {
                field: "work branch subject",
            })
    }
}

/// Exact materialization fact admitted by a deterministic workspace/provider
/// boundary. No field is inferred from model prose or command output text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkBranchSubjectChange {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub expected_branch_revision: WorkBranchRevision,
    pub graph_revision: GraphRevision,
    pub subject_ref: WorkSubjectRef,
    pub subject_revision: WorkContentHash,
    pub source_ref: WorkChangeRef,
}

/// Removes the current materialized subject before an execution boundary may
/// mutate its workspace. This is deliberately revision-pinned: stale evidence
/// must disappear before tools run, and a concurrent branch advance must not
/// be overwritten.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkBranchSubjectInvalidation {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub expected_branch_revision: WorkBranchRevision,
    pub graph_revision: GraphRevision,
    pub source_ref: WorkChangeRef,
}

/// Current subject projection. Workspace content is not copied here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkBranchSubject {
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub subject_record_revision: WorkBranchSubjectRevision,
    pub branch_revision: WorkBranchRevision,
    pub graph_revision: GraphRevision,
    pub subject_ref: WorkSubjectRef,
    pub subject_revision: WorkContentHash,
    pub source_ref: WorkChangeRef,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl WorkBranchSubject {
    pub(crate) fn represents(&self, change: &WorkBranchSubjectChange) -> bool {
        self.work_id == change.work_id
            && self.branch_id == change.branch_id
            && self.graph_revision == change.graph_revision
            && self.subject_ref == change.subject_ref
            && self.subject_revision == change.subject_revision
    }

    pub(crate) fn is_exact_replay(&self, change: &WorkBranchSubjectChange) -> bool {
        self.represents(change) && self.source_ref == change.source_ref
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_identity_is_bounded_without_interpreting_its_text() {
        let opaque = WorkSubjectRef::parse("workspace://tenant-a/repository/head@opaque")
            .expect("provider-neutral opaque ref");
        assert_eq!(
            opaque.as_str(),
            "workspace://tenant-a/repository/head@opaque"
        );
        assert!(WorkSubjectRef::parse("contains whitespace").is_err());
        assert!(WorkSubjectRef::parse("x".repeat(WORK_SUBJECT_REF_MAX_CHARS + 1)).is_err());
    }

    #[test]
    fn subject_revision_fails_closed_at_numeric_exhaustion() {
        assert!(WorkBranchSubjectRevision::new(0).is_err());
        assert!(
            WorkBranchSubjectRevision::new(i64::MAX)
                .expect("maximum revision")
                .checked_next()
                .is_err()
        );
    }
}
