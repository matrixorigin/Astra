use super::{
    CheckRunId, CriterionRevisionRef, CriterionSetRevision, GoalRevision, GraphRevision,
    WorkBranchId, WorkBranchRevision, WorkChangeRef, WorkContentHash, WorkDomainError, WorkId,
    WorkOwnerId, WorkRevision, WorkSubjectRef, validate_resource_identity,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeSet;

const ACCEPTANCE_DECISION_ID_MAX_CHARS: usize = 64;
pub(crate) const ACCEPTANCE_MAX_GAPS: usize = 32;
pub(crate) const ACCEPTANCE_MAX_CHECK_REFS_PER_GAP: usize = 8;
pub(crate) const ACCEPTANCE_MAX_CHECK_REFS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct AcceptanceDecisionId(String);

impl AcceptanceDecisionId {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        validate_resource_identity(
            "acceptance_decision_id",
            &value,
            ACCEPTANCE_DECISION_ID_MAX_CHARS,
        )?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceGapReason {
    MissingEvidence,
    PartialCoverage,
    StaleEvidence,
    UnsupportedVerifier,
    HumanJudgment,
}

impl AcceptanceGapReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::MissingEvidence => "missing_evidence",
            Self::PartialCoverage => "partial_coverage",
            Self::StaleEvidence => "stale_evidence",
            Self::UnsupportedVerifier => "unsupported_verifier",
            Self::HumanJudgment => "human_judgment",
        }
    }

    pub(crate) fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "missing_evidence" => Some(Self::MissingEvidence),
            "partial_coverage" => Some(Self::PartialCoverage),
            "stale_evidence" => Some(Self::StaleEvidence),
            "unsupported_verifier" => Some(Self::UnsupportedVerifier),
            "human_judgment" => Some(Self::HumanJudgment),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AcceptedCriterionGap {
    pub criterion: CriterionRevisionRef,
    pub reason: AcceptanceGapReason,
    pub check_run_refs: Vec<CheckRunId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AcceptanceDecisionViolation {
    #[error("at least one exact criterion gap must be accepted")]
    EmptyGaps,
    #[error("accepted gaps exceed the bounded limit")]
    TooManyGaps,
    #[error("one criterion may appear only once in an acceptance decision")]
    DuplicateCriterionGap,
    #[error("one gap references too many verifier runs")]
    TooManyCheckRefsForGap,
    #[error("the decision references too many verifier runs")]
    TooManyCheckRefs,
    #[error("one verifier run cannot be reused across accepted gaps")]
    DuplicateCheckRef,
    #[error("partial or stale evidence gaps must identify the exact verifier run")]
    EvidenceGapWithoutCheckRef,
    #[error("missing, unsupported, or human-judgment gaps cannot claim verifier evidence")]
    NonEvidenceGapWithCheckRef,
}

/// Durable, non-inferential user acceptance of exact criterion gaps.
///
/// Every causal revision and evidence identity is explicit. A later Goal,
/// criterion set, graph, branch, or subject change makes this decision
/// inapplicable; callers never search decision text for intent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NewWorkAcceptanceDecision {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub decision_id: AcceptanceDecisionId,
    pub work_revision: WorkRevision,
    pub goal_revision: GoalRevision,
    pub branch_revision: WorkBranchRevision,
    pub graph_revision: GraphRevision,
    pub criterion_set_revision: CriterionSetRevision,
    pub subject_ref: WorkSubjectRef,
    pub subject_revision: WorkContentHash,
    pub accepted_gaps: Vec<AcceptedCriterionGap>,
    pub source_cursor: WorkChangeRef,
}

impl NewWorkAcceptanceDecision {
    pub fn validate(&self) -> Result<(), WorkDomainError> {
        if self.accepted_gaps.is_empty() {
            return Err(invalid(AcceptanceDecisionViolation::EmptyGaps));
        }
        if self.accepted_gaps.len() > ACCEPTANCE_MAX_GAPS {
            return Err(invalid(AcceptanceDecisionViolation::TooManyGaps));
        }
        let mut criteria = BTreeSet::new();
        let mut check_refs = BTreeSet::new();
        let mut check_ref_count = 0usize;
        for gap in &self.accepted_gaps {
            if !criteria.insert(gap.criterion.clone()) {
                return Err(invalid(AcceptanceDecisionViolation::DuplicateCriterionGap));
            }
            if gap.check_run_refs.len() > ACCEPTANCE_MAX_CHECK_REFS_PER_GAP {
                return Err(invalid(AcceptanceDecisionViolation::TooManyCheckRefsForGap));
            }
            let needs_check = matches!(
                gap.reason,
                AcceptanceGapReason::PartialCoverage | AcceptanceGapReason::StaleEvidence
            );
            if needs_check && gap.check_run_refs.is_empty() {
                return Err(invalid(
                    AcceptanceDecisionViolation::EvidenceGapWithoutCheckRef,
                ));
            }
            if !needs_check && !gap.check_run_refs.is_empty() {
                return Err(invalid(
                    AcceptanceDecisionViolation::NonEvidenceGapWithCheckRef,
                ));
            }
            check_ref_count = check_ref_count.saturating_add(gap.check_run_refs.len());
            for check_ref in &gap.check_run_refs {
                if !check_refs.insert(check_ref.clone()) {
                    return Err(invalid(AcceptanceDecisionViolation::DuplicateCheckRef));
                }
            }
        }
        if check_ref_count > ACCEPTANCE_MAX_CHECK_REFS {
            return Err(invalid(AcceptanceDecisionViolation::TooManyCheckRefs));
        }
        Ok(())
    }

    pub(crate) fn canonicalized(mut self) -> Result<Self, WorkDomainError> {
        self.validate()?;
        for gap in &mut self.accepted_gaps {
            gap.check_run_refs.sort_unstable();
        }
        self.accepted_gaps
            .sort_unstable_by(|left, right| left.criterion.cmp(&right.criterion));
        Ok(self)
    }
}

fn invalid(violation: AcceptanceDecisionViolation) -> WorkDomainError {
    WorkDomainError::InvalidAcceptanceDecision { violation }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedWorkAcceptanceDecision {
    pub decision: NewWorkAcceptanceDecision,
    pub payload_hash: WorkContentHash,
    pub decided_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{CriterionId, CriterionRevision};

    fn hash(byte: char) -> WorkContentHash {
        WorkContentHash::parse(format!("sha256:{}", byte.to_string().repeat(64))).expect("hash")
    }

    fn criterion(name: &str) -> CriterionRevisionRef {
        CriterionRevisionRef {
            criterion_id: CriterionId::parse(name).expect("criterion"),
            revision: CriterionRevision::INITIAL,
        }
    }

    fn decision() -> NewWorkAcceptanceDecision {
        NewWorkAcceptanceDecision {
            owner_id: WorkOwnerId::parse("owner-1").expect("owner"),
            work_id: WorkId::parse("work-1").expect("work"),
            branch_id: WorkBranchId::parse("branch-1").expect("branch"),
            decision_id: AcceptanceDecisionId::parse("decision-1").expect("decision"),
            work_revision: WorkRevision::INITIAL,
            goal_revision: GoalRevision::INITIAL,
            branch_revision: WorkBranchRevision::INITIAL,
            graph_revision: GraphRevision::INITIAL,
            criterion_set_revision: CriterionSetRevision::INITIAL,
            subject_ref: WorkSubjectRef::parse("workspace/repository/head").expect("subject"),
            subject_revision: hash('a'),
            accepted_gaps: vec![AcceptedCriterionGap {
                criterion: criterion("criterion-1"),
                reason: AcceptanceGapReason::MissingEvidence,
                check_run_refs: Vec::new(),
            }],
            source_cursor: WorkChangeRef::parse("cursor-1").expect("cursor"),
        }
    }

    #[test]
    fn explicit_gap_reason_controls_whether_check_identity_is_required() {
        decision().validate().expect("missing evidence decision");

        let mut partial = decision();
        partial.accepted_gaps[0].reason = AcceptanceGapReason::PartialCoverage;
        assert!(matches!(
            partial.validate(),
            Err(WorkDomainError::InvalidAcceptanceDecision {
                violation: AcceptanceDecisionViolation::EvidenceGapWithoutCheckRef
            })
        ));
        partial.accepted_gaps[0].check_run_refs =
            vec![CheckRunId::parse("check-1").expect("check")];
        partial.validate().expect("partial evidence decision");
    }

    #[test]
    fn accepted_gaps_are_bounded_unique_and_canonical() {
        let mut value = decision();
        value.accepted_gaps.push(AcceptedCriterionGap {
            criterion: criterion("criterion-0"),
            reason: AcceptanceGapReason::StaleEvidence,
            check_run_refs: vec![
                CheckRunId::parse("check-b").expect("check"),
                CheckRunId::parse("check-a").expect("check"),
            ],
        });
        let canonical = value.canonicalized().expect("canonical decision");
        assert_eq!(
            canonical.accepted_gaps[0].criterion.criterion_id.as_str(),
            "criterion-0"
        );
        assert_eq!(
            canonical.accepted_gaps[0].check_run_refs[0].as_str(),
            "check-a"
        );
    }
}
