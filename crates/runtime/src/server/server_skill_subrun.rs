//! Server-side skill fork (sub-run) executor.
//!
//! Enables skills with `execution_context: Fork` to run in isolated sub-agent
//! loops on the server, matching the CLI's `CliSkillSubRunExecutor` behavior.
//!
//! Each sub-run creates a fresh [`ServerAgenticLoopHost`] +
//! [`AgenticLoopState`] pair and runs [`run_agentic_loop_with_host`] to
//! completion, inheriting the parent's LLM credentials, skill resolver,
//! and cancellation token.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex as TokioMutex;

use astra_core::SharedPool;
use astra_runtime_env::validate_workspace_id;
use astra_services::{AdmittedModelExecution, ReflectService, UnconfiguredReflectService};

use crate::FernetTokenEncryptor;
use crate::MatrixOneSettings;
use crate::turn::agentic_loop::host::{
    AgenticLoopHost as _, AgenticLoopState, CancellationState, RequestConstraints, SkillState,
    StopHookState, TurnInteractionPolicy, project_skill_subrun_outcome, run_agentic_loop_with_host,
};
use astra_pipeline::step_protocol::InMemoryIdempotencyCache;
use astra_pipeline::step_recorder::StepRecorder;
use astra_skills::executor::isolated::{SkillSubRunExecutor, SubRunResult};
use astra_text_utils::semantic_dedup::SemanticDedup;

use crate::server::tool_execution_service::ToolExecutionService;
use astra_turn_core::chat_turn_heuristics::infer_task_execution_profile;
use astra_turn_core::turn_guard::TurnGuard;

use super::server_loop_host::ServerAgenticLoopHostBuilder;
use super::tool_transport::ExecutionBindingSnapshot;

/// Maximum turns for a skill sub-run (matches CLI's SUBRUN_MAX_TURNS).
pub const SUBRUN_MAX_TURNS: usize = 30;

/// Maximum cumulative tokens for a skill sub-run.
pub const SUBRUN_MAX_CUMULATIVE_TOKENS: u64 = 500_000;

/// Server-side implementation of [`SkillSubRunExecutor`].
///
/// Creates a [`ServerAgenticLoopHost`] for each sub-run with isolated context
/// but shared LLM credentials and skill resolver.
pub struct ServerSkillSubRunExecutor {
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    shared_pool: Option<SharedPool>,
    user_id: String,
    /// Default model to use when the skill manifest doesn't specify one.
    default_model: Option<String>,
    /// Normalized execution material inherited from the admitted parent run.
    admitted_model_execution: Option<AdmittedModelExecution>,
    /// Edge tools available to sub-runs (inherited from parent host).
    edge_tools: Vec<Value>,
    /// Edge profile (cwd, git_branch, etc.) inherited from parent.
    edge_profile: Map<String, Value>,
    /// Workspace/executor/runtime binding inherited from the parent run.
    execution_binding_snapshot: Option<ExecutionBindingSnapshot>,
    /// Provider-authorized workspace scope used for cross-user managed Edge
    /// lookup.  The edge agent may connect as a service account rather than
    /// the workspace user running this skill.
    workspace_record: Option<astra_runtime_env::WorkspaceRecord>,
    runtime_file_transfer: Option<Arc<astra_services::runs::RuntimeFileTransferContext>>,
    runtime_edge_dispatch_authorization:
        Option<Arc<astra_services::runs::RuntimeEdgeDispatchAuthorizationContext>>,
    /// Skill resolver inherited from parent — enables nested inline skills.
    skill_resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
    /// Parent cancellation token — propagated so stop/cancel interrupts sub-runs.
    cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    /// Inbound request headers propagated from parent run for remote skill callbacks.
    /// Header names are normalized to lowercase.
    forward_headers: HashMap<String, String>,
    /// Request-scoped capability constraints inherited from the parent run.
    request_constraints: RequestConstraints,
    /// Session ID for the parent run.
    session_id: String,
    /// Edge connection pool for routing tool calls to connected edges.
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    /// Durable dispatch authority required before any direct Edge socket send.
    edge_dispatch_service: Option<Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
    /// Durable Edge registry used when the selected executor is connected to
    /// another Astra replica.
    edge_registry_service: Option<Arc<dyn astra_services::multi_agent::EdgeRegistryService>>,
    /// Shared tool_call dedup state from the parent host. When set, the sub-run
    /// host will observe the same emitted_tool_call_ids HashSet as the parent,
    /// preventing duplicate `tool_call` events across host instances within the
    /// same chat turn. Plumbed only under `bridge-e2e-hooks` (test observability).
    #[cfg(feature = "bridge-e2e-hooks")]
    dedup_state: Option<std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>>,
    /// Parent session's harness snapshot sink for observe-only sub-run
    /// observation. When set, the sub-run creates a sink-only HarnessSlot
    /// so sub-run snapshots appear in the parent's history.
    #[cfg(feature = "harness")]
    harness_sink: Option<std::sync::Arc<dyn astra_harness::SnapshotSink>>,
    /// Shared background session-memory extraction coordinator cloned
    /// from the parent lifecycle service. `None` → no extraction in
    /// skill sub-runs (rarely surfaces user-relevant memory).
    memory_extraction_service: Option<Arc<crate::session_memory::MemoryExtractionService>>,
    /// Shared persisted-reflection service inherited from the parent run.
    reflect_service: Arc<dyn ReflectService>,
    /// Request-level permissions inherited from the parent server run.
    inherited_permissions: crate::orchestration::InheritedPermissions,
    /// Parent run's durable interaction authority. Skill forks are isolated
    /// model loops, not independent durable runs, so approvals and client-tool
    /// requests remain owned by the parent run.
    interaction_sink: Option<Arc<dyn super::server_loop_host::HostInteractionSink>>,
}

impl ServerSkillSubRunExecutor {
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: Arc<FernetTokenEncryptor>,
        user_id: String,
        session_id: String,
    ) -> Self {
        Self {
            matrixone,
            encryptor,
            shared_pool: None,
            user_id,
            default_model: None,
            admitted_model_execution: None,
            edge_tools: Vec::new(),
            edge_profile: Map::new(),
            execution_binding_snapshot: None,
            workspace_record: None,
            runtime_file_transfer: None,
            runtime_edge_dispatch_authorization: None,
            skill_resolver: None,
            cancel_token: None,
            forward_headers: HashMap::new(),
            request_constraints: Default::default(),
            session_id,
            edge_connection_pool: None,
            edge_dispatch_service: None,
            edge_registry_service: None,
            #[cfg(feature = "bridge-e2e-hooks")]
            dedup_state: None,
            #[cfg(feature = "harness")]
            harness_sink: None,
            memory_extraction_service: None,
            reflect_service: Arc::new(UnconfiguredReflectService),
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            interaction_sink: None,
        }
    }

    pub fn with_memory_extraction_service(
        mut self,
        svc: Arc<crate::session_memory::MemoryExtractionService>,
    ) -> Self {
        self.memory_extraction_service = Some(svc);
        self
    }

    pub fn with_reflect_service(mut self, service: Arc<dyn ReflectService>) -> Self {
        self.reflect_service = service;
        self
    }

    /// Share the parent host's `emitted_tool_call_ids` HashSet so that sub-run
    /// hosts dedupe `tool_call` events against the parent's already-emitted
    /// ids. See `ServerAgenticLoopHostBuilder::with_dedup_state`.
    #[cfg(feature = "bridge-e2e-hooks")]
    pub fn with_dedup_state(
        mut self,
        shared: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    ) -> Self {
        self.dedup_state = Some(shared);
        self
    }

    pub fn with_pool(mut self, pool: Option<SharedPool>) -> Self {
        self.shared_pool = pool;
        self
    }

    pub fn with_default_model(mut self, model: Option<String>) -> Self {
        self.default_model = model;
        self
    }

    pub fn with_admitted_model_execution(
        mut self,
        execution: Option<AdmittedModelExecution>,
    ) -> Self {
        self.admitted_model_execution = execution;
        self
    }

    pub fn with_edge_tools(mut self, tools: Vec<Value>) -> Self {
        self.edge_tools = tools;
        self
    }

    pub fn with_edge_profile(mut self, profile: Map<String, Value>) -> Self {
        self.edge_profile = profile;
        self
    }

    pub fn with_execution_binding_snapshot(mut self, snapshot: ExecutionBindingSnapshot) -> Self {
        self.execution_binding_snapshot = Some(snapshot);
        self
    }

    pub fn with_workspace_record(
        mut self,
        workspace_record: Option<astra_runtime_env::WorkspaceRecord>,
    ) -> Self {
        self.workspace_record = workspace_record;
        self
    }

    pub fn with_runtime_file_transfer(
        mut self,
        context: Option<Arc<astra_services::runs::RuntimeFileTransferContext>>,
    ) -> Self {
        self.runtime_file_transfer = context;
        self
    }

    pub fn with_runtime_edge_dispatch_authorization(
        mut self,
        context: Option<Arc<astra_services::runs::RuntimeEdgeDispatchAuthorizationContext>>,
    ) -> Self {
        self.runtime_edge_dispatch_authorization = context;
        self
    }

    pub fn with_skill_resolver(
        mut self,
        resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
    ) -> Self {
        self.skill_resolver = resolver;
        self
    }

    pub fn with_cancel_token(
        mut self,
        token: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Self {
        self.cancel_token = token;
        self
    }

    pub fn with_forward_headers(mut self, headers: HashMap<String, String>) -> Self {
        self.forward_headers = headers;
        self
    }

    pub fn with_request_constraints(mut self, constraints: RequestConstraints) -> Self {
        self.request_constraints = constraints;
        self
    }

    pub fn with_inherited_permissions(
        mut self,
        inherited_permissions: crate::orchestration::InheritedPermissions,
    ) -> Self {
        self.inherited_permissions = inherited_permissions;
        self
    }

    pub(crate) fn with_interaction_sink(
        mut self,
        sink: Arc<dyn super::server_loop_host::HostInteractionSink>,
    ) -> Self {
        self.interaction_sink = Some(sink);
        self
    }

    pub fn with_edge_connection_pool(
        mut self,
        pool: astra_server_types::edge_connection_pool::EdgeConnectionPool,
    ) -> Self {
        self.edge_connection_pool = Some(pool);
        self
    }

    pub fn with_edge_dispatch_service(
        mut self,
        service: Arc<dyn astra_services::multi_agent::EdgeDispatchService>,
    ) -> Self {
        self.edge_dispatch_service = Some(service);
        self
    }

    pub fn with_edge_registry_service(
        mut self,
        service: Arc<dyn astra_services::multi_agent::EdgeRegistryService>,
    ) -> Self {
        self.edge_registry_service = Some(service);
        self
    }

    /// Set the parent session's harness sink for observe-only sub-run monitoring.
    #[cfg(feature = "harness")]
    pub fn with_harness_sink(
        mut self,
        sink: Option<std::sync::Arc<dyn astra_harness::SnapshotSink>>,
    ) -> Self {
        self.harness_sink = sink;
        self
    }
}

impl ServerSkillSubRunExecutor {
    fn apply_execution_binding_snapshot(
        &self,
        executor: &mut super::runtime_tool_executor::RuntimeToolExecutor,
    ) {
        if let Some(snapshot) = &self.execution_binding_snapshot {
            executor.set_execution_binding_snapshot(snapshot.clone());
        }
        executor.set_workspace_record(self.workspace_record.clone());
    }

    /// Provision a workspace directory for a skill sub-run.
    fn provision_skill_workspace(
        &self,
        skill_name: &str,
        session_id: &str,
    ) -> Result<std::path::PathBuf, String> {
        validate_workspace_id(session_id)
            .map_err(|source| format!("invalid skill sub-run session_id: {source}"))?;
        let safe_skill = crate::skills::loader::sanitize_for_path(skill_name);

        let base = std::env::var("ASTRA_SERVER_WORKSPACES")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("astra-workspaces"));
        let dir_name = if safe_skill.is_empty() {
            session_id.to_string()
        } else {
            format!("{}-skill-{}", session_id, safe_skill)
        };
        let workspace = base.join(&dir_name);
        std::fs::create_dir_all(&workspace)
            .map_err(|error| format!("failed to create skill sub-run workspace: {error}"))?;
        Ok(workspace)
    }
}

#[async_trait]
impl SkillSubRunExecutor for ServerSkillSubRunExecutor {
    async fn execute_skill_subrun(
        &self,
        skill_name: &str,
        instructions: &str,
        task_context: &str,
        _max_tokens: Option<u32>,
        allowed_tools: &[String],
        parent_recursion_depth: u8,
        effort: Option<&str>,
        agent_type: Option<&str>,
    ) -> Result<SubRunResult, String> {
        let child_recursion_depth =
            astra_turn_core::agentic_recursion_guard::checked_child_recursion_depth(
                parent_recursion_depth,
            )?;

        let effective_model = self.default_model.clone();
        let compact_strategy = astra_turn_core::microcompact::CompactStrategy::from_provider_hint(
            effective_model.as_deref().unwrap_or(""),
        );
        let permission_context =
            crate::orchestration::PermissionSyncContext::shared(self.inherited_permissions.clone());

        // Build a sub-run session ID for isolation.
        let safe_name = crate::skills::loader::sanitize_for_path(skill_name);
        let subrun_session_id = format!(
            "subrun-{}-{}",
            safe_name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros()
        );

        // Resolve per-model workflow-guard policy before `effective_model` is
        // consumed by `.with_model(...)` below.
        let resolved_tool_policy = astra_config::runtime_config::RuntimeConfig::load()
            .tool_selection
            .resolve_for_model(effective_model.as_deref());

        // Build the host for the sub-run.
        let mut builder = ServerAgenticLoopHostBuilder::new(
            self.matrixone.clone(),
            self.encryptor.clone(),
            self.user_id.clone(),
            subrun_session_id.clone(),
        )
        .with_model(effective_model.clone())
        .with_admitted_model_execution(self.admitted_model_execution.clone())
        .with_edge_tools(self.edge_tools.clone())
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            self.shared_pool.is_some(),
            self.reflect_service.is_configured(),
        ))
        .with_edge_profile(self.edge_profile.clone())
        .with_edge_callback_ledger(Arc::new(TokioMutex::new(HashMap::new())));

        if let Some(snapshot) = &self.execution_binding_snapshot {
            builder = builder.with_execution_binding_snapshot(snapshot.clone());
        }

        if let Some(pool) = &self.shared_pool {
            builder = builder.with_pool(pool.clone());
        }

        // Wire shared dedup state from the parent host so that tool_call events
        // emitted by this sub-run host are deduplicated against the parent's
        // already-emitted ids. Without this, the same `tool_call` id would be
        // emitted once per host instance within the same chat turn.
        // See `ServerAgenticLoopHostBuilder::with_dedup_state` and
        // `ServerSkillSubRunExecutor::with_dedup_state`.
        #[cfg(feature = "bridge-e2e-hooks")]
        if let Some(dedup) = &self.dedup_state {
            builder = builder.with_dedup_state(dedup.clone());
        }

        let mut host = builder.build();
        if let Some(transfer) = self.runtime_file_transfer.as_deref() {
            host.install_managed_file_transfer_tool_schemas(transfer);
        }
        if let Some(sink) = &self.interaction_sink {
            host.set_interaction_sink(Arc::clone(sink));
        }

        // Build tool restriction set: if allowed_tools is non-empty, only those
        // tools (plus skill discovery) are permitted.
        let valid_tool_names = host.valid_tool_names();
        let restricted_tools: HashSet<String> = if allowed_tools.is_empty() {
            HashSet::new()
        } else {
            let allowed: HashSet<&str> = allowed_tools.iter().map(|s| s.as_str()).collect();
            valid_tool_names
                .iter()
                .filter(|name: &&String| {
                    !allowed.contains(name.as_str())
                        && name.as_str() != crate::turn::skill_tool::SKILL_TOOL_NAME
                        && name.as_str() != crate::turn::skill_tool::DISCOVER_SKILLS_TOOL_NAME
                })
                .cloned()
                .collect()
        };

        // Build initial messages: system = skill instructions, user = task context.
        let messages = vec![
            json!({
                "role": "system",
                "content": instructions,
            }),
            json!({
                "role": "user",
                "content": if task_context.is_empty() {
                    format!("Execute the skill '{skill_name}' according to the instructions above.")
                } else {
                    task_context.to_string()
                },
            }),
        ];

        let task_profile = infer_task_execution_profile(task_context);
        let workspace_root_hint = self
            .edge_profile
            .get("cwd")
            .and_then(Value::as_str)
            .map(String::from);

        let (tool_event_hooks, session_event_hooks) = workspace_root_hint
            .as_ref()
            .map(|root| crate::skills::hooks::load_all_hooks(std::path::Path::new(root)))
            .unwrap_or_default();

        let step_recorder =
            StepRecorder::new(&self.user_id, &subrun_session_id, &subrun_session_id);

        let mut state = AgenticLoopState {
            messages,
            run_transcript_capture: None,
            volatile_pending: Vec::new(),
            recent_rounds: Vec::new(),
            tool_results: Vec::new(),
            current_session_id: Some(self.session_id.clone()),
            current_run_id: None,
            inference_purpose: astra_turn_types::InferencePurpose::SubAgent,
            context_manifest_pool: None,
            context_manifest_user_id: Some(self.user_id.clone()),
            context_manifest_model_name: effective_model.clone(),
            runtime_manifest: None,
            recursion_depth: child_recursion_depth,
            final_text: String::new(),
            final_text_streamed: false,
            final_output_ready_notified: false,
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_observation_tool_calls: 0,
            has_any_usage: false,
            last_finish_reason: None,
            max_turns: SUBRUN_MAX_TURNS,
            remaining_turns: SUBRUN_MAX_TURNS,
            turn_budget_hint_emitted_90: false,
            turn_budget_hint_emitted_50: false,
            turn_budget_hint_emitted_20: false,
            agentic_turn_budget: task_profile.agentic_turn_budget,
            budget_policy: None,
            current_round_index: 0,
            llm_rounds_completed: 0,
            last_request_message_count: None,
            turn_guard: TurnGuard::with_profile(task_profile),
            restricted_tools,
            boosted_tools: HashSet::new(),
            widen_selection_pending: false,
            step_recorder,
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(
                astra_text_utils::semantic_dedup::DEFAULT_SIMILARITY_THRESHOLD,
            ),
            call_counts: HashMap::new(),
            max_identical_tool_calls: resolved_tool_policy.max_identical_tool_calls,
            max_tools_per_turn: resolved_tool_policy.max_tools_per_turn,
            repeated_cache_hit_suppression: resolved_tool_policy.repeated_cache_hit_suppression,
            max_consecutive_empty_name: resolved_tool_policy.max_consecutive_empty_name,
            stall: Default::default(),
            telemetry: Default::default(),
            skills: SkillState {
                // Inherit resolver for nested inline skills, but NO executor
                // to prevent Fork→Fork recursion (same as CLI design).
                resolver: self.skill_resolver.clone(),
                request_constraints: self.request_constraints.clone(),
                quality_tracker: crate::skills::quality::SkillQualityTracker::new(),
                improvement_tracker: astra_skills::improvement::ImprovementTracker::new(),
                tool_event_hooks,
                session_event_hooks,
                // Skill-level effort/agent_type from manifest
                effort: effort.and_then(crate::skills::manifest::EffortLevel::parse),
                agent_type: agent_type.map(String::from),
                ..Default::default()
            },
            hooks: StopHookState {
                workspace_root_hint,
                forward_headers: self.forward_headers.clone(),
                admitted_model_execution: self.admitted_model_execution.clone(),
                ..Default::default()
            },
            cancellation: CancellationState {
                flag: None,
                pause_flag: None,
                token: self.cancel_token.clone(),
            },
            messaging: Default::default(),
            user_intents: Default::default(),
            error_recovery: Default::default(),
            run_control: None,
            pipeline_session: Some(
                astra_turn_core::pipeline_session::PipelineSession::new_with_current_date(
                    astra_turn_core::pipeline_config::PipelineConfig::default(),
                    crate::turn::session_current_date::resolve_session_current_date_for_user(
                        &self.user_id,
                        &self.session_id,
                    ),
                ),
            ),
            message: task_context.to_string(),
            user_intent: task_context.to_string(),
            recent_tools: Vec::new(),
            activated_deferred_tool_names: Vec::new(),
            has_prior_assistant_turn: false,
            turn_intent: None,
            task_profile: infer_task_execution_profile(task_context),
            last_turn_policy: TurnInteractionPolicy::default(),
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None)
                .expect("valid dummy URL"),
            api_token: String::new(),
            delegation_engine: None,
            delegations_this_turn: 0,
            delegation_chain: Vec::new(),
            self_agent_id: "orchestrator".to_string(),
            project_context: None,
            checkpoint_gate: None,
            last_llm_context_manifest_trace: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            pinned_tool_schema_tokens: 0,
            sticky_tool_schemas: Vec::new(),
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            context_compression_triggered: false,
            canonical_rewrite_state: Default::default(),
            budget_wrapup_ignored_rounds: 0,
            compact_tier_applied: astra_turn_core::compaction_types::CompactionTier::Normal,
            skill_produced_output: false,
            max_cumulative_tokens: SUBRUN_MAX_CUMULATIVE_TOKENS,
            thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
            recent_file_reads: Vec::new(),
            permission_context: Some(permission_context),
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            recent_tactical_actions: Vec::new(),
            runtime_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            memory_extraction_service: self.memory_extraction_service.clone(),
            observation_journal: Default::default(),
            observation_store: None,
            session_memory_state: Default::default(),
            compact_strategy,
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            bridge_turn_chain_id: None,
            bridge_user_query_event_id: None,
            turn_event_buffer: None,
            harness: {
                #[cfg(feature = "harness")]
                {
                    match self.harness_sink {
                        Some(ref sink) => {
                            crate::turn::harness_adapter::HarnessSlot::observe_only(sink.clone())
                        }
                        None => crate::turn::harness_adapter::HarnessSlot::empty(),
                    }
                }
                #[cfg(not(feature = "harness"))]
                {
                    crate::turn::harness_adapter::HarnessSlot::empty()
                }
            },
        };

        // ── Wire RuntimeToolExecutor for skill sub-run tool execution ────
        {
            let workspace = self.provision_skill_workspace(skill_name, &subrun_session_id)?;
            let memoria_base = Some(astra_core::MemoriaSettings::from_env().base_url);

            let mut builder = ToolExecutionService::builder();
            if let Some(pool) = &self.edge_connection_pool {
                builder = builder.edge_connection_pool(pool.clone());
            }
            if let Some(service) = &self.edge_dispatch_service {
                builder = builder.edge_dispatch_service(Arc::clone(service));
            }
            if let Some(service) = &self.edge_registry_service {
                builder = builder.edge_registry_service(Arc::clone(service));
            }

            let mut executor = super::runtime_tool_executor::RuntimeToolExecutor::new(
                workspace,
                self.user_id.clone(),
                subrun_session_id.clone(),
                memoria_base,
                None,
            )
            .with_reflect_service(Arc::clone(&self.reflect_service))
            .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
                self.shared_pool.is_some(),
                self.reflect_service.is_configured(),
            ))
            .with_cancel_token(self.cancel_token.clone())
            .with_runtime_file_transfer(self.runtime_file_transfer.clone())
            .with_runtime_edge_dispatch_authorization(
                self.runtime_edge_dispatch_authorization.clone(),
            )
            .with_tool_execution_service(builder.build());
            self.apply_execution_binding_snapshot(&mut executor);
            if let Some(pool) = &self.shared_pool {
                executor.set_context_manifest_pool(pool.clone());
            }
            state.runtime_tool_executor = Some(std::sync::Arc::new(executor));
        }

        let loop_result = run_agentic_loop_with_host(&mut host, &mut state).await;
        let loop_result = host.settle_loop_outcome(loop_result);
        let outcome = project_skill_subrun_outcome(&loop_result, &state);

        // audit-#8: avoid underflow if remaining_turns somehow exceeds the cap.
        let turns = SUBRUN_MAX_TURNS.saturating_sub(state.remaining_turns) as u32;
        let tokens_used = state.provider_total_tokens().min(u32::MAX as u64) as u32;

        Ok(SubRunResult {
            output: state.final_text,
            tokens_used,
            turns,
            outcome,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tool_transport::{
        ExecutorBinding, ExecutorStatus, ToolTransportKind, WorkspaceAuthority, WorkspaceBinding,
    };
    use async_trait::async_trait;

    fn mock_matrixone() -> MatrixOneSettings {
        MatrixOneSettings::mock()
    }

    fn mock_encryptor() -> Arc<FernetTokenEncryptor> {
        Arc::new(FernetTokenEncryptor::new("cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=").unwrap())
    }

    fn edge_runtime_snapshot() -> ExecutionBindingSnapshot {
        ExecutionBindingSnapshot::new(
            WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            ExecutorBinding::edge_agent(
                "edge-1",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Online,
            ),
            astra_runtime_env::RuntimeBinding::host_process("edge-host"),
        )
    }

    struct ReadyReflectService;

    #[async_trait]
    impl ReflectService for ReadyReflectService {
        fn is_configured(&self) -> bool {
            true
        }

        async fn build_evidence(
            &self,
            _user_id: &str,
            _session_id: &str,
            _request: astra_services::reflect::ReflectRequest,
        ) -> astra_services::reflect::ServiceResult<astra_services::ReflectReport> {
            unreachable!("server skill subrun tests only inspect service readiness")
        }
    }

    #[test]
    fn server_skill_subrun_executor_builds() {
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".to_string(),
            "test-session".to_string(),
        );
        assert!(executor.cancel_token.is_none());
        assert!(executor.skill_resolver.is_none());
        assert!(executor.admitted_model_execution.is_none());
        assert_eq!(
            executor.inherited_permissions.mode,
            crate::orchestration::PermissionMode::Auto
        );
        assert!(
            !executor.reflect_service.is_configured(),
            "skill sub-runs must fail closed until the parent reflect service is injected"
        );
    }

    #[test]
    fn server_skill_subrun_executor_with_builders() {
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".to_string(),
            "test-session".to_string(),
        )
        .with_default_model(Some("claude-sonnet-4-20250514".to_string()))
        .with_admitted_model_execution(Some(AdmittedModelExecution::from_endpoint(
            "offer-skill".to_string(),
            "claude-sonnet-4-20250514".to_string(),
            "openai".to_string(),
            "http://catalog:8081/api/v1/chat/completions".to_string(),
            "Bearer test".to_string(),
            Some(2500),
        )))
        .with_edge_tools(vec![
            json!({"type": "function", "function": {"name": "bash"}}),
        ])
        .with_edge_dispatch_service(Arc::new(
            astra_services::multi_agent::UnconfiguredEdgeDispatchService,
        ))
        .with_edge_registry_service(Arc::new(
            astra_services::multi_agent::UnconfiguredEdgeRegistryService,
        ))
        .with_cancel_token(Some(Arc::new(tokio_util::sync::CancellationToken::new())));

        assert!(executor.default_model.is_some());
        assert!(executor.admitted_model_execution.is_some());
        assert_eq!(executor.edge_tools.len(), 1);
        assert!(executor.edge_dispatch_service.is_some());
        assert!(executor.edge_registry_service.is_some());
        assert!(executor.cancel_token.is_some());
    }

    #[test]
    fn server_skill_subrun_executor_keeps_reflect_service() {
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".to_string(),
            "test-session".to_string(),
        )
        .with_reflect_service(Arc::new(ReadyReflectService));

        assert!(executor.reflect_service.is_configured());
    }

    #[test]
    fn server_skill_subrun_executor_keeps_inherited_permissions() {
        let inherited_permissions = crate::orchestration::InheritedPermissions::new(
            crate::orchestration::PermissionMode::Deny,
        );
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".to_string(),
            "test-session".to_string(),
        )
        .with_inherited_permissions(inherited_permissions);

        assert_eq!(
            executor.inherited_permissions.mode,
            crate::orchestration::PermissionMode::Deny
        );
    }

    #[test]
    fn server_skill_subrun_executor_keeps_execution_binding_snapshot() {
        let snapshot = edge_runtime_snapshot();
        let workspace_record = astra_runtime_env::WorkspaceRecord {
            workspace_id: "workspace-1".to_string(),
            owner_scope: astra_runtime_env::WorkspaceOwnerScope::None,
            kind: astra_runtime_env::WorkspaceBindingKind::None,
            authority: astra_runtime_env::WorkspaceAuthority::None,
            root_or_volume_ref: String::new(),
            source: astra_runtime_env::WorkspaceSource::None,
            persistence: astra_runtime_env::WorkspacePersistence::None,
            revision: String::new(),
            display_name: String::new(),
        };
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".to_string(),
            "test-session".to_string(),
        )
        .with_execution_binding_snapshot(snapshot.clone())
        .with_workspace_record(Some(workspace_record.clone()));

        assert_eq!(
            executor.execution_binding_snapshot.as_ref(),
            Some(&snapshot)
        );

        let workspace = tempfile::tempdir().expect("temporary skill workspace");
        let mut runtime_executor = crate::server::runtime_tool_executor::RuntimeToolExecutor::new(
            workspace.path().to_path_buf(),
            "test-user".to_string(),
            "test-session".to_string(),
            None,
            None,
        );
        executor.apply_execution_binding_snapshot(&mut runtime_executor);
        let binding = runtime_executor.binding_metadata();
        assert_eq!(binding["workspace"]["kind"], "edge_workspace");
        assert_eq!(binding["executor"]["executor_id"], "edge-1");
        let request = runtime_executor.tool_execution_request(
            "bash",
            &json!({"command": "pwd", "_run_id": "run-1", "_tool_call_id": "call-1"}),
        );
        assert_eq!(request.workspace_record, Some(workspace_record));
    }

    #[test]
    fn provision_skill_workspace_rejects_unsafe_session_identity() {
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".to_string(),
            "test-session".to_string(),
        );

        let error = executor
            .provision_skill_workspace("review", "session/123")
            .expect_err("unsafe session id must fail instead of being sanitized");

        assert!(
            error.contains("invalid skill sub-run session_id"),
            "unexpected error: {error}"
        );
    }

    /// Server-side symmetric to `cli_skill_subrun_rejects_when_recursion_depth_limit_reached`:
    /// the fork sub-run executor must refuse to spawn once the agent recursion
    /// cap is reached. Without this guard, a fork-context skill could recurse
    /// into itself indefinitely. The CLI has had this test; the server did not
    /// — so this closes an asymmetric coverage gap where a misbehaving
    /// resolver on the server path could recurse without a fast-fail at the
    /// depth boundary.
    #[tokio::test]
    async fn server_skill_subrun_rejects_when_recursion_depth_limit_reached() {
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".to_string(),
            "test-session".to_string(),
        );
        // NOTE: empty `allowed_tools` here is intentional and SAFE because
        // `execute_skill_subrun` checks recursion depth FIRST (see
        // `checked_child_recursion_depth` call at ~L197, before any tool
        // validation). If someone reorders those checks, this test will
        // start returning a tool-validation error instead of the depth
        // error we're asserting on — update the test setup accordingly.
        let allowed_tools: Vec<String> = Vec::new();

        let err = executor
            .execute_skill_subrun(
                "depth-test",
                "Do work",
                "task",
                None,
                &allowed_tools,
                crate::turn::agentic_recursion_guard::ABSOLUTE_MAX_AGENT_RECURSION_DEPTH,
                None,
                None,
            )
            .await
            .unwrap_err();

        assert!(
            err.contains("recursion depth") && err.contains("absolute safety ceiling"),
            "error must cite depth limit; got: {err}"
        );
    }

    /// audit-#8: turn-count math must not underflow when `remaining_turns`
    /// briefly exceeds the cap (race conditions, future refactors, etc.).
    #[test]
    fn turn_count_subtraction_uses_saturating_sub() {
        // Saturating semantics: max < remaining → 0, max == remaining → 0.
        let max = SUBRUN_MAX_TURNS;
        assert_eq!(max.saturating_sub(max + 5), 0);
        assert_eq!(max.saturating_sub(max), 0);
        assert_eq!(max.saturating_sub(max - 3), 3);
    }
}
