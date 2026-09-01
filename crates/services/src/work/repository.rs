use super::{
    CheckRunId, CriterionId, CriterionRevision, CriterionRevisionRef, CriterionSetMemberChange,
    CriterionSetRevision, GoalRevision, GraphRevision, InternalSessionId,
    NewWorkAcceptanceDecision, NewWorkCheckRun, NewWorkPlanProposal, OriginalIntentRef, ProjectId,
    RecordedWorkAcceptanceDecision, RecordedWorkCheckRun, RecordedWorkPlanProposal,
    WorkAttentionCursorAdvance, WorkAttentionReceipt, WorkBranchBasisChange, WorkBranchId,
    WorkBranchRecord, WorkBranchRecordParts, WorkBranchRevision, WorkBranchSubject,
    WorkBranchSubjectChange, WorkChangeReason, WorkChangeRef, WorkCriteriaChange, WorkDomainError,
    WorkEventPage, WorkEventQuery, WorkGoal, WorkGraphChange, WorkId, WorkItemRevisionRef,
    WorkObservationQuery, WorkObservationReport, WorkOwnerId, WorkPatchArtifact,
    WorkPatchArtifactBasisResource, WorkPatchArtifactId, WorkPlanContext,
    WorkPlanProposalAcceptance, WorkProposalStatus, WorkRecord, WorkRecordParts, WorkRevision,
    WorkSessionPlanBinding, WorkTaskExecutionSnapshot, WorkTaskGraphPage, WorkTaskGraphQuery,
};
use astra_core::SharedPool;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{MySql, QueryBuilder, Row, Transaction, query};
use std::collections::BTreeSet;
use thiserror::Error;

const GENESIS_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub(super) const CRITERION_DEFINITION_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkConflictResource {
    WorkIdentity,
    InternalSessionIdentity,
    BranchSessionBinding,
    GoalRevision,
    CriterionSetRevision,
    GraphRevision,
    CriterionIdentity,
    CriterionRevision,
    WorkItemIdentity,
    WorkItemRevision,
    WorkItemEdge,
    CheckRunIdentity,
    AcceptanceDecisionIdentity,
    WorkEventSequence,
    WorkEventIdentity,
    WorkAttentionReceipt,
    WorkProposalIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkCheckBasisResource {
    Branch,
    GraphRevision,
    WorkItemRevision,
    RunBinding,
    CriterionSetRevision,
    CriterionRevision,
    CriterionSetMembership,
    Subject,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkAcceptanceBasisResource {
    WorkRevision,
    GoalRevision,
    BranchRevision,
    GraphRevision,
    CriterionSetRevision,
    CriterionMembership,
    CheckRunCriterion,
    CheckRunApplicability,
    Subject,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkProposalBasisResource {
    ProposalPayloadHash,
    BranchIdentity,
    WorkRevision,
    GoalRevision,
    CriterionSetRevision,
    BranchGoalRevision,
    BranchCriterionSetRevision,
    BranchRevision,
    GraphRevision,
    WorkItemRevision,
    DependencyEndpoint,
    DependencyIdentity,
    NewItemIdentity,
    NewCriterionIdentity,
}

#[derive(Debug, Error)]
pub enum WorkRepositoryError {
    #[error("invalid Work mutation: {source}")]
    InvalidMutation {
        #[source]
        source: WorkDomainError,
    },
    #[error("Work was not found")]
    NotFound,
    #[error("the existing session cannot be bound to a Work")]
    SessionNotBindable,
    #[error("the existing session has an active run")]
    SessionBusy,
    #[error("canonical Work identity conflict: {resource:?}")]
    Conflict { resource: WorkConflictResource },
    #[error("corrupt persisted {entity}: {source}")]
    Corrupt {
        entity: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("Work repository {operation} failed: {source}")]
    Persistence {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error("Work patch artifact conflict: {resource:?}")]
    PatchArtifactConflict {
        resource: WorkPatchArtifactBasisResource,
    },
    #[error("failed to encode canonical {entity}: {source}")]
    ManifestEncoding {
        entity: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("Work is archived")]
    Archived,
    #[error("the delivery branch cannot be archived until another branch is selected")]
    DeliveryBranchProtected,
    #[error("an active run still owns the branch session")]
    BranchActive,
    #[error("the Work branch has a durable deletion in progress")]
    BranchDeleting,
    #[error("branch retention has a stale or incoherent {resource:?} basis")]
    StaleBranchRetention {
        resource: super::WorkBranchRetentionBasisResource,
    },
    #[error("delivery branch selection has a stale or incoherent {resource:?} basis")]
    StaleDeliverySelection {
        resource: super::WorkDeliverySelectionBasisResource,
    },
    #[error(
        "stale Goal change: expected Work r{expected_work_revision:?}/Goal r{expected_goal_revision:?}, found Work r{actual_work_revision:?}/Goal r{actual_goal_revision:?}"
    )]
    StaleGoalRevision {
        expected_work_revision: WorkRevision,
        actual_work_revision: WorkRevision,
        expected_goal_revision: GoalRevision,
        actual_goal_revision: GoalRevision,
    },
    #[error(
        "stale criterion-set change: expected Work r{expected_work_revision:?}/set r{expected_criteria_set_revision:?}, found Work r{actual_work_revision:?}/set r{actual_criteria_set_revision:?}"
    )]
    StaleCriteriaRevision {
        expected_work_revision: WorkRevision,
        actual_work_revision: WorkRevision,
        expected_criteria_set_revision: CriterionSetRevision,
        actual_criteria_set_revision: CriterionSetRevision,
    },
    #[error(
        "stale branch basis adoption: expected Work r{expected_work_revision:?}, branch r{expected_branch_revision:?}/Goal r{expected_goal_revision:?}/set r{expected_criteria_set_revision:?}; found Work r{actual_work_revision:?}, branch r{actual_branch_revision:?}/Goal r{actual_goal_revision:?}/set r{actual_criteria_set_revision:?}"
    )]
    StaleBranchBasis {
        expected_work_revision: WorkRevision,
        actual_work_revision: WorkRevision,
        expected_branch_revision: WorkBranchRevision,
        actual_branch_revision: WorkBranchRevision,
        expected_goal_revision: GoalRevision,
        actual_goal_revision: GoalRevision,
        expected_criteria_set_revision: CriterionSetRevision,
        actual_criteria_set_revision: CriterionSetRevision,
    },
    #[error(
        "branch basis target Goal r{target_goal_revision:?}/set r{target_criteria_set_revision:?} is not current Work Goal r{current_goal_revision:?}/set r{current_criteria_set_revision:?}"
    )]
    InvalidBranchBasisTarget {
        target_goal_revision: GoalRevision,
        current_goal_revision: GoalRevision,
        target_criteria_set_revision: CriterionSetRevision,
        current_criteria_set_revision: CriterionSetRevision,
    },
    #[error("criterion set references missing immutable criterion revisions: {missing:?}")]
    MissingCriterionRevisions { missing: Vec<CriterionRevisionRef> },
    #[error(
        "stale graph change: expected branch r{expected_branch_revision:?}/graph r{expected_graph_revision:?}, found branch r{actual_branch_revision:?}/graph r{actual_graph_revision:?}"
    )]
    StaleGraphRevision {
        expected_branch_revision: WorkBranchRevision,
        actual_branch_revision: WorkBranchRevision,
        expected_graph_revision: GraphRevision,
        actual_graph_revision: GraphRevision,
    },
    #[error(
        "stale branch subject basis: expected branch r{expected_branch_revision:?}/graph r{expected_graph_revision:?}, found branch r{actual_branch_revision:?}/graph r{actual_graph_revision:?}"
    )]
    StaleSubjectBasis {
        expected_branch_revision: WorkBranchRevision,
        actual_branch_revision: WorkBranchRevision,
        expected_graph_revision: GraphRevision,
        actual_graph_revision: GraphRevision,
    },
    #[error("graph references missing immutable WorkItem revisions: {missing:?}")]
    MissingWorkItemRevisions { missing: Vec<WorkItemRevisionRef> },
    #[error("check run references missing or incoherent {resource:?}")]
    InvalidCheckBasis { resource: WorkCheckBasisResource },
    #[error("criterion kind {criterion_kind} cannot be verified by {verifier_kind}")]
    CheckVerifierMismatch {
        criterion_kind: &'static str,
        verifier_kind: &'static str,
    },
    #[error(
        "stale check graph basis: verifier used graph r{evidence_graph_revision:?}, branch is at r{current_graph_revision:?}"
    )]
    StaleCheckGraphRevision {
        evidence_graph_revision: GraphRevision,
        current_graph_revision: GraphRevision,
    },
    #[error("acceptance decision has a stale or incoherent {resource:?} basis")]
    InvalidAcceptanceBasis {
        resource: WorkAcceptanceBasisResource,
    },
    #[error("acceptance decision references missing verifier runs: {missing:?}")]
    MissingAcceptanceCheckRuns { missing: Vec<CheckRunId> },
    #[error(
        "Work attention cursor {through_event_seq} is ahead of the committed event head {event_head}"
    )]
    EventCursorAhead {
        through_event_seq: i64,
        event_head: i64,
    },
    #[error("Work proposal has a stale or incoherent {resource:?} basis")]
    InvalidWorkProposalBasis { resource: WorkProposalBasisResource },
    #[error("the branch already has the maximum number of pending work proposals")]
    WorkProposalCapacityExceeded,
    #[error("work proposal is already {status:?}")]
    WorkProposalAlreadyResolved { status: WorkProposalStatus },
    #[error(
        "Task Graph page is pinned to graph r{expected_graph_revision:?}, branch is at r{actual_graph_revision:?}"
    )]
    StaleTaskGraphRevision {
        expected_graph_revision: GraphRevision,
        actual_graph_revision: GraphRevision,
    },
    #[error(
        "Task Graph cursor is outside the pinned graph: item {item_offset}/{item_count}, dependency {dependency_offset}/{dependency_count}"
    )]
    TaskGraphCursorAhead {
        item_offset: u16,
        item_count: u16,
        dependency_offset: u16,
        dependency_count: u16,
    },
    #[error(
        "Work criteria page is pinned to set r{expected_criteria_set_revision:?}, Work is at r{actual_criteria_set_revision:?}"
    )]
    StaleCriteriaPageRevision {
        expected_criteria_set_revision: CriterionSetRevision,
        actual_criteria_set_revision: CriterionSetRevision,
    },
    #[error("Work criteria cursor offset {offset} is ahead of member count {member_count}")]
    CriteriaPageCursorAhead { offset: u16, member_count: u16 },
}

impl WorkRepositoryError {
    pub(super) fn persistence(operation: &'static str, source: sqlx::Error) -> Self {
        Self::Persistence { operation, source }
    }

    pub(super) fn insert(
        operation: &'static str,
        resource: WorkConflictResource,
        source: sqlx::Error,
    ) -> Self {
        if source
            .as_database_error()
            .is_some_and(|error| error.is_unique_violation())
        {
            Self::Conflict { resource }
        } else {
            Self::persistence(operation, source)
        }
    }

    pub(super) fn corrupt(
        entity: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Corrupt {
            entity,
            source: Box::new(source),
        }
    }
}

pub(super) fn invalid_mutation(source: WorkDomainError) -> WorkRepositoryError {
    WorkRepositoryError::InvalidMutation { source }
}

pub(super) async fn rollback_transaction(
    transaction: Transaction<'_, MySql>,
    operation: &'static str,
    original_error: WorkRepositoryError,
) -> WorkRepositoryError {
    match transaction.rollback().await {
        Ok(()) => original_error,
        Err(source) => WorkRepositoryError::persistence(operation, source),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkGenesisParts {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub session_id: InternalSessionId,
    pub project_id: Option<ProjectId>,
    pub original_intent_ref: OriginalIntentRef,
    pub goal: WorkGoal,
    pub criteria: Vec<super::NewWorkCriterion>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkGenesis {
    owner_id: WorkOwnerId,
    work_id: WorkId,
    branch_id: WorkBranchId,
    session_id: InternalSessionId,
    project_id: Option<ProjectId>,
    original_intent_ref: OriginalIntentRef,
    goal: WorkGoal,
    criteria: Vec<super::NewWorkCriterion>,
    root_item: super::NewWorkItem,
}

impl WorkGenesis {
    /// Build the one canonical initial shape for every Work.
    ///
    /// The root milestone is deterministic and minimal. It keeps short Work
    /// representable without inventing model-authored subtasks or allowing an
    /// empty graph to look complete.
    pub fn new(parts: WorkGenesisParts) -> Result<Self, WorkDomainError> {
        let WorkGenesisParts {
            owner_id,
            work_id,
            branch_id,
            session_id,
            project_id,
            original_intent_ref,
            goal,
            criteria,
        } = parts;
        let criteria = super::NewWorkCriterion::canonicalize_set(criteria)?;
        let root_item = super::NewWorkItem {
            item_id: super::WorkItemId::root(),
            kind: super::WorkItemKind::Milestone,
            objective: super::WorkItemText::parse(goal.as_str())?,
            expected_result: super::WorkItemText::parse(
                "The Work goal has a reviewable outcome with explicit verification evidence.",
            )?,
        };
        Ok(Self {
            owner_id,
            work_id,
            branch_id,
            session_id,
            project_id,
            original_intent_ref,
            goal,
            criteria,
            root_item,
        })
    }

    /// Reuse an owner-visible conversation as the Work branch conversation.
    ///
    /// Eligibility and ownership are deliberately enforced by the repository
    /// in the same transaction that creates the Work. This method only changes
    /// the proposed genesis identity; it does not make a session bindable.
    pub fn in_session(mut self, session_id: InternalSessionId) -> Self {
        self.session_id = session_id;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkGoalChange {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub expected_work_revision: WorkRevision,
    pub expected_goal_revision: GoalRevision,
    pub goal: WorkGoal,
    pub source_ref: super::WorkChangeRef,
    pub reason: Option<WorkChangeReason>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedWork {
    pub work: WorkRecord,
    pub delivery_branch: WorkBranchRecord,
}

#[async_trait]
pub trait WorkRepository: Send + Sync {
    async fn create_genesis(
        &self,
        genesis: WorkGenesis,
    ) -> Result<CreatedWork, WorkRepositoryError>;

    /// Create canonical Work state around an existing active, idle session.
    /// The owner/session check, execution-slot check, and branch binding are
    /// one transaction so a turn cannot race the promotion boundary.
    async fn create_genesis_in_existing_session(
        &self,
        genesis: WorkGenesis,
    ) -> Result<CreatedWork, WorkRepositoryError>;

    /// Promote the conversation while its exact current run owns the session
    /// execution slot. This is the model-tool path: a different or missing run
    /// identity fails closed, so Work creation cannot race another writer.
    async fn create_genesis_in_running_session(
        &self,
        genesis: WorkGenesis,
        run_id: &str,
    ) -> Result<CreatedWork, WorkRepositoryError>;

    async fn load(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
    ) -> Result<CreatedWork, WorkRepositoryError>;

    async fn list_catalog(
        &self,
        query: super::WorkCatalogQuery,
    ) -> Result<super::WorkCatalogPage, WorkRepositoryError>;

    async fn revise_goal(&self, change: WorkGoalChange)
    -> Result<CreatedWork, WorkRepositoryError>;

    async fn accept_criteria(
        &self,
        change: WorkCriteriaChange,
    ) -> Result<CreatedWork, WorkRepositoryError>;

    async fn adopt_branch_basis(
        &self,
        change: WorkBranchBasisChange,
    ) -> Result<WorkBranchRecord, WorkRepositoryError>;

    async fn replace_graph(
        &self,
        change: WorkGraphChange,
    ) -> Result<WorkBranchRecord, WorkRepositoryError>;

    async fn set_branch_subject(
        &self,
        change: WorkBranchSubjectChange,
    ) -> Result<WorkBranchSubject, WorkRepositoryError>;

    async fn invalidate_branch_subject(
        &self,
        invalidation: super::WorkBranchSubjectInvalidation,
    ) -> Result<WorkBranchRevision, WorkRepositoryError>;

    async fn load_branch_subject(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
    ) -> Result<Option<WorkBranchSubject>, WorkRepositoryError>;

    async fn record_patch_artifact(
        &self,
        artifact: super::NewWorkPatchArtifact,
    ) -> Result<WorkPatchArtifact, WorkRepositoryError>;

    async fn load_patch_artifact(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        patch_artifact_id: &WorkPatchArtifactId,
    ) -> Result<Option<WorkPatchArtifact>, WorkRepositoryError>;

    async fn load_patch_artifact_content(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        patch_artifact_id: &WorkPatchArtifactId,
    ) -> Result<Option<super::WorkPatchArtifactContent>, WorkRepositoryError>;

    async fn list_patch_artifacts(
        &self,
        query: super::WorkPatchArtifactQuery,
    ) -> Result<super::WorkPatchArtifactPage, WorkRepositoryError>;

    async fn propose_plan(
        &self,
        proposal: NewWorkPlanProposal,
    ) -> Result<RecordedWorkPlanProposal, WorkRepositoryError>;

    async fn load_plan_proposal(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        proposal_id: &super::WorkProposalId,
    ) -> Result<Option<RecordedWorkPlanProposal>, WorkRepositoryError>;

    async fn accept_plan_proposal(
        &self,
        acceptance: WorkPlanProposalAcceptance,
    ) -> Result<RecordedWorkPlanProposal, WorkRepositoryError>;

    async fn propose_criteria(
        &self,
        proposal: super::NewWorkCriteriaProposal,
    ) -> Result<super::RecordedWorkCriteriaProposal, WorkRepositoryError>;

    async fn load_criteria_proposal(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        proposal_id: &super::WorkProposalId,
    ) -> Result<Option<super::RecordedWorkCriteriaProposal>, WorkRepositoryError>;

    async fn list_pending_criteria_proposals(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
    ) -> Result<Vec<super::RecordedWorkCriteriaProposal>, WorkRepositoryError>;

    async fn accept_criteria_proposal(
        &self,
        acceptance: super::WorkCriteriaProposalAcceptance,
    ) -> Result<super::RecordedWorkCriteriaProposal, WorkRepositoryError>;

    async fn reject_criteria_proposal(
        &self,
        rejection: super::WorkCriteriaProposalRejection,
    ) -> Result<super::RecordedWorkCriteriaProposal, WorkRepositoryError>;

    async fn load_plan_context_for_session(
        &self,
        owner_id: &WorkOwnerId,
        session_id: &InternalSessionId,
    ) -> Result<WorkPlanContext, WorkRepositoryError>;

    /// Server-internal, coherent full-graph execution view for selecting the
    /// next foreground Work task. This is not a client pagination surface.
    async fn load_task_execution_snapshot_for_session(
        &self,
        owner_id: &WorkOwnerId,
        session_id: &InternalSessionId,
    ) -> Result<WorkTaskExecutionSnapshot, WorkRepositoryError>;

    async fn load_task_graph_page(
        &self,
        query: WorkTaskGraphQuery,
    ) -> Result<WorkTaskGraphPage, WorkRepositoryError>;

    async fn load_criteria_page(
        &self,
        query: super::WorkCriteriaQuery,
    ) -> Result<super::WorkCriteriaPage, WorkRepositoryError>;

    async fn load_session_item_runtime_binding(
        &self,
        owner_id: &WorkOwnerId,
        session_id: &InternalSessionId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        item: &super::WorkItemRevisionRef,
    ) -> Result<WorkSessionPlanBinding, WorkRepositoryError>;

    async fn load_session_plan_binding(
        &self,
        owner_id: &WorkOwnerId,
        session_id: &InternalSessionId,
    ) -> Result<WorkSessionPlanBinding, WorkRepositoryError>;

    async fn load_branch_runtime_binding(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
    ) -> Result<super::WorkBranchRuntimeBinding, WorkRepositoryError>;

    async fn observe_declared_work(
        &self,
        observation: WorkObservationQuery,
    ) -> Result<WorkObservationReport, WorkRepositoryError>;

    async fn select_delivery_branch(
        &self,
        selection: super::WorkDeliverySelection,
    ) -> Result<super::WorkDeliverySelectionReceipt, WorkRepositoryError>;

    async fn change_branch_retention(
        &self,
        change: super::WorkBranchRetentionChange,
    ) -> Result<super::WorkBranchRetentionReceipt, WorkRepositoryError>;

    async fn record_check_run(
        &self,
        check: NewWorkCheckRun,
    ) -> Result<RecordedWorkCheckRun, WorkRepositoryError>;

    async fn accept_gaps(
        &self,
        decision: NewWorkAcceptanceDecision,
    ) -> Result<RecordedWorkAcceptanceDecision, WorkRepositoryError>;

    async fn advance_attention_cursor(
        &self,
        advance: WorkAttentionCursorAdvance,
    ) -> Result<WorkAttentionReceipt, WorkRepositoryError>;

    async fn list_events(
        &self,
        query: WorkEventQuery,
    ) -> Result<WorkEventPage, WorkRepositoryError>;
}

#[derive(Clone, Debug)]
pub struct DatabaseWorkRepository {
    pub(super) pool: SharedPool,
}

impl DatabaseWorkRepository {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }
}

#[derive(Serialize)]
struct CriterionSetManifestV1 {
    schema_version: u32,
    members: Vec<CriterionRevisionRef>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CriterionSetManifestWire {
    schema_version: u32,
    members: Vec<CriterionRevisionRefWire>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CriterionRevisionRefWire {
    criterion_id: String,
    revision: i64,
}

pub(super) fn decode_criterion_set_manifest(
    manifest_json: &str,
    member_count: i64,
) -> Result<Vec<CriterionRevisionRef>, WorkRepositoryError> {
    let manifest: CriterionSetManifestWire = serde_json::from_str(manifest_json)
        .map_err(|source| WorkRepositoryError::corrupt("criterion-set manifest", source))?;
    if manifest.schema_version != GENESIS_MANIFEST_SCHEMA_VERSION
        || i64::try_from(manifest.members.len()).ok() != Some(member_count)
        || manifest.members.len() > super::criteria::CRITERION_SET_MAX_MEMBERS
    {
        return Err(WorkRepositoryError::corrupt(
            "criterion-set manifest",
            std::io::Error::other("schema version or member count violates the set contract"),
        ));
    }
    let members = manifest
        .members
        .into_iter()
        .map(|member| {
            Ok(CriterionRevisionRef {
                criterion_id: CriterionId::parse(member.criterion_id).map_err(|source| {
                    WorkRepositoryError::corrupt("criterion-set manifest", source)
                })?,
                revision: CriterionRevision::new(member.revision).map_err(|source| {
                    WorkRepositoryError::corrupt("criterion-set manifest", source)
                })?,
            })
        })
        .collect::<Result<Vec<_>, WorkRepositoryError>>()?;
    if members.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(WorkRepositoryError::corrupt(
            "criterion-set manifest",
            std::io::Error::other("members are not canonical and unique"),
        ));
    }
    Ok(members)
}

#[derive(Serialize)]
struct GenesisGraphManifestV1<'a> {
    schema_version: u32,
    item_revisions: &'a [super::WorkItemRevisionRef],
    edges: &'a [super::WorkItemEdge],
}

pub(super) fn canonical_json(
    entity: &'static str,
    value: &impl Serialize,
) -> Result<String, WorkRepositoryError> {
    serde_json::to_string(value)
        .map_err(|source| WorkRepositoryError::ManifestEncoding { entity, source })
}

pub(super) fn content_hash(payload: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(payload.as_bytes()))
}

struct GenesisManifests {
    criterion_hash: String,
    criterion_manifest: String,
    graph_hash: String,
    item_manifest: String,
    edge_manifest: String,
}

#[derive(Serialize)]
struct CriterionDefinitionV1<'a> {
    schema_version: u32,
    definition: &'a super::CriterionDefinition,
}

struct EncodedNewCriterion<'a> {
    criterion: &'a super::NewWorkCriterion,
    definition_json: String,
    definition_hash: String,
}

fn encode_new_criteria(
    members: &[CriterionSetMemberChange],
) -> Result<Vec<EncodedNewCriterion<'_>>, WorkRepositoryError> {
    members
        .iter()
        .filter_map(|member| match member {
            CriterionSetMemberChange::Existing(_) => None,
            CriterionSetMemberChange::New(criterion) => Some(criterion),
        })
        .map(|criterion| {
            let definition_json = canonical_json(
                "criterion definition",
                &CriterionDefinitionV1 {
                    schema_version: CRITERION_DEFINITION_SCHEMA_VERSION,
                    definition: &criterion.definition,
                },
            )?;
            Ok(EncodedNewCriterion {
                criterion,
                definition_hash: content_hash(&definition_json),
                definition_json,
            })
        })
        .collect()
}

async fn verify_existing_criterion_revisions(
    transaction: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    members: &[CriterionSetMemberChange],
) -> Result<(), WorkRepositoryError> {
    let expected = members
        .iter()
        .filter_map(|member| match member {
            CriterionSetMemberChange::Existing(reference) => Some(reference.clone()),
            CriterionSetMemberChange::New(_) => None,
        })
        .collect::<Vec<_>>();
    if expected.is_empty() {
        return Ok(());
    }

    let mut builder = QueryBuilder::<MySql>::new(
        "SELECT criterion_id, revision FROM work_criterion_revisions WHERE owner_id = ",
    );
    builder
        .push_bind(owner_id.as_str())
        .push(" AND work_id = ")
        .push_bind(work_id.as_str())
        .push(" AND (");
    for (index, reference) in expected.iter().enumerate() {
        if index > 0 {
            builder.push(" OR ");
        }
        builder
            .push("(criterion_id = ")
            .push_bind(reference.criterion_id.as_str())
            .push(" AND revision = ")
            .push_bind(reference.revision.get())
            .push(")");
    }
    builder.push(")");
    let found = builder
        .build()
        .fetch_all(&mut **transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("validate criterion revision references", source)
        })?
        .into_iter()
        .map(|row| {
            let criterion_id: String = row
                .try_get("criterion_id")
                .map_err(|source| WorkRepositoryError::corrupt("criterion revision", source))?;
            let revision: i64 = row
                .try_get("revision")
                .map_err(|source| WorkRepositoryError::corrupt("criterion revision", source))?;
            Ok((criterion_id, revision))
        })
        .collect::<Result<BTreeSet<_>, WorkRepositoryError>>()?;
    let missing = expected
        .into_iter()
        .filter(|reference| {
            !found.contains(&(
                reference.criterion_id.as_str().to_string(),
                reference.revision.get(),
            ))
        })
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(WorkRepositoryError::MissingCriterionRevisions { missing })
    }
}

async fn insert_new_criteria(
    transaction: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    source_kind: &str,
    source_ref: &super::WorkChangeRef,
    criteria: &[EncodedNewCriterion<'_>],
) -> Result<(), WorkRepositoryError> {
    if criteria.is_empty() {
        return Ok(());
    }

    let mut identities =
        QueryBuilder::<MySql>::new("INSERT INTO work_criteria (owner_id, work_id, criterion_id) ");
    identities.push_values(criteria, |mut row, encoded| {
        row.push_bind(owner_id.as_str())
            .push_bind(work_id.as_str())
            .push_bind(encoded.criterion.criterion_id.as_str());
    });
    identities
        .build()
        .execute(&mut **transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::insert(
                "insert criterion identities",
                WorkConflictResource::CriterionIdentity,
                source,
            )
        })?;

    let mut revisions = QueryBuilder::<MySql>::new(
        "INSERT INTO work_criterion_revisions
         (owner_id, work_id, criterion_id, revision, criterion_kind,
          definition_json, definition_hash, source_kind, source_ref) ",
    );
    revisions.push_values(criteria, |mut row, encoded| {
        row.push_bind(owner_id.as_str())
            .push_bind(work_id.as_str())
            .push_bind(encoded.criterion.criterion_id.as_str())
            .push_bind(super::CriterionRevision::INITIAL.get())
            .push_bind(encoded.criterion.definition.kind().as_str())
            .push_bind(&encoded.definition_json)
            .push_bind(&encoded.definition_hash)
            .push_bind(source_kind)
            .push_bind(source_ref.as_str());
    });
    revisions
        .build()
        .execute(&mut **transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::insert(
                "insert criterion revisions",
                WorkConflictResource::CriterionRevision,
                source,
            )
        })?;
    Ok(())
}

fn genesis_manifests(
    root_item: &super::NewWorkItem,
    criterion_members: Vec<CriterionRevisionRef>,
) -> Result<GenesisManifests, WorkRepositoryError> {
    let criterion_manifest = canonical_json(
        "criterion-set manifest",
        &CriterionSetManifestV1 {
            schema_version: GENESIS_MANIFEST_SCHEMA_VERSION,
            members: criterion_members,
        },
    )?;
    let item_revisions = [super::WorkItemRevisionRef {
        item_id: root_item.item_id.clone(),
        revision: super::WorkItemRevision::INITIAL,
    }];
    let edges: [super::WorkItemEdge; 0] = [];
    let item_manifest = canonical_json("WorkItem revision manifest", &item_revisions)?;
    let edge_manifest = canonical_json("WorkItem edge manifest", &edges)?;
    let graph_manifest = canonical_json(
        "graph manifest",
        &GenesisGraphManifestV1 {
            schema_version: GENESIS_MANIFEST_SCHEMA_VERSION,
            item_revisions: &item_revisions,
            edges: &edges,
        },
    )?;
    Ok(GenesisManifests {
        criterion_hash: content_hash(&criterion_manifest),
        criterion_manifest,
        graph_hash: content_hash(&graph_manifest),
        item_manifest,
        edge_manifest,
    })
}

async fn insert_genesis_rows(
    transaction: &mut Transaction<'_, MySql>,
    genesis: &WorkGenesis,
    create_session: bool,
) -> Result<(), WorkRepositoryError> {
    let owner_id = genesis.owner_id.as_str();
    let work_id = genesis.work_id.as_str();
    let branch_id = genesis.branch_id.as_str();
    let session_id = genesis.session_id.as_str();
    let project_id = genesis.project_id.as_ref().map(ProjectId::as_str);
    let criterion_members = genesis
        .criteria
        .iter()
        .cloned()
        .map(CriterionSetMemberChange::New)
        .collect::<Vec<_>>();
    let criterion_refs =
        super::criteria::canonical_member_refs(&criterion_members).map_err(invalid_mutation)?;
    let member_count =
        i64::try_from(criterion_refs.len()).expect("bounded criterion count fits i64");
    let encoded_criteria = encode_new_criteria(&criterion_members)?;
    let manifests = genesis_manifests(&genesis.root_item, criterion_refs)?;
    let genesis_source = super::WorkChangeRef::parse(genesis.original_intent_ref.as_str())
        .map_err(invalid_mutation)?;

    query(
        "INSERT INTO works
         (owner_id, work_id, work_revision, project_id, original_intent_ref,
          current_goal_revision, current_criteria_set_revision, delivery_branch_id)
         VALUES (?, ?, 1, ?, ?, 1, 1, ?)",
    )
    .bind(owner_id)
    .bind(work_id)
    .bind(project_id)
    .bind(genesis.original_intent_ref.as_str())
    .bind(branch_id)
    .execute(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::insert("insert Work", WorkConflictResource::WorkIdentity, source)
    })?;

    insert_new_criteria(
        transaction,
        &genesis.owner_id,
        &genesis.work_id,
        "user_accepted",
        &genesis_source,
        &encoded_criteria,
    )
    .await?;

    if create_session {
        query(
            "INSERT INTO agent_sessions
             (session_id, user_id, agent_id, title, status, event_count, metadata,
              project_id, created_at, updated_at, last_active_at)
             VALUES (?, ?, NULL, NULL, 'active', 0, NULL, ?, NOW(6), NOW(6), NOW(6))",
        )
        .bind(session_id)
        .bind(owner_id)
        .bind(project_id)
        .execute(&mut **transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::insert(
                "insert internal session",
                WorkConflictResource::InternalSessionIdentity,
                source,
            )
        })?;
    }

    query(
        "INSERT INTO work_goal_revisions
         (owner_id, work_id, revision, goal_text, source_kind, source_ref,
          accepted_by_kind, accepted_by_id, reason)
         VALUES (?, ?, 1, ?, 'user_intent', ?, 'user', ?, NULL)",
    )
    .bind(owner_id)
    .bind(work_id)
    .bind(genesis.goal.as_str())
    .bind(genesis.original_intent_ref.as_str())
    .bind(owner_id)
    .execute(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::insert(
            "insert initial Goal revision",
            WorkConflictResource::GoalRevision,
            source,
        )
    })?;

    query(
        "INSERT INTO work_criterion_sets
         (owner_id, work_id, revision, parent_revision, member_manifest_json,
          member_manifest_hash, member_count, accepted_by_kind, accepted_by_id, reason)
         VALUES (?, ?, 1, NULL, ?, ?, ?, 'user', ?, 'work_genesis')",
    )
    .bind(owner_id)
    .bind(work_id)
    .bind(manifests.criterion_manifest)
    .bind(manifests.criterion_hash)
    .bind(member_count)
    .bind(owner_id)
    .execute(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::insert(
            "insert initial criterion set",
            WorkConflictResource::CriterionSetRevision,
            source,
        )
    })?;

    query(
        "INSERT INTO work_items (owner_id, work_id, item_id, last_revision)
         VALUES (?, ?, ?, 1)",
    )
    .bind(owner_id)
    .bind(work_id)
    .bind(genesis.root_item.item_id.as_str())
    .execute(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::insert(
            "insert root WorkItem identity",
            WorkConflictResource::WorkItemIdentity,
            source,
        )
    })?;

    query(
        "INSERT INTO work_item_revisions
         (owner_id, work_id, item_id, revision, parent_revision, item_kind, objective,
          expected_result, declaration_state, source_ref)
         VALUES (?, ?, ?, 1, NULL, ?, ?, ?, 'active', ?)",
    )
    .bind(owner_id)
    .bind(work_id)
    .bind(genesis.root_item.item_id.as_str())
    .bind(genesis.root_item.kind.as_str())
    .bind(genesis.root_item.objective.as_str())
    .bind(genesis.root_item.expected_result.as_str())
    .bind(genesis.original_intent_ref.as_str())
    .execute(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::insert(
            "insert root WorkItem revision",
            WorkConflictResource::WorkItemRevision,
            source,
        )
    })?;

    query(
        "INSERT INTO work_graph_revisions
         (owner_id, work_id, revision, parent_revision, item_revision_manifest_json,
          edge_manifest_json, manifest_hash, item_count, edge_count, patch_ref, patch_hash,
          actor_kind, actor_id, reason)
         VALUES (?, ?, 1, NULL, ?, ?, ?, 1, 0, NULL, NULL, 'system', 'astra', 'work_genesis')",
    )
    .bind(owner_id)
    .bind(work_id)
    .bind(manifests.item_manifest)
    .bind(manifests.edge_manifest)
    .bind(manifests.graph_hash)
    .execute(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::insert(
            "insert initial graph revision",
            WorkConflictResource::GraphRevision,
            source,
        )
    })?;

    query(
        "INSERT INTO work_graph_sequences (owner_id, work_id, last_revision)
         VALUES (?, ?, 1)",
    )
    .bind(owner_id)
    .bind(work_id)
    .execute(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::insert(
            "insert graph revision sequence",
            WorkConflictResource::GraphRevision,
            source,
        )
    })?;

    query(
        "INSERT INTO work_branches
         (owner_id, work_id, branch_id, branch_revision, session_id,
          origin_branch_id, fork_cursor, goal_revision_ref, criteria_set_revision_ref,
          basis_graph_revision, current_graph_revision)
         VALUES (?, ?, ?, 1, ?, NULL, NULL, 1, 1, 1, 1)",
    )
    .bind(owner_id)
    .bind(work_id)
    .bind(branch_id)
    .bind(session_id)
    .execute(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::insert(
            "bind initial Work branch",
            WorkConflictResource::BranchSessionBinding,
            source,
        )
    })?;

    query(
        "INSERT INTO work_proposal_sequences
         (owner_id, work_id, branch_id, last_proposal_seq)
         VALUES (?, ?, ?, 0)",
    )
    .bind(owner_id)
    .bind(work_id)
    .bind(branch_id)
    .execute(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::insert(
            "insert plan proposal sequence",
            WorkConflictResource::WorkProposalIdentity,
            source,
        )
    })?;

    super::events_repository::insert_genesis_event(
        transaction,
        &super::events::NewWorkEvent {
            owner_id: genesis.owner_id.clone(),
            work_id: genesis.work_id.clone(),
            branch_id: Some(genesis.branch_id.clone()),
            kind: super::WorkEventKind::WorkCreated,
            work_revision: Some(WorkRevision::INITIAL),
            goal_revision: Some(GoalRevision::INITIAL),
            criterion_set_revision: Some(CriterionSetRevision::INITIAL),
            branch_revision: Some(WorkBranchRevision::INITIAL),
            graph_revision: Some(GraphRevision::INITIAL),
            source_ref: genesis_source,
        },
    )
    .await?;
    super::attention_repository::insert_genesis_receipt(
        transaction,
        &genesis.owner_id,
        &genesis.work_id,
    )
    .await?;
    super::runtime_event_outbox::insert_genesis_slot(
        transaction,
        &genesis.owner_id,
        &genesis.work_id,
    )
    .await?;

    Ok(())
}

const SELECT_WORK_WITH_DELIVERY_BRANCH_SQL: &str = "SELECT
    w.owner_id,
    w.work_id,
    w.work_revision,
    w.project_id,
    w.original_intent_ref,
    w.current_goal_revision,
    w.current_criteria_set_revision,
    w.delivery_branch_id,
    DATE_FORMAT(w.created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS work_created_at,
    DATE_FORMAT(w.archived_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS work_archived_at,
    b.work_id AS branch_work_id,
    b.branch_id,
    b.branch_revision,
    b.session_id,
    b.origin_branch_id,
    b.fork_cursor,
    b.goal_revision_ref,
    b.criteria_set_revision_ref,
    b.basis_graph_revision,
    b.current_graph_revision,
    DATE_FORMAT(b.created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS branch_created_at,
    DATE_FORMAT(b.archived_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS branch_archived_at
    FROM works w
    LEFT JOIN work_branches b
      ON b.owner_id = w.owner_id
     AND b.work_id = w.work_id
     AND b.branch_id = w.delivery_branch_id
    WHERE w.owner_id = ? AND w.work_id = ?
    LIMIT 1";

pub(super) fn decode_timestamp(
    entity: &'static str,
    field: &'static str,
    value: String,
) -> Result<DateTime<Utc>, WorkRepositoryError> {
    DateTime::parse_from_rfc3339(&value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|source| {
            WorkRepositoryError::corrupt(
                entity,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid {field} timestamp: {source}"),
                ),
            )
        })
}

pub(super) fn optional_timestamp(
    entity: &'static str,
    field: &'static str,
    value: Option<String>,
) -> Result<Option<DateTime<Utc>>, WorkRepositoryError> {
    value
        .map(|value| decode_timestamp(entity, field, value))
        .transpose()
}

fn decode_work(row: &sqlx::mysql::MySqlRow) -> Result<WorkRecord, WorkRepositoryError> {
    let entity = "Work";
    let get = |field: &'static str| {
        row.try_get::<String, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))
    };
    let get_i64 = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))
    };
    let project_id = row
        .try_get::<Option<String>, _>("project_id")
        .map_err(|source| WorkRepositoryError::corrupt(entity, source))?
        .map(|value| {
            ProjectId::parse(value).map_err(|source| WorkRepositoryError::corrupt(entity, source))
        })
        .transpose()?;
    let archived_at = row
        .try_get::<Option<String>, _>("work_archived_at")
        .map_err(|source| WorkRepositoryError::corrupt(entity, source))?;

    WorkRecord::from_parts(WorkRecordParts {
        work_id: WorkId::parse(get("work_id")?)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))?,
        owner_id: WorkOwnerId::parse(get("owner_id")?)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))?,
        work_revision: WorkRevision::new(get_i64("work_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))?,
        project_id,
        original_intent_ref: OriginalIntentRef::parse(get("original_intent_ref")?)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))?,
        current_goal_revision: GoalRevision::new(get_i64("current_goal_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))?,
        current_criteria_set_revision: CriterionSetRevision::new(get_i64(
            "current_criteria_set_revision",
        )?)
        .map_err(|source| WorkRepositoryError::corrupt(entity, source))?,
        delivery_branch_id: WorkBranchId::parse(get("delivery_branch_id")?)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))?,
        created_at: decode_timestamp(entity, "created_at", get("work_created_at")?)?,
        archived_at: optional_timestamp(entity, "archived_at", archived_at)?,
    })
    .map_err(|source| WorkRepositoryError::corrupt(entity, source))
}

fn decode_branch(row: &sqlx::mysql::MySqlRow) -> Result<WorkBranchRecord, WorkRepositoryError> {
    let entity = "Work branch";
    let get = |field: &'static str| {
        row.try_get::<String, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))
    };
    let get_optional = |field: &'static str| {
        row.try_get::<Option<String>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))
    };
    let get_i64 = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))
    };
    let archived_at = get_optional("branch_archived_at")?;

    WorkBranchRecord::from_parts(WorkBranchRecordParts {
        work_id: WorkId::parse(get("branch_work_id")?)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))?,
        branch_id: WorkBranchId::parse(get("branch_id")?)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))?,
        branch_revision: WorkBranchRevision::new(get_i64("branch_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))?,
        session_id: InternalSessionId::parse(get("session_id")?)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))?,
        origin_branch_id: get_optional("origin_branch_id")?
            .map(|value| {
                WorkBranchId::parse(value)
                    .map_err(|source| WorkRepositoryError::corrupt(entity, source))
            })
            .transpose()?,
        fork_cursor: get_optional("fork_cursor")?
            .map(|value| {
                super::ForkCursorRef::parse(value)
                    .map_err(|source| WorkRepositoryError::corrupt(entity, source))
            })
            .transpose()?,
        goal_revision_ref: GoalRevision::new(get_i64("goal_revision_ref")?)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))?,
        criteria_set_revision_ref: CriterionSetRevision::new(get_i64("criteria_set_revision_ref")?)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))?,
        basis_graph_revision: GraphRevision::new(get_i64("basis_graph_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))?,
        current_graph_revision: GraphRevision::new(get_i64("current_graph_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))?,
        created_at: decode_timestamp(entity, "created_at", get("branch_created_at")?)?,
        archived_at: optional_timestamp(entity, "archived_at", archived_at)?,
    })
    .map_err(|source| WorkRepositoryError::corrupt(entity, source))
}

struct CurrentWorkRevisions {
    work: WorkRevision,
    goal: GoalRevision,
    criteria_set: CriterionSetRevision,
    archived: bool,
}

async fn load_current_work_revisions(
    transaction: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    operation: &'static str,
) -> Result<CurrentWorkRevisions, WorkRepositoryError> {
    let row = query(
        "SELECT work_revision, current_goal_revision, current_criteria_set_revision,
                CASE WHEN archived_at IS NULL THEN 0 ELSE 1 END AS is_archived
         FROM works WHERE owner_id = ? AND work_id = ? LIMIT 1",
    )
    .bind(owner_id.as_str())
    .bind(work_id.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence(operation, source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let read_revision = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work", source))
    };
    Ok(CurrentWorkRevisions {
        work: WorkRevision::new(read_revision("work_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work", source))?,
        goal: GoalRevision::new(read_revision("current_goal_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work", source))?,
        criteria_set: CriterionSetRevision::new(read_revision("current_criteria_set_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work", source))?,
        archived: row
            .try_get::<i64, _>("is_archived")
            .map_err(|source| WorkRepositoryError::corrupt("Work", source))?
            != 0,
    })
}

async fn load_with_transaction(
    transaction: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
) -> Result<CreatedWork, WorkRepositoryError> {
    let row = query(SELECT_WORK_WITH_DELIVERY_BRANCH_SQL)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(|source| WorkRepositoryError::persistence("load created Work", source))?
        .ok_or(WorkRepositoryError::NotFound)?;
    let work = decode_work(&row)?;
    let branch = decode_branch(&row)?;
    super::validate_delivery_branch_binding(&work, &branch)
        .map_err(|source| WorkRepositoryError::corrupt("Work delivery binding", source))?;
    Ok(CreatedWork {
        work,
        delivery_branch: branch,
    })
}

pub(super) struct PreparedCriteriaChange {
    pub(super) change: WorkCriteriaChange,
    manifest_json: String,
    manifest_hash: String,
    member_count: i64,
    pub(super) next_work_revision: WorkRevision,
    pub(super) next_set_revision: CriterionSetRevision,
}

pub(super) struct CriteriaAcceptanceMetadata<'a> {
    pub(super) definition_source_kind: &'a str,
    pub(super) definition_source_ref: &'a WorkChangeRef,
    pub(super) accepted_by_kind: &'a str,
    pub(super) accepted_by_id: &'a str,
    pub(super) event_source_ref: &'a WorkChangeRef,
    pub(super) reason: Option<&'a WorkChangeReason>,
}

pub(super) fn prepare_criteria_change(
    change: WorkCriteriaChange,
) -> Result<PreparedCriteriaChange, WorkRepositoryError> {
    let member_refs =
        super::criteria::canonical_member_refs(&change.members).map_err(invalid_mutation)?;
    let member_count = i64::try_from(member_refs.len()).expect("bounded criterion count fits i64");
    let manifest_json = canonical_json(
        "criterion-set manifest",
        &CriterionSetManifestV1 {
            schema_version: GENESIS_MANIFEST_SCHEMA_VERSION,
            members: member_refs,
        },
    )?;
    let manifest_hash = content_hash(&manifest_json);
    let next_work_revision = change
        .expected_work_revision
        .checked_next()
        .map_err(invalid_mutation)?;
    let next_set_revision = change
        .expected_criteria_set_revision
        .checked_next()
        .map_err(invalid_mutation)?;
    Ok(PreparedCriteriaChange {
        change,
        manifest_json,
        manifest_hash,
        member_count,
        next_work_revision,
        next_set_revision,
    })
}

pub(super) async fn apply_prepared_criteria_change(
    transaction: &mut Transaction<'_, MySql>,
    prepared: &PreparedCriteriaChange,
    metadata: &CriteriaAcceptanceMetadata<'_>,
) -> Result<CreatedWork, WorkRepositoryError> {
    let change = &prepared.change;
    let update = query(
        "UPDATE works
         SET work_revision = ?, current_criteria_set_revision = ?, updated_at = NOW(6)
         WHERE owner_id = ? AND work_id = ?
           AND work_revision = ? AND current_criteria_set_revision = ?
           AND archived_at IS NULL",
    )
    .bind(prepared.next_work_revision.get())
    .bind(prepared.next_set_revision.get())
    .bind(change.owner_id.as_str())
    .bind(change.work_id.as_str())
    .bind(change.expected_work_revision.get())
    .bind(change.expected_criteria_set_revision.get())
    .execute(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::persistence("advance criterion-set revision CAS", source)
    })?;

    match update.rows_affected() {
        1 => {}
        0 => {
            let current = load_current_work_revisions(
                transaction,
                &change.owner_id,
                &change.work_id,
                "classify criterion-set CAS miss",
            )
            .await?;
            if current.archived {
                return Err(WorkRepositoryError::Archived);
            }
            return Err(WorkRepositoryError::StaleCriteriaRevision {
                expected_work_revision: change.expected_work_revision,
                actual_work_revision: current.work,
                expected_criteria_set_revision: change.expected_criteria_set_revision,
                actual_criteria_set_revision: current.criteria_set,
            });
        }
        affected => {
            return Err(WorkRepositoryError::corrupt(
                "Work criterion-set CAS",
                std::io::Error::other(format!("owner-scoped CAS updated {affected} Work rows")),
            ));
        }
    }

    verify_existing_criterion_revisions(
        transaction,
        &change.owner_id,
        &change.work_id,
        &change.members,
    )
    .await?;
    let encoded_new = encode_new_criteria(&change.members)?;
    insert_new_criteria(
        transaction,
        &change.owner_id,
        &change.work_id,
        metadata.definition_source_kind,
        metadata.definition_source_ref,
        &encoded_new,
    )
    .await?;

    query(
        "INSERT INTO work_criterion_sets
         (owner_id, work_id, revision, parent_revision, member_manifest_json,
          member_manifest_hash, member_count, accepted_by_kind, accepted_by_id, reason)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(change.owner_id.as_str())
    .bind(change.work_id.as_str())
    .bind(prepared.next_set_revision.get())
    .bind(change.expected_criteria_set_revision.get())
    .bind(&prepared.manifest_json)
    .bind(&prepared.manifest_hash)
    .bind(prepared.member_count)
    .bind(metadata.accepted_by_kind)
    .bind(metadata.accepted_by_id)
    .bind(metadata.reason.map(WorkChangeReason::as_str))
    .execute(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::insert(
            "insert criterion-set revision",
            WorkConflictResource::CriterionSetRevision,
            source,
        )
    })?;

    super::events_repository::append_event(
        transaction,
        &super::events::NewWorkEvent {
            owner_id: change.owner_id.clone(),
            work_id: change.work_id.clone(),
            branch_id: None,
            kind: super::WorkEventKind::CriteriaAccepted,
            work_revision: Some(prepared.next_work_revision),
            goal_revision: None,
            criterion_set_revision: Some(prepared.next_set_revision),
            branch_revision: None,
            graph_revision: None,
            source_ref: metadata.event_source_ref.clone(),
        },
    )
    .await?;

    load_with_transaction(transaction, &change.owner_id, &change.work_id).await
}

async fn lock_bindable_existing_session(
    transaction: &mut Transaction<'_, MySql>,
    genesis: &mut WorkGenesis,
    expected_active_run_id: Option<&str>,
) -> Result<(), WorkRepositoryError> {
    let session = query(
        "SELECT status, project_id
         FROM agent_sessions
         WHERE user_id = ? AND session_id = ?
         FOR UPDATE",
    )
    .bind(genesis.owner_id.as_str())
    .bind(genesis.session_id.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::persistence("lock existing session for Work genesis", source)
    })?
    .ok_or(WorkRepositoryError::SessionNotBindable)?;
    let status: String = session
        .try_get("status")
        .map_err(|source| WorkRepositoryError::corrupt("existing Work session status", source))?;
    if status != "active" {
        return Err(WorkRepositoryError::SessionNotBindable);
    }

    let active_run: Option<String> = query(
        "SELECT run_id
         FROM agent_session_execution_slots
         WHERE user_id = ? AND session_id = ?
         LIMIT 1
         FOR UPDATE",
    )
    .bind(genesis.owner_id.as_str())
    .bind(genesis.session_id.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::persistence("check existing session execution slot", source)
    })?
    .map(|row| row.try_get("run_id"))
    .transpose()
    .map_err(|source| WorkRepositoryError::corrupt("existing session execution slot", source))?;
    match (active_run.as_deref(), expected_active_run_id) {
        (None, None) => {}
        (Some(actual), Some(expected)) if actual == expected => {}
        _ => return Err(WorkRepositoryError::SessionBusy),
    }

    let session_project = session
        .try_get::<Option<String>, _>("project_id")
        .map_err(|source| WorkRepositoryError::corrupt("existing session project", source))?
        .map(ProjectId::parse)
        .transpose()
        .map_err(|source| WorkRepositoryError::corrupt("existing session project", source))?;
    match (&genesis.project_id, &session_project) {
        (Some(proposed), Some(actual)) if proposed != actual => {
            return Err(WorkRepositoryError::SessionNotBindable);
        }
        (Some(_), None) => return Err(WorkRepositoryError::SessionNotBindable),
        (None, project) => genesis.project_id = project.clone(),
        _ => {}
    }
    Ok(())
}

/// Bind the exact slot-owning coordinator run to the Work it just created.
///
/// The run is deliberately not assigned to the root WorkItem: establishing
/// and coordinating a plan is not evidence that the user's root outcome was
/// executed. Concrete item attempts acquire their own revision-pinned binding
/// when the coordinator starts them.
async fn bind_running_session_run(
    transaction: &mut Transaction<'_, MySql>,
    genesis: &WorkGenesis,
    run_id: &str,
) -> Result<(), WorkRepositoryError> {
    let result = query(
        "UPDATE agent_runs
         SET work_id = ?, work_branch_id = ?, work_graph_revision = ?, updated_at = NOW(6)
         WHERE user_id = ? AND session_id = ? AND run_id = ?
           AND status = 'running'
           AND work_id IS NULL AND work_branch_id IS NULL AND work_graph_revision IS NULL
           AND work_item_id IS NULL AND work_item_revision IS NULL
           AND work_item_attempt_id IS NULL",
    )
    .bind(genesis.work_id.as_str())
    .bind(genesis.branch_id.as_str())
    .bind(GraphRevision::INITIAL.get())
    .bind(genesis.owner_id.as_str())
    .bind(genesis.session_id.as_str())
    .bind(run_id)
    .execute(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::persistence("bind running session run to Work genesis", source)
    })?;
    if result.rows_affected() != 1 {
        // The execution slot establishes which run has authority. Requiring
        // its exact unbound durable row prevents a Work that subsequent child
        // runs cannot inherit and prevents rebinding an existing run lineage.
        return Err(WorkRepositoryError::SessionBusy);
    }
    Ok(())
}

#[async_trait]
impl WorkRepository for DatabaseWorkRepository {
    async fn create_genesis(
        &self,
        genesis: WorkGenesis,
    ) -> Result<CreatedWork, WorkRepositoryError> {
        let mut transaction = self.pool.get().begin().await.map_err(|source| {
            WorkRepositoryError::persistence("begin genesis transaction", source)
        })?;
        if let Err(error) = insert_genesis_rows(&mut transaction, &genesis, true).await {
            return Err(
                rollback_transaction(transaction, "rollback genesis transaction", error).await,
            );
        }
        let created =
            load_with_transaction(&mut transaction, &genesis.owner_id, &genesis.work_id).await?;
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit genesis transaction", source)
        })?;
        Ok(created)
    }

    async fn create_genesis_in_existing_session(
        &self,
        mut genesis: WorkGenesis,
    ) -> Result<CreatedWork, WorkRepositoryError> {
        let mut transaction = self.pool.get().begin().await.map_err(|source| {
            WorkRepositoryError::persistence("begin existing-session genesis transaction", source)
        })?;
        if let Err(error) =
            lock_bindable_existing_session(&mut transaction, &mut genesis, None).await
        {
            return Err(rollback_transaction(
                transaction,
                "rollback existing-session genesis transaction",
                error,
            )
            .await);
        }
        if let Err(error) = insert_genesis_rows(&mut transaction, &genesis, false).await {
            return Err(rollback_transaction(
                transaction,
                "rollback existing-session genesis transaction",
                error,
            )
            .await);
        }
        let created =
            load_with_transaction(&mut transaction, &genesis.owner_id, &genesis.work_id).await?;
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit existing-session genesis transaction", source)
        })?;
        Ok(created)
    }

    async fn create_genesis_in_running_session(
        &self,
        mut genesis: WorkGenesis,
        run_id: &str,
    ) -> Result<CreatedWork, WorkRepositoryError> {
        if run_id.trim().is_empty() {
            return Err(WorkRepositoryError::SessionBusy);
        }
        let mut transaction = self.pool.get().begin().await.map_err(|source| {
            WorkRepositoryError::persistence("begin running-session genesis transaction", source)
        })?;
        if let Err(error) =
            lock_bindable_existing_session(&mut transaction, &mut genesis, Some(run_id)).await
        {
            return Err(rollback_transaction(
                transaction,
                "rollback running-session genesis transaction",
                error,
            )
            .await);
        }
        if let Err(error) = insert_genesis_rows(&mut transaction, &genesis, false).await {
            return Err(rollback_transaction(
                transaction,
                "rollback running-session genesis transaction",
                error,
            )
            .await);
        }
        if let Err(error) = bind_running_session_run(&mut transaction, &genesis, run_id).await {
            return Err(rollback_transaction(
                transaction,
                "rollback running-session run binding transaction",
                error,
            )
            .await);
        }
        let created =
            load_with_transaction(&mut transaction, &genesis.owner_id, &genesis.work_id).await?;
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit running-session genesis transaction", source)
        })?;
        Ok(created)
    }

    async fn load(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
    ) -> Result<CreatedWork, WorkRepositoryError> {
        let row = query(SELECT_WORK_WITH_DELIVERY_BRANCH_SQL)
            .bind(owner_id.as_str())
            .bind(work_id.as_str())
            .fetch_optional(self.pool.get())
            .await
            .map_err(|source| WorkRepositoryError::persistence("load Work", source))?
            .ok_or(WorkRepositoryError::NotFound)?;
        let work = decode_work(&row)?;
        let branch = decode_branch(&row)?;
        super::validate_delivery_branch_binding(&work, &branch)
            .map_err(|source| WorkRepositoryError::corrupt("Work delivery binding", source))?;
        Ok(CreatedWork {
            work,
            delivery_branch: branch,
        })
    }

    async fn list_catalog(
        &self,
        query: super::WorkCatalogQuery,
    ) -> Result<super::WorkCatalogPage, WorkRepositoryError> {
        super::catalog_repository::list_catalog(self, query).await
    }

    async fn revise_goal(
        &self,
        change: WorkGoalChange,
    ) -> Result<CreatedWork, WorkRepositoryError> {
        let next_work_revision = change
            .expected_work_revision
            .checked_next()
            .map_err(invalid_mutation)?;
        let next_goal_revision = change
            .expected_goal_revision
            .checked_next()
            .map_err(invalid_mutation)?;
        let mut transaction = self.pool.get().begin().await.map_err(|source| {
            WorkRepositoryError::persistence("begin Goal revision transaction", source)
        })?;

        let update = query(
            "UPDATE works
             SET work_revision = ?, current_goal_revision = ?, updated_at = NOW(6)
             WHERE owner_id = ? AND work_id = ?
               AND work_revision = ? AND current_goal_revision = ?
               AND archived_at IS NULL",
        )
        .bind(next_work_revision.get())
        .bind(next_goal_revision.get())
        .bind(change.owner_id.as_str())
        .bind(change.work_id.as_str())
        .bind(change.expected_work_revision.get())
        .bind(change.expected_goal_revision.get())
        .execute(&mut *transaction)
        .await
        .map_err(|source| WorkRepositoryError::persistence("advance Goal revision CAS", source))?;

        match update.rows_affected() {
            1 => {}
            0 => {
                let current = load_current_work_revisions(
                    &mut transaction,
                    &change.owner_id,
                    &change.work_id,
                    "classify Goal revision CAS miss",
                )
                .await?;
                if current.archived {
                    return Err(WorkRepositoryError::Archived);
                }
                return Err(WorkRepositoryError::StaleGoalRevision {
                    expected_work_revision: change.expected_work_revision,
                    actual_work_revision: current.work,
                    expected_goal_revision: change.expected_goal_revision,
                    actual_goal_revision: current.goal,
                });
            }
            affected => {
                return Err(WorkRepositoryError::corrupt(
                    "Work Goal CAS",
                    std::io::Error::other(format!("owner-scoped CAS updated {affected} Work rows")),
                ));
            }
        }

        query(
            "INSERT INTO work_goal_revisions
             (owner_id, work_id, revision, goal_text, source_kind, source_ref,
              accepted_by_kind, accepted_by_id, reason)
             VALUES (?, ?, ?, ?, 'user_edit', ?, 'user', ?, ?)",
        )
        .bind(change.owner_id.as_str())
        .bind(change.work_id.as_str())
        .bind(next_goal_revision.get())
        .bind(change.goal.as_str())
        .bind(change.source_ref.as_str())
        .bind(change.owner_id.as_str())
        .bind(change.reason.as_ref().map(WorkChangeReason::as_str))
        .execute(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::insert(
                "insert Goal revision",
                WorkConflictResource::GoalRevision,
                source,
            )
        })?;

        let event_result = super::events_repository::append_event(
            &mut transaction,
            &super::events::NewWorkEvent {
                owner_id: change.owner_id.clone(),
                work_id: change.work_id.clone(),
                branch_id: None,
                kind: super::WorkEventKind::GoalRevised,
                work_revision: Some(next_work_revision),
                goal_revision: Some(next_goal_revision),
                criterion_set_revision: None,
                branch_revision: None,
                graph_revision: None,
                source_ref: change.source_ref.clone(),
            },
        )
        .await;
        if let Err(error) = event_result {
            return Err(rollback_transaction(
                transaction,
                "rollback Goal event transaction",
                error,
            )
            .await);
        }

        let updated =
            load_with_transaction(&mut transaction, &change.owner_id, &change.work_id).await?;
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit Goal revision transaction", source)
        })?;
        Ok(updated)
    }

    async fn accept_criteria(
        &self,
        change: WorkCriteriaChange,
    ) -> Result<CreatedWork, WorkRepositoryError> {
        let prepared = prepare_criteria_change(change)?;
        let mut transaction = self.pool.get().begin().await.map_err(|source| {
            WorkRepositoryError::persistence("begin criterion-set transaction", source)
        })?;
        let metadata = CriteriaAcceptanceMetadata {
            definition_source_kind: "user_accepted",
            definition_source_ref: &prepared.change.source_ref,
            accepted_by_kind: "user",
            accepted_by_id: prepared.change.owner_id.as_str(),
            event_source_ref: &prepared.change.source_ref,
            reason: prepared.change.reason.as_ref(),
        };
        let updated =
            match apply_prepared_criteria_change(&mut transaction, &prepared, &metadata).await {
                Ok(updated) => updated,
                Err(error) => {
                    return Err(rollback_transaction(
                        transaction,
                        "rollback criterion-set transaction",
                        error,
                    )
                    .await);
                }
            };
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit criterion-set transaction", source)
        })?;
        Ok(updated)
    }

    async fn adopt_branch_basis(
        &self,
        change: WorkBranchBasisChange,
    ) -> Result<WorkBranchRecord, WorkRepositoryError> {
        super::basis_repository::adopt_branch_basis(self, change).await
    }

    async fn replace_graph(
        &self,
        change: WorkGraphChange,
    ) -> Result<WorkBranchRecord, WorkRepositoryError> {
        super::graph_repository::replace_graph(self, change).await
    }

    async fn set_branch_subject(
        &self,
        change: WorkBranchSubjectChange,
    ) -> Result<WorkBranchSubject, WorkRepositoryError> {
        super::subject_repository::set_branch_subject(self, change).await
    }

    async fn invalidate_branch_subject(
        &self,
        invalidation: super::WorkBranchSubjectInvalidation,
    ) -> Result<WorkBranchRevision, WorkRepositoryError> {
        super::subject_repository::invalidate_branch_subject(self, invalidation).await
    }

    async fn load_branch_subject(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
    ) -> Result<Option<WorkBranchSubject>, WorkRepositoryError> {
        super::subject_repository::load_branch_subject(self, owner_id, work_id, branch_id).await
    }

    async fn record_patch_artifact(
        &self,
        artifact: super::NewWorkPatchArtifact,
    ) -> Result<WorkPatchArtifact, WorkRepositoryError> {
        super::patch_artifact_repository::record_patch_artifact(self, artifact).await
    }

    async fn load_patch_artifact(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        patch_artifact_id: &WorkPatchArtifactId,
    ) -> Result<Option<WorkPatchArtifact>, WorkRepositoryError> {
        super::patch_artifact_repository::load_patch_artifact(
            self,
            owner_id,
            work_id,
            patch_artifact_id,
        )
        .await
    }

    async fn load_patch_artifact_content(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        patch_artifact_id: &WorkPatchArtifactId,
    ) -> Result<Option<super::WorkPatchArtifactContent>, WorkRepositoryError> {
        super::patch_artifact_repository::load_patch_artifact_content(
            self,
            owner_id,
            work_id,
            branch_id,
            patch_artifact_id,
        )
        .await
    }

    async fn list_patch_artifacts(
        &self,
        query: super::WorkPatchArtifactQuery,
    ) -> Result<super::WorkPatchArtifactPage, WorkRepositoryError> {
        super::patch_artifact_repository::list_patch_artifacts(self, query).await
    }

    async fn propose_plan(
        &self,
        proposal: NewWorkPlanProposal,
    ) -> Result<RecordedWorkPlanProposal, WorkRepositoryError> {
        super::proposal_repository::propose_plan(self, proposal).await
    }

    async fn load_plan_proposal(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        proposal_id: &super::WorkProposalId,
    ) -> Result<Option<RecordedWorkPlanProposal>, WorkRepositoryError> {
        super::proposal_repository::load_plan_proposal(self, owner_id, work_id, proposal_id).await
    }

    async fn accept_plan_proposal(
        &self,
        acceptance: WorkPlanProposalAcceptance,
    ) -> Result<RecordedWorkPlanProposal, WorkRepositoryError> {
        super::proposal_acceptance_repository::accept_plan_proposal(self, acceptance).await
    }

    async fn propose_criteria(
        &self,
        proposal: super::NewWorkCriteriaProposal,
    ) -> Result<super::RecordedWorkCriteriaProposal, WorkRepositoryError> {
        super::criteria_proposal_repository::propose_criteria(self, proposal).await
    }

    async fn load_criteria_proposal(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        proposal_id: &super::WorkProposalId,
    ) -> Result<Option<super::RecordedWorkCriteriaProposal>, WorkRepositoryError> {
        super::criteria_proposal_repository::load_criteria_proposal(
            self,
            owner_id,
            work_id,
            proposal_id,
        )
        .await
    }

    async fn list_pending_criteria_proposals(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
    ) -> Result<Vec<super::RecordedWorkCriteriaProposal>, WorkRepositoryError> {
        super::criteria_proposal_repository::list_pending_criteria_proposals(
            self, owner_id, work_id, branch_id,
        )
        .await
    }

    async fn accept_criteria_proposal(
        &self,
        acceptance: super::WorkCriteriaProposalAcceptance,
    ) -> Result<super::RecordedWorkCriteriaProposal, WorkRepositoryError> {
        super::criteria_proposal_acceptance_repository::accept_criteria_proposal(self, acceptance)
            .await
    }

    async fn reject_criteria_proposal(
        &self,
        rejection: super::WorkCriteriaProposalRejection,
    ) -> Result<super::RecordedWorkCriteriaProposal, WorkRepositoryError> {
        super::criteria_proposal_acceptance_repository::reject_criteria_proposal(self, rejection)
            .await
    }

    async fn load_plan_context_for_session(
        &self,
        owner_id: &WorkOwnerId,
        session_id: &InternalSessionId,
    ) -> Result<WorkPlanContext, WorkRepositoryError> {
        super::plan_context_repository::load_plan_context_for_session(self, owner_id, session_id)
            .await
    }

    async fn load_task_execution_snapshot_for_session(
        &self,
        owner_id: &WorkOwnerId,
        session_id: &InternalSessionId,
    ) -> Result<WorkTaskExecutionSnapshot, WorkRepositoryError> {
        super::plan_context_repository::load_task_execution_snapshot_for_session(
            self, owner_id, session_id,
        )
        .await
    }

    async fn load_task_graph_page(
        &self,
        query: WorkTaskGraphQuery,
    ) -> Result<WorkTaskGraphPage, WorkRepositoryError> {
        super::plan_context_repository::load_task_graph_page(self, query).await
    }

    async fn load_criteria_page(
        &self,
        query: super::WorkCriteriaQuery,
    ) -> Result<super::WorkCriteriaPage, WorkRepositoryError> {
        super::criteria_read_repository::load_criteria_page(self, query).await
    }

    async fn load_session_item_runtime_binding(
        &self,
        owner_id: &WorkOwnerId,
        session_id: &InternalSessionId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        item: &super::WorkItemRevisionRef,
    ) -> Result<WorkSessionPlanBinding, WorkRepositoryError> {
        super::plan_context_repository::load_session_item_runtime_binding(
            self, owner_id, session_id, work_id, branch_id, item,
        )
        .await
    }

    async fn load_session_plan_binding(
        &self,
        owner_id: &WorkOwnerId,
        session_id: &InternalSessionId,
    ) -> Result<WorkSessionPlanBinding, WorkRepositoryError> {
        super::plan_context_repository::load_session_plan_binding(self, owner_id, session_id).await
    }

    async fn load_branch_runtime_binding(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
    ) -> Result<super::WorkBranchRuntimeBinding, WorkRepositoryError> {
        super::plan_context_repository::load_branch_runtime_binding(
            self, owner_id, work_id, branch_id,
        )
        .await
    }

    async fn observe_declared_work(
        &self,
        observation: WorkObservationQuery,
    ) -> Result<WorkObservationReport, WorkRepositoryError> {
        super::observation_repository::observe_declared_work(self, observation).await
    }

    async fn select_delivery_branch(
        &self,
        selection: super::WorkDeliverySelection,
    ) -> Result<super::WorkDeliverySelectionReceipt, WorkRepositoryError> {
        super::delivery_selection_repository::select_delivery_branch(self, selection).await
    }

    async fn change_branch_retention(
        &self,
        change: super::WorkBranchRetentionChange,
    ) -> Result<super::WorkBranchRetentionReceipt, WorkRepositoryError> {
        super::branch_retention_repository::change_branch_retention(self, change).await
    }

    async fn record_check_run(
        &self,
        check: NewWorkCheckRun,
    ) -> Result<RecordedWorkCheckRun, WorkRepositoryError> {
        super::evidence_repository::record_check_run(self, check).await
    }

    async fn accept_gaps(
        &self,
        decision: NewWorkAcceptanceDecision,
    ) -> Result<RecordedWorkAcceptanceDecision, WorkRepositoryError> {
        super::acceptance_repository::accept_gaps(self, decision).await
    }

    async fn advance_attention_cursor(
        &self,
        advance: WorkAttentionCursorAdvance,
    ) -> Result<WorkAttentionReceipt, WorkRepositoryError> {
        super::attention_repository::advance_cursor(self, advance).await
    }

    async fn list_events(
        &self,
        query: WorkEventQuery,
    ) -> Result<WorkEventPage, WorkRepositoryError> {
        super::event_read_repository::list_events(self, query).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{
        CriterionCommand, CriterionDefinition, CriterionStatement, NewWorkCriterion,
    };

    #[test]
    fn genesis_manifests_are_explicit_versioned_and_content_addressed() {
        let criteria = vec![
            NewWorkCriterion {
                criterion_id: CriterionId::parse("review").expect("criterion id"),
                definition: CriterionDefinition::HumanReview {
                    statement: CriterionStatement::parse("The result is reviewable.")
                        .expect("statement"),
                },
            },
            NewWorkCriterion {
                criterion_id: CriterionId::parse("tests").expect("criterion id"),
                definition: CriterionDefinition::TestCheck {
                    statement: CriterionStatement::parse("Relevant tests pass.")
                        .expect("statement"),
                    command: CriterionCommand::parse("cargo test").expect("command"),
                },
            },
        ];
        let genesis = WorkGenesis::new(WorkGenesisParts {
            owner_id: WorkOwnerId::parse("owner").expect("owner"),
            work_id: WorkId::parse("work").expect("work"),
            branch_id: WorkBranchId::parse("branch").expect("branch"),
            session_id: InternalSessionId::parse("session").expect("session"),
            project_id: None,
            original_intent_ref: OriginalIntentRef::parse("intent").expect("intent"),
            goal: WorkGoal::parse("Ship the canonical Work root.").expect("goal"),
            criteria,
        })
        .expect("genesis");
        let members = genesis
            .criteria
            .iter()
            .map(|criterion| CriterionRevisionRef {
                criterion_id: criterion.criterion_id.clone(),
                revision: CriterionRevision::INITIAL,
            })
            .collect();
        let manifests = genesis_manifests(&genesis.root_item, members).expect("manifests");
        let criterion: serde_json::Value =
            serde_json::from_str(&manifests.criterion_manifest).expect("criterion manifest JSON");
        let items: serde_json::Value =
            serde_json::from_str(&manifests.item_manifest).expect("item manifest JSON");

        assert_eq!(manifests.criterion_hash.len(), 71);
        assert!(manifests.criterion_hash.starts_with("sha256:"));
        assert_eq!(criterion["schema_version"], 1);
        assert_eq!(
            criterion["members"],
            serde_json::json!([
                {"criterion_id": "review", "revision": 1},
                {"criterion_id": "tests", "revision": 1}
            ])
        );
        assert_eq!(manifests.graph_hash.len(), 71);
        assert!(manifests.graph_hash.starts_with("sha256:"));
        assert_eq!(
            items,
            serde_json::json!([{"item_id": "root", "revision": 1}])
        );
        assert_eq!(manifests.edge_manifest, "[]");
        assert_ne!(manifests.criterion_hash, manifests.graph_hash);
    }

    #[test]
    fn genesis_rejects_a_goal_that_cannot_fit_its_canonical_root_item() {
        let result = WorkGenesis::new(WorkGenesisParts {
            owner_id: WorkOwnerId::parse("owner").expect("owner"),
            work_id: WorkId::parse("work").expect("work"),
            branch_id: WorkBranchId::parse("branch").expect("branch"),
            session_id: InternalSessionId::parse("session").expect("session"),
            project_id: None,
            original_intent_ref: OriginalIntentRef::parse("intent").expect("intent"),
            goal: WorkGoal::parse("x".repeat(super::super::WORK_GOAL_MAX_BYTES))
                .expect("valid goal"),
            criteria: Vec::new(),
        });
        assert!(matches!(
            result,
            Err(WorkDomainError::InvalidWorkItemText {
                violation: super::super::WorkItemTextViolation::TooLarge { .. }
            })
        ));
    }
}
