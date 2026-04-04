//! Outbound `/chat/turn` payload preparation + fetch + SSE consume.
//!
//! The heavy orchestrator (`run_agentic_loop_iteration`) has been replaced by
//! the runtime's [`run_agentic_loop_with_host`]; this module now only exposes
//! `fetch_chat_turn_sse` for use by [`super::cli_loop_host::CliAgenticLoopHost`].

use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use astra_runtime::{
    pipeline::step_recorder::StepRecorder,
    tool_registry::{self, ToolRegistry},
    tool_selector::{self, ToolSelector},
    turn::agentic_prepare_payload::apply_selector_hints_then_attach_filtered_edge_tools,
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
    turn::prepare_turn_explain_text::explain_stderr_payload_line_pair,
    turn::skill_instructions_merge::{
        merge_skill_instruction_bodies_for_chat, skill_instruction_activated_names_csv,
        skill_instruction_load_failed_message,
    },
    turn::tool_schema_prune::pin_invoked_tool_schemas,
    turn::turn_guard::{TurnGuard, merge_deprioritized_tools_into_restricted},
};
use crossterm::style::Stylize;
use serde_json::{Value, json};

use crate::{
    ExplainMode,
    cli_utils::compact_or_raw,
    edge_tools::ToolExecutor,
    permission_manager::PermissionManager,
    skill_instructions::SharedSkillRegistry,
    stream_render::{
        ChatPrepPhaseLabel, ChatTurnPrepLineGuard, EdgeSseContext, TurnResult, consume_turn_sse,
    },
};

use super::super::edge_executor::edge_executor_instance_id;

/// Per-phase stderr timings for `/chat/turn`. Enable with `MO_AGENT_CHAT_TURN_TIMING=1`
/// or `MO_DEBUG=1`.
pub(crate) fn chat_turn_timing_stderr_enabled() -> bool {
    std::env::var("MO_AGENT_CHAT_TURN_TIMING")
        .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        || std::env::var("MO_DEBUG").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

fn log_chat_turn_timing_phase(timing: bool, label: &str, mark: &mut Instant) {
    if !timing {
        return;
    }
    let ms = mark.elapsed().as_millis();
    eprintln!("{}", format!("  [chat-turn timing] {label}: {ms}ms").dim());
    *mark = Instant::now();
}

/// Updates the live stderr prep line (`Ns  Phase… ⠿`, braille animates at end) for normal chat.
fn touch_prep_ui_phase(phase: &Option<ChatPrepPhaseLabel>, label: &str) {
    if let Some(a) = phase
        && let Ok(mut w) = a.write()
    {
        *w = label.to_string();
    }
}

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
    is_plan_subtask: bool,
    plan_subtask_id: Option<&'a str>,
    /// When true, emit `[chat-turn timing] …` lines to stderr (see `chat_turn_timing_stderr_enabled`).
    timing_phases: bool,
    /// Normal chat: human-readable step shown after the elapsed second count on stderr.
    prep_ui_phase: Option<ChatPrepPhaseLabel>,
}

async fn prepare_chat_turn_payload(ctx: PrepareChatTurnRequest<'_>) -> Value {
    let timing = ctx.timing_phases;
    let mut mark = Instant::now();
    let prep_wall = mark;

    touch_prep_ui_phase(&ctx.prep_ui_phase, "Starting…");

    let git_branch = read_git_branch_abbrev();
    let mut payload = chat_turn_base_payload(ChatTurnBasePayloadInput {
        messages: ctx.messages,
        session_id: ctx.current_session_id,
        model: ctx.model,
        explain_verbose: ctx.explain.explain_verbose,
        explain_on: ctx.explain.explain_on,
        edge_executor_id: edge_executor_instance_id(),
        capabilities: astra_thin_client::builtin_capability_preset(),
        project_root: ctx.project_root,
        git_branch,
    });
    let active_skills = detect_active_system_skills_in_message(ctx.message);
    merge_active_skills_into_edge_profile(&mut payload, &active_skills);

    touch_prep_ui_phase(&ctx.prep_ui_phase, "Reading workspace…");
    let passive_msgs = ctx
        .executor
        .take_passive_workspace_diagnostic_messages(ctx.project_root, !ctx.tool_results.is_empty())
        .await;
    if !passive_msgs.is_empty()
        && let Some(root) = payload.as_object_mut()
        && let Some(messages) = root.get_mut("messages")
        && let Some(arr) = messages.as_array_mut()
    {
        for m in passive_msgs {
            arr.push(m);
        }
    }
    log_chat_turn_timing_phase(timing, "base_payload_passive_workspace", &mut mark);

    touch_prep_ui_phase(&ctx.prep_ui_phase, "Recalling memory…");

    let budget_pressure = {
        let schema_tokens = ctx.selector.registry().total_pinned_token_cost();
        budget_pressure_for_chat_turn(ctx.messages, ctx.model, schema_tokens as usize)
    };

    let mut boost_terms =
        astra_runtime::turn::retrieval::extract_boost_terms_from_pairs(ctx.history, ctx.message);
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
            let ranked =
                astra_runtime::turn::retrieval::rank_memory_results(ctx.message, &memory_contents);
            astra_runtime::turn::retrieval::append_boost_terms_from_ranked_memory(
                &mut boost_terms,
                ctx.message,
                &ranked,
            );
        }
    }
    log_chat_turn_timing_phase(timing, "memory_boost_search", &mut mark);

    touch_prep_ui_phase(&ctx.prep_ui_phase, "Preparing tools…");

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
    let learned_task_type = learned_context.task_archetype_payload_token();

    let mut selected_skills: Vec<String> = Vec::new();
    let (turn_schemas, selection_report, selection_confidence) = if ctx.tool_results.is_empty() {
        let sel_start = Instant::now();
        touch_prep_ui_phase(&ctx.prep_ui_phase, "Scanning context…");
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
        touch_prep_ui_phase(&ctx.prep_ui_phase, "Thinking…");
        let sel_result = ctx
            .selector
            .select_with_learned_context(&sel_ctx, &learned_context)
            .await;
        touch_prep_ui_phase(&ctx.prep_ui_phase, "Loading schemas…");
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
        touch_prep_ui_phase(&ctx.prep_ui_phase, "Continuing…");
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
        touch_prep_ui_phase(&ctx.prep_ui_phase, "Thinking…");
        let sel_result = ctx
            .selector
            .select_with_learned_context(&sel_ctx, &learned_context)
            .await;
        touch_prep_ui_phase(&ctx.prep_ui_phase, "Loading schemas…");
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
    log_chat_turn_timing_phase(timing, "tool_selector_resolve_schemas", &mut mark);

    touch_prep_ui_phase(&ctx.prep_ui_phase, "Merging skills…");

    let skill_instructions = load_skill_instructions_text(
        ctx.skill_registry,
        &selected_skills,
        ctx.quiet,
        ctx.explain.explain_verbose,
    );
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
    log_chat_turn_timing_phase(timing, "skill_merge_attach_edge_tools", &mut mark);

    touch_prep_ui_phase(&ctx.prep_ui_phase, "Finishing up…");

    if ctx.explain.explain_stderr {
        let (restricted_line, guidance_line) =
            explain_stderr_payload_line_pair(ctx.restricted_tools, &payload, selection_confidence);
        match (&restricted_line, &guidance_line) {
            (Some(r), Some(g)) => eprintln!("{}", format!("{r}  ·  {g}").dim()),
            (Some(r), None) => eprintln!("{}", r.as_str().dim()),
            (None, Some(g)) => eprintln!("{}", g.as_str().dim()),
            (None, None) => {}
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

    if (ctx.is_plan_subtask || ctx.plan_subtask_id.is_some())
        && let Some(root) = payload.as_object_mut()
    {
        if ctx.is_plan_subtask {
            root.insert("is_plan_subtask".into(), json!(true));
        }
        if let Some(id) = ctx.plan_subtask_id.map(str::trim).filter(|s| !s.is_empty()) {
            root.insert("plan_subtask_id".into(), json!(id));
        }
    }

    log_chat_turn_timing_phase(timing, "finalize_payload_records", &mut mark);
    if timing {
        eprintln!(
            "{}",
            format!(
                "  [chat-turn timing] prepare_payload_wall_total: {}ms",
                prep_wall.elapsed().as_millis()
            )
            .dim()
        );
    }

    payload
}

fn load_skill_instructions_text(
    skill_registry: &SharedSkillRegistry,
    selected_skills: &[String],
    quiet: bool,
    echo_skill_activation: bool,
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
    if !quiet && echo_skill_activation && !activated_skills.is_empty() {
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
    pub api: &'a astra_thin_client::ThinClient,
    pub token: &'a str,
    pub model: Option<&'a str>,
    pub explain: ExplainMode,
    pub render_md: bool,
    pub term_width: usize,
    pub quiet: bool,
    pub suppress_intermediate_output: bool,
    /// When true, do not paint assistant `text_delta` (plan JSON etc.); `full_text` still accumulates.
    pub hide_streaming_assistant_text: bool,
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
    pub turn_guard: &'a astra_runtime::turn::turn_guard::TurnGuard,
    pub restricted_tools: &'a mut HashSet<String>,
    pub step_recorder: &'a mut StepRecorder,
    pub skill_registry: &'a SharedSkillRegistry,
    pub file_context: &'a [String],
    pub assembly_start: Instant,
    pub telem: PrepareTurnTelemetry<'a>,
    pub perm_manager: &'a mut PermissionManager,
    /// Lines from the previous headless tool round that must be cleared
    /// before the next SSE stream starts rendering.
    pub pre_clear_lines: usize,
    pub is_plan_subtask: bool,
    pub plan_subtask_id: Option<&'a str>,
    /// Optional cancellation token for interrupting SSE streaming.
    pub cancel_token: Option<&'a tokio_util::sync::CancellationToken>,
    /// Plan-only: release the payload-phase stderr line before SSE consumes the body.
    pub plan_assemble_line_release: Option<Arc<AtomicBool>>,
    /// Optional channel for forwarding fine-grained stream events.
    pub stream_event_tx: Option<super::super::StreamEventTx>,
}

/// stderr prep line + timing toggles for [`fetch_chat_turn_sse`].
struct ChatTurnSseFetchUi {
    timing: bool,
    show_prep_line: bool,
    prep_ui_phase: Option<ChatPrepPhaseLabel>,
}

fn chat_turn_sse_fetch_ui(
    quiet: bool,
    suppress_intermediate_output: bool,
    plan_assemble_line_release: Option<&Arc<AtomicBool>>,
) -> ChatTurnSseFetchUi {
    let timing = chat_turn_timing_stderr_enabled();
    if timing {
        eprintln!(
            "{}",
            "  [chat-turn timing] starting prepare_chat_turn_payload…".dim()
        );
    }

    // Normal chat: one stderr status line during payload + HTTP (plan mode uses the outer
    // `PlanAssembleLineSpinner` + `plan_assemble_line_release` instead). Disabled with timing
    // stderr lines to avoid `\r` / `eprintln!` fighting.
    let show_prep_line = !quiet
        && !suppress_intermediate_output
        && std::io::stderr().is_terminal()
        && !timing
        && plan_assemble_line_release.is_none();

    let prep_ui_phase = if show_prep_line {
        Some(Arc::new(std::sync::RwLock::new("Starting…".to_string())))
    } else {
        None
    };

    ChatTurnSseFetchUi {
        timing,
        show_prep_line,
        prep_ui_phase,
    }
}

/// Build JSON payload (with optional prep line), POST `/chat/turn`, return response + prep guard.
///
/// The caller must drop [`ChatTurnPrepLineGuard`] when entering SSE consume (`consume_turn_sse`)
/// or on early error after reading the body, so the stderr status line stays through TTFB.
async fn chat_turn_post_payload_after_prepare(
    api: &astra_thin_client::ThinClient,
    token: &str,
    quiet: bool,
    ui: &ChatTurnSseFetchUi,
    prepare: PrepareChatTurnRequest<'_>,
) -> Result<(astra_thin_client::HttpResponse, ChatTurnPrepLineGuard), String> {
    let prep_line = ChatTurnPrepLineGuard::maybe_start(ui.show_prep_line, ui.prep_ui_phase.clone());
    let payload = prepare_chat_turn_payload(prepare).await;

    touch_prep_ui_phase(&ui.prep_ui_phase, "Sending…");
    let http_mark = Instant::now();
    let resp = api
        .post_chat_turn_retry_429(token, &payload, CHAT_TURN_POST_MAX_RETRIES, quiet)
        .await
        .map_err(|e| e.to_string())?;
    if ui.timing {
        eprintln!(
            "{}",
            format!(
                "  [chat-turn timing] http_post_until_response_object: {}ms",
                http_mark.elapsed().as_millis()
            )
            .dim()
        );
    }
    Ok((resp, prep_line))
}

pub(crate) async fn fetch_chat_turn_sse(
    ctx: ChatTurnSseFetchRequest<'_>,
) -> Result<TurnResult, String> {
    let ChatTurnSseFetchRequest {
        api,
        token,
        model,
        explain,
        render_md,
        term_width,
        quiet,
        suppress_intermediate_output,
        hide_streaming_assistant_text,
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
        pre_clear_lines,
        is_plan_subtask,
        plan_subtask_id,
        cancel_token,
        plan_assemble_line_release,
        stream_event_tx,
    } = ctx;

    let ui = chat_turn_sse_fetch_ui(
        quiet,
        suppress_intermediate_output,
        plan_assemble_line_release.as_ref(),
    );

    let (resp, prep_line) = chat_turn_post_payload_after_prepare(
        api,
        token,
        quiet,
        &ui,
        PrepareChatTurnRequest {
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
            is_plan_subtask,
            plan_subtask_id,
            timing_phases: ui.timing,
            prep_ui_phase: ui.prep_ui_phase.clone(),
        },
    )
    .await?;

    let status = resp.status();
    if !status.is_success() {
        drop(prep_line);
        let body = resp.text().await.map_err(|e| e.to_string())?;
        return Err(chat_turn_http_error_with_compact_body(
            status.as_u16(),
            body.as_str(),
            compact_or_raw,
        ));
    }

    // Paired with `Acquire` load in `PlanAssembleLineSpinner`: publish "HTTP response ready"
    // so the spinner thread always sees POST completion before SSE consumes the body (UI only).
    if let Some(flag) = plan_assemble_line_release.as_ref() {
        flag.store(true, Ordering::Release);
    }

    let show_reasoning_preview = hide_streaming_assistant_text;

    let edge_ctx = EdgeSseContext {
        api,
        token,
        executor_id: edge_executor_instance_id(),
        executor,
        quiet,
        suppress_intermediate_output,
        hide_streaming_assistant_text,
        show_reasoning_preview,
        perm_manager: Some(perm_manager),
        cancel_token,
        stream_event_tx,
    };

    let sse_mark = Instant::now();
    let turn = consume_turn_sse(
        prep_line,
        resp,
        render_md,
        term_width,
        quiet,
        suppress_intermediate_output,
        Some(edge_ctx),
        pre_clear_lines,
        cancel_token,
    )
    .await;
    if ui.timing {
        eprintln!(
            "{}",
            format!(
                "  [chat-turn timing] sse_consume_total: {}ms  ttft_ms: {:?}",
                sse_mark.elapsed().as_millis(),
                turn.ttft_ms
            )
            .dim()
        );
    }

    Ok(turn)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use astra_runtime::turn::chat_history_openai::merge_skill_names_track;

    #[test]
    fn merge_skill_names_track_dedupes() {
        let mut v = vec!["a".into()];
        merge_skill_names_track(&mut v, &["b".into(), "a".into()]);
        assert_eq!(v, vec!["a", "b"]);
    }
}

// Note: Environment variable parsing tests for `chat_turn_timing_stderr_enabled` were removed
// because unsafe `std::env::set_var` is unsound in multi-threaded programs. The function's
// logic is trivial (pattern matching on env var values) and not worth testing with unsafe code.
