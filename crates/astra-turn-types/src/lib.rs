//! Core turn types for astra runtime.
//!
//! This crate provides foundational types used during turn execution,
//! extracted from the monolithic runtime crate for better modularity.

mod agent_communication;
mod agent_transcript_evidence;
mod agent_transcript_location;
mod artifact_publication;
pub use artifact_publication::{ArtifactPublicationResult, ArtifactPublicationV1};
mod judgment;
pub use judgment::{
    JudgmentAnswer, JudgmentCodecError, JudgmentQuestion, JudgmentRequest, JudgmentResponse,
    JudgmentResponseProvenance, NormalizedJudgmentResponse, NoulCriteria, judgment_messages,
    judgment_request_from_messages, normalize_judgment_response,
    output_budget_exceeds_completion_cap,
};
mod canonical_tool_pairing;
mod completion_settlement;
#[doc(hidden)]
pub use completion_settlement::deserialize_required_option;
mod context_identity;
mod context_window;
mod deferred_tool;
mod explain_analyze;
mod memory_selection;
pub use memory_selection::*;
mod explain_analyze_projection;
mod explain_wire;
pub use explain_wire::decode_explain_analyze_wire;
mod inference;
mod memory_ranking;
mod memory_structure;
mod permission_mode;
pub use permission_mode::{ChildPermissionMode, ManualApprovalPolicy, PermissionMode};
mod provider_canonical_transition;
mod provider_contract;
mod recovery_point;
mod result_quality;
mod resume;
mod runtime_scaffolding;
mod semantic_judgment_observation;
mod semantic_read_cache;
pub use semantic_judgment_observation::*;
mod session_coordination;
mod session_cursor;
pub mod session_facts;
mod session_fork;
mod session_handoff;
mod stop_hooks;
pub mod task_resolution;
pub mod token_estimate;
mod tool_idempotency;
mod tool_invocation;
mod tool_result_projection;
mod turn_provenance;
mod user_intent;
mod verification_frontier;

pub use agent_communication::{
    AGENT_COMMUNICATION_SCHEMA_VERSION, AgentCommunicationDirection, AgentCommunicationEvent,
    AgentCommunicationParty, AgentCommunicationPayloadKind, AgentCommunicationTarget,
};
pub use agent_transcript_evidence::AgentTranscriptEvidence;
pub use agent_transcript_location::AgentTranscriptLocation;
pub use canonical_tool_pairing::{CanonicalToolPairingError, validate_canonical_tool_pairing};
pub use completion_settlement::{
    BudgetWrapupOrigin, CompletionAction, CompletionActionWindow, CompletionSettlementState,
    ForegroundFanoutPagination, RuntimeSuccessfulToolCompletion,
};
pub use context_identity::{
    ContextIdentityError, LLM_ARTIFACT_EVIDENCE_CONTRACT_VERSION,
    LLM_ARTIFACT_EVIDENCE_MAX_ENTRIES, LlmArtifactEvidenceEntryV1, LlmArtifactEvidenceManifestV1,
    NormalizedPromptCacheUsage, PROMPT_CACHE_IDENTITY_CONTRACT_VERSION, PromptCacheIdentityV1,
    PromptCacheInvalidationReason,
};
pub use context_window::{ContextWindowUsage, ContextWindowUsageSource, RequestTokenUsage};
pub use deferred_tool::DeferredToolActivation;
pub use explain_analyze::{
    EXPLAIN_ANALYZE_EVENT_TYPE, EXPLAIN_ANALYZE_MAX_SAFE_INTEGER, EXPLAIN_ANALYZE_SCHEMA_VERSION,
    ExplainAnalyzeAuxiliaryAttemptV1, ExplainAnalyzeAuxiliaryUsageStatusV1,
    ExplainAnalyzeAuxiliaryUsageV1, ExplainAnalyzeContextAssemblyBasisV1,
    ExplainAnalyzeContextAssemblyV1, ExplainAnalyzeContextBudgetBasisV1,
    ExplainAnalyzeContextBudgetV1, ExplainAnalyzeContextMetricsV1,
    ExplainAnalyzeContextSourceKindV1, ExplainAnalyzeContextSourceV1, ExplainAnalyzeCoverageGapV1,
    ExplainAnalyzeEventV1, ExplainAnalyzeNodeKindV1, ExplainAnalyzeOutcomeV1,
    ExplainAnalyzeTokenUsageV1, ExplainAnalyzeTransitionV1, ExplainAnalyzeUsageBasisV1,
};
pub use explain_analyze_projection::{
    ExplainAnalyzeGraphIntegrityV1, ExplainAnalyzeGraphV1, ExplainAnalyzeProjectedNodeV1,
    ExplainAnalyzeProjectionApplyResultV1, ExplainAnalyzeProjectionDiagnosticCodeV1,
    ExplainAnalyzeProjectionDiagnosticV1, ExplainAnalyzeProjectionDiagnosticsV1,
    ExplainAnalyzeScopeCoverageV1,
};
pub use inference::{
    CLIENT_DIRECT_EXECUTION_FIELDS, InferenceInvocationScope, InferencePurpose, ModelSelection,
    client_direct_execution_field,
};
pub use memory_ranking::{
    MemoryRetrievalOutcome, PERSISTENT_TYPES, RankableMemory, SESSION_SCOPED_TYPE,
    freshness_suffix_for, is_persistent_type, partition_by_scope, rfc3339_days_ago,
    sort_by_retrieval_score,
};
pub use memory_structure::{
    PERSISTENT_MEMORY_TYPES, PersistentStoreRejection, is_persistent_memory_type,
    should_store_persistent_memory, validate_persistent_memory_content,
};
pub use provider_canonical_transition::{
    CanonicalPrefixIdentityV1, MAX_PROVIDER_CANONICAL_RECOVERY_BYTES,
    MAX_PROVIDER_CANONICAL_TRANSITION_BYTES, MAX_PROVIDER_CANONICAL_TRANSITION_DURABLE_BYTES,
    MAX_PROVIDER_CANONICAL_WAL_BYTES, MAX_PROVIDER_CANONICAL_WAL_ENTRIES,
    PROVIDER_CANONICAL_TRANSITION_SCHEMA_VERSION, ProviderCanonicalHistoryIdentityV2,
    ProviderCanonicalRecoveryModeV2, ProviderCanonicalTransitionApply,
    ProviderCanonicalTransitionError, ProviderCanonicalTransitionV2, ProviderCanonicalWalBaseV2,
};
pub use provider_contract::{
    DescriptorVersion, NativeToolId, PROVIDER_INTERACTION_REQUEST_METADATA_KEY,
    PROVIDER_INTERACTION_RESPONSE_METADATA_KEY, ProviderBindingRef, ProviderCallOutcome,
    ProviderCallPayload, ProviderClaim, ProviderClaimSource, ProviderClaimTrust,
    ProviderContractError, ProviderDiscoverySnapshot, ProviderIdentity, ProviderInteractionOutcome,
    ProviderInteractionRequest, ProviderInteractionResponse, ProviderProtocolId, ProviderRejection,
    ProviderRejectionCode, ProviderResolverVersion, ProviderSemanticCacheContract,
    ProviderSemanticDiagnostic, ProviderSemanticDiagnosticCode, ProviderTaskSupport,
    ProviderToolClaims, ProviderToolDeclaration, PublicToolAlias, ResolvedConcurrencyBaseline,
    ResolvedProviderClaim, ResolvedProviderSnapshot, ResolvedProviderSnapshotRef,
    ResolvedProviderToolClaims, ResolvedSemanticCacheBaseline, ResolvedToolDescriptor,
    ResolvedToolDescriptorDraft, ResolvedToolDescriptorRef, ResolvedToolEffect,
    ResolvedToolIdempotency, ResolvedToolSemantics, STABLE_TOOL_ALIAS_METADATA_KEY,
    STABLE_TOOL_ALIAS_SCHEMA_KEY, StableToolAlias, ToolIdentity,
};
pub use recovery_point::{
    RECOVERY_POINT_MANIFEST_SCHEMA_VERSION, RecoveryPointArtifactReferenceV1,
    RecoveryPointBindingStateV1, RecoveryPointCapabilityAssessmentV1,
    RecoveryPointEnvironmentRequirementsV1, RecoveryPointExecutionBindingV1,
    RecoveryPointExecutorKindV1, RecoveryPointManifestV1, RecoveryPointReasonV1,
    RecoveryPointRunFrontierV1, RecoveryPointRunStateV1, RecoveryPointValidationError,
    RecoveryPointWorkspaceReferenceV1,
};
pub use result_quality::{ResultQuality, classify_result, quality_feedback};
pub use resume::{
    CAUSAL_PROJECTION_ENVELOPE_SCHEMA_VERSION, CausalProjectionEnvelopeV1, CursorRelationV1,
    RESUME_BUNDLE_SCHEMA_VERSION, ResumeActivationProjectionV1, ResumeBundleV1, ResumeCandidateV1,
    ResumeCheckpointProjectionV1, ResumeDegradedReasonV1, ResumeDescriptorV1,
    ResumeProjectionSetV1, ResumeProviderProjectionV1, ResumeRepairActionV1, ResumeSelectionError,
    ResumeSourceV1, cursor_relation, select_resume_bundle, select_resume_candidate_index,
};
pub use runtime_scaffolding::{
    APPEND_ONLY_RUNTIME_AUTHORITY_POLICY, APPEND_ONLY_RUNTIME_AUTHORITY_POLICY_FIELD,
    ParsedRuntimeAuthorityFrame, RUNTIME_MESSAGE_PROVENANCE_FIELD, RuntimeAuthorityFrameError,
    RuntimeAuthorityLifetime, RuntimeMessageDelivery,
    active_append_only_authority_protected_suffix_start, append_only_runtime_authority_is_active,
    has_append_only_runtime_authority_policy, is_human_user_message, is_runtime_owned_message,
    is_runtime_owned_provenance, mark_append_only_required_context,
    mark_append_only_runtime_authority_policy, mark_runtime_owned_message,
    parse_append_only_runtime_authority_frame, render_append_only_runtime_authority_frame,
    runtime_authority_kind, runtime_authority_lifetime, runtime_message_delivery,
    runtime_message_delivery_from_provenance, runtime_owned_message,
};
pub use semantic_read_cache::{
    SEMANTIC_READ_CACHE_CONTRACT_VERSION, SEMANTIC_READ_CONDITION_ACK_METADATA_KEY,
    SEMANTIC_READ_CONDITION_CONTRACT_VERSION, SEMANTIC_READ_OBSERVATION_CONTRACT_VERSION,
    SEMANTIC_READ_OBSERVATION_MAX_BYTES, SemanticFreshnessFact, SemanticFreshnessScope,
    SemanticReadCacheContractError, SemanticReadCacheKey, SemanticReadCacheLimits,
    SemanticReadCacheLookup, SemanticReadCondition, SemanticReadConditionAck,
    SemanticReadFreshnessContext, SemanticReadFreshnessResolution,
    SemanticReadFreshnessUnavailableReason, SemanticReadObservation,
};
pub use session_coordination::{
    ActorContextV1, ActorKindV1, AuthorityEpochsV1, CANONICAL_TURN_DELTA_SCHEMA_VERSION,
    CONTEXT_MANIFEST_NODE_SCHEMA_VERSION, CONVERSATION_AUTHORITY_ENVELOPE_SCHEMA_VERSION,
    CONVERSATION_SEGMENT_SCHEMA_VERSION, CanonicalDeltaModeV1, CanonicalTurnDeltaV1,
    ContextManifestNodeV1, ConversationAuthorityEnvelopeV1, ConversationSegmentRefV1,
    ConversationSegmentV1, ConversationWriterLeaseV1, CoordinatorConflictOptionV1,
    CoordinatorMutationV1, EXECUTION_GRANT_SCHEMA_VERSION, ExecutionGrantClaimsV1,
    SESSION_COORDINATION_SCHEMA_VERSION, SessionContextHeadV1, SessionCoordinationValidationError,
    SessionKeyV1, SessionSurfaceV1, SignedExecutionGrantV1, TurnReservationV1,
};
pub use session_cursor::{
    CONVERSATION_COMMIT_SCHEMA_VERSION, CONVERSATION_PROJECTION_SCHEMA_VERSION,
    ConversationCommitV1, ConversationDeltaV1, ConversationReplaceReason,
    DEFAULT_CONVERSATION_BRANCH_ID, SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION,
    SESSION_CURSOR_SCHEMA_VERSION, SessionCursorV1, canonical_conversation_identity,
    canonical_conversation_root, canonical_conversation_serialized_len, json_serialized_len,
};
pub use session_fork::{
    ForkBasisDimensionV1, ForkDimensionDispositionV1, ForkDimensionEvidenceV1,
    ForkExcludedAuthorityV1, SESSION_FORK_MANIFEST_SCHEMA_VERSION, SessionForkActivationV1,
    SessionForkManifestV1, SessionForkStateV1, SessionForkValidationError, SharedManifestPrefixV1,
};
pub use session_handoff::{
    HandoffOperationWatermarksV1, HandoffRiskEvidenceV1, MANIFEST_DELTA_SCHEMA_VERSION,
    MAX_HANDOFF_EFFECT_IDENTITIES, ManifestDeltaV1, SESSION_ATTACHMENT_SCHEMA_VERSION,
    SESSION_HANDOFF_SCHEMA_VERSION, SessionAttachmentModeV1, SessionAttachmentV1,
    SessionHandoffModeV1, SessionHandoffRecordV1, SessionHandoffStateV1,
    SessionHandoffValidationError, SessionPlacementV1, WorkspaceHandoffEvidenceV1,
    valid_transition,
};
pub use stop_hooks::{StopHook, StopHookObligations};
pub use tool_idempotency::{ToolIdempotency, classify_tool_idempotency};
pub use tool_invocation::{
    DispatchCertainty, DurableToolReference, TOOL_INVOCATION_CACHE_COMPLETION_CONTRACT_VERSION,
    TOOL_INVOCATION_CONTRACT_VERSION, TOOL_INVOCATION_DISPATCH_OWNER_MAX_BYTES,
    TOOL_INVOCATION_RESULT_ARTIFACT_METADATA_KEY, TOOL_INVOCATION_RESULT_CLASSIFIER_MAX_BYTES,
    TOOL_INVOCATION_RESULT_MAX_BYTES, TOOL_INVOCATION_RESULT_METADATA_MAX_BYTES,
    TOOL_INVOCATION_RESULT_METADATA_MAX_DEPTH, TOOL_INVOCATION_RESULT_METADATA_MAX_NODES,
    TOOL_INVOCATION_RESULT_OUTPUT_MAX_BYTES, TOOL_INVOCATION_RUN_CLOSURE_CONTRACT_VERSION,
    ToolInvocationCompletionRef, ToolInvocationCompletionSource, ToolInvocationContractError,
    ToolInvocationDecision, ToolInvocationDispatchLease, ToolInvocationFingerprint,
    ToolInvocationIdentity, ToolInvocationPrepareOutcome, ToolInvocationRecord,
    ToolInvocationResultPayload, ToolInvocationState, ToolInvocationTerminalOutcome,
    canonical_public_arguments_hash, canonical_public_tool_arguments,
};
pub use tool_result_projection::{
    TOOL_RESULT_PROJECTION_POLICY_VERSION, TOOL_RESULT_PROJECTION_RENDERER_VERSION,
    ToolResultProjectionBindingV1, ToolResultProjectionDecisionV1,
    ToolResultProjectionDispositionV1, ToolResultProjectionFallbackV1, ToolResultProjectionRangeV1,
    ToolResultProjectionReceiptV1, ToolResultProjectionWireStateV1,
};
pub use turn_provenance::{
    TURN_MESSAGE_PROVENANCE_FIELD, TURN_MESSAGE_PROVENANCE_SCHEMA_VERSION,
    TurnMessageProvenanceError, TurnMessageProvenanceV1, clear_turn_message_provenance,
    mark_turn_message, turn_message_provenance,
};
pub use user_intent::{
    ObjectiveRelation, USER_TURN_SEMANTICS_FIELD, USER_TURN_SEMANTICS_SCHEMA_VERSION, UserFeedback,
    UserFeedbackKind, UserFeedbackTarget, UserIntentDelivery, UserIntentStatus, UserTurnSemantics,
    UserTurnSemanticsError, mark_user_turn_semantics, user_turn_semantics,
};
pub use verification_frontier::{
    BoundVerificationFrontier, BoundWorkspaceObservation, VerificationEvidence,
    VerificationHandoff, VerificationUnavailable, WorkspaceMutationSource,
    WorkspaceObservationProof,
};

pub mod permission_control;
pub use permission_control::{
    RunPermissionModeApplied, RunPermissionModeRequest, RunPermissionModeSelection,
    RunPermissionModeSnapshot,
};
