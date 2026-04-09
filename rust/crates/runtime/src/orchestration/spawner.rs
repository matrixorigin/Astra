//! Dynamic agent spawner — runtime agent creation and lifecycle management.

use crate::messaging::router::AgentMailboxRouter;
use crate::messaging::types::AgentAddress;
use crate::orchestration::builtin_agents::get_agent_type_definition;
use crate::orchestration::context_cache::SharedContextCache;
use crate::orchestration::progress::{AgentProgressEvent, ProgressBroadcaster, ProgressEventType};
use crate::orchestration::spawn_tool::{SpawnAgentInput, SpawnAgentOutput};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::RwLock;
use uuid::Uuid;

use async_trait::async_trait;

// ─── Spawn Context ──────────────────────────────────────────────────────────

/// Context provided by the parent agent when spawning a child.
#[derive(Debug, Clone)]
pub struct SpawnContext {
    /// The parent's run ID.
    pub parent_run_id: String,
    /// The parent's agent ID (for tracking delegation chains).
    pub parent_agent_id: String,
    /// Working directory for the spawned agent.
    pub working_dir: PathBuf,
    /// Permissions inherited from the parent agent.
    pub inherited_permissions: Option<super::permission_sync::InheritedPermissions>,
}

// ─── Agent Status ───────────────────────────────────────────────────────────

/// Current status of a spawned agent.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentStatus {
    Initializing,
    Running { activity: String },
    Idle,
    Completed { result: String },
    Failed { error: String },
    Cancelled,
}

impl AgentStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled)
    }
}

// ─── Spawned Agent Metrics ──────────────────────────────────────────────────

/// Metrics tracked for a spawned agent.
#[derive(Debug, Clone, Default)]
pub struct SpawnedAgentMetrics {
    pub turns_completed: u32,
    pub tool_calls: u32,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

// ─── Spawned Agent State ────────────────────────────────────────────────────

/// Full state of a spawned agent.
#[derive(Debug, Clone)]
pub struct SpawnedAgentState {
    pub agent_id: String,
    pub run_id: String,
    pub parent_run_id: String,
    pub agent_type: String,
    pub description: String,
    pub status: AgentStatus,
    pub messaging_address: Option<AgentAddress>,
    pub worktree_path: Option<PathBuf>,
    pub started_at: SystemTime,
    pub metrics: SpawnedAgentMetrics,
}

/// Summary info for listing agents (lighter than full state).
#[derive(Debug, Clone)]
pub struct SpawnedAgentInfo {
    pub agent_id: String,
    pub agent_type: String,
    pub description: String,
    pub status: AgentStatus,
    pub started_at: SystemTime,
    pub metrics: SpawnedAgentMetrics,
}

impl From<&SpawnedAgentState> for SpawnedAgentInfo {
    fn from(state: &SpawnedAgentState) -> Self {
        Self {
            agent_id: state.agent_id.clone(),
            agent_type: state.agent_type.clone(),
            description: state.description.clone(),
            status: state.status.clone(),
            started_at: state.started_at,
            metrics: state.metrics.clone(),
        }
    }
}

// ─── Spawn Agent Executor Trait ─────────────────────────────────────────────

/// Configuration for a spawned agent run.
pub struct SpawnRunConfig {
    /// Unique run ID.
    pub run_id: String,
    /// Agent ID (name@run_id).
    pub agent_id: String,
    /// The agent type (explore, code-review, task, general-purpose).
    pub agent_type: String,
    /// Detailed task prompt for the agent.
    pub task: String,
    /// System prompt addendum from agent type definition.
    pub system_prompt_addendum: String,
    /// Model to use (from agent type or override).
    pub model: String,
    /// Max turns allowed.
    pub max_turns: u32,
    /// Allowed tools for this agent type.
    pub allowed_tools: Vec<String>,
    /// Whether the agent is read-only.
    pub read_only: bool,
    /// Working directory for the agent.
    pub working_dir: PathBuf,
    /// Optional mailbox for inter-agent messaging.
    pub mailbox: Option<crate::messaging::router::AgentMailbox>,
    /// Optional progress emitter for broadcasting turn completion events.
    pub progress_emitter: Option<super::progress::AgentProgressEmitter>,
    /// Optional shared context cache for cross-agent knowledge sharing.
    pub context_cache: Option<Arc<SharedContextCache>>,
    /// Inherited permissions from parent agent.
    pub inherited_permissions: Option<super::permission_sync::InheritedPermissions>,
    /// Parent agent address for permission requests (if this is a child agent).
    pub parent_address: Option<crate::messaging::types::AgentAddress>,
}

impl std::fmt::Debug for SpawnRunConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnRunConfig")
            .field("run_id", &self.run_id)
            .field("agent_id", &self.agent_id)
            .field("agent_type", &self.agent_type)
            .field("task", &self.task)
            .field("model", &self.model)
            .field("max_turns", &self.max_turns)
            .field("mailbox", &self.mailbox.is_some())
            .finish()
    }
}

/// Result from a spawned agent run.
#[derive(Debug, Clone)]
pub struct SpawnRunResult {
    /// Agent ID.
    pub agent_id: String,
    /// Run ID.
    pub run_id: String,
    /// Final status.
    pub status: String,
    /// Output text (if completed).
    pub output: Option<String>,
    /// Error message (if failed).
    pub error: Option<String>,
    /// Total prompt tokens.
    pub prompt_tokens: u64,
    /// Total completion tokens.
    pub completion_tokens: u64,
    /// Total tool calls.
    pub tool_calls: u32,
}

/// Trait for executing spawned agent runs.
///
/// Similar to `SubRunExecutor` but specifically for spawn_agent.
/// CLI layer implements this to run the agentic loop.
#[async_trait]
pub trait SpawnAgentExecutor: Send + Sync {
    /// Execute a spawned agent run.
    async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String>;
}

// ─── Dynamic Agent Spawner ──────────────────────────────────────────────────

/// Handles dynamic agent creation at runtime.
///
/// This is the core component that allows LLMs to spawn sub-agents without
/// pre-defined team configurations.
pub struct DynamicAgentSpawner {
    /// For inter-agent messaging.
    mailbox_router: Arc<AgentMailboxRouter>,
    /// For tracking spawned agents.
    active_agents: Arc<RwLock<HashMap<String, SpawnedAgentState>>>,
    /// Progress event broadcaster.
    progress_broadcaster: Arc<ProgressBroadcaster>,
    /// Shared context cache for cross-agent knowledge sharing.
    context_cache: Arc<SharedContextCache>,
    /// Optional executor for running agents (provided by CLI layer).
    executor: Option<Arc<dyn SpawnAgentExecutor>>,
}

impl DynamicAgentSpawner {
    /// Create a new spawner with the given dependencies.
    pub fn new(mailbox_router: Arc<AgentMailboxRouter>) -> Self {
        Self {
            mailbox_router,
            active_agents: Arc::new(RwLock::new(HashMap::new())),
            progress_broadcaster: Arc::new(ProgressBroadcaster::default()),
            context_cache: Arc::new(SharedContextCache::default()),
            executor: None,
        }
    }

    /// Create a new spawner with a custom context cache.
    pub fn with_context_cache(mailbox_router: Arc<AgentMailboxRouter>, context_cache: Arc<SharedContextCache>) -> Self {
        Self {
            mailbox_router,
            active_agents: Arc::new(RwLock::new(HashMap::new())),
            progress_broadcaster: Arc::new(ProgressBroadcaster::default()),
            context_cache,
            executor: None,
        }
    }

    /// Set the executor for running spawned agents.
    pub fn with_executor(mut self, executor: Arc<dyn SpawnAgentExecutor>) -> Self {
        self.executor = Some(executor);
        self
    }

    /// Get the shared context cache.
    pub fn context_cache(&self) -> &Arc<SharedContextCache> {
        &self.context_cache
    }

    /// Check if an executor is configured.
    pub fn has_executor(&self) -> bool {
        self.executor.is_some()
    }

    /// Expose the shared mailbox router for top-level coordination tools.
    pub fn mailbox_router(&self) -> Arc<AgentMailboxRouter> {
        self.mailbox_router.clone()
    }

    /// Spawn a new agent from the given specification.
    ///
    /// This is called by the `spawn_agent` tool handler.
    pub async fn spawn(
        &self,
        input: SpawnAgentInput,
        context: &SpawnContext,
    ) -> Result<SpawnAgentOutput, SpawnError> {
        // 1. Validate agent type
        let agent_def = get_agent_type_definition(&input.agent_type)
            .ok_or_else(|| SpawnError::UnknownAgentType(input.agent_type.clone()))?;

        // 2. Generate IDs
        let agent_name = input.name.clone().unwrap_or_else(|| {
            format!("{}_{}", input.agent_type, &Uuid::new_v4().to_string()[..8])
        });
        let run_id = Uuid::new_v4().to_string();
        let agent_id = format!("{}@{}", agent_name, &run_id[..8]);

        // 3. Determine model and turns
        let model = input.model.clone().unwrap_or_else(|| agent_def.default_model.clone());
        let max_turns = input.max_turns.unwrap_or(agent_def.max_turns);

        // 4. Register mailbox if named
        let mailbox = if input.name.is_some() {
            let addr = AgentAddress::new(&run_id, &agent_id);
            let delegation_id = Some(context.parent_run_id.clone());
            match self.mailbox_router.register(addr.clone(), delegation_id).await {
                Ok(mb) => Some(mb),
                Err(e) => {
                    return Err(SpawnError::MailboxRegistration(e.to_string()));
                }
            }
        } else {
            None
        };

        let messaging_address = mailbox.as_ref().map(|mb| mb.address.clone());

        // 5. Register state
        let state = SpawnedAgentState {
            agent_id: agent_id.clone(),
            run_id: run_id.clone(),
            parent_run_id: context.parent_run_id.clone(),
            agent_type: input.agent_type.clone(),
            description: input.description.clone(),
            status: AgentStatus::Initializing,
            messaging_address: messaging_address.clone(),
            worktree_path: None, // TODO: worktree isolation
            started_at: SystemTime::now(),
            metrics: Default::default(),
        };

        self.active_agents.write().await.insert(agent_id.clone(), state);

        // 6. Emit started event
        let emitter = self.progress_broadcaster.for_agent(agent_id.clone());
        emitter.started(&input.description);

        // 7. Build parent address for permission requests
        let parent_address = crate::messaging::types::AgentAddress::new(
            &context.parent_run_id,
            &context.parent_agent_id,
        );

        // 8. Build run config
        let run_config = SpawnRunConfig {
            run_id: run_id.clone(),
            agent_id: agent_id.clone(),
            agent_type: input.agent_type.clone(),
            task: input.prompt.clone(),
            system_prompt_addendum: agent_def.system_prompt_addendum.clone(),
            model,
            max_turns,
            allowed_tools: agent_def.allowed_tools.iter().cloned().collect(),
            read_only: agent_def.read_only,
            working_dir: context.working_dir.clone(),
            mailbox,
            progress_emitter: Some(emitter.clone()),
            context_cache: Some(Arc::clone(&self.context_cache)),
            // Inherit permissions from parent context
            inherited_permissions: context.inherited_permissions.clone(),
            // Parent address for permission requests
            parent_address: Some(parent_address),
        };

        // 8. Execute or launch
        if input.background {
            // Background mode: launch async and return immediately
            if let Some(ref executor) = self.executor {
                let executor = Arc::clone(executor);
                let spawner = self.clone_for_task();
                let agent_id_clone = agent_id.clone();
                let _description = input.description.clone();
                
                tokio::spawn(async move {
                    let result = executor.execute(run_config).await;
                    spawner.handle_completion(&agent_id_clone, result).await;
                });
            }
            
            Ok(SpawnAgentOutput::Launched {
                agent_id,
                description: input.description,
                messaging_address: messaging_address.map(|a| a.to_string()),
            })
        } else {
            // Sync mode: wait for completion
            if let Some(ref executor) = self.executor {
                // Update status to running
                self.update_status(&agent_id, AgentStatus::Running { 
                    activity: "executing".to_string() 
                }).await;

                let result = executor.execute(run_config).await;
                let started_at = self.active_agents.read().await
                    .get(&agent_id)
                    .map(|s| s.started_at)
                    .unwrap_or_else(SystemTime::now);

                match result {
                    Ok(run_result) => {
                        // Update final status
                        self.update_status(&agent_id, AgentStatus::Completed {
                            result: run_result.output.clone().unwrap_or_default(),
                        }).await;

                        let duration_ms = started_at.elapsed()
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);

                        Ok(SpawnAgentOutput::Completed {
                            agent_id,
                            result: run_result.output.unwrap_or_default(),
                            tool_calls: run_result.tool_calls,
                            duration_ms,
                        })
                    }
                    Err(e) => {
                        self.update_status(&agent_id, AgentStatus::Failed {
                            error: e.clone(),
                        }).await;

                        Ok(SpawnAgentOutput::Failed { error: e })
                    }
                }
            } else {
                // No executor available - return as launched (degraded mode)
                Ok(SpawnAgentOutput::Launched {
                    agent_id,
                    description: input.description,
                    messaging_address: messaging_address.map(|a| a.to_string()),
                })
            }
        }
    }

    /// Handle completion of a background agent.
    async fn handle_completion(&self, agent_id: &str, result: Result<SpawnRunResult, String>) {
        match result {
            Ok(run_result) => {
                self.update_status(agent_id, AgentStatus::Completed {
                    result: run_result.output.unwrap_or_default(),
                }).await;
                
                // Update metrics
                if let Some(state) = self.active_agents.write().await.get_mut(agent_id) {
                    state.metrics.tool_calls = run_result.tool_calls;
                    state.metrics.prompt_tokens = run_result.prompt_tokens;
                    state.metrics.completion_tokens = run_result.completion_tokens;
                }
            }
            Err(e) => {
                self.update_status(agent_id, AgentStatus::Failed { error: e }).await;
            }
        }
    }

    /// Clone the spawner for use in spawned tasks.
    fn clone_for_task(&self) -> Self {
        Self {
            mailbox_router: Arc::clone(&self.mailbox_router),
            active_agents: Arc::clone(&self.active_agents),
            progress_broadcaster: Arc::clone(&self.progress_broadcaster),
            context_cache: Arc::clone(&self.context_cache),
            executor: self.executor.clone(),
        }
    }

    /// List all active agents spawned by a parent.
    pub async fn list_agents(&self, parent_run_id: &str) -> Vec<SpawnedAgentInfo> {
        self.active_agents
            .read()
            .await
            .values()
            .filter(|s| s.parent_run_id == parent_run_id)
            .map(SpawnedAgentInfo::from)
            .collect()
    }

    /// List all agents (no filter).
    pub async fn list_all_agents(&self) -> Vec<SpawnedAgentInfo> {
        self.active_agents
            .read()
            .await
            .values()
            .map(SpawnedAgentInfo::from)
            .collect()
    }

    /// Get state of a specific agent.
    pub async fn get_agent_state(&self, agent_id: &str) -> Option<SpawnedAgentState> {
        self.active_agents.read().await.get(agent_id).cloned()
    }

    /// Update agent status.
    pub async fn update_status(&self, agent_id: &str, status: AgentStatus) {
        if let Some(state) = self.active_agents.write().await.get_mut(agent_id) {
            state.status = status.clone();

            // Emit progress event
            let event_type = match &status {
                AgentStatus::Running { activity } => ProgressEventType::Busy {
                    activity: activity.clone(),
                },
                AgentStatus::Idle => ProgressEventType::Idle,
                AgentStatus::Completed { result } => {
                    let duration_ms = state.started_at.elapsed()
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    ProgressEventType::Completed {
                        result_summary: result.clone(),
                        total_tool_calls: state.metrics.tool_calls,
                        total_tokens: (state.metrics.prompt_tokens, state.metrics.completion_tokens),
                        duration_ms,
                    }
                }
                AgentStatus::Failed { error } => ProgressEventType::Failed {
                    error: error.clone(),
                },
                AgentStatus::Cancelled => ProgressEventType::Cancelled {
                    reason: "cancelled by parent".to_string(),
                },
                AgentStatus::Initializing => return, // No event for initializing
            };

            let timestamp_epoch_ms = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            self.progress_broadcaster.emit(AgentProgressEvent {
                agent_id: agent_id.to_string(),
                event_type,
                timestamp_epoch_ms,
            });
        }
    }

    /// Subscribe to progress events for all spawned agents.
    pub fn subscribe_progress(&self) -> tokio::sync::broadcast::Receiver<AgentProgressEvent> {
        self.progress_broadcaster.subscribe()
    }

    /// Get a reference to the progress broadcaster.
    pub fn progress_broadcaster(&self) -> Arc<ProgressBroadcaster> {
        Arc::clone(&self.progress_broadcaster)
    }
}

// ─── Errors ─────────────────────────────────────────────────────────────────

/// Errors that can occur during agent spawning.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("Unknown agent type: {0}")]
    UnknownAgentType(String),

    #[error("Mailbox registration failed: {0}")]
    MailboxRegistration(String),

    #[error("Worktree creation failed: {0}")]
    WorktreeCreation(String),

    #[error("Delegation failed: {0}")]
    DelegationFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::in_process::InProcessTransport;
    use crate::messaging::router::AgentMailboxRouter;
    use crate::server::delegation_engine::DelegationTracker;

    fn mock_router() -> Arc<AgentMailboxRouter> {
        let transport = Arc::new(InProcessTransport::new());
        let dt = Arc::new(DelegationTracker::new());
        Arc::new(AgentMailboxRouter::new(transport, dt))
    }

    #[tokio::test]
    async fn test_spawn_basic() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let input = SpawnAgentInput {
            description: "Test agent".to_string(),
            prompt: "Do a test".to_string(),
            agent_type: "explore".to_string(),
            model: None,
            background: true,
            name: None,
            max_turns: None,
            isolated: false,
            allowed_tools: None,
        };
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
        };

        let result = spawner.spawn(input, &context).await.unwrap();
        assert!(matches!(result, SpawnAgentOutput::Launched { .. }));
    }

    #[tokio::test]
    async fn test_unknown_agent_type() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let input = SpawnAgentInput {
            description: "Test".to_string(),
            prompt: "Test".to_string(),
            agent_type: "unknown-type".to_string(),
            model: None,
            background: true,
            name: None,
            max_turns: None,
            isolated: false,
            allowed_tools: None,
        };
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
        };

        let result = spawner.spawn(input, &context).await;
        assert!(matches!(result, Err(SpawnError::UnknownAgentType(_))));
    }

    #[tokio::test]
    async fn test_list_agents() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
        };

        // Spawn two agents
        for i in 0..2 {
            let input = SpawnAgentInput {
                description: format!("Agent {}", i),
                prompt: "Test".to_string(),
                agent_type: "explore".to_string(),
                model: None,
                background: true,
                name: None,
                max_turns: None,
                isolated: false,
                allowed_tools: None,
            };
            let _ = spawner.spawn(input, &context).await;
        }

        let agents = spawner.list_agents("parent-123").await;
        assert_eq!(agents.len(), 2);
    }

    #[tokio::test]
    async fn test_context_cache_shared_across_spawns() {
        use crate::orchestration::context_cache::SharedContextCache;
        
        // Create a shared context cache
        let cache = Arc::new(SharedContextCache::default());
        
        // Create spawner with custom cache
        let spawner = DynamicAgentSpawner::with_context_cache(mock_router(), Arc::clone(&cache));
        
        // Verify spawner has the same cache
        assert!(Arc::ptr_eq(&cache, spawner.context_cache()));
        
        // Parent agent stores some knowledge
        cache.share_knowledge("project/tech-stack", serde_json::json!({"db": "postgres"}), "parent-agent");
        
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
        };
        
        // Spawn an agent
        let input = SpawnAgentInput {
            description: "Explore codebase".to_string(),
            prompt: "Explore".to_string(),
            agent_type: "explore".to_string(),
            model: None,
            background: true,
            name: None,
            max_turns: None,
            isolated: false,
            allowed_tools: None,
        };
        let result = spawner.spawn(input, &context).await.unwrap();
        assert!(matches!(result, SpawnAgentOutput::Launched { .. }));
        
        // The cache still has the knowledge from parent
        let knowledge = cache.get_knowledge("project/tech-stack");
        assert!(knowledge.is_some());
        assert_eq!(knowledge.unwrap()["db"], "postgres");
        
        // Spawned agent can also add knowledge (simulated)
        cache.share_knowledge("project/auth", serde_json::json!({"type": "jwt"}), "spawned-agent");
        
        // All knowledge is accessible
        assert_eq!(cache.knowledge_count(), 2);
    }
}
