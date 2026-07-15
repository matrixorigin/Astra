//! Headless tool round after SSE ingest: OpenAI messages, cache, reflect hydrate, stderr lines.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use astra_core::agent_warn;
use astra_services::session_journal::ToolCallRecord;
use astra_thin_client::ThinClient;
use serde_json::Value;

use super::super::headless_tool_pipeline::{
    HeadlessToolExecutionCtx, HeadlessToolExecutionPipeline,
};
use astra_pipeline::step_protocol::{ExecutionSlotSpec, InMemoryIdempotencyCache};
use astra_pipeline::step_recorder::StepRecorder;
use astra_text_utils::semantic_dedup::SemanticDedup;
use astra_turn_core::edge_prompt_context::make_args_preview;
use astra_turn_core::headless_tool_assembly::{
    EdgeToolRoundRow, begin_headless_tool_round_opening_ext, openai_tool_roundtrip_values,
    resolve_headless_tool_slot,
};
use astra_turn_core::headless_tool_postprocess::HeadlessStepDeadline;
use astra_turn_core::tool_result_sanitize::tool_result_content_for_model;
use astra_turn_core::turn_guard::TurnGuard;

// Re-export headless types from turn-core (canonical definitions live there).
pub use astra_turn_core::headless_tool_body_preview::{
    HeadlessRoundTerminal, HeadlessStderrStyle, NoopHeadlessTerminal,
};

use crate::orchestration::PermissionSyncHandle;

/// Typed execution context for one headless tool round.
pub struct HeadlessToolRoundCtx<'a, E: EdgeToolRoundRow> {
    /// Internal agentic step index (0-based) for cache and loop accounting.
    pub turn_index: usize,
    /// User-visible session turn currently in progress (1-based).
    pub session_turn: u32,
    pub quiet: bool,
    pub api: &'a ThinClient,
    pub token: &'a str,
    pub current_user_id: Option<&'a str>,
    pub current_session_id: Option<&'a String>,
    /// Durable run identity for server-side tool invocations.
    pub current_run_id: Option<&'a str>,
    /// Durable causal turn identity. This may span retries/resume of one visible turn.
    pub current_turn_chain_id: Option<&'a str>,
    pub tool_calls: &'a [Value],
    pub edge_tool_round: &'a [E],
    pub reasoning_content: &'a str,
    pub reasoning_signature: &'a str,
    pub edge_callback_outputs: &'a HashMap<String, String>,
    pub messages: &'a mut Vec<Value>,
    pub tool_results: &'a mut Vec<Value>,
    pub valid_tool_names: &'a HashSet<String>,
    /// Names listed in this turn's `<deferred-tools>` manifest. Empty when
    /// the surface produced no deferred listing for this turn. Threaded into
    /// [`HeadlessToolExecutionCtx::deferred_tool_names`] so the validator can
    /// emit the activation hint instead of "Unknown tool" for these names.
    pub deferred_tool_names: &'a HashSet<String>,
    pub restricted_tools: &'a mut HashSet<String>,
    pub turn_guard: &'a mut TurnGuard,
    pub step_recorder: &'a mut StepRecorder,
    pub idempotency_cache: &'a mut InMemoryIdempotencyCache,
    pub semantic_dedup: &'a mut SemanticDedup,
    pub call_counts: &'a mut HashMap<String, u32>,
    pub max_identical_calls: u32,
    pub max_tools_per_turn: u32,
    /// Mirrors `AgenticLoopState::repeated_cache_hit_suppression`; threaded
    /// into the downstream `HeadlessToolExecutionCtx`.
    pub repeated_cache_hit_suppression: u32,
    /// Mirrors `AgenticLoopState::max_consecutive_empty_name`.
    pub max_consecutive_empty_name: u32,
    pub tool_call_records: &'a mut Vec<ToolCallRecord>,
    pub tool_event_hooks: &'a crate::skills::hooks::ToolEventHookRegistry,
    pub term: &'a mut dyn HeadlessRoundTerminal,
    pub mailbox: Option<&'a mut astra_messaging::router::AgentMailbox>,
    pub permission_context: Option<&'a PermissionSyncHandle>,
    pub progress_emitter: Option<&'a crate::orchestration::AgentProgressEmitter>,
    /// Tool results resolved by upstream interception layers (skill, send_message)
    /// before the headless round. Injected immediately after the assistant message
    /// to maintain correct ordering: assistant(tool_calls) → tool(pre_resolved) → tool(executed).
    pub pre_resolved_results: &'a [(String, String)],
    /// Optional server-side tool executor for web agent sessions.
    pub runtime_tool_executor:
        Option<&'a crate::server::runtime_tool_executor::RuntimeToolExecutor>,
    // ── Observability (Phase 1) ──
    /// Turn start instant for computing start_offset_ms on tool records.
    pub turn_start: Option<std::time::Instant>,
    /// Current LLM round index (0-based) within this turn.
    pub llm_round: u32,
    /// Whether the session is still in read-only plan authoring mode.
    pub plan_mode_active: bool,
}

struct HeadlessPreparedRound<'a> {
    effective_permission_timeout: Duration,
    tool_calls: std::borrow::Cow<'a, [Value]>,
    pre_resolved_ids: HashSet<String>,
    indices: Vec<astra_turn_core::headless_tool_assembly::HeadlessRoundToolIdx>,
    step_deadline: HeadlessStepDeadline,
    consumed_edge: Vec<bool>,
}

async fn prepare_headless_tool_round<'a, E: EdgeToolRoundRow>(
    permission_context: Option<&PermissionSyncHandle>,
    tool_calls: &'a [Value],
    edge_tool_round: &'a [E],
    reasoning_content: &str,
    reasoning_signature: &str,
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

    let force_reasoning = !reasoning_content.is_empty()
        || astra_turn_core::edge_ledger::history_has_reasoning(messages);
    let tool_calls = astra_turn_core::headless_tool_assembly::ensure_tool_call_ids(tool_calls);

    let opening = begin_headless_tool_round_opening_ext(
        &tool_calls,
        edge_tool_round,
        reasoning_content,
        reasoning_signature,
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

    let slot_specs = opening
        .indices
        .iter()
        .map(|idx| {
            let slot = resolve_headless_tool_slot(*idx, tool_calls.as_ref(), |edge_idx| {
                let edge = &edge_tool_round[edge_idx];
                (
                    edge.assistant_tool_call_id(edge_idx),
                    edge.tool_name().to_string(),
                    edge.tool_args().clone(),
                )
            });
            ExecutionSlotSpec {
                args_preview: make_args_preview(&slot.name, &slot.args),
                tool_name: slot.name,
                call_id: slot.id,
                idempotency_key: None,
            }
        })
        .collect::<Vec<_>>();
    if slot_specs.is_empty() {
        step_recorder.begin_act(opening.tool_count);
    } else {
        step_recorder.begin_act_with_slots(slot_specs);
    }
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
        session_turn,
        quiet,
        api,
        token,
        current_user_id,
        current_session_id,
        current_run_id,
        current_turn_chain_id,
        tool_calls,
        edge_tool_round,
        reasoning_content,
        reasoning_signature,
        edge_callback_outputs,
        messages,
        tool_results,
        valid_tool_names,
        deferred_tool_names,
        restricted_tools,
        turn_guard,
        step_recorder,
        idempotency_cache,
        semantic_dedup,
        call_counts,
        max_identical_calls,
        max_tools_per_turn,
        repeated_cache_hit_suppression,
        max_consecutive_empty_name,
        tool_call_records,
        tool_event_hooks,
        term,
        mailbox,
        permission_context,
        progress_emitter,
        pre_resolved_results,
        runtime_tool_executor,
        turn_start,
        llm_round,
        plan_mode_active,
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
        reasoning_signature,
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
            session_turn,
            quiet,
            api,
            token,
            current_user_id,
            current_session_id,
            current_run_id,
            current_turn_chain_id,
            tool_calls,
            edge_tool_round,
            by_sig: edge_callback_outputs,
            pre_resolved_ids: &pre_resolved_ids,
            messages,
            tool_results,
            valid_tool_names,
            deferred_tool_names,
            restricted_tools,
            turn_guard,
            step_recorder,
            idempotency_cache,
            semantic_dedup,
            call_counts,
            max_identical_calls,
            max_tools_per_turn,
            repeated_cache_hit_suppression,
            max_consecutive_empty_name,
            tool_call_records,
            tool_event_hooks,
            term,
            mailbox,
            permission_context,
            progress_emitter,
            effective_permission_timeout,
            runtime_tool_executor,
            turn_start,
            llm_round,
            plan_mode_active,
        },
        consumed_edge,
    );

    // Partition indices into batches: consecutive read-only tools run concurrently,
    // non-read-only tools run serially (one at a time).
    let batches = partition_tool_batches_with_provider_policy(&indices, tool_calls, |tool_name| {
        runtime_tool_executor.and_then(|executor| {
            match executor.provider_policy_lookup(tool_name) {
                crate::server::runtime_tool_executor::ProviderPolicyLookup::NotProvider => None,
                crate::server::runtime_tool_executor::ProviderPolicyLookup::Resolved(policy) => {
                    Some(policy.parallelizable)
                }
                crate::server::runtime_tool_executor::ProviderPolicyLookup::MissingPolicy {
                    ..
                } => Some(false),
            }
        })
    });
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

use astra_turn_core::headless_tool_assembly::HeadlessRoundToolIdx;

pub(crate) enum ToolBatch {
    Concurrent(Vec<HeadlessRoundToolIdx>),
    Serial(HeadlessRoundToolIdx),
}

#[cfg(test)]
pub(crate) fn partition_tool_batches(
    indices: &[HeadlessRoundToolIdx],
    tool_calls: &[Value],
) -> Vec<ToolBatch> {
    partition_tool_batches_with_provider_policy(indices, tool_calls, |_| None)
}

pub(crate) fn partition_tool_batches_with_provider_policy<F>(
    indices: &[HeadlessRoundToolIdx],
    tool_calls: &[Value],
    provider_parallelizable: F,
) -> Vec<ToolBatch>
where
    F: Fn(&str) -> Option<bool>,
{
    use astra_turn_core::headless_tool_assembly::READ_ONLY_TOOLS;
    use astra_turn_core::tool_policy::is_tool_concurrency_safe;

    let mut batches = Vec::new();
    let mut concurrent_buf: Vec<HeadlessRoundToolIdx> = Vec::new();

    for &idx in indices {
        let (tool_name, tool_args) = match &idx {
            HeadlessRoundToolIdx::ServerToolCall(i) => {
                let call = tool_calls.get(*i);
                (
                    call.and_then(|tc| tc.get("function"))
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or(""),
                    call.and_then(astra_turn_core::parallel_tool_exec::parse_tool_args),
                )
            }
            HeadlessRoundToolIdx::SyntheticEdge(_) => ("synthetic_edge", None),
        };

        let is_readonly = if tool_name == "synthetic_edge" {
            true
        } else if let Some(parallelizable) = provider_parallelizable(tool_name) {
            parallelizable
        } else {
            READ_ONLY_TOOLS.contains(&tool_name)
                || is_tool_concurrency_safe(tool_name, tool_args.as_ref())
        };

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn server_idx(i: usize) -> HeadlessRoundToolIdx {
        HeadlessRoundToolIdx::ServerToolCall(i)
    }

    #[test]
    fn partition_batches_agent_spawn_calls_concurrently() {
        let calls = vec![
            json!({
                "id": "a1",
                "function": {
                    "name": "agent",
                    "arguments": "{\"action\":\"spawn\",\"description\":\"one\",\"prompt\":\"p1\",\"run_in_background\":true}"
                }
            }),
            json!({
                "id": "a2",
                "function": {
                    "name": "agent",
                    "arguments": "{\"action\":\"spawn\",\"description\":\"two\",\"prompt\":\"p2\",\"run_in_background\":true}"
                }
            }),
        ];

        let batches = partition_tool_batches(&[server_idx(0), server_idx(1)], &calls);
        match batches.as_slice() {
            [ToolBatch::Concurrent(items)] => assert_eq!(items.len(), 2),
            _ => panic!("agent spawn fan-out should be one concurrent batch"),
        }
    }

    #[test]
    fn partition_batches_agent_send_message_serially() {
        let calls = vec![json!({
            "id": "m1",
            "function": {
                "name": "agent",
                "arguments": "{\"action\":\"send_message\",\"to\":\"agent-1\",\"message\":{\"content\":\"hi\"}}"
            }
        })];

        let batches = partition_tool_batches(&[server_idx(0)], &calls);
        assert!(
            matches!(batches.as_slice(), [ToolBatch::Serial(_)]),
            "agent.send_message mutates mailbox ordering and must stay serial"
        );
    }

    #[test]
    fn partition_batches_dynamic_tools_from_the_resolved_provider_policy() {
        let calls = vec![
            json!({
                "id": "r1",
                "function": {"name": "provider__read", "arguments": "{}"}
            }),
            json!({
                "id": "r2",
                "function": {"name": "provider__read_two", "arguments": "{}"}
            }),
            json!({
                "id": "w1",
                "function": {"name": "provider__unknown", "arguments": "{}"}
            }),
        ];
        let batches = partition_tool_batches_with_provider_policy(
            &[server_idx(0), server_idx(1), server_idx(2)],
            &calls,
            |name| match name {
                "provider__read" | "provider__read_two" => Some(true),
                "provider__unknown" => Some(false),
                _ => None,
            },
        );

        match batches.as_slice() {
            [ToolBatch::Concurrent(reads), ToolBatch::Serial(write)] => {
                assert_eq!(reads, &[server_idx(0), server_idx(1)]);
                assert_eq!(*write, server_idx(2));
            }
            _ => panic!("resolved provider reads should batch before the serial unknown tool"),
        }
    }
}
