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
    turn::agentic_loop_host::{AgenticLoopHost, AgenticLoopState, HostTurnResult},
};
use async_trait::async_trait;
use crossterm::style::Stylize;
use serde_json::Value;

use crate::{ExplainMode, edge_tools::ToolExecutor, permission_manager::PermissionManager};

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
    pub quiet: bool,
    pub suppress_intermediate_output: bool,
    pub hide_streaming_assistant_text: bool,
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
}

#[async_trait]
impl AgenticLoopHost for CliAgenticLoopHost<'_> {
    async fn execute_turn(
        &mut self,
        state: &mut AgenticLoopState,
    ) -> Result<HostTurnResult, String> {
        let assembly_start = Instant::now();
        let pre_clear = std::mem::take(&mut self.pending_clear_lines);

        // If a skill activation overrode the model, use that; otherwise fall back to host default.
        let effective_model = state.skill_model_override.as_deref().or(self.model);

        // Skill-scoped restrictions: computed fresh each turn from skill_allowed_tools
        // and applied transiently (removed after the turn) so they don't accumulate
        // in the permanent restricted_tools set.
        let skill_scoped_restrictions: HashSet<String> =
            if let Some(ref allowed) = state.skill_allowed_tools {
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
        if let Some(ref policy) = state.skill_sandbox_policy {
            self.executor.sandbox_policy = Some(policy.clone());
        } else {
            self.executor.sandbox_policy = prev_sandbox.clone();
        }
        self.executor.set_send_message_context(
            state
                .mailbox
                .as_ref()
                .map(
                    |mailbox| crate::edge_tools::agent_messaging::SendMessageRuntimeContext {
                        agent_id: mailbox.address.agent_id.clone(),
                        router: mailbox.router(),
                        metrics: state.messaging_metrics.clone(),
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
            quiet: self.quiet,
            suppress_intermediate_output: self.suppress_intermediate_output,
            hide_streaming_assistant_text: self.hide_streaming_assistant_text,
            message: self.message,
            history: self.history,
            recent_tools: self.recent_tools,
            project_root: self.project_root.as_path(),
            executor: &mut self.executor,
            selector: self.selector,
            registry: &self.registry,
            messages: state.messages.as_slice(),
            ephemeral_prefix: state.skill_listing_message.as_ref(),
            current_session_id: state.current_session_id.as_deref(),
            tool_results: state.tool_results.as_slice(),
            all_schemas: &self.all_schemas,
            turn_guard: &state.turn_guard,
            restricted_tools: &mut state.restricted_tools,
            step_recorder: &mut state.step_recorder,
            file_context: &self.file_context,
            assembly_start,
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut state.first_memoria_ms,
                first_selector_ms: &mut state.first_selector_ms,
                first_selector_strategy: &mut state.first_selector_strategy,
                first_selector_confidence: &mut state.first_selector_confidence,
                selector_tokens_in: &mut state.selector_tokens_in,
                selector_tokens_out: &mut state.selector_tokens_out,
                first_selection_report: &mut state.first_selection_report,
                first_budget_pressure: &mut state.first_budget_pressure,
                first_context_assembly_ms: &mut state.first_context_assembly_ms,
                all_selected_skills: &mut state.all_selected_skills,
            },
            perm_manager: self.perm_manager,
            skill_search: &state.skill_search,
            pre_clear_lines: pre_clear,
            is_plan_subtask: self.is_plan_subtask,
            plan_subtask_id: self.plan_subtask_id,
            cancel_token: state.cancel_token.as_deref(),
            plan_assemble_line_release: self.plan_assemble_line_release.clone(),
            stream_event_tx: self.stream_event_tx.clone(),
            approval_request_tx: self.approval_request_tx.clone(),
            skill_resolver: state.skill_resolver.clone(),
            skill_effort: state.skill_effort.as_ref().map(|e| e.to_string()),
            skill_agent_type: state.skill_agent_type.clone(),
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

    fn emit_headless_line(&mut self, style: HeadlessStderrStyle, line: String) {
        // Forward to stream event channel (even in suppress mode)
        if let Some(tx) = &self.stream_event_tx {
            let _ = tx.send(super::super::StreamEvent::StatusLine(line.clone()));
        }
        if self.suppress_intermediate_output {
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
        self.quiet
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
