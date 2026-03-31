//! Single agentic iteration: outbound `/chat` payload (selector, skills, explain stderr), `/chat/turn`
//! fetch + SSE consume, turn ingest, stall preflight, headless tool round, post-tool policy.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use crossterm::style::Stylize;
use mo_agent_core::agent_warn;
use mo_agent_runtime::{
    pipeline::step_protocol::{IdempotencyKey, InMemoryIdempotencyCache, StepCheckpoint},
    pipeline::step_recorder::StepRecorder,
    semantic_dedup::SemanticDedup,
    tool_registry::{self, ToolRegistry},
    tool_selector::{self, ToolSelector},
    turn::agentic_post_tool_policy::{
        AgenticPostToolIterationControl, AgenticPostToolPolicyRequest,
        apply_agentic_post_tool_policy, map_post_tool_policy_outcome,
    },
    turn::agentic_prepare_payload::apply_selector_hints_then_attach_filtered_edge_tools,
    turn::agentic_turn_flow::{
        agentic_round_stall_preflight_with_tool_calls, append_explain_turn_batch,
    },
    turn::agentic_turn_ingest::{
        AgenticIngestIterationControl, AgenticTurnIngestMut,
        agentic_turn_stream_snapshot_from_sse_accum, ingest_agentic_turn_stream,
        map_ingest_outcome_to_iteration_control,
    },
    turn::agentic_turn_telemetry::{
        accumulate_selector_token_usage, capture_first_selection_report_if_empty,
        record_first_latency_ms_since, record_first_selector_latency_and_strategy,
    },
    turn::boost_domain_hints::{domain_hints_debug_strings, domain_hints_from_boost_terms},
    turn::chat_history_openai::merge_skill_names_track,
    turn::chat_turn_api_error::{
        CHAT_TURN_POST_MAX_RETRIES, chat_turn_http_error_with_compact_body,
    },
    turn::chat_turn_budget_pressure::budget_pressure_for_chat_turn,
    turn::chat_turn_edge_profile::{
        detect_active_system_skills_in_message, read_git_branch_abbrev,
    },
    turn::chat_turn_explain_wire::{AgenticChatExplainFlags, AgenticExplainUiMode},
    turn::chat_turn_heuristics::extract_repos_from_memory,
    turn::chat_turn_payload::{
        ChatTurnBasePayloadInput, chat_turn_base_payload, merge_active_skills_into_edge_profile,
        merge_skill_instructions_into_edge_profile, set_payload_tool_results_if_non_empty,
    },
    turn::chat_turn_selection_context::build_agentic_tool_selection_context,
    turn::chat_turn_step_plan::record_agentic_step_plan_after_payload_prep,
    turn::edge_prompt_context::make_args_preview,
    turn::headless_tool_assembly::{
        CACHEABLE_TOOLS, HeadlessResolvedToolSlot, begin_headless_tool_round_opening,
        headless_idempotency_hit_openai_pair, headless_openai_duplicate_within_turn_pair,
        headless_unknown_local_tool_openai_pair, openai_tool_roundtrip_values,
        resolve_headless_tool_slot, take_edge_output_for_tool_call,
        unknown_local_tool_error_message,
    },
    turn::headless_tool_journal::{
        journal_record_cross_turn_cache_hit, journal_record_duplicate_within_turn,
        journal_record_executed_tool_call, journal_record_unknown_tool,
    },
    turn::headless_tool_postprocess::{
        HeadlessCacheableRecordCtx, HeadlessOutputEnrichSignal, HeadlessStepDeadline,
        append_headless_result_quality_feedback, enrich_headless_tool_output_for_errors_and_limits,
        format_headless_tool_duration, record_headless_cacheable_success_and_semantic_hint,
        try_write_light_headless_step_checkpoint,
    },
    turn::headless_tool_stderr_lines::{
        headless_stderr_cache_hit_line, headless_stderr_error_preview_line,
        headless_stderr_resource_limit_blocked, headless_stderr_resource_limit_in_output,
        headless_stderr_tool_error_detail_line, headless_stderr_tool_error_header,
        headless_stderr_tool_ok_footer_line, headless_stderr_tool_ok_header,
        headless_stderr_unknown_tool_detail, headless_stderr_unknown_tool_header,
    },
    turn::hydrate_reflect::hydrate_reflect_placeholder_if_needed,
    turn::prepare_turn_explain_text::explain_stderr_payload_line_pair,
    turn::skill_instructions_merge::{
        merge_skill_instruction_bodies_for_chat, skill_instruction_activated_names_csv,
        skill_instruction_load_failed_message,
    },
    turn::tool_result_semantics::{is_tool_error, tool_dedup_signature},
    turn::tool_schema_prune::pin_invoked_tool_schemas,
    turn::turn_guard::{TurnGuard, merge_deprioritized_tools_into_restricted},
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

// ─── Outbound `/chat` JSON body (was `prepare_turn_request.rs`) ───────────────

/// First-turn / cross-turn counters updated while building the payload.
pub(crate) struct PrepareTurnTelemetry<'a> {
    pub first_memoria_ms: &'a mut Option<u64>,
    pub first_selector_ms: &'a mut Option<u64>,
    pub first_selector_strategy: &'a mut Option<String>,
    pub selector_tokens_in: &'a mut u64,
    pub selector_tokens_out: &'a mut u64,
    pub first_selection_report: &'a mut Option<tool_registry::SelectionReport>,
    pub first_budget_pressure: &'a mut f64,
    pub first_context_assembly_ms: &'a mut Option<u64>,
    pub all_selected_skills: &'a mut Vec<String>,
}

struct PrepareChatTurnRequest<'a> {
    messages: &'a [Value],
    current_session_id: Option<&'a str>,
    model: Option<&'a str>,
    explain: AgenticChatExplainFlags,
    project_root: &'a Path,
    message: &'a str,
    history: &'a [(String, String)],
    recent_tools: &'a [String],
    executor: &'a mut ToolExecutor,
    selector: &'a dyn tool_selector::ToolSelector,
    registry: &'a tool_registry::ToolRegistry,
    tool_results: &'a [Value],
    all_schemas: &'a [Value],
    turn_guard: &'a TurnGuard,
    restricted_tools: &'a mut HashSet<String>,
    step_recorder: &'a mut StepRecorder,
    skill_registry: &'a SharedSkillRegistry,
    quiet: bool,
    file_context: &'a [String],
    assembly_start: Instant,
    telem: PrepareTurnTelemetry<'a>,
}

async fn prepare_chat_turn_payload(ctx: PrepareChatTurnRequest<'_>) -> Value {
    let git_branch = read_git_branch_abbrev();
    let mut payload = chat_turn_base_payload(ChatTurnBasePayloadInput {
        messages: ctx.messages,
        session_id: ctx.current_session_id,
        model: ctx.model,
        explain_verbose: ctx.explain.explain_verbose,
        explain_on: ctx.explain.explain_on,
        edge_executor_id: edge_executor_instance_id(),
        capabilities: mo_thin_client::builtin_capability_preset(),
        project_root: ctx.project_root,
        git_branch,
    });
    let active_skills = detect_active_system_skills_in_message(ctx.message);
    merge_active_skills_into_edge_profile(&mut payload, &active_skills);

    let budget_pressure = {
        let schema_tokens = ctx.selector.registry().total_pinned_token_cost();
        budget_pressure_for_chat_turn(ctx.messages, ctx.model, schema_tokens as usize)
    };

    let mut boost_terms =
        mo_agent_runtime::turn::retrieval::extract_boost_terms_from_pairs(ctx.history, ctx.message);
    {
        let mem_start = Instant::now();
        let memory_contents = ctx.executor.memory_boost_search(ctx.message, 5).await;
        record_first_latency_ms_since(ctx.telem.first_memoria_ms, mem_start);
        if !memory_contents.is_empty() {
            for content in &memory_contents {
                for repo in extract_repos_from_memory(content) {
                    ctx.executor.add_preferred_repo(&repo);
                }
            }
            let ranked = mo_agent_runtime::turn::retrieval::rank_memory_results(
                ctx.message,
                &memory_contents,
            );
            mo_agent_runtime::turn::retrieval::append_boost_terms_from_ranked_memory(
                &mut boost_terms,
                ctx.message,
                &ranked,
            );
        }
    }

    let memory_domain_hints = domain_hints_from_boost_terms(&boost_terms);
    merge_deprioritized_tools_into_restricted(ctx.turn_guard, ctx.restricted_tools);
    let restricted_vec: Vec<String> = ctx.restricted_tools.iter().cloned().collect();

    ctx.step_recorder.record_perceive(
        ctx.message,
        &[],
        &domain_hints_debug_strings(&memory_domain_hints),
        &boost_terms,
    );

    let learned_context = ctx.selector.learned_context(ctx.message, ctx.recent_tools);
    let learned_context_hint = learned_context.prompt_fragment();
    let learned_task_type = learned_context
        .task_archetype
        .map(|task_type| format!("{task_type:?}").to_lowercase());

    let mut selected_skills: Vec<String> = Vec::new();
    let (turn_schemas, selection_report, selection_confidence) = if ctx.tool_results.is_empty() {
        let sel_start = Instant::now();
        let sel_ctx = build_agentic_tool_selection_context(
            ctx.message,
            ctx.history.len(),
            ctx.recent_tools,
            ctx.registry,
            boost_terms.clone(),
            budget_pressure,
            memory_domain_hints.clone(),
            restricted_vec.clone(),
            ctx.file_context.to_vec(),
            false,
        );
        let sel_result = ctx
            .selector
            .select_with_learned_context(&sel_ctx, &learned_context)
            .await;
        record_first_selector_latency_and_strategy(
            ctx.telem.first_selector_ms,
            ctx.telem.first_selector_strategy,
            sel_start,
            sel_result.strategy,
            sel_result.confidence,
        );
        accumulate_selector_token_usage(
            ctx.telem.selector_tokens_in,
            ctx.telem.selector_tokens_out,
            sel_result.selector_tokens_in,
            sel_result.selector_tokens_out,
        );
        selected_skills = sel_result.selected_skills.clone();
        let conf = sel_result.confidence;
        let (schemas, report) = tool_selector::resolve_schemas_with_pressure(
            ctx.registry,
            &sel_result.tool_names,
            budget_pressure,
        );
        (schemas, report, conf)
    } else {
        let sel_ctx = build_agentic_tool_selection_context(
            ctx.message,
            ctx.history.len(),
            ctx.recent_tools,
            ctx.registry,
            boost_terms,
            budget_pressure,
            memory_domain_hints,
            restricted_vec,
            ctx.file_context.to_vec(),
            true,
        );
        let sel_result = ctx
            .selector
            .select_with_learned_context(&sel_ctx, &learned_context)
            .await;
        accumulate_selector_token_usage(
            ctx.telem.selector_tokens_in,
            ctx.telem.selector_tokens_out,
            sel_result.selector_tokens_in,
            sel_result.selector_tokens_out,
        );
        if !sel_result.selected_skills.is_empty() {
            selected_skills = sel_result.selected_skills.clone();
        }
        let conf = sel_result.confidence;
        let (mut selected, mut report) = tool_selector::resolve_schemas_with_pressure(
            ctx.registry,
            &sel_result.tool_names,
            budget_pressure,
        );
        pin_invoked_tool_schemas(
            &mut selected,
            &mut report,
            ctx.tool_results,
            ctx.all_schemas,
        );
        (selected, report, conf)
    };

    let skill_instructions =
        load_skill_instructions_text(ctx.skill_registry, &selected_skills, ctx.quiet);
    merge_skill_names_track(ctx.telem.all_selected_skills, &selected_skills);

    merge_skill_instructions_into_edge_profile(&mut payload, skill_instructions.as_deref());

    capture_first_selection_report_if_empty(
        ctx.telem.first_selection_report,
        ctx.telem.first_budget_pressure,
        selection_report,
        budget_pressure,
    );
    ctx.executor.set_budget_pressure(budget_pressure);

    apply_selector_hints_then_attach_filtered_edge_tools(
        &mut payload,
        turn_schemas,
        ctx.restricted_tools,
        ctx.telem.first_selection_report.as_ref(),
        selection_confidence,
        learned_context_hint.as_str(),
        learned_task_type.as_deref(),
    );
    if ctx.explain.explain_stderr {
        let (restricted_line, guidance_line) =
            explain_stderr_payload_line_pair(ctx.restricted_tools, &payload, selection_confidence);
        if let Some(line) = restricted_line {
            eprintln!("{}", line.dim());
        }
        if let Some(line) = guidance_line {
            eprintln!("{}", line.dim());
        }
    }
    set_payload_tool_results_if_non_empty(&mut payload, ctx.tool_results);

    record_agentic_step_plan_after_payload_prep(
        ctx.step_recorder,
        ctx.telem.first_selection_report.as_ref(),
        *ctx.telem.first_budget_pressure,
        selection_confidence,
    );

    record_first_latency_ms_since(ctx.telem.first_context_assembly_ms, ctx.assembly_start);

    payload
}

fn load_skill_instructions_text(
    skill_registry: &SharedSkillRegistry,
    selected_skills: &[String],
    quiet: bool,
) -> Option<String> {
    if selected_skills.is_empty() {
        return None;
    }
    let mut reg = skill_registry.try_write().ok()?;
    let (outcomes, merged, activated_skills) =
        merge_skill_instruction_bodies_for_chat(selected_skills, |name| {
            reg.load_instructions(name).map_err(|e| e.to_string())?;
            Ok(reg
                .get(name)
                .and_then(|sk| sk.instruction_text())
                .map(|t| t.to_string()))
        });
    for o in outcomes {
        if let Err(e) = o.result {
            eprintln!(
                "  {} {}",
                "⚠".yellow(),
                skill_instruction_load_failed_message(o.skill_name.as_str(), e.as_str())
            );
        }
    }
    let merged = merged?;
    if !quiet && !activated_skills.is_empty() {
        eprintln!(
            "  {} Using skill: {}",
            "◆".cyan(),
            skill_instruction_activated_names_csv(activated_skills.as_slice()).cyan()
        );
    }
    Some(merged)
}

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

    let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
        messages,
        current_session_id,
        model,
        explain: AgenticChatExplainFlags::from_explain_ui_mode(match explain {
            ExplainMode::Off => AgenticExplainUiMode::Off,
            ExplainMode::On => AgenticExplainUiMode::On,
            ExplainMode::Verbose => AgenticExplainUiMode::Verbose,
        }),
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
        .post_chat_turn_retry_429(token, &payload, CHAT_TURN_POST_MAX_RETRIES, quiet)
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.map_err(|e| e.to_string())?;
        return Err(chat_turn_http_error_with_compact_body(
            status.as_u16(),
            body.as_str(),
            compact_or_raw,
        ));
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

    let opening = begin_headless_tool_round_opening(
        &turn_result.tool_calls,
        &turn_result.edge_tool_round,
        turn_result.reasoning_content.as_str(),
    );
    messages.push(opening.assistant_message);

    let indices = opening.indices;
    let tool_count = opening.tool_count;
    let mut seen_calls: HashSet<String> = HashSet::new();
    step_recorder.begin_act(tool_count);
    let step_deadline =
        HeadlessStepDeadline::from_scheduling_timeout_ms(step_recorder.scheduling().timeout_ms);
    let mut consumed_edge = vec![false; turn_result.edge_tool_round.len()];
    let by_sig: &HashMap<String, String> = &turn_result.edge_callback_outputs;

    for item in &indices {
        if let Some((aborted_count, aborted_tools)) = step_deadline.step_timeout_abort(
            &indices,
            tool_results.len(),
            &turn_result.tool_calls,
            |i| turn_result.edge_tool_round[i].tool.clone(),
        ) {
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

        let slot = resolve_headless_tool_slot(*item, &turn_result.tool_calls, |i| {
            let e = &turn_result.edge_tool_round[i];
            (e.tool.clone(), e.args.clone())
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
                eprintln!("{}", headless_stderr_cache_hit_line(&name).dim());
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

        let mut result_str = if let Some(i) = synthetic_idx {
            turn_result.edge_tool_round[i].output.clone()
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
            let err_msg = unknown_local_tool_error_message(&name, valid_tool_names);
            if !quiet {
                eprintln!("{}", headless_stderr_unknown_tool_header(&name).red());
            }
            if !quiet {
                eprintln!("{}", headless_stderr_unknown_tool_detail(&err_msg).dim());
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
                        eprintln!("{}", headless_stderr_resource_limit_blocked(&tool).yellow());
                    }
                    HeadlessOutputEnrichSignal::ResourceLimitDetectedInOutput { tool } => {
                        eprintln!("{}", headless_stderr_resource_limit_in_output(&tool).dim());
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

        if !quiet {
            let duration_str = format_headless_tool_duration(tool_elapsed);
            let detail = tool_call_detail(&name, &args);
            let summary = if !is_err {
                tool_result_summary(&name, &result_str)
            } else {
                None
            };
            if is_err {
                eprintln!(
                    "{}",
                    headless_stderr_tool_error_header(&name, &duration_str).red()
                );
                if let Some(first_line) = result_str.lines().next() {
                    let preview = headless_stderr_error_preview_line(first_line, 100);
                    eprintln!("{}", headless_stderr_tool_error_detail_line(&preview).dim());
                }
            } else {
                eprintln!(
                    "{}",
                    headless_stderr_tool_ok_header(&name, &duration_str).green()
                );
                if let Some(line) =
                    headless_stderr_tool_ok_footer_line(detail.as_deref(), summary.as_deref())
                {
                    eprintln!("{}", line.dim());
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

    let snap = agentic_turn_stream_snapshot_from_sse_accum(&turn_result, turn_result.ttft_ms);
    let edge_len = turn_result.edge_tool_round.len();
    match map_ingest_outcome_to_iteration_control(ingest_agentic_turn_stream(
        &snap,
        edge_len,
        |i| turn_result.edge_tool_round[i].tool.clone(),
        message,
        recent_tools,
        quiet,
        AgenticTurnIngestMut {
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
        },
    )) {
        AgenticIngestIterationControl::Fatal(e) => return Err(e),
        AgenticIngestIterationControl::BreakLoop => return Ok(AgenticLoopTurnExit::BreakLoop),
        AgenticIngestIterationControl::ContinueIterating => {
            return Ok(AgenticLoopTurnExit::ContinueIterating);
        }
        AgenticIngestIterationControl::ProceedWithToolCalls => {}
    }

    let tool_calls_for_guard = agentic_round_stall_preflight_with_tool_calls(
        turn_index,
        &turn_result.tool_calls,
        &turn_result.edge_tool_round,
        turn_sigs,
        turn_tool_names,
        stall_events,
        turn_guard,
    );

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
    append_explain_turn_batch(explain_turns, turn_result.explain_turns.as_slice());

    match map_post_tool_policy_outcome(apply_agentic_post_tool_policy(
        AgenticPostToolPolicyRequest {
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
        },
    )) {
        AgenticPostToolIterationControl::Abort(e) => Err(e),
        AgenticPostToolIterationControl::RetryLlmClearToolResults => {
            tool_results.clear();
            Ok(AgenticLoopTurnExit::ContinueIterating)
        }
        AgenticPostToolIterationControl::ProceedEndTurn => {
            step_recorder.end_turn(false);
            Ok(AgenticLoopTurnExit::ContinueIterating)
        }
    }
}

#[cfg(test)]
mod tests {
    use mo_agent_runtime::turn::chat_history_openai::merge_skill_names_track;

    #[test]
    fn merge_skill_names_track_dedupes() {
        let mut v = vec!["a".into()];
        merge_skill_names_track(&mut v, &["b".into(), "a".into()]);
        assert_eq!(v, vec!["a", "b"]);
    }
}
