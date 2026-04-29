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
    semantic_dedup::SemanticDedup,
    turn::agentic_headless_round::HeadlessStderrStyle,
    turn::agentic_loop_finalization::run_agentic_loop_with_host,
    turn::agentic_loop_host::{
        AgenticLoopHost, AgenticLoopState, CancellationState, HostTurnResult, SkillState,
        StopHookState, TurnInteractionMode, TurnInteractionPolicy,
        interaction_scoped_tool_restrictions,
    },
    turn::chat_turn_heuristics::infer_task_execution_profile,
    turn::chat_turn_payload::{
        ChatTurnBasePayloadInput, chat_turn_base_payload, set_payload_tool_results_if_non_empty,
    },
    turn::tool_schema_prune::openai_tool_names_from_schemas,
    turn::turn_guard::TurnGuard,
};
use astra_skills::executor::isolated::{SkillSubRunExecutor, SubRunResult};
use serde_json::{Value, json};

use super::edge_tools;
use super::effects::ChatTurnPrepLineGuard;
use super::permission_manager::{PermissionManager, PermissionMode};
use super::stream_render::{EdgeSseContext, RenderPolicy, consume_turn_sse};
use crate::chat_stream::turn_policy_from_payload_edge_tools;

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
    /// Cross-turn tool output cache for edge-path dedup within this sub-run.
    pub(crate) tool_cache: super::stream_render::EdgeToolCache,
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

        let effective_model = state
            .skills
            .model_override
            .as_deref()
            .or(self.model.as_deref());
        let interaction_mode = TurnInteractionMode::NonInteractive;
        let interaction_scoped_restrictions =
            interaction_scoped_tool_restrictions(interaction_mode);
        state
            .restricted_tools
            .extend(interaction_scoped_restrictions.iter().cloned());

        let mut payload = chat_turn_base_payload(ChatTurnBasePayloadInput {
            messages: &state.messages,
            session_id: state.current_session_id.as_deref(),
            agent_id: Some(self.agent_id.as_str()),
            model: effective_model,
            explain_verbose: false,
            explain_on: false,
            edge_executor_id: "subrun",
            capabilities: astra_thin_client::builtin_capability_preset(),
            project_root: &self.project_root,
            git_branch: None,
            thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
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

        // Attach tool schemas directly (no selector).
        astra_runtime::turn::agentic_prepare_payload::apply_selector_hints_then_attach_filtered_edge_tools(
            &mut payload,
            self.all_schemas.clone(),
            &mut state.restricted_tools,
            None,  // no selection report
            0.5,   // neutral confidence
            "",    // no learned context
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
            stream_event_tx: None,
            approval_request_tx: None,
            skill_resolver: self.skill_resolver.clone(),
            skill_continuation: false,
            turn_rollback_on_failure: false,
            tool_cache: &mut self.tool_cache,
            observability_hub: None,
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

        let perm_manager =
            PermissionManager::with_project_mode(self.permission_mode, &self.project_root);

        let executor = edge_tools::ToolExecutor::new(&self.project_root)
            .with_cloud(self.api.api_origin(), &self.token);
        if let Some(session_id) = self.active_session_id.as_deref() {
            executor.set_active_session_id(session_id.to_string());
        }

        let mut host = SubRunHost {
            api: self.api.clone(),
            token: self.token.clone(),
            model: effective_model,
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
            tool_cache: super::stream_render::EdgeToolCache::new(
                resolved_tool_policy.max_identical_tool_calls,
            ),
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
            tool_results: Vec::new(),
            current_session_id: None,
            current_run_id: None,
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
            agentic_turn_budget: task_profile.agentic_turn_budget,
            current_round_index: 0,
            llm_rounds_completed: 0,
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
            evolution_service: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            pinned_tool_schema_tokens: 0,
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            skill_produced_output: false,
            max_cumulative_tokens: SUBRUN_MAX_CUMULATIVE_TOKENS,
            thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
            recent_file_reads: Vec::new(),
            permission_context: None,
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            pending_reflection_signals: Vec::new(),
            recent_tactical_actions: Vec::new(),
            server_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            continuity: Default::default(),
            compact_strategy,
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            bridge_turn_chain_id: None,
            bridge_user_query_event_id: None,
            turn_event_buffer: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime::turn::agentic_loop_host::ASK_USER_TOOL_NAME;

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
            tool_cache: crate::stream_render::EdgeToolCache::new(3),
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
            tool_cache: crate::stream_render::EdgeToolCache::new(3),
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
            tool_cache: crate::stream_render::EdgeToolCache::new(3),
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
            "",
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
}
