//! Canonical domain boundary for user-visible Work and its internal session binding.
//!
//! Work is the public product identity. A [`WorkBranchRecord`] owns exactly one
//! internal conversation session, but that session identity is deliberately not
//! a public projection. Persistence and HTTP adapters build on these types; they
//! must not recreate these invariants from strings or UI state.

use chrono::{DateTime, Utc};
use serde::Serialize;
use thiserror::Error;

mod acceptance;
mod acceptance_repository;
mod attempt_settlement;
mod attention;
mod attention_repository;
mod basis;
mod basis_repository;
mod branch_catalog;
mod branch_comparison;
mod branch_creation_operation;
mod branch_deletion_operation;
mod branch_retention;
mod branch_retention_repository;
mod catalog;
mod catalog_repository;
mod control_operation;
mod criteria;
mod criteria_proposal;
mod criteria_proposal_acceptance_repository;
mod criteria_proposal_repository;
mod criteria_read_repository;
mod delivery_selection;
mod delivery_selection_repository;
mod event_read_repository;
mod events;
mod events_repository;
mod evidence;
mod evidence_repository;
mod graph;
mod graph_repository;
mod observation;
mod observation_repository;
mod patch_artifact;
mod patch_artifact_repository;
mod patch_commit;
mod patch_materialization;
mod plan_context;
mod plan_context_repository;
mod proposal;
mod proposal_acceptance_repository;
mod proposal_queue;
mod proposal_repository;
mod repository;
mod runtime_event_outbox;
mod subject;
mod subject_repository;

pub use acceptance::{
    AcceptanceDecisionId, AcceptanceDecisionViolation, AcceptanceGapReason, AcceptedCriterionGap,
    NewWorkAcceptanceDecision, RecordedWorkAcceptanceDecision,
};
pub use attempt_settlement::{
    DatabaseWorkAttemptSettlementService, NewWorkAttemptSettlement, NewWorkItemAttempt,
    PrimaryWorkAttemptAdvance, PrimaryWorkAttemptCarrierState, RecordedPrimaryWorkAttemptAdvance,
    RecordedWorkAttemptSettlement, WorkAttemptBlockerKind, WorkAttemptExecutionMode,
    WorkAttemptOutcome, WorkAttemptSettlementError,
};
pub use attention::{
    WorkAttentionCursorAdvance, WorkAttentionCursorKind, WorkAttentionReceipt,
    WorkAttentionReceiptRevision,
};
pub use basis::WorkBranchBasisChange;
pub use branch_catalog::{
    DatabaseWorkBranchCatalogService, WORK_ARCHIVED_BRANCH_PAGE_MAX_ITEMS,
    WORK_BRANCH_CATALOG_SCHEMA_VERSION, WorkArchivedBranchCursor, WorkArchivedBranchEntry,
    WorkArchivedBranchPage, WorkBranchCatalog, WorkBranchCatalogEntry, WorkBranchCatalogError,
    WorkBranchDimension, WorkBranchDimensionDisposition, WorkBranchDimensionSummary,
};
pub use branch_comparison::{
    DatabaseWorkBranchComparisonService, WORK_BRANCH_COMPARISON_SCHEMA_VERSION,
    WorkBranchComparisonBlocker, WorkBranchComparisonCoverageGap, WorkBranchComparisonCriteria,
    WorkBranchComparisonError, WorkBranchComparisonEvidence, WorkBranchComparisonGraph,
    WorkBranchComparisonRelation, WorkBranchComparisonReport, WorkBranchComparisonSide,
    WorkBranchComparisonSubject,
};
pub use branch_creation_operation::{
    DatabaseWorkBranchCreationService, WORK_ACTIVE_BRANCH_MAX,
    WORK_BRANCH_CREATION_OPERATION_SCHEMA_VERSION, WorkBranchCreationAdmission,
    WorkBranchCreationError, WorkBranchCreationOperation, WorkBranchCreationOutcome,
    WorkBranchCreationRequest, WorkBranchCreationState,
};
pub use branch_deletion_operation::{
    DatabaseWorkBranchDeletionService, WORK_BRANCH_DELETION_OPERATION_SCHEMA_VERSION,
    WorkBranchDeletionAdmission, WorkBranchDeletionError, WorkBranchDeletionExecutionClaim,
    WorkBranchDeletionOperation, WorkBranchDeletionOutcome, WorkBranchDeletionPhase,
    WorkBranchDeletionRequest, WorkBranchDeletionState,
};
pub use branch_retention::{
    WORK_BRANCH_RETENTION_SCHEMA_VERSION, WorkBranchRetentionBasisResource,
    WorkBranchRetentionChange, WorkBranchRetentionKind, WorkBranchRetentionOutcome,
    WorkBranchRetentionReceipt,
};
pub use catalog::{
    WORK_CATALOG_PAGE_MAX_ITEMS, WorkBranchActivity, WorkCatalogAttention, WorkCatalogCursor,
    WorkCatalogEntry, WorkCatalogPage, WorkCatalogPageLimit, WorkCatalogQuery,
};
pub use control_operation::{
    DatabaseWorkBranchControlService, WORK_BRANCH_CONTROL_OPERATION_SCHEMA_VERSION,
    WorkBranchControlError, WorkBranchControlKind, WorkBranchControlOperation,
    WorkBranchControlOutcome, WorkBranchControlPhase, WorkBranchControlProgress,
    WorkBranchControlRequest, WorkBranchControlState, WorkBranchForceAdmission,
    WorkBranchForceContext, force_handoff_is_abortable,
};
pub use criteria::{
    CriterionCommand, CriterionDefinition, CriterionId, CriterionKind, CriterionRevision,
    CriterionRevisionRef, CriterionSetMemberChange, CriterionStatement, NewWorkCriterion,
    WORK_CRITERIA_PAGE_MAX_ITEMS, WorkCriteriaBasis, WorkCriteriaChange, WorkCriteriaCursor,
    WorkCriteriaPage, WorkCriteriaQuery, WorkCriterionView,
};
pub use criteria_proposal::{
    NewWorkCriteriaProposal, RecordedWorkCriteriaProposal, WorkCriteriaProposalAcceptance,
    WorkCriteriaProposalMember, WorkCriteriaProposalRejection, WorkCriteriaProposalResolution,
    WorkCriteriaProposalViolation,
};
pub use delivery_selection::{
    WORK_DELIVERY_SELECTION_SCHEMA_VERSION, WorkDeliverySelection,
    WorkDeliverySelectionBasisResource, WorkDeliverySelectionOutcome, WorkDeliverySelectionReceipt,
    WorkDeliverySelectionSubject,
};
pub use events::{
    WORK_EVENT_PAGE_MAX_ITEMS, WorkEventCoverage, WorkEventKind, WorkEventPage, WorkEventPageLimit,
    WorkEventQuery, WorkEventRecord, WorkEventSeq,
};
pub use evidence::{
    CheckCoverage, CheckCoverageGap, CheckErrorKind, CheckEvidenceRef, CheckOutcome, CheckRunId,
    CheckRunViolation, CheckVerifierKind, NewWorkCheckRun, RecordedWorkCheckRun,
};
pub use graph::{
    NewWorkItem, WorkGraphChange, WorkGraphItemChange, WorkItemAttemptId, WorkItemDeclarationState,
    WorkItemEdge, WorkItemEdgeKind, WorkItemId, WorkItemKind, WorkItemRevision,
    WorkItemRevisionChange, WorkItemRevisionRef, WorkItemText,
};
pub use observation::{
    ObservationCoherence, ObservationCoverageGap, ObservationGapReason, ObservationScope,
    ObservationSourceKind, ObservationSourceRevision, RevisionAlignment, WorkBranchOverview,
    WorkContentHash, WorkCriteriaSummary, WorkDeliveryStatus, WorkDeliverySummary,
    WorkGoalOverview, WorkGraphSummary, WorkObservationCauseCode, WorkObservationCursor,
    WorkObservationFactCode, WorkObservationFinding, WorkObservationQuery, WorkObservationReport,
    WorkObservationSatisfactionEvidenceRef, WorkOverview, WorkRetentionState,
};
pub use patch_artifact::{
    NewWorkPatchArtifact, WORK_PATCH_ARTIFACT_MAX_BYTES, WORK_PATCH_ARTIFACT_MAX_LINES,
    WORK_PATCH_ARTIFACT_PAGE_MAX_ITEMS, WORK_PATCH_ARTIFACT_SCHEMA_VERSION, WorkPatchArtifact,
    WorkPatchArtifactBasisResource, WorkPatchArtifactContent, WorkPatchArtifactCursor,
    WorkPatchArtifactId, WorkPatchArtifactPage, WorkPatchArtifactPageLimit, WorkPatchArtifactQuery,
    WorkPatchFormat, WorkProviderInvocationRef, work_patch_line_count,
};
pub use patch_commit::{
    DatabaseWorkPatchCommitService, SERVER_GIT_WORKTREE_COMMIT_PROVIDER_REF,
    WORK_PATCH_COMMIT_PAGE_MAX_ITEMS, WORK_PATCH_COMMIT_SCHEMA_VERSION, WorkPatchCommitCommitted,
    WorkPatchCommitConflict, WorkPatchCommitCursor, WorkPatchCommitError, WorkPatchCommitFailure,
    WorkPatchCommitFailureCode, WorkPatchCommitId, WorkPatchCommitOperation, WorkPatchCommitPage,
    WorkPatchCommitPageLimit, WorkPatchCommitPhase, WorkPatchCommitProviderRef,
    WorkPatchCommitQuery, WorkPatchCommitRecoveryItem, WorkPatchCommitRequest,
    WorkPatchCommitState,
};
pub use patch_materialization::{
    DatabaseWorkPatchMaterializationService, SERVER_GIT_WORKTREE_MATERIALIZATION_PROVIDER_REF,
    WORK_PATCH_MATERIALIZATION_PAGE_MAX_ITEMS, WORK_PATCH_MATERIALIZATION_SCHEMA_VERSION,
    WorkMaterializationProviderRef, WorkPatchMaterializationApplied,
    WorkPatchMaterializationApplyOutcome, WorkPatchMaterializationConflict,
    WorkPatchMaterializationCursor, WorkPatchMaterializationError,
    WorkPatchMaterializationFailureCode, WorkPatchMaterializationId,
    WorkPatchMaterializationNotApplied, WorkPatchMaterializationOperation,
    WorkPatchMaterializationPage, WorkPatchMaterializationPageLimit, WorkPatchMaterializationPhase,
    WorkPatchMaterializationQuery, WorkPatchMaterializationRecoveryItem,
    WorkPatchMaterializationRequest, WorkPatchMaterializationState,
    WorkPatchMaterializationVerificationOutcome,
};
pub use plan_context::{
    WORK_TASK_GRAPH_DEPENDENCY_PAGE_MAX_ITEMS, WORK_TASK_GRAPH_ITEM_PAGE_MAX_ITEMS,
    WorkBranchRuntimeBinding, WorkCheckFreshness, WorkItemCheckFact, WorkItemDelivery,
    WorkItemDeliveryStatus, WorkItemExecution, WorkItemExecutionRunRef, WorkItemExecutionStatus,
    WorkItemVerification, WorkItemVerificationStatus, WorkPlanBasis, WorkPlanContext, WorkPlanItem,
    WorkSessionPlanBinding, WorkTaskExecutionItem, WorkTaskExecutionNext,
    WorkTaskExecutionSnapshot, WorkTaskGraphCursor, WorkTaskGraphDependencyPage, WorkTaskGraphItem,
    WorkTaskGraphItemPage, WorkTaskGraphPage, WorkTaskGraphQuery,
};
pub use proposal::{
    NewWorkPlanProposal, RecordedWorkPlanProposal, WorkPlanProposalAcceptance,
    WorkPlanProposalResolution, WorkPlanProposalViolation, WorkProposalId, WorkProposalKind,
    WorkProposalSourceKind, WorkProposalStatus,
};
pub use repository::{
    CreatedWork, DatabaseWorkRepository, WorkAcceptanceBasisResource, WorkCheckBasisResource,
    WorkConflictResource, WorkGenesis, WorkGenesisParts, WorkGoalChange, WorkProposalBasisResource,
    WorkRepository, WorkRepositoryError,
};
pub use runtime_event_outbox::{
    WorkRuntimeEventProjectionResult, project_pending_runtime_events,
    spawn_work_runtime_event_projector,
};
pub use subject::{
    WorkBranchSubject, WorkBranchSubjectChange, WorkBranchSubjectInvalidation,
    WorkBranchSubjectRevision, WorkSubjectRef,
};

pub(crate) const WORKS_CREATE_SQL: &str = "CREATE TABLE IF NOT EXISTS works (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    work_revision BIGINT NOT NULL,
    project_id VARCHAR(128) NULL,
    original_intent_ref VARCHAR(256) NOT NULL,
    current_goal_revision BIGINT NOT NULL,
    current_criteria_set_revision BIGINT NOT NULL,
    delivery_branch_id VARCHAR(64) NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    archived_at DATETIME(6) NULL,
    PRIMARY KEY (owner_id, work_id),
    UNIQUE KEY uq_works_public_identity (work_id),
    INDEX idx_works_owner_project_updated (owner_id, project_id, updated_at, work_id),
    INDEX idx_works_owner_created (owner_id, created_at, work_id),
    CONSTRAINT chk_works_revision CHECK (work_revision > 0),
    CONSTRAINT chk_works_goal_revision CHECK (current_goal_revision > 0),
    CONSTRAINT chk_works_criteria_revision CHECK (current_criteria_set_revision > 0),
    CONSTRAINT chk_works_archive_order CHECK (archived_at IS NULL OR archived_at >= created_at)
)";

pub(crate) const WORK_GOAL_REVISIONS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_goal_revisions (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    revision BIGINT NOT NULL,
    goal_text LONGTEXT NOT NULL,
    source_kind VARCHAR(32) NOT NULL,
    source_ref VARCHAR(256) NOT NULL,
    accepted_by_kind VARCHAR(32) NOT NULL,
    accepted_by_id VARCHAR(128) NOT NULL,
    reason VARCHAR(512) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, work_id, revision),
    CONSTRAINT chk_work_goal_revision CHECK (revision > 0)
)";

pub(crate) const WORK_CRITERIA_CREATE_SQL: &str = "CREATE TABLE IF NOT EXISTS work_criteria (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    criterion_id VARCHAR(64) NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, work_id, criterion_id)
)";

pub(crate) const WORK_CRITERION_REVISIONS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_criterion_revisions (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    criterion_id VARCHAR(64) NOT NULL,
    revision BIGINT NOT NULL,
    criterion_kind VARCHAR(32) NOT NULL,
    definition_json LONGTEXT NOT NULL,
    definition_hash CHAR(71) NOT NULL,
    source_kind VARCHAR(32) NOT NULL,
    source_ref VARCHAR(256) NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, work_id, criterion_id, revision),
    CONSTRAINT chk_work_criterion_revision CHECK (revision > 0),
    CONSTRAINT chk_work_criterion_kind CHECK (criterion_kind IN (
        'command_check', 'test_check', 'artifact_check', 'state_check',
        'human_review', 'model_assessment'
    ))
)";

pub(crate) const WORK_CRITERION_SETS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_criterion_sets (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    revision BIGINT NOT NULL,
    parent_revision BIGINT NULL,
    member_manifest_json LONGTEXT NOT NULL,
    member_manifest_hash CHAR(71) NOT NULL,
    member_count INT NOT NULL,
    accepted_by_kind VARCHAR(32) NOT NULL,
    accepted_by_id VARCHAR(128) NOT NULL,
    reason VARCHAR(512) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, work_id, revision),
    CONSTRAINT chk_work_criterion_set_revision CHECK (revision > 0),
    CONSTRAINT chk_work_criterion_set_parent CHECK (parent_revision IS NULL OR parent_revision > 0),
    CONSTRAINT chk_work_criterion_set_count CHECK (member_count >= 0 AND member_count <= 128)
)";

pub(crate) const WORK_GRAPH_REVISIONS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_graph_revisions (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    revision BIGINT NOT NULL,
    parent_revision BIGINT NULL,
    item_revision_manifest_json LONGTEXT NOT NULL,
    edge_manifest_json LONGTEXT NOT NULL,
    manifest_hash CHAR(71) NOT NULL,
    item_count INT NOT NULL,
    edge_count INT NOT NULL,
    patch_ref VARCHAR(256) NULL,
    patch_hash CHAR(71) NULL,
    actor_kind VARCHAR(32) NOT NULL,
    actor_id VARCHAR(128) NOT NULL,
    reason VARCHAR(512) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, work_id, revision),
    CONSTRAINT chk_work_graph_revision CHECK (revision > 0),
    CONSTRAINT chk_work_graph_parent CHECK (parent_revision IS NULL OR parent_revision > 0),
    CONSTRAINT chk_work_graph_item_count CHECK (item_count >= 0 AND item_count <= 256),
    CONSTRAINT chk_work_graph_edge_count CHECK (edge_count >= 0 AND edge_count <= 1024),
    CONSTRAINT chk_work_graph_patch CHECK ((patch_ref IS NULL) = (patch_hash IS NULL))
)";

pub(crate) const WORK_GRAPH_SEQUENCES_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_graph_sequences (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    last_revision BIGINT NOT NULL,
    PRIMARY KEY (owner_id, work_id),
    CONSTRAINT chk_work_graph_sequence CHECK (last_revision > 0)
)";

pub(crate) const WORK_ITEMS_CREATE_SQL: &str = "CREATE TABLE IF NOT EXISTS work_items (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    item_id VARCHAR(64) NOT NULL,
    last_revision BIGINT NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, work_id, item_id),
    CONSTRAINT chk_work_item_last_revision CHECK (last_revision > 0)
)";

pub(crate) const WORK_ITEM_REVISIONS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_item_revisions (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    item_id VARCHAR(64) NOT NULL,
    revision BIGINT NOT NULL,
    parent_revision BIGINT NULL,
    item_kind VARCHAR(32) NOT NULL,
    objective LONGTEXT NOT NULL,
    expected_result LONGTEXT NOT NULL,
    declaration_state VARCHAR(32) NOT NULL,
    source_ref VARCHAR(256) NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, work_id, item_id, revision),
    CONSTRAINT chk_work_item_revision CHECK (revision > 0),
    CONSTRAINT chk_work_item_parent_revision CHECK (
        (revision = 1 AND parent_revision IS NULL)
        OR (revision > 1 AND parent_revision > 0 AND parent_revision < revision)
    ),
    CONSTRAINT chk_work_item_kind CHECK (item_kind IN ('milestone', 'task')),
    CONSTRAINT chk_work_item_declaration_state CHECK (
        declaration_state IN ('active', 'superseded', 'cancelled')
    )
)";

pub(crate) const WORK_ITEM_EDGES_CREATE_SQL: &str = "CREATE TABLE IF NOT EXISTS work_item_edges (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    graph_revision BIGINT NOT NULL,
    predecessor_item_id VARCHAR(64) NOT NULL,
    successor_item_id VARCHAR(64) NOT NULL,
    edge_kind VARCHAR(32) NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (
        owner_id, work_id, graph_revision,
        predecessor_item_id, successor_item_id, edge_kind
    ),
    CONSTRAINT chk_work_item_edge_graph_revision CHECK (graph_revision > 0),
    CONSTRAINT chk_work_item_edge_kind CHECK (edge_kind IN ('dependency')),
    CONSTRAINT chk_work_item_edge_not_self CHECK (predecessor_item_id <> successor_item_id)
)";

pub(crate) const WORK_BRANCHES_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_branches (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    branch_id VARCHAR(64) NOT NULL,
    branch_revision BIGINT NOT NULL,
    session_id VARCHAR(64) NOT NULL,
    origin_branch_id VARCHAR(64) NULL,
    fork_cursor VARCHAR(512) NULL,
    goal_revision_ref BIGINT NOT NULL,
    criteria_set_revision_ref BIGINT NOT NULL,
    basis_graph_revision BIGINT NOT NULL,
    current_graph_revision BIGINT NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    archived_at DATETIME(6) NULL,
    deletion_operation_id VARCHAR(64) NULL,
    deletion_requested_at DATETIME(6) NULL,
    PRIMARY KEY (owner_id, work_id, branch_id),
    UNIQUE KEY uq_work_branches_session (owner_id, session_id),
    INDEX idx_work_branches_owner_work_updated (owner_id, work_id, updated_at, branch_id),
    INDEX idx_work_branches_owner_archive (owner_id, work_id, archived_at, branch_id),
    INDEX idx_work_branches_deletion (owner_id, deletion_operation_id, deletion_requested_at),
    CONSTRAINT chk_work_branch_revision CHECK (branch_revision > 0),
    CONSTRAINT chk_work_branch_goal_revision CHECK (goal_revision_ref > 0),
    CONSTRAINT chk_work_branch_criteria_revision CHECK (criteria_set_revision_ref > 0),
    CONSTRAINT chk_work_branch_basis_graph_revision CHECK (basis_graph_revision > 0),
    CONSTRAINT chk_work_branch_current_graph_revision CHECK (current_graph_revision >= basis_graph_revision),
    CONSTRAINT chk_work_branch_fork_lineage CHECK ((origin_branch_id IS NULL) = (fork_cursor IS NULL)),
    CONSTRAINT chk_work_branch_archive_order CHECK (archived_at IS NULL OR archived_at >= created_at),
    CONSTRAINT chk_work_branch_deletion_marker CHECK (
        (deletion_operation_id IS NULL) = (deletion_requested_at IS NULL)
    )
)";

pub(crate) const WORK_BRANCH_DELETION_OPERATIONS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_branch_deletion_operations (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    branch_id VARCHAR(64) NOT NULL,
    operation_id VARCHAR(64) NOT NULL,
    idempotency_hash CHAR(64) NOT NULL,
    request_hash CHAR(64) NOT NULL,
    session_id VARCHAR(64) NOT NULL,
    operation_state VARCHAR(32) NOT NULL,
    operation_phase VARCHAR(32) NOT NULL,
    operation_outcome VARCHAR(32) NOT NULL,
    expected_work_revision BIGINT NOT NULL,
    expected_branch_revision BIGINT NOT NULL,
    observed_work_revision BIGINT NOT NULL,
    observed_branch_revision BIGINT NOT NULL,
    executor_token VARCHAR(64) NULL,
    executor_lease_expires_at DATETIME(6) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    completed_at DATETIME(6) NULL,
    PRIMARY KEY (owner_id, work_id, branch_id, operation_id),
    UNIQUE KEY uq_work_branch_deletion_request (
        owner_id, work_id, branch_id, idempotency_hash
    ),
    INDEX idx_work_branch_deletion_pending (
        operation_state, executor_lease_expires_at, created_at, operation_id
    ),
    INDEX idx_work_branch_deletion_session (owner_id, session_id),
    CONSTRAINT chk_work_branch_deletion_state CHECK (
        operation_state IN ('pending', 'succeeded', 'conflict')
    ),
    CONSTRAINT chk_work_branch_deletion_phase CHECK (
        operation_phase IN ('fence', 'session_cleanup', 'lineage_gc', 'branch_cleanup', 'complete')
    ),
    CONSTRAINT chk_work_branch_deletion_outcome CHECK (
        operation_outcome IN (
            'pending', 'deleted', 'delivery_branch_protected',
            'work_revision_conflict', 'branch_revision_conflict'
        )
    ),
    CONSTRAINT chk_work_branch_deletion_revisions CHECK (
        expected_work_revision > 0 AND expected_branch_revision > 0
        AND observed_work_revision > 0 AND observed_branch_revision > 0
    ),
    CONSTRAINT chk_work_branch_deletion_executor CHECK (
        (executor_token IS NULL AND executor_lease_expires_at IS NULL)
        OR
        (operation_state = 'pending' AND executor_token IS NOT NULL
         AND executor_lease_expires_at IS NOT NULL)
    ),
    CONSTRAINT chk_work_branch_deletion_terminal CHECK (
        (operation_state = 'pending' AND operation_phase <> 'complete'
         AND operation_outcome = 'pending' AND completed_at IS NULL)
        OR
        (operation_state = 'succeeded' AND operation_phase = 'complete'
         AND operation_outcome = 'deleted' AND completed_at IS NOT NULL)
        OR
        (operation_state = 'conflict' AND operation_phase = 'complete'
         AND operation_outcome IN (
             'delivery_branch_protected', 'work_revision_conflict',
             'branch_revision_conflict'
         ) AND completed_at IS NOT NULL)
    )
)";

pub(crate) const WORK_BRANCH_CONTROL_OPERATIONS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_branch_control_operations (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    branch_id VARCHAR(64) NOT NULL,
    operation_id VARCHAR(64) NOT NULL,
    idempotency_hash CHAR(64) NOT NULL,
    request_hash CHAR(64) NOT NULL,
    session_id VARCHAR(64) NOT NULL,
    attachment_id VARCHAR(128) NOT NULL,
    operation_kind VARCHAR(32) NOT NULL,
    operation_state VARCHAR(32) NOT NULL,
    operation_outcome VARCHAR(32) NOT NULL,
    expected_branch_revision BIGINT NOT NULL,
    expected_writer_epoch BIGINT NOT NULL,
    expected_root_hash CHAR(64) NULL,
    observed_branch_revision BIGINT NULL,
    observed_writer_epoch BIGINT NULL,
    observed_root_hash CHAR(64) NULL,
    forced_authorization_id VARCHAR(80) NULL,
    handoff_id VARCHAR(64) NULL,
    executor_token VARCHAR(64) NULL,
    executor_lease_until DATETIME(6) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    completed_at DATETIME(6) NULL,
    PRIMARY KEY (owner_id, work_id, branch_id, operation_id),
    UNIQUE KEY uq_work_branch_control_request (
        owner_id, work_id, branch_id, idempotency_hash
    ),
    INDEX idx_work_branch_control_session (owner_id, session_id),
    INDEX idx_work_branch_control_created (owner_id, work_id, branch_id, created_at, operation_id),
    INDEX idx_work_branch_control_completed (completed_at, operation_id),
    INDEX idx_work_branch_control_pending (operation_state, created_at, operation_id),
    INDEX idx_work_branch_control_executor (operation_state, executor_lease_until, operation_id),
    CONSTRAINT chk_work_branch_control_kind CHECK (
        operation_kind IN ('acquire_branch_control', 'force_takeover', 'release_branch_control')
    ),
    CONSTRAINT chk_work_branch_control_state CHECK (
        operation_state IN ('pending', 'aborted', 'succeeded', 'conflict')
    ),
    CONSTRAINT chk_work_branch_control_outcome CHECK (
        operation_outcome IN (
            'pending', 'aborted', 'acquired', 'already_controlled', 'released',
            'already_released', 'taken_over', 'writer_conflict',
            'branch_revision_conflict', 'head_conflict'
        )
    ),
    CONSTRAINT chk_work_branch_control_expected_revision CHECK (expected_branch_revision > 0),
    CONSTRAINT chk_work_branch_control_expected_epoch CHECK (expected_writer_epoch >= 0),
    CONSTRAINT chk_work_branch_control_observed_revision CHECK (
        observed_branch_revision IS NULL OR observed_branch_revision > 0
    ),
    CONSTRAINT chk_work_branch_control_observed_epoch CHECK (
        observed_writer_epoch IS NULL OR observed_writer_epoch >= 0
    ),
    CONSTRAINT chk_work_branch_control_completion CHECK (
        (operation_state = 'pending' AND completed_at IS NULL AND observed_branch_revision IS NULL)
        OR
        (operation_state <> 'pending' AND completed_at IS NOT NULL AND observed_branch_revision IS NOT NULL)
    )
)";

pub(crate) const WORK_BRANCH_CREATION_OPERATIONS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_branch_creation_operations (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    origin_branch_id VARCHAR(64) NOT NULL,
    operation_id VARCHAR(64) NOT NULL,
    idempotency_hash CHAR(64) NOT NULL,
    request_hash CHAR(64) NOT NULL,
    child_branch_id VARCHAR(64) NOT NULL,
    origin_session_id VARCHAR(64) NOT NULL,
    child_session_id VARCHAR(64) NOT NULL,
    fork_cursor VARCHAR(512) NOT NULL,
    session_fork_id VARCHAR(64) NULL,
    executor_token VARCHAR(64) NULL,
    executor_lease_expires_at DATETIME(6) NULL,
    operation_state VARCHAR(32) NOT NULL,
    operation_outcome VARCHAR(32) NOT NULL,
    expected_branch_revision BIGINT NOT NULL,
    observed_branch_revision BIGINT NOT NULL,
    goal_revision_ref BIGINT NOT NULL,
    criteria_set_revision_ref BIGINT NOT NULL,
    graph_revision_ref BIGINT NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    completed_at DATETIME(6) NULL,
    PRIMARY KEY (owner_id, work_id, origin_branch_id, operation_id),
    UNIQUE KEY uq_work_branch_creation_request (
        owner_id, work_id, origin_branch_id, idempotency_hash
    ),
    UNIQUE KEY uq_work_branch_creation_child (owner_id, work_id, child_branch_id),
    INDEX idx_work_branch_creation_pending (operation_state, created_at, operation_id),
    INDEX idx_work_branch_creation_executor (
        operation_state, executor_lease_expires_at, operation_id
    ),
    INDEX idx_work_branch_creation_completed (completed_at, operation_id),
    INDEX idx_work_branch_creation_origin_session (owner_id, origin_session_id),
    INDEX idx_work_branch_creation_child_session (owner_id, child_session_id),
    CONSTRAINT chk_work_branch_creation_state CHECK (
        operation_state IN ('pending', 'aborted', 'succeeded', 'conflict')
    ),
    CONSTRAINT chk_work_branch_creation_outcome CHECK (
        operation_outcome IN (
            'pending', 'aborted', 'created', 'branch_revision_conflict', 'cursor_conflict',
            'capacity_exceeded'
        )
    ),
    CONSTRAINT chk_work_branch_creation_expected_revision CHECK (expected_branch_revision > 0),
    CONSTRAINT chk_work_branch_creation_observed_revision CHECK (observed_branch_revision > 0),
    CONSTRAINT chk_work_branch_creation_goal_revision CHECK (goal_revision_ref > 0),
    CONSTRAINT chk_work_branch_creation_criteria_revision CHECK (criteria_set_revision_ref > 0),
    CONSTRAINT chk_work_branch_creation_graph_revision CHECK (graph_revision_ref > 0),
    CONSTRAINT chk_work_branch_creation_completion CHECK (
        (operation_state = 'pending' AND operation_outcome = 'pending' AND completed_at IS NULL)
        OR
        (operation_state = 'aborted' AND operation_outcome = 'aborted'
         AND completed_at IS NOT NULL)
        OR
        (operation_state = 'succeeded' AND operation_outcome = 'created'
         AND completed_at IS NOT NULL)
        OR
        (operation_state = 'conflict'
         AND operation_outcome IN (
             'branch_revision_conflict', 'cursor_conflict', 'capacity_exceeded'
         )
         AND completed_at IS NOT NULL)
    ),
    CONSTRAINT chk_work_branch_creation_executor CHECK (
        (executor_token IS NULL AND executor_lease_expires_at IS NULL)
        OR
        (operation_state = 'pending' AND executor_token IS NOT NULL
         AND executor_lease_expires_at IS NOT NULL)
    )
)";

/// The single materialized target currently represented by a Work branch.
///
/// Workspace/materialization services continue to own the referenced content;
/// this row only owns Work's selection of that immutable subject. Keeping one
/// row per branch makes freshness checks constant-time and prevents long runs
/// from accumulating mutable "current head" history.
pub(crate) const WORK_BRANCH_SUBJECTS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_branch_subjects (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    branch_id VARCHAR(64) NOT NULL,
    subject_record_revision BIGINT NOT NULL,
    branch_revision BIGINT NOT NULL,
    graph_revision BIGINT NOT NULL,
    subject_ref VARCHAR(256) NOT NULL,
    subject_revision CHAR(71) NOT NULL,
    source_ref VARCHAR(256) NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, work_id, branch_id),
    CONSTRAINT chk_work_branch_subject_record_revision CHECK (subject_record_revision > 0),
    CONSTRAINT chk_work_branch_subject_branch_revision CHECK (branch_revision > 0),
    CONSTRAINT chk_work_branch_subject_graph_revision CHECK (graph_revision > 0)
)";

/// Immutable Work-level binding for a provider-produced patch payload.
/// Patch bytes remain in the bounded session artifact store; this table holds
/// only exact source/target identity, digest, invocation, and reachability.
pub(crate) const WORK_PATCH_ARTIFACTS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_patch_artifacts (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    branch_id VARCHAR(64) NOT NULL,
    patch_artifact_id VARCHAR(64) NOT NULL,
    session_id VARCHAR(64) NOT NULL,
    payload_artifact_id VARCHAR(64) NOT NULL,
    source_branch_revision BIGINT NOT NULL,
    source_graph_revision BIGINT NOT NULL,
    source_subject_record_revision BIGINT NOT NULL,
    subject_ref VARCHAR(256) NOT NULL,
    base_subject_revision CHAR(71) NOT NULL,
    result_subject_revision CHAR(71) NOT NULL,
    payload_hash CHAR(71) NOT NULL,
    payload_bytes BIGINT NOT NULL,
    patch_format VARCHAR(32) NOT NULL,
    provider_invocation_ref VARCHAR(128) NOT NULL,
    source_ref VARCHAR(256) NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, work_id, patch_artifact_id),
    UNIQUE KEY uq_work_patch_payload
      (owner_id, session_id, payload_artifact_id),
    INDEX idx_work_patch_branch_created
      (owner_id, work_id, branch_id, created_at, patch_artifact_id),
    CONSTRAINT chk_work_patch_branch_revision CHECK (source_branch_revision > 0),
    CONSTRAINT chk_work_patch_graph_revision CHECK (source_graph_revision > 0),
    CONSTRAINT chk_work_patch_subject_revision CHECK (source_subject_record_revision > 0),
    CONSTRAINT chk_work_patch_payload_bytes CHECK (
      payload_bytes >= 0 AND payload_bytes <= 16777216
    ),
    CONSTRAINT chk_work_patch_format CHECK (patch_format = 'unified_diff_v1'),
    CONSTRAINT chk_work_patch_changes_subject CHECK (
      base_subject_revision <> result_subject_revision
    )
)";

pub(crate) const WORK_PATCH_MATERIALIZATION_OPERATIONS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_patch_materialization_operations (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    operation_id VARCHAR(64) NOT NULL,
    request_id VARCHAR(256) NOT NULL,
    request_digest CHAR(64) NOT NULL,
    patch_artifact_id VARCHAR(64) NOT NULL,
    source_branch_id VARCHAR(64) NOT NULL,
    target_branch_id VARCHAR(64) NOT NULL,
    target_branch_revision BIGINT NOT NULL,
    target_graph_revision BIGINT NOT NULL,
    target_subject_record_revision BIGINT NOT NULL,
    subject_ref VARCHAR(256) NOT NULL,
    base_subject_revision CHAR(71) NOT NULL,
    result_subject_revision CHAR(71) NOT NULL,
    payload_hash CHAR(71) NOT NULL,
    provider_ref VARCHAR(128) NOT NULL,
    policy_decision_ref VARCHAR(256) NOT NULL,
    operation_state VARCHAR(32) NOT NULL,
    operation_phase VARCHAR(32) NOT NULL,
    executor_token VARCHAR(128) NULL,
    executor_lease_expires_at DATETIME(6) NULL,
    recovery_after DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    apply_invocation_ref VARCHAR(128) NULL,
    observed_subject_revision CHAR(71) NULL,
    apply_outcome VARCHAR(32) NULL,
    failure_code VARCHAR(32) NULL,
    verification_evidence_hash CHAR(71) NULL,
    verification_outcome VARCHAR(32) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    completed_at DATETIME(6) NULL,
    PRIMARY KEY (owner_id, work_id, operation_id),
    UNIQUE KEY uq_work_patch_materialization_request (owner_id, work_id, request_id),
    INDEX idx_work_patch_materialization_pending
      (operation_state, operation_phase, executor_lease_expires_at, created_at, operation_id),
    INDEX idx_work_patch_materialization_recovery
      (operation_state, recovery_after, operation_id),
    INDEX idx_work_patch_materialization_target
      (owner_id, work_id, target_branch_id, operation_state, created_at, operation_id),
    INDEX idx_work_patch_materialization_source_history
      (owner_id, work_id, target_branch_id, source_branch_id, created_at, operation_id),
    CONSTRAINT chk_work_patch_materialization_branch_revision CHECK
      (target_branch_revision > 0),
    CONSTRAINT chk_work_patch_materialization_graph_revision CHECK
      (target_graph_revision > 0),
    CONSTRAINT chk_work_patch_materialization_subject_revision CHECK
      (target_subject_record_revision > 0),
    CONSTRAINT chk_work_patch_materialization_state CHECK
      (operation_state IN ('pending', 'aborted', 'succeeded', 'conflict', 'failed')),
    CONSTRAINT chk_work_patch_materialization_phase CHECK
      (operation_phase IN ('awaiting_dispatch', 'applying', 'reconciling', 'verifying', 'complete')),
    CONSTRAINT chk_work_patch_materialization_terminal CHECK (
      (operation_state = 'pending' AND operation_phase <> 'complete' AND completed_at IS NULL)
      OR
      (operation_state <> 'pending' AND operation_phase = 'complete' AND completed_at IS NOT NULL)
    ),
    CONSTRAINT chk_work_patch_materialization_executor CHECK (
      (executor_token IS NULL AND executor_lease_expires_at IS NULL)
      OR
      (operation_state = 'pending' AND operation_phase IN ('applying', 'reconciling')
       AND executor_token IS NOT NULL AND executor_lease_expires_at IS NOT NULL)
    ),
    CONSTRAINT chk_work_patch_materialization_apply_report CHECK (
      (apply_invocation_ref IS NULL AND observed_subject_revision IS NULL
       AND apply_outcome IS NULL AND failure_code IS NULL)
      OR
      (apply_invocation_ref IS NOT NULL AND observed_subject_revision IS NOT NULL
       AND apply_outcome IN ('applied', 'result_mismatch', 'target_changed')
       AND failure_code IS NULL)
      OR
      (apply_invocation_ref IS NOT NULL AND observed_subject_revision IS NULL
       AND apply_outcome IS NULL AND failure_code IS NULL
       AND operation_state = 'pending' AND operation_phase IN ('applying', 'reconciling'))
      OR
      (apply_invocation_ref IS NOT NULL AND observed_subject_revision IS NULL
       AND apply_outcome = 'not_applied'
       AND failure_code IN ('provider_unavailable', 'authorization_denied',
                            'workspace_unavailable', 'patch_rejected',
                            'invocation_cancelled', 'provider_internal')
       AND operation_state = 'failed' AND operation_phase = 'complete')
    ),
    CONSTRAINT chk_work_patch_materialization_verification CHECK (
      (verification_outcome IS NULL AND verification_evidence_hash IS NULL)
      OR (verification_outcome = 'passed' AND verification_evidence_hash IS NOT NULL)
      OR (verification_outcome = 'target_changed' AND verification_evidence_hash IS NULL)
    )
    )";

pub(crate) const WORK_PATCH_COMMIT_OPERATIONS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_patch_commit_operations (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    operation_id VARCHAR(64) NOT NULL,
    request_id VARCHAR(256) NOT NULL,
    request_digest CHAR(64) NOT NULL,
    patch_artifact_id VARCHAR(64) NOT NULL,
    source_branch_id VARCHAR(64) NOT NULL,
    target_branch_id VARCHAR(64) NOT NULL,
    active_target_branch_id VARCHAR(64) NULL,
    target_branch_revision BIGINT NOT NULL,
    target_graph_revision BIGINT NOT NULL,
    target_subject_record_revision BIGINT NOT NULL,
    subject_ref VARCHAR(256) NOT NULL,
    base_subject_revision CHAR(71) NOT NULL,
    result_subject_revision CHAR(71) NOT NULL,
    payload_hash CHAR(71) NOT NULL,
    commit_message TEXT NOT NULL,
    commit_author_name VARCHAR(256) NOT NULL,
    commit_author_email VARCHAR(320) NOT NULL,
    provider_ref VARCHAR(128) NOT NULL,
    policy_decision_ref VARCHAR(256) NOT NULL,
    operation_state VARCHAR(32) NOT NULL,
    operation_phase VARCHAR(32) NOT NULL,
    executor_token VARCHAR(128) NULL,
    executor_lease_expires_at DATETIME(6) NULL,
    recovery_after DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    commit_invocation_ref VARCHAR(128) NULL,
    commit_sha VARCHAR(64) NULL,
    observed_subject_revision CHAR(71) NULL,
    index_reconciled TINYINT NULL,
    failure_code VARCHAR(64) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    completed_at DATETIME(6) NULL,
    PRIMARY KEY (owner_id, work_id, operation_id),
    UNIQUE KEY uq_work_patch_commit_request (owner_id, work_id, request_id),
    UNIQUE KEY uq_work_patch_commit_active_target
      (owner_id, work_id, active_target_branch_id),
    INDEX idx_work_patch_commit_target
      (owner_id, work_id, target_branch_id, operation_state, created_at, operation_id),
    INDEX idx_work_patch_commit_pending
      (operation_state, operation_phase, executor_lease_expires_at, operation_id),
    INDEX idx_work_patch_commit_recovery
      (operation_state, recovery_after, operation_id),
    CONSTRAINT chk_work_patch_commit_state CHECK
      (operation_state IN ('pending', 'aborted', 'succeeded', 'conflict', 'failed')),
    CONSTRAINT chk_work_patch_commit_phase CHECK
      (operation_phase IN ('awaiting_dispatch', 'committing', 'reconciling', 'complete')),
    CONSTRAINT chk_work_patch_commit_terminal CHECK (
      (operation_state = 'pending' AND operation_phase <> 'complete' AND completed_at IS NULL)
      OR
      (operation_state <> 'pending' AND operation_phase = 'complete' AND completed_at IS NOT NULL)
    ),
    CONSTRAINT chk_work_patch_commit_active_target CHECK (
      (operation_state = 'pending' AND active_target_branch_id = target_branch_id)
      OR (operation_state <> 'pending' AND active_target_branch_id IS NULL)
    ),
    CONSTRAINT chk_work_patch_commit_executor CHECK (
      (executor_token IS NULL AND executor_lease_expires_at IS NULL)
      OR
      (operation_state = 'pending' AND operation_phase IN ('committing', 'reconciling')
       AND executor_token IS NOT NULL AND executor_lease_expires_at IS NOT NULL)
    ),
    CONSTRAINT chk_work_patch_commit_result CHECK (
      (commit_sha IS NULL AND observed_subject_revision IS NULL AND index_reconciled IS NULL
       AND failure_code IS NULL)
      OR
      (commit_sha IS NOT NULL AND observed_subject_revision IS NOT NULL
       AND index_reconciled IN (0, 1) AND failure_code IS NULL)
      OR
      (commit_sha IS NULL AND index_reconciled IS NULL AND failure_code IS NOT NULL)
    )
)";

pub(crate) const WORK_PROPOSAL_SEQUENCES_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_proposal_sequences (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    branch_id VARCHAR(64) NOT NULL,
    last_proposal_seq BIGINT NOT NULL,
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, work_id, branch_id),
    CONSTRAINT chk_work_proposal_sequence CHECK (last_proposal_seq >= 0)
)";

pub(crate) const WORK_PROPOSALS_CREATE_SQL: &str = "CREATE TABLE IF NOT EXISTS work_proposals (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    proposal_id VARCHAR(64) NOT NULL,
    branch_id VARCHAR(64) NOT NULL,
    proposal_seq BIGINT NOT NULL,
    proposal_kind VARCHAR(32) NOT NULL,
    expected_work_revision BIGINT NOT NULL,
    expected_goal_revision BIGINT NOT NULL,
    expected_criteria_set_revision BIGINT NOT NULL,
    expected_branch_revision BIGINT NOT NULL,
    expected_graph_revision BIGINT NOT NULL,
    payload_json LONGTEXT NOT NULL,
    payload_hash CHAR(71) NOT NULL,
    item_change_count INT NULL,
    dependency_change_count INT NULL,
    criterion_count INT NULL,
    source_kind VARCHAR(32) NOT NULL,
    source_ref VARCHAR(256) NOT NULL,
    status VARCHAR(32) NOT NULL,
    resolution_ref VARCHAR(256) NULL,
    result_work_revision BIGINT NULL,
    result_criteria_set_revision BIGINT NULL,
    result_branch_revision BIGINT NULL,
    result_graph_revision BIGINT NULL,
    proposed_at DATETIME(6) NOT NULL,
    expires_at DATETIME(6) NOT NULL,
    resolved_at DATETIME(6) NULL,
    PRIMARY KEY (owner_id, work_id, proposal_id),
    UNIQUE KEY uq_work_proposal_branch_seq (
        owner_id, work_id, branch_id, proposal_seq
    ),
    INDEX idx_work_proposal_pending (
        owner_id, work_id, branch_id, status, expires_at, proposal_seq
    ),
    CONSTRAINT chk_work_proposal_seq CHECK (proposal_seq > 0),
    CONSTRAINT chk_work_proposal_kind CHECK (
        proposal_kind IN ('plan_patch', 'criteria_set')
    ),
    CONSTRAINT chk_work_proposal_work_revision CHECK (expected_work_revision > 0),
    CONSTRAINT chk_work_proposal_goal_revision CHECK (expected_goal_revision > 0),
    CONSTRAINT chk_work_proposal_criteria_revision CHECK (
        expected_criteria_set_revision > 0
    ),
    CONSTRAINT chk_work_proposal_branch_revision CHECK (expected_branch_revision > 0),
    CONSTRAINT chk_work_proposal_graph_revision CHECK (expected_graph_revision > 0),
    CONSTRAINT chk_work_proposal_payload_shape CHECK (
        (proposal_kind = 'plan_patch'
         AND item_change_count IS NOT NULL
         AND item_change_count >= 0 AND item_change_count <= 64
         AND dependency_change_count IS NOT NULL
         AND dependency_change_count >= 0 AND dependency_change_count <= 256
         AND (item_change_count > 0 OR dependency_change_count > 0)
         AND criterion_count IS NULL)
        OR (proposal_kind = 'criteria_set'
            AND item_change_count IS NULL AND dependency_change_count IS NULL
            AND criterion_count IS NOT NULL
            AND criterion_count > 0 AND criterion_count <= 128)
    ),
    CONSTRAINT chk_work_proposal_source CHECK (source_kind IN ('model', 'reflection')),
    CONSTRAINT chk_work_proposal_status CHECK (status IN (
        'pending', 'accepted', 'rejected', 'stale', 'superseded', 'expired'
    )),
    CONSTRAINT chk_work_proposal_expiry CHECK (expires_at > proposed_at),
    CONSTRAINT chk_work_proposal_resolution CHECK (
        (status = 'pending' AND resolution_ref IS NULL AND resolved_at IS NULL
         AND result_work_revision IS NULL AND result_criteria_set_revision IS NULL
         AND result_branch_revision IS NULL AND result_graph_revision IS NULL)
        OR (status = 'accepted' AND resolution_ref IS NOT NULL AND resolved_at IS NOT NULL
            AND ((proposal_kind = 'plan_patch'
                  AND result_work_revision IS NULL
                  AND result_criteria_set_revision IS NULL
                  AND result_branch_revision > 0 AND result_graph_revision > 0)
                 OR (proposal_kind = 'criteria_set'
                     AND result_work_revision > 0
                     AND result_criteria_set_revision > 0
                     AND result_branch_revision IS NULL
                     AND result_graph_revision IS NULL)))
        OR (status NOT IN ('pending', 'accepted')
            AND resolution_ref IS NOT NULL AND resolved_at IS NOT NULL
            AND result_work_revision IS NULL AND result_criteria_set_revision IS NULL
            AND result_branch_revision IS NULL AND result_graph_revision IS NULL)
    )
)";

pub(crate) const WORK_CHECK_RUNS_CREATE_SQL: &str = "CREATE TABLE IF NOT EXISTS work_check_runs (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    check_run_id VARCHAR(64) NOT NULL,
    branch_id VARCHAR(64) NOT NULL,
    graph_revision BIGINT NOT NULL,
    work_item_id VARCHAR(64) NOT NULL,
    work_item_revision BIGINT NOT NULL,
    work_item_attempt_id VARCHAR(64) NOT NULL,
    criterion_set_revision BIGINT NOT NULL,
    criterion_id VARCHAR(64) NOT NULL,
    criterion_revision BIGINT NOT NULL,
    criterion_definition_hash CHAR(71) NOT NULL,
    subject_ref VARCHAR(256) NOT NULL,
    subject_revision CHAR(71) NOT NULL,
    artifact_digest CHAR(71) NULL,
    run_ref VARCHAR(256) NOT NULL,
    invocation_ref VARCHAR(256) NOT NULL,
    verifier_kind VARCHAR(32) NOT NULL,
    verifier_fingerprint CHAR(71) NOT NULL,
    environment_fingerprint CHAR(71) NOT NULL,
    outcome VARCHAR(32) NOT NULL,
    error_kind VARCHAR(32) NULL,
    coverage_state VARCHAR(32) NOT NULL,
    coverage_gaps_json LONGTEXT NOT NULL,
    coverage_gap_count INT NOT NULL,
    evidence_refs_json LONGTEXT NOT NULL,
    evidence_ref_count INT NOT NULL,
    source_cursor VARCHAR(256) NOT NULL,
    produced_at DATETIME(6) NOT NULL,
    expires_at DATETIME(6) NULL,
    payload_hash CHAR(71) NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, work_id, check_run_id),
    INDEX idx_work_check_runs_branch_time (
        owner_id, work_id, branch_id, produced_at, check_run_id
    ),
    INDEX idx_work_check_runs_item_attempt_time (
        owner_id, work_id, branch_id, work_item_id, work_item_revision,
        work_item_attempt_id, produced_at, check_run_id
    ),
    INDEX idx_work_check_runs_criterion_time (
        owner_id, work_id, criterion_id, criterion_revision, produced_at, check_run_id
    ),
    INDEX idx_work_check_runs_branch_criterion_time (
        owner_id, work_id, branch_id, criterion_id, criterion_revision,
        produced_at, check_run_id
    ),
    CONSTRAINT chk_work_check_graph_revision CHECK (graph_revision > 0),
    CONSTRAINT chk_work_check_item_revision CHECK (work_item_revision > 0),
    CONSTRAINT chk_work_check_criterion_set_revision CHECK (criterion_set_revision > 0),
    CONSTRAINT chk_work_check_criterion_revision CHECK (criterion_revision > 0),
    CONSTRAINT chk_work_check_verifier_kind CHECK (verifier_kind IN ('command', 'test')),
    CONSTRAINT chk_work_check_outcome CHECK (outcome IN ('passed', 'failed', 'error', 'cancelled')),
    CONSTRAINT chk_work_check_coverage CHECK (
        coverage_state IN ('complete', 'partial', 'unavailable')
    ),
    CONSTRAINT chk_work_check_gap_count CHECK (
        coverage_gap_count >= 0 AND coverage_gap_count <= 16
    ),
    CONSTRAINT chk_work_check_evidence_count CHECK (
        evidence_ref_count >= 0 AND evidence_ref_count <= 32
    ),
    CONSTRAINT chk_work_check_error_kind CHECK ((outcome = 'error') = (error_kind IS NOT NULL)),
    CONSTRAINT chk_work_check_expiry CHECK (expires_at IS NULL OR expires_at > produced_at)
)";

pub(crate) const WORK_ACCEPTANCE_DECISIONS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_acceptance_decisions (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    decision_id VARCHAR(64) NOT NULL,
    branch_id VARCHAR(64) NOT NULL,
    work_revision BIGINT NOT NULL,
    goal_revision BIGINT NOT NULL,
    branch_revision BIGINT NOT NULL,
    graph_revision BIGINT NOT NULL,
    criterion_set_revision BIGINT NOT NULL,
    subject_ref VARCHAR(256) NOT NULL,
    subject_revision CHAR(71) NOT NULL,
    accepted_gaps_json LONGTEXT NOT NULL,
    accepted_gap_count INT NOT NULL,
    check_ref_count INT NOT NULL,
    source_cursor VARCHAR(256) NOT NULL,
    decided_by_id VARCHAR(128) NOT NULL,
    decided_at DATETIME(6) NOT NULL,
    payload_hash CHAR(71) NOT NULL,
    PRIMARY KEY (owner_id, work_id, decision_id),
    INDEX idx_work_acceptance_branch_time (
        owner_id, work_id, branch_id, decided_at, decision_id
    ),
    CONSTRAINT chk_work_acceptance_work_revision CHECK (work_revision > 0),
    CONSTRAINT chk_work_acceptance_goal_revision CHECK (goal_revision > 0),
    CONSTRAINT chk_work_acceptance_branch_revision CHECK (branch_revision > 0),
    CONSTRAINT chk_work_acceptance_graph_revision CHECK (graph_revision > 0),
    CONSTRAINT chk_work_acceptance_criteria_revision CHECK (criterion_set_revision > 0),
    CONSTRAINT chk_work_acceptance_gap_count CHECK (
        accepted_gap_count > 0 AND accepted_gap_count <= 32
    ),
    CONSTRAINT chk_work_acceptance_check_count CHECK (
        check_ref_count >= 0 AND check_ref_count <= 64
    )
)";

/// Bounded canonical projection of the exact gaps currently accepted for each
/// criterion on a branch. Immutable decision history may follow Work event
/// retention; this projection preserves the user-owned acceptance fact and
/// the hashes of verifier records it relied on without retaining every check.
pub(crate) const WORK_CURRENT_GAP_ACCEPTANCES_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_current_gap_acceptances (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    branch_id VARCHAR(64) NOT NULL,
    criterion_id VARCHAR(64) NOT NULL,
    criterion_revision BIGINT NOT NULL,
    decision_id VARCHAR(64) NOT NULL,
    decision_event_seq BIGINT NOT NULL,
    work_revision BIGINT NOT NULL,
    goal_revision BIGINT NOT NULL,
    branch_revision BIGINT NOT NULL,
    graph_revision BIGINT NOT NULL,
    criterion_set_revision BIGINT NOT NULL,
    subject_ref VARCHAR(256) NOT NULL,
    subject_revision CHAR(71) NOT NULL,
    gap_reason VARCHAR(32) NOT NULL,
    resolved_check_refs_json LONGTEXT NOT NULL,
    resolved_check_ref_count INT NOT NULL,
    decision_payload_hash CHAR(71) NOT NULL,
    source_cursor VARCHAR(256) NOT NULL,
    decided_by_id VARCHAR(128) NOT NULL,
    decided_at DATETIME(6) NOT NULL,
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, work_id, branch_id, criterion_id),
    INDEX idx_work_current_gap_decision (
        owner_id, work_id, decision_event_seq, decision_id
    ),
    CONSTRAINT chk_work_current_gap_criterion_revision CHECK (criterion_revision > 0),
    CONSTRAINT chk_work_current_gap_event_seq CHECK (decision_event_seq > 0),
    CONSTRAINT chk_work_current_gap_work_revision CHECK (work_revision > 0),
    CONSTRAINT chk_work_current_gap_goal_revision CHECK (goal_revision > 0),
    CONSTRAINT chk_work_current_gap_branch_revision CHECK (branch_revision > 0),
    CONSTRAINT chk_work_current_gap_graph_revision CHECK (graph_revision > 0),
    CONSTRAINT chk_work_current_gap_criteria_revision CHECK (criterion_set_revision > 0),
    CONSTRAINT chk_work_current_gap_reason CHECK (gap_reason IN (
        'missing_evidence', 'partial_coverage', 'stale_evidence',
        'unsupported_verifier', 'human_judgment'
    )),
    CONSTRAINT chk_work_current_gap_check_count CHECK (
        resolved_check_ref_count >= 0 AND resolved_check_ref_count <= 8
    )
)";

pub(crate) const WORK_EVENT_SEQUENCES_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_event_sequences (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    last_event_seq BIGINT NOT NULL,
    retained_from_event_seq BIGINT NOT NULL,
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, work_id),
    CONSTRAINT chk_work_event_sequence CHECK (last_event_seq > 0),
    CONSTRAINT chk_work_event_retention CHECK (
        retained_from_event_seq > 0 AND retained_from_event_seq <= last_event_seq
    )
)";

pub(crate) const WORK_ATTENTION_RECEIPTS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_attention_receipts (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    receipt_revision BIGINT NOT NULL,
    delivered_through_event_seq BIGINT NOT NULL,
    seen_through_event_seq BIGINT NOT NULL,
    delivered_receipt_hash CHAR(71) NULL,
    seen_receipt_hash CHAR(71) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, work_id),
    CONSTRAINT chk_work_attention_revision CHECK (receipt_revision > 0),
    CONSTRAINT chk_work_attention_delivered_cursor CHECK (delivered_through_event_seq >= 0),
    CONSTRAINT chk_work_attention_seen_cursor CHECK (seen_through_event_seq >= 0),
    CONSTRAINT chk_work_attention_delivered_hash CHECK (
        (delivered_through_event_seq = 0) = (delivered_receipt_hash IS NULL)
    ),
    CONSTRAINT chk_work_attention_seen_hash CHECK (
        (seen_through_event_seq = 0) = (seen_receipt_hash IS NULL)
    )
)";

pub(crate) const WORK_ITEM_ATTEMPTS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_item_attempts (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    branch_id VARCHAR(64) NOT NULL,
    work_item_id VARCHAR(64) NOT NULL,
    work_item_revision BIGINT NOT NULL,
    attempt_id VARCHAR(64) NOT NULL,
    executor_run_id VARCHAR(64) NOT NULL,
    execution_mode VARCHAR(32) NOT NULL,
    status VARCHAR(32) NOT NULL,
    graph_revision BIGINT NOT NULL,
    run_generation BIGINT NOT NULL DEFAULT 0,
    last_event_idx BIGINT NOT NULL DEFAULT -1,
    outcome VARCHAR(32) NULL,
    summary_text LONGTEXT NULL,
    blocker_kind VARCHAR(32) NULL,
    unavailable_capabilities_json LONGTEXT NOT NULL,
    started_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    settled_at DATETIME(6) NULL,
    PRIMARY KEY (
        owner_id, work_id, branch_id, work_item_id, work_item_revision, attempt_id
    ),
    UNIQUE KEY uq_work_item_attempt_identity (owner_id, attempt_id),
    INDEX idx_work_item_attempt_latest (
        owner_id, work_id, branch_id, work_item_id, work_item_revision,
        started_at, attempt_id
    ),
    INDEX idx_work_item_attempt_executor (
        owner_id, executor_run_id, status, started_at, attempt_id
    ),
    INDEX idx_work_item_attempt_branch_active (
        owner_id, work_id, branch_id, execution_mode, status, started_at, attempt_id
    ),
    CONSTRAINT chk_work_item_attempt_revision CHECK (
        work_item_revision > 0 AND graph_revision > 0
    ),
    CONSTRAINT chk_work_item_attempt_mode CHECK (
        execution_mode IN ('primary', 'delegated')
    ),
    CONSTRAINT chk_work_item_attempt_status CHECK (
        status IN ('running', 'waiting', 'paused', 'completed', 'delegated', 'failed', 'cancelled')
    ),
    CONSTRAINT chk_work_item_attempt_cursor CHECK (
        run_generation >= 0 AND last_event_idx >= -1
    ),
    CONSTRAINT chk_work_item_attempt_outcome CHECK (
        outcome IS NULL OR outcome IN ('delivered', 'blocked', 'failed')
    ),
    CONSTRAINT chk_work_item_attempt_blocker CHECK (
        (outcome = 'blocked' AND blocker_kind IN (
            'capability_unavailable', 'dependency_blocked',
            'policy_blocked', 'external_unavailable'
        ))
        OR ((outcome IS NULL OR outcome <> 'blocked') AND blocker_kind IS NULL)
    ),
    CONSTRAINT chk_work_item_attempt_settlement CHECK (
        (outcome IS NULL AND summary_text IS NULL AND settled_at IS NULL)
        OR (outcome IS NOT NULL AND summary_text IS NOT NULL AND settled_at IS NOT NULL)
    )
)";

pub(crate) const WORK_TERMINAL_CUTS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_terminal_cuts (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    branch_id VARCHAR(64) NOT NULL,
    graph_revision BIGINT NOT NULL,
    attempt_id VARCHAR(64) NOT NULL,
    control_epoch BIGINT NOT NULL,
    PRIMARY KEY (owner_id, work_id, branch_id, graph_revision),
    UNIQUE KEY uq_work_terminal_cut_attempt (owner_id, attempt_id)
)";

pub(crate) const WORK_EVENTS_CREATE_SQL: &str = "CREATE TABLE IF NOT EXISTS work_events (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    event_seq BIGINT NOT NULL,
    branch_id VARCHAR(64) NULL,
    event_kind VARCHAR(32) NOT NULL,
    work_revision BIGINT NULL,
    goal_revision BIGINT NULL,
    criterion_set_revision BIGINT NULL,
    branch_revision BIGINT NULL,
    graph_revision BIGINT NULL,
    source_ref VARCHAR(256) NOT NULL,
    payload_hash CHAR(71) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, work_id, event_seq),
    UNIQUE KEY uq_work_event_source (owner_id, work_id, event_kind, source_ref),
    INDEX idx_work_events_branch_seq (owner_id, work_id, branch_id, event_seq),
    CONSTRAINT chk_work_event_seq CHECK (event_seq > 0),
    CONSTRAINT chk_work_event_kind CHECK (event_kind IN (
        'work_created', 'goal_revised', 'criteria_accepted', 'branch_basis_adopted',
        'graph_replaced', 'delivery_branch_selected',
        'subject_changed', 'plan_proposed', 'criteria_proposed', 'proposal_rejected',
        'check_recorded', 'gaps_accepted',
        'run_completed', 'run_delegated', 'run_failed', 'run_cancelled',
        'runtime_events_expired'
    )),
    CONSTRAINT chk_work_event_work_revision CHECK (
        work_revision IS NULL OR work_revision > 0
    ),
    CONSTRAINT chk_work_event_goal_revision CHECK (
        goal_revision IS NULL OR goal_revision > 0
    ),
    CONSTRAINT chk_work_event_criteria_revision CHECK (
        criterion_set_revision IS NULL OR criterion_set_revision > 0
    ),
    CONSTRAINT chk_work_event_branch_revision CHECK (
        branch_revision IS NULL OR branch_revision > 0
    ),
    CONSTRAINT chk_work_event_graph_revision CHECK (
        graph_revision IS NULL OR graph_revision > 0
    )
)";

pub(crate) const WORK_RUNTIME_EVENT_OUTBOX_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_runtime_event_outbox (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    branch_id VARCHAR(64) NOT NULL,
    runtime_event_seq BIGINT NOT NULL,
    event_kind VARCHAR(32) NOT NULL,
    graph_revision BIGINT NOT NULL,
    source_ref VARCHAR(256) NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, work_id, runtime_event_seq),
    UNIQUE KEY uq_work_runtime_event_source
        (owner_id, work_id, event_kind, source_ref),
    CONSTRAINT chk_work_runtime_event_outbox_kind CHECK (event_kind IN (
        'run_completed', 'run_delegated', 'run_failed', 'run_cancelled'
    )),
    CONSTRAINT chk_work_runtime_event_outbox_sequence CHECK (runtime_event_seq > 0),
    CONSTRAINT chk_work_runtime_event_outbox_graph_revision CHECK (graph_revision > 0)
)";

pub(crate) const WORK_RUNTIME_EVENT_OUTBOX_SLOTS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS work_runtime_event_outbox_slots (
    owner_id VARCHAR(128) NOT NULL,
    work_id VARCHAR(64) NOT NULL,
    last_enqueued_event_seq BIGINT NOT NULL,
    last_projected_event_seq BIGINT NOT NULL,
    has_pending TINYINT NOT NULL,
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (owner_id, work_id),
    INDEX idx_work_runtime_event_pending
        (has_pending, updated_at, owner_id, work_id),
    CONSTRAINT chk_work_runtime_event_outbox_sequence CHECK (
        last_enqueued_event_seq >= 0
        AND last_projected_event_seq >= 0
        AND last_projected_event_seq <= last_enqueued_event_seq
    ),
    CONSTRAINT chk_work_runtime_event_outbox_pending CHECK (
        (has_pending = 0 AND last_projected_event_seq = last_enqueued_event_seq)
        OR
        (has_pending = 1 AND last_projected_event_seq < last_enqueued_event_seq)
    )
)";

/// Canonical schema owned by the Work domain.
///
/// Storage initialization consumes this manifest directly so adding a Work
/// table cannot silently create a second, storage-owned schema list. Legacy
/// task, plan, checklist, transcript, and reflection stores are deliberately
/// absent: they are not inputs to a fresh WorkRepository.
pub(crate) const WORK_SCHEMA_TABLES: &[(&str, &str)] = &[
    ("works", WORKS_CREATE_SQL),
    ("work_goal_revisions", WORK_GOAL_REVISIONS_CREATE_SQL),
    ("work_criteria", WORK_CRITERIA_CREATE_SQL),
    (
        "work_criterion_revisions",
        WORK_CRITERION_REVISIONS_CREATE_SQL,
    ),
    ("work_criterion_sets", WORK_CRITERION_SETS_CREATE_SQL),
    ("work_graph_revisions", WORK_GRAPH_REVISIONS_CREATE_SQL),
    ("work_graph_sequences", WORK_GRAPH_SEQUENCES_CREATE_SQL),
    ("work_items", WORK_ITEMS_CREATE_SQL),
    ("work_item_revisions", WORK_ITEM_REVISIONS_CREATE_SQL),
    ("work_item_edges", WORK_ITEM_EDGES_CREATE_SQL),
    ("work_branches", WORK_BRANCHES_CREATE_SQL),
    (
        "work_branch_creation_operations",
        WORK_BRANCH_CREATION_OPERATIONS_CREATE_SQL,
    ),
    (
        "work_branch_control_operations",
        WORK_BRANCH_CONTROL_OPERATIONS_CREATE_SQL,
    ),
    (
        "work_branch_deletion_operations",
        WORK_BRANCH_DELETION_OPERATIONS_CREATE_SQL,
    ),
    ("work_branch_subjects", WORK_BRANCH_SUBJECTS_CREATE_SQL),
    ("work_patch_artifacts", WORK_PATCH_ARTIFACTS_CREATE_SQL),
    (
        "work_patch_materialization_operations",
        WORK_PATCH_MATERIALIZATION_OPERATIONS_CREATE_SQL,
    ),
    (
        "work_patch_commit_operations",
        WORK_PATCH_COMMIT_OPERATIONS_CREATE_SQL,
    ),
    (
        "work_proposal_sequences",
        WORK_PROPOSAL_SEQUENCES_CREATE_SQL,
    ),
    ("work_proposals", WORK_PROPOSALS_CREATE_SQL),
    ("work_check_runs", WORK_CHECK_RUNS_CREATE_SQL),
    (
        "work_acceptance_decisions",
        WORK_ACCEPTANCE_DECISIONS_CREATE_SQL,
    ),
    (
        "work_current_gap_acceptances",
        WORK_CURRENT_GAP_ACCEPTANCES_CREATE_SQL,
    ),
    ("work_event_sequences", WORK_EVENT_SEQUENCES_CREATE_SQL),
    (
        "work_attention_receipts",
        WORK_ATTENTION_RECEIPTS_CREATE_SQL,
    ),
    ("work_item_attempts", WORK_ITEM_ATTEMPTS_CREATE_SQL),
    ("work_terminal_cuts", WORK_TERMINAL_CUTS_CREATE_SQL),
    ("work_events", WORK_EVENTS_CREATE_SQL),
    (
        "work_runtime_event_outbox_slots",
        WORK_RUNTIME_EVENT_OUTBOX_SLOTS_CREATE_SQL,
    ),
    (
        "work_runtime_event_outbox",
        WORK_RUNTIME_EVENT_OUTBOX_CREATE_SQL,
    ),
];

pub(crate) use runtime_event_outbox::enqueue_root_run_terminal_event;

const WORK_ID_MAX_CHARS: usize = 64;
const WORK_BRANCH_ID_MAX_CHARS: usize = 64;
const INTERNAL_SESSION_ID_MAX_CHARS: usize = 64;
const WORK_OWNER_ID_MAX_CHARS: usize = 128;
const PROJECT_ID_MAX_CHARS: usize = 128;
const ORIGINAL_INTENT_REF_MAX_CHARS: usize = 256;
const FORK_CURSOR_REF_MAX_CHARS: usize = 512;
const WORK_CHANGE_REF_MAX_CHARS: usize = 256;
// The accepted Goal is a concise product contract, not the original prompt or
// transcript. Keeping it bounded leaves room for the rest of a <64 KiB shell.
const WORK_GOAL_MAX_BYTES: usize = 16 * 1024;
const WORK_CHANGE_REASON_MAX_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum WorkIdentityViolation {
    #[error("must not be empty")]
    Empty,
    #[error("must not contain whitespace")]
    Whitespace,
    #[error("must not contain control characters")]
    ControlCharacter,
    #[error("must use only ASCII letters, digits, dot, underscore, or hyphen")]
    UnsafeResourceCharacter,
    #[error("exceeds the {max_chars} character limit")]
    TooLong { max_chars: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum WorkGoalViolation {
    #[error("must not be empty")]
    Empty,
    #[error("exceeds the {max_bytes} byte limit")]
    TooLarge { max_bytes: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum WorkChangeReasonViolation {
    #[error("must not be empty")]
    Empty,
    #[error("exceeds the {max_bytes} byte limit")]
    TooLarge { max_bytes: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum CriterionStatementViolation {
    #[error("must not be empty")]
    Empty,
    #[error("exceeds the {max_bytes} byte limit")]
    TooLarge { max_bytes: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum CriterionCommandViolation {
    #[error("must not be empty")]
    Empty,
    #[error("exceeds the {max_bytes} byte limit")]
    TooLarge { max_bytes: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum WorkItemTextViolation {
    #[error("must not be empty")]
    Empty,
    #[error("exceeds the {max_bytes} byte limit")]
    TooLarge { max_bytes: usize },
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum WorkDomainError {
    #[error("invalid {field}: {violation}")]
    InvalidIdentity {
        field: &'static str,
        violation: WorkIdentityViolation,
    },
    #[error("invalid {field} revision {value}: revisions start at one")]
    InvalidRevision { field: &'static str, value: i64 },
    #[error("invalid goal: {violation}")]
    InvalidGoal { violation: WorkGoalViolation },
    #[error("invalid change reason: {violation}")]
    InvalidChangeReason {
        violation: WorkChangeReasonViolation,
    },
    #[error("{field} revision cannot advance beyond i64::MAX")]
    RevisionExhausted { field: &'static str },
    #[error("Work item {item_id} has no repository-allocated successor revision")]
    UnallocatedWorkItemRevision { item_id: String },
    #[error("invalid criterion statement: {violation}")]
    InvalidCriterionStatement {
        violation: CriterionStatementViolation,
    },
    #[error("invalid criterion command: {violation}")]
    InvalidCriterionCommand {
        violation: CriterionCommandViolation,
    },
    #[error("criterion set exceeds the {max_members} member limit")]
    TooManyCriteria { max_members: usize },
    #[error("criterion set repeats criterion identity {criterion_id}")]
    DuplicateCriterion { criterion_id: String },
    #[error("criterion definitions exceed the {max_bytes} byte aggregate limit")]
    CriteriaPayloadTooLarge { max_bytes: usize },
    #[error("invalid WorkItem text: {violation}")]
    InvalidWorkItemText { violation: WorkItemTextViolation },
    #[error("Work graph exceeds the {max_items} item limit")]
    TooManyWorkItems { max_items: usize },
    #[error("Work graph exceeds the {max_edges} edge limit")]
    TooManyWorkItemEdges { max_edges: usize },
    #[error("Work graph repeats item identity {item_id}")]
    DuplicateWorkItem { item_id: String },
    #[error("Work graph repeats an edge")]
    DuplicateWorkItemEdge,
    #[error("Work item {item_id} depends on itself")]
    SelfDependentWorkItem { item_id: String },
    #[error("Work graph edge references unknown item {item_id}")]
    UnknownWorkItemEdgeEndpoint { item_id: String },
    #[error("Work item dependency graph contains a cycle")]
    CyclicWorkItemGraph,
    #[error("Task Graph page limits must be positive and at most {max_items}")]
    InvalidTaskGraphPageLimit { max_items: u16 },
    #[error("Task Graph continuation offsets require an exact graph revision")]
    UnpinnedTaskGraphCursor,
    #[error("Work criteria page limits must be positive and at most {max_items}")]
    InvalidCriteriaPageLimit { max_items: u16 },
    #[error("Work criteria page offsets must be at most {max_items}")]
    InvalidCriteriaPageOffset { max_items: u16 },
    #[error("Work criteria continuation offsets require an exact criterion-set revision")]
    UnpinnedCriteriaPageCursor,
    #[error("Work criteria page entries do not exactly cover one canonical requested slice")]
    InvalidCriteriaPageEntries,
    #[error("fork origin and fork cursor must either both be present or both be absent")]
    IncompleteForkLineage,
    #[error("current graph revision precedes the branch basis graph revision")]
    GraphRevisionBeforeBasis,
    #[error("archived_at precedes created_at")]
    ArchiveBeforeCreation,
    #[error("delivery branch belongs to a different Work")]
    DeliveryBranchWorkMismatch,
    #[error("branch is not the Work delivery branch")]
    DeliveryBranchIdentityMismatch,
    #[error("invalid Work check run: {violation}")]
    InvalidCheckRun { violation: CheckRunViolation },
    #[error("invalid Work acceptance decision: {violation}")]
    InvalidAcceptanceDecision {
        violation: AcceptanceDecisionViolation,
    },
    #[error("Work event page limit {value} must be between 1 and {maximum}")]
    InvalidEventPageLimit { value: u16, maximum: u16 },
    #[error("Work catalog page limit {value} must be between 1 and {maximum}")]
    InvalidCatalogPageLimit { value: u16, maximum: u16 },
    #[error("Work patch artifact page limit {value} must be between 1 and {maximum}")]
    InvalidPatchArtifactPageLimit { value: u16, maximum: u16 },
    #[error("invalid Work plan proposal: {violation}")]
    InvalidPlanProposal {
        violation: WorkPlanProposalViolation,
    },
    #[error("invalid Work criteria proposal: {violation}")]
    InvalidCriteriaProposal {
        violation: WorkCriteriaProposalViolation,
    },
}

fn validate_identity(
    field: &'static str,
    value: &str,
    max_chars: usize,
) -> Result<(), WorkDomainError> {
    let violation = if value.is_empty() {
        Some(WorkIdentityViolation::Empty)
    } else if value.chars().any(char::is_control) {
        Some(WorkIdentityViolation::ControlCharacter)
    } else if value.chars().any(char::is_whitespace) {
        Some(WorkIdentityViolation::Whitespace)
    } else if value.chars().count() > max_chars {
        Some(WorkIdentityViolation::TooLong { max_chars })
    } else {
        None
    };

    match violation {
        Some(violation) => Err(WorkDomainError::InvalidIdentity { field, violation }),
        None => Ok(()),
    }
}

fn validate_resource_identity(
    field: &'static str,
    value: &str,
    max_chars: usize,
) -> Result<(), WorkDomainError> {
    validate_identity(field, value, max_chars)?;
    if value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(WorkDomainError::InvalidIdentity {
            field,
            violation: WorkIdentityViolation::UnsafeResourceCharacter,
        });
    }
    Ok(())
}

macro_rules! opaque_identity {
    ($name:ident, $field:literal, $max_chars:expr) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
                let value = value.into();
                validate_identity($field, &value, $max_chars)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }
    };
}

macro_rules! opaque_resource_identity {
    ($name:ident, $field:literal, $max_chars:expr) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
                let value = value.into();
                validate_resource_identity($field, &value, $max_chars)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }
    };
}

opaque_resource_identity!(WorkId, "work_id", WORK_ID_MAX_CHARS);
opaque_resource_identity!(WorkBranchId, "branch_id", WORK_BRANCH_ID_MAX_CHARS);
opaque_identity!(
    InternalSessionId,
    "session_id",
    INTERNAL_SESSION_ID_MAX_CHARS
);
opaque_identity!(WorkOwnerId, "owner_id", WORK_OWNER_ID_MAX_CHARS);
opaque_identity!(ProjectId, "project_id", PROJECT_ID_MAX_CHARS);
opaque_identity!(
    OriginalIntentRef,
    "original_intent_ref",
    ORIGINAL_INTENT_REF_MAX_CHARS
);
opaque_identity!(ForkCursorRef, "fork_cursor", FORK_CURSOR_REF_MAX_CHARS);
opaque_identity!(WorkChangeRef, "work_change_ref", WORK_CHANGE_REF_MAX_CHARS);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct WorkGoal(String);

impl WorkGoal {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        let violation = if value.trim().is_empty() {
            Some(WorkGoalViolation::Empty)
        } else if value.len() > WORK_GOAL_MAX_BYTES {
            Some(WorkGoalViolation::TooLarge {
                max_bytes: WORK_GOAL_MAX_BYTES,
            })
        } else {
            None
        };
        match violation {
            Some(violation) => Err(WorkDomainError::InvalidGoal { violation }),
            None => Ok(Self(value)),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct WorkChangeReason(String);

impl WorkChangeReason {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        let violation = if value.trim().is_empty() {
            Some(WorkChangeReasonViolation::Empty)
        } else if value.len() > WORK_CHANGE_REASON_MAX_BYTES {
            Some(WorkChangeReasonViolation::TooLarge {
                max_bytes: WORK_CHANGE_REASON_MAX_BYTES,
            })
        } else {
            None
        };
        match violation {
            Some(violation) => Err(WorkDomainError::InvalidChangeReason { violation }),
            None => Ok(Self(value)),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

macro_rules! revision_type {
    ($name:ident, $field:literal) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(i64);

        impl $name {
            pub const INITIAL: Self = Self(1);

            pub fn new(value: i64) -> Result<Self, WorkDomainError> {
                if value < 1 {
                    return Err(WorkDomainError::InvalidRevision {
                        field: $field,
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
                    .ok_or(WorkDomainError::RevisionExhausted { field: $field })
            }
        }
    };
}

revision_type!(WorkRevision, "work");
revision_type!(WorkBranchRevision, "work branch");
revision_type!(GoalRevision, "goal");
revision_type!(CriterionSetRevision, "criterion set");
revision_type!(GraphRevision, "graph");

/// One atomic authority fact for the settlement that made a Work graph
/// terminal. Persistence stores this in a dedicated relation, so absence and
/// presence cannot be split across independently nullable attempt columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WorkAttemptTerminalCut {
    pub graph_revision: GraphRevision,
    pub control_epoch: i64,
}

impl WorkAttemptTerminalCut {
    pub(crate) fn new(graph_revision: GraphRevision, control_epoch: i64) -> Option<Self> {
        (control_epoch >= -1).then_some(Self {
            graph_revision,
            control_epoch,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkRecordParts {
    pub work_id: WorkId,
    pub owner_id: WorkOwnerId,
    pub work_revision: WorkRevision,
    pub project_id: Option<ProjectId>,
    pub original_intent_ref: OriginalIntentRef,
    pub current_goal_revision: GoalRevision,
    pub current_criteria_set_revision: CriterionSetRevision,
    pub delivery_branch_id: WorkBranchId,
    pub created_at: DateTime<Utc>,
    pub archived_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkRecord {
    parts: WorkRecordParts,
}

impl WorkRecord {
    pub fn from_parts(parts: WorkRecordParts) -> Result<Self, WorkDomainError> {
        if parts.archived_at.is_some_and(|at| at < parts.created_at) {
            return Err(WorkDomainError::ArchiveBeforeCreation);
        }
        Ok(Self { parts })
    }

    pub fn parts(&self) -> &WorkRecordParts {
        &self.parts
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkBranchRecordParts {
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub branch_revision: WorkBranchRevision,
    pub session_id: InternalSessionId,
    pub origin_branch_id: Option<WorkBranchId>,
    pub fork_cursor: Option<ForkCursorRef>,
    pub goal_revision_ref: GoalRevision,
    pub criteria_set_revision_ref: CriterionSetRevision,
    pub basis_graph_revision: GraphRevision,
    pub current_graph_revision: GraphRevision,
    pub created_at: DateTime<Utc>,
    pub archived_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkBranchRecord {
    parts: WorkBranchRecordParts,
}

impl WorkBranchRecord {
    pub fn from_parts(parts: WorkBranchRecordParts) -> Result<Self, WorkDomainError> {
        if parts.origin_branch_id.is_some() != parts.fork_cursor.is_some() {
            return Err(WorkDomainError::IncompleteForkLineage);
        }
        if parts.current_graph_revision < parts.basis_graph_revision {
            return Err(WorkDomainError::GraphRevisionBeforeBasis);
        }
        if parts.archived_at.is_some_and(|at| at < parts.created_at) {
            return Err(WorkDomainError::ArchiveBeforeCreation);
        }
        Ok(Self { parts })
    }

    pub fn parts(&self) -> &WorkBranchRecordParts {
        &self.parts
    }
}

/// Verifies the minimum invariant required before a branch may be selected as
/// a Work's delivery branch. Goal/criteria freshness is a separate derived
/// decision and must not be collapsed into identity validation.
pub fn validate_delivery_branch_binding(
    work: &WorkRecord,
    branch: &WorkBranchRecord,
) -> Result<(), WorkDomainError> {
    if work.parts.work_id != branch.parts.work_id {
        return Err(WorkDomainError::DeliveryBranchWorkMismatch);
    }
    if work.parts.delivery_branch_id != branch.parts.branch_id {
        return Err(WorkDomainError::DeliveryBranchIdentityMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::collections::BTreeSet;

    fn at(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 1, 10, 0, second)
            .single()
            .expect("fixture timestamp")
    }

    fn work(work_id: &str, delivery_branch_id: &str) -> WorkRecord {
        WorkRecord::from_parts(WorkRecordParts {
            work_id: WorkId::parse(work_id).expect("work id"),
            owner_id: WorkOwnerId::parse("owner-1").expect("owner id"),
            work_revision: WorkRevision::INITIAL,
            project_id: None,
            original_intent_ref: OriginalIntentRef::parse("event-1").expect("intent ref"),
            current_goal_revision: GoalRevision::INITIAL,
            current_criteria_set_revision: CriterionSetRevision::INITIAL,
            delivery_branch_id: WorkBranchId::parse(delivery_branch_id).expect("branch id"),
            created_at: at(0),
            archived_at: None,
        })
        .expect("valid work")
    }

    fn branch(work_id: &str, branch_id: &str) -> WorkBranchRecord {
        WorkBranchRecord::from_parts(WorkBranchRecordParts {
            work_id: WorkId::parse(work_id).expect("work id"),
            branch_id: WorkBranchId::parse(branch_id).expect("branch id"),
            branch_revision: WorkBranchRevision::INITIAL,
            session_id: InternalSessionId::parse("session-1").expect("session id"),
            origin_branch_id: None,
            fork_cursor: None,
            goal_revision_ref: GoalRevision::INITIAL,
            criteria_set_revision_ref: CriterionSetRevision::INITIAL,
            basis_graph_revision: GraphRevision::INITIAL,
            current_graph_revision: GraphRevision::INITIAL,
            created_at: at(0),
            archived_at: None,
        })
        .expect("valid branch")
    }

    #[test]
    fn identities_reject_non_canonical_or_unbounded_values() {
        let cases = [
            (WorkId::parse(""), WorkIdentityViolation::Empty),
            (WorkId::parse("work id"), WorkIdentityViolation::Whitespace),
            (
                WorkId::parse("work\u{7f}"),
                WorkIdentityViolation::ControlCharacter,
            ),
            (
                WorkId::parse("work/id"),
                WorkIdentityViolation::UnsafeResourceCharacter,
            ),
            (
                WorkId::parse("x".repeat(WORK_ID_MAX_CHARS + 1)),
                WorkIdentityViolation::TooLong {
                    max_chars: WORK_ID_MAX_CHARS,
                },
            ),
        ];

        for (result, expected_violation) in cases {
            assert!(matches!(
                result,
                Err(WorkDomainError::InvalidIdentity { violation, .. })
                    if violation == expected_violation
            ));
        }
    }

    #[test]
    fn revision_types_reject_zero_and_negative_database_values() {
        for value in [0, -1, i64::MIN] {
            assert!(matches!(
                GraphRevision::new(value),
                Err(WorkDomainError::InvalidRevision {
                    field: "graph",
                    value: actual,
                }) if actual == value
            ));
        }
    }

    #[test]
    fn goal_preserves_user_text_but_rejects_empty_or_unbounded_payloads() {
        let goal = WorkGoal::parse("  Diagnose the failure\nthen fix it.  ").expect("goal");
        assert_eq!(goal.as_str(), "  Diagnose the failure\nthen fix it.  ");

        assert!(matches!(
            WorkGoal::parse(" \n\t "),
            Err(WorkDomainError::InvalidGoal {
                violation: WorkGoalViolation::Empty
            })
        ));
        assert!(matches!(
            WorkGoal::parse("x".repeat(WORK_GOAL_MAX_BYTES + 1)),
            Err(WorkDomainError::InvalidGoal {
                violation: WorkGoalViolation::TooLarge { .. }
            })
        ));
    }

    #[test]
    fn revision_advancement_and_change_reason_are_bounded_domain_facts() {
        assert_eq!(WorkRevision::INITIAL.checked_next().expect("next").get(), 2);
        assert_eq!(
            WorkRevision::new(i64::MAX)
                .expect("maximum revision")
                .checked_next(),
            Err(WorkDomainError::RevisionExhausted { field: "work" })
        );
        assert!(matches!(
            WorkChangeReason::parse(" \n "),
            Err(WorkDomainError::InvalidChangeReason {
                violation: WorkChangeReasonViolation::Empty
            })
        ));
    }

    #[test]
    fn fork_lineage_is_atomic() {
        let mut parts = branch("work-1", "branch-2").parts().clone();
        parts.origin_branch_id = Some(WorkBranchId::parse("branch-1").expect("origin"));

        assert_eq!(
            WorkBranchRecord::from_parts(parts),
            Err(WorkDomainError::IncompleteForkLineage)
        );
    }

    #[test]
    fn branch_head_cannot_precede_its_fork_basis() {
        let mut parts = branch("work-1", "branch-2").parts().clone();
        parts.basis_graph_revision = GraphRevision::new(3).expect("basis");
        parts.current_graph_revision = GraphRevision::new(2).expect("head");

        assert_eq!(
            WorkBranchRecord::from_parts(parts),
            Err(WorkDomainError::GraphRevisionBeforeBasis)
        );
    }

    #[test]
    fn persisted_records_reject_impossible_archive_ordering() {
        let mut parts = work("work-1", "branch-1").parts().clone();
        parts.archived_at = Some(at(0));
        parts.created_at = at(1);

        assert_eq!(
            WorkRecord::from_parts(parts),
            Err(WorkDomainError::ArchiveBeforeCreation)
        );
    }

    #[test]
    fn delivery_binding_rejects_cross_work_and_non_delivery_branches() {
        let delivery = work("work-1", "branch-1");

        assert_eq!(
            validate_delivery_branch_binding(&delivery, &branch("work-2", "branch-1")),
            Err(WorkDomainError::DeliveryBranchWorkMismatch)
        );
        assert_eq!(
            validate_delivery_branch_binding(&delivery, &branch("work-1", "branch-2")),
            Err(WorkDomainError::DeliveryBranchIdentityMismatch)
        );
        assert_eq!(
            validate_delivery_branch_binding(&delivery, &branch("work-1", "branch-1")),
            Ok(())
        );
    }

    #[test]
    fn fresh_work_schema_has_one_owner_and_no_legacy_task_authority() {
        let table_names = WORK_SCHEMA_TABLES
            .iter()
            .map(|(table_name, _)| *table_name)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            table_names.len(),
            WORK_SCHEMA_TABLES.len(),
            "Work schema ownership cannot contain duplicate table declarations"
        );
        for required in [
            "works",
            "work_goal_revisions",
            "work_criterion_revisions",
            "work_criterion_sets",
            "work_branches",
            "work_item_revisions",
            "work_item_edges",
            "work_graph_revisions",
            "work_check_runs",
            "work_item_attempts",
            "work_terminal_cuts",
            "work_acceptance_decisions",
            "work_events",
            "work_attention_receipts",
        ] {
            assert!(
                table_names.contains(required),
                "missing M1 owner {required}"
            );
        }
        assert!(
            table_names.is_disjoint(&BTreeSet::from([
                "session_todos",
                "plans",
                "plan_steps",
                "tasks",
                "task_contracts",
                "task_branches",
            ])),
            "legacy task/plan stores cannot enter the Work schema manifest"
        );
        for &(table_name, ddl) in WORK_SCHEMA_TABLES {
            assert!(
                ddl.starts_with("CREATE TABLE IF NOT EXISTS "),
                "{table_name} must have one canonical fresh-schema declaration"
            );
            assert!(
                ddl["CREATE TABLE IF NOT EXISTS ".len()..].starts_with(table_name),
                "Work schema entry {table_name} points at a different table"
            );
        }
    }
}
