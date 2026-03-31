//! Owns all agentic `/chat/turn` SSE loop state: bootstrap, `run_all_turns`, post-loop sidecars, and `StreamResult` assembly.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use crossterm::style::Stylize;
use crossterm::terminal;
use mo_agent_core::RuntimeLimits;
use mo_agent_runtime::{
    pipeline::persistence::ToolHealthEntry,
    pipeline::step_protocol::{InMemoryIdempotencyCache, StepCheckpoint},
    pipeline::step_recorder::StepRecorder,
    semantic_dedup::SemanticDedup,
    tool_registry::{self, ToolRegistry},
    turn::agentic_turn_telemetry::{
        format_token_count_compact, session_id_footer_abbrev, step_recorder_chat_ephemeral_run_id,
    },
    turn::chat_history_openai::openai_messages_from_repl_history,
    turn::edge_prompt_context::detect_project_languages,
    turn::stall::CLI_AGENTIC_TURN_BUDGET_STALL_ABORT_MSG,
    turn::tool_health::ToolHealthTracker,
    turn::tool_schema_prune::openai_tool_names_from_schemas,
    turn::turn_guard::TurnGuard,
};
use mo_agent_services::session_journal::ToolCallRecord;
use serde_json::Value;

use crate::{ExplainMode, StreamResult, VerdictEvent, edge_tools};

use super::super::ChatTurnParams;
use super::super::explain_reports::{print_explain_report, print_verdict_report};
use super::agentic_loop_turn::{
    AgenticLoopTurnExit, AgenticTurnRequest, PrepareTurnTelemetry, run_agentic_loop_iteration,
};

pub(crate) struct AgenticSseLoopState {
    start: Instant,
    term_width: usize,
    project_root: PathBuf,
    file_context: Vec<String>,
    executor: edge_tools::ToolExecutor,
    all_schemas: Vec<Value>,
    registry: ToolRegistry,
    valid_tool_names: HashSet<String>,
    current_session_id: Option<String>,
    messages: Vec<Value>,
    tool_results: Vec<Value>,
    final_text: String,
    total_prompt: u64,
    total_completion: u64,
    total_tool_calls: u32,
    has_any_usage: bool,
    explain_turns: Vec<Value>,
    first_selection_report: Option<tool_registry::SelectionReport>,
    first_budget_pressure: f64,
    all_tools_used: HashSet<String>,
    turn_sigs: Vec<BTreeSet<String>>,
    turn_tool_names: Vec<HashSet<String>>,
    forced_factual_retry: bool,
    current_run_id: Option<String>,
    stall_events: Vec<(String, u32)>,
    verdict_events: Vec<VerdictEvent>,
    last_heavy_checkpoint: Option<StepCheckpoint>,
    tool_call_records: Vec<ToolCallRecord>,
    first_ttft_ms: Option<u64>,
    idempotency_cache: InMemoryIdempotencyCache,
    semantic_dedup: SemanticDedup,
    turn_guard: TurnGuard,
    restricted_tools: HashSet<String>,
    max_turns: usize,
    remaining_turns: usize,
    intent_tool_turns: Vec<(Vec<String>, String)>,
    step_recorder: StepRecorder,
    first_context_assembly_ms: Option<u64>,
    first_memoria_ms: Option<u64>,
    first_selector_ms: Option<u64>,
    first_selector_strategy: Option<String>,
    selector_tokens_in: u64,
    selector_tokens_out: u64,
    all_selected_skills: Vec<String>,
}

impl AgenticSseLoopState {
    pub(crate) fn new(p: &ChatTurnParams<'_>) -> Self {
        let start = Instant::now();
        let term_width = terminal::size().map(|(w, _)| w as usize).unwrap_or(80);
        let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let file_context = detect_project_languages(&project_root);
        let executor =
            edge_tools::ToolExecutor::new(&project_root).with_cloud(p.api.api_origin(), p.token);
        let all_schemas = edge_tools::all_tool_schemas();
        let registry = ToolRegistry::new(all_schemas.clone());
        let valid_tool_names = openai_tool_names_from_schemas(&all_schemas);

        let current_session_id = p.session_id.map(|s| s.to_string());
        let messages = openai_messages_from_repl_history(p.history, p.message);

        let turn_guard = if p.tool_health_entries.is_empty() {
            TurnGuard::new()
        } else {
            let health = ToolHealthTracker::from_entries(p.tool_health_entries);
            TurnGuard::with_health(health)
        };

        let max_turns = RuntimeLimits::global().max_turns;
        let step_recorder = StepRecorder::with_persistence(
            current_session_id.as_deref().unwrap_or("ephemeral"),
            step_recorder_chat_ephemeral_run_id(start.elapsed().as_millis()).as_str(),
        );

        Self {
            start,
            term_width,
            project_root,
            file_context,
            executor,
            all_schemas,
            registry,
            valid_tool_names,
            current_session_id,
            messages,
            tool_results: Vec::new(),
            final_text: String::new(),
            total_prompt: 0,
            total_completion: 0,
            total_tool_calls: 0,
            has_any_usage: false,
            explain_turns: Vec::new(),
            first_selection_report: None,
            first_budget_pressure: 0.0,
            all_tools_used: HashSet::new(),
            turn_sigs: Vec::new(),
            turn_tool_names: Vec::new(),
            forced_factual_retry: false,
            current_run_id: None,
            stall_events: Vec::new(),
            verdict_events: Vec::new(),
            last_heavy_checkpoint: None,
            tool_call_records: Vec::new(),
            first_ttft_ms: None,
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(
                mo_agent_runtime::semantic_dedup::DEFAULT_SIMILARITY_THRESHOLD,
            ),
            turn_guard,
            restricted_tools: HashSet::new(),
            max_turns,
            remaining_turns: max_turns,
            intent_tool_turns: Vec::new(),
            step_recorder,
            first_context_assembly_ms: None,
            first_memoria_ms: None,
            first_selector_ms: None,
            first_selector_strategy: None,
            selector_tokens_in: 0,
            selector_tokens_out: 0,
            all_selected_skills: Vec::new(),
        }
    }

    pub(crate) async fn run_all_turns(&mut self, p: &mut ChatTurnParams<'_>) -> Result<(), String> {
        for turn_index in 0..self.max_turns {
            if self.remaining_turns == 0 {
                return Err(CLI_AGENTIC_TURN_BUDGET_STALL_ABORT_MSG.to_string());
            }
            self.remaining_turns = self.remaining_turns.saturating_sub(1);
            self.step_recorder.begin_turn(turn_index as u32);

            match run_agentic_loop_iteration(AgenticTurnRequest {
                turn_index,
                max_turns: self.max_turns,
                api: p.api,
                token: p.token,
                model: p.model,
                explain: p.explain,
                render_md: p.render_md,
                term_width: self.term_width,
                quiet: p.quiet,
                message: p.message,
                history: p.history,
                recent_tools: p.recent_tools,
                project_root: self.project_root.as_path(),
                executor: &mut self.executor,
                selector: p.selector,
                registry: &self.registry,
                messages: &mut self.messages,
                current_session_id: &mut self.current_session_id,
                tool_results: &mut self.tool_results,
                all_schemas: &self.all_schemas,
                turn_guard: &mut self.turn_guard,
                restricted_tools: &mut self.restricted_tools,
                step_recorder: &mut self.step_recorder,
                skill_registry: p.skill_registry,
                file_context: &self.file_context,
                perm_manager: p.perm_manager,
                valid_tool_names: &self.valid_tool_names,
                idempotency_cache: &mut self.idempotency_cache,
                semantic_dedup: &mut self.semantic_dedup,
                turn_sigs: &mut self.turn_sigs,
                turn_tool_names: &mut self.turn_tool_names,
                stall_events: &mut self.stall_events,
                intent_tool_turns: &mut self.intent_tool_turns,
                verdict_events: &mut self.verdict_events,
                remaining_turns: &mut self.remaining_turns,
                last_heavy_checkpoint: &mut self.last_heavy_checkpoint,
                tool_call_records: &mut self.tool_call_records,
                first_ttft_ms: &mut self.first_ttft_ms,
                current_run_id: &mut self.current_run_id,
                final_text: &mut self.final_text,
                total_prompt: &mut self.total_prompt,
                total_completion: &mut self.total_completion,
                total_tool_calls: &mut self.total_tool_calls,
                all_tools_used: &mut self.all_tools_used,
                has_any_usage: &mut self.has_any_usage,
                forced_factual_retry: &mut self.forced_factual_retry,
                explain_turns: &mut self.explain_turns,
                telem: PrepareTurnTelemetry {
                    first_memoria_ms: &mut self.first_memoria_ms,
                    first_selector_ms: &mut self.first_selector_ms,
                    first_selector_strategy: &mut self.first_selector_strategy,
                    selector_tokens_in: &mut self.selector_tokens_in,
                    selector_tokens_out: &mut self.selector_tokens_out,
                    first_selection_report: &mut self.first_selection_report,
                    first_budget_pressure: &mut self.first_budget_pressure,
                    first_context_assembly_ms: &mut self.first_context_assembly_ms,
                    all_selected_skills: &mut self.all_selected_skills,
                },
            })
            .await
            {
                Ok(AgenticLoopTurnExit::BreakLoop) => break,
                Ok(AgenticLoopTurnExit::ContinueIterating) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub(crate) fn into_stream_result(self, p: &ChatTurnParams<'_>) -> StreamResult {
        eprint_stream_loop_sidecars(StreamLoopSidecarEprint {
            explain: p.explain,
            quiet: p.quiet,
            verbose_mode: p.verbose_mode,
            start: self.start,
            model: p.model,
            explain_turns: &self.explain_turns,
            verdict_events: &self.verdict_events,
            has_any_usage: self.has_any_usage,
            total_prompt: self.total_prompt,
            total_completion: self.total_completion,
            current_session_id: self.current_session_id.as_deref(),
        });

        build_stream_result(StreamResultBuild {
            tool_health_entries: p.tool_health_entries,
            session_id: self.current_session_id,
            run_id: self.current_run_id,
            full_text: self.final_text,
            prompt_tokens: self.total_prompt,
            completion_tokens: self.total_completion,
            tool_calls_count: self.total_tool_calls,
            first_selection_report: self.first_selection_report,
            selected_skills: self.all_selected_skills,
            tools_used: self.all_tools_used,
            tool_call_records: self.tool_call_records,
            budget_pressure: self.first_budget_pressure,
            stall_events: self.stall_events,
            verdict_events: self.verdict_events,
            step_recorder: &self.step_recorder,
            turn_guard: &self.turn_guard,
            last_heavy_checkpoint: self.last_heavy_checkpoint,
            ttft_ms: self.first_ttft_ms,
            context_ms: self.first_context_assembly_ms,
            selector_strategy: self.first_selector_strategy,
            selector_ms: self.first_selector_ms,
            selector_tokens_in: self.selector_tokens_in,
            selector_tokens_out: self.selector_tokens_out,
            memoria_ms: self.first_memoria_ms,
        })
    }
}

// ─── Post-loop CLI + `StreamResult` (was `stream_result_finalize.rs`) ────────

struct StreamLoopSidecarEprint<'a> {
    explain: ExplainMode,
    quiet: bool,
    verbose_mode: bool,
    start: Instant,
    model: Option<&'a str>,
    explain_turns: &'a [Value],
    verdict_events: &'a [VerdictEvent],
    has_any_usage: bool,
    total_prompt: u64,
    total_completion: u64,
    current_session_id: Option<&'a str>,
}

fn eprint_stream_loop_sidecars(ctx: StreamLoopSidecarEprint<'_>) {
    let StreamLoopSidecarEprint {
        explain,
        quiet,
        verbose_mode,
        start,
        model,
        explain_turns,
        verdict_events,
        has_any_usage,
        total_prompt,
        total_completion,
        current_session_id,
    } = ctx;

    if explain != ExplainMode::Off && !explain_turns.is_empty() && !quiet {
        print_explain_report(explain_turns, explain == ExplainMode::Verbose);
    }
    if explain != ExplainMode::Off && !verdict_events.is_empty() && !quiet {
        print_verdict_report(verdict_events, explain == ExplainMode::Verbose);
    }

    let elapsed = start.elapsed().as_secs_f64();
    let model_tag = model.unwrap_or("auto");
    let session_tag = session_id_footer_abbrev(current_session_id);
    if verbose_mode && !quiet {
        eprintln!(
            "{}",
            format!(
                "  ⏱ {:.1}s  ↓ {}  ↑ {}  model: {}  session: {}",
                elapsed,
                if has_any_usage {
                    format_token_count_compact(total_completion)
                } else {
                    "?".to_string()
                },
                if has_any_usage {
                    format_token_count_compact(total_prompt)
                } else {
                    "?".to_string()
                },
                model_tag,
                session_tag,
            )
            .dim()
        );
    }
}

struct StreamResultBuild<'a> {
    tool_health_entries: &'a [ToolHealthEntry],
    session_id: Option<String>,
    run_id: Option<String>,
    full_text: String,
    prompt_tokens: u64,
    completion_tokens: u64,
    tool_calls_count: u32,
    first_selection_report: Option<tool_registry::SelectionReport>,
    selected_skills: Vec<String>,
    tools_used: HashSet<String>,
    tool_call_records: Vec<ToolCallRecord>,
    budget_pressure: f64,
    stall_events: Vec<(String, u32)>,
    verdict_events: Vec<VerdictEvent>,
    step_recorder: &'a StepRecorder,
    turn_guard: &'a TurnGuard,
    last_heavy_checkpoint: Option<StepCheckpoint>,
    ttft_ms: Option<u64>,
    context_ms: Option<u64>,
    selector_strategy: Option<String>,
    selector_ms: Option<u64>,
    selector_tokens_in: u64,
    selector_tokens_out: u64,
    memoria_ms: Option<u64>,
}

fn build_stream_result(ctx: StreamResultBuild<'_>) -> StreamResult {
    let StreamResultBuild {
        tool_health_entries,
        session_id,
        run_id,
        full_text,
        prompt_tokens,
        completion_tokens,
        tool_calls_count,
        first_selection_report,
        selected_skills,
        tools_used,
        tool_call_records,
        budget_pressure,
        stall_events,
        verdict_events,
        step_recorder,
        turn_guard,
        last_heavy_checkpoint,
        ttft_ms,
        context_ms,
        selector_strategy,
        selector_ms,
        selector_tokens_in,
        selector_tokens_out,
        memoria_ms,
    } = ctx;

    let report = first_selection_report.unwrap_or_else(|| tool_registry::SelectionReport {
        tools_selected: Vec::new(),
        selected_count: 0,
        budget_used: 0,
        budget_total: 0,
    });

    let deduped_stall_events: Vec<(String, u32)> = {
        let mut seen = HashSet::new();
        stall_events
            .into_iter()
            .filter(|(stall_type, _)| seen.insert(stall_type.clone()))
            .map(|(stall_type, _)| (stall_type, 0))
            .collect()
    };

    let deduped_verdict_events: Vec<VerdictEvent> = {
        let mut seen = HashSet::new();
        verdict_events
            .into_iter()
            .filter(|ve| seen.insert(ve.severity.clone()))
            .map(|mut ve| {
                ve.turn = 0;
                ve
            })
            .collect()
    };

    StreamResult {
        session_id,
        run_id,
        full_text,
        prompt_tokens,
        completion_tokens,
        tool_calls_count,
        tools_selected: report.tools_selected,
        selected_skills,
        tools_used: tools_used.into_iter().collect(),
        tool_call_records,
        budget_used: report.budget_used,
        budget_pressure,
        stall_events: deduped_stall_events,
        verdict_events: deduped_verdict_events,
        step_recorder_summary: Some(step_recorder.summary()),
        tool_health_export: turn_guard.health.export_merged(tool_health_entries),
        last_heavy_checkpoint,
        ttft_ms,
        context_ms,
        selector_strategy,
        selector_ms,
        selector_tokens_in,
        selector_tokens_out,
        memoria_ms,
    }
}
