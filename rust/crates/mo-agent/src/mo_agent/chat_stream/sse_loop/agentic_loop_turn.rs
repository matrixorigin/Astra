//! Single agentic iteration: `/chat/turn` fetch + SSE consume, turn ingest, stall preflight,
//! headless tool round (in-file), post-tool policy.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use crossterm::style::Stylize;
use mo_agent_core::agent_warn;
use mo_agent_runtime::{
    pipeline::step_checkpoint,
    pipeline::step_protocol::{
        CachedToolResult, IdempotencyKey, InMemoryIdempotencyCache, StepCheckpoint,
    },
    pipeline::step_recorder::StepRecorder,
    semantic_dedup::SemanticDedup,
    tool_registry::ToolRegistry,
    tool_selector::ToolSelector,
    turn::chat_history_openai::{append_openai_user_content_messages, openai_user_content_message},
    turn::chat_turn_heuristics::{
        openai_factual_tool_retry_user_message, should_force_factual_tool_retry,
    },
    turn::edge_prompt_context::make_args_preview,
    turn::headless_tool_assembly::{
        CACHEABLE_TOOLS, openai_assistant_with_tool_calls_message, openai_tool_roundtrip_values,
        take_edge_output_for_tool_call, tool_calls_for_stall_guard,
    },
    turn::response_guard::apply_response_guards,
    turn::stall::{IntentDrift, detect_intent_drift},
    turn::tool_result_semantics::{is_resource_limit_output, is_tool_error, tool_dedup_signature},
    turn::turn_guard::{TurnGuard, VerdictSeverity},
};
use mo_agent_services::session_journal::ToolCallRecord;
use serde_json::Value;

use crate::{
    ExplainMode, VerdictEvent,
    cli_utils::{compact_or_raw, tool_call_detail, tool_result_summary},
    edge_tools::ToolExecutor,
    permission_manager::PermissionManager,
    skill_instructions::SharedSkillRegistry,
    stream_render::{EdgeSseContext, TurnResult, consume_turn_sse},
};

use super::super::edge_executor::edge_executor_instance_id;
use super::super::hydrate_reflect::hydrate_reflect_placeholder_if_needed;
use super::prepare_turn_request::{
    PrepareChatTurnRequest, PrepareTurnTelemetry, prepare_chat_turn_payload,
};

// ─── Fetch: payload → POST → consume_turn_sse ─────────────────────────────────

pub(crate) struct ChatTurnSseFetchRequest<'a> {
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
    pub project_root: &'a Path,
    pub executor: &'a mut ToolExecutor,
    pub selector: &'a dyn ToolSelector,
    pub registry: &'a ToolRegistry,
    pub messages: &'a [Value],
    pub current_session_id: Option<&'a str>,
    pub tool_results: &'a [Value],
    pub all_schemas: &'a [Value],
    pub turn_guard: &'a mo_agent_runtime::turn::turn_guard::TurnGuard,
    pub restricted_tools: &'a mut HashSet<String>,
    pub step_recorder: &'a mut StepRecorder,
    pub skill_registry: &'a SharedSkillRegistry,
    pub file_context: &'a [String],
    pub assembly_start: Instant,
    pub telem: PrepareTurnTelemetry<'a>,
    pub perm_manager: &'a mut PermissionManager,
}

async fn fetch_chat_turn_sse(ctx: ChatTurnSseFetchRequest<'_>) -> Result<TurnResult, String> {
    let ChatTurnSseFetchRequest {
        api,
        token,
        model,
        explain,
        render_md,
        term_width,
        quiet,
        message,
        history,
        recent_tools,
        project_root,
        executor,
        selector,
        registry,
        messages,
        current_session_id,
        tool_results,
        all_schemas,
        turn_guard,
        restricted_tools,
        step_recorder,
        skill_registry,
        file_context,
        assembly_start,
        telem,
        perm_manager,
    } = ctx;

    let explain_stderr = explain != ExplainMode::Off;
    let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
        messages,
        current_session_id,
        model,
        explain_verbose: matches!(explain, ExplainMode::Verbose),
        explain_on: matches!(explain, ExplainMode::On),
        explain_stderr,
        project_root,
        message,
        history,
        recent_tools,
        executor,
        selector,
        registry,
        tool_results,
        all_schemas,
        turn_guard,
        restricted_tools,
        step_recorder,
        skill_registry,
        quiet,
        file_context,
        assembly_start,
        telem,
    })
    .await;

    let resp = api
        .post_chat_turn_retry_429(token, &payload, 3, quiet)
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.map_err(|e| e.to_string())?;
        return Err(format!("API Error ({}): {}", status, compact_or_raw(&body)));
    }

    let edge_ctx = EdgeSseContext {
        api,
        token,
        executor_id: edge_executor_instance_id(),
        executor,
        quiet,
        perm_manager: Some(std::ptr::NonNull::from(&mut *perm_manager)),
        _pm: std::marker::PhantomData,
    };

    Ok(consume_turn_sse(resp, render_md, term_width, quiet, Some(edge_ctx)).await)
}

// ─── Ingest TurnResult (guards, usage, no-tool exit) ──────────────────────────

struct TurnResultIngestRequest<'a> {
    turn_result: &'a TurnResult,
    message: &'a str,
    recent_tools: &'a [String],
    quiet: bool,
    first_ttft_ms: &'a mut Option<u64>,
    current_session_id: &'a mut Option<String>,
    current_run_id: &'a mut Option<String>,
    final_text: &'a mut String,
    total_prompt: &'a mut u64,
    total_completion: &'a mut u64,
    total_tool_calls: &'a mut u32,
    step_recorder: &'a mut StepRecorder,
    all_tools_used: &'a mut HashSet<String>,
    has_any_usage: &'a mut bool,
    forced_factual_retry: &'a mut bool,
    messages: &'a mut Vec<Value>,
}

enum TurnIngestOutcome {
    Break,
    Continue,
    Fatal(String),
    HasToolCalls,
}

fn ingest_turn_sse_result(ctx: TurnResultIngestRequest<'_>) -> TurnIngestOutcome {
    let TurnResultIngestRequest {
        turn_result,
        message,
        recent_tools,
        quiet,
        first_ttft_ms,
        current_session_id,
        current_run_id,
        final_text,
        total_prompt,
        total_completion,
        total_tool_calls,
        step_recorder,
        all_tools_used,
        has_any_usage,
        forced_factual_retry,
        messages,
    } = ctx;

    if first_ttft_ms.is_none() {
        *first_ttft_ms = turn_result.ttft_ms;
    }

    if let Some(sid) = &turn_result.session_id {
        *current_session_id = Some(sid.clone());
    }
    if turn_result.run_id.is_some() {
        *current_run_id = turn_result.run_id.clone();
    }
    if !turn_result.full_text.is_empty() {
        *final_text = turn_result.full_text.clone();

        let guard =
            apply_response_guards(final_text.as_str(), &turn_result.tool_calls, &[], message);
        if let Some(replacement) = guard.replacement {
            agent_warn!("response_guard", "Guard triggered, replacing LLM output");
            *final_text = replacement;
            return TurnIngestOutcome::Break;
        }
        if guard.quality.has_fabrication_markers {
            agent_warn!(
                "response_guard",
                "Fabrication markers detected: placeholder paths in response"
            );
        }
        if guard.quality.is_echo {
            agent_warn!(
                "response_guard",
                "Echo detected: LLM repeated user query instead of answering"
            );
        }
    }

    *total_prompt += turn_result.prompt_tokens;
    *total_completion += turn_result.completion_tokens;
    *total_tool_calls += if !turn_result.tool_calls.is_empty() {
        turn_result.tool_calls.len()
    } else {
        turn_result.edge_tool_round.len()
    } as u32;

    step_recorder.record_tokens(turn_result.prompt_tokens, turn_result.completion_tokens);

    for tc in &turn_result.tool_calls {
        if let Some(name) = tc.get("name").and_then(|v| v.as_str()) {
            all_tools_used.insert(name.to_string());
        }
    }
    for e in &turn_result.edge_tool_round {
        all_tools_used.insert(e.tool.clone());
    }
    *has_any_usage = *has_any_usage || turn_result.has_usage;

    if let Some(ref err) = turn_result.error_message {
        return TurnIngestOutcome::Fatal(err.clone());
    }

    let round_has_edge_work =
        !turn_result.tool_calls.is_empty() || !turn_result.edge_tool_round.is_empty();
    if !round_has_edge_work {
        if should_force_factual_tool_retry(
            message,
            recent_tools,
            *total_tool_calls,
            *forced_factual_retry,
        ) {
            *forced_factual_retry = true;
            if !quiet {
                eprintln!(
                    "{}",
                    "  ↻ No tool call on a live-data query; forcing one corrective retry…".yellow()
                );
            }
            messages.push(openai_factual_tool_retry_user_message(message));
            final_text.clear();
            return TurnIngestOutcome::Continue;
        }
        return TurnIngestOutcome::Break;
    }

    TurnIngestOutcome::HasToolCalls
}

// ─── Stall preflight (signatures + name-stall) ────────────────────────────────

const TOOL_NAME_STALL_WINDOW: usize = 3;

struct StallPreflightRequest<'a> {
    turn_index: u32,
    tool_calls_for_guard: &'a [Value],
    turn_sigs: &'a mut Vec<BTreeSet<String>>,
    turn_tool_names: &'a mut Vec<HashSet<String>>,
    stall_events: &'a mut Vec<(String, u32)>,
    turn_guard: &'a mut TurnGuard,
}

fn apply_stall_preflight(ctx: StallPreflightRequest<'_>) {
    let StallPreflightRequest {
        turn_index,
        tool_calls_for_guard,
        turn_sigs,
        turn_tool_names,
        stall_events,
        turn_guard,
    } = ctx;

    let sig_set: BTreeSet<String> = tool_calls_for_guard
        .iter()
        .map(|tc| {
            let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = tc.get("arguments").cloned().unwrap_or_default();
            format!(
                "{}:{}",
                name,
                serde_json::to_string(&args).unwrap_or_default()
            )
        })
        .collect();
    let name_set: HashSet<String> = tool_calls_for_guard
        .iter()
        .map(|tc| {
            tc.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        })
        .collect();
    turn_sigs.push(sig_set);
    turn_tool_names.push(name_set.clone());

    turn_guard.record_tool_calls(tool_calls_for_guard);

    let name_stall = turn_tool_names.len() >= TOOL_NAME_STALL_WINDOW
        && turn_tool_names[turn_tool_names.len() - TOOL_NAME_STALL_WINDOW..]
            .windows(2)
            .all(|w| w[0] == w[1]);

    if name_stall {
        stall_events.push(("name_stall".to_string(), turn_index));
    }
}

// ─── Post-tool: intent drift + TurnGuard verdict ─────────────────────────────

struct PostToolTurnRequest<'a> {
    turn_index: u32,
    message: &'a str,
    tool_calls_for_guard: &'a [Value],
    intent_tool_turns: &'a mut Vec<(Vec<String>, String)>,
    messages: &'a mut Vec<Value>,
    stall_events: &'a mut Vec<(String, u32)>,
    turn_guard: &'a mut TurnGuard,
    verdict_events: &'a mut Vec<VerdictEvent>,
    restricted_tools: &'a mut HashSet<String>,
    remaining_turns: &'a mut usize,
    step_recorder: &'a mut StepRecorder,
    current_session_id: Option<&'a String>,
    max_turns: usize,
    loop_turn: usize,
    recent_tools: &'a [String],
    last_heavy_checkpoint: &'a mut Option<StepCheckpoint>,
}

enum PostToolTurnOutcome {
    ProceedEndTurn,
    RetryLlmClearToolResults,
    Abort(String),
}

fn apply_post_tool_turn_policy(ctx: PostToolTurnRequest<'_>) -> PostToolTurnOutcome {
    let PostToolTurnRequest {
        turn_index,
        message,
        tool_calls_for_guard,
        intent_tool_turns,
        messages,
        stall_events,
        turn_guard,
        verdict_events,
        restricted_tools,
        remaining_turns,
        step_recorder,
        current_session_id,
        max_turns,
        loop_turn,
        recent_tools,
        last_heavy_checkpoint,
    } = ctx;

    {
        let turn_names: Vec<String> = tool_calls_for_guard
            .iter()
            .filter_map(|tc| tc.get("name").and_then(|v| v.as_str()).map(String::from))
            .collect();
        let turn_args_text: String = tool_calls_for_guard
            .iter()
            .filter_map(|tc| {
                tc.get("arguments")
                    .map(|v| serde_json::to_string(v).unwrap_or_default())
            })
            .collect::<Vec<_>>()
            .join(" ");
        intent_tool_turns.push((turn_names, turn_args_text));

        if let IntentDrift::Drifting { correction, .. } =
            detect_intent_drift(message, intent_tool_turns)
        {
            messages.push(openai_user_content_message(&correction));
            stall_events.push(("intent_drift".to_string(), turn_index));
        }
    }

    {
        let verdict = turn_guard.evaluate();

        if verdict.severity > VerdictSeverity::Healthy {
            let severity_str = match verdict.severity {
                VerdictSeverity::Critical => "critical",
                VerdictSeverity::Warning => "warning",
                VerdictSeverity::Info => "info",
                VerdictSeverity::Healthy => unreachable!(),
            };
            let health_summary = turn_guard.health.summary();
            verdict_events.push(VerdictEvent {
                turn: turn_index,
                severity: severity_str.to_string(),
                injections: verdict.injections.clone(),
                avoid_tools: verdict.avoid_tools.clone(),
                force_stop: verdict.force_stop,
                nudge_count: turn_guard.nudge_count,
                total_errors: turn_guard.errors.total_errors,
                deprioritized_count: health_summary.deprioritized_count,
                total_timeouts: health_summary.total_timeouts,
                total_cache_hits: health_summary.total_cache_hits,
                flaky_count: health_summary.flaky_count,
            });
        }

        append_openai_user_content_messages(messages, &verdict.injections);

        for tool in &verdict.avoid_tools {
            restricted_tools.insert(tool.clone());
        }

        match verdict.severity {
            VerdictSeverity::Critical => {
                *remaining_turns = remaining_turns.saturating_sub(5);
            }
            VerdictSeverity::Warning => {
                *remaining_turns = remaining_turns.saturating_sub(2);
            }
            _ => {}
        }

        let severity_label = match verdict.severity {
            VerdictSeverity::Critical => "critical",
            VerdictSeverity::Warning => "warning",
            VerdictSeverity::Info => "info",
            VerdictSeverity::Healthy => "healthy",
        };
        step_recorder.record_verdict(
            severity_label,
            verdict.stall_detected,
            verdict.is_diverging,
            verdict.force_stop,
            verdict.injections.len(),
        );

        if let Some(sid) = current_session_id
            && let Some(heavy) = step_recorder.build_heavy_checkpoint(
                messages,
                0,
                max_turns.saturating_sub(loop_turn) as u32,
                &turn_guard
                    .health
                    .deprioritized_tools()
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
                recent_tools,
            )
        {
            let cp = StepCheckpoint::Heavy(Box::new(heavy));
            let _ = step_checkpoint::write_step_checkpoint(
                sid,
                step_recorder.summary().checkpoints,
                &cp,
            );
            *last_heavy_checkpoint = Some(cp);
        }

        if verdict.force_stop {
            step_recorder.end_turn(true);
            return PostToolTurnOutcome::Abort(
                "Agent escalated to critical — too many errors and stalls. Aborting.".to_string(),
            );
        }

        if !verdict.injections.is_empty() && verdict.severity >= VerdictSeverity::Warning {
            step_recorder.end_turn(false);
            return PostToolTurnOutcome::RetryLlmClearToolResults;
        }
    }

    PostToolTurnOutcome::ProceedEndTurn
}

// ─── Headless tool round (was `tool_round.rs`) ─────────────────────────────────

struct HeadlessToolRoundRequest<'a> {
    turn_index: usize,
    quiet: bool,
    api: &'a mo_thin_client::ThinClient,
    token: &'a str,
    current_session_id: Option<&'a String>,
    turn_result: &'a TurnResult,
    messages: &'a mut Vec<serde_json::Value>,
    tool_results: &'a mut Vec<serde_json::Value>,
    valid_tool_names: &'a HashSet<String>,
    restricted_tools: &'a mut HashSet<String>,
    turn_guard: &'a mut TurnGuard,
    step_recorder: &'a mut StepRecorder,
    idempotency_cache: &'a mut InMemoryIdempotencyCache,
    semantic_dedup: &'a mut SemanticDedup,
    tool_call_records: &'a mut Vec<mo_agent_services::session_journal::ToolCallRecord>,
}

enum RoundToolItem {
    ServerTc(usize),
    Synthetic(usize),
}

/// Clears `tool_results`, appends the assistant tool-call message, then fills `tool_results` and
/// matching `tool` OpenAI messages for the next `/chat` request.
async fn run_headless_tool_round(ctx: HeadlessToolRoundRequest<'_>) {
    let HeadlessToolRoundRequest {
        turn_index,
        quiet,
        api,
        token,
        current_session_id,
        turn_result,
        messages,
        tool_results,
        valid_tool_names,
        restricted_tools,
        turn_guard,
        step_recorder,
        idempotency_cache,
        semantic_dedup,
        tool_call_records,
    } = ctx;

    tool_results.clear();

    let assistant_tc_msg = openai_assistant_with_tool_calls_message(
        &turn_result.tool_calls,
        &turn_result.edge_tool_round,
        &turn_result.reasoning_content,
    );
    messages.push(assistant_tc_msg);

    let indices: Vec<RoundToolItem> = if !turn_result.tool_calls.is_empty() {
        (0..turn_result.tool_calls.len())
            .map(RoundToolItem::ServerTc)
            .collect()
    } else {
        (0..turn_result.edge_tool_round.len())
            .map(RoundToolItem::Synthetic)
            .collect()
    };

    let tool_count = indices.len().max(1);
    let mut seen_calls: HashSet<String> = HashSet::new();
    step_recorder.begin_act(tool_count);
    let step_start_time = std::time::Instant::now();
    let step_timeout_ms = step_recorder.scheduling().timeout_ms;
    let mut consumed_edge = vec![false; turn_result.edge_tool_round.len()];
    let by_sig: &HashMap<String, String> = &turn_result.edge_callback_outputs;

    for item in &indices {
        let step_elapsed_ms = step_start_time.elapsed().as_millis() as u64;
        if step_elapsed_ms > step_timeout_ms {
            let aborted_count = indices.len() - tool_results.len();
            let aborted_tools: Vec<String> = indices[tool_results.len()..]
                .iter()
                .map(|it| match it {
                    RoundToolItem::ServerTc(i) => turn_result.tool_calls[*i]
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    RoundToolItem::Synthetic(i) => turn_result.edge_tool_round[*i].tool.clone(),
                })
                .collect();
            agent_warn!(
                "step",
                "Step timeout exceeded: {}ms > {}ms, aborting {} tools: {:?}",
                step_elapsed_ms,
                step_timeout_ms,
                aborted_count,
                aborted_tools
            );
            turn_guard.record_step_abort(&aborted_tools);
            break;
        }

        let (id, name, args, from_synthetic) = match item {
            RoundToolItem::ServerTc(i) => {
                let tc_event = &turn_result.tool_calls[*i];
                let id = tc_event
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = tc_event
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let args_raw = tc_event
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(Default::default()));
                let args = match args_raw {
                    serde_json::Value::String(s) => serde_json::from_str::<serde_json::Value>(&s)
                        .unwrap_or_else(|_| serde_json::Value::Object(Default::default())),
                    other => other,
                };
                (id, name, args, false)
            }
            RoundToolItem::Synthetic(i) => {
                let e = &turn_result.edge_tool_round[*i];
                (format!("edge-{i}"), e.tool.clone(), e.args.clone(), true)
            }
        };

        let call_sig = tool_dedup_signature(&name, &args);
        if !seen_calls.insert(call_sig.clone()) {
            let dup = "(duplicate call — result same as previous identical call this turn)";
            let (tool_msg, tr) = openai_tool_roundtrip_values(&id, &name, dup);
            messages.push(tool_msg);
            tool_results.push(tr);
            tool_call_records.push(mo_agent_services::session_journal::ToolCallRecord {
                name: name.clone(),
                ok: true,
                ms: 0,
                error: Some("duplicate_within_turn".to_string()),
                input_bytes: None,
                output_bytes: None,
                args_preview: make_args_preview(&name, &args),
            });
            continue;
        }

        let idem_key = IdempotencyKey::semantic(&name, &args);
        if CACHEABLE_TOOLS.contains(&name.as_str())
            && let Some(cached) = idempotency_cache.check(&idem_key)
        {
            let cached_note = format!(
                "(cached from earlier turn — identical call)\n{}",
                cached.output
            );
            if !quiet {
                eprintln!("{}", format!("  ↻ {name} (cached)").dim());
            }
            let (tool_msg, tr) = openai_tool_roundtrip_values(&id, &name, &cached_note);
            messages.push(tool_msg);
            tool_results.push(tr);
            let cache_key = idem_key.cache_key();
            step_recorder.begin_tool_with_key(&name, &id, Some(&cache_key));
            step_recorder.record_cache_hit(&name, cached.clone());
            turn_guard.record_cache_hit(&name);
            tool_call_records.push(mo_agent_services::session_journal::ToolCallRecord {
                name: name.clone(),
                ok: true,
                ms: 0,
                error: Some("cached_cross_turn".to_string()),
                input_bytes: None,
                output_bytes: Some(cached.output.len() as u32),
                args_preview: make_args_preview(&name, &args),
            });
            continue;
        }

        let mut result_str = if from_synthetic {
            match item {
                RoundToolItem::Synthetic(i) => turn_result.edge_tool_round[*i].output.clone(),
                _ => unreachable!(),
            }
        } else {
            take_edge_output_for_tool_call(
                &name,
                &args,
                &turn_result.edge_tool_round,
                &mut consumed_edge,
                by_sig,
            )
        };

        if !valid_tool_names.contains(&name) {
            let err_msg = format!(
                "Unknown tool '{}'. Available: {}",
                name,
                valid_tool_names
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            if !quiet {
                eprintln!("{}", format!("  ✗ {name}").red());
            }
            if !quiet {
                eprintln!("  {}", format!("└ {err_msg}").dim());
            }
            let (tool_msg, err_tr) = openai_tool_roundtrip_values(&id, &name, &err_msg);
            messages.push(tool_msg);
            tool_results.push(err_tr);
            tool_call_records.push(mo_agent_services::session_journal::ToolCallRecord {
                name: name.clone(),
                ok: false,
                ms: 0,
                error: Some(format!("unknown_tool: {name}")),
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
            });
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
        let mut resource_limit_recorded = false;

        if is_err && !tool_already_restricted {
            use mo_agent_runtime::turn::error_recovery::{build_recovery_message, classify_error};
            let category = classify_error(&result_str);

            if matches!(
                category,
                mo_agent_runtime::turn::error_recovery::ErrorCategory::ResourceLimit
            ) {
                turn_guard.health.record_resource_limit_failure(&name);
                turn_guard.errors.record_error(category);
                restricted_tools.insert(name.clone());
                resource_limit_recorded = true;
                if !quiet {
                    eprintln!(
                        "{}",
                        format!("  ⚠ {name} blocked: system resource limit reached").yellow()
                    );
                }
            }

            if matches!(
                category,
                mo_agent_runtime::turn::error_recovery::ErrorCategory::Transient
            ) {
                turn_guard.errors.record_retry(false);
            }

            let deprioritized = turn_guard.health.deprioritized_tools();
            let recovery_msg = build_recovery_message(&name, &result_str, category, &deprioritized);
            result_str.push_str(&format!("\n{recovery_msg}"));
        }

        if !is_err && !tool_already_restricted && is_resource_limit_output(&result_str) {
            turn_guard.health.record_resource_limit_failure(&name);
            turn_guard
                .errors
                .record_error(mo_agent_runtime::turn::error_recovery::ErrorCategory::ResourceLimit);
            restricted_tools.insert(name.clone());
            is_err = true;
            resource_limit_recorded = true;
            if !quiet {
                eprintln!(
                    "{}",
                    format!("  ⚠ {name}: resource limit detected in output — tool blocked").dim()
                );
            }
        }

        let result_quality = if resource_limit_recorded {
            mo_agent_runtime::turn::result_quality::ResultQuality::Error
        } else {
            turn_guard.record_tool_result(&name, &result_str)
        };

        if let Some(feedback) = turn_guard.result_feedback(&name, result_quality) {
            result_str.push_str(&format!("\n{feedback}"));
        }

        let args_size = serde_json::to_string(&args)
            .map(|s| s.len() as u32)
            .unwrap_or(0);
        let result_size = result_str.len() as u32;
        let args_preview = make_args_preview(&name, &args);
        let tool_elapsed = tool_start.elapsed();
        tool_call_records.push(mo_agent_services::session_journal::ToolCallRecord {
            name: name.clone(),
            ok: !is_err,
            ms: tool_elapsed.as_millis() as u64,
            error: if is_err {
                result_str
                    .lines()
                    .next()
                    .map(|l| l.chars().take(200).collect())
            } else {
                None
            },
            input_bytes: Some(args_size),
            output_bytes: Some(result_size),
            args_preview,
        });
        step_recorder.complete_tool_with_result(
            &name,
            is_err,
            tool_elapsed.as_millis() as u64,
            false,
            &result_str,
        );

        if let Some(sid) = current_session_id
            && let Some(light) = step_recorder.build_light_checkpoint()
        {
            let cp = StepCheckpoint::Light(light);
            let _ = step_checkpoint::write_step_checkpoint(
                sid,
                step_recorder.summary().checkpoints,
                &cp,
            );
        }

        if !is_err && CACHEABLE_TOOLS.contains(&name.as_str()) {
            let cached_result = CachedToolResult {
                tool_name: name.clone(),
                output: result_str.clone(),
                is_error: false,
                cached_at: mo_agent_runtime::pipeline::step_protocol::epoch_ms(),
            };
            step_recorder.attach_cached_result(cached_result.clone());
            idempotency_cache.record(&idem_key, cached_result);
            if let Some((prev_turn, reason)) =
                semantic_dedup.check_and_record(&name, &args, &result_str, turn_index)
            {
                let hint = format!(
                    "\n⚠ Note: this result is similar to a previous {} call (turn {}, {}). \
                     Avoid re-fetching the same information.",
                    name,
                    prev_turn + 1,
                    reason
                );
                result_str.push_str(&hint);
            }
        }

        if !quiet {
            let duration_str = if tool_elapsed.as_secs_f64() >= 1.0 {
                format!("{:.1}s", tool_elapsed.as_secs_f64())
            } else {
                format!("{}ms", tool_elapsed.as_millis())
            };
            let detail = tool_call_detail(&name, &args);
            let summary = if !is_err {
                tool_result_summary(&name, &result_str)
            } else {
                None
            };
            if is_err {
                eprintln!("{}", format!("  ✗ {name} ({duration_str})").red());
                if let Some(first_line) = result_str.lines().next() {
                    let preview = if first_line.len() > 100 {
                        format!("{}…", &first_line[..100])
                    } else {
                        first_line.to_string()
                    };
                    eprintln!("  {}", format!("└ Error: {preview}").dim());
                }
            } else {
                eprintln!("{}", format!("  ✓ {name} ({duration_str})").green());
                match (&detail, &summary) {
                    (Some(d), Some(s)) => {
                        eprintln!("  {}", format!("└ {d}  →  {s}").dim());
                    }
                    (Some(d), None) => {
                        eprintln!("  {}", format!("└ {d}").dim());
                    }
                    (None, Some(s)) => {
                        eprintln!("  {}", format!("└ {s}").dim());
                    }
                    (None, None) => {}
                }
            }
        }

        let (tool_msg, tr) = openai_tool_roundtrip_values(&id, &name, &result_str);
        messages.push(tool_msg);
        tool_results.push(tr);
    }
}

// ─── Orchestrator: one full iteration ────────────────────────────────────────

pub(crate) enum AgenticLoopTurnExit {
    ContinueIterating,
    BreakLoop,
}

pub(crate) struct AgenticTurnRequest<'a> {
    pub turn_index: usize,
    pub max_turns: usize,
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
    pub project_root: &'a Path,
    pub executor: &'a mut ToolExecutor,
    pub selector: &'a dyn ToolSelector,
    pub registry: &'a ToolRegistry,
    pub messages: &'a mut Vec<Value>,
    pub current_session_id: &'a mut Option<String>,
    pub tool_results: &'a mut Vec<Value>,
    pub all_schemas: &'a [Value],
    pub turn_guard: &'a mut TurnGuard,
    pub restricted_tools: &'a mut HashSet<String>,
    pub step_recorder: &'a mut StepRecorder,
    pub skill_registry: &'a SharedSkillRegistry,
    pub file_context: &'a [String],
    pub perm_manager: &'a mut PermissionManager,
    pub valid_tool_names: &'a HashSet<String>,
    pub idempotency_cache: &'a mut InMemoryIdempotencyCache,
    pub semantic_dedup: &'a mut SemanticDedup,
    pub turn_sigs: &'a mut Vec<BTreeSet<String>>,
    pub turn_tool_names: &'a mut Vec<HashSet<String>>,
    pub stall_events: &'a mut Vec<(String, u32)>,
    pub intent_tool_turns: &'a mut Vec<(Vec<String>, String)>,
    pub verdict_events: &'a mut Vec<VerdictEvent>,
    pub remaining_turns: &'a mut usize,
    pub last_heavy_checkpoint: &'a mut Option<StepCheckpoint>,
    pub tool_call_records: &'a mut Vec<ToolCallRecord>,
    pub first_ttft_ms: &'a mut Option<u64>,
    pub current_run_id: &'a mut Option<String>,
    pub final_text: &'a mut String,
    pub total_prompt: &'a mut u64,
    pub total_completion: &'a mut u64,
    pub total_tool_calls: &'a mut u32,
    pub all_tools_used: &'a mut HashSet<String>,
    pub has_any_usage: &'a mut bool,
    pub forced_factual_retry: &'a mut bool,
    pub explain_turns: &'a mut Vec<Value>,
    pub telem: PrepareTurnTelemetry<'a>,
}

pub(crate) async fn run_agentic_loop_iteration(
    ctx: AgenticTurnRequest<'_>,
) -> Result<AgenticLoopTurnExit, String> {
    let AgenticTurnRequest {
        turn_index,
        max_turns,
        api,
        token,
        model,
        explain,
        render_md,
        term_width,
        quiet,
        message,
        history,
        recent_tools,
        project_root,
        executor,
        selector,
        registry,
        messages,
        current_session_id,
        tool_results,
        all_schemas,
        turn_guard,
        restricted_tools,
        step_recorder,
        skill_registry,
        file_context,
        perm_manager,
        valid_tool_names,
        idempotency_cache,
        semantic_dedup,
        turn_sigs,
        turn_tool_names,
        stall_events,
        intent_tool_turns,
        verdict_events,
        remaining_turns,
        last_heavy_checkpoint,
        tool_call_records,
        first_ttft_ms,
        current_run_id,
        final_text,
        total_prompt,
        total_completion,
        total_tool_calls,
        all_tools_used,
        has_any_usage,
        forced_factual_retry,
        explain_turns,
        telem,
    } = ctx;

    let assembly_start = Instant::now();
    let turn_result = fetch_chat_turn_sse(ChatTurnSseFetchRequest {
        api,
        token,
        model,
        explain,
        render_md,
        term_width,
        quiet,
        message,
        history,
        recent_tools,
        project_root,
        executor,
        selector,
        registry,
        messages: messages.as_slice(),
        current_session_id: current_session_id.as_deref(),
        tool_results: tool_results.as_slice(),
        all_schemas,
        turn_guard,
        restricted_tools,
        step_recorder,
        skill_registry,
        file_context,
        assembly_start,
        telem,
        perm_manager,
    })
    .await?;

    match ingest_turn_sse_result(TurnResultIngestRequest {
        turn_result: &turn_result,
        message,
        recent_tools,
        quiet,
        first_ttft_ms,
        current_session_id,
        current_run_id,
        final_text,
        total_prompt,
        total_completion,
        total_tool_calls,
        step_recorder,
        all_tools_used,
        has_any_usage,
        forced_factual_retry,
        messages,
    }) {
        TurnIngestOutcome::Fatal(e) => return Err(e),
        TurnIngestOutcome::Break => return Ok(AgenticLoopTurnExit::BreakLoop),
        TurnIngestOutcome::Continue => return Ok(AgenticLoopTurnExit::ContinueIterating),
        TurnIngestOutcome::HasToolCalls => {}
    }

    let tool_calls_for_guard =
        tool_calls_for_stall_guard(&turn_result.tool_calls, &turn_result.edge_tool_round);

    apply_stall_preflight(StallPreflightRequest {
        turn_index: turn_index as u32,
        tool_calls_for_guard: &tool_calls_for_guard,
        turn_sigs,
        turn_tool_names,
        stall_events,
        turn_guard,
    });

    run_headless_tool_round(HeadlessToolRoundRequest {
        turn_index,
        quiet,
        api,
        token,
        current_session_id: current_session_id.as_ref(),
        turn_result: &turn_result,
        messages,
        tool_results,
        valid_tool_names,
        restricted_tools,
        turn_guard,
        step_recorder,
        idempotency_cache,
        semantic_dedup,
        tool_call_records,
    })
    .await;
    explain_turns.extend(turn_result.explain_turns.iter().cloned());

    match apply_post_tool_turn_policy(PostToolTurnRequest {
        turn_index: turn_index as u32,
        message,
        tool_calls_for_guard: &tool_calls_for_guard,
        intent_tool_turns,
        messages,
        stall_events,
        turn_guard,
        verdict_events,
        restricted_tools,
        remaining_turns,
        step_recorder,
        current_session_id: current_session_id.as_ref(),
        max_turns,
        loop_turn: turn_index,
        recent_tools,
        last_heavy_checkpoint,
    }) {
        PostToolTurnOutcome::Abort(e) => Err(e),
        PostToolTurnOutcome::RetryLlmClearToolResults => {
            tool_results.clear();
            Ok(AgenticLoopTurnExit::ContinueIterating)
        }
        PostToolTurnOutcome::ProceedEndTurn => {
            step_recorder.end_turn(false);
            Ok(AgenticLoopTurnExit::ContinueIterating)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_stall_fires_when_last_three_turns_repeat_same_tool_names() {
        let tc = serde_json::json!({"name":"bash","arguments":{}});
        let mut turn_sigs = Vec::new();
        let mut turn_tool_names = Vec::new();
        let mut stall_events = Vec::new();
        let mut turn_guard = TurnGuard::new();
        for i in 0..3u32 {
            apply_stall_preflight(StallPreflightRequest {
                turn_index: i,
                tool_calls_for_guard: std::slice::from_ref(&tc),
                turn_sigs: &mut turn_sigs,
                turn_tool_names: &mut turn_tool_names,
                stall_events: &mut stall_events,
                turn_guard: &mut turn_guard,
            });
        }
        assert_eq!(stall_events, vec![("name_stall".to_string(), 2)]);
    }
}
