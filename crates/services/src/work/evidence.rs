use super::{
    CriterionRevisionRef, CriterionSetRevision, GraphRevision, WorkBranchId, WorkChangeRef,
    WorkContentHash, WorkDomainError, WorkId, WorkItemAttemptId, WorkItemRevisionRef, WorkOwnerId,
    WorkSubjectRef, validate_resource_identity,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeSet;

const CHECK_RUN_ID_MAX_CHARS: usize = 64;
const CHECK_EVIDENCE_REF_MAX_BYTES: usize = 512;
pub(crate) const CHECK_RUN_MAX_GAPS: usize = 16;
pub(crate) const CHECK_RUN_MAX_EVIDENCE_REFS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CheckRunId(String);

impl CheckRunId {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        validate_resource_identity("check_run_id", &value, CHECK_RUN_ID_MAX_CHARS)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CheckEvidenceRef(String);

impl CheckEvidenceRef {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        if value.len() > CHECK_EVIDENCE_REF_MAX_BYTES
            || value.chars().any(char::is_control)
            || astra_core::observation::EvidenceRef::parse(&value).is_err()
        {
            return Err(WorkDomainError::InvalidCheckRun {
                violation: CheckRunViolation::InvalidEvidenceRef,
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckVerifierKind {
    Command,
    Test,
}

impl CheckVerifierKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Test => "test",
        }
    }

    pub(crate) fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "command" => Some(Self::Command),
            "test" => Some(Self::Test),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    Passed,
    Failed,
    Error,
    Cancelled,
}

impl CheckOutcome {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }

    pub(crate) fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "passed" => Some(Self::Passed),
            "failed" => Some(Self::Failed),
            "error" => Some(Self::Error),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckErrorKind {
    VerifierUnavailable,
    EnvironmentUnavailable,
    InvocationFailed,
    ResultUnavailable,
}

impl CheckErrorKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::VerifierUnavailable => "verifier_unavailable",
            Self::EnvironmentUnavailable => "environment_unavailable",
            Self::InvocationFailed => "invocation_failed",
            Self::ResultUnavailable => "result_unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckCoverage {
    Complete,
    Partial,
    Unavailable,
}

impl CheckCoverage {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Unavailable => "unavailable",
        }
    }

    pub(crate) fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "complete" => Some(Self::Complete),
            "partial" => Some(Self::Partial),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckCoverageGap {
    TargetNotObserved,
    EnvironmentNotReproduced,
    ArtifactUnavailable,
    ResultTruncated,
    UnsupportedVerifier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CheckRunViolation {
    #[error("evidence reference must be a bounded canonical Astra evidence URN")]
    InvalidEvidenceRef,
    #[error("coverage gaps exceed the bounded limit")]
    TooManyCoverageGaps,
    #[error("evidence references exceed the bounded limit")]
    TooManyEvidenceRefs,
    #[error("coverage gaps must be unique")]
    DuplicateCoverageGap,
    #[error("evidence references must be unique")]
    DuplicateEvidenceRef,
    #[error("complete coverage must have no gaps and incomplete coverage must name a gap")]
    IncoherentCoverage,
    #[error("only error outcomes may carry an error kind, and every error must carry one")]
    IncoherentErrorKind,
    #[error("a passed check requires complete coverage")]
    PassedWithoutCompleteCoverage,
    #[error("passed and failed checks require durable evidence")]
    OutcomeWithoutEvidence,
    #[error("evidence expiry must be later than its production time")]
    InvalidExpiry,
}

/// Immutable evidence produced by one typed verifier invocation.
///
/// The exact WorkItem attempt, subject, accepted criterion set, graph revision,
/// verifier, environment, and source cursor are all explicit. Nothing in this
/// type derives an outcome or task association from command output, run text,
/// or free-form model prose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NewWorkCheckRun {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub check_run_id: CheckRunId,
    pub graph_revision: GraphRevision,
    pub item: WorkItemRevisionRef,
    pub attempt_id: WorkItemAttemptId,
    pub criterion_set_revision: CriterionSetRevision,
    pub criterion: CriterionRevisionRef,
    pub subject_ref: WorkSubjectRef,
    pub subject_revision: WorkContentHash,
    pub artifact_digest: Option<WorkContentHash>,
    pub run_ref: WorkChangeRef,
    pub invocation_ref: WorkChangeRef,
    pub verifier_kind: CheckVerifierKind,
    pub verifier_fingerprint: WorkContentHash,
    pub environment_fingerprint: WorkContentHash,
    pub outcome: CheckOutcome,
    pub error_kind: Option<CheckErrorKind>,
    pub coverage: CheckCoverage,
    pub coverage_gaps: Vec<CheckCoverageGap>,
    pub evidence_refs: Vec<CheckEvidenceRef>,
    pub source_cursor: WorkChangeRef,
    pub produced_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl NewWorkCheckRun {
    pub fn validate(&self) -> Result<(), WorkDomainError> {
        if self.coverage_gaps.len() > CHECK_RUN_MAX_GAPS {
            return Err(invalid(CheckRunViolation::TooManyCoverageGaps));
        }
        if self.evidence_refs.len() > CHECK_RUN_MAX_EVIDENCE_REFS {
            return Err(invalid(CheckRunViolation::TooManyEvidenceRefs));
        }
        if self
            .coverage_gaps
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            != self.coverage_gaps.len()
        {
            return Err(invalid(CheckRunViolation::DuplicateCoverageGap));
        }
        if self.evidence_refs.iter().collect::<BTreeSet<_>>().len() != self.evidence_refs.len() {
            return Err(invalid(CheckRunViolation::DuplicateEvidenceRef));
        }
        if (self.coverage == CheckCoverage::Complete) != self.coverage_gaps.is_empty() {
            return Err(invalid(CheckRunViolation::IncoherentCoverage));
        }
        if (self.outcome == CheckOutcome::Error) != self.error_kind.is_some() {
            return Err(invalid(CheckRunViolation::IncoherentErrorKind));
        }
        if self.outcome == CheckOutcome::Passed && self.coverage != CheckCoverage::Complete {
            return Err(invalid(CheckRunViolation::PassedWithoutCompleteCoverage));
        }
        if matches!(self.outcome, CheckOutcome::Passed | CheckOutcome::Failed)
            && self.evidence_refs.is_empty()
        {
            return Err(invalid(CheckRunViolation::OutcomeWithoutEvidence));
        }
        if self
            .expires_at
            .is_some_and(|expires_at| expires_at <= self.produced_at)
        {
            return Err(invalid(CheckRunViolation::InvalidExpiry));
        }
        Ok(())
    }

    pub(crate) fn canonicalized(mut self) -> Result<Self, WorkDomainError> {
        self.validate()?;
        self.coverage_gaps.sort_unstable();
        self.evidence_refs.sort_unstable();
        Ok(self)
    }
}

fn invalid(violation: CheckRunViolation) -> WorkDomainError {
    WorkDomainError::InvalidCheckRun { violation }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedWorkCheckRun {
    pub check: NewWorkCheckRun,
    pub payload_hash: WorkContentHash,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{CriterionId, CriterionRevision, WorkBranchId, WorkId, WorkOwnerId};

    fn hash(byte: char) -> WorkContentHash {
        WorkContentHash::parse(format!("sha256:{}", byte.to_string().repeat(64))).expect("hash")
    }

    fn check() -> NewWorkCheckRun {
        NewWorkCheckRun {
            owner_id: WorkOwnerId::parse("owner-1").expect("owner"),
            work_id: WorkId::parse("work-1").expect("work"),
            branch_id: WorkBranchId::parse("branch-1").expect("branch"),
            check_run_id: CheckRunId::parse("check-1").expect("check"),
            graph_revision: GraphRevision::INITIAL,
            item: crate::work::WorkItemRevisionRef {
                item_id: crate::work::WorkItemId::root(),
                revision: crate::work::WorkItemRevision::INITIAL,
            },
            attempt_id: WorkItemAttemptId::parse("run-1").expect("attempt"),
            criterion_set_revision: CriterionSetRevision::INITIAL,
            criterion: CriterionRevisionRef {
                criterion_id: CriterionId::parse("criterion-1").expect("criterion"),
                revision: CriterionRevision::INITIAL,
            },
            subject_ref: WorkSubjectRef::parse("workspace-1/repository-1/head-1").expect("subject"),
            subject_revision: hash('a'),
            artifact_digest: Some(hash('b')),
            run_ref: WorkChangeRef::parse("run-1").expect("run"),
            invocation_ref: WorkChangeRef::parse("invocation-1").expect("invocation"),
            verifier_kind: CheckVerifierKind::Test,
            verifier_fingerprint: hash('c'),
            environment_fingerprint: hash('d'),
            outcome: CheckOutcome::Passed,
            error_kind: None,
            coverage: CheckCoverage::Complete,
            coverage_gaps: Vec::new(),
            evidence_refs: vec![
                CheckEvidenceRef::parse("urn:astra:artifact:local:check-1/result")
                    .expect("evidence"),
            ],
            source_cursor: WorkChangeRef::parse("cursor-1").expect("cursor"),
            produced_at: "2026-08-01T00:00:00Z".parse().expect("time"),
            expires_at: None,
        }
    }

    #[test]
    fn passed_check_requires_complete_coverage_and_durable_evidence() {
        check().validate().expect("valid check");

        let mut partial = check();
        partial.coverage = CheckCoverage::Partial;
        partial.coverage_gaps = vec![CheckCoverageGap::TargetNotObserved];
        assert!(matches!(
            partial.validate(),
            Err(WorkDomainError::InvalidCheckRun {
                violation: CheckRunViolation::PassedWithoutCompleteCoverage
            })
        ));

        let mut unsupported = check();
        unsupported.evidence_refs.clear();
        assert!(matches!(
            unsupported.validate(),
            Err(WorkDomainError::InvalidCheckRun {
                violation: CheckRunViolation::OutcomeWithoutEvidence
            })
        ));
    }

    #[test]
    fn coverage_and_error_facts_are_structural_not_message_derived() {
        let mut failed = check();
        failed.outcome = CheckOutcome::Failed;
        failed.coverage = CheckCoverage::Partial;
        failed.coverage_gaps = vec![CheckCoverageGap::ResultTruncated];
        failed.validate().expect("typed partial failure");

        let mut errored = check();
        errored.outcome = CheckOutcome::Error;
        errored.error_kind = Some(CheckErrorKind::EnvironmentUnavailable);
        errored.coverage = CheckCoverage::Unavailable;
        errored.coverage_gaps = vec![CheckCoverageGap::EnvironmentNotReproduced];
        errored.evidence_refs.clear();
        errored.validate().expect("typed environment error");
    }
}
