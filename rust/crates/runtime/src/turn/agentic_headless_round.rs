//! Headless tool round after SSE ingest: OpenAI messages, cache, reflect hydrate, stderr lines.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use astra_core::agent_warn;
use astra_services::session_journal::ToolCallRecord;
use astra_thin_client::ThinClient;
use serde_json::Value;

use super::headless_tool_assembly::{
    CACHEABLE_TOOLS, EdgeToolRoundRow, HeadlessResolvedToolSlot,
    begin_headless_tool_round_opening_ext, headless_idempotency_hit_openai_pair,
    headless_openai_duplicate_within_turn_pair, headless_unknown_local_tool_openai_pair,
    openai_tool_roundtrip_values, resolve_headless_tool_slot, take_edge_output_for_tool_call,
    unknown_local_tool_error_message,
};
use super::headless_tool_body_preview::emit_headless_tool_body_preview;
use super::headless_tool_journal::{
    journal_record_cross_turn_cache_hit, journal_record_duplicate_within_turn,
    journal_record_executed_tool_call, journal_record_unknown_tool,
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
    headless_stderr_tool_error_detail_line, headless_stderr_tool_error_header,
    headless_stderr_tool_ok_footer_line, headless_stderr_tool_ok_header,
    headless_stderr_unknown_tool_detail, headless_stderr_unknown_tool_header,
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

/// Clears `tool_results`, appends the assistant tool-call message, then fills `tool_results` and
/// matching `tool` OpenAI messages for the next `/chat` request.
#[allow(clippy::too_many_arguments)]
pub async fn run_agentic_headless_tool_round<E: EdgeToolRoundRow>(
    turn_index: usize,
    quiet: bool,
    api: &ThinClient,
    token: &str,
    current_session_id: Option<&String>,
    tool_calls: &[Value],
    edge_tool_round: &[E],
    reasoning_content: &str,
    edge_callback_outputs: &HashMap<String, String>,
    messages: &mut Vec<Value>,
    tool_results: &mut Vec<Value>,
    valid_tool_names: &HashSet<String>,
    restricted_tools: &mut HashSet<String>,
    turn_guard: &mut TurnGuard,
    step_recorder: &mut StepRecorder,
    idempotency_cache: &mut InMemoryIdempotencyCache,
    semantic_dedup: &mut SemanticDedup,
    tool_call_records: &mut Vec<ToolCallRecord>,
    term: &mut dyn HeadlessRoundTerminal,
) {
    tool_results.clear();

    // Detect thinking-model session: if any prior assistant message has
    // reasoning_content, force the field on all new assistant messages.
    let force_reasoning =
        !reasoning_content.is_empty() || super::edge_ledger::history_has_reasoning(messages);

    let opening = begin_headless_tool_round_opening_ext(
        tool_calls,
        edge_tool_round,
        reasoning_content,
        force_reasoning,
    );
    messages.push(opening.assistant_message);

    let indices = opening.indices;
    let tool_count = opening.tool_count;
    let mut seen_calls: HashSet<String> = HashSet::new();
    step_recorder.begin_act(tool_count);
    let step_deadline =
        HeadlessStepDeadline::from_scheduling_timeout_ms(step_recorder.scheduling().timeout_ms);
    let mut consumed_edge = vec![false; edge_tool_round.len()];
    let by_sig: &HashMap<String, String> = edge_callback_outputs;

    for item in &indices {
        if let Some((aborted_count, aborted_tools)) =
            step_deadline.step_timeout_abort(&indices, tool_results.len(), tool_calls, |i| {
                edge_tool_round[i].tool_name().to_string()
            })
        {
            agent_warn!(
                "step",
                "Step timeout exceeded: {}ms > {}ms, aborting {} tools: {:?}",
                step_deadline.elapsed_ms(),
                step_recorder.scheduling().timeout_ms,
                aborted_count,
                aborted_tools
            );
            turn_guard.record_step_abort(&aborted_tools);
            break;
        }

        let slot = resolve_headless_tool_slot(*item, tool_calls, |i| {
            let e = &edge_tool_round[i];
            (e.tool_name().to_string(), e.tool_args().clone())
        });
        let HeadlessResolvedToolSlot {
            id,
            name,
            args,
            synthetic_edge_index: synthetic_idx,
        } = slot;

        let call_sig = tool_dedup_signature(&name, &args);
        if !seen_calls.insert(call_sig.clone()) {
            let (tool_msg, tr) = headless_openai_duplicate_within_turn_pair(&id, &name);
            messages.push(tool_msg);
            tool_results.push(tr);
            tool_call_records.push(journal_record_duplicate_within_turn(
                name.clone(),
                make_args_preview(&name, &args),
            ));
            continue;
        }

        let idem_key = IdempotencyKey::semantic(&name, &args);
        if CACHEABLE_TOOLS.contains(&name.as_str())
            && let Some(cached) = idempotency_cache.check(&idem_key)
        {
            if !quiet {
                term.emit_line(
                    HeadlessStderrStyle::Dim,
                    headless_stderr_cache_hit_line(&name),
                );
                emit_headless_tool_body_preview(term, quiet, &name, &cached.output, false);
            }
            let (tool_msg, tr) = headless_idempotency_hit_openai_pair(&id, &name, &cached.output);
            messages.push(tool_msg);
            tool_results.push(tr);
            let cache_key = idem_key.cache_key();
            step_recorder.begin_tool_with_key(&name, &id, Some(&cache_key));
            step_recorder.record_cache_hit(&name, cached.clone());
            turn_guard.record_cache_hit(&name);
            tool_call_records.push(journal_record_cross_turn_cache_hit(
                name.clone(),
                cached.output.len() as u32,
                make_args_preview(&name, &args),
            ));
            continue;
        }

        // Track whether this tool was executed at the edge (already displayed during SSE).
        // There are two ways a tool can be an edge tool:
        // 1. synthetic_idx.is_some() - synthetic edge slot
        // 2. take_edge_output_for_tool_call consumes an edge result - server tool matched to edge
        let consumed_before = consumed_edge.iter().filter(|&&c| c).count();

        let mut result_str = if let Some(i) = synthetic_idx {
            edge_tool_round[i].tool_output().to_string()
        } else {
            take_edge_output_for_tool_call(
                &name,
                &args,
                edge_tool_round,
                &mut consumed_edge,
                by_sig,
            )
        };

        let consumed_after = consumed_edge.iter().filter(|&&c| c).count();
        // If synthetic or if we just consumed an edge result, this was an edge tool
        let is_edge_tool = synthetic_idx.is_some() || consumed_after > consumed_before;

        if !valid_tool_names.contains(&name) {
            let err_msg = unknown_local_tool_error_message(&name, valid_tool_names);
            if !quiet {
                term.emit_line(
                    HeadlessStderrStyle::Red,
                    headless_stderr_unknown_tool_header(&name),
                );
            }
            if !quiet {
                term.emit_line(
                    HeadlessStderrStyle::Dim,
                    headless_stderr_unknown_tool_detail(&err_msg),
                );
            }
            let (tool_msg, err_tr) =
                headless_unknown_local_tool_openai_pair(&id, &name, valid_tool_names);
            messages.push(tool_msg);
            tool_results.push(err_tr);
            tool_call_records.push(journal_record_unknown_tool(name.clone()));
            continue;
        }

        result_str = hydrate_reflect_placeholder_if_needed(
            api,
            token,
            current_session_id,
            &name,
            &args,
            result_str,
        )
        .await;

        let tool_start = Instant::now();
        let tool_idem_key = if CACHEABLE_TOOLS.contains(&name.as_str()) {
            Some(idem_key.cache_key())
        } else {
            None
        };
        step_recorder.begin_tool_with_key(&name, &id, tool_idem_key.as_deref());

        let mut is_err = is_tool_error(&result_str);
        let tool_already_restricted = restricted_tools.contains(&name);
        let resource_limit_recorded = enrich_headless_tool_output_for_errors_and_limits(
            &name,
            &mut result_str,
            &mut is_err,
            tool_already_restricted,
            turn_guard,
            restricted_tools,
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
            &name,
            &mut result_str,
            resource_limit_recorded,
            turn_guard,
        );

        let args_size = serde_json::to_string(&args)
            .map(|s| s.len() as u32)
            .unwrap_or(0);
        let args_preview = make_args_preview(&name, &args);
        let tool_elapsed = tool_start.elapsed();
        tool_call_records.push(journal_record_executed_tool_call(
            name.clone(),
            is_err,
            tool_elapsed.as_millis() as u64,
            args_size,
            result_str.as_str(),
            args_preview,
        ));
        step_recorder.complete_tool_with_result(
            &name,
            is_err,
            tool_elapsed.as_millis() as u64,
            false,
            &result_str,
        );

        if let Some(sid) = current_session_id {
            try_write_light_headless_step_checkpoint(sid, step_recorder);
        }

        if !is_err && CACHEABLE_TOOLS.contains(&name.as_str()) {
            record_headless_cacheable_success_and_semantic_hint(
                &name,
                &args,
                &idem_key,
                HeadlessCacheableRecordCtx {
                    result_str: &mut result_str,
                    turn_index,
                    idempotency_cache,
                    step_recorder,
                    semantic_dedup,
                },
            );
        }

        // Skip redundant display for edge tools (already shown during SSE stream).
        if !quiet && !is_edge_tool {
            let duration_str = format_headless_tool_duration(tool_elapsed);
            let detail = tool_call_detail(&name, &args);
            let summary = if !is_err {
                tool_result_summary(&name, &result_str)
            } else {
                None
            };
            if is_err {
                term.emit_line(
                    HeadlessStderrStyle::Red,
                    headless_stderr_tool_error_header(&name, &duration_str),
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
                    headless_stderr_tool_ok_header(&name, &duration_str),
                );
                if let Some(line) =
                    headless_stderr_tool_ok_footer_line(detail.as_deref(), summary.as_deref())
                {
                    term.emit_line(HeadlessStderrStyle::Dim, line);
                }
            }
        }

        // Also skip body preview for edge tools.
        if !is_edge_tool {
            emit_headless_tool_body_preview(term, quiet, &name, &result_str, is_err);
        }

        let model_result_str = tool_result_content_for_model(&name, &result_str);
        let (tool_msg, tr) = openai_tool_roundtrip_values(&id, &name, &model_result_str);
        messages.push(tool_msg);
        tool_results.push(tr);
    }
}
