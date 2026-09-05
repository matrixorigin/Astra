mod completions;
#[cfg(feature = "server")]
pub mod conflict_resolver;
#[cfg(feature = "server")]
pub mod edge_connection_pool;
pub mod edge_ws_protocol;
pub use astra_turn_types::runner_inference;
pub mod session_run_tree;
#[cfg(feature = "server")]
pub mod team_orchestrator_traits;
#[cfg(feature = "server")]
pub mod team_orchestrator_types;
#[cfg(feature = "server")]
pub mod worktree_isolation;
#[cfg(feature = "server")]
pub mod ws_progress_callback;

#[cfg(feature = "server")]
use astra_core::{ErrorResponse, error_response};
#[cfg(feature = "server")]
use astra_services::auth::{
    ReauthenticationProofRecord, ReauthenticationPurpose, ReauthenticationRequestData,
};
#[cfg(feature = "server")]
use astra_services::auth::{SessionActivityCursor, SessionActivityRecord, SessionListCursor};
#[cfg(feature = "server")]
use astra_services::{
    AdminAuditRecord, AdminFeedbackStatsRecord, AdminInitRecord, AdminTokenRecord,
    AdminUserRoleRecord, AuthTokenRecord, AuthUserRecord, CancelRunRecord, ChatRequestData,
    ChatRunRecord, RunContinuationRecord, RunListCursor, RunListRecord, RunMutationDisposition,
    RunMutationRecord, RunStatusRecord, SessionArtifactListCursor, SessionListRecord,
    SessionRecord, run_list_cursor_db_updated_at, run_list_cursor_run_id,
};
#[cfg(feature = "server")]
use astra_tools::AskUserPrompt;
#[cfg(feature = "server")]
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};

pub use completions::{
    CompletionChoice, CompletionMessage, CompletionOperation, CompletionRequest,
    CompletionResponse, CompletionUsage, MAX_COMPLETION_OUTPUT_TOKENS,
};
pub use edge_ws_protocol::{
    EDGE_AUTH_TIMEOUT_SECS, EDGE_HEARTBEAT_INTERVAL_SECS, EDGE_TOOL_RESULT_GRACE_SECS,
    EDGE_TOOL_TIMEOUT_SECS, EdgeClientMessage, EdgeServerMessage, MAX_EDGE_TOOL_TIMEOUT_SECS,
};
pub use session_run_tree::{
    SESSION_RUN_TREE_SCHEMA_VERSION, SessionRunAction, SessionRunLifecycleStatus, SessionRunNode,
    SessionRunPermissionFacts, SessionRunRuntimeFacts, SessionRunTreeSnapshot,
};

pub const WORK_API_MAJOR_HEADER: &str = "x-astra-work-api-major";
pub const WORK_API_MAJOR: &str = "1";
/// Major version of the interactive run/tool contract shared by CLI, Web,
/// and Server. Version 3 carries the server-authored Edge command-timeout
/// cap. A missing or different value is a deployment mismatch, not a
/// recoverable model/tool error.
pub const AGENT_INTERACTION_API_MAJOR_HEADER: &str = "x-astra-agent-interaction-api-major";
pub const AGENT_INTERACTION_API_MAJOR: &str = "3";

/// Versioned, server-issued lifecycle update for the compact live Work board.
///
/// This is intentionally an event projection rather than a second task
/// authority: the server emits it only after the corresponding Work mutation
/// is durable.  Clients can therefore render current execution immediately
/// without waiting for a later REST poll, while their normal graph observer
/// remains responsible for reconciliation and deep navigation.
pub const WORK_TASK_BOARD_UPDATE_SCHEMA_VERSION: u16 = 1;
/// Stable SSE event type for a durable Work board projection. The payload is
/// emitted by the server after the corresponding Work mutation commits; it is
/// not inferred from tool names or assistant text by clients.
pub const WORK_TASK_BOARD_UPDATE_EVENT_TYPE: &str = "work_task_board_update";
/// A live-board receipt travels over the interactive event stream. It holds
/// concise display text; complete task text belongs to the canonical graph
/// read model. Keeping each field small lets every supported transport retain
/// the receipt as structured output.
pub const WORK_TASK_BOARD_TEXT_MAX_BYTES: usize = 512;
pub const WORK_TASK_BOARD_MAX_UNAVAILABLE_CAPABILITIES: usize = 16;
pub const WORK_TASK_BOARD_CAPABILITY_MAX_BYTES: usize = 128;

/// Shared turn-lifecycle receipt protocol. It lives in `astra-turn-types` so
/// the server, durable replay service, and clients validate one definition
/// without a dependency cycle.
pub use astra_turn_types::{
    TURN_PHASE_EVENT_TYPE, TURN_PHASE_SCHEMA_VERSION, TurnPhaseKindV1, TurnPhaseOutcomeV1,
    TurnPhaseReceiptV1,
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkTaskBoardUpdateV1 {
    pub schema_version: u16,
    pub work_id: String,
    pub branch_id: String,
    #[serde(flatten)]
    pub change: WorkTaskBoardChangeV1,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkTaskBoardChangeV1 {
    /// A complete bounded initial list. `start_work` accepts at most eight
    /// tasks, so this is both exact and small.
    Snapshot {
        goal: String,
        graph_revision: i64,
        criteria_member_count: u16,
        tasks: Vec<WorkTaskBoardTaskV1>,
    },
    /// One or more durable task-state transitions. Upserts preserve rows not
    /// mentioned by the transition, so a long-lived board never requires a
    /// full graph read on every task settlement.
    Upsert {
        graph_revision: Option<i64>,
        tasks: Vec<WorkTaskBoardTaskV1>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskBoardTaskV1 {
    pub item_id: String,
    pub item_revision: i64,
    pub objective: String,
    pub expected_result: String,
    pub declaration_state: WorkTaskBoardDeclarationStateV1,
    pub execution_status: WorkTaskBoardExecutionStatusV1,
    pub delivery_status: WorkTaskBoardDeliveryStatusV1,
    pub delivery_summary: Option<String>,
    pub blocker_kind: Option<WorkTaskBoardBlockerKindV1>,
    pub unavailable_capabilities: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkTaskBoardDeclarationStateV1 {
    Active,
    Superseded,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkTaskBoardExecutionStatusV1 {
    NotStarted,
    Running,
    Waiting,
    Paused,
    Completed,
    Delegated,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkTaskBoardDeliveryStatusV1 {
    Unreported,
    Delivered,
    Blocked,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkTaskBoardBlockerKindV1 {
    CapabilityUnavailable,
    DependencyBlocked,
    PolicyBlocked,
    ExternalUnavailable,
}

/// Constant-size public identity projection for a session already owned by a
/// canonical Work branch. The opaque session identifier is intentionally not
/// echoed: surfaces use it only to bootstrap into the public Work identity.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkSessionBindingResponseV1 {
    pub schema_version: u16,
    pub work_id: String,
    pub branch_id: String,
    pub graph_revision: i64,
}

#[cfg(feature = "server")]
#[derive(Serialize)]
#[serde(transparent)]
pub struct WorkObservationResponseV1(pub astra_services::work::WorkObservationReport);

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCatalogQueryV1 {
    pub before_created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub before_work_id: Option<String>,
    pub limit: Option<u16>,
}

#[cfg(feature = "server")]
#[derive(Serialize)]
pub struct WorkCatalogResponseV1 {
    pub schema_version: u16,
    #[serde(flatten)]
    pub page: astra_services::work::WorkCatalogPage,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkBranchAttachRequestV1 {
    pub request_id: String,
}

#[cfg(feature = "server")]
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchAttachmentModeV1 {
    ReadOnly,
    Controller,
}

#[cfg(feature = "server")]
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchSyncStateV1 {
    Current,
    ProjectionStale,
    Degraded,
    Corrupt,
    Offline,
}

#[cfg(feature = "server")]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkConversationHeadV1 {
    pub completed_turn: u32,
    pub journal_event_seq: u64,
    pub conversation_seq: u64,
    pub canonical_root_hash: String,
    pub projection_schema: u32,
    pub compaction_generation: u64,
    pub config_version_id: Option<String>,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkBranchCreationRequestV1 {
    pub request_id: String,
    pub expected_branch_revision: i64,
    pub committed_cursor: WorkConversationHeadV1,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkBranchComparisonRequestV1 {
    pub left_branch_id: String,
    pub right_branch_id: String,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkArchivedBranchesQueryV1 {
    pub before_archived_at: Option<chrono::DateTime<chrono::Utc>>,
    pub before_branch_id: Option<String>,
    pub limit: Option<u16>,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkDeliverySelectionSubjectV1 {
    pub graph_revision: i64,
    pub subject_ref: String,
    pub subject_revision: String,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkActionV1 {
    SelectDeliveryBranch {
        branch_id: String,
        expected_branch_revision: i64,
        expected_goal_revision: i64,
        expected_criteria_set_revision: i64,
        expected_graph_revision: i64,
        expected_subject: Option<WorkDeliverySelectionSubjectV1>,
        expected_evidence_manifest_hash: String,
    },
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkActionRequestV1 {
    pub request_id: String,
    pub expected_work_revision: i64,
    pub action: WorkActionV1,
}

#[cfg(feature = "server")]
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkBranchActionV1 {
    Archive,
    Restore,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkBranchActionRequestV1 {
    pub request_id: String,
    pub expected_work_revision: i64,
    pub expected_branch_revision: i64,
    pub action: WorkBranchActionV1,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkBranchDeletionRequestV1 {
    pub request_id: String,
    pub expected_work_revision: i64,
    pub expected_branch_revision: i64,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkPatchMaterializationRequestV1 {
    pub request_id: String,
    pub patch_artifact_id: String,
    pub expected_target_branch_revision: i64,
    pub expected_target_graph_revision: i64,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkPatchMaterializationsQueryV1 {
    pub source_branch_id: String,
    pub before_created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub before_operation_id: Option<String>,
    pub limit: Option<u16>,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkPatchCommitRequestV1 {
    pub request_id: String,
    pub patch_artifact_id: String,
    pub expected_target_branch_revision: i64,
    pub expected_target_graph_revision: i64,
    pub message: String,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkPatchCommitsQueryV1 {
    pub before_created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub before_operation_id: Option<String>,
    pub limit: Option<u16>,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkPatchArtifactExportRequestV1 {
    pub request_id: String,
    pub expected_branch_revision: i64,
    pub expected_graph_revision: i64,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkPatchArtifactsQueryV1 {
    pub before_created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub before_patch_artifact_id: Option<String>,
    pub limit: Option<u16>,
}

#[cfg(feature = "server")]
#[derive(Clone, Debug, Serialize)]
pub struct WorkBranchAttachResponseV1 {
    pub schema_version: u16,
    pub work_id: astra_services::work::WorkId,
    pub branch_id: astra_services::work::WorkBranchId,
    pub attachment_id: String,
    pub attachment_epoch: u64,
    pub branch_revision: astra_services::work::WorkBranchRevision,
    pub mode: WorkBranchAttachmentModeV1,
    pub sync: WorkBranchSyncStateV1,
    pub control_basis: WorkBranchControlBasisV1,
    pub head: Option<WorkConversationHeadV1>,
    pub attached_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(feature = "server")]
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WorkBranchControlBasisV1 {
    pub writer_epoch: u64,
    pub canonical_root_hash: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkBranchControlCommandV1 {
    AcquireBranchControl {
        attachment_id: String,
    },
    ForceTakeover {
        attachment_id: String,
        reauthentication_proof: String,
    },
    ReleaseBranchControl {
        attachment_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkBranchControlOperationRequestV1 {
    pub request_id: String,
    pub expected_branch_revision: i64,
    pub expected_writer_epoch: u64,
    pub expected_canonical_root_hash: Option<String>,
    pub command: WorkBranchControlCommandV1,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkTranscriptQueryV1 {
    pub before_item_seq: Option<u64>,
    pub limit: Option<u16>,
}

#[cfg(feature = "server")]
#[derive(Clone, Debug, Serialize)]
pub struct WorkTranscriptItemV1 {
    pub item_seq: u64,
    pub committed_turn: u32,
    pub role: String,
    pub content: String,
    pub content_truncated: bool,
    pub payload: Option<serde_json::Value>,
    pub payload_omitted: bool,
    pub content_hash: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(feature = "server")]
#[derive(Clone, Debug, Serialize)]
pub struct WorkTranscriptPageResponseV1 {
    pub schema_version: u16,
    pub work_id: astra_services::work::WorkId,
    pub branch_id: astra_services::work::WorkBranchId,
    pub sync: WorkBranchSyncStateV1,
    pub canonical_head: Option<WorkConversationHeadV1>,
    pub transcript_cursor: Option<WorkConversationHeadV1>,
    pub items: Vec<WorkTranscriptItemV1>,
    pub next_before_item_seq: Option<u64>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkCreateCriterionV1 {
    CommandCheck {
        criterion_id: String,
        statement: String,
        command: String,
    },
    TestCheck {
        criterion_id: String,
        statement: String,
        command: String,
    },
    HumanReview {
        criterion_id: String,
        statement: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkCreateRequestV1 {
    /// Caller-generated logical request identity. Exact retries create at
    /// most one Work; reusing it with a different goal or criterion set fails closed.
    pub request_id: String,
    pub goal: String,
    /// Explicit user-authored Done-when criteria. The server never infers
    /// criterion definitions from goal text.
    pub criteria: Vec<WorkCreateCriterionV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTurnRequestV1 {
    /// Caller-generated identity for one logical branch continuation. Exact
    /// retries attach to the same run; a changed message fails closed.
    pub request_id: String,
    pub attachment_id: String,
    pub message: String,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkReadCursorRequestV1 {
    pub through_event_seq: i64,
}

#[cfg(feature = "server")]
#[derive(Serialize)]
pub struct WorkReadCursorResponseV1 {
    pub schema_version: u16,
    pub work_id: astra_services::work::WorkId,
    pub through_event_seq: astra_services::work::WorkEventSeq,
    pub receipt_revision: astra_services::work::WorkAttentionReceiptRevision,
    pub receipt_hash: astra_services::work::WorkContentHash,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkEventsQueryV1 {
    pub after_event_seq: Option<i64>,
    pub limit: Option<u16>,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkTaskGraphQueryV1 {
    /// Required on every continuation page; pins offsets to one immutable
    /// graph revision and prevents torn pagination across a replan.
    pub graph_revision: Option<i64>,
    pub item_offset: Option<u16>,
    pub item_limit: Option<u16>,
    pub dependency_offset: Option<u16>,
    pub dependency_limit: Option<u16>,
}

#[cfg(feature = "server")]
#[derive(Serialize)]
#[serde(transparent)]
pub struct WorkTaskGraphResponseV1(pub astra_services::work::WorkTaskGraphPage);

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCriteriaQueryV1 {
    /// Required on continuation pages so a concurrent criterion-set change
    /// cannot splice two immutable sets into one user view.
    pub criteria_set_revision: Option<i64>,
    pub offset: Option<u16>,
    pub limit: Option<u16>,
}

#[cfg(feature = "server")]
#[derive(Serialize)]
#[serde(transparent)]
pub struct WorkCriteriaResponseV1(pub astra_services::work::WorkCriteriaPage);

#[cfg(feature = "server")]
#[derive(Serialize)]
pub struct WorkCriteriaProposalBasisV1 {
    pub work_revision: astra_services::work::WorkRevision,
    pub goal_revision: astra_services::work::GoalRevision,
    pub criteria_set_revision: astra_services::work::CriterionSetRevision,
    pub branch_revision: astra_services::work::WorkBranchRevision,
    pub graph_revision: astra_services::work::GraphRevision,
}

#[cfg(feature = "server")]
#[derive(Serialize)]
pub struct WorkCriteriaProposalSummaryV1 {
    pub work_id: astra_services::work::WorkId,
    pub branch_id: astra_services::work::WorkBranchId,
    pub proposal_id: astra_services::work::WorkProposalId,
    pub proposal_seq: i64,
    pub payload_hash: astra_services::work::WorkContentHash,
    pub status: astra_services::work::WorkProposalStatus,
    pub basis: WorkCriteriaProposalBasisV1,
    pub member_count: u16,
    pub source_kind: astra_services::work::WorkProposalSourceKind,
    pub proposed_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(feature = "server")]
#[derive(Serialize)]
pub struct WorkCriteriaProposalResolutionV1 {
    pub resolution_ref: astra_services::work::WorkChangeRef,
    pub resolved_at: chrono::DateTime<chrono::Utc>,
    pub result_work_revision: Option<astra_services::work::WorkRevision>,
    pub result_criteria_set_revision: Option<astra_services::work::CriterionSetRevision>,
}

#[cfg(feature = "server")]
#[derive(Serialize)]
pub struct WorkCriteriaProposalDetailResponseV1 {
    pub schema_version: u16,
    pub proposal: WorkCriteriaProposalSummaryV1,
    pub members: Vec<astra_services::work::WorkCriteriaProposalMember>,
    pub resolution: Option<WorkCriteriaProposalResolutionV1>,
}

#[cfg(feature = "server")]
#[derive(Serialize)]
pub struct WorkCriteriaProposalListResponseV1 {
    pub schema_version: u16,
    pub work_id: astra_services::work::WorkId,
    pub branch_id: astra_services::work::WorkBranchId,
    pub proposals: Vec<WorkCriteriaProposalSummaryV1>,
}

#[cfg(feature = "server")]
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkCriteriaProposalDecisionV1 {
    Accept,
    Reject,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCriteriaProposalDecisionRequestV1 {
    pub request_id: String,
    pub decision: WorkCriteriaProposalDecisionV1,
    pub payload_hash: String,
    pub expected_work_revision: i64,
    pub expected_goal_revision: i64,
    pub expected_criteria_set_revision: i64,
    pub expected_branch_revision: i64,
    pub expected_graph_revision: i64,
}

#[cfg(feature = "server")]
#[derive(Serialize)]
pub struct WorkEventPageResponseV1 {
    pub schema_version: u16,
    #[serde(flatten)]
    pub page: astra_services::work::WorkEventPage,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct RootResponse {
    pub name: String,
    pub version: String,
    pub docs: String,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthRegisterRequest {
    pub username: String,
    pub email: String,
    pub password: String,
    pub display_name: Option<String>,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthLoginRequest {
    pub username: String,
    pub password: String,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthRefreshRequest {
    pub refresh_token: String,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthReauthenticateRequest {
    pub password: String,
    pub purpose: ReauthenticationPurpose,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatRequest {
    pub message: String,
    #[serde(default)]
    pub user_intent: Option<String>,
    #[serde(default)]
    pub parts: Vec<serde_json::Value>,
    #[serde(default)]
    pub attachments: Vec<serde_json::Value>,
    #[serde(default)]
    pub stable_runtime_system_prompt: Option<String>,
    #[serde(default)]
    pub runtime_system_prompt: Option<String>,
    pub session_id: Option<String>,
    #[serde(default)]
    pub work_binding: Option<astra_services::runs::WorkRuntimeBindingRequest>,
    pub agent_id: Option<String>,
    #[serde(default)]
    pub model_selection: Option<astra_turn_types::ModelSelection>,
    #[serde(default)]
    pub resolved_model_selection: Option<astra_services::runs::ResolvedModelSelection>,
    #[serde(default)]
    pub capability_descriptors: Option<astra_services::runs::RuntimeCapabilityDescriptorsRequest>,
    #[serde(default)]
    pub agent_bindings: Vec<astra_services::runs::AgentBindingRuntimeRequest>,
    #[serde(default)]
    pub agent_binding: Option<astra_services::runs::AgentBindingRuntimeRequest>,
    #[serde(default)]
    pub runtime_auth: Option<astra_services::runs::RuntimeAuthRequest>,
    #[serde(default)]
    pub runtime_profile: Option<astra_services::runs::RuntimeProfileRequest>,
    #[serde(default)]
    pub skill_search: Option<astra_core::SkillSearchSettings>,
    #[serde(default)]
    pub allow_skills: Option<Vec<String>>,
    #[serde(default)]
    pub allow_skill_sources: Option<Vec<String>>,
    #[serde(default)]
    pub allow_tools: Option<Vec<String>>,
    /// Optional external tools explicitly enabled by the embedding product.
    /// Unlike `allow_tools`, this does not restrict core tools.
    #[serde(default)]
    pub enabled_tools: Option<Vec<String>>,
    #[serde(default)]
    pub workspace_binding: Option<astra_services::runs::WorkspaceBindingRequest>,
    #[serde(default)]
    pub executor_binding: Option<astra_services::runs::ExecutorBindingRequest>,
    #[serde(default)]
    pub runtime_mcp_bindings: Vec<astra_services::runs::RuntimeMcpBindingRequest>,
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    pub edge_executor_id: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub execution_budget: Option<astra_services::runs::ExecutionBudget>,
    #[serde(default)]
    pub execution_time_budget: Option<astra_services::runs::ExecutionTimeBudget>,
    #[serde(default)]
    pub execution_policy: astra_services::runs::ExecutionPolicyRequest,
    #[serde(default)]
    pub explain: bool,
    #[serde(default)]
    pub interaction_mode: Option<astra_services::runs::RequestedTurnInteractionMode>,
    /// Whether the client can handle interactive callbacks (`ask_user` / approval prompts).
    #[serde(default)]
    pub interactive_client: bool,
    /// Durable plan subtask id — merged into `context` for cloud stop-hooks (`when: task_completed`).
    #[serde(default)]
    pub plan_subtask_id: Option<String>,
    #[serde(default)]
    pub is_plan_subtask: Option<bool>,
    /// Server-issued canonical authority. Omitted on initial admission;
    /// identity inside this envelope is never trusted without signature and
    /// authenticated-owner validation.
    #[serde(default)]
    pub conversation_authority: Option<astra_turn_types::ConversationAuthorityEnvelopeV1>,
}

#[cfg(feature = "server")]
#[derive(Deserialize, Default)]
pub struct RunStreamQuery {
    #[serde(default)]
    pub last_index: u32,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCreateRequest {
    pub agent_id: Option<String>,
    pub title: Option<String>,
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUpdateRequest {
    pub title: Option<String>,
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
    pub metadata_patch: Option<serde_json::Map<String, serde_json::Value>>,
    pub status: Option<String>,
}

#[cfg(feature = "server")]
#[derive(Deserialize, Default)]
pub struct SessionListQuery {
    pub agent_id: Option<String>,
    pub session_status: Option<String>,
    #[serde(default = "default_session_limit")]
    pub limit: u32,
    pub after_updated_at: Option<String>,
    pub after_session_id: Option<String>,
}

#[cfg(feature = "server")]
impl SessionListQuery {
    pub fn cursor(&self) -> Result<Option<SessionListCursor>, (StatusCode, Json<ErrorResponse>)> {
        match (&self.after_updated_at, &self.after_session_id) {
            (None, None) => Ok(None),
            (Some(updated_at), Some(session_id)) => Ok(Some(SessionListCursor {
                updated_at: updated_at.clone(),
                session_id: session_id.clone(),
            })),
            _ => Err(error_response(
                StatusCode::BAD_REQUEST,
                "session list cursor requires both after_updated_at and after_session_id",
            )),
        }
    }
}

#[cfg(feature = "server")]
#[derive(Deserialize, Default)]
pub struct SessionActivityQuery {
    #[serde(default = "default_session_activity_limit")]
    pub limit: u32,
    pub after_created_at: Option<String>,
    pub after_log_id: Option<String>,
}

#[cfg(feature = "server")]
impl SessionActivityQuery {
    pub fn cursor(
        &self,
    ) -> Result<Option<SessionActivityCursor>, (StatusCode, Json<ErrorResponse>)> {
        match (&self.after_created_at, &self.after_log_id) {
            (None, None) => Ok(None),
            (Some(created_at), Some(log_id)) => Ok(Some(SessionActivityCursor {
                created_at: created_at.clone(),
                log_id: log_id.clone(),
            })),
            _ => Err(error_response(
                StatusCode::BAD_REQUEST,
                "session activity cursor requires both after_created_at and after_log_id",
            )),
        }
    }
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct SessionActivityEntry {
    pub log_id: String,
    pub action: String,
    pub details: serde_json::Value,
    pub created_at: String,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct SessionActivityResponse {
    pub session_id: String,
    pub activities: Vec<SessionActivityEntry>,
    pub total: i64,
    pub limit: u32,
    pub next_cursor: Option<SessionActivityCursor>,
}

#[cfg(feature = "server")]
#[derive(Deserialize, Default)]
pub struct SessionArtifactListQuery {
    pub artifact_kind: Option<String>,
    #[serde(default = "default_session_artifact_limit")]
    pub limit: u32,
    pub after_created_at: Option<String>,
    pub after_artifact_id: Option<String>,
}

#[cfg(feature = "server")]
impl SessionArtifactListQuery {
    pub fn cursor(
        &self,
    ) -> Result<Option<SessionArtifactListCursor>, (StatusCode, Json<ErrorResponse>)> {
        match (&self.after_created_at, &self.after_artifact_id) {
            (None, None) => Ok(None),
            (Some(created_at), Some(artifact_id)) => {
                let db_created_at = created_at.trim().replace('T', " ");
                if db_created_at.len() != "YYYY-MM-DD HH:MM:SS.ffffff".len()
                    || db_created_at.as_bytes().get(10) != Some(&b' ')
                    || db_created_at.as_bytes().get(19) != Some(&b'.')
                    || chrono::NaiveDateTime::parse_from_str(
                        &db_created_at,
                        "%Y-%m-%d %H:%M:%S%.6f",
                    )
                    .is_err()
                {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        format!("invalid session artifact list cursor timestamp: {created_at}"),
                    ));
                }
                if artifact_id.trim().is_empty() {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "invalid session artifact list cursor: artifact_id is required",
                    ));
                }
                Ok(Some(SessionArtifactListCursor {
                    created_at: created_at.clone(),
                    artifact_id: artifact_id.clone(),
                }))
            }
            _ => Err(error_response(
                StatusCode::BAD_REQUEST,
                "session artifact list cursor requires both after_created_at and after_artifact_id",
            )),
        }
    }
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq)]
pub struct SessionArtifactResponse {
    pub artifact_id: String,
    pub session_id: String,
    pub user_id: String,
    pub artifact_kind: String,
    pub source: Option<String>,
    pub turn: Option<u32>,
    pub round: Option<u32>,
    pub content: serde_json::Value,
    pub metadata: Option<serde_json::Value>,
    pub retention_policy: Option<String>,
    pub retention_until: Option<String>,
    pub status: Option<String>,
    pub referenced_by_manifest_count: u32,
    pub referenced_by_state_items_count: u32,
    pub referenced_by_citation_count: u32,
    pub referenced_by_durable_count: u32,
    pub created_at: Option<String>,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq)]
pub struct SessionArtifactListResponse {
    pub session_id: String,
    pub artifacts: Vec<SessionArtifactResponse>,
    pub limit: u32,
    pub next_cursor: Option<SessionArtifactListCursor>,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct AuthUserResponse {
    pub user_id: String,
    pub username: String,
    pub email: String,
    pub display_name: Option<String>,
}

/// Returned by POST /auth/register — includes the user record plus ready-to-use tokens
/// so callers don't need a separate login round-trip.
#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct AuthRegisterResponse {
    pub user_id: String,
    pub username: String,
    pub email: String,
    pub display_name: Option<String>,
    pub roles: Vec<String>,
    pub is_admin: bool,
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: u32,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct AuthTokenResponse {
    pub user_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: u32,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct AuthLogoutResponse {
    pub message: String,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct AuthReauthenticateResponse {
    pub proof: String,
    pub purpose: ReauthenticationPurpose,
    pub expires_in: u32,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq)]
pub struct SessionResponse {
    pub session_id: String,
    pub user_id: String,
    pub agent_id: Option<String>,
    pub title: Option<String>,
    pub metadata: serde_json::Map<String, serde_json::Value>,
    pub status: String,
    pub event_count: i64,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub ended_at: Option<String>,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq)]
pub struct SessionListResponse {
    pub sessions: Vec<SessionResponse>,
    pub total: Option<i64>,
    pub limit: u32,
    pub next_cursor: Option<SessionListCursor>,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq)]
pub struct ChatResponse {
    pub session_id: String,
    pub run_id: String,
    pub status: String,
    pub explain: Option<serde_json::Value>,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct RunStatusResponse {
    pub run_id: String,
    pub session_id: String,
    pub parent_run_id: Option<String>,
    pub root_run_id: Option<String>,
    pub depth: u32,
    pub status: String,
    pub waiting_for: Option<String>,
    pub events_count: i64,
    pub workspace: Option<serde_json::Value>,
    pub executor: Option<serde_json::Value>,
    pub transport: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounting: Option<serde_json::Value>,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct CancelRunResponse {
    pub run_id: String,
    pub status: String,
    pub execution_settled: bool,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct RunMutationResponse {
    pub run_id: String,
    pub status: String,
    pub previous_status: String,
    pub disposition: RunMutationDisposition,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation: Option<RunContinuationRecord>,
}

#[cfg(feature = "server")]
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RunListQuery {
    #[serde(default = "default_run_list_limit")]
    pub limit: u32,
    pub after_updated_at: Option<String>,
    pub after_run_id: Option<String>,
}

#[cfg(feature = "server")]
impl RunListQuery {
    pub fn cursor(&self) -> Result<Option<RunListCursor>, (StatusCode, Json<ErrorResponse>)> {
        match (&self.after_updated_at, &self.after_run_id) {
            (None, None) => Ok(None),
            (Some(updated_at), Some(run_id)) => {
                let cursor = RunListCursor {
                    updated_at: updated_at.clone(),
                    run_id: run_id.clone(),
                };
                run_list_cursor_db_updated_at(&cursor)
                    .map_err(|error| error_response(StatusCode::BAD_REQUEST, error))?;
                run_list_cursor_run_id(&cursor)
                    .map_err(|error| error_response(StatusCode::BAD_REQUEST, error))?;
                Ok(Some(cursor))
            }
            _ => Err(error_response(
                StatusCode::BAD_REQUEST,
                "run list cursor requires both after_updated_at and after_run_id",
            )),
        }
    }
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct RunListResponse {
    pub runs: Vec<RunStatusResponse>,
    pub total: Option<i64>,
    pub limit: u32,
    pub next_cursor: Option<RunListCursor>,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub status: String,
    pub database: String,
    pub memoria: String,
    pub interaction_api_major: String,
    pub build_git_sha: String,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct LearningHealthResponse {
    pub status: String,
    pub service: String,
    pub version: String,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lesson_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_count: Option<u64>,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct LearningSignalsResponse {
    pub signal_types: Vec<&'static str>,
    pub descriptions: LearningSignalDescriptions,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct LearningSignalDescriptions {
    pub wrong_skill: &'static str,
    pub slow_execution: &'static str,
    pub high_cost: &'static str,
    pub low_satisfaction: &'static str,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq)]
pub struct LearningStatsResponse {
    pub total_learnings: i32,
    pub high_confidence: i32,
    pub low_confidence: i32,
    pub avg_confidence: f64,
    pub by_signal_type: serde_json::Map<String, serde_json::Value>,
    pub weights: serde_json::Map<String, serde_json::Value>,
    pub weights_per_signal: serde_json::Map<String, serde_json::Value>,
    pub decay: serde_json::Map<String, serde_json::Value>,
    pub total_gates: i32,
    pub passed_gates: i32,
    pub failed_gates: i32,
    pub pass_rate: f64,
    pub avg_improvement_pct: f64,
    pub per_skill: serde_json::Map<String, serde_json::Value>,
    pub last_learning_time: Option<String>,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
pub struct LearningTriggerRequest {
    #[serde(default = "default_days")]
    pub days: i32,
    #[serde(default)]
    pub force: bool,
    #[serde(default = "default_signal_types")]
    pub signal_types: Vec<String>,
    #[serde(default)]
    pub weights: Option<serde_json::Map<String, serde_json::Value>>,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq)]
pub struct LearningTriggerResponse {
    pub status: &'static str,
    pub learned: i32,
    pub signals_by_type: Option<serde_json::Value>,
    pub gate_verdict: Option<String>,
    pub improvement_pct: Option<serde_json::Value>,
    pub test_count: Option<i32>,
    pub error: Option<&'static str>,
    pub message: Option<serde_json::Value>,
    pub model_version: &'static str,
}

#[cfg(feature = "server")]
#[derive(Deserialize, Default)]
pub struct AdminTokenListQuery {
    pub token_type: Option<String>,
    pub scope: Option<String>,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
pub struct AdminTokenCreateRequest {
    pub token_type: String,
    pub provider: Option<String>,
    #[serde(default = "default_admin_scope")]
    pub scope: String,
    pub scope_id: Option<String>,
    pub token_value: Option<String>,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
pub struct PromptOptimizeRequest {
    pub agent_id: String,
    #[serde(default = "default_prompt_optimization_type")]
    pub optimization_type: String,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct PromptOptimizeResponse {
    pub job_id: String,
    pub status: &'static str,
    pub message: String,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
pub struct FeedbackExportRequest {
    pub agent_id: Option<String>,
    #[serde(default = "default_feedback_export_format")]
    pub format: String,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct FeedbackExportResponse {
    pub job_id: String,
    pub status: &'static str,
    pub download_url: Option<String>,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
pub struct AdminFeedbackStatsQuery {
    pub agent_id: Option<String>,
    pub since: Option<String>,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
pub struct AdminAuditListQuery {
    pub user_id: Option<String>,
    pub since: Option<String>,
    #[serde(default = "default_admin_audit_limit")]
    pub limit: u32,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct AdminTokenResponse {
    pub token_id: String,
    pub token_type: String,
    pub provider: Option<String>,
    pub scope: String,
    pub scope_id: Option<String>,
    pub created_at: String,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq)]
pub struct AdminAuditResponse {
    pub log_id: String,
    pub user_id: String,
    pub action: String,
    pub resource_type: String,
    pub resource_id: Option<String>,
    pub timestamp: String,
    pub details: Option<serde_json::Value>,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq)]
pub struct AdminFeedbackStatsResponse {
    pub total_feedback: i64,
    pub positive_feedback: i64,
    pub negative_feedback: i64,
    pub avg_rating: Option<f64>,
    pub feedback_by_type: serde_json::Map<String, serde_json::Value>,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct AdminInitResponse {
    pub message: String,
    pub tables_created: i64,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
pub struct AdminUserRoleRequest {
    pub username: String,
    pub role_name: String,
}

#[cfg(feature = "server")]
#[derive(Serialize, PartialEq, Eq)]
pub struct AdminUserRoleResponse {
    pub username: String,
    pub role_name: String,
    pub message: String,
}

/// Messages sent from browser client to server.
#[cfg(feature = "server")]
#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum WsClientMessage {
    /// Authenticate with a Bearer token (must be first message).
    #[serde(rename = "auth")]
    Auth {
        token: String,
        interaction_api_major: String,
    },

    /// Send a chat message to the agent.
    #[serde(rename = "message")]
    ChatMessage {
        content: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        agent_id: Option<String>,
        model_selection: astra_turn_types::ModelSelection,
        #[serde(default)]
        skill_search: Option<astra_core::SkillSearchSettings>,
        #[serde(default)]
        context: Option<serde_json::Map<String, serde_json::Value>>,
        #[serde(default)]
        execution_budget: Option<astra_services::runs::ExecutionBudget>,
        #[serde(default)]
        explain: bool,
        #[serde(default)]
        plan_subtask_id: Option<String>,
        #[serde(default)]
        is_plan_subtask: Option<bool>,
    },

    /// Cancel an active run.
    #[serde(rename = "cancel_run")]
    CancelRun { run_id: String },

    /// Pause an active run.
    #[serde(rename = "pause_run")]
    PauseRun { run_id: String },

    /// Resume a paused run.
    #[serde(rename = "resume_run")]
    ResumeRun { run_id: String },

    /// Respond to a tool approval request.
    #[serde(rename = "tool_approval")]
    ToolApproval {
        request_id: String,
        approved: bool,
        #[serde(default)]
        reason: Option<String>,
    },

    /// Client heartbeat.
    #[serde(rename = "ping")]
    Ping,
}

/// Messages sent from server to browser client.
#[cfg(feature = "server")]
#[derive(Serialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum WsServerMessage {
    /// Authentication succeeded.
    #[serde(rename = "auth_ok")]
    AuthOk {
        user_id: String,
        username: String,
        interaction_api_major: String,
    },

    /// Authentication failed.
    #[serde(rename = "auth_error")]
    AuthError { message: String },

    /// Session/run identifiers for the active websocket chat stream.
    #[serde(rename = "session_info")]
    SessionInfo {
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
    },

    /// Agentic run started — client should track this run_id.
    #[serde(rename = "run_started")]
    RunStarted {
        run_id: String,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        explain: Option<serde_json::Value>,
    },

    /// Agentic run finished (completed or failed).
    #[serde(rename = "run_finished")]
    RunFinished {
        run_id: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// Run was cancelled by client request.
    #[serde(rename = "run_cancelled")]
    RunCancelled { run_id: String },

    /// Run was paused.
    #[serde(rename = "run_paused")]
    RunPaused { run_id: String },

    /// Run is waiting for an external event before it can continue.
    #[serde(rename = "run_waiting")]
    RunWaiting {
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    /// Run was resumed.
    #[serde(rename = "run_resumed")]
    RunResumed { run_id: String },

    /// Tool requires user approval before execution.
    #[serde(rename = "tool_approval_request")]
    ToolApprovalRequest {
        request_id: String,
        tool: String,
        args: serde_json::Value,
    },

    /// ask_user requires a frontend response before the turn can continue.
    #[serde(rename = "user_prompt_request")]
    UserPromptRequest {
        request_id: String,
        prompt: AskUserPrompt,
    },

    /// ask_user prompt resolved and the turn can continue.
    #[serde(rename = "user_prompt_resolved")]
    UserPromptResolved {
        request_id: String,
        outcome: String,
        answers: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        was_custom: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// Tool execution started on server.
    #[serde(rename = "tool_execution_started")]
    ToolExecutionStarted { call_id: String, tool: String },

    /// Incremental output from a running tool.
    #[serde(rename = "tool_output_delta")]
    ToolOutputDelta { call_id: String, content: String },

    /// Tool execution completed on server.
    #[serde(rename = "tool_execution_completed")]
    ToolExecutionCompleted { call_id: String, success: bool },

    /// Error during processing.
    #[serde(rename = "error")]
    Error {
        message: String,
        code: String,
        retryable: bool,
    },

    /// Server heartbeat response.
    #[serde(rename = "pong")]
    Pong,

    /// Connection is being closed.
    #[serde(rename = "closing")]
    Closing { reason: String },
}

/// Query params for WebSocket upgrade. Credentials belong in the typed first
/// WebSocket frame and are never accepted in URLs.
#[cfg(feature = "server")]
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WsUpgradeQuery {
    /// Optional session ID to request on the first chat turn.
    pub session_id: Option<String>,
}

#[cfg(feature = "server")]
#[doc(hidden)]
pub fn merge_plan_subtask_context(
    mut context: Option<serde_json::Map<String, serde_json::Value>>,
    plan_subtask_id: Option<String>,
    is_plan_subtask: Option<bool>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if plan_subtask_id.is_some() || is_plan_subtask == Some(true) {
        let ctx = context.get_or_insert_with(serde_json::Map::new);
        if let Some(id) = plan_subtask_id
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            ctx.entry("plan_subtask_id".to_string())
                .or_insert(serde_json::Value::String(id));
        }
        if is_plan_subtask == Some(true) {
            ctx.entry("is_plan_subtask".to_string())
                .or_insert(serde_json::Value::Bool(true));
        }
    }
    context
}

#[cfg(feature = "server")]
#[doc(hidden)]
pub fn default_days() -> i32 {
    7
}

#[cfg(feature = "server")]
#[doc(hidden)]
pub fn default_admin_scope() -> String {
    "global".to_string()
}

#[cfg(feature = "server")]
#[doc(hidden)]
pub fn default_session_limit() -> u32 {
    50
}

#[cfg(feature = "server")]
#[doc(hidden)]
pub fn default_run_list_limit() -> u32 {
    50
}

#[cfg(feature = "server")]
#[doc(hidden)]
pub fn default_session_activity_limit() -> u32 {
    100
}

#[cfg(feature = "server")]
#[doc(hidden)]
pub fn default_session_artifact_limit() -> u32 {
    20
}

#[cfg(feature = "server")]
#[doc(hidden)]
pub fn default_prompt_optimization_type() -> String {
    "compression".to_string()
}

#[cfg(feature = "server")]
#[doc(hidden)]
pub fn default_feedback_export_format() -> String {
    "jsonl".to_string()
}

#[cfg(feature = "server")]
#[doc(hidden)]
pub fn default_admin_audit_limit() -> u32 {
    100
}

#[cfg(feature = "server")]
#[doc(hidden)]
pub fn default_signal_types() -> Vec<String> {
    vec!["wrong_skill".to_string()]
}

#[cfg(feature = "server")]
pub fn sse_error_code_for_status(status: u16) -> &'static str {
    match status {
        401 | 403 => "AUTH_ERROR",
        404 => "NOT_FOUND",
        422 => "VALIDATION_ERROR",
        _ => "INTERNAL_ERROR",
    }
}

#[cfg(feature = "server")]
pub fn sse_retryable_for_status(status: u16) -> bool {
    status >= 500 || status == 429
}

#[cfg(feature = "server")]
pub fn build_sse_error_event_payload(status: u16, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "message": message.into(),
        "code": sse_error_code_for_status(status),
        "retryable": sse_retryable_for_status(status),
    })
}

#[cfg(feature = "server")]
impl From<AdminTokenRecord> for AdminTokenResponse {
    fn from(value: AdminTokenRecord) -> Self {
        Self {
            token_id: value.token_id,
            token_type: value.token_type,
            provider: value.provider,
            scope: value.scope,
            scope_id: value.scope_id,
            created_at: value.created_at,
        }
    }
}

#[cfg(feature = "server")]
impl From<AdminAuditRecord> for AdminAuditResponse {
    fn from(value: AdminAuditRecord) -> Self {
        Self {
            log_id: value.log_id,
            user_id: value.user_id,
            action: value.action,
            resource_type: value.resource_type,
            resource_id: value.resource_id,
            timestamp: value.timestamp,
            details: value.details,
        }
    }
}

#[cfg(feature = "server")]
impl From<AdminFeedbackStatsRecord> for AdminFeedbackStatsResponse {
    fn from(value: AdminFeedbackStatsRecord) -> Self {
        Self {
            total_feedback: value.total_feedback,
            positive_feedback: value.positive_feedback,
            negative_feedback: value.negative_feedback,
            avg_rating: value.avg_rating,
            feedback_by_type: value.feedback_by_type,
        }
    }
}

#[cfg(feature = "server")]
impl From<AdminInitRecord> for AdminInitResponse {
    fn from(value: AdminInitRecord) -> Self {
        Self {
            message: value.message,
            tables_created: value.tables_created,
        }
    }
}

#[cfg(feature = "server")]
impl From<AdminUserRoleRecord> for AdminUserRoleResponse {
    fn from(value: AdminUserRoleRecord) -> Self {
        Self {
            username: value.username,
            role_name: value.role_name,
            message: value.message,
        }
    }
}

#[cfg(feature = "server")]
impl From<SessionRecord> for SessionResponse {
    fn from(value: SessionRecord) -> Self {
        Self {
            session_id: value.session_id,
            user_id: value.user_id,
            agent_id: value.agent_id,
            title: value.title,
            metadata: value.metadata,
            status: value.status,
            event_count: value.event_count,
            created_at: value.created_at,
            updated_at: value.updated_at,
            ended_at: value.ended_at,
        }
    }
}

#[cfg(feature = "server")]
impl From<SessionListRecord> for SessionListResponse {
    fn from(value: SessionListRecord) -> Self {
        Self {
            sessions: value
                .sessions
                .into_iter()
                .map(SessionResponse::from)
                .collect(),
            total: value.total,
            limit: value.limit,
            next_cursor: value.next_cursor,
        }
    }
}

#[cfg(feature = "server")]
impl From<SessionActivityRecord> for SessionActivityResponse {
    fn from(value: SessionActivityRecord) -> Self {
        Self {
            session_id: value.session_id,
            activities: value
                .activities
                .into_iter()
                .map(|e| SessionActivityEntry {
                    log_id: e.log_id,
                    action: e.action,
                    details: e.details,
                    created_at: e.created_at,
                })
                .collect(),
            total: value.total,
            limit: value.limit,
            next_cursor: value.next_cursor,
        }
    }
}

#[cfg(feature = "server")]
impl From<ChatRunRecord> for ChatResponse {
    fn from(value: ChatRunRecord) -> Self {
        Self {
            session_id: value.session_id,
            run_id: value.run_id,
            status: value.status,
            explain: value.explain,
        }
    }
}

#[cfg(feature = "server")]
impl From<RunStatusRecord> for RunStatusResponse {
    fn from(value: RunStatusRecord) -> Self {
        Self {
            run_id: value.run_id,
            session_id: value.session_id,
            parent_run_id: value.parent_run_id,
            root_run_id: value.root_run_id,
            depth: value.depth,
            status: value.status,
            waiting_for: value.waiting_for,
            events_count: value.events_count,
            workspace: value.workspace,
            executor: value.executor,
            transport: value.transport,
            accounting: value.accounting,
        }
    }
}

#[cfg(feature = "server")]
impl From<CancelRunRecord> for CancelRunResponse {
    fn from(value: CancelRunRecord) -> Self {
        Self {
            run_id: value.run_id,
            status: value.status,
            execution_settled: value.execution_settled,
        }
    }
}

#[cfg(feature = "server")]
impl From<RunMutationRecord> for RunMutationResponse {
    fn from(value: RunMutationRecord) -> Self {
        Self {
            run_id: value.run_id,
            status: value.status,
            previous_status: value.previous_status,
            disposition: value.disposition,
            continuation: value.continuation,
        }
    }
}

#[cfg(feature = "server")]
impl From<RunListRecord> for RunListResponse {
    fn from(value: RunListRecord) -> Self {
        Self {
            runs: value
                .runs
                .into_iter()
                .map(RunStatusResponse::from)
                .collect(),
            total: value.total,
            limit: value.limit,
            next_cursor: value.next_cursor,
        }
    }
}

#[cfg(feature = "server")]
impl From<AuthUserRecord> for AuthUserResponse {
    fn from(value: AuthUserRecord) -> Self {
        Self {
            user_id: value.user_id,
            username: value.username,
            email: value.email,
            display_name: value.display_name,
        }
    }
}

#[cfg(feature = "server")]
impl From<AuthTokenRecord> for AuthTokenResponse {
    fn from(value: AuthTokenRecord) -> Self {
        Self {
            user_id: value.user_id,
            access_token: value.access_token,
            refresh_token: value.refresh_token,
            token_type: value.token_type,
            expires_in: value.expires_in,
        }
    }
}

#[cfg(feature = "server")]
impl From<AuthReauthenticateRequest> for ReauthenticationRequestData {
    fn from(value: AuthReauthenticateRequest) -> Self {
        Self {
            password: value.password,
            purpose: value.purpose,
        }
    }
}

#[cfg(feature = "server")]
impl From<ReauthenticationProofRecord> for AuthReauthenticateResponse {
    fn from(value: ReauthenticationProofRecord) -> Self {
        Self {
            proof: value.proof,
            purpose: value.purpose,
            expires_in: value.expires_in,
        }
    }
}

#[cfg(feature = "server")]
#[doc(hidden)]
pub fn chat_request_into_data(mut request: ChatRequest) -> ChatRequestData {
    let context = merge_plan_subtask_context(
        request.context.take(),
        request.plan_subtask_id.take(),
        request.is_plan_subtask,
    );
    // Prefer an explicit edge_executor_id; fall back to the id carried in
    // capability_descriptors.edge_agent when moi-core sets the executor via
    // capability descriptor rather than the legacy field.
    let edge_executor_id = request.edge_executor_id.take().or_else(|| {
        request
            .capability_descriptors
            .as_ref()
            .and_then(|cd| cd.edge_agent.as_ref())
            .map(|ea| ea.id.clone())
    });
    ChatRequestData {
        message: request.message,
        user_intent: request.user_intent,
        parts: request.parts,
        attachments: request.attachments,
        stable_runtime_system_prompt: request.stable_runtime_system_prompt,
        runtime_system_prompt: request.runtime_system_prompt,
        session_id: request.session_id,
        work_binding: request.work_binding,
        run_start_idempotency: None,
        full_llm_capture: false,
        agent_id: request.agent_id,
        model: None,
        model_selection_mode: astra_services::runs::ModelSelectionMode::ExplicitOffering,
        model_selection: request.model_selection,
        resolved_model_selection: request.resolved_model_selection,
        admitted_model_execution: None,
        capability_descriptors: request.capability_descriptors,
        provider_runtime_authorized: false,
        agent_bindings: request.agent_bindings,
        agent_binding: request.agent_binding,
        runtime_auth: request.runtime_auth,
        runtime_skill_binding: None,
        runtime_profile: request.runtime_profile,
        skill_search: request.skill_search,
        allow_skills: request.allow_skills,
        allow_skill_sources: request.allow_skill_sources,
        allow_tools: request.allow_tools,
        enabled_tools: request.enabled_tools,
        workspace_binding: request.workspace_binding,
        executor_binding: request.executor_binding,
        runtime_mcp_bindings: request.runtime_mcp_bindings,
        context,
        edge_executor_id,
        capabilities: request.capabilities,
        forward_headers: std::collections::HashMap::new(),
        provider_run_owner: None,
        provider_workspace_id: None,
        agent_binding_owner_scope: None,
        execution_budget: request.execution_budget,
        execution_time_budget: request.execution_time_budget,
        execution_policy: request.execution_policy,
        explain: request.explain,
        interaction_mode: request.interaction_mode,
        interactive_client: request.interactive_client,
        conversation_authority: request.conversation_authority,
    }
}

#[cfg(all(test, feature = "server"))]
mod sse_error_payload_tests {
    use super::*;

    #[test]
    fn sse_error_code_maps_common_statuses() {
        assert_eq!(sse_error_code_for_status(401), "AUTH_ERROR");
        assert_eq!(sse_error_code_for_status(404), "NOT_FOUND");
        assert_eq!(sse_error_code_for_status(422), "VALIDATION_ERROR");
        assert_eq!(sse_error_code_for_status(418), "INTERNAL_ERROR");
    }

    #[test]
    fn sse_error_event_payload_includes_code_and_retryable() {
        let payload = build_sse_error_event_payload(503, "upstream unavailable");
        assert_eq!(payload["type"], "error");
        assert_eq!(payload["code"], "INTERNAL_ERROR");
        assert_eq!(payload["retryable"], true);
        assert_eq!(payload["message"], "upstream unavailable");
    }
}

#[cfg(all(test, feature = "server"))]
mod http_type_tests;
