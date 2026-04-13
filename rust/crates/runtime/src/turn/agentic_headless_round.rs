//! Headless tool round after SSE ingest: OpenAI messages, cache, reflect hydrate, stderr lines.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use astra_core::agent_warn;
use astra_services::session_journal::ToolCallRecord;
use astra_thin_client::ThinClient;
use serde_json::{Map, Value};

use super::headless_tool_assembly::{
    CACHEABLE_TOOLS, EdgeToolRoundRow, HeadlessResolvedToolSlot, HeadlessRoundToolIdx,
    begin_headless_tool_round_opening_ext, headless_idempotency_hit_openai_pair,
    headless_openai_duplicate_within_turn_pair, headless_unknown_local_tool_openai_pair,
    openai_tool_roundtrip_values, openai_tool_roundtrip_values_with_result_fields,
    resolve_headless_tool_slot, take_edge_output_for_tool_call_with_duration,
    unknown_local_tool_error_message,
};
use super::headless_tool_body_preview::emit_headless_tool_body_preview;
use super::headless_tool_journal::{
    journal_record_blocked_tool, journal_record_cross_turn_cache_hit,
    journal_record_duplicate_within_turn, journal_record_executed_tool_call,
    journal_record_unknown_tool,
};
use super::headless_tool_postprocess::{
    HeadlessCacheableRecordCtx, HeadlessOutputEnrichSignal, HeadlessStepDeadline,
    append_headless_result_quality_feedback, enrich_headless_tool_output_for_errors_and_limits,
    format_headless_tool_duration, record_headless_cacheable_success_and_semantic_hint,
    try_write_light_headless_step_checkpoint,
};
use super::headless_tool_status_display::{tool_call_detail, tool_result_summary};
use super::headless_tool_stderr_lines::{
    headless_stderr_cache_hit_line, headless_stderr_error_preview_line,
    headless_stderr_resource_limit_blocked, headless_stderr_resource_limit_in_output,
    headless_stderr_tool_error_detail_line, headless_stderr_tool_error_line,
    headless_stderr_tool_ok_line, headless_stderr_unknown_tool_detail,
    headless_stderr_unknown_tool_header,
};
use super::hydrate_reflect::hydrate_reflect_placeholder_if_needed;
use super::tool_result_sanitize::tool_result_content_for_model;
use super::tool_result_semantics::{is_tool_error, tool_dedup_signature};
use super::turn_guard::TurnGuard;
use crate::pipeline::step_protocol::{IdempotencyKey, InMemoryIdempotencyCache};
use crate::pipeline::step_recorder::StepRecorder;
use crate::semantic_dedup::SemanticDedup;
use crate::turn::edge_prompt_context::make_args_preview;

/// Terminal styling for one stderr line (host maps to crossterm etc.).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadlessStderrStyle {
    Dim,
    Red,
    Green,
    Yellow,
    /// File / `diff --git` headers (terminal preview).
    CyanBold,
    Magenta,
    /// Unified diff `+` line (not `+++`).
    DiffAdd,
    /// Unified diff `-` line (not `---`).
    DiffRemove,
    /// Unified diff context (` `) and `\ No newline…` meta lines.
    DiffContext,
    /// Read file body / neutral code line.
    Normal,
}

/// Host sink for headless tool round stderr (noop when CLI passes [`NoopHeadlessTerminal`]).
pub trait HeadlessRoundTerminal: Send {
    fn emit_line(&mut self, style: HeadlessStderrStyle, line: String);
}

/// No-op implementation (e.g. `--quiet`).
pub struct NoopHeadlessTerminal;

impl HeadlessRoundTerminal for NoopHeadlessTerminal {
    fn emit_line(&mut self, _: HeadlessStderrStyle, _: String) {}
}

type PermissionSyncHandle = std::sync::Arc<
    tokio::sync::RwLock<crate::orchestration::permission_sync::PermissionSyncContext>,
>;

/// Typed execution context for one headless tool round.
pub struct HeadlessToolRoundCtx<'a, E: EdgeToolRoundRow> {
    pub turn_index: usize,
    pub quiet: bool,
    pub api: &'a ThinClient,
    pub token: &'a str,
    pub current_session_id: Option<&'a String>,
    pub tool_calls: &'a [Value],
    pub edge_tool_round: &'a [E],
    pub reasoning_content: &'a str,
    pub edge_callback_outputs: &'a HashMap<String, String>,
    pub messages: &'a mut Vec<Value>,
    pub tool_results: &'a mut Vec<Value>,
    pub valid_tool_names: &'a HashSet<String>,
    pub restricted_tools: &'a mut HashSet<String>,
    pub turn_guard: &'a mut TurnGuard,
    pub step_recorder: &'a mut StepRecorder,
    pub idempotency_cache: &'a mut InMemoryIdempotencyCache,
    pub semantic_dedup: &'a mut SemanticDedup,
    pub call_counts: &'a mut HashMap<String, u32>,
    pub max_identical_calls: u32,
    pub max_tools_per_turn: u32,
    pub tool_call_records: &'a mut Vec<ToolCallRecord>,
    pub tool_event_hooks: &'a crate::skills::hooks::ToolEventHookRegistry,
    pub term: &'a mut dyn HeadlessRoundTerminal,
    pub mailbox: Option<&'a mut crate::messaging::router::AgentMailbox>,
    pub permission_context: Option<&'a PermissionSyncHandle>,
    pub progress_emitter: Option<&'a crate::orchestration::AgentProgressEmitter>,
    /// Tool results resolved by upstream interception layers (skill, send_message)
    /// before the headless round. Injected immediately after the assistant message
    /// to maintain correct ordering: assistant(tool_calls) → tool(pre_resolved) → tool(executed).
    pub pre_resolved_results: &'a [(String, String)],
}

struct HeadlessResolvedExecution {
    id: String,
    name: String,
    args: Value,
    result_str: String,
    tool_result_fields: Option<Map<String, Value>>,
    edge_duration_ms: u64,
    is_edge_tool: bool,
    early_exit_ms: u64,
}

struct HeadlessBlockedTool<'a> {
    id: &'a str,
    name: &'a str,
    args: &'a Value,
    err_msg: String,
    journal_reason: String,
    early_exit_ms: u64,
    status_line: Option<String>,
}

struct HeadlessPreparedRound<'a> {
    effective_permission_timeout: Duration,
    tool_calls: std::borrow::Cow<'a, [Value]>,
    pre_resolved_ids: HashSet<String>,
    indices: Vec<super::headless_tool_assembly::HeadlessRoundToolIdx>,
    step_deadline: HeadlessStepDeadline,
    consumed_edge: Vec<bool>,
}

async fn prepare_headless_tool_round<'a, E: EdgeToolRoundRow>(
    permission_context: Option<&PermissionSyncHandle>,
    tool_calls: &'a [Value],
    edge_tool_round: &'a [E],
    reasoning_content: &str,
    pre_resolved_results: &[(String, String)],
    messages: &mut Vec<Value>,
    tool_results: &mut Vec<Value>,
    step_recorder: &mut StepRecorder,
) -> HeadlessPreparedRound<'a> {
    const PERMISSION_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
    const PERMISSION_REQUEST_TIMEOUT_BACKGROUND: Duration = Duration::from_secs(5);

    let effective_permission_timeout = if let Some(ctx) = permission_context {
        let guard = ctx.read().await;
        if guard.inherited.is_background {
            PERMISSION_REQUEST_TIMEOUT_BACKGROUND
        } else {
            PERMISSION_REQUEST_TIMEOUT
        }
    } else {
        PERMISSION_REQUEST_TIMEOUT
    };

    tool_results.clear();

    let force_reasoning =
        !reasoning_content.is_empty() || super::edge_ledger::history_has_reasoning(messages);
    let tool_calls = super::headless_tool_assembly::ensure_tool_call_ids(tool_calls);

    let opening = begin_headless_tool_round_opening_ext(
        &tool_calls,
        edge_tool_round,
        reasoning_content,
        force_reasoning,
    );
    messages.push(opening.assistant_message);

    let mut pre_resolved_ids = HashSet::new();
    for (call_id, result_text) in pre_resolved_results {
        pre_resolved_ids.insert(call_id.clone());
        let content_for_model = tool_result_content_for_model("pre_resolved", result_text);
        let (tool_msg, tr) =
            openai_tool_roundtrip_values(call_id, "pre_resolved", &content_for_model);
        messages.push(tool_msg);
        tool_results.push(tr);
    }

    step_recorder.begin_act(opening.tool_count);
    let step_deadline =
        HeadlessStepDeadline::from_scheduling_timeout_ms(step_recorder.scheduling().timeout_ms);

    HeadlessPreparedRound {
        effective_permission_timeout,
        tool_calls,
        pre_resolved_ids,
        indices: opening.indices,
        step_deadline,
        consumed_edge: vec![false; edge_tool_round.len()],
    }
}

fn resolve_headless_tool_execution<E: EdgeToolRoundRow>(
    slot: HeadlessResolvedToolSlot,
    edge_tool_round: &[E],
    consumed_edge: &mut [bool],
    by_sig: &HashMap<String, String>,
) -> HeadlessResolvedExecution {
    let HeadlessResolvedToolSlot {
        id,
        name,
        args,
        synthetic_edge_index,
    } = slot;
    let consumed_before = consumed_edge.iter().filter(|&&c| c).count();

    let (result_str, edge_duration_ms, tool_result_fields) = if let Some(i) = synthetic_edge_index {
        (
            edge_tool_round[i].tool_output().to_string(),
            edge_tool_round[i].tool_duration_ms(),
            edge_tool_round[i].tool_result_fields().cloned(),
        )
    } else {
        let matched = take_edge_output_for_tool_call_with_duration(
            &name,
            &args,
            edge_tool_round,
            consumed_edge,
            by_sig,
        );
        (
            matched.output,
            matched.duration_ms,
            matched.tool_result_fields,
        )
    };

    let consumed_after = consumed_edge.iter().filter(|&&c| c).count();
    let is_edge_tool = synthetic_edge_index.is_some() || consumed_after > consumed_before;
    let early_exit_ms = if is_edge_tool && edge_duration_ms > 0 {
        edge_duration_ms
    } else {
        0
    };

    HeadlessResolvedExecution {
        id,
        name,
        args,
        result_str,
        tool_result_fields,
        edge_duration_ms,
        is_edge_tool,
        early_exit_ms,
    }
}

fn emit_blocked_tool_result(
    blocked: HeadlessBlockedTool<'_>,
    quiet: bool,
    term: &mut dyn HeadlessRoundTerminal,
    messages: &mut Vec<Value>,
    tool_results: &mut Vec<Value>,
    tool_call_records: &mut Vec<ToolCallRecord>,
) {
    if !quiet && let Some(status_line) = blocked.status_line {
        term.emit_line(HeadlessStderrStyle::Yellow, status_line);
    }
    let (tool_msg, err_tr) =
        openai_tool_roundtrip_values(blocked.id, blocked.name, &blocked.err_msg);
    messages.push(tool_msg);
    tool_results.push(err_tr);
    tool_call_records.push(journal_record_blocked_tool(
        blocked.name.to_string(),
        blocked.journal_reason,
        make_args_preview(blocked.name, blocked.args),
        blocked.early_exit_ms,
    ));
}

fn emit_tool_display_feedback(
    quiet: bool,
    term: &mut dyn HeadlessRoundTerminal,
    name: &str,
    args: &Value,
    result_str: &str,
    is_err: bool,
    is_edge_tool: bool,
    executed_ms: u64,
) {
    if !quiet && !is_edge_tool {
        let duration_str = format_headless_tool_duration(Duration::from_millis(executed_ms));
        let detail = tool_call_detail(name, args);
        let summary = if !is_err {
            tool_result_summary(name, result_str)
        } else {
            None
        };
        if is_err {
            term.emit_line(
                HeadlessStderrStyle::Red,
                headless_stderr_tool_error_line(name, &duration_str, detail.as_deref()),
            );
            if let Some(first_line) = result_str.lines().next() {
                let preview = headless_stderr_error_preview_line(first_line, 100);
                term.emit_line(
                    HeadlessStderrStyle::Dim,
                    headless_stderr_tool_error_detail_line(&preview),
                );
            }
        } else {
            term.emit_line(
                HeadlessStderrStyle::Green,
                headless_stderr_tool_ok_line(
                    name,
                    &duration_str,
                    detail.as_deref(),
                    summary.as_deref(),
                ),
            );
        }
    }

    if !is_edge_tool {
        emit_headless_tool_body_preview(term, quiet, name, result_str, is_err);
    }
}

fn maybe_persist_model_tool_result(
    current_session_id: Option<&String>,
    id: &str,
    name: &str,
    model_result_str: String,
) -> String {
    if let Some(sid) = current_session_id {
        let session_dir = astra_services::session_journal::local_sessions_dir().join(sid);
        match super::tool_result_storage::maybe_persist_tool_result(
            &session_dir,
            id,
            name,
            &model_result_str,
        ) {
            Some(replacement) => replacement,
            None => model_result_str,
        }
    } else {
        model_result_str
    }
}

enum HeadlessToolSlotControl {
    Continue,
    AbortRound,
}

enum HeadlessPipelineStage<T> {
    Continue(T),
    ShortCircuit,
    AbortRound,
}

struct HeadlessPreparedExecution {
    execution: HeadlessResolvedExecution,
    idem_key: IdempotencyKey,
}

struct HeadlessExecutedExecution {
    execution: HeadlessResolvedExecution,
    idem_key: IdempotencyKey,
    is_err: bool,
    executed_ms: u64,
}

struct HeadlessToolExecutionCtx<'a, E: EdgeToolRoundRow> {
    turn_index: usize,
    quiet: bool,
    api: &'a ThinClient,
    token: &'a str,
    current_session_id: Option<&'a String>,
    tool_calls: &'a [Value],
    edge_tool_round: &'a [E],
    by_sig: &'a HashMap<String, String>,
    pre_resolved_ids: &'a HashSet<String>,
    messages: &'a mut Vec<Value>,
    tool_results: &'a mut Vec<Value>,
    valid_tool_names: &'a HashSet<String>,
    restricted_tools: &'a mut HashSet<String>,
    turn_guard: &'a mut TurnGuard,
    step_recorder: &'a mut StepRecorder,
    idempotency_cache: &'a mut InMemoryIdempotencyCache,
    semantic_dedup: &'a mut SemanticDedup,
    call_counts: &'a mut HashMap<String, u32>,
    max_identical_calls: u32,
    max_tools_per_turn: u32,
    tool_call_records: &'a mut Vec<ToolCallRecord>,
    tool_event_hooks: &'a crate::skills::hooks::ToolEventHookRegistry,
    term: &'a mut dyn HeadlessRoundTerminal,
    mailbox: Option<&'a mut crate::messaging::router::AgentMailbox>,
    permission_context: Option<&'a PermissionSyncHandle>,
    progress_emitter: Option<&'a crate::orchestration::AgentProgressEmitter>,
    effective_permission_timeout: Duration,
}

struct HeadlessToolExecutionPipeline<'a, E: EdgeToolRoundRow> {
    ctx: HeadlessToolExecutionCtx<'a, E>,
    consumed_edge: Vec<bool>,
    consecutive_empty_name: u32,
    executed_this_turn: u32,
}

impl<'a, E: EdgeToolRoundRow> HeadlessToolExecutionPipeline<'a, E> {
    /// After this many consecutive empty-name tool calls in one headless round,
    /// stop processing — the model is stuck emitting malformed calls.
    const MAX_CONSECUTIVE_EMPTY_NAME: u32 = 3;

    fn new(ctx: HeadlessToolExecutionCtx<'a, E>, consumed_edge: Vec<bool>) -> Self {
        Self {
            ctx,
            consumed_edge,
            consecutive_empty_name: 0,
            executed_this_turn: 0,
        }
    }

    fn tool_results_len(&self) -> usize {
        self.ctx.tool_results.len()
    }

    fn tool_calls(&self) -> &[Value] {
        self.ctx.tool_calls
    }

    fn edge_tool_name(&self, i: usize) -> String {
        self.ctx.edge_tool_round[i].tool_name().to_string()
    }

    fn scheduling_timeout_ms(&self) -> u64 {
        self.ctx.step_recorder.scheduling().timeout_ms
    }

    fn record_step_abort(&mut self, aborted_tools: &[String]) {
        self.ctx.turn_guard.record_step_abort(aborted_tools);
    }

    fn resolve_slot(&self, item: HeadlessRoundToolIdx) -> HeadlessResolvedToolSlot {
        resolve_headless_tool_slot(item, self.ctx.tool_calls, |i| {
            let edge = &self.ctx.edge_tool_round[i];
            (edge.tool_name().to_string(), edge.tool_args().clone())
        })
    }

    fn emit_turn_budget_stub(&mut self, slot: &HeadlessResolvedToolSlot) {
        let body = format!(
            "⛔ Per-turn tool budget exhausted ({max_tools_per_turn} tools). \
             Skipping this call. Prioritize the most important remaining \
             tools in your next response — do not repeat all skipped calls.",
            max_tools_per_turn = self.ctx.max_tools_per_turn,
        );
        let (tool_msg, tr) = headless_idempotency_hit_openai_pair(&slot.id, &slot.name, &body);
        self.ctx.messages.push(tool_msg);
        self.ctx.tool_results.push(tr);
    }

    fn handle_empty_tool_name(
        &mut self,
        item: HeadlessRoundToolIdx,
        slot: &HeadlessResolvedToolSlot,
    ) -> HeadlessToolSlotControl {
        self.consecutive_empty_name = self.consecutive_empty_name.saturating_add(1);
        let raw_tc = match item {
            HeadlessRoundToolIdx::ServerToolCall(i) => {
                self.ctx.tool_calls.get(i).map(|v| v.to_string())
            }
            _ => None,
        };
        agent_warn!(
            "step",
            "Empty tool name in slot {item:?} (id={}), raw tool_call: {}",
            slot.id,
            raw_tc.as_deref().unwrap_or("(synthetic edge)")
        );
        let err_msg = unknown_local_tool_error_message(&slot.name, self.ctx.valid_tool_names);
        if !self.ctx.quiet {
            self.ctx.term.emit_line(
                HeadlessStderrStyle::Red,
                headless_stderr_unknown_tool_header(&slot.name),
            );
            self.ctx.term.emit_line(
                HeadlessStderrStyle::Dim,
                headless_stderr_unknown_tool_detail(&err_msg),
            );
        }
        let (tool_msg, err_tr) = headless_unknown_local_tool_openai_pair(
            &slot.id,
            &slot.name,
            self.ctx.valid_tool_names,
        );
        self.ctx.messages.push(tool_msg);
        self.ctx.tool_results.push(err_tr);
        self.ctx
            .tool_call_records
            .push(journal_record_unknown_tool(slot.name.clone(), 0));
        if self.consecutive_empty_name >= Self::MAX_CONSECUTIVE_EMPTY_NAME {
            agent_warn!(
                "step",
                "Aborting headless tool round after {} consecutive empty-name tool calls",
                self.consecutive_empty_name
            );
            HeadlessToolSlotControl::AbortRound
        } else {
            HeadlessToolSlotControl::Continue
        }
    }

    fn validate_slot(
        &mut self,
        item: HeadlessRoundToolIdx,
    ) -> HeadlessPipelineStage<HeadlessPreparedExecution> {
        if self.executed_this_turn >= self.ctx.max_tools_per_turn {
            let slot = self.resolve_slot(item);
            self.emit_turn_budget_stub(&slot);
            return HeadlessPipelineStage::ShortCircuit;
        }

        let slot = self.resolve_slot(item);

        if self.ctx.pre_resolved_ids.contains(slot.id.as_str()) {
            return HeadlessPipelineStage::ShortCircuit;
        }

        if slot.name.is_empty() {
            return match self.handle_empty_tool_name(item, &slot) {
                HeadlessToolSlotControl::Continue => HeadlessPipelineStage::ShortCircuit,
                HeadlessToolSlotControl::AbortRound => HeadlessPipelineStage::AbortRound,
            };
        }
        self.consecutive_empty_name = 0;

        let call_sig = tool_dedup_signature(&slot.name, &slot.args);
        let count = self.ctx.call_counts.entry(call_sig).or_insert(0);
        *count += 1;
        if *count > self.ctx.max_identical_calls {
            let idem_key = IdempotencyKey::semantic(&slot.name, &slot.args);
            if let Some(_cached) = self.ctx.idempotency_cache.check(&idem_key) {
                let body = format!(
                    "⛔ Cached repeat (call #{} for identical args, limit: {}). \
                     The result is already in this conversation from an earlier call. \
                     Do NOT call this tool again with the same arguments.",
                    *count, self.ctx.max_identical_calls
                );
                let (tool_msg, tr) =
                    headless_idempotency_hit_openai_pair(&slot.id, &slot.name, &body);
                self.ctx.messages.push(tool_msg);
                self.ctx.tool_results.push(tr);
            } else {
                let (tool_msg, tr) =
                    headless_openai_duplicate_within_turn_pair(&slot.id, &slot.name);
                self.ctx.messages.push(tool_msg);
                self.ctx.tool_results.push(tr);
            }
            self.ctx
                .tool_call_records
                .push(journal_record_duplicate_within_turn(
                    slot.name.clone(),
                    make_args_preview(&slot.name, &slot.args),
                ));
            self.ctx.turn_guard.health.record_cache_hit(&slot.name);
            agent_warn!(
                "dedup",
                "Hard cap: tool '{}' (id={}) call #{} (limit: {})",
                slot.name,
                slot.id,
                *count,
                self.ctx.max_identical_calls
            );
            return HeadlessPipelineStage::ShortCircuit;
        }

        let idem_key = IdempotencyKey::semantic(&slot.name, &slot.args);
        if CACHEABLE_TOOLS.contains(&slot.name.as_str())
            && let Some(cached) = self.ctx.idempotency_cache.check(&idem_key)
        {
            if !self.ctx.quiet {
                self.ctx.term.emit_line(
                    HeadlessStderrStyle::Dim,
                    headless_stderr_cache_hit_line(&slot.name),
                );
                emit_headless_tool_body_preview(
                    self.ctx.term,
                    self.ctx.quiet,
                    &slot.name,
                    &cached.output,
                    false,
                );
            }
            let (tool_msg, tr) =
                headless_idempotency_hit_openai_pair(&slot.id, &slot.name, &cached.output);
            self.ctx.messages.push(tool_msg);
            self.ctx.tool_results.push(tr);
            let cache_key = idem_key.cache_key();
            self.ctx
                .step_recorder
                .begin_tool_with_key(&slot.name, &slot.id, Some(&cache_key));
            self.ctx
                .step_recorder
                .record_cache_hit(&slot.name, cached.clone());
            self.ctx.turn_guard.record_cache_hit(&slot.name);
            self.ctx
                .tool_call_records
                .push(journal_record_cross_turn_cache_hit(
                    slot.name.clone(),
                    cached.output.len() as u32,
                    make_args_preview(&slot.name, &slot.args),
                ));
            return HeadlessPipelineStage::ShortCircuit;
        }

        if CACHEABLE_TOOLS.contains(&slot.name.as_str())
            && let Some((prev_turn, cached_output)) =
                self.ctx
                    .semantic_dedup
                    .pre_check_block(&slot.name, &slot.args, self.ctx.turn_index)
        {
            let body = format!(
                "{cached_output}\n\n⛔ BLOCKED DUPLICATE: This {} call is semantically \
                 identical to turn {} — same tool with equivalent arguments. \
                 Execution was skipped. Use the result above instead of calling again.",
                slot.name,
                prev_turn + 1,
            );
            let (tool_msg, tr) = headless_idempotency_hit_openai_pair(&slot.id, &slot.name, &body);
            self.ctx.messages.push(tool_msg);
            self.ctx.tool_results.push(tr);
            self.ctx.turn_guard.health.record_cache_hit(&slot.name);
            self.ctx
                .tool_call_records
                .push(journal_record_cross_turn_cache_hit(
                    slot.name.clone(),
                    cached_output.len() as u32,
                    make_args_preview(&slot.name, &slot.args),
                ));
            agent_warn!(
                "dedup",
                "Semantic block: tool '{}' (id={}) matches turn {} via param-aware dedup",
                slot.name,
                slot.id,
                prev_turn + 1,
            );
            return HeadlessPipelineStage::ShortCircuit;
        }

        let execution = resolve_headless_tool_execution(
            slot,
            self.ctx.edge_tool_round,
            &mut self.consumed_edge,
            self.ctx.by_sig,
        );

        if !self.ctx.valid_tool_names.contains(&execution.name) {
            let err_msg =
                unknown_local_tool_error_message(&execution.name, self.ctx.valid_tool_names);
            if !self.ctx.quiet {
                self.ctx.term.emit_line(
                    HeadlessStderrStyle::Red,
                    headless_stderr_unknown_tool_header(&execution.name),
                );
                self.ctx.term.emit_line(
                    HeadlessStderrStyle::Dim,
                    headless_stderr_unknown_tool_detail(&err_msg),
                );
            }
            let (tool_msg, err_tr) = headless_unknown_local_tool_openai_pair(
                &execution.id,
                &execution.name,
                self.ctx.valid_tool_names,
            );
            self.ctx.messages.push(tool_msg);
            self.ctx.tool_results.push(err_tr);
            self.ctx.tool_call_records.push(journal_record_unknown_tool(
                execution.name.clone(),
                execution.early_exit_ms,
            ));
            return HeadlessPipelineStage::ShortCircuit;
        }

        HeadlessPipelineStage::Continue(HeadlessPreparedExecution {
            execution,
            idem_key,
        })
    }

    async fn permit_execution(
        &mut self,
        execution: &mut HeadlessResolvedExecution,
    ) -> HeadlessPipelineStage<()> {
        if self.ctx.restricted_tools.contains(&execution.name) {
            let err_msg = format!(
                "Tool '{}' is currently restricted and cannot be executed. \
                 Use only the tools whose schemas were provided.",
                execution.name
            );
            emit_blocked_tool_result(
                HeadlessBlockedTool {
                    id: &execution.id,
                    name: &execution.name,
                    args: &execution.args,
                    journal_reason: err_msg.clone(),
                    err_msg,
                    early_exit_ms: execution.early_exit_ms,
                    status_line: Some(format!("  ⚠ Blocked restricted tool: {}", execution.name)),
                },
                self.ctx.quiet,
                self.ctx.term,
                self.ctx.messages,
                self.ctx.tool_results,
                self.ctx.tool_call_records,
            );
            return HeadlessPipelineStage::ShortCircuit;
        }

        let args_str = serde_json::to_string(&execution.args).ok();
        let permission_context = self.ctx.permission_context;
        let effective_permission_timeout = self.ctx.effective_permission_timeout;
        let mailbox = self.ctx.mailbox.as_deref_mut();
        match super::permission_gate::check_tool_permission(
            &execution.name,
            args_str.as_deref(),
            permission_context,
            mailbox,
            effective_permission_timeout,
        )
        .await
        {
            super::permission_gate::PermissionCheckResult::Allowed => {}
            super::permission_gate::PermissionCheckResult::AllowedViaRequest { .. } => {
                if !self.ctx.quiet {
                    self.ctx.term.emit_line(
                        HeadlessStderrStyle::Yellow,
                        format!("  🔓 Permission granted by parent: {}", execution.name),
                    );
                }
            }
            super::permission_gate::PermissionCheckResult::Denied { reason } => {
                let err_msg = super::permission_gate::permission_denied_error_result(
                    &execution.name,
                    &reason,
                );
                emit_blocked_tool_result(
                    HeadlessBlockedTool {
                        id: &execution.id,
                        name: &execution.name,
                        args: &execution.args,
                        err_msg,
                        journal_reason: reason,
                        early_exit_ms: execution.early_exit_ms,
                        status_line: Some(format!("  🔒 Permission denied: {}", execution.name)),
                    },
                    self.ctx.quiet,
                    self.ctx.term,
                    self.ctx.messages,
                    self.ctx.tool_results,
                    self.ctx.tool_call_records,
                );
                return HeadlessPipelineStage::ShortCircuit;
            }
        }

        if !self.ctx.tool_event_hooks.is_empty() {
            let decision = crate::skills::hooks::evaluate_pre_tool_hooks(
                self.ctx.tool_event_hooks,
                &execution.name,
                &execution.args,
            )
            .await;
            match decision {
                crate::skills::hooks::PreToolDecision::Block(reason) => {
                    let err_msg = format!(
                        "Tool '{}' blocked by PreToolUse hook: {}",
                        execution.name, reason
                    );
                    emit_blocked_tool_result(
                        HeadlessBlockedTool {
                            id: &execution.id,
                            name: &execution.name,
                            args: &execution.args,
                            journal_reason: err_msg.clone(),
                            err_msg,
                            early_exit_ms: execution.early_exit_ms,
                            status_line: Some(format!(
                                "  ⚠ Hook blocked: {} — {}",
                                execution.name, reason
                            )),
                        },
                        self.ctx.quiet,
                        self.ctx.term,
                        self.ctx.messages,
                        self.ctx.tool_results,
                        self.ctx.tool_call_records,
                    );
                    return HeadlessPipelineStage::ShortCircuit;
                }
                crate::skills::hooks::PreToolDecision::AllowWithContext(ctx) => {
                    execution.result_str =
                        format!("{}\n\n[Hook context]: {ctx}", execution.result_str);
                }
                crate::skills::hooks::PreToolDecision::Allow => {}
            }
        }

        HeadlessPipelineStage::Continue(())
    }

    async fn execute_execution(
        &mut self,
        prepared: HeadlessPreparedExecution,
    ) -> HeadlessExecutedExecution {
        let HeadlessPreparedExecution {
            mut execution,
            idem_key,
        } = prepared;
        execution.result_str = hydrate_reflect_placeholder_if_needed(
            self.ctx.api,
            self.ctx.token,
            self.ctx.current_session_id,
            &execution.name,
            &execution.args,
            execution.result_str,
        )
        .await;

        let tool_start = Instant::now();
        let tool_idem_key = if CACHEABLE_TOOLS.contains(&execution.name.as_str()) {
            Some(idem_key.cache_key())
        } else {
            None
        };
        self.ctx.step_recorder.begin_tool_with_key(
            &execution.name,
            &execution.id,
            tool_idem_key.as_deref(),
        );

        if let Some(emitter) = self.ctx.progress_emitter {
            emitter.tool_executing(&execution.name, self.ctx.turn_index as u32);
        }

        let mut is_err = is_tool_error(&execution.result_str);
        let tool_already_restricted = self.ctx.restricted_tools.contains(&execution.name);
        let quiet = self.ctx.quiet;
        let term = &mut self.ctx.term;
        let resource_limit_recorded = enrich_headless_tool_output_for_errors_and_limits(
            &execution.name,
            &mut execution.result_str,
            &mut is_err,
            tool_already_restricted,
            self.ctx.turn_guard,
            self.ctx.restricted_tools,
            |sig| {
                if quiet {
                    return;
                }
                match sig {
                    HeadlessOutputEnrichSignal::ResourceLimitBlocked { tool } => {
                        term.emit_line(
                            HeadlessStderrStyle::Yellow,
                            headless_stderr_resource_limit_blocked(&tool),
                        );
                    }
                    HeadlessOutputEnrichSignal::ResourceLimitDetectedInOutput { tool } => {
                        term.emit_line(
                            HeadlessStderrStyle::Dim,
                            headless_stderr_resource_limit_in_output(&tool),
                        );
                    }
                }
            },
        );
        let _result_quality = append_headless_result_quality_feedback(
            &execution.name,
            &mut execution.result_str,
            resource_limit_recorded,
            self.ctx.turn_guard,
        );

        let executed_ms = if execution.is_edge_tool && execution.edge_duration_ms > 0 {
            execution.edge_duration_ms
        } else {
            tool_start.elapsed().as_millis() as u64
        };

        HeadlessExecutedExecution {
            execution,
            idem_key,
            is_err,
            executed_ms,
        }
    }

    async fn record_execution(&mut self, executed: HeadlessExecutedExecution) {
        let HeadlessExecutedExecution {
            mut execution,
            idem_key,
            is_err,
            executed_ms,
        } = executed;
        let args_size = serde_json::to_string(&execution.args)
            .map(|s| s.len() as u32)
            .unwrap_or(0);
        let args_preview = make_args_preview(&execution.name, &execution.args);
        self.ctx
            .tool_call_records
            .push(journal_record_executed_tool_call(
                execution.name.clone(),
                is_err,
                executed_ms,
                args_size,
                execution.result_str.as_str(),
                args_preview,
            ));
        self.ctx.step_recorder.complete_tool_with_result(
            &execution.name,
            is_err,
            executed_ms,
            false,
            &execution.result_str,
        );
        self.executed_this_turn += 1;

        if let Some(sid) = self.ctx.current_session_id {
            try_write_light_headless_step_checkpoint(sid, self.ctx.step_recorder);
        }

        if !is_err && CACHEABLE_TOOLS.contains(&execution.name.as_str()) {
            record_headless_cacheable_success_and_semantic_hint(
                &execution.name,
                &execution.args,
                &idem_key,
                HeadlessCacheableRecordCtx {
                    result_str: &mut execution.result_str,
                    turn_index: self.ctx.turn_index,
                    idempotency_cache: self.ctx.idempotency_cache,
                    step_recorder: self.ctx.step_recorder,
                    semantic_dedup: self.ctx.semantic_dedup,
                },
            );
        }

        emit_tool_display_feedback(
            self.ctx.quiet,
            self.ctx.term,
            &execution.name,
            &execution.args,
            &execution.result_str,
            is_err,
            execution.is_edge_tool,
            executed_ms,
        );

        if !self.ctx.tool_event_hooks.is_empty() && !is_err {
            if let Some(modified) = crate::skills::hooks::evaluate_post_tool_hooks(
                self.ctx.tool_event_hooks,
                &execution.name,
                &execution.args,
                &execution.result_str,
            )
            .await
            {
                execution.result_str = modified;
            }
        }

        let model_result_str =
            tool_result_content_for_model(&execution.name, &execution.result_str);
        let model_result_str = maybe_persist_model_tool_result(
            self.ctx.current_session_id,
            &execution.id,
            &execution.name,
            model_result_str,
        );

        let (tool_msg, tr) = openai_tool_roundtrip_values_with_result_fields(
            &execution.id,
            &execution.name,
            &model_result_str,
            execution.tool_result_fields.as_ref(),
        );
        self.ctx.messages.push(tool_msg);
        self.ctx.tool_results.push(tr);
    }

    async fn run_slot(&mut self, item: HeadlessRoundToolIdx) -> HeadlessToolSlotControl {
        let mut prepared = match self.validate_slot(item) {
            HeadlessPipelineStage::Continue(prepared) => prepared,
            HeadlessPipelineStage::ShortCircuit => return HeadlessToolSlotControl::Continue,
            HeadlessPipelineStage::AbortRound => return HeadlessToolSlotControl::AbortRound,
        };

        match self.permit_execution(&mut prepared.execution).await {
            HeadlessPipelineStage::Continue(()) => {}
            HeadlessPipelineStage::ShortCircuit => return HeadlessToolSlotControl::Continue,
            HeadlessPipelineStage::AbortRound => return HeadlessToolSlotControl::AbortRound,
        }

        let executed = self.execute_execution(prepared).await;
        self.record_execution(executed).await;
        HeadlessToolSlotControl::Continue
    }
}

/// Clears `tool_results`, appends the assistant tool-call message, then fills `tool_results` and
/// matching `tool` OpenAI messages for the next `/chat` request.
pub async fn run_agentic_headless_tool_round<E: EdgeToolRoundRow>(
    ctx: HeadlessToolRoundCtx<'_, E>,
) {
    let HeadlessToolRoundCtx {
        turn_index,
        quiet,
        api,
        token,
        current_session_id,
        tool_calls,
        edge_tool_round,
        reasoning_content,
        edge_callback_outputs,
        messages,
        tool_results,
        valid_tool_names,
        restricted_tools,
        turn_guard,
        step_recorder,
        idempotency_cache,
        semantic_dedup,
        call_counts,
        max_identical_calls,
        max_tools_per_turn,
        tool_call_records,
        tool_event_hooks,
        term,
        mailbox,
        permission_context,
        progress_emitter,
        pre_resolved_results,
    } = ctx;
    let HeadlessPreparedRound {
        effective_permission_timeout,
        tool_calls,
        pre_resolved_ids,
        indices,
        step_deadline,
        consumed_edge,
    } = prepare_headless_tool_round(
        permission_context,
        tool_calls,
        edge_tool_round,
        reasoning_content,
        pre_resolved_results,
        messages,
        tool_results,
        step_recorder,
    )
    .await;
    let tool_calls = tool_calls.as_ref();
    let mut pipeline = HeadlessToolExecutionPipeline::new(
        HeadlessToolExecutionCtx {
            turn_index,
            quiet,
            api,
            token,
            current_session_id,
            tool_calls,
            edge_tool_round,
            by_sig: edge_callback_outputs,
            pre_resolved_ids: &pre_resolved_ids,
            messages,
            tool_results,
            valid_tool_names,
            restricted_tools,
            turn_guard,
            step_recorder,
            idempotency_cache,
            semantic_dedup,
            call_counts,
            max_identical_calls,
            max_tools_per_turn,
            tool_call_records,
            tool_event_hooks,
            term,
            mailbox,
            permission_context,
            progress_emitter,
            effective_permission_timeout,
        },
        consumed_edge,
    );

    for item in &indices {
        if let Some((aborted_count, aborted_tools)) = step_deadline.step_timeout_abort(
            &indices,
            pipeline.tool_results_len(),
            pipeline.tool_calls(),
            |i| pipeline.edge_tool_name(i),
        ) {
            agent_warn!(
                "step",
                "Step timeout exceeded: {}ms > {}ms, aborting {} tools: {:?}",
                step_deadline.elapsed_ms(),
                pipeline.scheduling_timeout_ms(),
                aborted_count,
                aborted_tools
            );
            pipeline.record_step_abort(&aborted_tools);
            break;
        }
        if let HeadlessToolSlotControl::AbortRound = pipeline.run_slot(*item).await {
            break;
        }
    }
}
