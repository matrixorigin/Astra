//! Dynamic agent spawner — runtime agent creation and lifecycle management.

use crate::messaging::router::AgentMailboxRouter;
use crate::messaging::types::AgentAddress;
use crate::orchestration::builtin_agents::get_agent_type_definition;
use crate::orchestration::progress::{ProgressBroadcaster, AgentProgressEvent, ProgressEventType};
use crate::orchestration::spawn_tool::{SpawnAgentInput, SpawnAgentOutput};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::RwLock;
use uuid::Uuid;

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
}

impl DynamicAgentSpawner {
    /// Create a new spawner with the given dependencies.
    pub fn new(mailbox_router: Arc<AgentMailboxRouter>) -> Self {
        Self {
            mailbox_router,
            active_agents: Arc::new(RwLock::new(HashMap::new())),
            progress_broadcaster: Arc::new(ProgressBroadcaster::default()),
        }
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

        // 3. Register mailbox if named
        let messaging_address = if input.name.is_some() {
            let addr = AgentAddress::new(&run_id, &agent_id);
            let delegation_id = Some(context.parent_run_id.clone());
            match self.mailbox_router.register(addr.clone(), delegation_id).await {
                Ok(_mailbox) => Some(addr),
                Err(e) => {
                    return Err(SpawnError::MailboxRegistration(e.to_string()));
                }
            }
        } else {
            None
        };

        // 4. Determine max turns (will be used when DelegationEngine integration is complete)
        let _max_turns = input.max_turns.unwrap_or(agent_def.max_turns);

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

        // 6. Emit progress event
        let emitter = self.progress_broadcaster.for_agent(agent_id.clone());
        emitter.started(&input.description);

        // 7. Return appropriate output
        // NOTE: Actual execution integration with DelegationEngine is TODO
        // For now we return Launched to unblock the tool schema
        if input.background {
            Ok(SpawnAgentOutput::Launched {
                agent_id,
                description: input.description,
                messaging_address: messaging_address.map(|a| a.to_string()),
            })
        } else {
            // Sync mode: would block until completion
            // For now, return launched (integration with delegation engine pending)
            Ok(SpawnAgentOutput::Launched {
                agent_id,
                description: input.description,
                messaging_address: messaging_address.map(|a| a.to_string()),
            })
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
}
