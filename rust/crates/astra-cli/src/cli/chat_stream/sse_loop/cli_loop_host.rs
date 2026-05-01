//! CLI implementation of [`AgenticLoopHost`].
//!
//! Wraps CLI-specific concerns (tool executor, permission manager, selector,
//! skill registry, terminal rendering) behind the runtime trait so the
//! multi-turn loop runs in the runtime crate.

use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use astra_runtime::{
    tool_registry::ToolRegistry,
    tool_selector::ToolSelector,
    turn::agentic_headless_round::HeadlessStderrStyle,
    turn::agentic_loop_host::{
        AgenticLoopHost, AgenticLoopState, HostReflectionRequest, HostReflectionResult,
        HostTurnResult, TurnInteractionMode, interaction_scoped_tool_restrictions,
    },
    turn::chat_turn_api_error::CHAT_TURN_POST_MAX_RETRIES,
    turn::chat_turn_payload::{ChatTurnBasePayloadInput, chat_turn_base_payload},
};
use async_trait::async_trait;
use crossterm::style::Stylize;
use serde_json::{Value, json};

use crate::{
    ExplainMode,
    edge_tools::ToolExecutor,
    effects::ChatTurnPrepLineGuard,
    permission_manager::{PermissionManager, PermissionMode},
    stream_render::{EdgeSseContext, RenderPolicy, consume_turn_sse},
};

use super::agentic_loop_turn::{
    ChatTurnSseFetchRequest, PrepareTurnTelemetry, fetch_chat_turn_sse,
};

/// CLI host for the runtime agentic loop.
///
/// Holds all CLI-specific dependencies; the runtime loop calls `execute_turn()`
/// which delegates to the existing `fetch_chat_turn_sse` pipeline.
pub(crate) struct CliAgenticLoopHost<'a> {
    pub api: &'a astra_thin_client::ThinClient,
    pub token: String,
    pub auth_profile: Option<&'a str>,
    pub model: Option<&'a str>,
    pub explain: ExplainMode,
    pub render_md: bool,
    pub term_width: usize,
    pub render_policy: RenderPolicy,
    pub message: &'a str,
    pub history: &'a [(String, String)],
    pub recent_tools: &'a [String],
    pub project_root: PathBuf,
    pub executor: Arc<ToolExecutor>,
    pub selector: &'a dyn ToolSelector,
    pub registry: ToolRegistry,
    pub all_schemas: Vec<Value>,
    pub file_context: Vec<String>,
    pub perm_manager: &'a mut PermissionManager,
    pub valid_tool_names: HashSet<String>,
    /// Lines written to stderr between SSE turns (headless tool output, etc.)
    /// that the next `consume_turn_sse` must clear before streaming.
    pub pending_clear_lines: usize,
    pub is_plan_subtask: bool,
    pub plan_subtask_id: Option<&'a str>,
    pub plan_assemble_line_release: Option<Arc<AtomicBool>>,
    /// Optional channel for forwarding fine-grained stream events.
    pub stream_event_tx: Option<super::super::StreamEventTx>,
    /// Optional channel for async tool approval requests during plan execution.
    pub approval_request_tx: Option<super::super::ApprovalRequestTx>,
    /// Root-level messaging context used when the current turn has no mailbox.
    pub root_send_message_context:
        Option<crate::edge_tools::agent_messaging::SendMessageRuntimeContext>,
    /// REPL turn counter (0-based) for correct turn_id in trace collector.
    pub repl_turn_index: u32,
    /// Cross-turn tool output cache for edge-path dedup.
    pub tool_cache: crate::stream_render::EdgeToolCache,
    /// Optional fork-prefix store: when set + fork flag enabled,
    /// this host calls `capture_parent_prefix` in its
    /// `on_turn_completed` hook, feeding the store that the
    /// DynamicAgentSpawner and DelegationEngine share. Without
    /// this, a captured parent prefix never exists and children
    /// always resolve to None — which was the exact observation
    /// during live MiniMax verification (spawn succeeded,
    /// delegate succeeded, but fork-cache events never fired
    /// because no parent capture happened).
    pub prefix_store:
        Option<std::sync::Arc<dyn astra_turn_core::fork_prefix_store::PrefixCaptureSink>>,
}

fn derive_turn_interaction_mode(
    permission_mode: PermissionMode,
    is_plan_subtask: bool,
    has_approval_request_tx: bool,
    render_is_silent: bool,
    stdin_is_terminal: bool,
) -> TurnInteractionMode {
    if is_plan_subtask || has_approval_request_tx || render_is_silent || !stdin_is_terminal {
        return TurnInteractionMode::NonInteractive;
    }
    match permission_mode {
        PermissionMode::Prompt => TurnInteractionMode::Prompt,
        PermissionMode::Auto => TurnInteractionMode::Auto,
        PermissionMode::Deny => TurnInteractionMode::Deny,
    }
}

impl CliAgenticLoopHost<'_> {
    fn turn_interaction_mode(&self) -> TurnInteractionMode {
        derive_turn_interaction_mode(
            self.perm_manager.mode(),
            self.is_plan_subtask,
            self.approval_request_tx.is_some(),
            self.render_policy.is_silent(),
            std::io::stdin().is_terminal(),
        )
    }
}

#[async_trait]
impl AgenticLoopHost for CliAgenticLoopHost<'_> {
    async fn execute_turn(
        &mut self,
        state: &mut AgenticLoopState,
    ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
        let assembly_start = Instant::now();

        // Preserve the lifecycle-created collector: it may already contain the
        // initial skill selector shortlist for this turn.
        let turn_id = format!("turn-{}", self.repl_turn_index);
        let session_id = state.current_session_id.clone().unwrap_or_default();
        if let Some(ref collector) = state.telemetry.turn_trace_collector {
            collector.set_turn_id(turn_id);
            collector.set_session_id(session_id);
        } else {
            state.telemetry.turn_trace_collector = Some(
                astra_runtime::turn::turn_trace_collector::TurnTraceCollector::new(
                    turn_id, session_id,
                ),
            );
        }
        let pre_clear = std::mem::take(&mut self.pending_clear_lines);

        // If a skill activation overrode the model, use that; otherwise fall back to host default.
        let effective_model = state.skills.model_override.as_deref().or(self.model);

        // Skill allowed_tools is additive — it ensures skill-referenced tools
        // are visible to the model via schema injection, but never restricts
        // other tools. Only interaction-scoped restrictions apply here.
        let interaction_mode = self.turn_interaction_mode();
        let interaction_scoped_restrictions =
            interaction_scoped_tool_restrictions(interaction_mode);
        state
            .restricted_tools
            .extend(interaction_scoped_restrictions.iter().cloned());

        // Propagate skill sandbox policy to the tool executor for this turn.
        // Saved/restored so it doesn't persist after the skill deactivates.
        let prev_sandbox = self
            .executor
            .sandbox_policy
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(ref policy) = state.skills.sandbox_policy {
            *self
                .executor
                .sandbox_policy
                .write()
                .unwrap_or_else(|e| e.into_inner()) = Some(policy.clone());
        } else {
            *self
                .executor
                .sandbox_policy
                .write()
                .unwrap_or_else(|e| e.into_inner()) = prev_sandbox.clone();
        }
        self.executor.set_send_message_context(
            state
                .messaging
                .mailbox
                .as_ref()
                .map(
                    |mailbox| crate::edge_tools::agent_messaging::SendMessageRuntimeContext {
                        agent_id: mailbox.address.agent_id.clone(),
                        router: mailbox.router(),
                        metrics: state.messaging.metrics.clone(),
                        delegation_id: mailbox.delegation_id.clone(),
                    },
                )
                .or_else(|| self.root_send_message_context.clone()),
        );

        let turn_result = fetch_chat_turn_sse(ChatTurnSseFetchRequest {
            api: self.api,
            token: self.token.as_str(),
            auth_profile: self.auth_profile,
            model: effective_model,
            explain: self.explain,
            render_md: self.render_md,
            term_width: self.term_width,
            render_policy: self.render_policy,
            message: self.message,
            history: self.history,
            recent_tools: self.recent_tools,
            project_root: self.project_root.as_path(),
            executor: Arc::clone(&self.executor),
            selector: self.selector,
            registry: &self.registry,
            messages: state.messages.as_slice(),
            ephemeral_prefix: state.skills.listing_message.as_ref(),
            current_session_id: state.current_session_id.as_deref(),
            tool_results: state.tool_results.as_slice(),
            all_schemas: &self.all_schemas,
            turn_guard: &state.turn_guard,
            restricted_tools: &mut state.restricted_tools,
            step_recorder: &mut state.step_recorder,
            file_context: &self.file_context,
            assembly_start,
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut state.telemetry.first_memoria_ms,
                first_selector_ms: &mut state.telemetry.first_selector_ms,
                first_selector_strategy: &mut state.telemetry.first_selector_strategy,
                first_selector_confidence: &mut state.telemetry.first_selector_confidence,
                selector_tokens_in: &mut state.telemetry.selector_tokens_in,
                selector_tokens_out: &mut state.telemetry.selector_tokens_out,
                first_selection_report: &mut state.telemetry.first_selection_report,
                first_budget_pressure: &mut state.telemetry.first_budget_pressure,
                first_context_assembly_ms: &mut state.telemetry.first_context_assembly_ms,
                all_selected_skills: &mut state.telemetry.all_selected_skills,
                initial_skill_selector_shortlist: state
                    .telemetry
                    .initial_skill_selector_shortlist
                    .as_ref()
                    .and_then(|shortlist| serde_json::to_value(shortlist).ok()),
                trace_collector: state.telemetry.turn_trace_collector.as_ref(),
            },
            perm_manager: self.perm_manager,
            skill_search: &state.skills.search,
            pre_clear_lines: pre_clear,
            is_plan_subtask: self.is_plan_subtask,
            plan_subtask_id: self.plan_subtask_id,
            cancel_token: state.cancellation.token.as_deref(),
            plan_assemble_line_release: self.plan_assemble_line_release.clone(),
            stream_event_tx: self.stream_event_tx.clone(),
            approval_request_tx: self.approval_request_tx.clone(),
            skill_resolver: state.skills.resolver.clone(),
            skill_effort: state.skills.effort.as_ref().map(|e| e.to_string()),
            skill_agent_type: state.skills.agent_type.clone(),
            tool_budget_override: state.tool_budget_override,
            interaction_mode,
            turn_policy: &mut state.last_turn_policy,
            skill_allowed_tools: state
                .skills
                .allowed_tools
                .as_ref()
                .map(|s| s.iter().cloned().collect::<Vec<_>>()),
            skill_continuation: state.skill_produced_output,
            tool_cache: &mut self.tool_cache,
            previous_confidence_fallback: state
                .last_confidence_diagnosis
                .as_ref()
                .map(|d| d.fallback.clone()),
            round_index: state.current_round_index,
            session_turn: state.session_turn,
            turn_chain_id: state.bridge_turn_chain_id.as_deref(),
            user_query_event_id: state.bridge_user_query_event_id.as_deref(),
            observability_hub: state.telemetry.observability_hub.as_ref(),
        })
        .await;

        for name in &interaction_scoped_restrictions {
            state.restricted_tools.remove(name);
        }

        // Restore previous sandbox policy after the turn.
        *self
            .executor
            .sandbox_policy
            .write()
            .unwrap_or_else(|e| e.into_inner()) = prev_sandbox;

        // Sync latest approval overrides into state for checkpoint persistence.
        state.approval_overrides = self.perm_manager.export_session_overrides();

        let turn_result = turn_result?;
        if let Some(refreshed_token) = turn_result.refreshed_token.clone() {
            self.executor.set_cloud_token(refreshed_token.clone());
            self.token = refreshed_token;
        }

        Ok(HostTurnResult {
            accum: turn_result.core,
            ttft_ms: turn_result.ttft_ms,
            edge_tool_round: turn_result.edge_tool_round,
            error_kind: None,
        })
    }

    fn supports_auto_reflection(&self) -> bool {
        true
    }

    async fn execute_reflection(
        &mut self,
        state: &mut AgenticLoopState,
        request: HostReflectionRequest<'_>,
    ) -> Result<Option<HostReflectionResult>, astra_core::ClassifiedError> {
        let effective_model = state.skills.model_override.as_deref().or(self.model);
        let reflection_messages = vec![
            json!({"role": "system", "content": request.system_prompt}),
            json!({"role": "user", "content": request.user_prompt}),
        ];

        let mut payload = chat_turn_base_payload(ChatTurnBasePayloadInput {
            messages: &reflection_messages,
            session_id: state.current_session_id.as_deref(),
            agent_id: Some("astra-cli-reflect"),
            model: effective_model,
            explain_verbose: false,
            explain_on: false,
            edge_executor_id: "auto-reflection",
            capabilities: astra_thin_client::builtin_capability_preset(),
            project_root: self.project_root.as_path(),
            git_branch: None,
            thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
        });
        if let Some(max_tokens) = request.max_output_tokens {
            payload["max_tokens"] = json!(max_tokens);
        }

        let resp = self
            .api
            .post_chat_turn_retry_429(&self.token, &payload, CHAT_TURN_POST_MAX_RETRIES, true)
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            return Err(format!("auto-reflection API error {status}: {body}").into());
        }

        let prep_line = ChatTurnPrepLineGuard::maybe_start(false, None);
        let turn = consume_turn_sse(
            prep_line,
            resp,
            false,
            self.term_width,
            RenderPolicy::Silent,
            Some(EdgeSseContext {
                api: self.api,
                token: &self.token,
                executor_id: "auto-reflection",
                executor: Arc::clone(&self.executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: Some(self.perm_manager),
                cancel_token: state.cancellation.token.as_deref(),
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut self.tool_cache,
                observability_hub: None,
            }),
            0,
            self.auth_profile,
            state.cancellation.token.as_deref(),
        )
        .await;

        if turn.core.has_tool_calls {
            return Err("auto-reflection unexpectedly returned tool calls"
                .to_string()
                .into());
        }

        Ok(Some(HostReflectionResult {
            full_text: turn.core.full_text.trim().to_string(),
            prompt_tokens: turn.core.prompt_tokens,
            completion_tokens: turn.core.completion_tokens,
            cache_read_tokens: turn.core.cache_read_tokens,
            cache_creation_tokens: turn.core.cache_creation_tokens,
            has_usage: turn.core.has_usage,
        }))
    }

    fn emit_headless_line(&mut self, style: HeadlessStderrStyle, line: String) {
        // Forward to stream event channel (even in suppress mode)
        if let Some(tx) = &self.stream_event_tx {
            let _ = tx.send(super::super::StreamEvent::StatusLine(line.clone()));
        }
        if self.render_policy.suppress_headless() {
            return;
        }
        match style {
            HeadlessStderrStyle::Dim => eprintln!("{}", line.dim()),
            HeadlessStderrStyle::Red => eprintln!("{}", line.red()),
            HeadlessStderrStyle::Green => eprintln!("{}", line.green()),
            HeadlessStderrStyle::Yellow => eprintln!("{}", line.yellow()),
            HeadlessStderrStyle::CyanBold => eprintln!("{}", line.cyan().bold()),
            HeadlessStderrStyle::Magenta => {
                eprint!("{}", "│ ".dim());
                eprintln!("{}", line.magenta());
            }
            HeadlessStderrStyle::DiffAdd => {
                let body = line.strip_prefix('+').unwrap_or(line.as_str());
                eprint!("{}", "│ ".dim());
                eprint!("{}", "+".green().bold());
                eprintln!("{}", body.green());
            }
            HeadlessStderrStyle::DiffRemove => {
                let body = line.strip_prefix('-').unwrap_or(line.as_str());
                eprint!("{}", "│ ".dim());
                eprint!("{}", "-".red().bold());
                eprintln!("{}", body.red());
            }
            HeadlessStderrStyle::DiffContext => {
                eprint!("{}", "│ ".dim());
                eprintln!("{}", line.dim());
            }
            HeadlessStderrStyle::Normal => eprintln!("{}", line),
        }
        self.pending_clear_lines += 1;
    }

    fn is_quiet(&self) -> bool {
        self.render_policy.is_silent()
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
            self.registry.upsert_schema(schema.clone());
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

    fn render_final_text(&mut self, text: &str) {
        use std::io::Write;
        if self.render_policy.is_silent() || self.render_policy == RenderPolicy::PlanDecompose {
            return;
        }
        if text.is_empty() {
            return;
        }
        if self.render_md {
            let mut md = crate::streaming_md::StreamingMarkdown::new(self.term_width);
            md.push(text);
            md.finish();
        } else {
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
            let _ = std::io::stdout().flush();
        }
    }

    fn on_turn_completed(
        &mut self,
        state: &astra_runtime::turn::agentic_loop_host::AgenticLoopState,
    ) {
        // Bug B step 3: capture the parent turn's cacheable prefix
        // so subsequent spawn_agent / delegate calls can inherit it
        // for prompt-cache reuse. No-op unless:
        //   - the `prefix_store` Arc was plumbed in (CLI startup
        //     sets this on every host when fork_prefix.enabled)
        //   - feature flag is on (capture_parent_prefix
        //     early-returns otherwise, preserving the
        //     FeatureDisabled contract)
        //   - ingest populated the expected state fields
        let Some(store) = self.prefix_store.as_ref() else {
            return;
        };
        let parent_run_id = match state.current_run_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return,
        };
        let model_id = self.model.unwrap_or("").to_string();
        let provider = astra_turn_core::fork_prefix::ProviderKind::from_provider_hint(&model_id);
        // Canonical prefix bytes: JSON-serialize the messages as-is.
        // This is the format `fork_reconstruct::reconstruct_messages`
        // expects on the consuming end. System prompts and tool
        // schemas are captured separately; for step 3 we ship a
        // minimal-but-correct subset — the messages array is what
        // downstream prepending into child state cares about.
        let Ok(canonical_prefix_bytes) = serde_json::to_vec(&state.messages) else {
            return;
        };
        let captured_at_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // G1 payload fill: `all_schemas` is exactly the tool list the
        // CLI advertised to the LLM this turn, so it's the honest
        // source for `tool_schemas`. `system_blocks` stays empty here
        // because the CLI doesn't assemble the final system prompt
        // locally — the server-side bridge does that. A future PR on
        // the server host (G2) will populate system_blocks where the
        // real bytes exist.
        let tool_schemas =
            astra_turn_core::fork_prefix::build_tool_schema_entries(&self.all_schemas);
        let req = astra_turn_core::fork_capture::CaptureRequest {
            parent_run_id,
            parent_turn_seq: self.repl_turn_index,
            provider,
            model_id,
            thinking: None,
            system_blocks: vec![],
            tool_schemas,
            beta_headers: vec![],
            canonical_prefix_bytes,
            cache_mode: astra_turn_core::fork_prefix::CacheMode::Write,
            captured_at_secs,
            microcompact_fired_in_turn: false,
        };
        let _ = astra_turn_core::fork_capture::capture_parent_prefix(req, store.as_ref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_turn_interaction_mode_maps_permission_mode_when_interactive() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Prompt, false, false, false, true),
            TurnInteractionMode::Prompt
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Auto, false, false, false, true),
            TurnInteractionMode::Auto
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Deny, false, false, false, true),
            TurnInteractionMode::Deny
        );
    }

    #[test]
    fn derive_turn_interaction_mode_forces_noninteractive_for_subtasks_and_silent_turns() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Prompt, true, false, false, true),
            TurnInteractionMode::NonInteractive
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Prompt, false, false, true, true),
            TurnInteractionMode::NonInteractive
        );
    }

    #[test]
    fn derive_turn_interaction_mode_forces_noninteractive_without_tty_or_with_approval_channel() {
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Prompt, false, false, false, false),
            TurnInteractionMode::NonInteractive
        );
        assert_eq!(
            derive_turn_interaction_mode(PermissionMode::Prompt, false, true, false, true),
            TurnInteractionMode::NonInteractive
        );
    }
}
