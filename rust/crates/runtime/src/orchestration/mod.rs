//! Dynamic agent orchestration — runtime agent spawning and management.
//!
//! This module provides the ability for LLMs to dynamically spawn sub-agents
//! at runtime without pre-defined team configurations.

mod fork_cache_probe;
pub(crate) mod spawner;

pub use astra_turn_core::orchestration_context_cache::{
    AgentFindings, CacheStats, CachedFile, Finding, FindingCategory, Knowledge, SharedContextCache,
    query_context_schema, share_context_schema,
};
pub use astra_turn_core::orchestration_progress::{
    AgentProgressEmitter, AgentProgressEvent, ProgressBroadcaster, ProgressEventType,
};
pub use astra_turn_core::orchestration_spawn_tool::{
    SpawnAgentInput, SpawnAgentOutput, spawn_agent_schema,
};
pub use astra_turn_core::orchestration_team_config::{AgentRegistry, AgentTypeConfig};
pub use astra_turn_core::permission_sync;
pub use fork_cache_probe::{ForkCacheProbeState, maybe_emit_fork_cache_probe};
pub use permission_sync::{
    InheritedPermissions, PermissionAction, PermissionCallback, PermissionDecision, PermissionMode,
    PermissionRequest, PermissionRequestHandler, PermissionRequestMessaging, PermissionResponse,
    PermissionResponseMessaging, PermissionRule, PermissionSyncContext, PermissionUpdate,
};
pub use spawner::{
    AgentHistoryRecord, AgentStatus, DynamicAgentSpawner, InheritedChildPrefix, PermissionSummary,
    SpawnAgentExecutor, SpawnContext, SpawnError, SpawnRunConfig, SpawnRunResult, SpawnedAgentInfo,
    SpawnedAgentMetrics, SpawnedAgentState,
};
