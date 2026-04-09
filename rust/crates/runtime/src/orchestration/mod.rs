//! Dynamic agent orchestration — runtime agent spawning and management.
//!
//! This module provides the ability for LLMs to dynamically spawn sub-agents
//! at runtime without pre-defined team configurations.

mod builtin_agents;
pub mod context_cache;
pub mod permission_sync;
mod progress;
mod spawn_tool;
mod spawner;
pub mod team_config;

pub use builtin_agents::{
    AgentTypeDefinition, BuiltinAgentType, get_agent_type_definition, get_builtin_agent_types,
};
pub use context_cache::{
    AgentFindings, CacheStats, CachedFile, Finding, FindingCategory, Knowledge, SharedContextCache,
    query_context_schema, share_context_schema,
};
pub use permission_sync::{
    InheritedPermissions, PermissionAction, PermissionCallback, PermissionDecision, PermissionMode,
    PermissionRequest, PermissionRequestHandler, PermissionResponse, PermissionRule,
    PermissionSyncContext, PermissionUpdate,
};
pub use progress::{
    AgentProgressEmitter, AgentProgressEvent, ProgressBroadcaster, ProgressEventType,
};
pub use spawn_tool::{SpawnAgentInput, SpawnAgentOutput, spawn_agent_schema};
pub use spawner::{
    AgentHistoryRecord, AgentStatus, DynamicAgentSpawner, PermissionSummary, SpawnAgentExecutor,
    SpawnContext, SpawnError, SpawnRunConfig, SpawnRunResult, SpawnedAgentInfo,
    SpawnedAgentMetrics, SpawnedAgentState,
};
pub use team_config::{AgentRegistry, AgentTypeConfig};
