//! Dynamic agent orchestration — runtime agent spawning and management.
//!
//! This module provides the ability for LLMs to dynamically spawn sub-agents
//! at runtime without pre-defined team configurations.

mod agent_result_status;
pub mod agent_tool;
pub mod agent_trace_status;
mod fork_cache_probe;
pub(crate) mod spawner;
pub mod worktree_registry;
pub mod worktree_sweep;

pub use agent_result_status::{
    AgentToolBudgetRecordProjection, AgentToolRecordActionKind, AgentToolRecordProjection,
    project_agent_tool_budget_record, project_agent_tool_record,
    render_agent_tool_budget_unfinished_detail, summarize_agent_tool_budget_result,
};
pub use agent_tool::{
    AgentToolContext, handle_agent_fanout_tool, handle_agent_get_result_action,
    handle_agent_spawn_action, handle_agent_tool, normalize_agent_spawn_args,
    recover_agent_fanout_tool_result, render_agent_runtime_binding_error,
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
    AgentFindings, CacheStats, CachedFile, Finding, FindingCategory, Knowledge, SharedContextCache,
    query_context_schema, share_context_schema,
};
pub use astra_turn_core::orchestration_progress::{
    AgentProgressEmitter, AgentProgressEvent, ProgressBroadcaster, ProgressEventType,
};
pub use astra_turn_core::orchestration_spawn_tool::{SpawnAgentInput, SpawnAgentOutput};
pub use astra_turn_core::orchestration_team_config::{AgentRegistry, AgentTypeConfig};
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
pub use spawner::{
    AgentHistoryRecord, AgentStatus, DynamicAgentSpawner, InheritedChildPrefix, PermissionSummary,
    SpawnAgentExecutor, SpawnContext, SpawnError, SpawnRunConfig, SpawnRunResult,
    SpawnStatusProjection, SpawnedAgentInfo, SpawnedAgentMetrics, SpawnedAgentState,
    WaitForAgentOutcome, project_subrun_status_to_spawn,
    spawn_completion_status_from_finish_reason,
};
