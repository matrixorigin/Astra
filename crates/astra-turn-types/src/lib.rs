//! Core turn types for astra runtime.
//!
//! This crate provides foundational types used during turn execution,
//! extracted from the monolithic runtime crate for better modularity.

mod agent_communication;
mod agent_transcript_evidence;
mod agent_transcript_location;
mod context_identity;
mod context_window;
mod inference;
mod memory_ranking;
mod memory_structure;
mod provider_contract;
mod result_quality;
mod resume;
mod runtime_scaffolding;
mod semantic_read_cache;
mod session_coordination;
mod session_cursor;
pub mod session_facts;
mod tool_idempotency;
mod tool_invocation;
mod user_intent;

pub use agent_communication::{
    AGENT_COMMUNICATION_SCHEMA_VERSION, AgentCommunicationDirection, AgentCommunicationEvent,
    AgentCommunicationParty, AgentCommunicationTarget,
};
pub use agent_transcript_evidence::AgentTranscriptEvidence;
pub use agent_transcript_location::AgentTranscriptLocation;
pub use context_identity::{
    ContextIdentityError, LLM_ARTIFACT_EVIDENCE_CONTRACT_VERSION,
    LLM_ARTIFACT_EVIDENCE_MAX_ENTRIES, LlmArtifactEvidenceEntryV1, LlmArtifactEvidenceManifestV1,
    NormalizedPromptCacheUsage, PROMPT_CACHE_IDENTITY_CONTRACT_VERSION, PromptCacheIdentityV1,
    PromptCacheInvalidationReason,
};
pub use context_window::{ContextWindowUsage, ContextWindowUsageSource};
pub use inference::{
    CLIENT_DIRECT_EXECUTION_FIELDS, InferenceInvocationScope, InferencePurpose, ModelSelection,
    client_direct_execution_field,
};
pub use memory_ranking::{
    PERSISTENT_TYPES, RankableMemory, SESSION_SCOPED_TYPE, freshness_suffix_for,
    is_persistent_type, partition_by_scope, sort_by_retrieval_score,
};
pub use memory_structure::{
    PERSISTENT_MEMORY_TYPES, PersistentStoreRejection, is_persistent_memory_type,
    should_store_persistent_memory, validate_persistent_memory_content,
};
pub use provider_contract::{
    DescriptorVersion, NativeToolId, ProviderBindingRef, ProviderCallOutcome, ProviderCallPayload,
    ProviderClaim, ProviderClaimSource, ProviderClaimTrust, ProviderContractError,
    ProviderDiscoverySnapshot, ProviderIdentity, ProviderProtocolId, ProviderRejection,
    ProviderRejectionCode, ProviderResolverVersion, ProviderSemanticCacheContract,
    ProviderSemanticDiagnostic, ProviderSemanticDiagnosticCode, ProviderTaskSupport,
    ProviderToolClaims, ProviderToolDeclaration, PublicToolAlias, ResolvedConcurrencyBaseline,
    ResolvedProviderClaim, ResolvedProviderSnapshot, ResolvedProviderSnapshotRef,
    ResolvedProviderToolClaims, ResolvedSemanticCacheBaseline, ResolvedToolDescriptor,
    ResolvedToolDescriptorDraft, ResolvedToolDescriptorRef, ResolvedToolEffect,
    ResolvedToolIdempotency, ResolvedToolSemantics, ToolIdentity,
};
pub use result_quality::{ResultQuality, classify_result, quality_feedback};
pub use resume::{
    CAUSAL_PROJECTION_ENVELOPE_SCHEMA_VERSION, CausalProjectionEnvelopeV1, CursorRelationV1,
    RESUME_BUNDLE_SCHEMA_VERSION, ResumeActivationProjectionV1, ResumeBundleV1, ResumeCandidateV1,
    ResumeCheckpointProjectionV1, ResumeDegradedReasonV1, ResumeDescriptorV1,
    ResumeProjectionSetV1, ResumeProviderProjectionV1, ResumeRepairActionV1, ResumeSelectionError,
    ResumeSourceV1, ResumeTaskProjectionV1, cursor_relation, legacy_resume_cursor,
    select_resume_bundle, select_resume_candidate_index,
};
pub use runtime_scaffolding::{
    RUNTIME_MESSAGE_PROVENANCE_FIELD, RuntimeMessageDelivery, is_runtime_owned_message,
    mark_runtime_owned_message, runtime_message_delivery, runtime_owned_message,
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
    CONVERSATION_SEGMENT_SCHEMA_VERSION, CanonicalTurnDeltaV1, ContextManifestNodeV1,
    ConversationAuthorityEnvelopeV1, ConversationSegmentRefV1, ConversationSegmentV1,
    ConversationWriterLeaseV1, CoordinatorConflictOptionV1, CoordinatorMutationV1,
    EXECUTION_GRANT_SCHEMA_VERSION, ExecutionGrantClaimsV1, SESSION_COORDINATION_SCHEMA_VERSION,
    SessionContextHeadV1, SessionCoordinationValidationError, SessionKeyV1, SessionSurfaceV1,
    SignedExecutionGrantV1, TurnReservationV1,
};
pub use session_cursor::{
    CONVERSATION_COMMIT_SCHEMA_VERSION, CONVERSATION_PROJECTION_SCHEMA_VERSION,
    ConversationCommitV1, ConversationDeltaV1, ConversationReplaceReason,
    DEFAULT_CONVERSATION_BRANCH_ID, SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION,
    SESSION_CURSOR_SCHEMA_VERSION, SessionCursorV1, canonical_conversation_root,
    canonical_conversation_serialized_len, json_serialized_len,
};
pub use tool_idempotency::{ToolIdempotency, classify_tool_idempotency};
pub use tool_invocation::{
    DispatchCertainty, DurableToolReference, TOOL_INVOCATION_CACHE_COMPLETION_CONTRACT_VERSION,
    TOOL_INVOCATION_CONTRACT_VERSION, TOOL_INVOCATION_DISPATCH_OWNER_MAX_BYTES,
    TOOL_INVOCATION_RESULT_ARTIFACT_METADATA_KEY, TOOL_INVOCATION_RESULT_CLASSIFIER_MAX_BYTES,
    TOOL_INVOCATION_RESULT_MAX_BYTES, TOOL_INVOCATION_RESULT_METADATA_MAX_BYTES,
    TOOL_INVOCATION_RESULT_METADATA_MAX_DEPTH, TOOL_INVOCATION_RESULT_METADATA_MAX_NODES,
    TOOL_INVOCATION_RESULT_OUTPUT_MAX_BYTES, TOOL_INVOCATION_RUN_CLOSURE_CONTRACT_VERSION,
    ToolInvocationCompletionSource, ToolInvocationContractError, ToolInvocationDecision,
    ToolInvocationDispatchLease, ToolInvocationFingerprint, ToolInvocationIdentity,
    ToolInvocationPrepareOutcome, ToolInvocationRecord, ToolInvocationResultPayload,
    ToolInvocationState, ToolInvocationTerminalOutcome, canonical_public_arguments_hash,
    canonical_public_tool_arguments,
};
pub use user_intent::{
    ObjectiveRelation, USER_TURN_SEMANTICS_FIELD, USER_TURN_SEMANTICS_SCHEMA_VERSION, UserFeedback,
    UserFeedbackKind, UserFeedbackTarget, UserIntentDelivery, UserIntentStatus, UserTurnSemantics,
    UserTurnSemanticsError, mark_user_turn_semantics, user_turn_semantics,
};
