//! Fork skill sub-run executor for the CLI.
//!
//! When a skill has `execution_context: Fork`, the agentic loop delegates
//! execution to an [`IsolatedSkillExecutor`] which wraps a [`SkillSubRunExecutor`].
//! This module provides the CLI implementation that runs a separate agentic loop
//! using the same API and tool infrastructure as the parent conversation.

use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use astra_core::SkillSearchSettings;
use astra_runtime::{
    pipeline::step_protocol::InMemoryIdempotencyCache,
    pipeline::step_recorder::StepRecorder,
    prompts,
    semantic_dedup::SemanticDedup,
    turn::agentic::headless_round::HeadlessStderrStyle,
    turn::agentic_loop::finalization::run_agentic_loop_with_host,
    turn::agentic_loop::host::{
        AgenticLoopHost, AgenticLoopState, CancellationState, HostTurnResult, SkillState,
        StopHookState, TurnInteractionMode, TurnInteractionPolicy,
        interaction_scoped_tool_restrictions,
    },
    turn::chat_turn_heuristics::infer_task_execution_profile,
    turn::chat_turn_payload::{
        ChatTurnBasePayloadInput, chat_turn_base_payload, merge_edge_profile_extensions,
        set_payload_tool_results_if_non_empty,
    },
    turn::tool_schema_prune::openai_tool_names_from_schemas,
    turn::turn_guard::TurnGuard,
};
use astra_skills::executor::isolated::{SkillSubRunExecutor, SubRunResult};
use serde_json::{Value, json};

use super::effects::ChatTurnPrepLineGuard;
use super::permission_manager::{PermissionManager, PermissionMode};
use crate::cli::stream::stream_render::{EdgeSseContext, RenderPolicy, consume_turn_sse};
use crate::cli::chat_stream::turn_policy_from_payload_edge_tools;
use crate::edge_tools;

const SUBRUN_MAX_TURNS: usize = 25;

/// Cumulative token budget for skill subruns.
/// Caps total (prompt + completion) across all rounds to prevent runaway cost.
const SUBRUN_MAX_CUMULATIVE_TOKENS: u64 = 120_000;

// ─── SubRunHost ──────────────────────────────────────────────────────────────

/// Minimal agentic loop host for fork sub-runs.
///
/// Owns all resources so it doesn't borrow from a parent scope. Runs in quiet
/// mode with all terminal rendering suppressed.
///
/// Shared between skill sub-runs and delegate sub-runs.
pub(crate) struct SubRunHost {
    pub(crate) api: astra_thin_client::ThinClient,
    pub(crate) token: String,
    pub(crate) model: Option<String>,
    pub(crate) project_root: PathBuf,
    pub(crate) executor: std::sync::Arc<edge_tools::ToolExecutor>,
    pub(crate) all_schemas: Vec<Value>,
    pub(crate) valid_tool_names: HashSet<String>,
    pub(crate) perm_manager: PermissionManager,
    /// Shared journal writer from the parent session. When present,
    /// child LLM rounds are written to the parent's journal with an
    /// `agent_id` tag so the unified timeline can interleave them.
    pub(crate) journal: Option<std::sync::Arc<astra_services::session_journal::JournalWriter>>,
    /// Per-response completion token limit from the skill manifest.
    pub(crate) max_completion_tokens: Option<u32>,
    /// Effort level from the skill manifest.
    pub(crate) effort: Option<String>,
    /// Agent type hint from the skill manifest.
    pub(crate) agent_type: Option<String>,
    /// Parent cancellation token — so Ctrl+C / stop propagates into sub-runs.
    pub(crate) cancel_token: Option<std::sync::Arc<tokio_util::sync::CancellationToken>>,
    /// Same resolver as the parent loop so `skill` tool calls during the SSE edge
    /// round resolve (nested skills run inline — sub-run has no `skill_executor`).
    pub(crate) skill_resolver: Option<Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    /// When set, headless tool-round status lines are forwarded through this
    /// channel instead of being silently dropped. The receiver (e.g. `/team run`)
    /// renders them with an agent-id prefix.
    pub(crate) progress_tx: Option<tokio::sync::mpsc::UnboundedSender<SubRunProgressEvent>>,
    /// Agent identifier used to tag progress events.
    pub(crate) agent_id: String,
    /// Fine-grained live stream for spawned-agent UI drill-in.
    pub(crate) stream_event_tx: Option<crate::cli::chat_stream::StreamEventTx>,
    /// Direct live stream sink for spawned agents; avoids buffering
    /// child output through an unbounded channel.
    pub(crate) stream_event_sink: Option<crate::cli::chat_stream::SharedStreamEventSink>,
    /// Cross-turn tool output cache for edge-path dedup within this sub-run.
    pub(crate) tool_cache: super::stream_render::EdgeToolCache,
    /// Captured parent prefix, if the spawner resolved one. Consumed
    /// by `on_turn_completed` on the first successful ingested turn
    /// to emit a single [`ForkCacheEvent`]. `None` means the child
    /// wasn't asked to inherit — no probe runs.
    pub(crate) inherited_prefix: Option<astra_runtime::orchestration::InheritedChildPrefix>,
    /// Sink for fork-cache events. Shares lifetime with the
    /// executor. When `None` no probe fires — the executor simply
    /// didn't plumb one through (harmless, telemetry is off).
    pub(crate) fork_cache_sink:
        Option<std::sync::Arc<dyn astra_turn_core::fork_cache_event::ForkCacheEventSink>>,
    /// One-shot state tracking whether the first-turn probe has
    /// already fired. The hook is called every turn; we only want
    /// to emit one event per child spawn.
    pub(crate) fork_cache_probe_state: astra_runtime::orchestration::ForkCacheProbeState,
}

/// A progress event emitted by a sub-run agent.
#[derive(Debug, Clone)]
pub(crate) struct SubRunProgressEvent {
    pub agent_id: String,
    pub style: HeadlessStderrStyle,
    pub line: String,
}

pub(crate) fn persist_failed_subrun(state: &mut AgenticLoopState, error: &str) -> String {
    let failure_output = if state.final_text.trim().is_empty() {
        format!("[sub-run failed] {error}")
    } else {
        format!(
            "[sub-run failed] {error}\n\nPartial output:\n{}",
            state.final_text
        )
    };
    state.final_text = failure_output.clone();
    state.messages.push(json!({
        "role": "assistant",
        "content": failure_output.clone(),
    }));
    state.step_recorder.end_turn(false);

    let summary = state.step_recorder.summary();
    let blocked_tools = state
        .turn_guard
        .health
        .deprioritized_tools()
        .iter()
        .map(|tool| tool.to_string())
        .collect::<Vec<_>>();
    if let Some(heavy) = state.step_recorder.build_heavy_checkpoint(
        &state.messages,
        state.max_turn_input_tokens,
        state.remaining_turns as u32,
        &blocked_tools,
        &state.recent_tools,
    ) {
        let checkpoint = astra_pipeline::step_protocol::StepCheckpoint::Heavy(Box::new(heavy));
        let _ = astra_pipeline::step_checkpoint::write_step_checkpoint(
            &summary.session_id,
            summary.checkpoints,
            &checkpoint,
        );
    }

    failure_output
}

#[async_trait]
impl AgenticLoopHost for SubRunHost {
    async fn execute_turn(
        &mut self,
        state: &mut AgenticLoopState,
    ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
        self.executor
            .set_send_message_context(state.messaging.mailbox.as_ref().map(|mailbox| {
                crate::edge_tools::agent_messaging::SendMessageRuntimeContext {
                    agent_id: mailbox.address.agent_id.clone(),
                    router: mailbox.router(),
                    metrics: state.messaging.metrics.clone(),
                    delegation_id: mailbox.delegation_id.clone(),
                }
            }));

        // Session c47c2dca regression fix: drain runtime volatile lane so
        // stall nudges / circuit-breaker / budget advisories reach the
        // LLM on subrun paths too. Using `_appended_to` keeps the
        // outgoing payload protocol-valid (no consecutive role=user
        // pairs → no Bedrock HTTP 400).
        let augmented_messages: Option<Vec<serde_json::Value>> =
            state.take_volatile_pending_appended_to(state.messages.clone());
        let messages_slice: &[serde_json::Value] = match augmented_messages.as_ref() {
            Some(vec) => vec.as_slice(),
            None => state.messages.as_slice(),
        };

        let effective_model = state
            .skills
            .model_override
            .as_deref()
            .or(self.model.as_deref());
        let thinking = effective_model
            .map(|model| astra_turn_core::thinking_config::resolve_model_thinking(model).1)
            .unwrap_or_default();
        let interaction_mode = TurnInteractionMode::NonInteractive;
        let interaction_scoped_restrictions =
            interaction_scoped_tool_restrictions(interaction_mode);
        state
            .restricted_tools
            .extend(interaction_scoped_restrictions.iter().cloned());

        let mut payload = chat_turn_base_payload(ChatTurnBasePayloadInput {
            messages: messages_slice,
            session_id: state.current_session_id.as_deref(),
            agent_id: Some(self.agent_id.as_str()),
            model: effective_model,
            interaction_mode: Some(interaction_mode.label()),
            explain_verbose: false,
            explain_on: false,
            edge_executor_id: "subrun",
            capabilities: astra_thin_client::builtin_capability_preset(),
            project_root: &self.project_root,
            git_branch: None,
            thinking: thinking.clone(),
        });

        if let Some(max_tokens) = self.max_completion_tokens {
            payload["max_tokens"] = json!(max_tokens);
        }

        if let Some(ref effort) = self.effort {
            payload["effort"] = json!(effort);
        }

        if let Some(ref agent_type) = self.agent_type {
            payload["agent_type"] = json!(agent_type);
        }
        payload["skill_search"] =
            serde_json::to_value(&state.skills.search).unwrap_or_else(|_| json!({}));

        // Attach tool schemas. In fork mode, prefer the parent's frozen
        // canonical schemas so the tool-schema hash matches the parent's
        // cached prefix (cache key alignment). Falls back to live
        // registry if no frozen schemas are available.
        let tool_surface = astra_runtime::tool_registry::surface::ToolSurface::from_runtime_config(
            &self.all_schemas,
        );
        if let Some(deferred_tools_text) = tool_surface.deferred_block_text(effective_model) {
            let deferred_tools_context_window =
                prompts::budget_for_model(effective_model).model_limit;
            merge_edge_profile_extensions(
                &mut payload,
                &json!({
                    astra_runtime::turn::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT:
                        deferred_tools_text,
                    astra_runtime::turn::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_CONTEXT_WINDOW:
                        deferred_tools_context_window
                }),
            );
        }
        let schemas_to_use = resolve_subrun_schemas(
            self.inherited_prefix.as_ref(),
            tool_surface.pinned_schemas(),
        );
        astra_runtime::turn::agentic_prepare_payload::apply_selector_hints_then_attach_filtered_edge_tools(
            &mut payload,
            schemas_to_use,
            &mut state.restricted_tools,
            None,  // no selection report
            0.5,   // neutral confidence
            None,  // no learned task type
        );
        state.last_turn_policy = turn_policy_from_payload_edge_tools(&payload, interaction_mode);

        set_payload_tool_results_if_non_empty(&mut payload, &state.tool_results);

        // Sub-runs share the parent's session_id but have no turn_event_buffer.
        // Tell the bridge not to write llm_round events — the parent's journal
        // already records delegation results. Without this, the bridge writes
        // duplicate rounds to the parent's journal file.
        if let Some(root) = payload.as_object_mut() {
            root.insert("root_turn_journal_owned".into(), json!(true));
        }

        let resp = self
            .api
            .post_chat_turn_retry_429(&self.token, &payload, 3, true)
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            for name in &interaction_scoped_restrictions {
                state.restricted_tools.remove(name);
            }
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            return Err(astra_core::ClassifiedError::new(
                if status.as_u16() == 401 || status.as_u16() == 403 {
                    astra_core::ErrorKind::Auth
                } else if status.as_u16() == 429 {
                    astra_core::ErrorKind::RateLimit
                } else if status.is_server_error() {
                    astra_core::ErrorKind::ServerError
                } else {
                    astra_core::ErrorKind::Unknown
                },
                format!("Sub-run API error {status}: {body}"),
            ));
        }

        let edge_ctx = EdgeSseContext {
            api: &self.api,
            token: &self.token,
            executor_id: "subrun",
            executor: std::sync::Arc::clone(&self.executor),
            render_policy: RenderPolicy::Silent,
            perm_manager: Some(&mut self.perm_manager),
            cancel_token: self.cancel_token.as_ref().map(|t| t.as_ref()),
            stream_event_tx: self.stream_event_tx.clone(),
            stream_event_sink: self.stream_event_sink.clone(),
            approval_request_tx: None,
            ask_user_request_tx: None,
            skill_resolver: self.skill_resolver.clone(),
            skill_continuation: false,
            turn_rollback_on_failure: false,
            tool_cache: &mut self.tool_cache,
            observability_hub: None,
            incremental_state: None,
        };

        let prep_line = ChatTurnPrepLineGuard::maybe_start(false, None);
        let turn = consume_turn_sse(
            prep_line,
            resp,
            false, // render_md
            80,    // term_width
            RenderPolicy::Silent,
            Some(edge_ctx),
            0,                                              // pre_clear_lines
            None,                                           // auth_profile
            self.cancel_token.as_ref().map(|t| t.as_ref()), // propagate parent cancel
        )
        .await;
        for name in &interaction_scoped_restrictions {
            state.restricted_tools.remove(name);
        }

        Ok(HostTurnResult {
            accum: turn.core,
            ttft_ms: turn.ttft_ms,
            edge_tool_round: turn.edge_tool_round,
            error_kind: None,
        })
    }

    fn emit_headless_line(&mut self, style: HeadlessStderrStyle, line: String) {
        if let Some(ref tx) = self.progress_tx {
            let _ = tx.send(SubRunProgressEvent {
                agent_id: self.agent_id.clone(),
                style,
                line,
            });
        }
    }

    fn is_quiet(&self) -> bool {
        self.progress_tx.is_none()
    }

    fn valid_tool_names(&self) -> &HashSet<String> {
        &self.valid_tool_names
    }

    fn inject_tool_schema(&mut self, schema: Value) {
        if let Some(name) = schema
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
        {
            let name_owned = name.to_string();
            self.valid_tool_names.insert(name_owned.clone());
            if let Some(existing) = self.all_schemas.iter_mut().find(|tool| {
                tool.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    == Some(name_owned.as_str())
            }) {
                *existing = schema;
            } else {
                self.all_schemas.push(schema);
            }
        }
    }

    fn on_turn_completed(&mut self, state: &AgenticLoopState) {
        // PR 5.6: probe the first successful ingested turn's
        // cache_read_input_tokens against the parent-side estimate.
        if let Some(ref sink) = self.fork_cache_sink {
            astra_runtime::orchestration::maybe_emit_fork_cache_probe(
                &mut self.fork_cache_probe_state,
                self.inherited_prefix.as_ref(),
                state.current_run_id.as_deref().unwrap_or(""),
                state.total_cache_read,
                astra_turn_core::fork_cache_event::ForkCacheThresholds::default(),
                sink.as_ref(),
            );
        }

        // Unified timeline: emit the LATEST round event tagged with
        // agent_id to the parent's journal so the timeline renderer
        // can interleave child rounds with parent rounds.
        //
        // NOTE: `state.recent_rounds` is a **ring buffer** (capacity
        // RECENT_ROUNDS_RING_CAPACITY=32) that accumulates across
        // turns. Iterating the whole ring here would re-journal every
        // historical round on every turn end, causing duplicate
        // entries and inflated token accounting in the parent
        // timeline. Only the most recent entry — the round that just
        // completed — should be emitted.
        if let Some(ref journal) = self.journal {
            if let Some(round_summary) = state.recent_rounds.last() {
                let mut buf = astra_services::session_journal::TurnEventBuffer::begin_turn(
                    state.current_session_id.as_deref(),
                    state.current_round_index.saturating_add(1),
                );
                buf.record_llm_round(astra_services::session_journal::LlmRoundRecord {
                    duration_ms: round_summary.duration_ms,
                    prompt_tokens: round_summary.prompt_tokens,
                    completion_tokens: round_summary.completion_tokens,
                    cache_read_tokens: round_summary.cache_read_tokens,
                    cache_creation_tokens: round_summary.cache_creation_tokens,
                    tool_calls_returned: round_summary.tool_calls_returned,
                    tool_call_names: round_summary.tool_call_names.clone(),
                    finish_reason: round_summary.finish_reason.clone(),
                    source: Some("child_agent".to_string()),
                    run_id: state.current_run_id.clone(),
                    agent_id: Some(self.agent_id.clone()),
                    ..Default::default()
                });
                let events = buf.drain();
                crate::cli::cli_utils::append_bulk_journal_events_no_sync_or_warn(
                    journal,
                    state.current_session_id.as_deref(),
                    &events,
                    "skill_subrun:flush_round_events",
                );
            }
        }
    }
}

// ─── CliSkillSubRunExecutor ──────────────────────────────────────────────────

/// CLI implementation of [`SkillSubRunExecutor`].
///
/// Creates a fresh [`SubRunHost`] and [`AgenticLoopState`] for each sub-run,
/// then runs [`run_agentic_loop_with_host`] to completion.
///
/// Inherits the parent session's full [`PermissionMode`] (Auto/Prompt/Deny)
/// so that fork sub-runs enforce the same approval policy as the parent.
pub(crate) struct CliSkillSubRunExecutor {
    api: astra_thin_client::ThinClient,
    token: String,
    default_model: Option<String>,
    project_root: PathBuf,
    /// Full permission mode inherited from the parent session.
    permission_mode: PermissionMode,
    /// Parent cancellation token — propagated so Ctrl+C / stop interrupts subruns.
    cancel_token: Option<std::sync::Arc<tokio_util::sync::CancellationToken>>,
    /// Skill resolver inherited from parent — enables nested skill invocations.
    skill_resolver: Option<std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    /// Same surfacing policy as the parent loop / session state.
    skill_search: SkillSearchSettings,
    /// Parent interactive session id for self-introspection persistence.
    active_session_id: Option<String>,
}

impl CliSkillSubRunExecutor {
    pub fn new(
        api: astra_thin_client::ThinClient,
        token: String,
        default_model: Option<String>,
        project_root: PathBuf,
        permission_mode: PermissionMode,
        cancel_token: Option<std::sync::Arc<tokio_util::sync::CancellationToken>>,
    ) -> Self {
        Self {
            api,
            token,
            default_model,
            project_root,
            permission_mode,
            cancel_token,
            skill_resolver: None,
            skill_search: SkillSearchSettings::default(),
            active_session_id: None,
        }
    }

    /// Attach a skill resolver so sub-runs can invoke other skills.
    pub fn with_skill_resolver(
        mut self,
        resolver: Option<std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    ) -> Self {
        self.skill_resolver = resolver;
        self
    }

    pub fn with_skill_search(mut self, skill_search: SkillSearchSettings) -> Self {
        self.skill_search = skill_search;
        self
    }

    pub fn with_active_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.active_session_id = Some(session_id.into());
        self
    }
}

#[async_trait]
impl SkillSubRunExecutor for CliSkillSubRunExecutor {
    async fn execute_skill_subrun(
        &self,
        skill_name: &str,
        instructions: &str,
        task_context: &str,
        model: Option<&str>,
        max_tokens: Option<u32>,
        allowed_tools: &[String],
        parent_recursion_depth: u8,
        effort: Option<&str>,
        agent_type: Option<&str>,
    ) -> Result<SubRunResult, String> {
        let child_recursion_depth =
            astra_turn_core::agentic_recursion_guard::checked_child_recursion_depth(
                parent_recursion_depth,
            )?;
        let effective_model = model
            .map(String::from)
            .or_else(|| self.default_model.clone());
        let thinking = effective_model
            .as_deref()
            .map(|model| astra_turn_core::thinking_config::resolve_model_thinking(model).1)
            .unwrap_or_default();
        let compact_strategy = astra_turn_core::microcompact::CompactStrategy::from_provider_hint(
            effective_model.as_deref().unwrap_or(""),
        );
        // Resolve per-model workflow-guard policy up front; `effective_model`
        // is moved into the SubRunHost below.
        let resolved_tool_policy = astra_config::runtime_config::RuntimeConfig::load()
            .tool_selection
            .resolve_for_model(effective_model.as_deref());

        let all_schemas = edge_tools::all_tool_schemas();
        let valid_tool_names = openai_tool_names_from_schemas(&all_schemas);

        // Issue #326 P5b: skill subruns are headless — never read
        // project allow rules. Deny rules and the user-level rule
        // file are still honoured (apply_load_policy(HeadlessSafe)).
        let perm_manager = PermissionManager::with_load_policy(
            self.permission_mode,
            &self.project_root,
            &super::permission_manager::PermissionLoadPolicy::HeadlessSafe,
        );

        let executor = edge_tools::ToolExecutor::new(&self.project_root)
            .with_cloud(self.api.api_origin(), &self.token);
        if let Some(session_id) = self.active_session_id.as_deref() {
            executor.set_active_session_id(session_id.to_string());
        }

        let mut host = SubRunHost {
            api: self.api.clone(),
            token: self.token.clone(),
            model: effective_model.clone(),
            project_root: self.project_root.clone(),
            executor: std::sync::Arc::new(executor),
            all_schemas,
            valid_tool_names: valid_tool_names.clone(),
            perm_manager,
            max_completion_tokens: max_tokens,
            effort: effort.map(String::from),
            agent_type: agent_type.map(String::from),
            cancel_token: self.cancel_token.clone(),
            skill_resolver: self.skill_resolver.clone(),
            progress_tx: None,
            agent_id: String::new(),
            stream_event_tx: None,
            stream_event_sink: None,
            tool_cache: super::stream_render::EdgeToolCache::new(
                resolved_tool_policy.max_identical_tool_calls,
            ),
            // Skill sub-runs don't participate in fork-prefix cache
            // inheritance — skills are user-invoked, not spawner-
            // driven. Leave empty.
            inherited_prefix: None,
            fork_cache_sink: None,
            fork_cache_probe_state: astra_runtime::orchestration::ForkCacheProbeState::new(),
            journal: None,
        };

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

        let restricted_tools: HashSet<String> = if allowed_tools.is_empty() {
            HashSet::new()
        } else {
            let allowed: HashSet<&str> = allowed_tools.iter().map(|s| s.as_str()).collect();
            valid_tool_names
                .iter()
                .filter(|name| {
                    !allowed.contains(name.as_str())
                        && name.as_str() != astra_runtime::turn::skill_tool::SKILL_TOOL_NAME
                        && name.as_str()
                            != astra_runtime::turn::skill_tool::DISCOVER_SKILLS_TOOL_NAME
                })
                .cloned()
                .collect()
        };

        let task_profile = infer_task_execution_profile(task_context);
        let safe_name = astra_skills::loader::sanitize_for_path(skill_name);
        let subrun_session_id = format!(
            "subrun-{}-{}",
            safe_name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros()
        );
        let step_recorder = StepRecorder::with_persistence(
            &subrun_session_id,
            &format!("{}-task", subrun_session_id),
        );

        let mut state = AgenticLoopState {
            messages,
            volatile_pending: Vec::new(),
            recent_rounds: Vec::new(),
            tool_results: Vec::new(),
            session_memory_state: Default::default(),
            session_memory_llm_params: None,
            current_session_id: None,
            current_run_id: None,
            context_manifest_pool: None,
            context_manifest_user_id: None,
            context_manifest_model_name: effective_model.clone(),
            recursion_depth: child_recursion_depth,
            final_text: String::new(),
            final_text_streamed: false,
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_evidence_tool_calls: 0,
            has_any_usage: false,
            max_turns: SUBRUN_MAX_TURNS,
            remaining_turns: SUBRUN_MAX_TURNS,
            turn_budget_hint_emitted_90: false,
            turn_budget_hint_emitted_50: false,
            turn_budget_hint_emitted_20: false,
            agentic_turn_budget: task_profile.agentic_turn_budget,
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
                search: self.skill_search.clone(),
                tool_event_hooks: astra_skills::hooks::load_tool_event_hooks(&self.project_root),
                session_event_hooks: astra_skills::hooks::load_session_event_hooks(
                    &self.project_root,
                ),
                ..Default::default()
            },
            hooks: StopHookState {
                workspace_root_hint: Some(self.project_root.to_string_lossy().into_owned()),
                ..Default::default()
            },
            messaging: Default::default(),
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
            message: task_context.to_string(),
            recent_tools: Vec::new(),
            task_profile: infer_task_execution_profile(task_context),
            last_turn_policy: TurnInteractionPolicy::default(),
            api: self.api.clone(),
            api_token: self.token.clone(),
            delegation_engine: None,
            delegations_this_turn: 0,
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
            max_cumulative_tokens: SUBRUN_MAX_CUMULATIVE_TOKENS,
            thinking,
            recent_file_reads: Vec::new(),
            permission_context: None,
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

        if let Err(err) = run_agentic_loop_with_host(&mut host, &mut state).await {
            let err_str = err.to_string();
            let failure_output = persist_failed_subrun(&mut state, &err_str);
            return Err(failure_output);
        }

        let turns = (SUBRUN_MAX_TURNS - state.remaining_turns) as u32;
        let tokens_used = (state.total_prompt + state.total_completion) as u32;

        Ok(SubRunResult {
            output: state.final_text,
            tokens_used,
            turns,
        })
    }
}

/// Resolve the tool schema set for a sub-run.
///
/// Returns the parent's frozen canonical schemas when fork-prefix
/// inheritance is active **and** schemas were captured; otherwise
/// returns `fallback_pinned` (the live surface's T1 set).
///
/// When fork inheritance is configured but `frozen_tool_schemas` is
/// `None` we **must** fall back, but we also emit a warning: the
/// resulting `tool_schema_hash` will not align with the parent's, so
/// the prefix-cache reuse path silently misses. Without telemetry the
/// regression looks like "cache just doesn't help today" and lingers
/// in production unnoticed (silent miss → wasted tokens). Loud is
/// better than silent.
fn resolve_subrun_schemas(
    inherited: Option<&astra_runtime::orchestration::InheritedChildPrefix>,
    fallback_pinned: Vec<Value>,
) -> Vec<Value> {
    match inherited {
        Some(ip) => match &ip.frozen_tool_schemas {
            Some(schemas) => schemas.clone(),
            None => {
                tracing::warn!(
                    target: "astra_cli::skill_subrun",
                    prefix_id = %ip.prefix_id,
                    parent_run_id = %ip.parent_run_id,
                    "fork inheritance active but frozen_tool_schemas is None; \
                     falling back to T1 pinned schemas — child tool_schema_hash \
                     will not match parent's, prefix-cache reuse will miss"
                );
                fallback_pinned
            }
        },
        None => fallback_pinned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime::turn::agentic_loop::host::ASK_USER_TOOL_NAME;

    fn schema(name: &str) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": format!("{name} tool"),
                "parameters": { "type": "object", "properties": {} }
            }
        })
    }

    #[test]
    fn subrun_host_is_quiet_without_progress() {
        let root = PathBuf::from(".");
        let host = SubRunHost {
            api: astra_thin_client::ThinClient::new("http://unused", None).unwrap(),
            token: String::new(),
            model: None,
            project_root: root.clone(),
            executor: std::sync::Arc::new(edge_tools::ToolExecutor::new(&root)),
            all_schemas: Vec::new(),
            valid_tool_names: HashSet::new(),
            perm_manager: PermissionManager::with_project(true, &root),
            max_completion_tokens: None,
            effort: None,
            agent_type: None,
            cancel_token: None,
            skill_resolver: None,
            progress_tx: None,
            agent_id: String::new(),
            stream_event_tx: None,
            stream_event_sink: None,
            tool_cache: crate::cli::stream_render::EdgeToolCache::new(3),
            inherited_prefix: None,
            fork_cache_sink: None,
            fork_cache_probe_state: astra_runtime::orchestration::ForkCacheProbeState::new(),
            journal: None,
        };
        assert!(host.is_quiet());
    }

    #[test]
    fn subrun_host_not_quiet_with_progress() {
        let root = PathBuf::from(".");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let host = SubRunHost {
            api: astra_thin_client::ThinClient::new("http://unused", None).unwrap(),
            token: String::new(),
            model: None,
            project_root: root.clone(),
            executor: std::sync::Arc::new(edge_tools::ToolExecutor::new(&root)),
            all_schemas: Vec::new(),
            valid_tool_names: HashSet::new(),
            perm_manager: PermissionManager::with_project(true, &root),
            max_completion_tokens: None,
            effort: None,
            agent_type: None,
            cancel_token: None,
            skill_resolver: None,
            progress_tx: Some(tx),
            agent_id: "test-agent".to_string(),
            stream_event_tx: None,
            stream_event_sink: None,
            tool_cache: crate::cli::stream_render::EdgeToolCache::new(3),
            inherited_prefix: None,
            fork_cache_sink: None,
            fork_cache_probe_state: astra_runtime::orchestration::ForkCacheProbeState::new(),
            journal: None,
        };
        assert!(!host.is_quiet());
    }

    #[test]
    fn subrun_host_inject_tool_schema() {
        let root = PathBuf::from(".");
        let mut host = SubRunHost {
            api: astra_thin_client::ThinClient::new("http://unused", None).unwrap(),
            token: String::new(),
            model: None,
            project_root: root.clone(),
            executor: std::sync::Arc::new(edge_tools::ToolExecutor::new(&root)),
            all_schemas: Vec::new(),
            valid_tool_names: HashSet::new(),
            perm_manager: PermissionManager::with_project(true, &root),
            max_completion_tokens: None,
            effort: None,
            agent_type: None,
            cancel_token: None,
            skill_resolver: None,
            progress_tx: None,
            agent_id: String::new(),
            stream_event_tx: None,
            stream_event_sink: None,
            tool_cache: crate::cli::stream_render::EdgeToolCache::new(3),
            inherited_prefix: None,
            fork_cache_sink: None,
            fork_cache_probe_state: astra_runtime::orchestration::ForkCacheProbeState::new(),
            journal: None,
        };
        let schema = json!({
            "type": "function",
            "function": {
                "name": "test_tool",
                "description": "A test tool",
            }
        });
        host.inject_tool_schema(schema);
        assert!(host.valid_tool_names.contains("test_tool"));
        assert_eq!(host.all_schemas.len(), 1);
    }

    #[test]
    fn subrun_payload_policy_excludes_ask_user_in_noninteractive_mode() {
        let mut payload = json!({});
        let interaction_mode = TurnInteractionMode::NonInteractive;
        let mut restricted_tools = interaction_scoped_tool_restrictions(interaction_mode);

        astra_runtime::turn::agentic_prepare_payload::apply_selector_hints_then_attach_filtered_edge_tools(
            &mut payload,
            vec![schema("mo_query"), schema(ASK_USER_TOOL_NAME)],
            &mut restricted_tools,
            None,
            0.5,
            None,
        );

        let policy = turn_policy_from_payload_edge_tools(&payload, interaction_mode);
        assert_eq!(policy.visible_tool_names, vec!["mo_query".to_string()]);
        assert_eq!(policy.evidence_tool_names, vec!["mo_query".to_string()]);
        assert!(!policy.allow_ask_user);
    }

    #[tokio::test]
    async fn cli_skill_subrun_rejects_when_recursion_depth_limit_reached() {
        let executor = CliSkillSubRunExecutor::new(
            astra_thin_client::ThinClient::new("http://unused", None).unwrap(),
            "token".to_string(),
            Some("test-model".to_string()),
            PathBuf::from("."),
            PermissionMode::Deny,
            None,
        );
        let allowed_tools: Vec<String> = Vec::new();

        let err = executor
            .execute_skill_subrun(
                "depth-test",
                "Do work",
                "task",
                None,
                None,
                &allowed_tools,
                astra_turn_core::agentic_recursion_guard::MAX_AGENT_RECURSION_DEPTH,
                None,
                None,
            )
            .await
            .unwrap_err();

        assert!(err.contains("recursion depth 3 reached maximum 3"));
    }

    // ── Phase-R10 adversarial contract pins (CLI-side constants) ────────
    //
    // These pin the exact values of [`SUBRUN_MAX_TURNS`] and
    // [`SUBRUN_MAX_CUMULATIVE_TOKENS`] so silent drift (e.g. a typo
    // bumping 25→35 or 120_000→12_000) breaks this test loudly.
    // The server-side equivalents are pinned in
    // `rust/crates/astra-cli/tests/phase_r10_skill_subrun_contracts.rs`
    // via the now-`pub` constants in
    // [`astra_runtime::server::server_skill_subrun`].

    #[test]
    fn cli_subrun_max_turns_is_exactly_25() {
        assert_eq!(SUBRUN_MAX_TURNS, 25);
    }

    #[test]
    fn cli_subrun_max_cumulative_tokens_is_exactly_120_000() {
        assert_eq!(SUBRUN_MAX_CUMULATIVE_TOKENS, 120_000);
    }

    /// No fork inheritance → just use the live surface's pinned set.
    #[test]
    fn resolve_subrun_schemas_no_inheritance_uses_pinned_fallback() {
        let pinned = vec![schema("read_file"), schema("write_file")];
        let resolved = resolve_subrun_schemas(None, pinned.clone());
        assert_eq!(resolved, pinned);
    }

    /// Fork inheritance with captured schemas → use the captured set
    /// verbatim so the child's tool_schema_hash matches the parent's.
    #[test]
    fn resolve_subrun_schemas_fork_with_frozen_uses_parent_schemas() {
        use astra_runtime::orchestration::InheritedChildPrefix;
        let frozen = vec![schema("bash"), schema("grep")];
        let pinned_fallback = vec![schema("read_file"), schema("write_file")];
        let ip = InheritedChildPrefix {
            prefix_id: "p1".into(),
            parent_run_id: "r1".into(),
            provider: astra_turn_core::fork_prefix::ProviderKind::Anthropic,
            thinking: None,
            prefix_messages: Vec::new(),
            frozen_tool_schemas: Some(frozen.clone()),
            expected_cache_read_tokens: 0,
        };
        let resolved = resolve_subrun_schemas(Some(&ip), pinned_fallback);
        assert_eq!(resolved, frozen);
    }

    /// Fork inheritance present but `frozen_tool_schemas` is None — the
    /// degenerate case the reviewer flagged. We still have to return
    /// *something* that lets the child run, so we fall back to the
    /// T1 pinned set, but the helper's job is to make the regression
    /// loud (verified by the tracing target/log assertions in the
    /// surrounding integration; here we pin behavior + payload shape).
    #[test]
    fn resolve_subrun_schemas_fork_without_frozen_falls_back_to_pinned() {
        use astra_runtime::orchestration::InheritedChildPrefix;
        let pinned_fallback = vec![schema("read_file"), schema("write_file")];
        let ip = InheritedChildPrefix {
            prefix_id: "p2".into(),
            parent_run_id: "r2".into(),
            provider: astra_turn_core::fork_prefix::ProviderKind::Anthropic,
            thinking: None,
            prefix_messages: Vec::new(),
            frozen_tool_schemas: None,
            expected_cache_read_tokens: 0,
        };
        let resolved = resolve_subrun_schemas(Some(&ip), pinned_fallback.clone());
        // Behaviour: must return fallback (NOT empty, NOT inherited).
        assert_eq!(resolved, pinned_fallback);
    }

    // Session c47c2dca regression guard. Same invariant as
    // `cli_loop_host::tests::execute_turn_drains_volatile_lane_into_outgoing_messages`
    // but for the skill-subrun path (sub-agents also run the stall
    // detection machinery and their nudges must reach their LLM too).
    #[test]
    fn subrun_execute_turn_drains_volatile_lane() {
        // Session c47c2dca regression guard. Same shape as
        // `cli_loop_host::tests::execute_turn_drains_volatile_lane_into_outgoing_messages`
        // but for skill-subrun. Split the expected method name so this
        // test's literals don't self-match.
        let source = include_str!("skill_subrun.rs");
        // Look for the protocol-safe call syntax (`.method(`) assembled
        // via concat! so this test literal can't self-match. Do not
        // quote the call form verbatim anywhere in this test body.
        let safe_call = concat!(".take_volatile_pending", "_appended_to(");
        assert!(
            source.contains(safe_call),
            "skill_subrun::execute_turn must invoke the protocol-safe \
             drain method so runtime nudges reach the subrun LLM \
             (session c47c2dca regression + consecutive-user guard)."
        );
        assert!(
            source.contains("augmented.push(msg)"),
            "skill_subrun::execute_turn must append the drained volatile \
             msg to a local clone of state.messages"
        );
        assert!(
            source.contains("messages_slice"),
            "skill_subrun::execute_turn must pass an augmented messages_slice \
             to chat_turn_base_payload, not raw &state.messages"
        );
    }
}
