//! Headless tool round after SSE ingest: OpenAI messages, cache, reflect hydrate, stderr lines.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use astra_core::agent_warn;
use astra_services::session_journal::ToolCallRecord;
use astra_thin_client::ThinClient;
use serde_json::Value;

use super::headless_tool_assembly::{
    CACHEABLE_TOOLS, EdgeToolRoundRow, HeadlessResolvedToolSlot,
    begin_headless_tool_round_opening_ext, headless_idempotency_hit_openai_pair,
    headless_openai_duplicate_within_turn_pair, headless_unknown_local_tool_openai_pair,
    openai_tool_roundtrip_values, resolve_headless_tool_slot,
    take_edge_output_for_tool_call_with_duration, unknown_local_tool_error_message,
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
    call_counts: &mut HashMap<String, u32>,
    max_identical_calls: u32,
    tool_call_records: &mut Vec<ToolCallRecord>,
    tool_event_hooks: &crate::skills::hooks::ToolEventHookRegistry,
    term: &mut dyn HeadlessRoundTerminal,
    mut mailbox: Option<&mut crate::messaging::router::AgentMailbox>,
    permission_context: Option<
        &std::sync::Arc<
            tokio::sync::RwLock<crate::orchestration::permission_sync::PermissionSyncContext>,
        >,
    >,
    progress_emitter: Option<&crate::orchestration::AgentProgressEmitter>,
    // Tool results resolved by upstream interception layers (skill, send_message)
    // before the headless round. Injected immediately after the assistant message
    // to maintain correct ordering: assistant(tool_calls) → tool(pre_resolved) → tool(executed).
    pre_resolved_results: &[(String, String)],
) {
    const PERMISSION_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
    const PERMISSION_REQUEST_TIMEOUT_BACKGROUND: Duration = Duration::from_secs(5);

    // Choose shorter timeout for background agents to avoid long stalls
    // when the parent is mid-LLM-call and not polling its mailbox.
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

    // Detect thinking-model session: if any prior assistant message has
    // reasoning_content, force the field on all new assistant messages.
    let force_reasoning =
        !reasoning_content.is_empty() || super::edge_ledger::history_has_reasoning(messages);

    // Normalize tool_calls: replace empty/missing ids with synthetic UUIDs
    // so the assistant message and tool result messages share the same id.
    // Without this, APIs reject tool results with "tool_call_id not found".
    // NOTE: agentic_loop_host already calls ensure_tool_call_ids before
    // interception layers, so this is a no-op (Cow::Borrowed) in that path.
    // Kept for defense-in-depth: other callers (tests, cloud runtime) may
    // invoke this function directly without prior normalization.
    let tool_calls = super::headless_tool_assembly::ensure_tool_call_ids(tool_calls);
    let tool_calls = &tool_calls;

    let opening = begin_headless_tool_round_opening_ext(
        tool_calls,
        edge_tool_round,
        reasoning_content,
        force_reasoning,
    );
    messages.push(opening.assistant_message);

    // Inject pre-resolved tool results (from skill/send_message interception)
    // immediately after the assistant message, before headless tool execution.
    // This guarantees correct ordering: assistant(tool_calls) → tool(pre_resolved) → tool(executed).
    let mut pre_resolved_ids: HashSet<&str> = HashSet::new();
    for (call_id, result_text) in pre_resolved_results {
        pre_resolved_ids.insert(call_id.as_str());
        let (tool_msg, tr) =
            openai_tool_roundtrip_values(call_id, "pre_resolved", result_text);
        messages.push(tool_msg);
        tool_results.push(tr);
    }

    let indices = opening.indices;
    let tool_count = opening.tool_count;
    step_recorder.begin_act(tool_count);
    let step_deadline =
        HeadlessStepDeadline::from_scheduling_timeout_ms(step_recorder.scheduling().timeout_ms);
    let mut consumed_edge = vec![false; edge_tool_round.len()];
    let by_sig: &HashMap<String, String> = edge_callback_outputs;

    let max_identical = max_identical_calls;
    /// After this many consecutive empty-name tool calls in one headless round,
    /// stop processing — the model is stuck emitting malformed calls.
    const MAX_CONSECUTIVE_EMPTY_NAME: u32 = 3;
    let mut consecutive_empty_name: u32 = 0;

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

        // Skip tool calls that were already resolved by upstream interception
        // layers (skill, send_message). Their results are already in messages.
        if pre_resolved_ids.contains(id.as_str()) {
            continue;
        }

        // Reject empty tool names immediately — some models emit tool_call
        // objects with a missing/empty name field.  Treating these as
        // unknown_tool *before* dedup counting prevents a single malformed
        // burst from inflating call_counts and flooding the context.
        if name.is_empty() {
            consecutive_empty_name = consecutive_empty_name.saturating_add(1);
            // Log the raw tool call so we can diagnose *why* the name is empty
            // (e.g. OpenAI-format function.name not parsed, or truly missing).
            let raw_tc = match item {
                super::headless_tool_assembly::HeadlessRoundToolIdx::ServerToolCall(i) => {
                    tool_calls.get(*i).map(|v| v.to_string())
                }
                _ => None,
            };
            agent_warn!(
                "step",
                "Empty tool name in slot {item:?} (id={id}), raw tool_call: {}",
                raw_tc.as_deref().unwrap_or("(synthetic edge)")
            );
            let err_msg = unknown_local_tool_error_message(&name, valid_tool_names);
            if !quiet {
                term.emit_line(
                    HeadlessStderrStyle::Red,
                    headless_stderr_unknown_tool_header(&name),
                );
                term.emit_line(
                    HeadlessStderrStyle::Dim,
                    headless_stderr_unknown_tool_detail(&err_msg),
                );
            }
            let (tool_msg, err_tr) =
                headless_unknown_local_tool_openai_pair(&id, &name, valid_tool_names);
            messages.push(tool_msg);
            tool_results.push(err_tr);
            tool_call_records.push(journal_record_unknown_tool(name.clone(), 0));
            if consecutive_empty_name >= MAX_CONSECUTIVE_EMPTY_NAME {
                agent_warn!(
                    "step",
                    "Aborting headless tool round after {} consecutive empty-name tool calls",
                    consecutive_empty_name
                );
                break;
            }
            continue;
        }
        consecutive_empty_name = 0;

        let call_sig = tool_dedup_signature(&name, &args);
        let count = call_counts.entry(call_sig.clone()).or_insert(0);
        *count += 1;
        if *count > max_identical {
            // Hard cap: return cached result if available, otherwise short stub.
            let idem_key = IdempotencyKey::semantic(&name, &args);
            if let Some(cached) = idempotency_cache.check(&idem_key) {
                let body = format!(
                    "{}\n\n⛔ This is a cached repeat (call #{} for identical args, limit: {}). \
                     Do NOT call this tool again with the same arguments.",
                    cached.output, *count, max_identical
                );
                let (tool_msg, tr) = headless_idempotency_hit_openai_pair(&id, &name, &body);
                messages.push(tool_msg);
                tool_results.push(tr);
            } else {
                let (tool_msg, tr) = headless_openai_duplicate_within_turn_pair(&id, &name);
                messages.push(tool_msg);
                tool_results.push(tr);
            }
            tool_call_records.push(journal_record_duplicate_within_turn(
                name.clone(),
                make_args_preview(&name, &args),
            ));
            turn_guard.health.record_cache_hit(&name);
            agent_warn!(
                "dedup",
                "Hard cap: tool '{}' (id={}) call #{} (limit: {})",
                name,
                id,
                *count,
                max_identical
            );
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

        let (mut result_str, edge_duration_ms) = if let Some(i) = synthetic_idx {
            (
                edge_tool_round[i].tool_output().to_string(),
                edge_tool_round[i].tool_duration_ms(),
            )
        } else {
            let matched = take_edge_output_for_tool_call_with_duration(
                &name,
                &args,
                edge_tool_round,
                &mut consumed_edge,
                by_sig,
            );
            (matched.output, matched.duration_ms)
        };

        let consumed_after = consumed_edge.iter().filter(|&&c| c).count();
        // If synthetic or if we just consumed an edge result, this was an edge tool
        let is_edge_tool = synthetic_idx.is_some() || consumed_after > consumed_before;
        let early_exit_ms = if is_edge_tool && edge_duration_ms > 0 {
            edge_duration_ms
        } else {
            0
        };

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
            tool_call_records.push(journal_record_unknown_tool(name.clone(), early_exit_ms));
            continue;
        }

        // Enforce restricted_tools: the schema was filtered from the payload,
        // but if the model generates a call anyway, refuse execution.
        if restricted_tools.contains(&name) {
            let err_msg = format!(
                "Tool '{}' is currently restricted and cannot be executed. \
                 Use only the tools whose schemas were provided.",
                name
            );
            if !quiet {
                term.emit_line(
                    HeadlessStderrStyle::Yellow,
                    format!("  ⚠ Blocked restricted tool: {name}"),
                );
            }
            let (tool_msg, err_tr) = openai_tool_roundtrip_values(&id, &name, &err_msg);
            messages.push(tool_msg);
            tool_results.push(err_tr);
            tool_call_records.push(journal_record_blocked_tool(
                name.clone(),
                err_msg,
                make_args_preview(&name, &args),
                early_exit_ms,
            ));
            continue;
        }

        // ── Permission gate check ──
        let args_str = serde_json::to_string(&args).ok();
        match super::permission_gate::check_tool_permission(
            &name,
            args_str.as_deref(),
            permission_context,
            mailbox.as_deref_mut(),
            effective_permission_timeout,
        )
        .await
        {
            super::permission_gate::PermissionCheckResult::Allowed => {}
            super::permission_gate::PermissionCheckResult::AllowedViaRequest { .. } => {
                if !quiet {
                    term.emit_line(
                        HeadlessStderrStyle::Yellow,
                        format!("  🔓 Permission granted by parent: {name}"),
                    );
                }
            }
            super::permission_gate::PermissionCheckResult::Denied { reason } => {
                let err_msg =
                    super::permission_gate::permission_denied_error_result(&name, &reason);
                if !quiet {
                    term.emit_line(
                        HeadlessStderrStyle::Yellow,
                        format!("  🔒 Permission denied: {name}"),
                    );
                }
                let (tool_msg, err_tr) = openai_tool_roundtrip_values(&id, &name, &err_msg);
                messages.push(tool_msg);
                tool_results.push(err_tr);
                tool_call_records.push(journal_record_blocked_tool(
                    name.clone(),
                    reason,
                    make_args_preview(&name, &args),
                    early_exit_ms,
                ));
                continue;
            }
        }

        // ── PreToolUse hook evaluation ──
        if !tool_event_hooks.is_empty() {
            let decision =
                crate::skills::hooks::evaluate_pre_tool_hooks(tool_event_hooks, &name, &args).await;
            match decision {
                crate::skills::hooks::PreToolDecision::Block(reason) => {
                    let err_msg = format!("Tool '{}' blocked by PreToolUse hook: {}", name, reason);
                    if !quiet {
                        term.emit_line(
                            HeadlessStderrStyle::Yellow,
                            format!("  ⚠ Hook blocked: {name} — {reason}"),
                        );
                    }
                    let (tool_msg, err_tr) = openai_tool_roundtrip_values(&id, &name, &err_msg);
                    messages.push(tool_msg);
                    tool_results.push(err_tr);
                    tool_call_records.push(journal_record_blocked_tool(
                        name.clone(),
                        err_msg,
                        make_args_preview(&name, &args),
                        early_exit_ms,
                    ));
                    continue;
                }
                crate::skills::hooks::PreToolDecision::AllowWithContext(ctx) => {
                    // Context will be appended to the tool result after execution
                    result_str = format!("{result_str}\n\n[Hook context]: {ctx}");
                }
                crate::skills::hooks::PreToolDecision::Allow => {}
            }
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

        if let Some(emitter) = progress_emitter {
            emitter.tool_executing(&name, turn_index as u32);
        }

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

        let executed_ms = if is_edge_tool && edge_duration_ms > 0 {
            edge_duration_ms
        } else {
            tool_start.elapsed().as_millis() as u64
        };
        let args_size = serde_json::to_string(&args)
            .map(|s| s.len() as u32)
            .unwrap_or(0);
        let args_preview = make_args_preview(&name, &args);
        tool_call_records.push(journal_record_executed_tool_call(
            name.clone(),
            is_err,
            executed_ms,
            args_size,
            result_str.as_str(),
            args_preview,
        ));
        step_recorder.complete_tool_with_result(&name, is_err, executed_ms, false, &result_str);

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

        // Skip redundant display for edge tools — they were already shown during
        // the SSE tool round in the CLI.  Only emit headless status lines for
        // server-side (non-edge) tools, or when running in sub-run / quiet-false
        // mode where the SSE stream is suppressed and edge tools are the only
        // source of progress.
        if !quiet && !is_edge_tool {
            let duration_str = format_headless_tool_duration(Duration::from_millis(executed_ms));
            let detail = tool_call_detail(&name, &args);
            let summary = if !is_err {
                tool_result_summary(&name, &result_str)
            } else {
                None
            };
            if is_err {
                term.emit_line(
                    HeadlessStderrStyle::Red,
                    headless_stderr_tool_error_line(&name, &duration_str, detail.as_deref()),
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
                        &name,
                        &duration_str,
                        detail.as_deref(),
                        summary.as_deref(),
                    ),
                );
            }
        }

        // Also skip body preview for edge tools in interactive mode (already shown in SSE).
        if !is_edge_tool {
            emit_headless_tool_body_preview(term, quiet, &name, &result_str, is_err);
        }

        // ── PostToolUse hook evaluation ──
        if !tool_event_hooks.is_empty() && !is_err {
            if let Some(modified) = crate::skills::hooks::evaluate_post_tool_hooks(
                tool_event_hooks,
                &name,
                &args,
                &result_str,
            )
            .await
            {
                result_str = modified;
            }
        }

        let model_result_str = tool_result_content_for_model(&name, &result_str);

        // Persist large tool results to disk, replacing inline content with
        // a compact preview + file reference to keep the context window lean.
        let model_result_str = if let Some(sid) = current_session_id {
            let session_dir = astra_services::session_journal::local_sessions_dir().join(sid);
            match super::tool_result_storage::maybe_persist_tool_result(
                &session_dir,
                &id,
                &name,
                &model_result_str,
            ) {
                Some(replacement) => replacement,
                None => model_result_str,
            }
        } else {
            model_result_str
        };

        let (tool_msg, tr) = openai_tool_roundtrip_values(&id, &name, &model_result_str);
        messages.push(tool_msg);
        tool_results.push(tr);
    }
}
