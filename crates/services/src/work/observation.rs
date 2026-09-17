use super::{
    AcceptanceDecisionId, CheckRunId, CriterionRevisionRef, CriterionSetRevision, ForkCursorRef,
    GoalRevision, GraphRevision, OriginalIntentRef, ProjectId, WorkBranchId, WorkBranchRevision,
    WorkEventSeq, WorkGoal, WorkId, WorkOwnerId, WorkRevision,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const WORK_OBSERVATION_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, thiserror::Error)]
pub(crate) enum WorkObservationBuildError {
    #[error("incoherent Work observation evidence: {0}")]
    IncoherentEvidence(&'static str),
    #[error("Work observation encoding failed")]
    Encoding(#[from] serde_json::Error),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkObservationQuery {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct WorkContentHash(String);

impl WorkContentHash {
    pub fn parse(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        let bytes = value.as_bytes();
        if bytes.len() != 71 || bytes.get(..7) != Some(b"sha256:") {
            return Err("must use the sha256:<64 lowercase hex> format");
        }
        if !bytes[7..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        {
            return Err("must use the sha256:<64 lowercase hex> format");
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkRetentionState {
    Active,
    Archived,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionAlignment {
    Current,
    Behind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkGoalOverview {
    pub revision: GoalRevision,
    pub goal: WorkGoal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkCriteriaSummary {
    pub revision: CriterionSetRevision,
    pub member_count: u16,
    pub manifest_hash: WorkContentHash,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkGraphSummary {
    pub revision: GraphRevision,
    pub item_count: u16,
    pub edge_count: u16,
    pub manifest_hash: WorkContentHash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkDeliveryStatus {
    CriteriaNotAccepted,
    BranchBasisOutOfDate,
    SubjectUnavailable,
    VerificationRequired,
    ReadyForReview,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkDeliverySummary {
    pub status: WorkDeliveryStatus,
    pub required_criterion_count: u16,
    pub satisfied_criterion_count: u16,
    pub fresh_check_count: u16,
    pub accepted_gap_count: u16,
    pub remaining_criterion_count: u16,
    pub subject_revision: Option<WorkContentHash>,
    pub freshness_valid_until: Option<DateTime<Utc>>,
}

pub(crate) enum WorkDeliverySubjectBasis {
    Unavailable,
    OutOfDate,
    Current(WorkContentHash),
}

impl WorkDeliverySummary {
    pub(crate) fn derive(
        criteria: &[CriterionRevisionRef],
        branch_basis_current: bool,
        subject: WorkDeliverySubjectBasis,
        fresh_checks: BTreeMap<CriterionRevisionRef, Option<DateTime<Utc>>>,
        accepted_gaps: BTreeSet<CriterionRevisionRef>,
    ) -> Result<Self, &'static str> {
        let required_criterion_count =
            u16::try_from(criteria.len()).map_err(|_| "criterion count exceeds u16")?;
        let members = criteria.iter().cloned().collect::<BTreeSet<_>>();
        let fresh_check_criteria = fresh_checks.keys().cloned().collect::<BTreeSet<_>>();
        if members.len() != criteria.len()
            || !fresh_check_criteria.is_subset(&members)
            || !accepted_gaps.is_subset(&members)
        {
            return Err("delivery facts do not belong to the current criterion set");
        }
        let evidence_admissible = required_criterion_count > 0
            && branch_basis_current
            && matches!(&subject, WorkDeliverySubjectBasis::Current(_));
        if !evidence_admissible && (!fresh_checks.is_empty() || !accepted_gaps.is_empty()) {
            return Err("delivery evidence is present without a current delivery basis");
        }
        let fresh_check_count =
            u16::try_from(fresh_checks.len()).map_err(|_| "fresh check count exceeds u16")?;
        let accepted_only = accepted_gaps
            .difference(&fresh_check_criteria)
            .cloned()
            .collect::<BTreeSet<_>>();
        let accepted_gap_count =
            u16::try_from(accepted_only.len()).map_err(|_| "accepted gap count exceeds u16")?;
        let satisfied_criterion_count = fresh_check_count
            .checked_add(accepted_gap_count)
            .ok_or("satisfied criterion count exceeds u16")?;
        let remaining_criterion_count = required_criterion_count
            .checked_sub(satisfied_criterion_count)
            .ok_or("satisfied criterion count exceeds required criteria")?;
        let freshness_valid_until = fresh_checks.values().filter_map(|expiry| *expiry).min();
        let (status, subject_revision) = if required_criterion_count == 0 {
            (WorkDeliveryStatus::CriteriaNotAccepted, None)
        } else if !branch_basis_current {
            (WorkDeliveryStatus::BranchBasisOutOfDate, None)
        } else {
            match subject {
                WorkDeliverySubjectBasis::Unavailable => {
                    (WorkDeliveryStatus::SubjectUnavailable, None)
                }
                WorkDeliverySubjectBasis::OutOfDate => {
                    (WorkDeliveryStatus::SubjectUnavailable, None)
                }
                WorkDeliverySubjectBasis::Current(revision) => (
                    if remaining_criterion_count == 0 {
                        WorkDeliveryStatus::ReadyForReview
                    } else {
                        WorkDeliveryStatus::VerificationRequired
                    },
                    Some(revision),
                ),
            }
        };
        Ok(Self {
            status,
            required_criterion_count,
            satisfied_criterion_count,
            fresh_check_count,
            accepted_gap_count,
            remaining_criterion_count,
            subject_revision,
            freshness_valid_until,
        })
    }
}

/// Public branch projection. Internal session identity is intentionally absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkBranchOverview {
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub branch_revision: WorkBranchRevision,
    pub origin_branch_id: Option<WorkBranchId>,
    pub fork_cursor: Option<ForkCursorRef>,
    pub goal_revision_ref: GoalRevision,
    pub goal_alignment: RevisionAlignment,
    pub criteria_set_revision_ref: CriterionSetRevision,
    pub criteria_alignment: RevisionAlignment,
    pub basis_graph_revision: GraphRevision,
    pub current_graph_revision: GraphRevision,
    pub retention_state: WorkRetentionState,
    pub created_at: DateTime<Utc>,
    pub archived_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkOverview {
    pub work_id: WorkId,
    pub work_revision: WorkRevision,
    pub project_id: Option<ProjectId>,
    pub original_intent_ref: OriginalIntentRef,
    pub goal: WorkGoalOverview,
    pub criteria: WorkCriteriaSummary,
    pub delivery_branch: WorkBranchOverview,
    pub graph: WorkGraphSummary,
    pub delivery: WorkDeliverySummary,
    pub event_head: WorkEventSeq,
    pub retention_state: WorkRetentionState,
    pub created_at: DateTime<Utc>,
    pub archived_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSourceKind {
    Work,
    Goal,
    CriterionSet,
    DeliveryBranch,
    Graph,
    WorkEvents,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ObservationSourceRevision {
    Work {
        revision: WorkRevision,
    },
    Goal {
        revision: GoalRevision,
    },
    CriterionSet {
        revision: CriterionSetRevision,
        content_hash: WorkContentHash,
    },
    DeliveryBranch {
        revision: WorkBranchRevision,
    },
    Graph {
        revision: GraphRevision,
        content_hash: WorkContentHash,
    },
    WorkEvents {
        event_head: WorkEventSeq,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationScope {
    DeclaredWork,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationGapReason {
    SourceUnavailableAtCausalCut,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ObservationCoverageGap {
    pub source: ObservationSourceKind,
    pub reason: ObservationGapReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationCoherence {
    Coherent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkObservationFactCode {
    CriteriaNotAccepted,
    BranchBasisOutOfDate,
    SubjectUnavailable,
    VerificationRequired,
    ReadyForReview,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkObservationCauseCode {
    AcceptedCriteriaEmpty,
    BranchBasisStale,
    CurrentSubjectMissing,
    CurrentEvidenceIncomplete,
    CurrentEvidenceComplete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkObservationFinding {
    pub fact_code: WorkObservationFactCode,
    pub cause_code: WorkObservationCauseCode,
}

impl WorkObservationFinding {
    fn from_delivery(delivery: &WorkDeliverySummary) -> Self {
        let (fact_code, cause_code) = match delivery.status {
            WorkDeliveryStatus::CriteriaNotAccepted => (
                WorkObservationFactCode::CriteriaNotAccepted,
                WorkObservationCauseCode::AcceptedCriteriaEmpty,
            ),
            WorkDeliveryStatus::BranchBasisOutOfDate => (
                WorkObservationFactCode::BranchBasisOutOfDate,
                WorkObservationCauseCode::BranchBasisStale,
            ),
            WorkDeliveryStatus::SubjectUnavailable => (
                WorkObservationFactCode::SubjectUnavailable,
                WorkObservationCauseCode::CurrentSubjectMissing,
            ),
            WorkDeliveryStatus::VerificationRequired => (
                WorkObservationFactCode::VerificationRequired,
                WorkObservationCauseCode::CurrentEvidenceIncomplete,
            ),
            WorkDeliveryStatus::ReadyForReview => (
                WorkObservationFactCode::ReadyForReview,
                WorkObservationCauseCode::CurrentEvidenceComplete,
            ),
        };
        Self {
            fact_code,
            cause_code,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkObservationSatisfactionEvidenceRef {
    CheckRun {
        criterion: CriterionRevisionRef,
        check_run_id: CheckRunId,
        payload_hash: WorkContentHash,
    },
    AcceptanceDecision {
        criterion: CriterionRevisionRef,
        decision_id: AcceptanceDecisionId,
        payload_hash: WorkContentHash,
    },
}

impl WorkObservationSatisfactionEvidenceRef {
    fn criterion(&self) -> &CriterionRevisionRef {
        match self {
            Self::CheckRun { criterion, .. } | Self::AcceptanceDecision { criterion, .. } => {
                criterion
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkObservationCursor {
    pub work_revision: WorkRevision,
    pub goal_revision: GoalRevision,
    pub criteria_set_revision: CriterionSetRevision,
    pub delivery_branch_revision: WorkBranchRevision,
    pub graph_revision: GraphRevision,
    pub event_head: WorkEventSeq,
}

#[derive(Serialize)]
struct WorkObservationContent<'a> {
    schema_version: u16,
    scope: ObservationScope,
    as_of: &'a WorkObservationCursor,
    source_revisions: &'a [ObservationSourceRevision],
    coherence: ObservationCoherence,
    coverage_gaps: &'a [ObservationCoverageGap],
    finding: &'a WorkObservationFinding,
    satisfaction_evidence_refs: &'a [WorkObservationSatisfactionEvidenceRef],
    overview: &'a WorkOverview,
}

/// Content-addressed declared-Work snapshot. Expiring evidence carries its
/// earliest validity boundary so consumers know exactly when to refresh.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkObservationReport {
    schema_version: u16,
    report_id: String,
    content_hash: WorkContentHash,
    scope: ObservationScope,
    as_of: WorkObservationCursor,
    source_revisions: Vec<ObservationSourceRevision>,
    coherence: ObservationCoherence,
    coverage_gaps: Vec<ObservationCoverageGap>,
    finding: WorkObservationFinding,
    satisfaction_evidence_refs: Vec<WorkObservationSatisfactionEvidenceRef>,
    overview: WorkOverview,
}

impl WorkObservationReport {
    pub(crate) fn from_overview(
        overview: WorkOverview,
        satisfaction_evidence_refs: Vec<WorkObservationSatisfactionEvidenceRef>,
    ) -> Result<Self, WorkObservationBuildError> {
        let as_of = WorkObservationCursor {
            work_revision: overview.work_revision,
            goal_revision: overview.goal.revision,
            criteria_set_revision: overview.criteria.revision,
            delivery_branch_revision: overview.delivery_branch.branch_revision,
            graph_revision: overview.graph.revision,
            event_head: overview.event_head,
        };
        let source_revisions = vec![
            ObservationSourceRevision::Work {
                revision: overview.work_revision,
            },
            ObservationSourceRevision::Goal {
                revision: overview.goal.revision,
            },
            ObservationSourceRevision::CriterionSet {
                revision: overview.criteria.revision,
                content_hash: overview.criteria.manifest_hash.clone(),
            },
            ObservationSourceRevision::DeliveryBranch {
                revision: overview.delivery_branch.branch_revision,
            },
            ObservationSourceRevision::Graph {
                revision: overview.graph.revision,
                content_hash: overview.graph.manifest_hash.clone(),
            },
            ObservationSourceRevision::WorkEvents {
                event_head: overview.event_head,
            },
        ];
        let evidence_criteria = satisfaction_evidence_refs
            .iter()
            .map(WorkObservationSatisfactionEvidenceRef::criterion)
            .collect::<BTreeSet<_>>();
        if evidence_criteria.len() != satisfaction_evidence_refs.len() {
            return Err(WorkObservationBuildError::IncoherentEvidence(
                "one current criterion has multiple satisfaction evidence refs",
            ));
        }
        if satisfaction_evidence_refs.len()
            != usize::from(overview.delivery.satisfied_criterion_count)
        {
            return Err(WorkObservationBuildError::IncoherentEvidence(
                "satisfaction evidence refs do not account for every satisfied criterion",
            ));
        }
        let fresh_check_count = satisfaction_evidence_refs
            .iter()
            .filter(|evidence| {
                matches!(
                    evidence,
                    WorkObservationSatisfactionEvidenceRef::CheckRun { .. }
                )
            })
            .count();
        let acceptance_count = satisfaction_evidence_refs.len() - fresh_check_count;
        if fresh_check_count != usize::from(overview.delivery.fresh_check_count)
            || acceptance_count != usize::from(overview.delivery.accepted_gap_count)
        {
            return Err(WorkObservationBuildError::IncoherentEvidence(
                "evidence kinds disagree with delivery coverage",
            ));
        }
        let coverage_gaps = Vec::new();
        let coherence = ObservationCoherence::Coherent;
        let finding = WorkObservationFinding::from_delivery(&overview.delivery);
        let scope = ObservationScope::DeclaredWork;
        let canonical = serde_json::to_vec(&WorkObservationContent {
            schema_version: WORK_OBSERVATION_SCHEMA_VERSION,
            scope,
            as_of: &as_of,
            source_revisions: &source_revisions,
            coherence,
            coverage_gaps: &coverage_gaps,
            finding: &finding,
            satisfaction_evidence_refs: &satisfaction_evidence_refs,
            overview: &overview,
        })?;
        let digest = format!("{:x}", Sha256::digest(canonical));
        Ok(Self {
            schema_version: WORK_OBSERVATION_SCHEMA_VERSION,
            report_id: format!("work-observation:{digest}"),
            content_hash: WorkContentHash(format!("sha256:{digest}")),
            scope,
            as_of,
            source_revisions,
            coherence,
            coverage_gaps,
            finding,
            satisfaction_evidence_refs,
            overview,
        })
    }

    pub fn report_id(&self) -> &str {
        &self.report_id
    }

    pub fn content_hash(&self) -> &WorkContentHash {
        &self.content_hash
    }

    pub fn scope(&self) -> ObservationScope {
        self.scope
    }

    pub fn as_of(&self) -> &WorkObservationCursor {
        &self.as_of
    }

    pub fn source_revisions(&self) -> &[ObservationSourceRevision] {
        &self.source_revisions
    }

    pub fn coherence(&self) -> ObservationCoherence {
        self.coherence
    }

    pub fn coverage_gaps(&self) -> &[ObservationCoverageGap] {
        &self.coverage_gaps
    }

    pub fn finding(&self) -> &WorkObservationFinding {
        &self.finding
    }

    pub fn satisfaction_evidence_refs(&self) -> &[WorkObservationSatisfactionEvidenceRef] {
        &self.satisfaction_evidence_refs
    }

    pub fn overview(&self) -> &WorkOverview {
        &self.overview
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn hash(byte: char) -> WorkContentHash {
        WorkContentHash::parse(format!("sha256:{}", byte.to_string().repeat(64))).expect("hash")
    }

    fn criterion(id: &str) -> CriterionRevisionRef {
        CriterionRevisionRef {
            criterion_id: super::super::CriterionId::parse(id).expect("criterion"),
            revision: super::super::CriterionRevision::INITIAL,
        }
    }

    fn overview(goal: WorkGoal) -> WorkOverview {
        let created_at = Utc
            .with_ymd_and_hms(2026, 8, 1, 12, 0, 0)
            .single()
            .expect("timestamp");
        WorkOverview {
            work_id: WorkId::parse("work-1").expect("work"),
            work_revision: WorkRevision::INITIAL,
            project_id: None,
            original_intent_ref: OriginalIntentRef::parse("intent-1").expect("intent"),
            goal: WorkGoalOverview {
                revision: GoalRevision::INITIAL,
                goal,
            },
            criteria: WorkCriteriaSummary {
                revision: CriterionSetRevision::INITIAL,
                member_count: 0,
                manifest_hash: hash('a'),
            },
            delivery_branch: WorkBranchOverview {
                work_id: WorkId::parse("work-1").expect("work"),
                branch_id: WorkBranchId::parse("branch-1").expect("branch"),
                branch_revision: WorkBranchRevision::INITIAL,
                origin_branch_id: None,
                fork_cursor: None,
                goal_revision_ref: GoalRevision::INITIAL,
                goal_alignment: RevisionAlignment::Current,
                criteria_set_revision_ref: CriterionSetRevision::INITIAL,
                criteria_alignment: RevisionAlignment::Current,
                basis_graph_revision: GraphRevision::INITIAL,
                current_graph_revision: GraphRevision::INITIAL,
                retention_state: WorkRetentionState::Active,
                created_at,
                archived_at: None,
            },
            graph: WorkGraphSummary {
                revision: GraphRevision::INITIAL,
                item_count: 1,
                edge_count: 0,
                manifest_hash: hash('b'),
            },
            delivery: WorkDeliverySummary::derive(
                &[],
                true,
                WorkDeliverySubjectBasis::Unavailable,
                BTreeMap::new(),
                BTreeSet::new(),
            )
            .expect("delivery"),
            event_head: WorkEventSeq::INITIAL,
            retention_state: WorkRetentionState::Active,
            created_at,
            archived_at: None,
        }
    }

    #[test]
    fn report_identity_is_deterministic_and_shell_is_bounded() {
        let overview = overview(
            WorkGoal::parse("x".repeat(super::super::WORK_GOAL_MAX_BYTES)).expect("maximum goal"),
        );
        let first =
            WorkObservationReport::from_overview(overview.clone(), Vec::new()).expect("report");
        let second = WorkObservationReport::from_overview(overview, Vec::new()).expect("report");
        assert_eq!(first, second);
        assert_eq!(first.coherence(), ObservationCoherence::Coherent);
        assert!(first.coverage_gaps().is_empty());
        assert!(first.satisfaction_evidence_refs().is_empty());
        assert_eq!(
            first.finding(),
            &WorkObservationFinding {
                fact_code: WorkObservationFactCode::CriteriaNotAccepted,
                cause_code: WorkObservationCauseCode::AcceptedCriteriaEmpty,
            }
        );
        assert_eq!(first.source_revisions().len(), 6);
        let wire = serde_json::to_vec(&first).expect("wire");
        assert!(
            wire.len() < 64 * 1024,
            "declared-Work shell was {} bytes",
            wire.len()
        );
        let value: serde_json::Value = serde_json::from_slice(&wire).expect("wire JSON");
        assert!(
            value["overview"]["delivery_branch"]
                .get("session_id")
                .is_none()
        );
    }

    #[test]
    fn content_hash_parser_rejects_ambiguous_encodings() {
        for invalid in [
            "sha256:abc".to_string(),
            format!("SHA256:{}", "a".repeat(64)),
            format!("sha256:{}", "A".repeat(64)),
            format!("sha256:{}", "g".repeat(64)),
        ] {
            assert!(WorkContentHash::parse(invalid).is_err());
        }
    }

    #[test]
    fn delivery_requires_nonempty_current_criteria_and_exact_satisfaction_facts() {
        let first = criterion("criterion-a");
        let second = criterion("criterion-b");
        let ready = WorkDeliverySummary::derive(
            &[first.clone(), second.clone()],
            true,
            WorkDeliverySubjectBasis::Current(hash('c')),
            BTreeMap::from([(first, None)]),
            BTreeSet::from([second]),
        )
        .expect("ready delivery");
        assert_eq!(ready.status, WorkDeliveryStatus::ReadyForReview);
        assert_eq!(ready.satisfied_criterion_count, 2);
        assert_eq!(ready.fresh_check_count, 1);
        assert_eq!(ready.accepted_gap_count, 1);
        assert_eq!(ready.remaining_criterion_count, 0);

        let empty = WorkDeliverySummary::derive(
            &[],
            true,
            WorkDeliverySubjectBasis::Current(hash('d')),
            BTreeMap::new(),
            BTreeSet::new(),
        )
        .expect("empty criteria are explicit");
        assert_eq!(empty.status, WorkDeliveryStatus::CriteriaNotAccepted);
        assert!(empty.subject_revision.is_none());

        assert!(
            WorkDeliverySummary::derive(
                &[criterion("criterion-a")],
                true,
                WorkDeliverySubjectBasis::Current(hash('e')),
                BTreeMap::from([(criterion("criterion-outside"), None)]),
                BTreeSet::new(),
            )
            .is_err(),
            "facts outside the current set must not be silently dropped"
        );
        assert!(
            WorkDeliverySummary::derive(
                &[criterion("criterion-a")],
                false,
                WorkDeliverySubjectBasis::Current(hash('f')),
                BTreeMap::from([(criterion("criterion-a"), None)]),
                BTreeSet::new(),
            )
            .is_err(),
            "evidence cannot survive a stale branch basis"
        );
    }

    #[test]
    fn report_fails_closed_when_evidence_identity_or_kind_is_incoherent() {
        let checked = criterion("criterion-checked");
        let accepted = criterion("criterion-accepted");
        let mut ready = overview(WorkGoal::parse("Verify exact evidence.").expect("goal"));
        ready.criteria.member_count = 2;
        ready.delivery = WorkDeliverySummary::derive(
            &[checked.clone(), accepted.clone()],
            true,
            WorkDeliverySubjectBasis::Current(hash('c')),
            BTreeMap::from([(checked.clone(), None)]),
            BTreeSet::from([accepted.clone()]),
        )
        .expect("ready delivery");
        let check = WorkObservationSatisfactionEvidenceRef::CheckRun {
            criterion: checked,
            check_run_id: CheckRunId::parse("check-1").expect("check"),
            payload_hash: hash('d'),
        };
        let acceptance = WorkObservationSatisfactionEvidenceRef::AcceptanceDecision {
            criterion: accepted,
            decision_id: AcceptanceDecisionId::parse("acceptance-1").expect("decision"),
            payload_hash: hash('e'),
        };
        let report = WorkObservationReport::from_overview(
            ready.clone(),
            vec![check.clone(), acceptance.clone()],
        )
        .expect("coherent evidence");
        assert_eq!(report.satisfaction_evidence_refs().len(), 2);
        assert_eq!(
            report.finding(),
            &WorkObservationFinding {
                fact_code: WorkObservationFactCode::ReadyForReview,
                cause_code: WorkObservationCauseCode::CurrentEvidenceComplete,
            }
        );

        assert!(matches!(
            WorkObservationReport::from_overview(ready.clone(), vec![check.clone(), check]),
            Err(WorkObservationBuildError::IncoherentEvidence(_))
        ));
        assert!(matches!(
            WorkObservationReport::from_overview(ready, vec![acceptance]),
            Err(WorkObservationBuildError::IncoherentEvidence(_))
        ));
    }

    #[test]
    fn rust_projection_matches_the_cross_language_v1_fixture() {
        let report = WorkObservationReport::from_overview(
            overview(WorkGoal::parse("Ship the typed Work read contract.").expect("goal")),
            Vec::new(),
        )
        .expect("report");
        let actual = serde_json::to_value(report).expect("Rust wire value");
        let expected: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../fixtures/contracts/work_observation_v1.json"
        ))
        .expect("shared fixture");
        assert_eq!(actual, expected);
    }
}
