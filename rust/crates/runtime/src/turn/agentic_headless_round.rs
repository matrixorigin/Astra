//! Headless tool round after SSE ingest: OpenAI messages, cache, reflect hydrate, stderr lines.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use astra_core::agent_warn;
use astra_services::session_journal::ToolCallRecord;
use astra_thin_client::ThinClient;
use serde_json::Value;

use super::headless_tool_assembly::{
    EdgeToolRoundRow, begin_headless_tool_round_opening_ext, openai_tool_roundtrip_values,
};
use super::headless_tool_pipeline::{HeadlessToolExecutionCtx, HeadlessToolExecutionPipeline};
use super::headless_tool_postprocess::HeadlessStepDeadline;
use super::tool_result_sanitize::tool_result_content_for_model;
use super::turn_guard::TurnGuard;
use crate::pipeline::step_protocol::InMemoryIdempotencyCache;
use crate::pipeline::step_recorder::StepRecorder;
use crate::semantic_dedup::SemanticDedup;

// Re-export headless types from turn-core (canonical definitions live there).
pub use astra_turn_core::headless_types::{
    HeadlessRoundTerminal, HeadlessStderrStyle, NoopHeadlessTerminal,
};

pub(crate) type PermissionSyncHandle = std::sync::Arc<
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
    /// Optional server-side tool executor for web agent sessions.
    pub server_tool_executor: Option<&'a crate::server::server_tool_executor::ServerToolExecutor>,
    // ── Observability (Phase 1) ──
    /// Turn start instant for computing start_offset_ms on tool records.
    pub turn_start: Option<std::time::Instant>,
    /// Current LLM round index (0-based) within this turn.
    pub llm_round: u32,
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
    llm_round: u32,
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
        let (mut tool_msg, tr) =
            openai_tool_roundtrip_values(call_id, "pre_resolved", &content_for_model);
        if let Some(obj) = tool_msg.as_object_mut() {
            obj.insert(
                "_round_index".to_string(),
                serde_json::Value::Number(llm_round.into()),
            );
            obj.insert(
                "_tool_name".to_string(),
                serde_json::Value::String("pre_resolved".to_string()),
            );
        }
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
        server_tool_executor,
        turn_start,
        llm_round,
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
        llm_round,
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
            server_tool_executor,
            turn_start,
            llm_round,
        },
        consumed_edge,
    );

    // Partition indices into batches: consecutive read-only tools run concurrently,
    // non-read-only tools run serially (one at a time).
    let batches = partition_tool_batches(&indices, tool_calls);
    'outer: for batch in &batches {
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

        match batch {
            ToolBatch::Concurrent(items) => {
                if !pipeline.run_batch_concurrent(items).await {
                    break 'outer;
                }
            }
            ToolBatch::Serial(item) => {
                if !pipeline.run_slot_with_control(*item).await {
                    break 'outer;
                }
            }
        }
    }
}

use super::headless_tool_assembly::HeadlessRoundToolIdx;

pub(crate) enum ToolBatch {
    Concurrent(Vec<HeadlessRoundToolIdx>),
    Serial(HeadlessRoundToolIdx),
}

pub(crate) fn partition_tool_batches(
    indices: &[HeadlessRoundToolIdx],
    tool_calls: &[Value],
) -> Vec<ToolBatch> {
    use super::headless_tool_assembly::READ_ONLY_TOOLS;

    let mut batches = Vec::new();
    let mut concurrent_buf: Vec<HeadlessRoundToolIdx> = Vec::new();

    for &idx in indices {
        let tool_name = match &idx {
            HeadlessRoundToolIdx::ServerToolCall(i) => tool_calls
                .get(*i)
                .and_then(|tc| tc.get("function"))
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or(""),
            HeadlessRoundToolIdx::SyntheticEdge(_) => "synthetic_edge",
        };

        let is_readonly = READ_ONLY_TOOLS.contains(&tool_name) || tool_name == "synthetic_edge";

        if is_readonly {
            concurrent_buf.push(idx);
        } else {
            if !concurrent_buf.is_empty() {
                batches.push(ToolBatch::Concurrent(std::mem::take(&mut concurrent_buf)));
            }
            batches.push(ToolBatch::Serial(idx));
        }
    }
    if !concurrent_buf.is_empty() {
        batches.push(ToolBatch::Concurrent(concurrent_buf));
    }
    batches
}
