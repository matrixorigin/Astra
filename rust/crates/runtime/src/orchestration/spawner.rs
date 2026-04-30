//! Dynamic agent spawner — runtime agent creation and lifecycle management.

use crate::messaging::SubRunInfo;
use astra_messaging::router::AgentMailboxRouter;
use astra_messaging::types::AgentAddress;
use astra_turn_core::fork_prefix_store::PrefixCaptureSink;
use astra_turn_core::fork_reconstruct::reconstruct_messages;
use astra_turn_core::fork_resolve::{
    PrefixResolveOutcome, SpawnResolveContext, resolve_inherit_prefix,
};
use astra_turn_core::orchestration_context_cache::SharedContextCache;
use astra_turn_core::orchestration_progress::{
    AgentProgressEvent, ProgressBroadcaster, ProgressEventType,
};
use astra_turn_core::orchestration_spawn_tool::{SpawnAgentInput, SpawnAgentOutput};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::RwLock;
use uuid::Uuid;

use async_trait::async_trait;

// ─── Constants ──────────────────────────────────────────────────────────────

/// Sentinel run ID for the top-level (root) agent.
pub const ROOT_RUN_ID: &str = "root";

// ─── Spawn Context ──────────────────────────────────────────────────────────

/// Context provided by the parent agent when spawning a child.
#[derive(Debug, Clone)]
pub struct SpawnContext {
    /// The parent's run ID.
    pub parent_run_id: String,
    /// The parent's agent ID (for tracking delegation chains).
    pub parent_agent_id: String,
    /// Current nested agent/sub-run depth of the parent.
    pub recursion_depth: u8,
    /// Working directory for the spawned agent.
    pub working_dir: PathBuf,
    /// Permissions inherited from the parent agent.
    pub inherited_permissions: Option<super::permission_sync::InheritedPermissions>,
    /// Skills inherited from the parent agent (subset of parent's active skills).
    pub inherited_skills: Vec<String>,
}

// ─── Agent Status ───────────────────────────────────────────────────────────

// Re-export from turn-core (canonical definitions live there).
pub use astra_turn_core::orchestration_types::{
    AgentStatus, SpawnedAgentInfo, SpawnedAgentMetrics,
};

/// Permission summary for display purposes.
#[derive(Debug, Clone, Default)]
pub struct PermissionSummary {
    /// Permission mode (auto, prompt, deny).
    pub mode: String,
    /// Number of explicit allow rules.
    pub allow_rules: u32,
    /// Number of explicit deny rules.
    pub deny_rules: u32,
    /// Whether this agent has a parent for permission escalation.
    pub has_parent: bool,
    /// Recent permission denials (tool names).
    pub recent_denials: Vec<String>,
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
    /// Permission summary for this agent.
    pub permission_summary: PermissionSummary,
}

// SpawnedAgentInfo is re-exported from orchestration_types above.

impl From<&SpawnedAgentState> for SpawnedAgentInfo {
    fn from(state: &SpawnedAgentState) -> Self {
        Self {
            agent_id: state.agent_id.clone(),
            run_id: state.run_id.clone(),
            parent_run_id: state.parent_run_id.clone(),
            agent_type: state.agent_type.clone(),
            description: state.description.clone(),
            status: state.status.clone(),
            started_at: state.started_at,
            metrics: state.metrics.clone(),
            has_permission_issues: state.metrics.tools_blocked > 0,
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
    /// Current nested agent/sub-run depth of the spawned child loop.
    pub recursion_depth: u8,
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
    pub mailbox: Option<astra_messaging::router::AgentMailbox>,
    /// Optional progress emitter for broadcasting turn completion events.
    pub progress_emitter: Option<astra_turn_core::orchestration_progress::AgentProgressEmitter>,
    /// Optional shared context cache for cross-agent knowledge sharing.
    pub context_cache: Option<Arc<SharedContextCache>>,
    /// Inherited permissions from parent agent.
    pub inherited_permissions: Option<super::permission_sync::InheritedPermissions>,
    /// Parent agent address for permission requests (if this is a child agent).
    pub parent_address: Option<astra_messaging::types::AgentAddress>,
    /// Permission context for runtime permission management.
    /// Created from inherited_permissions or as a fresh root context.
    pub permission_context:
        Option<std::sync::Arc<tokio::sync::RwLock<super::permission_sync::PermissionSyncContext>>>,
    /// Skills inherited from parent agent.
    pub inherited_skills: Vec<String>,
    /// Captured parent prefix for prompt-cache inheritance. Present
    /// only when the child spawn requested inherit_prefix AND the
    /// resolver returned `Resolved`. Executors (CLI / server) that
    /// implement fork-prefix consumption prepend
    /// `inherited_prefix.prefix_messages` to the child's state.messages
    /// and emit a `ForkCacheEvent` after the child's first response.
    /// Executors that don't yet support it can ignore this field — the
    /// child will simply run without cache inheritance (equivalent to
    /// the PR 4 soft-fallback path).
    pub inherited_prefix: Option<InheritedChildPrefix>,
}

/// Payload an executor needs to consume an inherited prefix.
///
/// Assembled by the spawner at spawn time from a resolved
/// [`ForkPrefix`]. Held as a plain struct (not an `Arc<ForkPrefix>`)
/// so the executor sees exactly the inputs it needs without also
/// needing to know about prefix storage internals.
#[derive(Debug, Clone)]
pub struct InheritedChildPrefix {
    /// Cross-reference to the captured prefix. Forwarded into the
    /// `ForkCacheEvent` the executor emits after the child's first
    /// response, so telemetry can join back to the capture.
    pub prefix_id: String,
    /// Parent run id (for the same join key).
    pub parent_run_id: String,
    /// Provider-scoped model id the prefix was captured against;
    /// required for ForkCacheEvent payload.
    pub provider: astra_turn_core::fork_prefix::ProviderKind,
    /// Reconstructed message array to prepend to the child's
    /// `state.messages` — the output of
    /// [`astra_turn_core::fork_reconstruct::reconstruct_messages`]
    /// called on the prefix with no additional suffix (the executor
    /// appends its own child task message).
    pub prefix_messages: Vec<serde_json::Value>,
    /// Estimated cache-eligible tokens from the parent's perspective.
    /// Used as the `expected_cache_read_tokens` baseline when the
    /// executor evaluates the child's first response for a
    /// `ForkCacheEvent`. Zero is a valid sentinel for "no estimate
    /// available" — the evaluator handles it via the degenerate
    /// branch in `evaluate_fork_cache`.
    pub expected_cache_read_tokens: u64,
}

impl std::fmt::Debug for SpawnRunConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnRunConfig")
            .field("run_id", &self.run_id)
            .field("agent_id", &self.agent_id)
            .field("recursion_depth", &self.recursion_depth)
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
    /// Final permission summary for UI/status surfaces.
    pub permission_summary: Option<PermissionSummary>,
    /// Number of permission requests sent to parent.
    pub permission_requests: u32,
    /// Number of permission requests approved by parent.
    pub permission_requests_approved: u32,
    /// Number of tools blocked by permission.
    pub tools_blocked: u32,
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
    /// Optional session ID for persisting agent state to journal.
    session_id: Option<String>,
    /// Agent type registry (builtins + user-defined).
    agent_registry: astra_turn_core::orchestration_team_config::AgentRegistry,
    /// Completed agents archive for history queries.
    completed_agents: Arc<RwLock<Vec<SpawnedAgentState>>>,
    /// JoinSet tracking all in-flight background agent tasks for graceful shutdown drain.
    /// Shared across `clone_for_task` clones so every background handle lands here.
    background_tasks: Arc<std::sync::Mutex<tokio::task::JoinSet<()>>>,
    /// Optional fork-prefix store for cache inheritance across
    /// parent/child spawns. When `None` (default), spawn behavior is
    /// identical to pre-fork-prefix builds — existing callers are
    /// unaffected until they opt in via `with_prefix_store`.
    prefix_store: Option<Arc<dyn PrefixCaptureSink>>,
    /// Resolve outcomes keyed by spawned agent_id. Populated on every
    /// spawn, including spawns that produced no inherit request
    /// (they record `Disabled`) so telemetry / observability layers
    /// can distinguish "nobody asked" from "asked but fell back".
    /// Size-bounded implicitly by agent lifecycle: the CLI layer
    /// should evict entries via `clear_prefix_resolve` when the
    /// corresponding agent completes.
    prefix_resolve_outcomes: Arc<RwLock<HashMap<String, PrefixResolveOutcome>>>,
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
            session_id: None,
            agent_registry:
                astra_turn_core::orchestration_team_config::AgentRegistry::builtins_only(),
            completed_agents: Arc::new(RwLock::new(Vec::new())),
            background_tasks: Arc::new(std::sync::Mutex::new(tokio::task::JoinSet::new())),
            prefix_store: None,
            prefix_resolve_outcomes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new spawner with a shared progress broadcaster.
    ///
    /// Use this when delegation sub-runs also need to emit to the same broadcaster.
    pub fn with_broadcaster(
        mailbox_router: Arc<AgentMailboxRouter>,
        progress_broadcaster: Arc<ProgressBroadcaster>,
    ) -> Self {
        Self {
            mailbox_router,
            active_agents: Arc::new(RwLock::new(HashMap::new())),
            progress_broadcaster,
            context_cache: Arc::new(SharedContextCache::default()),
            executor: None,
            session_id: None,
            agent_registry:
                astra_turn_core::orchestration_team_config::AgentRegistry::builtins_only(),
            completed_agents: Arc::new(RwLock::new(Vec::new())),
            background_tasks: Arc::new(std::sync::Mutex::new(tokio::task::JoinSet::new())),
            prefix_store: None,
            prefix_resolve_outcomes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new spawner with a custom context cache.
    pub fn with_context_cache(
        mailbox_router: Arc<AgentMailboxRouter>,
        context_cache: Arc<SharedContextCache>,
    ) -> Self {
        Self {
            mailbox_router,
            active_agents: Arc::new(RwLock::new(HashMap::new())),
            progress_broadcaster: Arc::new(ProgressBroadcaster::default()),
            context_cache,
            executor: None,
            session_id: None,
            agent_registry:
                astra_turn_core::orchestration_team_config::AgentRegistry::builtins_only(),
            completed_agents: Arc::new(RwLock::new(Vec::new())),
            background_tasks: Arc::new(std::sync::Mutex::new(tokio::task::JoinSet::new())),
            prefix_store: None,
            prefix_resolve_outcomes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Set the executor for running spawned agents.
    pub fn with_executor(mut self, executor: Arc<dyn SpawnAgentExecutor>) -> Self {
        self.executor = Some(executor);
        self
    }

    /// Install a fork-prefix store. When set, `spawn` resolves any
    /// `InheritPrefixSpec` in the request and records the outcome
    /// for later query via `last_prefix_resolve`. When unset,
    /// `spawn` behaves as if inherit_prefix were never requested —
    /// fully backwards compatible with pre-fork-prefix callers.
    pub fn with_prefix_store(mut self, store: Arc<dyn PrefixCaptureSink>) -> Self {
        self.prefix_store = Some(store);
        self
    }

    /// Query the resolve outcome recorded for a spawned agent.
    /// Returns `None` if the agent was never spawned by this
    /// spawner, or if its outcome has been cleared via
    /// `clear_prefix_resolve`. Every successful `spawn` records
    /// exactly one outcome, even in the no-inherit case (recorded
    /// as `Disabled`).
    pub async fn last_prefix_resolve(&self, agent_id: &str) -> Option<PrefixResolveOutcome> {
        self.prefix_resolve_outcomes
            .read()
            .await
            .get(agent_id)
            .cloned()
    }

    /// Drop the recorded resolve outcome for an agent. CLI layers
    /// should call this when the agent completes so the map
    /// doesn't grow unbounded over a long-running runtime process.
    pub async fn clear_prefix_resolve(&self, agent_id: &str) {
        self.prefix_resolve_outcomes.write().await.remove(agent_id);
    }

    /// Enable journal persistence for agent lifecycle events.
    pub fn with_session(mut self, session_id: String) -> Self {
        self.session_id = Some(session_id);
        self
    }
    /// Get a reference to the agent registry.
    pub fn agent_registry(&self) -> &astra_turn_core::orchestration_team_config::AgentRegistry {
        &self.agent_registry
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
        let agent_def = self
            .agent_registry
            .get(&input.agent_type)
            .ok_or_else(|| SpawnError::UnknownAgentType(input.agent_type.clone()))?;
        let child_recursion_depth =
            astra_turn_core::agentic_recursion_guard::checked_child_recursion_depth(
                context.recursion_depth,
            )
            .map_err(SpawnError::DepthLimitExceeded)?;

        // 2. Generate IDs
        let agent_name = input.name.clone().unwrap_or_else(|| {
            format!("{}_{}", input.agent_type, &Uuid::new_v4().to_string()[..8])
        });
        let run_id = Uuid::new_v4().to_string();
        let agent_id = format!("{}@{}", agent_name, &run_id[..8]);

        // 3. Determine model and turns
        let model = input
            .model
            .clone()
            .unwrap_or_else(|| agent_def.default_model.clone());
        let max_turns = input.max_turns.unwrap_or(agent_def.max_turns);

        // 3b. Resolve fork-prefix inheritance before any side effects
        // (mailbox, worktree, active_agents state). A hard-fail from
        // `required=true` must NOT leave half-constructed state
        // behind; soft-fallback recording only happens for spawns
        // that are definitely going to succeed.
        //
        // The resolver itself is a pure function over a store; if no
        // store is configured we skip even building the context
        // (saves a clone + RwLock write in the common path).
        let resolve_outcome = if let Some(store) = self.prefix_store.as_ref() {
            // Infer the child's provider from the model string via
            // the same normalization scheme PR 1 uses for capture
            // (`ProviderKind::from_provider_hint`). Ensures the
            // child's resolve context matches the provider that
            // captured the parent prefix, so
            // Anthropic-captured prefixes resolve for Anthropic
            // children, OpenAI-captured for OpenAI, etc. Providers
            // that astra doesn't yet wire-compatibly reconstruct
            // for (OpenAI / Bedrock / Other) will still resolve
            // against a matching capture and carry the prefix into
            // SpawnRunConfig; executor-side consumption is gated
            // by the sink the caller installs.
            let child_provider = astra_turn_core::fork_prefix::ProviderKind::from_provider_hint(&model);
            let resolve_ctx = SpawnResolveContext {
                caller_run_id: Some(context.parent_run_id.clone()),
                child_provider,
                child_model_id: model.clone(),
                child_max_output_tokens: input.max_output_tokens,
            };
            resolve_inherit_prefix(input.inherit_prefix.as_ref(), &resolve_ctx, store.as_ref())
        } else {
            // No store configured — inherit_prefix is silently
            // ignored. Record Disabled so observability is uniform
            // regardless of store presence.
            PrefixResolveOutcome::Disabled
        };
        if let PrefixResolveOutcome::Failed { reason } = &resolve_outcome {
            return Err(SpawnError::PrefixInheritanceRequired {
                reason: format!("{reason:?}"),
            });
        }

        // 4. Register mailbox if named
        let mailbox = if input.name.is_some() {
            let addr = AgentAddress::new(&run_id, &agent_id);
            let delegation_id = Some(context.parent_run_id.clone());
            match self
                .mailbox_router
                .register(addr.clone(), delegation_id)
                .await
            {
                Ok(mb) => Some(mb),
                Err(e) => {
                    return Err(SpawnError::MailboxRegistration(e.to_string()));
                }
            }
        } else {
            None
        };

        let messaging_address = mailbox.as_ref().map(|mb| mb.address.clone());
        if messaging_address.is_some() {
            let depth = self
                .mailbox_router
                .run_depth(&context.parent_run_id)
                .await
                .unwrap_or(0)
                + 1;
            self.mailbox_router
                .record_sub_run(SubRunInfo {
                    run_id: run_id.clone(),
                    parent_run_id: context.parent_run_id.clone(),
                    delegation_id: context.parent_run_id.clone(),
                    agent_id: agent_id.clone(),
                    depth,
                })
                .await;
        }

        // 5. Build permission summary
        let permission_summary = build_permission_summary(context);

        // 5b. Create isolated worktree if requested
        let worktree_path = if input.isolated {
            match create_agent_worktree(&context.working_dir, &run_id) {
                Ok(path) => Some(path),
                Err(e) => {
                    return Err(SpawnError::WorktreeCreation(format!(
                        "failed to create worktree for {agent_id}: {e}"
                    )));
                }
            }
        } else {
            None
        };

        // 6. Register state
        let state = SpawnedAgentState {
            agent_id: agent_id.clone(),
            run_id: run_id.clone(),
            parent_run_id: context.parent_run_id.clone(),
            agent_type: input.agent_type.clone(),
            description: input.description.clone(),
            status: AgentStatus::Initializing,
            messaging_address: messaging_address.clone(),
            worktree_path: worktree_path.clone(),
            started_at: SystemTime::now(),
            metrics: Default::default(),
            permission_summary,
        };

        self.active_agents
            .write()
            .await
            .insert(agent_id.clone(), state);

        // 6b. Reconstruct the inherited prefix payload for the
        // executor BEFORE moving resolve_outcome into the outcomes
        // map. Called only if the resolver produced `Resolved`;
        // all other outcomes (Disabled / Fallback / Failed) leave
        // `inherited_prefix` as None so the executor runs fresh.
        // Reconstruct errors are rare (would imply corrupt capture
        // bytes) — we degrade to None rather than fail the spawn,
        // mirroring soft-fallback semantics.
        let inherited_prefix = build_inherited_child_prefix(&resolve_outcome);

        // 6c. Record the resolve outcome. We do this after the
        // active_agents insert so any observer who sees the agent
        // via `list_agents` can safely look up its resolve outcome
        // without a race. Key is agent_id (not run_id) because
        // callers see agent_id in `SpawnAgentOutput::Launched`.
        self.prefix_resolve_outcomes
            .write()
            .await
            .insert(agent_id.clone(), resolve_outcome);

        // 7. Emit started event
        let emitter = self.progress_broadcaster.for_agent(agent_id.clone());
        emitter.started(&input.description);
        emitter.agent_spawned(
            &run_id,
            &context.parent_run_id,
            &input.agent_type,
            &input.description,
        );

        // 7. Build parent address for permission requests
        let parent_address = astra_messaging::types::AgentAddress::new(
            &context.parent_run_id,
            &context.parent_agent_id,
        );

        // 7b. Build permission context from inherited permissions
        let permission_context = context.inherited_permissions.as_ref().map(|inherited| {
            let ctx = super::permission_sync::PermissionSyncContext::new(inherited.clone());
            std::sync::Arc::new(tokio::sync::RwLock::new(ctx))
        });

        // 8. Build run config
        let run_config = SpawnRunConfig {
            run_id: run_id.clone(),
            agent_id: agent_id.clone(),
            recursion_depth: child_recursion_depth,
            agent_type: input.agent_type.clone(),
            task: input.prompt.clone(),
            system_prompt_addendum: agent_def.system_prompt_addendum.clone(),
            model,
            max_turns,
            allowed_tools: agent_def.allowed_tools.iter().cloned().collect(),
            read_only: agent_def.read_only,
            working_dir: worktree_path.unwrap_or_else(|| context.working_dir.clone()),
            mailbox,
            progress_emitter: Some(emitter.clone()),
            context_cache: Some(Arc::clone(&self.context_cache)),
            // Inherit permissions from parent context
            inherited_permissions: context.inherited_permissions.clone(),
            // Parent address for permission requests
            parent_address: Some(parent_address),
            // Permission context for runtime permission management
            permission_context,
            // Skills inherited from parent
            inherited_skills: context.inherited_skills.clone(),
            inherited_prefix,
        };

        // 8. Execute or launch
        if input.background {
            // Background mode: launch async and return immediately.
            // The JoinHandle is tracked in `background_tasks` so `shutdown_and_wait`
            // can drain it and panics are surfaced instead of silently lost.
            if let Some(ref executor) = self.executor {
                let executor = Arc::clone(executor);
                let spawner = self.clone_for_task();
                let agent_id_clone = agent_id.clone();
                let _description = input.description.clone();

                if let Ok(mut tasks) = self.background_tasks.lock() {
                    tasks.spawn(async move {
                        let result = executor.execute(run_config).await;
                        spawner.handle_completion(&agent_id_clone, result).await;
                    });
                } else {
                    // Lock poisoned (extremely unlikely) — fall back to untracked spawn.
                    tokio::spawn(async move {
                        let result = executor.execute(run_config).await;
                        spawner.handle_completion(&agent_id_clone, result).await;
                    });
                }
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
                self.update_status(
                    &agent_id,
                    AgentStatus::Running {
                        activity: "executing".to_string(),
                    },
                )
                .await;

                let result = executor.execute(run_config).await;
                let started_at = self
                    .active_agents
                    .read()
                    .await
                    .get(&agent_id)
                    .map(|s| s.started_at)
                    .unwrap_or_else(SystemTime::now);

                match result {
                    Ok(run_result) => {
                        if let Some(state) = self.active_agents.write().await.get_mut(&agent_id) {
                            state.metrics.tool_calls = run_result.tool_calls;
                            state.metrics.prompt_tokens = run_result.prompt_tokens;
                            state.metrics.completion_tokens = run_result.completion_tokens;
                            state.metrics.permission_requests = run_result.permission_requests;
                            state.metrics.permission_requests_approved =
                                run_result.permission_requests_approved;
                            state.metrics.tools_blocked = run_result.tools_blocked;
                            if let Some(summary) = run_result.permission_summary.clone() {
                                state.permission_summary = summary;
                            }
                        }

                        let status = match run_result.status.as_str() {
                            "cancelled" => AgentStatus::Cancelled,
                            "failed" => AgentStatus::Failed {
                                error: run_result
                                    .error
                                    .clone()
                                    .unwrap_or_else(|| "agent run failed".to_string()),
                            },
                            "waiting" => AgentStatus::Idle,
                            _ => AgentStatus::Completed {
                                result: run_result.output.clone().unwrap_or_default(),
                            },
                        };
                        self.update_status(&agent_id, status).await;
                        self.unregister_mailbox(&agent_id).await;

                        let duration_ms = started_at
                            .elapsed()
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);

                        match run_result.status.as_str() {
                            "cancelled" => Ok(SpawnAgentOutput::Cancelled {
                                agent_id,
                                reason: run_result
                                    .output
                                    .unwrap_or_else(|| "cancelled".to_string()),
                                tool_calls: run_result.tool_calls,
                                duration_ms,
                            }),
                            "waiting" => Ok(SpawnAgentOutput::Waiting {
                                agent_id,
                                reason: run_result.output.unwrap_or_default(),
                                tool_calls: run_result.tool_calls,
                                duration_ms,
                            }),
                            "failed" => Ok(SpawnAgentOutput::Failed {
                                error: run_result
                                    .error
                                    .unwrap_or_else(|| "agent run failed".to_string()),
                            }),
                            _ => Ok(SpawnAgentOutput::Completed {
                                agent_id,
                                result: run_result.output.unwrap_or_default(),
                                tool_calls: run_result.tool_calls,
                                duration_ms,
                            }),
                        }
                    }
                    Err(e) => {
                        self.update_status(&agent_id, AgentStatus::Failed { error: e.clone() })
                            .await;
                        self.unregister_mailbox(&agent_id).await;

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
                if let Some(state) = self.active_agents.write().await.get_mut(agent_id) {
                    state.metrics.tool_calls = run_result.tool_calls;
                    state.metrics.prompt_tokens = run_result.prompt_tokens;
                    state.metrics.completion_tokens = run_result.completion_tokens;
                    state.metrics.permission_requests = run_result.permission_requests;
                    state.metrics.permission_requests_approved =
                        run_result.permission_requests_approved;
                    state.metrics.tools_blocked = run_result.tools_blocked;
                    if let Some(summary) = run_result.permission_summary.clone() {
                        state.permission_summary = summary;
                    }
                }

                let status = match run_result.status.as_str() {
                    "cancelled" => AgentStatus::Cancelled,
                    "failed" => AgentStatus::Failed {
                        error: run_result
                            .error
                            .clone()
                            .unwrap_or_else(|| "agent run failed".to_string()),
                    },
                    "waiting" => AgentStatus::Idle,
                    _ => AgentStatus::Completed {
                        result: run_result.output.unwrap_or_default(),
                    },
                };
                // Persist to journal before updating status
                self.persist_agent_terminated(agent_id, &run_result.status)
                    .await;
                self.update_status(agent_id, status).await;
                self.archive_agent(agent_id).await;
                self.unregister_mailbox(agent_id).await;
            }
            Err(e) => {
                self.persist_agent_terminated(agent_id, "failed").await;
                self.update_status(agent_id, AgentStatus::Failed { error: e })
                    .await;
                self.archive_agent(agent_id).await;
                self.unregister_mailbox(agent_id).await;
            }
        }
    }

    /// Persist final agent state to session journal (best-effort).
    async fn persist_agent_terminated(&self, agent_id: &str, status: &str) {
        let Some(ref sid) = self.session_id else {
            return;
        };
        let state = self.active_agents.read().await.get(agent_id).cloned();
        let Some(state) = state else { return };
        let writer = match astra_services::session_journal::JournalWriter::new(sid) {
            Ok(w) => w,
            Err(e) => {
                astra_core::agent_warn!("spawner", "journal writer init failed: {e}");
                return;
            }
        };
        let duration_ms = state
            .started_at
            .elapsed()
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let event = astra_services::session_journal::JournalEvent::agent_terminated(
            Some(sid.as_str()),
            agent_id,
            &state.run_id,
            &state.agent_type,
            status,
            state.metrics.turns_completed,
            state.metrics.tool_calls,
            state.metrics.prompt_tokens,
            state.metrics.completion_tokens,
            duration_ms,
        );
        let _ = writer.append(&event);
    }

    /// Archive a completed agent state for history queries.
    async fn archive_agent(&self, agent_id: &str) {
        if let Some(state) = self.active_agents.read().await.get(agent_id) {
            let mut completed = self.completed_agents.write().await;
            // Cap history to prevent unbounded memory growth.
            const MAX_COMPLETED_AGENTS: usize = 256;
            if completed.len() >= MAX_COMPLETED_AGENTS {
                completed.remove(0);
            }
            completed.push(state.clone());
        }
    }

    async fn unregister_mailbox(&self, agent_id: &str) {
        let messaging_address = self
            .active_agents
            .write()
            .await
            .get_mut(agent_id)
            .and_then(|state| state.messaging_address.take());

        if let Some(addr) = messaging_address
            && let Err(err) = self.mailbox_router.unregister(&addr).await
        {
            eprintln!(
                "  ⚠ messaging: failed to unregister mailbox for '{}': {}",
                agent_id, err
            );
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
            session_id: self.session_id.clone(),
            agent_registry: self.agent_registry.clone(),
            completed_agents: Arc::clone(&self.completed_agents),
            // Share the same JoinSet so shutdown can drain tasks spawned by clones.
            background_tasks: Arc::clone(&self.background_tasks),
            // Share prefix-store + resolve-outcomes map so clones
            // see/write the same view. The store is an Arc<dyn ...>
            // itself already, so cloning the Option just bumps refcount.
            prefix_store: self.prefix_store.clone(),
            prefix_resolve_outcomes: Arc::clone(&self.prefix_resolve_outcomes),
        }
    }

    /// Signal all background agents to finish and wait for them to drain.
    ///
    /// This aborts tasks that exceed `deadline` rather than leaving them as zombies.
    /// Panics inside a background task are caught via [`tokio::task::JoinError`] and
    /// logged; they do not propagate to the caller.
    pub async fn shutdown_and_wait(&self, deadline: std::time::Duration) {
        let mut set = self
            .background_tasks
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();

        if set.is_empty() {
            return;
        }

        match tokio::time::timeout(deadline, async {
            while let Some(result) = set.join_next().await {
                if let Err(e) = result {
                    if e.is_panic() {
                        astra_core::agent_warn!(
                            "spawner",
                            "background agent task panicked during shutdown drain"
                        );
                    }
                }
            }
        })
        .await
        {
            Ok(()) => {}
            Err(_) => {
                astra_core::agent_warn!(
                    "spawner",
                    "background agent drain timed out after {deadline:?}; aborting remaining tasks"
                );
                set.abort_all();
            }
        }
    }

    /// Number of in-flight background tasks currently tracked.
    /// Primarily useful for tests and observability.
    pub fn background_task_count(&self) -> usize {
        self.background_tasks.lock().map(|g| g.len()).unwrap_or(0)
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

    /// Get state of a specific agent, including archived completed agents.
    pub async fn get_agent_state_any(&self, agent_id: &str) -> Option<SpawnedAgentState> {
        if let Some(state) = self.active_agents.read().await.get(agent_id).cloned() {
            return Some(state);
        }
        self.completed_agents
            .read()
            .await
            .iter()
            .find(|state| state.agent_id == agent_id)
            .cloned()
    }

    /// Get history of completed agents (both active and archived).
    pub async fn get_agent_history(&self, parent_run_id: Option<&str>) -> Vec<SpawnedAgentInfo> {
        let mut history: Vec<SpawnedAgentInfo> = self
            .completed_agents
            .read()
            .await
            .iter()
            .filter(|s| parent_run_id.is_none_or(|pid| s.parent_run_id == pid))
            .map(SpawnedAgentInfo::from)
            .collect();
        // Also include still-active agents.
        for state in self.active_agents.read().await.values() {
            if parent_run_id.is_none_or(|pid| state.parent_run_id == pid) {
                if !history.iter().any(|h| h.agent_id == state.agent_id) {
                    history.push(SpawnedAgentInfo::from(state));
                }
            }
        }
        history
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
                    let duration_ms = state
                        .started_at
                        .elapsed()
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    ProgressEventType::Completed {
                        result_summary: result.clone(),
                        total_tool_calls: state.metrics.tool_calls,
                        total_tokens: (
                            state.metrics.prompt_tokens,
                            state.metrics.completion_tokens,
                        ),
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

/// Historical record of a terminated agent, reconstructed from journal.
#[derive(Debug, Clone)]
pub struct AgentHistoryRecord {
    pub agent_id: String,
    pub run_id: String,
    pub agent_type: String,
    pub status: String,
    pub turns_completed: u32,
    pub tool_calls: u32,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub duration_ms: u64,
    pub timestamp: String,
}

// ─── Errors ─────────────────────────────────────────────────────────────────

/// Errors that can occur during agent spawning.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("Unknown agent type: {0}")]
    UnknownAgentType(String),

    #[error("Recursion depth limit exceeded: {0}")]
    DepthLimitExceeded(String),

    #[error("Mailbox registration failed: {0}")]
    MailboxRegistration(String),

    #[error("Worktree creation failed: {0}")]
    WorktreeCreation(String),

    #[error("Delegation failed: {0}")]
    DelegationFailed(String),

    /// Fired when `inherit_prefix.required=true` but the resolver
    /// could not attach a matching prefix (missing, incompatible,
    /// or feature-disabled). Soft failures (`required=false`)
    /// produce no error; they fall back to a fresh spawn and are
    /// only visible via `last_prefix_resolve`.
    #[error("Required prefix inheritance failed: {reason}")]
    PrefixInheritanceRequired { reason: String },
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Build the [`InheritedChildPrefix`] payload handed to the executor.
///
/// Only produces `Some` when `resolve_outcome` is `Resolved`. On
/// reconstruct error (corrupt canonical bytes — rare, implies the
/// captured prefix was mangled in-flight) we degrade to `None`: the
/// child runs fresh, same as the non-resolved path. A future
/// telemetry layer (PR 5.6+) may want to emit a structured
/// "reconstruct failed" event here; currently the failure is silent
/// because no sink is wired through at this layer.
fn build_inherited_child_prefix(
    resolve_outcome: &PrefixResolveOutcome,
) -> Option<InheritedChildPrefix> {
    let prefix = match resolve_outcome {
        PrefixResolveOutcome::Resolved { prefix } => prefix,
        _ => return None,
    };
    // reconstruct_messages ONLY fails when the captured canonical
    // bytes are not a JSON array of message objects. That would
    // imply a corrupt capture — extremely rare in practice. We
    // degrade to None rather than failing the spawn, so the child
    // runs fresh (equivalent to the resolver's soft-fallback path).
    // A future telemetry layer (PR 5.6+) may want to emit a
    // "reconstruct failed" event here; currently no sink is wired
    // through so the failure is silent.
    match reconstruct_messages(prefix, Vec::new()) {
        Ok(r) => Some(InheritedChildPrefix {
            prefix_id: prefix.prefix_id.clone(),
            parent_run_id: prefix.parent_run_id.clone(),
            provider: prefix.provider.clone(),
            prefix_messages: r.messages,
            // Best-effort estimate. PR 1 doesn't carry a cache-token
            // estimate on ForkPrefix itself; the capture site could
            // plumb one through in a future PR. For now we pass 0 so
            // the evaluator's degenerate branch classifies early
            // observations as Miss or ExceededExpected until a real
            // estimate is wired — neither label is a false positive
            // against "cache worked", which preserves dashboard
            // integrity.
            expected_cache_read_tokens: 0,
        }),
        Err(_) => None,
    }
}

/// Build permission summary from spawn context.
fn build_permission_summary(context: &SpawnContext) -> PermissionSummary {
    let mut summary = PermissionSummary::default();

    if let Some(ref inherited) = context.inherited_permissions {
        summary.mode = match inherited.mode {
            super::permission_sync::PermissionMode::Auto => "auto".to_string(),
            super::permission_sync::PermissionMode::Prompt => "prompt".to_string(),
            super::permission_sync::PermissionMode::Deny => "deny".to_string(),
        };
        summary.allow_rules = inherited.allow_rules.len() as u32;
        summary.deny_rules = inherited.deny_rules.len() as u32;
        // Has parent if parent_run_id is not empty and not "root"
        summary.has_parent =
            !context.parent_run_id.is_empty() && context.parent_run_id != ROOT_RUN_ID;
    } else {
        summary.mode = "auto".to_string();
        summary.has_parent =
            !context.parent_run_id.is_empty() && context.parent_run_id != ROOT_RUN_ID;
    }

    summary
}

/// Create an isolated git worktree for a spawned agent.
///
/// Creates `<parent_dir>/.agent-worktrees/<run_id>` via `git worktree add`.
/// Returns the path on success. Falls back to a simple directory copy if
/// the parent directory is not a git repo.
fn create_agent_worktree(parent_dir: &std::path::Path, run_id: &str) -> Result<PathBuf, String> {
    let worktree_base = parent_dir.join(".agent-worktrees");
    std::fs::create_dir_all(&worktree_base)
        .map_err(|e| format!("cannot create worktree base: {e}"))?;

    let worktree_path = worktree_base.join(run_id);

    // Try git worktree first
    let output = std::process::Command::new("git")
        .args(["worktree", "add", "--detach"])
        .arg(&worktree_path)
        .arg("HEAD")
        .current_dir(parent_dir)
        .output()
        .map_err(|e| format!("git worktree exec failed: {e}"))?;

    if output.status.success() {
        return Ok(worktree_path);
    }

    // Fallback: create an empty working directory (non-git isolation)
    std::fs::create_dir_all(&worktree_path)
        .map_err(|e| format!("cannot create worktree dir: {e}"))?;
    Ok(worktree_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::delegation_engine::DelegationTracker;
    use astra_messaging::in_process::InProcessTransport;
    use astra_messaging::router::AgentMailboxRouter;
    use astra_messaging::types::{AgentMessage, MessagePayload, MessageTarget};
    use tokio::time::{Duration, sleep};

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
            ..Default::default()
        };
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            recursion_depth: 0,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
            inherited_skills: vec![],
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
            ..Default::default()
        };
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            recursion_depth: 0,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
            inherited_skills: vec![],
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
            recursion_depth: 0,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
            inherited_skills: vec![],
        };

        // Spawn two agents
        for i in 0..2 {
            let input = SpawnAgentInput {
                description: format!("Agent {}", i),
                prompt: "Test".to_string(),
                agent_type: "explore".to_string(),
                ..Default::default()
            };
            let _ = spawner.spawn(input, &context).await;
        }

        let agents = spawner.list_agents("parent-123").await;
        assert_eq!(agents.len(), 2);
    }

    #[tokio::test]
    async fn test_context_cache_shared_across_spawns() {
        use astra_turn_core::orchestration_context_cache::SharedContextCache;

        // Create a shared context cache
        let cache = Arc::new(SharedContextCache::default());

        // Create spawner with custom cache
        let spawner = DynamicAgentSpawner::with_context_cache(mock_router(), Arc::clone(&cache));

        // Verify spawner has the same cache
        assert!(Arc::ptr_eq(&cache, spawner.context_cache()));

        // Parent agent stores some knowledge
        cache.share_knowledge(
            "project/tech-stack",
            serde_json::json!({"db": "postgres"}),
            "parent-agent",
        );

        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            recursion_depth: 0,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
            inherited_skills: vec![],
        };

        // Spawn an agent
        let input = SpawnAgentInput {
            description: "Explore codebase".to_string(),
            prompt: "Explore".to_string(),
            agent_type: "explore".to_string(),
            ..Default::default()
        };
        let result = spawner.spawn(input, &context).await.unwrap();
        assert!(matches!(result, SpawnAgentOutput::Launched { .. }));

        // The cache still has the knowledge from parent
        let knowledge = cache.get_knowledge("project/tech-stack");
        assert!(knowledge.is_some());
        assert_eq!(knowledge.unwrap()["db"], "postgres");

        // Spawned agent can also add knowledge (simulated)
        cache.share_knowledge(
            "project/auth",
            serde_json::json!({"type": "jwt"}),
            "spawned-agent",
        );

        // All knowledge is accessible
        assert_eq!(cache.knowledge_count(), 2);
    }

    #[tokio::test]
    async fn test_named_spawn_records_parent_routing() {
        let router = mock_router();
        let spawner = DynamicAgentSpawner::new(router.clone());
        let mut parent_mailbox = router
            .register(AgentAddress::new("parent-123", "main"), None)
            .await
            .unwrap();
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "main".to_string(),
            recursion_depth: 0,
            inherited_permissions: None,
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
        };
        let input = SpawnAgentInput {
            description: "Named agent".to_string(),
            prompt: "Send a message".to_string(),
            agent_type: "explore".to_string(),
            name: Some("named".to_string()),
            ..Default::default()
        };

        let agent_id = match spawner.spawn(input, &context).await.unwrap() {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched output, got {other:?}"),
        };
        let state = spawner.get_agent_state(&agent_id).await.unwrap();
        let child_addr = state
            .messaging_address
            .expect("named agent should have mailbox");

        router
            .send(AgentMessage::new(
                child_addr,
                MessageTarget::Parent,
                MessagePayload::Text {
                    content: "done".into(),
                    summary: None,
                },
            ))
            .await
            .unwrap();

        let received = parent_mailbox
            .try_recv()
            .expect("parent should receive message");
        match &received.payload {
            MessagePayload::Text { content, .. } => assert_eq!(content, "done"),
            other => panic!("expected text payload, got {other:?}"),
        }
    }

    struct ImmediateSuccessExecutor;

    struct ImmediateStatusExecutor {
        status: &'static str,
        output: Option<&'static str>,
        error: Option<&'static str>,
    }

    struct CapturingDepthExecutor {
        captured_depth: std::sync::Mutex<Option<u8>>,
    }

    impl CapturingDepthExecutor {
        fn new() -> Self {
            Self {
                captured_depth: std::sync::Mutex::new(None),
            }
        }
    }

    /// Executor that captures the `inherited_prefix` field from the
    /// SpawnRunConfig so PR 5.5 tests can assert the spawner builds
    /// it correctly from the resolver outcome.
    struct CapturingPrefixExecutor {
        captured: std::sync::Mutex<Option<Option<InheritedChildPrefix>>>,
    }

    impl CapturingPrefixExecutor {
        fn new() -> Self {
            Self {
                captured: std::sync::Mutex::new(None),
            }
        }
        fn take_captured(&self) -> Option<Option<InheritedChildPrefix>> {
            self.captured.lock().unwrap().take()
        }
    }

    #[async_trait]
    impl SpawnAgentExecutor for CapturingPrefixExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            *self.captured.lock().unwrap() = Some(config.inherited_prefix.clone());
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "completed".into(),
                output: Some("ok".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    #[async_trait]
    impl SpawnAgentExecutor for ImmediateSuccessExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "completed".into(),
                output: Some("ok".into()),
                error: None,
                prompt_tokens: 1,
                completion_tokens: 1,
                tool_calls: 0,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    #[async_trait]
    impl SpawnAgentExecutor for ImmediateStatusExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: self.status.into(),
                output: self.output.map(str::to_string),
                error: self.error.map(str::to_string),
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    #[async_trait]
    impl SpawnAgentExecutor for CapturingDepthExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            *self.captured_depth.lock().unwrap() = Some(config.recursion_depth);
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "completed".into(),
                output: Some("ok".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    #[tokio::test]
    async fn test_background_completion_unregisters_mailbox() {
        let router = mock_router();
        let spawner = DynamicAgentSpawner::new(router.clone())
            .with_executor(Arc::new(ImmediateSuccessExecutor));
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "main".to_string(),
            recursion_depth: 0,
            inherited_permissions: None,
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
        };
        let input = SpawnAgentInput {
            description: "Background agent".to_string(),
            prompt: "Finish immediately".to_string(),
            agent_type: "explore".to_string(),
            name: Some("bg".to_string()),
            ..Default::default()
        };

        let agent_id = match spawner.spawn(input, &context).await.unwrap() {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched output, got {other:?}"),
        };

        for _ in 0..20 {
            if router.list_registered_agents().await.is_empty() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        assert!(
            router.list_registered_agents().await.is_empty(),
            "background completion should unregister mailbox"
        );
        let state = spawner.get_agent_state(&agent_id).await.unwrap();
        assert!(matches!(state.status, AgentStatus::Completed { .. }));
        assert!(state.messaging_address.is_none());
    }

    #[tokio::test]
    async fn test_spawn_threads_child_recursion_depth_to_run_config() {
        let executor = Arc::new(CapturingDepthExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router()).with_executor(executor.clone());
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "main".to_string(),
            recursion_depth: 2,
            inherited_permissions: None,
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
        };
        let input = SpawnAgentInput {
            description: "Depth test".to_string(),
            prompt: "Run depth test".to_string(),
            agent_type: "explore".to_string(),
            background: false, // drive the synchronous Completed path
            ..Default::default()
        };

        let result = spawner.spawn(input, &context).await.unwrap();
        assert!(matches!(result, SpawnAgentOutput::Completed { .. }));
        assert_eq!(*executor.captured_depth.lock().unwrap(), Some(3));
    }

    #[tokio::test]
    async fn test_spawn_rejects_when_recursion_depth_limit_reached() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "main".to_string(),
            recursion_depth: astra_turn_core::agentic_recursion_guard::MAX_AGENT_RECURSION_DEPTH,
            inherited_permissions: None,
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
        };
        let input = SpawnAgentInput {
            description: "Depth reject".to_string(),
            prompt: "Should fail".to_string(),
            agent_type: "explore".to_string(),
            ..Default::default()
        };

        let result = spawner.spawn(input, &context).await;
        assert!(matches!(result, Err(SpawnError::DepthLimitExceeded(_))));
    }

    #[tokio::test]
    async fn test_sync_spawn_returns_failed_output_for_failed_run() {
        let spawner = DynamicAgentSpawner::new(mock_router()).with_executor(Arc::new(
            ImmediateStatusExecutor {
                status: "failed",
                output: None,
                error: Some("boom"),
            },
        ));
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "main".to_string(),
            recursion_depth: 0,
            inherited_permissions: None,
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
        };
        let input = SpawnAgentInput {
            description: "Sync agent".to_string(),
            prompt: "Fail immediately".to_string(),
            agent_type: "explore".to_string(),
            background: false, // drive the synchronous Failed path
            ..Default::default()
        };

        let result = spawner.spawn(input, &context).await.unwrap();
        assert!(matches!(
            result,
            SpawnAgentOutput::Failed { ref error } if error == "boom"
        ));
    }

    #[tokio::test]
    async fn test_inherited_skills_passed_to_run_config() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            recursion_depth: 0,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
            inherited_skills: vec!["review-changes".to_string(), "analyze-session".to_string()],
        };
        let input = SpawnAgentInput {
            description: "Test with skills".to_string(),
            prompt: "Test".to_string(),
            agent_type: "explore".to_string(),
            ..Default::default()
        };
        // Skills are stored in context and passed through — spawner launches successfully
        let result = spawner.spawn(input, &context).await.unwrap();
        assert!(matches!(result, SpawnAgentOutput::Launched { .. }));
    }

    #[test]
    fn test_spawn_context_empty_skills_default() {
        let context = SpawnContext {
            parent_run_id: "run-1".to_string(),
            parent_agent_id: "agent-1".to_string(),
            recursion_depth: 0,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
            inherited_skills: Vec::new(),
        };
        assert!(context.inherited_skills.is_empty());
    }

    // ─── HIGH #5: Background agent shutdown drain tests ─────────────────────

    struct BlockingExecutorFactory {
        gate_tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        gate_rx: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    impl BlockingExecutorFactory {
        fn new() -> Arc<Self> {
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            Arc::new(Self {
                gate_tx: std::sync::Mutex::new(Some(tx)),
                gate_rx: std::sync::Mutex::new(Some(rx)),
            })
        }

        fn unblock(&self) {
            if let Some(tx) = self.gate_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
        }
    }

    #[async_trait]
    impl SpawnAgentExecutor for BlockingExecutorFactory {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            let rx = self.gate_rx.lock().unwrap().take();
            if let Some(rx) = rx {
                let _ = rx.await;
            }
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "completed".into(),
                output: Some("done".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    struct PanicExecutor;

    #[async_trait]
    impl SpawnAgentExecutor for PanicExecutor {
        async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            panic!("deliberate panic in background executor");
        }
    }

    fn make_bg_context() -> SpawnContext {
        SpawnContext {
            parent_run_id: "root".to_string(),
            parent_agent_id: "root".to_string(),
            recursion_depth: 0,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
            inherited_skills: vec![],
        }
    }

    fn make_bg_input() -> SpawnAgentInput {
        SpawnAgentInput {
            description: "bg test".to_string(),
            prompt: "do it".to_string(),
            agent_type: "explore".to_string(),
            ..Default::default()
        }
    }

    /// HIGH #5: background agent tracked in JoinSet; shutdown_and_wait drains it.
    #[tokio::test]
    async fn background_agent_tracked_and_drained_on_shutdown() {
        let factory = BlockingExecutorFactory::new();
        let factory2 = Arc::clone(&factory);
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory as Arc<dyn SpawnAgentExecutor>);

        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        assert!(
            matches!(result, SpawnAgentOutput::Launched { .. }),
            "background spawn should return Launched"
        );

        // Task is in flight — JoinSet should have at least one entry.
        assert!(
            spawner.background_task_count() > 0,
            "background task must be tracked before unblocking"
        );

        // Unblock the executor so it can complete.
        factory2.unblock();

        // shutdown_and_wait must drain the JoinSet within the deadline.
        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;

        assert_eq!(
            spawner.background_task_count(),
            0,
            "all background tasks must be drained after shutdown"
        );
    }

    /// HIGH #5: background agent that panics does not leave a zombie in the JoinSet.
    #[tokio::test]
    async fn background_agent_panic_does_not_leave_zombie() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(PanicExecutor) as Arc<dyn SpawnAgentExecutor>);

        let _ = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();

        // Give the panic time to propagate; shutdown_and_wait catches the JoinError.
        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;

        assert_eq!(
            spawner.background_task_count(),
            0,
            "panicked background task must not leave zombie in JoinSet"
        );
    }

    // ---------------------------------------------------------------
    // PR 4.5 — prefix-inheritance wiring into the live spawner.
    //
    // These tests define the expected behavior:
    // 1. No prefix_store configured → spawn unaffected (backwards
    //    compatible; every existing test continues passing).
    // 2. prefix_store + feature on + matching captured prefix →
    //    spawn succeeds; resolve outcome queryable as `Resolved`.
    // 3. prefix_store + feature on + missing prefix + required=true
    //    → spawn returns SpawnError::PrefixInheritanceRequired.
    // 4. prefix_store + feature on + missing prefix + required=false
    //    → spawn succeeds; resolve outcome is `Fallback`.
    // ---------------------------------------------------------------

    use astra_turn_core::fork_capture::{
        CaptureRequest, FORK_FLAG_TEST_MUTEX, capture_parent_prefix,
        restore_fork_flag_raw_for_tests, set_fork_flag_for_tests,
    };
    use astra_turn_core::fork_prefix::{
        CacheMode, ProviderKind, SystemBlock, ThinkingConfigSlice, ToolSchemaEntry,
        hash_tool_schema,
    };
    use astra_turn_core::fork_prefix_store::{InMemoryPrefixStore, PrefixCaptureSink};
    use astra_turn_core::fork_resolve::PrefixResolveOutcome;
    use astra_turn_core::orchestration_spawn_tool::InheritPrefixSpec;

    /// RAII guard copied from fork_capture tests: set flag to
    /// `enabled` for the test duration, restore raw u8 on drop.
    struct FlagGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_raw: u8,
    }
    impl FlagGuard {
        fn set(enabled: bool) -> Self {
            let lock = FORK_FLAG_TEST_MUTEX
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev_raw = set_fork_flag_for_tests(enabled);
            Self {
                _lock: lock,
                prev_raw,
            }
        }
    }
    impl Drop for FlagGuard {
        fn drop(&mut self) {
            restore_fork_flag_raw_for_tests(self.prev_raw);
        }
    }

    fn wall_now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn capture_parent_for(store: &dyn PrefixCaptureSink, parent_run_id: &str, model: &str) {
        let schema = serde_json::json!({"function": {"name": "bash"}});
        let (bytes, hash) = hash_tool_schema(&schema);
        let req = CaptureRequest {
            parent_run_id: parent_run_id.to_string(),
            parent_turn_seq: 1,
            provider: ProviderKind::Anthropic,
            model_id: model.to_string(),
            thinking: Some(ThinkingConfigSlice {
                enabled: false,
                budget_tokens: 0,
                kind: "disabled".into(),
            }),
            system_blocks: vec![SystemBlock {
                bytes: b"sys".to_vec(),
                has_cache_control: true,
            }],
            tool_schemas: vec![ToolSchemaEntry {
                name: "bash".into(),
                canonical_bytes: bytes,
                hash,
            }],
            beta_headers: vec![],
            // Must be a JSON array of message objects — the
            // reconstructor (PR 5b) parses this back on the child
            // spawn path. A raw non-JSON marker would make
            // `reconstruct_messages` fail and produce a None
            // inherited_prefix, hiding the actual resolve behavior
            // we're testing.
            canonical_prefix_bytes: serde_json::to_vec(&serde_json::json!([
                {"role": "user", "content": "parent message"}
            ]))
            .expect("static json encodes"),
            cache_mode: CacheMode::Write,
            captured_at_secs: wall_now_secs(),
            microcompact_fired_in_turn: false,
        };
        let _ = capture_parent_prefix(req, store);
    }

    fn parent_context(run_id: &str) -> SpawnContext {
        SpawnContext {
            parent_run_id: run_id.to_string(),
            parent_agent_id: "parent".to_string(),
            recursion_depth: 0,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
            inherited_skills: vec![],
        }
    }

    fn child_with_inherit(required: bool) -> SpawnAgentInput {
        SpawnAgentInput {
            description: "child".into(),
            prompt: "work".into(),
            agent_type: "explore".into(),
            // Match the captured parent's model/thinking. Default
            // "explore" agent uses whatever model is in agent_def —
            // the test's captured prefix must match that model.
            inherit_prefix: Some(InheritPrefixSpec {
                from_run_id: None, // use caller's run id
                required,
            }),
            ..Default::default()
        }
    }

    /// Extract the default model an "explore" agent uses. Tests need
    /// to capture prefixes under this model name so validate_spawn
    /// sees a match.
    fn explore_agent_model(spawner: &DynamicAgentSpawner) -> String {
        spawner
            .agent_registry()
            .get("explore")
            .expect("explore agent must exist")
            .default_model
            .clone()
    }

    #[tokio::test]
    async fn spawn_without_prefix_store_is_backwards_compatible() {
        // Existing callers that never configured a prefix_store must
        // continue to work identically — this test pins the
        // additive-only property. Even with inherit_prefix set in
        // the input, spawn must succeed (prefix request silently
        // has no effect without a store).
        let _g = FlagGuard::set(true);
        let spawner = DynamicAgentSpawner::new(mock_router());
        let input = child_with_inherit(false);
        let ctx = parent_context("parent-unused");
        let result = spawner.spawn(input, &ctx).await;
        assert!(
            matches!(result, Ok(SpawnAgentOutput::Launched { .. })),
            "spawn must succeed without store even when inherit_prefix is set, got {result:?}"
        );
    }

    #[tokio::test]
    async fn spawn_resolves_matching_captured_prefix() {
        let _g = FlagGuard::set(true);
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let spawner = DynamicAgentSpawner::new(mock_router()).with_prefix_store(store.clone());
        let model = explore_agent_model(&spawner);
        capture_parent_for(&*store, "run-parent-A", &model);

        let input = child_with_inherit(false);
        let ctx = parent_context("run-parent-A");
        let out = spawner.spawn(input, &ctx).await.unwrap();
        let agent_id = match out {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };
        let outcome = spawner
            .last_prefix_resolve(&agent_id)
            .await
            .expect("outcome must be recorded for a spawn that requested inheritance");
        assert!(
            matches!(outcome, PrefixResolveOutcome::Resolved { .. }),
            "expected Resolved, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn spawn_with_required_and_missing_prefix_hard_fails() {
        let _g = FlagGuard::set(true);
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let spawner = DynamicAgentSpawner::new(mock_router()).with_prefix_store(store);
        // No capture — store is empty.
        let input = child_with_inherit(true); // required
        let ctx = parent_context("run-no-capture");
        let result = spawner.spawn(input, &ctx).await;
        match result {
            Err(SpawnError::PrefixInheritanceRequired { .. }) => {}
            other => panic!("expected PrefixInheritanceRequired, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_with_optional_and_missing_prefix_falls_back() {
        let _g = FlagGuard::set(true);
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let spawner = DynamicAgentSpawner::new(mock_router()).with_prefix_store(store);
        let input = child_with_inherit(false); // not required
        let ctx = parent_context("run-no-capture");
        let out = spawner.spawn(input, &ctx).await.unwrap();
        let agent_id = match out {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };
        let outcome = spawner.last_prefix_resolve(&agent_id).await.unwrap();
        assert!(
            matches!(outcome, PrefixResolveOutcome::Fallback { .. }),
            "expected Fallback, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn spawn_without_inherit_spec_records_disabled() {
        // inherit_prefix=None → outcome should be Disabled, regardless
        // of whether a store is configured or the flag is on.
        let _g = FlagGuard::set(true);
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let spawner = DynamicAgentSpawner::new(mock_router()).with_prefix_store(store);
        let input = SpawnAgentInput {
            description: "child".into(),
            prompt: "work".into(),
            agent_type: "explore".into(),
            ..Default::default()
        };
        let ctx = parent_context("run-parent");
        let out = spawner.spawn(input, &ctx).await.unwrap();
        let agent_id = match out {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };
        let outcome = spawner.last_prefix_resolve(&agent_id).await.unwrap();
        assert!(
            matches!(outcome, PrefixResolveOutcome::Disabled),
            "expected Disabled, got {outcome:?}"
        );
    }

    // ---------------------------------------------------------------
    // PR 5.5 — inherited_prefix populates SpawnRunConfig when resolver
    // returns Resolved, and is None in every other outcome path. The
    // executor is the only place that sees SpawnRunConfig; we use a
    // CapturingPrefixExecutor to introspect what the spawner built.
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn resolved_prefix_populates_spawn_run_config_inherited_prefix() {
        let _g = FlagGuard::set(true);
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let exec = Arc::new(CapturingPrefixExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_prefix_store(store.clone())
            .with_executor(exec.clone() as Arc<dyn SpawnAgentExecutor>);

        let model = explore_agent_model(&spawner);
        capture_parent_for(&*store, "run-parent-A", &model);

        // Sync spawn (background=false) so the executor runs before
        // spawn() returns and we can read the captured config.
        let mut input = child_with_inherit(false);
        input.background = false;
        let ctx = parent_context("run-parent-A");
        let _ = spawner.spawn(input, &ctx).await.unwrap();

        let captured = exec
            .take_captured()
            .expect("executor must have been called exactly once");
        let inherited = captured.expect("Resolved outcome must produce Some inherited_prefix");
        assert_eq!(inherited.parent_run_id, "run-parent-A");
        assert!(!inherited.prefix_id.is_empty());
        assert!(
            !inherited.prefix_messages.is_empty(),
            "prefix_messages must carry the parent's captured messages"
        );
        assert!(matches!(
            inherited.provider,
            astra_turn_core::fork_prefix::ProviderKind::Anthropic
        ));
    }

    #[tokio::test]
    async fn fallback_outcome_leaves_inherited_prefix_none() {
        // Resolver Fallback (no matching parent capture) must yield
        // `inherited_prefix: None` on the config — executor can tell
        // from the config alone that it should run fresh.
        let _g = FlagGuard::set(true);
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let exec = Arc::new(CapturingPrefixExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_prefix_store(store)
            .with_executor(exec.clone() as Arc<dyn SpawnAgentExecutor>);

        let mut input = child_with_inherit(false);
        input.background = false;
        let ctx = parent_context("run-no-capture");
        let _ = spawner.spawn(input, &ctx).await.unwrap();

        let captured = exec.take_captured().unwrap();
        assert!(
            captured.is_none(),
            "Fallback outcome must produce None inherited_prefix, got Some(...)"
        );
    }

    #[tokio::test]
    async fn disabled_outcome_leaves_inherited_prefix_none() {
        // No inherit_prefix spec at all (most common path) — outcome
        // is Disabled and inherited_prefix is None, same as Fallback.
        let _g = FlagGuard::set(true);
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let exec = Arc::new(CapturingPrefixExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_prefix_store(store)
            .with_executor(exec.clone() as Arc<dyn SpawnAgentExecutor>);

        let input = SpawnAgentInput {
            description: "no inherit".into(),
            prompt: "work".into(),
            agent_type: "explore".into(),
            background: false,
            ..Default::default()
        };
        let ctx = parent_context("run-parent");
        let _ = spawner.spawn(input, &ctx).await.unwrap();

        let captured = exec.take_captured().unwrap();
        assert!(
            captured.is_none(),
            "Disabled outcome must produce None inherited_prefix"
        );
    }

    #[tokio::test]
    async fn inherited_prefix_messages_round_trip_byte_identical() {
        // Cache-reuse precondition: the executor-visible
        // prefix_messages, when re-serialized, must equal the bytes
        // the capture recorded. Without this, no prompt cache hit is
        // possible on the child's first API call.
        let _g = FlagGuard::set(true);
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let exec = Arc::new(CapturingPrefixExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_prefix_store(store.clone())
            .with_executor(exec.clone() as Arc<dyn SpawnAgentExecutor>);
        let model = explore_agent_model(&spawner);
        capture_parent_for(&*store, "run-parent-bid", &model);

        // Grab the captured prefix bytes from the sink directly so we
        // can diff against what the executor eventually sees.
        let stored_prefix = store
            .get_prefix("run-parent-bid")
            .expect("capture must have persisted");
        let captured_canonical = stored_prefix.canonical_prefix_bytes().clone();

        let mut input = child_with_inherit(false);
        input.background = false;
        let _ = spawner
            .spawn(input, &parent_context("run-parent-bid"))
            .await
            .unwrap();

        let captured = exec.take_captured().unwrap().unwrap();
        let reserialized = serde_json::to_vec(&captured.prefix_messages).unwrap();
        assert_eq!(
            reserialized.as_slice(),
            captured_canonical.as_slice(),
            "re-serialized prefix_messages must equal captured canonical bytes"
        );
    }
}
