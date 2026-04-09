//! Dynamic agent orchestration — runtime agent spawning and management.
//!
//! This module provides the ability for LLMs to dynamically spawn sub-agents
//! at runtime without pre-defined team configurations.

mod builtin_agents;
mod context_cache;
mod progress;
mod spawner;
mod spawn_tool;

pub use builtin_agents::{
    AgentTypeDefinition, BuiltinAgentType, get_builtin_agent_types, get_agent_type_definition,
};
pub use context_cache::{
    SharedContextCache, CachedFile, Knowledge, AgentFindings, Finding, FindingCategory,
    CacheStats, share_context_schema, query_context_schema,
};
pub use progress::{AgentProgressEvent, ProgressEventType, ProgressBroadcaster, AgentProgressEmitter};
pub use spawner::{
    DynamicAgentSpawner, SpawnContext, SpawnedAgentState, SpawnedAgentInfo, AgentStatus,
    SpawnedAgentMetrics, SpawnError, SpawnAgentExecutor, SpawnRunConfig, SpawnRunResult,
};
pub use spawn_tool::{SpawnAgentInput, SpawnAgentOutput, spawn_agent_schema};
