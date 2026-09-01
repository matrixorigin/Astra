//! Dynamic agent orchestration — runtime agent spawning and management.
//!
//! This module provides the ability for LLMs to dynamically spawn sub-agents
//! at runtime without pre-defined team configurations.

mod agent_result_status;
pub mod agent_tool;
pub mod agent_trace_status;
mod fork_cache_probe;
pub(crate) mod spawner;

pub use agent_result_status::{
    AgentToolBudgetRecordProjection, AgentToolRecordActionKind, AgentToolRecordProjection,
    project_agent_tool_budget_record, project_agent_tool_record,
    render_agent_tool_budget_unfinished_detail, summarize_agent_tool_budget_result,
};
pub use agent_tool::{
    AgentToolContext, AgentTranscriptLocation, WorkspaceMutationAuthority,
    handle_agent_fanout_tool, handle_agent_get_result_action,
    handle_agent_send_message_with_router, handle_agent_spawn_action, handle_agent_tool,
    normalize_agent_spawn_args, recover_agent_fanout_tool_result,
    render_agent_runtime_binding_error,
};
pub use agent_trace_status::{
    AGENT_TRACE_EVENT_CANCELLED, AGENT_TRACE_EVENT_COMPLETED, AGENT_TRACE_EVENT_FAILED,
    AGENT_TRACE_EVENT_INTERRUPTED, AGENT_TRACE_EVENT_SPAWNED, AGENT_TRACE_EVENT_WAITING,
    AGENT_TRACE_STATUS_CANCELLED, AGENT_TRACE_STATUS_COMPLETED, AGENT_TRACE_STATUS_FAILED,
    AGENT_TRACE_STATUS_INTERRUPTED, AGENT_TRACE_STATUS_RUNNING, AGENT_TRACE_STATUS_SPAWNED,
    AGENT_TRACE_STATUS_WAITING, AgentTraceLifecycleStatusKind, agent_trace_lifecycle_kind,
    agent_trace_requires_result_collection, agent_trace_status_from_event,
    agent_trace_terminal_event_type, is_agent_trace_settled_event,
};
pub use astra_turn_core::orchestration_context_cache::{
    CacheStats, CachedFile, Knowledge, SharedContextCache, query_context_schema,
    share_context_schema,
};
pub use astra_turn_core::orchestration_progress::{
    AgentProgressEmitter, AgentProgressEvent, ProgressBroadcaster, ProgressEventType,
};
pub use astra_turn_core::orchestration_spawn_tool::{SpawnAgentInput, SpawnAgentOutput};
pub use astra_turn_core::orchestration_team_config::{AgentRegistry, AgentTypeConfig};
pub use astra_turn_core::orchestration_types::CancellationOrigin;
pub mod permission_sync {
    pub use astra_turn_core::permission::sync::*;
}
pub use fork_cache_probe::{ForkCacheProbeState, maybe_emit_fork_cache_probe};
pub use permission_sync::{
    ChildPermissionMode, InheritedPermissions, PermissionAction, PermissionCallback,
    PermissionDecision, PermissionMode, PermissionRequest, PermissionRequestHandler,
    PermissionRequestMessaging, PermissionResponse, PermissionResponseMessaging, PermissionRule,
    PermissionSyncContext, PermissionSyncHandle, PermissionUpdate,
};

/// Reserved runtime sideband key for inheriting the root workspace-effect
/// decision. Model-authored context is overwritten at the root boundary.
pub(crate) const WORKSPACE_MUTATION_CONTEXT_KEY: &str = "__astra_workspace_mutation";

pub(crate) fn workspace_mutation_from_context(
    context: &std::collections::HashMap<String, serde_json::Value>,
) -> astra_config::user_profile::WorkspaceMutationIntent {
    context
        .get(WORKSPACE_MUTATION_CONTEXT_KEY)
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}
pub use spawner::{
    AgentHistoryRecord, AgentStatus, CANCELLATION_ORIGIN_UNVERIFIED, CancellationTransferOutcome,
    DescendantCancellationReason, DurableAgentReconciler, DynamicAgentSpawner,
    FanoutGroupCancellation, InheritedChildPrefix, PermissionSummary, ROOT_RUN_ID,
    SpawnAgentExecutor, SpawnContext, SpawnError, SpawnRunCancellationDurability, SpawnRunConfig,
    SpawnRunResult, SpawnStatusProjection, SpawnedAgentInfo, SpawnedAgentMetrics,
    SpawnedAgentState, WaitForAgentOutcome, project_subrun_status_to_spawn,
    spawn_completion_status_from_finish_reason,
};
