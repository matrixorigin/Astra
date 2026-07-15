//! Core turn types for astra runtime.
//!
//! This crate provides foundational types used during turn execution,
//! extracted from the monolithic runtime crate for better modularity.

mod agent_communication;
mod agent_transcript_evidence;
mod agent_transcript_location;
mod context_window;
mod correction_signal;
mod implicit_feedback;
mod memory_ranking;
mod memory_structure;
mod memory_writability;
mod provider_contract;
mod result_quality;
mod runtime_scaffolding;
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
pub use context_window::{ContextWindowUsage, ContextWindowUsageSource};
pub use correction_signal::{
    UserCorrectionSignalKind, classify_user_correction_signal, has_durable_correction_directive,
    is_user_correction_signal,
};
pub use implicit_feedback::{
    ImplicitSignal, StructuredFeedback, detect_implicit_feedback_signal,
    implicit_feedback_context_injection, implicit_feedback_rating,
};
pub use memory_ranking::{
    PERSISTENT_TYPES, RankableMemory, SESSION_SCOPED_TYPE, freshness_suffix_for,
    is_persistent_type, partition_by_scope, sort_by_retrieval_score,
};
pub use memory_structure::{
    PERSISTENT_MEMORY_TYPES, PersistentStoreRejection, is_persistent_memory_type,
    should_store_persistent_memory, validate_persistent_memory_content,
};
pub use memory_writability::{is_transient_runtime_status_text, should_store_in_memory};
pub use provider_contract::{
    DescriptorVersion, NativeToolId, ProviderBindingRef, ProviderCallOutcome, ProviderCallPayload,
    ProviderClaim, ProviderClaimSource, ProviderClaimTrust, ProviderContractError,
    ProviderDiscoverySnapshot, ProviderIdentity, ProviderProtocolId, ProviderRejection,
    ProviderRejectionCode, ProviderResolverVersion, ProviderSemanticDiagnostic,
    ProviderSemanticDiagnosticCode, ProviderTaskSupport, ProviderToolClaims,
    ProviderToolDeclaration, PublicToolAlias, ResolvedConcurrencyBaseline, ResolvedProviderClaim,
    ResolvedProviderSnapshot, ResolvedProviderSnapshotRef, ResolvedProviderToolClaims,
    ResolvedSemanticCacheBaseline, ResolvedToolDescriptor, ResolvedToolDescriptorDraft,
    ResolvedToolDescriptorRef, ResolvedToolEffect, ResolvedToolIdempotency, ResolvedToolSemantics,
    ToolIdentity,
};
pub use result_quality::{ResultQuality, classify_result, quality_feedback};
pub use runtime_scaffolding::{
    SCAFFOLDING_BODY_PREFIXES, is_runtime_scaffolding_message,
    scaffolding_body_prefixes_for_filtering,
};
pub use tool_idempotency::{ToolIdempotency, classify_tool_idempotency};
pub use tool_invocation::{
    DispatchCertainty, DurableToolReference, TOOL_INVOCATION_CONTRACT_VERSION,
    ToolInvocationContractError, ToolInvocationFingerprint, ToolInvocationIdentity,
    ToolInvocationPrepareOutcome, ToolInvocationRecord, ToolInvocationResultPayload,
    ToolInvocationState, ToolInvocationTerminalOutcome, canonical_public_arguments_hash,
    canonical_public_tool_arguments,
};
pub use user_intent::{UserIntentDelivery, UserIntentStatus};
