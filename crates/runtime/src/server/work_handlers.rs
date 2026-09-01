use super::*;
use crate::server::header_utils::collect_forward_headers;
use astra_services::runs::{
    ChatRequestData, ModelSelectionMode, RunStartIdempotency, RunStartIdempotencyKind,
    WorkItemRuntimeBindingRequest, WorkRuntimeBindingRequest, WorkspaceAuthorityRequest,
    WorkspaceBindingRequest, WorkspaceBindingRequestKind,
};
use astra_services::work::{
    CriterionCommand, CriterionDefinition, CriterionId, CriterionSetRevision, CriterionStatement,
    DatabaseWorkBranchCatalogService, DatabaseWorkBranchComparisonService,
    DatabaseWorkBranchCreationService, DatabaseWorkBranchDeletionService,
    DatabaseWorkPatchCommitService, DatabaseWorkPatchMaterializationService,
    DatabaseWorkRepository, ForkCursorRef, GoalRevision, GraphRevision, InternalSessionId,
    NewWorkCriterion, OriginalIntentRef, WorkArchivedBranchCursor, WorkAttentionCursorAdvance,
    WorkAttentionCursorKind, WorkBranchId, WorkBranchRetentionChange, WorkBranchRetentionKind,
    WorkBranchRevision, WorkCatalogCursor, WorkCatalogPageLimit, WorkCatalogQuery, WorkChangeRef,
    WorkConflictResource, WorkContentHash, WorkCriteriaProposalAcceptance,
    WorkCriteriaProposalRejection, WorkCriteriaQuery, WorkEventPageLimit, WorkEventQuery,
    WorkEventSeq, WorkGenesis, WorkGenesisParts, WorkGoal, WorkId, WorkItemId, WorkItemRevision,
    WorkMaterializationProviderRef, WorkObservationQuery, WorkOwnerId, WorkPatchArtifactId,
    WorkPatchCommitId, WorkPatchCommitPageLimit, WorkPatchCommitProviderRef,
    WorkPatchMaterializationId, WorkProposalId, WorkRepository, WorkRepositoryError, WorkRevision,
    WorkSubjectRef, WorkTaskGraphQuery,
};
use axum::extract::rejection::{JsonRejection, QueryRejection};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::Row;

use astra_server_types::{
    WORK_API_MAJOR, WORK_API_MAJOR_HEADER, WorkActionRequestV1, WorkActionV1,
    WorkArchivedBranchesQueryV1, WorkBranchActionRequestV1, WorkBranchActionV1,
    WorkBranchAttachRequestV1, WorkBranchAttachResponseV1, WorkBranchAttachmentModeV1,
    WorkBranchComparisonRequestV1, WorkBranchControlBasisV1, WorkBranchControlCommandV1,
    WorkBranchControlOperationRequestV1, WorkBranchCreationRequestV1, WorkBranchDeletionRequestV1,
    WorkBranchSyncStateV1, WorkCatalogQueryV1, WorkCatalogResponseV1, WorkConversationHeadV1,
    WorkCreateCriterionV1, WorkCreateRequestV1, WorkCriteriaProposalBasisV1,
    WorkCriteriaProposalDecisionRequestV1, WorkCriteriaProposalDecisionV1,
    WorkCriteriaProposalDetailResponseV1, WorkCriteriaProposalListResponseV1,
    WorkCriteriaProposalResolutionV1, WorkCriteriaProposalSummaryV1, WorkCriteriaQueryV1,
    WorkCriteriaResponseV1, WorkEventPageResponseV1, WorkEventsQueryV1, WorkObservationResponseV1,
    WorkPatchArtifactExportRequestV1, WorkPatchArtifactsQueryV1, WorkPatchCommitRequestV1,
    WorkPatchCommitsQueryV1, WorkPatchMaterializationRequestV1, WorkReadCursorRequestV1,
    WorkReadCursorResponseV1, WorkSessionBindingResponseV1, WorkTaskGraphQueryV1,
    WorkTaskGraphResponseV1, WorkTranscriptItemV1, WorkTranscriptPageResponseV1,
    WorkTranscriptQueryV1, WorkTurnRequestV1,
};

const WORK_REQUEST_ID_MAX_BYTES: usize = 256;
const WORK_TURN_MESSAGE_MAX_BYTES: usize = 256 * 1024;
const WORK_TASK_GRAPH_DEFAULT_ITEM_LIMIT: u16 = 8;
const WORK_TASK_GRAPH_DEFAULT_DEPENDENCY_LIMIT: u16 = 128;
const WORK_CRITERIA_DEFAULT_LIMIT: u16 = 8;
const WORK_CATALOG_DEFAULT_LIMIT: u16 = 20;
const WORK_ARCHIVED_BRANCH_DEFAULT_LIMIT: u16 = 20;
const WORK_PATCH_ARTIFACT_DEFAULT_LIMIT: u16 = 20;
const WORK_PATCH_MATERIALIZATION_DEFAULT_LIMIT: u16 = 20;
const WORK_PATCH_COMMIT_DEFAULT_LIMIT: u16 = 20;
const WORK_TRANSCRIPT_DEFAULT_LIMIT: u16 = 30;
const WORK_TRANSCRIPT_MAX_LIMIT: u16 = 50;
const WORK_TRANSCRIPT_CONTENT_PREVIEW_CHARS: i64 = 2_048;
const WORK_TRANSCRIPT_PAYLOAD_MAX_BYTES: i64 = 8 * 1_024;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum WorkApiErrorCategory {
    Authentication,
    InvalidRequest,
    NotFound,
    Conflict,
    Version,
    Availability,
    Degraded,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum WorkApiActionHint {
    UpgradeClient,
    RefreshWork,
    RetryRead,
    RetryWrite,
    RetryAttach,
}

#[derive(Debug, Serialize)]
pub(super) struct WorkApiErrorV1 {
    code: &'static str,
    category: WorkApiErrorCategory,
    retryable: bool,
    action_hints: Vec<WorkApiActionHint>,
}

type WorkApiResult<T> = Result<Json<T>, (StatusCode, Json<WorkApiErrorV1>)>;

fn work_error(
    status: StatusCode,
    code: &'static str,
    category: WorkApiErrorCategory,
    retryable: bool,
    action_hints: Vec<WorkApiActionHint>,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    (
        status,
        Json(WorkApiErrorV1 {
            code,
            category,
            retryable,
            action_hints,
        }),
    )
}

fn require_work_api_major(headers: &HeaderMap) -> Result<(), (StatusCode, Json<WorkApiErrorV1>)> {
    if headers
        .get(WORK_API_MAJOR_HEADER)
        .and_then(|value| value.to_str().ok())
        == Some(WORK_API_MAJOR)
    {
        return Ok(());
    }
    Err(work_error(
        StatusCode::UPGRADE_REQUIRED,
        "unsupported_client_version",
        WorkApiErrorCategory::Version,
        false,
        vec![WorkApiActionHint::UpgradeClient],
    ))
}

fn map_auth_error(error: (StatusCode, Json<ErrorResponse>)) -> (StatusCode, Json<WorkApiErrorV1>) {
    let status = error.0;
    work_error(
        status,
        if status == StatusCode::UNAUTHORIZED {
            "authentication_required"
        } else {
            "authentication_rejected"
        },
        WorkApiErrorCategory::Authentication,
        false,
        Vec::new(),
    )
}

fn map_repository_error(
    work_id: &str,
    error: WorkRepositoryError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    match error {
        WorkRepositoryError::NotFound => work_error(
            StatusCode::NOT_FOUND,
            "work_not_found",
            WorkApiErrorCategory::NotFound,
            false,
            Vec::new(),
        ),
        WorkRepositoryError::BranchDeleting => work_error(
            StatusCode::CONFLICT,
            "work_branch_deleting",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        error @ WorkRepositoryError::Persistence { .. } => {
            tracing::warn!(work_id, error = %error, "Work read persistence failed");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_read_unavailable",
                WorkApiErrorCategory::Availability,
                true,
                vec![WorkApiActionHint::RetryRead],
            )
        }
        error => {
            tracing::error!(work_id, error = %error, "canonical Work projection is degraded");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "causal_projection_degraded",
                WorkApiErrorCategory::Degraded,
                true,
                vec![WorkApiActionHint::RetryRead],
            )
        }
    }
}

fn map_cursor_repository_error(
    work_id: &str,
    error: WorkRepositoryError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    match error {
        WorkRepositoryError::NotFound => work_error(
            StatusCode::NOT_FOUND,
            "work_not_found",
            WorkApiErrorCategory::NotFound,
            false,
            Vec::new(),
        ),
        WorkRepositoryError::EventCursorAhead { .. } => work_error(
            StatusCode::CONFLICT,
            "work_event_cursor_ahead",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        error @ WorkRepositoryError::Persistence { .. } => {
            tracing::warn!(work_id, error = %error, "Work read cursor persistence failed");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_write_unavailable",
                WorkApiErrorCategory::Availability,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )
        }
        error => {
            tracing::error!(work_id, error = %error, "canonical Work read cursor is degraded");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "causal_projection_degraded",
                WorkApiErrorCategory::Degraded,
                true,
                vec![WorkApiActionHint::RetryRead],
            )
        }
    }
}

fn map_event_page_repository_error(
    work_id: &str,
    error: WorkRepositoryError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    match error {
        WorkRepositoryError::EventCursorAhead { .. } => work_error(
            StatusCode::CONFLICT,
            "work_event_cursor_ahead",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        error => map_repository_error(work_id, error),
    }
}

fn map_task_graph_repository_error(
    work_id: &str,
    error: WorkRepositoryError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    match error {
        WorkRepositoryError::NotFound | WorkRepositoryError::Archived => work_error(
            StatusCode::NOT_FOUND,
            "branch_not_found",
            WorkApiErrorCategory::NotFound,
            false,
            Vec::new(),
        ),
        WorkRepositoryError::StaleTaskGraphRevision { .. } => work_error(
            StatusCode::CONFLICT,
            "work_graph_revision_conflict",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        WorkRepositoryError::TaskGraphCursorAhead { .. } => work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_task_graph_cursor",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        ),
        error => map_repository_error(work_id, error),
    }
}

fn map_criteria_page_repository_error(
    work_id: &str,
    error: WorkRepositoryError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    match error {
        WorkRepositoryError::StaleCriteriaPageRevision { .. } => work_error(
            StatusCode::CONFLICT,
            "work_criteria_revision_conflict",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        WorkRepositoryError::CriteriaPageCursorAhead { .. } => work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_criteria_cursor",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        ),
        error => map_repository_error(work_id, error),
    }
}

fn map_criteria_proposal_read_error(
    work_id: &str,
    error: WorkRepositoryError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    match error {
        WorkRepositoryError::NotFound => work_error(
            StatusCode::NOT_FOUND,
            "work_criteria_proposal_not_found",
            WorkApiErrorCategory::NotFound,
            false,
            Vec::new(),
        ),
        error => map_repository_error(work_id, error),
    }
}

fn map_criteria_proposal_decision_error(
    work_id: &str,
    error: WorkRepositoryError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    match error {
        WorkRepositoryError::NotFound => work_error(
            StatusCode::NOT_FOUND,
            "work_criteria_proposal_not_found",
            WorkApiErrorCategory::NotFound,
            false,
            Vec::new(),
        ),
        WorkRepositoryError::InvalidWorkProposalBasis { .. }
        | WorkRepositoryError::StaleCriteriaRevision { .. } => work_error(
            StatusCode::CONFLICT,
            "work_proposal_basis_conflict",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        WorkRepositoryError::WorkProposalAlreadyResolved { .. } => work_error(
            StatusCode::CONFLICT,
            "work_proposal_already_resolved",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        WorkRepositoryError::Conflict {
            resource: WorkConflictResource::WorkProposalIdentity,
        } => work_error(
            StatusCode::CONFLICT,
            "work_proposal_identity_conflict",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        error @ WorkRepositoryError::Persistence { .. } => {
            tracing::warn!(work_id, error = %error, "Work criteria proposal decision failed");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_write_unavailable",
                WorkApiErrorCategory::Availability,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )
        }
        error => {
            tracing::error!(work_id, error = %error, "Work criteria proposal decision degraded");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "causal_projection_degraded",
                WorkApiErrorCategory::Degraded,
                true,
                vec![WorkApiActionHint::RetryRead],
            )
        }
    }
}

async fn authenticated_work_owner(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<WorkOwnerId, (StatusCode, Json<WorkApiErrorV1>)> {
    authenticated_work_user(state, headers)
        .await
        .map(|(owner_id, _)| owner_id)
}

async fn authenticated_work_user(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(WorkOwnerId, astra_services::AuthUserRecord), (StatusCode, Json<WorkApiErrorV1>)> {
    let user = state
        .auth_service
        .current_user(headers)
        .await
        .map_err(map_auth_error)?;
    let owner_id = WorkOwnerId::parse(user.user_id.clone()).map_err(|error| {
        tracing::error!(error = %error, "authenticated owner identity violates Work contract");
        work_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "authentication_context_invalid",
            WorkApiErrorCategory::Authentication,
            false,
            Vec::new(),
        )
    })?;
    Ok((owner_id, user))
}

fn work_digest(domain: &str, fields: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(
        u64::try_from(domain.len())
            .expect("Work digest domain length fits u64")
            .to_be_bytes(),
    );
    hasher.update(domain.as_bytes());
    for field in fields {
        hasher.update(
            u64::try_from(field.len())
                .expect("bounded Work digest field length fits u64")
                .to_be_bytes(),
        );
        hasher.update(field.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn valid_work_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= WORK_REQUEST_ID_MAX_BYTES
        && !value.chars().any(char::is_control)
}

fn criteria_proposal_summary(
    recorded: &astra_services::work::RecordedWorkCriteriaProposal,
) -> WorkCriteriaProposalSummaryV1 {
    WorkCriteriaProposalSummaryV1 {
        work_id: recorded.proposal.work_id.clone(),
        branch_id: recorded.proposal.branch_id.clone(),
        proposal_id: recorded.proposal.proposal_id.clone(),
        proposal_seq: recorded.proposal_seq,
        payload_hash: recorded.payload_hash.clone(),
        status: recorded.status,
        basis: WorkCriteriaProposalBasisV1 {
            work_revision: recorded.proposal.expected_work_revision,
            goal_revision: recorded.proposal.expected_goal_revision,
            criteria_set_revision: recorded.proposal.expected_criteria_set_revision,
            branch_revision: recorded.proposal.expected_branch_revision,
            graph_revision: recorded.proposal.expected_graph_revision,
        },
        member_count: u16::try_from(recorded.proposal.members.len())
            .expect("criteria proposal member count is domain bounded"),
        source_kind: recorded.proposal.source_kind,
        proposed_at: recorded.proposed_at,
        expires_at: recorded.expires_at,
    }
}

fn criteria_proposal_detail(
    recorded: astra_services::work::RecordedWorkCriteriaProposal,
) -> WorkCriteriaProposalDetailResponseV1 {
    let summary = criteria_proposal_summary(&recorded);
    WorkCriteriaProposalDetailResponseV1 {
        schema_version: 1,
        proposal: summary,
        members: recorded.proposal.members,
        resolution: recorded
            .resolution
            .map(|resolution| WorkCriteriaProposalResolutionV1 {
                resolution_ref: resolution.resolution_ref,
                resolved_at: resolution.resolved_at,
                result_work_revision: resolution.result_work_revision,
                result_criteria_set_revision: resolution.result_criteria_set_revision,
            }),
    }
}

enum DerivedCriteriaProposalDecision {
    Accept(WorkCriteriaProposalAcceptance),
    Reject(WorkCriteriaProposalRejection),
}

fn derive_criteria_proposal_decision(
    owner_id: &WorkOwnerId,
    work_id: WorkId,
    branch_id: WorkBranchId,
    proposal_id: WorkProposalId,
    request: WorkCriteriaProposalDecisionRequestV1,
) -> Result<DerivedCriteriaProposalDecision, (StatusCode, Json<WorkApiErrorV1>)> {
    if !valid_work_request_id(&request.request_id) {
        return Err(work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_proposal_decision",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        ));
    }
    let payload_hash = WorkContentHash::parse(request.payload_hash).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_proposal_decision",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let expected_work_revision = WorkRevision::new(request.expected_work_revision);
    let expected_goal_revision = GoalRevision::new(request.expected_goal_revision);
    let expected_criteria_set_revision =
        CriterionSetRevision::new(request.expected_criteria_set_revision);
    let expected_branch_revision = WorkBranchRevision::new(request.expected_branch_revision);
    let expected_graph_revision = GraphRevision::new(request.expected_graph_revision);
    let (
        Ok(expected_work_revision),
        Ok(expected_goal_revision),
        Ok(expected_criteria_set_revision),
        Ok(expected_branch_revision),
        Ok(expected_graph_revision),
    ) = (
        expected_work_revision,
        expected_goal_revision,
        expected_criteria_set_revision,
        expected_branch_revision,
        expected_graph_revision,
    )
    else {
        return Err(work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_proposal_decision",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        ));
    };
    let decision_name = match request.decision {
        WorkCriteriaProposalDecisionV1::Accept => "accept",
        WorkCriteriaProposalDecisionV1::Reject => "reject",
    };
    let digest = work_digest(
        "work-criteria-proposal-decision-v1",
        &[
            owner_id.as_str(),
            work_id.as_str(),
            branch_id.as_str(),
            proposal_id.as_str(),
            &request.request_id,
            decision_name,
        ],
    );
    let resolution_ref = WorkChangeRef::parse(format!("criteria-decision-{}", &digest[..48]))
        .expect("digest decision ref");
    let common = || {
        (
            owner_id.clone(),
            work_id.clone(),
            branch_id.clone(),
            proposal_id.clone(),
            payload_hash.clone(),
        )
    };
    Ok(match request.decision {
        WorkCriteriaProposalDecisionV1::Accept => {
            let (owner_id, work_id, branch_id, proposal_id, payload_hash) = common();
            DerivedCriteriaProposalDecision::Accept(WorkCriteriaProposalAcceptance {
                owner_id,
                work_id,
                branch_id,
                proposal_id,
                payload_hash,
                expected_work_revision,
                expected_goal_revision,
                expected_criteria_set_revision,
                expected_branch_revision,
                expected_graph_revision,
                resolution_ref,
            })
        }
        WorkCriteriaProposalDecisionV1::Reject => {
            let (owner_id, work_id, branch_id, proposal_id, payload_hash) = common();
            DerivedCriteriaProposalDecision::Reject(WorkCriteriaProposalRejection {
                owner_id,
                work_id,
                branch_id,
                proposal_id,
                payload_hash,
                expected_work_revision,
                expected_goal_revision,
                expected_criteria_set_revision,
                expected_branch_revision,
                expected_graph_revision,
                resolution_ref,
            })
        }
    })
}

#[derive(Debug)]
pub(super) struct DerivedWorkCreation {
    pub(super) genesis: WorkGenesis,
    pub(super) work_id: WorkId,
    pub(super) branch_id: WorkBranchId,
    pub(super) original_intent_ref: OriginalIntentRef,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExistingSessionCreation {
    Missing,
    Exact,
    Mismatch,
}

async fn classify_existing_session_creation(
    repository: &DatabaseWorkRepository,
    owner_id: &WorkOwnerId,
    creation: &DerivedWorkCreation,
    session_id: &InternalSessionId,
) -> Result<ExistingSessionCreation, WorkRepositoryError> {
    match repository.load(owner_id, &creation.work_id).await {
        Ok(existing) => {
            if existing.work.parts().original_intent_ref == creation.original_intent_ref
                && existing.work.parts().delivery_branch_id == creation.branch_id
                && existing.delivery_branch.parts().session_id == *session_id
            {
                Ok(ExistingSessionCreation::Exact)
            } else {
                Ok(ExistingSessionCreation::Mismatch)
            }
        }
        Err(WorkRepositoryError::NotFound) => Ok(ExistingSessionCreation::Missing),
        Err(error) => Err(error),
    }
}

#[derive(Debug)]
struct DerivedWorkTurn {
    attachment_id: String,
    message: String,
    start_idempotency: RunStartIdempotency,
}

fn derive_work_turn(
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    branch_id: &WorkBranchId,
    request: WorkTurnRequestV1,
) -> Result<DerivedWorkTurn, (StatusCode, Json<WorkApiErrorV1>)> {
    if !valid_work_request_id(&request.request_id)
        || request.attachment_id.is_empty()
        || request.attachment_id.len() > 128
        || request.attachment_id.chars().any(char::is_control)
        || request.message.trim().is_empty()
        || request.message.len() > WORK_TURN_MESSAGE_MAX_BYTES
    {
        return Err(work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_turn_request",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        ));
    }
    let identity = work_digest(
        "work-turn-identity-v1",
        &[
            owner_id.as_str(),
            work_id.as_str(),
            branch_id.as_str(),
            &request.request_id,
        ],
    );
    let fingerprint = work_digest(
        "work-turn-payload-v1",
        &[
            owner_id.as_str(),
            work_id.as_str(),
            branch_id.as_str(),
            &request.request_id,
            &request.message,
        ],
    );
    let start_idempotency = RunStartIdempotency::new(
        RunStartIdempotencyKind::WorkTurn,
        format!("run-{}", &identity[..48]),
        fingerprint,
    )
    .expect("SHA-256-derived Work turn identity satisfies the run contract");
    Ok(DerivedWorkTurn {
        attachment_id: request.attachment_id,
        message: request.message,
        start_idempotency,
    })
}

fn map_branch_repository_error(
    work_id: &str,
    error: WorkRepositoryError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    match error {
        WorkRepositoryError::NotFound => work_error(
            StatusCode::NOT_FOUND,
            "branch_not_found",
            WorkApiErrorCategory::NotFound,
            false,
            Vec::new(),
        ),
        error @ WorkRepositoryError::Persistence { .. } => {
            tracing::warn!(work_id, error = %error, "Work branch binding persistence failed");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_write_unavailable",
                WorkApiErrorCategory::Availability,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )
        }
        error => {
            tracing::error!(work_id, error = %error, "canonical Work branch binding is degraded");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "causal_projection_degraded",
                WorkApiErrorCategory::Degraded,
                true,
                vec![WorkApiActionHint::RetryRead],
            )
        }
    }
}

fn map_turn_start_error(
    work_id: &str,
    error: (StatusCode, Json<ErrorResponse>),
) -> (StatusCode, Json<WorkApiErrorV1>) {
    let status = error.0;
    let domain_code = error.1.0.error_code.as_deref();
    match domain_code {
        Some("idempotency_mismatch") => work_error(
            StatusCode::CONFLICT,
            "idempotency_mismatch",
            WorkApiErrorCategory::Conflict,
            false,
            Vec::new(),
        ),
        Some("work_binding_not_found") => work_error(
            StatusCode::NOT_FOUND,
            "branch_not_found",
            WorkApiErrorCategory::NotFound,
            false,
            Vec::new(),
        ),
        Some("work_item_binding_not_found") => work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "causal_projection_degraded",
            WorkApiErrorCategory::Degraded,
            true,
            vec![WorkApiActionHint::RetryRead],
        ),
        Some(
            "session_writer_conflict" | "session_turn_conflict" | "session_execution_conflict",
        ) => work_error(
            StatusCode::CONFLICT,
            "writer_conflict",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        Some(
            "model_default_unavailable"
            | "model_catalog_unavailable"
            | "model_offering_not_found"
            | "model_offering_unavailable"
            | "model_execution_configuration_invalid",
        ) => work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "provider_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        ),
        _ if status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY => {
            tracing::warn!(
                work_id,
                ?domain_code,
                "Work turn admission rejected the server-owned request"
            );
            work_error(
                StatusCode::BAD_REQUEST,
                "work_turn_rejected",
                WorkApiErrorCategory::InvalidRequest,
                false,
                Vec::new(),
            )
        }
        _ => {
            tracing::warn!(work_id, %status, ?domain_code, "Work turn start failed");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_turn_unavailable",
                WorkApiErrorCategory::Availability,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )
        }
    }
}

fn contains_structural_field(value: &serde_json::Value, field: &str) -> bool {
    match value {
        serde_json::Value::Object(object) => {
            object.contains_key(field)
                || object
                    .values()
                    .any(|child| contains_structural_field(child, field))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .any(|child| contains_structural_field(child, field)),
        _ => false,
    }
}

fn project_work_turn_events(
    run_id: &str,
    events: Vec<serde_json::Value>,
    pending_run_error: &mut Option<String>,
) -> Vec<serde_json::Value> {
    let runtime_events =
        crate::server::run::handlers::transform_stream_run_events_for_client_with_pending(
            run_id,
            events,
            pending_run_error,
        );
    let mut projected = Vec::with_capacity(runtime_events.len());
    for mut event in runtime_events {
        if event.get("type").and_then(serde_json::Value::as_str) == Some("session_info") {
            continue;
        }
        if let Some(object) = event.as_object_mut() {
            object.remove("session_id");
        }
        if contains_structural_field(&event, "session_id") {
            tracing::warn!(
                run_id,
                "runtime event with nested session identity was excluded from Work SSE"
            );
            continue;
        }
        let graph_change = committed_task_graph_change_event(&event);
        projected.push(event);
        if let Some(graph_change) = graph_change {
            projected.push(graph_change);
        }
    }
    projected
}

/// Project a public invalidation only after the canonical graph transaction
/// has committed. The signal is derived from the typed server-tool identity
/// and its exact accepted-result contract; model prose is never inspected.
fn committed_task_graph_change_event(event: &serde_json::Value) -> Option<serde_json::Value> {
    if event.get("type").and_then(serde_json::Value::as_str) != Some("tool_call_end")
        || event.get("tool").and_then(serde_json::Value::as_str) != Some("propose_work_plan")
        || event.get("success").and_then(serde_json::Value::as_bool) == Some(false)
    {
        return None;
    }
    let result = event.get("result")?.as_str()?;
    let result: serde_json::Value = serde_json::from_str(result).ok()?;
    let result = result.as_object()?;
    if result.get("status").and_then(serde_json::Value::as_str) != Some("accepted") {
        return None;
    }
    let graph_revision = result
        .get("result_graph_revision")
        .and_then(serde_json::Value::as_u64)
        .filter(|revision| *revision > 0 && *revision <= i64::MAX as u64)?;
    let branch_revision = result
        .get("result_branch_revision")
        .and_then(serde_json::Value::as_u64)
        .filter(|revision| *revision > 0 && *revision <= i64::MAX as u64)?;
    Some(serde_json::json!({
        "type": "work_task_graph_changed",
        "schema_version": 1,
        "graph_revision": graph_revision,
        "branch_revision": branch_revision,
    }))
}

pub(super) fn derive_work_creation(
    owner_id: &WorkOwnerId,
    request: WorkCreateRequestV1,
) -> Result<DerivedWorkCreation, (StatusCode, Json<WorkApiErrorV1>)> {
    if !valid_work_request_id(&request.request_id) {
        return Err(work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_create_request",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        ));
    }
    let goal = WorkGoal::parse(request.goal).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_goal",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let criteria = request
        .criteria
        .into_iter()
        .map(|criterion| {
            let (criterion_id, definition) = match criterion {
                WorkCreateCriterionV1::CommandCheck {
                    criterion_id,
                    statement,
                    command,
                } => (
                    CriterionId::parse(criterion_id)?,
                    CriterionDefinition::CommandCheck {
                        statement: CriterionStatement::parse(statement)?,
                        command: CriterionCommand::parse(command)?,
                    },
                ),
                WorkCreateCriterionV1::TestCheck {
                    criterion_id,
                    statement,
                    command,
                } => (
                    CriterionId::parse(criterion_id)?,
                    CriterionDefinition::TestCheck {
                        statement: CriterionStatement::parse(statement)?,
                        command: CriterionCommand::parse(command)?,
                    },
                ),
                WorkCreateCriterionV1::HumanReview {
                    criterion_id,
                    statement,
                } => (
                    CriterionId::parse(criterion_id)?,
                    CriterionDefinition::HumanReview {
                        statement: CriterionStatement::parse(statement)?,
                    },
                ),
            };
            Ok(NewWorkCriterion {
                criterion_id,
                definition,
            })
        })
        .collect::<Result<Vec<_>, astra_services::work::WorkDomainError>>()
        .map_err(|_| {
            work_error(
                StatusCode::BAD_REQUEST,
                "invalid_work_criteria",
                WorkApiErrorCategory::InvalidRequest,
                false,
                Vec::new(),
            )
        })?;
    let criteria = NewWorkCriterion::canonicalize_set(criteria).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_criteria",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let canonical_criteria = serde_json::to_string(&criteria)
        .expect("typed Work criteria have an infallible canonical representation");
    let identity = work_digest(
        "work-create-identity-v1",
        &[owner_id.as_str(), &request.request_id],
    );
    let intent = work_digest(
        "work-create-payload-v1",
        &[
            owner_id.as_str(),
            &request.request_id,
            goal.as_str(),
            &canonical_criteria,
        ],
    );
    let work_id = WorkId::parse(format!("work-{}", &identity[..48])).expect("digest Work id");
    let branch_digest = work_digest("work-create-branch-v1", &[&identity]);
    let branch_id =
        WorkBranchId::parse(format!("branch-{}", &branch_digest[..48])).expect("digest branch id");
    let session_digest = work_digest("work-create-session-v1", &[&identity]);
    let session_id = InternalSessionId::parse(format!("session-{}", &session_digest[..48]))
        .expect("digest session id");
    let original_intent_ref =
        OriginalIntentRef::parse(format!("work-create-{intent}")).expect("digest intent ref");
    let genesis = WorkGenesis::new(WorkGenesisParts {
        owner_id: owner_id.clone(),
        work_id: work_id.clone(),
        branch_id: branch_id.clone(),
        session_id,
        project_id: None,
        original_intent_ref: original_intent_ref.clone(),
        goal,
        criteria,
    })
    .map_err(|error| {
        let code = if matches!(
            error,
            astra_services::work::WorkDomainError::InvalidWorkItemText { .. }
        ) {
            "invalid_work_goal"
        } else {
            "invalid_work_criteria"
        };
        work_error(
            StatusCode::BAD_REQUEST,
            code,
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    Ok(DerivedWorkCreation {
        genesis,
        work_id,
        branch_id,
        original_intent_ref,
    })
}

fn server_owned_work_workspace_binding() -> WorkspaceBindingRequest {
    WorkspaceBindingRequest {
        kind: WorkspaceBindingRequestKind::ServerSandbox,
        display_name: Some("Work workspace".to_string()),
        root: None,
        source: None,
        authority: Some(WorkspaceAuthorityRequest::ReadWrite),
    }
}

fn map_create_repository_error(
    work_id: &str,
    error: WorkRepositoryError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    match error {
        WorkRepositoryError::Conflict { .. } => work_error(
            StatusCode::CONFLICT,
            "work_creation_conflict",
            WorkApiErrorCategory::Conflict,
            false,
            Vec::new(),
        ),
        error @ WorkRepositoryError::Persistence { .. } => {
            tracing::warn!(work_id, error = %error, "Work creation persistence failed");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_write_unavailable",
                WorkApiErrorCategory::Availability,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )
        }
        error => {
            tracing::error!(work_id, error = %error, "canonical Work creation is degraded");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "causal_projection_degraded",
                WorkApiErrorCategory::Degraded,
                true,
                vec![WorkApiActionHint::RetryRead],
            )
        }
    }
}

/// Owner-scoped, bounded Work catalog. Pagination follows immutable creation
/// order, so concurrent Work updates cannot move entries across page boundaries.
pub(super) async fn get_works_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<WorkCatalogQueryV1>, QueryRejection>,
) -> WorkApiResult<WorkCatalogResponseV1> {
    require_work_api_major(&headers)?;
    let Query(query) = query.map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_catalog_query",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let before = match (query.before_created_at, query.before_work_id) {
        (None, None) => None,
        (Some(created_at), Some(work_id)) => Some(WorkCatalogCursor {
            created_at,
            work_id: WorkId::parse(work_id).map_err(|_| {
                work_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_work_catalog_cursor",
                    WorkApiErrorCategory::InvalidRequest,
                    false,
                    Vec::new(),
                )
            })?,
        }),
        _ => {
            return Err(work_error(
                StatusCode::BAD_REQUEST,
                "invalid_work_catalog_cursor",
                WorkApiErrorCategory::InvalidRequest,
                false,
                Vec::new(),
            ));
        }
    };
    let limit = WorkCatalogPageLimit::new(query.limit.unwrap_or(WORK_CATALOG_DEFAULT_LIMIT))
        .map_err(|_| {
            work_error(
                StatusCode::BAD_REQUEST,
                "invalid_work_catalog_limit",
                WorkApiErrorCategory::InvalidRequest,
                false,
                Vec::new(),
            )
        })?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let repository = DatabaseWorkRepository::new(pool);
    let page = repository
        .list_catalog(WorkCatalogQuery {
            owner_id,
            before,
            limit,
        })
        .await
        .map_err(|error| map_repository_error("catalog", error))?;
    Ok(Json(WorkCatalogResponseV1 {
        schema_version: 1,
        page,
    }))
}

/// Resolve a session the caller already knows to the public Work identity
/// that owns it. This is a constant-size bootstrap projection for CLI/Web
/// surfaces; it neither exposes the opaque session in the response nor scans
/// the Work catalog client-side.
pub(super) async fn get_work_session_binding_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> WorkApiResult<WorkSessionBindingResponseV1> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let session_id = InternalSessionId::parse(session_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_session_binding",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let binding = DatabaseWorkRepository::new(pool)
        .load_session_plan_binding(&owner_id, &session_id)
        .await
        .map_err(|error| match error {
            WorkRepositoryError::NotFound | WorkRepositoryError::Archived => work_error(
                StatusCode::NOT_FOUND,
                "work_session_binding_not_found",
                WorkApiErrorCategory::NotFound,
                false,
                Vec::new(),
            ),
            other => map_repository_error("session-binding", other),
        })?;
    Ok(Json(WorkSessionBindingResponseV1 {
        schema_version: 1,
        work_id: binding.work_id.as_str().to_owned(),
        branch_id: binding.branch_id.as_str().to_owned(),
        graph_revision: binding.graph_revision.get(),
    }))
}

/// Promote an existing active conversation to canonical Work without moving
/// its transcript to a second, hidden session. The repository serializes this
/// boundary with run admission on the session row and rejects active runs.
pub(super) async fn post_work_session_binding_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    payload: Result<Json<WorkCreateRequestV1>, JsonRejection>,
) -> Result<(StatusCode, Json<WorkObservationResponseV1>), (StatusCode, Json<WorkApiErrorV1>)> {
    require_work_api_major(&headers)?;
    let Json(payload) = payload.map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_create_request",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let session_id = InternalSessionId::parse(session_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_session_binding",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let mut creation = derive_work_creation(&owner_id, payload)?;
    creation.genesis = creation.genesis.in_session(session_id.clone());
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let repository = DatabaseWorkRepository::new(pool);
    if let Err(error) = repository
        .create_genesis_in_existing_session(creation.genesis.clone())
        .await
    {
        match classify_existing_session_creation(&repository, &owner_id, &creation, &session_id)
            .await
            .map_err(|load_error| {
                map_create_repository_error(creation.work_id.as_str(), load_error)
            })? {
            ExistingSessionCreation::Exact => {}
            ExistingSessionCreation::Mismatch => {
                return Err(work_error(
                    StatusCode::CONFLICT,
                    "idempotency_mismatch",
                    WorkApiErrorCategory::Conflict,
                    false,
                    Vec::new(),
                ));
            }
            ExistingSessionCreation::Missing => match error {
                WorkRepositoryError::Conflict {
                    resource: WorkConflictResource::BranchSessionBinding,
                } => {
                    return Err(work_error(
                        StatusCode::CONFLICT,
                        "work_session_already_bound",
                        WorkApiErrorCategory::Conflict,
                        false,
                        Vec::new(),
                    ));
                }
                WorkRepositoryError::SessionBusy => {
                    return Err(work_error(
                        StatusCode::CONFLICT,
                        "work_session_busy",
                        WorkApiErrorCategory::Conflict,
                        true,
                        vec![WorkApiActionHint::RetryWrite],
                    ));
                }
                WorkRepositoryError::SessionNotBindable => {
                    return Err(work_error(
                        StatusCode::NOT_FOUND,
                        "work_session_not_bindable",
                        WorkApiErrorCategory::NotFound,
                        false,
                        Vec::new(),
                    ));
                }
                error => {
                    return Err(map_create_repository_error(
                        creation.work_id.as_str(),
                        error,
                    ));
                }
            },
        }
    }
    let report = repository
        .observe_declared_work(WorkObservationQuery {
            owner_id,
            work_id: creation.work_id.clone(),
        })
        .await
        .map_err(|error| map_repository_error(creation.work_id.as_str(), error))?;
    Ok((StatusCode::CREATED, Json(WorkObservationResponseV1(report))))
}

/// Idempotent public Start Work boundary. Internal session identity is derived
/// and retained server-side; the response is the same bounded public
/// observation used by later reads.
pub(super) async fn post_work_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<WorkCreateRequestV1>, JsonRejection>,
) -> Result<(StatusCode, Json<WorkObservationResponseV1>), (StatusCode, Json<WorkApiErrorV1>)> {
    require_work_api_major(&headers)?;
    let Json(payload) = payload.map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_create_request",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let creation = derive_work_creation(&owner_id, payload)?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let repository = DatabaseWorkRepository::new(pool);
    match repository.create_genesis(creation.genesis).await {
        Ok(_) => {}
        Err(WorkRepositoryError::Conflict {
            resource: WorkConflictResource::WorkIdentity,
        }) => {
            let existing = repository
                .load(&owner_id, &creation.work_id)
                .await
                .map_err(|error| map_create_repository_error(creation.work_id.as_str(), error))?;
            if existing.work.parts().original_intent_ref != creation.original_intent_ref
                || existing.work.parts().delivery_branch_id != creation.branch_id
            {
                return Err(work_error(
                    StatusCode::CONFLICT,
                    "idempotency_mismatch",
                    WorkApiErrorCategory::Conflict,
                    false,
                    Vec::new(),
                ));
            }
        }
        Err(error) => {
            return Err(map_create_repository_error(
                creation.work_id.as_str(),
                error,
            ));
        }
    }
    let report = repository
        .observe_declared_work(WorkObservationQuery {
            owner_id,
            work_id: creation.work_id.clone(),
        })
        .await
        .map_err(|error| map_repository_error(creation.work_id.as_str(), error))?;
    Ok((StatusCode::CREATED, Json(WorkObservationResponseV1(report))))
}

/// Establish a durable read-only attachment to the current Work branch head.
/// The public response projects the canonical coordinates without exposing
/// the internal conversation identity or transferring writer authority.
pub(super) async fn post_work_branch_attachment_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id)): Path<(String, String)>,
    payload: Result<Json<WorkBranchAttachRequestV1>, JsonRejection>,
) -> WorkApiResult<WorkBranchAttachResponseV1> {
    require_work_api_major(&headers)?;
    let Json(payload) = payload.map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_attachment_request",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    if !valid_work_request_id(&payload.request_id) {
        return Err(work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_attachment_request",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        ));
    }
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let branch_id = WorkBranchId::parse(branch_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_branch_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let pool = state.shared_pool.clone().ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_attach_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryAttach],
        )
    })?;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let binding = repository
        .load_branch_runtime_binding(&owner_id, &work_id, &branch_id)
        .await
        .map_err(|error| map_branch_repository_error(work_id.as_str(), error))?;
    let coordinator = state.session_context_coordinator.clone().ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_attach_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryAttach],
        )
    })?;
    let key = astra_turn_types::SessionKeyV1::owner_session(
        "server",
        owner_id.as_str(),
        binding.session_id.as_str(),
        astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
    );
    let head = coordinator
        .load_head(&key)
        .await
        .map_err(map_work_attachment_coordinator_error)?;
    let authority_epochs = coordinator
        .load_authority_epochs(&key)
        .await
        .map_err(map_work_attachment_coordinator_error)?
        .unwrap_or_default();
    let actor = astra_turn_types::ActorContextV1::owner_user(
        owner_id.as_str(),
        state.session_actor_id.clone(),
        astra_turn_types::ActorKindV1::Server,
        astra_turn_types::SessionSurfaceV1::Server,
        None,
        authority_epochs,
    );
    let service = astra_services::DatabaseSessionHandoffService::new(pool, coordinator);
    let outcome = service
        .attach_read_only(
            &astra_services::AttachSessionRequestV1 {
                idempotency_key: payload.request_id,
                key: key.clone(),
                actor,
                placement: astra_turn_types::SessionPlacementV1::Server,
                after_manifest_root: head.as_ref().map(|head| head.latest_manifest_root.clone()),
                workspace: None,
            },
            std::time::Duration::from_secs(15 * 60),
        )
        .await
        .map_err(map_work_attachment_error)?;
    let control_basis = service
        .load_controller_basis(&key)
        .await
        .map_err(map_work_attachment_error)?;
    let attachment = outcome.attachment;
    let head = attachment.observed_cursor.map(work_conversation_head);
    let attached_at = chrono::DateTime::from_timestamp_millis(attachment.attached_at_unix_ms)
        .ok_or_else(work_attachment_projection_degraded)?;
    let expires_at = chrono::DateTime::from_timestamp_millis(attachment.expires_at_unix_ms)
        .ok_or_else(work_attachment_projection_degraded)?;
    Ok(Json(WorkBranchAttachResponseV1 {
        schema_version: 1,
        work_id,
        branch_id,
        attachment_id: attachment.attachment_id,
        attachment_epoch: attachment.attachment_epoch,
        branch_revision: binding.branch_revision,
        mode: match attachment.mode {
            astra_turn_types::SessionAttachmentModeV1::ReadOnly => {
                WorkBranchAttachmentModeV1::ReadOnly
            }
            astra_turn_types::SessionAttachmentModeV1::Controller => {
                WorkBranchAttachmentModeV1::Controller
            }
        },
        sync: WorkBranchSyncStateV1::Current,
        control_basis: WorkBranchControlBasisV1 {
            writer_epoch: control_basis.writer_epoch,
            canonical_root_hash: control_basis.canonical_root_hash,
        },
        head,
        attached_at,
        expires_at,
    }))
}

fn work_conversation_head(cursor: astra_turn_types::SessionCursorV1) -> WorkConversationHeadV1 {
    WorkConversationHeadV1 {
        completed_turn: cursor.completed_turn,
        journal_event_seq: cursor.journal_event_seq,
        conversation_seq: cursor.conversation_seq,
        canonical_root_hash: cursor.canonical_root_hash,
        projection_schema: cursor.projection_schema,
        compaction_generation: cursor.compaction_generation,
        config_version_id: cursor.config_version_id,
    }
}

fn work_transcript_projection_degraded(
    work_id: &str,
    error: impl std::fmt::Display,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    tracing::warn!(work_id, error = %error, "Work transcript projection is unavailable");
    work_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "causal_projection_degraded",
        WorkApiErrorCategory::Degraded,
        true,
        vec![WorkApiActionHint::RetryRead],
    )
}

fn transcript_row_u64(row: &sqlx::mysql::MySqlRow, field: &str) -> Result<u64, String> {
    let value = row
        .try_get::<i64, _>(field)
        .map_err(|error| error.to_string())?;
    u64::try_from(value).map_err(|_| format!("{field} is negative"))
}

fn decode_work_transcript_head(
    row: &sqlx::mysql::MySqlRow,
) -> Result<WorkConversationHeadV1, String> {
    let completed_turn = u32::try_from(transcript_row_u64(row, "completed_turn")?)
        .map_err(|_| "completed_turn exceeds u32".to_string())?;
    let projection_schema = u32::try_from(transcript_row_u64(row, "projection_schema")?)
        .map_err(|_| "projection_schema exceeds u32".to_string())?;
    let canonical_root_hash = row
        .try_get::<String, _>("canonical_root_hash")
        .map_err(|error| error.to_string())?;
    if completed_turn == 0
        || projection_schema == 0
        || canonical_root_hash.len() != 64
        || !canonical_root_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("transcript cursor is structurally invalid".to_string());
    }
    Ok(WorkConversationHeadV1 {
        completed_turn,
        journal_event_seq: transcript_row_u64(row, "journal_event_seq")?,
        conversation_seq: transcript_row_u64(row, "conversation_seq")?,
        canonical_root_hash,
        projection_schema,
        compaction_generation: transcript_row_u64(row, "compaction_generation")?,
        config_version_id: row
            .try_get::<Option<String>, _>("config_version_id")
            .map_err(|error| error.to_string())?,
    })
}

fn classify_work_transcript_sync(
    canonical: Option<&WorkConversationHeadV1>,
    projected: Option<&WorkConversationHeadV1>,
) -> WorkBranchSyncStateV1 {
    match (canonical, projected) {
        (None, None) => WorkBranchSyncStateV1::Current,
        (Some(_), None) => WorkBranchSyncStateV1::ProjectionStale,
        (None, Some(_)) => WorkBranchSyncStateV1::Corrupt,
        (Some(canonical), Some(projected)) if canonical == projected => {
            WorkBranchSyncStateV1::Current
        }
        (Some(canonical), Some(projected))
            if projected.completed_turn < canonical.completed_turn
                && projected.journal_event_seq <= canonical.journal_event_seq
                && projected.conversation_seq <= canonical.conversation_seq
                && projected.compaction_generation <= canonical.compaction_generation =>
        {
            WorkBranchSyncStateV1::ProjectionStale
        }
        (Some(_), Some(_)) => WorkBranchSyncStateV1::Corrupt,
    }
}

fn decode_work_transcript_item(row: sqlx::mysql::MySqlRow) -> Result<WorkTranscriptItemV1, String> {
    let item_seq = transcript_row_u64(&row, "item_seq")?;
    let committed_turn = u32::try_from(transcript_row_u64(&row, "canonical_completed_turn")?)
        .map_err(|_| "canonical_completed_turn exceeds u32".to_string())?;
    if item_seq == 0 || committed_turn == 0 {
        return Err("transcript item cursor is structurally invalid".to_string());
    }
    let payload = row
        .try_get::<Option<String>, _>("payload_json")
        .map_err(|error| error.to_string())?
        .map(|payload| serde_json::from_str(&payload).map_err(|error| error.to_string()))
        .transpose()?;
    let content_truncated = row
        .try_get::<i64, _>("content_truncated")
        .map_err(|error| error.to_string())?
        != 0;
    let payload_omitted = row
        .try_get::<i64, _>("payload_omitted")
        .map_err(|error| error.to_string())?
        != 0;
    let created_at = row
        .try_get::<String, _>("created_at")
        .map_err(|error| error.to_string())?;
    let created_at = chrono::DateTime::parse_from_rfc3339(&created_at)
        .map_err(|error| error.to_string())?
        .with_timezone(&chrono::Utc);
    Ok(WorkTranscriptItemV1 {
        item_seq,
        committed_turn,
        role: row.try_get("role").map_err(|error| error.to_string())?,
        content: row.try_get("content").map_err(|error| error.to_string())?,
        content_truncated,
        payload,
        payload_omitted,
        content_hash: row
            .try_get("content_hash")
            .map_err(|error| error.to_string())?,
        created_at,
    })
}

/// Return one bounded chronological page from the transcript prefix known to
/// be complete at `transcript_cursor`. Rows from an active/uncommitted turn
/// never enter this view; their progress remains available through activity.
pub(super) async fn get_work_branch_transcript_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id)): Path<(String, String)>,
    query: Result<Query<WorkTranscriptQueryV1>, QueryRejection>,
) -> WorkApiResult<WorkTranscriptPageResponseV1> {
    require_work_api_major(&headers)?;
    let Query(query) = query.map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_transcript_query",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let limit = query.limit.unwrap_or(WORK_TRANSCRIPT_DEFAULT_LIMIT);
    if !(1..=WORK_TRANSCRIPT_MAX_LIMIT).contains(&limit) || query.before_item_seq == Some(0) {
        return Err(work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_transcript_query",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        ));
    }
    let before_item_seq = query.before_item_seq.unwrap_or(u64::MAX);
    let before_item_seq = i64::try_from(before_item_seq).unwrap_or(i64::MAX);
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let branch_id = WorkBranchId::parse(branch_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_branch_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let pool = state.shared_pool.clone().ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let binding = DatabaseWorkRepository::new(pool.clone())
        .load_branch_runtime_binding(&owner_id, &work_id, &branch_id)
        .await
        .map_err(|error| map_branch_repository_error(work_id.as_str(), error))?;
    let coordinator = state.session_context_coordinator.clone().ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let key = astra_turn_types::SessionKeyV1::owner_session(
        "server",
        owner_id.as_str(),
        binding.session_id.as_str(),
        astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
    );
    let mut tx = pool
        .get()
        .begin()
        .await
        .map_err(|error| work_transcript_projection_degraded(work_id.as_str(), error))?;
    let projection_row = sqlx::query(
        "SELECT completed_turn, journal_event_seq, conversation_seq,
                canonical_root_hash, projection_schema, compaction_generation,
                config_version_id
         FROM session_transcript_projection_heads
         WHERE user_id = ? AND session_id = ?",
    )
    .bind(owner_id.as_str())
    .bind(binding.session_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| work_transcript_projection_degraded(work_id.as_str(), error))?;
    let transcript_cursor = projection_row
        .as_ref()
        .map(decode_work_transcript_head)
        .transpose()
        .map_err(|error| work_transcript_projection_degraded(work_id.as_str(), error))?;
    let mut items = Vec::new();
    let mut has_more = false;
    if let Some(cursor) = transcript_cursor.as_ref() {
        let rows = sqlx::query(
            "SELECT transcript.item_seq, transcript.canonical_completed_turn,
                    transcript.role,
                    LEFT(transcript.content, ?) AS content,
                    IF(CHAR_LENGTH(transcript.content) > ?, 1, 0) AS content_truncated,
                    CASE WHEN OCTET_LENGTH(transcript.payload_json) <= ?
                         THEN transcript.payload_json ELSE NULL END AS payload_json,
                    IF(COALESCE(OCTET_LENGTH(transcript.payload_json), 0) > ?, 1, 0)
                        AS payload_omitted,
                    transcript.content_hash,
                    DATE_FORMAT(transcript.created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at
             FROM session_transcript_items AS transcript
             WHERE transcript.user_id = ? AND transcript.session_id = ?
               AND transcript.canonical_completed_turn IS NOT NULL
               AND transcript.canonical_completed_turn <= ?
               AND transcript.item_seq < ?
             ORDER BY transcript.item_seq DESC
             LIMIT ?",
        )
        .bind(WORK_TRANSCRIPT_CONTENT_PREVIEW_CHARS)
        .bind(WORK_TRANSCRIPT_CONTENT_PREVIEW_CHARS)
        .bind(WORK_TRANSCRIPT_PAYLOAD_MAX_BYTES)
        .bind(WORK_TRANSCRIPT_PAYLOAD_MAX_BYTES)
        .bind(owner_id.as_str())
        .bind(binding.session_id.as_str())
        .bind(i64::from(cursor.completed_turn))
        .bind(before_item_seq)
        .bind(i64::from(limit) + 1)
        .fetch_all(&mut *tx)
        .await
        .map_err(|error| work_transcript_projection_degraded(work_id.as_str(), error))?;
        has_more = rows.len() > usize::from(limit);
        items = rows
            .into_iter()
            .take(usize::from(limit))
            .map(decode_work_transcript_item)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| work_transcript_projection_degraded(work_id.as_str(), error))?;
        items.reverse();
    }
    tx.commit()
        .await
        .map_err(|error| work_transcript_projection_degraded(work_id.as_str(), error))?;
    // Read canonical authority after the projection snapshot. Projection
    // promotion is ordered after canonical commit, so this ordering cannot
    // falsely label a healthy concurrent promotion as "ahead/corrupt".
    let canonical_head = coordinator
        .load_head(&key)
        .await
        .map_err(map_work_attachment_coordinator_error)?
        .map(|head| work_conversation_head(head.cursor));
    let sync = classify_work_transcript_sync(canonical_head.as_ref(), transcript_cursor.as_ref());
    if sync == WorkBranchSyncStateV1::Corrupt {
        items.clear();
        has_more = false;
    }
    let next_before_item_seq = has_more
        .then(|| items.first().map(|item| item.item_seq))
        .flatten();
    Ok(Json(WorkTranscriptPageResponseV1 {
        schema_version: 1,
        work_id,
        branch_id,
        sync,
        canonical_head,
        transcript_cursor,
        items,
        next_before_item_seq,
        has_more,
    }))
}

pub(super) async fn post_work_branch_fork_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, origin_branch_id)): Path<(String, String)>,
    payload: Result<Json<WorkBranchCreationRequestV1>, JsonRejection>,
) -> Result<
    (
        StatusCode,
        Json<astra_services::work::WorkBranchCreationOperation>,
    ),
    (StatusCode, Json<WorkApiErrorV1>),
> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_work_fork_request())?;
    let origin_branch_id =
        WorkBranchId::parse(origin_branch_id).map_err(|_| invalid_work_fork_request())?;
    let payload = payload.map_err(|_| invalid_work_fork_request())?.0;
    let expected_branch_revision = WorkBranchRevision::new(payload.expected_branch_revision)
        .map_err(|_| invalid_work_fork_request())?;
    if !valid_committed_work_cursor(&payload.committed_cursor) {
        return Err(invalid_work_fork_request());
    }
    let encoded_cursor = serde_json::to_vec(&payload.committed_cursor).map_err(|_| {
        work_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "work_fork_unavailable",
            WorkApiErrorCategory::Degraded,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let fork_cursor =
        ForkCursorRef::parse(format!("sha256:{}", hex_sha256(encoded_cursor.as_slice())))
            .map_err(|_| invalid_work_fork_request())?;
    let pool = state.shared_pool.clone().ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let request = astra_services::work::WorkBranchCreationRequest {
        request_id: payload.request_id,
        owner_id: owner_id.clone(),
        work_id: work_id.clone(),
        origin_branch_id: origin_branch_id.clone(),
        expected_branch_revision,
        fork_cursor,
    };
    let service = DatabaseWorkBranchCreationService::new(pool);
    let mut admission = service
        .admit(&request)
        .await
        .map_err(map_work_branch_creation_error)?;
    if admission.operation.state != astra_services::work::WorkBranchCreationState::Pending {
        return Ok((StatusCode::CREATED, Json(admission.operation)));
    }
    let Some(executor_token) = service
        .claim_execution(
            &owner_id,
            &work_id,
            &origin_branch_id,
            &admission.operation.operation_id,
        )
        .await
        .map_err(map_work_branch_creation_error)?
    else {
        let latest = service
            .load(
                &owner_id,
                &work_id,
                &origin_branch_id,
                &admission.operation.operation_id,
            )
            .await
            .map_err(map_work_branch_creation_error)?
            .operation;
        let status = if latest.state == astra_services::work::WorkBranchCreationState::Pending {
            StatusCode::ACCEPTED
        } else {
            StatusCode::CREATED
        };
        return Ok((status, Json(latest)));
    };
    let execution = async {
        let parent_key = astra_turn_types::SessionKeyV1::owner_session(
            "server",
            owner_id.as_str(),
            admission.origin_session_id.as_str(),
            astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
        );
        let child_key = astra_turn_types::SessionKeyV1::owner_session(
            "server",
            owner_id.as_str(),
            admission.child_session_id.as_str(),
            astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
        );
        let committed_cursor = astra_turn_types::SessionCursorV1 {
            schema_version: astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION,
            owner_id: owner_id.as_str().to_owned(),
            session_id: admission.origin_session_id.as_str().to_owned(),
            branch_id: astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID.to_owned(),
            completed_turn: payload.committed_cursor.completed_turn,
            journal_event_seq: payload.committed_cursor.journal_event_seq,
            conversation_seq: payload.committed_cursor.conversation_seq,
            canonical_root_hash: payload.committed_cursor.canonical_root_hash,
            projection_schema: payload.committed_cursor.projection_schema,
            compaction_generation: payload.committed_cursor.compaction_generation,
            config_version_id: payload.committed_cursor.config_version_id,
        };
        let dimensions = [
            astra_turn_types::ForkBasisDimensionV1::Conversation,
            astra_turn_types::ForkBasisDimensionV1::TaskBoard,
            astra_turn_types::ForkBasisDimensionV1::Checkpoint,
            astra_turn_types::ForkBasisDimensionV1::Workspace,
            astra_turn_types::ForkBasisDimensionV1::Artifacts,
        ]
        .into_iter()
        .map(|dimension| {
            let conversation = dimension == astra_turn_types::ForkBasisDimensionV1::Conversation;
            astra_turn_types::ForkDimensionEvidenceV1 {
                dimension,
                disposition: if conversation {
                    astra_turn_types::ForkDimensionDispositionV1::SharedPrefix
                } else {
                    astra_turn_types::ForkDimensionDispositionV1::Gap
                },
                source_cursor: conversation.then(|| committed_cursor.clone()),
                evidence_digest: conversation.then(|| committed_cursor.canonical_root_hash.clone()),
                detail: (!conversation).then(|| {
                    "this state dimension requires explicit materialization on the child branch"
                        .into()
                }),
            }
        })
        .collect();
        let fork_coordinator = state.session_fork_coordinator.as_ref().ok_or_else(|| {
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_fork_unavailable",
                WorkApiErrorCategory::Availability,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )
        })?;
        let manifest = if let Some(fork_id) = admission.session_fork_id.as_deref() {
            fork_coordinator
                .load(&parent_key, fork_id)
                .await
                .map_err(map_work_session_fork_error)?
        } else {
            let prepared = fork_coordinator
                .prepare(&astra_services::PrepareSessionForkV1 {
                    idempotency_key: format!("work-fork:{}", admission.operation.operation_id),
                    parent_key: parent_key.clone(),
                    child_key: child_key.clone(),
                    expected_parent_cursor: committed_cursor,
                    dimensions,
                    reason: "work_alternative_branch".into(),
                })
                .await;
            let manifest = match prepared {
                Ok(manifest) => manifest,
                Err(
                    astra_services::SessionForkCoordinatorError::NotFound
                    | astra_services::SessionForkCoordinatorError::Conflict,
                ) => {
                    let operation = service
                        .reject_cursor(&request, &admission.operation.operation_id, &executor_token)
                        .await
                        .map_err(map_work_branch_creation_error)?;
                    return Ok(operation);
                }
                Err(error) => return Err(map_work_session_fork_error(error)),
            };
            service
                .record_session_fork(
                    &request,
                    &admission.operation.operation_id,
                    &executor_token,
                    &manifest.fork_id,
                )
                .await
                .map_err(map_work_branch_creation_error)?;
            admission.session_fork_id = Some(manifest.fork_id.clone());
            manifest
        };
        service
            .renew_execution(
                &owner_id,
                &work_id,
                &origin_branch_id,
                &admission.operation.operation_id,
                &executor_token,
            )
            .await
            .map_err(map_work_branch_creation_error)?;
        match manifest.state {
            astra_turn_types::SessionForkStateV1::Prepared => {
                let context = state.session_context_coordinator.as_ref().ok_or_else(|| {
                    work_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "work_fork_unavailable",
                        WorkApiErrorCategory::Availability,
                        true,
                        vec![WorkApiActionHint::RetryWrite],
                    )
                })?;
                let epochs = context
                    .load_authority_epochs(&child_key)
                    .await
                    .map_err(|_| {
                        work_error(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "work_fork_unavailable",
                            WorkApiErrorCategory::Availability,
                            true,
                            vec![WorkApiActionHint::RetryWrite],
                        )
                    })?
                    .unwrap_or_default();
                let actor = astra_turn_types::ActorContextV1::owner_user(
                    owner_id.as_str(),
                    state.session_actor_id.clone(),
                    astra_turn_types::ActorKindV1::Server,
                    astra_turn_types::SessionSurfaceV1::Server,
                    None,
                    epochs,
                );
                let activation = fork_coordinator
                    .activate(
                        &parent_key,
                        &manifest.fork_id,
                        &actor,
                        std::time::Duration::from_secs(60),
                    )
                    .await
                    .map_err(map_work_session_fork_error)?;
                context
                    .release_writer(&activation.writer_lease)
                    .await
                    .map_err(|_| {
                        work_error(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "work_fork_unavailable",
                            WorkApiErrorCategory::Availability,
                            true,
                            vec![WorkApiActionHint::RetryWrite],
                        )
                    })?;
            }
            astra_turn_types::SessionForkStateV1::Active => {}
            astra_turn_types::SessionForkStateV1::Aborted => {
                return Err(work_error(
                    StatusCode::CONFLICT,
                    "work_fork_aborted",
                    WorkApiErrorCategory::Conflict,
                    false,
                    vec![WorkApiActionHint::RefreshWork],
                ));
            }
        }
        service
            .renew_execution(
                &owner_id,
                &work_id,
                &origin_branch_id,
                &admission.operation.operation_id,
                &executor_token,
            )
            .await
            .map_err(map_work_branch_creation_error)?;
        let operation = service
            .activate(&request, &admission.operation.operation_id, &executor_token)
            .await
            .map_err(map_work_branch_creation_error)?;
        Ok(operation)
    }
    .await;
    match execution {
        Ok(operation) => Ok((StatusCode::CREATED, Json(operation))),
        Err(error) => {
            if let Err(release_error) = service
                .release_execution(
                    &owner_id,
                    &work_id,
                    &origin_branch_id,
                    &admission.operation.operation_id,
                    &executor_token,
                )
                .await
            {
                tracing::warn!(error = %release_error, "failed to release Work fork executor");
            }
            Err(error)
        }
    }
}

pub(super) async fn get_work_branch_fork_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, origin_branch_id, operation_id)): Path<(String, String, String)>,
) -> WorkApiResult<astra_services::work::WorkBranchCreationOperation> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_work_fork_request())?;
    let origin_branch_id =
        WorkBranchId::parse(origin_branch_id).map_err(|_| invalid_work_fork_request())?;
    if !valid_work_operation_id(&operation_id) {
        return Err(invalid_work_fork_request());
    }
    let pool = state.shared_pool.clone().ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let admission = DatabaseWorkBranchCreationService::new(pool)
        .load(&owner_id, &work_id, &origin_branch_id, &operation_id)
        .await
        .map_err(map_work_branch_creation_error)?;
    Ok(Json(admission.operation))
}

pub(super) async fn delete_work_branch_fork_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, origin_branch_id, operation_id)): Path<(String, String, String)>,
) -> Result<StatusCode, (StatusCode, Json<WorkApiErrorV1>)> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_work_fork_request())?;
    let origin_branch_id =
        WorkBranchId::parse(origin_branch_id).map_err(|_| invalid_work_fork_request())?;
    if !valid_work_operation_id(&operation_id) {
        return Err(invalid_work_fork_request());
    }
    let pool = state.shared_pool.clone().ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let service = DatabaseWorkBranchCreationService::new(pool);
    let admission = service
        .load(&owner_id, &work_id, &origin_branch_id, &operation_id)
        .await
        .map_err(map_work_branch_creation_error)?;
    if admission.operation.state == astra_services::work::WorkBranchCreationState::Aborted {
        return Ok(StatusCode::NO_CONTENT);
    }
    if admission.operation.state != astra_services::work::WorkBranchCreationState::Pending {
        return Err(work_error(
            StatusCode::CONFLICT,
            "work_fork_terminal",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ));
    }
    let Some(executor_token) = service
        .claim_execution(&owner_id, &work_id, &origin_branch_id, &operation_id)
        .await
        .map_err(map_work_branch_creation_error)?
    else {
        let latest = service
            .load(&owner_id, &work_id, &origin_branch_id, &operation_id)
            .await
            .map_err(map_work_branch_creation_error)?;
        return match latest.operation.state {
            astra_services::work::WorkBranchCreationState::Aborted => Ok(StatusCode::NO_CONTENT),
            astra_services::work::WorkBranchCreationState::Pending => Err(work_error(
                StatusCode::CONFLICT,
                "work_fork_busy",
                WorkApiErrorCategory::Conflict,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )),
            _ => Err(work_error(
                StatusCode::CONFLICT,
                "work_fork_terminal",
                WorkApiErrorCategory::Conflict,
                false,
                vec![WorkApiActionHint::RefreshWork],
            )),
        };
    };
    let abortion = async {
        if let Some(fork_id) = admission.session_fork_id.as_deref() {
            let parent_key = astra_turn_types::SessionKeyV1::owner_session(
                "server",
                owner_id.as_str(),
                admission.origin_session_id.as_str(),
                astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
            );
            let manifest = state
                .session_fork_coordinator
                .as_ref()
                .ok_or_else(|| {
                    work_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "work_fork_unavailable",
                        WorkApiErrorCategory::Availability,
                        true,
                        vec![WorkApiActionHint::RetryWrite],
                    )
                })?
                .abort(
                    &parent_key,
                    fork_id,
                    std::time::Duration::from_secs(24 * 60 * 60),
                    "work_fork_aborted",
                )
                .await
                .map_err(map_work_session_fork_error)?;
            if manifest.state != astra_turn_types::SessionForkStateV1::Aborted {
                return Err(work_error(
                    StatusCode::CONFLICT,
                    "work_fork_not_abortable",
                    WorkApiErrorCategory::Conflict,
                    false,
                    vec![WorkApiActionHint::RefreshWork],
                ));
            }
        }
        service
            .abort(
                &owner_id,
                &work_id,
                &origin_branch_id,
                &operation_id,
                &executor_token,
            )
            .await
            .map_err(map_work_branch_creation_error)?;
        Ok(())
    }
    .await;
    match abortion {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(error) => {
            if let Err(release_error) = service
                .release_execution(
                    &owner_id,
                    &work_id,
                    &origin_branch_id,
                    &operation_id,
                    &executor_token,
                )
                .await
            {
                tracing::warn!(error = %release_error, "failed to release Work fork abort executor");
            }
            Err(error)
        }
    }
}

/// Release a read attachment. This is deliberately separate from branch
/// control release: detaching must never fence, transfer, or acquire a writer.
pub(super) async fn delete_work_branch_attachment_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id, attachment_id)): Path<(String, String, String)>,
) -> Result<StatusCode, (StatusCode, Json<WorkApiErrorV1>)> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let branch_id = WorkBranchId::parse(branch_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_branch_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let pool = state.shared_pool.clone().ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let binding = DatabaseWorkRepository::new(pool.clone())
        .load_branch_runtime_binding(&owner_id, &work_id, &branch_id)
        .await
        .map_err(|error| map_branch_repository_error(work_id.as_str(), error))?;
    let coordinator = state.session_context_coordinator.clone().ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let key = astra_turn_types::SessionKeyV1::owner_session(
        "server",
        owner_id.as_str(),
        binding.session_id.as_str(),
        astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
    );
    astra_services::DatabaseSessionHandoffService::new(pool, coordinator)
        .detach_read_only(&key, &attachment_id)
        .await
        .map_err(map_work_attachment_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn post_work_branch_control_operation_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id)): Path<(String, String)>,
    payload: Result<Json<WorkBranchControlOperationRequestV1>, JsonRejection>,
) -> Result<
    (
        StatusCode,
        Json<astra_services::work::WorkBranchControlOperation>,
    ),
    (StatusCode, Json<WorkApiErrorV1>),
> {
    require_work_api_major(&headers)?;
    let Json(payload) = payload.map_err(|_| invalid_work_control_request())?;
    if !valid_work_request_id(&payload.request_id)
        || payload
            .expected_canonical_root_hash
            .as_deref()
            .is_some_and(|root| !valid_canonical_root_hash(root))
    {
        return Err(invalid_work_control_request());
    }
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let branch_id = WorkBranchId::parse(branch_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_branch_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let expected_branch_revision = WorkBranchRevision::new(payload.expected_branch_revision)
        .map_err(|_| invalid_work_control_request())?;
    let (kind, attachment_id, reauthentication_proof) = match payload.command {
        WorkBranchControlCommandV1::AcquireBranchControl { attachment_id } => (
            astra_services::work::WorkBranchControlKind::AcquireBranchControl,
            attachment_id,
            None,
        ),
        WorkBranchControlCommandV1::ForceTakeover {
            attachment_id,
            reauthentication_proof,
        } => (
            astra_services::work::WorkBranchControlKind::ForceTakeover,
            attachment_id,
            Some(reauthentication_proof),
        ),
        WorkBranchControlCommandV1::ReleaseBranchControl { attachment_id } => (
            astra_services::work::WorkBranchControlKind::ReleaseBranchControl,
            attachment_id,
            None,
        ),
    };
    if !valid_work_attachment_id(&attachment_id) {
        return Err(invalid_work_control_request());
    }
    let pool = state.shared_pool.clone().ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let request = astra_services::work::WorkBranchControlRequest {
        request_id: payload.request_id,
        owner_id,
        work_id,
        branch_id,
        attachment_id,
        expected_branch_revision,
        expected_basis: astra_services::SessionControllerBasisV1 {
            writer_epoch: payload.expected_writer_epoch,
            canonical_root_hash: payload.expected_canonical_root_hash,
        },
        kind,
    };
    let service = astra_services::work::DatabaseWorkBranchControlService::new(pool);
    let operation = if kind == astra_services::work::WorkBranchControlKind::ForceTakeover {
        execute_work_force_takeover(
            &state,
            &service,
            &request,
            reauthentication_proof.as_deref().unwrap_or_default(),
        )
        .await?
    } else {
        service
            .execute(&request)
            .await
            .map_err(map_work_control_error)?
    };
    let status = if operation.state == astra_services::work::WorkBranchControlState::Pending {
        StatusCode::ACCEPTED
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(operation)))
}

async fn execute_work_force_takeover(
    state: &AppState,
    control: &astra_services::work::DatabaseWorkBranchControlService,
    request: &astra_services::work::WorkBranchControlRequest,
    reauthentication_proof: &str,
) -> Result<astra_services::work::WorkBranchControlOperation, (StatusCode, Json<WorkApiErrorV1>)> {
    let mut admission = control
        .admit_force_takeover(request)
        .await
        .map_err(map_work_control_error)?;
    if admission.operation.state != astra_services::work::WorkBranchControlState::Pending {
        return Ok(admission.operation);
    }
    let authorization_id = if let Some(id) = admission.authorization_id.clone() {
        id
    } else {
        state
            .auth_service
            .consume_reauthentication_proof(
                request.owner_id.as_str(),
                astra_services::ReauthenticationPurpose::SessionForcedTakeover,
                reauthentication_proof,
            )
            .await
            .map_err(|_| {
                work_error(
                    StatusCode::FORBIDDEN,
                    "reauthentication_required",
                    WorkApiErrorCategory::Authentication,
                    false,
                    Vec::new(),
                )
            })?;
        control
            .record_force_authorization(
                request,
                &admission.operation.operation_id,
                &format!(
                    "consumed-reauth:{}",
                    hex_sha256(reauthentication_proof.as_bytes())
                ),
            )
            .await
            .map_err(map_work_control_error)?
    };
    admission.authorization_id = Some(authorization_id);
    admission.operation = control
        .load_force_context(
            &request.owner_id,
            &request.work_id,
            &request.branch_id,
            &admission.operation.operation_id,
        )
        .await
        .map_err(map_work_control_error)?
        .operation;
    let operation = admission.operation.clone();
    let Some(executor_token) = control
        .claim_force_executor(
            &request.owner_id,
            &request.work_id,
            &request.branch_id,
            &operation.operation_id,
            60,
        )
        .await
        .map_err(map_work_control_error)?
    else {
        return Ok(operation);
    };
    let state = state.clone();
    let control = control.clone();
    let request = request.clone();
    let operation_id = operation.operation_id.clone();
    tokio::spawn(async move {
        let mut attempt = 0_u32;
        loop {
            match resume_work_force_takeover(
                &state,
                &control,
                &request,
                &executor_token,
                admission.clone(),
            )
            .await
            {
                Ok(_) => return,
                Err(error)
                    if error.1.0.retryable
                        && error.1.0.code != "work_control_executor_reassigned" =>
                {
                    let delay_ms = 250_u64.saturating_mul(1_u64 << attempt.min(3));
                    tracing::debug!(
                        target: "astra_runtime::work",
                        work_id = %request.work_id.as_str(),
                        branch_id = %request.branch_id.as_str(),
                        operation_id = %operation_id,
                        status = %error.0,
                        retry_delay_ms = delay_ms,
                        "background forced Work takeover will retry from durable state"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    attempt = attempt.saturating_add(1);
                }
                Err(error) => {
                    tracing::warn!(
                        target: "astra_runtime::work",
                        work_id = %request.work_id.as_str(),
                        branch_id = %request.branch_id.as_str(),
                        operation_id = %operation_id,
                        status = %error.0,
                        "background forced Work takeover stopped before a terminal outcome"
                    );
                    break;
                }
            }
        }
        if let Err(release_error) = control
            .release_force_executor(
                &request.owner_id,
                &request.work_id,
                &request.branch_id,
                &operation_id,
                &executor_token,
            )
            .await
        {
            tracing::warn!(
                target: "astra_runtime::work",
                work_id = %request.work_id.as_str(),
                branch_id = %request.branch_id.as_str(),
                operation_id = %operation_id,
                error = %release_error,
                "failed to release forced Work takeover executor lease"
            );
        }
    });
    Ok(operation)
}

async fn resume_work_force_takeover(
    state: &AppState,
    control: &astra_services::work::DatabaseWorkBranchControlService,
    request: &astra_services::work::WorkBranchControlRequest,
    executor_token: &str,
    admission: astra_services::work::WorkBranchForceAdmission,
) -> Result<astra_services::work::WorkBranchControlOperation, (StatusCode, Json<WorkApiErrorV1>)> {
    renew_work_force_executor(
        control,
        request,
        &admission.operation.operation_id,
        executor_token,
    )
    .await?;
    let authorization_id = admission.authorization_id.clone().ok_or_else(|| {
        work_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "work_control_requires_repair",
            WorkApiErrorCategory::Degraded,
            false,
            Vec::new(),
        )
    })?;
    let key = astra_turn_types::SessionKeyV1::owner_session(
        "server",
        request.owner_id.as_str(),
        &admission.session_id,
        astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
    );
    let handoff = state.session_handoff_service.as_ref().ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_control_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let handoff_idempotency = format!("work-force:{}", admission.operation.operation_id);
    let existing_handoff = if let Some(handoff_id) = admission.handoff_id.as_deref() {
        Some(
            handoff
                .load_handoff(&key, handoff_id)
                .await
                .map_err(|error| map_work_control_error(error.into()))?,
        )
    } else {
        handoff
            .find_handoff_by_idempotency(&key, &handoff_idempotency)
            .await
            .map_err(|error| map_work_control_error(error.into()))?
    };
    let mut record = if let Some(record) = existing_handoff {
        record
    } else {
        let observed_basis = handoff
            .load_controller_basis(&key)
            .await
            .map_err(|error| map_work_control_error(error.into()))?;
        if observed_basis != request.expected_basis {
            return control
                .conflict_force_takeover(
                    request,
                    &admission.operation.operation_id,
                    &observed_basis,
                )
                .await
                .map_err(map_work_control_error);
        }
        let target = handoff
            .load_attachment(&key, &request.attachment_id)
            .await
            .map_err(|error| map_work_control_error(error.into()))?;
        let coordinator = state.session_context_coordinator.as_ref().ok_or_else(|| {
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_control_unavailable",
                WorkApiErrorCategory::Availability,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )
        })?;
        let head = coordinator.load_head(&key).await.map_err(|error| {
            map_work_control_error(astra_services::SessionHandoffError::Coordinator(error).into())
        })?;
        if head
            .as_ref()
            .map(|head| head.cursor.canonical_root_hash.as_str())
            != request.expected_basis.canonical_root_hash.as_deref()
        {
            let observed_basis = handoff
                .load_controller_basis(&key)
                .await
                .map_err(|error| map_work_control_error(error.into()))?;
            return control
                .conflict_force_takeover(
                    request,
                    &admission.operation.operation_id,
                    &observed_basis,
                )
                .await
                .map_err(map_work_control_error);
        }
        let authority_epochs = coordinator
            .load_authority_epochs(&key)
            .await
            .map_err(|error| {
                map_work_control_error(
                    astra_services::SessionHandoffError::Coordinator(error).into(),
                )
            })?
            .unwrap_or_default();
        renew_work_force_executor(
            control,
            request,
            &admission.operation.operation_id,
            executor_token,
        )
        .await?;
        handoff
            .request_handoff(
                &astra_services::RequestSessionHandoffV1 {
                    idempotency_key: handoff_idempotency,
                    key: key.clone(),
                    mode: astra_turn_types::SessionHandoffModeV1::Forced,
                    from_attachment_id: None,
                    to_attachment_id: target.attachment_id.clone(),
                    from_placement: astra_turn_types::SessionPlacementV1::Server,
                    to_placement: target.placement,
                    target_actor: target.actor,
                    base_cursor: head.map(|head| head.cursor),
                    authority_epochs,
                    workspace: target.workspace,
                    watermarks: astra_turn_types::HandoffOperationWatermarksV1::default(),
                    risk: astra_turn_types::HandoffRiskEvidenceV1 {
                        forced_authorization_id: Some(authorization_id.clone()),
                        ..astra_turn_types::HandoffRiskEvidenceV1::default()
                    },
                    reason: "work_force_takeover".into(),
                },
                std::time::Duration::from_secs(5 * 60),
            )
            .await
            .map_err(|error| map_work_control_error(error.into()))?
    };
    if record.mode != astra_turn_types::SessionHandoffModeV1::Forced
        || record.idempotency_key != format!("work-force:{}", admission.operation.operation_id)
        || record.to_attachment_id.as_deref() != Some(request.attachment_id.as_str())
        || record.risk.forced_authorization_id.as_deref() != Some(authorization_id.as_str())
    {
        return Err(work_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "work_control_requires_repair",
            WorkApiErrorCategory::Degraded,
            false,
            Vec::new(),
        ));
    }
    if let Err(error) = control
        .record_force_handoff(
            request,
            &admission.operation.operation_id,
            &record.handoff_id,
        )
        .await
    {
        let context = control
            .load_force_context(
                &request.owner_id,
                &request.work_id,
                &request.branch_id,
                &admission.operation.operation_id,
            )
            .await
            .map_err(map_work_control_error)?;
        if context.operation.state == astra_services::work::WorkBranchControlState::Aborted {
            let current = handoff
                .load_handoff(&key, &record.handoff_id)
                .await
                .map_err(|error| map_work_control_error(error.into()))?;
            if current.state != astra_turn_types::SessionHandoffStateV1::Aborted {
                handoff
                    .transition_handoff(&astra_services::TransitionSessionHandoffV1 {
                        idempotency_key: format!(
                            "work-force:{}:abort-raced-admission",
                            current.handoff_id
                        ),
                        key: key.clone(),
                        handoff_id: current.handoff_id,
                        expected_state: current.state,
                        expected_transition_seq: current.transition_seq,
                        next_state: astra_turn_types::SessionHandoffStateV1::Aborted,
                        patch: astra_services::HandoffTransitionPatchV1::default(),
                    })
                    .await
                    .map_err(|error| map_work_control_error(error.into()))?;
            }
            return Ok(context.operation);
        }
        return Err(map_work_control_error(error));
    }
    loop {
        renew_work_force_executor(
            control,
            request,
            &admission.operation.operation_id,
            executor_token,
        )
        .await?;
        record = match record.state {
            astra_turn_types::SessionHandoffStateV1::Requested => handoff
                .transition_handoff(&astra_services::TransitionSessionHandoffV1 {
                    idempotency_key: format!("work-force:{}:validate", record.handoff_id),
                    key: key.clone(),
                    handoff_id: record.handoff_id.clone(),
                    expected_state: record.state,
                    expected_transition_seq: record.transition_seq,
                    next_state: astra_turn_types::SessionHandoffStateV1::Validating,
                    patch: astra_services::HandoffTransitionPatchV1::default(),
                })
                .await
                .map_err(|error| map_work_control_error(error.into()))?,
            astra_turn_types::SessionHandoffStateV1::Validating
            | astra_turn_types::SessionHandoffStateV1::Fencing => {
                let fence = handoff
                    .fence_writer(
                        &key,
                        &record.handoff_id,
                        None,
                        Some(request.expected_basis.writer_epoch),
                        std::time::Duration::from_secs(15 * 60),
                        &format!("work-force:{}:fence", record.handoff_id),
                    )
                    .await
                    .map_err(|error| map_work_control_error(error.into()))?;
                if matches!(
                    fence.transfer,
                    Some(astra_services::TransferWriterOutcome::Conflict { .. })
                ) {
                    let blocked = fence.handoff;
                    handoff
                        .transition_handoff(&astra_services::TransitionSessionHandoffV1 {
                            idempotency_key: format!(
                                "work-force:{}:abort-conflict",
                                blocked.handoff_id
                            ),
                            key: key.clone(),
                            handoff_id: blocked.handoff_id,
                            expected_state: blocked.state,
                            expected_transition_seq: blocked.transition_seq,
                            next_state: astra_turn_types::SessionHandoffStateV1::Aborted,
                            patch: astra_services::HandoffTransitionPatchV1::default(),
                        })
                        .await
                        .map_err(|error| map_work_control_error(error.into()))?;
                    let observed_basis = handoff
                        .load_controller_basis(&key)
                        .await
                        .map_err(|error| map_work_control_error(error.into()))?;
                    return control
                        .conflict_force_takeover(
                            request,
                            &admission.operation.operation_id,
                            &observed_basis,
                        )
                        .await
                        .map_err(map_work_control_error);
                }
                fence.handoff
            }
            astra_turn_types::SessionHandoffStateV1::Fenced => {
                state
                    .execution
                    .run_lifecycle_service
                    .cancel_session_runs(
                        admission.session_id.clone(),
                        request.owner_id.as_str().to_owned(),
                    )
                    .await
                    .map_err(|_| {
                        work_error(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "force_takeover_run_cancel_failed",
                            WorkApiErrorCategory::Availability,
                            true,
                            vec![WorkApiActionHint::RetryWrite],
                        )
                    })?;
                handoff
                    .transition_handoff(&astra_services::TransitionSessionHandoffV1 {
                        idempotency_key: format!("work-force:{}:hydrate", record.handoff_id),
                        key: key.clone(),
                        handoff_id: record.handoff_id.clone(),
                        expected_state: record.state,
                        expected_transition_seq: record.transition_seq,
                        next_state: astra_turn_types::SessionHandoffStateV1::Hydrating,
                        patch: astra_services::HandoffTransitionPatchV1::default(),
                    })
                    .await
                    .map_err(|error| map_work_control_error(error.into()))?
            }
            astra_turn_types::SessionHandoffStateV1::Hydrating => handoff
                .activate_handoff(
                    &key,
                    &record.handoff_id,
                    record.transition_seq,
                    &format!("work-force:{}:activate", record.handoff_id),
                )
                .await
                .map_err(|error| map_work_control_error(error.into()))?,
            astra_turn_types::SessionHandoffStateV1::Active => break,
            astra_turn_types::SessionHandoffStateV1::Aborted => {
                let observed_basis = handoff
                    .load_controller_basis(&key)
                    .await
                    .map_err(|error| map_work_control_error(error.into()))?;
                return control
                    .conflict_force_takeover(
                        request,
                        &admission.operation.operation_id,
                        &observed_basis,
                    )
                    .await
                    .map_err(map_work_control_error);
            }
            _ => {
                return Err(work_error(
                    StatusCode::CONFLICT,
                    "force_takeover_blocked",
                    WorkApiErrorCategory::Conflict,
                    true,
                    vec![WorkApiActionHint::RefreshWork],
                ));
            }
        };
    }
    let basis = handoff
        .load_controller_basis(&key)
        .await
        .map_err(|error| map_work_control_error(error.into()))?;
    renew_work_force_executor(
        control,
        request,
        &admission.operation.operation_id,
        executor_token,
    )
    .await?;
    control
        .complete_force_takeover(request, &admission.operation.operation_id, &basis)
        .await
        .map_err(map_work_control_error)
}

async fn renew_work_force_executor(
    control: &astra_services::work::DatabaseWorkBranchControlService,
    request: &astra_services::work::WorkBranchControlRequest,
    operation_id: &str,
    executor_token: &str,
) -> Result<(), (StatusCode, Json<WorkApiErrorV1>)> {
    let owned = control
        .renew_force_executor(
            &request.owner_id,
            &request.work_id,
            &request.branch_id,
            operation_id,
            executor_token,
            60,
        )
        .await
        .map_err(map_work_control_error)?;
    if owned {
        Ok(())
    } else {
        Err(work_error(
            StatusCode::CONFLICT,
            "work_control_executor_reassigned",
            WorkApiErrorCategory::Conflict,
            true,
            vec![WorkApiActionHint::RetryRead],
        ))
    }
}

pub(super) async fn get_work_branch_control_operation_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id, operation_id)): Path<(String, String, String)>,
) -> WorkApiResult<astra_services::work::WorkBranchControlOperation> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_work_control_request())?;
    let branch_id = WorkBranchId::parse(branch_id).map_err(|_| invalid_work_control_request())?;
    if !valid_work_operation_id(&operation_id) {
        return Err(invalid_work_control_request());
    }
    let pool = state.shared_pool.clone().ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let control = astra_services::work::DatabaseWorkBranchControlService::new(pool);
    let mut context = control
        .load_force_context(&owner_id, &work_id, &branch_id, &operation_id)
        .await
        .map_err(map_work_control_error)?;
    if context.operation.kind == astra_services::work::WorkBranchControlKind::ForceTakeover
        && context.operation.state == astra_services::work::WorkBranchControlState::Pending
    {
        let handoff = state.session_handoff_service.as_ref().ok_or_else(|| {
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_control_unavailable",
                WorkApiErrorCategory::Availability,
                true,
                vec![WorkApiActionHint::RetryRead],
            )
        })?;
        let key = astra_turn_types::SessionKeyV1::owner_session(
            "server",
            owner_id.as_str(),
            &context.session_id,
            astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
        );
        let record = if let Some(handoff_id) = context.handoff_id.as_deref() {
            Some(
                handoff
                    .load_handoff(&key, handoff_id)
                    .await
                    .map_err(|error| map_work_control_error(error.into()))?,
            )
        } else {
            handoff
                .find_handoff_by_idempotency(
                    &key,
                    &format!("work-force:{}", context.operation.operation_id),
                )
                .await
                .map_err(|error| map_work_control_error(error.into()))?
        };
        if let Some(record) = record {
            context.operation.observe_handoff_state(record.state);
        }
    }
    Ok(Json(context.operation))
}

pub(super) async fn delete_work_branch_control_operation_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id, operation_id)): Path<(String, String, String)>,
) -> Result<StatusCode, (StatusCode, Json<WorkApiErrorV1>)> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_work_control_request())?;
    let branch_id = WorkBranchId::parse(branch_id).map_err(|_| invalid_work_control_request())?;
    if !valid_work_operation_id(&operation_id) {
        return Err(invalid_work_control_request());
    }
    let pool = state.shared_pool.clone().ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let control = astra_services::work::DatabaseWorkBranchControlService::new(pool);
    let context = control
        .load_force_context(&owner_id, &work_id, &branch_id, &operation_id)
        .await
        .map_err(map_work_control_error)?;
    if context.operation.state == astra_services::work::WorkBranchControlState::Aborted {
        return Ok(StatusCode::NO_CONTENT);
    }
    if context.operation.state != astra_services::work::WorkBranchControlState::Pending
        || context.operation.kind != astra_services::work::WorkBranchControlKind::ForceTakeover
    {
        return Err(work_error(
            StatusCode::CONFLICT,
            "control_operation_terminal",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ));
    }
    let handoff = state.session_handoff_service.as_ref().ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_control_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let key = astra_turn_types::SessionKeyV1::owner_session(
        "server",
        owner_id.as_str(),
        &context.session_id,
        astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
    );
    let record = if let Some(handoff_id) = context.handoff_id.as_deref() {
        Some(
            handoff
                .load_handoff(&key, handoff_id)
                .await
                .map_err(|error| map_work_control_error(error.into()))?,
        )
    } else {
        handoff
            .find_handoff_by_idempotency(
                &key,
                &format!("work-force:{}", context.operation.operation_id),
            )
            .await
            .map_err(|error| map_work_control_error(error.into()))?
    };
    if let Some(record) = record {
        if !astra_services::work::force_handoff_is_abortable(record.state) {
            return Err(work_error(
                StatusCode::CONFLICT,
                "control_operation_not_abortable",
                WorkApiErrorCategory::Conflict,
                true,
                vec![WorkApiActionHint::RetryRead],
            ));
        }
        if record.state != astra_turn_types::SessionHandoffStateV1::Aborted {
            handoff
                .transition_handoff(&astra_services::TransitionSessionHandoffV1 {
                    idempotency_key: format!("work-force:{}:user-abort", record.handoff_id),
                    key,
                    handoff_id: record.handoff_id,
                    expected_state: record.state,
                    expected_transition_seq: record.transition_seq,
                    next_state: astra_turn_types::SessionHandoffStateV1::Aborted,
                    patch: astra_services::HandoffTransitionPatchV1::default(),
                })
                .await
                .map_err(|error| map_work_control_error(error.into()))?;
        }
    }
    control
        .abort_force_takeover(&owner_id, &work_id, &branch_id, &operation_id)
        .await
        .map_err(map_work_control_error)?;
    Ok(StatusCode::NO_CONTENT)
}

fn valid_canonical_root_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn valid_committed_work_cursor(cursor: &WorkConversationHeadV1) -> bool {
    cursor.completed_turn > 0
        && cursor.journal_event_seq > 0
        && cursor.conversation_seq > 0
        && cursor.projection_schema > 0
        && valid_canonical_root_hash(&cursor.canonical_root_hash)
        && cursor.config_version_id.as_ref().is_none_or(|value| {
            !value.is_empty()
                && value.len() <= 256
                && !value.chars().any(char::is_whitespace)
                && !value.bytes().any(|byte| byte.is_ascii_control())
        })
}

fn valid_work_attachment_id(value: &str) -> bool {
    !matches!(value, "." | "..")
        && !value.is_empty()
        && value.chars().count() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

fn valid_work_operation_id(value: &str) -> bool {
    !matches!(value, "." | "..")
        && !value.is_empty()
        && value.chars().count() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn invalid_work_control_request() -> (StatusCode, Json<WorkApiErrorV1>) {
    work_error(
        StatusCode::BAD_REQUEST,
        "invalid_work_control_request",
        WorkApiErrorCategory::InvalidRequest,
        false,
        Vec::new(),
    )
}

fn invalid_work_fork_request() -> (StatusCode, Json<WorkApiErrorV1>) {
    work_error(
        StatusCode::BAD_REQUEST,
        "invalid_work_fork_request",
        WorkApiErrorCategory::InvalidRequest,
        false,
        Vec::new(),
    )
}

fn map_work_branch_creation_error(
    error: astra_services::work::WorkBranchCreationError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    match error {
        astra_services::work::WorkBranchCreationError::Invalid(_) => invalid_work_fork_request(),
        astra_services::work::WorkBranchCreationError::NotFound => work_error(
            StatusCode::NOT_FOUND,
            "branch_not_found",
            WorkApiErrorCategory::NotFound,
            false,
            Vec::new(),
        ),
        astra_services::work::WorkBranchCreationError::OperationNotFound => work_error(
            StatusCode::NOT_FOUND,
            "work_fork_not_found",
            WorkApiErrorCategory::NotFound,
            false,
            Vec::new(),
        ),
        astra_services::work::WorkBranchCreationError::Archived => work_error(
            StatusCode::CONFLICT,
            "work_archived",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        astra_services::work::WorkBranchCreationError::Deleting => work_error(
            StatusCode::CONFLICT,
            "work_branch_deleting",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        astra_services::work::WorkBranchCreationError::IdempotencyMismatch => work_error(
            StatusCode::CONFLICT,
            "idempotency_mismatch",
            WorkApiErrorCategory::Conflict,
            false,
            Vec::new(),
        ),
        astra_services::work::WorkBranchCreationError::Conflict => work_error(
            StatusCode::CONFLICT,
            "work_fork_conflict",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        error => {
            tracing::warn!(error = %error, "Work branch creation operation degraded");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_fork_unavailable",
                WorkApiErrorCategory::Degraded,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )
        }
    }
}

fn map_work_session_fork_error(
    error: astra_services::SessionForkCoordinatorError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    match error {
        astra_services::SessionForkCoordinatorError::Invalid(_) => invalid_work_fork_request(),
        astra_services::SessionForkCoordinatorError::NotFound
        | astra_services::SessionForkCoordinatorError::Conflict
        | astra_services::SessionForkCoordinatorError::WriterConflict => work_error(
            StatusCode::CONFLICT,
            "work_fork_cursor_conflict",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        astra_services::SessionForkCoordinatorError::IdempotencyMismatch => work_error(
            StatusCode::CONFLICT,
            "idempotency_mismatch",
            WorkApiErrorCategory::Conflict,
            false,
            Vec::new(),
        ),
        error => {
            tracing::warn!(error = %error, "Work session fork degraded");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_fork_unavailable",
                WorkApiErrorCategory::Degraded,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )
        }
    }
}

fn map_work_control_error(
    error: astra_services::work::WorkBranchControlError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    match error {
        astra_services::work::WorkBranchControlError::NotFound => work_error(
            StatusCode::NOT_FOUND,
            "branch_not_found",
            WorkApiErrorCategory::NotFound,
            false,
            Vec::new(),
        ),
        astra_services::work::WorkBranchControlError::OperationNotFound => work_error(
            StatusCode::NOT_FOUND,
            "control_operation_not_found",
            WorkApiErrorCategory::NotFound,
            false,
            Vec::new(),
        ),
        astra_services::work::WorkBranchControlError::IdempotencyMismatch => work_error(
            StatusCode::CONFLICT,
            "idempotency_mismatch",
            WorkApiErrorCategory::Conflict,
            false,
            Vec::new(),
        ),
        astra_services::work::WorkBranchControlError::Session(
            astra_services::SessionHandoffError::NotFound
            | astra_services::SessionHandoffError::AttachmentExpired,
        ) => work_error(
            StatusCode::CONFLICT,
            "attachment_fenced",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RetryAttach],
        ),
        error => {
            tracing::warn!(error = %error, "Work branch control operation degraded");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "control_operation_unavailable",
                WorkApiErrorCategory::Degraded,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )
        }
    }
}

fn work_attachment_projection_degraded() -> (StatusCode, Json<WorkApiErrorV1>) {
    work_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "causal_projection_degraded",
        WorkApiErrorCategory::Degraded,
        true,
        vec![WorkApiActionHint::RetryAttach],
    )
}

fn map_work_attachment_coordinator_error(
    error: astra_services::SessionContextCoordinatorError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    tracing::warn!(error = %error, "Work attachment could not read canonical continuity");
    work_attachment_projection_degraded()
}

fn map_work_attachment_error(
    error: astra_services::SessionHandoffError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    match error {
        astra_services::SessionHandoffError::AttachmentCapacityExceeded => work_error(
            StatusCode::TOO_MANY_REQUESTS,
            "work_attachment_capacity",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryAttach],
        ),
        astra_services::SessionHandoffError::IdempotencyMismatch => work_error(
            StatusCode::CONFLICT,
            "idempotency_mismatch",
            WorkApiErrorCategory::Conflict,
            false,
            Vec::new(),
        ),
        astra_services::SessionHandoffError::AttachmentInUse
        | astra_services::SessionHandoffError::AttachmentControlsBranch => work_error(
            StatusCode::CONFLICT,
            "attachment_in_use",
            WorkApiErrorCategory::Conflict,
            false,
            Vec::new(),
        ),
        error => {
            tracing::warn!(error = %error, "Work read attachment degraded");
            work_attachment_projection_degraded()
        }
    }
}

fn map_work_controller_claim_error(
    error: astra_services::SessionHandoffError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    match error {
        astra_services::SessionHandoffError::NotFound
        | astra_services::SessionHandoffError::AttachmentExpired => work_error(
            StatusCode::CONFLICT,
            "attachment_fenced",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RetryAttach],
        ),
        error => map_work_attachment_error(error),
    }
}

/// Start exactly one new turn on a public Work branch. The caller never sees
/// or supplies the opaque session that backs the branch; the shared runtime
/// receives a server-derived binding, idempotency identity, and default model
/// selection directive.
pub(super) async fn post_work_branch_turn_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id)): Path<(String, String)>,
    payload: Result<Json<WorkTurnRequestV1>, JsonRejection>,
) -> Result<Response, (StatusCode, Json<WorkApiErrorV1>)> {
    require_work_api_major(&headers)?;
    let Json(payload) = payload.map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_turn_request",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let branch_id = WorkBranchId::parse(branch_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_branch_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let turn = derive_work_turn(&owner_id, &work_id, &branch_id, payload)?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let binding = repository
        .load_branch_runtime_binding(&owner_id, &work_id, &branch_id)
        .await
        .map_err(|error| map_branch_repository_error(work_id.as_str(), error))?;
    let coordinator = state.session_context_coordinator.clone().ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let key = astra_turn_types::SessionKeyV1::owner_session(
        "server",
        owner_id.as_str(),
        binding.session_id.as_str(),
        astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
    );
    let handoff = astra_services::DatabaseSessionHandoffService::new(pool, coordinator);
    match handoff
        .claim_idle_controller(&key, &turn.attachment_id)
        .await
        .map_err(map_work_controller_claim_error)?
    {
        astra_services::ClaimSessionControllerOutcomeV1::Acquired(_)
        | astra_services::ClaimSessionControllerOutcomeV1::AlreadyControlled(_) => {}
        astra_services::ClaimSessionControllerOutcomeV1::Conflict => {
            return Err(work_error(
                StatusCode::CONFLICT,
                "writer_conflict",
                WorkApiErrorCategory::Conflict,
                false,
                vec![WorkApiActionHint::RefreshWork],
            ));
        }
    }
    let expected_run_id = turn.start_idempotency.run_id().to_string();
    let request = ChatRequestData {
        message: turn.message,
        user_intent: None,
        parts: Vec::new(),
        attachments: Vec::new(),
        stable_runtime_system_prompt: None,
        runtime_system_prompt: None,
        session_id: Some(binding.session_id.as_str().to_string()),
        work_binding: Some(WorkRuntimeBindingRequest {
            work_id: binding.work_id.as_str().to_string(),
            branch_id: binding.branch_id.as_str().to_string(),
            item: Some(WorkItemRuntimeBindingRequest {
                item_id: WorkItemId::root().as_str().to_string(),
                item_revision: WorkItemRevision::INITIAL.get(),
                attempt_id: expected_run_id.clone(),
            }),
        }),
        run_start_idempotency: Some(turn.start_idempotency),
        full_llm_capture: false,
        agent_id: None,
        model: None,
        model_selection_mode: ModelSelectionMode::ServerDefault,
        model_selection: None,
        resolved_model_selection: None,
        admitted_model_execution: None,
        capability_descriptors: None,
        provider_runtime_authorized: false,
        agent_bindings: Vec::new(),
        agent_binding: None,
        runtime_auth: None,
        runtime_skill_binding: None,
        runtime_profile: None,
        skill_search: None,
        allow_skills: None,
        allow_skill_sources: None,
        allow_tools: None,
        enabled_tools: None,
        // Server-owned Work turns use one persistent workspace per internal
        // branch session. The runtime records the resolved binding before the
        // run becomes visible; clients never select a topology per turn.
        workspace_binding: Some(server_owned_work_workspace_binding()),
        executor_binding: None,
        runtime_mcp_bindings: Vec::new(),
        context: None,
        edge_executor_id: None,
        capabilities: Vec::new(),
        forward_headers: collect_forward_headers(&headers),
        provider_run_owner: None,
        provider_workspace_id: None,
        agent_binding_owner_scope: None,
        execution_budget: None,
        execution_time_budget: None,
        execution_policy: Default::default(),
        explain: false,
        interaction_mode: None,
        interactive_client: false,
        conversation_authority: None,
    };
    let stream_result = state
        .execution
        .run_lifecycle_service
        .stream_chat(owner_id.as_str().to_string(), request)
        .await;
    let mut stream = match stream_result {
        Ok(stream) => stream,
        Err(error) => {
            // The shared lifecycle returns Err only before it hands back a
            // live stream; it terminalizes any partially created run and
            // releases canonical turn authority first. Relinquish the
            // conversation controller as the matching compensation step so a
            // provider/admission/DB start failure cannot fence this Work for
            // the attachment TTL. If a writer did become active, the handoff
            // service fails closed with Conflict and preserves control.
            match handoff
                .release_idle_controller(&key, &turn.attachment_id)
                .await
            {
                Ok(
                    astra_services::ReleaseSessionControllerOutcomeV1::Released(_)
                    | astra_services::ReleaseSessionControllerOutcomeV1::AlreadyReleased(_),
                ) => {}
                Ok(astra_services::ReleaseSessionControllerOutcomeV1::Conflict) => {
                    tracing::warn!(
                        work_id = work_id.as_str(),
                        branch_id = branch_id.as_str(),
                        attachment_id = turn.attachment_id,
                        "Work turn start failed while execution authority was still active; controller preserved"
                    );
                }
                Err(release_error) => {
                    tracing::warn!(
                        work_id = work_id.as_str(),
                        branch_id = branch_id.as_str(),
                        attachment_id = turn.attachment_id,
                        error = %release_error,
                        "Work turn start failed and controller compensation could not be confirmed"
                    );
                }
            }
            return Err(map_turn_start_error(work_id.as_str(), error));
        }
    };
    if stream.run_id != expected_run_id || stream.session_id != binding.session_id.as_str() {
        tracing::error!(
            work_id = work_id.as_str(),
            branch_id = branch_id.as_str(),
            expected_run_id,
            actual_run_id = stream.run_id,
            "run lifecycle returned an incoherent Work turn binding"
        );
        return Err(work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "causal_projection_degraded",
            WorkApiErrorCategory::Degraded,
            true,
            vec![WorkApiActionHint::RetryRead],
        ));
    }
    let started = serde_json::json!({
        "type": "work_turn_started",
        "schema_version": 1,
        "work_id": work_id,
        "branch_id": branch_id,
        "run_id": stream.run_id,
    });
    if let Some(event_rx) = stream.event_rx.take() {
        Ok(
            crate::server::http_helpers::sse_projected_streaming_response(
                started,
                binding.session_id.as_str().to_string(),
                expected_run_id,
                None,
                event_rx,
                project_work_turn_events,
            ),
        )
    } else {
        let mut pending_run_error = None;
        let mut events = vec![started];
        events.extend(project_work_turn_events(
            &expected_run_id,
            stream.events,
            &mut pending_run_error,
        ));
        Ok(crate::server::http_helpers::sse_json_response(events))
    }
}

/// Exact-major, read-only public Work overview. This handler never accepts an
/// internal session identity and never falls back to legacy task/plan stores.
pub(super) async fn get_work_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(work_id): Path<String>,
) -> WorkApiResult<WorkObservationResponseV1> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let repository = DatabaseWorkRepository::new(pool);
    let report = repository
        .observe_declared_work(WorkObservationQuery {
            owner_id,
            work_id: work_id.clone(),
        })
        .await
        .map_err(|error| map_repository_error(work_id.as_str(), error))?;
    Ok(Json(WorkObservationResponseV1(report)))
}

/// Complete active branch catalog. Branch admission keeps this projection
/// bounded; internal session identities and archived history are excluded.
pub(super) async fn get_work_branches_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(work_id): Path<String>,
) -> WorkApiResult<astra_services::work::WorkBranchCatalog> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let catalog = DatabaseWorkBranchCatalogService::new(pool)
        .load_active(&owner_id, &work_id)
        .await
        .map_err(|error| match error {
            astra_services::work::WorkBranchCatalogError::NotFound => work_error(
                StatusCode::NOT_FOUND,
                "work_not_found",
                WorkApiErrorCategory::NotFound,
                false,
                Vec::new(),
            ),
            error => {
                tracing::warn!(error = %error, "Work branch catalog degraded");
                work_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "work_branches_unavailable",
                    WorkApiErrorCategory::Degraded,
                    true,
                    vec![WorkApiActionHint::RetryRead],
                )
            }
        })?;
    Ok(Json(catalog))
}

/// Bounded archived branch history. The cursor is an exact archive-time and
/// branch-identity pair, so long-lived Work does not require offset scans.
pub(super) async fn get_archived_work_branches_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(work_id): Path<String>,
    query: Result<Query<WorkArchivedBranchesQueryV1>, QueryRejection>,
) -> WorkApiResult<astra_services::work::WorkArchivedBranchPage> {
    require_work_api_major(&headers)?;
    let Query(query) = query.map_err(|_| invalid_archived_branch_query())?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_archived_branch_query())?;
    let cursor = match (query.before_archived_at, query.before_branch_id) {
        (None, None) => None,
        (Some(archived_at), Some(branch_id)) => Some(WorkArchivedBranchCursor {
            archived_at,
            branch_id: WorkBranchId::parse(branch_id)
                .map_err(|_| invalid_archived_branch_query())?,
        }),
        _ => return Err(invalid_archived_branch_query()),
    };
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let page = DatabaseWorkBranchCatalogService::new(pool)
        .load_archived(
            &owner_id,
            &work_id,
            cursor.as_ref(),
            query.limit.unwrap_or(WORK_ARCHIVED_BRANCH_DEFAULT_LIMIT),
        )
        .await
        .map_err(|error| match error {
            astra_services::work::WorkBranchCatalogError::NotFound => work_error(
                StatusCode::NOT_FOUND,
                "work_not_found",
                WorkApiErrorCategory::NotFound,
                false,
                Vec::new(),
            ),
            astra_services::work::WorkBranchCatalogError::InvalidQuery => {
                invalid_archived_branch_query()
            }
            error => {
                tracing::warn!(error = %error, "Archived Work branch catalog degraded");
                work_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "work_branches_unavailable",
                    WorkApiErrorCategory::Degraded,
                    true,
                    vec![WorkApiActionHint::RetryRead],
                )
            }
        })?;
    Ok(Json(page))
}

fn invalid_archived_branch_query() -> (StatusCode, Json<WorkApiErrorV1>) {
    work_error(
        StatusCode::BAD_REQUEST,
        "invalid_archived_branch_query",
        WorkApiErrorCategory::InvalidRequest,
        false,
        Vec::new(),
    )
}

/// Compare two exact active branches. Missing evidence domains remain typed
/// coverage gaps; this endpoint never asks a model to infer or rank results.
pub(super) async fn post_work_branch_comparison_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(work_id): Path<String>,
    payload: Result<Json<WorkBranchComparisonRequestV1>, JsonRejection>,
) -> WorkApiResult<astra_services::work::WorkBranchComparisonReport> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_branch_comparison_request",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let payload = payload
        .map_err(|_| {
            work_error(
                StatusCode::BAD_REQUEST,
                "invalid_work_branch_comparison_request",
                WorkApiErrorCategory::InvalidRequest,
                false,
                Vec::new(),
            )
        })?
        .0;
    let left_branch_id = WorkBranchId::parse(payload.left_branch_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_branch_comparison_request",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let right_branch_id = WorkBranchId::parse(payload.right_branch_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_branch_comparison_request",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let report = DatabaseWorkBranchComparisonService::new(pool)
        .compare(&owner_id, &work_id, &left_branch_id, &right_branch_id)
        .await
        .map_err(|error| match error {
            astra_services::work::WorkBranchComparisonError::SameBranch => work_error(
                StatusCode::BAD_REQUEST,
                "invalid_work_branch_comparison_request",
                WorkApiErrorCategory::InvalidRequest,
                false,
                Vec::new(),
            ),
            astra_services::work::WorkBranchComparisonError::NotFound => work_error(
                StatusCode::NOT_FOUND,
                "work_branch_not_found",
                WorkApiErrorCategory::NotFound,
                false,
                Vec::new(),
            ),
            error => {
                tracing::warn!(error = %error, "Work branch comparison degraded");
                work_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "work_branch_comparison_unavailable",
                    WorkApiErrorCategory::Degraded,
                    true,
                    vec![WorkApiActionHint::RetryRead],
                )
            }
        })?;
    Ok(Json(report))
}

/// Export one immutable, exact-base patch from the branch's Server-owned Git
/// workspace. Caller revisions are only optimistic concurrency facts; provider
/// identity, workspace authority, payload identity, and invocation identity are
/// all resolved by the Server.
pub(super) async fn post_work_branch_patch_artifact_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id)): Path<(String, String)>,
    payload: Result<Json<WorkPatchArtifactExportRequestV1>, JsonRejection>,
) -> WorkApiResult<astra_services::work::WorkPatchArtifact> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_work_patch_artifact_request())?;
    let branch_id =
        WorkBranchId::parse(branch_id).map_err(|_| invalid_work_patch_artifact_request())?;
    let payload = payload
        .map_err(|_| invalid_work_patch_artifact_request())?
        .0;
    if !valid_work_request_id(&payload.request_id) {
        return Err(invalid_work_patch_artifact_request());
    }
    let request_id = WorkChangeRef::parse(payload.request_id)
        .map_err(|_| invalid_work_patch_artifact_request())?;
    let expected_branch_revision = WorkBranchRevision::new(payload.expected_branch_revision)
        .map_err(|_| invalid_work_patch_artifact_request())?;
    let expected_graph_revision = GraphRevision::new(payload.expected_graph_revision)
        .map_err(|_| invalid_work_patch_artifact_request())?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    super::work_patch_export_runtime::export_work_patch(
        pool,
        super::work_patch_export_runtime::WorkPatchExportCommand {
            owner_id,
            work_id,
            branch_id,
            request_id,
            expected_branch_revision,
            expected_graph_revision,
        },
    )
    .await
    .map(Json)
    .map_err(map_work_patch_export_error)
}

/// Discover immutable review results without loading their potentially large
/// diff bodies. Content remains an explicit per-artifact request.
pub(super) async fn get_work_branch_patch_artifacts_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id)): Path<(String, String)>,
    query: Result<Query<WorkPatchArtifactsQueryV1>, QueryRejection>,
) -> WorkApiResult<astra_services::work::WorkPatchArtifactPage> {
    require_work_api_major(&headers)?;
    let Query(query) = query.map_err(|_| invalid_work_patch_artifact_request())?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_work_patch_artifact_request())?;
    let branch_id =
        WorkBranchId::parse(branch_id).map_err(|_| invalid_work_patch_artifact_request())?;
    let before = match (query.before_created_at, query.before_patch_artifact_id) {
        (None, None) => None,
        (Some(created_at), Some(patch_artifact_id)) => {
            Some(astra_services::work::WorkPatchArtifactCursor {
                created_at,
                patch_artifact_id: WorkPatchArtifactId::parse(patch_artifact_id)
                    .map_err(|_| invalid_work_patch_artifact_request())?,
            })
        }
        _ => return Err(invalid_work_patch_artifact_request()),
    };
    let limit = astra_services::work::WorkPatchArtifactPageLimit::new(
        query.limit.unwrap_or(WORK_PATCH_ARTIFACT_DEFAULT_LIMIT),
    )
    .map_err(|_| invalid_work_patch_artifact_request())?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    DatabaseWorkRepository::new(pool)
        .list_patch_artifacts(astra_services::work::WorkPatchArtifactQuery {
            owner_id,
            work_id,
            branch_id,
            before,
            limit,
        })
        .await
        .map(Json)
        .map_err(|error| match error {
            WorkRepositoryError::NotFound => work_error(
                StatusCode::NOT_FOUND,
                "work_branch_not_found",
                WorkApiErrorCategory::NotFound,
                false,
                Vec::new(),
            ),
            error => {
                tracing::warn!(error = %error, "Work patch artifact page degraded");
                work_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "work_patch_artifacts_unavailable",
                    WorkApiErrorCategory::Degraded,
                    true,
                    vec![WorkApiActionHint::RetryRead],
                )
            }
        })
}

fn map_work_patch_export_error(
    error: super::work_patch_export_runtime::WorkPatchExportError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    use super::work_patch_export_runtime::WorkPatchExportError;
    match error {
        WorkPatchExportError::NotFound => work_error(
            StatusCode::NOT_FOUND,
            "work_branch_not_found",
            WorkApiErrorCategory::NotFound,
            false,
            Vec::new(),
        ),
        WorkPatchExportError::BasisConflict => work_error(
            StatusCode::CONFLICT,
            "work_patch_export_basis_conflict",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        WorkPatchExportError::IdempotencyConflict => work_error(
            StatusCode::CONFLICT,
            "work_patch_export_idempotency_conflict",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        WorkPatchExportError::NoChanges => work_error(
            StatusCode::CONFLICT,
            "work_patch_export_has_no_changes",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        WorkPatchExportError::TooLarge => work_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "work_patch_export_too_large",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        ),
        WorkPatchExportError::PayloadUnsupported => work_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "work_patch_export_payload_unsupported",
            WorkApiErrorCategory::Degraded,
            false,
            Vec::new(),
        ),
        WorkPatchExportError::Unavailable(reason) => {
            tracing::warn!(%reason, "Work patch export degraded");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_patch_export_unavailable",
                WorkApiErrorCategory::Availability,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )
        }
    }
}

/// Read immutable patch provenance by Work identity. Internal session storage
/// identity is deliberately omitted from the serialized domain object.
pub(super) async fn get_work_branch_patch_artifact_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id, patch_artifact_id)): Path<(String, String, String)>,
) -> WorkApiResult<astra_services::work::WorkPatchArtifact> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_work_patch_artifact_request())?;
    let branch_id =
        WorkBranchId::parse(branch_id).map_err(|_| invalid_work_patch_artifact_request())?;
    let patch_artifact_id = WorkPatchArtifactId::parse(patch_artifact_id)
        .map_err(|_| invalid_work_patch_artifact_request())?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let artifact = DatabaseWorkRepository::new(pool)
        .load_patch_artifact(&owner_id, &work_id, &patch_artifact_id)
        .await
        .map_err(|error| match error {
            WorkRepositoryError::NotFound => work_error(
                StatusCode::NOT_FOUND,
                "work_patch_artifact_not_found",
                WorkApiErrorCategory::NotFound,
                false,
                Vec::new(),
            ),
            error => {
                tracing::warn!(error = %error, "Work patch artifact read degraded");
                work_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "work_patch_artifact_unavailable",
                    WorkApiErrorCategory::Degraded,
                    true,
                    vec![WorkApiActionHint::RetryRead],
                )
            }
        })?
        .filter(|artifact| artifact.branch_id == branch_id)
        .ok_or_else(|| {
            work_error(
                StatusCode::NOT_FOUND,
                "work_patch_artifact_not_found",
                WorkApiErrorCategory::NotFound,
                false,
                Vec::new(),
            )
        })?;
    Ok(Json(artifact))
}

/// Read reviewable patch bytes through the Work aggregate. The repository
/// revalidates the backing artifact against its immutable provenance before
/// any bytes are returned.
pub(super) async fn get_work_branch_patch_artifact_content_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id, patch_artifact_id)): Path<(String, String, String)>,
) -> Result<Response, (StatusCode, Json<WorkApiErrorV1>)> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_work_patch_artifact_request())?;
    let branch_id =
        WorkBranchId::parse(branch_id).map_err(|_| invalid_work_patch_artifact_request())?;
    let patch_artifact_id = WorkPatchArtifactId::parse(patch_artifact_id)
        .map_err(|_| invalid_work_patch_artifact_request())?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let content = DatabaseWorkRepository::new(pool)
        .load_patch_artifact_content(&owner_id, &work_id, &branch_id, &patch_artifact_id)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "Work patch artifact content read degraded");
            let (category, retryable, hints) = match error {
                WorkRepositoryError::Persistence { .. } => (
                    WorkApiErrorCategory::Availability,
                    true,
                    vec![WorkApiActionHint::RetryRead],
                ),
                _ => (WorkApiErrorCategory::Degraded, false, Vec::new()),
            };
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_patch_artifact_content_unavailable",
                category,
                retryable,
                hints,
            )
        })?
        .ok_or_else(|| {
            work_error(
                StatusCode::NOT_FOUND,
                "work_patch_artifact_not_found",
                WorkApiErrorCategory::NotFound,
                false,
                Vec::new(),
            )
        })?;
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/x-diff; charset=utf-8")
        .header("content-length", content.artifact.payload_bytes)
        .header(
            "etag",
            format!("\"{}\"", content.artifact.payload_hash.as_str()),
        )
        .header("cache-control", "private, max-age=31536000, immutable")
        .body(axum::body::Body::from(content.data))
        .map_err(|error| {
            tracing::warn!(error = %error, "Work patch artifact response construction failed");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_patch_artifact_content_unavailable",
                WorkApiErrorCategory::Degraded,
                true,
                vec![WorkApiActionHint::RetryRead],
            )
        })
}

fn invalid_work_patch_artifact_request() -> (StatusCode, Json<WorkApiErrorV1>) {
    work_error(
        StatusCode::BAD_REQUEST,
        "invalid_work_patch_artifact_request",
        WorkApiErrorCategory::InvalidRequest,
        false,
        Vec::new(),
    )
}

pub(super) async fn post_work_patch_materialization_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id)): Path<(String, String)>,
    payload: Result<Json<WorkPatchMaterializationRequestV1>, JsonRejection>,
) -> Result<
    (
        StatusCode,
        Json<astra_services::work::WorkPatchMaterializationOperation>,
    ),
    (StatusCode, Json<WorkApiErrorV1>),
> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id =
        WorkId::parse(work_id).map_err(|_| invalid_work_patch_materialization_request())?;
    let branch_id =
        WorkBranchId::parse(branch_id).map_err(|_| invalid_work_patch_materialization_request())?;
    let payload = payload
        .map_err(|_| invalid_work_patch_materialization_request())?
        .0;
    if !valid_work_request_id(&payload.request_id) {
        return Err(invalid_work_patch_materialization_request());
    }
    let request_id = WorkChangeRef::parse(payload.request_id)
        .map_err(|_| invalid_work_patch_materialization_request())?;
    let request = astra_services::work::WorkPatchMaterializationRequest {
        owner_id,
        work_id: work_id.clone(),
        request_id: request_id.clone(),
        patch_artifact_id: WorkPatchArtifactId::parse(payload.patch_artifact_id)
            .map_err(|_| invalid_work_patch_materialization_request())?,
        target_branch_id: branch_id,
        expected_target_branch_revision: WorkBranchRevision::new(
            payload.expected_target_branch_revision,
        )
        .map_err(|_| invalid_work_patch_materialization_request())?,
        expected_target_graph_revision: GraphRevision::new(payload.expected_target_graph_revision)
            .map_err(|_| invalid_work_patch_materialization_request())?,
        // The authenticated command is the policy decision. Provider routing
        // is resolved by the Server from the target branch workspace; neither
        // authority may be asserted by an untrusted caller.
        provider_ref: WorkMaterializationProviderRef::parse(
            astra_services::work::SERVER_GIT_WORKTREE_MATERIALIZATION_PROVIDER_REF,
        )
        .expect("static provider identity"),
        policy_decision_ref: request_id.clone(),
    };
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let operation = DatabaseWorkPatchMaterializationService::new(pool)
        .admit(&request)
        .await
        .map_err(map_work_patch_materialization_error)?;
    let status = if operation.state == astra_services::work::WorkPatchMaterializationState::Pending
    {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(operation)))
}

pub(super) async fn get_work_patch_materializations_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id)): Path<(String, String)>,
    query: Result<Query<astra_server_types::WorkPatchMaterializationsQueryV1>, QueryRejection>,
) -> WorkApiResult<astra_services::work::WorkPatchMaterializationPage> {
    require_work_api_major(&headers)?;
    let Query(query) = query.map_err(|_| invalid_work_patch_materialization_request())?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id =
        WorkId::parse(work_id).map_err(|_| invalid_work_patch_materialization_request())?;
    let target_branch_id =
        WorkBranchId::parse(branch_id).map_err(|_| invalid_work_patch_materialization_request())?;
    let source_branch_id = WorkBranchId::parse(query.source_branch_id)
        .map_err(|_| invalid_work_patch_materialization_request())?;
    let before = match (query.before_created_at, query.before_operation_id) {
        (None, None) => None,
        (Some(created_at), Some(operation_id)) => {
            Some(astra_services::work::WorkPatchMaterializationCursor {
                created_at,
                operation_id: WorkPatchMaterializationId::parse(operation_id)
                    .map_err(|_| invalid_work_patch_materialization_request())?,
            })
        }
        _ => return Err(invalid_work_patch_materialization_request()),
    };
    let limit = astra_services::work::WorkPatchMaterializationPageLimit::new(
        query
            .limit
            .unwrap_or(WORK_PATCH_MATERIALIZATION_DEFAULT_LIMIT),
    )
    .map_err(|_| invalid_work_patch_materialization_request())?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    DatabaseWorkPatchMaterializationService::new(pool)
        .list_for_source(astra_services::work::WorkPatchMaterializationQuery {
            owner_id,
            work_id,
            target_branch_id,
            source_branch_id,
            before,
            limit,
        })
        .await
        .map(Json)
        .map_err(map_work_patch_materialization_read_error)
}

pub(super) async fn get_work_patch_materialization_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id, operation_id)): Path<(String, String, String)>,
) -> WorkApiResult<astra_services::work::WorkPatchMaterializationOperation> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id =
        WorkId::parse(work_id).map_err(|_| invalid_work_patch_materialization_request())?;
    let branch_id =
        WorkBranchId::parse(branch_id).map_err(|_| invalid_work_patch_materialization_request())?;
    let operation_id = WorkPatchMaterializationId::parse(operation_id)
        .map_err(|_| invalid_work_patch_materialization_request())?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let operation = DatabaseWorkPatchMaterializationService::new(pool)
        .load(&owner_id, &work_id, &branch_id, &operation_id)
        .await
        .map_err(map_work_patch_materialization_read_error)?;
    Ok(Json(operation))
}

pub(super) async fn delete_work_patch_materialization_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id, operation_id)): Path<(String, String, String)>,
) -> Result<StatusCode, (StatusCode, Json<WorkApiErrorV1>)> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id =
        WorkId::parse(work_id).map_err(|_| invalid_work_patch_materialization_request())?;
    let branch_id =
        WorkBranchId::parse(branch_id).map_err(|_| invalid_work_patch_materialization_request())?;
    let operation_id = WorkPatchMaterializationId::parse(operation_id)
        .map_err(|_| invalid_work_patch_materialization_request())?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let operation = DatabaseWorkPatchMaterializationService::new(pool)
        .abort(&owner_id, &work_id, &branch_id, &operation_id)
        .await
        .map_err(map_work_patch_materialization_error)?;
    debug_assert_eq!(operation.target_branch_id, branch_id);
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn post_work_patch_commit_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id)): Path<(String, String)>,
    payload: Result<Json<WorkPatchCommitRequestV1>, JsonRejection>,
) -> Result<
    (
        StatusCode,
        Json<astra_services::work::WorkPatchCommitOperation>,
    ),
    (StatusCode, Json<WorkApiErrorV1>),
> {
    require_work_api_major(&headers)?;
    let (owner_id, user) = authenticated_work_user(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_work_patch_commit_request())?;
    let branch_id =
        WorkBranchId::parse(branch_id).map_err(|_| invalid_work_patch_commit_request())?;
    let payload = payload.map_err(|_| invalid_work_patch_commit_request())?.0;
    if !valid_work_request_id(&payload.request_id) {
        return Err(invalid_work_patch_commit_request());
    }
    let request_id = WorkChangeRef::parse(payload.request_id)
        .map_err(|_| invalid_work_patch_commit_request())?;
    let author_name = user
        .display_name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(user.username);
    let request = astra_services::work::WorkPatchCommitRequest {
        owner_id,
        work_id: work_id.clone(),
        request_id: request_id.clone(),
        target_branch_id: branch_id,
        patch_artifact_id: WorkPatchArtifactId::parse(payload.patch_artifact_id)
            .map_err(|_| invalid_work_patch_commit_request())?,
        expected_target_branch_revision: WorkBranchRevision::new(
            payload.expected_target_branch_revision,
        )
        .map_err(|_| invalid_work_patch_commit_request())?,
        expected_target_graph_revision: GraphRevision::new(payload.expected_target_graph_revision)
            .map_err(|_| invalid_work_patch_commit_request())?,
        message: payload.message,
        author_name,
        author_email: user.email,
        provider_ref: WorkPatchCommitProviderRef::parse(
            astra_services::work::SERVER_GIT_WORKTREE_COMMIT_PROVIDER_REF,
        )
        .expect("static commit provider identity"),
        policy_decision_ref: request_id,
    };
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let operation = DatabaseWorkPatchCommitService::new(pool)
        .admit(&request)
        .await
        .map_err(map_work_patch_commit_error)?;
    let status = if operation.state == astra_services::work::WorkPatchCommitState::Pending {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(operation)))
}

pub(super) async fn get_work_patch_commits_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id)): Path<(String, String)>,
    query: Result<Query<WorkPatchCommitsQueryV1>, QueryRejection>,
) -> WorkApiResult<astra_services::work::WorkPatchCommitPage> {
    require_work_api_major(&headers)?;
    let Query(query) = query.map_err(|_| invalid_work_patch_commit_request())?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_work_patch_commit_request())?;
    let target_branch_id =
        WorkBranchId::parse(branch_id).map_err(|_| invalid_work_patch_commit_request())?;
    let before = match (query.before_created_at, query.before_operation_id) {
        (None, None) => None,
        (Some(created_at), Some(operation_id)) => {
            Some(astra_services::work::WorkPatchCommitCursor {
                created_at,
                operation_id: WorkPatchCommitId::parse(operation_id)
                    .map_err(|_| invalid_work_patch_commit_request())?,
            })
        }
        _ => return Err(invalid_work_patch_commit_request()),
    };
    let limit =
        WorkPatchCommitPageLimit::new(query.limit.unwrap_or(WORK_PATCH_COMMIT_DEFAULT_LIMIT))
            .map_err(|_| invalid_work_patch_commit_request())?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    DatabaseWorkPatchCommitService::new(pool)
        .list_for_target(astra_services::work::WorkPatchCommitQuery {
            owner_id,
            work_id,
            target_branch_id,
            before,
            limit,
        })
        .await
        .map(Json)
        .map_err(map_work_patch_commit_read_error)
}

pub(super) async fn get_work_patch_commit_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id, operation_id)): Path<(String, String, String)>,
) -> WorkApiResult<astra_services::work::WorkPatchCommitOperation> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_work_patch_commit_request())?;
    let branch_id =
        WorkBranchId::parse(branch_id).map_err(|_| invalid_work_patch_commit_request())?;
    let operation_id =
        WorkPatchCommitId::parse(operation_id).map_err(|_| invalid_work_patch_commit_request())?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    DatabaseWorkPatchCommitService::new(pool)
        .load(&owner_id, &work_id, &branch_id, &operation_id)
        .await
        .map(Json)
        .map_err(map_work_patch_commit_read_error)
}

pub(super) async fn delete_work_patch_commit_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id, operation_id)): Path<(String, String, String)>,
) -> Result<StatusCode, (StatusCode, Json<WorkApiErrorV1>)> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_work_patch_commit_request())?;
    let branch_id =
        WorkBranchId::parse(branch_id).map_err(|_| invalid_work_patch_commit_request())?;
    let operation_id =
        WorkPatchCommitId::parse(operation_id).map_err(|_| invalid_work_patch_commit_request())?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    DatabaseWorkPatchCommitService::new(pool)
        .abort(&owner_id, &work_id, &branch_id, &operation_id)
        .await
        .map_err(map_work_patch_commit_error)?;
    Ok(StatusCode::NO_CONTENT)
}

fn invalid_work_patch_commit_request() -> (StatusCode, Json<WorkApiErrorV1>) {
    work_error(
        StatusCode::BAD_REQUEST,
        "invalid_work_patch_commit_request",
        WorkApiErrorCategory::InvalidRequest,
        false,
        Vec::new(),
    )
}

fn map_work_patch_commit_error(
    error: astra_services::work::WorkPatchCommitError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    use astra_services::work::WorkPatchCommitError;
    match error {
        WorkPatchCommitError::InvalidPage | WorkPatchCommitError::InvalidMessage => {
            invalid_work_patch_commit_request()
        }
        WorkPatchCommitError::NotFound => work_error(
            StatusCode::NOT_FOUND,
            "work_patch_commit_not_found",
            WorkApiErrorCategory::NotFound,
            false,
            Vec::new(),
        ),
        WorkPatchCommitError::Conflict(_) => work_error(
            StatusCode::CONFLICT,
            "work_patch_commit_conflict",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        WorkPatchCommitError::ExecutorConflict => work_error(
            StatusCode::CONFLICT,
            "work_patch_commit_busy",
            WorkApiErrorCategory::Conflict,
            true,
            vec![WorkApiActionHint::RetryWrite],
        ),
        WorkPatchCommitError::InvalidTransition => work_error(
            StatusCode::CONFLICT,
            "work_patch_commit_already_dispatched",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        error @ WorkPatchCommitError::Database(_) => {
            tracing::warn!(error = %error, "Work patch commit persistence failed");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_write_unavailable",
                WorkApiErrorCategory::Availability,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )
        }
        error => {
            tracing::error!(error = %error, "Work patch commit degraded");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_patch_commit_degraded",
                WorkApiErrorCategory::Degraded,
                true,
                vec![WorkApiActionHint::RetryRead],
            )
        }
    }
}

fn map_work_patch_commit_read_error(
    error: astra_services::work::WorkPatchCommitError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    if matches!(
        error,
        astra_services::work::WorkPatchCommitError::Database(_)
    ) {
        tracing::warn!(error = %error, "Work patch commit read failed");
        return work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        );
    }
    map_work_patch_commit_error(error)
}

fn invalid_work_patch_materialization_request() -> (StatusCode, Json<WorkApiErrorV1>) {
    work_error(
        StatusCode::BAD_REQUEST,
        "invalid_work_patch_materialization_request",
        WorkApiErrorCategory::InvalidRequest,
        false,
        Vec::new(),
    )
}

fn map_work_patch_materialization_error(
    error: astra_services::work::WorkPatchMaterializationError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    use astra_services::work::WorkPatchMaterializationError;
    match error {
        WorkPatchMaterializationError::InvalidPage => invalid_work_patch_materialization_request(),
        WorkPatchMaterializationError::NotFound => work_error(
            StatusCode::NOT_FOUND,
            "work_patch_materialization_not_found",
            WorkApiErrorCategory::NotFound,
            false,
            Vec::new(),
        ),
        WorkPatchMaterializationError::Conflict(_)
        | WorkPatchMaterializationError::UnavailableTarget => work_error(
            StatusCode::CONFLICT,
            "work_patch_materialization_conflict",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        WorkPatchMaterializationError::VerificationRequired => work_error(
            StatusCode::CONFLICT,
            "work_patch_materialization_verification_required",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        WorkPatchMaterializationError::ExecutorConflict => work_error(
            StatusCode::CONFLICT,
            "work_patch_materialization_busy",
            WorkApiErrorCategory::Conflict,
            true,
            vec![WorkApiActionHint::RetryWrite],
        ),
        WorkPatchMaterializationError::InvalidTransition => work_error(
            StatusCode::CONFLICT,
            "work_patch_materialization_already_dispatched",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        error @ WorkPatchMaterializationError::Database(_) => {
            tracing::warn!(error = %error, "Work patch materialization persistence failed");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_write_unavailable",
                WorkApiErrorCategory::Availability,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )
        }
        error => {
            tracing::error!(error = %error, "Work patch materialization degraded");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_patch_materialization_degraded",
                WorkApiErrorCategory::Degraded,
                true,
                vec![WorkApiActionHint::RetryRead],
            )
        }
    }
}

fn map_work_patch_materialization_read_error(
    error: astra_services::work::WorkPatchMaterializationError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    if matches!(
        error,
        astra_services::work::WorkPatchMaterializationError::Database(_)
    ) {
        tracing::warn!(error = %error, "Work patch materialization read failed");
        return work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        );
    }
    map_work_patch_materialization_error(error)
}

/// Atomically select one exact, already-compared branch as the Work result.
/// Every mutable input is revision-pinned and evidence is re-derived inside
/// the same transaction as the delivery pointer CAS.
pub(super) async fn post_work_action_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(work_id): Path<String>,
    payload: Result<Json<WorkActionRequestV1>, JsonRejection>,
) -> WorkApiResult<astra_services::work::WorkDeliverySelectionReceipt> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_work_action_request())?;
    let payload = payload.map_err(|_| invalid_work_action_request())?.0;
    if !valid_work_request_id(&payload.request_id) {
        return Err(invalid_work_action_request());
    }
    let request_id =
        WorkChangeRef::parse(payload.request_id).map_err(|_| invalid_work_action_request())?;
    let expected_work_revision = WorkRevision::new(payload.expected_work_revision)
        .map_err(|_| invalid_work_action_request())?;
    let WorkActionV1::SelectDeliveryBranch {
        branch_id,
        expected_branch_revision,
        expected_goal_revision,
        expected_criteria_set_revision,
        expected_graph_revision,
        expected_subject,
        expected_evidence_manifest_hash,
    } = payload.action;
    let branch_id = WorkBranchId::parse(branch_id).map_err(|_| invalid_work_action_request())?;
    let expected_branch_revision = WorkBranchRevision::new(expected_branch_revision)
        .map_err(|_| invalid_work_action_request())?;
    let expected_goal_revision =
        GoalRevision::new(expected_goal_revision).map_err(|_| invalid_work_action_request())?;
    let expected_criteria_set_revision = CriterionSetRevision::new(expected_criteria_set_revision)
        .map_err(|_| invalid_work_action_request())?;
    let expected_graph_revision =
        GraphRevision::new(expected_graph_revision).map_err(|_| invalid_work_action_request())?;
    let expected_subject = expected_subject
        .map(|subject| {
            Ok(astra_services::work::WorkDeliverySelectionSubject {
                graph_revision: GraphRevision::new(subject.graph_revision)
                    .map_err(|_| invalid_work_action_request())?,
                subject_ref: WorkSubjectRef::parse(subject.subject_ref)
                    .map_err(|_| invalid_work_action_request())?,
                subject_revision: WorkContentHash::parse(subject.subject_revision)
                    .map_err(|_| invalid_work_action_request())?,
            })
        })
        .transpose()?;
    let expected_evidence_manifest_hash = WorkContentHash::parse(expected_evidence_manifest_hash)
        .map_err(|_| invalid_work_action_request())?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let receipt = DatabaseWorkRepository::new(pool)
        .select_delivery_branch(astra_services::work::WorkDeliverySelection {
            owner_id,
            work_id: work_id.clone(),
            request_id,
            branch_id,
            expected_work_revision,
            expected_branch_revision,
            expected_goal_revision,
            expected_criteria_set_revision,
            expected_graph_revision,
            expected_subject,
            expected_evidence_manifest_hash,
        })
        .await
        .map_err(|error| map_delivery_selection_error(work_id.as_str(), error))?;
    Ok(Json(receipt))
}

/// Archive or restore one exact branch without changing its immutable
/// conversation lineage. The aggregate and branch revisions advance together,
/// so clients never observe a half-applied retention transition.
pub(super) async fn post_work_branch_action_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id)): Path<(String, String)>,
    payload: Result<Json<WorkBranchActionRequestV1>, JsonRejection>,
) -> WorkApiResult<astra_services::work::WorkBranchRetentionReceipt> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_work_branch_action_request())?;
    let branch_id =
        WorkBranchId::parse(branch_id).map_err(|_| invalid_work_branch_action_request())?;
    let payload = payload.map_err(|_| invalid_work_branch_action_request())?.0;
    if !valid_work_request_id(&payload.request_id) {
        return Err(invalid_work_branch_action_request());
    }
    let request_id = WorkChangeRef::parse(payload.request_id)
        .map_err(|_| invalid_work_branch_action_request())?;
    let expected_work_revision = WorkRevision::new(payload.expected_work_revision)
        .map_err(|_| invalid_work_branch_action_request())?;
    let expected_branch_revision = WorkBranchRevision::new(payload.expected_branch_revision)
        .map_err(|_| invalid_work_branch_action_request())?;
    let kind = match payload.action {
        WorkBranchActionV1::Archive => WorkBranchRetentionKind::Archive,
        WorkBranchActionV1::Restore => WorkBranchRetentionKind::Restore,
    };
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let receipt = DatabaseWorkRepository::new(pool)
        .change_branch_retention(WorkBranchRetentionChange {
            owner_id,
            work_id: work_id.clone(),
            branch_id,
            request_id,
            kind,
            expected_work_revision,
            expected_branch_revision,
        })
        .await
        .map_err(|error| map_branch_retention_error(work_id.as_str(), error))?;
    Ok(Json(receipt))
}

pub(super) async fn post_work_branch_deletion_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id)): Path<(String, String)>,
    payload: Result<Json<WorkBranchDeletionRequestV1>, JsonRejection>,
) -> Result<
    (
        StatusCode,
        Json<astra_services::work::WorkBranchDeletionOperation>,
    ),
    (StatusCode, Json<WorkApiErrorV1>),
> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_work_branch_deletion_request())?;
    let branch_id =
        WorkBranchId::parse(branch_id).map_err(|_| invalid_work_branch_deletion_request())?;
    let payload = payload
        .map_err(|_| invalid_work_branch_deletion_request())?
        .0;
    let expected_work_revision = WorkRevision::new(payload.expected_work_revision)
        .map_err(|_| invalid_work_branch_deletion_request())?;
    let expected_branch_revision = WorkBranchRevision::new(payload.expected_branch_revision)
        .map_err(|_| invalid_work_branch_deletion_request())?;
    let pool = state.shared_pool.clone().ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let service = DatabaseWorkBranchDeletionService::new(pool);
    let admission = service
        .admit(&astra_services::work::WorkBranchDeletionRequest {
            request_id: payload.request_id,
            owner_id: owner_id.clone(),
            work_id: work_id.clone(),
            branch_id: branch_id.clone(),
            expected_work_revision,
            expected_branch_revision,
        })
        .await
        .map_err(map_work_branch_deletion_error)?;
    if admission.operation.state != astra_services::work::WorkBranchDeletionState::Pending {
        return Ok((StatusCode::CREATED, Json(admission.operation)));
    }
    let operation_id = admission.operation.operation_id.clone();
    let Some(executor_token) = service
        .claim_execution(&owner_id, &work_id, &branch_id, &operation_id)
        .await
        .map_err(map_work_branch_deletion_error)?
    else {
        let operation = service
            .load(&owner_id, &work_id, &branch_id, &operation_id)
            .await
            .map_err(map_work_branch_deletion_error)?;
        let status = if operation.state == astra_services::work::WorkBranchDeletionState::Pending {
            StatusCode::ACCEPTED
        } else {
            StatusCode::OK
        };
        return Ok((status, Json(operation)));
    };
    let claim = astra_services::work::WorkBranchDeletionExecutionClaim {
        owner_id,
        work_id,
        branch_id,
        session_id: admission.session_id,
        operation: admission.operation,
        executor_token,
    };
    match crate::server::work_branch_deletion_runtime::drive_claimed_work_branch_deletion(
        &service,
        &state.execution.run_lifecycle_service,
        &claim,
    )
    .await
    {
        Ok(crate::server::work_branch_deletion_runtime::WorkBranchDeletionDriveResult::Terminal(
            operation,
        )) => Ok((StatusCode::OK, Json(operation))),
        Ok(crate::server::work_branch_deletion_runtime::WorkBranchDeletionDriveResult::Deferred(
            operation,
        )) => Ok((StatusCode::ACCEPTED, Json(operation))),
        Err(
            crate::server::work_branch_deletion_runtime::WorkBranchDeletionDriveError::CancellationUnavailable(
                status,
            ),
        ) => Err(work_error(
            status,
            "work_branch_run_cancellation_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )),
        Err(crate::server::work_branch_deletion_runtime::WorkBranchDeletionDriveError::Service(
            error,
        )) => Err(map_work_branch_deletion_error(error)),
    }
}

pub(super) async fn get_work_branch_deletion_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id, operation_id)): Path<(String, String, String)>,
) -> WorkApiResult<astra_services::work::WorkBranchDeletionOperation> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| invalid_work_branch_deletion_request())?;
    let branch_id =
        WorkBranchId::parse(branch_id).map_err(|_| invalid_work_branch_deletion_request())?;
    if !valid_work_operation_id(&operation_id) {
        return Err(invalid_work_branch_deletion_request());
    }
    let pool = state.shared_pool.clone().ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let operation = DatabaseWorkBranchDeletionService::new(pool)
        .load(&owner_id, &work_id, &branch_id, &operation_id)
        .await
        .map_err(map_work_branch_deletion_error)?;
    Ok(Json(operation))
}

fn invalid_work_branch_deletion_request() -> (StatusCode, Json<WorkApiErrorV1>) {
    work_error(
        StatusCode::BAD_REQUEST,
        "invalid_work_branch_deletion_request",
        WorkApiErrorCategory::InvalidRequest,
        false,
        Vec::new(),
    )
}

fn map_work_branch_deletion_error(
    error: astra_services::work::WorkBranchDeletionError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    use astra_services::work::WorkBranchDeletionError;
    match error {
        WorkBranchDeletionError::Invalid(_) => invalid_work_branch_deletion_request(),
        WorkBranchDeletionError::NotFound | WorkBranchDeletionError::OperationNotFound => {
            work_error(
                StatusCode::NOT_FOUND,
                "work_branch_deletion_not_found",
                WorkApiErrorCategory::NotFound,
                false,
                Vec::new(),
            )
        }
        WorkBranchDeletionError::IdempotencyMismatch => work_error(
            StatusCode::CONFLICT,
            "idempotency_mismatch",
            WorkApiErrorCategory::Conflict,
            false,
            Vec::new(),
        ),
        WorkBranchDeletionError::DeletionInProgress | WorkBranchDeletionError::ExecutorConflict => {
            work_error(
                StatusCode::CONFLICT,
                "work_branch_deletion_conflict",
                WorkApiErrorCategory::Conflict,
                false,
                vec![WorkApiActionHint::RefreshWork],
            )
        }
        WorkBranchDeletionError::ActiveRuns | WorkBranchDeletionError::LineagePending { .. } => {
            work_error(
                StatusCode::ACCEPTED,
                "work_branch_deletion_converging",
                WorkApiErrorCategory::Conflict,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )
        }
        error @ (WorkBranchDeletionError::Database { .. }
        | WorkBranchDeletionError::SessionCleanup(_)) => {
            tracing::warn!(error = %error, "Work branch deletion unavailable");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_branch_deletion_unavailable",
                WorkApiErrorCategory::Availability,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )
        }
        error @ WorkBranchDeletionError::NeedsRepair(_) => {
            tracing::error!(error = %error, "Work branch deletion requires repair");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_branch_deletion_degraded",
                WorkApiErrorCategory::Degraded,
                false,
                vec![WorkApiActionHint::RefreshWork],
            )
        }
    }
}

fn invalid_work_branch_action_request() -> (StatusCode, Json<WorkApiErrorV1>) {
    work_error(
        StatusCode::BAD_REQUEST,
        "invalid_work_branch_action_request",
        WorkApiErrorCategory::InvalidRequest,
        false,
        Vec::new(),
    )
}

fn map_branch_retention_error(
    work_id: &str,
    error: WorkRepositoryError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    match error {
        WorkRepositoryError::NotFound | WorkRepositoryError::Archived => work_error(
            StatusCode::NOT_FOUND,
            "work_branch_not_found",
            WorkApiErrorCategory::NotFound,
            false,
            Vec::new(),
        ),
        WorkRepositoryError::BranchDeleting => work_error(
            StatusCode::CONFLICT,
            "work_branch_deleting",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        WorkRepositoryError::DeliveryBranchProtected => work_error(
            StatusCode::CONFLICT,
            "work_delivery_branch_protected",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        WorkRepositoryError::BranchActive => work_error(
            StatusCode::CONFLICT,
            "work_branch_active",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        WorkRepositoryError::StaleBranchRetention {
            resource: astra_services::work::WorkBranchRetentionBasisResource::RequestPayload,
        } => work_error(
            StatusCode::CONFLICT,
            "idempotency_mismatch",
            WorkApiErrorCategory::Conflict,
            false,
            Vec::new(),
        ),
        WorkRepositoryError::StaleBranchRetention { .. } => work_error(
            StatusCode::CONFLICT,
            "work_branch_retention_conflict",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        error @ WorkRepositoryError::Persistence { .. } => {
            tracing::warn!(work_id, error = %error, "Work branch retention failed");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_write_unavailable",
                WorkApiErrorCategory::Availability,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )
        }
        error => {
            tracing::error!(work_id, error = %error, "Work branch retention degraded");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "causal_projection_degraded",
                WorkApiErrorCategory::Degraded,
                true,
                vec![WorkApiActionHint::RetryRead],
            )
        }
    }
}

fn invalid_work_action_request() -> (StatusCode, Json<WorkApiErrorV1>) {
    work_error(
        StatusCode::BAD_REQUEST,
        "invalid_work_action_request",
        WorkApiErrorCategory::InvalidRequest,
        false,
        Vec::new(),
    )
}

fn map_delivery_selection_error(
    work_id: &str,
    error: WorkRepositoryError,
) -> (StatusCode, Json<WorkApiErrorV1>) {
    match error {
        WorkRepositoryError::NotFound | WorkRepositoryError::Archived => work_error(
            StatusCode::NOT_FOUND,
            "work_branch_not_found",
            WorkApiErrorCategory::NotFound,
            false,
            Vec::new(),
        ),
        WorkRepositoryError::BranchDeleting => work_error(
            StatusCode::CONFLICT,
            "work_branch_deleting",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        WorkRepositoryError::StaleDeliverySelection {
            resource: astra_services::work::WorkDeliverySelectionBasisResource::RequestPayload,
        } => work_error(
            StatusCode::CONFLICT,
            "idempotency_mismatch",
            WorkApiErrorCategory::Conflict,
            false,
            Vec::new(),
        ),
        WorkRepositoryError::StaleDeliverySelection { .. } => work_error(
            StatusCode::CONFLICT,
            "work_delivery_selection_conflict",
            WorkApiErrorCategory::Conflict,
            false,
            vec![WorkApiActionHint::RefreshWork],
        ),
        error @ WorkRepositoryError::Persistence { .. } => {
            tracing::warn!(work_id, error = %error, "Work delivery selection failed");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "work_write_unavailable",
                WorkApiErrorCategory::Availability,
                true,
                vec![WorkApiActionHint::RetryWrite],
            )
        }
        error => {
            tracing::error!(work_id, error = %error, "Work delivery selection degraded");
            work_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "causal_projection_degraded",
                WorkApiErrorCategory::Degraded,
                true,
                vec![WorkApiActionHint::RetryRead],
            )
        }
    }
}

/// Bounded, revision-pinned accepted Done-when page. Criterion definitions are
/// intentionally separate from the constant-size Work overview and loaded by
/// exact immutable member references rather than by scanning criterion history.
pub(super) async fn get_work_criteria_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(work_id): Path<String>,
    query: Result<Query<WorkCriteriaQueryV1>, QueryRejection>,
) -> WorkApiResult<WorkCriteriaResponseV1> {
    require_work_api_major(&headers)?;
    let Query(query) = query.map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_criteria_query",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let expected_revision = query
        .criteria_set_revision
        .map(astra_services::work::CriterionSetRevision::new)
        .transpose()
        .map_err(|_| {
            work_error(
                StatusCode::BAD_REQUEST,
                "invalid_work_criteria_cursor",
                WorkApiErrorCategory::InvalidRequest,
                false,
                Vec::new(),
            )
        })?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let criteria_query = WorkCriteriaQuery::new(
        owner_id,
        work_id.clone(),
        expected_revision,
        query.offset.unwrap_or_default(),
        query.limit.unwrap_or(WORK_CRITERIA_DEFAULT_LIMIT),
    )
    .map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_criteria_query",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let page = DatabaseWorkRepository::new(pool)
        .load_criteria_page(criteria_query)
        .await
        .map_err(|error| map_criteria_page_repository_error(work_id.as_str(), error))?;
    Ok(Json(WorkCriteriaResponseV1(page)))
}

/// Constant-cardinality discovery for provisional Done-when proposals. The
/// list carries only summaries; full definitions are loaded one proposal at a
/// time so eight near-limit payloads cannot amplify one refresh.
pub(super) async fn list_work_criteria_proposals_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id)): Path<(String, String)>,
) -> WorkApiResult<WorkCriteriaProposalListResponseV1> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let branch_id = WorkBranchId::parse(branch_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_branch_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let proposals = DatabaseWorkRepository::new(pool)
        .list_pending_criteria_proposals(&owner_id, &work_id, &branch_id)
        .await
        .map_err(|error| map_criteria_proposal_read_error(work_id.as_str(), error))?
        .iter()
        .map(criteria_proposal_summary)
        .collect();
    Ok(Json(WorkCriteriaProposalListResponseV1 {
        schema_version: 1,
        work_id,
        branch_id,
        proposals,
    }))
}

/// Load one exact provisional payload for review. Owner and branch checks are
/// structural; a proposal on another branch is indistinguishable from absent.
pub(super) async fn get_work_criteria_proposal_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id, proposal_id)): Path<(String, String, String)>,
) -> WorkApiResult<WorkCriteriaProposalDetailResponseV1> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let branch_id = WorkBranchId::parse(branch_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_branch_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let proposal_id = WorkProposalId::parse(proposal_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_proposal_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let proposal = DatabaseWorkRepository::new(pool)
        .load_criteria_proposal(&owner_id, &work_id, &proposal_id)
        .await
        .map_err(|error| map_criteria_proposal_read_error(work_id.as_str(), error))?
        .filter(|proposal| proposal.proposal.branch_id == branch_id)
        .ok_or_else(|| {
            work_error(
                StatusCode::NOT_FOUND,
                "work_criteria_proposal_not_found",
                WorkApiErrorCategory::NotFound,
                false,
                Vec::new(),
            )
        })?;
    Ok(Json(criteria_proposal_detail(proposal)))
}

/// Resolve one immutable proposal using its full revision/hash precondition.
/// Accept and reject share one public mutation endpoint and remain distinct
/// typed domain commands below the transport boundary.
pub(super) async fn put_work_criteria_proposal_decision_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id, proposal_id)): Path<(String, String, String)>,
    payload: Result<Json<WorkCriteriaProposalDecisionRequestV1>, JsonRejection>,
) -> WorkApiResult<WorkCriteriaProposalDetailResponseV1> {
    require_work_api_major(&headers)?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let branch_id = WorkBranchId::parse(branch_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_branch_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let proposal_id = WorkProposalId::parse(proposal_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_proposal_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let Json(request) = payload.map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_proposal_decision",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let decision = derive_criteria_proposal_decision(
        &owner_id,
        work_id.clone(),
        branch_id,
        proposal_id,
        request,
    )?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let repository = DatabaseWorkRepository::new(pool);
    let resolved = match decision {
        DerivedCriteriaProposalDecision::Accept(acceptance) => {
            repository.accept_criteria_proposal(acceptance).await
        }
        DerivedCriteriaProposalDecision::Reject(rejection) => {
            repository.reject_criteria_proposal(rejection).await
        }
    }
    .map_err(|error| map_criteria_proposal_decision_error(work_id.as_str(), error))?;
    Ok(Json(criteria_proposal_detail(resolved)))
}

/// Bounded Task Graph page for one public Work branch. Declared-work
/// pagination is revision-pinned; execution and evidence freshness reconcile
/// only exact durable Run/Check bindings, never transcript, legacy task, or
/// plan stores.
pub(super) async fn get_work_task_graph_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((work_id, branch_id)): Path<(String, String)>,
    query: Result<Query<WorkTaskGraphQueryV1>, QueryRejection>,
) -> WorkApiResult<WorkTaskGraphResponseV1> {
    require_work_api_major(&headers)?;
    let Query(query) = query.map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_task_graph_query",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let expected_graph_revision = query
        .graph_revision
        .map(GraphRevision::new)
        .transpose()
        .map_err(|_| {
            work_error(
                StatusCode::BAD_REQUEST,
                "invalid_work_task_graph_cursor",
                WorkApiErrorCategory::InvalidRequest,
                false,
                Vec::new(),
            )
        })?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let branch_id = WorkBranchId::parse(branch_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_branch_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let graph_query = WorkTaskGraphQuery::new(
        owner_id,
        work_id.clone(),
        branch_id,
        expected_graph_revision,
        query.item_offset.unwrap_or_default(),
        query
            .item_limit
            .unwrap_or(WORK_TASK_GRAPH_DEFAULT_ITEM_LIMIT),
        query.dependency_offset.unwrap_or_default(),
        query
            .dependency_limit
            .unwrap_or(WORK_TASK_GRAPH_DEFAULT_DEPENDENCY_LIMIT),
    )
    .map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_task_graph_query",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let page = DatabaseWorkRepository::new(pool)
        .load_task_graph_page(graph_query)
        .await
        .map_err(|error| map_task_graph_repository_error(work_id.as_str(), error))?;
    Ok(Json(WorkTaskGraphResponseV1(page)))
}

/// Monotonic user read receipt. The exact through-sequence is the idempotency
/// identity; this command never infers a cursor from page-open time or text.
pub(super) async fn put_work_read_cursor_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(work_id): Path<String>,
    payload: Result<Json<WorkReadCursorRequestV1>, JsonRejection>,
) -> WorkApiResult<WorkReadCursorResponseV1> {
    require_work_api_major(&headers)?;
    let Json(payload) = payload.map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_read_cursor_request",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let through_event_seq = WorkEventSeq::new(payload.through_event_seq).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_event_cursor",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_write_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryWrite],
        )
    })?;
    let repository = DatabaseWorkRepository::new(pool);
    let receipt = repository
        .advance_attention_cursor(WorkAttentionCursorAdvance {
            owner_id,
            work_id: work_id.clone(),
            kind: WorkAttentionCursorKind::Seen,
            through_event_seq,
        })
        .await
        .map_err(|error| map_cursor_repository_error(work_id.as_str(), error))?;
    let through_event_seq = receipt.seen_through_event_seq.ok_or_else(|| {
        tracing::error!(
            work_id = work_id.as_str(),
            "committed receipt has no seen cursor"
        );
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "causal_projection_degraded",
            WorkApiErrorCategory::Degraded,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let receipt_hash = receipt.seen_receipt_hash.ok_or_else(|| {
        tracing::error!(
            work_id = work_id.as_str(),
            "committed receipt has no seen receipt hash"
        );
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "causal_projection_degraded",
            WorkApiErrorCategory::Degraded,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    Ok(Json(WorkReadCursorResponseV1 {
        schema_version: 1,
        work_id,
        through_event_seq,
        receipt_revision: receipt.revision,
        receipt_hash,
        updated_at: receipt.updated_at,
    }))
}

/// Bounded semantic Work timeline. Reading never advances the durable seen
/// cursor; clients perform that distinct mutation through the read-cursor PUT.
pub(super) async fn get_work_events_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(work_id): Path<String>,
    query: Result<Query<WorkEventsQueryV1>, QueryRejection>,
) -> WorkApiResult<WorkEventPageResponseV1> {
    require_work_api_major(&headers)?;
    let Query(query) = query.map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_event_query",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let after_event_seq = query
        .after_event_seq
        .map(WorkEventSeq::new)
        .transpose()
        .map_err(|_| {
            work_error(
                StatusCode::BAD_REQUEST,
                "invalid_work_event_cursor",
                WorkApiErrorCategory::InvalidRequest,
                false,
                Vec::new(),
            )
        })?;
    let limit = WorkEventPageLimit::new(query.limit.unwrap_or(50)).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_event_limit",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let owner_id = authenticated_work_owner(&state, &headers).await?;
    let work_id = WorkId::parse(work_id).map_err(|_| {
        work_error(
            StatusCode::BAD_REQUEST,
            "invalid_work_id",
            WorkApiErrorCategory::InvalidRequest,
            false,
            Vec::new(),
        )
    })?;
    let pool = state.shared_pool.ok_or_else(|| {
        work_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "work_read_unavailable",
            WorkApiErrorCategory::Availability,
            true,
            vec![WorkApiActionHint::RetryRead],
        )
    })?;
    let repository = DatabaseWorkRepository::new(pool);
    let page = repository
        .list_events(WorkEventQuery {
            owner_id,
            work_id: work_id.clone(),
            after_event_seq,
            limit,
        })
        .await
        .map_err(|error| map_event_page_repository_error(work_id.as_str(), error))?;
    Ok(Json(WorkEventPageResponseV1 {
        schema_version: 1,
        page,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript_head(
        completed_turn: u32,
        journal_event_seq: u64,
        conversation_seq: u64,
        root: char,
    ) -> WorkConversationHeadV1 {
        WorkConversationHeadV1 {
            completed_turn,
            journal_event_seq,
            conversation_seq,
            canonical_root_hash: root.to_string().repeat(64),
            projection_schema: 2,
            compaction_generation: 0,
            config_version_id: None,
        }
    }

    #[test]
    fn transcript_sync_requires_an_exact_or_monotonic_causal_prefix() {
        let first = transcript_head(1, 1, 1, 'a');
        let second = transcript_head(2, 2, 2, 'b');
        assert_eq!(
            classify_work_transcript_sync(None, None),
            WorkBranchSyncStateV1::Current
        );
        assert_eq!(
            classify_work_transcript_sync(Some(&first), None),
            WorkBranchSyncStateV1::ProjectionStale
        );
        assert_eq!(
            classify_work_transcript_sync(Some(&first), Some(&first)),
            WorkBranchSyncStateV1::Current
        );
        assert_eq!(
            classify_work_transcript_sync(Some(&second), Some(&first)),
            WorkBranchSyncStateV1::ProjectionStale
        );

        let same_turn_different_root = transcript_head(2, 2, 2, 'c');
        assert_eq!(
            classify_work_transcript_sync(Some(&second), Some(&same_turn_different_root)),
            WorkBranchSyncStateV1::Corrupt
        );
        assert_eq!(
            classify_work_transcript_sync(Some(&first), Some(&second)),
            WorkBranchSyncStateV1::Corrupt
        );
        let impossible_prefix = transcript_head(1, 3, 1, 'd');
        assert_eq!(
            classify_work_transcript_sync(Some(&second), Some(&impossible_prefix)),
            WorkBranchSyncStateV1::Corrupt
        );
    }

    fn creation(owner: &str, request_id: &str, goal: &str) -> DerivedWorkCreation {
        creation_with_criteria(owner, request_id, goal, Vec::new())
    }

    fn creation_with_criteria(
        owner: &str,
        request_id: &str,
        goal: &str,
        criteria: Vec<WorkCreateCriterionV1>,
    ) -> DerivedWorkCreation {
        derive_work_creation(
            &WorkOwnerId::parse(owner).expect("owner"),
            WorkCreateRequestV1 {
                request_id: request_id.to_string(),
                goal: goal.to_string(),
                criteria,
            },
        )
        .expect("derived Work creation")
    }

    #[test]
    fn work_protocol_major_is_exact_and_has_typed_upgrade_guidance() {
        for invalid in [None, Some("0"), Some("1.0"), Some("2"), Some(" 1 ")] {
            let mut headers = HeaderMap::new();
            if let Some(value) = invalid {
                headers.insert(WORK_API_MAJOR_HEADER, value.parse().expect("header"));
            }
            let (status, Json(error)) =
                require_work_api_major(&headers).expect_err("reject version");
            assert_eq!(status, StatusCode::UPGRADE_REQUIRED);
            assert_eq!(error.code, "unsupported_client_version");
            assert!(matches!(
                error.action_hints.as_slice(),
                [WorkApiActionHint::UpgradeClient]
            ));
        }

        let mut headers = HeaderMap::new();
        headers.insert(WORK_API_MAJOR_HEADER, "1".parse().expect("header"));
        require_work_api_major(&headers).expect("supported version");
    }

    #[test]
    fn not_found_mapping_is_owner_neutral_and_non_retryable() {
        let (status, Json(error)) = map_repository_error("work-1", WorkRepositoryError::NotFound);
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(error.code, "work_not_found");
        assert!(!error.retryable);
        assert!(error.action_hints.is_empty());
    }

    fn criteria_proposal_decision_request(
        decision: WorkCriteriaProposalDecisionV1,
    ) -> WorkCriteriaProposalDecisionRequestV1 {
        WorkCriteriaProposalDecisionRequestV1 {
            request_id: "decision-request".to_string(),
            decision,
            payload_hash: format!("sha256:{}", "a".repeat(64)),
            expected_work_revision: 1,
            expected_goal_revision: 1,
            expected_criteria_set_revision: 1,
            expected_branch_revision: 1,
            expected_graph_revision: 1,
        }
    }

    fn derive_test_criteria_proposal_decision(
        request: WorkCriteriaProposalDecisionRequestV1,
    ) -> DerivedCriteriaProposalDecision {
        derive_criteria_proposal_decision(
            &WorkOwnerId::parse("owner").expect("owner"),
            WorkId::parse("work").expect("work"),
            WorkBranchId::parse("branch").expect("branch"),
            WorkProposalId::parse("proposal").expect("proposal"),
            request,
        )
        .expect("valid decision")
    }

    #[test]
    fn criteria_proposal_decision_identity_is_exact_and_action_specific() {
        let first = derive_test_criteria_proposal_decision(criteria_proposal_decision_request(
            WorkCriteriaProposalDecisionV1::Accept,
        ));
        let retry = derive_test_criteria_proposal_decision(criteria_proposal_decision_request(
            WorkCriteriaProposalDecisionV1::Accept,
        ));
        let reject = derive_test_criteria_proposal_decision(criteria_proposal_decision_request(
            WorkCriteriaProposalDecisionV1::Reject,
        ));

        let DerivedCriteriaProposalDecision::Accept(first) = first else {
            panic!("accept request must derive an acceptance");
        };
        let DerivedCriteriaProposalDecision::Accept(retry) = retry else {
            panic!("accept retry must derive an acceptance");
        };
        let DerivedCriteriaProposalDecision::Reject(reject) = reject else {
            panic!("reject request must derive a rejection");
        };
        assert_eq!(first, retry);
        assert_ne!(first.resolution_ref, reject.resolution_ref);
    }

    #[test]
    fn criteria_proposal_decision_rejects_invalid_exact_preconditions() {
        let mut invalid_request_id =
            criteria_proposal_decision_request(WorkCriteriaProposalDecisionV1::Accept);
        invalid_request_id.request_id = "bad\nrequest".to_string();

        let mut invalid_hash =
            criteria_proposal_decision_request(WorkCriteriaProposalDecisionV1::Accept);
        invalid_hash.payload_hash = "not-a-content-hash".to_string();

        let mut invalid_revision =
            criteria_proposal_decision_request(WorkCriteriaProposalDecisionV1::Accept);
        invalid_revision.expected_graph_revision = 0;

        for request in [invalid_request_id, invalid_hash, invalid_revision] {
            let result = derive_criteria_proposal_decision(
                &WorkOwnerId::parse("owner").expect("owner"),
                WorkId::parse("work").expect("work"),
                WorkBranchId::parse("branch").expect("branch"),
                WorkProposalId::parse("proposal").expect("proposal"),
                request,
            );
            let Err(error) = result else {
                panic!("invalid decision precondition must be rejected");
            };
            assert_eq!(error.0, StatusCode::BAD_REQUEST);
            assert_eq!(error.1.code, "invalid_work_proposal_decision");
        }
    }

    #[test]
    fn future_cursor_mapping_is_a_typed_refresh_conflict() {
        let (status, Json(error)) = map_cursor_repository_error(
            "work-1",
            WorkRepositoryError::EventCursorAhead {
                through_event_seq: 2,
                event_head: 1,
            },
        );
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(error.code, "work_event_cursor_ahead");
        assert!(!error.retryable);
        assert!(matches!(
            error.action_hints.as_slice(),
            [WorkApiActionHint::RefreshWork]
        ));
    }

    #[test]
    fn start_work_identity_is_owner_scoped_and_payload_exact() {
        let first = creation("owner-1", "request-1", "Ship the Work boundary.");
        let retry = creation("owner-1", "request-1", "Ship the Work boundary.");
        assert_eq!(first.work_id, retry.work_id);
        assert_eq!(first.branch_id, retry.branch_id);
        assert_eq!(first.original_intent_ref, retry.original_intent_ref);
        assert_eq!(
            first.work_id.as_str(),
            "work-83ed1e7974824f83346de9c4f86d59ca244b6fa3392fae0c"
        );
        assert_eq!(
            first.branch_id.as_str(),
            "branch-d21c5233bc897e0e840def11c628a5fb7780cc689d2a8487"
        );
        assert_eq!(
            first.original_intent_ref.as_str(),
            "work-create-19444fb277185e01c0df571dfd32a37d04fe0dc3bcd66fe4978234fd2d008030"
        );

        let changed = creation("owner-1", "request-1", "Ship a different result.");
        assert_eq!(first.work_id, changed.work_id);
        assert_eq!(first.branch_id, changed.branch_id);
        assert_ne!(first.original_intent_ref, changed.original_intent_ref);

        let other_owner = creation("owner-2", "request-1", "Ship the Work boundary.");
        assert_ne!(first.work_id, other_owner.work_id);
        assert_ne!(first.branch_id, other_owner.branch_id);
    }

    #[test]
    fn start_work_criteria_are_structural_order_independent_and_payload_exact() {
        let criteria = || {
            vec![
                WorkCreateCriterionV1::HumanReview {
                    criterion_id: "review".to_string(),
                    statement: "The result is reviewable.".to_string(),
                },
                WorkCreateCriterionV1::TestCheck {
                    criterion_id: "tests".to_string(),
                    statement: "Relevant tests pass.".to_string(),
                    command: "cargo test -p astra-runtime work_handlers".to_string(),
                },
            ]
        };
        let first = creation_with_criteria("owner", "request", "Ship it.", criteria());
        let mut reversed = criteria();
        reversed.reverse();
        let retry = creation_with_criteria("owner", "request", "Ship it.", reversed);
        assert_eq!(first.work_id, retry.work_id);
        assert_eq!(first.original_intent_ref, retry.original_intent_ref);

        let changed = creation_with_criteria(
            "owner",
            "request",
            "Ship it.",
            vec![WorkCreateCriterionV1::HumanReview {
                criterion_id: "review".to_string(),
                statement: "A materially different review contract.".to_string(),
            }],
        );
        assert_eq!(first.work_id, changed.work_id);
        assert_ne!(first.original_intent_ref, changed.original_intent_ref);

        let duplicate = derive_work_creation(
            &WorkOwnerId::parse("owner").expect("owner"),
            WorkCreateRequestV1 {
                request_id: "other-request".to_string(),
                goal: "Ship it.".to_string(),
                criteria: vec![
                    WorkCreateCriterionV1::HumanReview {
                        criterion_id: "same".to_string(),
                        statement: "First.".to_string(),
                    },
                    WorkCreateCriterionV1::HumanReview {
                        criterion_id: "same".to_string(),
                        statement: "Second.".to_string(),
                    },
                ],
            },
        )
        .expect_err("duplicate criterion identity");
        assert_eq!(duplicate.1.code, "invalid_work_criteria");
    }

    #[test]
    fn start_work_rejects_invalid_request_identity_and_unrepresentable_goal() {
        for request_id in ["", "bad\nrequest", "bad\u{85}request"] {
            let error = derive_work_creation(
                &WorkOwnerId::parse("owner").expect("owner"),
                WorkCreateRequestV1 {
                    request_id: request_id.to_string(),
                    goal: "Valid goal".to_string(),
                    criteria: Vec::new(),
                },
            )
            .expect_err("invalid request identity");
            assert_eq!(error.1.code, "invalid_work_create_request");
        }
        let error = derive_work_creation(
            &WorkOwnerId::parse("owner").expect("owner"),
            WorkCreateRequestV1 {
                request_id: "x".repeat(WORK_REQUEST_ID_MAX_BYTES + 1),
                goal: "Valid goal".to_string(),
                criteria: Vec::new(),
            },
        )
        .expect_err("oversized request identity");
        assert_eq!(error.1.code, "invalid_work_create_request");
        let error = derive_work_creation(
            &WorkOwnerId::parse("owner").expect("owner"),
            WorkCreateRequestV1 {
                request_id: "request".to_string(),
                goal: "x".repeat(8 * 1024 + 1),
                criteria: Vec::new(),
            },
        )
        .expect_err("goal must fit the canonical root item");
        assert_eq!(error.1.code, "invalid_work_goal");
    }

    fn turn(
        owner: &str,
        work: &str,
        branch: &str,
        request_id: &str,
        message: &str,
    ) -> DerivedWorkTurn {
        derive_work_turn(
            &WorkOwnerId::parse(owner).expect("owner"),
            &WorkId::parse(work).expect("work"),
            &WorkBranchId::parse(branch).expect("branch"),
            WorkTurnRequestV1 {
                request_id: request_id.to_string(),
                attachment_id: "attachment-1".to_string(),
                message: message.to_string(),
            },
        )
        .expect("derived Work turn")
    }

    #[test]
    fn work_turn_identity_is_owner_branch_and_payload_exact() {
        let first = turn("owner-1", "work-1", "branch-1", "request-1", "Continue.");
        let retry = turn("owner-1", "work-1", "branch-1", "request-1", "Continue.");
        assert_eq!(
            first.start_idempotency.run_id(),
            retry.start_idempotency.run_id()
        );
        assert_eq!(
            first.start_idempotency.request_fingerprint(),
            retry.start_idempotency.request_fingerprint()
        );
        assert_eq!(
            first.start_idempotency.kind(),
            RunStartIdempotencyKind::WorkTurn
        );
        let moved_attachment = derive_work_turn(
            &WorkOwnerId::parse("owner-1").expect("owner"),
            &WorkId::parse("work-1").expect("work"),
            &WorkBranchId::parse("branch-1").expect("branch"),
            WorkTurnRequestV1 {
                request_id: "request-1".to_string(),
                attachment_id: "attachment-2".to_string(),
                message: "Continue.".to_string(),
            },
        )
        .expect("same logical turn after an explicit control move");
        assert_eq!(
            first.start_idempotency, moved_attachment.start_idempotency,
            "attachment identity is an admission coordinate, not turn payload identity"
        );

        let changed = turn(
            "owner-1",
            "work-1",
            "branch-1",
            "request-1",
            "Continue differently.",
        );
        assert_eq!(
            first.start_idempotency.run_id(),
            changed.start_idempotency.run_id()
        );
        assert_ne!(
            first.start_idempotency.request_fingerprint(),
            changed.start_idempotency.request_fingerprint()
        );
        for scoped in [
            turn("owner-2", "work-1", "branch-1", "request-1", "Continue."),
            turn("owner-1", "work-2", "branch-1", "request-1", "Continue."),
            turn("owner-1", "work-1", "branch-2", "request-1", "Continue."),
        ] {
            assert_ne!(
                first.start_idempotency.run_id(),
                scoped.start_idempotency.run_id()
            );
        }
    }

    #[test]
    fn work_turns_request_one_server_owned_read_write_workspace() {
        let binding = server_owned_work_workspace_binding();
        assert_eq!(binding.kind, WorkspaceBindingRequestKind::ServerSandbox);
        assert_eq!(
            binding.authority,
            Some(WorkspaceAuthorityRequest::ReadWrite)
        );
        assert!(
            binding.root.is_none(),
            "clients must not choose a server path"
        );
        assert!(binding.source.is_none());
    }

    #[test]
    fn work_turn_rejects_ambiguous_or_unbounded_input() {
        for (request_id, attachment_id, message) in [
            ("", "attachment-1", "continue"),
            ("bad\nrequest", "attachment-1", "continue"),
            ("ok", "", "continue"),
            ("ok", "bad\nattachment", "continue"),
            ("ok", "attachment-1", " \n "),
        ] {
            let error = derive_work_turn(
                &WorkOwnerId::parse("owner").expect("owner"),
                &WorkId::parse("work").expect("work"),
                &WorkBranchId::parse("branch").expect("branch"),
                WorkTurnRequestV1 {
                    request_id: request_id.to_string(),
                    attachment_id: attachment_id.to_string(),
                    message: message.to_string(),
                },
            )
            .expect_err("invalid turn input");
            assert_eq!(error.1.code, "invalid_work_turn_request");
        }
        let error = derive_work_turn(
            &WorkOwnerId::parse("owner").expect("owner"),
            &WorkId::parse("work").expect("work"),
            &WorkBranchId::parse("branch").expect("branch"),
            WorkTurnRequestV1 {
                request_id: "request".to_string(),
                attachment_id: "attachment-1".to_string(),
                message: "x".repeat(WORK_TURN_MESSAGE_MAX_BYTES + 1),
            },
        )
        .expect_err("oversized turn message");
        assert_eq!(error.1.code, "invalid_work_turn_request");
    }

    #[test]
    fn work_turn_event_projection_removes_session_identity_and_fails_closed_on_nested_leaks() {
        let mut pending = None;
        let projected = project_work_turn_events(
            "run-1",
            vec![
                serde_json::json!({
                    "event_type": "run_started",
                    "data": {"run_id": "run-1", "session_id": "internal-session"}
                }),
                serde_json::json!({
                    "type": "session_info",
                    "session_id": "internal-session",
                    "run_id": "run-1"
                }),
                serde_json::json!({
                    "type": "warning",
                    "details": {"session_id": "internal-session"}
                }),
                serde_json::json!({"event_type": "text_delta", "data": {"chunk": "hello"}}),
            ],
            &mut pending,
        );
        assert_eq!(projected.len(), 2);
        assert_eq!(projected[0]["type"], "run_started");
        assert_eq!(projected[0]["run_id"], "run-1");
        assert!(!contains_structural_field(&projected[0], "session_id"));
        assert_eq!(
            projected[1],
            serde_json::json!({"type": "text_delta", "content": "hello"})
        );
    }

    #[test]
    fn work_turn_projects_committed_task_graph_revision_immediately() {
        let mut pending = None;
        let projected = project_work_turn_events(
            "run-1",
            vec![serde_json::json!({
                "type": "tool_call_end",
                "tool": "propose_work_plan",
                "call_id": "call-1",
                "success": true,
                "result": serde_json::json!({
                    "status": "accepted",
                    "proposal_id": "proposal-1",
                    "result_branch_revision": 4,
                    "result_graph_revision": 3
                }).to_string()
            })],
            &mut pending,
        );

        assert_eq!(projected.len(), 2);
        assert_eq!(projected[0]["type"], "tool_call_end");
        assert_eq!(
            projected[1],
            serde_json::json!({
                "type": "work_task_graph_changed",
                "schema_version": 1,
                "graph_revision": 3,
                "branch_revision": 4
            })
        );
    }

    #[test]
    fn work_turn_does_not_infer_graph_changes_from_prose_or_pending_proposals() {
        let mut pending = None;
        let projected = project_work_turn_events(
            "run-1",
            vec![
                serde_json::json!({
                    "type": "text_delta",
                    "content": "Plan accepted at graph revision 99"
                }),
                serde_json::json!({
                    "type": "tool_call_end",
                    "tool": "propose_work_plan",
                    "call_id": "call-1",
                    "success": true,
                    "result": serde_json::json!({
                        "status": "pending",
                        "result_branch_revision": 4,
                        "result_graph_revision": 3
                    }).to_string()
                }),
                serde_json::json!({
                    "type": "tool_call_end",
                    "tool": "another_tool",
                    "call_id": "call-2",
                    "success": true,
                    "result": serde_json::json!({
                        "status": "accepted",
                        "result_branch_revision": 5,
                        "result_graph_revision": 4
                    }).to_string()
                }),
            ],
            &mut pending,
        );

        assert_eq!(projected.len(), 3);
        assert!(projected.iter().all(|event| {
            event.get("type").and_then(serde_json::Value::as_str) != Some("work_task_graph_changed")
        }));
    }

    #[test]
    fn work_turn_runtime_errors_map_only_typed_codes() {
        let mismatch = map_turn_start_error(
            "work-1",
            error_response_coded(
                StatusCode::CONFLICT,
                "arbitrary detail",
                "idempotency_mismatch",
            ),
        );
        assert_eq!(mismatch.0, StatusCode::CONFLICT);
        assert_eq!(mismatch.1.code, "idempotency_mismatch");

        let catalog_race = map_turn_start_error(
            "work-1",
            error_response_coded(
                StatusCode::NOT_FOUND,
                "arbitrary detail",
                "model_offering_unavailable",
            ),
        );
        assert_eq!(catalog_race.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(catalog_race.1.code, "provider_unavailable");
        assert!(catalog_race.1.retryable);

        let missing_root_item = map_turn_start_error(
            "work-1",
            error_response_coded(
                StatusCode::NOT_FOUND,
                "arbitrary detail",
                "work_item_binding_not_found",
            ),
        );
        assert_eq!(missing_root_item.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(missing_root_item.1.code, "causal_projection_degraded");

        let same_words_without_code = map_turn_start_error(
            "work-1",
            error_response(StatusCode::CONFLICT, "idempotency_mismatch"),
        );
        assert_eq!(same_words_without_code.1.code, "work_turn_unavailable");
    }

    #[test]
    fn committed_work_cursor_validation_is_structural_and_bounded() {
        let cursor = WorkConversationHeadV1 {
            completed_turn: 1,
            journal_event_seq: 1,
            conversation_seq: 1,
            canonical_root_hash: "a".repeat(64),
            projection_schema: 2,
            compaction_generation: 0,
            config_version_id: None,
        };
        assert!(valid_committed_work_cursor(&cursor));
        for invalid in [
            WorkConversationHeadV1 {
                completed_turn: 0,
                ..cursor.clone()
            },
            WorkConversationHeadV1 {
                journal_event_seq: 0,
                ..cursor.clone()
            },
            WorkConversationHeadV1 {
                conversation_seq: 0,
                ..cursor.clone()
            },
            WorkConversationHeadV1 {
                canonical_root_hash: "A".repeat(64),
                ..cursor.clone()
            },
            WorkConversationHeadV1 {
                config_version_id: Some("config\nversion".into()),
                ..cursor.clone()
            },
        ] {
            assert!(!valid_committed_work_cursor(&invalid));
        }
    }
}
