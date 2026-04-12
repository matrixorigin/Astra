//! CLI implementation of [`AgenticLoopHost`].
//!
//! Wraps CLI-specific concerns (tool executor, permission manager, selector,
//! skill registry, terminal rendering) behind the runtime trait so the
//! multi-turn loop runs in the runtime crate.

use std::collections::HashSet;
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
        HostTurnResult,
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
    permission_manager::PermissionManager,
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
    pub token: &'a str,
    pub model: Option<&'a str>,
    pub explain: ExplainMode,
    pub render_md: bool,
    pub term_width: usize,
    pub render_policy: RenderPolicy,
    pub message: &'a str,
    pub history: &'a [(String, String)],
    pub recent_tools: &'a [String],
    pub project_root: PathBuf,
    pub executor: ToolExecutor,
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
}

#[async_trait]
impl AgenticLoopHost for CliAgenticLoopHost<'_> {
    async fn execute_turn(
        &mut self,
        state: &mut AgenticLoopState,
    ) -> Result<HostTurnResult, String> {
        let assembly_start = Instant::now();

        // Create a fresh trace collector for each turn (so /context breakdown reflects
        // this turn only, not accumulated values from prior turns).
        let turn_id = format!("turn-{}", self.repl_turn_index);
        let session_id = state.current_session_id.clone().unwrap_or_default();
        state.telemetry.turn_trace_collector = Some(
            astra_runtime::turn::turn_trace_collector::TurnTraceCollector::new(turn_id, session_id),
        );
        let pre_clear = std::mem::take(&mut self.pending_clear_lines);

        // If a skill activation overrode the model, use that; otherwise fall back to host default.
        let effective_model = state.skills.model_override.as_deref().or(self.model);

        // Skill-scoped restrictions: computed fresh each turn from skill_allowed_tools
        // and applied transiently (removed after the turn) so they don't accumulate
        // in the permanent restricted_tools set.
        let skill_scoped_restrictions: HashSet<String> =
            if let Some(ref allowed) = state.skills.allowed_tools {
                self.valid_tool_names
                    .iter()
                    .filter(|name| {
                        !allowed.contains(*name)
                            && *name != astra_runtime::turn::skill_tool::SKILL_TOOL_NAME
                            && *name != astra_runtime::turn::skill_tool::DISCOVER_SKILLS_TOOL_NAME
                    })
                    .cloned()
                    .collect()
            } else {
                HashSet::new()
            };
        state
            .restricted_tools
            .extend(skill_scoped_restrictions.iter().cloned());

        // Propagate skill sandbox policy to the tool executor for this turn.
        // Saved/restored so it doesn't persist after the skill deactivates.
        let prev_sandbox = self.executor.sandbox_policy.take();
        if let Some(ref policy) = state.skills.sandbox_policy {
            self.executor.sandbox_policy = Some(policy.clone());
        } else {
            self.executor.sandbox_policy = prev_sandbox.clone();
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
            token: self.token,
            model: effective_model,
            explain: self.explain,
            render_md: self.render_md,
            term_width: self.term_width,
            render_policy: self.render_policy,
            message: self.message,
            history: self.history,
            recent_tools: self.recent_tools,
            project_root: self.project_root.as_path(),
            executor: &mut self.executor,
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
        })
        .await?;

        // Remove skill-scoped restrictions so they don't accumulate permanently.
        // They'll be re-computed fresh on the next turn if skill_allowed_tools is still set.
        for name in &skill_scoped_restrictions {
            state.restricted_tools.remove(name);
        }

        // Restore previous sandbox policy after the turn.
        self.executor.sandbox_policy = prev_sandbox;

        Ok(HostTurnResult {
            accum: turn_result.core,
            ttft_ms: turn_result.ttft_ms,
            edge_tool_round: turn_result.edge_tool_round,
        })
    }

    fn supports_auto_reflection(&self) -> bool {
        true
    }

    async fn execute_reflection(
        &mut self,
        state: &mut AgenticLoopState,
        request: HostReflectionRequest<'_>,
    ) -> Result<Option<HostReflectionResult>, String> {
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
            thinking_budget_tokens: None,
        });
        if let Some(max_tokens) = request.max_output_tokens {
            payload["max_tokens"] = json!(max_tokens);
        }

        let resp = self
            .api
            .post_chat_turn_retry_429(self.token, &payload, CHAT_TURN_POST_MAX_RETRIES, true)
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            return Err(format!("auto-reflection API error {status}: {body}"));
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
                token: self.token,
                executor_id: "auto-reflection",
                executor: &mut self.executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: Some(self.perm_manager),
                cancel_token: state.cancellation.token.as_deref(),
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
            }),
            0,
            state.cancellation.token.as_deref(),
        )
        .await;

        if turn.core.has_tool_calls {
            return Err("auto-reflection unexpectedly returned tool calls".to_string());
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
}
