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
        AgenticPostToolPolicyOutcome, AgenticPostToolPolicyRequest, apply_agentic_post_tool_policy,
    },
    turn::agentic_stall_preflight::{
        CliAgenticStallPreflightRequest, apply_cli_agentic_stall_preflight,
    },
    turn::agentic_turn_ingest::{
        AgenticTurnIngestMut, AgenticTurnIngestOutcome, AgenticTurnStreamSnapshot,
        ingest_agentic_turn_stream,
    },
    turn::boost_domain_hints::domain_hints_from_boost_terms,
    turn::chat_history_openai::merge_skill_names_track,
    turn::chat_turn_budget_pressure::budget_pressure_for_chat_turn,
    turn::chat_turn_edge_profile::{
        detect_active_system_skills_in_message, read_git_branch_abbrev,
    },
    turn::chat_turn_heuristics::extract_repos_from_memory,
    turn::chat_turn_payload::{
        ChatTurnBasePayloadInput, chat_turn_base_payload, merge_active_skills_into_edge_profile,
        merge_skill_instructions_into_edge_profile, set_payload_edge_tools,
        set_payload_tool_results_if_non_empty,
    },
    turn::chat_turn_selection_context::build_agentic_tool_selection_context,
    turn::edge_prompt_context::make_args_preview,
    turn::headless_tool_assembly::{
        CACHEABLE_TOOLS, HeadlessRoundToolIdx, begin_headless_tool_round_opening,
        headless_idempotency_hit_openai_pair, headless_openai_duplicate_within_turn_pair,
        headless_unknown_local_tool_openai_pair, openai_tool_roundtrip_values,
        parse_flat_tool_call_event, take_edge_output_for_tool_call, tool_calls_for_stall_guard,
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
    turn::hydrate_reflect::hydrate_reflect_placeholder_if_needed,
    turn::prepare_turn_explain_text::{
        restricted_tools_explain_text, selector_guidance_explain_text,
    },
    turn::tool_result_semantics::{is_tool_error, tool_dedup_signature},
    turn::tool_schema_prune::{filter_tool_schemas_by_excluded_names, pin_invoked_tool_schemas},
    turn::turn_guard::TurnGuard,
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
    explain_verbose: bool,
    explain_on: bool,
    explain_stderr: bool,
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
        explain_verbose: ctx.explain_verbose,
        explain_on: ctx.explain_on,
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
        let mem_elapsed = mem_start.elapsed().as_millis() as u64;
        if ctx.telem.first_memoria_ms.is_none() {
            *ctx.telem.first_memoria_ms = Some(mem_elapsed);
        }
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
    for tool in ctx.turn_guard.health.deprioritized_tools() {
        ctx.restricted_tools.insert(tool.to_string());
    }
    let restricted_vec: Vec<String> = ctx.restricted_tools.iter().cloned().collect();

    ctx.step_recorder.record_perceive(
        ctx.message,
        &[],
        &memory_domain_hints
            .iter()
            .map(|h| format!("{h:?}"))
            .collect::<Vec<_>>(),
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
        if ctx.telem.first_selector_ms.is_none() {
            *ctx.telem.first_selector_ms = Some(sel_start.elapsed().as_millis() as u64);
            *ctx.telem.first_selector_strategy = Some(format!(
                "{} (conf={:.2})",
                sel_result.strategy, sel_result.confidence
            ));
        }
        *ctx.telem.selector_tokens_in += sel_result.selector_tokens_in;
        *ctx.telem.selector_tokens_out += sel_result.selector_tokens_out;
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

    if ctx.telem.first_selection_report.is_none() {
        *ctx.telem.first_selection_report = Some(selection_report);
        *ctx.telem.first_budget_pressure = budget_pressure;
    }
    ctx.executor.set_budget_pressure(budget_pressure);

    tool_registry::apply_selector_hints_to_edge_profile(
        &mut payload["edge_profile"],
        ctx.telem.first_selection_report.as_ref(),
        selection_confidence,
        &learned_context_hint,
        learned_task_type.as_deref(),
    );
    let final_schemas = filter_tool_schemas_by_excluded_names(turn_schemas, ctx.restricted_tools);
    set_payload_edge_tools(&mut payload, final_schemas);
    eprint_restricted_tools_explain(ctx.explain_stderr, ctx.restricted_tools);
    eprint_selector_guidance_explain(ctx.explain_stderr, &payload, selection_confidence);
    set_payload_tool_results_if_non_empty(&mut payload, ctx.tool_results);

    {
        let selected_tool_names: Vec<String> = ctx
            .telem
            .first_selection_report
            .as_ref()
            .map(|r| r.tools_selected.clone())
            .unwrap_or_default();
        let bp = *ctx.telem.first_budget_pressure;
        let bt = ctx
            .telem
            .first_selection_report
            .as_ref()
            .map(|r| r.budget_used as u64)
            .unwrap_or(0);
        ctx.step_recorder
            .record_plan(&selected_tool_names, selection_confidence, bp, bt);
    }

    if ctx.telem.first_context_assembly_ms.is_none() {
        *ctx.telem.first_context_assembly_ms =
            Some(ctx.assembly_start.elapsed().as_millis() as u64);
    }

    payload
}

fn eprint_restricted_tools_explain(show: bool, restricted_tools: &HashSet<String>) {
    if !show {
        return;
    }
    if let Some(line) = restricted_tools_explain_text(restricted_tools) {
        eprintln!("{}", line.dim());
    }
}

fn eprint_selector_guidance_explain(show: bool, payload: &Value, selection_confidence: f64) {
    if !show {
        return;
    }
    if let Some(line) = selector_guidance_explain_text(payload, selection_confidence) {
        eprintln!("{}", line.dim());
    }
}

fn load_skill_instructions_text(
    skill_registry: &SharedSkillRegistry,
    selected_skills: &[String],
    quiet: bool,
) -> Option<String> {
    if selected_skills.is_empty() {
        return None;
    }
    let mut instructions = Vec::new();
    let mut activated_skills = Vec::new();
    if let Ok(mut reg) = skill_registry.try_write() {
        for skill_name in selected_skills {
            if let Err(e) = reg.load_instructions(skill_name) {
                eprintln!(
                    "  {} Failed to load skill {}: {}",
                    "⚠".yellow(),
                    skill_name,
                    e
                );
                continue;
            }
            if let Some(skill) = reg.get(skill_name)
                && let Some(text) = skill.instruction_text()
            {
                activated_skills.push(skill_name.clone());
                instructions.push(format!("## Skill: {skill_name}\n\n{text}"));
            }
        }
    }
    if instructions.is_empty() {
        return None;
    }
    if !quiet {
        eprintln!(
            "  {} Using skill: {}",
            "◆".cyan(),
            activated_skills.join(", ").cyan()
        );
    }
    Some(instructions.join("\n\n---\n\n"))
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

        let (id, name, args, synthetic_idx) = match *item {
            HeadlessRoundToolIdx::ServerToolCall(i) => {
                let (id, name, args) = parse_flat_tool_call_event(&turn_result.tool_calls[i]);
                (id, name, args, None)
            }
            HeadlessRoundToolIdx::SyntheticEdge(i) => {
                let e = &turn_result.edge_tool_round[i];
                (format!("edge-{i}"), e.tool.clone(), e.args.clone(), Some(i))
            }
        };

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
                eprintln!("{}", format!("  ↻ {name} (cached)").dim());
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
                eprintln!("{}", format!("  ✗ {name}").red());
            }
            if !quiet {
                eprintln!("  {}", format!("└ {err_msg}").dim());
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
                        eprintln!(
                            "{}",
                            format!("  ⚠ {tool} blocked: system resource limit reached").yellow()
                        );
                    }
                    HeadlessOutputEnrichSignal::ResourceLimitDetectedInOutput { tool } => {
                        eprintln!(
                            "{}",
                            format!("  ⚠ {tool}: resource limit detected in output — tool blocked")
                                .dim()
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

        if !quiet {
            let duration_str = format_headless_tool_duration(tool_elapsed);
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

    let snap = AgenticTurnStreamSnapshot {
        ttft_ms: turn_result.ttft_ms,
        session_id: &turn_result.session_id,
        run_id: &turn_result.run_id,
        full_text: turn_result.full_text.as_str(),
        tool_calls: &turn_result.tool_calls,
        prompt_tokens: turn_result.prompt_tokens,
        completion_tokens: turn_result.completion_tokens,
        has_usage: turn_result.has_usage,
        error_message: &turn_result.error_message,
    };
    let edge_len = turn_result.edge_tool_round.len();
    match ingest_agentic_turn_stream(
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
    ) {
        AgenticTurnIngestOutcome::Fatal(e) => return Err(e),
        AgenticTurnIngestOutcome::Break => return Ok(AgenticLoopTurnExit::BreakLoop),
        AgenticTurnIngestOutcome::Continue => return Ok(AgenticLoopTurnExit::ContinueIterating),
        AgenticTurnIngestOutcome::HasToolCalls => {}
    }

    let tool_calls_for_guard =
        tool_calls_for_stall_guard(&turn_result.tool_calls, &turn_result.edge_tool_round);

    apply_cli_agentic_stall_preflight(CliAgenticStallPreflightRequest {
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

    match apply_agentic_post_tool_policy(AgenticPostToolPolicyRequest {
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
        AgenticPostToolPolicyOutcome::Abort(e) => Err(e),
        AgenticPostToolPolicyOutcome::RetryLlmClearToolResults => {
            tool_results.clear();
            Ok(AgenticLoopTurnExit::ContinueIterating)
        }
        AgenticPostToolPolicyOutcome::ProceedEndTurn => {
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
