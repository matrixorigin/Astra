//! CLI implementation of SpawnAgentExecutor.
//!
//! Runs spawned agents using the same agentic loop infrastructure as delegation.

use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use astra_runtime::{
    orchestration::{
        InheritedPermissions, PermissionSummary, SpawnAgentExecutor, SpawnRunConfig,
        SpawnRunResult, spawn_completion_status_from_finish_reason,
    },
    pipeline::step_protocol::InMemoryIdempotencyCache,
    pipeline::step_recorder::StepRecorder,
    semantic_dedup::SemanticDedup,
    turn::agentic_loop::finalization::run_agentic_loop_with_host,
    turn::agentic_loop::host::{
        AgenticLoopOutcome, AgenticLoopState, CancellationState, MessagingState, SkillState,
        StopHookState, runtime_manifest_for_model,
    },
    turn::chat_turn_heuristics::infer_task_execution_profile,
    turn::turn_guard::TurnGuard,
};
use astra_turn_core::{
    agent_live_event::SharedAgentLiveEventSink, tool::schema::tool_names_from_schemas,
};
use serde_json::{Value, json};

use super::chat_stream::StreamEvent;
use super::skill_subrun::SubRunHost;
use crate::cli::cli_config::cli_utils::cli_user_id;
use crate::edge_tools;

// ─── CliSpawnAgentExecutor ──────────────────────────────────────────────────

/// Re-export from runtime so all CLI components share one type.
pub type TokenProvider = astra_runtime::capabilities::TokenProvider;

/// CLI implementation of [`SpawnAgentExecutor`].
///
/// Runs spawned agents using the same agentic loop as delegation,
/// but with agent-type-specific configuration (model, tools, prompts).
pub struct CliSpawnAgentExecutor {
    api: astra_thin_client::ThinClient,
    /// Token captured at construction. Used as the **fallback** when
    /// `token_provider` is unset OR returns `None`. In production the
    /// REPL installs a provider so sub-agent spawns always read the
    /// freshest token; this field stays as a safety net for tests and
    /// the one-shot `chat -m` path that doesn't have a profile to query.
    token: String,
    /// Reads the current access token at spawn time. When set, takes
    /// precedence over `self.token` so token refreshes done by the
    /// parent turn flow propagate to children. When `None`, the executor
    /// falls back to `self.token` for parity with the pre-fix behaviour.
    token_provider: Option<TokenProvider>,
    project_root: PathBuf,
    cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    skill_resolver: Option<Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    active_session_id: Option<String>,
    /// Optional sink for fork-cache telemetry. When `None` the
    /// executor still forwards `inherited_prefix` so child messages
    /// prepend the parent prefix — but no ForkCacheEvent is emitted.
    /// Zero-cost when unset.
    fork_cache_sink: Option<Arc<dyn astra_turn_core::fork_cache_event::ForkCacheEventSink>>,
    /// Parent session journal writer for unified timeline.
    journal: Option<std::sync::Arc<astra_services::session_journal::JournalWriter>>,
    /// Shared command queue for the parent's BackgroundTaskRegistry.
    /// Threaded into the child's ToolExecutor so spawned sub-agents
    /// can inspect or stop background shell tasks promoted in the TUI.
    bg_task_commands: Option<Arc<std::sync::Mutex<Vec<crate::edge_tools::BgTaskCommand>>>>,
    /// Threaded from `SessionState.bg_task_list_cache` so spawned
    /// sub-agents can read the latest task-list snapshot directly.
    bg_task_list_cache: Option<std::sync::Arc<tokio::sync::RwLock<String>>>,
    /// Session/default model fallback when the spawn request itself omits one.
    default_model: Option<String>,
}

/// Build the child agent's message array from system prompt, optional
/// inherited prefix, and the child task. Ensures role alternation is
/// valid for providers that require strict user/assistant alternation
/// (e.g. Bedrock Converse).
///
/// **Fork mode** (`prefix_messages` is `Some`): The prefix already
/// contains the parent's system message at [0] plus its full
/// conversation history — reconstructed byte-for-byte from the
/// captured `ForkPrefix.canonical_prefix_bytes`. We do NOT prepend a
/// fresh system message because that would:
/// - Create a duplicate system message (providers only expect one)
/// - Break byte-for-byte prefix cache reuse (the extra bytes shift
///   the cache key so the parent's cached KV is unusable)
///
/// The child's identity ("You are agent_id, specialized sub-agent…")
/// is communicated via the child_task user message, not via a system
/// block, so the fork child still knows its role.
///
/// **Fresh mode** (`prefix_messages` is `None`): the child gets a
/// system message with its identity, then the child task as a user
/// message — same as before fork support.
pub(crate) fn build_child_messages(
    system_prompt: &str,
    prefix_messages: Option<&[Value]>,
    child_task: &str,
    force_reasoning_field: bool,
) -> Vec<Value> {
    fn ensure_assistant_reasoning_fields(messages: &mut [Value]) {
        for msg in messages {
            if msg.get("role").and_then(Value::as_str) == Some("assistant")
                && msg.get("reasoning_content").is_none()
            {
                msg["reasoning_content"] = Value::String(String::new());
            }
        }
    }

    if let Some(prefix) = prefix_messages {
        // Fork mode: reuse parent prefix verbatim for cache alignment.
        let mut messages = Vec::with_capacity(prefix.len() + 2);
        messages.extend(prefix.iter().cloned());
        if force_reasoning_field {
            ensure_assistant_reasoning_fields(&mut messages);
        }
        // Bedrock Converse requires strict role alternation. If the
        // prefix ends with user or tool role, inserting the child task
        // (also user) would create consecutive user messages → HTTP 400.
        // Insert a synthetic assistant bridge to maintain alternation.
        let last_role = messages
            .iter()
            .rev()
            .find_map(|m| m.get("role").and_then(|r| r.as_str()))
            .filter(|r| *r != "system");
        if matches!(last_role, Some("user") | Some("tool")) {
            let mut bridge = json!({
                "role": "assistant",
                "content": "I'll now work on the delegated task."
            });
            if force_reasoning_field {
                bridge["reasoning_content"] = Value::String(String::new());
            }
            messages.push(bridge);
        }
        messages.push(json!({ "role": "user", "content": child_task }));
        messages
    } else {
        // Fresh mode: system prompt + child task only.
        vec![
            json!({ "role": "system", "content": system_prompt }),
            json!({ "role": "user", "content": child_task }),
        ]
    }
}

#[derive(Clone)]
struct AgentLiveStreamEventSink {
    agent_id: String,
    sink: SharedAgentLiveEventSink,
}

impl std::fmt::Debug for AgentLiveStreamEventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLiveStreamEventSink")
            .field("agent_id", &self.agent_id)
            .finish_non_exhaustive()
    }
}

impl super::chat_stream::StreamEventSink for AgentLiveStreamEventSink {
    fn send(&self, event: StreamEvent) {
        if let Some(kind) = stream_event_to_agent_live_kind(event)
            && let Err(err) = self
                .sink
                .send(astra_turn_core::agent_live_event::AgentLiveEvent {
                    agent_id: self.agent_id.clone(),
                    kind,
                })
        {
            astra_core::agent_warn!(
                "spawn_subrun",
                "dropping live event for {}: {err:?}",
                self.agent_id
            );
        }
    }
}

fn agent_live_stream_event_sink(
    agent_id: String,
    sink: Option<SharedAgentLiveEventSink>,
) -> Option<super::chat_stream::SharedStreamEventSink> {
    Some(Arc::new(AgentLiveStreamEventSink {
        agent_id,
        sink: sink?,
    }))
}

fn emit_agent_terminated(
    sink: Option<&SharedAgentLiveEventSink>,
    agent_id: &str,
    started_at: std::time::Instant,
    termination: astra_turn_core::agent_live_event::AgentLiveTermination,
    reason: Option<String>,
) {
    use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};
    let Some(sink) = sink else {
        return;
    };
    if let Err(err) = sink.send(AgentLiveEvent {
        agent_id: agent_id.to_string(),
        kind: AgentLiveEventKind::AgentTerminated {
            termination,
            duration_ms: started_at.elapsed().as_millis() as u64,
            reason,
        },
    }) {
        astra_core::agent_warn!(
            "spawn_subrun",
            "failed to emit terminal live event for {agent_id}: {err:?}"
        );
    }
}

fn stream_event_to_agent_live_kind(
    event: StreamEvent,
) -> Option<astra_turn_core::agent_live_event::AgentLiveEventKind> {
    use astra_turn_core::agent_live_event::AgentLiveEventKind;
    match event {
        StreamEvent::Token(text) => Some(AgentLiveEventKind::OutputDelta(text)),
        StreamEvent::ThinkingChunk(text) => Some(AgentLiveEventKind::ThinkingDelta(text)),
        StreamEvent::ToolStarted {
            name,
            description,
            tool_use_id,
            ..
        } => Some(AgentLiveEventKind::ToolStarted {
            name,
            description,
            tool_use_id,
        }),
        StreamEvent::ToolCompleted {
            name,
            description,
            status,
            duration_ms,
            output_summary,
            output,
            tool_use_id,
            ..
        } => Some(AgentLiveEventKind::ToolCompleted {
            name,
            description,
            status,
            duration_ms,
            output_summary,
            output,
            tool_use_id,
        }),
        StreamEvent::WaitingForModel => {
            Some(AgentLiveEventKind::Status("waiting for model".to_string()))
        }
        StreamEvent::ModelResponding => {
            Some(AgentLiveEventKind::Status("model responding".to_string()))
        }
        StreamEvent::AskUserPrompted { prompt, .. } => Some(AgentLiveEventKind::Status(format!(
            "ask_user waiting ({} questions)",
            prompt
                .get("prompt")
                .and_then(|value| value.get("question_count"))
                .and_then(|value| value.as_u64())
                .unwrap_or(0)
        ))),
        StreamEvent::AskUserResolved { resolution, .. } => {
            Some(AgentLiveEventKind::Status(format!(
                "ask_user {}",
                resolution
                    .get("audit")
                    .and_then(|value| value.get("response"))
                    .and_then(|value| value.get("outcome"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("resolved")
            )))
        }
        StreamEvent::StatusLine(text) => Some(AgentLiveEventKind::Status(text)),
        StreamEvent::PermissionAutoApproved { tool, reason } => Some(AgentLiveEventKind::Status(
            astra_turn_core::permission::notice::format_auto_approved_permission(&tool, &reason)
                .trim()
                .to_string(),
        )),
        StreamEvent::AgentControlStarted { label, .. } => Some(AgentLiveEventKind::Status(
            format!("agent control started: {label}"),
        )),
        StreamEvent::AgentControlCompleted { label, status, .. } => Some(
            AgentLiveEventKind::Status(format!("agent control {status}: {label}")),
        ),
        StreamEvent::ToolOutput { name, lines, bytes } => Some(AgentLiveEventKind::Status(
            format!("{name} streaming: {lines} lines, {bytes} bytes"),
        )),
        StreamEvent::Thinking(_)
        | StreamEvent::AgentLive(_)
        | StreamEvent::Compaction(_)
        | StreamEvent::ExplainReport(_)
        | StreamEvent::ExplainText(_)
        | StreamEvent::VerdictReport(_) => None,
    }
}

impl CliSpawnAgentExecutor {
    pub fn new(
        api: astra_thin_client::ThinClient,
        token: String,
        project_root: PathBuf,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Self {
        Self {
            api,
            token,
            token_provider: None,
            project_root,
            cancel_token,
            skill_resolver: None,
            active_session_id: None,
            fork_cache_sink: None,
            journal: None,
            bg_task_commands: None,
            bg_task_list_cache: None,
            default_model: None,
        }
    }

    pub fn with_default_model(mut self, model: Option<String>) -> Self {
        self.default_model = model;
        self
    }

    /// Install a token provider so each spawn reads the freshest
    /// access token at the moment of execution. The REPL wires this
    /// to `current_access_token(profile)` so token refreshes done by
    /// the parent agent's 401-retry path propagate to sub-agents.
    /// Without this, long-running sessions hit "Could not validate
    /// credentials" on every spawn after the first token rotation.
    pub fn with_token_provider(mut self, provider: TokenProvider) -> Self {
        self.token_provider = Some(provider);
        self
    }

    /// Resolve the access token for the next spawn. Provider takes
    /// precedence so refreshes propagate; falls back to the captured
    /// `self.token` when the provider is absent or returns `None`.
    fn resolve_token(&self) -> String {
        if let Some(provider) = &self.token_provider {
            if let Some(t) = provider() {
                return t;
            }
        }
        self.token.clone()
    }

    fn resolve_effective_model(&self, config_model: Option<&str>) -> Option<String> {
        config_model
            .map(ToOwned::to_owned)
            .or_else(|| self.default_model.clone())
    }

    async fn resolve_token_async(&self) -> Result<String, String> {
        let fallback = self.token.clone();
        let Some(provider) = self.token_provider.clone() else {
            return Ok(fallback);
        };
        tokio::task::spawn_blocking(move || provider().unwrap_or(fallback.clone()))
            .await
            .map_err(|err| format!("token provider task failed: {err}"))
    }

    /// Install the parent's bg task command queue so spawned children
    /// can inspect or stop background shell tasks.
    pub fn with_bg_task_commands(
        mut self,
        commands: Arc<std::sync::Mutex<Vec<crate::edge_tools::BgTaskCommand>>>,
    ) -> Self {
        self.bg_task_commands = Some(commands);
        self
    }

    /// Install the parent's bg task list cache so spawned children
    /// can read the latest task-list snapshot directly.
    pub fn with_bg_task_list_cache(
        mut self,
        cache: std::sync::Arc<tokio::sync::RwLock<String>>,
    ) -> Self {
        self.bg_task_list_cache = Some(cache);
        self
    }

    /// Install the parent session's journal writer for unified timeline.
    pub fn with_journal(
        mut self,
        journal: std::sync::Arc<astra_services::session_journal::JournalWriter>,
    ) -> Self {
        self.journal = Some(journal);
        self
    }

    /// Install a fork-cache event sink. When present, every child
    /// spawn that inherited a parent prefix emits exactly one
    /// `ForkCacheEvent` on its first ingested turn.
    pub fn with_fork_cache_sink(
        mut self,
        sink: Arc<dyn astra_turn_core::fork_cache_event::ForkCacheEventSink>,
    ) -> Self {
        self.fork_cache_sink = Some(sink);
        self
    }

    pub fn with_skill_resolver(
        mut self,
        resolver: Option<Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    ) -> Self {
        self.skill_resolver = resolver;
        self
    }

    pub fn with_active_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.active_session_id = Some(session_id.into());
        self
    }
}

#[async_trait]
impl SpawnAgentExecutor for CliSpawnAgentExecutor {
    async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
        let all_schemas = edge_tools::all_tool_schemas();
        let valid_tool_names = tool_names_from_schemas(&all_schemas);

        // Hold a clone for emitting the terminal `AgentTerminated`
        // event after the agentic loop returns. Without this, a
        // crashed / timed-out / cancelled sub-agent would leave its
        // multi_agent strip row stuck in the `live` state forever
        // (reviewer L2-5 — UX C1 + Arch M1).
        let live_event_sink_for_terminal = config.live_event_sink.clone();
        let agent_id_for_terminal = config.agent_id.clone();
        let started_at = std::time::Instant::now();

        let inherited_permissions: InheritedPermissions = config.inherited_permissions.clone();
        let perm_manager = super::permission_manager::PermissionManager::with_inherited(
            &self.project_root,
            inherited_permissions,
        );

        // Use the working directory from config (may be a worktree)
        let effective_root = config.working_dir.clone();
        let compact_strategy = config
            .model
            .as_deref()
            .map(astra_turn_core::microcompact::CompactStrategy::from_provider_hint)
            .unwrap_or_default();

        // Resolve the freshest token at spawn time. Without this,
        // sub-agents fail with 401 in long-running sessions after the
        // parent's auth refresh rotates the token (session 82ff91e5).
        let token = match self.resolve_token_async().await {
            Ok(token) => token,
            Err(err) => {
                emit_agent_terminated(
                    live_event_sink_for_terminal.as_ref(),
                    &agent_id_for_terminal,
                    started_at,
                    astra_turn_core::agent_live_event::AgentLiveTermination::Failed,
                    Some(err.clone()),
                );
                return Err(err);
            }
        };
        let effective_model = self.resolve_effective_model(config.model.as_deref());

        let mut executor = edge_tools::ToolExecutor::new(&effective_root)
            .with_cloud(self.api.api_origin(), &token);
        if let Some(ref cmds) = self.bg_task_commands {
            executor = executor.with_bg_task_commands(cmds.clone());
        }
        if let Some(ref cache) = self.bg_task_list_cache {
            executor = executor.with_bg_task_list_cache(cache.clone());
        }
        if let Some(session_id) = self.active_session_id.as_deref() {
            executor.set_active_session_id(session_id.to_string());
        }

        // Resolve per-model workflow-guard policy once; used for both the
        // `SubRunHost::tool_cache` and the `AgenticLoopState` below.
        let resolved_tool_policy = astra_config::runtime_config::RuntimeConfig::load()
            .tool_policy
            .resolve_for_model(effective_model.as_deref());

        let mut host = SubRunHost {
            api: self.api.clone(),
            token: token.clone(),
            model: effective_model.clone(),
            project_root: effective_root.clone(),
            executor: std::sync::Arc::new(executor),
            all_schemas,
            valid_tool_names: valid_tool_names.clone(),
            perm_manager,
            max_completion_tokens: None,
            effort: None,
            agent_type: Some(config.agent_type.clone()),
            cancel_token: self.cancel_token.clone(),
            skill_resolver: self.skill_resolver.clone(),
            progress_tx: None,
            agent_id: config.agent_id.clone(),
            stream_event_tx: None,
            stream_event_sink: agent_live_stream_event_sink(
                config.agent_id.clone(),
                config.live_event_sink.clone(),
            ),
            tool_cache: crate::cli::stream::stream_render::EdgeToolCache::new(
                resolved_tool_policy.max_identical_tool_calls,
            ),
            inherited_prefix: config.inherited_prefix.clone(),
            fork_cache_sink: self.fork_cache_sink.clone(),
            fork_cache_probe_state: astra_runtime::orchestration::ForkCacheProbeState::new(),
            journal: self.journal.clone(),
        };

        // Build system message from agent type definition
        let system_prompt = if config.system_prompt_addendum.is_empty() {
            format!(
                "You are '{}', a specialized sub-agent. Complete the task thoroughly.",
                config.agent_id
            )
        } else {
            format!(
                "You are '{}', a specialized sub-agent.\n\n{}\n\nComplete the task thoroughly.",
                config.agent_id, config.system_prompt_addendum
            )
        };

        // PR 5.6: if the spawner resolved a parent prefix, prepend
        // the captured prefix messages between the system prompt
        // and the child's own task. System prompt stays at [0] so
        // it reads as the child's own identity; inherited messages
        // sit behind it as "historical context from parent" that
        // the provider can hit in its prompt cache. The child task
        // at the tail is always fresh (cacheable only for future
        // child turns, not for this first call).
        let force_reasoning_field = config.inherited_prefix.as_ref().is_some_and(|ip| {
            ip.thinking
                .as_ref()
                .is_some_and(|thinking| thinking.enabled)
                || astra_turn_core::edge_ledger::history_has_reasoning(&ip.prefix_messages)
        }) || effective_model.as_deref().is_some_and(|model| {
            astra_turn_core::reasoning_capabilities::reasoning_capabilities("", model)
                .requires_replay()
        });

        let messages = build_child_messages(
            &system_prompt,
            config
                .inherited_prefix
                .as_ref()
                .map(|ip| ip.prefix_messages.as_slice()),
            &config.task,
            force_reasoning_field,
        );

        // Build restricted tools based on agent type's allowed_tools
        let restricted_tools: HashSet<String> = if config.allowed_tools.iter().any(|t| t == "*") {
            // All tools allowed
            HashSet::new()
        } else {
            // Only allow specified tools
            let allowed: HashSet<&str> = config.allowed_tools.iter().map(|s| s.as_str()).collect();
            valid_tool_names
                .iter()
                .filter(|name| !allowed.contains(name.as_str()))
                .cloned()
                .collect()
        };

        // Add edit/create to restricted if read_only
        let restricted_tools = if config.read_only {
            let mut restricted = restricted_tools;
            restricted.insert("edit".to_string());
            restricted.insert("create".to_string());
            restricted.insert("write_file".to_string());
            restricted.insert("str_replace".to_string());
            restricted
        } else {
            restricted_tools
        };

        let task_profile = infer_task_execution_profile(&config.task);
        // Local step-recorder session id: kept synthetic (`spawn-...`)
        // because it's only used for local journal / step file
        // persistence — server never sees this.
        let local_subrun_session_id = format!("spawn-{}-{}", config.run_id, config.agent_id);
        let user_id = cli_user_id();
        let step_recorder = StepRecorder::with_persistence(
            &user_id,
            &local_subrun_session_id,
            &format!("{}-run", config.run_id),
        );

        // Wire session for the *server-facing* `chat_turn_base_payload`:
        // pass None so the server opens a fresh session for this child
        // turn rather than rejecting a synthetic `spawn-...` id with
        // "Session not found" — discovered during real-world MiniMax
        // spawn_agent verification. Reusing the parent's active
        // session id would risk cross-contamination when multiple
        // children share one parent, and cross-child race conditions
        // on per-session state.
        //
        // Local continuity (transcript, step recorder, tool journal)
        // still uses `local_subrun_session_id` which is client-side
        // only, so children remain traceable offline.
        let server_session_id: Option<String> = None;

        let start_time = std::time::Instant::now();
        let progress_emitter = config.progress_emitter.clone();
        let has_parent_permissions = config.parent_address.is_some();

        let max_turns = config.max_turns as usize;

        let child_thinking = effective_model
            .as_deref()
            .map(|model| astra_turn_core::thinking_config::resolve_model_thinking(model).1)
            .unwrap_or_default();
        let runtime_manifest = runtime_manifest_for_model(
            "cli_spawn_subrun",
            "cli_spawn_subrun",
            effective_model.as_deref(),
        );

        let mut state = AgenticLoopState {
            observation_store: None,
            observation_journal: Default::default(),
            messages,
            volatile_pending: Vec::new(),
            recent_rounds: Vec::new(),
            tool_results: Vec::new(),
            session_memory_state: Default::default(),
            session_memory_llm_params: None,
            current_session_id: server_session_id,
            current_run_id: Some(config.run_id.clone()),
            context_manifest_pool: None,
            context_manifest_user_id: Some(user_id),
            context_manifest_model_name: effective_model,
            runtime_manifest,
            recursion_depth: config.recursion_depth,
            final_text: String::new(),
            final_text_streamed: false,
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            textless_stop_retries: 0,
            last_finish_reason: None,
            total_evidence_tool_calls: 0,
            has_any_usage: false,
            max_turns,
            remaining_turns: max_turns,
            turn_budget_hint_emitted_90: false,
            turn_budget_hint_emitted_50: false,
            turn_budget_hint_emitted_20: false,
            agentic_turn_budget: task_profile.agentic_turn_budget,
            current_round_index: 0,
            llm_rounds_completed: 0,
            last_request_message_count: None,
            turn_guard: TurnGuard::with_profile(task_profile),
            budget_policy: None,
            policy_expanded_this_turn: false,
            restricted_tools,
            boosted_tools: HashSet::new(),
            widen_selection_pending: false,
            step_recorder,
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(
                astra_runtime::semantic_dedup::DEFAULT_SIMILARITY_THRESHOLD,
            ),
            call_counts: HashMap::new(),
            max_identical_tool_calls: resolved_tool_policy.max_identical_tool_calls,
            max_tools_per_turn: resolved_tool_policy.max_tools_per_turn,
            repeated_cache_hit_suppression: resolved_tool_policy.repeated_cache_hit_suppression,
            max_consecutive_empty_name: resolved_tool_policy.max_consecutive_empty_name,
            stall: Default::default(),
            telemetry: Default::default(),
            skills: SkillState {
                resolver: self.skill_resolver.clone(),
                quality_tracker: astra_skills::quality::SkillQualityTracker::new(),
                improvement_tracker: astra_skills::improvement::ImprovementTracker::new(),
                tool_event_hooks: astra_skills::hooks::load_tool_event_hooks(&effective_root),
                session_event_hooks: astra_skills::hooks::load_session_event_hooks(&effective_root),
                ..Default::default()
            },
            hooks: StopHookState {
                workspace_root_hint: Some(effective_root.to_string_lossy().into_owned()),
                ..Default::default()
            },
            messaging: MessagingState {
                mailbox: config.mailbox,
                progress_emitter: config.progress_emitter,
                ..Default::default()
            },
            deferred_input: Default::default(),
            cancellation: CancellationState {
                flag: None,
                pause_flag: None,
                token: self.cancel_token.clone(),
            },
            error_recovery: Default::default(),
            run_control: None,
            pipeline_session: Some(
                astra_turn_core::pipeline_session::PipelineSession::new_with_current_date(
                    astra_turn_core::pipeline_config::PipelineConfig::default(),
                    astra_runtime::turn::session_current_date::resolve_session_current_date(
                        self.active_session_id.as_deref().unwrap_or(""),
                    ),
                ),
            ),
            message: config.task.clone(),
            recent_tools: Vec::new(),
            has_prior_assistant_turn: false,
            task_profile,
            last_turn_policy:
                astra_runtime::turn::agentic_loop::host::TurnInteractionPolicy::default(),
            api: self.api.clone(),
            api_token: token.clone(),
            delegation_engine: None,
            delegations_this_turn: 0,
            delegation_chain: Vec::new(),
            self_agent_id: "spawn_subrun".to_string(),
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
            budget_wrapup_ignored_rounds: 0,
            compact_tier_applied: astra_turn_core::compaction_types::CompactionTier::Normal,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking: child_thinking,
            recent_file_reads: Vec::new(),
            permission_context: Some(config.permission_context),
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            recent_tactical_actions: Vec::new(),
            server_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            memory_extraction_service: None,
            compact_strategy,
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            bridge_turn_chain_id: None,
            bridge_user_query_event_id: None,
            turn_event_buffer: None,
            harness: astra_runtime::turn::harness_adapter::HarnessSlot::empty(),
        };

        // Inherit skills from parent: pre-populate discovered skills
        if !config.inherited_skills.is_empty() {
            for skill_name in &config.inherited_skills {
                state.skills.discovered.insert(skill_name.clone());
            }
        }

        let loop_result = run_agentic_loop_with_host(&mut host, &mut state).await;

        let tool_calls = state.total_tool_calls as u32;
        let agent_id = config.agent_id.clone();
        let run_id = config.run_id;
        let prompt_tokens = state.total_prompt;
        let completion_tokens = state.total_completion;
        let duration_ms = start_time.elapsed().as_millis() as u64;
        let ctx = match state.permission_context.as_ref() {
            Some(ctx) => ctx,
            None => {
                // Fail with an actionable error rather than panicking the
                // whole process. A missing permission_context means the
                // spawn path didn't wire runtime permissions into the child
                // agent state — surface it so the caller can fix the config
                // instead of crashing the CLI.
                return Err(format!(
                    "spawned agent {agent_id} (run_id={run_id}) completed without a runtime permission_context; \
                     the spawn path must install `permission_context` into SpawnRunConfig before delegating to the agentic loop"
                ));
            }
        };
        let ctx_guard = ctx.read().await;
        let telemetry = ctx_guard.telemetry();
        let mode = match ctx_guard.mode() {
            astra_runtime::orchestration::PermissionMode::Auto => "auto".to_string(),
            astra_runtime::orchestration::PermissionMode::Plan => "plan".to_string(),
            astra_runtime::orchestration::PermissionMode::AcceptEdits => "accept_edits".to_string(),
            astra_runtime::orchestration::PermissionMode::Prompt => "prompt".to_string(),
            astra_runtime::orchestration::PermissionMode::Deny => "deny".to_string(),
        };
        let permission_summary = Some(PermissionSummary {
            mode,
            allow_rules: ctx_guard.effective_allow_rule_count(),
            deny_rules: ctx_guard.effective_deny_rule_count(),
            has_parent: has_parent_permissions,
            recent_denials: telemetry.recent_denials.clone(),
        });
        let permission_requests = telemetry.permission_requests;
        let permission_requests_approved = telemetry.permission_requests_approved;
        let tools_blocked = telemetry.tools_blocked;
        drop(ctx_guard);

        // Derive finish_reason from the structured interruption
        // record if present. This surfaces budget exhaustion /
        // token budget exceeded / context overflow as first-class
        // signals even when the legacy `status` remains
        // `"completed"` — which is how the loop reports all
        // resumable interruptions. Parents (and agent get_result)
        // can now switch on this field without regex-matching
        // the output.
        let finish_reason_from_state = state
            .interruption
            .as_ref()
            .map(|i| i.kind.label().to_string());

        // Helper: tell any live event sink that the sub-agent has
        // reached a terminal state. The TUI's `agent_runs` registry
        // is updated via this signal so the strip row flips from ◦
        // (live) to ✓ / ✗ / ⊘ (terminal). Without this, a crashed or
        // timed-out child leaves the row stuck in `live`.
        let emit_terminated = |termination, reason: Option<String>| {
            emit_agent_terminated(
                live_event_sink_for_terminal.as_ref(),
                &agent_id_for_terminal,
                started_at,
                termination,
                reason,
            );
        };

        match loop_result {
            Ok(AgenticLoopOutcome::Completed) => {
                // Emit completed event
                if let Some(ref emitter) = progress_emitter {
                    let summary = if state.final_text.len() > 100 {
                        format!(
                            "{}...",
                            state.final_text.chars().take(100).collect::<String>()
                        )
                    } else {
                        state.final_text.clone()
                    };
                    emitter.completed(
                        summary,
                        tool_calls,
                        (prompt_tokens, completion_tokens),
                        duration_ms,
                    );
                }
                emit_terminated(
                    astra_turn_core::agent_live_event::AgentLiveTermination::Completed,
                    finish_reason_from_state.clone(),
                );
                Ok(SpawnRunResult {
                    agent_id,
                    run_id,
                    status: spawn_completion_status_from_finish_reason(
                        finish_reason_from_state.as_deref(),
                    )
                    .to_string(),
                    finish_reason: finish_reason_from_state.unwrap_or_else(|| "normal".to_string()),
                    cancelled_by_user: None,
                    output: Some(state.final_text),
                    error: None,
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                    permission_summary,
                    permission_requests,
                    permission_requests_approved,
                    tools_blocked,
                })
            }
            Ok(AgenticLoopOutcome::Cancelled) => {
                // Emit cancelled event
                if let Some(ref emitter) = progress_emitter {
                    emitter.cancelled("user cancellation");
                }
                emit_terminated(
                    astra_turn_core::agent_live_event::AgentLiveTermination::Cancelled,
                    Some(
                        finish_reason_from_state
                            .clone()
                            .unwrap_or_else(|| "user cancellation".to_string()),
                    ),
                );
                Ok(SpawnRunResult {
                    agent_id,
                    run_id,
                    status: "cancelled".to_string(),
                    finish_reason: finish_reason_from_state
                        .unwrap_or_else(|| "cancelled".to_string()),
                    cancelled_by_user: Some(true),
                    output: if state.final_text.is_empty() {
                        None
                    } else {
                        Some(state.final_text)
                    },
                    error: None,
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                    permission_summary,
                    permission_requests,
                    permission_requests_approved,
                    tools_blocked,
                })
            }
            Ok(AgenticLoopOutcome::Error(error)) => {
                // Emit failed event
                if let Some(ref emitter) = progress_emitter {
                    emitter.failed(&error);
                }
                emit_terminated(
                    astra_turn_core::agent_live_event::AgentLiveTermination::Failed,
                    Some(error.clone()),
                );
                Ok(SpawnRunResult {
                    agent_id,
                    run_id,
                    status: "failed".to_string(),
                    finish_reason: finish_reason_from_state.unwrap_or_else(|| "failed".to_string()),
                    cancelled_by_user: None,
                    output: if state.final_text.is_empty() {
                        None
                    } else {
                        Some(state.final_text)
                    },
                    error: Some(error),
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                    permission_summary,
                    permission_requests,
                    permission_requests_approved,
                    tools_blocked,
                })
            }
            Ok(AgenticLoopOutcome::Waiting(reason)) => {
                // Emit idle event
                if let Some(ref emitter) = progress_emitter {
                    emitter.idle();
                }
                Ok(SpawnRunResult {
                    agent_id,
                    run_id,
                    status: "waiting".to_string(),
                    finish_reason: finish_reason_from_state
                        .unwrap_or_else(|| "waiting".to_string()),
                    cancelled_by_user: None,
                    output: Some(reason),
                    error: None,
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                    permission_summary,
                    permission_requests,
                    permission_requests_approved,
                    tools_blocked,
                })
            }
            Err(e) => {
                let msg = e.to_string();
                if let Some(ref emitter) = progress_emitter {
                    emitter.failed(&msg);
                }
                emit_terminated(
                    astra_turn_core::agent_live_event::AgentLiveTermination::Failed,
                    Some(msg.clone()),
                );
                Err(msg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CliSpawnAgentExecutor, TokenProvider, agent_live_stream_event_sink, build_child_messages,
    };
    use crate::lock_recovery::LockRecovery;
    use astra_runtime::orchestration::{
        InheritedPermissions, PermissionMode, PermissionSyncContext, SpawnAgentExecutor,
        SpawnRunConfig,
    };
    use astra_turn_core::agent_live_event::{
        AgentLiveEvent, AgentLiveEventKind, AgentLiveEventSink, AgentLiveSendError,
    };
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::cli::chat_stream::StreamEvent;
    fn test_permission_context() -> (
        InheritedPermissions,
        astra_runtime::orchestration::PermissionSyncHandle,
    ) {
        let inherited_permissions = InheritedPermissions::new(PermissionMode::Prompt);
        let permission_context = PermissionSyncContext::shared(inherited_permissions.clone());
        (inherited_permissions, permission_context)
    }

    #[derive(Debug, Default)]
    struct RecordingLiveSink(std::sync::Mutex<Vec<AgentLiveEvent>>);

    impl AgentLiveEventSink for RecordingLiveSink {
        fn send(&self, event: AgentLiveEvent) -> Result<(), AgentLiveSendError> {
            self.0.lock_recover().push(event);
            Ok(())
        }
    }

    #[test]
    fn test_executor_creation() {
        let api = astra_thin_client::ThinClient::new("http://test", None).expect("test api");
        let executor =
            CliSpawnAgentExecutor::new(api, "token".to_string(), PathBuf::from("/tmp"), None);
        assert!(executor.skill_resolver.is_none());
        assert!(
            executor.token_provider.is_none(),
            "fresh executor must have no provider — production wires \
             one via with_token_provider"
        );
        assert!(executor.default_model.is_none());
    }

    #[test]
    fn resolve_effective_model_prefers_spawn_model_then_default() {
        let api = astra_thin_client::ThinClient::new("http://test", None).expect("test api");
        let executor =
            CliSpawnAgentExecutor::new(api, "token".to_string(), PathBuf::from("/tmp"), None)
                .with_default_model(Some("session-default".to_string()));

        assert_eq!(
            executor
                .resolve_effective_model(Some("spawn-model"))
                .as_deref(),
            Some("spawn-model")
        );
        assert_eq!(
            executor.resolve_effective_model(None).as_deref(),
            Some("session-default")
        );
    }

    #[test]
    fn agent_live_stream_event_sink_translates_directly_without_stream_channel() {
        let live_sink = Arc::new(RecordingLiveSink::default());
        let stream_sink =
            agent_live_stream_event_sink("reviewer@abc12345".into(), Some(live_sink.clone()))
                .expect("sink");

        stream_sink.send(StreamEvent::Token("hello".into()));
        stream_sink.send(StreamEvent::ToolStarted {
            name: "bash".into(),
            description: "cargo test".into(),
            tool_use_id: "tool-1".into(),
            parent_tool_use_id: None,
        });

        let events = live_sink.0.lock_recover();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].kind, AgentLiveEventKind::OutputDelta(_)));
        assert!(matches!(
            events[1].kind,
            AgentLiveEventKind::ToolStarted { .. }
        ));
        assert_eq!(events[0].agent_id, "reviewer@abc12345");
    }

    /// REGRESSION (session 82ff91e5): sub-agent spawns failed with
    /// "Could not validate credentials" because the executor froze
    /// `token: String` at construction time and never refreshed it.
    /// Long-running interactive sessions rotate the token via the
    /// parent's 401-refresh-and-retry path; without a provider, the
    /// stale token stays in the spawn executor and every spawn 401s.
    ///
    /// This test pins the fix: when a token provider is installed,
    /// `resolve_token()` MUST return the provider's value, not the
    /// frozen one. Mutating the provider's source between calls
    /// proves freshness.
    #[test]
    fn token_provider_overrides_stale_captured_token() {
        let api = astra_thin_client::ThinClient::new("http://test", None).expect("test api");
        let executor_no_provider = CliSpawnAgentExecutor::new(
            api.clone(),
            "stale-token".to_string(),
            PathBuf::from("/tmp"),
            None,
        );
        assert_eq!(
            executor_no_provider.resolve_token(),
            "stale-token",
            "without a provider, fall back to the captured token"
        );

        // Provider returns whatever the shared mutable cell currently holds.
        let live_token = std::sync::Arc::new(std::sync::Mutex::new("v1".to_string()));
        let live_token_for_closure = live_token.clone();
        let provider: TokenProvider =
            std::sync::Arc::new(move || Some(live_token_for_closure.lock_recover().clone()));
        let executor = CliSpawnAgentExecutor::new(
            api,
            "stale-frozen-fallback".to_string(),
            PathBuf::from("/tmp"),
            None,
        )
        .with_token_provider(provider);

        assert_eq!(
            executor.resolve_token(),
            "v1",
            "provider must take precedence over the captured fallback"
        );
        // Simulate a token refresh in the parent flow.
        *live_token.lock_recover() = "v2-refreshed".to_string();
        assert_eq!(
            executor.resolve_token(),
            "v2-refreshed",
            "subsequent spawns must read the refreshed token, not a frozen copy"
        );
    }

    /// Defensive: when the provider returns `None` (e.g. user logged
    /// out mid-session), fall back to the captured token rather than
    /// crashing or sending an empty string. The captured token will
    /// itself fail with 401 — but at least with a recognisable error.
    #[test]
    fn token_provider_none_falls_back_to_captured() {
        let api = astra_thin_client::ThinClient::new("http://test", None).expect("test api");
        let provider: TokenProvider = std::sync::Arc::new(|| None);
        let executor = CliSpawnAgentExecutor::new(
            api,
            "fallback-token".to_string(),
            PathBuf::from("/tmp"),
            None,
        )
        .with_token_provider(provider);

        assert_eq!(
            executor.resolve_token(),
            "fallback-token",
            "provider returning None must fall back to the captured token"
        );
    }

    #[tokio::test]
    async fn async_token_provider_panic_surfaces_instead_of_using_stale_fallback() {
        let api = astra_thin_client::ThinClient::new("http://test", None).expect("test api");
        let provider: TokenProvider = std::sync::Arc::new(|| panic!("token store poisoned"));
        let executor =
            CliSpawnAgentExecutor::new(api, "stale-token".to_string(), PathBuf::from("/tmp"), None)
                .with_token_provider(provider);

        let err = executor.resolve_token_async().await.unwrap_err();
        assert!(
            err.contains("token provider task failed"),
            "join errors must be surfaced, got {err}"
        );
    }

    #[tokio::test]
    async fn token_resolution_failure_emits_terminal_live_event() {
        let api = astra_thin_client::ThinClient::new("http://test", None).expect("test api");
        let provider: TokenProvider = std::sync::Arc::new(|| panic!("token store poisoned"));
        let live_sink = Arc::new(RecordingLiveSink::default());
        let (inherited_permissions, permission_context) = test_permission_context();
        let executor =
            CliSpawnAgentExecutor::new(api, "stale-token".to_string(), PathBuf::from("/tmp"), None)
                .with_token_provider(provider);

        let err = executor
            .execute(SpawnRunConfig {
                run_id: "run-1".into(),
                agent_id: "reviewer@panic".into(),
                recursion_depth: 1,
                agent_type: "task".into(),
                task: "review".into(),
                system_prompt_addendum: String::new(),
                model: Some("test-model".into()),
                max_turns: 1,
                allowed_tools: Vec::new(),
                read_only: true,
                working_dir: PathBuf::from("/tmp"),
                mailbox: None,
                progress_emitter: None,
                context_cache: None,
                inherited_permissions,
                parent_address: None,
                permission_context,
                inherited_skills: Vec::new(),
                live_event_sink: Some(live_sink.clone()),
                inherited_prefix: None,
                execution_metadata: None,
                is_fork_child: false,
                delegation_chain: Vec::new(),
            })
            .await
            .expect_err("token provider panic should fail execute");

        assert!(err.contains("token provider task failed"), "{err}");
        let events = live_sink.0.lock_recover();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].kind,
            AgentLiveEventKind::AgentTerminated { .. }
        ));
    }

    /// Bug1 regression: when inherited prefix ends with a user or tool
    /// message, the child task (also user role) creates consecutive
    /// user messages. Bedrock Converse rejects this with HTTP 400:
    /// "The provided request is not valid".
    ///
    /// The fix must insert a synthetic assistant message between the
    /// prefix and the child task when the last prefix message has
    /// role "user" or "tool".
    #[test]
    fn prefix_ending_with_user_or_tool_must_insert_assistant_bridge() {
        // Simulate the message construction logic from execute()
        let prefix_messages = vec![json!({"role": "user", "content": "original prompt"})];
        let system_prompt = "You are a child agent.";
        let child_task = "Reply: inherited-ok";

        let messages =
            build_child_messages(system_prompt, Some(&prefix_messages), child_task, false);

        // After system, the messages should NOT have two consecutive user roles.
        let non_system: Vec<&str> = messages
            .iter()
            .filter_map(|m| m.get("role").and_then(|r| r.as_str()))
            .filter(|r| *r != "system")
            .collect();

        for window in non_system.windows(2) {
            let both_user = (window[0] == "user" || window[0] == "tool")
                && (window[1] == "user" || window[1] == "tool");
            assert!(
                !both_user,
                "consecutive user/tool messages detected: [{}, {}] — \
                 Bedrock will reject with HTTP 400",
                window[0], window[1]
            );
        }
    }

    /// Same as above but for prefix ending with tool role.
    #[test]
    fn prefix_ending_with_tool_must_insert_assistant_bridge() {
        let prefix_messages = vec![
            json!({"role": "user", "content": "do something"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "1", "function": {"name": "bash", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "done"}),
        ];
        let messages = build_child_messages("system", Some(&prefix_messages), "child task", false);

        let non_system: Vec<&str> = messages
            .iter()
            .filter_map(|m| m.get("role").and_then(|r| r.as_str()))
            .filter(|r| *r != "system")
            .collect();

        for window in non_system.windows(2) {
            let both_user = (window[0] == "user" || window[0] == "tool")
                && (window[1] == "user" || window[1] == "tool");
            assert!(
                !both_user,
                "consecutive user/tool messages detected: [{}, {}]",
                window[0], window[1]
            );
        }
    }

    /// When prefix ends with assistant, no bridge is needed.
    /// Fork mode: prefix is used verbatim (no system prepend).
    #[test]
    fn prefix_ending_with_assistant_needs_no_bridge() {
        let prefix_messages = vec![
            json!({"role": "system", "content": "parent system prompt"}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "hello"}),
        ];
        let messages = build_child_messages("ignored", Some(&prefix_messages), "child task", false);

        // Fork mode: prefix verbatim + child task. No bridge needed
        // because prefix ends with assistant → child task (user) is valid.
        let roles: Vec<&str> = messages
            .iter()
            .filter_map(|m| m.get("role").and_then(|r| r.as_str()))
            .collect();
        assert_eq!(roles, vec!["system", "user", "assistant", "user"]);
    }

    #[test]
    fn fork_mode_backfills_reasoning_fields_when_inherited_prefix_requires_them() {
        let prefix_messages = vec![
            json!({"role": "user", "content": "do something"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "1", "function": {"name": "bash", "arguments": "{}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "1", "content": "done"}),
        ];
        let messages = build_child_messages("system", Some(&prefix_messages), "child task", true);
        assert_eq!(messages[1]["reasoning_content"], "");
        assert_eq!(messages[3]["role"], "assistant");
        assert_eq!(messages[3]["reasoning_content"], "");
    }
}
