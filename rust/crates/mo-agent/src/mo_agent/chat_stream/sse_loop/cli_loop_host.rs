//! CLI implementation of [`AgenticLoopHost`].
//!
//! Wraps CLI-specific concerns (tool executor, permission manager, selector,
//! skill registry, terminal rendering) behind the runtime trait so the
//! multi-turn loop runs in the runtime crate.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use async_trait::async_trait;
use crossterm::style::Stylize;
use mo_agent_runtime::{
    tool_registry::ToolRegistry,
    tool_selector::ToolSelector,
    turn::agentic_headless_round::HeadlessStderrStyle,
    turn::agentic_loop_host::{AgenticLoopHost, AgenticLoopState, HostTurnResult},
};
use serde_json::Value;

use crate::{
    ExplainMode, edge_tools::ToolExecutor, permission_manager::PermissionManager,
    skill_instructions::SharedSkillRegistry,
};

use super::agentic_loop_turn::{
    ChatTurnSseFetchRequest, PrepareTurnTelemetry, fetch_chat_turn_sse,
};

/// CLI host for the runtime agentic loop.
///
/// Holds all CLI-specific dependencies; the runtime loop calls `execute_turn()`
/// which delegates to the existing `fetch_chat_turn_sse` pipeline.
pub(crate) struct CliAgenticLoopHost<'a> {
    pub api: &'a mo_thin_client::ThinClient,
    pub token: &'a str,
    pub model: Option<&'a str>,
    pub explain: ExplainMode,
    pub render_md: bool,
    pub term_width: usize,
    pub quiet: bool,
    pub message: &'a str,
    pub history: &'a [(String, String)],
    pub recent_tools: &'a [String],
    pub project_root: PathBuf,
    pub executor: ToolExecutor,
    pub selector: &'a dyn ToolSelector,
    pub registry: ToolRegistry,
    pub all_schemas: Vec<Value>,
    pub skill_registry: &'a SharedSkillRegistry,
    pub file_context: Vec<String>,
    pub perm_manager: &'a mut PermissionManager,
    pub valid_tool_names: HashSet<String>,
}

#[async_trait]
impl AgenticLoopHost for CliAgenticLoopHost<'_> {
    async fn execute_turn(
        &mut self,
        state: &mut AgenticLoopState,
    ) -> Result<HostTurnResult, String> {
        let assembly_start = Instant::now();

        let turn_result = fetch_chat_turn_sse(ChatTurnSseFetchRequest {
            api: self.api,
            token: self.token,
            model: self.model,
            explain: self.explain,
            render_md: self.render_md,
            term_width: self.term_width,
            quiet: self.quiet,
            message: self.message,
            history: self.history,
            recent_tools: self.recent_tools,
            project_root: self.project_root.as_path(),
            executor: &mut self.executor,
            selector: self.selector,
            registry: &self.registry,
            messages: state.messages.as_slice(),
            current_session_id: state.current_session_id.as_deref(),
            tool_results: state.tool_results.as_slice(),
            all_schemas: &self.all_schemas,
            turn_guard: &state.turn_guard,
            restricted_tools: &mut state.restricted_tools,
            step_recorder: &mut state.step_recorder,
            skill_registry: self.skill_registry,
            file_context: &self.file_context,
            assembly_start,
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut state.first_memoria_ms,
                first_selector_ms: &mut state.first_selector_ms,
                first_selector_strategy: &mut state.first_selector_strategy,
                selector_tokens_in: &mut state.selector_tokens_in,
                selector_tokens_out: &mut state.selector_tokens_out,
                first_selection_report: &mut state.first_selection_report,
                first_budget_pressure: &mut state.first_budget_pressure,
                first_context_assembly_ms: &mut state.first_context_assembly_ms,
                all_selected_skills: &mut state.all_selected_skills,
            },
            perm_manager: self.perm_manager,
        })
        .await?;

        Ok(HostTurnResult {
            accum: turn_result.core,
            ttft_ms: turn_result.ttft_ms,
            edge_tool_round: turn_result.edge_tool_round,
        })
    }

    fn emit_headless_line(&mut self, style: HeadlessStderrStyle, line: String) {
        match style {
            HeadlessStderrStyle::Dim => eprintln!("{}", line.dim()),
            HeadlessStderrStyle::Red => eprintln!("{}", line.red()),
            HeadlessStderrStyle::Green => eprintln!("{}", line.green()),
            HeadlessStderrStyle::Yellow => eprintln!("{}", line.yellow()),
        }
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
            if self.valid_tool_names.insert(name.to_string()) {
                self.all_schemas.push(schema);
            }
        }
    }
}
